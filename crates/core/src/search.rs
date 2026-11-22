// SPDX-License-Identifier: GPL-3.0-or-later

//! Searching the active account. The local index (SQLite FTS5) answers at
//! once; as long as not every message is stored here with its text, the
//! server is asked as well and its hits are added without duplicates.

use std::collections::HashSet;

use crate::model::{Folder, FolderId, MessageId, ThreadSummary};
use crate::server::MailServer;
use crate::sync::{SharedStore, load_headers};

/// At most this many messages are looked at when there is no search text.
pub const MAX_HITS: usize = 2000;
/// At most this many conversations are shown.
pub const MAX_RESULTS: usize = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filters {
    pub unread: bool,
    pub flagged: bool,
    pub attachment: bool,
    /// Only this folder; otherwise everything but trash and spam.
    pub folder: Option<FolderId>,
}

impl Filters {
    pub fn any(&self) -> bool {
        self.unread || self.flagged || self.attachment || self.folder.is_some()
    }

    pub fn accepts(&self, thread: &ThreadSummary) -> bool {
        (!self.unread || thread.unread)
            && (!self.flagged || thread.flagged)
            && (!self.attachment || thread.has_attachment)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub filters: Filters,
}

impl SearchQuery {
    /// True if there is anything to search for.
    pub fn is_active(&self) -> bool {
        !self.text.trim().is_empty() || self.filters.any()
    }

    /// The words to highlight in the results.
    pub fn terms(&self) -> Vec<String> {
        words(&self.text)
    }
}

/// A conversation found, with the folder it was found in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub thread: ThreadSummary,
    pub folder: FolderId,
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| word.trim_matches('"').to_owned())
        .filter(|word| !word.is_empty())
        .collect()
}

/// The FTS5 query for what the user typed: every word must appear, as the
/// start of a word. Quotes keep the input from being read as FTS syntax
/// (AND, OR, NEAR, column filters, `*`). None if nothing is left to look for.
pub fn fts_query(text: &str) -> Option<String> {
    let parts: Vec<String> = words(text)
        .into_iter()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Byte ranges in `text` where one of the terms appears, ignoring case,
/// sorted and without overlaps. Used to highlight hits.
pub fn match_ranges(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let lower = text.to_lowercase();
    // Lowercasing can change byte lengths; only highlight when it did not.
    if lower.len() != text.len() {
        return Vec::new();
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        let term = term.to_lowercase();
        if term.is_empty() {
            continue;
        }
        let mut from = 0;
        while let Some(pos) = lower[from..].find(&term) {
            let start = from + pos;
            let end = start + term.len();
            if text.is_char_boundary(start) && text.is_char_boundary(end) {
                ranges.push((start, end));
            }
            from = end;
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Asks the server to search `folders` for every word of `text`. Messages
/// the store does not know yet are loaded (headers only) so they can be
/// shown. Returns the messages found.
pub async fn server_search<S: MailServer>(
    server: &mut S,
    store: &SharedStore,
    folders: &[Folder],
    text: &str,
) -> anyhow::Result<Vec<MessageId>> {
    let terms = words(text);
    // Words with umlauts and the like cannot be sent in a server search
    // (see `ImapServer::search_text`); searching for the others alone would
    // find too much, so such searches stay local.
    if terms.is_empty() || terms.iter().any(|term| !term.is_ascii()) {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for folder in folders {
        server.select(&folder.path).await?;
        let uids = server.search_text(&terms).await?;
        if uids.is_empty() {
            continue;
        }
        let local: HashSet<u32> = lock(store).uids(folder.id)?.into_iter().collect();
        let missing: Vec<u32> = uids
            .iter()
            .copied()
            .filter(|uid| !local.contains(uid))
            .collect();
        for batch in missing.chunks(50) {
            load_headers(server, store, folder.id, batch).await?;
        }
        found.extend(lock(store).message_ids(folder.id, &uids)?);
    }
    Ok(found)
}

fn lock(store: &SharedStore) -> std::sync::MutexGuard<'_, crate::store::Store> {
    store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_input_cannot_break_the_query() {
        assert_eq!(fts_query("  "), None);
        assert_eq!(fts_query("\"\""), None);
        assert_eq!(fts_query("Rechnung"), Some("\"Rechnung\"*".into()));
        assert_eq!(fts_query("a OR b"), Some("\"a\"* \"OR\"* \"b\"*".into()));
        assert_eq!(
            fts_query("subject:x a\"b NEAR(c)"),
            Some("\"subject:x\"* \"a\"\"b\"* \"NEAR(c)\"*".into())
        );
    }

    #[test]
    fn filters_need_text_or_a_choice() {
        let mut query = SearchQuery::default();
        assert!(!query.is_active());
        query.filters.unread = true;
        assert!(query.is_active());
        query = SearchQuery {
            text: " berlin ".into(),
            ..SearchQuery::default()
        };
        assert!(query.is_active());
        assert_eq!(query.terms(), ["berlin"]);
    }

    #[test]
    fn hits_are_found_regardless_of_case() {
        let terms = vec!["ber".to_owned(), "zug".to_owned()];
        assert_eq!(
            match_ranges("Zug nach Berlin, ZUGabe", &terms),
            [(0, 3), (9, 12), (17, 20)]
        );
        // Overlapping terms become one range.
        assert_eq!(
            match_ranges("Rechnung", &["rech".into(), "chnung".into()]),
            [(0, 8)]
        );
        assert_eq!(match_ranges("Grüße aus Köln", &["köln".into()]), [(12, 17)]);
        assert!(match_ranges("nothing", &["x".into()]).is_empty());
    }
}
