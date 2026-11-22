// SPDX-License-Identifier: GPL-3.0-or-later

//! Helpers for grouping messages into conversations. Messages belong
//! together when one names the other in `References` or `In-Reply-To`.
//! Replies without these headers are matched by their subject instead.

/// Prefixes mail programs put in front of the subject of replies and
/// forwards, in the languages our users are most likely to meet.
const PREFIXES: [&str; 12] = [
    "re", "aw", "wg", "fw", "fwd", "sv", "vs", "antw", "tr", "rif", "r", "odp",
];

/// The subject without reply prefixes, list tags and spacing differences,
/// lowercased so that "Re: Hello" and "AW: hello" match.
pub fn base_subject(subject: &str) -> String {
    let mut rest = subject.trim();
    loop {
        let before = rest;
        rest = strip_list_tag(rest);
        rest = strip_prefix(rest).unwrap_or(rest);
        if rest.len() == before.len() {
            break;
        }
    }
    rest.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// True if the subject starts with a reply or forward prefix.
pub fn is_reply(subject: &str) -> bool {
    strip_prefix(strip_list_tag(subject.trim())).is_some()
}

/// Removes one prefix such as "Re:", "AW:" or "Re[2]:".
fn strip_prefix(subject: &str) -> Option<&str> {
    let colon = subject.find(':')?;
    let head = subject[..colon].trim_end();
    let word = match head.find('[') {
        Some(open) if head.ends_with(']') => {
            let count = &head[open + 1..head.len() - 1];
            if !count.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            &head[..open]
        }
        _ => head,
    };
    PREFIXES
        .iter()
        .any(|prefix| word.eq_ignore_ascii_case(prefix))
        .then(|| subject[colon + 1..].trim_start())
}

/// Removes a mailing list tag such as "[team]" at the start.
fn strip_list_tag(subject: &str) -> &str {
    if let Some(rest) = subject.strip_prefix('[')
        && let Some(close) = rest.find(']')
        && close < 40
    {
        return rest[close + 1..].trim_start();
    }
    subject
}

/// All message IDs a message refers to, without duplicates and without
/// its own ID, oldest first as in the `References` header.
pub fn parent_ids(references: &[String], in_reply_to: &[String], own: Option<&str>) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for id in references.iter().chain(in_reply_to) {
        let id = id.trim().trim_start_matches('<').trim_end_matches('>');
        if id.is_empty() || Some(id) == own || ids.iter().any(|known| known == id) {
            continue;
        }
        ids.push(id.to_owned());
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_reply_prefixes() {
        assert_eq!(base_subject("Re: Hello"), "hello");
        assert_eq!(
            base_subject("AW: WG: Re:  Fahrrad  noch da?"),
            "fahrrad noch da?"
        );
        assert_eq!(base_subject("RE[2]: Offer"), "offer");
        assert_eq!(base_subject("[team] Re: Sprint review"), "sprint review");
        assert_eq!(base_subject("Fwd: [list] Plans"), "plans");
    }

    #[test]
    fn keeps_ordinary_colons() {
        assert_eq!(
            base_subject("Server maintenance: Sunday"),
            "server maintenance: sunday"
        );
        assert_eq!(base_subject("Agenda: Re-launch"), "agenda: re-launch");
        assert!(!is_reply("Agenda: Re-launch"));
        assert!(!is_reply("Return to sender"));
    }

    #[test]
    fn detects_replies() {
        assert!(is_reply("Re: Hello"));
        assert!(is_reply("AW: Hallo"));
        assert!(is_reply("[team] Fwd: Plans"));
        assert!(!is_reply("Hello"));
    }

    #[test]
    fn collects_parent_ids() {
        let ids = parent_ids(
            &["<a@x>".into(), "b@x".into(), "a@x".into()],
            &["c@x".into(), "b@x".into(), "own@x".into()],
            Some("own@x"),
        );
        assert_eq!(ids, ["a@x", "b@x", "c@x"]);
    }
}
