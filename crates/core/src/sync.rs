// SPDX-License-Identifier: GPL-3.0-or-later

//! Keeps the local store in step with the server. Each account gets two
//! tasks: a worker that runs commands one after another, and a watcher that
//! waits for new mail in the inbox (IMAP IDLE) and asks the worker to sync.
//!
//! A sync first loads the headers of the last 30 days of every folder. The
//! older headers follow in small batches whenever the worker has nothing
//! else to do, so the user's own actions never wait for them. Changes the
//! user made are sent to the server before anything else, and so are
//! messages in the outbox: drafts to save and mail to send.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use async_channel::{Receiver, Sender, TryRecvError};
use chrono::{Days, NaiveDate, Utc};
use tokio::task::JoinHandle;

use crate::compose;
use crate::folder::FolderRole;
use crate::model::{Account, AccountId, Folder, FolderId, MessageId};
use crate::parse;
use crate::server::{Connector, FlagKind, IdleOutcome, MailServer, ServerError};
use crate::smtp::{SendError, Transport};
use crate::store::{ChangeKind, FolderSyncState, OutboxItem, OutboxKind, PendingChange, Store};

pub type SharedStore = Arc<Mutex<Store>>;

/// How far back the first sync goes before the rest follows in the background.
pub const SYNC_DAYS: u64 = 30;
/// How long a deletion or move can be undone before it reaches the server.
pub const UNDO_SECONDS: i64 = 8;
const HEADER_BATCH: usize = 50;
const BACKFILL_BATCH: usize = 200;
const FLAG_BATCH: usize = 500;
const PERIODIC_SYNC: Duration = Duration::from_secs(15 * 60);
const RETRY_DELAY: Duration = Duration::from_secs(30);
const INCOMPLETE_RETRIES: usize = 3;
const INCOMPLETE_PAUSE: Duration = Duration::from_millis(700);
const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);
/// When to try again after the outgoing server could not be reached, and
/// after it rejected the password or refused to save a draft.
const SEND_RETRY_SECONDS: i64 = 60;
const SEND_LOGIN_RETRY_SECONDS: i64 = 15 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    SyncAll,
    SyncFolder(FolderId),
    FetchBody(MessageId),
    /// Sends waiting changes to the server.
    Flush,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountStatus {
    Connecting,
    Online,
    Offline(String),
    LoginFailed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    Status {
        account: AccountId,
        status: AccountStatus,
    },
    FoldersChanged {
        account: AccountId,
    },
    MessagesChanged {
        account: AccountId,
        folder: FolderId,
    },
    /// A full sync of all folders finished.
    Synced {
        account: AccountId,
    },
    BodyReady {
        account: AccountId,
        message: MessageId,
    },
    BodyFailed {
        account: AccountId,
        message: MessageId,
        error: String,
    },
    /// The server refused a change; the messages are shown as before.
    ChangeFailed {
        account: AccountId,
    },
    /// New messages arrived in the inbox (not at the first sync).
    NewMail {
        account: AccountId,
        messages: Vec<MessageId>,
    },
    /// A message left the outbox.
    Sent {
        account: AccountId,
    },
    /// Sending did not work. A refused message is a draft again; in the
    /// other cases it stays in the outbox and is tried again later.
    SendFailed {
        account: AccountId,
        error: SendError,
    },
}

fn lock(store: &SharedStore) -> MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn sync_since() -> NaiveDate {
    Utc::now()
        .date_naive()
        .checked_sub_days(Days::new(SYNC_DAYS))
        .unwrap_or(NaiveDate::MIN)
}

/// Older messages per folder whose headers still have to be loaded, in
/// sync order. The UIDs are sorted so the newest come first off the end.
#[derive(Debug, Default)]
pub struct Backlog {
    folders: Vec<(FolderId, Vec<u32>)>,
}

impl Backlog {
    pub fn is_empty(&self) -> bool {
        self.folders.iter().all(|(_, uids)| uids.is_empty())
    }

    pub fn knows(&self, folder: FolderId) -> bool {
        self.folders.iter().any(|(id, _)| *id == folder)
    }

    pub fn remaining(&self) -> usize {
        self.folders.iter().map(|(_, uids)| uids.len()).sum()
    }

    fn set(&mut self, folder: FolderId, mut uids: Vec<u32>) {
        uids.sort_unstable();
        match self.folders.iter_mut().find(|(id, _)| *id == folder) {
            Some(entry) => entry.1 = uids,
            None => self.folders.push((folder, uids)),
        }
    }

    fn forget(&mut self, folder: FolderId) {
        self.folders.retain(|(id, _)| *id != folder);
    }

