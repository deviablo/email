// SPDX-License-Identifier: GPL-3.0-or-later

//! New mail in a window of its own, so the inbox stays readable next to it.
//! "Send" sits at the top left, there is no title. Closing always keeps the
//! message as a draft; it is deleted in the drafts folder.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use e_mail_core::compose::Draft;
use e_mail_core::model::Account;
use gettextrs::gettext;
use gtk::{gdk, glib};

use crate::composer::Composer;
use crate::window::Window;

/// How long after a change the draft is saved by itself.
pub const AUTOSAVE: Duration = Duration::from_secs(30);

pub struct ComposeWindow {
    pub window: adw::Window,
    pub account: Account,
    pub composer: Rc<Composer>,
    subject: gtk::Entry,
    toasts: adw::ToastOverlay,
    saved: gtk::Revealer,
    host: Weak<Window>,
    autosave: RefCell<Option<glib::SourceId>>,
    /// Set once the window may really close.
    done: Cell<bool>,
    busy: Cell<bool>,
}

impl ComposeWindow {
    pub fn new(host: &Rc<Window>, account: Account, draft: &Draft) -> Rc<ComposeWindow> {
        let composer = Composer::new(draft, host.contacts_for(account.id));

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
        let header = adw::HeaderBar::builder().show_title(false).build();
        header.pack_start(&send);

        let from = gtk::Box::builder()
            .spacing(8)
            .margin_start(14)
            .margin_end(10)
            .margin_top(10)
            .margin_bottom(10)
            .build();
        from.append(
            &gtk::Label::builder()
                .label(gettext("From"))
                .xalign(0.0)
                .width_chars(7)
                .css_classes(["dim-label"])
                .build(),
        );
        let sender = if account.display_name.is_empty() {
            account.email.clone()
        } else {
            format!("{} <{}>", account.display_name, account.email)
        };
        from.append(
            &gtk::Label::builder()
                .label(&sender)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .tooltip_text(gettext(
                    "Mail is always sent from the active account. Switch the account in the folder sidebar to write from another one.",
                ))
                .build(),
        );

        let subject_row = gtk::Box::builder()
            .spacing(8)
            .margin_start(14)
            .margin_end(10)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        subject_row.append(
            &gtk::Label::builder()
                .label(gettext("Subject"))
                .xalign(0.0)
                .width_chars(7)
                .css_classes(["dim-label"])
                .build(),
        );
        let subject = gtk::Entry::builder()
            .has_frame(false)
            .css_classes(["plain-field"])
            .hexpand(true)
            .text(&draft.subject)
            .build();
        subject.update_property(&[gtk::accessible::Property::Label(&gettext("Subject"))]);
        subject_row.append(&subject);

        composer
            .editor
            .bar
            .append(&gtk::Box::builder().hexpand(true).build());
        // Shows briefly after each automatic save.
        let saved = gtk::Revealer::builder()
            .child(
                &gtk::Label::builder()
                    .label(gettext("Saved"))
                    .css_classes(["dim-label", "caption"])
                    .margin_end(6)
                    .build(),
            )
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .build();
        composer.editor.bar.append(&saved);

        let fields = gtk::Box::new(gtk::Orientation::Vertical, 0);
        fields.append(&from);
        fields.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        fields.append(&composer.to.widget);
        fields.append(&composer.extra_fields);
        fields.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        fields.append(&subject_row);
        fields.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&fields);
        content.append(&composer.editor.web);
        content.append(&composer.attachment_box);
        content.append(&composer.editor.toolbar);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&content));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&toasts));

        let window = adw::Window::builder()
            .title(title_for(&draft.subject))
            .default_width(720)
            .default_height(640)
            .transient_for(&host.window)
            .width_request(360)
            .height_request(360)
            .content(&view)
            .build();
        if let Some(app) = host.window.application() {
            window.set_application(Some(&app));
        }

        let this = Rc::new(ComposeWindow {
            window,
            account,
            composer,
            subject,
            toasts,
            saved,
            host: Rc::downgrade(host),
            autosave: RefCell::default(),
            done: Cell::new(false),
            busy: Cell::new(false),
        });

        let weak = Rc::downgrade(&this);
        send.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.send();
            }
        });
        let weak = Rc::downgrade(&this);
        this.subject.connect_changed(move |entry| {
            if let Some(this) = weak.upgrade() {
                this.composer.set_subject(&entry.text());
                this.window.set_title(Some(&title_for(&entry.text())));
                this.composer.mark_changed();
            }
        });
        let weak = Rc::downgrade(&this);
        this.composer.connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.schedule_autosave();
            }
        });
        let toasts = this.toasts.clone();
        this.composer.connect_problem(move |text| {
            toasts.add_toast(adw::Toast::new(text));
        });
        let toasts = this.toasts.clone();
        this.composer.connect_undo(move |text, undo| {
            let toast = adw::Toast::builder()
                .title(text)
                .button_label(gettext("Undo"))
                .build();
            toast.connect_button_clicked(move |_| undo());
            toasts.add_toast(toast);
        });
        let weak = Rc::downgrade(&this);
        this.window.connect_close_request(move |_| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if this.done.get() {
                return glib::Propagation::Proceed;
            }
            this.close_as_draft();
            glib::Propagation::Stop
        });

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::Return | gdk::Key::KP_Enter if ctrl => {
                    this.send();
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    this.window.close();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        this.window.add_controller(keys);

        if draft.to.is_empty() {
            this.composer.to.grab_focus();
        } else {
            this.composer.editor.focus_start();
        }
        this
    }

    pub fn present(&self) {
        self.window.present();
    }

    fn schedule_autosave(self: &Rc<Self>) {
        if self.autosave.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(AUTOSAVE, move || {
            if let Some(this) = weak.upgrade() {
                this.autosave.take();
                this.save(false);
            }
        });
        *self.autosave.borrow_mut() = Some(source);
    }

    fn cancel_autosave(&self) {
        if let Some(source) = self.autosave.take() {
            source.remove();
        }
    }

    /// Saves the current state as a draft, if anything changed.
    fn save(self: &Rc<Self>, then_close: bool) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            let changed = this.composer.is_dirty();
            if changed {
                this.composer.mark_saved();
                let draft = this.composer.draft().await;
                if let Some(host) = this.host.upgrade() {
                    host.save_draft(&this.account, draft);
                    if then_close {
                        host.toast(&gettext("Saved as draft"));
                    } else {
                        this.saved.set_reveal_child(true);
                        let saved = this.saved.clone();
                        glib::timeout_add_seconds_local_once(2, move || {
                            saved.set_reveal_child(false)
                        });
                    }
                }
            }
            if then_close {
                this.finish();
            }
        });
    }

    fn close_as_draft(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        self.cancel_autosave();
        self.save(true);
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
                    }
                    this.finish();
                }
                Err(problem) => {
                    this.busy.set(false);
                    this.toasts.add_toast(adw::Toast::new(&problem));
                }
            }
        });
    }

    fn finish(&self) {
        self.done.set(true);
        self.window.close();
        if let Some(host) = self.host.upgrade() {
            host.compose_closed(&self.composer.message_id());
        }
    }
}

fn title_for(subject: &str) -> String {
    if subject.trim().is_empty() {
        gettext("New Message")
    } else {
        subject.trim().to_owned()
    }
}
