// SPDX-License-Identifier: GPL-3.0-or-later

mod application;
mod background;
mod compose_window;
mod composer;
mod config;
mod dialogs;
mod editor;
mod files;
mod folder_list;
mod message_list;
mod reader;
mod recipients;
mod reply_editor;
mod runtime;
mod secrets;
mod settings;
mod setup;
mod shortcuts;
mod tray;
mod widgets;
mod window;
mod wrap_layout;

use gettextrs::LocaleCategory;
use gtk::glib;

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    // The language chosen in Settings → General wins over the system.
    // SAFETY: called before any other thread exists.
    if let Some(language) = chosen_language() {
        unsafe { std::env::set_var("LANGUAGE", language) };
    }
    // SAFETY: called before any other thread exists.
    unsafe { gettextrs::setlocale(LocaleCategory::LcAll, "") };
    if let Err(error) = gettextrs::bindtextdomain(config::GETTEXT_PACKAGE, config::LOCALEDIR) {
        tracing::warn!("translations unavailable: {error}");
    }
    let _ = gettextrs::bind_textdomain_codeset(config::GETTEXT_PACKAGE, "UTF-8");
    let _ = gettextrs::textdomain(config::GETTEXT_PACKAGE);

    glib::set_application_name("E-Mail");
    application::run()
}

/// "de" or "en" if the user picked a language, None to follow the system.
fn chosen_language() -> Option<String> {
    use gtk::gio;
    use gtk::prelude::SettingsExt;
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(config::APP_ID, true)?;
    let language = gio::Settings::new(config::APP_ID).string("language");
    matches!(language.as_str(), "de" | "en").then(|| language.to_string())
}