    /// The next batch: the newest missing messages of the first folder that
    /// still has some.
    fn next_batch(&mut self) -> Option<(FolderId, Vec<u32>)> {
        let (folder, uids) = self.folders.iter_mut().find(|(_, uids)| !uids.is_empty())?;
        let start = uids.len().saturating_sub(BACKFILL_BATCH);
        let mut batch = uids.split_off(start);
        batch.reverse();
        Some((*folder, batch))
    }
}

/// Fetches the folder list and stores it. Returns the folders in sync order:
/// inbox first, then the other standard folders, then the rest.
pub async fn sync_folder_list<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
) -> anyhow::Result<Vec<Folder>> {
    let remote = server.list_folders().await?;
    lock(store).sync_folders(&remote)?;
    // The sidebar always shows all standard folders, so missing ones are
    // created right away (only once the list is trustworthy, with an inbox).
    let has_inbox = lock(store).folder_by_role(FolderRole::Inbox)?.is_some();
    if has_inbox {
        for role in [
            FolderRole::Drafts,
            FolderRole::Sent,
            FolderRole::Archive,
            FolderRole::Junk,
            FolderRole::Trash,
        ] {
            if lock(store).folder_by_role(role)?.is_none()
                && let Err(error) = standard_folder(server, store, role).await
            {
                tracing::info!("could not create the {} folder: {error}", role.as_str());
            }
        }
    }
    Ok(lock(store).folders()?)
}

