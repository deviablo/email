// SPDX-License-Identifier: GPL-3.0-or-later

//! The editor for replies and forwards, shown right below the message it
//! answers. Its text is kept as a draft while it is open, so nothing is
//! lost when the user looks at another conversation.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use e_mail_core::compose::{Draft, Original, ReplyKind};
use e_mail_core::model::{Account, MessageId};
use gettextrs::gettext;
use gtk::glib;

use crate::compose_window::AUTOSAVE;
use crate::composer::Composer;
use crate::window::Window;

pub struct ReplyEditor {
    pub widget: gtk::Box,
    /// The message this answers.
    pub message: MessageId,
    pub account: Account,
    pub composer: Rc<Composer>,
    original: Original,
    kind: Cell<ReplyKind>,
    buttons: Vec<(ReplyKind, gtk::ToggleButton)>,
    host: Weak<Window>,
    autosave: RefCell<Option<glib::SourceId>>,
    /// True once a version was handed over for saving.
    saved: Cell<bool>,
    busy: Cell<bool>,
}

impl ReplyEditor {
    pub fn new(
        host: &Rc<Window>,
        account: Account,
        message: MessageId,
        original: Original,
        kind: ReplyKind,
    ) -> Rc<ReplyEditor> {
        let draft = host.reply_draft(&account, &original, kind);
        let composer = Composer::new(&draft, host.contacts_for(account.id));

        let kinds = gtk::Box::builder()
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .margin_top(10)
            .margin_bottom(4)
            .build();
        let mut buttons: Vec<(ReplyKind, gtk::ToggleButton)> = Vec::new();
        for (each, label) in [
            (ReplyKind::Reply, gettext("Reply")),
            (ReplyKind::ReplyAll, gettext("Reply All")),
            (ReplyKind::Forward, gettext("Forward")),
        ] {
            let button = gtk::ToggleButton::builder()
                .label(&label)
                .active(each == kind)
                .css_classes(["reply-kind"])
                .build();
            if let Some((_, first)) = buttons.first() {
                button.set_group(Some(first));
            }
            kinds.append(&button);
            buttons.push((each, button));
        }

        let discard = gtk::Button::builder()
            .label(gettext("Discard"))
            .tooltip_text(gettext("Throw away this reply"))
            .css_classes(["flat"])
            .build();
        let send = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("io.github.deviablo.EMail-send-symbolic")
                    .label(gettext("Send"))
                    .build(),
            )
            .tooltip_text(gettext("Send (Ctrl+Enter)"))
            .css_classes(["suggested-action"])
            .build();
        composer
            .editor
            .bar
            .append(&gtk::Box::builder().hexpand(true).build());
        composer.editor.bar.append(&discard);
        composer.editor.bar.append(&send);
        // At least this high; otherwise it takes all the room left in the
        // reading pane and grows and shrinks with the window.
        composer.editor.web.set_height_request(240);
        composer.editor.web.set_vexpand(true);

        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .vexpand(true)
            .css_classes(["card", "reply-editor"])
            .build();
        widget.append(&kinds);
        widget.append(&composer.to.widget);
        widget.append(&composer.extra_fields);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&composer.editor.web);
        widget.append(&composer.attachment_box);
        widget.append(&composer.editor.toolbar);

        let this = Rc::new(ReplyEditor {
            widget,
            message,
            account,
            composer,
            original,
            kind: Cell::new(kind),
            buttons,
            host: Rc::downgrade(host),
            autosave: RefCell::default(),
            saved: Cell::new(false),
            busy: Cell::new(false),
        });

        for (each, button) in &this.buttons {
            let weak = Rc::downgrade(&this);
            let each = *each;
            button.connect_toggled(move |button| {
                if button.is_active()
                    && let Some(this) = weak.upgrade()
                {
                    this.set_kind(each);
                }
            });
        }
        let weak = Rc::downgrade(&this);
        discard.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.discard();
            }
        });
        let weak = Rc::downgrade(&this);
        send.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.send();
            }
        });
        let weak = Rc::downgrade(&this);
        this.composer.connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.schedule_autosave();
            }
        });
        let weak = Rc::downgrade(&this);
        this.composer.connect_undo(move |text, undo| {
            if let Some(host) = weak.upgrade().and_then(|this| this.host.upgrade()) {
                host.toast_with_undo(text, undo);
            }
        });
        let weak = Rc::downgrade(&this);
        this.composer.connect_problem(move |text| {
            if let Some(host) = weak.upgrade().and_then(|this| this.host.upgrade()) {
                host.toast(text);
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let ctrl = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            if ctrl
                && matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                && let Some(this) = weak.upgrade()
            {
                this.send();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        this.widget.add_controller(keys);

        if kind == ReplyKind::Forward {
            this.composer.to.grab_focus();
        } else {
            this.composer.editor.focus_start();
        }
        this
    }

    /// Switches between reply, reply all and forward. Text the user wrote
    /// stays; an untouched text gets the fitting quote.
    pub fn set_kind(&self, kind: ReplyKind) {
        for (each, button) in &self.buttons {
            if *each == kind && !button.is_active() {
                button.set_active(true);
            }
        }
        if self.kind.replace(kind) == kind {
            return;
        }
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let draft: Draft = host.reply_draft(&self.account, &self.original, kind);
        self.composer.take_headers(&draft);
        self.composer.set_attachments(draft.attachments.clone());
        if !self.composer.is_dirty() {
            self.composer.editor.set_html(&draft.html);
        }
        if kind == ReplyKind::Forward {
            self.composer.to.grab_focus();
        } else {
            self.composer.editor.focus_start();
        }
    }

    fn schedule_autosave(self: &Rc<Self>) {
        if self.autosave.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(AUTOSAVE, move || {
            if let Some(this) = weak.upgrade() {
                this.autosave.take();
                this.save();
            }
        });
        *self.autosave.borrow_mut() = Some(source);
    }

    fn cancel_autosave(&self) {
        if let Some(source) = self.autosave.take() {
            source.remove();
        }
    }

    /// Saves the reply as a draft if anything changed since the last save.
    pub fn save(self: &Rc<Self>) {
        if !self.composer.is_dirty() || self.busy.get() {
            return;
        }
        self.composer.mark_saved();
        self.saved.set(true);
        let draft = self.composer.draft();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let draft = draft.await;
            if let Some(host) = this.host.upgrade() {
                host.save_draft(&this.account, draft);
            }
        });
    }

    /// The editor goes away, for example because another conversation was
    /// opened. Returns true if the text was kept as a draft.
    pub fn leave(self: &Rc<Self>) -> bool {
        self.cancel_autosave();
        let keep = self.composer.is_dirty();
        self.save();
        self.busy.set(true);
        keep
    }

    fn discard(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        self.cancel_autosave();
        if let Some(host) = self.host.upgrade() {
            if self.saved.get() {
                host.discard_draft(self.account.id, &self.composer.message_id());
            }
            host.close_reply();
        }
    }

    fn send(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            match this.composer.checked_draft().await {
                Ok(draft) => {
                    this.cancel_autosave();
                    if let Some(host) = this.host.upgrade() {
                        host.send_draft(&this.account, draft);
                        host.close_reply();
                    }
                }
                Err(problem) => {
                    this.busy.set(false);
                    if let Some(host) = this.host.upgrade() {
                        host.toast(&problem);
                    }
                }
            }
        });
    }
}
