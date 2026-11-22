// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use crate::model::{Flags, Security, ServerConfig, ServerSettings};
use crate::server::{RemoteFolder, RemoteHeader, SelectedFolder, ServerResult};
use std::collections::BTreeMap;
use tokio::sync::Notify;

#[derive(Clone)]
struct FakeMessage {
    uid: u32,
    date: i64,
    flags: Flags,
    modseq: u64,
    raw: Vec<u8>,
}

#[derive(Default)]
struct FakeMailbox {
    validity: u32,
    next_uid: u32,
    messages: Vec<FakeMessage>,
}

#[derive(Default)]
struct FakeState {
    folders: BTreeMap<String, FakeMailbox>,
    reject_login: bool,
    condstore: bool,
    modseq: u64,
    /// Flag fetches without CHANGEDSINCE, to see which path a sync took.
    full_flag_fetches: usize,
    refuse_moves: bool,
    /// Behaves like Gmail, which files sent mail by itself.
    gmail: bool,
    /// Drops the connection on the next APPEND.
    drop_on_append: bool,
    /// Answers the next folder list with nothing, as a broken server might.
    empty_list_once: bool,
    /// Leaves UIDVALIDITY out of the next SELECT answers.
    validity_missing: usize,
    /// Reports this UIDVALIDITY in the next SELECT answer only.
    validity_once: Option<u32>,
}

#[derive(Clone, Default)]
struct Fake {
    state: Arc<Mutex<FakeState>>,
    changed: Arc<Notify>,
}

impl Fake {
    fn with_folders(names: &[&str]) -> Fake {
        let fake = Fake::default();
        for name in names {
            fake.state.lock().unwrap().folders.insert(
                name.to_string(),
                FakeMailbox {
                    validity: 1,
                    next_uid: 1,
                    messages: Vec::new(),
                },
            );
        }
        fake
    }

    fn deliver(&self, folder: &str, uid: u32, days_ago: u64, subject: &str) {
        self.deliver_raw(folder, uid, days_ago, subject, "");
    }

    fn deliver_raw(&self, folder: &str, uid: u32, days_ago: u64, subject: &str, extra: &str) {
        let date = Utc::now().timestamp() - (days_ago as i64) * 86_400;
        let raw = format!(
            "From: Anna <anna@example.org>\r\nSubject: {subject}\r\n{extra}\
             Content-Type: text/plain\r\n\r\nBody of {subject}\r\n"
        )
        .into_bytes();
        {
            let mut state = self.state.lock().unwrap();
            state.modseq += 1;
            let modseq = state.modseq;
            let mailbox = state.folders.get_mut(folder).unwrap();
            mailbox.next_uid = mailbox.next_uid.max(uid + 1);
            mailbox.messages.push(FakeMessage {
                uid,
                date,
                flags: Flags::default(),
                modseq,
                raw,
            });
        }
        self.changed.notify_waiters();
    }

    fn mailbox<T>(&self, folder: &str, f: impl FnOnce(&mut FakeMailbox) -> T) -> T {
        f(self.state.lock().unwrap().folders.get_mut(folder).unwrap())
    }

    fn flags(&self, folder: &str, uid: u32) -> Flags {
        self.mailbox(folder, |m| {
            m.messages.iter().find(|m| m.uid == uid).unwrap().flags
        })
    }

    fn subjects_on_server(&self, folder: &str) -> Vec<String> {
        self.mailbox(folder, |m| {
            m.messages
                .iter()
                .map(|m| crate::parse::parse_header(&m.raw).subject)
                .collect()
        })
    }
}

struct FakeServer {
    fake: Fake,
    selected: Option<String>,
}

impl FakeServer {
    fn messages(&self) -> Vec<FakeMessage> {
        let name = self.selected.clone().expect("a selected folder");
        self.fake.mailbox(&name, |mailbox| mailbox.messages.clone())
    }

    fn with_selected<T>(&self, f: impl FnOnce(&mut FakeState, &str) -> T) -> T {
        let name = self.selected.clone().expect("a selected folder");
        f(&mut self.fake.state.lock().unwrap(), &name)
    }
}

impl MailServer for FakeServer {
    async fn list_folders(&mut self) -> ServerResult<Vec<RemoteFolder>> {
        if std::mem::take(&mut self.fake.state.lock().unwrap().empty_list_once) {
            return Ok(Vec::new());
        }
        Ok(self
            .fake
            .state
            .lock()
            .unwrap()
            .folders
            .keys()
            .map(|path| RemoteFolder {
                path: path.clone(),
                delimiter: Some("/".into()),
                role: FolderRole::from_name(path, Some("/")),
                selectable: true,
            })
            .collect())
    }

    async fn select(&mut self, path: &str) -> ServerResult<SelectedFolder> {
        let state = self.fake.state.lock().unwrap();
        let Some(mailbox) = state.folders.get(path) else {
            return Err(ServerError::Protocol(format!("no folder {path}")));
        };
        let validity = mailbox.validity;
        let mut state = state;
        let uid_validity = if state.validity_missing > 0 {
            state.validity_missing -= 1;
            None
        } else {
            Some(state.validity_once.take().unwrap_or(validity))
        };
        let Some(mailbox) = state.folders.get(path) else {
            return Err(ServerError::Protocol(format!("no folder {path}")));
        };
        let selected = SelectedFolder {
            exists: if uid_validity.is_some() {
                mailbox.messages.len() as u32
            } else {
                // GreenMail's incomplete answers lack the count as well.
                0
            },
            uid_validity,
            highest_modseq: state.condstore.then_some(state.modseq),
        };
        drop(state);
        self.selected = Some(path.to_owned());
        Ok(selected)
    }

    async fn search_since(&mut self, since: NaiveDate) -> ServerResult<Vec<u32>> {
        let since = since.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
        Ok(self
            .messages()
            .into_iter()
            .filter(|m| m.date >= since)
            .map(|m| m.uid)
            .collect())
    }

    async fn search_all(&mut self) -> ServerResult<Vec<u32>> {
        Ok(self.messages().into_iter().map(|m| m.uid).collect())
    }

    async fn count_unseen(&mut self) -> ServerResult<u32> {
        Ok(self.messages().iter().filter(|m| !m.flags.seen).count() as u32)
    }