/// Brings one folder up to date: new messages, removed messages and flags.
/// Headers of new messages from the last days (or that arrived since the
/// last sync) are loaded right away, older ones go into the backlog.
/// Returns true if anything visible changed.
pub async fn sync_folder<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    folder: &Folder,
    since: NaiveDate,
    events: &Sender<SyncEvent>,
    backlog: &mut Backlog,
) -> anyhow::Result<bool> {
    let mut selected = server.select(&folder.path).await?;
    let known = lock(store).uid_validity(folder.id)?;
    // Seen with GreenMail while mail is being delivered: for a moment the
    // folder looks empty and has no UIDVALIDITY. Nothing from such an answer
    // may change the local copy; wait briefly, then give up the connection so
    // the worker starts over with a fresh one.
    if known.is_some() && selected.uid_validity.is_none() {
        for _ in 0..INCOMPLETE_RETRIES {
            tokio::time::sleep(INCOMPLETE_PAUSE).await;
            selected = server.select(&folder.path).await?;
            if selected.uid_validity.is_some() {
                break;
            }
        }
        if selected.uid_validity.is_none() {
            return Err(
                ServerError::Connection(format!("incomplete answer for {}", folder.path)).into(),
            );
        }
    }
    let validity = match (known, selected.uid_validity) {
        // A missing UIDVALIDITY is a garbled answer, not a rebuilt folder.
        // Treating it as one would throw away the folder and keep new mail
        // from being announced.
        (Some(old), None) => {
            tracing::debug!(
                "{}: no UIDVALIDITY in the answer, keeping {old}",
                folder.path
            );
            Some(old)
        }
        // A real change is rare and expensive; only believe it when a
        // second look says the same.
        (Some(old), Some(new)) if old != new => {
            selected = server.select(&folder.path).await?;
            if selected.uid_validity == Some(new) {
                tracing::debug!("{}: UIDVALIDITY changed from {old} to {new}", folder.path);
                Some(new)
            } else {
                tracing::debug!(
                    "{}: UIDVALIDITY {new} not confirmed ({:?}), keeping {old}",
                    folder.path,
                    selected.uid_validity
                );
                Some(old)
            }
        }
        (_, value) => value,
    };
    let reset = lock(store).set_uid_validity(folder.id, validity)?;
    let state = lock(store).sync_state(folder.id)?;
    let local: HashSet<u32> = lock(store).uids(folder.id)?.into_iter().collect();

    let unchanged = selected.highest_modseq.is_some()
        && state.highest_modseq == selected.highest_modseq
        && state.exists == Some(selected.exists);
    if unchanged && (backlog.knows(folder.id) || local.len() as u32 >= selected.exists) {
        return Ok(false);
    }

    // Always ask: a message count of 0 may come from an incomplete answer,
    // and trusting it would delete every message kept here.
    let remote: HashSet<u32> = server.search_all().await?.into_iter().collect();
    if remote.is_empty() && !local.is_empty() && selected.exists > 0 {
        // The server counts messages but finds none: a confused answer,
        // not an empty folder.
        tracing::debug!(
            "{}: search found nothing in a non-empty folder",
            folder.path
        );
        return Ok(false);
    }

    let gone: Vec<u32> = local.difference(&remote).copied().collect();
    let mut changed = reset || !gone.is_empty();
    if !gone.is_empty() {
        tracing::debug!(
            "{}: {} of {} messages are no longer on the server",
            folder.path,
            gone.len(),
            local.len()
        );
        lock(store).remove_messages(folder.id, &gone)?;
    }

    let missing: HashSet<u32> = remote.difference(&local).copied().collect();
    let mut now_wanted: Vec<u32> = match state.synced_uid {
        Some(synced) => missing
            .iter()
            .copied()
            .filter(|uid| *uid > synced)
            .collect(),
        None if missing.is_empty() => Vec::new(),
        None => server
            .search_since(since)
            .await?
            .into_iter()
            .filter(|uid| missing.contains(uid))
            .collect(),
    };
    now_wanted.sort_unstable_by(|a, b| b.cmp(a));
    let later: Vec<u32> = {
        let wanted: HashSet<u32> = now_wanted.iter().copied().collect();
        missing.difference(&wanted).copied().collect()
    };

    let kept: Vec<u32> = {
        let mut kept: Vec<u32> = local.intersection(&remote).copied().collect();
        kept.sort_unstable();
        kept
    };
    let flags = match (state.highest_modseq, selected.highest_modseq) {
        (Some(old), Some(_)) if !reset => server.fetch_changed_flags(old).await?,
        _ => {
            let mut all = Vec::new();
            for batch in kept.chunks(FLAG_BATCH) {
                all.extend(server.fetch_flags(batch).await?);
            }
            all
        }
    };
    {
        let store = lock(store);
        for (uid, flags) in flags {
            if local.contains(&uid) {
                changed |= store.set_flags(folder.id, uid, flags)?;
            }
        }
    }

    // Mail that arrived in the inbox since the last sync. The first sync
    // of a folder, or one after the folder was rebuilt, reports nothing.
    let arrivals: Vec<u32> =
        if state.synced_uid.is_some() && !reset && folder.role == Some(FolderRole::Inbox) {
            now_wanted.clone()
        } else {
            Vec::new()
        };
    if reset {
        tracing::debug!("folder {} was rebuilt (new UIDVALIDITY)", folder.path);
    }
    if folder.role == Some(FolderRole::Inbox) && !now_wanted.is_empty() {
        tracing::debug!(
            "inbox {}: {} to load, {} reported as new",
            folder.id,
            now_wanted.len(),
            arrivals.len()
        );
    }
    for batch in now_wanted.chunks(HEADER_BATCH) {
        load_headers(server, store, folder.id, batch).await?;
        changed = true;
        let _ = events
            .send(SyncEvent::MessagesChanged {
                account: folder.account_id,
                folder: folder.id,
            })
            .await;
    }
    backlog.set(folder.id, later);
    if !arrivals.is_empty() {
        let mut messages = lock(store).message_ids(folder.id, &arrivals)?;
        let spam = lock(store).from_spam_senders(&messages)?;
        if !spam.is_empty() {
            let junk = lock(store).folder_by_role(FolderRole::Junk)?;
            if let Some(junk) = junk {
                lock(store).move_messages(&spam, junk.id, Utc::now().timestamp())?;
                messages.retain(|id| !spam.contains(id));
                changed = true;
            }
        }
        if !messages.is_empty() {
            let _ = events
                .send(SyncEvent::NewMail {
                    account: folder.account_id,
                    messages,
                })
                .await;
        }
    }

    let unseen = server.count_unseen().await?;
    let store = lock(store);
    let unread = unseen.saturating_sub(store.hidden_unseen(folder.id)?);
    let old_unread = store.folder(folder.id)?.map(|f| f.unread);
    if old_unread != Some(unread) {
        store.set_unread(folder.id, unread)?;
        let _ = events.try_send(SyncEvent::FoldersChanged {
            account: folder.account_id,
        });
    }
    store.set_sync_state(
        folder.id,
        FolderSyncState {
            highest_modseq: selected.highest_modseq,
            exists: Some(selected.exists),
            synced_uid: remote
                .iter()
                .copied()
                .max()
                .or(state.synced_uid)
                .or(Some(0)),
        },
    )?;
    Ok(changed)
}

pub(crate) async fn load_headers<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    folder: FolderId,
    uids: &[u32],
) -> anyhow::Result<()> {
    let headers = server.fetch_headers(uids).await?;
    let now = Utc::now().timestamp();
    let rows: Vec<_> = headers
        .iter()
        .map(|header| {
            (
                header.uid,
                parse::parse_header(&header.raw_header),
                header.internal_date.unwrap_or(now),
                header.flags,
            )
        })
        .collect();
    lock(store).insert_messages(folder, &rows)?;
    Ok(())
}

