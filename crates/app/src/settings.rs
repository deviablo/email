// SPDX-License-Identifier: GPL-3.0-or-later

//! Settings → Accounts, with the subpage "Edit Account", Settings →
//! General (running in the background, notifications), Settings →
//! Signature for the active account and Settings → Keyboard Shortcuts.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use e_mail_core::folder::FolderRole;
use e_mail_core::model::{AccountId, ServerSettings};
use gettextrs::gettext;
use gtk::glib;

use crate::editor::Editor;
use crate::setup::ServerFields;
use crate::widgets::{self, fmt};
use crate::window::Window;

struct EditPage {
    account: AccountId,
    page: adw::NavigationPage,
    connection: adw::ActionRow,
    connection_suffix: gtk::Box,
}

pub struct Settings {
    pub dialog: adw::PreferencesDialog,
    window: Weak<Window>,
    list: adw::PreferencesGroup,
    rows: RefCell<Vec<gtk::Widget>>,
    edit: RefCell<Option<EditPage>>,
    signature_group: adw::PreferencesGroup,
    signature: RefCell<Option<(AccountId, Rc<Editor>, gtk::Widget)>>,
    signature_save: RefCell<Option<glib::SourceId>>,
    notify_group: adw::PreferencesGroup,
    notify_rows: RefCell<Vec<gtk::Widget>>,
    notify_switch: adw::SwitchRow,
}

