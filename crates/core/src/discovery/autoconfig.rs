// SPDX-License-Identifier: GPL-3.0-or-later

//! Reads the Thunderbird autoconfig format, which providers publish on their
//! own web servers and Mozilla collects in its database (ISPDB).
//! <https://wiki.mozilla.org/Thunderbird:Autoconfiguration:ConfigFileFormat>

use crate::model::{Security, ServerConfig, ServerSettings};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Candidate {
    server: ServerConfig,
    username: Option<String>,
}

/// The first usable IMAP and SMTP server with password login and
/// encryption, or `None` if the file offers no such pair.
pub fn parse(xml: &str, email: &str) -> Option<ServerSettings> {
    let document = roxmltree::Document::parse(xml).ok()?;
    let provider = document
        .root_element()
        .children()
        .find(|node| node.has_tag_name("emailProvider"))?;

    let pick = |tag: &str, kind: &str| {
        provider
            .children()
            .filter(|node| node.has_tag_name(tag) && node.attribute("type") == Some(kind))
            .find_map(candidate)
    };
    let imap = pick("incomingServer", "imap")?;
    let smtp = pick("outgoingServer", "smtp")?;
    let username = imap
        .username
        .as_deref()
        .map(|template| expand(template, email))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| email.trim().to_owned());
    Some(ServerSettings {
        imap: ServerConfig {
            host: expand(&imap.server.host, email),
            ..imap.server
        },
        smtp: ServerConfig {
            host: expand(&smtp.server.host, email),
            ..smtp.server
        },
        username,
    })
}

fn candidate(node: roxmltree::Node<'_, '_>) -> Option<Candidate> {
    let text = |tag: &str| {
        node.children()
            .find(|child| child.has_tag_name(tag))
            .and_then(|child| child.text())
            .map(str::trim)
    };
    let host = text("hostname").filter(|host| !host.is_empty())?;
    let port: u16 = text("port")?.parse().ok()?;
    let security = match text("socketType")? {
        "SSL" | "TLS" => Security::Tls,
        "STARTTLS" => Security::StartTls,
        _ => return None,
    };
    let methods: Vec<&str> = node
        .children()
        .filter(|child| child.has_tag_name("authentication"))
        .filter_map(|child| child.text())
        .map(str::trim)
        .collect();
    let password_login = methods.is_empty()
        || methods
            .iter()
            .any(|method| matches!(*method, "password-cleartext" | "password-encrypted"));
    if !password_login {
        return None;
    }
    Some(Candidate {
        server: ServerConfig::new(host, port, security),
        username: text("username").map(str::to_owned),
    })
}

fn expand(template: &str, email: &str) -> String {
    let email = email.trim();
    let (local, domain) = email.rsplit_once('@').unwrap_or((email, ""));
    template
        .replace("%EMAILADDRESS%", email)
        .replace("%EMAILLOCALPART%", local)
        .replace("%EMAILDOMAIN%", &domain.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<clientConfig version="1.1">
  <emailProvider id="example.org">
    <domain>example.org</domain>
    <displayName>Example Mail</displayName>
    <incomingServer type="pop3">
      <hostname>pop.example.org</hostname>
      <port>995</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <incomingServer type="imap">
      <hostname>imap-oauth.example.org</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <authentication>OAuth2</authentication>
    </incomingServer>
    <incomingServer type="imap">
      <hostname>plain.example.org</hostname>
      <port>143</port>
      <socketType>plain</socketType>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <incomingServer type="imap">
      <hostname> imap.%EMAILDOMAIN% </hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILLOCALPART%</username>
      <authentication>OAuth2</authentication>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.org</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILLOCALPART%</username>
      <authentication>password-cleartext</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    #[test]
    fn picks_encrypted_password_servers() {
        let settings = parse(EXAMPLE, "max.berger@Example.org").unwrap();
        assert_eq!(
            settings.imap,
            ServerConfig::new("imap.example.org", 993, Security::Tls)
        );
        assert_eq!(
            settings.smtp,
            ServerConfig::new("smtp.example.org", 587, Security::StartTls)
        );
        assert_eq!(settings.username, "max.berger");
    }

    #[test]
    fn defaults_to_full_address_as_username() {
        let xml = EXAMPLE.replace("<username>%EMAILLOCALPART%</username>", "");
        let settings = parse(&xml, "max@example.org").unwrap();
        assert_eq!(settings.username, "max@example.org");
    }

    #[test]
    fn rejects_incomplete_or_broken_files() {
        assert!(parse("", "a@example.org").is_none());
        assert!(parse("<html><body>Not found</body></html>", "a@example.org").is_none());
        let without_smtp = EXAMPLE.replace("type=\"smtp\"", "type=\"other\"");
        assert!(parse(&without_smtp, "a@example.org").is_none());
        let only_plain = r#"<clientConfig><emailProvider id="x">
            <incomingServer type="imap"><hostname>imap.x.de</hostname><port>143</port>
              <socketType>plain</socketType></incomingServer>
            <outgoingServer type="smtp"><hostname>smtp.x.de</hostname><port>25</port>
              <socketType>plain</socketType></outgoingServer>
          </emailProvider></clientConfig>"#;
        assert!(parse(only_plain, "a@x.de").is_none());
    }
}
