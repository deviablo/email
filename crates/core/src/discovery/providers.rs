// SPDX-License-Identifier: GPL-3.0-or-later

//! Providers the setup recognises while the address is being typed. Their
//! server settings are built in, so no lookup is needed for them.

use crate::model::{Security, ServerConfig, ServerSettings};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderId {
    Gmail,
    ICloud,
    Yahoo,
    Gmx,
    WebDe,
    Posteo,
    MailboxOrg,
    TOnline,
    Microsoft,
}

/// What the user has to do at the provider before signing in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preparation {
    None,
    /// Mail apps need a separate app password.
    AppPassword,
    /// Access for mail apps has to be switched on in the web interface.
    EnableImap,
    /// Cannot be used with a password at all.
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Provider {
    pub id: ProviderId,
    pub name: &'static str,
    pub preparation: Preparation,
    /// Help page for the preparation, without the scheme.
    pub help_link: Option<&'static str>,
    domains: &'static [&'static str],
    imap: (&'static str, u16, Security),
    smtp: (&'static str, u16, Security),
}

impl Provider {
    pub fn settings(&self, email: &str) -> Option<ServerSettings> {
        if self.preparation == Preparation::Unsupported {
            return None;
        }
        let server =
            |(host, port, security): (&str, u16, Security)| ServerConfig::new(host, port, security);
        Some(ServerSettings {
            imap: server(self.imap),
            smtp: server(self.smtp),
            username: email.trim().to_owned(),
        })
    }

    pub fn help_url(&self) -> Option<String> {
        self.help_link.map(|link| format!("https://{link}"))
    }
}

const NO_SERVER: (&str, u16, Security) = ("", 0, Security::Tls);

pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: ProviderId::Gmail,
        name: "Gmail",
        preparation: Preparation::AppPassword,
        help_link: Some("myaccount.google.com/apppasswords"),
        domains: &["gmail.com", "googlemail.com"],
        imap: ("imap.gmail.com", 993, Security::Tls),
        smtp: ("smtp.gmail.com", 465, Security::Tls),
    },
    Provider {
        id: ProviderId::ICloud,
        name: "iCloud",
        preparation: Preparation::AppPassword,
        help_link: Some("account.apple.com"),
        domains: &["icloud.com", "me.com", "mac.com"],
        imap: ("imap.mail.me.com", 993, Security::Tls),
        smtp: ("smtp.mail.me.com", 587, Security::StartTls),
    },
    Provider {
        id: ProviderId::Yahoo,
        name: "Yahoo",
        preparation: Preparation::AppPassword,
        help_link: Some("login.yahoo.com/account/security"),
        domains: &["yahoo.com", "yahoo.de", "ymail.com", "rocketmail.com"],
        imap: ("imap.mail.yahoo.com", 993, Security::Tls),
        smtp: ("smtp.mail.yahoo.com", 465, Security::Tls),
    },
    Provider {
        id: ProviderId::Gmx,
        name: "GMX",
        preparation: Preparation::EnableImap,
        help_link: Some("gmx.net"),
        domains: &["gmx.de", "gmx.net", "gmx.at", "gmx.ch"],
        imap: ("imap.gmx.net", 993, Security::Tls),
        smtp: ("mail.gmx.net", 465, Security::Tls),
    },
    Provider {
        id: ProviderId::WebDe,
        name: "WEB.DE",
        preparation: Preparation::EnableImap,
        help_link: Some("web.de"),
        domains: &["web.de"],
        imap: ("imap.web.de", 993, Security::Tls),
        smtp: ("smtp.web.de", 587, Security::StartTls),
    },
    Provider {
        id: ProviderId::Posteo,
        name: "Posteo",
        preparation: Preparation::None,
        help_link: None,
        domains: &[
            "posteo.de",
            "posteo.net",
            "posteo.at",
            "posteo.ch",
            "posteo.eu",
            "posteo.org",
            "posteo.be",
        ],
        imap: ("posteo.de", 993, Security::Tls),
        smtp: ("posteo.de", 465, Security::Tls),
    },
    Provider {
        id: ProviderId::MailboxOrg,
        name: "mailbox.org",
        preparation: Preparation::None,
        help_link: None,
        domains: &["mailbox.org"],
        imap: ("imap.mailbox.org", 993, Security::Tls),
        smtp: ("smtp.mailbox.org", 465, Security::Tls),
    },
    Provider {
        id: ProviderId::TOnline,
        name: "T-Online",
        preparation: Preparation::None,
        help_link: None,
        domains: &["t-online.de", "magenta.de"],
        imap: ("secureimap.t-online.de", 993, Security::Tls),
        smtp: ("securesmtp.t-online.de", 465, Security::Tls),
    },
    Provider {
        id: ProviderId::Microsoft,
        name: "Microsoft",
        preparation: Preparation::Unsupported,
        help_link: None,
        domains: &[
            "outlook.com",
            "outlook.de",
            "hotmail.com",
            "hotmail.de",
            "live.com",
            "live.de",
            "msn.com",
        ],
        imap: NO_SERVER,
        smtp: NO_SERVER,
    },
];

