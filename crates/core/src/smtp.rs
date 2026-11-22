// SPDX-License-Identifier: GPL-3.0-or-later

//! Sending messages. The sync worker only knows the [`Transport`] trait;
//! [`SmtpTransport`] is the real one, tests use a fake.

use std::future::Future;

use lettre::AsyncTransport;
use lettre::address::{Address, Envelope};

use crate::check;
use crate::model::ServerConfig;
use crate::tls::Trust;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The outgoing mail server did not accept the password.
    #[error("the outgoing mail server rejected the login: {0}")]
    Login(String),
    /// No connection, or the server asked to try again later.
    #[error("could not reach the outgoing mail server: {0}")]
    Unreachable(String),
    /// The server refused the message for good, for example an unknown
    /// recipient or a message that is too large.
    #[error("the outgoing mail server refused the message: {0}")]
    Rejected(String),
}

pub trait Transport: Send + Sync + 'static {
    /// Sends `raw` to `recipients`. The message must already be without
    /// its Bcc header.
    fn send(
        &self,
        from: &str,
        recipients: &[String],
        raw: &[u8],
    ) -> impl Future<Output = Result<(), SendError>> + Send;
}

#[derive(Clone)]
pub struct SmtpTransport {
    pub server: ServerConfig,
    pub username: String,
    pub password: String,
    pub trust: Trust,
}

impl std::fmt::Debug for SmtpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpTransport")
            .field("server", &self.server)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl Transport for SmtpTransport {
    async fn send(&self, from: &str, recipients: &[String], raw: &[u8]) -> Result<(), SendError> {
        let envelope = envelope(from, recipients)?;
        let transport =
            check::smtp_transport(&self.server, &self.username, &self.password, &self.trust)
                .map_err(|error| SendError::Unreachable(error.to_string()))?;
        transport
            .send_raw(&envelope, raw)
            .await
            .map(|_| ())
            .map_err(|error| classify(&error))
    }
}

pub fn envelope(from: &str, recipients: &[String]) -> Result<Envelope, SendError> {
    let parse = |address: &str| {
        address
            .trim()
            .parse::<Address>()
            .map_err(|_| SendError::Rejected(format!("invalid address: {address}")))
    };
    let to = recipients
        .iter()
        .map(|r| parse(r))
        .collect::<Result<Vec<_>, _>>()?;
    Envelope::new(Some(parse(from)?), to).map_err(|error| SendError::Rejected(error.to_string()))
}

fn classify(error: &lettre::transport::smtp::Error) -> SendError {
    let text = error.to_string();
    if check::is_smtp_login_error(error) {
        SendError::Login(text)
    } else if error.is_permanent() {
        SendError::Rejected(text)
    } else {
        SendError::Unreachable(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_needs_valid_addresses() {
        let envelope = envelope(
            "max@example.org",
            &["lena@example.org".into(), "tom@example.org".into()],
        )
        .unwrap();
        assert_eq!(envelope.to().len(), 2);
        assert!(matches!(
            super::envelope("max@example.org", &["not an address".into()]),
            Err(SendError::Rejected(_))
        ));
        assert!(matches!(
            super::envelope("max@example.org", &[]),
            Err(SendError::Rejected(_))
        ));
    }

    #[tokio::test]
    async fn unreachable_server_can_be_tried_again() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let transport = SmtpTransport {
            server: ServerConfig::new("127.0.0.1", port, crate::model::Security::Tls),
            username: "max@example.org".into(),
            password: "secret".into(),
            trust: Trust::system(),
        };
        let error = transport
            .send(
                "max@example.org",
                &["lena@example.org".into()],
                b"Subject: x\r\n\r\nx",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, SendError::Unreachable(_)), "{error:?}");
    }
}
