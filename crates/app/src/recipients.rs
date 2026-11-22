// SPDX-License-Identifier: GPL-3.0-or-later

//! Recipient fields: typed addresses turn into chips, known addresses are
//! suggested while typing. Shared by new mail and inline replies.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use e_mail_core::compose::{self, Mailbox};
use gettextrs::gettext;
use gtk::{gdk, glib};

/// Looks up known addresses for what was typed.
pub type ContactSource = Rc<dyn Fn(&str) -> Vec<Mailbox>>;

pub struct RecipientField {
    /// Label, chips and entry in one row.
    pub widget: gtk::Box,
    flow: gtk::FlowBox,
    entry: gtk::Entry,
    popover: gtk::Popover,
    suggestions: gtk::ListBox,
    shown: RefCell<Vec<Mailbox>>,
    chips: RefCell<Vec<(Mailbox, gtk::FlowBoxChild)>>,
    contacts: ContactSource,
    on_changed: RefCell<Option<Box<dyn Fn()>>>,
    weak: RefCell<Weak<RecipientField>>,
}

impl RecipientField {
    pub fn new(label: &str, contacts: ContactSource) -> Rc<RecipientField> {
        let title = gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .width_chars(7)
            .valign(gtk::Align::Center)
            .css_classes(["dim-label"])
            .build();
        let entry = gtk::Entry::builder()
            .has_frame(false)
            .css_classes(["plain-field"])
            .hexpand(true)
            .width_chars(14)
            .input_purpose(gtk::InputPurpose::Email)
            .build();
        entry.update_property(&[gtk::accessible::Property::Label(label)]);
        let flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(false)
            .hexpand(true)
            .min_children_per_line(1)
            .max_children_per_line(30)
            .column_spacing(4)
            .row_spacing(4)
            .valign(gtk::Align::Center)
            .build();
        let entry_child = gtk::FlowBoxChild::builder()
            .child(&entry)
            .focusable(false)
            .build();
        flow.append(&entry_child);
        let widget = gtk::Box::builder()
            .spacing(8)
            .margin_start(14)
            .margin_end(10)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        widget.append(&title);
        widget.append(&flow);

        let suggestions = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar"])
            .build();
        let popover = gtk::Popover::builder()
            .child(&suggestions)
            .autohide(false)
            .has_arrow(false)
            .can_focus(false)
            .position(gtk::PositionType::Bottom)
            .halign(gtk::Align::Start)
            .build();
        popover.set_parent(&entry);

        let this = Rc::new(RecipientField {
            widget,
            flow,
            entry,
            popover,
            suggestions,
            shown: RefCell::default(),
            chips: RefCell::default(),
            contacts,
            on_changed: RefCell::default(),
            weak: RefCell::default(),
        });
        *this.weak.borrow_mut() = Rc::downgrade(&this);
        this.connect_signals();
        this
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.entry.connect_changed(move |entry| {
            if let Some(this) = weak.upgrade() {
                entry.remove_css_class("error");
                this.update_suggestions();
                this.changed();
            }
        });
        let weak = Rc::downgrade(self);
        self.entry.connect_activate(move |_| {
            if let Some(this) = weak.upgrade() {
                this.accept();
            }
        });
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match key {
                gdk::Key::comma | gdk::Key::semicolon => {
                    this.commit();
                    glib::Propagation::Stop
                }
                gdk::Key::Tab if !this.entry.text().trim().is_empty() => {
                    this.accept();
                    glib::Propagation::Proceed
                }
                gdk::Key::BackSpace if this.entry.text().is_empty() => {
                    let last = this.chips.borrow().last().map(|(m, _)| m.clone());
                    if let Some(last) = last {
                        this.remove(&last);
                    }
                    glib::Propagation::Stop
                }
                gdk::Key::Escape if this.popover.is_visible() => {
                    this.popover.popdown();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.entry.add_controller(keys);
        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            let weak = weak.clone();
            // Let a click on a suggestion land first.
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                if let Some(this) = weak.upgrade()
                    && !this.entry.has_focus()
                {
                    this.popover.popdown();
                    this.commit();
                }
            });
        });
        self.entry.add_controller(focus);
        let weak = Rc::downgrade(self);
        self.suggestions.connect_row_activated(move |_, row| {
            if let Some(this) = weak.upgrade() {
                let mailbox = this.shown.borrow().get(row.index() as usize).cloned();
                if let Some(mailbox) = mailbox {
                    this.entry.set_text("");
                    this.add(mailbox);
                    this.popover.popdown();
                    this.entry.grab_focus();
                }
            }
        });
        // Clicking the free space of the row puts the cursor into the field.
        let click = gtk::GestureClick::new();
        let weak = Rc::downgrade(self);
        click.connect_released(move |_, _, _, _| {
            if let Some(this) = weak.upgrade() {
                this.entry.grab_focus();
            }
        });
        self.flow.add_controller(click);
    }

    pub fn connect_changed(&self, callback: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Box::new(callback));
    }

    fn changed(&self) {
        if let Some(callback) = self.on_changed.borrow().as_ref() {
            callback();
        }
    }

    pub fn grab_focus(&self) {
        self.entry.grab_focus();
    }

    /// Enter or Tab: takes the first suggestion if the text is no complete
    /// address yet, otherwise the text itself.
    fn accept(&self) {
        let text = self.entry.text().trim().to_owned();
        if self.popover.is_visible() && compose::parse_mailbox(&text).is_none() {
            let first = self.shown.borrow().first().cloned();
            if let Some(first) = first {
                self.entry.set_text("");
                self.add(first);
                self.popover.popdown();
                return;
            }
        }
        self.commit();
    }

    /// Turns the typed text into chips. Invalid parts stay in the entry,
    /// which is marked as an error.
    fn commit(&self) {
        let text = self.entry.text().to_string();
        if text.trim().is_empty() {
            return;
        }
        match compose::parse_addresses(&text) {
            Ok(mailboxes) => {
                self.entry.set_text("");
                for mailbox in mailboxes {
                    self.add(mailbox);
                }
            }
            Err(invalid) => {
                let valid = compose::parse_addresses(
                    &text
                        .split([',', ';'])
                        .filter(|part| !invalid.iter().any(|bad| bad == part.trim()))
                        .collect::<Vec<_>>()
                        .join(","),
                )
                .unwrap_or_default();
                for mailbox in valid {
                    self.add(mailbox);
                }
                self.entry.set_text(&invalid.join(", "));
                self.entry.add_css_class("error");
            }
        }
        self.popover.popdown();
    }

    fn add(&self, mailbox: Mailbox) {
        if self
            .chips
            .borrow()
            .iter()
            .any(|(known, _)| known.email.eq_ignore_ascii_case(&mailbox.email))
        {
            return;
        }
        let label = gtk::Label::builder()
            .label(mailbox.short())
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(28)
            .build();
        let remove = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text(crate::widgets::fmt(
                &gettext("Remove {}"),
                &[mailbox.short()],
            ))
            .css_classes(["flat", "circular", "chip-remove"])
            .valign(gtk::Align::Center)
            .build();
        let chip = gtk::Box::builder()
            .spacing(2)
            .css_classes(["recipient-chip"])
            .tooltip_text(mailbox.display())
            .build();
        chip.append(&label);
        chip.append(&remove);
        let child = gtk::FlowBoxChild::builder()
            .child(&chip)
            .focusable(false)
            .build();
        let position = self.chips.borrow().len() as i32;
        self.flow.insert(&child, position);
        let weak = self.weak.borrow().clone();
        let removed = mailbox.clone();
        remove.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.remove(&removed);
                this.entry.grab_focus();
            }
        });
        self.chips.borrow_mut().push((mailbox, child));
        self.changed();
    }

    fn remove(&self, mailbox: &Mailbox) {
        let removed = {
            let mut chips = self.chips.borrow_mut();
            chips
                .iter()
                .position(|(known, _)| known == mailbox)
                .map(|index| chips.remove(index))
        };
        if let Some((_, child)) = removed {
            self.flow.remove(&child);
            self.changed();
        }
    }

    pub fn set(&self, mailboxes: &[Mailbox]) {
        for (_, child) in self.chips.take() {
            self.flow.remove(&child);
        }
        self.entry.set_text("");
        for mailbox in mailboxes {
            self.add(mailbox.clone());
        }
    }

    /// The recipients, including anything still typed. Text that is no
    /// valid address comes back as error.
    pub fn mailboxes(&self) -> Result<Vec<Mailbox>, Vec<String>> {
        self.commit();
        let text = self.entry.text();
        if !text.trim().is_empty() {
            return Err(vec![text.to_string()]);
        }
        Ok(self.chips.borrow().iter().map(|(m, _)| m.clone()).collect())
    }

    /// The chips only, for saving a draft without touching the entry.
    pub fn current(&self) -> Vec<Mailbox> {
        let mut all: Vec<Mailbox> = self.chips.borrow().iter().map(|(m, _)| m.clone()).collect();
        if let Ok(typed) = compose::parse_addresses(&self.entry.text()) {
            all.extend(typed);
        }
        all
    }

    fn update_suggestions(&self) {
        let text = self.entry.text();
        let query = text.rsplit([',', ';']).next().unwrap_or_default().trim();
        while let Some(row) = self.suggestions.first_child() {
            self.suggestions.remove(&row);
        }
        if query.chars().count() < 2 {
            self.shown.borrow_mut().clear();
            self.popover.popdown();
            return;
        }
        let taken: Vec<String> = self
            .chips
            .borrow()
            .iter()
            .map(|(m, _)| m.email.to_lowercase())
            .collect();
        let found: Vec<Mailbox> = (self.contacts)(query)
            .into_iter()
            .filter(|m| !taken.contains(&m.email.to_lowercase()))
            .take(4)
            .collect();
        for mailbox in &found {
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(mailbox.short()))
                .subtitle(glib::markup_escape_text(&mailbox.email))
                .activatable(true)
                .build();
            row.add_prefix(&adw::Avatar::new(28, Some(mailbox.short()), true));
            self.suggestions.append(&row);
        }
        let any = !found.is_empty();
        *self.shown.borrow_mut() = found;
        if any {
            self.popover.popup();
        } else {
            self.popover.popdown();
        }
    }
}

impl Drop for RecipientField {
    fn drop(&mut self) {
        self.popover.unparent();
    }
}
