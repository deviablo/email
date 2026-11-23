// SPDX-License-Identifier: GPL-3.0-or-later

//! The main window: folders, message list and reading pane. Three columns
//! on wide screens, the folder sidebar folds away at medium width, and a
//! single column remains on narrow screens.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use adw::prelude::*;
use chrono::{DateTime, Local};
use e_mail_core::accounts::{self, Accounts};
use e_mail_core::check;
use e_mail_core::compose::{self, Draft, Mailbox, Original, QuoteTexts, ReplyKind};
use e_mail_core::discovery::providers;
use e_mail_core::folder::FolderRole;
use e_mail_core::html;
use e_mail_core::imap::ImapConnector;
use e_mail_core::model::{
    Account, AccountId, ChangeId, Folder, FolderId, MessageId, NewAccount, Security, ServerConfig,
    ServerSettings, ThreadId,
};
use e_mail_core::notify::{
    Notice, NoticeTexts, NotificationCenter, Notifier, NotifySettings, OnScreen,
};
use e_mail_core::parse::{self, Attachment};
use e_mail_core::search::{self, SearchQuery};
use e_mail_core::server::{Connector, MailServer};
use e_mail_core::smtp::{SendError, SmtpTransport};
use e_mail_core::store::Store;
use e_mail_core::sync::{
    AccountHandle, AccountStatus, Command, SharedStore, SyncEvent, UNDO_SECONDS, start_account,
};
use e_mail_core::tls::Trust;
use gettextrs::gettext;
use gtk::{gdk, gio, glib};

use crate::background;
use crate::compose_window::ComposeWindow;
use crate::config::APP_ID;
use crate::dialogs;
use crate::files;
use crate::folder_list::{FolderList, folder_label};
use crate::message_list::{
    ListContext, MessageList, RowAction, format_long_time, format_quote_time,
};
use crate::reader::{BodyContent, CardAction, ImageChoice, Reader, sender_domain};
use crate::recipients::ContactSource;
use crate::reply_editor::ReplyEditor;
use crate::settings::Settings;
use crate::setup::{self, Hooks, SetupFlow};
use crate::tray::{self, MenuEntry, Tray};
use crate::widgets::{self, fmt};
use crate::{runtime, secrets};

type ConversationHandler = fn(&Rc<Window>);
/// Subject, sender, recipients, date and the stored message, if any.
type PrintItem = (String, String, String, String, Option<Vec<u8>>);

#[derive(Default)]
struct State {
    accounts: Vec<Account>,
    stores: HashMap<AccountId, SharedStore>,
    handles: HashMap<AccountId, AccountHandle>,
    statuses: HashMap<AccountId, AccountStatus>,
    last_sync: HashMap<AccountId, DateTime<Local>>,
    active: Option<AccountId>,
    folder: Option<FolderId>,
    thread: Option<ThreadId>,
    /// Messages whose external images the user allowed once.
    images_once: HashSet<MessageId>,
    /// A draft to open as soon as its content has arrived.
    pending_draft: Option<MessageId>,
    /// A reply to open as soon as the message it answers has arrived.
    pending_reply: Option<(MessageId, ReplyKind)>,
    /// Connections for searching on the server, per account.
    connectors: HashMap<AccountId, ImapConnector>,
    search: Option<SearchState>,
}

/// An open search above the message list.
#[derive(Default)]
struct SearchState {
    query: SearchQuery,
    /// What the server found for `server_text`.
    server_hits: Vec<MessageId>,
    server_text: String,
    /// Counts up with every new search, so late answers are ignored.
    generation: u64,
    /// The folder each conversation was found in.
    folders: HashMap<ThreadId, FolderId>,
}

struct AccountSwitcher {
    button: gtk::MenuButton,
    /// Shows that another account has unread mail.
    others_unread: gtk::Box,
    avatar: gtk::Box,
    title: gtk::Label,
    subtitle: gtk::Label,
    list: gtk::ListBox,
    popover: gtk::Popover,
}

pub struct Window {
    pub window: adw::ApplicationWindow,
    root: gtk::Stack,
    toasts: adw::ToastOverlay,
    setup_page: RefCell<Option<Rc<SetupFlow>>>,
    setup_dialog: RefCell<Option<adw::Dialog>>,
    settings_dialog: RefCell<Weak<Settings>>,
    registry: Accounts,
    data_dir: PathBuf,
    outer: adw::OverlaySplitView,
    inner: adw::NavigationSplitView,
    list_title: adw::WindowTitle,
    list_page: adw::NavigationPage,
    list_stack: gtk::Stack,
    list_empty: adw::StatusPage,
    select_button: gtk::ToggleButton,
    selection_bar: gtk::Revealer,
    selection_label: gtk::Label,
    selection_buttons: Vec<gtk::Button>,
    refresh_pending: Cell<bool>,
    folders: Rc<FolderList>,
    messages: Rc<MessageList>,
    reader: Rc<Reader>,
    switcher: AccountSwitcher,
    events: async_channel::Sender<SyncEvent>,
    settings: Option<gio::Settings>,
    state: RefCell<State>,
    reply: RefCell<Option<Rc<ReplyEditor>>>,
    composers: RefCell<Vec<Rc<ComposeWindow>>>,
    search_button: gtk::ToggleButton,
    search_bar: gtk::Revealer,
    search_entry: gtk::SearchEntry,
    search_filters: Vec<gtk::ToggleButton>,
    search_status: gtk::Label,
    search_spinner: gtk::Spinner,
    prefs: RefCell<Prefs>,
    notifications: RefCell<NotificationCenter>,
    network_up: Cell<bool>,
    network_restart: RefCell<Option<glib::SourceId>>,
    tray: RefCell<Option<Rc<Tray>>>,
}

/// Settings → General. Kept in GSettings when the app is installed; a
/// build run from the source tree keeps them for the session only.
#[derive(Clone, Debug)]
pub struct Prefs {
    pub run_in_background: bool,
    /// The icon in the panel while running in the background.
    pub show_tray_icon: bool,
    pub notify: NotifySettings,
    /// "" follows the system, otherwise "de" or "en".
    pub language: String,
    pub load_remote_images: bool,
    /// Group messages into conversations (otherwise one row per message).
    pub group_conversations: bool,
    /// Remove mail older than 30 days from the trash and the spam folder.
    pub empty_trash: bool,
    pub empty_spam: bool,
}

impl Prefs {
    fn load(settings: Option<&gio::Settings>) -> Prefs {
        match settings {
            Some(settings) => Prefs {
                run_in_background: settings.boolean("run-in-background"),
                show_tray_icon: settings.boolean("show-tray-icon"),
                notify: NotifySettings {
                    enabled: settings.boolean("notifications"),
                    muted: settings
                        .value("muted-accounts")
                        .get::<Vec<i64>>()
                        .unwrap_or_default()
                        .into_iter()
                        .collect(),
                },
                language: settings.string("language").to_string(),
                load_remote_images: settings.boolean("load-remote-images"),
                group_conversations: settings.boolean("group-conversations"),
                empty_trash: settings.boolean("empty-trash"),
                empty_spam: settings.boolean("empty-spam"),
            },
            None => Prefs {
                run_in_background: true,
                show_tray_icon: true,
                notify: NotifySettings {
                    enabled: true,
                    muted: HashSet::new(),
                },
                language: String::new(),
                load_remote_images: false,
                group_conversations: true,
                empty_trash: false,
                empty_spam: false,
            },
        }
    }

    fn save(&self, settings: Option<&gio::Settings>) {
        let Some(settings) = settings else {
            return;
        };
        let mut muted: Vec<i64> = self.notify.muted.iter().copied().collect();
        muted.sort_unstable();
        let results = [
            settings.set_boolean("run-in-background", self.run_in_background),
            settings.set_boolean("show-tray-icon", self.show_tray_icon),
            settings.set_boolean("notifications", self.notify.enabled),
            settings.set_value("muted-accounts", &muted.to_variant()),
            settings.set_string("language", &self.language),
            settings.set_boolean("load-remote-images", self.load_remote_images),
            settings.set_boolean("group-conversations", self.group_conversations),
            settings.set_boolean("empty-trash", self.empty_trash),
            settings.set_boolean("empty-spam", self.empty_spam),
        ];
        for result in results {
            if let Err(error) = result {
                tracing::warn!("could not save a setting: {error}");
            }
        }
    }
}

/// Hands notices to the system through GNotification, which uses the
/// notification portal inside Flatpak.
struct SystemNotifier(gio::Application);

impl Notifier for SystemNotifier {
    fn show(&self, notice: &Notice) {
        let notification = gio::Notification::new(&notice.title);
        notification.set_body(Some(&notice.body));
        notification.set_icon(&gio::ThemedIcon::new(APP_ID));
        notification.set_default_action_and_target_value(
            "app.notification-open",
            Some(&(notice.account, notice.open).to_variant()),
        );
        let target = (notice.account, notice.messages.clone()).to_variant();
        notification.add_button_with_target_value(
            &gettext("Mark as Read"),
            "app.notification-read",
            Some(&target),
        );
        if notice.can_delete {
            notification.add_button_with_target_value(
                &gettext("Delete"),
                "app.notification-delete",
                Some(&target),
            );
        }
        self.0.send_notification(Some(&notice.id), &notification);
    }

    fn withdraw(&self, id: &str) {
        self.0.withdraw_notification(id);
    }
}

fn settings() -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(APP_ID, true)?;
    Some(gio::Settings::new(APP_ID))
}

fn open_registry(dir: &std::path::Path) -> anyhow::Result<Accounts> {
    files::private_dir(dir)?;
    let registry = Accounts::open(&dir.join("accounts.db"))?;
    import_legacy(dir, &registry);
    files::tidy(dir, &cache_dir());
    Ok(registry)
}

fn cache_dir() -> PathBuf {
    glib::user_cache_dir().join("e-mail")
}

/// The first test version kept everything in `mail.db`. Its accounts are
/// taken over; their mails are simply loaded again.
fn import_legacy(dir: &std::path::Path, registry: &Accounts) {
    let legacy = dir.join("mail.db");
    if !legacy.exists() {
        return;
    }
    match accounts::read_legacy_accounts(&legacy) {
        Ok(old) => {
            for account in old {
                let provider = providers::provider_for_address(&account.email);
                let settings = provider
                    .and_then(|provider| provider.settings(&account.email))
                    .unwrap_or_else(|| {
                        let domain = providers::split_address(&account.email)
                            .map(|(_, domain)| domain)
                            .unwrap_or_default();
                        ServerSettings {
                            imap: ServerConfig::new(
                                account.imap_host.clone(),
                                account.imap_port,
                                Security::for_port(account.imap_port).unwrap_or(Security::Tls),
                            ),
                            smtp: ServerConfig::new(format!("smtp.{domain}"), 465, Security::Tls),
                            username: account.email.clone(),
                        }
                    });
                let new = NewAccount {
                    label: setup::guess_label(&account.email, provider),
                    display_name: setup::guess_name(&account.email),
                    email: account.email,
                    settings,
                };
                if let Err(error) = registry.add(&new) {
                    tracing::warn!("could not take over an old account: {error}");
                }
            }
        }
        Err(error) => tracing::warn!("could not read the old mail storage: {error}"),
    }
    if let Err(error) = accounts::remove_store_files(&legacy) {
        tracing::warn!("could not remove the old mail storage: {error}");
    }
}