    async fn fetch_headers(&mut self, uids: &[u32]) -> ServerResult<Vec<RemoteHeader>> {
        Ok(self
            .messages()
            .into_iter()
            .filter(|m| uids.contains(&m.uid))
            .map(|m| {
                let end = m.raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                RemoteHeader {
                    uid: m.uid,
                    flags: m.flags,
                    internal_date: Some(m.date),
                    raw_header: m.raw[..end].to_vec(),
                }
            })
            .collect())
    }

    async fn fetch_flags(&mut self, uids: &[u32]) -> ServerResult<Vec<(u32, Flags)>> {
        self.fake.state.lock().unwrap().full_flag_fetches += 1;
        Ok(self
            .messages()
            .into_iter()
            .filter(|m| uids.contains(&m.uid))
            .map(|m| (m.uid, m.flags))
            .collect())
    }

    async fn fetch_changed_flags(&mut self, modseq: u64) -> ServerResult<Vec<(u32, Flags)>> {
        Ok(self
            .messages()
            .into_iter()
            .filter(|m| m.modseq > modseq)
            .map(|m| (m.uid, m.flags))
            .collect())
    }

    async fn fetch_message(&mut self, uid: u32) -> ServerResult<Vec<u8>> {
        Ok(self
            .messages()
            .into_iter()
            .find(|m| m.uid == uid)
            .unwrap()
            .raw)
    }

    async fn set_flag(&mut self, uids: &[u32], flag: FlagKind, value: bool) -> ServerResult<()> {
        self.with_selected(|state, name| {
            state.modseq += 1;
            let modseq = state.modseq;
            for message in &mut state.folders.get_mut(name).unwrap().messages {
                if uids.contains(&message.uid) {
                    match flag {
                        FlagKind::Seen => message.flags.seen = value,
                        FlagKind::Flagged => message.flags.flagged = value,
                    }
                    message.modseq = modseq;
                }
            }
        });
        Ok(())
    }

    async fn move_messages(&mut self, uids: &[u32], target: &str) -> ServerResult<()> {
        self.with_selected(|state, name| {
            if state.refuse_moves || !state.folders.contains_key(target) {
                return Err(ServerError::Protocol("move refused".into()));
            }
            state.modseq += 1;
            let source = state.folders.get_mut(name).unwrap();
            let (moved, kept): (Vec<_>, Vec<_>) = source
                .messages
                .drain(..)
                .partition(|m| uids.contains(&m.uid));
            source.messages = kept;
            let target = state.folders.get_mut(target).unwrap();
            for mut message in moved {
                message.uid = target.next_uid;
                target.next_uid += 1;
                target.messages.push(message);
            }
            Ok(())
        })
    }

    async fn delete_messages(&mut self, uids: &[u32]) -> ServerResult<()> {
        self.with_selected(|state, name| {
            state
                .folders
                .get_mut(name)
                .unwrap()
                .messages
                .retain(|m| !uids.contains(&m.uid));
        });
        Ok(())
    }

    async fn save_message(&mut self, path: &str, _draft: bool, raw: &[u8]) -> ServerResult<()> {
        let mut state = self.fake.state.lock().unwrap();
        if std::mem::take(&mut state.drop_on_append) {
            return Err(ServerError::Connection("connection lost".into()));
        }
        state.modseq += 1;
        let modseq = state.modseq;
        let Some(mailbox) = state.folders.get_mut(path) else {
            return Err(ServerError::Protocol(format!("no folder {path}")));
        };
        let uid = mailbox.next_uid.max(1);
        mailbox.next_uid = uid + 1;
        mailbox.messages.push(FakeMessage {
            uid,
            date: Utc::now().timestamp(),
            flags: Flags {
                seen: true,
                flagged: false,
            },
            modseq,
            raw: raw.to_vec(),
        });
        Ok(())
    }

    async fn search_message_id(&mut self, message_id: &str) -> ServerResult<Vec<u32>> {
        Ok(self
            .messages()
            .into_iter()
            .filter(|m| {
                crate::parse::parse_header(&m.raw).message_id.as_deref() == Some(message_id)
            })
            .map(|m| m.uid)
            .collect())
    }

    async fn search_text(&mut self, words: &[String]) -> ServerResult<Vec<u32>> {
        Ok(self
            .messages()
            .into_iter()
            .filter(|m| {
                let text = String::from_utf8_lossy(&m.raw).to_lowercase();
                words.iter().all(|w| text.contains(&w.to_lowercase()))
            })
            .map(|m| m.uid)
            .collect())
    }

    async fn create_mailbox(&mut self, path: &str) -> ServerResult<()> {
        self.fake
            .state
            .lock()
            .unwrap()
            .folders
            .entry(path.to_owned())
            .or_insert(FakeMailbox {
                validity: 1,
                next_uid: 1,
                messages: Vec::new(),
            });
        Ok(())
    }

    fn saves_sent_mail(&self) -> bool {
        self.fake.state.lock().unwrap().gmail
    }

    async fn idle(&mut self, _timeout: Duration) -> ServerResult<IdleOutcome> {
        self.fake.changed.notified().await;
        Ok(IdleOutcome::NewData)
    }

    async fn logout(self) {}
}

impl Connector for Fake {
    type Server = FakeServer;

    async fn connect(&self) -> ServerResult<FakeServer> {
        if self.state.lock().unwrap().reject_login {
            return Err(ServerError::Authentication("wrong password".into()));
        }
        Ok(FakeServer {
            fake: self.clone(),
            selected: None,
        })
    }
}

/// Sender, recipients and message.
type SentMail = (String, Vec<String>, Vec<u8>);

/// Records what would have been sent.
#[derive(Default)]
struct FakeTransport {
    sent: Mutex<Vec<SentMail>>,
    fail: Mutex<Option<SendError>>,
}

impl Transport for FakeTransport {
    async fn send(&self, from: &str, recipients: &[String], raw: &[u8]) -> Result<(), SendError> {
        if let Some(error) = self.fail.lock().unwrap().clone() {
            return Err(error);
        }
        self.sent
            .lock()
            .unwrap()
            .push((from.to_owned(), recipients.to_vec(), raw.to_vec()));
        Ok(())
    }
}

