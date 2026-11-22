// SPDX-License-Identifier: GPL-3.0-or-later

//! "About E-Mail": name, version, the website and "Report an Issue",
//! nothing else. The issue form is filled in with technical details only:
//! no addresses, passwords or anything from messages.

use adw::prelude::*;

use crate::background;
use crate::config::{APP_ID, ISSUE_URL, PACKAGE_FORMAT, VERSION, WEBSITE};

pub fn show(parent: &impl IsA<gtk::Widget>, accounts: usize) {
    let details = technical_details(accounts);
    let about = adw::AboutDialog::builder()
        .application_name("E-Mail")
        .application_icon(APP_ID)
        .version(VERSION)
        .website(WEBSITE)
        .issue_url(issue_url(&details))
        .build();
    about.present(Some(parent));
}

/// Version, system and desktop, nothing personal.
pub fn technical_details(accounts: usize) -> String {
    let package = if background::is_flatpak() {
        "flatpak"
    } else {
        PACKAGE_FORMAT
    };
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "?".into());
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "?".into());
    format!(
        "E-Mail {VERSION} ({package})\nSystem: {}\nDesktop: {desktop} ({session})\n\
         GTK {}.{}.{}, libadwaita {}.{}.{}, WebKitGTK {}.{}.{}\nLanguage: {}\nAccounts: {accounts}\n",
        distribution(),
        gtk::major_version(),
        gtk::minor_version(),
        gtk::micro_version(),
        adw::major_version(),
        adw::minor_version(),
        adw::micro_version(),
        webkit::functions::major_version(),
        webkit::functions::minor_version(),
        webkit::functions::micro_version(),
        ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
            .unwrap_or_default(),
    )
}

/// The name of the system, also from inside Flatpak.
fn distribution() -> String {
    [
        "/run/host/os-release",
        "/etc/os-release",
        "/usr/lib/os-release",
    ]
    .iter()
    .find_map(|path| std::fs::read_to_string(path).ok())
    .and_then(|text| pretty_name(&text))
    .unwrap_or_else(|| "?".into())
}

fn pretty_name(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|line| {
        line.strip_prefix("PRETTY_NAME=")
            .map(|value| value.trim().trim_matches('"').to_owned())
    })
}

/// The address of a new issue with the details already in the form.
pub fn issue_url(details: &str) -> String {
    let body = format!(
        "**What happened?**\n\n\n**What did you expect?**\n\n\n**Technical details**\n```\n{details}```\n"
    );
    format!("{ISSUE_URL}?body={}", percent_encode(&body))
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_form_is_filled_in() {
        let url = issue_url("E-Mail 1.0.0 (deb)\n");
        assert!(url.starts_with("https://github.com/deviablo/email/issues/new?body="));
        assert!(url.contains("E-Mail%201.0.0%20%28deb%29%0A"));
        assert!(!url.contains(' '));
        assert_eq!(percent_encode("a b&c=ä"), "a%20b%26c%3D%C3%A4");
    }

    #[test]
    fn reads_the_system_name() {
        assert_eq!(
            pretty_name("NAME=Ubuntu\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n").as_deref(),
            Some("Ubuntu 24.04.1 LTS")
        );
        assert_eq!(pretty_name("ID=x"), None);
    }
}
