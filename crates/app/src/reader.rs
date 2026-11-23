// SPDX-License-Identifier: GPL-3.0-or-later

//! The reading pane shows a whole conversation as a column of cards, oldest
//! on top. Only one thing is open at a time: one message (at first the
//! newest, or the oldest unread one) or the reply editor, while all other
//! messages are folded. Every open card has its own actions.
//!
//! Message bodies are shown in WebKit. Scripts in the message itself are
//! removed and the sanitised document forbids loading anything from the
//! network unless the user allows images. The only script that runs is our
//! own, in a separate world, to report the height of the content so the
//! card can grow to fit it.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use e_mail_core::model::{MessageId, MessageSummary, ThreadId};
use e_mail_core::parse::Attachment;
use gettextrs::gettext;
use gtk::prelude::*;
use gtk::{gdk, glib};
use webkit::prelude::*;

use crate::config::APP_ID;
use crate::message_list::{format_long_time, format_message_time, format_time};
use crate::widgets::fmt;

const WORLD: &str = "e-mail";
const HEIGHT_SCRIPT: &str = "(() => {
  const send = () => window.webkit.messageHandlers.height.postMessage(
    Math.ceil(document.documentElement.getBoundingClientRect().height));
  new ResizeObserver(send).observe(document.documentElement);
  send();
})();";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardAction {
    Reply,
    ReplyAll,
    Forward,
    ToggleStar,
    ToggleRead,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageChoice {
    Once,
    AlwaysForDomain,
}

/// A message body prepared for display, away from the main thread.
#[derive(Clone, Debug)]
pub struct BodyContent {
    pub document: String,
    pub has_remote_content: bool,
    pub attachments: Vec<Attachment>,
}

struct OpenCard {
    star: gtk::Button,
    read: gtk::Button,
    body: gtk::Stack,
    web: webkit::WebView,
    error: adw::StatusPage,
    banner: gtk::Box,
    attachments: gtk::FlowBox,
    loaded: Cell<bool>,
}

struct Card {
    message: RefCell<MessageSummary>,
    frame: gtk::Box,
    open: RefCell<Option<OpenCard>>,
}

type ActionCallback = Box<dyn Fn(CardAction, MessageId)>;
type BodyCallback = Box<dyn Fn(MessageId)>;
type ImageCallback = Box<dyn Fn(MessageId, ImageChoice)>;
type AttachmentCallback = Box<dyn Fn(MessageId, Attachment, bool)>;

pub struct Reader {
    pub widget: gtk::Stack,
    pub title: adw::WindowTitle,
    pub menu_button: gtk::MenuButton,
    scroller: gtk::ScrolledWindow,
    link_preview: gtk::Label,
    subject: gtk::Label,
    folder_tag: gtk::Label,
    cards_box: gtk::Box,
    cards: RefCell<Vec<Rc<Card>>>,
    /// The inline reply editor and the message it answers.
    editor: RefCell<Option<(gtk::Widget, MessageId)>>,
    reply_button: gtk::Button,
    reply_label: gtk::Label,
    thread: Cell<Option<ThreadId>>,
    /// The one older message that is open.
    open_older: Cell<Option<MessageId>>,
    own_address: RefCell<String>,
    own_name: RefCell<String>,
    on_action: RefCell<Option<ActionCallback>>,
    on_need_body: RefCell<Option<BodyCallback>>,
    on_images: RefCell<Option<ImageCallback>>,
    on_attachment: RefCell<Option<AttachmentCallback>>,
    weak: RefCell<Weak<Reader>>,
    /// Opening by the user slides the message in.
    animate: Cell<bool>,
}

