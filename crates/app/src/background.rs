// SPDX-License-Identifier: GPL-3.0-or-later

//! Running in the background and starting after login. Inside Flatpak the
//! Background portal asks the user and sets up the autostart; elsewhere an
//! autostart entry in `~/.config/autostart` does the same.

use std::path::PathBuf;

use gtk::glib;

use crate::config::APP_ID;
use crate::runtime;

/// Started at login: stay hidden until the user opens the app.
pub const BACKGROUND_FLAG: &str = "--background";

pub fn is_flatpak() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

/// Turns starting after login on or off, to match "Run in background".
pub fn set_autostart(enabled: bool, reason: String) {
    if is_flatpak() {
        glib::spawn_future_local(async move {
            let result = runtime::run(async move {
                ashpd::desktop::background::Background::request()
                    .reason(reason.as_str())
                    .auto_start(enabled)
                    .command(["e-mail", BACKGROUND_FLAG])
                    .dbus_activatable(false)
                    .send()
                    .await
                    .and_then(|request| request.response())
                    .map(|response| response.auto_start())
            })
            .await;
            match result {
                Ok(granted) => tracing::info!("background portal: autostart {granted}"),
                Err(error) => tracing::warn!("background portal: {error}"),
            }
        });
        return;
    }
    let path = autostart_file();
    let result = if enabled {
        let exec = std::env::current_exe()
            .ok()
            .and_then(|path| path.to_str().map(str::to_owned))
            .filter(|path| !path.contains(char::is_whitespace))
            .unwrap_or_else(|| "e-mail".to_owned());
        path.parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::write(&path, autostart_entry(&exec)))
    } else {
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    };
    if let Err(error) = result {
        tracing::warn!("could not change the autostart entry: {error}");
    }
}

fn autostart_file() -> PathBuf {
    glib::user_config_dir()
        .join("autostart")
        .join(format!("{APP_ID}.desktop"))
}

fn autostart_entry(exec: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=E-Mail\nIcon={APP_ID}\n\
         Exec={exec} {BACKGROUND_FLAG}\nTerminal=false\nNoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

/// The command that opens the notification settings of the desktop, if
/// one is known. Not available inside Flatpak.
pub fn system_notification_settings() -> Option<&'static str> {
    if is_flatpak() {
        return None;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_lowercase();
    let candidates: &[(&str, &str)] = if desktop.contains("kde") {
        &[("systemsettings", "systemsettings kcm_notifications")]
    } else {
        &[("gnome-control-center", "gnome-control-center notifications")]
    };
    candidates
        .iter()
        .find(|(program, _)| glib::find_program_in_path(program).is_some())
        .map(|(_, command)| *command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_entry_starts_hidden() {
        let entry = autostart_entry("/usr/bin/e-mail");
        assert!(entry.contains("Exec=/usr/bin/e-mail --background\n"));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Icon=io.github.deviablo.EMail\n"));
    }
}