/// Loads one batch of older headers. Returns the folder that got them.
pub async fn backfill_step<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    backlog: &mut Backlog,
) -> anyhow::Result<Option<FolderId>> {
    let Some((id, batch)) = backlog.next_batch() else {
        return Ok(None);
    };
    let folder = lock(store).folder(id)?;
    let Some(folder) = folder else {
        backlog.forget(id);
        return Ok(None);
    };
    let selected = server.select(&folder.path).await?;
    // A missing value is no change; a different one is left to the next
    // full sync, which double-checks it.
    if selected.uid_validity.is_some() && lock(store).uid_validity(id)? != selected.uid_validity {
        // The folder was rebuilt on the server; the next sync starts over.
        backlog.forget(id);
        return Ok(None);
    }
    load_headers(server, store, id, &batch).await?;
    Ok(Some(id))
}

/// Downloads a whole message and stores it.
pub async fn fetch_body<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    message: MessageId,
) -> anyhow::Result<Folder> {
    let (summary, folder) = {
        let store = lock(store);
        let summary = store
            .message(message)?
            .ok_or_else(|| anyhow!("message {message} is gone"))?;
        let folder = store
            .folder(summary.folder_id)?
            .ok_or_else(|| anyhow!("folder {} is gone", summary.folder_id))?;
        (summary, folder)
    };
    server.select(&folder.path).await?;
    let raw = server.fetch_message(summary.uid).await?;
    let parsed = parse::parse_message(&raw);
    let text = parse::searchable_text(&parsed.body);
    lock(store).store_body(
        message,
        &raw,
        &parsed.preview,
        &text,
        !parsed.attachments.is_empty(),
    )?;
    Ok(folder)
}

/// Works through the outbox: saves drafts, sends mail and files copies in
/// "Sent". Returns the folders that changed. A lost IMAP connection ends
/// the run with an error; everything not done yet stays queued. A message
/// that went out is never sent twice, even if filing it fails.
pub async fn process_outbox<S: MailServer, T: Transport>(
    server: &mut S,
    transport: &T,
    store: &SharedStore,
    account: AccountId,
    sender: &str,
    events: &Sender<SyncEvent>,
) -> anyhow::Result<HashSet<FolderId>> {
    let mut touched = HashSet::new();
    let now = Utc::now().timestamp();
    let items = lock(store).outbox(now)?;
    for mut item in items {
        match item.kind {
            OutboxKind::Draft => save_draft(server, store, &item, &mut touched).await?,
            OutboxKind::Discard => {
                remove_drafts(server, store, &item.message_id, &mut touched).await?;
                lock(store).finish_outbox(item.id)?;
            }
            OutboxKind::Send => {
                let wire = compose::strip_bcc(&item.raw);
                match transport.send(sender, &item.recipients, &wire).await {
                    Ok(()) => {
                        lock(store).mark_sent(item.id)?;
                        let _ = events.send(SyncEvent::Sent { account }).await;
                        item.kind = OutboxKind::Sent;
                        file_sent(server, store, &item, &mut touched).await?;
                    }
                    Err(SendError::Rejected(reason)) => {
                        tracing::warn!("account {account}: message refused: {reason}");
                        lock(store).send_failed_to_draft(item.id)?;
                        item.kind = OutboxKind::Draft;
                        save_draft(server, store, &item, &mut touched).await?;
                        let _ = events
                            .send(SyncEvent::SendFailed {
                                account,
                                error: SendError::Rejected(reason),
                            })
                            .await;
                    }
                    Err(error) => {
                        let (reason, wait) = match &error {
                            SendError::Login(reason) => (reason.clone(), SEND_LOGIN_RETRY_SECONDS),
                            SendError::Unreachable(reason) | SendError::Rejected(reason) => {
                                (reason.clone(), SEND_RETRY_SECONDS)
                            }
                        };
                        tracing::info!("account {account}: sending later: {reason}");
                        lock(store).postpone_outbox(item.id, now + wait, &reason)?;
                        if item.error.as_deref() != Some(reason.as_str()) {
                            let _ = events.send(SyncEvent::SendFailed { account, error }).await;
                        }
                    }
                }
            }
            OutboxKind::Sent => file_sent(server, store, &item, &mut touched).await?,
        }
    }
    Ok(touched)
}

