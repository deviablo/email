// SPDX-License-Identifier: GPL-3.0-or-later

//! Local copy of the folders and messages of one account in SQLite. Every
//! account has its own database file, which keeps accounts strictly apart.
//!
//! Changes the user makes (read, star, move, delete) are applied here at
//! once and queued in the `changes` table. The sync worker sends them to the
//! server and removes them from the queue. Moves and deletions wait a few
//! seconds first so that they can be undone.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::folder::FolderRole;
use crate::model::{
    AccountId, ChangeId, Flags, Folder, FolderId, MessageId, MessageSummary, Sender, ThreadId,
    ThreadSummary,
};
use crate::parse::HeaderInfo;
use crate::search::{self, SearchQuery, SearchResult};
use crate::server::RemoteFolder;
use crate::{thread, utf7};

pub type StoreResult<T> = rusqlite::Result<T>;

const SCHEMA_VERSION: i32 = 5;

/// How far apart replies without `References` may be from the message they
/// answer, for matching by subject.
const SUBJECT_MATCH_WINDOW: i64 = 90 * 86_400;

/// Messages waiting to be saved as draft, sent or removed from drafts. Added
/// in version 4; older version 3 databases only get this table.
const OUTBOX_SCHEMA: &str = "
CREATE TABLE outbox (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    message_id TEXT NOT NULL,
    raw BLOB,
    recipients TEXT NOT NULL DEFAULT '',
    retry_at INTEGER NOT NULL DEFAULT 0,
    error TEXT
);
CREATE INDEX outbox_by_message_id ON outbox (message_id);
";

const SCHEMA: &str = "
CREATE TABLE folders (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    delimiter TEXT,
    role TEXT,
    uid_validity INTEGER,
    highest_modseq INTEGER,
    exists_count INTEGER,
    synced_uid INTEGER,
    unread INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid INTEGER NOT NULL,
    message_id TEXT,
    thread_id INTEGER NOT NULL DEFAULT 0,
    base_subject TEXT NOT NULL DEFAULT '',
    is_reply INTEGER NOT NULL DEFAULT 0,
    has_refs INTEGER NOT NULL DEFAULT 0,
    subject TEXT NOT NULL,
    from_name TEXT NOT NULL,
    from_address TEXT NOT NULL,
    recipients TEXT NOT NULL,
    date INTEGER NOT NULL,
    seen INTEGER NOT NULL,
    flagged INTEGER NOT NULL,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    hidden INTEGER NOT NULL DEFAULT 0,
    preview TEXT NOT NULL DEFAULT '',
    body BLOB,
    UNIQUE (folder_id, uid)
);
CREATE INDEX messages_by_date ON messages (folder_id, date DESC);
CREATE INDEX messages_by_thread ON messages (thread_id);
CREATE INDEX messages_by_message_id ON messages (message_id);
CREATE INDEX messages_by_subject ON messages (base_subject);
CREATE TABLE message_refs (
    message INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ref TEXT NOT NULL
);
CREATE INDEX message_refs_by_ref ON message_refs (ref);
CREATE INDEX message_refs_by_message ON message_refs (message);
CREATE TABLE changes (
    id INTEGER PRIMARY KEY,
    message INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    target INTEGER,
    due INTEGER NOT NULL
);
CREATE INDEX changes_by_message ON changes (message);
CREATE TABLE image_senders (
    domain TEXT PRIMARY KEY
);
";

/// Added without a new schema version, so it is created on every start if
/// it is missing.
const EXTRA_TABLES: &str = "
CREATE TABLE IF NOT EXISTS spam_senders (
    address TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS arrivals (
    message INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    at INTEGER NOT NULL
);
INSERT OR IGNORE INTO arrivals (message, at)
    SELECT id, CAST(strftime('%s', 'now') AS INTEGER) FROM messages;
";

/// The search index. `people` holds sender and recipients. Version 5
/// added the recipients; version 4 databases get a rebuilt index.
const FTS_SCHEMA: &str = "
CREATE VIRTUAL TABLE messages_fts USING fts5 (
    subject, people, body, content = '', contentless_delete = 1
);
CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, subject, people, body)
    VALUES (new.id, new.subject,
            new.from_name || ' ' || new.from_address || ' ' || new.recipients, '');
END;
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    DELETE FROM messages_fts WHERE rowid = old.id;
END;
";

const DROP_FTS: &str = "
DROP TRIGGER IF EXISTS messages_fts_insert;
DROP TRIGGER IF EXISTS messages_fts_delete;
DROP TABLE IF EXISTS messages_fts;
";

/// Older databases only held a cache of the last 30 days without the data
/// needed for conversations; they are rebuilt and loaded again.
const DROP_OLD: &str = "
DROP TRIGGER IF EXISTS messages_fts_insert;
DROP TRIGGER IF EXISTS messages_fts_delete;
DROP TABLE IF EXISTS messages_fts;
DROP TABLE IF EXISTS outbox;
DROP TABLE IF EXISTS changes;
DROP TABLE IF EXISTS message_refs;
DROP TABLE IF EXISTS image_senders;
DROP TABLE IF EXISTS messages;
DROP TABLE IF EXISTS folders;
";

/// The folder a message shows in: its own, or for a move that still waits
/// for the server (to keep "Undo" possible), already the target folder.
/// NULL for messages that are on their way out (deleted, replaced drafts).
macro_rules! shown_in {
    ($m:literal) => {
        concat!(
            "(CASE WHEN ",
            $m,
            ".hidden = 0 THEN ",
            $m,
            ".folder_id ELSE (SELECT c.target FROM changes c WHERE c.message = ",
            $m,
            ".id AND c.kind = 'move' LIMIT 1) END)"
        )
    };
}

const SUMMARY_COLUMNS: &str = concat!(
    "m.id, ",
    shown_in!("m"),
    ", m.uid, m.thread_id, m.message_id, m.subject, \
     m.from_name, m.from_address, m.date, m.seen, m.flagged, m.preview, m.has_attachment, \
     m.recipients"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Seen,
    Unseen,
    Flag,
    Unflag,
    Move,
    /// Removes the message for good, used in the trash.
    Delete,
}

impl ChangeKind {
    fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Seen => "seen",
            ChangeKind::Unseen => "unseen",
            ChangeKind::Flag => "flag",
            ChangeKind::Unflag => "unflag",
            ChangeKind::Move => "move",
            ChangeKind::Delete => "delete",
        }
    }

    fn parse(value: &str) -> Option<ChangeKind> {
        Some(match value {
            "seen" => ChangeKind::Seen,
            "unseen" => ChangeKind::Unseen,
            "flag" => ChangeKind::Flag,
            "unflag" => ChangeKind::Unflag,
            "move" => ChangeKind::Move,
            "delete" => ChangeKind::Delete,
            _ => return None,
        })
    }

    /// Moves and deletions take the message away from its folder.
    pub fn removes(self) -> bool {
        matches!(self, ChangeKind::Move | ChangeKind::Delete)
    }
}

/// A change waiting to be sent to the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingChange {
    pub id: ChangeId,
    pub message: MessageId,
    pub folder: FolderId,
    pub folder_path: String,
    pub uid: u32,
    pub kind: ChangeKind,
    pub target: Option<FolderId>,
    pub target_path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboxKind {
    /// Save as draft, replacing older versions with the same Message-ID.
    Draft,
    /// Send by SMTP, then file a copy in "Sent".
    Send,
    /// Sent already; the copy in "Sent" and removing the draft are left.
    Sent,
    /// Remove all versions from the drafts folder.
    Discard,
}

impl OutboxKind {
    fn parse(value: &str) -> Option<OutboxKind> {
        Some(match value {
            "draft" => OutboxKind::Draft,
            "send" => OutboxKind::Send,
            "sent" => OutboxKind::Sent,
            "discard" => OutboxKind::Discard,
            _ => return None,
        })
    }
}

/// A message on its way to the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxItem {
    pub id: i64,
    pub kind: OutboxKind,
    pub message_id: String,
    pub raw: Vec<u8>,
    /// Envelope recipients for sending.
    pub recipients: Vec<String>,
    /// The last problem, kept so it is reported only once.
    pub error: Option<String>,
}

/// What the sync remembers about a folder between two runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderSyncState {
    pub highest_modseq: Option<u64>,
    /// Number of messages on the server at the last sync.
    pub exists: Option<u32>,
    /// Highest UID seen at the last sync. Newer UIDs are new arrivals.
    pub synced_uid: Option<u32>,
}

/// The result of deleting messages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deletion {
    /// Moves to the trash that can still be undone.
    pub undo: Vec<ChangeId>,
    /// Messages that were already in the trash and are removed for good.
    pub permanent: usize,
}

pub struct Store {
    conn: Connection,
    account: AccountId,
}

impl Store {
    pub fn open(path: &Path, account: AccountId) -> StoreResult<Store> {
        Store::init(Connection::open(path)?, account)
    }

    pub fn open_in_memory(account: AccountId) -> StoreResult<Store> {
        Store::init(Connection::open_in_memory()?, account)
    }

