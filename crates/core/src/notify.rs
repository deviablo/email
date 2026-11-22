// SPDX-License-Identifier: GPL-3.0-or-later

//! Deciding which new mail is worth a notification and what it says. The
//! system shows it; the app never shows banners of its own. The output goes
//! through [`Notifier`], so the decisions can be tested without a desktop.

use std::collections::{HashMap, HashSet};

use crate::model::{AccountId, MessageId, MessageSummary, ThreadId};

/// How old a message may be and still count as new. Mail moved into the
/// inbox (for example "Not spam") keeps its old date and stays quiet.
pub const NEW_FOR_SECONDS: i64 = 2 * 86_400;
/// More senders than this are summed up as "and N more".
const LISTED: usize = 3;

/// What the user chose in Settings → General.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotifySettings {
    pub enabled: bool,
    pub muted: HashSet<AccountId>,
}

impl NotifySettings {
    pub fn allows(&self, account: AccountId) -> bool {
        self.enabled && !self.muted.contains(&account)
    }
}

/// What the user is looking at right now: the conversation open in the
/// focused window, if any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OnScreen {
    pub account: Option<AccountId>,
    pub thread: Option<ThreadId>,
}

/// A notification as the system should show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// Stable per account, so a newer notice replaces the older one.
    pub id: String,
    pub title: String,
    pub body: String,
    pub account: AccountId,
    /// The message a click opens.
    pub open: MessageId,
    /// The messages "Mark as read" and "Delete" act on.
    pub messages: Vec<MessageId>,
    /// "Delete" is only offered for a single message.
    pub can_delete: bool,
}

/// Texts in the language of the app.
pub struct NoticeTexts {
    /// "{count} new messages · {account}"
    pub many: String,
    /// "{name} · {account}" for a single message.
    pub one: String,
    /// "(No subject)"
    pub no_subject: String,
    /// "and {count} more"
    pub more: String,
}

pub trait Notifier {
    fn show(&self, notice: &Notice);
    fn withdraw(&self, id: &str);
}

pub fn notice_id(account: AccountId) -> String {
    format!("new-mail-{account}")
}

/// Whether a new message deserves a notification.
pub fn worth_notifying(
    message: &MessageSummary,
    account: AccountId,
    settings: &NotifySettings,
    on_screen: OnScreen,
    now: i64,
) -> bool {
    settings.allows(account)
        && !message.flags.seen
        && message.date >= now - NEW_FOR_SECONDS
        && !(on_screen.account == Some(account) && on_screen.thread == Some(message.thread_id))
}

/// Keeps one notification per account. New mail is added to what is
/// still unread from earlier, so several arrivals become one notice.
#[derive(Default)]
pub struct NotificationCenter {
    pending: HashMap<AccountId, Vec<MessageSummary>>,
}

impl NotificationCenter {
    /// Handles mail that just arrived. `still_unread` tells which of the
    /// earlier messages are unread and present; the others drop out.
    #[allow(clippy::too_many_arguments)]
    pub fn new_mail(
        &mut self,
        notifier: &impl Notifier,
        account: AccountId,
        account_label: &str,
        arrived: Vec<MessageSummary>,
        settings: &NotifySettings,
        on_screen: OnScreen,
        now: i64,
        texts: &NoticeTexts,
        still_unread: impl Fn(MessageId) -> bool,
    ) {
        let fresh: Vec<MessageSummary> = arrived
            .into_iter()
            .filter(|m| worth_notifying(m, account, settings, on_screen, now))
            .collect();
        if fresh.is_empty() {
            return;
        }
        let pending = self.pending.entry(account).or_default();
        pending.retain(|m| still_unread(m.id));
        for message in fresh {
            if !pending.iter().any(|known| known.id == message.id) {
                pending.push(message);
            }
        }
        pending.sort_by(|a, b| b.date.cmp(&a.date).then(b.id.cmp(&a.id)));
        notifier.show(&compose_notice(account, account_label, pending, texts));
    }

