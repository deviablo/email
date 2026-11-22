// SPDX-License-Identifier: GPL-3.0-or-later

//! The interface between the sync logic and a mail server. The real
//! implementation lives in [`crate::imap`]; tests use an in-memory server.

use std::future::Future;
use std::time::Duration;

use chrono::NaiveDate;

use crate::folder::FolderRole;
use crate::model::Flags;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFolder {
    pub path: String,
    pub delimiter: Option<String>,
    pub role: Option<FolderRole>,
    pub selectable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedFolder {
    pub uid_validity: Option<u32>,
    pub exists: u32,
    /// Only reported by servers with CONDSTORE. If it has not changed since
    /// the last sync, no flags have changed either.
    pub highest_modseq: Option<u64>,
}

/// A flag the app changes on the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagKind {
    Seen,
    Flagged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteHeader {
    pub uid: u32,
    pub flags: Flags,
    /// Arrival time on the server, used when the Date header is missing.
    pub internal_date: Option<i64>,
    pub raw_header: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleOutcome {
    NewData,
    Timeout,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("the server rejected the login")]
    Authentication(String),
    #[error("could not reach the server: {0}")]
    Connection(String),
    #[error("{0}")]
    Protocol(String),
}

pub type ServerResult<T> = Result<T, ServerError>;

pub trait MailServer: Send {
    fn list_folders(&mut self) -> impl Future<Output = ServerResult<Vec<RemoteFolder>>> + Send;

    fn select(&mut self, path: &str) -> impl Future<Output = ServerResult<SelectedFolder>> + Send;

    /// UIDs of all messages that arrived on or after `since` in the selected folder.
    fn search_since(
        &mut self,
        since: NaiveDate,
    ) -> impl Future<Output = ServerResult<Vec<u32>>> + Send;

    /// UIDs of all messages in the selected folder.
    fn search_all(&mut self) -> impl Future<Output = ServerResult<Vec<u32>>> + Send;

    /// Number of unread messages in the selected folder.
    fn count_unseen(&mut self) -> impl Future<Output = ServerResult<u32>> + Send;

    fn fetch_headers(
        &mut self,
        uids: &[u32],
    ) -> impl Future<Output = ServerResult<Vec<RemoteHeader>>> + Send;

    fn fetch_flags(
        &mut self,
        uids: &[u32],
    ) -> impl Future<Output = ServerResult<Vec<(u32, Flags)>>> + Send;

    /// Flags of all messages changed after `modseq`. Only called when the
    /// server reported a modification sequence on select.
    fn fetch_changed_flags(
        &mut self,
        modseq: u64,
    ) -> impl Future<Output = ServerResult<Vec<(u32, Flags)>>> + Send;

    /// The complete message. Does not mark it as read.
    fn fetch_message(&mut self, uid: u32) -> impl Future<Output = ServerResult<Vec<u8>>> + Send;

    fn set_flag(
        &mut self,
        uids: &[u32],
        flag: FlagKind,
        value: bool,
    ) -> impl Future<Output = ServerResult<()>> + Send;

    /// Moves messages of the selected folder to another folder.
    fn move_messages(
        &mut self,
        uids: &[u32],
        target: &str,
    ) -> impl Future<Output = ServerResult<()>> + Send;

    /// Removes messages of the selected folder for good.
    fn delete_messages(&mut self, uids: &[u32]) -> impl Future<Output = ServerResult<()>> + Send;

    /// Adds a message to a folder, marked as read and, for drafts, as draft.
    fn save_message(
        &mut self,
        path: &str,
        draft: bool,
        raw: &[u8],
    ) -> impl Future<Output = ServerResult<()>> + Send;

    /// UIDs of the messages in the selected folder with this Message-ID
    /// (given without `<>`).
    fn search_message_id(
        &mut self,
        message_id: &str,
    ) -> impl Future<Output = ServerResult<Vec<u32>>> + Send;

    /// UIDs of the messages in the selected folder that contain every one
    /// of the words, in the headers or the text.
    fn search_text(
        &mut self,
        words: &[String],
    ) -> impl Future<Output = ServerResult<Vec<u32>>> + Send;

    fn create_mailbox(&mut self, path: &str) -> impl Future<Output = ServerResult<()>> + Send;

    /// True if the provider files sent messages in "Sent" by itself, as
    /// Gmail does. The app must not add a second copy then.
    fn saves_sent_mail(&self) -> bool;

    /// Waits until the selected folder changes or the timeout expires.
    fn idle(&mut self, timeout: Duration)
    -> impl Future<Output = ServerResult<IdleOutcome>> + Send;

    fn logout(self) -> impl Future<Output = ()> + Send;
}

/// Opens new, logged-in connections for one account.
pub trait Connector: Send + Sync + 'static {
    type Server: MailServer + 'static;

    fn connect(&self) -> impl Future<Output = ServerResult<Self::Server>> + Send;
}

/// Turns a sorted list of UIDs into a compact IMAP sequence set like `1:4,7`.
pub fn uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts = Vec::new();
    let mut iter = sorted.into_iter().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek() == Some(&(end + 1)) {
            end = iter.next().unwrap_or(end);
        }
        if start == end {
            parts.push(start.to_string());
        } else {
            parts.push(format!("{start}:{end}"));
        }
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::uid_set;

    #[test]
    fn compresses_ranges() {
        assert_eq!(uid_set(&[7, 1, 2, 3, 4, 9, 10]), "1:4,7,9:10");
        assert_eq!(uid_set(&[5]), "5");
        assert_eq!(uid_set(&[]), "");
    }
}