/// Splits an address into local part and lower-case domain, if it looks
/// complete: something before the @, and a domain with an ending of at
/// least two letters.
pub fn split_address(email: &str) -> Option<(&str, String)> {
    let email = email.trim();
    let (local, domain) = email.rsplit_once('@')?;
    let valid_local = !local.is_empty() && !local.contains(char::is_whitespace);
    let domain = domain.to_ascii_lowercase();
    let labels: Vec<&str> = domain.split('.').collect();
    let valid_domain = labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || !c.is_ascii())
        })
        && labels
            .last()
            .is_some_and(|tld| tld.chars().count() >= 2 && tld.chars().all(char::is_alphabetic));
    (valid_local && valid_domain).then_some((local, domain))
}

pub fn is_valid_address(email: &str) -> bool {
    split_address(email).is_some()
}

/// The provider an address belongs to, judged by its domain alone.
pub fn provider_for_domain(domain: &str) -> Option<&'static Provider> {
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    // Microsoft also has country domains like hotmail.co.uk or live.fr.
    let microsoft = ["outlook.", "hotmail.", "live.", "msn."]
        .iter()
        .any(|prefix| domain.starts_with(prefix));
    PROVIDERS.iter().find(|provider| {
        provider.domains.contains(&domain.as_str())
            || (microsoft && provider.id == ProviderId::Microsoft)
    })
}

pub fn provider_for_address(email: &str) -> Option<&'static Provider> {
    let (_, domain) = split_address(email)?;
    provider_for_domain(&domain)
}

pub fn provider(id: ProviderId) -> &'static Provider {
    PROVIDERS
        .iter()
        .find(|provider| provider.id == id)
        .expect("every provider id has an entry")
}

/// Mail servers (MX) that belong to a provider we do not support, for
/// custom domains hosted there.
pub fn is_unsupported_mail_exchanger(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host.ends_with(".mail.protection.outlook.com") || host.ends_with(".olc.protection.outlook.com")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_addresses() {
        assert!(is_valid_address("name@posteo.de"));
        assert!(is_valid_address(" Name.Last+tag@Mail.Example.COM "));
        assert!(is_valid_address("anna@müller.de"));
        for bad in [
            "",
            "name",
            "name@",
            "@posteo.de",
            "name@posteo",
            "name@posteo.d",
            "na me@posteo.de",
            "name@posteo..de",
            "name@-posteo.de",
            "name@posteo.de1",
        ] {
            assert!(!is_valid_address(bad), "{bad} should be invalid");
        }
        assert_eq!(
            split_address("Max@GMX.de"),
            Some(("Max", "gmx.de".to_owned()))
        );
    }

    #[test]
    fn finds_known_providers() {
        let id = |email: &str| provider_for_address(email).map(|p| p.id);
        assert_eq!(id("a@gmail.com"), Some(ProviderId::Gmail));
        assert_eq!(id("a@googlemail.com"), Some(ProviderId::Gmail));
        assert_eq!(id("a@me.com"), Some(ProviderId::ICloud));
        assert_eq!(id("a@yahoo.de"), Some(ProviderId::Yahoo));
        assert_eq!(id("a@GMX.net"), Some(ProviderId::Gmx));
        assert_eq!(id("a@web.de"), Some(ProviderId::WebDe));
        assert_eq!(id("a@posteo.net"), Some(ProviderId::Posteo));
        assert_eq!(id("a@mailbox.org"), Some(ProviderId::MailboxOrg));
        assert_eq!(id("a@t-online.de"), Some(ProviderId::TOnline));
        assert_eq!(id("a@hotmail.co.uk"), Some(ProviderId::Microsoft));
        assert_eq!(id("a@outlook.de"), Some(ProviderId::Microsoft));
        assert_eq!(id("a@example.org"), None);
        assert_eq!(id("a@gmail"), None);
        // A look-alike domain must not count as the provider.
        assert_eq!(id("a@notgmail.com"), None);
    }

    #[test]
    fn known_providers_have_settings() {
        for provider in PROVIDERS {
            match provider.preparation {
                Preparation::Unsupported => assert!(provider.settings("a@b.de").is_none()),
                _ => {
                    let settings = provider.settings(" a@b.de ").unwrap();
                    assert!(!settings.imap.host.is_empty());
                    assert!(!settings.smtp.host.is_empty());
                    assert_eq!(settings.username, "a@b.de");
                }
            }
            let needs_help = matches!(
                provider.preparation,
                Preparation::AppPassword | Preparation::EnableImap
            );
            assert_eq!(
                needs_help,
                provider.help_url().is_some(),
                "{}",
                provider.name
            );
        }
    }

    #[test]
    fn recognises_microsoft_hosted_domains() {
        assert!(is_unsupported_mail_exchanger(
            "example-org.mail.protection.outlook.com."
        ));
        assert!(!is_unsupported_mail_exchanger("mx.posteo.de"));
    }
}
