// SPDX-License-Identifier: GPL-3.0-or-later

//! What new mail and inline replies have in common: recipients, the
//! editor, attachments and turning it all into a [`Draft`].

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use e_mail_core::compose::{Draft, FileAttachment, Mailbox};
use gettextrs::gettext;
use gtk::{gio, glib};

use crate::editor::Editor;
use crate::recipients::{ContactSource, RecipientField};
use crate::widgets::fmt;

/// Many providers refuse larger messages.
const LARGE_ATTACHMENTS: usize = 20 * 1024 * 1024;

type ProblemCallback = Box<dyn Fn(&str)>;
/// Shows a message with "Undo" that runs the given function.
type UndoCallback = Box<dyn Fn(&str, Box<dyn Fn()>)>;

/// The parts of a draft without fields on screen.
#[derive(Clone, Debug, Default)]
struct Meta {
    message_id: String,
    subject: String,
    in_reply_to: Option<String>,
    references: Vec<String>,
}

pub struct Composer {
    pub to: Rc<RecipientField>,
    pub cc: Rc<RecipientField>,
    pub bcc: Rc<RecipientField>,
    cc_button: gtk::Button,
    bcc_button: gtk::Button,
    cc_row: gtk::Revealer,
    bcc_row: gtk::Revealer,
    /// The Cc and Bcc rows. Each is hidden until "+ Cc" or "+ Bcc" is
    /// pressed or it has addresses.
    pub extra_fields: gtk::Box,
    pub editor: Rc<Editor>,
    pub attach_button: gtk::Button,
    /// Chips of the attached files.
    pub attachment_box: gtk::FlowBox,
    attachments: RefCell<Vec<FileAttachment>>,
    meta: RefCell<Meta>,
    dirty: Cell<bool>,
    on_changed: RefCell<Option<Box<dyn Fn()>>>,
    on_problem: RefCell<Option<ProblemCallback>>,
    on_undo: RefCell<Option<UndoCallback>>,
    weak: RefCell<Weak<Composer>>,
}