impl Window {
    pub fn new(app: &adw::Application) -> Rc<Window> {
        let data_dir = glib::user_data_dir().join("e-mail");
        let registry = open_registry(&data_dir).expect("could not open the local account list");
        let (events, event_receiver) = async_channel::unbounded();

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("E-Mail")
            .default_width(1100)
            .default_height(720)
            .width_request(360)
            .height_request(294)
            .build();
        let settings = settings();
        let settings_for_prefs = settings.clone();
        if let Some(settings) = &settings {
            window.set_default_size(settings.int("window-width"), settings.int("window-height"));
            if settings.boolean("window-maximized") {
                window.maximize();
            }
        }

        // Folder sidebar.
        let folders = FolderList::new();
        let switcher = build_account_switcher();
        let settings_button = gtk::Button::builder()
            .icon_name("emblem-system-symbolic")
            .action_name("app.settings")
            .tooltip_text(gettext("Settings (Ctrl+,)"))
            .build();
        settings_button.update_property(&[gtk::accessible::Property::Label(&gettext("Settings"))]);
        let sidebar_header = adw::HeaderBar::builder().show_title(false).build();
        sidebar_header.pack_end(&settings_button);
        let sidebar_view = adw::ToolbarView::new();
        sidebar_view.add_top_bar(&sidebar_header);
        sidebar_view.set_content(Some(&folders.widget));
        sidebar_view.add_bottom_bar(&switcher.button);

        // Message list.
        let messages = MessageList::new();
        let list_title = adw::WindowTitle::new("", "");
        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text(gettext("Folders"))
            .visible(false)
            .build();
        let list_header = adw::HeaderBar::builder().title_widget(&list_title).build();
        list_header.pack_start(&sidebar_toggle);
        let select_button = gtk::ToggleButton::builder()
            .icon_name(format!("{APP_ID}-select-symbolic"))
            .tooltip_text(gettext("Select conversations"))
            .build();
        select_button.update_property(&[gtk::accessible::Property::Label(&gettext("Select"))]);
        let compose_button = gtk::Button::builder()
            .icon_name(format!("{APP_ID}-compose-symbolic"))
            .tooltip_text(gettext("New Mail (Ctrl+N)"))
            .action_name("app.compose")
            .build();
        compose_button.update_property(&[gtk::accessible::Property::Label(&gettext("New Mail"))]);
        // From the left: search, select, new message.
        list_header.pack_end(&compose_button);
        list_header.pack_end(&select_button);
        let search_button = gtk::ToggleButton::builder()
            .icon_name("system-search-symbolic")
            .tooltip_text(gettext("Search (/)"))
            .build();
        search_button.update_property(&[gtk::accessible::Property::Label(&gettext("Search"))]);
        list_header.pack_end(&search_button);
        let search_entry = gtk::SearchEntry::builder()
            .hexpand(true)
            .search_delay(250)
            .build();
        // One line of small pills; scrolls sideways when the list is narrow.
        let filter_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let filter_scroller = gtk::ScrolledWindow::builder()
            .child(&filter_box)
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .build();
        let mut search_filters = Vec::new();
        for label in [
            gettext("Unread"),
            gettext("Starred"),
            gettext("With Attachment"),
            gettext("Only This Folder"),
        ] {
            let chip = gtk::ToggleButton::builder()
                .label(&label)
                .css_classes(["filter-chip"])
                .build();
            filter_box.append(&chip);
            search_filters.push(chip);
        }
        let search_status = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let search_spinner = gtk::Spinner::builder().visible(false).build();
        search_spinner.set_tooltip_text(Some(&gettext("Searching on the server …")));
        let status_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        status_line.append(&search_status);
        status_line.append(&search_spinner);
        let search_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_start(10)
            .margin_end(10)
            .margin_top(6)
            .margin_bottom(8)
            .build();
        search_box.append(&search_entry);
        search_box.append(&filter_scroller);
        search_box.append(&status_line);
        let search_bar = gtk::Revealer::builder()
            .child(&search_box)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .css_classes(["search-panel"])
            .build();
        let list_empty = adw::StatusPage::builder()
            .icon_name("mail-unread-symbolic")
            .title(gettext("Nothing Here"))
            .description(gettext("This folder is empty."))
            .build();
        list_empty.add_css_class("compact");
        let list_stack = gtk::Stack::new();
        list_stack.add_named(&messages.widget, Some("list"));
        list_stack.add_named(&list_empty, Some("empty"));
        let selection_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading"])
            .build();
        let selection_box = gtk::Box::builder()
            .spacing(4)
            .margin_start(10)
            .margin_end(6)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        selection_box.append(&selection_label);
        let mut selection_buttons = Vec::new();
        for (icon, tooltip) in [
            ("mail-read-symbolic", gettext("Mark as Read")),
            ("starred-symbolic", gettext("Star")),
            ("folder-symbolic", gettext("Move To…")),
            ("user-trash-symbolic", gettext("Delete")),
        ] {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(&tooltip)
                .css_classes(["flat"])
                .build();
            selection_box.append(&button);
            selection_buttons.push(button);
        }
        let selection_bar = gtk::Revealer::builder()
            .child(&selection_box)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .css_classes(["selection-bar"])
            .build();
        let list_view = adw::ToolbarView::new();
        list_view.add_top_bar(&list_header);
        list_view.add_top_bar(&search_bar);
        list_view.set_content(Some(&list_stack));
        list_view.add_bottom_bar(&selection_bar);
        // The list keeps its width when the window grows; only the reading
        // pane takes the extra room. The border between them can be dragged.
        let list_handle = gtk::Box::builder()
            .width_request(6)
            .halign(gtk::Align::Start)
            .css_classes(["pane-handle"])
            .tooltip_text(gettext("Drag to change the width of the list"))
            .build();
        list_handle.set_cursor_from_name(Some("col-resize"));
        let list_page = adw::NavigationPage::builder()
            .title(gettext("Messages"))
            .tag("list")
            .child(&list_view)
            .build();

        // Reading pane.
        let reader = Reader::new();
        let reader_header = adw::HeaderBar::builder()
            .title_widget(&reader.title)
            .build();
        reader_header.pack_end(&reader.menu_button);
        let reader_view = adw::ToolbarView::new();
        reader_view.add_top_bar(&reader_header);
        reader_view.set_content(Some(&reader.widget));
        let reader_overlay = gtk::Overlay::builder().child(&reader_view).build();
        reader_overlay.add_overlay(&list_handle);
        let reader_page = adw::NavigationPage::builder()
            .title(gettext("Message"))
            .tag("message")
            .child(&reader_overlay)
            .build();

        let inner = adw::NavigationSplitView::builder()
            .sidebar(&list_page)
            .content(&reader_page)
            .build();
        let list_width = f64::from(
            settings
                .as_ref()
                .map_or(LIST_WIDTH, |settings| settings.int("list-width"))
                .clamp(MIN_LIST_WIDTH, MAX_LIST_WIDTH),
        );
        inner.set_min_sidebar_width(list_width);
        inner.set_max_sidebar_width(list_width);
        inner
            .bind_property("collapsed", &list_handle, "visible")
            .invert_boolean()
            .sync_create()
            .build();
        let drag = gtk::GestureDrag::new();
        {
            let inner = inner.clone();
            let window = window.downgrade();
            let settings = settings.clone();
            let start = Rc::new(Cell::new(list_width));
            let begin = start.clone();
            let split = inner.clone();
            drag.connect_drag_begin(move |_, _, _| begin.set(split.max_sidebar_width()));
            let update_inner = inner.clone();
            let update_start = start.clone();
            drag.connect_drag_update(move |_, dx, _| {
                // Leave the reading pane some room.
                let Some(window) = window.upgrade() else {
                    return;
                };
                let widest = (f64::from(window.width()) - 560.0)
                    .clamp(f64::from(MIN_LIST_WIDTH), f64::from(MAX_LIST_WIDTH));
                let width = (update_start.get() + dx).clamp(f64::from(MIN_LIST_WIDTH), widest);
                update_inner.set_min_sidebar_width(width);
                update_inner.set_max_sidebar_width(width);
            });
            drag.connect_drag_end(move |_, _, _| {
                if let Some(settings) = &settings {
                    let _ =
                        settings.set_int("list-width", inner.max_sidebar_width().round() as i32);
                }
            });
        }
        list_handle.add_controller(drag);
        let outer = adw::OverlaySplitView::builder()
            .sidebar(&sidebar_view)
            .content(&inner)
            .min_sidebar_width(230.0)
            .max_sidebar_width(230.0)
            .build();
        outer
            .bind_property("collapsed", &sidebar_toggle, "visible")
            .sync_create()
            .build();
        outer
            .bind_property("show-sidebar", &sidebar_toggle, "active")
            .sync_create()
            .bidirectional()
            .build();

        let root = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        root.add_named(&outer, Some("main"));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&root));
        window.set_content(Some(&toasts));

        add_breakpoints(&window, &outer, &inner);

        let this = Rc::new(Window {
            window,
            root,
            toasts,
            setup_page: RefCell::default(),
            setup_dialog: RefCell::default(),
            settings_dialog: RefCell::default(),
            registry,
            data_dir,
            outer,
            inner,
            list_title,
            list_page,
            list_stack,
            list_empty,
            select_button,
            selection_bar,
            selection_label,
            selection_buttons,
            refresh_pending: Cell::new(false),
            folders,
            messages,
            reader,
            switcher,
            events,
            settings,
            state: RefCell::default(),
            reply: RefCell::default(),
            composers: RefCell::default(),
            search_button,
            search_bar,
            search_entry,
            search_filters,
            search_status,
            search_spinner,
            prefs: RefCell::new(Prefs::load(settings_for_prefs.as_ref())),
            notifications: RefCell::default(),
            network_up: Cell::new(true),
            network_restart: RefCell::default(),
            tray: RefCell::default(),
        });
        this.set_up_tray();
        this.connect_signals();
        this.load_accounts();

        let weak = Rc::downgrade(&this);
        glib::spawn_future_local(async move {
            while let Ok(event) = event_receiver.recv().await {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.handle_event(event);
            }
        });
        this
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.folders.connect_selected(move |id| {
            if let Some(this) = weak.upgrade() {
                this.select_folder(id);
            }
        });
        let weak = Rc::downgrade(self);
        self.messages.connect_open(move |id| {
            if let Some(this) = weak.upgrade() {
                this.open_thread(id);
            }
        });
        let weak = Rc::downgrade(self);
        self.messages.connect_selection_changed(move |count| {
            if let Some(this) = weak.upgrade() {
                this.selection_label
                    .set_label(&fmt(&gettext("{} selected"), &[&count.to_string()]));
                for button in &this.selection_buttons {
                    button.set_sensitive(count > 0);
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.messages.connect_request_selection(move || {
            if let Some(this) = weak.upgrade() {
                this.select_button.set_active(true);
            }
        });
        let weak = Rc::downgrade(self);
        self.select_button.connect_toggled(move |button| {
            if let Some(this) = weak.upgrade() {
                let on = button.is_active();
                this.messages.set_selection_mode(on);
                this.selection_bar.set_reveal_child(on);
            }
        });
        for (index, button) in self.selection_buttons.iter().enumerate() {
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.bulk_action(index);
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.reader.connect_action(move |action, message| {
            if let Some(this) = weak.upgrade() {
                this.card_action(action, message);
            }
        });
        let weak = Rc::downgrade(self);
        self.reader.connect_need_body(move |message| {
            if let Some(this) = weak.upgrade() {
                this.show_body(message);
            }
        });
        let weak = Rc::downgrade(self);
        self.reader.connect_images(move |message, choice| {
            if let Some(this) = weak.upgrade() {
                this.allow_images(message, choice);
            }
        });
        let weak = Rc::downgrade(self);
        self.reader
            .connect_attachment(move |message, attachment, open| {
                if let Some(this) = weak.upgrade() {
                    this.handle_attachment(message, attachment, open);
                }
            });
        self.add_conversation_actions();
        self.connect_search();
        self.add_shortcuts();
        self.add_row_actions();
        let weak = Rc::downgrade(self);
        self.folders.connect_drop(move |folder, threads| {
            if let Some(this) = weak.upgrade() {
                let name = this
                    .with_store(|store| store.folder(folder).ok().flatten())
                    .map(|folder| folder_label(&folder))
                    .unwrap_or_default();
                this.move_threads(&threads, folder, &fmt(&gettext("Moved to {}"), &[&name]));
                this.select_button.set_active(false);
            }
        });
        let weak = Rc::downgrade(self);
        self.messages.connect_menu(move |thread, widget, x, y| {
            if let Some(this) = weak.upgrade() {
                this.show_row_menu(thread, &widget, x, y);
            }
        });

        let weak = Rc::downgrade(self);
        self.switcher.list.connect_row_activated(move |_, row| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.switcher.popover.popdown();
            let account = this
                .state
                .borrow()
                .accounts
                .get(row.index() as usize)
                .map(|a| a.id);
            if let Some(id) = account {
                this.set_active(id);
            }
        });
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |window| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if let Some(settings) = &this.settings {
                let (width, height) = window.default_size();
                let _ = settings.set_int("window-width", width);
                let _ = settings.set_int("window-height", height);
                let _ = settings.set_boolean("window-maximized", window.is_maximized());
            }
            if this.prefs.borrow().run_in_background && this.has_accounts() {
                // Keep syncing and notifying; starting the app again shows
                // the window.
                this.leave_reply();
                window.set_visible(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |window| {
            if window.is_active()
                && let Some(this) = weak.upgrade()
                && let Some(account) = this.active()
            {
                this.clear_notice(account);
            }
        });
        self.watch_network();
    }

    /// The store of the account on screen. Only that account is shown.
    fn active_store(&self) -> Option<SharedStore> {
        let state = self.state.borrow();
        state.active.and_then(|id| state.stores.get(&id).cloned())
    }

    fn with_store<T: Default>(&self, f: impl FnOnce(&Store) -> T) -> T {
        match self.active_store() {
            Some(store) => f(&store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())),
            None => T::default(),
        }
    }

    fn open_store(&self, id: AccountId) -> Option<SharedStore> {
        let path = accounts::store_path(&self.data_dir, id);
        match Store::open(&path, id) {
            Ok(store) => {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                Some(Arc::new(Mutex::new(store)))
            }
            Err(error) => {
                tracing::error!("could not open the mail storage of account {id}: {error}");
                None
            }
        }
    }

    fn load_accounts(self: &Rc<Self>) {
        let accounts = self.registry.all().unwrap_or_else(|error| {
            tracing::error!("could not read accounts: {error}");
            Vec::new()
        });
        for account in &accounts {
            if let Some(store) = self.open_store(account.id) {
                self.state.borrow_mut().stores.insert(account.id, store);
            }
        }
        self.state.borrow_mut().accounts = accounts.clone();
        for account in &accounts {
            self.start_sync(account.clone(), None);
        }
        let remembered = self
            .settings
            .as_ref()
            .map(|settings| settings.int64("active-account"));
        let active = accounts
            .iter()
            .find(|account| Some(account.id) == remembered)
            .or(accounts.first())
            .map(|account| account.id);
        match active {
            Some(id) => self.set_active(id),
            None => self.show_setup_page(),
        }
    }

    fn start_sync(self: &Rc<Self>, account: Account, password: Option<String>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let password = match password {
                Some(password) => Some(password),
                None => match runtime::run(secrets::load_password(account.email.clone())).await {
                    Ok(password) => password,
                    Err(error) => {
                        tracing::warn!("keyring: {error}");
                        None
                    }
                },
            };
            let Some(this) = weak.upgrade() else {
                return;
            };
            let Some(store) = this.state.borrow().stores.get(&account.id).cloned() else {
                return;
            };
            let Some(password) = password else {
                this.handle_event(SyncEvent::Status {
                    account: account.id,
                    status: AccountStatus::LoginFailed(gettext(
                        "No password found in the system keyring.",
                    )),
                });
                return;
            };
            let transport = SmtpTransport {
                server: account.settings.smtp.clone(),
                username: account.username().to_owned(),
                password: password.clone(),
                trust: Trust::system(),
            };
            let connector = ImapConnector {
                server: account.settings.imap.clone(),
                username: account.username().to_owned(),
                password,
                trust: Trust::system(),
            };
            this.state
                .borrow_mut()
                .connectors
                .insert(account.id, connector.clone());
            let handle = start_account(
                &runtime::handle(),
                account.clone(),
                Arc::new(connector),
                Arc::new(transport),
                store,
                this.events.clone(),
            );
            this.state.borrow_mut().handles.insert(account.id, handle);
        });
    }

    fn restart_sync(self: &Rc<Self>, id: AccountId, password: Option<String>) {
        if let Some(store) = self.state.borrow().stores.get(&id)
            && let Err(error) = store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retry_outbox_now()
        {
            tracing::warn!("could not wake the outbox: {error}");
        }
        let account = {
            let mut state = self.state.borrow_mut();
            state.handles.remove(&id);
            state.statuses.remove(&id);
            state.accounts.iter().find(|a| a.id == id).cloned()
        };
        if let Some(account) = account {
            self.start_sync(account, password);
        }
        self.update_switcher();
    }

    fn setup_hooks(self: &Rc<Self>, switch_to_new: bool) -> Hooks {
        let weak = Rc::downgrade(self);
        let is_known = Box::new(move |email: &str| {
            weak.upgrade()
                .is_some_and(|this| this.registry.contains_email(email).unwrap_or(false))
        });
        let weak = Rc::downgrade(self);
        let add = Box::new(move |new: NewAccount, password: String| {
            let this = weak.upgrade().ok_or_else(String::new)?;
            this.add_checked_account(new, password)
        });
        let weak = Rc::downgrade(self);
        let open = Box::new(move |id: AccountId| {
            if let Some(this) = weak.upgrade() {
                if let Some(dialog) = this.setup_dialog.take() {
                    dialog.force_close();
                }
                if switch_to_new || this.root.visible_child_name().as_deref() == Some("setup") {
                    this.set_active(id);
                }
            }
        });
        Hooks {
            is_known,
            add,
            open,
        }
    }

    /// Fills the window with the setup, for the first start without accounts.
    fn show_setup_page(self: &Rc<Self>) {
        let existing = self.setup_page.borrow().clone();
        let flow = match existing {
            Some(flow) => {
                flow.reset();
                flow
            }
            None => {
                let flow = SetupFlow::new(self.setup_hooks(true));
                self.root.add_named(&flow.widget, Some("setup"));
                *self.setup_page.borrow_mut() = Some(flow.clone());
                flow
            }
        };
        drop(flow);
        self.root.set_visible_child_name("setup");
    }

    /// Opens the setup as a dialog above the app. Accounts added from the
    /// settings do not become the active one, as there the user is busy
    /// with the list of accounts.
    pub fn add_account(self: &Rc<Self>) {
        if self.root.visible_child_name().as_deref() == Some("setup") {
            return;
        }
        if let Some(dialog) = self.setup_dialog.borrow().as_ref() {
            dialog.present(Some(&self.window));
            return;
        }
        let from_settings = self.settings_dialog.borrow().upgrade().is_some();
        let flow = SetupFlow::new(self.setup_hooks(!from_settings));
        let dialog = adw::Dialog::builder()
            .title(gettext("Add Account"))
            .content_width(860)
            .content_height(620)
            .child(&flow.widget)
            .build();
        let weak = Rc::downgrade(self);
        dialog.connect_closed(move |_| {
            // Keep the flow alive while the dialog is open.
            let _ = &flow;
            if let Some(this) = weak.upgrade() {
                this.setup_dialog.take();
            }
        });
        dialog.present(Some(&self.window));
        *self.setup_dialog.borrow_mut() = Some(dialog);
    }

    fn add_checked_account(
        self: &Rc<Self>,
        new: NewAccount,
        password: String,
    ) -> Result<AccountId, String> {
        let account = self.registry.add(&new).map_err(|error| {
            tracing::error!("could not save account: {error}");
            gettext("The account could not be saved. Please try again.")
        })?;
        let Some(store) = self.open_store(account.id) else {
            let _ = self.registry.remove(account.id);
            return Err(gettext(
                "The local storage for this account could not be created.",
            ));
        };
        {
            let mut state = self.state.borrow_mut();
            state.stores.insert(account.id, store);
            state.accounts.push(account.clone());
        }
        self.start_sync(account.clone(), Some(password));
        if self.state.borrow().active.is_none() {
            self.set_active(account.id);
            // The first-start page stays until "Open Inbox".
            self.root.set_visible_child_name("setup");
        }
        self.update_switcher();
        if let Some(settings) = self.settings_dialog.borrow().upgrade() {
            settings.refresh();
        }
        Ok(account.id)
    }

    pub fn show_settings(self: &Rc<Self>) {
        if self.state.borrow().accounts.is_empty() {
            return;
        }
        if let Some(settings) = self.settings_dialog.borrow().upgrade() {
            settings.dialog.present(Some(&self.window));
            return;
        }
        let settings = Settings::new(self);
        settings.dialog.present(Some(&self.window));
        *self.settings_dialog.borrow_mut() = Rc::downgrade(&settings);
        let keep = RefCell::new(Some(settings.clone()));
        settings.dialog.connect_closed(move |_| {
            keep.take();
        });
    }

    pub fn toast(&self, text: &str) {
        let toast = adw::Toast::new(text);
        match self.settings_dialog.borrow().upgrade() {
            Some(settings) => settings.dialog.add_toast(toast),
            None => self.toasts.add_toast(toast),
        }
    }

    /// A short message with "Undo" that runs `undo`.
    pub fn toast_with_undo(&self, text: &str, undo: Box<dyn Fn()>) {
        let toast = adw::Toast::builder()
            .title(text)
            .button_label(gettext("Undo"))
            .build();
        toast.connect_button_clicked(move |_| undo());
        self.toasts.add_toast(toast);
    }

    pub fn accounts(&self) -> Vec<Account> {
        self.state.borrow().accounts.clone()
    }

    pub fn account(&self, id: AccountId) -> Option<Account> {
        self.state
            .borrow()
            .accounts
            .iter()
            .find(|account| account.id == id)
            .cloned()
    }

    pub fn active(&self) -> Option<AccountId> {
        self.state.borrow().active
    }

    pub fn status(&self, id: AccountId) -> Option<AccountStatus> {
        self.state.borrow().statuses.get(&id).cloned()
    }

    pub fn last_sync(&self, id: AccountId) -> Option<DateTime<Local>> {
        self.state.borrow().last_sync.get(&id).copied()
    }

    fn update_account(self: &Rc<Self>, id: AccountId, change: impl FnOnce(&mut Account)) {
        if let Some(account) = self
            .state
            .borrow_mut()
            .accounts
            .iter_mut()
            .find(|account| account.id == id)
        {
            change(account);
        }
        self.update_switcher();
        if self.state.borrow().active == Some(id)
            && let Some(folder) = self.state.borrow().folder
        {
            self.update_list_title(Some(folder));
        }
    }

    pub fn set_label(self: &Rc<Self>, id: AccountId, label: &str) {
        if let Err(error) = self.registry.set_label(id, label) {
            tracing::error!("could not rename account: {error}");
            return;
        }
        let label = label.to_owned();
        self.update_account(id, |account| account.label = label);
    }

    pub fn set_display_name(self: &Rc<Self>, id: AccountId, name: &str) {
        if let Err(error) = self.registry.set_display_name(id, name) {
            tracing::error!("could not change the name: {error}");
            return;
        }
        let name = name.to_owned();
        self.update_account(id, |account| account.display_name = name);
    }

    /// Checks the new password right away and keeps it only if it works.
    pub async fn change_password(
        self: Rc<Self>,
        id: AccountId,
        password: String,
    ) -> Result<(), String> {
        let account = self.account(id).ok_or_else(String::new)?;
        let settings = account.settings.clone();
        let check_password = password.clone();
        let checked = runtime::run(async move {
            check::check_imap_login(&settings, &check_password, &Trust::system()).await
        })
        .await;
        if let Err(error) = checked {
            return Err(setup::advice_message(
                &error,
                providers::provider_for_address(&account.email),
                &account.settings,
            ));
        }
        runtime::run(secrets::save_password(
            account.email.clone(),
            password.clone(),
        ))
        .await
        .map_err(|error| {
            tracing::warn!("keyring: {error}");
            gettext("The password could not be saved in the system keyring.")
        })?;
        self.restart_sync(id, Some(password));
        Ok(())
    }

    /// Checks changed server settings for real before saving them.
    pub async fn change_settings(
        self: Rc<Self>,
        id: AccountId,
        settings: ServerSettings,
    ) -> Result<(), String> {
        let account = self.account(id).ok_or_else(String::new)?;
        let password = runtime::run(secrets::load_password(account.email.clone()))
            .await
            .ok()
            .flatten()
            .ok_or_else(|| gettext("No password found in the system keyring."))?;
        let check_settings = settings.clone();
        let check_password = password.clone();
        let checked = runtime::run(async move {
            check::check_account(&check_settings, &check_password, &Trust::system(), |_| {}).await
        })
        .await;
        if let Err(error) = checked {
            return Err(setup::advice_message(
                &error,
                providers::provider_for_address(&account.email),
                &settings,
            ));
        }
        self.registry.set_settings(id, &settings).map_err(|error| {
            tracing::error!("could not save server settings: {error}");
            gettext("The settings could not be saved. Please try again.")
        })?;
        self.update_account(id, |account| account.settings = settings);
        self.restart_sync(id, Some(password));
        Ok(())
    }

    /// Forgets the account: stops its sync, deletes its local data and
    /// removes its password from the keyring. Mails stay at the provider.
    pub fn remove_account(self: &Rc<Self>, id: AccountId) {
        let Some(account) = self.account(id) else {
            return;
        };
        self.clear_notice(id);
        {
            let mut prefs = self.prefs.borrow_mut();
            if prefs.notify.muted.remove(&id) {
                prefs.save(self.settings.as_ref());
            }
        }
        {
            let mut state = self.state.borrow_mut();
            state.handles.remove(&id);
            state.stores.remove(&id);
            state.statuses.remove(&id);
            state.last_sync.remove(&id);
            state.accounts.retain(|account| account.id != id);
        }
        if let Err(error) = self.registry.remove(id) {
            tracing::error!("could not remove account {id}: {error}");
        }
        let _ = std::fs::remove_dir_all(files::attachments_dir(&cache_dir()).join(id.to_string()));
        if let Err(error) = accounts::remove_store_files(&accounts::store_path(&self.data_dir, id))
        {
            tracing::error!("could not delete the data of account {id}: {error}");
        }
        let email = account.email.clone();
        glib::spawn_future_local(async move {
            if let Err(error) = runtime::run(secrets::delete_password(email)).await {
                tracing::warn!("keyring: {error}");
            }
        });

        let first = self.state.borrow().accounts.first().map(|a| a.id);
        match first {
            None => {
                self.state.borrow_mut().active = None;
                if let Some(settings) = self.settings_dialog.borrow().upgrade() {
                    settings.dialog.force_close();
                }
                self.show_setup_page();
            }
            Some(first) => {
                if self.state.borrow().active == Some(id) {
                    self.set_active(first);
                } else {
                    self.update_switcher();
                }
                if let Some(settings) = self.settings_dialog.borrow().upgrade() {
                    settings.refresh();
                }
            }
        }
        self.toast(&fmt(&gettext("Account “{}” removed"), &[&account.label]));
    }

    fn send(&self, account: AccountId, command: Command) {
        if let Some(handle) = self.state.borrow().handles.get(&account) {
            handle.send(command);
        }
    }

    pub fn set_active(self: &Rc<Self>, id: AccountId) {
        if self.window.is_active() {
            self.clear_notice(id);
        }
        self.leave_reply();
        self.close_search();
        {
            let mut state = self.state.borrow_mut();
            state.active = Some(id);
            state.folder = None;
            state.thread = None;
            state.images_once.clear();
        }
        self.select_button.set_active(false);
        if let Some(settings) = &self.settings {
            let _ = settings.set_int64("active-account", id);
        }
        self.root.set_visible_child_name("main");
        self.update_switcher();
        self.reader.show_empty();
        self.inner.set_show_content(false);
        self.reload_folders();
    }

    fn inbox_unread(&self, id: AccountId) -> u32 {
        let store = self.state.borrow().stores.get(&id).cloned();
        store.map_or(0, |store| {
            store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .inbox_unread()
                .unwrap_or(0)
        })
    }

    fn update_switcher(&self) {
        let (accounts, active, statuses) = {
            let state = self.state.borrow();
            (state.accounts.clone(), state.active, state.statuses.clone())
        };
        let Some(current) = accounts.iter().find(|account| Some(account.id) == active) else {
            return;
        };
        while let Some(child) = self.switcher.avatar.first_child() {
            self.switcher.avatar.remove(&child);
        }
        self.switcher.avatar.append(&widgets::account_avatar(
            &current.label,
            32,
            statuses.get(&current.id),
        ));
        let others: u32 = accounts
            .iter()
            .filter(|account| Some(account.id) != active)
            .map(|account| self.inbox_unread(account.id))
            .sum();
        self.switcher.others_unread.set_visible(others > 0);
        self.update_tray();
        self.switcher.title.set_label(&current.label);
        self.switcher.subtitle.set_label(&current.email);
        self.switcher.button.set_tooltip_text(Some(&fmt(
            &gettext("Switch account · {}"),
            &[&widgets::status_text(statuses.get(&current.id))],
        )));

        self.switcher.list.remove_all();
        for account in &accounts {
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_opacity(if Some(account.id) == active { 1.0 } else { 0.0 });
            let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
            text.set_hexpand(true);
            text.append(
                &gtk::Label::builder()
                    .label(&account.label)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["heading"])
                    .build(),
            );
            text.append(
                &gtk::Label::builder()
                    .label(&account.email)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["dim-label", "caption"])
                    .build(),
            );
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.append(&check);
            row.append(&text);
            let unread = self.inbox_unread(account.id);
            if unread > 0 {
                row.append(
                    &gtk::Label::builder()
                        .label(unread.to_string())
                        .valign(gtk::Align::Center)
                        .css_classes(["count-pill", "unread"])
                        .build(),
                );
            }
            let list_row = gtk::ListBoxRow::builder().child(&row).build();
            list_row.update_property(&[gtk::accessible::Property::Label(&fmt(
                &gettext("{}, {} unread"),
                &[&account.label, &unread.to_string()],
            ))]);
            self.switcher.list.append(&list_row);
        }
    }

    fn reload_folders(self: &Rc<Self>) {
        let folders = self.with_store(|store| store.folders().unwrap_or_default());
        let selected = self.state.borrow().folder;
        let selected = selected
            .filter(|id| folders.iter().any(|folder| folder.id == *id))
            .or(folders.first().map(|folder| folder.id));
        self.folders.set_folders(&folders, selected);
        match selected {
            None => {
                self.state.borrow_mut().folder = None;
                self.update_list_title(None);
                self.messages.set_threads(Vec::new(), None);
                self.list_stack.set_visible_child_name("empty");
            }
            Some(id) if Some(id) != self.state.borrow().folder => self.select_folder(id),
            Some(id) => self.update_list_title(Some(id)),
        }
    }

    fn update_list_title(&self, folder: Option<FolderId>) {
        let folder = folder.and_then(|id| self.with_store(|store| store.folder(id).ok().flatten()));
        let label = folder
            .as_ref()
            .map_or_else(|| gettext("Inbox"), folder_label);
        self.list_title.set_title(&label);
        self.list_page.set_title(&label);
        let state = self.state.borrow();
        let label = state
            .accounts
            .iter()
            .find(|account| Some(account.id) == state.active)
            .map(|account| account.label.clone())
            .unwrap_or_default();
        self.list_title.set_subtitle(&label);
        let role = folder.as_ref().and_then(|folder| folder.role);
        let (title, description) = match role {
            Some(FolderRole::Inbox) => (gettext("No Mail"), gettext("New mail appears here.")),
            Some(FolderRole::Drafts) => (
                gettext("No Drafts"),
                gettext("Mail you have not sent yet is kept here."),
            ),
            Some(FolderRole::Sent) => (gettext("Nothing Sent Yet"), String::new()),
            Some(FolderRole::Archive) => (gettext("Archive Is Empty"), String::new()),
            Some(FolderRole::Junk) => (
                gettext("No Spam"),
                gettext("The server has not sorted anything out."),
            ),
            Some(FolderRole::Trash) => (gettext("Trash Is Empty"), String::new()),
            None => (gettext("Nothing Here"), gettext("This folder is empty.")),
        };
        self.list_empty
            .set_icon_name(Some(crate::folder_list::role_icon(role)));
        self.list_empty.set_title(&title);
        self.list_empty
            .set_description((!description.is_empty()).then_some(description.as_str()));
    }

    fn select_folder(self: &Rc<Self>, id: FolderId) {
        self.leave_reply();
        self.close_search();
        {
            let mut state = self.state.borrow_mut();
            state.folder = Some(id);
            state.thread = None;
        }
        self.select_button.set_active(false);
        self.update_list_title(Some(id));
        self.reload_threads();
        self.reader.show_empty();
        self.update_conversation_actions();
        self.inner.set_show_content(false);
        if self.outer.is_collapsed() {
            self.outer.set_show_sidebar(false);
        }
        if let Some(account) = self.state.borrow().active {
            self.send(account, Command::SyncFolder(id));
        }
    }

    fn current_folder(&self) -> Option<Folder> {
        let id = self.state.borrow().folder?;
        self.with_store(|store| store.folder(id).ok().flatten())
    }

    fn own_name(&self) -> String {
        let state = self.state.borrow();
        state
            .accounts
            .iter()
            .find(|account| Some(account.id) == state.active)
            .map(|account| account.display_name.clone())
            .unwrap_or_default()
    }

    fn own_address(&self) -> String {
        let state = self.state.borrow();
        state
            .accounts
            .iter()
            .find(|account| Some(account.id) == state.active)
            .map(|account| account.email.clone())
            .unwrap_or_default()
    }

    fn reload_threads(&self) {
        let (folder, thread) = {
            let state = self.state.borrow();
            (state.folder, state.thread)
        };
        let Some(folder) = folder else {
            return;
        };
        if self.search_active() {
            self.show_search_results();
            return;
        }
        let drafts = self
            .current_folder()
            .is_some_and(|folder| folder.role == Some(FolderRole::Drafts));
        self.messages.set_context(ListContext {
            own_address: self.own_address(),
            drafts,
            ..ListContext::default()
        });
        let grouped = self.prefs.borrow().group_conversations;
        let threads = self.with_store(|store| {
            if grouped {
                store.threads(folder)
            } else {
                store.single_messages(folder)
            }
            .unwrap_or_else(|error| {
                tracing::warn!("could not load the conversations of folder {folder}: {error}");
                Vec::new()
            })
        });
        self.messages.set_threads(threads, thread);
        self.list_stack
            .set_visible_child_name(if self.messages.is_empty() {
                "empty"
            } else {
                "list"
            });
    }

    /// Asks before removing every message of trash or spam for good.
    fn confirm_empty_folder(self: &Rc<Self>, folder: Folder) {
        let (heading, button) = match folder.role {
            Some(FolderRole::Trash) => (gettext("Empty Trash?"), gettext("Empty Trash")),
            Some(FolderRole::Junk) => (gettext("Empty Spam?"), gettext("Empty Spam")),
            _ => return,
        };
        let count = self
            .with_store(|store| store.visible_ids(folder.id).ok())
            .map_or(0, |ids| ids.len());
        if count == 0 {
            return;
        }
        let body = if count == 1 {
            gettext("1 message will be deleted for good. This cannot be undone.")
        } else {
            fmt(
                &gettext("{} messages will be deleted for good. This cannot be undone."),
                &[&count.to_string()],
            )
        };
        let dialog = adw::AlertDialog::builder()
            .heading(heading)
            .body(body)
            .close_response("cancel")
            .default_response("cancel")
            .build();
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("empty", &button);
        dialog.set_response_appearance("empty", adw::ResponseAppearance::Destructive);
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response == "empty"
                && let Some(this) = weak.upgrade()
            {
                this.empty_folder(folder.id);
            }
        });
        dialog.present(Some(&self.window));
    }

    fn empty_folder(self: &Rc<Self>, folder: FolderId) {
        let Some(account) = self.active() else {
            return;
        };
        let Some(store) = self.active_store() else {
            return;
        };
        let removed = {
            let mut store = store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store
                .visible_ids(folder)
                .and_then(|ids| store.delete_for_good(&ids))
                .unwrap_or_else(|error| {
                    tracing::error!("could not empty the folder: {error}");
                    0
                })
        };
        if removed > 0 {
            self.send(account, Command::Flush);
            self.schedule_refresh();
            self.toast(&if removed == 1 {
                gettext("1 message deleted for good")
            } else {
                fmt(
                    &gettext("{} messages deleted for good"),
                    &[&removed.to_string()],
                )
            });
        }
    }

    /// Reloads what is on screen after a change, at most once per moment
    /// even if many changes arrive together.
    fn schedule_refresh(self: &Rc<Self>) {
        if self.refresh_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
            if let Some(this) = weak.upgrade() {
                this.refresh_pending.set(false);
                this.refresh_now();
            }
        });
    }

    fn refresh_now(self: &Rc<Self>) {
        self.reload_threads();
        self.refresh_conversation();
        self.reload_folders();
        self.update_switcher();
    }

    fn open_thread(self: &Rc<Self>, id: ThreadId) {
        if self.messages.selection_mode() {
            return;
        }
        let Some(folder) = self.thread_folder(id) else {
            return;
        };
        if self
            .with_store(|store| store.folder(folder).ok().flatten())
            .is_some_and(|folder| folder.role == Some(FolderRole::Drafts))
        {
            self.open_draft_thread(id, folder);
            return;
        }
        if self.state.borrow().thread != Some(id) {
            self.leave_reply();
        }
        self.state.borrow_mut().thread = Some(id);
        let unread: Vec<MessageId> = self.with_store(|store| {
            store
                .conversation(id, folder)
                .unwrap_or_default()
                .into_iter()
                .filter(|message| !message.flags.seen)
                .map(|message| message.id)
                .collect()
        });
        if !unread.is_empty() {
            self.change(|store| store.set_seen(&unread, true));
        }
        self.refresh_conversation();
        self.inner.set_show_content(true);
    }

    fn refresh_conversation(self: &Rc<Self>) {
        let thread = self.state.borrow().thread;
        let folder = thread.and_then(|thread| self.thread_folder(thread));
        let (Some(folder), Some(thread)) = (folder, thread) else {
            self.update_conversation_actions();
            return;
        };
        let messages =
            self.with_store(|store| store.conversation(thread, folder).unwrap_or_default());
        if messages.is_empty() {
            self.leave_reply();
            self.state.borrow_mut().thread = None;
            self.reader.show_empty();
            self.messages.unselect();
            self.inner.set_show_content(false);
        } else {
            let label = self
                .with_store(|store| store.folder(folder).ok().flatten())
                .map(|folder| folder_label(&folder))
                .unwrap_or_default();
            let subject = messages[0].subject.clone();
            self.reader.show_conversation(
                thread,
                &subject,
                &label,
                messages,
                &self.own_address(),
                &self.own_name(),
            );
        }
        self.update_conversation_actions();
    }

    /// Applies a change to the store of the active account, sends it on to
    /// the server and updates the screen.
    fn change<T: Default>(
        self: &Rc<Self>,
        f: impl FnOnce(&mut Store) -> e_mail_core::store::StoreResult<T>,
    ) -> T {
        let Some(store) = self.active_store() else {
            return T::default();
        };
        let result = f(&mut store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()));
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                tracing::error!("could not save a change: {error}");
                T::default()
            }
        };
        if let Some(account) = self.state.borrow().active {
            self.send(account, Command::Flush);
        }
        self.schedule_refresh();
        value
    }

    fn undo_due() -> i64 {
        chrono::Utc::now().timestamp() + UNDO_SECONDS
    }

    fn undo_toast(self: &Rc<Self>, text: &str, changes: Vec<ChangeId>) {
        let toast = adw::Toast::builder().title(text).timeout(5).build();
        if !changes.is_empty() {
            toast.set_button_label(Some(&gettext("Undo")));
            let weak = Rc::downgrade(self);
            toast.connect_button_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    let changes = changes.clone();
                    this.change(move |store| store.cancel_changes(&changes));
                }
            });
        }
        self.toasts.add_toast(toast);
    }

    /// The messages a conversation shows from the current folder.
    fn conversation_ids(&self, thread: ThreadId) -> Vec<(MessageId, bool, bool)> {
        let Some(folder) = self.thread_folder(thread) else {
            return Vec::new();
        };
        self.with_store(|store| {
            store
                .conversation(thread, folder)
                .unwrap_or_default()
                .into_iter()
                .map(|m| (m.id, m.flags.seen, m.flags.flagged))
                .collect()
        })
    }

    fn thread_action(self: &Rc<Self>, action: RowAction, thread: ThreadId) {
        let messages = self.conversation_ids(thread);
        let Some(&(latest, ..)) = messages.last() else {
            return;
        };
        let ids: Vec<MessageId> = messages.iter().map(|(id, ..)| *id).collect();
        match action {
            RowAction::ToggleRead => {
                if messages.iter().any(|(_, seen, _)| !seen) {
                    self.change(|store| store.set_seen(&ids, true));
                    self.undo_toast(&gettext("Marked as read"), Vec::new());
                } else {
                    self.change(|store| store.set_seen(&[latest], false));
                    self.undo_toast(&gettext("Marked as unread"), Vec::new());
                    if self.state.borrow().thread == Some(thread) && self.inner.is_collapsed() {
                        self.inner.set_show_content(false);
                    }
                }
            }
            RowAction::ToggleStar => {
                if messages.iter().any(|(.., flagged)| *flagged) {
                    self.change(|store| store.set_flagged(&ids, false));
                } else {
                    self.change(|store| store.set_flagged(&[latest], true));
                }
            }
            RowAction::Delete => self.delete_threads(&[thread]),
        }
    }

    fn messages_in_folder(&self, threads: &[ThreadId]) -> Vec<MessageId> {
        let folders: Vec<(ThreadId, FolderId)> = threads
            .iter()
            .filter_map(|thread| Some((*thread, self.thread_folder(*thread)?)))
            .collect();
        self.with_store(|store| {
            folders
                .iter()
                .flat_map(|(thread, folder)| {
                    store
                        .thread_messages_in(*thread, *folder)
                        .unwrap_or_default()
                })
                .collect()
        })
    }

    /// The folder a conversation is shown from: where the search found it,
    /// otherwise the selected folder.
    fn thread_folder(&self, thread: ThreadId) -> Option<FolderId> {
        let state = self.state.borrow();
        state
            .search
            .as_ref()
            .and_then(|search| search.folders.get(&thread).copied())
            .or(state.folder)
    }

    fn delete_threads(self: &Rc<Self>, threads: &[ThreadId]) {
        let ids = self.messages_in_folder(threads);
        let deletion = self.change(|store| store.delete_messages(&ids, Window::undo_due()));
        if !deletion.undo.is_empty() {
            let text = if threads.len() == 1 {
                gettext("Conversation deleted")
            } else {
                fmt(
                    &gettext("{} conversations deleted"),
                    &[&threads.len().to_string()],
                )
            };
            self.undo_toast(&text, deletion.undo);
        } else if deletion.permanent > 0 {
            self.undo_toast(&gettext("Permanently deleted"), Vec::new());
        }
    }

    fn move_threads(self: &Rc<Self>, threads: &[ThreadId], target: FolderId, text: &str) {
        let ids = self.messages_in_folder(threads);
        let changes = self.change(|store| store.move_messages(&ids, target, Window::undo_due()));
        if !changes.is_empty() {
            self.undo_toast(text, changes);
        }
    }

    fn choose_target(self: &Rc<Self>, threads: Vec<ThreadId>) {
        let folders = self.with_store(|store| store.folders().unwrap_or_default());
        let current = self.state.borrow().folder;
        let weak = Rc::downgrade(self);
        dialogs::choose_folder(&self.window, &folders, current, move |target| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            let name = this
                .with_store(|store| store.folder(target).ok().flatten())
                .map(|folder| folder_label(&folder))
                .unwrap_or_default();
            this.move_threads(&threads, target, &fmt(&gettext("Moved to {}"), &[&name]));
            this.select_button.set_active(false);
        });
    }

    fn bulk_action(self: &Rc<Self>, index: usize) {
        let threads = self.messages.selected_threads();
        if threads.is_empty() {
            return;
        }
        match index {
            0 => {
                let ids: Vec<MessageId> = threads
                    .iter()
                    .flat_map(|thread| self.conversation_ids(*thread))
                    .map(|(id, ..)| id)
                    .collect();
                self.change(|store| store.set_seen(&ids, true));
                self.undo_toast(
                    &fmt(&gettext("{} updated"), &[&threads.len().to_string()]),
                    Vec::new(),
                );
            }
            1 => {
                let latest: Vec<MessageId> = threads
                    .iter()
                    .filter_map(|thread| self.conversation_ids(*thread).last().map(|m| m.0))
                    .collect();
                self.change(|store| store.set_flagged(&latest, true));
                self.undo_toast(
                    &fmt(&gettext("{} updated"), &[&threads.len().to_string()]),
                    Vec::new(),
                );
            }
            2 => {
                self.choose_target(threads);
                return;
            }
            _ => self.delete_threads(&threads),
        }
        self.select_button.set_active(false);
    }

    fn card_action(self: &Rc<Self>, action: CardAction, id: MessageId) {
        let Some(message) = self.with_store(|store| store.message(id).ok().flatten()) else {
            return;
        };
        match action {
            CardAction::Reply => self.open_reply(id, ReplyKind::Reply),
            CardAction::ReplyAll => self.open_reply(id, ReplyKind::ReplyAll),
            CardAction::Forward => self.open_reply(id, ReplyKind::Forward),
            CardAction::ToggleStar => {
                self.change(|store| store.set_flagged(&[id], !message.flags.flagged));
            }
            CardAction::ToggleRead => {
                let seen = !message.flags.seen;
                self.change(|store| store.set_seen(&[id], seen));
                self.undo_toast(
                    &if seen {
                        gettext("Message marked as read")
                    } else {
                        gettext("Message marked as unread")
                    },
                    Vec::new(),
                );
            }
            CardAction::Delete => {
                if self.reader.messages().len() <= 1 {
                    let thread = self.reader.thread().unwrap_or(message.thread_id);
                    self.delete_threads(&[thread]);
                    return;
                }
                let deletion =
                    self.change(|store| store.delete_messages(&[id], Window::undo_due()));
                if !deletion.undo.is_empty() {
                    self.undo_toast(&gettext("Message deleted"), deletion.undo);
                } else if deletion.permanent > 0 {
                    self.undo_toast(&gettext("Permanently deleted"), Vec::new());
                }
            }
        }
    }

    fn add_conversation_actions(self: &Rc<Self>) {
        let actions = gio::SimpleActionGroup::new();
        let entries: [(&str, ConversationHandler); 4] = [
            ("move", Window::move_conversation),
            ("spam", Window::spam_conversation),
            ("source", Window::show_source),
            ("print", Window::print_conversation),
        ];
        for (name, handler) in entries {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    handler(&this);
                }
            });
            actions.add_action(&action);
        }
        self.window
            .insert_action_group("conversation", Some(&actions));
        self.update_conversation_actions();
    }

    fn update_conversation_actions(&self) {
        let junk = self
            .current_folder()
            .is_some_and(|folder| folder.role == Some(FolderRole::Junk));
        let menu = gio::Menu::new();
        let first = gio::Menu::new();
        first.append(Some(&gettext("Move To…")), Some("conversation.move"));
        first.append(
            Some(&if junk {
                gettext("Not Spam")
            } else {
                gettext("Mark as Spam")
            }),
            Some("conversation.spam"),
        );
        menu.append_section(None, &first);
        let second = gio::Menu::new();
        second.append(Some(&gettext("Show Source")), Some("conversation.source"));
        second.append(Some(&gettext("Print…")), Some("conversation.print"));
        menu.append_section(None, &second);
        self.reader.menu_button.set_menu_model(Some(&menu));
    }

    fn move_conversation(self: &Rc<Self>) {
        if let Some(thread) = self.reader.thread() {
            self.choose_target(vec![thread]);
        }
    }

    fn spam_conversation(self: &Rc<Self>) {
        if let Some(thread) = self.reader.thread() {
            self.spam_threads(&[thread]);
        }
    }

    /// Marks conversations as spam, or in the spam folder as not spam.
    fn spam_threads(self: &Rc<Self>, threads: &[ThreadId]) {
        let Some(&thread) = threads.first() else {
            return;
        };
        let junk = self
            .thread_folder(thread)
            .and_then(|id| self.with_store(|store| store.folder(id).ok().flatten()))
            .is_some_and(|folder| folder.role == Some(FolderRole::Junk));
        let (role, text) = if junk {
            (FolderRole::Inbox, gettext("Moved to the inbox"))
        } else {
            (
                FolderRole::Junk,
                gettext("Marked as spam. New mail from this sender goes to spam."),
            )
        };
        let Some(target) = self.with_store(|store| store.folder_by_role(role).ok().flatten())
        else {
            self.toast(&gettext("This account has no spam folder."));
            return;
        };
        // Senders marked as spam are remembered; their new mail goes to the
        // spam folder by itself. "Not spam" trusts them again.
        let own = self.own_address().to_lowercase();
        let senders: Vec<String> = {
            let ids = self.messages_in_folder(threads);
            self.with_store(|store| {
                ids.iter()
                    .filter_map(|id| store.message(*id).ok().flatten())
                    .map(|m| m.from_address.to_lowercase())
                    .filter(|address| !address.is_empty() && *address != own)
                    .collect()
            })
        };
        let remember = |store: &Store, spam: bool| {
            let result = if spam {
                store.mark_spam_senders(&senders)
            } else {
                store.unmark_spam_senders(&senders)
            };
            if let Err(error) = result {
                tracing::error!("could not remember the spam senders: {error}");
            }
        };
        self.with_store(|store| remember(store, !junk));
        let ids = self.messages_in_folder(&[thread]);
        let changes = self.change(|store| store.move_messages(&ids, target.id, Window::undo_due()));
        let toast = adw::Toast::builder().title(&text).timeout(5).build();
        if !changes.is_empty() {
            toast.set_button_label(Some(&gettext("Undo")));
            let weak = Rc::downgrade(self);
            let senders = senders.clone();
            toast.connect_button_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.with_store(|store| {
                        let result = if junk {
                            store.mark_spam_senders(&senders)
                        } else {
                            store.unmark_spam_senders(&senders)
                        };
                        if let Err(error) = result {
                            tracing::error!("could not restore the spam senders: {error}");
                        }
                    });
                    let changes = changes.clone();
                    this.change(move |store| store.cancel_changes(&changes));
                }
            });
        }
        self.toasts.add_toast(toast);
    }

    fn show_source(self: &Rc<Self>) {
        let Some(newest) = self.reader.messages().last().map(|m| m.id) else {
            return;
        };
        match self.with_store(|store| store.body(newest).ok().flatten()) {
            Some(raw) => dialogs::show_source(&self.window, &raw),
            None => self.toast(&gettext("The message is still loading.")),
        }
    }

    fn print_conversation(self: &Rc<Self>) {
        let messages = self.reader.messages();
        if messages.is_empty() {
            return;
        }
        let own = self.own_address();
        let me = gettext("Me");
        let items: Vec<PrintItem> = messages
            .iter()
            .map(|message| {
                let from = if message.from_address.eq_ignore_ascii_case(&own) {
                    me.clone()
                } else if message.from_name.is_empty() {
                    message.from_address.clone()
                } else {
                    format!("{} <{}>", message.from_name, message.from_address)
                };
                (
                    message.subject.clone(),
                    from,
                    message.recipients.clone(),
                    format_long_time(message.date),
                    self.with_store(|store| store.body(message.id).ok().flatten()),
                )
            })
            .collect();
        let labels = [
            gettext("To:"),
            gettext("This message has not been loaded yet."),
        ];
        let window = self.window.clone();
        glib::spawn_future_local(async move {
            let document = gio::spawn_blocking(move || {
                let parsed: Vec<Option<parse::ParsedMessage>> = items
                    .iter()
                    .map(|item| item.4.as_deref().map(parse::parse_message))
                    .collect();
                let empty = parse::Body::Empty;
                let prints: Vec<html::PrintMessage<'_>> = items
                    .iter()
                    .zip(&parsed)
                    .map(|(item, parsed)| html::PrintMessage {
                        subject: &item.0,
                        from: &item.1,
                        to: &item.2,
                        date: &item.3,
                        body: parsed.as_ref().map_or(&empty, |p| &p.body),
                        inline_images: parsed.as_ref().map_or(&[][..], |p| &p.inline_images),
                    })
                    .collect();
                html::print_document(&prints, [&labels[0], &labels[1]])
            })
            .await;
            match document {
                Ok(document) => dialogs::print(window.upcast_ref(), &document),
                Err(_) => tracing::error!("preparing the print document failed"),
            }
        });
    }

    /// Shows the body of an open message, loading it from the server first
    /// if it is not stored yet. Preparing the HTML happens off the main
    /// thread, as large messages take a while.
    fn show_body(self: &Rc<Self>, id: MessageId) {
        let (raw, domain_allowed) = self.with_store(|store| {
            let message = store.message(id).ok().flatten();
            let domain = message
                .as_ref()
                .map(|m| sender_domain(&m.from_address))
                .unwrap_or_default();
            (
                store.body(id).ok().flatten(),
                !domain.is_empty() && store.images_allowed_from(&domain).unwrap_or(false),
            )
        });
        let Some(raw) = raw else {
            if let Some(account) = self.state.borrow().active {
                self.send(account, Command::FetchBody(id));
            }
            return;
        };
        let allowed = domain_allowed
            || self.prefs.borrow().load_remote_images
            || self.state.borrow().images_once.contains(&id);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let content = gio::spawn_blocking(move || {
                let parsed = parse::parse_message(&raw);
                let rendered = html::render_body(&parsed.body, &parsed.inline_images, allowed);
                BodyContent {
                    document: rendered.document,
                    has_remote_content: rendered.has_remote_content,
                    attachments: parsed.attachments,
                }
            })
            .await;
            let Some(this) = weak.upgrade() else {
                return;
            };
            match content {
                Ok(content) => this.reader.set_body(id, &content, allowed),
                Err(_) => this
                    .reader
                    .set_body_error(id, &gettext("The message could not be read.")),
            }
        });
    }

    fn allow_images(self: &Rc<Self>, id: MessageId, choice: ImageChoice) {
        match choice {
            ImageChoice::Once => {
                self.state.borrow_mut().images_once.insert(id);
                self.show_body(id);
            }
            ImageChoice::AlwaysForDomain => {
                let domain = self
                    .with_store(|store| store.message(id).ok().flatten())
                    .map(|m| sender_domain(&m.from_address))
                    .unwrap_or_default();
                if domain.is_empty() {
                    return;
                }
                self.with_store(|store| {
                    if let Err(error) = store.allow_images_from(&domain) {
                        tracing::error!("could not save the image permission: {error}");
                    }
                });
                for message in self.reader.messages() {
                    if sender_domain(&message.from_address) == domain
                        && self.reader.is_open(message.id)
                    {
                        self.show_body(message.id);
                    }
                }
            }
        }
    }

    fn handle_attachment(self: &Rc<Self>, id: MessageId, attachment: Attachment, open: bool) {
        let Some(raw) = self.with_store(|store| store.body(id).ok().flatten()) else {
            return;
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let part = attachment.part;
            let data = gio::spawn_blocking(move || parse::attachment_data(&raw, part))
                .await
                .ok()
                .flatten();
            let Some(this) = weak.upgrade() else {
                return;
            };
            let Some(data) = data else {
                this.toast(&gettext("The attachment could not be read."));
                return;
            };
            if open {
                this.open_attachment(id, &attachment, data);
            } else {
                this.save_attachment(&attachment, data);
            }
        });
    }

    fn open_attachment(&self, id: MessageId, attachment: &Attachment, data: Vec<u8>) {
        let account = self.active().unwrap_or_default();
        let path = files::attachments_dir(&cache_dir())
            .join(account.to_string())
            .join(format!("{id}-{}", attachment.part))
            .join(&attachment.name);
        if let Err(error) = files::write_private(&path, &data) {
            tracing::warn!("could not keep the attachment: {error}");
            self.toast(&gettext("The attachment could not be opened."));
            return;
        }
        let toasts = self.toasts.clone();
        gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |result| {
                if let Err(error) = result {
                    tracing::warn!("could not open the attachment: {error}");
                    toasts.add_toast(adw::Toast::new(&gettext(
                        "The attachment could not be opened.",
                    )));
                }
            },
        );
    }

    fn save_attachment(&self, attachment: &Attachment, data: Vec<u8>) {
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Save Attachment"))
            .initial_name(&attachment.name)
            .modal(true)
            .build();
        let toasts = self.toasts.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let Ok(file) = result else {
                return;
            };
            let saved = file.replace_contents(
                &data,
                None,
                false,
                gio::FileCreateFlags::REPLACE_DESTINATION,
                gio::Cancellable::NONE,
            );
            let text = match saved {
                Ok(_) => gettext("Attachment saved"),
                Err(error) => {
                    tracing::warn!("could not save the attachment: {error}");
                    gettext("The attachment could not be saved.")
                }
            };
            toasts.add_toast(adw::Toast::new(&text));
        });
    }

    fn handle_event(self: &Rc<Self>, event: SyncEvent) {
        let active = self.state.borrow().active;
        let is_active = |account: AccountId| Some(account) == active;
        match event {
            SyncEvent::Status { account, status } => {
                if let AccountStatus::Offline(reason) | AccountStatus::LoginFailed(reason) = &status
                {
                    tracing::warn!("account {account}: {reason}");
                }
                self.state.borrow_mut().statuses.insert(account, status);
                self.update_switcher();
                self.refresh_settings_status();
            }
            SyncEvent::Synced { account } => {
                self.clean_up(account);
                self.state
                    .borrow_mut()
                    .last_sync
                    .insert(account, Local::now());
                self.refresh_settings_status();
            }
            SyncEvent::FoldersChanged { account } => {
                if is_active(account) {
                    self.reload_folders();
                }
                // Unread counts of every account show in the switcher.
                self.update_switcher();
            }
            // A conversation can include messages of other folders, so any
            // change of the active account may concern the screen.
            SyncEvent::MessagesChanged { account, .. } if is_active(account) => {
                self.schedule_refresh();
            }
            SyncEvent::BodyReady { account, message } if is_active(account) => {
                if self.state.borrow().pending_draft == Some(message) {
                    self.state.borrow_mut().pending_draft = None;
                    self.open_draft(message);
                }
                let reply = self.state.borrow().pending_reply;
                if let Some((id, kind)) = reply.filter(|(id, _)| *id == message) {
                    self.state.borrow_mut().pending_reply = None;
                    self.open_reply(id, kind);
                }
                if self.reader.wants_body(message) {
                    self.show_body(message);
                }
            }
            SyncEvent::BodyFailed {
                account,
                message,
                error,
            } if is_active(account) => {
                self.reader.set_body_error(message, &error);
            }
            SyncEvent::ChangeFailed { account } if is_active(account) => {
                self.toast(&gettext("A change could not be saved on the server."));
                self.schedule_refresh();
            }
            SyncEvent::Sent { .. } => self.toast(&gettext("Message sent")),
            SyncEvent::NewMail { account, messages } => self.new_mail(account, &messages),
            SyncEvent::SendFailed { account, error } => self.send_failed(account, &error),
            _ => {}
        }
    }

    pub fn has_accounts(&self) -> bool {
        !self.state.borrow().accounts.is_empty()
    }

    pub fn prefs(&self) -> Prefs {
        self.prefs.borrow().clone()
    }

    pub fn set_run_in_background(&self, on: bool) {
        self.prefs.borrow_mut().run_in_background = on;
        self.prefs.borrow().save(self.settings.as_ref());
        background::set_autostart(on, background_reason());
        self.update_tray();
    }

    pub fn set_show_tray_icon(&self, on: bool) {
        self.prefs.borrow_mut().show_tray_icon = on;
        self.prefs.borrow().save(self.settings.as_ref());
        self.update_tray();
    }

    fn set_up_tray(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let tray = Tray::new(move |entry| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            match entry {
                MenuEntry::Open => this.window.present(),
                MenuEntry::Compose => this.compose_new(),
                MenuEntry::Quit => {
                    if let Some(app) = this.window.application() {
                        app.quit();
                    }
                }
            }
        });
        *self.tray.borrow_mut() = Some(tray);
        let weak = Rc::downgrade(self);
        self.window.connect_visible_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.update_tray();
            }
        });
    }

    /// Shows or hides the icon in the panel and keeps its unread count.
    fn update_tray(&self) {
        let Some(tray) = self.tray.borrow().clone() else {
            return;
        };
        let wanted = {
            let prefs = self.prefs.borrow();
            tray::visible(
                prefs.run_in_background,
                prefs.show_tray_icon,
                self.has_accounts(),
                self.window.is_visible(),
            )
        };
        let unread: u32 = self
            .accounts()
            .iter()
            .map(|account| self.inbox_unread(account.id))
            .sum();
        tray.set_unread(unread);
        tray.set_wanted(wanted);
    }

    /// Conversations or one row per message; applies at once.
    pub fn set_group_conversations(self: &Rc<Self>, on: bool) {
        {
            let mut prefs = self.prefs.borrow_mut();
            if prefs.group_conversations == on {
                return;
            }
            prefs.group_conversations = on;
            prefs.save(self.settings.as_ref());
        }
        self.leave_reply();
        self.state.borrow_mut().thread = None;
        self.reader.show_empty();
        self.inner.set_show_content(false);
        if self.search_active() {
            self.show_search_results();
        } else {
            self.reload_threads();
        }
    }

    pub fn set_cleanup(self: &Rc<Self>, role: FolderRole, on: bool) {
        {
            let mut prefs = self.prefs.borrow_mut();
            match role {
                FolderRole::Trash => prefs.empty_trash = on,
                _ => prefs.empty_spam = on,
            }
            prefs.save(self.settings.as_ref());
        }
        if on {
            let accounts: Vec<AccountId> = self.accounts().iter().map(|a| a.id).collect();
            for account in accounts {
                self.clean_up(account);
            }
        }
    }

    /// Empties trash and spam of mail older than 30 days, if wanted.
    fn clean_up(self: &Rc<Self>, account: AccountId) {
        let prefs = self.prefs.borrow().clone();
        let before = chrono::Utc::now().timestamp() - 30 * 86_400;
        for (role, wanted) in [
            (FolderRole::Trash, prefs.empty_trash),
            (FolderRole::Junk, prefs.empty_spam),
        ] {
            if !wanted {
                continue;
            }
            let Some(store) = self.store_of(account) else {
                return;
            };
            let removed = {
                let mut store = store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(folder) = store.folder_by_role(role).ok().flatten() else {
                    continue;
                };
                store
                    .older_than(folder.id, before)
                    .and_then(|ids| store.delete_for_good(&ids))
                    .unwrap_or_else(|error| {
                        tracing::error!("could not clean up: {error}");
                        0
                    })
            };
            if removed > 0 {
                tracing::info!("account {account}: removed {removed} old messages");
                self.send(account, Command::Flush);
                if self.active() == Some(account) {
                    self.schedule_refresh();
                }
            }
        }
    }

    pub fn set_language(&self, language: &str) {
        self.prefs.borrow_mut().language = language.to_owned();
        self.prefs.borrow().save(self.settings.as_ref());
    }

    pub fn set_load_remote_images(self: &Rc<Self>, on: bool) {
        self.prefs.borrow_mut().load_remote_images = on;
        self.prefs.borrow().save(self.settings.as_ref());
        for message in self.reader.messages() {
            if self.reader.is_open(message.id) {
                self.show_body(message.id);
            }
        }
    }

    /// Starts E-Mail anew, for a changed language. The new process waits a
    /// moment so this one has given up its place on the session bus.
    pub fn restart(self: &Rc<Self>) {
        // Open messages are kept as drafts first.
        let composers: Vec<_> = self.composers.borrow().clone();
        for composer in composers {
            composer.window.close();
        }
        self.leave_reply();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
            if let Some(this) = weak.upgrade() {
                this.restart_now();
            }
        });
    }

    fn restart_now(&self) {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let spawned = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 1; exec \"$0\"")
            .arg(exe)
            .spawn();
        match spawned {
            Ok(_) => {
                if let Some(app) = self.window.application() {
                    app.quit();
                }
            }
            Err(error) => tracing::warn!("could not restart: {error}"),
        }
    }

    pub fn set_notifications(&self, on: bool) {
        self.prefs.borrow_mut().notify.enabled = on;
        self.prefs.borrow().save(self.settings.as_ref());
        if !on {
            let accounts: Vec<AccountId> = self.accounts().iter().map(|a| a.id).collect();
            for account in accounts {
                self.clear_notice(account);
            }
        }
    }

    pub fn set_account_notifications(&self, account: AccountId, on: bool) {
        {
            let mut prefs = self.prefs.borrow_mut();
            if on {
                prefs.notify.muted.remove(&account);
            } else {
                prefs.notify.muted.insert(account);
            }
            prefs.save(self.settings.as_ref());
        }
        if !on {
            self.clear_notice(account);
        }
    }

    /// The installed app sets up its autostart on the first start, to
    /// match the default "Run in background". A build run from the source
    /// tree (without installed settings) leaves the system alone.
    fn apply_autostart(&self) {
        if self.settings.is_some() && self.prefs.borrow().run_in_background {
            background::set_autostart(true, background_reason());
        }
    }

    fn notifier(&self) -> Option<SystemNotifier> {
        self.window
            .application()
            .map(|app| SystemNotifier(app.upcast()))
    }

    fn clear_notice(&self, account: AccountId) {
        if let Some(notifier) = self.notifier() {
            self.notifications.borrow_mut().clear(&notifier, account);
        }
    }

    /// New mail arrived in the inbox of an account: tell the system.
    fn new_mail(&self, account: AccountId, messages: &[MessageId]) {
        let Some(store) = self.store_of(account) else {
            return;
        };
        let Some(label) = self.account(account).map(|a| a.label) else {
            return;
        };
        let Some(notifier) = self.notifier() else {
            return;
        };
        let store = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let arrived: Vec<_> = messages
            .iter()
            .filter_map(|id| match store.message(*id) {
                Ok(message) => message,
                Err(error) => {
                    tracing::warn!("new message {id} cannot be read: {error}");
                    None
                }
            })
            .collect();
        tracing::debug!(
            "account {account}: {} new, {} readable, window visible {}",
            messages.len(),
            arrived.len(),
            self.window.is_visible()
        );
        let on_screen = if self.window.is_active() && self.window.is_visible() {
            OnScreen {
                account: self.active(),
                thread: self.state.borrow().thread,
            }
        } else {
            OnScreen::default()
        };
        let prefs = self.prefs.borrow().notify.clone();
        let texts = NoticeTexts {
            // Translators: title of a notification; {count} is a number.
            many: gettext("{count} new messages · {account}"),
            // Translators: title of a notification; {name} is the sender.
            one: gettext("{name} · {account}"),
            no_subject: gettext("(No subject)"),
            // Translators: last line of a notification listing messages.
            more: gettext("and {count} more"),
        };
        self.notifications.borrow_mut().new_mail(
            &notifier,
            account,
            &label,
            arrived,
            &prefs,
            on_screen,
            chrono::Utc::now().timestamp(),
            &texts,
            |id| {
                store
                    .message(id)
                    .ok()
                    .flatten()
                    .is_some_and(|m| !m.flags.seen)
            },
        );
    }

    /// A click on a notification: switch to the account and open the mail.
    pub fn open_message(self: &Rc<Self>, account: AccountId, message: MessageId) {
        if self.account(account).is_none() {
            return;
        }
        self.clear_notice(account);
        if self.active() != Some(account) {
            self.set_active(account);
        }
        let Some(summary) = self.with_store(|store| store.message(message).ok().flatten()) else {
            return;
        };
        self.folders.set_folders(
            &self.with_store(|store| store.folders().unwrap_or_default()),
            Some(summary.folder_id),
        );
        self.select_folder(summary.folder_id);
        let thread = if self.prefs.borrow().group_conversations {
            summary.thread_id
        } else {
            e_mail_core::store::single_thread(summary.id)
        };
        self.open_thread(thread);
    }

    /// "Mark as Read" in a notification, also with the window closed.
    pub fn notification_read(self: &Rc<Self>, account: AccountId, messages: &[MessageId]) {
        self.clear_notice(account);
        self.change_in(account, |store| store.set_seen(messages, true).map(|_| ()));
    }

    /// "Delete" in a notification. There is nothing to undo it with, so it
    /// goes to the server right away (into the trash).
    pub fn notification_delete(self: &Rc<Self>, account: AccountId, messages: &[MessageId]) {
        self.clear_notice(account);
        let now = chrono::Utc::now().timestamp();
        self.change_in(account, |store| {
            store.delete_messages(messages, now).map(|_| ())
        });
    }

    fn change_in(
        self: &Rc<Self>,
        account: AccountId,
        f: impl FnOnce(&mut Store) -> e_mail_core::store::StoreResult<()>,
    ) {
        let Some(store) = self.store_of(account) else {
            return;
        };
        if let Err(error) = f(&mut store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
        {
            tracing::error!("could not save a change: {error}");
        }
        self.send(account, Command::Flush);
        if self.active() == Some(account) {
            self.schedule_refresh();
        }
        self.update_switcher();
    }

    /// After the network comes back or the computer wakes up, every account
    /// reconnects at once instead of waiting for dead connections to time out.
    fn watch_network(self: &Rc<Self>) {
        let monitor = gio::NetworkMonitor::default();
        self.network_up.set(monitor.is_network_available());
        let weak = Rc::downgrade(self);
        monitor.connect_network_changed(move |_, available| {
            if let Some(this) = weak.upgrade() {
                let was = this.network_up.replace(available);
                if available && !was {
                    this.reconnect_all();
                }
            }
        });
        // The monotonic clock stands still during suspend, the wall clock
        // does not: a gap between them means the computer was asleep.
        let last = Cell::new((glib::monotonic_time(), glib::real_time()));
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(30, move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let (mono, real) = (glib::monotonic_time(), glib::real_time());
            let (last_mono, last_real) = last.replace((mono, real));
            let slept = (real - last_real) - (mono - last_mono);
            if slept > 60_000_000 {
                tracing::info!("woke up after {} s", slept / 1_000_000);
                this.reconnect_all();
            }
            glib::ControlFlow::Continue
        });
        self.apply_autostart();
    }

    fn reconnect_all(self: &Rc<Self>) {
        if let Some(source) = self.network_restart.take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        // Give the network a moment to settle.
        let source = glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.network_restart.take();
            let accounts: Vec<AccountId> = this.accounts().iter().map(|a| a.id).collect();
            for account in accounts {
                this.restart_sync(account, None);
            }
        });
        *self.network_restart.borrow_mut() = Some(source);
    }

    /// The actions of the row menu, each taking the conversation id.
    fn add_row_actions(self: &Rc<Self>) {
        let group = gio::SimpleActionGroup::new();
        for name in [
            "reply",
            "reply-all",
            "forward",
            "read",
            "star",
            "move",
            "spam",
            "delete",
            "edit",
            "empty",
        ] {
            let action = gio::SimpleAction::new(name, Some(glib::VariantTy::INT64));
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, parameter| {
                let (Some(this), Some(thread)) = (
                    weak.upgrade(),
                    parameter.and_then(|value| value.get::<i64>()),
                ) else {
                    return;
                };
                this.row_action(name, thread);
            });
            group.add_action(&action);
        }
        self.window.insert_action_group("row", Some(&group));
    }

    fn show_row_menu(self: &Rc<Self>, thread: ThreadId, widget: &gtk::Widget, x: f64, y: f64) {
        let folder = self
            .thread_folder(thread)
            .and_then(|id| self.with_store(|store| store.folder(id).ok().flatten()));
        let role = folder.and_then(|folder| folder.role);
        let targets = self.messages.targets(thread);
        let summary: Vec<(MessageId, bool, bool)> = targets
            .iter()
            .flat_map(|thread| self.conversation_ids(*thread))
            .collect();
        let unread = summary.iter().any(|(_, seen, _)| !seen);
        let starred = summary.iter().any(|(.., flagged)| *flagged);
        let item = |menu: &gio::Menu, label: String, action: &str| {
            let entry = gio::MenuItem::new(Some(&label), None);
            entry.set_action_and_target_value(
                Some(&format!("row.{action}")),
                Some(&thread.to_variant()),
            );
            menu.append_item(&entry);
        };
        let menu = gio::Menu::new();
        if role == Some(FolderRole::Drafts) {
            let section = gio::Menu::new();
            if targets.len() == 1 {
                item(&section, gettext("Edit"), "edit");
            }
            item(&section, gettext("Delete"), "delete");
            menu.append_section(None, &section);
        } else {
            if targets.len() == 1 {
                let answer = gio::Menu::new();
                item(&answer, gettext("Reply"), "reply");
                item(&answer, gettext("Reply All"), "reply-all");
                item(&answer, gettext("Forward"), "forward");
                menu.append_section(None, &answer);
            }
            let mark = gio::Menu::new();
            item(
                &mark,
                if unread {
                    gettext("Mark as Read")
                } else {
                    gettext("Mark as Unread")
                },
                "read",
            );
            item(
                &mark,
                if starred {
                    gettext("Remove Star")
                } else {
                    gettext("Star")
                },
                "star",
            );
            menu.append_section(None, &mark);
            let place = gio::Menu::new();
            item(&place, gettext("Move To…"), "move");
            item(
                &place,
                if role == Some(FolderRole::Junk) {
                    gettext("Not Spam")
                } else {
                    gettext("Mark as Spam")
                },
                "spam",
            );
            menu.append_section(None, &place);
            let remove = gio::Menu::new();
            item(&remove, gettext("Delete"), "delete");
            menu.append_section(None, &remove);
            let empty = match role {
                Some(FolderRole::Trash) => Some(gettext("Empty Trash")),
                Some(FolderRole::Junk) => Some(gettext("Empty Spam")),
                _ => None,
            };
            if let Some(label) = empty {
                let section = gio::Menu::new();
                let entry = gio::MenuItem::new(Some(&label), None);
                entry.set_action_and_target_value(Some("row.empty"), Some(&thread.to_variant()));
                section.append_item(&entry);
                menu.append_section(None, &section);
            }
        }
        let popover = gtk::PopoverMenu::builder()
            .menu_model(&menu)
            .has_arrow(false)
            .halign(gtk::Align::Start)
            .pointing_to(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1))
            .build();
        popover.set_parent(widget);
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            // Unparent after the chosen action has run.
            glib::idle_add_local_once(move || popover.unparent());
        });
        popover.popup();
    }

    fn row_action(self: &Rc<Self>, name: &str, thread: ThreadId) {
        let targets = self.messages.targets(thread);
        match name {
            "empty" => {
                let folder = self
                    .thread_folder(thread)
                    .and_then(|id| self.with_store(|store| store.folder(id).ok().flatten()));
                if let Some(folder) = folder {
                    self.confirm_empty_folder(folder);
                }
            }
            "reply" | "reply-all" | "forward" => {
                let kind = match name {
                    "reply" => ReplyKind::Reply,
                    "reply-all" => ReplyKind::ReplyAll,
                    _ => ReplyKind::Forward,
                };
                self.open_thread(thread);
                if let Some(message) = self.reader.reply_target() {
                    self.open_reply(message, kind);
                }
            }
            "read" => {
                let summary: Vec<(MessageId, bool, bool)> = targets
                    .iter()
                    .flat_map(|thread| self.conversation_ids(*thread))
                    .collect();
                if summary.iter().any(|(_, seen, _)| !seen) {
                    let ids: Vec<MessageId> = summary.iter().map(|(id, ..)| *id).collect();
                    self.change(|store| store.set_seen(&ids, true));
                } else {
                    let latest: Vec<MessageId> = targets
                        .iter()
                        .filter_map(|thread| self.conversation_ids(*thread).last().map(|m| m.0))
                        .collect();
                    self.change(|store| store.set_seen(&latest, false));
                }
            }
            "star" => {
                let summary: Vec<(MessageId, bool, bool)> = targets
                    .iter()
                    .flat_map(|thread| self.conversation_ids(*thread))
                    .collect();
                if summary.iter().any(|(.., flagged)| *flagged) {
                    let ids: Vec<MessageId> = summary.iter().map(|(id, ..)| *id).collect();
                    self.change(|store| store.set_flagged(&ids, false));
                } else {
                    let latest: Vec<MessageId> = targets
                        .iter()
                        .filter_map(|thread| self.conversation_ids(*thread).last().map(|m| m.0))
                        .collect();
                    self.change(|store| store.set_flagged(&latest, true));
                }
            }
            "move" => {
                self.choose_target(targets);
                return;
            }
            "spam" => self.spam_threads(&targets),
            "delete" => self.delete_threads(&targets),
            "edit" => {
                if let Some(folder) = self.thread_folder(thread) {
                    self.open_draft_thread(thread, folder);
                }
            }
            _ => {}
        }
        self.select_button.set_active(false);
    }

    fn connect_search(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.search_button.connect_toggled(move |button| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if button.is_active() {
                if this.state.borrow().search.is_none() {
                    this.state.borrow_mut().search = Some(SearchState::default());
                }
                this.search_bar.set_reveal_child(true);
                this.search_entry.grab_focus();
                this.update_search_status(0);
            } else if this.state.borrow().search.is_some() {
                this.end_search();
            } else {
                this.search_bar.set_reveal_child(false);
            }
        });
        let weak = Rc::downgrade(self);
        self.search_entry.connect_search_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.run_search();
            }
        });
        let weak = Rc::downgrade(self);
        self.search_entry.connect_stop_search(move |_| {
            if let Some(this) = weak.upgrade() {
                this.end_search();
            }
        });
        // Enter or Down moves from the search field into the results.
        let weak = Rc::downgrade(self);
        self.search_entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.messages.open_relative(1);
            }
        });
        for chip in &self.search_filters {
            let weak = Rc::downgrade(self);
            chip.connect_toggled(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.run_search();
                }
            });
        }
    }

    fn search_active(&self) -> bool {
        self.state
            .borrow()
            .search
            .as_ref()
            .is_some_and(|search| search.query.is_active())
    }

    /// Opens the search, or puts the cursor back into it.
    pub fn open_search(&self) {
        if self.state.borrow().active.is_none() {
            return;
        }
        if !self.search_button.is_active() {
            self.search_button.set_active(true);
        }
        self.search_entry.grab_focus();
    }

    /// Takes the search away without touching the list.
    fn close_search(&self) {
        if self.state.borrow_mut().search.take().is_none() {
            return;
        }
        self.search_entry.set_text("");
        for chip in &self.search_filters {
            chip.set_active(false);
        }
        self.search_button.set_active(false);
        self.search_bar.set_reveal_child(false);
        self.search_spinner.set_visible(false);
        self.search_spinner.set_spinning(false);
    }

    /// Esc: back to the folder as it was before.
    fn end_search(self: &Rc<Self>) {
        let thread = self.state.borrow().thread;
        self.close_search();
        let folder = self.state.borrow().folder;
        if let (Some(thread), Some(folder)) = (thread, folder) {
            let here = self.with_store(|store| {
                !store
                    .conversation(thread, folder)
                    .unwrap_or_default()
                    .is_empty()
            });
            if !here {
                self.leave_reply();
                self.state.borrow_mut().thread = None;
                self.reader.show_empty();
                self.inner.set_show_content(false);
            }
        }
        self.update_list_title(folder);
        self.reload_threads();
        self.refresh_conversation();
    }

    fn run_search(self: &Rc<Self>) {
        let text = self.search_entry.text().to_string();
        let only = self.search_filters[3].is_active();
        let folder = self.state.borrow().folder;
        let query = SearchQuery {
            text: text.clone(),
            filters: search::Filters {
                unread: self.search_filters[0].is_active(),
                flagged: self.search_filters[1].is_active(),
                attachment: self.search_filters[2].is_active(),
                folder: folder.filter(|_| only),
            },
        };
        let generation = {
            let mut state = self.state.borrow_mut();
            let Some(search) = state.search.as_mut() else {
                return;
            };
            if search.query == query {
                return;
            }
            if search.server_text != text.trim() {
                search.server_hits.clear();
                search.server_text.clear();
            }
            search.query = query.clone();
            search.generation += 1;
            search.generation
        };
        self.messages.scroll_to_top();
        if !query.is_active() {
            if let Some(search) = self.state.borrow_mut().search.as_mut() {
                search.folders.clear();
            }
            self.update_list_title(folder);
            self.reload_threads();
            self.update_search_status(0);
            return;
        }
        self.show_search_results();
        if !text.trim().is_empty() {
            self.start_server_search(generation, text.trim().to_owned(), query.filters.folder);
        }
    }

    /// Fills the list with the results of the current search.
    fn show_search_results(&self) {
        let (query, extra, thread) = {
            let state = self.state.borrow();
            let Some(search) = state.search.as_ref() else {
                return;
            };
            (
                search.query.clone(),
                search.server_hits.clone(),
                state.thread,
            )
        };
        let grouped = self.prefs.borrow().group_conversations;
        let (results, names) = self.with_store(|store| {
            let results = store
                .search_threads(&query, &extra, grouped)
                .unwrap_or_else(|error| {
                    tracing::error!("search failed: {error}");
                    Vec::new()
                });
            let names: HashMap<FolderId, String> = store
                .folders()
                .unwrap_or_default()
                .iter()
                .map(|folder| (folder.id, folder_label(folder)))
                .collect();
            (results, names)
        });
        let folders: HashMap<ThreadId, FolderId> =
            results.iter().map(|r| (r.thread.id, r.folder)).collect();
        let labels: HashMap<ThreadId, String> = if query.filters.folder.is_some() {
            HashMap::new()
        } else {
            folders
                .iter()
                .filter_map(|(thread, folder)| Some((*thread, names.get(folder)?.clone())))
                .collect()
        };
        if let Some(search) = self.state.borrow_mut().search.as_mut() {
            search.folders = folders;
        }
        self.messages.set_context(ListContext {
            own_address: self.own_address(),
            drafts: false,
            highlight: query.terms(),
            folders: labels,
        });
        let count = results.len();
        self.messages
            .set_threads(results.into_iter().map(|r| r.thread).collect(), thread);
        let title = gettextrs::pgettext("title of the search results", "Search");
        self.list_title.set_title(&title);
        self.list_title.set_subtitle("");
        self.list_page.set_title(&title);
        self.update_search_status(count);
        self.list_empty
            .set_icon_name(Some("system-search-symbolic"));
        self.list_empty.set_title(&gettext("No Results"));
        self.list_empty
            .set_description(Some(&gettext("Try another word or remove a filter.")));
        self.list_stack
            .set_visible_child_name(if count == 0 { "empty" } else { "list" });
    }

    fn update_search_status(&self, count: usize) {
        let (active, only) = {
            let state = self.state.borrow();
            let query = state
                .search
                .as_ref()
                .map(|s| s.query.clone())
                .unwrap_or_default();
            (query.is_active(), query.filters.folder)
        };
        let text = if !active {
            let label = self
                .active()
                .and_then(|id| self.account(id))
                .map(|a| a.label)
                .unwrap_or_default();
            fmt(&gettext("Search in “{}”"), &[&label])
        } else {
            let place = match only.and_then(|id| self.with_store(|s| s.folder(id).ok().flatten())) {
                Some(folder) => folder_label(&folder),
                None => gettext("all folders"),
            };
            if count == 1 {
                fmt(&gettext("1 result in {}"), &[&place])
            } else {
                fmt(&gettext("{} results in {}"), &[&count.to_string(), &place])
            }
        };
        // The count shows once, below the filters.
        self.search_status.set_label(&text);
    }

    /// As long as not everything is stored here, the server searches too.
    /// Waits a moment so that typing does not start a search per letter.
    fn start_server_search(self: &Rc<Self>, generation: u64, text: String, only: Option<FolderId>) {
        let Some(account) = self.active() else {
            return;
        };
        if !matches!(self.status(account), Some(AccountStatus::Online)) {
            return;
        }
        let Some(connector) = self.state.borrow().connectors.get(&account).cloned() else {
            return;
        };
        let Some(store) = self.active_store() else {
            return;
        };
        let folders = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .folders_to_search_remotely(only)
            .unwrap_or_default();
        if folders.is_empty() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if !this.is_current_search(generation) {
                return;
            }
            this.search_spinner.set_visible(true);
            this.search_spinner.set_spinning(true);
            let weak = Rc::downgrade(&this);
            glib::spawn_future_local(async move {
                let search_text = text.clone();
                let result = runtime::run(async move {
                    let mut server = connector.connect().await?;
                    let found =
                        search::server_search(&mut server, &store, &folders, &search_text).await;
                    server.logout().await;
                    found
                })
                .await;
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if !this.is_current_search(generation) {
                    return;
                }
                this.search_spinner.set_visible(false);
                this.search_spinner.set_spinning(false);
                match result {
                    Ok(found) => {
                        if let Some(search) = this.state.borrow_mut().search.as_mut() {
                            search.server_hits = found;
                            search.server_text = text;
                        }
                        this.show_search_results();
                    }
                    Err(error) => tracing::info!("the server search did not work: {error:#}"),
                }
            });
        });
    }

    fn is_current_search(&self, generation: u64) -> bool {
        self.state
            .borrow()
            .search
            .as_ref()
            .is_some_and(|search| search.generation == generation)
    }

    /// Single-key shortcuts as in the wireframe. They only count while no
    /// text field or editor has the keyboard.
    fn add_shortcuts(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if this.handle_key(key, modifiers) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.window.add_controller(keys);
    }

    fn handle_key(self: &Rc<Self>, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
        if self.root.visible_child_name().as_deref() != Some("main")
            || self.window.visible_dialog().is_some()
        {
            return false;
        }
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        let alt = modifiers.contains(gdk::ModifierType::ALT_MASK);
        if ctrl && !alt && matches!(key, gdk::Key::f | gdk::Key::F) {
            self.open_search();
            return true;
        }
        if key == gdk::Key::Escape {
            return self.escape();
        }
        if ctrl || alt || modifiers.contains(gdk::ModifierType::SUPER_MASK) || self.typing() {
            return false;
        }
        let thread = self.state.borrow().thread;
        match key {
            gdk::Key::slash => self.open_search(),
            gdk::Key::c => self.compose_new(),
            gdk::Key::question => show_shortcuts(&self.window),
            gdk::Key::j => self.messages.open_relative(1),
            gdk::Key::k => self.messages.open_relative(-1),
            gdk::Key::F9 => {
                if self.outer.is_collapsed() {
                    self.outer.set_show_sidebar(!self.outer.shows_sidebar());
                }
            }
            gdk::Key::r | gdk::Key::R | gdk::Key::f => {
                let kind = match key {
                    gdk::Key::r => ReplyKind::Reply,
                    gdk::Key::R => ReplyKind::ReplyAll,
                    _ => ReplyKind::Forward,
                };
                let Some(message) = self.reader.reply_target() else {
                    return false;
                };
                self.open_reply(message, kind);
            }
            gdk::Key::s | gdk::Key::u | gdk::Key::Delete | gdk::Key::KP_Delete => {
                let Some(thread) = thread else {
                    return false;
                };
                let action = match key {
                    gdk::Key::s => RowAction::ToggleStar,
                    gdk::Key::u => RowAction::ToggleRead,
                    _ => RowAction::Delete,
                };
                self.thread_action(action, thread);
            }
            _ => return false,
        }
        true
    }

    /// True while a text field or the editor has the keyboard.
    fn typing(&self) -> bool {
        let mut widget = gtk::prelude::GtkWindowExt::focus(&self.window);
        while let Some(current) = widget {
            if current.is::<gtk::Text>() || current.is::<gtk::TextView>() {
                return true;
            }
            if let Some(web) = current.downcast_ref::<webkit::WebView>() {
                return webkit::prelude::WebViewExt::is_editable(web);
            }
            widget = current.parent();
        }
        false
    }

    /// Esc closes what is open, one thing at a time.
    fn escape(self: &Rc<Self>) -> bool {
        if self.state.borrow().search.is_some() {
            self.end_search();
        } else if self.messages.selection_mode() {
            self.select_button.set_active(false);
        } else if self.reply.borrow().is_some() {
            self.leave_reply();
        } else if self.inner.is_collapsed() && self.inner.shows_content() {
            self.inner.set_show_content(false);
        } else if self.outer.is_collapsed() && self.outer.shows_sidebar() {
            self.outer.set_show_sidebar(false);
        } else {
            return false;
        }
        true
    }

    /// Known addresses of an account for the recipient fields.
    pub fn contacts_for(self: &Rc<Self>, account: AccountId) -> ContactSource {
        let store = self.state.borrow().stores.get(&account).cloned();
        let own = self
            .account(account)
            .map(|account| account.email.to_lowercase())
            .unwrap_or_default();
        Rc::new(move |query: &str| {
            let Some(store) = &store else {
                return Vec::new();
            };
            store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contacts(query, 6)
                .unwrap_or_default()
                .into_iter()
                .filter(|(_, address)| address.to_lowercase() != own)
                .map(|(name, address)| Mailbox::new(name, address))
                .collect()
        })
    }

    pub fn signature(&self, account: AccountId) -> String {
        self.registry.signature(account).unwrap_or_else(|error| {
            tracing::error!("could not read the signature: {error}");
            String::new()
        })
    }

    pub fn set_signature(&self, account: AccountId, html: &str) {
        if let Err(error) = self.registry.set_signature(account, html) {
            tracing::error!("could not save the signature: {error}");
            self.toast(&gettext("The signature could not be saved."));
        }
    }

    fn quote_texts() -> QuoteTexts {
        QuoteTexts {
            // Translators: introduces the quoted message in a reply.
            wrote: gettext("On {date}, {name} wrote:"),
            forwarded: gettext("Forwarded message"),
            from: gettext("From"),
            date: gettext("Date"),
            subject: gettext("Subject"),
            to: gettext("To"),
            format_date: Box::new(format_quote_time),
        }
    }

    /// A reply or forward of `original` from `account`, with its signature.
    pub fn reply_draft(&self, account: &Account, original: &Original, kind: ReplyKind) -> Draft {
        compose::reply_draft(
            original,
            kind,
            &account.email,
            &self.signature(account.id),
            &Window::quote_texts(),
        )
    }

    /// Opens a window for a new message from the active account.
    pub fn compose_new(self: &Rc<Self>) {
        let Some(account) = self.active().and_then(|id| self.account(id)) else {
            return;
        };
        let draft = compose::new_draft(&account.email, &self.signature(account.id));
        self.open_compose(account, &draft);
    }

    fn open_compose(self: &Rc<Self>, account: Account, draft: &Draft) {
        let existing = self
            .composers
            .borrow()
            .iter()
            .find(|c| c.composer.message_id() == draft.message_id)
            .cloned();
        if let Some(existing) = existing {
            existing.present();
            return;
        }
        let window = ComposeWindow::new(self, account, draft);
        window.present();
        self.composers.borrow_mut().push(window);
    }

    pub fn compose_closed(&self, message_id: &str) {
        self.composers
            .borrow_mut()
            .retain(|c| c.composer.message_id() != message_id);
    }

    /// Drafts open in the compose window, as in the drafts folder there is
    /// nothing to read.
    fn open_draft_thread(self: &Rc<Self>, thread: ThreadId, folder: FolderId) {
        let latest = self.with_store(|store| {
            store
                .conversation(thread, folder)
                .unwrap_or_default()
                .into_iter()
                .rfind(|message| message.folder_id == folder)
        });
        let Some(message) = latest else {
            return;
        };
        self.messages.unselect();
        let stored = self.with_store(|store| store.body(message.id).ok().flatten().is_some());
        if stored {
            self.open_draft(message.id);
        } else {
            self.state.borrow_mut().pending_draft = Some(message.id);
            if let Some(account) = self.active() {
                self.send(account, Command::FetchBody(message.id));
            }
        }
    }

    fn open_draft(self: &Rc<Self>, id: MessageId) {
        let Some(account) = self.active().and_then(|id| self.account(id)) else {
            return;
        };
        let Some(raw) = self.with_store(|store| store.body(id).ok().flatten()) else {
            return;
        };
        let email = account.email.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(draft) = gio::spawn_blocking(move || compose::read_draft(&raw, &email)).await
            else {
                return;
            };
            if let Some(this) = weak.upgrade() {
                this.open_compose(account, &draft);
            }
        });
    }

    /// Opens the inline editor below a message, or switches its kind if
    /// it is already open there.
    fn open_reply(self: &Rc<Self>, id: MessageId, kind: ReplyKind) {
        let current = self.reply.borrow().clone();
        if let Some(current) = current {
            if current.message == id {
                current.set_kind(kind);
                return;
            }
            self.leave_reply();
        }
        let Some(account) = self.active().and_then(|id| self.account(id)) else {
            return;
        };
        let Some(raw) = self.with_store(|store| store.body(id).ok().flatten()) else {
            // Opens by itself once the message has arrived.
            self.state.borrow_mut().pending_reply = Some((id, kind));
            if let Some(account) = self.active() {
                self.send(account, Command::FetchBody(id));
            }
            return;
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(original) = gio::spawn_blocking(move || compose::read_original(&raw)).await
            else {
                return;
            };
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.reply.borrow().is_some() || this.active() != Some(account.id) {
                return;
            }
            let editor = ReplyEditor::new(&this, account, id, original, kind);
            this.reader
                .set_editor(Some((editor.widget.clone().upcast(), id)));
            *this.reply.borrow_mut() = Some(editor);
        });
    }

    /// Takes the reply editor away and keeps what was written as a draft.
    fn leave_reply(self: &Rc<Self>) {
        let Some(reply) = self.reply.take() else {
            return;
        };
        if reply.leave() {
            self.toast(&gettext("Reply saved as draft"));
        }
        self.reader.set_editor(None);
    }

    /// Closes the reply editor without saving anything.
    pub fn close_reply(&self) {
        self.reply.take();
        self.reader.set_editor(None);
    }

    fn store_of(&self, account: AccountId) -> Option<SharedStore> {
        self.state.borrow().stores.get(&account).cloned()
    }

    fn sender(account: &Account) -> Mailbox {
        Mailbox::new(account.display_name.clone(), account.email.clone())
    }

    /// Hands a draft to the outbox of its account; the worker saves it in
    /// the drafts folder.
    pub fn save_draft(self: &Rc<Self>, account: &Account, draft: Draft) {
        let Some(store) = self.store_of(account.id) else {
            return;
        };
        let from = Window::sender(account);
        let id = account.id;
        let events = self.events.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let saved = gio::spawn_blocking(move || {
                let raw = compose::build_message(&from, &draft, chrono::Utc::now().timestamp());
                store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .queue_draft(&draft.message_id, &raw)
            })
            .await;
            match saved {
                Ok(Ok(())) => {
                    if let Some(this) = weak.upgrade() {
                        this.send(id, Command::Flush);
                        this.schedule_refresh();
                    }
                }
                _ => {
                    tracing::error!("could not keep the draft");
                    let _ = events.try_send(SyncEvent::ChangeFailed { account: id });
                }
            }
        });
    }

    /// Queues a message for sending. It goes out as soon as there is a
    /// connection; the toast says when that is not right now.
    pub fn send_draft(self: &Rc<Self>, account: &Account, draft: Draft) {
        let Some(store) = self.store_of(account.id) else {
            return;
        };
        let from = Window::sender(account);
        let id = account.id;
        let weak = Rc::downgrade(self);
        let offline = matches!(
            self.status(account.id),
            Some(AccountStatus::Offline(_) | AccountStatus::LoginFailed(_))
        );
        if offline {
            self.toast(&gettext(
                "No connection. The message will be sent as soon as you are online again.",
            ));
        }
        let toasts = self.toasts.clone();
        glib::spawn_future_local(async move {
            let queued = gio::spawn_blocking(move || {
                let raw = compose::build_message(&from, &draft, chrono::Utc::now().timestamp());
                let mut store = store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                store.queue_send(&draft.message_id, &raw, &draft.recipients())?;
                store.retry_outbox_now()
            })
            .await;
            match queued {
                Ok(Ok(())) => {
                    if let Some(this) = weak.upgrade() {
                        this.send(id, Command::Flush);
                        this.schedule_refresh();
                    }
                }
                _ => {
                    tracing::error!("could not queue the message");
                    toasts.add_toast(adw::Toast::new(&gettext(
                        "The message could not be sent. Please try again.",
                    )));
                }
            }
        });
        self.schedule_refresh();
    }

    /// Removes every saved version of a draft.
    pub fn discard_draft(self: &Rc<Self>, account: AccountId, message_id: &str) {
        let Some(store) = self.store_of(account) else {
            return;
        };
        if let Err(error) = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .queue_discard(message_id)
        {
            tracing::error!("could not discard the draft: {error}");
        }
        self.send(account, Command::Flush);
        self.schedule_refresh();
    }

    fn send_failed(self: &Rc<Self>, account: AccountId, error: &SendError) {
        let label = self.account(account).map(|a| a.label).unwrap_or_default();
        match error {
            SendError::Rejected(reason) => {
                let dialog = adw::AlertDialog::builder()
                    .heading(gettext("Message Not Sent"))
                    .body(fmt(
                        &gettext(
                            "The outgoing mail server of “{}” refused the message. It is waiting in Drafts so you can correct it.\n\nThe server said: {}",
                        ),
                        &[&label, reason],
                    ))
                    .close_response("ok")
                    .build();
                dialog.add_response("ok", &gettext("OK"));
                dialog.present(Some(&self.window));
                self.schedule_refresh();
            }
            SendError::Login(_) => self.toast(&fmt(
                &gettext(
                    "Sending from “{}” failed: the outgoing mail server did not accept the password. The message will be sent once that works again.",
                ),
                &[&label],
            )),
            SendError::Unreachable(_) => self.toast(&gettext(
                "The outgoing mail server cannot be reached right now. The message will be sent later.",
            )),
        }
    }

    fn refresh_settings_status(&self) {
        if let Some(settings) = self.settings_dialog.borrow().upgrade() {
            settings.refresh_status();
        }
    }
}

