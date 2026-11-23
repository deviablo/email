// SPDX-License-Identifier: GPL-3.0-or-later

//! The overview of keyboard shortcuts (Ctrl+?, F1 or ?).

use gettextrs::gettext;

pub fn window() -> gtk::ShortcutsWindow {
    let groups: [(String, Vec<(String, &str)>); 3] = [
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
    ];
    let section = gtk::ShortcutsSection::builder()
        .section_name("main")
        .max_height(16)
        .build();
    for (title, rows) in groups {
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
