// SPDX-License-Identifier: GPL-3.0-or-later

//! Small dialogs of the reading pane: choosing a folder to move to, the
//! source of a message, and printing a conversation.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use e_mail_core::model::{Folder, FolderId};
use gettextrs::gettext;
use webkit::prelude::*;

use crate::folder_list::{folder_label, role_icon};

/// Lets the user pick one of `folders`; `chosen` gets its id.
pub fn choose_folder(
    parent: &impl IsA<gtk::Widget>,
    folders: &[Folder],
    current: Option<FolderId>,
    chosen: impl Fn(FolderId) + 'static,
) {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let mut ids = Vec::new();
    for folder in folders.iter().filter(|folder| Some(folder.id) != current) {
        let row = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&folder_label(folder)))
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(role_icon(folder.role)));
        if folder.depth > 0 {
            row.set_margin_start(folder.depth as i32 * 18);
        }
        list.append(&row);
        ids.push(folder.id);
    }
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(12)
        .build();
    content.append(&list);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(480)
        .child(&content)
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&scroller));
    let dialog = adw::Dialog::builder()
        .title(gettext("Move To"))
        .content_width(360)
        .child(&view)
        .build();
    let weak = dialog.downgrade();
    list.connect_row_activated(move |_, row| {
        if let Some(id) = ids.get(row.index() as usize) {
            chosen(*id);
        }
        if let Some(dialog) = weak.upgrade() {
            dialog.close();
        }
    });
    dialog.present(Some(parent));
}

/// Shows the raw text of a message as it came from the server.
pub fn show_source(parent: &impl IsA<gtk::Widget>, raw: &[u8]) {
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&String::from_utf8_lossy(raw));
    let text = gtk::TextView::builder()
        .buffer(&buffer)
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&text)
        .vexpand(true)
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&scroller));
    let dialog = adw::Dialog::builder()
        .title(gettext("Message Source"))
        .content_width(760)
        .content_height(600)
        .child(&view)
        .build();
    dialog.present(Some(parent));
}

/// Opens the system print dialog for an HTML document. The web view that
/// renders it lives until printing is done.
pub fn print(window: &gtk::Window, document: &str) {
    // Like every web view of the app: nothing is kept on disk.
    let view = webkit::WebView::builder()
        .network_session(&crate::reader::network_session())
        .build();
    crate::reader::configure(&view);
    let keep: Rc<RefCell<Option<webkit::WebView>>> = Rc::new(RefCell::new(Some(view.clone())));
    let window = window.downgrade();
    view.connect_load_changed(move |view, event| {
        if event != webkit::LoadEvent::Finished {
            return;
        }
        let operation = webkit::PrintOperation::new(view);
        let done = keep.clone();
        operation.connect_finished(move |_| {
            done.take();
        });
        let keep_failed = keep.clone();
        operation.connect_failed(move |_, error| {
            tracing::warn!("printing failed: {error}");
            keep_failed.take();
        });
        let window = window.upgrade();
        if operation.run_dialog(window.as_ref()) == webkit::PrintOperationResponse::Cancel {
            keep.take();
        }
    });
    view.load_html(document, None);
}
