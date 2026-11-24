// SPDX-License-Identifier: GPL-3.0-or-later

//! Takes over what an installation of version 1.0.0 or 1.0.1 left under the
//! old App ID: the settings and the autostart entry. Passwords move over on
//! first use (see `secrets.rs`). Flatpak keeps the data of each App ID
//! apart, so there is nothing to reach from inside the sandbox.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::background;

const LEGACY_PATH: &str = "/io/github/deviablo/EMail/";
const PATH: &str = "/io/github/digistalter/EMail/";

pub fn from_legacy_app_id() {
    if background::is_flatpak() {
        return;
    }
    copy_settings();
    background::take_over_legacy_autostart();
}

/// Copies the old settings once, if there are any and the new ones are
/// still untouched. GSettings stores them in dconf on GNOME and KDE alike.
fn copy_settings() {
    let Some(old) = dconf(&["dump", LEGACY_PATH], None) else {
        return;
    };
    if old.trim().is_empty() {
        return;
    }
    if dconf(&["dump", PATH], None).is_some_and(|new| !new.trim().is_empty()) {
        return;
    }
    if dconf(&["load", PATH], Some(&old)).is_some() {
        dconf(&["reset", "-f", LEGACY_PATH], None);
        tracing::info!("settings taken over from the old App ID");
    }
}

/// Runs the `dconf` tool; None if it is missing or fails.
fn dconf(args: &[&str], input: Option<&str>) -> Option<String> {
    let mut child = Command::new("dconf")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(input) = input {
        child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
