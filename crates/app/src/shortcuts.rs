// SPDX-License-Identifier: GPL-3.0-or-later

//! The keyboard shortcuts: as an overview window (Ctrl+?, F1, ? or the
//! main menu) and as the page "Keyboard Shortcuts" in the settings.

use adw::prelude::*;
use gettextrs::gettext;

/// Groups of (label, accelerators); several accelerators are separated by
/// spaces, as `GtkShortcutsShortcut` and `GtkShortcutLabel` expect.
fn groups() -> [(String, Vec<(String, &'static str)>); 3] {
    [
        (
            gettext("Navigation"),
            vec![
                (gettext("Next conversation"), "j"),
                (gettext("Previous conversation"), "k"),
                (gettext("Open"), "Return"),
                (gettext("Back / Close"), "Escape"),
                (gettext("Folder sidebar"), "F9"),
            ],
        ),
        (
            gettext("Message"),
            vec![
                (gettext("Reply"), "r"),
                (gettext("Reply All"), "<Shift>r"),
                (gettext("Forward"), "f"),
                (gettext("Star"), "s"),
                (gettext("Read/Unread"), "u"),
                (gettext("Delete"), "Delete"),
            ],
        ),
        (
            gettext("General"),
            vec![
                (gettext("New Message"), "c <Control>n"),
                (gettext("Search"), "slash <Control>f"),
                (gettext("Send"), "<Control>Return"),
                (gettext("Settings"), "<Control>comma"),
                (
                    gettext("Keyboard Shortcuts"),
                    "question <Control>question F1",
                ),
            ],
        ),
    ]
}

pub fn window() -> gtk::ShortcutsWindow {
    let section = gtk::ShortcutsSection::builder()
        .section_name("main")
        .max_height(16)
        .build();
    for (title, rows) in groups() {
        let group = gtk::ShortcutsGroup::builder().title(&title).build();
        for (label, accelerator) in rows {
            let shortcut = gtk::ShortcutsShortcut::builder()
                .title(&label)
                .accelerator(accelerator)
                .build();
            group.add_shortcut(&shortcut);
        }
        section.add_group(&group);
    }
    let window = gtk::ShortcutsWindow::builder().build();
    window.add_section(&section);
    window
}

/// Settings → Keyboard Shortcuts: the same list, to look up.
pub fn page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title(gettext("Keyboard Shortcuts"))
        .icon_name("input-keyboard-symbolic")
        .build();
    for (title, rows) in groups() {
        let group = adw::PreferencesGroup::builder().title(&title).build();
        for (label, accelerator) in rows {
            let row = adw::ActionRow::builder().title(&label).build();
            row.add_suffix(
                &gtk::ShortcutLabel::builder()
                    .accelerator(accelerator)
                    .valign(gtk::Align::Center)
                    .build(),
            );
            group.add(&row);
        }
        page.add(&group);
    }
    page
}
