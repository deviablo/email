// SPDX-License-Identifier: GPL-3.0-or-later

pub const APP_ID: &str = "io.github.digistalter.EMail";
/// The App ID up to version 1.0.1, before the project moved to the
/// digistalter account. Only used to take over passwords, settings and the
/// autostart entry of earlier installations (see `migrate.rs`).
pub const LEGACY_APP_ID: &str = "io.github.deviablo.EMail";
pub const GETTEXT_PACKAGE: &str = "e-mail";

/// Set by Meson at build time; `cargo run` falls back to the system path.
pub const LOCALEDIR: &str = match option_env!("E_MAIL_LOCALEDIR") {
    Some(dir) => dir,
    None => "/usr/share/locale",
};