    /// The user dealt with the mail of an account (opened the app on it,
    /// read or deleted it from the notification, turned notices off).
    pub fn clear(&mut self, notifier: &impl Notifier, account: AccountId) {
        if self.pending.remove(&account).is_some() {
            notifier.withdraw(&notice_id(account));
        }
    }

    pub fn pending(&self, account: AccountId) -> &[MessageSummary] {
        self.pending.get(&account).map_or(&[], Vec::as_slice)
    }
}

/// The text of the notice for the unread new messages of one account,
/// newest first.
pub fn compose_notice(
    account: AccountId,
    account_label: &str,
    messages: &[MessageSummary],
    texts: &NoticeTexts,
) -> Notice {
    let subject = |m: &MessageSummary| {
        if m.subject.trim().is_empty() {
            texts.no_subject.clone()
        } else {
            m.subject.clone()
        }
    };
    let newest = &messages[0];
    let (title, body) = if messages.len() == 1 {
        (
            texts
                .one
                .replace("{name}", newest.sender_label())
                .replace("{account}", account_label),
            subject(newest),
        )
    } else {
        let mut lines: Vec<String> = messages
            .iter()
            .take(LISTED)
            .map(|m| format!("{}: {}", m.sender_label(), subject(m)))
            .collect();
        if messages.len() > LISTED {
            lines.push(
                texts
                    .more
                    .replace("{count}", &(messages.len() - LISTED).to_string()),
            );
        }
        (
            texts
                .many
                .replace("{count}", &messages.len().to_string())
                .replace("{account}", account_label),
            lines.join("\n"),
        )
    };
    Notice {
        id: notice_id(account),
        title,
        body,
        account,
        open: newest.id,
        messages: messages.iter().map(|m| m.id).collect(),
        can_delete: messages.len() == 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Flags;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Recorder {
        shown: RefCell<Vec<Notice>>,
        withdrawn: RefCell<Vec<String>>,
    }

    impl Notifier for Recorder {
        fn show(&self, notice: &Notice) {
            self.shown.borrow_mut().push(notice.clone());
        }
        fn withdraw(&self, id: &str) {
            self.withdrawn.borrow_mut().push(id.to_owned());
        }
    }

    const NOW: i64 = 1_800_000_000;

    fn message(id: MessageId, from: &str, subject: &str, seen: bool) -> MessageSummary {
        MessageSummary {
            id,
            folder_id: 1,
            uid: id as u32,
            thread_id: id * 10,
            message_id: None,
            subject: subject.into(),
            from_name: from.into(),
            from_address: format!("{}@example.org", from.to_lowercase()),
            date: NOW - 60 + id,
            flags: Flags {
                seen,
                flagged: false,
            },
            preview: String::new(),
            has_attachment: false,
            recipients: String::new(),
        }
    }

    fn texts() -> NoticeTexts {
        NoticeTexts {
            many: "{count} new messages · {account}".into(),
            one: "{name} · {account}".into(),
            no_subject: "(No subject)".into(),
            more: "and {count} more".into(),
        }
    }

    fn on() -> NotifySettings {
        NotifySettings {
            enabled: true,
            muted: HashSet::new(),
        }
    }

    #[test]
    fn decides_what_is_worth_a_notice() {
        let fresh = message(1, "Lena", "Hi", false);
        let nobody = OnScreen::default();
        assert!(worth_notifying(&fresh, 7, &on(), nobody, NOW));
        assert!(!worth_notifying(
            &message(1, "Lena", "Hi", true),
            7,
            &on(),
            nobody,
            NOW
        ));
        let off = NotifySettings::default();
        assert!(!worth_notifying(&fresh, 7, &off, nobody, NOW));
        let muted = NotifySettings {
            enabled: true,
            muted: HashSet::from([7]),
        };
        assert!(!worth_notifying(&fresh, 7, &muted, nobody, NOW));
        assert!(worth_notifying(&fresh, 8, &muted, nobody, NOW));
        let mut old = fresh.clone();
        old.date = NOW - NEW_FOR_SECONDS - 1;
        assert!(!worth_notifying(&old, 7, &on(), nobody, NOW));
        let watching = OnScreen {
            account: Some(7),
            thread: Some(fresh.thread_id),
        };
        assert!(!worth_notifying(&fresh, 7, &on(), watching, NOW));
        // The same conversation in another account is something else.
        assert!(worth_notifying(&fresh, 8, &on(), watching, NOW));
    }

    #[test]
    fn several_messages_become_one_notice() {
        let recorder = Recorder::default();
        let mut center = NotificationCenter::default();
        center.new_mail(
            &recorder,
            7,
            "Work",
            vec![message(1, "Lena", "Plan", false)],
            &on(),
            OnScreen::default(),
            NOW,
            &texts(),
            |_| true,
        );
        let first = recorder.shown.borrow()[0].clone();
        assert_eq!(first.title, "Lena · Work");
        assert_eq!(first.body, "Plan");
        assert_eq!(first.id, "new-mail-7");
        assert!(first.can_delete);
        assert_eq!(first.open, 1);

        center.new_mail(
            &recorder,
            7,
            "Work",
            vec![
                message(2, "Tom", "", false),
                message(3, "Kim", "Lunch", false),
                message(4, "Ina", "Old news", true),
                message(5, "Max", "Report", false),
            ],
            &on(),
            OnScreen::default(),
            NOW,
            &texts(),
            |_| true,
        );
        let last = recorder.shown.borrow().last().unwrap().clone();
        assert_eq!(last.id, "new-mail-7");
        assert_eq!(last.title, "4 new messages · Work");
        assert_eq!(
            last.body,
            "Max: Report\nKim: Lunch\nTom: (No subject)\nand 1 more"
        );
        assert_eq!(last.open, 5);
        assert_eq!(last.messages, [5, 3, 2, 1]);
        assert!(!last.can_delete);
    }

    #[test]
    fn read_mail_drops_out_and_quiet_batches_show_nothing() {
        let recorder = Recorder::default();
        let mut center = NotificationCenter::default();
        let first = vec![message(1, "Lena", "Plan", false)];
        center.new_mail(
            &recorder,
            7,
            "Work",
            first,
            &on(),
            OnScreen::default(),
            NOW,
            &texts(),
            |_| true,
        );
        center.new_mail(
            &recorder,
            7,
            "Work",
            vec![message(2, "Tom", "Re: Plan", false)],
            &on(),
            OnScreen::default(),
            NOW,
            &texts(),
            |id| id != 1,
        );
        let last = recorder.shown.borrow().last().unwrap().clone();
        assert_eq!(last.messages, [2]);
        assert_eq!(last.title, "Tom · Work");

        // Only read mail: nothing new to say.
        center.new_mail(
            &recorder,
            7,
            "Work",
            vec![message(3, "Kim", "x", true)],
            &on(),
            OnScreen::default(),
            NOW,
            &texts(),
            |_| true,
        );
        assert_eq!(recorder.shown.borrow().len(), 2);

        center.clear(&recorder, 7);
        assert_eq!(*recorder.withdrawn.borrow(), ["new-mail-7"]);
        assert!(center.pending(7).is_empty());
        center.clear(&recorder, 7);
        assert_eq!(recorder.withdrawn.borrow().len(), 1);
    }

    #[test]
    fn accounts_get_separate_notices() {
        let recorder = Recorder::default();
        let mut center = NotificationCenter::default();
        for (account, label) in [(1, "Home"), (2, "Work")] {
            center.new_mail(
                &recorder,
                account,
                label,
                vec![message(account * 10, "Lena", "Hi", false)],
                &on(),
                OnScreen::default(),
                NOW,
                &texts(),
                |_| true,
            );
        }
        let ids: Vec<String> = recorder
            .shown
            .borrow()
            .iter()
            .map(|n| n.id.clone())
            .collect();
        assert_eq!(ids, ["new-mail-1", "new-mail-2"]);
        assert_eq!(recorder.shown.borrow()[1].title, "Lena · Work");
    }
}