fn setup() -> (SharedStore, Account) {
    let account = Account {
        id: 1,
        email: "me@example.org".into(),
        label: "Test".into(),
        display_name: "Me".into(),
        settings: ServerSettings {
            imap: ServerConfig::new("imap.example.org", 993, Security::Tls),
            smtp: ServerConfig::new("smtp.example.org", 465, Security::Tls),
            username: "me@example.org".into(),
        },
    };
    let store = Store::open_in_memory(account.id).unwrap();
    (Arc::new(Mutex::new(store)), account)
}

/// Runs one command on a fresh connection, like the worker does.
async fn run(fake: &Fake, store: &SharedStore, command: Command, backlog: &mut Backlog) {
    let transport = Arc::new(FakeTransport::default());
    let (events, _rx) = async_channel::unbounded();
    run_with(fake, &transport, store, command, &events, backlog)
        .await
        .unwrap();
}

async fn run_with(
    fake: &Fake,
    transport: &Arc<FakeTransport>,
    store: &SharedStore,
    command: Command,
    events: &Sender<SyncEvent>,
    backlog: &mut Backlog,
) -> anyhow::Result<()> {
    let mut server = fake.connect().await.unwrap();
    let outgoing = Outgoing {
        account: 1,
        sender: "me@example.org".into(),
        transport: transport.clone(),
    };
    run_command(&mut server, store, &outgoing, command, events, backlog).await
}

async fn sync_all(fake: &Fake, store: &SharedStore, backlog: &mut Backlog) {
    run(fake, store, Command::SyncAll, backlog).await;
}

async fn backfill_all(fake: &Fake, store: &SharedStore, backlog: &mut Backlog) -> usize {
    let mut server = fake.connect().await.unwrap();
    let mut steps = 0;
    while backfill_step(&mut server, store, backlog)
        .await
        .unwrap()
        .is_some()
    {
        steps += 1;
    }
    steps
}

fn folder(store: &SharedStore, role: FolderRole) -> Folder {
    lock(store).folder_by_role(role).unwrap().unwrap()
}

fn subjects(store: &SharedStore, role: FolderRole) -> Vec<String> {
    let Some(folder) = lock(store).folder_by_role(role).unwrap() else {
        return Vec::new();
    };
    lock(store)
        .messages(folder.id)
        .unwrap()
        .into_iter()
        .map(|m| m.subject)
        .collect()
}

fn message_id(store: &SharedStore, role: FolderRole, subject: &str) -> MessageId {
    let folder = folder(store, role);
    lock(store)
        .messages(folder.id)
        .unwrap()
        .into_iter()
        .find(|m| m.subject == subject)
        .unwrap()
        .id
}

#[tokio::test]
async fn first_sync_loads_last_thirty_days_and_the_rest_follows() {
    let fake = Fake::with_folders(&["INBOX", "Sent", "Projects"]);
    fake.deliver("INBOX", 1, 400, "Very old");
    fake.deliver("INBOX", 2, 40, "Old");
    fake.deliver("INBOX", 3, 3, "Older");
    fake.deliver("INBOX", 4, 1, "Newer");
    fake.deliver("Sent", 7, 2, "My reply");
    fake.deliver("Projects", 1, 90, "Plan");
    let (store, _) = setup();
    let mut backlog = Backlog::default();

    sync_all(&fake, &store, &mut backlog).await;

    assert_eq!(subjects(&store, FolderRole::Inbox), ["Newer", "Older"]);
    assert_eq!(subjects(&store, FolderRole::Sent), ["My reply"]);
    assert_eq!(backlog.remaining(), 3);
    let folders = lock(&store).folders().unwrap();
    // INBOX, Sent and Projects, plus the missing standard folders.
    assert_eq!(folders.len(), 7);
    assert_eq!(folders[0].unread, 4);
    for role in [
        FolderRole::Drafts,
        FolderRole::Archive,
        FolderRole::Junk,
        FolderRole::Trash,
    ] {
        assert!(
            folders.iter().any(|folder| folder.role == Some(role)),
            "{role:?}"
        );
    }

    // A second sync keeps what is loaded and drops nothing.
    sync_all(&fake, &store, &mut backlog).await;
    assert_eq!(subjects(&store, FolderRole::Inbox), ["Newer", "Older"]);

    assert_eq!(backfill_all(&fake, &store, &mut backlog).await, 2);
    assert!(backlog.is_empty());
    assert_eq!(
        subjects(&store, FolderRole::Inbox),
        ["Newer", "Older", "Old", "Very old"]
    );
    let projects = lock(&store).folders().unwrap()[2].id;
    assert_eq!(lock(&store).messages(projects).unwrap().len(), 1);

    // Nothing falls out with later syncs either.
    sync_all(&fake, &store, &mut backlog).await;
    assert!(backlog.is_empty());
    assert_eq!(subjects(&store, FolderRole::Inbox).len(), 4);
}

#[tokio::test]
async fn backfill_works_in_batches_newest_first() {
    let fake = Fake::with_folders(&["INBOX"]);
    for uid in 1..=450 {
        fake.deliver("INBOX", uid, 100, &format!("Old {uid}"));
    }
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;
    assert!(subjects(&store, FolderRole::Inbox).is_empty());
    assert_eq!(backlog.remaining(), 450);

    let mut server = fake.connect().await.unwrap();
    backfill_step(&mut server, &store, &mut backlog)
        .await
        .unwrap();
    let inbox = folder(&store, FolderRole::Inbox).id;
    let uids = lock(&store).uids(inbox).unwrap();
    assert_eq!(uids.len(), BACKFILL_BATCH);
    assert_eq!(uids[0], 251);
    assert_eq!(backfill_all(&fake, &store, &mut backlog).await, 2);
    assert_eq!(lock(&store).uids(inbox).unwrap().len(), 450);
}