/// Saves a draft and removes the versions it replaces. The new version is
/// added first, so a draft is never lost in between.
async fn save_draft<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    item: &OutboxItem,
    touched: &mut HashSet<FolderId>,
) -> anyhow::Result<()> {
    let result = async {
        let folder = standard_folder(server, store, FolderRole::Drafts).await?;
        server.select(&folder.path).await?;
        let older = server.search_message_id(&item.message_id).await?;
        server.save_message(&folder.path, true, &item.raw).await?;
        if !older.is_empty() {
            server.select(&folder.path).await?;
            server.delete_messages(&older).await?;
        }
        Ok::<_, ServerError>(folder.id)
    }
    .await;
    match result {
        Ok(folder) => {
            touched.insert(folder);
            lock(store).finish_outbox(item.id)?;
            Ok(())
        }
        Err(ServerError::Protocol(reason)) => {
            tracing::warn!("could not save a draft: {reason}");
            let retry = Utc::now().timestamp() + SEND_LOGIN_RETRY_SECONDS;
            lock(store).postpone_outbox(item.id, retry, &reason)?;
            Ok(())
        }
        Err(other) => Err(other.into()),
    }
}

/// Files the copy in "Sent" (unless the provider does that itself) and
/// removes the draft the message came from.
async fn file_sent<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    item: &OutboxItem,
    touched: &mut HashSet<FolderId>,
) -> anyhow::Result<()> {
    if !server.saves_sent_mail() {
        let result = async {
            let folder = standard_folder(server, store, FolderRole::Sent).await?;
            server.save_message(&folder.path, false, &item.raw).await?;
            Ok::<_, ServerError>(folder.id)
        }
        .await;
        match result {
            Ok(folder) => {
                touched.insert(folder);
            }
            Err(ServerError::Protocol(reason)) => {
                tracing::warn!("could not file the sent message: {reason}");
            }
            Err(other) => return Err(other.into()),
        }
    }
    remove_drafts(server, store, &item.message_id, touched).await?;
    lock(store).finish_outbox(item.id)?;
    Ok(())
}

async fn remove_drafts<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    message_id: &str,
    touched: &mut HashSet<FolderId>,
) -> anyhow::Result<()> {
    let folder = lock(store).folder_by_role(FolderRole::Drafts)?;
    let Some(folder) = folder else {
        return Ok(());
    };
    let result = async {
        server.select(&folder.path).await?;
        let uids = server.search_message_id(message_id).await?;
        server.delete_messages(&uids).await?;
        Ok::<_, ServerError>(!uids.is_empty())
    }
    .await;
    match result {
        Ok(removed) => {
            if removed {
                touched.insert(folder.id);
            }
            Ok(())
        }
        Err(ServerError::Protocol(reason)) => {
            tracing::warn!("could not remove a draft: {reason}");
            Ok(())
        }
        Err(other) => Err(other.into()),
    }
}

/// The drafts or sent folder, created on the server if it is missing.
async fn standard_folder<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    role: FolderRole,
) -> Result<Folder, ServerError> {
    let known = |store: &SharedStore| {
        lock(store)
            .folder_by_role(role)
            .map_err(|error| ServerError::Protocol(error.to_string()))
    };
    if let Some(folder) = known(store)? {
        return Ok(folder);
    }
    let name = match role {
        FolderRole::Inbox => "INBOX",
        FolderRole::Drafts => "Drafts",
        FolderRole::Sent => "Sent",
        FolderRole::Archive => "Archive",
        FolderRole::Junk => "Junk",
        FolderRole::Trash => "Trash",
    };
    let created = match server.create_mailbox(name).await {
        Ok(()) => true,
        Err(error) => {
            // It may exist already without being recognised.
            tracing::info!("creating {name}: {error}");
            false
        }
    };
    let remote = server.list_folders().await?;
    lock(store)
        .sync_folders(&remote)
        .map_err(|error| ServerError::Protocol(error.to_string()))?;
    if let Some(folder) = known(store)? {
        return Ok(folder);
    }
    if created {
        // Some servers do not list a new folder right away.
        lock(store)
            .add_folder(name, role)
            .map_err(|error| ServerError::Protocol(error.to_string()))?;
        if let Some(folder) = known(store)? {
            return Ok(folder);
        }
    }
    Err(ServerError::Protocol(format!("there is no {name} folder")))
}

/// What sending the waiting changes touched.
#[derive(Debug, Default)]
pub struct Flushed {
    /// Folders that lost or changed messages.
    pub sources: HashSet<FolderId>,
    /// Folders that received messages and should be synced.
    pub targets: HashSet<FolderId>,
    /// Changes the server refused.
    pub failed: usize,
}

