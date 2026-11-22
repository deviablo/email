// SPDX-License-Identifier: GPL-3.0-or-later

//! [`MailServer`] over a real IMAP connection. Only encrypted connections
//! are offered: TLS from the start (usually port 993) or STARTTLS (port 143).

use std::time::Duration;

use async_imap::types::{Fetch, Flag, Mailbox, NameAttribute};
use async_imap::{Client, Session};
use chrono::NaiveDate;
use futures::TryStreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::folder::FolderRole;
use crate::model::{Flags, Security, ServerConfig};
use crate::server::{
    Connector, FlagKind, IdleOutcome, MailServer, RemoteFolder, RemoteHeader, SelectedFolder,
    ServerError, ServerResult, uid_set,
};
use crate::tls::Trust;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_secs(60);

type ImapSession = Session<TlsStream<TcpStream>>;

#[derive(Clone)]
pub struct ImapConnector {
    pub server: ServerConfig,
    pub username: String,
    pub password: String,
    pub trust: Trust,
}

impl std::fmt::Debug for ImapConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapConnector")
            .field("server", &self.server)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl Connector for ImapConnector {
    type Server = ImapServer;

    async fn connect(&self) -> ServerResult<ImapServer> {
        tokio::time::timeout(CONNECT_TIMEOUT, self.connect_inner())
            .await
            .map_err(|_| ServerError::Connection("timed out".into()))?
    }
}

impl ImapConnector {
    async fn connect_inner(&self) -> ServerResult<ImapServer> {
        let host = self.server.host.as_str();
        let tcp = TcpStream::connect((host, self.server.port))
            .await
            .map_err(connection_error)?;
        let starttls = self.server.security == Security::StartTls;
        let tcp = if starttls { start_tls(tcp).await? } else { tcp };

        let name = ServerName::try_from(host.to_owned())
            .map_err(|_| ServerError::Connection(format!("invalid host name {host}")))?;
        let tls = TlsConnector::from(self.trust.client_config())
            .connect(name, tcp)
            .await
            .map_err(connection_error)?;

        let mut client = Client::new(tls);
        if !starttls {
            client
                .read_response()
                .await
                .map_err(connection_error)?
                .ok_or_else(|| ServerError::Connection("no greeting from server".into()))?;
        }
        let mut session = client.login(&self.username, &self.password).await.map_err(
            |(error, _)| match error {
                async_imap::error::Error::No(reason) | async_imap::error::Error::Bad(reason) => {
                    ServerError::Authentication(reason)
                }
                other => protocol_error(other),
            },
        )?;
        let capabilities = session.capabilities().await.map_err(protocol_error)?;
        Ok(ImapServer {
            session: Some(session),
            supports_idle: capabilities.has_str("IDLE"),
            supports_special_use: capabilities.has_str("SPECIAL-USE"),
            supports_condstore: capabilities.has_str("CONDSTORE")
                || capabilities.has_str("QRESYNC"),
            supports_move: capabilities.has_str("MOVE"),
            supports_uidplus: capabilities.has_str("UIDPLUS"),
            // Gmail, including Google Workspace domains.
            saves_sent_mail: capabilities.has_str("X-GM-EXT-1"),
        })
    }
}

/// Plain-text part of STARTTLS: read the greeting, ask to upgrade, wait for OK.
async fn start_tls(tcp: TcpStream) -> ServerResult<TcpStream> {
    let mut reader = BufReader::new(tcp);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(connection_error)?;
    if !line.starts_with("* OK") {
        return Err(ServerError::Connection(format!(
            "unexpected greeting: {}",
            line.trim()
        )));
    }
    reader
        .get_mut()
        .write_all(b"S1 STARTTLS\r\n")
        .await
        .map_err(connection_error)?;
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .await
            .map_err(connection_error)?
            == 0
        {
            return Err(ServerError::Connection("connection closed".into()));
        }
        if let Some(status) = line.strip_prefix("S1 ") {
            if status.starts_with("OK") {
                break;
            }
            return Err(ServerError::Connection(format!(
                "server refused encryption: {}",
                status.trim()
            )));
        }
    }
    if !reader.buffer().is_empty() {
        return Err(ServerError::Connection("unexpected data before TLS".into()));
    }
    Ok(reader.into_inner())
}

