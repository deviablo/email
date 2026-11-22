// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::background;
use crate::config::APP_ID;
use crate::window::Window;

const STYLE: &str = "
.count-pill {
    background-color: alpha(currentColor, 0.1);
    border-radius: 999px;
    padding: 0 8px;
    font-size: smaller;
    font-weight: bold;
    min-width: 12px;
}
.message-row-frame { border-radius: 8px; }
.message-row-frame.picked {
    background-color: alpha(@accent_bg_color, 0.22);
    box-shadow: inset 0 0 0 1px alpha(@accent_bg_color, 0.6);
}
.unread-dot {
    background-color: @accent_bg_color;
    border: 2px solid @window_bg_color;
    border-radius: 999px;
    min-width: 8px;
    min-height: 8px;
}
.account-switcher { padding: 6px; }
.account-switcher > button,
.account-switcher > button:hover {
    background-color: alpha(currentColor, 0.08);
}
.account-switcher > button:active,
.account-switcher > button:checked {
    background-color: alpha(currentColor, 0.14);
}
.count-pill.unread {
    background-color: @accent_bg_color;
    color: @accent_fg_color;
}
.status-dot {
    min-width: 10px;
    min-height: 10px;
    border-radius: 999px;
    border: 2px solid @window_bg_color;
}
.status-dot.ok { background-color: @success_color; }
.status-dot.busy { background-color: @warning_color; }
.status-dot.error { background-color: @error_color; }
.setup-rail {
    background-color: alpha(currentColor, 0.04);
    border-right: 1px solid alpha(currentColor, 0.1);
    padding: 28px 22px 20px;
}
.setup-step.pending { opacity: 0.55; }
.setup-dot {
    min-width: 28px;
    min-height: 28px;
    border-radius: 999px;
    border: 1.5px solid alpha(currentColor, 0.3);
    font-weight: bold;
    font-size: smaller;
}
.setup-step.current .setup-dot {
    border-color: currentColor;
    box-shadow: 0 0 0 4px alpha(currentColor, 0.1);
}
.setup-step.past .setup-dot {
    background-color: @accent_bg_color;
    border-color: @accent_bg_color;
    color: @accent_fg_color;
}
.setup-segment {
    min-height: 4px;
    border-radius: 2px;
    background-color: alpha(currentColor, 0.15);
}
.setup-segment.done { background-color: @accent_bg_color; }
.setup-inner { padding: 40px 32px 32px; }
.setup-eyebrow {
    font-family: monospace;
    font-size: smaller;
    letter-spacing: 1px;
}
.setup-lead { font-size: 1.05em; }
.setup-footer { padding: 10px 14px; }
.setup-error {
    background-color: alpha(@error_color, 0.1);
    border-radius: 12px;
    padding: 10px 12px;
}
.setup-number {
    min-width: 24px;
    min-height: 24px;
    border-radius: 999px;
    background-color: alpha(currentColor, 0.1);
    font-weight: bold;
    font-size: smaller;
}
.setup-pending {
    min-width: 12px;
    min-height: 12px;
    border-radius: 999px;
    border: 1.5px solid alpha(currentColor, 0.3);
    margin: 2px;
}
.setup-tile { padding: 16px; }
.setup-preview {
    border: 1px dashed alpha(currentColor, 0.25);
    border-radius: 12px;
    padding: 10px 12px;
}
.setup-success {
    background-color: @success_bg_color;
    color: @success_fg_color;
    border-radius: 999px;
    min-width: 64px;
    min-height: 64px;
}
.account-confirm {
    padding: 14px;
}
.count-pill.outline {
    background-color: transparent;
    border: 1px solid alpha(currentColor, 0.25);
    font-weight: normal;
}
.navigation-sidebar > row:drop(active) {
    background-color: alpha(@accent_bg_color, 0.25);
    box-shadow: inset 0 0 0 2px @accent_bg_color;
}
.message-card.folded { padding: 8px 14px; }
.message-head { padding: 0; border-radius: 12px 12px 0 0; font-weight: normal; }
.message-card.newest {
    background-color: mix(@card_bg_color, @window_fg_color, 0.03);
    box-shadow: 0 0 0 2px alpha(@window_fg_color, 0.8), 0 4px 14px alpha(black, 0.08);
}
.message-card.unread { box-shadow: 0 0 0 2px @accent_bg_color; }
.message-card.newest.unread {
    box-shadow: 0 0 0 2px alpha(@window_fg_color, 0.8), 0 0 0 4px @accent_bg_color,
        0 4px 14px alpha(black, 0.08);
}
.newest-tag {
    font-size: smaller;
    font-weight: bold;
    padding: 2px 8px;
    border-radius: 999px;
    background-color: @window_fg_color;
    color: @window_bg_color;
}
.folder-tag {
    background-color: alpha(currentColor, 0.1);
    border-radius: 999px;
    padding: 1px 8px;
}
.image-banner {
    background-color: alpha(currentColor, 0.06);
    border-radius: 8px;
    padding: 8px 10px;
}
.selection-bar > box { border-top: 1px solid alpha(currentColor, 0.12); }
.reply-placeholder {
    padding: 12px 14px;
    background: none;
    box-shadow: none;
    border: 1px dashed alpha(currentColor, 0.3);
    color: alpha(currentColor, 0.7);
}
.reply-placeholder:hover { color: currentColor; border-color: alpha(currentColor, 0.5); }
.reply-editor { padding-bottom: 0; }
.link-preview {
    background-color: alpha(@window_bg_color, 0.95);
    border: 1px solid alpha(currentColor, 0.15);
    border-radius: 6px;
    padding: 3px 8px;
}
/* The fields of new mail and replies are plain lines as in the wireframe:
   no frame, no outline and no underline; the text cursor shows where
   typing goes. */
