// SPDX-License-Identifier: GPL-3.0-or-later

/// The standard folders, in the order the sidebar shows them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FolderRole {
    Inbox,
    Drafts,
    Sent,
    Archive,
    Junk,
    Trash,
}

impl FolderRole {
    pub const ALL: [FolderRole; 6] = [
        FolderRole::Inbox,
        FolderRole::Drafts,
        FolderRole::Sent,
        FolderRole::Archive,
        FolderRole::Junk,
        FolderRole::Trash,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FolderRole::Inbox => "inbox",
            FolderRole::Drafts => "drafts",
            FolderRole::Sent => "sent",
            FolderRole::Archive => "archive",
            FolderRole::Junk => "junk",
            FolderRole::Trash => "trash",
        }
    }

    pub fn parse(value: &str) -> Option<FolderRole> {
        FolderRole::ALL
            .into_iter()
            .find(|role| role.as_str() == value)
    }

    /// Guesses the role from the folder name for servers without the
    /// SPECIAL-USE extension. The list covers the names used by common
    /// English and German providers.
    pub fn from_name(path: &str, delimiter: Option<&str>) -> Option<FolderRole> {
        if path.eq_ignore_ascii_case("INBOX") {
            return Some(FolderRole::Inbox);
        }
        let leaf = match delimiter {
            Some(d) if !d.is_empty() => path.rsplit(d).next().unwrap_or(path),
            _ => path,
        };
        let is_top_level =
            leaf.len() == path.len() || path.to_ascii_uppercase().starts_with("INBOX");
        if !is_top_level {
            return None;
        }
        let leaf = crate::utf7::decode(leaf).to_lowercase();
        let role = match leaf.as_str() {
            "drafts" | "draft" | "entwürfe" | "entwurf" => FolderRole::Drafts,
            "sent" | "sent items" | "sent messages" | "sent mail" | "gesendet"
            | "gesendete objekte" | "gesendete elemente" => FolderRole::Sent,
            "archive" | "archives" | "archiv" => FolderRole::Archive,
            "junk" | "spam" | "junk e-mail" | "junk email" | "spamverdacht" => FolderRole::Junk,
            "trash"
            | "deleted"
            | "deleted items"
            | "deleted messages"
            | "bin"
            | "papierkorb"
            | "gelöscht"
            | "gelöschte objekte"
            | "gelöschte elemente" => FolderRole::Trash,
            _ => return None,
        };
        Some(role)
    }
}

#[cfg(test)]
mod tests {
    use super::FolderRole;

    #[test]
    fn inbox_is_case_insensitive() {
        assert_eq!(
            FolderRole::from_name("inbox", Some("/")),
            Some(FolderRole::Inbox)
        );
    }

    #[test]
    fn detects_german_and_english_names() {
        assert_eq!(
            FolderRole::from_name("Gesendet", Some("/")),
            Some(FolderRole::Sent)
        );
        assert_eq!(
            FolderRole::from_name("Sent Items", Some("/")),
            Some(FolderRole::Sent)
        );
        assert_eq!(
            FolderRole::from_name("Entw&APw-rfe", Some("/")),
            Some(FolderRole::Drafts)
        );
        assert_eq!(
            FolderRole::from_name("INBOX.Trash", Some(".")),
            Some(FolderRole::Trash)
        );
        assert_eq!(FolderRole::from_name("Spam", None), Some(FolderRole::Junk));
    }

    #[test]
    fn nested_custom_folders_are_not_standard() {
        assert_eq!(FolderRole::from_name("Projects/Sent", Some("/")), None);
        assert_eq!(FolderRole::from_name("Invoices", Some("/")), None);
    }

    #[test]
    fn role_round_trips_through_string() {
        for role in FolderRole::ALL {
            assert_eq!(FolderRole::parse(role.as_str()), Some(role));
        }
    }
}