fn connection_error(error: impl std::fmt::Display) -> ServerError {
    ServerError::Connection(error.to_string())
}

fn protocol_error(error: async_imap::error::Error) -> ServerError {
    match error {
        async_imap::error::Error::Io(io) => ServerError::Connection(io.to_string()),
        async_imap::error::Error::ConnectionLost => {
            ServerError::Connection("connection lost".into())
        }
        other => ServerError::Protocol(other.to_string()),
    }
}

pub struct ImapServer {
    /// Taken out while the connection is in IDLE.
    session: Option<ImapSession>,
    supports_idle: bool,
    supports_special_use: bool,
    supports_condstore: bool,
    supports_move: bool,
    supports_uidplus: bool,
    saves_sent_mail: bool,
}

impl ImapServer {
    async fn select_once(&mut self, path: &str, condstore: bool) -> ServerResult<Mailbox> {
        let session = self.session()?;
        // async-imap quotes the name itself.
        if condstore {
            session.select_condstore(path).await
        } else {
            session.select(path).await
        }
        .map_err(protocol_error)
    }

    fn session(&mut self) -> ServerResult<&mut ImapSession> {
        self.session
            .as_mut()
            .ok_or_else(|| ServerError::Connection("connection lost".into()))
    }

    async fn uid_fetch(&mut self, uids: &[u32], query: &str) -> ServerResult<Vec<Fetch>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let session = self.session()?;
        let stream = session
            .uid_fetch(uid_set(uids), query)
            .await
            .map_err(protocol_error)?;
        stream.try_collect().await.map_err(protocol_error)
    }

    async fn store(&mut self, uids: &[u32], query: &str) -> ServerResult<()> {
        let session = self.session()?;
        let stream = session
            .uid_store(uid_set(uids), query)
            .await
            .map_err(protocol_error)?;
        let _: Vec<Fetch> = stream.try_collect().await.map_err(protocol_error)?;
        Ok(())
    }

    /// Removes messages already marked as deleted. Without UIDPLUS a plain
    /// EXPUNGE would also remove messages another app only marked; those
    /// are unmarked for the moment and marked again afterwards.
    async fn expunge(&mut self, uids: &[u32]) -> ServerResult<()> {
        if self.supports_uidplus {
            let stream = self
                .session()?
                .uid_expunge(uid_set(uids))
                .await
                .map_err(protocol_error)?;
            let _: Vec<u32> = stream.try_collect().await.map_err(protocol_error)?;
            return Ok(());
        }
        let marked: Vec<u32> = self
            .session()?
            .uid_search("DELETED")
            .await
            .map_err(protocol_error)?
            .into_iter()
            .collect();
        let others = others_marked(&marked, uids);
        if !others.is_empty() {
            self.store(&others, "-FLAGS.SILENT (\\Deleted)").await?;
        }
        let stream = self.session()?.expunge().await.map_err(protocol_error)?;
        let expunged: Result<Vec<u32>, _> = stream.try_collect().await;
        if !others.is_empty() {
            self.store(&others, "+FLAGS.SILENT (\\Deleted)").await?;
        }
        expunged.map_err(protocol_error)?;
        Ok(())
    }

    /// Creates a folder.
    pub async fn create_folder(&mut self, path: &str) -> ServerResult<()> {
        self.session()?.create(path).await.map_err(protocol_error)
    }

    /// Adds a message to a folder. `internal_date` like `01-Jan-2026 10:00:00 +0000`.
    pub async fn append(
        &mut self,
        path: &str,
        flags: Option<&str>,
        internal_date: Option<&str>,
        raw: &[u8],
    ) -> ServerResult<()> {
        // async-imap passes the date as it is, but IMAP wants it quoted.
        let internal_date = internal_date.map(|date| format!("\"{date}\""));
        self.session()?
            .append(path, flags, internal_date.as_deref(), raw)
            .await
            .map_err(protocol_error)
    }
}

