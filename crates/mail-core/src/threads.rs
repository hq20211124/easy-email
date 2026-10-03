//! Conversation identity: which messages belong to the same thread, decided from the headers.
//!
//! `design/07` §2 wants the list to read like conversations. Doing that from the subject (the way
//! `src/threads.js` does as a fallback) is a guess; the `References` chain is a fact, and it is the
//! one field that joins a reply to the message it answers *and* to the message that started it —
//! including a reply the user sent, which a subject+sender key cannot connect (the counterpart is
//! the user themselves on that side).
//!
//! The key is the **root** of the chain: the oldest reference, or the message's own id when it has
//! none. That makes the key symmetric — every message in a thread computes the same value, no matter
//! which one you look at first, and no matter whether the client has ever seen the others.
//!
//! This is a pure function of header strings so it can be pinned by tests; the caller decides where
//! to keep the result.

/// The thread a message belongs to, or an empty string when it carries no identity at all.
///
/// An empty answer is deliberate rather than a made-up key: a message with neither a `Message-ID`
/// nor a `References` chain (a malformed header, or a server that strips them) has nothing to match
/// on, and inventing a key would fold unrelated mail together.
pub fn root_of(message_id: &str, references: &str) -> String {
    // `References` is a whitespace-separated list, oldest first; anything non-`<...>` in it (a
    // display name, a client that wrote it without brackets) is not an id.
    if let Some(first) = references
        .split_whitespace()
        .map(str::trim)
        .find(|token| !token.is_empty())
    {
        return normalise(first);
    }

    let own = message_id.trim();
    if own.is_empty() {
        return String::new();
    }
    normalise(own)
}

/// The canonical form of one message id: wrapped in angle brackets, as `References` requires.
///
/// Clients and servers disagree about whether the brackets belong, and the same message must not
/// land in two threads because one header wrote `<a@b>` and another `a@b`.
fn normalise(raw: &str) -> String {
    let trimmed = raw.trim();
    let inner = trimmed.trim_start_matches('<').trim_end_matches('>');
    format!("<{inner}>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_and_its_original_agree_on_one_key() {
        // The whole point: the reply lists the original in `References`, so both compute the same
        // root and a client that has seen only one of them still puts it in the right thread.
        let original = root_of("<original@example.com>", "");
        let reply = root_of("<reply@example.com>", "<original@example.com>");
        let deep = root_of(
            "<third@example.com>",
            "<original@example.com> <reply@example.com>",
        );
        assert_eq!(original, "<original@example.com>");
        assert_eq!(reply, original);
        assert_eq!(deep, original, "the oldest reference is the root");
    }

    #[test]
    fn brackets_are_not_part_of_the_identity() {
        // Servers and clients disagree about the brackets; the same message must not land in two
        // threads because of it.
        assert_eq!(root_of("a@b", ""), "<a@b>");
        assert_eq!(root_of("<a@b>", ""), "<a@b>");
        assert_eq!(root_of("  <a@b>  ", ""), "<a@b>");
        assert_eq!(
            root_of("<x@y>", "a@b <c@d>"),
            "<a@b>",
            "an unbracketed reference is still the root"
        );
    }

    #[test]
    fn a_message_with_no_identity_gets_no_key() {
        // Not a made-up key: folding every malformed message into one thread would hide mail.
        assert_eq!(root_of("", ""), "");
        assert_eq!(root_of("   ", "   "), "");
    }

    #[test]
    fn a_self_reference_does_not_change_the_root() {
        // Some clients include the message's own id in `References`; the root is still the oldest.
        assert_eq!(
            root_of("<b@x>", "<a@x> <b@x>"),
            "<a@x>",
            "the chain wins over the message's own id"
        );
    }
}
