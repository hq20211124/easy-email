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

/// The subject with its reply/forward prefixes removed, lower-cased, whitespace collapsed.
///
/// This is the **second** conversation signal, for mail that carries no `References`/`In-Reply-To` at
/// all. Real example that motivated it (2026-10-06): a correspondent sent four separate messages with
/// the subject `王芳 您好`, none of them a reply to anything — four conversations of one, four rows in
/// the list, and the user's reasonable reading is "these are one conversation with that person".
///
/// Repeatedly stripping the prefixes matters for the same reason as in `src/threads.js`: "Re: Fwd: x"
/// is what a long thread looks like, and stripping once leaves a different key.
///
/// **This must stay in step with `normaliseSubject` in `src/threads.js`** — the renderer groups the
/// list it holds with that one, the cache groups the conversation with this one, and a disagreement
/// would make the row count and the bubble count differ. Both are pinned by tests with the same cases.
pub fn normalise_subject(subject: &str) -> String {
    /// Prefixes that already mean "this is a reply" or "this is a forward", in both languages.
    const PREFIXES: [&str; 7] = ["re", "fwd", "fw", "答复", "回覆", "回复", "转发"];

    let mut text = subject.trim().to_string();
    loop {
        let lowered = text.to_lowercase();
        let mut next = None;
        for prefix in PREFIXES {
            if let Some(rest) = lowered.strip_prefix(prefix) {
                let rest = rest.trim_start();
                if let Some(after) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('：')) {
                    next = Some(after.trim_start().to_string());
                    break;
                }
            }
        }
        match next {
            // `Re:` alone is a subject; stripping it to nothing would fold every such message together.
            Some(rest) if !rest.is_empty() => text = rest,
            _ => break,
        }
    }
    // Lower-cased like the renderer's version: `Invoice` and `invoice` from the same sender are the
    // same key, and a grouping key has no business preserving the case somebody typed.
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The subject-and-sender key used **in addition** to the `References` chain.
///
/// Two signals, unioned by the caller: rows sharing a `thread_key` are one conversation (a fact), and
/// rows sharing this key are one conversation too (a heuristic). Union rather than replacement, so
/// nothing that groups today — a thread whose subject someone edited, a thread whose chain reaches
/// back further than the cache — can be split by the heuristic.
///
/// **Only mail the user received participates**: a message whose sender *is* the account gets an empty
/// key. The reason is that a sent message's counterpart is a recipient, and the recipient is not in
/// the header we keep for a row (`to_addrs` arrives only when the body is opened) — so a fresh mail
/// with a familiar subject would otherwise merge with every other fresh mail with that subject. A
/// *reply* the user sent is unaffected: it carries a chain and joins by `thread_key` as before.
///
/// An empty subject or an unknown sender also yields no key: folding every subject-less message
/// together would hide mail, which is the one failure worse than not grouping.
pub fn subject_key(subject: &str, from_addr: &str, account_address: &str) -> String {
    let sender = from_addr.trim().to_ascii_lowercase();
    if sender.is_empty() || sender == account_address.trim().to_ascii_lowercase() {
        return String::new();
    }
    let normalised = normalise_subject(subject);
    if normalised.is_empty() {
        return String::new();
    }
    format!("subj:{normalised}|{sender}")
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

    // ---------------------------------------------------------------- the second signal

    /// The same cases `tools/thread_keys_test.mjs` asserts for `normaliseSubject`. If one side learns a
    /// new prefix and the other does not, the list and the conversation disagree about how many
    /// conversations there are.
    #[test]
    fn subject_prefixes_are_stripped_the_same_way_as_in_the_renderer() {
        for (input, expected) in [
            ("十月报销", "十月报销"),
            ("Re: 十月报销", "十月报销"),
            ("RE: 十月报销", "十月报销"),
            ("Re：十月报销", "十月报销"),
            ("Fwd: 十月报销", "十月报销"),
            ("Fw: 十月报销", "十月报销"),
            ("Re: Fwd: Re: 十月报销", "十月报销"),
            ("回复：十月报销", "十月报销"),
            ("答复: 十月报销", "十月报销"),
            ("转发：十月报销", "十月报销"),
            ("  回覆 :  十月报销  ", "十月报销"),
            ("re : 十月报销", "十月报销"),
        ] {
            assert_eq!(normalise_subject(input), expected, "for {input:?}");
        }
    }

    #[test]
    fn a_bare_prefix_is_kept_because_it_is_the_whole_subject() {
        // Stripping "Re:" to nothing would give every such message the same (empty) key.
        assert_eq!(normalise_subject("Re:"), "re:");
        assert_eq!(normalise_subject("  "), "");
    }

    #[test]
    fn the_subject_key_uses_the_sender_and_ignores_reply_prefixes() {
        let me = "me@example.com";
        // The reported case: four separate messages from one person, one subject.
        let a = subject_key("王芳 您好", "wangfang@example.com", me);
        let b = subject_key("Re: 王芳 您好", "wangfang@example.com", me);
        assert_eq!(a, b, "a reply-shaped subject is the same conversation");
        assert_eq!(a, "subj:王芳 您好|wangfang@example.com");
        // Case of the address is not part of the identity; servers disagree about it.
        assert_eq!(
            subject_key("x", "X@Example.COM", me),
            subject_key("x", "x@example.com", me)
        );
    }

    #[test]
    fn the_subject_key_refuses_the_cases_that_would_hide_mail() {
        let me = "me@example.com";
        // A sender-less message has nothing to match on.
        assert_eq!(subject_key("发票", "", me), "");
        // Neither does a subject-less one: folding all of them together is worse than not grouping.
        assert_eq!(subject_key("", "x@example.com", me), "");
        assert_eq!(subject_key("   ", "x@example.com", me), "");
        // The user's own mail never joins by subject — see the note on the function.
        assert_eq!(subject_key("发票", me, me), "");
        assert_eq!(subject_key("发票", "ME@example.com", me), "");
    }
}