#[tokio::test]
async fn mail_arriving_later_is_loaded_at_once_whatever_its_date() {
    let fake = Fake::with_folders(&["INBOX", "Trash"]);
    fake.deliver("INBOX", 1, 1, "Recent");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;

    // An old message moved here by another app counts as new.
    fake.deliver("Trash", 5, 300, "Thrown away");
    fake.deliver("INBOX", 2, 200, "Moved back");
    sync_all(&fake, &store, &mut backlog).await;
    assert_eq!(
        subjects(&store, FolderRole::Inbox),
        ["Recent", "Moved back"]
    );
    assert_eq!(subjects(&store, FolderRole::Trash), ["Thrown away"]);
}

#[tokio::test]
async fn resync_applies_new_deleted_and_flags() {
    let fake = Fake::with_folders(&["INBOX"]);
    fake.deliver("INBOX", 1, 1, "Stays");
    fake.deliver("INBOX", 2, 1, "Deleted");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;

    fake.mailbox("INBOX", |mailbox| {
        mailbox.messages.retain(|m| m.uid != 2);
        mailbox.messages[0].flags.flagged = true;
    });
    fake.deliver("INBOX", 3, 0, "Arrived");
    sync_all(&fake, &store, &mut backlog).await;

    assert_eq!(subjects(&store, FolderRole::Inbox), ["Arrived", "Stays"]);
    let stays = message_id(&store, FolderRole::Inbox, "Stays");
    assert!(lock(&store).message(stays).unwrap().unwrap().flags.flagged);
}

#[tokio::test]
async fn condstore_only_fetches_changed_flags() {
    let fake = Fake::with_folders(&["INBOX"]);
    fake.state.lock().unwrap().condstore = true;
    fake.deliver("INBOX", 1, 1, "One");
    fake.deliver("INBOX", 2, 1, "Two");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;
    assert_eq!(fake.state.lock().unwrap().full_flag_fetches, 0);

    // Nothing changed: the folder is skipped.
    sync_all(&fake, &store, &mut backlog).await;
    // Another app stars a message.
    {
        let mut state = fake.state.lock().unwrap();
        state.modseq += 1;
        let modseq = state.modseq;
        let message = &mut state.folders.get_mut("INBOX").unwrap().messages[1];
        message.flags.flagged = true;
        message.modseq = modseq;
    }
    sync_all(&fake, &store, &mut backlog).await;
    assert_eq!(fake.state.lock().unwrap().full_flag_fetches, 0);
    let two = message_id(&store, FolderRole::Inbox, "Two");
    assert!(lock(&store).message(two).unwrap().unwrap().flags.flagged);
}

#[tokio::test]
async fn new_uid_validity_reloads_folder() {
    let fake = Fake::with_folders(&["INBOX"]);
    fake.deliver("INBOX", 1, 1, "Before");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;

    fake.mailbox("INBOX", |mailbox| {
        mailbox.validity = 2;
        mailbox.messages[0].uid = 100;
    });
    sync_all(&fake, &store, &mut backlog).await;

    let inbox = folder(&store, FolderRole::Inbox).id;
    assert_eq!(lock(&store).uids(inbox).unwrap(), [100]);
}

#[tokio::test]
async fn opening_a_message_stores_its_body_without_marking_it_read() {
    let fake = Fake::with_folders(&["INBOX"]);
    fake.deliver("INBOX", 1, 1, "Hello");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;
    let id = message_id(&store, FolderRole::Inbox, "Hello");

    let mut server = fake.connect().await.unwrap();
    fetch_body(&mut server, &store, id).await.unwrap();

    let store = lock(&store);
    let message = store.message(id).unwrap().unwrap();
    assert!(!message.flags.seen);
    assert_eq!(message.preview, "Body of Hello");
    assert!(store.body(id).unwrap().is_some());
    assert_eq!(store.search("body").unwrap().len(), 1);
    assert!(!fake.flags("INBOX", 1).seen);
}

#[tokio::test]
async fn read_and_star_reach_the_server_and_survive_a_sync() {
    let fake = Fake::with_folders(&["INBOX"]);
    fake.deliver("INBOX", 1, 1, "Hello");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;
    let id = message_id(&store, FolderRole::Inbox, "Hello");

    lock(&store).set_seen(&[id], true).unwrap();
    lock(&store).set_flagged(&[id], true).unwrap();
    assert_eq!(folder(&store, FolderRole::Inbox).unread, 0);
    run(&fake, &store, Command::Flush, &mut backlog).await;

    let flags = fake.flags("INBOX", 1);
    assert!(flags.seen && flags.flagged);
    assert!(lock(&store).due_changes(i64::MAX).unwrap().is_empty());

    // Changes made elsewhere come back the other way.
    fake.mailbox("INBOX", |m| m.messages[0].flags.flagged = false);
    sync_all(&fake, &store, &mut backlog).await;
    assert!(!lock(&store).message(id).unwrap().unwrap().flags.flagged);
    assert_eq!(folder(&store, FolderRole::Inbox).unread, 0);
}

#[tokio::test]
async fn a_sync_does_not_undo_a_change_still_on_its_way() {
    let fake = Fake::with_folders(&["INBOX"]);
    fake.deliver("INBOX", 1, 1, "Hello");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;
    let id = message_id(&store, FolderRole::Inbox, "Hello");
    let inbox = folder(&store, FolderRole::Inbox);

    lock(&store).set_flagged(&[id], true).unwrap();
    let mut server = fake.connect().await.unwrap();
    let (events, _rx) = async_channel::unbounded();
    sync_folder(
        &mut server,
        &store,
        &inbox,
        sync_since(),
        &events,
        &mut backlog,
    )
    .await
    .unwrap();
    assert!(lock(&store).message(id).unwrap().unwrap().flags.flagged);
}

