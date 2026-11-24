// SPDX-License-Identifier: GPL-3.0-or-later

//! The folder sidebar: standard folders on top, the account's own folders
//! below in a section that can be folded away.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use e_mail_core::folder::FolderRole;
use e_mail_core::model::{Folder, FolderId, ThreadId};
use gettextrs::gettext;
use gtk::glib;
use gtk::prelude::*;

pub fn role_label(role: FolderRole) -> String {
    match role {
        FolderRole::Inbox => gettext("Inbox"),
        FolderRole::Drafts => gettext("Drafts"),
        FolderRole::Sent => gettext("Sent"),
        FolderRole::Archive => gettext("Archive"),
        FolderRole::Junk => gettext("Spam"),
        FolderRole::Trash => gettext("Trash"),
    }
}

pub fn folder_label(folder: &Folder) -> String {
    folder.role.map_or_else(|| folder.name.clone(), role_label)
}

/// Adwaita has no symbols for these folders, so the app brings its own.
pub fn role_icon(role: Option<FolderRole>) -> &'static str {
    match role {
        Some(FolderRole::Inbox) => "io.github.digistalter.EMail-folder-inbox-symbolic",
        Some(FolderRole::Drafts) => "io.github.digistalter.EMail-folder-drafts-symbolic",
        Some(FolderRole::Sent) => "io.github.digistalter.EMail-folder-sent-symbolic",
        Some(FolderRole::Archive) => "io.github.digistalter.EMail-folder-archive-symbolic",
        Some(FolderRole::Junk) => "mail-mark-junk-symbolic",
        Some(FolderRole::Trash) => "user-trash-symbolic",
        None => "folder-symbolic",
    }
}

type Callback = Box<dyn Fn(FolderId)>;
type DropCallback = Box<dyn Fn(FolderId, Vec<ThreadId>)>;

pub struct FolderList {
    pub widget: gtk::ScrolledWindow,
    list: gtk::ListBox,
    rows: RefCell<Vec<(gtk::ListBoxRow, FolderId)>>,
    custom_rows: RefCell<Vec<gtk::ListBoxRow>>,
    section_arrow: RefCell<Option<gtk::Image>>,
    custom_open: Cell<bool>,
    updating: Cell<bool>,
    on_selected: RefCell<Option<Callback>>,
    on_drop: RefCell<Option<DropCallback>>,
    weak: RefCell<Weak<FolderList>>,
}

