// SPDX-License-Identifier: GPL-3.0-or-later

//! Runs against a local GreenMail server, see `build-aux/test-mail-server.sh`.
//! Ignored by default; CI runs it with `cargo test -- --ignored`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use e_mail_core::accounts::{Accounts, store_path};
use e_mail_core::check::{self, Advice, CheckError, Stage};
use e_mail_core::discovery::providers::{ProviderId, provider};
use e_mail_core::folder::FolderRole;
use e_mail_core::imap::{ImapConnector, ImapServer};
use e_mail_core::model::{Folder, NewAccount, Security, ServerConfig, ServerSettings};
use e_mail_core::server::{Connector, MailServer};
use e_mail_core::store::Store;
use e_mail_core::sync::{self, Backlog, SharedStore};
use e_mail_core::tls::Trust;
use lettre::AsyncTransport;
use lettre::address::{Address, Envelope};

const EMAIL: &str = "alice@example.test";
const PASSWORD: &str = "secret";

fn trust() -> Trust {
    let path = std::env::var("E_MAIL_TEST_CA").expect("E_MAIL_TEST_CA points to the test CA (DER)");
    Trust::only(vec![std::fs::read(path).expect("readable test CA")])
}

fn settings() -> ServerSettings {
    ServerSettings {
        imap: ServerConfig::new("localhost", 3993, Security::Tls),
        smtp: ServerConfig::new("localhost", 3465, Security::Tls),
        username: EMAIL.into(),
    }
}