impl Reader {
    pub fn new() -> Rc<Reader> {
        let empty = adw::StatusPage::builder()
            .icon_name(format!("{APP_ID}-symbolic"))
            .title(gettext("No Conversation Selected"))
            .description(gettext("Choose a message on the left to read it."))
            .css_classes(["compact"])
            .build();

        let subject = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .css_classes(["title-2"])
            .build();
        let folder_tag = gtk::Label::builder()
            .css_classes(["folder-tag", "caption"])
            .build();
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        line.append(&folder_tag);
        let cards_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
        let reply_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let reply_content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        reply_content.append(&gtk::Image::from_icon_name("mail-reply-sender-symbolic"));
        reply_content.append(&reply_label);
        let reply_button = gtk::Button::builder()
            .child(&reply_content)
            .css_classes(["card", "reply-placeholder"])
            .visible(false)
            .build();
        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_start(18)
            .margin_end(18)
            .margin_top(18)
            .margin_bottom(24)
            .build();
        column.append(&subject);
        column.append(&line);
        column.append(&cards_box);
        column.append(&reply_button);
        let clamp = adw::Clamp::builder()
            .maximum_size(900)
            .tightening_threshold(600)
            .child(&column)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        let widget = gtk::Stack::new();
        widget.add_named(&empty, Some("empty"));
        // Shows where a link really leads while the pointer is on it, so a
        // link labelled "Your bank" pointing elsewhere stands out.
        let link_preview = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::End)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(80)
            .margin_start(8)
            .margin_bottom(8)
            .can_target(false)
            .visible(false)
            .css_classes(["link-preview", "caption"])
            .build();
        let conversation = gtk::Overlay::builder().child(&scroller).build();
        conversation.add_overlay(&link_preview);
        widget.add_named(&conversation, Some("conversation"));

        let title = adw::WindowTitle::new("", "");
        let menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text(gettext("Conversation Menu"))
            .visible(false)
            .build();