entry.plain-field,
entry.plain-field:focus-within {
    background: none;
    box-shadow: none;
    outline-color: transparent;
}
.pane-handle { background: none; }
.pane-handle:hover { background-color: alpha(@accent_bg_color, 0.35); }
.filter-chip {
    border-radius: 999px;
    padding: 0 10px;
    min-height: 22px;
    font-size: 0.9em;
    font-weight: normal;
}
.search-panel > box { border-bottom: 1px solid alpha(currentColor, 0.12); }
.reply-kind {
    border-radius: 999px;
    padding: 2px 12px;
    min-height: 24px;
    font-weight: normal;
}
.recipient-chip {
    background-color: alpha(currentColor, 0.1);
    border-radius: 999px;
    padding: 1px 2px 1px 10px;
}
.recipient-chip .chip-remove { min-width: 20px; min-height: 20px; padding: 0; }
.signature-editor { border-radius: 12px; }
";

type WindowSlot = Rc<RefCell<Option<Rc<Window>>>>;

/// The main window, created on first use. A notification action can start
/// the app without showing it, so creating and showing are separate.
fn ensure_window(app: &adw::Application, slot: &WindowSlot) -> Rc<Window> {
    slot.borrow_mut()
        .get_or_insert_with(|| Window::new(app))
        .clone()
}

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let window: WindowSlot = Rc::default();
    // After login the app starts hidden and only watches for new mail.
    let mut args: Vec<String> = std::env::args().collect();
    let start_hidden = args.iter().any(|arg| arg == background::BACKGROUND_FLAG);
    args.retain(|arg| arg != background::BACKGROUND_FLAG);
    let hidden_once = std::cell::Cell::new(start_hidden);

    app.connect_startup(|app| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(STYLE);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            // The app's own icons; installed builds find them in hicolor.
            let source_icons = concat!(env!("CARGO_MANIFEST_DIR"), "/../../design/icon");
            if std::path::Path::new(source_icons).is_dir() {
                gtk::IconTheme::for_display(&display).add_search_path(source_icons);
            }
        }
        gtk::Window::set_default_icon_name(APP_ID);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("window.close", &["<Control>w"]);
        app.set_accels_for_action("app.settings", &["<Control>comma"]);
        app.set_accels_for_action("app.compose", &["<Control>n"]);
        app.set_accels_for_action("app.shortcuts", &["<Control>question", "F1"]);
    });

    let quit = gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.quit())
        .build();
    let about = {
        let window = window.clone();
        gio::ActionEntry::builder("about")
            .activate(move |_: &adw::Application, _, _| {
                if let Some(window) = window.borrow().as_ref() {
                    crate::about::show(&window.window, window.accounts().len());
                }
            })
            .build()
    };
    let add_account = {
        let window = window.clone();
        gio::ActionEntry::builder("add-account")
            .activate(move |_: &adw::Application, _, _| {
                if let Some(window) = window.borrow().as_ref() {
                    window.add_account();
                }
            })
            .build()
    };
    let settings = {
        let window = window.clone();
        gio::ActionEntry::builder("settings")
            .activate(move |_: &adw::Application, _, _| {
                if let Some(window) = window.borrow().as_ref() {
                    window.show_settings();
                }
            })
            .build()
    };
    let compose = {
        let window = window.clone();
        gio::ActionEntry::builder("compose")
            .activate(move |_: &adw::Application, _, _| {
                if let Some(window) = window.borrow().as_ref() {
                    window.compose_new();
                }
            })
            .build()
    };
    let shortcuts = {
        let window = window.clone();
        gio::ActionEntry::builder("shortcuts")
            .activate(move |_: &adw::Application, _, _| {
                if let Some(window) = window.borrow().as_ref() {
                    crate::window::show_shortcuts(&window.window);
                }
            })
            .build()
    };
    let notification_open = {
        let window = window.clone();
        gio::ActionEntry::builder("notification-open")
            .parameter_type(Some(&<(i64, i64)>::static_variant_type()))
            .activate(move |app: &adw::Application, _, target| {
                let Some((account, message)) = target.and_then(|t| t.get::<(i64, i64)>()) else {
                    return;
                };
                let window = ensure_window(app, &window);
                window.window.present();
                window.open_message(account, message);
            })
            .build()
    };
    let notification_read = {
        let window = window.clone();
        gio::ActionEntry::builder("notification-read")
            .parameter_type(Some(&<(i64, Vec<i64>)>::static_variant_type()))
            .activate(move |app: &adw::Application, _, target| {
                if let Some((account, messages)) = target.and_then(|t| t.get::<(i64, Vec<i64>)>()) {
                    ensure_window(app, &window).notification_read(account, &messages);
                }
            })
            .build()
    };
    let notification_delete = {
        let window = window.clone();
        gio::ActionEntry::builder("notification-delete")
            .parameter_type(Some(&<(i64, Vec<i64>)>::static_variant_type()))
            .activate(move |app: &adw::Application, _, target| {
                if let Some((account, messages)) = target.and_then(|t| t.get::<(i64, Vec<i64>)>()) {
                    ensure_window(app, &window).notification_delete(account, &messages);
                }
            })
            .build()
    };
    app.add_action_entries([
        quit,
        about,
        add_account,
        settings,
        compose,
        shortcuts,
        notification_open,
        notification_read,
        notification_delete,
    ]);

    app.connect_activate(move |app| {
        let window = ensure_window(app, &window);
        if hidden_once.replace(false) && window.has_accounts() {
            return;
        }
        window.window.present();
    });

    app.run_with_args(&args)
}