#[tokio::test]
async fn deleting_waits_for_undo_then_moves_to_trash() {
    let fake = Fake::with_folders(&["INBOX", "Trash"]);
    fake.deliver("INBOX", 1, 1, "Offer");
    fake.deliver("INBOX", 2, 1, "Keep");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;
    let offer = message_id(&store, FolderRole::Inbox, "Offer");
    let keep = message_id(&store, FolderRole::Inbox, "Keep");

    // Undone in time: nothing reaches the server.
    let later = Utc::now().timestamp() + 60;
    let undo = lock(&store).delete_messages(&[offer], later).unwrap().undo;
    run(&fake, &store, Command::Flush, &mut backlog).await;
    assert_eq!(fake.subjects_on_server("INBOX"), ["Offer", "Keep"]);
    assert_eq!(lock(&store).cancel_changes(&undo).unwrap(), 1);
    assert_eq!(subjects(&store, FolderRole::Inbox).len(), 2);

    // Not undone: the move happens once it is due.
    lock(&store).delete_messages(&[keep], 0).unwrap();
    assert_eq!(subjects(&store, FolderRole::Inbox), ["Offer"]);
    run(&fake, &store, Command::Flush, &mut backlog).await;
    assert_eq!(fake.subjects_on_server("INBOX"), ["Offer"]);
    assert_eq!(fake.subjects_on_server("Trash"), ["Keep"]);
    assert_eq!(subjects(&store, FolderRole::Trash), ["Keep"]);

    // In the trash, deleting is final.
    let trashed = message_id(&store, FolderRole::Trash, "Keep");
    let deletion = lock(&store).delete_messages(&[trashed], 0).unwrap();
    assert_eq!(deletion.permanent, 1);
    run(&fake, &store, Command::Flush, &mut backlog).await;
    assert!(fake.subjects_on_server("Trash").is_empty());
    assert!(subjects(&store, FolderRole::Trash).is_empty());
}

#[tokio::test]
async fn moving_to_another_folder_and_refusals() {
    let fake = Fake::with_folders(&["INBOX", "Projects"]);
    fake.deliver("INBOX", 1, 1, "Plan");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;
    let plan = message_id(&store, FolderRole::Inbox, "Plan");
    let projects = lock(&store)
        .folders()
        .unwrap()
        .into_iter()
        .find(|folder| folder.path == "Projects")
        .unwrap()
        .id;

    fake.state.lock().unwrap().refuse_moves = true;
    lock(&store).move_messages(&[plan], projects, 0).unwrap();
    let mut server = fake.connect().await.unwrap();
    let flushed = flush_changes(&mut server, &store).await.unwrap();
    assert_eq!(flushed.failed, 1);
    assert_eq!(subjects(&store, FolderRole::Inbox), ["Plan"]);

    fake.state.lock().unwrap().refuse_moves = false;
    lock(&store).move_messages(&[plan], projects, 0).unwrap();
    run(&fake, &store, Command::Flush, &mut backlog).await;
    assert!(subjects(&store, FolderRole::Inbox).is_empty());
    assert_eq!(fake.subjects_on_server("Projects"), ["Plan"]);
    let moved = lock(&store).messages(projects).unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].subject, "Plan");
}

#[tokio::test]
async fn conversations_span_inbox_and_sent() {
    let fake = Fake::with_folders(&["INBOX", "Sent"]);
    fake.deliver_raw("INBOX", 1, 3, "Bike", "Message-ID: <q@x>\r\n");
    fake.deliver_raw(
        "Sent",
        1,
        2,
        "Re: Bike",
        "Message-ID: <a@x>\r\nIn-Reply-To: <q@x>\r\n",
    );
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;
    let inbox = folder(&store, FolderRole::Inbox).id;
    let threads = lock(&store).threads(inbox).unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].count, 2);
}

async fn wait_for(events: &Receiver<SyncEvent>, wanted: impl Fn(&SyncEvent) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if wanted(&event) {
                return;
            }
        }
    })
    .await
    .expect("event did not arrive in time");
}

#[tokio::test]
async fn new_mail_arrives_through_idle() {
    let fake = Fake::with_folders(&["INBOX"]);
    fake.deliver("INBOX", 1, 1, "First");
    let (store, account) = setup();
    let (events, receiver) = async_channel::unbounded();

    let _handle = start_account(
        &tokio::runtime::Handle::current(),
        account.clone(),
        Arc::new(fake.clone()),
        Arc::new(FakeTransport::default()),
        store.clone(),
        events,
    );
    wait_for(&receiver, |e| matches!(e, SyncEvent::Synced { .. })).await;
    assert_eq!(subjects(&store, FolderRole::Inbox), ["First"]);

    // Give the watcher time to enter IDLE before the mail arrives.
    tokio::time::sleep(Duration::from_millis(200)).await;
    fake.deliver("INBOX", 2, 0, "Second");
    tokio::time::timeout(Duration::from_secs(5), async {
        while subjects(&store, FolderRole::Inbox) != ["Second", "First"] {
            let _ = receiver.recv().await;
        }
    })
    .await
    .expect("new mail did not arrive in time");
}

#[tokio::test]
async fn the_worker_loads_older_mail_and_flushes_changes_by_itself() {
    let fake = Fake::with_folders(&["INBOX", "Trash"]);
    fake.deliver("INBOX", 1, 300, "Ancient");
    fake.deliver("INBOX", 2, 1, "Recent");
    let (store, account) = setup();
    let (events, receiver) = async_channel::unbounded();
    let handle = start_account(
        &tokio::runtime::Handle::current(),
        account,
        Arc::new(fake.clone()),
        Arc::new(FakeTransport::default()),
        store.clone(),
        events,
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while subjects(&store, FolderRole::Inbox).len() < 2 {
            let _ = receiver.recv().await;
        }
    })
    .await
    .expect("older mail was not loaded in the background");

    let recent = message_id(&store, FolderRole::Inbox, "Recent");
    lock(&store).delete_messages(&[recent], 0).unwrap();
    handle.send(Command::Flush);
    tokio::time::timeout(Duration::from_secs(5), async {
        while fake.subjects_on_server("Trash").is_empty() {
            let _ = receiver.recv().await;
        }
    })
    .await
    .expect("the deletion did not reach the server");
}

#[tokio::test]
async fn wrong_password_is_reported() {
    let fake = Fake::with_folders(&["INBOX"]);
    fake.state.lock().unwrap().reject_login = true;
    let (store, account) = setup();
    let (events, receiver) = async_channel::unbounded();
    let _handle = start_account(
        &tokio::runtime::Handle::current(),
        account,
        Arc::new(fake),
        Arc::new(FakeTransport::default()),
        store,
        events,
    );
    wait_for(&receiver, |e| {
        matches!(
            e,
            SyncEvent::Status {
                status: AccountStatus::LoginFailed(_),
                ..
            }
        )
    })
    .await;
}