        let this = Rc::new(Reader {
            widget,
            title,
            menu_button,
            scroller,
            link_preview,
            subject,
            folder_tag,
            cards_box,
            cards: RefCell::default(),
            editor: RefCell::default(),
            reply_button,
            reply_label,
            thread: Cell::new(None),
            open_older: Cell::default(),
            animate: Cell::new(false),
            own_address: RefCell::default(),
            own_name: RefCell::default(),
            on_action: RefCell::default(),
            on_need_body: RefCell::default(),
            on_images: RefCell::default(),
            on_attachment: RefCell::default(),
            weak: RefCell::default(),
        });
        *this.weak.borrow_mut() = Rc::downgrade(&this);
        let weak = Rc::downgrade(&this);
        adw::StyleManager::default().connect_dark_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                for card in this.cards.borrow().iter() {
                    if let Some(open) = card.open.borrow().as_ref()
                        && let Some(manager) = open.web.user_content_manager()
                    {
                        apply_theme(&manager);
                    }
                }
            }
        });
        let weak = Rc::downgrade(&this);
        this.reply_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade()
                && let Some(id) = this.reply_target()
                && let Some(callback) = this.on_action.borrow().as_ref()
            {
                callback(CardAction::Reply, id);
            }
        });
        this
    }

    /// The newest message written by someone else, which "Reply" at the
    /// end of the conversation answers.
    pub fn reply_target(&self) -> Option<MessageId> {
        let own = self.own_address.borrow().clone();
        let cards = self.cards.borrow();
        cards
            .iter()
            .rev()
            .find(|card| {
                !card
                    .message
                    .borrow()
                    .from_address
                    .eq_ignore_ascii_case(&own)
            })
            .or(cards.last())
            .map(|card| card.message.borrow().id)
    }

    fn update_reply_button(&self) {
        let target = self.reply_target().and_then(|id| self.card(id));
        let has_editor = self.editor.borrow().is_some();
        match target {
            Some(card) if !has_editor => {
                let name = self.sender_text(&card.message.borrow());
                self.reply_label
                    .set_label(&fmt(&gettext("Reply to {} …"), &[&name]));
                self.reply_button.set_visible(true);
            }
            _ => self.reply_button.set_visible(false),
        }
    }

    /// Shows the reply editor below the message it answers, or removes it.
    pub fn set_editor(&self, editor: Option<(gtk::Widget, MessageId)>) {
        if let Some((old, _)) = self.editor.take() {
            self.cards_box.remove(&old);
        }
        // While writing, the editor fills the rest of the pane.
        self.cards_box.set_vexpand(editor.is_some());
        let had_editor = self.editor.borrow().is_some();
        if let Some((widget, message)) = editor {
            self.cards_box.append(&widget);
            *self.editor.borrow_mut() = Some((widget, message));
            self.place_editor();
            // Writing takes the whole pane: every message folds.
            if let Some(active) = self.open_older.take()
                && let Some(card) = self.card(active)
            {
                self.fold_card(&card);
            }
            self.keep_in_view(message);
        } else if had_editor
            && self.open_older.get().is_none()
            && let Some(newest) = self.newest()
        {
            self.expand_older(newest);
        }
        self.update_reply_button();
    }

    fn place_editor(&self) {
        let editor = self.editor.borrow();
        let Some((widget, message)) = editor.as_ref() else {
            return;
        };
        let after = self
            .card(*message)
            .map(|card| card.frame.clone())
            .or_else(|| self.cards.borrow().last().map(|card| card.frame.clone()));
        self.cards_box.reorder_child_after(widget, after.as_ref());
    }

    pub fn connect_action(&self, callback: impl Fn(CardAction, MessageId) + 'static) {
        *self.on_action.borrow_mut() = Some(Box::new(callback));
    }

    /// Asked for whenever an open card has no body yet.
    pub fn connect_need_body(&self, callback: impl Fn(MessageId) + 'static) {
        *self.on_need_body.borrow_mut() = Some(Box::new(callback));
    }

    pub fn connect_images(&self, callback: impl Fn(MessageId, ImageChoice) + 'static) {
        *self.on_images.borrow_mut() = Some(Box::new(callback));
    }

    /// Called with true to open an attachment, false to save it.
    pub fn connect_attachment(&self, callback: impl Fn(MessageId, Attachment, bool) + 'static) {
        *self.on_attachment.borrow_mut() = Some(Box::new(callback));
    }

    pub fn thread(&self) -> Option<ThreadId> {
        self.thread.get()
    }

    /// The messages on screen, oldest first.
    pub fn messages(&self) -> Vec<MessageSummary> {
        self.cards
            .borrow()
            .iter()
            .map(|card| card.message.borrow().clone())
            .collect()
    }

    pub fn show_empty(&self) {
        self.set_editor(None);
        self.reply_button.set_visible(false);
        self.thread.set(None);
        self.open_older.set(None);
        self.clear_cards();
        self.title.set_title("");
        self.title.set_subtitle("");
        self.menu_button.set_visible(false);
        self.widget.set_visible_child_name("empty");
    }

    fn clear_cards(&self) {
        for card in self.cards.borrow_mut().drain(..) {
            self.cards_box.remove(&card.frame);
        }
    }

    /// Shows a conversation. Showing the same conversation again only
    /// updates what changed, so open messages are not loaded twice.
    pub fn show_conversation(
        &self,
        thread: ThreadId,
        subject: &str,
        folder: &str,
        messages: Vec<MessageSummary>,
        own_address: &str,
        own_name: &str,
    ) {
        if messages.is_empty() {
            self.show_empty();
            return;
        }
        *self.own_address.borrow_mut() = own_address.to_owned();
        *self.own_name.borrow_mut() = own_name.to_owned();
        let subject = if subject.is_empty() {
            gettext("(No subject)")
        } else {
            subject.to_owned()
        };
        self.subject.set_label(&subject);
        self.folder_tag.set_label(folder);
        // The subject and the cards say it all; the title bar stays empty.
        self.title.set_title("");
        self.title.set_subtitle("");
        self.menu_button.set_visible(true);
        self.widget.set_visible_child_name("conversation");

        let same_thread = self.thread.get() == Some(thread);
        let same_messages = same_thread && {
            let cards = self.cards.borrow();
            cards.len() == messages.len()
                && cards
                    .iter()
                    .zip(&messages)
                    .all(|(card, message)| card.message.borrow().id == message.id)
        };
        if same_messages {
            let cards = self.cards.borrow().clone();
            for (card, message) in cards.iter().zip(messages) {
                *card.message.borrow_mut() = message;
                self.update_card(card);
            }
            self.update_reply_button();
            return;
        }
        if !same_thread {
            // One message opens: the oldest unread one, else the newest.
            let first = messages
                .iter()
                .find(|m| !m.flags.seen)
                .or(messages.last())
                .map(|m| m.id);
            let editing = self.editor.borrow().is_some();
            self.open_older.set(if editing { None } else { first });
            self.scroller.vadjustment().set_value(0.0);
        } else if self.editor.borrow().is_none()
            && !self
                .open_older
                .get()
                .is_some_and(|id| messages.iter().any(|m| m.id == id))
        {
            self.open_older.set(messages.last().map(|m| m.id));
        }
        self.thread.set(Some(thread));
        self.clear_cards();
        let last = messages.len() - 1;
        for (index, message) in messages.into_iter().enumerate() {
            let card = Rc::new(Card {
                message: RefCell::new(message),
                frame: gtk::Box::new(gtk::Orientation::Vertical, 0),
                open: RefCell::default(),
            });
            self.cards_box.append(&card.frame);
            self.cards.borrow_mut().push(card.clone());
            let id = card.message.borrow().id;
            if self.open_older.get() == Some(id) {
                self.open_card(&card, index == last, last > 0);
            } else {
                self.fold_card(&card);
            }
        }
        self.place_editor();
        self.update_reply_button();
        if !same_thread && last > 0 {
            // Bring the newest message into view once the cards have a size.
            let weak = self.weak.borrow().clone();
            glib::idle_add_local_once(move || {
                if let Some(this) = weak.upgrade()
                    && let Some(card) = this.cards.borrow().last()
                {
                    let adjustment = this.scroller.vadjustment();
                    if let Some(point) = card
                        .frame
                        .compute_point(&this.cards_box, &gtk::graphene::Point::new(0.0, 0.0))
                    {
                        adjustment.set_value(f64::from(point.y()).max(0.0));
                    }
                }
            });
        }
    }

    fn is_newest(&self, id: MessageId) -> bool {
        self.cards
            .borrow()
            .last()
            .is_some_and(|card| card.message.borrow().id == id)
    }

    fn newest(&self) -> Option<MessageId> {
        self.cards
            .borrow()
            .last()
            .map(|card| card.message.borrow().id)
    }

    /// Opens a message and folds the one open before.
    fn expand_older(&self, id: MessageId) {
        let Some(card) = self.card(id) else {
            return;
        };
        if let Some(previous) = self.open_older.replace(Some(id))
            && previous != id
            && let Some(old) = self.card(previous)
        {
            self.fold_card(&old);
        }
        if card.open.borrow().is_none() {
            self.animate.set(true);
            let newest = self.is_newest(id);
            self.open_card(&card, newest, self.cards.borrow().len() > 1);
            self.animate.set(false);
        }
        self.keep_in_view(id);
    }

    /// Folding an older message opens the newest again.
    fn fold_older(&self, id: MessageId) {
        if self.is_newest(id) {
            return;
        }
        if let Some(newest) = self.newest() {
            self.expand_older(newest);
        }
    }

    /// Scrolls up to a message whose top went out of view after the cards
    /// above it changed size.
    fn keep_in_view(&self, id: MessageId) {
        let weak = self.weak.borrow().clone();
        glib::idle_add_local_once(move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            let Some(card) = this.card(id) else {
                return;
            };
            let adjustment = this.scroller.vadjustment();
            if let Some(point) = card
                .frame
                .compute_point(&this.cards_box, &gtk::graphene::Point::new(0.0, 0.0))
            {
                let top = f64::from(point.y()) - 12.0;
                if top < adjustment.value() {
                    adjustment.set_value(top.max(0.0));
                }
            }
        });
    }

    /// The recipients with the user as "me".
    fn recipients_text(&self, recipients: &str) -> String {
        let address = self.own_address.borrow();
        let name = self.own_name.borrow();
        let is_me = |part: &str| {
            part.eq_ignore_ascii_case(&address) || (!name.is_empty() && part == name.as_str())
        };
        recipients
            .split(", ")
            .filter(|part| !part.is_empty())
            .map(|part| {
                if is_me(part) {
                    gettext("me")
                } else {
                    part.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn card(&self, id: MessageId) -> Option<Rc<Card>> {
        self.cards
            .borrow()
            .iter()
            .find(|card| card.message.borrow().id == id)
            .cloned()
    }

    fn sender_text(&self, message: &MessageSummary) -> String {
        if message
            .from_address
            .eq_ignore_ascii_case(&self.own_address.borrow())
        {
            gettext("Me")
        } else if message.from_name.is_empty() {
            message.from_address.clone()
        } else {
            message.from_name.clone()
        }
    }

    fn fold_card(&self, card: &Rc<Card>) {
        clear(&card.frame);
        card.open.borrow_mut().take();
        let message = card.message.borrow();
        let sender = self.sender_text(&message);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        content.append(&adw::Avatar::new(28, Some(&sender), true));
        let name = gtk::Label::builder()
            .label(&sender)
            .css_classes(["heading"])
            .build();
        content.append(&name);
        let snippet = gtk::Label::builder()
            .label(&message.preview)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        content.append(&snippet);
        content.append(
            &gtk::Label::builder()
                .label(format_time(message.date))
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        let button = gtk::Button::builder()
            .child(&content)
            .css_classes(["card", "message-card", "folded"])
            .tooltip_text(gettext("Show message"))
            .build();
        button.update_state(&[gtk::accessible::State::Expanded(Some(false))]);
        if !message.flags.seen {
            button.add_css_class("unread");
        }
        let weak = self.weak.borrow().clone();
        let id = message.id;
        button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.expand_older(id);
            }
        });
        card.frame.append(&button);
    }

    /// `newest` keeps the message open for good; `tagged` marks it as the
    /// newest of several.
    fn open_card(&self, card: &Rc<Card>, newest: bool, tagged: bool) {
        clear(&card.frame);
        let message = card.message.borrow().clone();
        let sender = self.sender_text(&message);

        let who = gtk::Box::new(gtk::Orientation::Vertical, 2);
        who.set_hexpand(true);
        who.set_valign(gtk::Align::Center);
        let full_sender = if message.from_name.is_empty() {
            message.from_address.clone()
        } else {
            format!("{} <{}>", message.from_name, message.from_address)
        };
        who.append(
            &gtk::Label::builder()
                .label(&sender)
                .tooltip_text(&full_sender)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["heading"])
                .build(),
        );
        let date = format_message_time(message.date);
        let recipients = self.recipients_text(&message.recipients);
        let details = if recipients.is_empty() {
            date
        } else {
            // Translators: "to <recipients> · <date>" below the sender.
            gettext("to {recipients} · {date}")
                .replace("{recipients}", &recipients)
                .replace("{date}", &date)
        };
        who.append(
            &gtk::Label::builder()
                .label(&details)
                .tooltip_text(format_long_time(message.date))
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        let head = gtk::Box::builder()
            .spacing(12)
            .margin_start(14)
            .margin_end(14)
            .margin_top(12)
            .margin_bottom(8)
            .build();
        head.append(&adw::Avatar::new(36, Some(&sender), true));
        head.append(&who);
        if newest && tagged {
            head.append(
                &gtk::Label::builder()
                    .label(gettext("Newest"))
                    .valign(gtk::Align::Center)
                    .css_classes(["newest-tag"])
                    .build(),
            );
        }

        // An older message folds again by clicking its head.
        let head: gtk::Widget = if newest {
            head.upcast()
        } else {
            let button = gtk::Button::builder()
                .child(&head)
                .css_classes(["flat", "message-head"])
                .tooltip_text(gettext("Hide message"))
                .build();
            button.update_state(&[gtk::accessible::State::Expanded(Some(true))]);
            let weak = self.weak.borrow().clone();
            let id = message.id;
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.fold_older(id);
                }
            });
            button.upcast()
        };
        let banner = self.image_banner(&message);
        let web = self.build_web_view();
        let error = adw::StatusPage::builder()
            .icon_name("dialog-warning-symbolic")
            .title(gettext("Message Could Not Be Loaded"))
            .css_classes(["compact"])
            .build();
        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .margin_top(24)
            .margin_bottom(24)
            .build();
        let body = gtk::Stack::builder()
            .vhomogeneous(false)
            .interpolate_size(true)
            .build();
        body.add_named(&spinner, Some("loading"));
        body.add_named(&web, Some("web"));
        body.add_named(&error, Some("error"));

        let attachments = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(6)
            .row_spacing(6)
            .max_children_per_line(4)
            .margin_start(14)
            .margin_end(14)
            .margin_bottom(6)
            .visible(false)
            .build();

        let actions = gtk::Box::builder()
            .spacing(4)
            .margin_start(8)
            .margin_end(8)
            .margin_top(4)
            .margin_bottom(6)
            .css_classes(["message-actions"])
            .build();
        actions.update_property(&[gtk::accessible::Property::Label(&gettext(
            "Actions for this message",
        ))]);
        for (icon, label, tooltip, action) in [
            (
                "mail-reply-sender-symbolic",
                gettext("Reply"),
                gettext("Reply to this message"),
                CardAction::Reply,
            ),
            (
                "mail-reply-all-symbolic",
                gettext("Reply All"),
                gettext("Reply to all recipients of this message"),
                CardAction::ReplyAll,
            ),
            (
                "mail-forward-symbolic",
                gettext("Forward"),
                gettext("Forward this message"),
                CardAction::Forward,
            ),
        ] {
            // Symbols only: labels did not fit in German or narrow windows.
            let button = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(&tooltip)
                .css_classes(["flat"])
                .build();
            button.update_property(&[gtk::accessible::Property::Label(&label)]);
            let weak = self.weak.borrow().clone();
            let id = message.id;
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade()
                    && let Some(callback) = this.on_action.borrow().as_ref()
                {
                    callback(action, id);
                }
            });
            actions.append(&button);
        }
        actions.append(&gtk::Box::builder().hexpand(true).build());
        let action_button = |icon: &str, tooltip: &str, action: CardAction| {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .css_classes(["flat"])
                .build();
            let weak = self.weak.borrow().clone();
            let id = message.id;
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade()
                    && let Some(callback) = this.on_action.borrow().as_ref()
                {
                    callback(action, id);
                }
            });
            actions.append(&button);
            button
        };
        let star = action_button("non-starred-symbolic", "", CardAction::ToggleStar);
        let read = action_button("mail-unread-symbolic", "", CardAction::ToggleRead);
        action_button(
            "user-trash-symbolic",
            &gettext("Delete this message"),
            CardAction::Delete,
        );

        let frame = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card", "message-card"])
            .build();
        if newest && tagged {
            frame.add_css_class("newest");
        }
        frame.append(&head);
        let rest = gtk::Box::new(gtk::Orientation::Vertical, 0);
        rest.append(&banner);
        rest.append(&body);
        rest.append(&attachments);
        rest.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        rest.append(&actions);
        let revealer = gtk::Revealer::builder()
            .child(&rest)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .transition_duration(180)
            .reveal_child(newest || !self.animate.get())
            .build();
        frame.append(&revealer);
        card.frame.append(&frame);
        if !revealer.reveals_child() {
            glib::idle_add_local_once(move || revealer.set_reveal_child(true));
        }

        *card.open.borrow_mut() = Some(OpenCard {
            star,
            read,
            body,
            web,
            error,
            banner,
            attachments,
            loaded: Cell::new(false),
        });
        self.update_card(card);
        if let Some(callback) = self.on_need_body.borrow().as_ref() {
            callback(message.id);
        }
    }

    fn image_banner(&self, message: &MessageSummary) -> gtk::Box {
        let text = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        text.append(
            &gtk::Image::builder()
                .icon_name("image-x-generic-symbolic")
                .valign(gtk::Align::Start)
                .build(),
        );
        text.append(
            &gtk::Label::builder()
                .label(gettext(
                    "External images were blocked so the sender cannot see that you opened this message.",
                ))
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .hexpand(true)
                .width_chars(10)
                .css_classes(["caption"])
                .build(),
        );
        let buttons = gtk::Box::builder()
            .spacing(6)
            .halign(gtk::Align::End)
            .build();
        let once = gtk::Button::builder()
            .child(
                &gtk::Label::builder()
                    .label(gettext("Show Once"))
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
            )
            .build();
        buttons.append(&once);
        let domain = sender_domain(&message.from_address);
        let always = gtk::Button::builder()
            .child(
                &gtk::Label::builder()
                    .label(fmt(&gettext("Always for {}"), &[&domain]))
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .build(),
            )
            .build();
        if !domain.is_empty() {
            buttons.append(&always);
        }
        let banner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(14)
            .margin_end(14)
            .margin_bottom(6)
            .css_classes(["image-banner"])
            .visible(false)
            .build();
        banner.append(&text);
        banner.append(&buttons);
        for (button, choice) in [
            (&once, ImageChoice::Once),
            (&always, ImageChoice::AlwaysForDomain),
        ] {
            let weak = self.weak.borrow().clone();
            let id = message.id;
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade()
                    && let Some(callback) = this.on_images.borrow().as_ref()
                {
                    callback(id, choice);
                }
            });
        }
        banner
    }

    fn update_card(&self, card: &Rc<Card>) {
        let message = card.message.borrow();
        let open = card.open.borrow();
        let Some(open) = open.as_ref() else {
            drop(open);
            drop(message);
            // Folded cards are cheap to rebuild.
            self.fold_card(card);
            return;
        };
        if let Some(frame) = card.frame.first_child() {
            if message.flags.seen {
                frame.remove_css_class("unread");
            } else {
                frame.add_css_class("unread");
            }
        }
        let flagged = message.flags.flagged;
        open.star.set_icon_name(if flagged {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
        open.star.set_tooltip_text(Some(&if flagged {
            gettext("Remove star")
        } else {
            gettext("Star this message")
        }));
        if flagged {
            open.star.add_css_class("accent");
        } else {
            open.star.remove_css_class("accent");
        }
        let seen = message.flags.seen;
        open.read.set_icon_name(if seen {
            "mail-unread-symbolic"
        } else {
            "mail-read-symbolic"
        });
        open.read.set_tooltip_text(Some(&if seen {
            gettext("Mark as Unread")
        } else {
            gettext("Mark as Read")
        }));
    }

    /// True if the message is open on screen and has no body yet.
    pub fn wants_body(&self, id: MessageId) -> bool {
        self.card(id)
            .and_then(|card| card.open.borrow().as_ref().map(|open| !open.loaded.get()))
            .unwrap_or(false)
    }

    /// True if the message is open on screen.
    pub fn is_open(&self, id: MessageId) -> bool {
        self.card(id)
            .is_some_and(|card| card.open.borrow().is_some())
    }

    pub fn set_body(&self, id: MessageId, content: &BodyContent, images_allowed: bool) {
        let Some(card) = self.card(id) else {
            return;
        };
        let open = card.open.borrow();
        let Some(open) = open.as_ref() else {
            return;
        };
        open.loaded.set(true);
        open.web.load_html(&content.document, None);
        open.banner
            .set_visible(content.has_remote_content && !images_allowed);
        while let Some(child) = open.attachments.first_child() {
            open.attachments.remove(&child);
        }
        for attachment in &content.attachments {
            open.attachments
                .append(&self.attachment_chip(id, attachment));
        }
        open.attachments
            .set_visible(!content.attachments.is_empty());
        open.body.set_visible_child_name("web");
    }

    pub fn set_body_error(&self, id: MessageId, error: &str) {
        let Some(card) = self.card(id) else {
            return;
        };
        if let Some(open) = card.open.borrow().as_ref() {
            open.error
                .set_description(Some(&glib::markup_escape_text(error)));
            open.body.set_visible_child_name("error");
            // Allow another try when the card is shown again.
            open.loaded.set(false);
        }
    }

    fn attachment_chip(&self, id: MessageId, attachment: &Attachment) -> gtk::Box {
        let label = format!(
            "{} · {}",
            attachment.name,
            glib::format_size(attachment.size as u64)
        );
        let (content_type, _) = gtk::gio::content_type_guess(Some(&attachment.name), None);
        let icon = gtk::gio::content_type_get_symbolic_icon(&content_type);
        let icon_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        icon_box.append(&gtk::Image::from_gicon(&icon));
        icon_box.append(
            &gtk::Label::builder()
                .label(&label)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(40)
                .build(),
        );
        let content = icon_box;
        let open = gtk::Button::builder()
            .child(&content)
            .tooltip_text(gettext("Open"))
            .build();
        let save = gtk::Button::builder()
            .icon_name("document-save-symbolic")
            .tooltip_text(gettext("Save"))
            .build();
        for (button, open_it) in [(&open, true), (&save, false)] {
            let weak = self.weak.borrow().clone();
            let attachment = attachment.clone();
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade()
                    && let Some(callback) = this.on_attachment.borrow().as_ref()
                {
                    callback(id, attachment.clone(), open_it);
                }
            });
        }
        let chip = gtk::Box::builder()
            .css_classes(["linked", "attachment-chip"])
            .halign(gtk::Align::Start)
            .build();
        chip.append(&open);
        chip.append(&save);
        chip
    }

    fn build_web_view(&self) -> webkit::WebView {
        let manager = webkit::UserContentManager::new();
        manager.register_script_message_handler("height", Some(WORLD));
        manager.add_script(&webkit::UserScript::for_world(
            HEIGHT_SCRIPT,
            webkit::UserContentInjectedFrames::TopFrame,
            webkit::UserScriptInjectionTime::End,
            WORLD,
            &[],
            &[],
        ));
        apply_theme(&manager);
        let web_view = webkit::WebView::builder()
            .network_session(&network_session())
            .user_content_manager(&manager)
            .height_request(40)
            .build();
        let weak_view = web_view.downgrade();
        manager.connect_script_message_received(Some("height"), move |_, value| {
            if let Some(view) = weak_view.upgrade() {
                let height = value.to_double();
                tracing::debug!("message body is {height} px high");
                if height.is_finite() && height > 0.0 {
                    view.set_height_request(height.ceil().min(100_000.0) as i32);
                }
            }
        });
        configure(&web_view);
        let preview = self.link_preview.downgrade();
        web_view.connect_mouse_target_changed(move |_, hit, _| {
            let Some(preview) = preview.upgrade() else {
                return;
            };
            match hit.link_uri().filter(|_| hit.context_is_link()) {
                Some(uri) => {
                    preview.set_label(&e_mail_core::html::link_target_text(&uri));
                    preview.set_visible(true);
                }
                None => preview.set_visible(false),
            }
        });

        // The page cannot scroll, as it is as tall as its content. Scrolling
        // over it moves the conversation instead.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let scroller = self.scroller.downgrade();
        scroll.connect_scroll(move |controller, dx, dy| {
            let Some(scroller) = scroller.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if dx.abs() > dy.abs() {
                return glib::Propagation::Proceed;
            }
            let adjustment = scroller.vadjustment();
            let step = match controller.unit() {
                gdk::ScrollUnit::Wheel => adjustment.page_size().powf(2.0 / 3.0),
                _ => 1.0,
            };
            adjustment.set_value(adjustment.value() + dy * step);
            glib::Propagation::Stop
        });
        web_view.add_controller(scroll);
        web_view
    }
}