impl Composer {
    pub fn new(draft: &Draft, contacts: ContactSource) -> Rc<Composer> {
        let to = RecipientField::new(&gettext("To"), contacts.clone());
        let cc = RecipientField::new(&gettext("Cc"), contacts.clone());
        let bcc = RecipientField::new(&gettext("Bcc"), contacts);
        let add_button = |label: String, tooltip: String| {
            gtk::Button::builder()
                .label(label)
                .tooltip_text(tooltip)
                .valign(gtk::Align::Center)
                .css_classes(["flat", "caption"])
                .build()
        };
        let cc_button = add_button(gettext("+ Cc"), gettext("Add a copy recipient"));
        let bcc_button = add_button(
            gettext("+ Bcc"),
            gettext("Add a blind copy recipient, invisible to the others"),
        );
        to.widget.append(&cc_button);
        to.widget.append(&bcc_button);
        let row = |field: &RecipientField| {
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            content.append(&field.widget);
            gtk::Revealer::builder()
                .child(&content)
                .transition_type(gtk::RevealerTransitionType::SlideDown)
                .build()
        };
        let cc_row = row(&cc);
        let bcc_row = row(&bcc);
        // A row opened by mistake can be closed again.
        let close_button = |tooltip: String| {
            let button = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text(&tooltip)
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .build();
            button.update_property(&[gtk::accessible::Property::Label(&tooltip)]);
            button
        };
        let cc_close = close_button(gettext("Remove Cc"));
        let bcc_close = close_button(gettext("Remove Bcc"));
        cc.widget.append(&cc_close);
        bcc.widget.append(&bcc_close);
        let extra_fields = gtk::Box::new(gtk::Orientation::Vertical, 0);
        extra_fields.append(&cc_row);
        extra_fields.append(&bcc_row);

        let editor = Editor::new(&draft.html);
        let attach_button = gtk::Button::builder()
            .icon_name("mail-attachment-symbolic")
            .tooltip_text(gettext("Attach File"))
            .css_classes(["flat"])
            .build();
        editor.bar.append(&attach_button);
        let attachment_box = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(6)
            .row_spacing(6)
            .max_children_per_line(4)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .visible(false)
            .build();

        let this = Rc::new(Composer {
            to,
            cc,
            bcc,
            cc_button,
            bcc_button,
            cc_row,
            bcc_row,
            extra_fields,
            editor,
            attach_button,
            attachment_box,
            attachments: RefCell::default(),
            meta: RefCell::new(Meta {
                message_id: draft.message_id.clone(),
                subject: draft.subject.clone(),
                in_reply_to: draft.in_reply_to.clone(),
                references: draft.references.clone(),
            }),
            dirty: Cell::new(false),
            on_changed: RefCell::default(),
            on_problem: RefCell::default(),
            on_undo: RefCell::default(),
            weak: RefCell::default(),
        });
        *this.weak.borrow_mut() = Rc::downgrade(&this);
        this.set_recipients(&draft.to, &draft.cc, &draft.bcc);
        this.set_attachments(draft.attachments.clone());

        let weak = Rc::downgrade(&this);
        this.cc_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.show_cc(true);
                this.cc.grab_focus();
            }
        });
        let weak = Rc::downgrade(&this);
        cc_close.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.close_row(true);
            }
        });
        let weak = Rc::downgrade(&this);
        bcc_close.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.close_row(false);
            }
        });
        let weak = Rc::downgrade(&this);
        this.bcc_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.show_bcc(true);
                this.bcc.grab_focus();
            }
        });
        for field in [&this.to, &this.cc, &this.bcc] {
            let weak = Rc::downgrade(&this);
            field.connect_changed(move || {
                if let Some(this) = weak.upgrade() {
                    this.mark_changed();
                }
            });
        }
        let weak = Rc::downgrade(&this);
        this.editor.connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.mark_changed();
            }
        });
        let weak = Rc::downgrade(&this);
        this.attach_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.choose_files();
            }
        });
        // Setting up the fields is no change by the user.
        this.dirty.set(false);
        this
    }

    /// Called whenever the user changes something.
    pub fn connect_changed(&self, callback: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Box::new(callback));
    }

    /// Shows a short message, such as a file that could not be read.
    pub fn connect_problem(&self, callback: impl Fn(&str) + 'static) {
        *self.on_problem.borrow_mut() = Some(Box::new(callback));
    }

    fn problem(&self, text: &str) {
        if let Some(callback) = self.on_problem.borrow().as_ref() {
            callback(text);
        }
    }

    pub fn mark_changed(&self) {
        self.dirty.set(true);
        if let Some(callback) = self.on_changed.borrow().as_ref() {
            callback();
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.get()
    }

    pub fn mark_saved(&self) {
        self.dirty.set(false);
    }

    pub fn message_id(&self) -> String {
        self.meta.borrow().message_id.clone()
    }

    pub fn set_subject(&self, subject: &str) {
        self.meta.borrow_mut().subject = subject.to_owned();
    }

    /// Takes over recipients, subject and threading of another draft,
    /// for example when a reply becomes a forward.
    pub fn take_headers(&self, draft: &Draft) {
        let mut meta = self.meta.borrow_mut();
        meta.subject = draft.subject.clone();
        meta.in_reply_to = draft.in_reply_to.clone();
        meta.references = draft.references.clone();
        drop(meta);
        self.set_recipients(&draft.to, &draft.cc, &draft.bcc);
    }

    pub fn set_recipients(&self, to: &[Mailbox], cc: &[Mailbox], bcc: &[Mailbox]) {
        self.to.set(to);
        self.cc.set(cc);
        self.bcc.set(bcc);
        self.show_cc(!cc.is_empty());
        self.show_bcc(!bcc.is_empty());
    }

    /// Hides the Cc or Bcc row. Addresses in it go too, with "Undo".
    fn close_row(&self, cc: bool) {
        let field = if cc { &self.cc } else { &self.bcc };
        let before = field.current();
        field.set(&[]);
        if cc {
            self.show_cc(false);
            self.to.grab_focus();
        } else {
            self.show_bcc(false);
            self.to.grab_focus();
        }
        if before.is_empty() {
            return;
        }
        self.mark_changed();
        let weak = self.weak.borrow().clone();
        let restore: Box<dyn Fn()> = Box::new(move || {
            if let Some(this) = weak.upgrade() {
                let field = if cc { &this.cc } else { &this.bcc };
                field.set(&before);
                if cc {
                    this.show_cc(true);
                } else {
                    this.show_bcc(true);
                }
                this.mark_changed();
            }
        });
        let text = if cc {
            gettext("Cc removed")
        } else {
            gettext("Bcc removed")
        };
        if let Some(callback) = self.on_undo.borrow().as_ref() {
            callback(&text, restore);
        }
    }

    /// Shows a message with "Undo", for removed Cc or Bcc addresses.
    pub fn connect_undo(&self, callback: impl Fn(&str, Box<dyn Fn()>) + 'static) {
        *self.on_undo.borrow_mut() = Some(Box::new(callback));
    }

    fn show_cc(&self, on: bool) {
        self.cc_row.set_reveal_child(on);
        self.cc_button.set_visible(!on);
    }

    fn show_bcc(&self, on: bool) {
        self.bcc_row.set_reveal_child(on);
        self.bcc_button.set_visible(!on);
    }

    pub fn set_attachments(&self, attachments: Vec<FileAttachment>) {
        *self.attachments.borrow_mut() = attachments;
        self.show_attachments();
    }

    fn show_attachments(&self) {
        while let Some(child) = self.attachment_box.first_child() {
            self.attachment_box.remove(&child);
        }
        let attachments = self.attachments.borrow().clone();
        for (index, attachment) in attachments.iter().enumerate() {
            let content = adw::ButtonContent::builder()
                .icon_name("mail-attachment-symbolic")
                .label(format!(
                    "{} · {}",
                    attachment.name,
                    glib::format_size(attachment.data.len() as u64)
                ))
                .build();
            let name = gtk::Button::builder()
                .child(&content)
                .can_target(false)
                .build();
            let remove = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .tooltip_text(fmt(&gettext("Remove {}"), &[&attachment.name]))
                .build();
            let weak = self.weak.borrow().clone();
            remove.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    if index < this.attachments.borrow().len() {
                        this.attachments.borrow_mut().remove(index);
                    }
                    this.show_attachments();
                    this.mark_changed();
                }
            });
            let chip = gtk::Box::builder()
                .css_classes(["linked", "attachment-chip"])
                .halign(gtk::Align::Start)
                .build();
            chip.append(&name);
            chip.append(&remove);
            self.attachment_box.append(&chip);
        }
        self.attachment_box.set_visible(!attachments.is_empty());
    }

    fn choose_files(&self) {
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Attach Files"))
            .modal(true)
            .build();
        let window = self.editor.web.root().and_downcast::<gtk::Window>();
        let weak = self.weak.borrow().clone();
        dialog.open_multiple(window.as_ref(), gio::Cancellable::NONE, move |result| {
            let Ok(files) = result else {
                return;
            };
            glib::spawn_future_local(async move {
                let mut added = Vec::new();
                let mut failed = Vec::new();
                for file in files.iter::<gio::File>().flatten() {
                    let name = file
                        .basename()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "attachment".to_owned());
                    match file.load_contents_future().await {
                        Ok((data, _)) => {
                            let (guess, _) = gio::content_type_guess(Some(&name), Some(&data[..]));
                            let mime_type = gio::content_type_get_mime_type(&guess)
                                .map(|m| m.to_string())
                                .unwrap_or_else(|| "application/octet-stream".to_owned());
                            added.push(FileAttachment {
                                name,
                                mime_type,
                                data: data.to_vec(),
                            });
                        }
                        Err(error) => {
                            tracing::warn!("could not read {name}: {error}");
                            failed.push(name);
                        }
                    }
                }
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if !failed.is_empty() {
                    this.problem(&fmt(
                        &gettext("Could not read {}."),
                        &[&failed.join(", ")],
                    ));
                }
                if added.is_empty() {
                    return;
                }
                this.attachments.borrow_mut().extend(added);
                this.show_attachments();
                this.mark_changed();
                let total: usize = this.attachments.borrow().iter().map(|a| a.data.len()).sum();
                if total > LARGE_ATTACHMENTS {
                    this.problem(&gettext(
                        "The attachments are very large. Many providers do not accept messages over 20 MB.",
                    ));
                }
            });
        });
    }

    /// Everything as it is now, for saving as a draft.
    /// Fields are taken at once, the text arrives from the editor later.
    pub fn draft(&self) -> impl Future<Output = Draft> + 'static {
        let html = self.editor.html();
        let meta = self.meta.borrow().clone();
        let mut draft = Draft {
            message_id: meta.message_id,
            to: self.to.current(),
            cc: self.cc.current(),
            bcc: self.bcc.current(),
            subject: meta.subject,
            html: String::new(),
            in_reply_to: meta.in_reply_to,
            references: meta.references,
            attachments: self.attachments.borrow().clone(),
        };
        async move {
            draft.html = html.await;
            draft
        }
    }

    /// The draft ready for sending, or what is wrong with it.
    pub async fn checked_draft(&self) -> Result<Draft, String> {
        let mut invalid = Vec::new();
        let mut lists = Vec::new();
        for field in [&self.to, &self.cc, &self.bcc] {
            match field.mailboxes() {
                Ok(list) => lists.push(list),
                Err(bad) => {
                    invalid.extend(bad);
                    lists.push(Vec::new());
                }
            }
        }
        if !invalid.is_empty() {
            return Err(fmt(
                &gettext("Please check the address “{}”."),
                &[&invalid.join(", ")],
            ));
        }
        let mut draft = self.draft().await;
        draft.bcc = lists.pop().unwrap_or_default();
        draft.cc = lists.pop().unwrap_or_default();
        draft.to = lists.pop().unwrap_or_default();
        if !draft.has_recipients() {
            return Err(gettext("Add at least one recipient."));
        }
        Ok(draft)
    }
}
