// SPDX-License-Identifier: GPL-3.0-or-later

//! The list of accounts. Mail data lives elsewhere: every account has its
//! own database file (see [`crate::store`]), so removing an account removes
//! all of its local data at once.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{Account, AccountId, NewAccount, Security, ServerConfig, ServerSettings};

pub type AccountsResult<T> = rusqlite::Result<T>;

const SCHEMA_VERSION: i32 = 2;

// AUTOINCREMENT keeps the id of a removed account from being reused, so a
// new account can never pick up leftovers of an old one.
const SCHEMA: &str = "
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email TEXT NOT NULL UNIQUE COLLATE NOCASE,
    label TEXT NOT NULL,
    display_name TEXT NOT NULL,
    username TEXT NOT NULL,
    imap_host TEXT NOT NULL,
    imap_port INTEGER NOT NULL,
    imap_security TEXT NOT NULL,
    smtp_host TEXT NOT NULL,
    smtp_port INTEGER NOT NULL,
    smtp_security TEXT NOT NULL
);
";

/// Version 2 adds the signature.
const SIGNATURE_SCHEMA: &str =
    "ALTER TABLE accounts ADD COLUMN signature TEXT NOT NULL DEFAULT '';";

const COLUMNS: &str = "id, email, label, display_name, username, imap_host, imap_port, \
                       imap_security, smtp_host, smtp_port, smtp_security";

pub struct Accounts {
    conn: Connection,
}

impl Accounts {
    pub fn open(path: &Path) -> AccountsResult<Accounts> {
        Accounts::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> AccountsResult<Accounts> {
        Accounts::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> AccountsResult<Accounts> {
        let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 0 {
            conn.execute_batch(&format!(
                "BEGIN; {SCHEMA} {SIGNATURE_SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?;
        } else if version == 1 {
            conn.execute_batch(&format!(
                "BEGIN; {SIGNATURE_SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
            ))?;
        }
        Ok(Accounts { conn })
    }

    pub fn add(&self, account: &NewAccount) -> AccountsResult<Account> {
        let s = &account.settings;
        self.conn.execute(
            "INSERT INTO accounts (email, label, display_name, username, imap_host, imap_port,
                                   imap_security, smtp_host, smtp_port, smtp_security)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                account.email,
                account.label,
                account.display_name,
                s.username,
                s.imap.host,
                s.imap.port,
                s.imap.security.as_str(),
                s.smtp.host,
                s.smtp.port,
                s.smtp.security.as_str(),
            ],
        )?;
        Ok(Account {
            id: self.conn.last_insert_rowid(),
            email: account.email.clone(),
            label: account.label.clone(),
            display_name: account.display_name.clone(),
            settings: account.settings.clone(),
        })
    }

    /// In the order they were added.
    pub fn all(&self) -> AccountsResult<Vec<Account>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM accounts ORDER BY id"))?;
        stmt.query_map([], account_from_row)?.collect()
    }

    pub fn get(&self, id: AccountId) -> AccountsResult<Option<Account>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM accounts WHERE id = ?1"))?;
        stmt.query_row([id], account_from_row).optional()
    }

    pub fn contains_email(&self, email: &str) -> AccountsResult<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM accounts WHERE email = ?1",
                [email.trim()],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
    }

    pub fn set_label(&self, id: AccountId, label: &str) -> AccountsResult<()> {
        self.conn
            .execute(
                "UPDATE accounts SET label = ?2 WHERE id = ?1",
                params![id, label],
            )
            .map(|_| ())
    }

    pub fn set_display_name(&self, id: AccountId, name: &str) -> AccountsResult<()> {
        self.conn
            .execute(
                "UPDATE accounts SET display_name = ?2 WHERE id = ?1",
                params![id, name],
            )
            .map(|_| ())
    }

    pub fn set_settings(&self, id: AccountId, s: &ServerSettings) -> AccountsResult<()> {
        self.conn
            .execute(
                "UPDATE accounts SET username = ?2, imap_host = ?3, imap_port = ?4,
                        imap_security = ?5, smtp_host = ?6, smtp_port = ?7, smtp_security = ?8
                 WHERE id = ?1",
                params![
                    id,
                    s.username,
                    s.imap.host,
                    s.imap.port,
                    s.imap.security.as_str(),
                    s.smtp.host,
                    s.smtp.port,
                    s.smtp.security.as_str(),
                ],
            )
            .map(|_| ())
    }

    /// The signature as editor HTML, empty if there is none.
    pub fn signature(&self, id: AccountId) -> AccountsResult<String> {
        self.conn
            .query_row(
                "SELECT signature FROM accounts WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map(Option::unwrap_or_default)
    }

    pub fn set_signature(&self, id: AccountId, html: &str) -> AccountsResult<()> {
        self.conn.execute(
            "UPDATE accounts SET signature = ?2 WHERE id = ?1",
            params![id, html],
        )?;
        Ok(())
    }

    pub fn remove(&self, id: AccountId) -> AccountsResult<()> {
        self.conn
            .execute("DELETE FROM accounts WHERE id = ?1", [id])
            .map(|_| ())
    }
}

fn account_from_row(row: &rusqlite::Row<'_>) -> AccountsResult<Account> {
    let security = |index: usize| -> AccountsResult<Security> {
        let value: String = row.get(index)?;
        Ok(Security::parse(&value).unwrap_or(Security::Tls))
    };
    Ok(Account {
        id: row.get(0)?,
        email: row.get(1)?,
        label: row.get(2)?,
        display_name: row.get(3)?,
        settings: ServerSettings {
            username: row.get(4)?,
            imap: ServerConfig {
                host: row.get(5)?,
                port: row.get(6)?,
                security: security(7)?,
            },
            smtp: ServerConfig {
                host: row.get(8)?,
                port: row.get(9)?,
                security: security(10)?,
            },
        },
    })
}