/// Plain-text mail takes the text colour of the app. HTML mail is designed
/// for a white page, so in dark mode it gets one, unless it sets its own.
fn apply_theme(manager: &webkit::UserContentManager) {
    manager.remove_all_style_sheets();
    let css = if adw::StyleManager::default().is_dark() {
        "body.mail-text { color: #ffffff !important; }\n\
         body.mail-html { background-color: #ffffff; color: #000000; border-radius: 8px; }"
    } else {
        "body.mail-text { color: rgba(0, 0, 6, 0.8) !important; }"
    };
    manager.add_style_sheet(&webkit::UserStyleSheet::new(
        css,
        webkit::UserContentInjectedFrames::TopFrame,
        webkit::UserStyleLevel::User,
        &[],
        &[],
    ));
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

/// The part after the @, which is what image permissions are kept for.
pub fn sender_domain(address: &str) -> String {
    address
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim().trim_end_matches('>').to_lowercase())
        .unwrap_or_default()
}

thread_local! {
    static SESSION: webkit::NetworkSession = webkit::NetworkSession::new_ephemeral();
}

pub fn network_session() -> webkit::NetworkSession {
    SESSION.with(Clone::clone)
}

/// Settings shared by every web view that shows mail.
pub fn configure(web_view: &webkit::WebView) {
    if let Some(settings) = WebViewExt::settings(web_view) {
        // Scripts inside messages are removed while parsing; only our own
        // script (in its own world) can run.
        settings.set_enable_javascript(true);
        settings.set_enable_javascript_markup(false);
        settings.set_enable_html5_local_storage(false);
        settings.set_enable_html5_database(false);
        settings.set_enable_webgl(false);
        settings.set_enable_webaudio(false);
        settings.set_enable_media(false);
        settings.set_enable_page_cache(false);
        settings.set_enable_developer_extras(false);
        settings.set_enable_back_forward_navigation_gestures(false);
        settings.set_allow_file_access_from_file_urls(false);
        settings.set_allow_universal_access_from_file_urls(false);
    }
    web_view.set_background_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    web_view.connect_decide_policy(|view, decision, kind| {
        if !matches!(
            kind,
            webkit::PolicyDecisionType::NavigationAction
                | webkit::PolicyDecisionType::NewWindowAction
        ) {
            return false;
        }
        let Some(navigation) = decision.downcast_ref::<webkit::NavigationPolicyDecision>() else {
            return false;
        };
        let Some(action) = navigation.navigation_action() else {
            return false;
        };
        let uri = action
            .request()
            .and_then(|request| request.uri())
            .map(|uri| uri.to_string())
            .unwrap_or_default();
        if uri.starts_with("about:") {
            return false;
        }
        decision.ignore();
        if action.navigation_type() == webkit::NavigationType::LinkClicked && is_openable(&uri) {
            let window = view.root().and_downcast::<gtk::Window>();
            gtk::UriLauncher::new(&uri).launch(
                window.as_ref(),
                gtk::gio::Cancellable::NONE,
                |result| {
                    if let Err(error) = result {
                        tracing::warn!("could not open link: {error}");
                    }
                },
            );
        }
        true
    });
}

/// Links are handed to the browser or mail app; anything else is ignored.
fn is_openable(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.starts_with("https:") || lower.starts_with("http:") || lower.starts_with("mailto:")
}

#[cfg(test)]
mod tests {
    use super::sender_domain;

    #[test]
    fn finds_the_sender_domain() {
        assert_eq!(sender_domain("info@Shop.Example"), "shop.example");
        assert_eq!(sender_domain("no-at-sign"), "");
    }
}
