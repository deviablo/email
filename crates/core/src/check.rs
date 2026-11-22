// SPDX-License-Identifier: GPL-3.0-or-later

//! Checks new or changed account settings for real: signs in to IMAP and
//! SMTP and opens the inbox, before anything is saved.

use std::net::Ipv4Addr;
use std::time::Duration;

use lettre::Tokio1Executor;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Certificate, CertificateStore, Tls, TlsParameters};
use lettre::transport::smtp::extension::ClientId;

use crate::discovery::providers::{Preparation, Provider};
use crate::imap::ImapConnector;
use crate::model::{Security, ServerConfig, ServerSettings};
use crate::server::{Connector, MailServer, ServerError};
use crate::tls::Trust;

const SMTP_TIMEOUT: Duration = Duration::from_secs(20);

/// The parts of the check, in the order they happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    SignIn,
    OpenMailbox,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    #[error("the incoming mail server rejected the login: {0}")]
    ImapLogin(String),
    #[error("could not connect to the incoming mail server: {0}")]
    ImapConnection(String),
    #[error("the outgoing mail server rejected the login: {0}")]
    SmtpLogin(String),
    #[error("could not connect to the outgoing mail server: {0}")]
    SmtpConnection(String),
    #[error("the mailbox could not be opened: {0}")]
    Mailbox(String),
}

impl CheckError {
    pub fn is_login(&self) -> bool {
        matches!(self, CheckError::ImapLogin(_) | CheckError::SmtpLogin(_))
    }
}

/// What the setup tells the user after a failed check. The texts live in
/// the user interface; this decides which one fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advice {
    /// Wrong password, nothing special known about the provider.
    CheckPassword,
    /// The provider needs an app password, not the normal one.
    UseAppPassword,
    /// Wrong password, or access for mail apps is still switched off.
    CheckPasswordAndAccess,
    /// The incoming mail server did not answer.
    ImapUnreachable,
    /// Receiving works, but the outgoing mail server did not answer.
    SmtpUnreachable,
    /// Signed in, but the mailbox could not be opened.
    MailboxFailed,
}

pub fn advice(error: &CheckError, provider: Option<&Provider>) -> Advice {
    match error {
        CheckError::ImapLogin(_) | CheckError::SmtpLogin(_) => {
            match provider.map(|provider| provider.preparation) {
                Some(Preparation::AppPassword) => Advice::UseAppPassword,
                Some(Preparation::EnableImap) => Advice::CheckPasswordAndAccess,
                _ => Advice::CheckPassword,
            }
        }
        CheckError::ImapConnection(_) => Advice::ImapUnreachable,
        CheckError::SmtpConnection(_) => Advice::SmtpUnreachable,
        CheckError::Mailbox(_) => Advice::MailboxFailed,
    }
}

/// Runs the whole check. `progress` is called when a stage begins.
pub async fn check_account(
    settings: &ServerSettings,
    password: &str,
    trust: &Trust,
    mut progress: impl FnMut(Stage) + Send,
) -> Result<(), CheckError> {
    progress(Stage::SignIn);
    let connector = ImapConnector {
        server: settings.imap.clone(),
        username: settings.username.clone(),
        password: password.to_owned(),
        trust: trust.clone(),
    };
    let mut server = connector.connect().await.map_err(|error| match error {
        ServerError::Authentication(reason) => CheckError::ImapLogin(reason),
        other => CheckError::ImapConnection(other.to_string()),
    })?;
    if let Err(error) = check_smtp(&settings.smtp, &settings.username, password, trust).await {
        server.logout().await;
        return Err(error);
    }

    progress(Stage::OpenMailbox);
    let result = async {
        let folders = server.list_folders().await?;
        let inbox = folders
            .iter()
            .find(|folder| folder.path.eq_ignore_ascii_case("INBOX"))
            .map_or("INBOX", |folder| folder.path.as_str());
        server.select(inbox).await.map(|_| ())
    }
    .await;
    server.logout().await;
    result.map_err(|error| CheckError::Mailbox(error.to_string()))
}

