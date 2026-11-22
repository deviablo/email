// SPDX-License-Identifier: GPL-3.0-or-later

//! Finds the server settings for an email address. In this order:
//! 1. providers we know ourselves,
//! 2. the provider's own autoconfig file and Mozilla's database (ISPDB),
//! 3. DNS SRV records (RFC 6186),
//! 4. the mail servers (MX) of the domain, which often belong to a known
//!    provider when someone uses their own domain,
//! 5. common host names like `imap.<domain>`, if they answer at all.
//!
//! All network access goes through [`Network`], so the order and the
//! decisions can be tested without a network.

pub mod autoconfig;
pub mod net;
pub mod providers;

use std::future::Future;

use futures::future::join_all;

use crate::model::{Security, ServerConfig, ServerSettings};
use providers::{Preparation, ProviderId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SrvRecord {
    pub priority: u16,
    pub weight: u16,
    pub port: u16,
    pub target: String,
}

pub trait Network: Send + Sync {
    /// The body of a successful HTTPS GET request, if there is one.
    fn fetch(&self, url: &str) -> impl Future<Output = Option<String>> + Send;

    fn srv(&self, name: &str) -> impl Future<Output = Vec<SrvRecord>> + Send;

    /// Mail exchangers of a domain, best first.
    fn mx(&self, domain: &str) -> impl Future<Output = Vec<String>> + Send;

    /// Whether a TCP connection to the port can be opened.
    fn reachable(&self, host: &str, port: u16) -> impl Future<Output = bool> + Send;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Builtin,
    Autoconfig,
    Ispdb,
    Srv,
    MailExchanger,
    Guess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Discovery {
    Found {
        settings: ServerSettings,
        source: Source,
        /// The known provider behind the settings, if any.
        provider: Option<ProviderId>,
    },
    /// The address belongs to a provider that cannot be used with a password.
    Unsupported,
    NotFound,
}

pub const ISPDB_URL: &str = "https://autoconfig.thunderbird.net/v1.1/";

pub async fn discover<N: Network>(email: &str, net: &N) -> Discovery {
    let email = email.trim();
    let Some((_, domain)) = providers::split_address(email) else {
        return Discovery::NotFound;
    };

    if let Some(found) = builtin(&domain, email) {
        return found;
    }
    if let Some(found) = autoconfig(&domain, email, net).await {
        return found;
    }
    if let Some(found) = srv(&domain, email, net).await {
        return found;
    }
    if let Some(found) = mail_exchanger(&domain, email, net).await {
        return found;
    }
    match (
        guess_imap(&domain, net).await,
        guess_smtp(&domain, net).await,
    ) {
        (Some(imap), Some(smtp)) => Discovery::Found {
            settings: ServerSettings {
                imap,
                smtp,
                username: email.to_owned(),
            },
            source: Source::Guess,
            provider: None,
        },
        _ => Discovery::NotFound,
    }
}

fn builtin(domain: &str, email: &str) -> Option<Discovery> {
    let provider = providers::provider_for_domain(domain)?;
    Some(match provider.settings(email) {
        None => Discovery::Unsupported,
        Some(settings) => Discovery::Found {
            settings,
            source: Source::Builtin,
            provider: Some(provider.id),
        },
    })
}

async fn autoconfig<N: Network>(domain: &str, email: &str, net: &N) -> Option<Discovery> {
    let own = format!(
        "https://autoconfig.{domain}/mail/config-v1.1.xml?emailaddress={}",
        percent_encode(email)
    );
    let well_known = format!("https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml");
    let ispdb = format!("{ISPDB_URL}{domain}");
    let (own, well_known, ispdb) =
        futures::join!(net.fetch(&own), net.fetch(&well_known), net.fetch(&ispdb));
    let parse = |body: Option<String>| body.and_then(|xml| autoconfig::parse(&xml, email));
    let (settings, source) = parse(own)
        .or_else(|| parse(well_known))
        .map(|settings| (settings, Source::Autoconfig))
        .or_else(|| parse(ispdb).map(|settings| (settings, Source::Ispdb)))?;
    Some(Discovery::Found {
        settings,
        source,
        provider: None,
    })
}

async fn srv<N: Network>(domain: &str, email: &str, net: &N) -> Option<Discovery> {
    let names = [
        format!("_imaps._tcp.{domain}"),
        format!("_imap._tcp.{domain}"),
        format!("_submissions._tcp.{domain}"),
        format!("_submission._tcp.{domain}"),
    ];
    let [imaps, imap, submissions, submission]: [Vec<SrvRecord>; 4] =
        join_all(names.iter().map(|name| net.srv(name)))
            .await
            .try_into()
            .ok()?;
    // Implicit TLS is preferred over STARTTLS (RFC 8314).
    let imap = best_srv(&imaps, Security::Tls).or_else(|| best_srv(&imap, Security::StartTls))?;
    let smtp = match best_srv(&submissions, Security::Tls)
        .or_else(|| best_srv(&submission, Security::StartTls))
    {
        Some(smtp) => smtp,
        None => guess_smtp(domain, net).await?,
    };
    Some(Discovery::Found {
        settings: ServerSettings {
            imap,
            smtp,
            username: email.to_owned(),
        },
        source: Source::Srv,
        provider: None,
    })
}

fn best_srv(records: &[SrvRecord], security: Security) -> Option<ServerConfig> {
    records
        .iter()
        .map(|record| (record, record.target.trim_end_matches('.')))
        // A target of "." means the service is deliberately not offered.
        .filter(|(record, target)| !target.is_empty() && record.port != 0)
        .min_by_key(|(record, _)| (record.priority, std::cmp::Reverse(record.weight)))
        .map(|(record, target)| ServerConfig::new(target, record.port, security))
}

async fn mail_exchanger<N: Network>(domain: &str, email: &str, net: &N) -> Option<Discovery> {
    let exchangers = net.mx(domain).await;
    if exchangers
        .iter()
        .any(|host| providers::is_unsupported_mail_exchanger(host))
    {
        return Some(Discovery::Unsupported);
    }
    let host = exchangers
        .first()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    for base in base_domains(&host) {
        if base == domain {
            continue;
        }
        if let Some(provider) = providers::provider_for_domain(&base)
            && provider.preparation != Preparation::Unsupported
            && let Some(settings) = provider.settings(email)
        {
            return Some(Discovery::Found {
                settings,
                source: Source::MailExchanger,
                provider: Some(provider.id),
            });
        }
        if let Some(settings) = net
            .fetch(&format!("{ISPDB_URL}{base}"))
            .await
            .and_then(|xml| autoconfig::parse(&xml, email))
        {
            return Some(Discovery::Found {
                settings,
                source: Source::MailExchanger,
                provider: None,
            });
        }
    }
    None
}

/// The registrable domain of a host, roughly: the last two labels, or the
/// last three for endings like `co.uk`. Without a public suffix list this
/// is a guess, so both are tried when they differ.
fn base_domains(host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    let mut bases = Vec::new();
    if labels.len() >= 3 && labels[labels.len() - 2].len() <= 3 {
        bases.push(labels[labels.len() - 3..].join("."));
    }
    if labels.len() >= 2 {
        bases.push(labels[labels.len() - 2..].join("."));
    }
    bases
}

async fn first_reachable<N: Network>(
    net: &N,
    candidates: Vec<ServerConfig>,
) -> Option<ServerConfig> {
    let answers = join_all(
        candidates
            .iter()
            .map(|candidate| net.reachable(&candidate.host, candidate.port)),
    )
    .await;
    candidates
        .into_iter()
        .zip(answers)
        .find_map(|(candidate, reachable)| reachable.then_some(candidate))
}

async fn guess_imap<N: Network>(domain: &str, net: &N) -> Option<ServerConfig> {
    let mut candidates = Vec::new();
    for (port, security) in [(993, Security::Tls), (143, Security::StartTls)] {
        for host in [format!("imap.{domain}"), format!("mail.{domain}")] {
            candidates.push(ServerConfig::new(host, port, security));
        }
    }
    first_reachable(net, candidates).await
}

async fn guess_smtp<N: Network>(domain: &str, net: &N) -> Option<ServerConfig> {
    let mut candidates = Vec::new();
    for (port, security) in [(465, Security::Tls), (587, Security::StartTls)] {
        for host in [format!("smtp.{domain}"), format!("mail.{domain}")] {
            candidates.push(ServerConfig::new(host, port, security));
        }
    }
    first_reachable(net, candidates).await
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// True if the servers came from unsigned DNS answers (SRV, MX, guessing)
/// and neither belong to the mail domain nor to a known provider. A
/// manipulated network could point there to collect the password, so the
/// user sees the server names before anything is sent.
pub fn needs_confirmation(
    email: &str,
    settings: &ServerSettings,
    source: Source,
    provider: Option<ProviderId>,
) -> bool {
    if provider.is_some() || !matches!(source, Source::Srv | Source::MailExchanger | Source::Guess)
    {
        return false;
    }
    let Some((_, domain)) = providers::split_address(email) else {
        return true;
    };
    let domain = domain.to_ascii_lowercase();
    let belongs = |host: &str| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        host == domain || host.ends_with(&format!(".{domain}"))
    };
    !(belongs(&settings.imap.host) && belongs(&settings.smtp.host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeNetwork {
        pages: HashMap<String, String>,
        srv: HashMap<String, Vec<SrvRecord>>,
        mx: HashMap<String, Vec<String>>,
        open_ports: HashSet<(String, u16)>,
        requests: Mutex<Vec<String>>,
    }

    impl FakeNetwork {
        fn page(mut self, url: &str, body: &str) -> Self {
            self.pages.insert(url.into(), body.into());
            self
        }

        fn srv(mut self, name: &str, target: &str, port: u16, priority: u16) -> Self {
            self.srv.entry(name.into()).or_default().push(SrvRecord {
                priority,
                weight: 0,
                port,
                target: target.into(),
            });
            self
        }

        fn mx(mut self, domain: &str, host: &str) -> Self {
            self.mx.entry(domain.into()).or_default().push(host.into());
            self
        }

        fn open(mut self, host: &str, port: u16) -> Self {
            self.open_ports.insert((host.into(), port));
            self
        }

        fn requested(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Network for FakeNetwork {
        async fn fetch(&self, url: &str) -> Option<String> {
            self.requests.lock().unwrap().push(url.to_owned());
            self.pages.get(url).cloned()
        }

        async fn srv(&self, name: &str) -> Vec<SrvRecord> {
            self.srv.get(name).cloned().unwrap_or_default()
        }

        async fn mx(&self, domain: &str) -> Vec<String> {
            self.mx.get(domain).cloned().unwrap_or_default()
        }

        async fn reachable(&self, host: &str, port: u16) -> bool {
            self.open_ports.contains(&(host.to_owned(), port))
        }
    }

    fn config(host: &str, imap_port: u16, smtp_port: u16) -> String {
        format!(
            r#"<clientConfig version="1.1"><emailProvider id="x">
              <incomingServer type="imap"><hostname>imap.{host}</hostname><port>{imap_port}</port>
                <socketType>SSL</socketType><username>%EMAILADDRESS%</username></incomingServer>
              <outgoingServer type="smtp"><hostname>smtp.{host}</hostname><port>{smtp_port}</port>
                <socketType>SSL</socketType><username>%EMAILADDRESS%</username></outgoingServer>
            </emailProvider></clientConfig>"#
        )
    }

    fn found(discovery: Discovery) -> (ServerSettings, Source, Option<ProviderId>) {
        match discovery {
            Discovery::Found {
                settings,
                source,
                provider,
            } => (settings, source, provider),
            other => panic!("expected settings, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn known_providers_need_no_network() {
        let net = FakeNetwork::default();
        let (settings, source, provider) = found(discover("max@posteo.de", &net).await);
        assert_eq!(source, Source::Builtin);
        assert_eq!(provider, Some(ProviderId::Posteo));
        assert_eq!(settings.imap.host, "posteo.de");
        assert!(net.requested().is_empty());

        assert_eq!(
            discover("max@outlook.com", &net).await,
            Discovery::Unsupported
        );
        assert_eq!(discover("not an address", &net).await, Discovery::NotFound);
    }

    #[tokio::test]
    async fn provider_autoconfig_wins_over_ispdb() {
        let net = FakeNetwork::default()
            .page(
                "https://autoconfig.example.org/mail/config-v1.1.xml?emailaddress=max%2Btag%40example.org",
                &config("own.example.org", 993, 465),
            )
            .page(
                "https://autoconfig.thunderbird.net/v1.1/example.org",
                &config("ispdb.example.org", 993, 465),
            );
        let (settings, source, _) = found(discover("max+tag@example.org", &net).await);
        assert_eq!(source, Source::Autoconfig);
        assert_eq!(settings.imap.host, "imap.own.example.org");
        assert_eq!(settings.username, "max+tag@example.org");
    }

    #[tokio::test]
    async fn well_known_and_ispdb_are_used() {
        let net = FakeNetwork::default().page(
            "https://example.org/.well-known/autoconfig/mail/config-v1.1.xml",
            &config("wk.example.org", 993, 465),
        );
        let (settings, source, _) = found(discover("a@example.org", &net).await);
        assert_eq!(
            (settings.imap.host.as_str(), source),
            ("imap.wk.example.org", Source::Autoconfig)
        );

        let net = FakeNetwork::default().page(
            "https://autoconfig.thunderbird.net/v1.1/example.org",
            &config("ispdb.example.org", 993, 587),
        );
        let (settings, source, _) = found(discover("a@example.org", &net).await);
        assert_eq!(source, Source::Ispdb);
        assert_eq!(settings.smtp.port, 587);
    }

    #[tokio::test]
    async fn srv_records_follow_rfc_6186() {
        let net = FakeNetwork::default()
            .srv(
                "_imap._tcp.example.org",
                "imap-starttls.example.org.",
                143,
                0,
            )
            .srv("_imaps._tcp.example.org", "backup.example.org.", 993, 20)
            .srv("_imaps._tcp.example.org", "imap.example.org.", 993, 10)
            .srv("_submission._tcp.example.org", "smtp.example.org.", 587, 0)
            .srv("_submissions._tcp.example.org", ".", 0, 0);
        let (settings, source, _) = found(discover("a@example.org", &net).await);
        assert_eq!(source, Source::Srv);
        assert_eq!(
            settings.imap,
            ServerConfig::new("imap.example.org", 993, Security::Tls)
        );
        assert_eq!(
            settings.smtp,
            ServerConfig::new("smtp.example.org", 587, Security::StartTls)
        );
    }

    #[tokio::test]
    async fn own_domains_at_known_providers_are_found_by_mx() {
        let net = FakeNetwork::default().mx("berger.example", "mx01.posteo.de.");
        let (settings, source, provider) = found(discover("max@berger.example", &net).await);
        assert_eq!(source, Source::MailExchanger);
        assert_eq!(provider, Some(ProviderId::Posteo));
        assert_eq!(settings.username, "max@berger.example");

        let net = FakeNetwork::default()
            .mx("club.example", "mx.mailhost.co.uk.")
            .page(
                "https://autoconfig.thunderbird.net/v1.1/mailhost.co.uk",
                &config("mailhost.co.uk", 993, 465),
            );
        let (settings, _, _) = found(discover("a@club.example", &net).await);
        assert_eq!(settings.imap.host, "imap.mailhost.co.uk");

        let net = FakeNetwork::default().mx(
            "firma.example",
            "firma-example.mail.protection.outlook.com.",
        );
        assert_eq!(
            discover("a@firma.example", &net).await,
            Discovery::Unsupported
        );
    }

    #[tokio::test]
    async fn common_host_names_are_tried_last() {
        let net = FakeNetwork::default()
            .open("mail.example.org", 993)
            .open("imap.example.org", 143)
            .open("smtp.example.org", 587);
        let (settings, source, _) = found(discover("a@example.org", &net).await);
        assert_eq!(source, Source::Guess);
        assert_eq!(
            settings.imap,
            ServerConfig::new("mail.example.org", 993, Security::Tls)
        );
        assert_eq!(
            settings.smtp,
            ServerConfig::new("smtp.example.org", 587, Security::StartTls)
        );
    }

    #[tokio::test]
    async fn nothing_found() {
        let net = FakeNetwork::default().open("imap.example.org", 993);
        assert_eq!(discover("a@example.org", &net).await, Discovery::NotFound);
    }

    #[test]
    fn base_domains_of_hosts() {
        assert_eq!(base_domains("mx01.posteo.de"), ["posteo.de"]);
        assert_eq!(
            base_domains("mx.mailhost.co.uk"),
            ["mailhost.co.uk", "co.uk"]
        );
        assert_eq!(base_domains("aspmx.l.google.com"), ["google.com"]);
        assert!(base_domains("localhost").is_empty());
    }

    #[test]
    fn foreign_servers_from_dns_need_a_look() {
        use crate::model::{Security, ServerConfig};
        let settings = |imap: &str, smtp: &str| ServerSettings {
            imap: ServerConfig::new(imap, 993, Security::Tls),
            smtp: ServerConfig::new(smtp, 465, Security::Tls),
            username: "max@berger.example".into(),
        };
        let own = settings("imap.berger.example", "mail.berger.example.");
        let foreign = settings("mx01.hoster.example", "mx01.hoster.example");
        let email = "max@berger.example";
        assert!(!needs_confirmation(email, &own, Source::Srv, None));
        assert!(needs_confirmation(email, &foreign, Source::Srv, None));
        assert!(needs_confirmation(
            email,
            &foreign,
            Source::MailExchanger,
            None
        ));
        assert!(needs_confirmation(email, &foreign, Source::Guess, None));
        assert!(!needs_confirmation(
            email,
            &foreign,
            Source::Autoconfig,
            None
        ));
        assert!(!needs_confirmation(email, &foreign, Source::Ispdb, None));
        assert!(!needs_confirmation(email, &foreign, Source::Builtin, None));
        assert!(!needs_confirmation(
            email,
            &foreign,
            Source::MailExchanger,
            Some(ProviderId::Posteo)
        ));
        // A look-alike domain is not the own domain.
        let lookalike = settings("imap.berger.example.evil.example", "imap.berger.example");
        assert!(needs_confirmation(email, &lookalike, Source::Srv, None));
    }
}