/// Messages marked as deleted that the app did not mean to remove.
fn others_marked(marked: &[u32], ours: &[u32]) -> Vec<u32> {
    let mut others: Vec<u32> = marked
        .iter()
        .copied()
        .filter(|uid| !ours.contains(uid))
        .collect();
    others.sort_unstable();
    others.dedup();
    others
}

/// `TEXT` for every word, which IMAP combines with AND. Words outside
/// ASCII would need IMAP literals, which the library cannot send in a
/// search; they are left out here and found by the local search.
fn text_search_query(words: &[String]) -> String {
    words
        .iter()
        .filter(|word| word.is_ascii())
        .map(|word| {
            word.chars()
                .filter(|c| !matches!(c, '"' | '\\') && !c.is_control())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .map(|word| format!("TEXT \"{word}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

fn flags_of(fetch: &Fetch) -> Flags {
    let mut flags = Flags::default();
    for flag in fetch.flags() {
        match flag {
            Flag::Seen => flags.seen = true,
            Flag::Flagged => flags.flagged = true,
            _ => {}
        }
    }
    flags
}

fn role_of(attributes: &[NameAttribute<'_>]) -> Option<FolderRole> {
    attributes.iter().find_map(|attribute| match attribute {
        NameAttribute::Drafts => Some(FolderRole::Drafts),
        NameAttribute::Sent => Some(FolderRole::Sent),
        NameAttribute::Archive => Some(FolderRole::Archive),
        NameAttribute::Junk => Some(FolderRole::Junk),
        NameAttribute::Trash => Some(FolderRole::Trash),
        _ => None,
    })
}

impl MailServer for ImapServer {
    async fn list_folders(&mut self) -> ServerResult<Vec<RemoteFolder>> {
        let special_use = self.supports_special_use;
        let session = self.session()?;
        let names: Vec<_> = session
            .list(Some(""), Some("*"))
            .await
            .map_err(protocol_error)?
            .try_collect()
            .await
            .map_err(protocol_error)?;

        let mut folders: Vec<RemoteFolder> = names
            .iter()
            .map(|name| {
                let attributes = name.attributes();
                let path = name.name().to_owned();
                let delimiter = name.delimiter().map(str::to_owned);
                let role = if path.eq_ignore_ascii_case("INBOX") {
                    Some(FolderRole::Inbox)
                } else if special_use {
                    role_of(attributes)
                } else {
                    None
                }
                .or_else(|| role_of(attributes))
                .or_else(|| FolderRole::from_name(&path, delimiter.as_deref()));
                RemoteFolder {
                    selectable: !attributes
                        .iter()
                        .any(|a| matches!(a, NameAttribute::NoSelect)),
                    path,
                    delimiter,
                    role,
                }
            })
            .collect();

        // Two folders may look like the same standard folder (for example
        // "Trash" and "Deleted Items"); only the first one keeps the role.
        let mut seen = std::collections::HashSet::new();
        for folder in &mut folders {
            if let Some(role) = folder.role
                && !seen.insert(role)
            {
                folder.role = None;
            }
        }
        Ok(folders)
    }

    async fn select(&mut self, path: &str) -> ServerResult<SelectedFolder> {
        let condstore = self.supports_condstore;
        let mut mailbox = self.select_once(path, condstore).await?;
        if mailbox.uid_validity.is_none() {
            // Seen with GreenMail while mail is being delivered: the SELECT
            // answer lacks UIDVALIDITY and the message count, and asking
            // again gives the same. STATUS reports both reliably.
            tracing::debug!(
                "{path}: SELECT answer incomplete (exists {}, uidnext {:?}, {} flags), asking STATUS",
                mailbox.exists,
                mailbox.uid_next,
                mailbox.flags.len()
            );
            let status = self
                .session()?
                .status(path, "(UIDVALIDITY MESSAGES)")
                .await
                .map_err(protocol_error)?;
            mailbox.uid_validity = status.uid_validity;
            mailbox.exists = mailbox.exists.max(status.exists);
            mailbox.highest_modseq = None;
        }
        Ok(SelectedFolder {
            uid_validity: mailbox.uid_validity,
            exists: mailbox.exists,
            highest_modseq: mailbox.highest_modseq.filter(|_| condstore),
        })
    }

    async fn search_all(&mut self) -> ServerResult<Vec<u32>> {
        let uids = self
            .session()?
            .uid_search("ALL")
            .await
            .map_err(protocol_error)?;
        Ok(uids.into_iter().collect())
    }

    async fn search_since(&mut self, since: NaiveDate) -> ServerResult<Vec<u32>> {
        let query = format!("SINCE {}", since.format("%-d-%b-%Y"));
        let uids = self
            .session()?
            .uid_search(query)
            .await
            .map_err(protocol_error)?;
        Ok(uids.into_iter().collect())
    }

    async fn count_unseen(&mut self) -> ServerResult<u32> {
        let uids = self
            .session()?
            .uid_search("UNSEEN")
            .await
            .map_err(protocol_error)?;
        Ok(uids.len() as u32)
    }

    async fn fetch_headers(&mut self, uids: &[u32]) -> ServerResult<Vec<RemoteHeader>> {
        let fetches = self
            .uid_fetch(uids, "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER])")
            .await?;
        Ok(fetches
            .iter()
            .filter_map(|fetch| {
                Some(RemoteHeader {
                    uid: fetch.uid?,
                    flags: flags_of(fetch),
                    internal_date: fetch.internal_date().map(|date| date.timestamp()),
                    raw_header: fetch.header().unwrap_or_default().to_vec(),
                })
            })
            .collect())
    }

    async fn fetch_flags(&mut self, uids: &[u32]) -> ServerResult<Vec<(u32, Flags)>> {
        let fetches = self.uid_fetch(uids, "(UID FLAGS)").await?;
        Ok(fetches
            .iter()
            .filter_map(|fetch| Some((fetch.uid?, flags_of(fetch))))
            .collect())
    }

    async fn fetch_changed_flags(&mut self, modseq: u64) -> ServerResult<Vec<(u32, Flags)>> {
        let session = self.session()?;
        let stream = session
            .uid_fetch("1:*", format!("(UID FLAGS) (CHANGEDSINCE {modseq})"))
            .await
            .map_err(protocol_error)?;
        let fetches: Vec<Fetch> = stream.try_collect().await.map_err(protocol_error)?;
        Ok(fetches
            .iter()
            .filter_map(|fetch| Some((fetch.uid?, flags_of(fetch))))
            .collect())
    }

    async fn set_flag(&mut self, uids: &[u32], flag: FlagKind, value: bool) -> ServerResult<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let name = match flag {
            FlagKind::Seen => "\\Seen",
            FlagKind::Flagged => "\\Flagged",
        };
        let sign = if value { '+' } else { '-' };
        self.store(uids, &format!("{sign}FLAGS.SILENT ({name})"))
            .await
    }

    async fn move_messages(&mut self, uids: &[u32], target: &str) -> ServerResult<()> {
        if uids.is_empty() {
            return Ok(());
        }
        if self.supports_move {
            return self
                .session()?
                .uid_mv(uid_set(uids), target)
                .await
                .map_err(protocol_error);
        }
        self.session()?
            .uid_copy(uid_set(uids), target)
            .await
            .map_err(protocol_error)?;
        self.delete_messages(uids).await
    }

    async fn delete_messages(&mut self, uids: &[u32]) -> ServerResult<()> {
        if uids.is_empty() {
            return Ok(());
        }
        self.store(uids, "+FLAGS.SILENT (\\Deleted)").await?;
        self.expunge(uids).await
    }

    async fn fetch_message(&mut self, uid: u32) -> ServerResult<Vec<u8>> {
        let fetches = self.uid_fetch(&[uid], "(UID BODY.PEEK[])").await?;
        fetches
            .iter()
            .find(|fetch| fetch.uid == Some(uid))
            .and_then(|fetch| fetch.body())
            .map(<[u8]>::to_vec)
            .ok_or_else(|| ServerError::Protocol(format!("message {uid} not found")))
    }

    async fn save_message(&mut self, path: &str, draft: bool, raw: &[u8]) -> ServerResult<()> {
        let flags = if draft {
            "(\\Seen \\Draft)"
        } else {
            "(\\Seen)"
        };
        self.append(path, Some(flags), None, raw).await
    }

    async fn search_message_id(&mut self, message_id: &str) -> ServerResult<Vec<u32>> {
        let id: String = message_id
            .chars()
            .filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n'))
            .collect();
        let found = self
            .session()?
            .uid_search(format!("HEADER Message-ID \"{id}\""))
            .await
            .map_err(protocol_error)?;
        let mut uids: Vec<u32> = found.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    async fn search_text(&mut self, words: &[String]) -> ServerResult<Vec<u32>> {
        let query = text_search_query(words);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let found = self
            .session()?
            .uid_search(query)
            .await
            .map_err(protocol_error)?;
        let mut uids: Vec<u32> = found.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    async fn create_mailbox(&mut self, path: &str) -> ServerResult<()> {
        self.create_folder(path).await
    }

    fn saves_sent_mail(&self) -> bool {
        self.saves_sent_mail
    }

    async fn idle(&mut self, timeout: Duration) -> ServerResult<IdleOutcome> {
        if !self.supports_idle {
            tokio::time::sleep(POLL_INTERVAL.min(timeout)).await;
            self.session()?.noop().await.map_err(protocol_error)?;
            return Ok(IdleOutcome::NewData);
        }
        let session = self
            .session
            .take()
            .ok_or_else(|| ServerError::Connection("connection lost".into()))?;
        let mut handle = session.idle();
        handle.init().await.map_err(protocol_error)?;
        let (wait, _stop) = handle.wait_with_timeout(timeout);
        let response = wait.await.map_err(protocol_error)?;
        let session = handle.done().await.map_err(protocol_error)?;
        self.session = Some(session);
        Ok(match response {
            async_imap::extensions::idle::IdleResponse::NewData(_) => IdleOutcome::NewData,
            _ => IdleOutcome::Timeout,
        })
    }

    async fn logout(mut self) {
        if let Some(mut session) = self.session.take() {
            let _ = session.logout().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{others_marked, text_search_query};

    #[test]
    fn only_our_messages_are_expunged() {
        assert_eq!(others_marked(&[3, 5, 7, 5], &[5]), [3, 7]);
        assert!(others_marked(&[5], &[5]).is_empty());
        assert!(others_marked(&[], &[5]).is_empty());
    }

    #[test]
    fn text_search_combines_words() {
        assert_eq!(
            text_search_query(&["Rechnung".into(), "Mai".into()]),
            "TEXT \"Rechnung\" TEXT \"Mai\""
        );
        assert_eq!(text_search_query(&["Köln".into()]), "");
        assert_eq!(
            text_search_query(&["Köln".into(), "Bahn".into()]),
            "TEXT \"Bahn\""
        );
        assert_eq!(text_search_query(&["a\"b\\".into()]), "TEXT \"ab\"");
        assert_eq!(text_search_query(&["\"".into()]), "");
    }
}
