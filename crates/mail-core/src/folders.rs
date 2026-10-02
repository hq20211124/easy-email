//! Human-readable names and ordering for the folders a server exposes.
//!
//! IMAP mailbox names are wire identifiers, not UI labels. A server sends `INBOX`,
//! `Sent Messages`, `Deleted Messages`, `Junk`, and a Chinese-language client that shows those
//! verbatim looks unfinished. This module turns them into labels and a stable order.
//!
//! Two signals, in order of trust:
//!
//! 1. **RFC 6154 special-use attributes** (`\Sent`, `\Drafts`, `\Trash`, `\Junk`, `\Archive`,
//!    `\All`, `\Flagged`). Authoritative when present, and language-independent.
//! 2. **The conventional English name.** Plenty of servers — QQ among them — send no special-use
//!    attributes at all and rely on the name alone, which is why this fallback is not optional.
//!
//! Only the **leaf** of a path is localised: `其他文件夹/QQ邮件订阅` keeps its parent, because the
//! parent is the user's own data rather than a well-known role. And this never touches
//! [`crate::model::MailboxInfo::name`] — that stays in wire form, and is the only form sent back in
//! commands.

use std::fmt;

/// A folder role the client knows how to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Inbox,
    Drafts,
    Sent,
    Archive,
    Junk,
    Trash,
    All,
    Flagged,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Inbox => "收件箱",
            Role::Drafts => "草稿箱",
            Role::Sent => "已发送",
            Role::Archive => "归档",
            Role::Junk => "垃圾邮件",
            Role::Trash => "已删除",
            Role::All => "全部邮件",
            Role::Flagged => "星标邮件",
        }
    }

    /// Sidebar order. Sorting localised labels alphabetically would put them in an order that means
    /// nothing to a reader (`其他文件夹` before `已删除` before `收件箱`); this is the order people
    /// actually expect. Unknown folders sort after all of these.
    pub fn rank(self) -> u8 {
        match self {
            Role::Inbox => 0,
            Role::Drafts => 1,
            Role::Sent => 2,
            Role::Archive => 3,
            Role::Junk => 4,
            Role::Trash => 5,
            Role::All => 6,
            Role::Flagged => 7,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Rank used for folders with no known role; after every known role.
pub const UNKNOWN_RANK: u8 = 100;

/// The conventional English names servers use, lowercased for comparison.
///
/// Several spellings per role on purpose: `Deleted Messages` (Dovecot/Coremail), `Trash`,
/// `Deleted Items` (Exchange) and `Bin` are all the same folder to a user.
fn role_from_ascii_name(leaf: &str) -> Option<Role> {
    let key = leaf.trim().to_ascii_lowercase();
    let role = match key.as_str() {
        "inbox" => Role::Inbox,
        "drafts" | "draft" | "草稿箱" => Role::Drafts,
        "sent" | "sent messages" | "sent items" | "sent mail" | "sent-mail" => Role::Sent,
        "archive" | "archives" | "归档" => Role::Archive,
        "junk" | "junk e-mail" | "junk email" | "spam" | "bulk mail" => Role::Junk,
        "trash" | "deleted" | "deleted messages" | "deleted items" | "bin" => Role::Trash,
        "all" | "all mail" => Role::All,
        "flagged" | "starred" => Role::Flagged,
        _ => return None,
    };
    Some(role)
}

/// The label for one mailbox, plus the role it was derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labelled {
    /// What the sidebar shows.
    pub label: String,
    /// The role the label came from — **including** the English-name fallback.
    ///
    /// Ordering must use this rather than the special-use attribute alone: a server that sends no
    /// attributes at all (QQ, for one) would otherwise give every folder an unknown role, and 收件箱
    /// would not sort first. The loopback test caught exactly that.
    pub role: Option<Role>,
}

/// Decide the sidebar label for one mailbox.
///
/// * `decoded` — the name already decoded from modified UTF-7, i.e. what a human would read.
/// * `delimiter` — the server's folder separator, used to split off the parent path.
/// * `special_use` — the role implied by RFC 6154 attributes, if the server sent any.
pub fn label_for(decoded: &str, delimiter: Option<&str>, special_use: Option<Role>) -> Labelled {
    // Split on the delimiter to find the leaf. An empty delimiter means a flat namespace.
    let (prefix, leaf) = match delimiter.filter(|d| !d.is_empty()) {
        Some(delimiter) => match decoded.rsplit_once(delimiter) {
            Some((parent, leaf)) => (Some(parent), leaf),
            None => (None, decoded),
        },
        None => (None, decoded),
    };

    let role = special_use.or_else(|| role_from_ascii_name(leaf));
    let label = match (role, prefix) {
        (Some(role), Some(parent)) => {
            format!("{parent}{}{}", delimiter.unwrap_or("/"), role.label())
        }
        (Some(role), None) => role.label().to_string(),
        // Not a role we know: show the server's own name, which is already decoded.
        (None, _) => decoded.to_string(),
    };

    Labelled { label, role }
}

/// Sidebar sort key: known roles in their conventional order, then everything else by label.
pub fn sort_key(label: &str, role: Option<Role>) -> (u8, String) {
    (role.map_or(UNKNOWN_RANK, Role::rank), label.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most assertions only care about the text.
    fn label(decoded: &str, delimiter: Option<&str>, special_use: Option<Role>) -> String {
        label_for(decoded, delimiter, special_use).label
    }

    #[test]
    fn well_known_english_names_become_chinese() {
        // The exact set a QQ account sends, which is why this exists at all.
        assert_eq!(label("INBOX", None, None), "收件箱");
        assert_eq!(label("Sent Messages", None, None), "已发送");
        assert_eq!(label("Deleted Messages", None, None), "已删除");
        assert_eq!(label("Drafts", None, None), "草稿箱");
        assert_eq!(label("Junk", None, None), "垃圾邮件");
    }

    #[test]
    fn matching_is_case_and_space_insensitive() {
        assert_eq!(label("  sent  ", None, None), "已发送");
        assert_eq!(label("TRASH", None, None), "已删除");
        assert_eq!(label("Junk E-mail", None, None), "垃圾邮件");
    }

    #[test]
    fn special_use_wins_over_the_name() {
        // A server may name it anything at all; the attribute is the authority.
        assert_eq!(label("Gesendet", None, Some(Role::Sent)), "已发送");
        assert_eq!(
            label("Elementos enviados", None, Some(Role::Sent)),
            "已发送"
        );
    }

    #[test]
    fn the_name_fallback_still_reports_its_role() {
        // A server with no RFC 6154 attributes at all (QQ) must still rank by the name it sent.
        // Ordering off the attribute alone is the bug the loopback test caught: INBOX sorted last.
        let inbox = label_for("INBOX", Some("/"), None);
        assert_eq!(inbox.label, "收件箱");
        assert_eq!(inbox.role, Some(Role::Inbox));

        let sent = label_for("Sent Messages", Some("/"), None);
        assert_eq!(sent.role, Some(Role::Sent));
    }

    #[test]
    fn only_the_leaf_of_a_path_is_localised() {
        // The parent is the user's own folder, so it must survive untouched.
        assert_eq!(label("Archive/Sent", Some("/"), None), "Archive/已发送");
        assert_eq!(
            label("其他文件夹/QQ邮件订阅", Some("/"), None),
            "其他文件夹/QQ邮件订阅",
            "an unknown leaf keeps the server's name"
        );
    }

    #[test]
    fn unknown_folders_keep_their_own_name() {
        assert_eq!(label("Project Falcon", None, None), "Project Falcon");
        assert_eq!(label("QQ邮件订阅", None, None), "QQ邮件订阅");
    }

    #[test]
    fn ordering_puts_inbox_first_and_unknown_folders_last() {
        let mut folders: Vec<Labelled> = [
            "Junk",
            "QQ邮件订阅",
            "INBOX",
            "Sent Messages",
            "Deleted Messages",
            "Drafts",
        ]
        .into_iter()
        .map(|name| label_for(name, None, None))
        .collect();
        folders.sort_by_key(|entry| sort_key(&entry.label, entry.role));

        let order: Vec<&str> = folders.iter().map(|entry| entry.label.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "收件箱",
                "草稿箱",
                "已发送",
                "垃圾邮件",
                "已删除",
                "QQ邮件订阅"
            ]
        );
    }

    #[test]
    fn ordering_survives_a_server_that_sends_no_attributes() {
        // Mirrors the loopback fixture: `Sent` carries \Sent, everything else carries nothing.
        let mut folders: Vec<Labelled> = [
            ("Sent", Some(Role::Sent)),
            ("[Gmail]", None),
            ("&UXZO1mWHTvZZOQ-", None),
            ("INBOX", None),
        ]
        .into_iter()
        .map(|(name, special)| label_for(name, None, special))
        .collect();
        folders.sort_by_key(|entry| sort_key(&entry.label, entry.role));

        assert_eq!(folders[0].label, "收件箱", "INBOX must still come first");
        assert_eq!(folders[1].label, "已发送");
    }
}
