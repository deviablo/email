// SPDX-License-Identifier: GPL-3.0-or-later

pub const APP_ID: &str = "io.github.deviablo.EMail";
pub const GETTEXT_PACKAGE: &str = "e-mail";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ISSUE_URL: &str = "https://github.com/deviablo/email/issues/new";
pub const WEBSITE: &str = "https://github.com/deviablo/email";

/// Set by Meson at build time; `cargo run` falls back to the system path.
pub const LOCALEDIR: &str = match option_env!("E_MAIL_LOCALEDIR") {
    Some(dir) => dir,
    None => "/usr/share/locale",
};

/// How this build was packaged ("deb", "rpm", "flatpak"), set by Meson.
pub const PACKAGE_FORMAT: &str = match option_env!("E_MAIL_PACKAGE") {
    Some(format) => format,
    None => "source",
};