impl Settings {
    pub fn new(window: &Rc<Window>) -> Rc<Settings> {
        let dialog = adw::PreferencesDialog::builder()
            .title(gettext("Settings"))
            .search_enabled(false)
            .content_width(640)
            .content_height(680)
            .build();
        let list = adw::PreferencesGroup::builder()
            .title(gettext("Accounts"))
            .build();
        let page = adw::PreferencesPage::builder()
            .title(gettext("Accounts"))
            .icon_name("system-users-symbolic")
            .build();
        page.add(&list);
        dialog.add(&page);

        let prefs = window.prefs();
        let general = adw::PreferencesPage::builder()
            .title(gettext("General"))
            .icon_name("preferences-system-symbolic")
            .build();
        let language_group = adw::PreferencesGroup::builder()
            .title(gettext("Language"))
            .build();
        // Each language is named in itself, so it can be found without
        // understanding the current one.
        let languages = ["", "de", "en"];
        let language_names =
            gtk::StringList::new(&[&gettext("As in the system"), "Deutsch", "English"]);
        let language_row = adw::ComboRow::builder()
            .title(gettext("Language of the app"))
            .subtitle(gettext("German or English, also different from the system"))
            .model(&language_names)
            .selected(
                languages
                    .iter()
                    .position(|l| *l == prefs.language)
                    .unwrap_or(0) as u32,
            )
            .build();
        let restart_button = gtk::Button::builder()
            .label(gettext("Restart Now"))
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        let restart_row = adw::ActionRow::builder()
            .title(gettext(
                "The new language applies after restarting E-Mail. Drafts are kept.",
            ))
            .visible(false)
            .build();
        restart_row.add_suffix(&restart_button);
        language_group.add(&language_row);
        language_group.add(&restart_row);
        general.add(&language_group);
        let started_with = prefs.language.clone();
        {
            let window = Rc::downgrade(window);
            let restart_row = restart_row.clone();
            language_row.connect_selected_notify(move |row| {
                let language = languages
                    .get(row.selected() as usize)
                    .copied()
                    .unwrap_or_default();
                if let Some(window) = window.upgrade() {
                    window.set_language(language);
                }
                restart_row.set_visible(language != started_with);
            });
        }
        {
            let window = Rc::downgrade(window);
            restart_button.connect_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    window.restart();
                }
            });
        }

        let behavior = adw::PreferencesGroup::builder()
            .title(gettext("Behavior"))
            .build();
        let background_switch = adw::SwitchRow::builder()
            .title(gettext("Run in background"))
            .subtitle(gettext(
                "New mail arrives even when the window is closed. E-Mail also starts after login.",
            ))
            .active(prefs.run_in_background)
            .build();
        behavior.add(&background_switch);
        let tray_switch = adw::SwitchRow::builder()
            .title(gettext("Show icon in the panel"))
            .subtitle(gettext("While E-Mail runs in the background"))
            .active(prefs.show_tray_icon)
            .sensitive(prefs.run_in_background)
            .build();
        behavior.add(&tray_switch);
        let group_switch = adw::SwitchRow::builder()
            .title(gettext("Group messages into conversations"))
            .subtitle(gettext(
                "Replies appear together with the message they answer",
            ))
            .active(prefs.group_conversations)
            .build();
        {
            let window = Rc::downgrade(window);
            group_switch.connect_active_notify(move |row| {
                if let Some(window) = window.upgrade() {
                    window.set_group_conversations(row.is_active());
                }
            });
        }
        behavior.add(&group_switch);
        general.add(&behavior);
        let notify_group = adw::PreferencesGroup::builder()
            .title(gettext("Notifications"))
            .build();
        let notify_switch = adw::SwitchRow::builder()
            .title(gettext("Notify about new mail"))
            .subtitle(gettext(
                "Through the notifications of the system, never as a notice inside the app",
            ))
            .active(prefs.notify.enabled)
            .build();
        notify_group.add(&notify_switch);
        general.add(&notify_group);
        let privacy = adw::PreferencesGroup::builder()
            .title(gettext("Privacy"))
            .build();
        let image_choices =
            gtk::StringList::new(&[&gettext("Always ask"), &gettext("Always show")]);
        let images_row = adw::ComboRow::builder()
            .title(gettext("External images"))
            .subtitle(gettext(
                "Images from other servers reveal that you opened a message",
            ))
            .model(&image_choices)
            .selected(u32::from(prefs.load_remote_images))
            .build();
        {
            let window = Rc::downgrade(window);
            images_row.connect_selected_notify(move |row| {
                if let Some(window) = window.upgrade() {
                    window.set_load_remote_images(row.selected() == 1);
                }
            });
        }
        privacy.add(&images_row);
        general.add(&privacy);

        let cleanup = adw::PreferencesGroup::builder()
            .title(gettext("Trash and Spam"))
            .build();
        for (role, title, on) in [
            (FolderRole::Trash, gettext("Empty trash"), prefs.empty_trash),
            (
                FolderRole::Junk,
                gettext("Empty spam folder"),
                prefs.empty_spam,
            ),
        ] {
            let choices =
                gtk::StringList::new(&[&gettext("Never automatically"), &gettext("After 30 days")]);
            let row = adw::ComboRow::builder()
                .title(&title)
                .subtitle(gettext(
                    "Mail that has been there for 30 days is removed for good",
                ))
                .model(&choices)
                .selected(u32::from(on))
                .build();
            let window = Rc::downgrade(window);
            row.connect_selected_notify(move |row| {
                if let Some(window) = window.upgrade() {
                    window.set_cleanup(role, row.selected() == 1);
                }
            });
            cleanup.add(&row);
        }
        general.add(&cleanup);
        dialog.add(&general);

        let signature_group = adw::PreferencesGroup::builder()
            .description(gettext(
                "Formatting, links and pictures such as a logo are possible. The signature is added below new messages and replies automatically. Every account has its own; switch the account in the folder sidebar to edit another one.",
            ))
            .build();
        let signature_page = adw::PreferencesPage::builder()
            .title(gettext("Signature"))
            .icon_name("document-edit-symbolic")
            .build();
        signature_page.add(&signature_group);
        dialog.add(&signature_page);
        dialog.add(&crate::shortcuts::page());

        let this = Rc::new(Settings {
            dialog,
            window: Rc::downgrade(window),
            list,
            rows: RefCell::default(),
            edit: RefCell::default(),
            signature_group,
            signature: RefCell::default(),
            signature_save: RefCell::default(),
            notify_group,
            notify_rows: RefCell::default(),
            notify_switch,
        });
        let weak = Rc::downgrade(&this);
        let tray = tray_switch.clone();
        background_switch.connect_active_notify(move |row| {
            tray.set_sensitive(row.is_active());
            if let Some(window) = weak.upgrade().and_then(|this| this.window.upgrade()) {
                window.set_run_in_background(row.is_active());
            }
        });
        let weak = Rc::downgrade(&this);
        tray_switch.connect_active_notify(move |row| {
            if let Some(window) = weak.upgrade().and_then(|this| this.window.upgrade()) {
                window.set_show_tray_icon(row.is_active());
            }
        });
        let weak = Rc::downgrade(&this);
        this.notify_switch.connect_active_notify(move |row| {
            if let Some(this) = weak.upgrade() {
                if let Some(window) = this.window.upgrade() {
                    window.set_notifications(row.is_active());
                }
                this.refresh_notifications();
            }
        });
        this.refresh();
        let weak = Rc::downgrade(&this);
        this.dialog.connect_closed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.save_signature();
            }
        });
        this
    }

    /// One switch per account below the main switch, and the way to the
    /// system settings for sound and "Do Not Disturb".
    fn refresh_notifications(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        for row in self.notify_rows.take() {
            self.notify_group.remove(&row);
        }
        let prefs = window.prefs();
        let mut rows: Vec<gtk::Widget> = Vec::new();
        for account in window.accounts() {
            let row = adw::SwitchRow::builder()
                .title(glib::markup_escape_text(&account.label))
                .subtitle(glib::markup_escape_text(&account.email))
                .active(!prefs.notify.muted.contains(&account.id))
                .sensitive(prefs.notify.enabled)
                .build();
            row.add_prefix(&adw::Avatar::new(32, Some(&account.label), true));
            let weak = self.window.clone();
            let id = account.id;
            row.connect_active_notify(move |row| {
                if let Some(window) = weak.upgrade() {
                    window.set_account_notifications(id, row.is_active());
                }
            });
            self.notify_group.add(&row);
            rows.push(row.upcast());
        }
        let system = adw::ActionRow::builder()
            .title(gettext("Sound, lock screen and “Do Not Disturb”"))
            .subtitle(gettext(
                "Set these in the system settings under “Notifications”",
            ))
            .build();
        if let Some(command) = crate::background::system_notification_settings() {
            let open = gtk::Button::builder()
                .label(gettext("System Settings"))
                .valign(gtk::Align::Center)
                .build();
            open.connect_clicked(move |_| {
                if let Err(error) = glib::spawn_command_line_async(command) {
                    tracing::warn!("could not open the system settings: {error}");
                }
            });
            system.add_suffix(&open);
        }
        self.notify_group.add(&system);
        rows.push(system.upcast());
        *self.notify_rows.borrow_mut() = rows;
    }

    /// Shows the signature of the active account for editing.
    fn refresh_signature(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(account) = window.active().and_then(|id| window.account(id)) else {
            return;
        };
        if self
            .signature
            .borrow()
            .as_ref()
            .is_some_and(|(id, ..)| *id == account.id)
        {
            return;
        }
        self.save_signature();
        if let Some((_, _, old)) = self.signature.take() {
            self.signature_group.remove(&old);
        }
        self.signature_group
            .set_title(&glib::markup_escape_text(&fmt(
                &gettext("Signature for “{}”"),
                &[&account.label],
            )));
        let editor = Editor::new(&window.signature(account.id));
        editor.web.set_height_request(200);
        editor.web.set_vexpand(false);
        editor
            .web
            .update_property(&[gtk::accessible::Property::Label(&gettext("Signature"))]);
        let frame = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card", "signature-editor"])
            .overflow(gtk::Overflow::Hidden)
            .build();
        frame.append(&editor.web);
        frame.append(&editor.toolbar);
        let weak = Rc::downgrade(self);
        editor.connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.schedule_signature_save();
            }
        });
        self.signature_group.add(&frame);
        *self.signature.borrow_mut() = Some((account.id, editor, frame.upcast()));
    }

    fn schedule_signature_save(self: &Rc<Self>) {
        if let Some(source) = self.signature_save.take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        let source =
            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                if let Some(this) = weak.upgrade() {
                    this.signature_save.take();
                    this.write_signature();
                }
            });
        *self.signature_save.borrow_mut() = Some(source);
    }

    /// Saves a pending change of the signature right away.
    fn save_signature(&self) {
        if let Some(source) = self.signature_save.take() {
            source.remove();
            self.write_signature();
        }
    }

    fn write_signature(&self) {
        let Some((account, editor, _)) = self.signature.borrow().clone() else {
            return;
        };
        let window = self.window.clone();
        glib::spawn_future_local(async move {
            let html = editor.html().await;
            if let Some(window) = window.upgrade() {
                window.set_signature(account, clean_signature(&html));
            }
        });
    }

    /// Rebuilds the list of accounts.
    pub fn refresh(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        self.refresh_signature();
        self.refresh_notifications();
        for row in self.rows.take() {
            self.list.remove(&row);
        }
        let active = window.active();
        let mut rows = Vec::new();
        for account in window.accounts() {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&account.label))
                .subtitle(glib::markup_escape_text(&account.email))
                .activatable(true)
                .build();
            row.add_prefix(&adw::Avatar::new(36, Some(&account.label), true));
            if Some(account.id) == active {
                row.add_suffix(
                    &gtk::Label::builder()
                        .label(gettext("Active"))
                        .valign(gtk::Align::Center)
                        .css_classes(["count-pill", "caption"])
                        .build(),
                );
            }
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let weak = Rc::downgrade(self);
            let id = account.id;
            row.connect_activated(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.open_account(id);
                }
            });
            self.list.add(&row);
            rows.push(row.upcast());
        }

        let add = adw::ActionRow::builder()
            .title(gettext("Add Account"))
            .subtitle(gettext(
                "Posteo, GMX, WEB.DE, Gmail, iCloud or your own server",
            ))
            .activatable(true)
            .action_name("app.add-account")
            .build();
        let plus = gtk::Image::from_icon_name("list-add-symbolic");
        plus.set_size_request(36, 36);
        add.add_prefix(&plus);
        self.list.add(&add);
        rows.push(add.upcast());

        let help = gtk::Label::builder()
            .label(gettext(
                "Choose an account to change its name, password or server settings, or to remove it.",
            ))
            .xalign(0.0)
            .wrap(true)
            .margin_top(12)
            .css_classes(["dim-label", "caption"])
            .build();
        self.list.add(&help);
        rows.push(help.upcast());
        *self.rows.borrow_mut() = rows;
    }

    /// Updates the connection line of an open "Edit Account" page.
    pub fn refresh_status(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if let Some(edit) = self.edit.borrow().as_ref() {
            fill_connection(
                &window,
                edit.account,
                &edit.connection,
                &edit.connection_suffix,
            );
        }
    }

    fn open_account(self: &Rc<Self>, id: AccountId) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(edit) = self.build_edit_page(&window, id) else {
            return;
        };
        self.dialog.push_subpage(&edit.page);
        let weak = Rc::downgrade(self);
        edit.page.connect_hidden(move |_| {
            if let Some(this) = weak.upgrade() {
                this.edit.take();
                this.refresh();
            }
        });
        *self.edit.borrow_mut() = Some(edit);
    }

    fn build_edit_page(self: &Rc<Self>, window: &Rc<Window>, id: AccountId) -> Option<EditPage> {
        let account = window.account(id)?;
        let content = gtk::Box::new(gtk::Orientation::Vertical, 24);

        // Header with the account's initials, name and address.
        let avatar = adw::Avatar::new(56, Some(&account.label), true);
        let title = gtk::Label::builder()
            .label(&account.label)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["title-2"])
            .build();
        let head_text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        head_text.set_valign(gtk::Align::Center);
        head_text.append(&title);
        head_text.append(
            &gtk::Label::builder()
                .label(&account.email)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["dim-label"])
                .build(),
        );
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        head.append(&avatar);
        head.append(&head_text);
        content.append(&head);

        // General.
        let general = adw::PreferencesGroup::builder()
            .title(gettext("General"))
            .build();
        let label_entry = suffix_entry(&account.label);
        let label_row = adw::ActionRow::builder()
            .title(gettext("Account name"))
            .subtitle(gettext("Only visible to you"))
            .activatable_widget(&label_entry)
            .build();
        label_row.add_suffix(&label_entry);
        let name_entry = suffix_entry(&account.display_name);
        let name_row = adw::ActionRow::builder()
            .title(gettext("Your name"))
            .subtitle(gettext("This is how recipients see you"))
            .activatable_widget(&name_entry)
            .build();
        name_row.add_suffix(&name_entry);
        let email_row = adw::ActionRow::builder()
            .title(gettext("Email address"))
            .subtitle(gettext(
                "To use a different address, please add a new account",
            ))
            .build();
        email_row.add_suffix(
            &gtk::Label::builder()
                .label(&account.email)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .css_classes(["dim-label"])
                .selectable(true)
                .build(),
        );
        general.add(&label_row);
        general.add(&name_row);
        general.add(&email_row);
        content.append(&general);
        {
            let window = Rc::downgrade(window);
            let (avatar, title) = (avatar.clone(), title.clone());
            label_entry.connect_changed(move |entry| {
                let text = entry.text();
                let label = text.trim();
                if label.is_empty() {
                    return;
                }
                title.set_label(label);
                avatar.set_text(Some(label));
                if let Some(window) = window.upgrade() {
                    window.set_label(id, label);
                }
            });
        }
        {
            let window = Rc::downgrade(window);
            name_entry.connect_changed(move |entry| {
                if let Some(window) = window.upgrade() {
                    window.set_display_name(id, entry.text().trim());
                }
            });
        }

        // Sign-in: connection state and password.
        let sign_in = adw::PreferencesGroup::builder()
            .title(gettext("Sign-in"))
            .build();
        let connection = adw::ActionRow::builder()
            .title(gettext("Connection"))
            .build();
        let connection_suffix = gtk::Box::builder()
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        connection.add_suffix(&connection_suffix);
        fill_connection(window, id, &connection, &connection_suffix);
        sign_in.add(&connection);

        let change = gtk::Button::builder()
            .label(gettext("Change"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let password_row = adw::ActionRow::builder()
            .title(gettext("Password"))
            .subtitle(gettext("Stored in the keyring"))
            .build();
        password_row.add_suffix(&change);
        sign_in.add(&password_row);
        let new_password = adw::PasswordEntryRow::builder()
            .title(gettext("New password"))
            .visible(false)
            .build();
        sign_in.add(&new_password);
        let (password_actions, password_error, cancel_password, save_password) =
            action_row(&gettext("Save and Check"));
        password_actions.set_visible(false);
        sign_in.add(&password_actions);
        content.append(&sign_in);

        let show_password_form = {
            let (change, new_password, password_actions, password_error) = (
                change.clone(),
                new_password.clone(),
                password_actions.clone(),
                password_error.clone(),
            );
            move |show: bool| {
                change.set_visible(!show);
                new_password.set_visible(show);
                password_actions.set_visible(show);
                password_error.set_visible(false);
                new_password.set_text("");
                if show {
                    new_password.grab_focus();
                }
            }
        };
        let show_password_form = Rc::new(show_password_form);
        {
            let show = show_password_form.clone();
            change.connect_clicked(move |_| show(true));
        }
        {
            let show = show_password_form.clone();
            cancel_password.connect_clicked(move |_| show(false));
        }
        let submit_password = {
            let window = Rc::downgrade(window);
            let settings = Rc::downgrade(self);
            let (new_password, password_error, save_password) = (
                new_password.clone(),
                password_error.clone(),
                save_password.clone(),
            );
            let show = show_password_form.clone();
            move || {
                let password = new_password.text().to_string();
                if password.is_empty() {
                    show_error(&password_error, &gettext("Please enter a new password."));
                    return;
                }
                let Some(window) = window.upgrade() else {
                    return;
                };
                save_password.set_sensitive(false);
                let (settings, password_error, save_password, show) = (
                    settings.clone(),
                    password_error.clone(),
                    save_password.clone(),
                    show.clone(),
                );
                glib::spawn_future_local(async move {
                    let result = window.clone().change_password(id, password).await;
                    save_password.set_sensitive(true);
                    match result {
                        Ok(()) => {
                            show(false);
                            if settings.upgrade().is_some() {
                                window.toast(&gettext("Password changed and checked successfully"));
                            }
                        }
                        Err(message) => show_error(&password_error, &message),
                    }
                });
            }
        };
        let submit_password = Rc::new(submit_password);
        {
            let submit = submit_password.clone();
            save_password.connect_clicked(move |_| submit());
        }
        new_password.connect_entry_activated(move |_| submit_password());

        // Server settings, folded away because they are rarely needed.
        let server_group = adw::PreferencesGroup::new();
        let expander = adw::ExpanderRow::builder()
            .title(gettext("Server settings"))
            .subtitle(gettext("Only change if your provider asks you to"))
            .build();
        server_group.add(&expander);
        let imap = ServerFields::new(&gettext("Incoming mail (IMAP)"), &account.settings.imap);
        let smtp = ServerFields::new(&gettext("Outgoing mail (SMTP)"), &account.settings.smtp);
        imap.host
            .set_title(&gettext("Server for incoming mail (IMAP)"));
        smtp.host
            .set_title(&gettext("Server for outgoing mail (SMTP)"));
        for fields in [&imap, &smtp] {
            fields.group.remove(&fields.host);
            fields.group.remove(&fields.port);
            fields.group.remove(&fields.security);
            expander.add_row(&fields.host);
            expander.add_row(&fields.port);
            expander.add_row(&fields.security);
        }
        let username = adw::EntryRow::builder()
            .title(gettext("User name"))
            .text(account.settings.username.as_str())
            .build();
        expander.add_row(&username);
        let (server_actions, server_error, reset_server, save_server) =
            action_row(&gettext("Save and Check"));
        reset_server.set_label(&gettext("Reset"));
        expander.add_row(&server_actions);
        content.append(&server_group);
        {
            let original = account.settings.clone();
            let (imap_host, imap_port, imap_security) =
                (imap.host.clone(), imap.port.clone(), imap.security.clone());
            let (smtp_host, smtp_port, smtp_security) =
                (smtp.host.clone(), smtp.port.clone(), smtp.security.clone());
            let username = username.clone();
            let server_error = server_error.clone();
            reset_server.connect_clicked(move |_| {
                imap_host.set_text(&original.imap.host);
                imap_port.set_value(f64::from(original.imap.port));
                imap_security.set_selected(security_index(original.imap.security));
                smtp_host.set_text(&original.smtp.host);
                smtp_port.set_value(f64::from(original.smtp.port));
                smtp_security.set_selected(security_index(original.smtp.security));
                username.set_text(&original.username);
                server_error.set_visible(false);
            });
        }
        {
            let window = Rc::downgrade(window);
            let settings = Rc::downgrade(self);
            let server_error = server_error.clone();
            let save = save_server.clone();
            save_server.connect_clicked(move |_| {
                let new = ServerSettings {
                    imap: imap.config(),
                    smtp: smtp.config(),
                    username: username.text().trim().to_owned(),
                };
                if new.imap.host.is_empty() || new.smtp.host.is_empty() || new.username.is_empty() {
                    show_error(
                        &server_error,
                        &gettext("Please fill in the server names and the user name."),
                    );
                    return;
                }
                let Some(window) = window.upgrade() else {
                    return;
                };
                save.set_sensitive(false);
                let (settings, server_error, save) =
                    (settings.clone(), server_error.clone(), save.clone());
                glib::spawn_future_local(async move {
                    let result = window.clone().change_settings(id, new).await;
                    save.set_sensitive(true);
                    match result {
                        Ok(()) => {
                            server_error.set_visible(false);
                            if settings.upgrade().is_some() {
                                window.toast(&gettext("Server settings saved and checked"));
                            }
                        }
                        Err(message) => show_error(&server_error, &message),
                    }
                });
            });
        }

        // Remove, with the question right on the page.
        let remove = gtk::Button::builder()
            .label(gettext("Remove Account"))
            .halign(gtk::Align::Center)
            .css_classes(["destructive-action", "pill"])
            .build();
        let confirm = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .visible(false)
            .css_classes(["card", "account-confirm"])
            .accessible_role(gtk::AccessibleRole::Alert)
            .build();
        confirm.append(
            &gtk::Label::builder()
                .label(fmt(&gettext("Really remove “{}”?"), &[&account.label]))
                .xalign(0.0)
                .wrap(true)
                .css_classes(["heading"])
                .build(),
        );
        confirm.append(
            &gtk::Label::builder()
                .label(gettext(
                    "Your emails stay with your provider. This app only forgets the account, the data stored on this device and the password in the keyring.",
                ))
                .xalign(0.0)
                .wrap(true)
                .build(),
        );
        let confirm_buttons = gtk::Box::builder()
            .spacing(8)
            .halign(gtk::Align::End)
            .build();
        let keep = gtk::Button::builder()
            .label(gettext("Cancel"))
            .css_classes(["flat"])
            .build();
        let remove_now = gtk::Button::builder()
            .label(gettext("Remove"))
            .css_classes(["destructive-action"])
            .build();
        confirm_buttons.append(&keep);
        confirm_buttons.append(&remove_now);
        confirm.append(&confirm_buttons);
        content.append(&remove);
        content.append(&confirm);
        {
            let (confirm, remove_button, keep) = (confirm.clone(), remove.clone(), keep.clone());
            remove.connect_clicked(move |_| {
                remove_button.set_visible(false);
                confirm.set_visible(true);
                keep.grab_focus();
            });
        }
        {
            let (confirm, remove) = (confirm.clone(), remove.clone());
            keep.connect_clicked(move |_| {
                confirm.set_visible(false);
                remove.set_visible(true);
                remove.grab_focus();
            });
        }
        {
            let window = Rc::downgrade(window);
            let settings = Rc::downgrade(self);
            let done = Cell::new(false);
            remove_now.connect_clicked(move |_| {
                if done.replace(true) {
                    return;
                }
                if let Some(settings) = settings.upgrade() {
                    settings.edit.take();
                    settings.dialog.pop_subpage();
                }
                if let Some(window) = window.upgrade() {
                    window.remove_account(id);
                }
            });
        }

        let clamp = adw::Clamp::builder()
            .maximum_size(600)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .child(&content)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&scroller));
        let page = adw::NavigationPage::builder()
            .title(gettext("Edit Account"))
            .child(&view)
            .build();
        Some(EditPage {
            account: id,
            page,
            connection,
            connection_suffix,
        })
    }
}