/// Signs in to the incoming mail server only, for example after the
/// password was changed.
pub async fn check_imap_login(
    settings: &ServerSettings,
    password: &str,
    trust: &Trust,
) -> Result<(), CheckError> {
    let connector = ImapConnector {
        server: settings.imap.clone(),
        username: settings.username.clone(),
        password: password.to_owned(),
        trust: trust.clone(),
    };
    match connector.connect().await {
        Ok(server) => {
            server.logout().await;
            Ok(())
        }
        Err(ServerError::Authentication(reason)) => Err(CheckError::ImapLogin(reason)),
        Err(other) => Err(CheckError::ImapConnection(other.to_string())),
    }
}

pub fn smtp_transport(
    server: &ServerConfig,
    username: &str,
    password: &str,
    trust: &Trust,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, CheckError> {
    let connection =
        |error: lettre::transport::smtp::Error| CheckError::SmtpConnection(error.to_string());
    let mut parameters = TlsParameters::builder(server.host.clone());
    if !trust.uses_system_roots() {
        parameters = parameters.certificate_store(CertificateStore::None);
    }
    for root in trust.extra_roots() {
        parameters = parameters
            .add_root_certificate(Certificate::from_der(root.clone()).map_err(connection)?);
    }
    let parameters = parameters.build_rustls().map_err(connection)?;
    let tls = match server.security {
        Security::Tls => Tls::Wrapper(parameters),
        Security::StartTls => Tls::Required(parameters),
    };
    Ok(
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(server.host.clone())
            .port(server.port)
            .tls(tls)
            // Thunderbird greets the same way; the real host name of the
            // computer would only reveal something about the user.
            .hello_name(ClientId::Ipv4(Ipv4Addr::LOCALHOST))
            .credentials(Credentials::new(username.to_owned(), password.to_owned()))
            .timeout(Some(SMTP_TIMEOUT))
            .build(),
    )
}

async fn check_smtp(
    server: &ServerConfig,
    username: &str,
    password: &str,
    trust: &Trust,
) -> Result<(), CheckError> {
    let transport = smtp_transport(server, username, password, trust)?;
    match transport.test_connection().await {
        Ok(true) => Ok(()),
        Ok(false) => Err(CheckError::SmtpConnection("no answer".into())),
        Err(error) if is_smtp_login_error(&error) => Err(CheckError::SmtpLogin(error.to_string())),
        Err(error) => Err(CheckError::SmtpConnection(error.to_string())),
    }
}

/// 530 to 535 are the answers to a failed AUTH (RFC 4954).
pub(crate) fn is_smtp_login_error(error: &lettre::transport::smtp::Error) -> bool {
    error
        .status()
        .is_some_and(|code| (530..=535).contains(&code.to_string().parse::<u16>().unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::providers::{ProviderId, provider};

    #[test]
    fn advice_fits_the_provider() {
        let login = CheckError::ImapLogin("NO".into());
        assert_eq!(
            advice(&login, Some(provider(ProviderId::Gmail))),
            Advice::UseAppPassword
        );
        assert_eq!(
            advice(&login, Some(provider(ProviderId::Gmx))),
            Advice::CheckPasswordAndAccess
        );
        assert_eq!(
            advice(&login, Some(provider(ProviderId::Posteo))),
            Advice::CheckPassword
        );
        assert_eq!(advice(&login, None), Advice::CheckPassword);
        assert_eq!(
            advice(
                &CheckError::SmtpLogin("535".into()),
                Some(provider(ProviderId::ICloud))
            ),
            Advice::UseAppPassword
        );
        assert_eq!(
            advice(&CheckError::ImapConnection("refused".into()), None),
            Advice::ImapUnreachable
        );
        assert_eq!(
            advice(&CheckError::SmtpConnection("refused".into()), None),
            Advice::SmtpUnreachable
        );
    }

    #[tokio::test]
    async fn unreachable_server_is_a_connection_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let settings = ServerSettings {
            imap: ServerConfig::new("127.0.0.1", port, Security::Tls),
            smtp: ServerConfig::new("127.0.0.1", port, Security::Tls),
            username: "a@example.org".into(),
        };
        let mut stages = Vec::new();
        let error = check_account(&settings, "secret", &Trust::system(), |stage| {
            stages.push(stage)
        })
        .await
        .unwrap_err();
        assert!(matches!(error, CheckError::ImapConnection(_)), "{error:?}");
        assert_eq!(stages, [Stage::SignIn]);
    }
}
