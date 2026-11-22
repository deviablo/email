// SPDX-License-Identifier: GPL-3.0-or-later

//! The real network for [`super::discover`]: DNS through `hickory-resolver`
//! and a deliberately tiny HTTPS client. Autoconfig files are small and
//! only ever fetched with GET, so a full HTTP library is not needed.

use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;

use super::{Network, SrvRecord};
use crate::tls::Trust;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const DNS_TIMEOUT: Duration = Duration::from_secs(8);
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_BODY: usize = 512 * 1024;
const MAX_REDIRECTS: usize = 3;

pub struct RealNetwork {
    resolver: Option<TokioResolver>,
    trust: Trust,
}

impl RealNetwork {
    pub fn new(trust: Trust) -> RealNetwork {
        let resolver = TokioResolver::builder_tokio()
            .and_then(|builder| builder.build())
            .map_err(|error| tracing::warn!("DNS lookups unavailable: {error}"))
            .ok();
        RealNetwork { resolver, trust }
    }

    async fn get(&self, url: &str) -> Result<Response, String> {
        let url = Url::parse(url).ok_or("only https URLs are supported")?;
        let tcp = TcpStream::connect((url.host.as_str(), url.port))
            .await
            .map_err(|e| e.to_string())?;
        let name = ServerName::try_from(url.host.clone()).map_err(|e| e.to_string())?;
        let mut tls = TlsConnector::from(self.trust.client_config())
            .connect(name, tcp)
            .await
            .map_err(|e| e.to_string())?;
        // HTTP/1.0 keeps the response simple: no chunked encoding, and the
        // body ends when the server closes the connection.
        let request = format!(
            "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: E-Mail\r\nAccept: application/xml, text/xml\r\nConnection: close\r\n\r\n",
            url.path, url.host
        );
        tls.write_all(request.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        let mut limited = (&mut tls).take((MAX_BODY + 16 * 1024) as u64);
        // Many servers close TLS without a close_notify; what arrived is fine.
        let _ = limited.read_to_end(&mut raw).await;
        parse_response(&raw).ok_or_else(|| "malformed HTTP response".to_owned())
    }
}

impl Network for RealNetwork {
    async fn fetch(&self, url: &str) -> Option<String> {
        let mut url = url.to_owned();
        for _ in 0..=MAX_REDIRECTS {
            let response = match tokio::time::timeout(HTTP_TIMEOUT, self.get(&url)).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    tracing::debug!("autoconfig {url}: {error}");
                    return None;
                }
                Err(_) => {
                    tracing::debug!("autoconfig {url}: timed out");
                    return None;
                }
            };
            match response.status {
                200 => return String::from_utf8(response.body).ok(),
                301 | 302 | 303 | 307 | 308 => url = response.location?,
                status => {
                    tracing::debug!("autoconfig {url}: HTTP {status}");
                    return None;
                }
            }
        }
        None
    }

    async fn srv(&self, name: &str) -> Vec<SrvRecord> {
        let Some(resolver) = &self.resolver else {
            return Vec::new();
        };
        let Ok(Ok(lookup)) = tokio::time::timeout(DNS_TIMEOUT, resolver.srv_lookup(name)).await
        else {
            return Vec::new();
        };
        lookup
            .answers()
            .iter()
            .filter_map(|record| match &record.data {
                RData::SRV(srv) => Some(SrvRecord {
                    priority: srv.priority,
                    weight: srv.weight,
                    port: srv.port,
                    target: srv.target.to_ascii(),
                }),
                _ => None,
            })
            .collect()
    }

    async fn mx(&self, domain: &str) -> Vec<String> {
        let Some(resolver) = &self.resolver else {
            return Vec::new();
        };
        let Ok(Ok(lookup)) = tokio::time::timeout(DNS_TIMEOUT, resolver.mx_lookup(domain)).await
        else {
            return Vec::new();
        };
        let mut exchangers: Vec<(u16, String)> = lookup
            .answers()
            .iter()
            .filter_map(|record| match &record.data {
                RData::MX(mx) => Some((mx.preference, mx.exchange.to_ascii())),
                _ => None,
            })
            .collect();
        exchangers.sort();
        exchangers.into_iter().map(|(_, host)| host).collect()
    }

    async fn reachable(&self, host: &str, port: u16) -> bool {
        matches!(
            tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect((host, port))).await,
            Ok(Ok(_))
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Url {
    host: String,
    port: u16,
    path: String,
}

impl Url {
    fn parse(url: &str) -> Option<Url> {
        let rest = url.strip_prefix("https://")?;
        let (authority, path) = match rest.find(['/', '?']) {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let path = if path.starts_with('?') {
            format!("/{path}")
        } else {
            path.to_owned()
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, 443),
        };
        if host.is_empty() || host.contains(['@', ' ']) {
            return None;
        }
        Some(Url {
            host: host.to_ascii_lowercase(),
            port,
            path,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Response {
    status: u16,
    location: Option<String>,
    body: Vec<u8>,
}

fn parse_response(raw: &[u8]) -> Option<Response> {
    let end = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let location = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("location")
            .then(|| value.trim().to_owned())
    });
    let body = raw[end + 4..].iter().take(MAX_BODY).copied().collect();
    Some(Response {
        status,
        location,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_urls() {
        assert_eq!(
            Url::parse("https://autoconfig.Example.org/mail/config-v1.1.xml?emailaddress=a%40b"),
            Some(Url {
                host: "autoconfig.example.org".into(),
                port: 443,
                path: "/mail/config-v1.1.xml?emailaddress=a%40b".into(),
            })
        );
        assert_eq!(
            Url::parse("https://example.org:8443"),
            Some(Url {
                host: "example.org".into(),
                port: 8443,
                path: "/".into(),
            })
        );
        assert_eq!(Url::parse("http://example.org/"), None);
        assert_eq!(Url::parse("https://user@example.org/"), None);
    }

    #[test]
    fn parses_responses() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\n\r\n<clientConfig/>";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"<clientConfig/>");

        let raw = b"HTTP/1.1 301 Moved\r\nlocation: https://other.example/x\r\n\r\n";
        assert_eq!(
            parse_response(raw).unwrap().location.as_deref(),
            Some("https://other.example/x")
        );
        assert_eq!(parse_response(b"garbage"), None);
    }
}