fn security_index(security: e_mail_core::model::Security) -> u32 {
    match security {
        e_mail_core::model::Security::Tls => 0,
        e_mail_core::model::Security::StartTls => 1,
    }
}

fn suffix_entry(text: &str) -> gtk::Entry {
    gtk::Entry::builder()
        .text(text)
        .valign(gtk::Align::Center)
        .width_chars(14)
        .build()
}

fn show_error(label: &gtk::Label, message: &str) {
    label.set_label(message);
    label.set_visible(true);
}

/// A row with an error line and the buttons "Cancel" and a main button.
fn action_row(main: &str) -> (gtk::ListBoxRow, gtk::Label, gtk::Button, gtk::Button) {
    let error = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .visible(false)
        .css_classes(["error"])
        .accessible_role(gtk::AccessibleRole::Alert)
        .build();
    let cancel = gtk::Button::builder()
        .label(gettext("Cancel"))
        .css_classes(["flat"])
        .build();
    let save = gtk::Button::builder()
        .label(main)
        .css_classes(["suggested-action"])
        .build();
    let buttons = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    buttons.append(&cancel);
    buttons.append(&save);
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&error);
    content.append(&buttons);
    let row = gtk::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&content)
        .build();
    (row, error, cancel, save)
}

fn fill_connection(window: &Window, id: AccountId, row: &adw::ActionRow, suffix: &gtk::Box) {
    let status = window.status(id);
    row.set_subtitle(&match window.last_sync(id) {
        Some(time) => fmt(
            &gettext("Last synced at {}"),
            &[&time.format("%H:%M").to_string()],
        ),
        None => gettext("Not synced yet"),
    });
    while let Some(child) = suffix.first_child() {
        suffix.remove(&child);
    }
    let dot = widgets::status_dot(status.as_ref());
    dot.set_valign(gtk::Align::Center);
    suffix.append(&dot);
    suffix.append(&gtk::Label::new(Some(&widgets::status_text(
        status.as_ref(),
    ))));
}

/// An editor that only holds an empty line counts as no signature.
fn clean_signature(html: &str) -> &str {
    let bare = html
        .replace("<br>", "")
        .replace("<div>", "")
        .replace("</div>", "")
        .replace("&nbsp;", "");
    if bare.trim().is_empty() { "" } else { html }
}
