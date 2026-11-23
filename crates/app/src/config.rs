// SPDX-License-Identifier: GPL-3.0-or-later

pub const APP_ID: &str = "io.github.deviablo.EMail";
pub const GETTEXT_PACKAGE: &str = "e-mail";

/// Set by Meson at build time; `cargo run` falls back to the system path.
pub const LOCALEDIR: &str = match option_env!("E_MAIL_LOCALEDIR") {
    Some(dir) => dir,
    None => "/usr/share/locale",
};
