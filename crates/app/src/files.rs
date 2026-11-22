// SPDX-License-Identifier: GPL-3.0-or-later

//! Local files hold mail, so only the user may read them: folders 0700,
//! files 0600. Opened attachments are only kept for a day.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long an opened attachment stays in the cache.
const ATTACHMENT_LIFETIME: Duration = Duration::from_secs(24 * 3600);

/// Creates a folder (and its parents) that only the user can enter.
pub fn private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Writes a file only the user can read, never executable.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(data)
}

/// Tightens the folders and databases of the app, removes what WebKit
/// left behind in earlier versions and attachments older than a day.
pub fn tidy(data_dir: &Path, cache_dir: &Path) {
    for dir in [data_dir, cache_dir] {
        if let Err(error) = private_dir(dir) {
            tracing::warn!("could not protect {}: {error}", dir.display());
        }
    }
    if let Ok(entries) = fs::read_dir(data_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_database = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".db"));
            if is_database && path.is_file() {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
        }
    }
    for leftover in [
        cache_dir.join("WebKitCache"),
        cache_dir.join("HSTS"),
        data_dir.join("storage"),
        data_dir.join("mediakeys"),
    ] {
        if leftover.is_dir() {
            let _ = fs::remove_dir_all(&leftover);
        }
    }
    remove_old(
        &attachments_dir(cache_dir),
        SystemTime::now(),
        ATTACHMENT_LIFETIME,
    );
}

pub fn attachments_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("attachments")
}

/// Removes entries of `dir` not changed for `age`.
fn remove_old(dir: &Path, now: SystemTime, age: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|since| since > age);
        if path.is_dir() {
            remove_old(&path, now, age);
            if old || fs::read_dir(&path).is_ok_and(|mut rest| rest.next().is_none()) {
                let _ = fs::remove_dir_all(&path);
            }
        } else if old {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("e-mail-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn files_are_private_and_never_executable() {
        let dir = temp("private");
        let file = dir.join("attachments").join("7").join("x.desktop");
        write_private(&file, b"[Desktop Entry]").unwrap();
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tidy_protects_data_and_clears_leftovers() {
        let data = temp("data");
        let cache = temp("cache");
        fs::create_dir_all(data.join("storage")).unwrap();
        fs::create_dir_all(cache.join("WebKitCache")).unwrap();
        fs::write(data.join("account-1.db"), b"db").unwrap();
        fs::set_permissions(data.join("account-1.db"), fs::Permissions::from_mode(0o644)).unwrap();
        tidy(&data, &cache);
        assert_eq!(mode(&data), 0o700);
        assert_eq!(mode(&cache), 0o700);
        assert_eq!(mode(&data.join("account-1.db")), 0o600);
        assert!(!data.join("storage").exists());
        assert!(!cache.join("WebKitCache").exists());
        fs::remove_dir_all(&data).unwrap();
        fs::remove_dir_all(&cache).unwrap();
    }

    #[test]
    fn old_attachments_go_away() {
        let cache = temp("old");
        let file = attachments_dir(&cache).join("1").join("a.pdf");
        write_private(&file, b"pdf").unwrap();
        let later = SystemTime::now() + Duration::from_secs(2 * 24 * 3600);
        remove_old(
            &attachments_dir(&cache),
            SystemTime::now(),
            ATTACHMENT_LIFETIME,
        );
        assert!(file.exists());
        remove_old(&attachments_dir(&cache), later, ATTACHMENT_LIFETIME);
        assert!(!file.exists());
        fs::remove_dir_all(&cache).unwrap();
    }
}