mod outbox {
    use super::*;
    use crate::compose::{self, Draft, Mailbox};

    fn me() -> Mailbox {
        Mailbox::new("Me", "me@example.org")
    }

    fn draft(subject: &str, text: &str) -> Draft {
        Draft {
            message_id: "draft-1@example.org".into(),
            to: vec![Mailbox::new("Lena", "lena@example.org")],
            bcc: vec![Mailbox::new("", "hidden@example.org")],
            subject: subject.into(),
            html: format!("<div>{text}</div>"),
            ..Draft::default()
        }
    }

    fn queue_draft(store: &SharedStore, draft: &Draft) {
        let raw = compose::build_message(&me(), draft, Utc::now().timestamp());
        lock(store).queue_draft(&draft.message_id, &raw).unwrap();
    }

    fn queue_send(store: &SharedStore, draft: &Draft) {
        let raw = compose::build_message(&me(), draft, Utc::now().timestamp());
        lock(store)
            .queue_send(&draft.message_id, &raw, &draft.recipients())
            .unwrap();
    }

    async fn flush(
        fake: &Fake,
        transport: &Arc<FakeTransport>,
        store: &SharedStore,
    ) -> Vec<SyncEvent> {
        let (events, receiver) = async_channel::unbounded();
        run_with(
            fake,
            transport,
            store,
            Command::Flush,
            &events,
            &mut Backlog::default(),
        )
        .await
        .unwrap();
        drop(events);
        let mut all = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            all.push(event);
        }
        all
    }

    fn sent_events(events: &[SyncEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, SyncEvent::Sent { .. }))
            .count()
    }

    #[tokio::test]
    async fn a_newer_draft_replaces_the_older_one() {
        let fake = Fake::with_folders(&["INBOX"]);
        let (store, _) = setup();
        let transport = Arc::new(FakeTransport::default());
        sync_all(&fake, &store, &mut Backlog::default()).await;

        queue_draft(&store, &draft("Plan", "first"));
        flush(&fake, &transport, &store).await;
        assert_eq!(fake.subjects_on_server("Drafts"), ["Plan"]);
        assert_eq!(subjects(&store, FolderRole::Drafts), ["Plan"]);

        // Two saves while offline: only the last one reaches the server.
        queue_draft(&store, &draft("Plan v2", "second"));
        queue_draft(&store, &draft("Plan v3", "third"));
        flush(&fake, &transport, &store).await;
        assert_eq!(fake.subjects_on_server("Drafts"), ["Plan v3"]);
        assert_eq!(subjects(&store, FolderRole::Drafts), ["Plan v3"]);
        assert!(lock(&store).outbox(i64::MAX).unwrap().is_empty());
    }

    #[tokio::test]
    async fn sending_files_a_copy_and_removes_the_draft() {
        let fake = Fake::with_folders(&["INBOX", "Sent"]);
        let (store, _) = setup();
        let transport = Arc::new(FakeTransport::default());
        sync_all(&fake, &store, &mut Backlog::default()).await;
        queue_draft(&store, &draft("Hello", "draft"));
        flush(&fake, &transport, &store).await;

        queue_send(&store, &draft("Hello", "final"));
        // The draft leaves the list at once.
        assert!(subjects(&store, FolderRole::Drafts).is_empty());
        let events = flush(&fake, &transport, &store).await;
        assert_eq!(sent_events(&events), 1);

        let sent = transport.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        let (from, recipients, wire) = &sent[0];
        assert_eq!(from, "me@example.org");
        assert_eq!(recipients, &["lena@example.org", "hidden@example.org"]);
        let wire = String::from_utf8_lossy(wire);
        assert!(!wire.contains("hidden@example.org"), "{wire}");

        assert!(fake.subjects_on_server("Drafts").is_empty());
        assert_eq!(fake.subjects_on_server("Sent"), ["Hello"]);
        let copy = fake.mailbox("Sent", |m| m.messages[0].raw.clone());
        assert!(String::from_utf8_lossy(&copy).contains("hidden@example.org"));
        assert_eq!(subjects(&store, FolderRole::Sent), ["Hello"]);
        assert!(lock(&store).outbox(i64::MAX).unwrap().is_empty());
    }

    #[tokio::test]
    async fn gmail_gets_no_second_copy() {
        let fake = Fake::with_folders(&["INBOX", "Sent"]);
        fake.state.lock().unwrap().gmail = true;
        let (store, _) = setup();
        let transport = Arc::new(FakeTransport::default());
        sync_all(&fake, &store, &mut Backlog::default()).await;
        queue_send(&store, &draft("Hello", "text"));
        flush(&fake, &transport, &store).await;
        assert_eq!(transport.sent.lock().unwrap().len(), 1);
        assert!(fake.subjects_on_server("Sent").is_empty());
    }

    #[tokio::test]
    async fn offline_mail_waits_and_is_reported_once() {
        let fake = Fake::with_folders(&["INBOX", "Sent"]);
        let (store, _) = setup();
        let transport = Arc::new(FakeTransport::default());
        *transport.fail.lock().unwrap() = Some(SendError::Unreachable("no route".into()));
        sync_all(&fake, &store, &mut Backlog::default()).await;
        queue_send(&store, &draft("Later", "text"));

        let events = flush(&fake, &transport, &store).await;
        assert!(events.iter().any(|e| matches!(
            e,
            SyncEvent::SendFailed {
                error: SendError::Unreachable(_),
                ..
            }
        )));
        assert_eq!(lock(&store).unsent().unwrap(), 1);
        // Not due again yet.
        assert!(
            lock(&store)
                .outbox(Utc::now().timestamp())
                .unwrap()
                .is_empty()
        );

        // The same problem is not reported again.
        lock(&store).retry_outbox_now().unwrap();
        let events = flush(&fake, &transport, &store).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SyncEvent::SendFailed { .. }))
        );

        *transport.fail.lock().unwrap() = None;
        lock(&store).retry_outbox_now().unwrap();
        let events = flush(&fake, &transport, &store).await;
        assert_eq!(sent_events(&events), 1);
        assert_eq!(lock(&store).unsent().unwrap(), 0);
        assert_eq!(fake.subjects_on_server("Sent"), ["Later"]);
    }

    #[tokio::test]
    async fn a_refused_message_becomes_a_draft_again() {
        let fake = Fake::with_folders(&["INBOX", "Drafts"]);
        let (store, _) = setup();
        let transport = Arc::new(FakeTransport::default());
        *transport.fail.lock().unwrap() = Some(SendError::Rejected("550 no such user".into()));
        sync_all(&fake, &store, &mut Backlog::default()).await;
        queue_send(&store, &draft("Typo", "text"));
        let events = flush(&fake, &transport, &store).await;
        assert!(events.iter().any(|e| matches!(
            e,
            SyncEvent::SendFailed {
                error: SendError::Rejected(_),
                ..
            }
        )));
        assert_eq!(fake.subjects_on_server("Drafts"), ["Typo"]);
        assert_eq!(lock(&store).unsent().unwrap(), 0);
        assert!(lock(&store).outbox(i64::MAX).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_lost_connection_after_sending_does_not_send_twice() {
        let fake = Fake::with_folders(&["INBOX", "Sent"]);
        let (store, _) = setup();
        let transport = Arc::new(FakeTransport::default());
        sync_all(&fake, &store, &mut Backlog::default()).await;
        queue_send(&store, &draft("Once", "text"));
        fake.state.lock().unwrap().drop_on_append = true;
        let (events, _rx) = async_channel::unbounded();
        let result = run_with(
            &fake,
            &transport,
            &store,
            Command::Flush,
            &events,
            &mut Backlog::default(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(transport.sent.lock().unwrap().len(), 1);

        flush(&fake, &transport, &store).await;
        assert_eq!(transport.sent.lock().unwrap().len(), 1);
        assert_eq!(fake.subjects_on_server("Sent"), ["Once"]);
        assert!(lock(&store).outbox(i64::MAX).unwrap().is_empty());
    }

    #[tokio::test]
    async fn discarding_removes_every_version() {
        let fake = Fake::with_folders(&["INBOX", "Drafts"]);
        let (store, _) = setup();
        let transport = Arc::new(FakeTransport::default());
        sync_all(&fake, &store, &mut Backlog::default()).await;
        queue_draft(&store, &draft("Reply", "text"));
        flush(&fake, &transport, &store).await;
        assert_eq!(subjects(&store, FolderRole::Drafts), ["Reply"]);

        lock(&store).queue_discard("draft-1@example.org").unwrap();
        assert!(subjects(&store, FolderRole::Drafts).is_empty());
        flush(&fake, &transport, &store).await;
        assert!(fake.subjects_on_server("Drafts").is_empty());
        assert!(lock(&store).outbox(i64::MAX).unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_worker_sends_by_itself() {
        let fake = Fake::with_folders(&["INBOX", "Sent"]);
        let (store, account) = setup();
        let transport = Arc::new(FakeTransport::default());
        let (events, receiver) = async_channel::unbounded();
        let handle = start_account(
            &tokio::runtime::Handle::current(),
            account,
            Arc::new(fake.clone()),
            transport.clone(),
            store.clone(),
            events,
        );
        wait_for(&receiver, |e| matches!(e, SyncEvent::Synced { .. })).await;
        queue_send(&store, &draft("Worker", "text"));
        handle.send(Command::Flush);
        wait_for(&receiver, |e| matches!(e, SyncEvent::Sent { .. })).await;
        assert_eq!(transport.sent.lock().unwrap().len(), 1);
    }
}

mod search {
    use super::*;
    use crate::search::{SearchQuery, server_search};

    #[tokio::test]
    async fn the_server_finds_text_and_older_mail_that_is_not_here_yet() {
        let fake = Fake::with_folders(&["INBOX", "Trash"]);
        fake.deliver("INBOX", 1, 1, "Recent invoice");
        fake.deliver("INBOX", 2, 400, "Old invoice");
        fake.deliver("INBOX", 3, 2, "Lunch");
        fake.deliver("Trash", 1, 1, "Deleted invoice");
        let (store, _) = setup();
        sync_all(&fake, &store, &mut Backlog::default()).await;
        // Only the last 30 days are here, and no message text.
        assert_eq!(subjects(&store, FolderRole::Inbox).len(), 2);

        let query = SearchQuery {
            text: "body of".into(),
            ..SearchQuery::default()
        };
        let local = lock(&store).search_threads(&query, &[], true).unwrap();
        assert!(local.is_empty());

        let folders = lock(&store).folders_to_search_remotely(None).unwrap();
        let paths: Vec<&str> = folders.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["INBOX"]);
        let mut server = fake.connect().await.unwrap();
        let query = SearchQuery {
            text: "INVOICE".into(),
            ..SearchQuery::default()
        };
        let found = server_search(&mut server, &store, &folders, &query.text)
            .await
            .unwrap();
        assert_eq!(found.len(), 2);
        let results = lock(&store).search_threads(&query, &found, true).unwrap();
        let subjects: Vec<&str> = results.iter().map(|r| r.thread.subject.as_str()).collect();
        assert_eq!(subjects, ["Recent invoice", "Old invoice"]);
        // The older message was loaded and now belongs to the folder.
        assert_eq!(subjects_in(&store, FolderRole::Inbox).len(), 3);
    }

    fn subjects_in(store: &SharedStore, role: FolderRole) -> Vec<String> {
        subjects(store, role)
    }
}

mod new_mail {
    use super::*;

    async fn sync_events(fake: &Fake, store: &SharedStore) -> Vec<SyncEvent> {
        let transport = Arc::new(FakeTransport::default());
        let (events, receiver) = async_channel::unbounded();
        run_with(
            fake,
            &transport,
            store,
            Command::SyncAll,
            &events,
            &mut Backlog::default(),
        )
        .await
        .unwrap();
        drop(events);
        let mut all = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            all.push(event);
        }
        all
    }

    fn new_mail(events: &[SyncEvent]) -> Vec<MessageId> {
        events
            .iter()
            .filter_map(|e| match e {
                SyncEvent::NewMail { messages, .. } => Some(messages.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[tokio::test]
    async fn only_inbox_arrivals_after_the_first_sync_count() {
        let fake = Fake::with_folders(&["INBOX", "Archive"]);
        fake.deliver("INBOX", 1, 1, "Already there");
        let (store, _) = setup();
        assert!(new_mail(&sync_events(&fake, &store).await).is_empty());

        fake.deliver("INBOX", 2, 0, "Fresh");
        fake.deliver("Archive", 1, 0, "Filed by a rule");
        let events = sync_events(&fake, &store).await;
        let ids = new_mail(&events);
        assert_eq!(ids, [message_id(&store, FolderRole::Inbox, "Fresh")]);

        assert!(new_mail(&sync_events(&fake, &store).await).is_empty());
    }

    #[tokio::test]
    async fn a_select_without_uidvalidity_keeps_the_folder_and_announces_new_mail() {
        let fake = Fake::with_folders(&["INBOX"]);
        fake.deliver("INBOX", 1, 1, "Already there");
        let (store, _) = setup();
        sync_events(&fake, &store).await;
        let kept = message_id(&store, FolderRole::Inbox, "Already there");

        fake.state.lock().unwrap().validity_missing = 1;
        fake.deliver("INBOX", 2, 0, "While the answer was garbled");
        let first = sync_events(&fake, &store).await;
        fake.deliver("INBOX", 3, 0, "Afterwards");
        let second = sync_events(&fake, &store).await;

        assert_eq!(message_id(&store, FolderRole::Inbox, "Already there"), kept);
        assert_eq!(
            new_mail(&first),
            [message_id(
                &store,
                FolderRole::Inbox,
                "While the answer was garbled"
            )]
        );
        assert_eq!(
            new_mail(&second),
            [message_id(&store, FolderRole::Inbox, "Afterwards")]
        );
    }

    #[tokio::test]
    async fn a_lasting_incomplete_answer_changes_nothing_and_mail_is_announced_later() {
        let fake = Fake::with_folders(&["INBOX"]);
        fake.deliver("INBOX", 1, 1, "Already there");
        let (store, _) = setup();
        sync_events(&fake, &store).await;
        let kept = message_id(&store, FolderRole::Inbox, "Already there");

        fake.state.lock().unwrap().validity_missing = 100;
        fake.deliver("INBOX", 2, 0, "During the hiccup");
        let transport = Arc::new(FakeTransport::default());
        let (events, _receiver) = async_channel::unbounded();
        let result = run_with(
            &fake,
            &transport,
            &store,
            Command::SyncAll,
            &events,
            &mut Backlog::default(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(message_id(&store, FolderRole::Inbox, "Already there"), kept);

        fake.state.lock().unwrap().validity_missing = 0;
        let later = sync_events(&fake, &store).await;
        assert_eq!(
            new_mail(&later),
            [message_id(&store, FolderRole::Inbox, "During the hiccup")]
        );
    }

    #[tokio::test]
    async fn a_uidvalidity_seen_only_once_is_not_believed() {
        let fake = Fake::with_folders(&["INBOX"]);
        fake.deliver("INBOX", 1, 1, "Already there");
        let (store, _) = setup();
        sync_events(&fake, &store).await;
        let kept = message_id(&store, FolderRole::Inbox, "Already there");

        fake.state.lock().unwrap().validity_once = Some(7);
        fake.deliver("INBOX", 2, 0, "Fresh");
        let events = sync_events(&fake, &store).await;

        assert_eq!(message_id(&store, FolderRole::Inbox, "Already there"), kept);
        assert_eq!(
            new_mail(&events),
            [message_id(&store, FolderRole::Inbox, "Fresh")]
        );
    }
}

#[tokio::test]
async fn mail_from_spam_senders_goes_to_spam_by_itself() {
    let fake = Fake::with_folders(&["INBOX", "Spam"]);
    fake.deliver("INBOX", 1, 1, "First");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;
    lock(&store)
        .mark_spam_senders(&["ANNA@example.org".into()])
        .unwrap();

    fake.deliver("INBOX", 2, 0, "Buy now");
    let transport = Arc::new(FakeTransport::default());
    let (events, receiver) = async_channel::unbounded();
    run_with(
        &fake,
        &transport,
        &store,
        Command::SyncAll,
        &events,
        &mut backlog,
    )
    .await
    .unwrap();
    drop(events);
    while let Ok(event) = receiver.try_recv() {
        assert!(
            !matches!(event, SyncEvent::NewMail { .. }),
            "spam is not announced"
        );
    }
    assert_eq!(subjects(&store, FolderRole::Junk), ["Buy now"]);
    run(&fake, &store, Command::Flush, &mut backlog).await;
    assert_eq!(fake.subjects_on_server("Spam"), ["Buy now"]);
    assert_eq!(fake.subjects_on_server("INBOX"), ["First"]);

    lock(&store)
        .unmark_spam_senders(&["anna@example.org".into()])
        .unwrap();
    fake.deliver("INBOX", 3, 0, "Hello again");
    run(&fake, &store, Command::SyncAll, &mut backlog).await;
    assert_eq!(
        subjects(&store, FolderRole::Inbox),
        ["Hello again", "First"]
    );
}

#[tokio::test]
async fn an_empty_folder_list_never_wipes_the_folders() {
    let fake = Fake::with_folders(&["INBOX"]);
    fake.deliver("INBOX", 1, 1, "Kept");
    let (store, _) = setup();
    let mut backlog = Backlog::default();
    sync_all(&fake, &store, &mut backlog).await;

    // A new folder that the server does not list right away.
    fake.state.lock().unwrap().empty_list_once = true;
    let draft = compose::Draft {
        message_id: "d@example.org".into(),
        subject: "Draft".into(),
        ..compose::Draft::default()
    };
    let raw = compose::build_message(&compose::Mailbox::new("Me", "me@example.org"), &draft, 0);
    lock(&store).queue_draft(&draft.message_id, &raw).unwrap();
    run(&fake, &store, Command::Flush, &mut backlog).await;

    assert_eq!(subjects(&store, FolderRole::Inbox), ["Kept"]);
    assert_eq!(fake.subjects_on_server("Drafts"), ["Draft"]);
    assert!(
        lock(&store)
            .folder_by_role(FolderRole::Drafts)
            .unwrap()
            .is_some()
    );
    assert!(lock(&store).outbox(i64::MAX).unwrap().is_empty());
}