impl FolderList {
    pub fn new() -> Rc<FolderList> {
        let list = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        list.update_property(&[gtk::accessible::Property::Label(&gettext("Folders"))]);
        let widget = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        let this = Rc::new(FolderList {
            widget,
            list,
            rows: RefCell::default(),
            custom_rows: RefCell::default(),
            on_drop: RefCell::default(),
            weak: RefCell::default(),
            section_arrow: RefCell::default(),
            custom_open: Cell::new(true),
            updating: Cell::new(false),
            on_selected: RefCell::default(),
        });

        *this.weak.borrow_mut() = Rc::downgrade(&this);
        let weak = Rc::downgrade(&this);
        this.list.connect_row_selected(move |_, row| {
            let (Some(this), Some(row)) = (weak.upgrade(), row) else {
                return;
            };
            if this.updating.get() {
                return;
            }
            let id = this
                .rows
                .borrow()
                .iter()
                .find(|(candidate, _)| candidate == row)
                .map(|(_, id)| *id);
            if let (Some(id), Some(callback)) = (id, this.on_selected.borrow().as_ref()) {
                callback(id);
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            if let Some(this) = weak.upgrade()
                && !row.is_selectable()
            {
                this.toggle_custom();
            }
        });
        this
    }

    /// Conversations dragged from the list and dropped on a folder.
    pub fn connect_drop(&self, callback: impl Fn(FolderId, Vec<ThreadId>) + 'static) {
        *self.on_drop.borrow_mut() = Some(Box::new(callback));
    }

    fn accept_drops(&self, row: &gtk::ListBoxRow, folder: FolderId) {
        let target = gtk::DropTarget::new(glib::Type::STRING, gtk::gdk::DragAction::MOVE);
        let weak = self.weak.borrow().clone();
        target.connect_drop(move |_, value, _, _| {
            let Some(this) = weak.upgrade() else {
                return false;
            };
            let Some(threads) = value
                .get::<String>()
                .ok()
                .and_then(|text| parse_drag(&text))
            else {
                return false;
            };
            if let Some(callback) = this.on_drop.borrow().as_ref() {
                callback(folder, threads);
            }
            true
        });
        row.add_controller(target);
    }

    pub fn connect_selected(&self, callback: impl Fn(FolderId) + 'static) {
        *self.on_selected.borrow_mut() = Some(Box::new(callback));
    }

    pub fn set_folders(&self, folders: &[Folder], selected: Option<FolderId>) {
        self.updating.set(true);
        self.list.remove_all();
        self.rows.borrow_mut().clear();
        self.custom_rows.borrow_mut().clear();

        let (standard, custom): (Vec<&Folder>, Vec<&Folder>) =
            folders.iter().partition(|folder| folder.role.is_some());
        for folder in standard {
            let row = folder_row(folder);
            self.accept_drops(&row, folder.id);
            self.list.append(&row);
            self.rows.borrow_mut().push((row, folder.id));
        }
        if !custom.is_empty() {
            self.list.append(&self.section_row());
            for folder in custom {
                let row = folder_row(folder);
                self.accept_drops(&row, folder.id);
                row.set_visible(self.custom_open.get());
                self.list.append(&row);
                self.rows.borrow_mut().push((row.clone(), folder.id));
                self.custom_rows.borrow_mut().push(row);
            }
        }

        let selected_row = self
            .rows
            .borrow()
            .iter()
            .find(|(_, id)| Some(*id) == selected)
            .map(|(row, _)| row.clone());
        if let Some(row) = &selected_row
            && !self.custom_open.get()
            && self.custom_rows.borrow().contains(row)
        {
            self.toggle_custom();
        }
        self.list.select_row(selected_row.as_ref());
        self.updating.set(false);
    }

    fn section_row(&self) -> gtk::ListBoxRow {
        let arrow = gtk::Image::from_icon_name(self.arrow_icon());
        let label = gtk::Label::builder()
            .label(gettext("Other Folders"))
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["dim-label", "caption-heading"])
            .build();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        content.append(&label);
        content.append(&arrow);
        *self.section_arrow.borrow_mut() = Some(arrow);
        let row = gtk::ListBoxRow::builder()
            .child(&content)
            .selectable(false)
            .activatable(true)
            .build();
        row.update_state(&[gtk::accessible::State::Expanded(Some(
            self.custom_open.get(),
        ))]);
        row
    }

    fn arrow_icon(&self) -> &'static str {
        if self.custom_open.get() {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        }
    }

    fn toggle_custom(&self) {
        self.custom_open.set(!self.custom_open.get());
        for row in self.custom_rows.borrow().iter() {
            row.set_visible(self.custom_open.get());
        }
        if let Some(arrow) = self.section_arrow.borrow().as_ref() {
            arrow.set_icon_name(Some(self.arrow_icon()));
        }
    }
}

/// The conversations in a dragged row, see `message_list::DRAG_PREFIX`.
fn parse_drag(text: &str) -> Option<Vec<ThreadId>> {
    let ids = text.strip_prefix(crate::message_list::DRAG_PREFIX)?;
    let threads: Vec<ThreadId> = ids.split(',').filter_map(|id| id.parse().ok()).collect();
    (!threads.is_empty()).then_some(threads)
}

fn folder_row(folder: &Folder) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.set_margin_start(folder.depth.min(6) as i32 * 18);
    content.append(&gtk::Image::from_icon_name(role_icon(folder.role)));
    content.append(
        &gtk::Label::builder()
            .label(folder_label(folder))
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    // Nobody needs to know how much unread spam or trash there is.
    let counted = !matches!(folder.role, Some(FolderRole::Junk | FolderRole::Trash));
    if counted && folder.unread > 0 {
        content.append(
            &gtk::Label::builder()
                .label(folder.unread.to_string())
                .css_classes(["count-pill"])
                .valign(gtk::Align::Center)
                .build(),
        );
    }
    gtk::ListBoxRow::builder().child(&content).build()
}

#[cfg(test)]
mod tests {
    use super::parse_drag;

    #[test]
    fn only_our_drags_are_taken() {
        assert_eq!(parse_drag("e-mail-conversations:3,7"), Some(vec![3, 7]));
        assert_eq!(parse_drag("e-mail-conversations:"), None);
        assert_eq!(parse_drag("file:///etc/passwd"), None);
    }
}