/// Width of the message list, changeable by dragging its border.
const LIST_WIDTH: i32 = 380;
const MIN_LIST_WIDTH: i32 = 300;
const MAX_LIST_WIDTH: i32 = 700;

fn background_reason() -> String {
    gettext("Fetch new mail and show notifications while the window is closed")
}

/// The overview of keyboard shortcuts, as in other GNOME apps.
pub fn show_shortcuts(parent: &impl IsA<gtk::Window>) {
    let window = crate::shortcuts::window();
    window.set_transient_for(Some(parent));
    window.set_modal(true);
    window.present();
}

fn build_account_switcher() -> AccountSwitcher {
    let avatar = gtk::Box::builder().valign(gtk::Align::Center).build();
    let title = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["heading"])
        .build();
    let subtitle = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label", "caption"])
        .build();
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    text.append(&title);
    text.append(&subtitle);
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.append(&avatar);
    content.append(&text);
    let others_unread = gtk::Box::builder()
        .css_classes(["unread-dot"])
        .valign(gtk::Align::Center)
        .tooltip_text(gettext("New mail in another account"))
        .visible(false)
        .build();
    others_unread.update_property(&[gtk::accessible::Property::Label(&gettext(
        "New mail in another account",
    ))]);
    content.append(&others_unread);
    content.append(&gtk::Image::from_icon_name("pan-up-symbolic"));

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["navigation-sidebar"])
        .build();
    let add = widgets::icon_label_button("list-add-symbolic", &gettext("Add Account"));
    add.set_action_name(Some("app.add-account"));
    add.add_css_class("flat");
    let popover_content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    popover_content.set_width_request(220);
    popover_content.append(
        &gtk::Label::builder()
            .label(gettext("Switch Account"))
            .xalign(0.0)
            .margin_start(6)
            .css_classes(["dim-label", "caption-heading"])
            .build(),
    );
    popover_content.append(&list);
    popover_content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    popover_content.append(&add);
    let popover = gtk::Popover::builder().child(&popover_content).build();
    {
        let popover = popover.clone();
        add.connect_clicked(move |_| popover.popdown());
    }

    let button = gtk::MenuButton::builder()
        .child(&content)
        .popover(&popover)
        .direction(gtk::ArrowType::Up)
        .tooltip_text(gettext("Switch Account"))
        .css_classes(["account-switcher"])
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    AccountSwitcher {
        button,
        others_unread,
        avatar,
        title,
        subtitle,
        list,
        popover,
    }
}

fn add_breakpoints(
    window: &adw::ApplicationWindow,
    outer: &adw::OverlaySplitView,
    inner: &adw::NavigationSplitView,
) {
    let medium = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 900sp").expect("valid condition"),
    );
    medium.add_setter(outer, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(medium);

    let narrow = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 560sp").expect("valid condition"),
    );
    narrow.add_setter(outer, "collapsed", Some(&true.to_value()));
    narrow.add_setter(inner, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(narrow);
}