/// Where the mail database of an account lives inside `dir`.
pub fn store_path(dir: &Path, id: AccountId) -> PathBuf {
    dir.join(format!("account-{id}.db"))
}

/// Deletes the mail database of an account together with SQLite's
/// side files. Missing files are fine.
pub fn remove_store_files(path: &Path) -> std::io::Result<()> {
    let mut candidates = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        candidates.push(PathBuf::from(name));
    }
    for file in candidates {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// An account from the first test version, which kept everything in one
/// database and only knew the IMAP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyAccount {
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
}

pub fn read_legacy_accounts(path: &Path) -> AccountsResult<Vec<LegacyAccount>> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT email, imap_host, imap_port FROM accounts ORDER BY id")?;
    stmt.query_map([], |row| {
        Ok(LegacyAccount {
            email: row.get(0)?,
            imap_host: row.get(1)?,
            imap_port: row.get(2)?,
        })
    })?
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_account(email: &str) -> NewAccount {
        NewAccount {
            email: email.into(),
            label: "Personal".into(),
            display_name: "Max Berger".into(),
            settings: ServerSettings {
                imap: ServerConfig::new("imap.example.org", 993, Security::Tls),
                smtp: ServerConfig::new("smtp.example.org", 587, Security::StartTls),
                username: email.into(),
            },
        }
    }

    #[test]
    fn accounts_round_trip() {
        let accounts = Accounts::open_in_memory().unwrap();
        let first = accounts.add(&new_account("max@example.org")).unwrap();
        let second = accounts.add(&new_account("club@example.org")).unwrap();
        assert_eq!(accounts.all().unwrap(), vec![first.clone(), second.clone()]);
        assert_eq!(accounts.get(second.id).unwrap(), Some(second.clone()));
        assert_eq!(
            accounts
                .get(second.id)
                .unwrap()
                .unwrap()
                .settings
                .smtp
                .security,
            Security::StartTls
        );
    }

    #[test]
    fn same_address_cannot_be_added_twice() {
        let accounts = Accounts::open_in_memory().unwrap();
        accounts.add(&new_account("max@example.org")).unwrap();
        assert!(accounts.contains_email("MAX@example.org").unwrap());
        assert!(!accounts.contains_email("other@example.org").unwrap());
        assert!(accounts.add(&new_account("Max@Example.org")).is_err());
    }

    #[test]
    fn names_and_settings_can_change() {
        let accounts = Accounts::open_in_memory().unwrap();
        let account = accounts.add(&new_account("max@example.org")).unwrap();
        accounts.set_label(account.id, "Club").unwrap();
        accounts.set_display_name(account.id, "Max B.").unwrap();
        let mut settings = account.settings.clone();
        settings.imap.host = "mail.example.org".into();
        settings.imap.port = 143;
        settings.imap.security = Security::StartTls;
        accounts.set_settings(account.id, &settings).unwrap();

        let changed = accounts.get(account.id).unwrap().unwrap();
        assert_eq!(changed.label, "Club");
        assert_eq!(changed.display_name, "Max B.");
        assert_eq!(changed.settings, settings);
        assert_eq!(changed.email, "max@example.org");
    }

    #[test]
    fn signature_is_kept_per_account() {
        let accounts = Accounts::open_in_memory().unwrap();
        let a = accounts.add(&new_account("a@example.org")).unwrap();
        let b = accounts.add(&new_account("b@example.org")).unwrap();
        assert_eq!(accounts.signature(a.id).unwrap(), "");
        accounts.set_signature(a.id, "<b>Anna</b>").unwrap();
        assert_eq!(accounts.signature(a.id).unwrap(), "<b>Anna</b>");
        assert_eq!(accounts.signature(b.id).unwrap(), "");
    }

    #[test]
    fn version_one_gets_the_signature_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("{SCHEMA} PRAGMA user_version = 1;"))
            .unwrap();
        let accounts = Accounts::init(conn).unwrap();
        let account = accounts.add(&new_account("a@example.org")).unwrap();
        accounts.set_signature(account.id, "Hi").unwrap();
        assert_eq!(accounts.signature(account.id).unwrap(), "Hi");
    }

    #[test]
    fn removed_ids_are_not_reused() {
        let accounts = Accounts::open_in_memory().unwrap();
        let first = accounts.add(&new_account("a@example.org")).unwrap();
        accounts.remove(first.id).unwrap();
        assert!(accounts.all().unwrap().is_empty());
        let again = accounts.add(&new_account("a@example.org")).unwrap();
        assert_ne!(first.id, again.id);
    }

    #[test]
    fn store_files_are_removed() {
        let dir = std::env::temp_dir().join(format!("e-mail-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = store_path(&dir, 7);
        std::fs::write(&path, b"db").unwrap();
        std::fs::write(dir.join("account-7.db-wal"), b"wal").unwrap();
        std::fs::write(store_path(&dir, 8), b"other").unwrap();

        remove_store_files(&path).unwrap();
        assert!(!path.exists());
        assert!(!dir.join("account-7.db-wal").exists());
        assert!(store_path(&dir, 8).exists());
        remove_store_files(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_accounts_are_read() {
        let dir = std::env::temp_dir().join(format!("e-mail-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE,
                                    imap_host TEXT NOT NULL, imap_port INTEGER NOT NULL);
             INSERT INTO accounts (email, imap_host, imap_port)
             VALUES ('me@example.org', 'imap.example.org', 993);",
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            read_legacy_accounts(&path).unwrap(),
            vec![LegacyAccount {
                email: "me@example.org".into(),
                imap_host: "imap.example.org".into(),
                imap_port: 993,
            }]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