/// Sends all due changes to the server, folder by folder. A change the
/// server refuses is dropped and its messages come back into view; a lost
/// connection keeps the rest for the next attempt.
pub async fn flush_changes<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
) -> anyhow::Result<Flushed> {
    let due = lock(store).due_changes(Utc::now().timestamp())?;
    let mut flushed = Flushed::default();
    let mut by_folder: Vec<(FolderId, String, Vec<PendingChange>)> = Vec::new();
    for change in due {
        match by_folder.iter_mut().find(|(id, ..)| *id == change.folder) {
            Some(entry) => entry.2.push(change),
            None => by_folder.push((change.folder, change.folder_path.clone(), vec![change])),
        }
    }
    for (folder, path, changes) in by_folder {
        flushed.sources.insert(folder);
        if let Err(error) = server.select(&path).await {
            settle(store, &changes, &error, &mut flushed)?;
            continue;
        }
        for group in group_changes(&changes) {
            let uids: Vec<u32> = group.iter().map(|change| change.uid).collect();
            let first = group[0];
            if matches!(first.kind, ChangeKind::Move | ChangeKind::Delete) {
                tracing::debug!(
                    "{:?} of {} messages in {path} to {:?}",
                    first.kind,
                    uids.len(),
                    first.target_path
                );
            }
            let result = match first.kind {
                ChangeKind::Seen => server.set_flag(&uids, FlagKind::Seen, true).await,
                ChangeKind::Unseen => server.set_flag(&uids, FlagKind::Seen, false).await,
                ChangeKind::Flag => server.set_flag(&uids, FlagKind::Flagged, true).await,
                ChangeKind::Unflag => server.set_flag(&uids, FlagKind::Flagged, false).await,
                ChangeKind::Move => match first.target_path.as_deref() {
                    Some(target) => server.move_messages(&uids, target).await,
                    None => Err(ServerError::Protocol("the target folder is gone".into())),
                },
                ChangeKind::Delete => server.delete_messages(&uids).await,
            };
            match result {
                Ok(()) => {
                    let mut store = lock(store);
                    for change in &group {
                        store.finish_change(change)?;
                    }
                    if let Some(target) = first.target.filter(|_| first.kind == ChangeKind::Move) {
                        flushed.targets.insert(target);
                    }
                }
                Err(error) => {
                    let group: Vec<PendingChange> = group.into_iter().cloned().collect();
                    settle(store, &group, &error, &mut flushed)?;
                }
            }
        }
    }
    Ok(flushed)
}

/// Changes of one folder in the order the server should get them: flags
/// before moves, each sort of change in one command.
fn group_changes(changes: &[PendingChange]) -> Vec<Vec<&PendingChange>> {
    let order = [
        ChangeKind::Seen,
        ChangeKind::Unseen,
        ChangeKind::Flag,
        ChangeKind::Unflag,
        ChangeKind::Move,
        ChangeKind::Delete,
    ];
    let mut groups: Vec<Vec<&PendingChange>> = Vec::new();
    for kind in order {
        let mut by_target: Vec<Vec<&PendingChange>> = Vec::new();
        for change in changes.iter().filter(|change| change.kind == kind) {
            match by_target
                .iter_mut()
                .find(|group| group[0].target == change.target)
            {
                Some(group) => group.push(change),
                None => by_target.push(vec![change]),
            }
        }
        groups.extend(by_target);
    }
    groups
}

/// A refused change is dropped; a broken connection ends the flush.
fn settle(
    store: &SharedStore,
    changes: &[PendingChange],
    error: &ServerError,
    flushed: &mut Flushed,
) -> anyhow::Result<()> {
    match error {
        ServerError::Protocol(reason) => {
            tracing::warn!("server refused a change: {reason}");
            let mut store = lock(store);
            for change in changes {
                store.abandon_change(change)?;
            }
            flushed.failed += changes.len();
            Ok(())
        }
        other => Err(ServerError::Connection(other.to_string()).into()),
    }
}

/// Running background tasks of one account. Dropping it stops them.
pub struct AccountHandle {
    commands: Sender<Command>,
    tasks: Vec<JoinHandle<()>>,
}

impl AccountHandle {
    pub fn send(&self, command: Command) {
        let _ = self.commands.try_send(command);
    }
}

impl Drop for AccountHandle {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub fn start_account<C: Connector, T: Transport>(
    runtime: &tokio::runtime::Handle,
    account: Account,
    connector: Arc<C>,
    transport: Arc<T>,
    store: SharedStore,
    events: Sender<SyncEvent>,
) -> AccountHandle {
    let (commands, receiver) = async_channel::unbounded();
    let _ = commands.try_send(Command::SyncAll);
    let worker = runtime.spawn(run_worker(
        Outgoing {
            account: account.id,
            sender: account.email.clone(),
            transport,
        },
        connector.clone(),
        store.clone(),
        receiver,
        events.clone(),
    ));
    let watcher = runtime.spawn(run_watcher(account.id, connector, store, commands.clone()));
    AccountHandle {
        commands,
        tasks: vec![worker, watcher],
    }
}

/// Work the worker does when no command is waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Job {
    Command(Command),
    Backfill,
}

/// What the worker needs for sending.
struct Outgoing<T> {
    account: AccountId,
    sender: String,
    transport: Arc<T>,
}