#[tokio::test]
#[ignore = "needs the local test mail server"]
async fn set_up_account_and_load_folders() {
    let trust = trust();
    let settings = settings();

    let mut stages = Vec::new();
    check::check_account(&settings, PASSWORD, &trust, |stage| stages.push(stage))
        .await
        .expect("the check succeeds with the right password");
    assert_eq!(stages, [Stage::SignIn, Stage::OpenMailbox]);

    let transport = check::smtp_transport(&settings.smtp, EMAIL, PASSWORD, &trust).unwrap();
    let address: Address = EMAIL.parse().unwrap();
    let envelope = Envelope::new(Some(address.clone()), vec![address]).unwrap();
    let raw = format!(
        "From: Alice <{EMAIL}>\r\nTo: {EMAIL}\r\nSubject: Hello from the test\r\n\
         Date: {}\r\n\r\nJust a test.\r\n",
        chrono::Utc::now().to_rfc2822()
    );
    transport
        .send_raw(&envelope, raw.as_bytes())
        .await
        .expect("sending through SMTP works");

    let dir = std::env::temp_dir().join(format!("e-mail-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let accounts = Accounts::open(&dir.join("accounts.db")).unwrap();
    let account = accounts
        .add(&NewAccount {
            email: EMAIL.into(),
            label: "Test".into(),
            display_name: "Alice".into(),
            settings: settings.clone(),
        })
        .unwrap();
    let store = Arc::new(Mutex::new(
        Store::open(&store_path(&dir, account.id), account.id).unwrap(),
    ));

    let connector = ImapConnector {
        server: settings.imap.clone(),
        username: settings.username.clone(),
        password: PASSWORD.into(),
        trust: trust.clone(),
    };
    let (events, _receiver) = async_channel::unbounded();
    let mut subjects = Vec::new();
    for _ in 0..20 {
        let mut server = connector.connect().await.unwrap();
        let folders = sync::sync_folder_list(&mut server, &store).await.unwrap();
        let inbox = folders
            .iter()
            .find(|folder| folder.role == Some(FolderRole::Inbox))
            .expect("the inbox is loaded")
            .clone();
        sync::sync_folder(
            &mut server,
            &store,
            &inbox,
            sync::sync_since(),
            &events,
            &mut sync::Backlog::default(),
        )
        .await
        .unwrap();
        server.logout().await;
        subjects = store
            .lock()
            .unwrap()
            .messages(inbox.id)
            .unwrap()
            .into_iter()
            .map(|message| message.subject)
            .collect();
        if !subjects.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(subjects, ["Hello from the test"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
#[ignore = "needs the local test mail server"]
async fn wrong_password_gives_the_right_advice() {
    let error = check::check_account(&settings(), "wrong", &trust(), |_| {})
        .await
        .unwrap_err();
    assert!(matches!(error, CheckError::ImapLogin(_)), "{error:?}");
    assert_eq!(check::advice(&error, None), Advice::CheckPassword);
    assert_eq!(
        check::advice(&error, Some(provider(ProviderId::Gmail))),
        Advice::UseAppPassword
    );
    assert_eq!(
        check::advice(&error, Some(provider(ProviderId::WebDe))),
        Advice::CheckPasswordAndAccess
    );

    let error = check::check_imap_login(&settings(), "wrong", &trust())
        .await
        .unwrap_err();
    assert!(error.is_login());
}

fn connector() -> ImapConnector {
    let settings = settings();
    ImapConnector {
        server: settings.imap,
        username: settings.username,
        password: PASSWORD.into(),
        trust: trust(),
    }
}

/// Every test works in folders of its own, as all tests share one mailbox.
async fn fresh_folders(server: &mut ImapServer, test: &str, names: &[&str]) -> Vec<String> {
    let mut paths = Vec::new();
    for name in names {
        let path = format!("{test}-{}-{name}", std::process::id());
        server.create_folder(&path).await.unwrap();
        paths.push(path);
    }
    paths
}

fn message(subject: &str, date: &str) -> Vec<u8> {
    format!(
        "From: Bob <bob@example.test>\r\nTo: {EMAIL}\r\nSubject: {subject}\r\n\
         Message-ID: <{subject}-{}@example.test>\r\nDate: {date}\r\n\r\nText of {subject}\r\n",
        std::process::id()
    )
    .into_bytes()
}

fn memory_store() -> SharedStore {
    Arc::new(Mutex::new(Store::open_in_memory(1).unwrap()))
}

async fn local_folder(server: &mut ImapServer, store: &SharedStore, path: &str) -> Folder {
    sync::sync_folder_list(server, store)
        .await
        .unwrap()
        .into_iter()
        .find(|folder| folder.path == path)
        .expect("the new folder is listed")
}

fn subjects(store: &SharedStore, folder: &Folder) -> Vec<String> {
    store
        .lock()
        .unwrap()
        .messages(folder.id)
        .unwrap()
        .into_iter()
        .map(|m| m.subject)
        .collect()
}

#[tokio::test]
#[ignore = "needs the local test mail server"]
async fn older_mail_is_loaded_in_the_background() {
    let mut server = connector().connect().await.unwrap();
    let paths = fresh_folders(&mut server, "backfill", &["old"]).await;
    let now = chrono::Utc::now();
    for (subject, days) in [("Recent", 1), ("Last year", 400), ("Ancient", 3000)] {
        let date = now - chrono::Duration::days(days);
        let internal = date.format("%d-%b-%Y %H:%M:%S +0000").to_string();
        server
            .append(
                &paths[0],
                None,
                Some(&internal),
                &message(subject, &date.to_rfc2822()),
            )
            .await
            .unwrap();
    }

    let store = memory_store();
    let folder = local_folder(&mut server, &store, &paths[0]).await;
    let (events, _receiver) = async_channel::unbounded();
    let mut backlog = Backlog::default();
    // Pretend the first sync happens far in the past, so only mail from the
    // last days counts as recent no matter how the server dates the appends.
    let since = (now + chrono::Duration::days(1)).date_naive();
    sync::sync_folder(&mut server, &store, &folder, since, &events, &mut backlog)
        .await
        .unwrap();
    assert!(subjects(&store, &folder).is_empty());
    assert_eq!(backlog.remaining(), 3);

    while sync::backfill_step(&mut server, &store, &mut backlog)
        .await
        .unwrap()
        .is_some()
    {}
    assert_eq!(
        subjects(&store, &folder),
        ["Recent", "Last year", "Ancient"]
    );

    // Later syncs keep everything.
    sync::sync_folder(
        &mut server,
        &store,
        &folder,
        sync::sync_since(),
        &events,
        &mut backlog,
    )
    .await
    .unwrap();
    assert_eq!(subjects(&store, &folder).len(), 3);
    server.logout().await;
}

#[tokio::test]
#[ignore = "needs the local test mail server"]
async fn read_star_move_and_delete_reach_the_server() {
    let mut server = connector().connect().await.unwrap();
    let paths = fresh_folders(&mut server, "changes", &["from", "to"]).await;
    let date = chrono::Utc::now().to_rfc2822();
    for subject in ["Keep", "Move"] {
        server
            .append(&paths[0], None, None, &message(subject, &date))
            .await
            .unwrap();
    }

    let store = memory_store();
    let from = local_folder(&mut server, &store, &paths[0]).await;
    let to = local_folder(&mut server, &store, &paths[1]).await;
    let (events, _receiver) = async_channel::unbounded();
    let mut backlog = Backlog::default();
    for folder in [&from, &to] {
        sync::sync_folder(
            &mut server,
            &store,
            folder,
            sync::sync_since(),
            &events,
            &mut backlog,
        )
        .await
        .unwrap();
    }
    let id = |subject: &str| {
        store
            .lock()
            .unwrap()
            .messages(from.id)
            .unwrap()
            .into_iter()
            .find(|m| m.subject == subject)
            .unwrap()
            .id
    };
    let (keep, moving) = (id("Keep"), id("Move"));
    {
        let mut store = store.lock().unwrap();
        store.set_seen(&[keep], true).unwrap();
        store.set_flagged(&[keep], true).unwrap();
        store.move_messages(&[moving], to.id, 0).unwrap();
    }
    let flushed = sync::flush_changes(&mut server, &store).await.unwrap();
    assert_eq!(flushed.failed, 0);
    assert!(flushed.targets.contains(&to.id));

    // Look at the server through a second connection.
    let mut check = connector().connect().await.unwrap();
    check.select(&paths[0]).await.unwrap();
    let uids = check.search_all().await.unwrap();
    assert_eq!(uids.len(), 1);
    let flags = check.fetch_flags(&uids).await.unwrap();
    assert!(flags[0].1.seen && flags[0].1.flagged);
    check.select(&paths[1]).await.unwrap();
    assert_eq!(check.search_all().await.unwrap().len(), 1);

    // The moved message shows up in its new folder, and deleting it puts it
    // into the trash, which the app created during the folder sync.
    sync::sync_folder(
        &mut server,
        &store,
        &to,
        sync::sync_since(),
        &events,
        &mut backlog,
    )
    .await
    .unwrap();
    assert_eq!(subjects(&store, &to), ["Move"]);
    assert_eq!(subjects(&store, &from), ["Keep"]);
    let moved = store.lock().unwrap().messages(to.id).unwrap()[0].id;
    let deletion = store.lock().unwrap().delete_messages(&[moved], 0).unwrap();
    assert_eq!(deletion.permanent, 0);
    sync::flush_changes(&mut server, &store).await.unwrap();
    check.select(&paths[1]).await.unwrap();
    assert!(check.search_all().await.unwrap().is_empty());

    // A flag changed by another app arrives here too.
    check.select(&paths[0]).await.unwrap();
    check
        .set_flag(&uids, e_mail_core::server::FlagKind::Flagged, false)
        .await
        .unwrap();
    sync::sync_folder(
        &mut server,
        &store,
        &from,
        sync::sync_since(),
        &events,
        &mut backlog,
    )
    .await
    .unwrap();
    let keep = store.lock().unwrap().message(keep).unwrap().unwrap();
    assert!(keep.flags.seen && !keep.flags.flagged);
    check.logout().await;
    server.logout().await;
}

#[tokio::test]
#[ignore = "needs the local test mail server"]
async fn drafts_are_replaced_and_sent_mail_is_filed() {
    use e_mail_core::compose::{self, Draft, Mailbox};
    use e_mail_core::smtp::SmtpTransport;

    let store = memory_store();
    let mut server = connector().connect().await.unwrap();
    sync::sync_folder_list(&mut server, &store).await.unwrap();
    let transport = SmtpTransport {
        server: settings().smtp,
        username: EMAIL.into(),
        password: PASSWORD.into(),
        trust: trust(),
    };
    let (events, receiver) = async_channel::unbounded();
    let me = Mailbox::new("Alice", EMAIL);
    let subject = format!("Draft test {}", std::process::id());
    let mut draft = Draft {
        message_id: compose::new_message_id(EMAIL),
        to: vec![Mailbox::new("Bob", "bob@example.test")],
        subject: subject.clone(),
        html: format!(
            "<div>First <b>version</b></div><img src=\"{}\">",
            compose::data_uri("image/png", b"\x89PNG picture")
        ),
        ..Draft::default()
    };
    let now = chrono::Utc::now().timestamp();

    let on_server = |store: &SharedStore, role: FolderRole| {
        let folder = store.lock().unwrap().folder_by_role(role).unwrap().unwrap();
        (folder.path.clone(), folder)
    };
    async fn count(server: &mut ImapServer, path: &str, message_id: &str) -> usize {
        server.select(path).await.unwrap();
        server.search_message_id(message_id).await.unwrap().len()
    }

    for version in ["first", "second"] {
        draft.html = draft.html.replace("First", version);
        let raw = compose::build_message(&me, &draft, now);
        store
            .lock()
            .unwrap()
            .queue_draft(&draft.message_id, &raw)
            .unwrap();
        sync::process_outbox(&mut server, &transport, &store, 1, EMAIL, &events)
            .await
            .expect("saving the draft works");
        let (drafts, _) = on_server(&store, FolderRole::Drafts);
        assert_eq!(count(&mut server, &drafts, &draft.message_id).await, 1);
    }

    let raw = compose::build_message(&me, &draft, now);
    store
        .lock()
        .unwrap()
        .queue_send(&draft.message_id, &raw, &draft.recipients())
        .unwrap();
    sync::process_outbox(&mut server, &transport, &store, 1, EMAIL, &events)
        .await
        .expect("sending works");
    assert!(matches!(
        receiver.try_recv(),
        Ok(sync::SyncEvent::Sent { .. })
    ));
    let (drafts, _) = on_server(&store, FolderRole::Drafts);
    assert_eq!(count(&mut server, &drafts, &draft.message_id).await, 0);
    let (sent, sent_folder) = on_server(&store, FolderRole::Sent);
    assert_eq!(count(&mut server, &sent, &draft.message_id).await, 1);
    sync::sync_folder(
        &mut server,
        &store,
        &sent_folder,
        sync::sync_since(),
        &events,
        &mut Backlog::default(),
    )
    .await
    .unwrap();
    assert!(subjects(&store, &sent_folder).contains(&subject));
    assert!(store.lock().unwrap().outbox(i64::MAX).unwrap().is_empty());
    server.logout().await;
}

#[tokio::test]
#[ignore = "needs the local test mail server"]
async fn the_server_search_finds_text_and_loads_missing_headers() {
    use e_mail_core::search::{SearchQuery, server_search};

    let mut server = connector().connect().await.unwrap();
    let paths = fresh_folders(&mut server, "search", &["box"]).await;
    let date = chrono::Utc::now().to_rfc2822();
    for subject in ["Rechnung Mai", "Rechnung Juni", "Mittagessen"] {
        server
            .append(&paths[0], None, None, &message(subject, &date))
            .await
            .unwrap();
    }
    let store = memory_store();
    let folder = local_folder(&mut server, &store, &paths[0]).await;
    assert!(subjects(&store, &folder).is_empty());

    let found = server_search(
        &mut server,
        &store,
        std::slice::from_ref(&folder),
        "rechnung",
    )
    .await
    .expect("the server searches");
    assert_eq!(found.len(), 2);
    let query = SearchQuery {
        text: "rechnung".into(),
        ..SearchQuery::default()
    };
    let mut results: Vec<String> = store
        .lock()
        .unwrap()
        .search_threads(&query, &found, true)
        .unwrap()
        .into_iter()
        .map(|r| r.thread.subject)
        .collect();
    results.sort();
    assert_eq!(results, ["Rechnung Juni", "Rechnung Mai"]);

    // Words in the text count as well, and every word must match.
    let found = server_search(&mut server, &store, &[folder], "text mittagessen")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    server.logout().await;
}

#[tokio::test]
#[ignore = "needs the local test mail server"]
async fn new_mail_through_smtp_creates_a_notification() {
    use e_mail_core::notify::{
        Notice, NoticeTexts, NotificationCenter, Notifier, NotifySettings, OnScreen,
    };
    use std::cell::RefCell;

    #[derive(Default)]
    struct Recorder(RefCell<Vec<Notice>>);
    impl Notifier for Recorder {
        fn show(&self, notice: &Notice) {
            self.0.borrow_mut().push(notice.clone());
        }
        fn withdraw(&self, _: &str) {}
    }

    // A mailbox of its own, so other tests do not see this mail.
    const BOB: &str = "bob@example.test";
    let bob_settings = ServerSettings {
        username: BOB.into(),
        ..settings()
    };
    let connector = ImapConnector {
        server: bob_settings.imap.clone(),
        username: BOB.into(),
        password: PASSWORD.into(),
        trust: trust(),
    };
    let transport = check::smtp_transport(&settings().smtp, EMAIL, PASSWORD, &trust()).unwrap();
    let send = |subject: &str| {
        let raw = format!(
            "From: Alice <{EMAIL}>\r\nTo: {BOB}\r\nSubject: {subject}\r\n\
             Message-ID: <{subject}-{}@example.test>\r\nDate: {}\r\n\r\nHello Bob\r\n",
            std::process::id(),
            chrono::Utc::now().to_rfc2822()
        );
        let envelope = Envelope::new(
            Some(EMAIL.parse::<Address>().unwrap()),
            vec![BOB.parse::<Address>().unwrap()],
        )
        .unwrap();
        let transport = transport.clone();
        async move {
            transport
                .send_raw(&envelope, raw.as_bytes())
                .await
                .expect("delivery works");
        }
    };
    send("first").await;

    let store = memory_store();
    let (events, receiver) = async_channel::unbounded();
    let mut server = None;
    for _ in 0..20 {
        match connector.connect().await {
            Ok(connected) => {
                server = Some(connected);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
    let mut server = server.expect("bob can sign in");
    let inbox = sync::sync_folder_list(&mut server, &store)
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.role == Some(FolderRole::Inbox))
        .unwrap();
    sync::sync_folder(
        &mut server,
        &store,
        &inbox,
        sync::sync_since(),
        &events,
        &mut Backlog::default(),
    )
    .await
    .unwrap();
    while let Ok(event) = receiver.try_recv() {
        assert!(
            !matches!(event, sync::SyncEvent::NewMail { .. }),
            "first sync stays quiet"
        );
    }

    send("second").await;
    let mut arrived = Vec::new();
    for _ in 0..20 {
        sync::sync_folder(
            &mut server,
            &store,
            &inbox,
            sync::sync_since(),
            &events,
            &mut Backlog::default(),
        )
        .await
        .unwrap();
        while let Ok(event) = receiver.try_recv() {
            if let sync::SyncEvent::NewMail { messages, .. } = event {
                arrived.extend(messages);
            }
        }
        if !arrived.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(arrived.len(), 1);

    let summaries: Vec<_> = arrived
        .iter()
        .map(|id| store.lock().unwrap().message(*id).unwrap().unwrap())
        .collect();
    let recorder = Recorder::default();
    NotificationCenter::default().new_mail(
        &recorder,
        1,
        "Bob",
        summaries,
        &NotifySettings {
            enabled: true,
            ..NotifySettings::default()
        },
        OnScreen::default(),
        chrono::Utc::now().timestamp(),
        &NoticeTexts {
            many: "{count} new · {account}".into(),
            one: "{name} · {account}".into(),
            no_subject: "-".into(),
            more: "+{count}".into(),
        },
        |_| true,
    );
    let shown = recorder.0.borrow().clone();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].title, "Alice · Bob");
    assert_eq!(shown[0].body, "second");
    server.logout().await;
}