    fn init(conn: Connection, account: AccountId) -> StoreResult<Store> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
        )?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 3 || version == 4 {
            if version == 3 {
                conn.execute_batch(&format!("BEGIN; {OUTBOX_SCHEMA} COMMIT;"))?;
            }
            conn.execute_batch(EXTRA_TABLES)?;
            let mut store = Store { conn, account };
            store.rebuild_index()?;
            store
                .conn
                .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
            return Ok(store);
        } else if version != SCHEMA_VERSION {
            conn.execute_batch(&format!(
                "BEGIN; {DROP_OLD} {SCHEMA} {FTS_SCHEMA} {OUTBOX_SCHEMA} \
                 PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?;
        }
        conn.execute_batch(EXTRA_TABLES)?;
        Ok(Store { conn, account })
    }

    /// Builds the search index anew from the stored messages.
    fn rebuild_index(&mut self) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(&format!("{DROP_FTS} {FTS_SCHEMA}"))?;
        {
            let mut stmt = tx.prepare(
                "SELECT id, subject, from_name || ' ' || from_address || ' ' || recipients, body
                 FROM messages",
            )?;
            let mut insert = tx.prepare(
                "INSERT INTO messages_fts (rowid, subject, people, body) VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id: MessageId = row.get(0)?;
                let subject: String = row.get(1)?;
                let people: String = row.get(2)?;
                let body: Option<Vec<u8>> = row.get(3)?;
                let text = body
                    .map(|raw| {
                        crate::parse::searchable_text(&crate::parse::parse_message(&raw).body)
                    })
                    .unwrap_or_default();
                insert.execute(params![id, subject, people, text])?;
            }
        }
        tx.commit()
    }

    pub fn account(&self) -> AccountId {
        self.account
    }

    /// Unread messages in the inbox, as the account switcher shows them.
    pub fn inbox_unread(&self) -> StoreResult<u32> {
        self.conn
            .query_row(
                "SELECT unread FROM folders WHERE role = 'inbox'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map(Option::unwrap_or_default)
    }

    /// Makes the local folder list match the server: adds new folders,
    /// updates roles and removes folders that no longer exist.
    pub fn sync_folders(&mut self, remote: &[RemoteFolder]) -> StoreResult<()> {
        // Every mailbox has an inbox. A list without one is a broken answer
        // and must not wipe out the folders and mail kept here.
        if !remote
            .iter()
            .any(|folder| folder.role == Some(FolderRole::Inbox))
        {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut upsert = tx.prepare(
                "INSERT INTO folders (path, name, delimiter, role) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (path) DO UPDATE SET name = excluded.name,
                     delimiter = excluded.delimiter, role = excluded.role",
            )?;
            for folder in remote.iter().filter(|folder| folder.selectable) {
                upsert.execute(params![
                    folder.path,
                    display_name(folder),
                    folder.delimiter,
                    folder.role.map(FolderRole::as_str),
                ])?;
            }
            let existing: Vec<(FolderId, String)> = {
                let mut stmt = tx.prepare("SELECT id, path FROM folders")?;
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<StoreResult<_>>()?
            };
            for (id, path) in existing {
                if !remote
                    .iter()
                    .any(|folder| folder.selectable && folder.path == path)
                {
                    tx.execute("DELETE FROM folders WHERE id = ?1", [id])?;
                }
            }
        }
        tx.commit()
    }

    /// Adds a folder the app just created on the server, before the next
    /// folder list shows it.
    pub fn add_folder(&mut self, path: &str, role: FolderRole) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO folders (path, name, role) VALUES (?1, ?1, ?2)
             ON CONFLICT (path) DO UPDATE SET role = excluded.role",
            params![path, role.as_str()],
        )?;
        Ok(())
    }

    /// Standard folders first in a fixed order, then the others as a tree,
    /// each folder directly below its parent.
    pub fn folders(&self) -> StoreResult<Vec<Folder>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, name, role, unread, delimiter FROM folders")?;
        let rows: Vec<(Folder, Option<String>)> = stmt
            .query_map([], |row| Ok((self.folder_from_row(row)?, row.get(5)?)))?
            .collect::<StoreResult<_>>()?;
        let parts = |folder: &Folder, delimiter: &Option<String>| -> Vec<String> {
            match delimiter.as_deref() {
                Some(d) if !d.is_empty() => folder.path.split(d).map(str::to_lowercase).collect(),
                _ => vec![folder.path.to_lowercase()],
            }
        };
        let mut keyed: Vec<(Vec<String>, Folder)> = rows
            .iter()
            .map(|(folder, delimiter)| (parts(folder, delimiter), folder.clone()))
            .collect();
        let known: HashSet<Vec<String>> = keyed
            .iter()
            .filter(|(_, folder)| folder.role.is_none())
            .map(|(key, _)| key.clone())
            .collect();
        for (key, folder) in &mut keyed {
            if folder.role.is_none() {
                folder.depth = (1..key.len())
                    .filter(|end| known.contains(&key[..*end]))
                    .count() as u32;
            }
        }
        keyed.sort_by(|(ka, a), (kb, b)| match (a.role, b.role) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.path.cmp(&b.path)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => ka.cmp(kb),
        });
        Ok(keyed.into_iter().map(|(_, folder)| folder).collect())
    }

    pub fn folder(&self, id: FolderId) -> StoreResult<Option<Folder>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, name, role, unread FROM folders WHERE id = ?1")?;
        stmt.query_row([id], |row| self.folder_from_row(row))
            .optional()
    }

    pub fn folder_by_role(&self, role: FolderRole) -> StoreResult<Option<Folder>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, name, role, unread FROM folders WHERE role = ?1")?;
        stmt.query_row([role.as_str()], |row| self.folder_from_row(row))
            .optional()
    }

    fn folder_from_row(&self, row: &rusqlite::Row<'_>) -> StoreResult<Folder> {
        let role: Option<String> = row.get(3)?;
        let role = role.as_deref().and_then(FolderRole::parse);
        let id: FolderId = row.get(0)?;
        // Drafts are never unread; the folder shows how many there are,
        // including those still waiting to be saved on the server.
        let unread = if role == Some(FolderRole::Drafts) {
            self.draft_count(id)?
        } else {
            row.get(4)?
        };
        Ok(Folder {
            id,
            account_id: self.account,
            path: row.get(1)?,
            name: row.get(2)?,
            role,
            unread,
            depth: 0,
        })
    }

    fn draft_count(&self, folder: FolderId) -> StoreResult<u32> {
        self.conn.query_row(
            "SELECT
                 (SELECT count(*) FROM messages WHERE folder_id = ?1 AND hidden = 0)
               + (SELECT count(*) FROM outbox o WHERE o.kind = 'draft' AND NOT EXISTS
                     (SELECT 1 FROM messages m WHERE m.folder_id = ?1 AND m.hidden = 0
                        AND m.message_id = o.message_id))",
            [folder],
            |row| row.get(0),
        )
    }

    pub fn uid_validity(&self, folder: FolderId) -> StoreResult<Option<u32>> {
        self.conn
            .query_row(
                "SELECT uid_validity FROM folders WHERE id = ?1",
                [folder],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
    }

    /// Stores a new UIDVALIDITY. If it differs from the old one, all cached
    /// messages of the folder are dropped, because their UIDs mean nothing now.
    pub fn set_uid_validity(&self, folder: FolderId, value: Option<u32>) -> StoreResult<bool> {
        let old = self.uid_validity(folder)?;
        if old == value {
            return Ok(false);
        }
        self.conn
            .execute("DELETE FROM messages WHERE folder_id = ?1", [folder])?;
        self.conn.execute(
            "UPDATE folders SET uid_validity = ?2, highest_modseq = NULL, exists_count = NULL,
                 synced_uid = NULL
             WHERE id = ?1",
            params![folder, value],
        )?;
        Ok(old.is_some())
    }

    pub fn sync_state(&self, folder: FolderId) -> StoreResult<FolderSyncState> {
        self.conn
            .query_row(
                "SELECT highest_modseq, exists_count, synced_uid FROM folders WHERE id = ?1",
                [folder],
                |row| {
                    Ok(FolderSyncState {
                        highest_modseq: row.get::<_, Option<i64>>(0)?.map(|v| v as u64),
                        exists: row.get(1)?,
                        synced_uid: row.get(2)?,
                    })
                },
            )
            .optional()
            .map(Option::unwrap_or_default)
    }

    pub fn set_sync_state(&self, folder: FolderId, state: FolderSyncState) -> StoreResult<()> {
        self.conn
            .execute(
                "UPDATE folders SET highest_modseq = ?2, exists_count = ?3, synced_uid = ?4
                 WHERE id = ?1",
                params![
                    folder,
                    state.highest_modseq.map(|v| v as i64),
                    state.exists,
                    state.synced_uid
                ],
            )
            .map(|_| ())
    }

    pub fn set_unread(&self, folder: FolderId, unread: u32) -> StoreResult<()> {
        self.conn
            .execute(
                "UPDATE folders SET unread = ?2 WHERE id = ?1",
                params![folder, unread],
            )
            .map(|_| ())
    }

    fn adjust_unread(conn: &Connection, folder: FolderId, delta: i64) -> StoreResult<()> {
        conn.execute(
            "UPDATE folders SET unread = max(0, unread + ?2) WHERE id = ?1",
            params![folder, delta],
        )
        .map(|_| ())
    }

    /// Unread messages that are about to be moved away. The server still
    /// counts them, the user no longer sees them.
    pub fn hidden_unseen(&self, folder: FolderId) -> StoreResult<u32> {
        self.conn.query_row(
            "SELECT count(*) FROM messages WHERE folder_id = ?1 AND hidden = 1 AND seen = 0",
            [folder],
            |row| row.get(0),
        )
    }

    pub fn uids(&self, folder: FolderId) -> StoreResult<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT uid FROM messages WHERE folder_id = ?1 ORDER BY uid")?;
        stmt.query_map([folder], |row| row.get(0))?.collect()
    }

    /// The local messages of a folder with these UIDs.
    pub fn message_ids(&self, folder: FolderId, uids: &[u32]) -> StoreResult<Vec<MessageId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2 AND hidden = 0")?;
        let mut ids = Vec::new();
        for uid in uids {
            if let Some(id) = stmt
                .query_row(params![folder, uid], |row| row.get(0))
                .optional()?
            {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    pub fn insert_message(
        &mut self,
        folder: FolderId,
        uid: u32,
        header: &HeaderInfo,
        fallback_date: i64,
        flags: Flags,
    ) -> StoreResult<MessageId> {
        self.insert_messages(folder, &[(uid, header.clone(), fallback_date, flags)])?;
        self.conn.query_row(
            "SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2",
            params![folder, uid],
            |row| row.get(0),
        )
    }

    /// Adds messages known by their headers and puts each one into its
    /// conversation. Messages that are already there are left alone.
    pub fn insert_messages(
        &mut self,
        folder: FolderId,
        messages: &[(u32, HeaderInfo, i64, Flags)],
    ) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        for (uid, header, fallback_date, flags) in messages {
            let date = header.date.unwrap_or(*fallback_date);
            let parents = thread::parent_ids(
                &header.references,
                &header.in_reply_to,
                header.message_id.as_deref(),
            );
            let inserted = tx.execute(
                "INSERT INTO messages (folder_id, uid, message_id, base_subject, is_reply, has_refs,
                                       subject, from_name, from_address, recipients, date, seen,
                                       flagged, has_attachment)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT (folder_id, uid) DO NOTHING",
                params![
                    folder,
                    uid,
                    header.message_id,
                    thread::base_subject(&header.subject),
                    thread::is_reply(&header.subject),
                    !parents.is_empty(),
                    header.subject,
                    header.from_name,
                    header.from_address,
                    header.to,
                    date,
                    flags.seen,
                    flags.flagged,
                    header.likely_attachment,
                ],
            )?;
            if inserted == 0 {
                continue;
            }
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT OR REPLACE INTO arrivals (message, at)
                 VALUES (?1, CAST(strftime('%s', 'now') AS INTEGER))",
                [id],
            )?;
            for parent in &parents {
                tx.execute(
                    "INSERT INTO message_refs (message, ref) VALUES (?1, ?2)",
                    params![id, parent],
                )?;
            }
            assign_thread(&tx, id, header, &parents, date)?;
        }
        tx.commit()
    }

    /// Takes the flags the server reports. Messages with changes that have
    /// not reached the server yet keep their local state. Returns true if
    /// anything changed.
    pub fn set_flags(&self, folder: FolderId, uid: u32, flags: Flags) -> StoreResult<bool> {
        self.conn
            .execute(
                "UPDATE messages SET seen = ?3, flagged = ?4
                 WHERE folder_id = ?1 AND uid = ?2 AND (seen != ?3 OR flagged != ?4)
                   AND NOT EXISTS (SELECT 1 FROM changes WHERE changes.message = messages.id)",
                params![folder, uid, flags.seen, flags.flagged],
            )
            .map(|changed| changed > 0)
    }

    pub fn remove_messages(&mut self, folder: FolderId, uids: &[u32]) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM messages WHERE folder_id = ?1 AND uid = ?2")?;
            for uid in uids {
                stmt.execute(params![folder, uid])?;
            }
        }
        tx.commit()
    }

    /// Single messages of a folder, newest first.
    pub fn messages(&self, folder: FolderId) -> StoreResult<Vec<MessageSummary>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM messages m WHERE {SHOWN_IN} = ?1
             ORDER BY m.date DESC, m.uid DESC"
        ))?;
        stmt.query_map([folder], summary_from_row)?.collect()
    }

    pub fn message(&self, id: MessageId) -> StoreResult<Option<MessageSummary>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM messages m WHERE m.id = ?1"
        ))?;
        stmt.query_row([id], summary_from_row).optional()
    }

    pub fn recipients(&self, id: MessageId) -> StoreResult<String> {
        self.conn.query_row(
            "SELECT recipients FROM messages WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
    }

    pub fn body(&self, id: MessageId) -> StoreResult<Option<Vec<u8>>> {
        self.conn
            .query_row("SELECT body FROM messages WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map(Option::flatten)
    }

    /// Saves the full message and makes its text searchable.
    pub fn store_body(
        &mut self,
        id: MessageId,
        raw: &[u8],
        preview: &str,
        searchable_text: &str,
        has_attachment: bool,
    ) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE messages SET body = ?2, preview = ?3, has_attachment = ?4 WHERE id = ?1",
            params![id, raw, preview, has_attachment],
        )?;
        tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [id])?;
        tx.execute(
            "INSERT INTO messages_fts (rowid, subject, people, body)
             SELECT id, subject, from_name || ' ' || from_address || ' ' || recipients, ?2
             FROM messages WHERE id = ?1",
            params![id, searchable_text],
        )?;
        tx.commit()
    }

    /// Full-text search in the whole account, newest first.
    pub fn search(&self, query: &str) -> StoreResult<Vec<MessageSummary>> {
        let Some(fts) = search::fts_query(query) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS}
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             WHERE messages_fts MATCH ?1 AND m.hidden = 0
             ORDER BY m.date DESC"
        ))?;
        stmt.query_map([fts], summary_from_row)?.collect()
    }

    /// Search results as conversations, newest first. `extra` are messages
    /// the server found that the local index does not know; a message
    /// found both ways counts once. Without "only this folder", trash and
    /// spam are left out.
    pub fn search_threads(
        &self,
        query: &SearchQuery,
        extra: &[MessageId],
        grouped: bool,
    ) -> StoreResult<Vec<SearchResult>> {
        let mut hits: Vec<MessageSummary> = if query.text.trim().is_empty() {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT {SUMMARY_COLUMNS} FROM messages m WHERE m.hidden = 0
                 ORDER BY m.date DESC LIMIT ?1"
            ))?;
            stmt.query_map([search::MAX_HITS as i64], summary_from_row)?
                .collect::<StoreResult<_>>()?
        } else {
            self.search(&query.text)?
        };
        let known: HashSet<MessageId> = hits.iter().map(|m| m.id).collect();
        for id in extra {
            if !known.contains(id)
                && let Some(message) = self.message(*id)?
            {
                hits.push(message);
            }
        }
        let excluded: HashSet<FolderId> = self
            .folders()?
            .into_iter()
            .filter(|f| matches!(f.role, Some(FolderRole::Trash | FolderRole::Junk)))
            .map(|f| f.id)
            .collect();
        hits.retain(|m| match query.filters.folder {
            Some(folder) => m.folder_id == folder,
            None => !excluded.contains(&m.folder_id),
        });
        hits.sort_by(|a, b| b.date.cmp(&a.date).then(b.id.cmp(&a.id)));

        let mut seen: HashSet<ThreadId> = HashSet::new();
        let mut results = Vec::new();
        for hit in hits {
            let thread = if grouped {
                hit.thread_id
            } else {
                single_thread(hit.id)
            };
            if !seen.insert(thread) {
                continue;
            }
            let messages = self.conversation(thread, hit.folder_id)?;
            if messages.is_empty() {
                continue;
            }
            let thread = summarize(thread, messages);
            if query.filters.accepts(&thread) {
                results.push(SearchResult {
                    thread,
                    folder: hit.folder_id,
                });
            }
            if results.len() >= search::MAX_RESULTS {
                break;
            }
        }
        Ok(results)
    }

    /// Folders the server should search as well: those whose messages are
    /// not all here yet, headers or text.
    pub fn folders_to_search_remotely(&self, only: Option<FolderId>) -> StoreResult<Vec<Folder>> {
        let mut folders = Vec::new();
        for folder in self.folders()? {
            let wanted = match only {
                Some(id) => folder.id == id,
                None => !matches!(folder.role, Some(FolderRole::Trash | FolderRole::Junk)),
            };
            if !wanted {
                continue;
            }
            let state = self.sync_state(folder.id)?;
            let (local, without_text): (u32, u32) = self.conn.query_row(
                "SELECT count(*), coalesce(sum(body IS NULL), 0) FROM messages WHERE folder_id = ?1",
                [folder.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let missing_headers = state.exists.is_none_or(|exists| exists > local);
            if missing_headers || without_text > 0 {
                folders.push(folder);
            }
        }
        Ok(folders)
    }

    /// The conversations that have a message in `folder`, newest first.
    /// Each conversation also shows its messages from other folders (such
    /// as own replies in "Sent"), but never those in trash, spam or drafts.
    /// Inside those three folders only their own messages count.
    pub fn threads(&self, folder: FolderId) -> StoreResult<Vec<ThreadSummary>> {
        let separate = self.folder(folder)?.is_some_and(|f| f.is_separate());
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM messages m
             WHERE {SHOWN}
               AND m.thread_id IN (SELECT n.thread_id FROM messages n WHERE {SHOWN_IN_N} = ?1)
               AND {SCOPE}
             ORDER BY m.thread_id, m.date, m.id"
        ))?;
        let rows: Vec<MessageSummary> = stmt
            .query_map(params![folder, separate], summary_from_row)?
            .collect::<StoreResult<_>>()?;
        let mut threads: Vec<ThreadSummary> = Vec::new();
        let mut start = 0;
        while start < rows.len() {
            let id = rows[start].thread_id;
            let end = rows[start..]
                .iter()
                .position(|m| m.thread_id != id)
                .map_or(rows.len(), |len| start + len);
            let messages = dedupe(rows[start..end].to_vec(), folder);
            threads.push(summarize(id, messages));
            start = end;
        }
        threads.sort_by(|a, b| {
            b.latest
                .date
                .cmp(&a.latest.date)
                .then(b.latest.id.cmp(&a.latest.id))
        });
        Ok(threads)
    }

    /// Every message of a folder as its own row, for the view without
    /// conversations. Each one gets the id `single_thread(message)`, so the
    /// rest of the app treats it like a conversation of one message.
    pub fn single_messages(&self, folder: FolderId) -> StoreResult<Vec<ThreadSummary>> {
        Ok(self
            .messages(folder)?
            .into_iter()
            .map(|mut message| {
                let thread = single_thread(message.id);
                message.thread_id = thread;
                summarize(thread, vec![message])
            })
            .collect())
    }

    /// The messages of a conversation as seen from `folder`, oldest first.
    /// For a single message (`single_thread`) just that message.
    pub fn conversation(
        &self,
        thread: ThreadId,
        folder: FolderId,
    ) -> StoreResult<Vec<MessageSummary>> {
        if let Some(id) = single_message(thread) {
            return Ok(self
                .message(id)?
                .filter(|message| message.folder_id == folder)
                .map(|mut message| {
                    message.thread_id = thread;
                    message
                })
                .into_iter()
                .collect());
        }
        let separate = self.folder(folder)?.is_some_and(|f| f.is_separate());
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLUMNS} FROM messages m
             WHERE {SHOWN} AND m.thread_id = ?3 AND {SCOPE}
             ORDER BY m.date, m.id"
        ))?;
        let rows = stmt
            .query_map(params![folder, separate, thread], summary_from_row)?
            .collect::<StoreResult<Vec<_>>>()?;
        Ok(dedupe(rows, folder))
    }

    /// The messages of a conversation that are stored in `folder` itself.
    pub fn thread_messages_in(
        &self,
        thread: ThreadId,
        folder: FolderId,
    ) -> StoreResult<Vec<MessageId>> {
        if let Some(id) = single_message(thread) {
            return Ok(self
                .message(id)?
                .filter(|message| message.folder_id == folder)
                .map(|message| message.id)
                .into_iter()
                .collect());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.id FROM messages m WHERE m.thread_id = ?1 AND {SHOWN_IN} = ?2"
        ))?;
        stmt.query_map(params![thread, folder], |row| row.get(0))?
            .collect()
    }

    /// Marks messages as read or unread. Returns how many changed.
    pub fn set_seen(&mut self, ids: &[MessageId], seen: bool) -> StoreResult<usize> {
        let kind = if seen {
            ChangeKind::Seen
        } else {
            ChangeKind::Unseen
        };
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        for id in ids {
            let row: Option<(FolderId, bool, bool)> = tx
                .query_row(
                    "SELECT folder_id, seen, hidden FROM messages WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((folder, old, hidden)) = row else {
                continue;
            };
            if old == seen {
                continue;
            }
            tx.execute(
                "UPDATE messages SET seen = ?2 WHERE id = ?1",
                params![id, seen],
            )?;
            if !hidden {
                Store::adjust_unread(&tx, folder, if seen { -1 } else { 1 })?;
            }
            queue_change(&tx, *id, kind, None, 0)?;
            changed += 1;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Stars or unstars messages. Returns how many changed.
    pub fn set_flagged(&mut self, ids: &[MessageId], flagged: bool) -> StoreResult<usize> {
        let kind = if flagged {
            ChangeKind::Flag
        } else {
            ChangeKind::Unflag
        };
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        for id in ids {
            let updated = tx.execute(
                "UPDATE messages SET flagged = ?2 WHERE id = ?1 AND flagged != ?2",
                params![id, flagged],
            )?;
            if updated > 0 {
                queue_change(&tx, *id, kind, None, 0)?;
                changed += 1;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Moves messages to another folder once `due` (a Unix time) has
    /// passed. Until then they are hidden and the move can be undone.
    pub fn move_messages(
        &mut self,
        ids: &[MessageId],
        to: FolderId,
        due: i64,
    ) -> StoreResult<Vec<ChangeId>> {
        let tx = self.conn.transaction()?;
        let mut changes = Vec::new();
        for id in ids {
            if let Some(change) = hide(&tx, *id, Some(to), ChangeKind::Move, due)? {
                changes.push(change);
            }
        }
        tx.commit()?;
        Ok(changes)
    }

    /// Moves messages to the trash, or removes them for good if they are
    /// already there or the account has no trash.
    pub fn delete_messages(&mut self, ids: &[MessageId], due: i64) -> StoreResult<Deletion> {
        let trash = self.folder_by_role(FolderRole::Trash)?.map(|f| f.id);
        let tx = self.conn.transaction()?;
        let mut deletion = Deletion::default();
        for id in ids {
            let folder: Option<FolderId> = tx
                .query_row(
                    &format!("SELECT {SHOWN_IN} FROM messages m WHERE m.id = ?1"),
                    [id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let Some(folder) = folder else {
                continue;
            };
            match trash {
                Some(trash) if trash != folder => {
                    if let Some(change) = hide(&tx, *id, Some(trash), ChangeKind::Move, due)? {
                        deletion.undo.push(change);
                    }
                }
                _ => {
                    if hide(&tx, *id, None, ChangeKind::Delete, 0)?.is_some() {
                        deletion.permanent += 1;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(deletion)
    }

    /// Undoes moves that have not reached the server yet. Returns how many
    /// messages came back.
    pub fn cancel_changes(&mut self, changes: &[ChangeId]) -> StoreResult<usize> {
        let tx = self.conn.transaction()?;
        let mut restored = 0;
        for change in changes {
            let message: Option<MessageId> = tx
                .query_row(
                    "SELECT message FROM changes WHERE id = ?1",
                    [change],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(message) = message else {
                continue;
            };
            tx.execute("DELETE FROM changes WHERE id = ?1", [change])?;
            restored += unhide(&tx, message)?;
        }
        tx.commit()?;
        Ok(restored)
    }

    /// Changes whose time has come, oldest first.
    pub fn due_changes(&self, now: i64) -> StoreResult<Vec<PendingChange>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.message, m.folder_id, f.path, m.uid, c.kind, c.target, t.path
             FROM changes c
             JOIN messages m ON m.id = c.message
             JOIN folders f ON f.id = m.folder_id
             LEFT JOIN folders t ON t.id = c.target
             WHERE c.due <= ?1
             ORDER BY c.id",
        )?;
        let rows = stmt.query_map([now], |row| {
            let kind: String = row.get(5)?;
            Ok((
                PendingChange {
                    id: row.get(0)?,
                    message: row.get(1)?,
                    folder: row.get(2)?,
                    folder_path: row.get(3)?,
                    uid: row.get(4)?,
                    kind: ChangeKind::Seen,
                    target: row.get(6)?,
                    target_path: row.get(7)?,
                },
                kind,
            ))
        })?;
        let mut changes = Vec::new();
        for row in rows {
            let (mut change, kind) = row?;
            if let Some(kind) = ChangeKind::parse(&kind) {
                change.kind = kind;
                changes.push(change);
            }
        }
        Ok(changes)
    }

    /// When the next waiting change is due, as a Unix time.
    pub fn next_due(&self) -> StoreResult<Option<i64>> {
        self.conn
            .query_row("SELECT min(due) FROM changes", [], |row| row.get(0))
    }

    /// Takes a change off the queue once the server has it. A message that
    /// was moved or deleted leaves its folder; it shows up in the target
    /// folder with the next sync of that folder.
    pub fn finish_change(&mut self, change: &PendingChange) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM changes WHERE id = ?1", [change.id])?;
        if change.kind.removes() {
            tx.execute("DELETE FROM messages WHERE id = ?1", [change.message])?;
        }
        tx.commit()
    }

    /// Drops a change the server refused. A message that was meant to move
    /// comes back into view.
    pub fn abandon_change(&mut self, change: &PendingChange) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM changes WHERE id = ?1", [change.id])?;
        if change.kind.removes() {
            unhide(&tx, change.message)?;
        }
        tx.commit()
    }

    /// Queues a draft to be saved on the server. A newer version replaces
    /// an older one still waiting.
    pub fn queue_draft(&mut self, message_id: &str, raw: &[u8]) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM outbox WHERE message_id = ?1 AND kind IN ('draft', 'discard')",
            [message_id],
        )?;
        tx.execute(
            "INSERT INTO outbox (kind, message_id, raw) VALUES ('draft', ?1, ?2)",
            params![message_id, raw],
        )?;
        tx.commit()
    }

    /// Queues a message for sending. Waiting drafts of it are dropped and
    /// saved versions leave the drafts list at once.
    pub fn queue_send(
        &mut self,
        message_id: &str,
        raw: &[u8],
        recipients: &[String],
    ) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM outbox WHERE message_id = ?1 AND kind IN ('draft', 'discard')",
            [message_id],
        )?;
        tx.execute(
            "INSERT INTO outbox (kind, message_id, raw, recipients) VALUES ('send', ?1, ?2, ?3)",
            params![message_id, raw, recipients.join("\n")],
        )?;
        hide_drafts(&tx, message_id)?;
        tx.commit()
    }

    /// Queues removing every saved version of a draft.
    pub fn queue_discard(&mut self, message_id: &str) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM outbox WHERE message_id = ?1 AND kind IN ('draft', 'discard')",
            [message_id],
        )?;
        tx.execute(
            "INSERT INTO outbox (kind, message_id) VALUES ('discard', ?1)",
            [message_id],
        )?;
        hide_drafts(&tx, message_id)?;
        tx.commit()
    }

    /// Items whose turn has come, oldest first.
    pub fn outbox(&self, now: i64) -> StoreResult<Vec<OutboxItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, message_id, raw, recipients, error FROM outbox
             WHERE retry_at <= ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([now], |row| {
            let kind: String = row.get(1)?;
            let recipients: String = row.get(4)?;
            Ok((
                kind,
                OutboxItem {
                    id: row.get(0)?,
                    kind: OutboxKind::Draft,
                    message_id: row.get(2)?,
                    raw: row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
                    recipients: recipients
                        .split('\n')
                        .filter(|r| !r.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    error: row.get(5)?,
                },
            ))
        })?;
        let mut items = Vec::new();
        for row in rows {
            let (kind, mut item) = row?;
            if let Some(kind) = OutboxKind::parse(&kind) {
                item.kind = kind;
                items.push(item);
            }
        }
        Ok(items)
    }

    /// The earliest time a waiting item may be tried again.
    pub fn next_outbox_retry(&self) -> StoreResult<Option<i64>> {
        self.conn
            .query_row("SELECT min(retry_at) FROM outbox", [], |row| row.get(0))
    }

    /// Number of messages waiting to be sent.
    pub fn unsent(&self) -> StoreResult<usize> {
        self.conn
            .query_row(
                "SELECT count(*) FROM outbox WHERE kind = 'send'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as usize)
    }

    pub fn finish_outbox(&mut self, id: i64) -> StoreResult<()> {
        self.conn
            .execute("DELETE FROM outbox WHERE id = ?1", [id])
            .map(|_| ())
    }

    /// Notes that a message went out; filing it in "Sent" is left.
    pub fn mark_sent(&mut self, id: i64) -> StoreResult<()> {
        self.conn
            .execute(
                "UPDATE outbox SET kind = 'sent', error = NULL, retry_at = 0 WHERE id = ?1",
                [id],
            )
            .map(|_| ())
    }

    /// Keeps an item for a later try.
    pub fn postpone_outbox(&mut self, id: i64, retry_at: i64, error: &str) -> StoreResult<()> {
        self.conn
            .execute(
                "UPDATE outbox SET retry_at = ?2, error = ?3 WHERE id = ?1",
                params![id, retry_at, error],
            )
            .map(|_| ())
    }

    /// Makes every waiting item due again, for example after the password
    /// was changed or the user pressed "Send".
    pub fn retry_outbox_now(&mut self) -> StoreResult<()> {
        self.conn
            .execute("UPDATE outbox SET retry_at = 0", [])
            .map(|_| ())
    }

    /// A message the server refused to send becomes a draft again.
    pub fn send_failed_to_draft(&mut self, id: i64) -> StoreResult<()> {
        let tx = self.conn.transaction()?;
        let message_id: Option<String> = tx
            .query_row("SELECT message_id FROM outbox WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        tx.execute(
            "UPDATE outbox SET kind = 'draft', recipients = '', retry_at = 0, error = NULL
             WHERE id = ?1",
            [id],
        )?;
        if let Some(message_id) = message_id {
            tx.execute(
                "UPDATE messages SET hidden = 0
                 WHERE message_id = ?1 AND NOT EXISTS
                     (SELECT 1 FROM changes WHERE changes.message = messages.id)
                   AND folder_id IN (SELECT id FROM folders WHERE role = 'drafts')",
                [message_id],
            )?;
        }
        tx.commit()
    }

    /// Addresses the account has received mail from, most recent first,
    /// for suggestions while typing recipients.
    pub fn contacts(&self, query: &str, limit: usize) -> StoreResult<Vec<(String, String)>> {
        let pattern = format!(
            "%{}%",
            query
                .trim()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut stmt = self.conn.prepare(
            "SELECT from_name, from_address FROM messages
             WHERE from_address != '' AND (from_address LIKE ?1 ESCAPE '\\'
                                           OR from_name LIKE ?1 ESCAPE '\\')
             GROUP BY lower(from_address)
             ORDER BY max(date) DESC
             LIMIT ?2",
        )?;
        stmt.query_map(params![pattern, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect()
    }

    pub fn images_allowed_from(&self, domain: &str) -> StoreResult<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM image_senders WHERE domain = ?1",
                [domain.to_lowercase()],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
    }

    /// Messages that came into a folder before `before`, for emptying the
    /// trash and spam folders automatically. Counted from when the app first
    /// saw the message there, not from its date: an old mail deleted today
    /// stays for the full time.
    pub fn older_than(&self, folder: FolderId, before: i64) -> StoreResult<Vec<MessageId>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id FROM messages m JOIN arrivals a ON a.message = m.id
             WHERE m.folder_id = ?1 AND m.hidden = 0 AND a.at < ?2",
        )?;
        stmt.query_map(params![folder, before], |row| row.get(0))?
            .collect()
    }

    /// All messages shown in a folder, for emptying it by hand.
    pub fn visible_ids(&self, folder: FolderId) -> StoreResult<Vec<MessageId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM messages WHERE folder_id = ?1 AND hidden = 0")?;
        stmt.query_map([folder], |row| row.get(0))?.collect()
    }

    /// Removes messages for good, wherever they are. Returns how many.
    pub fn delete_for_good(&mut self, ids: &[MessageId]) -> StoreResult<usize> {
        let tx = self.conn.transaction()?;
        let mut removed = 0;
        for id in ids {
            if hide(&tx, *id, None, ChangeKind::Delete, 0)?.is_some() {
                removed += 1;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Remembers senders the user marked as spam; their new mail goes to
    /// the spam folder by itself.
    pub fn mark_spam_senders(&self, addresses: &[String]) -> StoreResult<()> {
        for address in addresses {
            self.conn.execute(
                "INSERT OR IGNORE INTO spam_senders (address) VALUES (?1)",
                [address.trim().to_lowercase()],
            )?;
        }
        Ok(())
    }

    /// "Not spam": the senders are trusted again.
    pub fn unmark_spam_senders(&self, addresses: &[String]) -> StoreResult<()> {
        for address in addresses {
            self.conn.execute(
                "DELETE FROM spam_senders WHERE address = ?1",
                [address.trim().to_lowercase()],
            )?;
        }
        Ok(())
    }

    /// The messages among `ids` from senders marked as spam.
    pub fn from_spam_senders(&self, ids: &[MessageId]) -> StoreResult<Vec<MessageId>> {
        let mut stmt = self.conn.prepare(
            "SELECT 1 FROM messages m JOIN spam_senders s ON s.address = lower(m.from_address)
             WHERE m.id = ?1",
        )?;
        let mut found = Vec::new();
        for id in ids {
            if stmt.exists([id])? {
                found.push(*id);
            }
        }
        Ok(found)
    }

    pub fn allow_images_from(&self, domain: &str) -> StoreResult<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO image_senders (domain) VALUES (?1)",
                [domain.to_lowercase()],
            )
            .map(|_| ())
    }
}

/// The part of a query that limits a conversation to what is shown from
/// folder `?1`; `?2` is true for trash, spam and drafts.
const SCOPE: &str = concat!(
    "(",
    shown_in!("m"),
    " = ?1 OR (?2 = 0 AND ",
    shown_in!("m"),
    " NOT IN (SELECT id FROM folders WHERE role IN ('trash', 'junk', 'drafts'))))"
);

const SHOWN_IN: &str = shown_in!("m");
const SHOWN_IN_N: &str = shown_in!("n");

/// Messages that show somewhere (see `shown_in`).
const SHOWN: &str = concat!(shown_in!("m"), " IS NOT NULL");

/// Puts a new message into the conversation of the messages it refers to,
/// that refer to it, or (for replies without references) that share its
/// subject. If it links several conversations, they become one.
fn assign_thread(
    tx: &Transaction<'_>,
    id: MessageId,
    header: &HeaderInfo,
    parents: &[String],
    date: i64,
) -> StoreResult<()> {
    let mut found: Vec<ThreadId> = Vec::new();
    let add = |found: &mut Vec<ThreadId>, thread: ThreadId| {
        if thread != 0 && !found.contains(&thread) {
            found.push(thread);
        }
    };
    {
        let mut by_id = tx.prepare_cached(
            "SELECT DISTINCT thread_id FROM messages WHERE message_id = ?1 AND id != ?2",
        )?;
        let own = header.message_id.as_deref();
        for message_id in parents.iter().map(String::as_str).chain(own) {
            for thread in by_id.query_map(params![message_id, id], |row| row.get(0))? {
                add(&mut found, thread?);
            }
        }
        if let Some(own) = own {
            let mut by_ref = tx.prepare_cached(
                "SELECT DISTINCT m.thread_id FROM message_refs r JOIN messages m ON m.id = r.message
                 WHERE r.ref = ?1 AND m.id != ?2",
            )?;
            for thread in by_ref.query_map(params![own, id], |row| row.get(0))? {
                add(&mut found, thread?);
            }
        }
    }
    let base = thread::base_subject(&header.subject);
    if found.is_empty() && !base.is_empty() {
        if parents.is_empty() && thread::is_reply(&header.subject) {
            let thread: Option<ThreadId> = tx
                .query_row(
                    "SELECT thread_id FROM messages
                     WHERE base_subject = ?1 AND id != ?2 AND date BETWEEN ?3 AND ?4
                     ORDER BY date DESC LIMIT 1",
                    params![base, id, date - SUBJECT_MATCH_WINDOW, date + 86_400],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(thread) = thread {
                add(&mut found, thread);
            }
        } else if !thread::is_reply(&header.subject) {
            let mut stmt = tx.prepare_cached(
                "SELECT DISTINCT thread_id FROM messages
                 WHERE base_subject = ?1 AND id != ?2 AND is_reply = 1 AND has_refs = 0
                   AND date BETWEEN ?3 AND ?4",
            )?;
            for thread in stmt.query_map(
                params![base, id, date - 86_400, date + SUBJECT_MATCH_WINDOW],
                |row| row.get(0),
            )? {
                add(&mut found, thread?);
            }
        }
    }
    let target = found.iter().copied().min().unwrap_or(id);
    tx.execute(
        "UPDATE messages SET thread_id = ?2 WHERE id = ?1",
        params![id, target],
    )?;
    for other in found.iter().filter(|thread| **thread != target) {
        tx.execute(
            "UPDATE messages SET thread_id = ?2 WHERE thread_id = ?1",
            params![other, target],
        )?;
    }
    Ok(())
}

/// Replaces an earlier change of the same sort, so only the latest wish is
/// sent to the server.
fn queue_change(
    tx: &Transaction<'_>,
    message: MessageId,
    kind: ChangeKind,
    target: Option<FolderId>,
    due: i64,
) -> StoreResult<ChangeId> {
    let related: &[ChangeKind] = match kind {
        ChangeKind::Seen | ChangeKind::Unseen => &[ChangeKind::Seen, ChangeKind::Unseen],
        ChangeKind::Flag | ChangeKind::Unflag => &[ChangeKind::Flag, ChangeKind::Unflag],
        ChangeKind::Move | ChangeKind::Delete => &[],
    };
    for old in related {
        tx.execute(
            "DELETE FROM changes WHERE message = ?1 AND kind = ?2",
            params![message, old.as_str()],
        )?;
    }
    tx.execute(
        "INSERT INTO changes (message, kind, target, due) VALUES (?1, ?2, ?3, ?4)",
        params![message, kind.as_str(), target, due],
    )?;
    Ok(tx.last_insert_rowid())
}

fn hide(
    tx: &Transaction<'_>,
    message: MessageId,
    target: Option<FolderId>,
    kind: ChangeKind,
    due: i64,
) -> StoreResult<Option<ChangeId>> {
    let row: Option<(FolderId, bool, bool)> = tx
        .query_row(
            "SELECT folder_id, seen, hidden FROM messages WHERE id = ?1",
            [message],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((folder, seen, hidden)) = row else {
        return Ok(None);
    };
    if hidden {
        // A move still waiting for the server: change where it goes.
        let pending: Option<(ChangeId, Option<FolderId>)> = tx
            .query_row(
                "SELECT id, target FROM changes WHERE message = ?1 AND kind = 'move'",
                [message],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((change, current)) = pending else {
            return Ok(None);
        };
        if target == current {
            return Ok(None);
        }
        if target == Some(folder) {
            tx.execute("DELETE FROM changes WHERE id = ?1", [change])?;
            unhide(tx, message)?;
            return Ok(None);
        }
        tx.execute(
            "UPDATE changes SET kind = ?2, target = ?3, due = ?4 WHERE id = ?1",
            params![change, kind.as_str(), target, due],
        )?;
        return Ok(Some(change));
    }
    if target == Some(folder) {
        return Ok(None);
    }
    tx.execute("UPDATE messages SET hidden = 1 WHERE id = ?1", [message])?;
    if !seen {
        Store::adjust_unread(tx, folder, -1)?;
    }
    queue_change(tx, message, kind, target, due).map(Some)
}

fn unhide(tx: &Transaction<'_>, message: MessageId) -> StoreResult<usize> {
    let row: Option<(FolderId, bool)> = tx
        .query_row(
            "SELECT folder_id, seen FROM messages WHERE id = ?1 AND hidden = 1",
            [message],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((folder, seen)) = row else {
        return Ok(0);
    };
    tx.execute("UPDATE messages SET hidden = 0 WHERE id = ?1", [message])?;
    if !seen {
        Store::adjust_unread(tx, folder, 1)?;
    }
    Ok(1)
}

/// The same message can sit in two folders (for example a copy in the
/// archive). It is shown once, preferably the copy in `folder`.
fn dedupe(messages: Vec<MessageSummary>, folder: FolderId) -> Vec<MessageSummary> {
    let mut out: Vec<MessageSummary> = Vec::with_capacity(messages.len());
    let mut seen: HashMap<String, usize> = HashMap::new();
    for message in messages {
        if let Some(id) = message.message_id.clone() {
            if let Some(&index) = seen.get(&id) {
                if message.folder_id == folder && out[index].folder_id != folder {
                    out[index] = message;
                }
                continue;
            }
            seen.insert(id, out.len());
        }
        out.push(message);
    }
    out
}

fn summarize(id: ThreadId, messages: Vec<MessageSummary>) -> ThreadSummary {
    let mut senders: Vec<Sender> = Vec::new();
    for message in &messages {
        let address = message.from_address.to_lowercase();
        if !senders.iter().any(|sender| sender.address == address) {
            senders.push(Sender {
                name: message.from_name.clone(),
                address,
            });
        }
    }
    let latest = messages
        .iter()
        .max_by(|a, b| a.date.cmp(&b.date).then(a.id.cmp(&b.id)))
        .cloned()
        .expect("a conversation has at least one message");
    ThreadSummary {
        id,
        subject: messages[0].subject.clone(),
        senders,
        count: messages.len(),
        unread: messages.iter().any(|m| !m.flags.seen),
        flagged: messages.iter().any(|m| m.flags.flagged),
        has_attachment: messages.iter().any(|m| m.has_attachment),
        latest,
    }
}

/// The id under which a single message is shown when messages are not
/// grouped into conversations. Real conversation ids are positive.
pub fn single_thread(message: MessageId) -> ThreadId {
    -message
}

/// The message behind an id made by `single_thread`.
pub fn single_message(thread: ThreadId) -> Option<MessageId> {
    (thread < 0).then_some(-thread)
}

/// Takes the saved versions of a draft out of the drafts list.
fn hide_drafts(tx: &Transaction<'_>, message_id: &str) -> StoreResult<()> {
    tx.execute(
        "UPDATE messages SET hidden = 1
         WHERE message_id = ?1
           AND folder_id IN (SELECT id FROM folders WHERE role = 'drafts')",
        [message_id],
    )?;
    Ok(())
}

fn summary_from_row(row: &rusqlite::Row<'_>) -> StoreResult<MessageSummary> {
    Ok(MessageSummary {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        uid: row.get(2)?,
        thread_id: row.get(3)?,
        message_id: row.get(4)?,
        subject: row.get(5)?,
        from_name: row.get(6)?,
        from_address: row.get(7)?,
        date: row.get(8)?,
        flags: Flags {
            seen: row.get(9)?,
            flagged: row.get(10)?,
        },
        preview: row.get(11)?,
        has_attachment: row.get(12)?,
        recipients: row.get(13)?,
    })
}

fn display_name(folder: &RemoteFolder) -> String {
    let leaf = match folder.delimiter.as_deref() {
        Some(d) if !d.is_empty() => folder.path.rsplit(d).next().unwrap_or(&folder.path),
        _ => &folder.path,
    };
    utf7::decode(leaf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(path: &str, role: Option<FolderRole>) -> RemoteFolder {
        RemoteFolder {
            path: path.into(),
            delimiter: Some("/".into()),
            role,
            selectable: true,
        }
    }

    fn header(subject: &str, date: i64) -> HeaderInfo {
        HeaderInfo {
            subject: subject.into(),
            from_name: "Anna".into(),
            from_address: "anna@example.org".into(),
            to: "me@example.org".into(),
            date: Some(date),
            ..HeaderInfo::default()
        }
    }

    fn reply(subject: &str, date: i64, id: &str, parents: &[&str]) -> HeaderInfo {
        HeaderInfo {
            message_id: Some(id.into()),
            references: parents.iter().map(|p| p.to_string()).collect(),
            ..header(subject, date)
        }
    }

    /// A store with inbox, sent, trash and a custom folder, in this order.
    fn store() -> (Store, [FolderId; 4]) {
        let mut store = Store::open_in_memory(1).unwrap();
        store
            .sync_folders(&[
                remote("INBOX", Some(FolderRole::Inbox)),
                remote("Sent", Some(FolderRole::Sent)),
                remote("Trash", Some(FolderRole::Trash)),
                remote("Projects", None),
            ])
            .unwrap();
        let ids: Vec<FolderId> = store.folders().unwrap().iter().map(|f| f.id).collect();
        (store, [ids[0], ids[1], ids[2], ids[3]])
    }

    fn unseen() -> Flags {
        Flags::default()
    }

    #[test]
    fn accounts_have_separate_files() {
        let dir = std::env::temp_dir().join(format!("e-mail-stores-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut first = Store::open(&dir.join("account-1.db"), 1).unwrap();
        let second = Store::open(&dir.join("account-2.db"), 2).unwrap();
        first
            .sync_folders(&[remote("INBOX", Some(FolderRole::Inbox))])
            .unwrap();
        let inbox = first.folders().unwrap()[0].id;
        first.set_unread(inbox, 4).unwrap();

        assert_eq!(first.folders().unwrap()[0].account_id, 1);
        assert_eq!(first.inbox_unread().unwrap(), 4);
        assert!(second.folders().unwrap().is_empty());
        assert_eq!(second.inbox_unread().unwrap(), 0);
        drop((first, second));

        let reopened = Store::open(&dir.join("account-1.db"), 1).unwrap();
        assert_eq!(reopened.inbox_unread().unwrap(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_databases_are_rebuilt() {
        let dir = std::env::temp_dir().join(format!("e-mail-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("account-1.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE folders (id INTEGER PRIMARY KEY, path TEXT); PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let mut store = Store::open(&path, 1).unwrap();
        store
            .sync_folders(&[remote("INBOX", Some(FolderRole::Inbox))])
            .unwrap();
        assert_eq!(store.folders().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folders_are_sorted_as_a_tree() {
        let mut store = Store::open_in_memory(1).unwrap();
        store
            .sync_folders(&[
                remote("Zeta", None),
                remote("Trash", Some(FolderRole::Trash)),
                remote("INBOX", Some(FolderRole::Inbox)),
                remote("Projects/Entw&APw-rfe", None),
                remote("Projects-Old", None),
                remote("Projects", None),
                remote("Club/Board", None),
                RemoteFolder {
                    selectable: false,
                    ..remote("Hidden", None)
                },
            ])
            .unwrap();
        let folders: Vec<(String, u32)> = store
            .folders()
            .unwrap()
            .into_iter()
            .map(|f| (f.name, f.depth))
            .collect();
        assert_eq!(
            folders,
            [
                ("INBOX".to_owned(), 0),
                ("Trash".to_owned(), 0),
                ("Board".to_owned(), 0),
                ("Projects".to_owned(), 0),
                ("Entwürfe".to_owned(), 1),
                ("Projects-Old".to_owned(), 0),
                ("Zeta".to_owned(), 0),
            ]
        );

        store
            .sync_folders(&[remote("INBOX", Some(FolderRole::Inbox))])
            .unwrap();
        assert_eq!(store.folders().unwrap().len(), 1);
    }

    #[test]
    fn messages_are_listed_newest_first() {
        let (mut store, [inbox, ..]) = store();
        store
            .insert_message(inbox, 1, &header("Old", 100), 0, unseen())
            .unwrap();
        store
            .insert_message(inbox, 2, &header("New", 200), 0, unseen())
            .unwrap();
        let subjects: Vec<_> = store
            .messages(inbox)
            .unwrap()
            .into_iter()
            .map(|m| m.subject)
            .collect();
        assert_eq!(subjects, ["New", "Old"]);
        assert_eq!(store.uids(inbox).unwrap(), [1, 2]);

        store.remove_messages(inbox, &[1]).unwrap();
        assert_eq!(store.uids(inbox).unwrap(), [2]);
    }

    #[test]
    fn missing_date_uses_fallback() {
        let (mut store, [inbox, ..]) = store();
        let mut no_date = header("Undated", 0);
        no_date.date = None;
        let id = store
            .insert_message(inbox, 5, &no_date, 4242, unseen())
            .unwrap();
        assert_eq!(store.message(id).unwrap().unwrap().date, 4242);
    }

    #[test]
    fn changed_uid_validity_clears_folder() {
        let (mut store, [inbox, ..]) = store();
        assert!(!store.set_uid_validity(inbox, Some(1)).unwrap());
        store
            .insert_message(inbox, 1, &header("A", 1), 0, unseen())
            .unwrap();
        store
            .set_sync_state(
                inbox,
                FolderSyncState {
                    highest_modseq: Some(9),
                    exists: Some(1),
                    synced_uid: Some(1),
                },
            )
            .unwrap();
        assert!(!store.set_uid_validity(inbox, Some(1)).unwrap());
        assert_eq!(store.uids(inbox).unwrap().len(), 1);
        assert!(store.set_uid_validity(inbox, Some(2)).unwrap());
        assert!(store.uids(inbox).unwrap().is_empty());
        assert_eq!(store.sync_state(inbox).unwrap(), FolderSyncState::default());
    }

    #[test]
    fn full_text_search_finds_subject_and_body() {
        let (mut store, [inbox, ..]) = store();
        let invoice = store
            .insert_message(inbox, 1, &header("Rechnung November", 1), 0, unseen())
            .unwrap();
        let trip = store
            .insert_message(inbox, 2, &header("Reise", 2), 0, unseen())
            .unwrap();
        store
            .store_body(
                trip,
                b"raw",
                "Zug nach Berlin",
                "Der Zug nach Berlin fährt um 8",
                true,
            )
            .unwrap();

        let ids = |q: &str| -> Vec<MessageId> {
            store.search(q).unwrap().into_iter().map(|m| m.id).collect()
        };
        assert_eq!(ids("rechn"), [invoice]);
        assert_eq!(ids("Berlin"), [trip]);
        assert_eq!(ids("anna"), [trip, invoice]);
        assert!(ids("\"unbalanced").is_empty());

        assert_eq!(store.body(trip).unwrap().as_deref(), Some(&b"raw"[..]));
        let message = store.message(trip).unwrap().unwrap();
        assert_eq!(message.preview, "Zug nach Berlin");
        assert!(message.has_attachment);

        store.remove_messages(inbox, &[2]).unwrap();
        assert!(store.search("Berlin").unwrap().is_empty());
    }

    #[test]
    fn server_flags_are_taken_unless_a_change_waits() {
        let (mut store, [inbox, ..]) = store();
        let id = store
            .insert_message(inbox, 1, &header("A", 1), 0, unseen())
            .unwrap();
        let both = Flags {
            seen: true,
            flagged: true,
        };
        assert!(store.set_flags(inbox, 1, both).unwrap());
        assert_eq!(store.message(id).unwrap().unwrap().flags, both);

        store.set_flagged(&[id], false).unwrap();
        assert!(!store.set_flags(inbox, 1, both).unwrap());
        assert!(!store.message(id).unwrap().unwrap().flags.flagged);

        let pending = store.due_changes(0).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, ChangeKind::Unflag);
        store.finish_change(&pending[0]).unwrap();
        assert!(store.set_flags(inbox, 1, both).unwrap());
    }

    #[test]
    fn reading_adjusts_the_unread_count_and_keeps_the_latest_wish() {
        let (mut store, [inbox, ..]) = store();
        let a = store
            .insert_message(inbox, 1, &header("A", 1), 0, unseen())
            .unwrap();
        let b = store
            .insert_message(inbox, 2, &header("B", 2), 0, unseen())
            .unwrap();
        store.set_unread(inbox, 2).unwrap();

        assert_eq!(store.set_seen(&[a, b], true).unwrap(), 2);
        assert_eq!(store.folder(inbox).unwrap().unwrap().unread, 0);
        assert_eq!(store.set_seen(&[a], true).unwrap(), 0);
        store.set_seen(&[a], false).unwrap();
        assert_eq!(store.folder(inbox).unwrap().unwrap().unread, 1);

        let kinds: Vec<(MessageId, ChangeKind)> = store
            .due_changes(0)
            .unwrap()
            .into_iter()
            .map(|c| (c.message, c.kind))
            .collect();
        assert_eq!(kinds, [(b, ChangeKind::Seen), (a, ChangeKind::Unseen)]);
    }

    #[test]
    fn deleting_moves_to_trash_and_can_be_undone() {
        let (mut store, [inbox, _, trash, _]) = store();
        let id = store
            .insert_message(inbox, 1, &header("Spam offer", 1), 0, unseen())
            .unwrap();
        store.set_unread(inbox, 1).unwrap();

        let deletion = store.delete_messages(&[id], 100).unwrap();
        assert_eq!(deletion.undo.len(), 1);
        assert_eq!(deletion.permanent, 0);
        assert!(store.messages(inbox).unwrap().is_empty());
        assert!(store.threads(inbox).unwrap().is_empty());
        assert_eq!(store.folder(inbox).unwrap().unwrap().unread, 0);
        assert!(store.due_changes(99).unwrap().is_empty());
        assert_eq!(store.next_due().unwrap(), Some(100));

        assert_eq!(store.cancel_changes(&deletion.undo).unwrap(), 1);
        assert_eq!(store.messages(inbox).unwrap().len(), 1);
        assert_eq!(store.folder(inbox).unwrap().unwrap().unread, 1);
        assert!(store.due_changes(1000).unwrap().is_empty());

        let deletion = store.delete_messages(&[id], 100).unwrap();
        let due = store.due_changes(100).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].kind, ChangeKind::Move);
        assert_eq!(due[0].target, Some(trash));
        assert_eq!(due[0].target_path.as_deref(), Some("Trash"));
        store.finish_change(&due[0]).unwrap();
        assert!(store.message(id).unwrap().is_none());
        assert_eq!(store.cancel_changes(&deletion.undo).unwrap(), 0);
    }

    #[test]
    fn deleting_in_the_trash_is_final() {
        let (mut store, [_, _, trash, _]) = store();
        let id = store
            .insert_message(trash, 1, &header("Old", 1), 0, unseen())
            .unwrap();
        let deletion = store.delete_messages(&[id], 100).unwrap();
        assert!(deletion.undo.is_empty());
        assert_eq!(deletion.permanent, 1);
        let due = store.due_changes(0).unwrap();
        assert_eq!(due[0].kind, ChangeKind::Delete);
    }

    #[test]
    fn refused_moves_bring_the_message_back() {
        let (mut store, [inbox, _, _, projects]) = store();
        let id = store
            .insert_message(inbox, 1, &header("A", 1), 0, unseen())
            .unwrap();
        store.move_messages(&[id], projects, 0).unwrap();
        assert!(store.messages(inbox).unwrap().is_empty());
        let due = store.due_changes(0).unwrap();
        store.abandon_change(&due[0]).unwrap();
        assert_eq!(store.messages(inbox).unwrap().len(), 1);
    }

    #[test]
    fn replies_join_their_conversation_across_folders() {
        let (mut store, [inbox, sent, ..]) = store();
        let question = store
            .insert_message(
                inbox,
                1,
                &reply("Bike for sale?", 100, "q@x", &[]),
                0,
                unseen(),
            )
            .unwrap();
        let mut answer = reply("Re: Bike for sale?", 200, "a@x", &["q@x"]);
        answer.from_address = "me@example.org".into();
        answer.from_name = "Me".into();
        store
            .insert_message(
                sent,
                7,
                &answer,
                0,
                Flags {
                    seen: true,
                    flagged: false,
                },
            )
            .unwrap();
        store
            .insert_message(inbox, 2, &header("Other", 150), 0, unseen())
            .unwrap();

        let threads = store.threads(inbox).unwrap();
        assert_eq!(threads.len(), 2);
        let bike = &threads[0];
        assert_eq!(bike.count, 2);
        assert_eq!(bike.subject, "Bike for sale?");
        assert_eq!(bike.latest.subject, "Re: Bike for sale?");
        assert!(bike.unread);
        assert_eq!(bike.senders.len(), 2);

        let conversation = store.conversation(bike.id, inbox).unwrap();
        let subjects: Vec<_> = conversation.iter().map(|m| m.subject.as_str()).collect();
        assert_eq!(subjects, ["Bike for sale?", "Re: Bike for sale?"]);
        assert_eq!(
            store.thread_messages_in(bike.id, inbox).unwrap(),
            [question]
        );
        assert_eq!(store.threads(sent).unwrap()[0].count, 2);
    }

    #[test]
    fn older_messages_found_later_join_and_merge() {
        let (mut store, [inbox, ..]) = store();
        // The newest message arrives first, as during the background sync.
        store
            .insert_message(
                inbox,
                3,
                &reply("Re: Plan", 300, "c@x", &["a@x", "b@x"]),
                0,
                unseen(),
            )
            .unwrap();
        store
            .insert_message(inbox, 9, &reply("Unrelated", 250, "z@x", &[]), 0, unseen())
            .unwrap();
        store
            .insert_message(inbox, 1, &reply("Plan", 100, "a@x", &[]), 0, unseen())
            .unwrap();
        store
            .insert_message(
                inbox,
                2,
                &reply("Re: Plan", 200, "b@x", &["a@x"]),
                0,
                unseen(),
            )
            .unwrap();
        let threads = store.threads(inbox).unwrap();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].count, 3);
        assert_eq!(threads[0].subject, "Plan");
    }

    #[test]
    fn conversations_split_by_references_merge_when_linked() {
        let (mut store, [inbox, ..]) = store();
        store
            .insert_message(inbox, 1, &reply("Left", 100, "l@x", &[]), 0, unseen())
            .unwrap();
        store
            .insert_message(inbox, 2, &reply("Right", 110, "r@x", &[]), 0, unseen())
            .unwrap();
        assert_eq!(store.threads(inbox).unwrap().len(), 2);
        store
            .insert_message(
                inbox,
                3,
                &reply("Re: Both", 120, "m@x", &["l@x", "r@x"]),
                0,
                unseen(),
            )
            .unwrap();
        assert_eq!(store.threads(inbox).unwrap().len(), 1);
    }

    #[test]
    fn replies_without_references_match_by_subject() {
        let (mut store, [inbox, sent, ..]) = store();
        store
            .insert_message(
                inbox,
                1,
                &header("Fahrrad noch zu haben?", 100),
                0,
                unseen(),
            )
            .unwrap();
        store
            .insert_message(
                sent,
                1,
                &header("AW: Fahrrad noch zu haben?", 200),
                0,
                unseen(),
            )
            .unwrap();
        // Same subject, but not a reply: a new conversation.
        store
            .insert_message(inbox, 2, &header("Monthly report", 100), 0, unseen())
            .unwrap();
        store
            .insert_message(inbox, 3, &header("Monthly report", 2_000_000), 0, unseen())
            .unwrap();
        let counts: Vec<usize> = store
            .threads(inbox)
            .unwrap()
            .into_iter()
            .map(|t| t.count)
            .collect();
        assert_eq!(counts, [1, 2, 1]);

        // A reply found before the message it answers.
        store
            .insert_message(inbox, 5, &header("Re: Lunch", 500), 0, unseen())
            .unwrap();
        store
            .insert_message(inbox, 4, &header("Lunch", 400), 0, unseen())
            .unwrap();
        let lunch = store
            .threads(inbox)
            .unwrap()
            .into_iter()
            .find(|t| t.subject == "Lunch")
            .unwrap();
        assert_eq!(lunch.count, 2);
    }

    #[test]
    fn trash_keeps_its_messages_apart() {
        let (mut store, [inbox, _, trash, _]) = store();
        let first = store
            .insert_message(inbox, 1, &reply("Plan", 100, "a@x", &[]), 0, unseen())
            .unwrap();
        store
            .insert_message(
                inbox,
                2,
                &reply("Re: Plan", 200, "b@x", &["a@x"]),
                0,
                unseen(),
            )
            .unwrap();
        store
            .insert_message(
                trash,
                1,
                &reply("Re: Plan", 300, "c@x", &["a@x"]),
                0,
                unseen(),
            )
            .unwrap();
        let thread = store.threads(inbox).unwrap()[0].id;
        assert_eq!(store.conversation(thread, inbox).unwrap().len(), 2);
        assert_eq!(store.conversation(thread, trash).unwrap().len(), 1);

        // A copy of the same message in another folder is shown once.
        store
            .insert_message(trash, 2, &reply("Plan", 100, "a@x", &[]), 0, unseen())
            .unwrap();
        let conversation = store.conversation(thread, trash).unwrap();
        assert_eq!(conversation.len(), 2);
        let inbox_view = store.conversation(thread, inbox).unwrap();
        assert_eq!(inbox_view[0].id, first);
    }

    #[test]
    fn moved_mail_shows_in_the_target_right_away() {
        let (mut store, [inbox, _, trash, projects]) = store();
        let id = store
            .insert_message(inbox, 1, &header("Plan", 100), 0, unseen())
            .unwrap();
        let changes = store.move_messages(&[id], projects, i64::MAX).unwrap();
        assert!(store.threads(inbox).unwrap().is_empty());
        let moved = store.threads(projects).unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].latest.folder_id, projects);
        assert_eq!(store.conversation(moved[0].id, projects).unwrap().len(), 1);
        assert_eq!(store.messages(projects).unwrap().len(), 1);

        // Undo puts it back.
        store.cancel_changes(&changes).unwrap();
        assert_eq!(store.threads(inbox).unwrap().len(), 1);
        assert!(store.threads(projects).unwrap().is_empty());

        // Deleting shows it in the trash at once.
        store.delete_messages(&[id], i64::MAX).unwrap();
        assert_eq!(store.threads(trash).unwrap().len(), 1);

        // A message still on its way can be moved on or deleted from there.
        let thread = store.threads(trash).unwrap()[0].id;
        assert_eq!(store.thread_messages_in(thread, trash).unwrap(), [id]);
        store.move_messages(&[id], projects, i64::MAX).unwrap();
        assert!(store.threads(trash).unwrap().is_empty());
        assert_eq!(store.threads(projects).unwrap().len(), 1);
        let pending = store.due_changes(i64::MAX).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].target, Some(projects));
        // Moving it back home simply cancels the move.
        store.move_messages(&[id], inbox, i64::MAX).unwrap();
        assert_eq!(store.threads(inbox).unwrap().len(), 1);
        assert!(store.due_changes(i64::MAX).unwrap().is_empty());
    }

    #[test]
    fn old_mail_can_be_removed_for_good() {
        let (mut store, [inbox, _, trash, _]) = store();
        let old = store
            .insert_message(trash, 1, &header("Old", 100), 0, unseen())
            .unwrap();
        store
            .insert_message(trash, 2, &header("New", 900), 0, unseen())
            .unwrap();
        store
            .insert_message(inbox, 1, &header("Inbox", 100), 0, unseen())
            .unwrap();
        store
            .conn
            .execute("UPDATE arrivals SET at = 100 WHERE message = ?1", [old])
            .unwrap();
        let now = chrono::Utc::now().timestamp();
        let ids = store.older_than(trash, now - 30 * 86_400).unwrap();
        assert_eq!(ids, [old]);
        assert_eq!(store.delete_for_good(&ids).unwrap(), 1);
        let due = store.due_changes(0).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].kind, ChangeKind::Delete);
        let left: Vec<String> = store
            .messages(trash)
            .unwrap()
            .into_iter()
            .map(|m| m.subject)
            .collect();
        assert_eq!(left, ["New"]);
    }

    #[test]
    fn drafts_show_how_many_there_are() {
        let mut store = Store::open_in_memory(1).unwrap();
        store
            .sync_folders(&[
                remote("INBOX", Some(FolderRole::Inbox)),
                remote("Drafts", Some(FolderRole::Drafts)),
            ])
            .unwrap();
        let drafts = store
            .folder_by_role(FolderRole::Drafts)
            .unwrap()
            .unwrap()
            .id;
        let mut header = header("Plan", 100);
        header.message_id = Some("d1@x".into());
        store
            .insert_message(
                drafts,
                1,
                &header,
                0,
                Flags {
                    seen: true,
                    flagged: false,
                },
            )
            .unwrap();
        let count = |store: &Store| store.folder(drafts).unwrap().unwrap().unread;
        assert_eq!(count(&store), 1);
        // A newer version of the same draft does not count twice; a new
        // draft still waiting for the server does.
        store.queue_draft("d1@x", b"raw").unwrap();
        store.queue_draft("d2@x", b"raw").unwrap();
        assert_eq!(count(&store), 2);
        store.queue_discard("d1@x").unwrap();
        assert_eq!(count(&store), 1);
    }

    #[test]
    fn a_folder_list_without_inbox_is_ignored() {
        let (mut store, _) = store();
        store.sync_folders(&[]).unwrap();
        store.sync_folders(&[remote("Other", None)]).unwrap();
        assert_eq!(store.folders().unwrap().len(), 4);
    }

    #[test]
    fn without_conversations_every_message_stands_alone() {
        let (mut store, [inbox, sent, ..]) = store();
        let first = store
            .insert_message(inbox, 1, &reply("Plan", 100, "a@x", &[]), 0, unseen())
            .unwrap();
        store
            .insert_message(
                sent,
                1,
                &reply("Re: Plan", 200, "b@x", &["a@x"]),
                0,
                unseen(),
            )
            .unwrap();
        let second = store
            .insert_message(
                inbox,
                2,
                &reply("Re: Plan", 300, "c@x", &["a@x"]),
                0,
                unseen(),
            )
            .unwrap();
        // Grouped: one conversation with all three, including "Sent".
        let grouped = store.threads(inbox).unwrap();
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].count, 3);

        // Single: the two inbox messages, newest first, nothing from "Sent".
        let single = store.single_messages(inbox).unwrap();
        let ids: Vec<ThreadId> = single.iter().map(|t| t.id).collect();
        assert_eq!(ids, [single_thread(second), single_thread(first)]);
        assert!(single.iter().all(|t| t.count == 1));
        let shown = store.conversation(single[0].id, inbox).unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].id, second);
        assert_eq!(shown[0].thread_id, single[0].id);
        assert_eq!(
            store.thread_messages_in(single[0].id, inbox).unwrap(),
            [second]
        );
        assert!(store.conversation(single[0].id, sent).unwrap().is_empty());

        // Search finds single messages as well.
        let query = SearchQuery {
            text: "plan".into(),
            ..SearchQuery::default()
        };
        assert_eq!(store.search_threads(&query, &[], true).unwrap().len(), 1);
        let single_hits = store.search_threads(&query, &[], false).unwrap();
        assert_eq!(single_hits.len(), 3);
        assert!(single_hits.iter().all(|hit| hit.thread.id < 0));
    }

    #[test]
    fn image_permissions_are_kept_per_domain() {
        let (store, _) = store();
        assert!(!store.images_allowed_from("shop.example").unwrap());
        store.allow_images_from("Shop.example").unwrap();
        assert!(store.images_allowed_from("shop.EXAMPLE").unwrap());
        assert!(!store.images_allowed_from("posteo.de").unwrap());
    }

    #[test]
    fn version_three_keeps_its_mail_and_gets_an_outbox() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("{SCHEMA} PRAGMA user_version = 3;"))
            .unwrap();
        conn.execute(
            "INSERT INTO folders (path, name, role) VALUES ('INBOX', 'Inbox', 'inbox')",
            [],
        )
        .unwrap();
        let mut store = Store::init(conn, 1).unwrap();
        assert_eq!(store.folders().unwrap().len(), 1);
        store.queue_draft("a@x", b"raw").unwrap();
        assert_eq!(store.outbox(0).unwrap().len(), 1);
    }

    #[test]
    fn outbox_keeps_only_the_newest_draft() {
        let (mut store, _) = store();
        store.queue_draft("a@x", b"one").unwrap();
        store.queue_draft("a@x", b"two").unwrap();
        store.queue_draft("b@x", b"other").unwrap();
        let items = store.outbox(0).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].raw, b"two");
        assert_eq!(items[0].kind, OutboxKind::Draft);

        store
            .queue_send("a@x", b"final", &["l@x.org".into(), "t@x.org".into()])
            .unwrap();
        let items = store.outbox(0).unwrap();
        assert_eq!(items.len(), 2);
        let send = items.iter().find(|i| i.message_id == "a@x").unwrap();
        assert_eq!(send.kind, OutboxKind::Send);
        assert_eq!(send.recipients, ["l@x.org", "t@x.org"]);
        assert_eq!(store.unsent().unwrap(), 1);

        store.postpone_outbox(send.id, 500, "offline").unwrap();
        assert_eq!(store.outbox(499).unwrap().len(), 1);
        assert_eq!(store.outbox(500).unwrap().len(), 2);
        assert_eq!(store.next_outbox_retry().unwrap(), Some(0));
    }

    #[test]
    fn contacts_come_from_received_mail() {
        let (mut store, [inbox, ..]) = store();
        let mut header = header("Hi", 100);
        header.from_name = "Lena Vogt".into();
        header.from_address = "lena@example.org".into();
        store
            .insert_message(inbox, 1, &header, 0, unseen())
            .unwrap();
        header.from_address = "LENA@example.org".into();
        header.date = Some(200);
        store
            .insert_message(inbox, 2, &header, 0, unseen())
            .unwrap();
        header.from_name = "Tom_".into();
        header.from_address = "tom@example.org".into();
        store
            .insert_message(inbox, 3, &header, 0, unseen())
            .unwrap();

        let found = store.contacts("len", 5).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "Lena Vogt");
        assert_eq!(store.contacts("example", 5).unwrap().len(), 2);
        assert_eq!(store.contacts("_", 5).unwrap().len(), 1);
    }

    mod search {
        use super::*;
        use crate::search::{Filters, SearchQuery};

        fn query(text: &str, filters: Filters) -> SearchQuery {
            SearchQuery {
                text: text.into(),
                filters,
            }
        }

        fn found(store: &Store, query: &SearchQuery, extra: &[MessageId]) -> Vec<String> {
            store
                .search_threads(query, extra, true)
                .unwrap()
                .into_iter()
                .map(|r| r.thread.subject)
                .collect()
        }

        #[test]
        fn results_are_conversations_with_filters() {
            let (mut store, [inbox, sent, trash, projects]) = store();
            let mut plan = reply("Plan Berlin", 100, "a@x", &[]);
            plan.to = "Lena Vogt".into();
            store.insert_message(inbox, 1, &plan, 0, unseen()).unwrap();
            store
                .insert_message(
                    sent,
                    1,
                    &reply("Re: Plan Berlin", 200, "b@x", &["a@x"]),
                    0,
                    Flags {
                        seen: true,
                        flagged: true,
                    },
                )
                .unwrap();
            store
                .insert_message(
                    trash,
                    1,
                    &reply("Berlin trash", 300, "c@x", &[]),
                    0,
                    unseen(),
                )
                .unwrap();
            let read = Flags {
                seen: true,
                flagged: false,
            };
            let berlin_trip = store
                .insert_message(projects, 1, &reply("Trip", 50, "d@x", &[]), 0, read)
                .unwrap();
            store
                .store_body(berlin_trip, b"raw", "", "Wir fahren nach Berlin", true)
                .unwrap();

            // One conversation for inbox + sent, nothing from the trash.
            assert_eq!(
                found(&store, &query("berl", Filters::default()), &[]),
                ["Plan Berlin", "Trip"]
            );
            // Recipients are searched too.
            assert_eq!(
                found(&store, &query("vogt", Filters::default()), &[]),
                ["Plan Berlin"]
            );
            let only = |f: Filters| found(&store, &query("berlin", f), &[]);
            assert_eq!(
                only(Filters {
                    unread: true,
                    ..Filters::default()
                }),
                ["Plan Berlin"]
            );
            assert_eq!(
                only(Filters {
                    flagged: true,
                    ..Filters::default()
                }),
                ["Plan Berlin"]
            );
            assert_eq!(
                only(Filters {
                    attachment: true,
                    ..Filters::default()
                }),
                ["Trip"]
            );
            assert_eq!(
                only(Filters {
                    folder: Some(trash),
                    ..Filters::default()
                }),
                ["Berlin trash"]
            );
            // Filters alone list everything that matches them.
            assert_eq!(
                found(
                    &store,
                    &query(
                        "",
                        Filters {
                            attachment: true,
                            ..Filters::default()
                        }
                    ),
                    &[]
                ),
                ["Trip"]
            );
            assert!(found(&store, &query("\"", Filters::default()), &[]).is_empty());
        }

        #[test]
        fn server_hits_are_merged_without_duplicates() {
            let (mut store, [inbox, ..]) = store();
            let local = store
                .insert_message(
                    inbox,
                    1,
                    &reply("Invoice May", 100, "a@x", &[]),
                    0,
                    unseen(),
                )
                .unwrap();
            let remote = store
                .insert_message(inbox, 2, &reply("Statement", 200, "b@x", &[]), 0, unseen())
                .unwrap();
            let results = store
                .search_threads(
                    &query("invoice", Filters::default()),
                    &[local, remote, remote],
                    true,
                )
                .unwrap();
            let subjects: Vec<&str> = results.iter().map(|r| r.thread.subject.as_str()).collect();
            assert_eq!(subjects, ["Statement", "Invoice May"]);
            assert_eq!(results[0].folder, inbox);
        }

        #[test]
        fn complete_folders_are_not_searched_on_the_server() {
            let (mut store, [inbox, sent, ..]) = store();
            let id = store
                .insert_message(inbox, 1, &reply("A", 1, "a@x", &[]), 0, unseen())
                .unwrap();
            store.store_body(id, b"raw", "", "text", false).unwrap();
            for folder in [inbox, sent] {
                store
                    .set_sync_state(
                        folder,
                        FolderSyncState {
                            exists: Some(if folder == inbox { 1 } else { 3 }),
                            ..FolderSyncState::default()
                        },
                    )
                    .unwrap();
            }
            let remote: Vec<FolderId> = store
                .folders_to_search_remotely(None)
                .unwrap()
                .iter()
                .map(|f| f.id)
                .collect();
            assert!(!remote.contains(&inbox));
            assert!(remote.contains(&sent));
            assert_eq!(
                store.folders_to_search_remotely(Some(inbox)).unwrap().len(),
                0
            );
        }

        #[test]
        fn version_four_gets_a_new_index() {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!(
                "{SCHEMA} {OUTBOX_SCHEMA}
                 CREATE VIRTUAL TABLE messages_fts USING fts5 (subject, sender, body, content = '');
                 INSERT INTO folders (path, name, role) VALUES ('INBOX', 'Inbox', 'inbox');
                 INSERT INTO messages (folder_id, uid, subject, from_name, from_address,
                                       recipients, date, seen, flagged)
                 VALUES (1, 1, 'Hello', 'Anna', 'anna@x.org', 'Lena Vogt', 0, 0, 0);
                 PRAGMA user_version = 4;"
            ))
            .unwrap();
            let store = Store::init(conn, 1).unwrap();
            assert_eq!(store.search("vogt").unwrap().len(), 1);
            assert_eq!(store.search("hello").unwrap().len(), 1);
        }
    }
}