async fn run_worker<C: Connector, T: Transport>(
    outgoing: Outgoing<T>,
    connector: Arc<C>,
    store: SharedStore,
    commands: Receiver<Command>,
    events: Sender<SyncEvent>,
) {
    let mut server: Option<C::Server> = None;
    let mut status: Option<AccountStatus> = None;
    let mut next_sync = Instant::now() + PERIODIC_SYNC;
    let mut backlog = Backlog::default();
    let account = outgoing.account;

    loop {
        let now = Instant::now();
        let connected = server.is_some();
        let due_in = if connected {
            let store = lock(&store);
            let changes = store.next_due().ok().flatten();
            let outbox = store.next_outbox_retry().ok().flatten();
            changes.into_iter().chain(outbox).min().map(|due| {
                Duration::from_secs(due.saturating_sub(Utc::now().timestamp()).max(0) as u64)
            })
        } else {
            None
        };
        let backfill = connected && !backlog.is_empty();
        let mut wait = next_sync.saturating_duration_since(now);
        if let Some(due_in) = due_in {
            wait = wait.min(due_in);
        }
        if backfill {
            wait = Duration::ZERO;
        }

        let received = if wait.is_zero() {
            match commands.try_recv() {
                Ok(command) => Some(command),
                Err(TryRecvError::Closed) => return,
                Err(TryRecvError::Empty) => None,
            }
        } else {
            match tokio::time::timeout(wait, commands.recv()).await {
                Ok(Ok(command)) => Some(command),
                Ok(Err(_)) => return,
                Err(_) => None,
            }
        };
        let job = match received {
            Some(command) => Job::Command(command),
            None if Instant::now() >= next_sync => Job::Command(Command::SyncAll),
            None if due_in.is_some_and(|due| due <= wait) => Job::Command(Command::Flush),
            None if backfill => Job::Backfill,
            None => continue,
        };
        if job == Job::Command(Command::SyncAll) {
            next_sync = Instant::now() + PERIODIC_SYNC;
        }

        if server.is_none() {
            if status != Some(AccountStatus::Connecting) && status != Some(AccountStatus::Online) {
                set_status(&events, account, &mut status, AccountStatus::Connecting).await;
            }
            match connector.connect().await {
                Ok(connected) => {
                    server = Some(connected);
                    set_status(&events, account, &mut status, AccountStatus::Online).await;
                }
                Err(error) => {
                    let failed = match &error {
                        ServerError::Authentication(message) => {
                            AccountStatus::LoginFailed(message.clone())
                        }
                        other => AccountStatus::Offline(other.to_string()),
                    };
                    if let Job::Command(Command::FetchBody(message)) = job {
                        let _ = events
                            .send(SyncEvent::BodyFailed {
                                account,
                                message,
                                error: error.to_string(),
                            })
                            .await;
                    }
                    next_sync = Instant::now()
                        + match failed {
                            AccountStatus::LoginFailed(_) => PERIODIC_SYNC,
                            _ => RETRY_DELAY,
                        };
                    set_status(&events, account, &mut status, failed).await;
                    continue;
                }
            }
        }
        let Some(connected) = server.as_mut() else {
            continue;
        };

        let result = match job {
            Job::Command(command) => {
                run_command(connected, &store, &outgoing, command, &events, &mut backlog).await
            }
            Job::Backfill => match backfill_step(connected, &store, &mut backlog).await {
                Ok(Some(folder)) => {
                    let _ = events
                        .send(SyncEvent::MessagesChanged { account, folder })
                        .await;
                    Ok(())
                }
                Ok(None) => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(error) = result {
            tracing::warn!("account {account}: {error:#}");
            if let Job::Command(Command::FetchBody(message)) = job {
                let _ = events
                    .send(SyncEvent::BodyFailed {
                        account,
                        message,
                        error: format!("{error:#}"),
                    })
                    .await;
            }
            let incomplete = matches!(
                error.downcast_ref::<ServerError>(),
                Some(ServerError::Connection(reason)) if reason.starts_with("incomplete answer")
            );
            if incomplete {
                // A hiccup of the server, not a lost connection: start over
                // right away with a fresh one, without telling anybody.
                if let Some(broken) = server.take() {
                    broken.logout().await;
                }
                next_sync = Instant::now() + INCOMPLETE_PAUSE;
            } else if error.downcast_ref::<ServerError>().is_some() {
                if let Some(broken) = server.take() {
                    broken.logout().await;
                }
                set_status(
                    &events,
                    account,
                    &mut status,
                    AccountStatus::Offline(error.to_string()),
                )
                .await;
                next_sync = Instant::now() + RETRY_DELAY;
            }
        }
    }
}

async fn set_status(
    events: &Sender<SyncEvent>,
    account: AccountId,
    current: &mut Option<AccountStatus>,
    status: AccountStatus,
) {
    if current.as_ref() == Some(&status) {
        return;
    }
    *current = Some(status.clone());
    let _ = events.send(SyncEvent::Status { account, status }).await;
}

async fn run_command<S: MailServer, T: Transport>(
    server: &mut S,
    store: &SharedStore,
    outgoing: &Outgoing<T>,
    command: Command,
    events: &Sender<SyncEvent>,
    backlog: &mut Backlog,
) -> anyhow::Result<()> {
    let account = outgoing.account;
    let filed = process_outbox(
        server,
        outgoing.transport.as_ref(),
        store,
        account,
        &outgoing.sender,
        events,
    )
    .await?;
    for folder in &filed {
        sync_one(server, store, account, *folder, events, backlog).await?;
    }
    if !filed.is_empty() {
        let _ = events.send(SyncEvent::FoldersChanged { account }).await;
    }
    let flushed = flush_changes(server, store).await?;
    if flushed.failed > 0 {
        let _ = events.send(SyncEvent::ChangeFailed { account }).await;
    }
    for folder in &flushed.sources {
        let _ = events
            .send(SyncEvent::MessagesChanged {
                account,
                folder: *folder,
            })
            .await;
    }
    for target in &flushed.targets {
        sync_one(server, store, account, *target, events, backlog).await?;
    }
    if !flushed.sources.is_empty() {
        let _ = events.send(SyncEvent::FoldersChanged { account }).await;
    }

    match command {
        Command::SyncAll => {
            let folders = sync_folder_list(server, store)
                .await
                .context("loading the folder list")?;
            let _ = events.send(SyncEvent::FoldersChanged { account }).await;
            let since = sync_since();
            let known: HashSet<FolderId> = folders.iter().map(|folder| folder.id).collect();
            backlog.folders.retain(|(id, _)| known.contains(id));
            for folder in &folders {
                if sync_folder(server, store, folder, since, events, backlog)
                    .await
                    .with_context(|| format!("syncing {}", folder.path))?
                {
                    let _ = events
                        .send(SyncEvent::MessagesChanged {
                            account,
                            folder: folder.id,
                        })
                        .await;
                }
            }
            let _ = events.send(SyncEvent::Synced { account }).await;
        }
        Command::SyncFolder(id) => {
            sync_one(server, store, account, id, events, backlog).await?;
        }
        Command::FetchBody(message) => {
            let folder = fetch_body(server, store, message).await?;
            let _ = events.send(SyncEvent::BodyReady { account, message }).await;
            let _ = events
                .send(SyncEvent::MessagesChanged {
                    account,
                    folder: folder.id,
                })
                .await;
        }
        Command::Flush => {}
    }
    Ok(())
}

async fn sync_one<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    account: AccountId,
    id: FolderId,
    events: &Sender<SyncEvent>,
    backlog: &mut Backlog,
) -> anyhow::Result<()> {
    let folder = lock(store).folder(id)?;
    if let Some(folder) = folder
        && sync_folder(server, store, &folder, sync_since(), events, backlog).await?
    {
        let _ = events
            .send(SyncEvent::MessagesChanged {
                account,
                folder: folder.id,
            })
            .await;
    }
    Ok(())
}

/// Keeps a second connection in IDLE on the inbox and asks the worker to
/// sync it whenever the server reports a change.
async fn run_watcher<C: Connector>(
    account: AccountId,
    connector: Arc<C>,
    store: SharedStore,
    commands: Sender<Command>,
) {
    loop {
        match connector.connect().await {
            Ok(mut server) => {
                if let Err(error) = watch_inbox(&mut server, &store, &commands).await {
                    tracing::info!("account {account}: inbox watch stopped: {error}");
                }
                server.logout().await;
            }
            Err(ServerError::Authentication(_)) => {
                tokio::time::sleep(PERIODIC_SYNC).await;
                continue;
            }
            Err(error) => tracing::info!("account {account}: watcher cannot connect: {error}"),
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

async fn watch_inbox<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    commands: &Sender<Command>,
) -> Result<(), ServerError> {
    let inbox = {
        let store = lock(store);
        store.folder_by_role(FolderRole::Inbox).ok().flatten()
    };
    let path = inbox
        .as_ref()
        .map_or("INBOX", |folder| folder.path.as_str());
    server.select(path).await?;
    loop {
        if server.idle(IDLE_TIMEOUT).await? == IdleOutcome::NewData {
            let command = match &inbox {
                Some(folder) => Command::SyncFolder(folder.id),
                None => Command::SyncAll,
            };
            if commands.send(command).await.is_err() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests;
