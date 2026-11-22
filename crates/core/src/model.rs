// SPDX-License-Identifier: GPL-3.0-or-later

use crate::folder::FolderRole;

pub type AccountId = i64;
pub type FolderId = i64;
pub type MessageId = i64;
pub type ThreadId = i64;
pub type ChangeId = i64;

/// How the connection to a server is encrypted. Unencrypted connections are
/// deliberately not offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Security {
    /// TLS from the first byte, usually ports 993 and 465.
    Tls,
    /// Plain connection upgraded with STARTTLS, usually ports 143 and 587.
    StartTls,
}

impl Security {
    pub fn as_str(self) -> &'static str {
        match self {
            Security::Tls => "tls",
            Security::StartTls => "starttls",
        }
    }

    pub fn parse(value: &str) -> Option<Security> {
        match value {
            "tls" => Some(Security::Tls),
            "starttls" => Some(Security::StartTls),
            _ => None,
        }
    }

    /// The usual encryption for a well-known port.
    pub fn for_port(port: u16) -> Option<Security> {
        match port {
            993 | 465 => Some(Security::Tls),
            143 | 587 => Some(Security::StartTls),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

impl ServerConfig {
    pub fn new(host: impl Into<String>, port: u16, security: Security) -> ServerConfig {
        ServerConfig {
            host: host.into(),
            port,
            security,
        }
    }
}

/// Everything needed to reach the mailbox, apart from the password.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerSettings {
    pub imap: ServerConfig,
    pub smtp: ServerConfig,
    pub username: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: AccountId,
    pub email: String,
    /// The name of the account as the user sees it, for example "Personal".
    pub label: String,
    /// The sender name recipients see.
    pub display_name: String,
    pub settings: ServerSettings,
}

impl Account {
    pub fn username(&self) -> &str {
        &self.settings.username
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewAccount {
    pub email: String,
    pub label: String,
    pub display_name: String,
    pub settings: ServerSettings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    pub account_id: AccountId,
    pub path: String,
    pub name: String,
    pub role: Option<FolderRole>,
    pub unread: u32,
    /// How many levels below another folder of the list this folder sits.
    pub depth: u32,
}

impl Folder {
    /// Trash, spam and drafts keep their messages out of the conversations
    /// shown in other folders.
    pub fn is_separate(&self) -> bool {
        matches!(
            self.role,
            Some(FolderRole::Trash | FolderRole::Junk | FolderRole::Drafts)
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub seen: bool,
    pub flagged: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSummary {
    pub id: MessageId,
    pub folder_id: FolderId,
    pub uid: u32,
    pub thread_id: ThreadId,
    pub message_id: Option<String>,
    pub subject: String,
    pub from_name: String,
    pub from_address: String,
    /// Unix timestamp in seconds.
    pub date: i64,
    pub flags: Flags,
    pub preview: String,
    pub has_attachment: bool,
    pub recipients: String,
}

impl MessageSummary {
    pub fn sender_label(&self) -> &str {
        if self.from_name.is_empty() {
            &self.from_address
        } else {
            &self.from_name
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sender {
    pub name: String,
    pub address: String,
}

/// One row of the message list: a conversation as seen from one folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadSummary {
    pub id: ThreadId,
    /// Subject of the first message.
    pub subject: String,
    /// Senders in the order they wrote, without repeats.
    pub senders: Vec<Sender>,
    /// The newest message.
    pub latest: MessageSummary,
    pub count: usize,
    pub unread: bool,
    pub flagged: bool,
    pub has_attachment: bool,
}
