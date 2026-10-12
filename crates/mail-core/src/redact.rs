//! Redaction for anything that leaves the process: log lines and the diagnostics bundle.
//!
//! `design/16` §3.4 asks for the rules to be **pinned by tests**, eight categories of them, and that
//! instruction is why this is a module rather than a few `replace` calls at the call sites. A leak
//! here is not a crash: nothing turns red, the bundle is written, and the damage is discovered later
//! by somebody else. So every category has a test that fails loudly if the pattern stops matching,
//! and the "a normal line is left alone" test matters just as much — a redactor that eats everything
//! makes diagnostics useless, and the next person deletes it.
//!
//! **Hand-rolled scanners, not a regex dependency** — the same choice `links.rs` makes: eight shapes
//! is a few hundred lines, and a new dependency earns its compile time against every target we build
//! for.
//!
//! Two rules keep part of what they matched, on purpose:
//!   * an email keeps its **domain** (which provider refused the login is the actionable half);
//!   * a path keeps the `Users\` **prefix** (so the rest of the path still reads).
//!
//! One mask over the whole span was the first version and it threw both away for nothing.

/// What stands in for something hidden. A fixed string, so a reader can see that redaction happened.
pub const MASK: &str = "[已脱敏]";

/// One scanner's answer: how many bytes it claims, and what should appear instead.
pub struct Span {
    len: usize,
    replacement: String,
}

impl Span {
    fn hidden(len: usize) -> Self {
        Self {
            len,
            replacement: MASK.to_string(),
        }
    }

    fn verbatim(len: usize) -> Self {
        Self {
            len,
            replacement: String::new(),
        }
    }

    fn replaced(len: usize, replacement: String) -> Self {
        Self { len, replacement }
    }
}

/// Redact one line (or one multi-line block), reporting how many things were hidden.
pub fn redact(input: &str) -> (String, usize) {
    let mut text = input.to_string();
    let mut total = 0;

    // The order is part of the contract: a private key block spans lines and its body contains
    // base64 runs the token pass would otherwise shred into something less obviously a key.
    let (next, n) = mask_key_blocks(&text);
    text = next;
    total += n;

    for rule in [
        mask_emails as fn(&str) -> (String, usize),
        mask_secrets,
        mask_windows_user,
        mask_phones,
        mask_long_digit_runs,
        mask_authorization_codes,
        mask_ipv4,
    ] {
        let (next, n) = rule(&text);
        text = next;
        total += n;
    }

    (text, total)
}

/// Run one scanner over the text.
///
/// A scanner that claims **zero** replacement (`Span::verbatim`) is how an exact-length rule skips a
/// run it decided not to match: returning "no match" instead would make the outer loop retry one
/// byte later and find the same shape as a *substring* — a 17-character run would be masked for the
/// 16-character one inside it.
fn scan<F>(text: &str, accept: F) -> (String, usize)
where
    F: Fn(&str, usize) -> Option<Span>,
{
    let mut out = String::with_capacity(text.len());
    let mut count = 0;
    let mut i = 0;
    while i < text.len() {
        if let Some(span) = accept(text, i) {
            if span.replacement.is_empty() {
                // Keep the span exactly as it is, but do not reconsider any byte inside it.
                out.push_str(&text[i..i + span.len]);
            } else {
                out.push_str(&span.replacement);
                count += 1;
            }
            i += span.len;
            continue;
        }
        // One character, not one byte: this text is Chinese as often as not.
        let ch = text[i..].chars().next().expect("a char starts at i");
        out.push(ch);
        i += ch.len_utf8();
    }
    (out, count)
}

// ---------------------------------------------------------------- 1. private key blocks

fn mask_key_blocks(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut count = 0;
    while let Some(start) = rest.find("-----BEGIN ") {
        let after = &rest[start..];
        let Some(break_at) = after.find('\n') else {
            break;
        };
        let header = &after[..break_at];
        if !header.contains("PRIVATE KEY") {
            // A certificate, a public key, a signature: public data, leave it be.
            out.push_str(&rest[..start + break_at]);
            rest = &after[break_at..];
            continue;
        }
        out.push_str(&rest[..start]);
        let tail = &after[break_at..];
        match tail.find("-----END ") {
            Some(end_at) => {
                let end_of_line = tail[end_at..]
                    .find('\n')
                    .map(|n| end_at + n)
                    .unwrap_or(tail.len());
                out.push_str(MASK);
                count += 1;
                rest = &tail[end_of_line..];
            }
            None => {
                // A truncated block is still a key: hide the rest rather than leave it.
                out.push_str(MASK);
                count += 1;
                rest = "";
            }
        }
    }
    out.push_str(rest);
    (out, count)
}

// ---------------------------------------------------------------- 2. email addresses

const LOCAL_EXTRA: &[u8] = b"._%+-";

fn mask_emails(text: &str) -> (String, usize) {
    scan(text, |text, at| {
        let bytes = text.as_bytes();
        if bytes[at] == b'@' {
            return None;
        }
        let mut end = at;
        // Bytes above ASCII are accepted on purpose: a Chinese local part (`张三@…`) is a real thing
        // and it is the part of the address that identifies the user. Letting the run cross into
        // non-ASCII is safe here because the `@` that follows is still what ends it.
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric()
                || LOCAL_EXTRA.contains(&bytes[end])
                || bytes[end] >= 0x80)
        {
            end += 1;
        }
        if end == at || end >= bytes.len() || bytes[end] != b'@' {
            return None;
        }
        let mut dom = end + 1;
        let mut dots = 0;
        let mut last_dot = None;
        while dom < bytes.len()
            && (bytes[dom].is_ascii_alphanumeric()
                || bytes[dom] == b'.'
                || bytes[dom] == b'-'
                || bytes[dom] >= 0x80)
        {
            if bytes[dom] == b'.' {
                dots += 1;
                last_dot = Some(dom);
            }
            dom += 1;
        }
        let looks_like_a_domain = dots >= 1
            && last_dot.is_some_and(|d| dom - d >= 3)
            && !matches!(bytes[dom - 1], b'.' | b'-');
        if !looks_like_a_domain {
            return None;
        }
        Some(Span::replaced(
            dom - at,
            format!("{MASK}@{}", &text[end + 1..dom]),
        ))
    })
}

// ---------------------------------------------------------------- 3. tokens and secrets

const SECRET_PREFIXES: [&str; 2] = ["ya29.", "1//"];
const SECRET_RUN: &[u8] = b"._-";
const MIN_SECRET_RUN: usize = 40;

fn mask_secrets(text: &str) -> (String, usize) {
    scan(text, |text, at| {
        let bytes = text.as_bytes();
        for prefix in SECRET_PREFIXES {
            if text[at..].starts_with(prefix) {
                let mut end = at + prefix.len();
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || SECRET_RUN.contains(&bytes[end]))
                {
                    end += 1;
                }
                return Some(Span::hidden(end - at));
            }
        }
        if !bytes[at].is_ascii_alphanumeric() {
            return None;
        }
        let mut end = at;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'+' | b'/' | b'='))
        {
            end += 1;
        }
        if end - at >= MIN_SECRET_RUN {
            Some(Span::hidden(end - at))
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------- 4. Windows paths

fn mask_windows_user(text: &str) -> (String, usize) {
    scan(text, |text, at| {
        const NEEDLE: &str = "Users\\";
        if !text[at..].starts_with(NEEDLE) {
            return None;
        }
        let start = at + NEEDLE.len();
        let bytes = text.as_bytes();
        let mut end = start;
        while end < bytes.len() && !matches!(bytes[end], b'\\' | b'/' | b'\n' | b'\r' | b'"') {
            end += 1;
        }
        if end == start {
            return None;
        }
        Some(Span::replaced(end - at, format!("Users\\{MASK}")))
    })
}

// ---------------------------------------------------------------- 5. phone numbers

fn mask_phones(text: &str) -> (String, usize) {
    scan(text, |text, at| {
        let bytes = text.as_bytes();
        if bytes[at] == b'+' && at + 1 < bytes.len() && bytes[at + 1].is_ascii_digit() {
            let mut end = at + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            return if end - at >= 11 {
                Some(Span::hidden(end - at))
            } else {
                Some(Span::verbatim(end - at))
            };
        }
        if bytes[at] != b'1' {
            return None;
        }
        let mut end = at;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end - at == 11 {
            Some(Span::hidden(end - at))
        } else {
            Some(Span::verbatim(end - at))
        }
    })
}

// ---------------------------------------------------------------- 6. long digit runs

const CARD_DIGITS: std::ops::RangeInclusive<usize> = 15..=19;

fn mask_long_digit_runs(text: &str) -> (String, usize) {
    scan(text, |text, at| {
        let bytes = text.as_bytes();
        if !bytes[at].is_ascii_digit() {
            return None;
        }
        let mut end = at;
        while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b' ') {
            end += 1;
        }
        let len = text[at..end].trim_end().len();
        let digits = text[at..at + len]
            .bytes()
            .filter(|b| b.is_ascii_digit())
            .count();
        if CARD_DIGITS.contains(&digits) {
            Some(Span::hidden(len))
        } else {
            // A size, a count, a date: the diagnosis needs these, so pass them through untouched.
            Some(Span::verbatim(len))
        }
    })
}

// ---------------------------------------------------------------- 7. authorization codes

fn mask_authorization_codes(text: &str) -> (String, usize) {
    scan(text, |text, at| {
        let bytes = text.as_bytes();
        if !bytes[at].is_ascii_alphanumeric() {
            return None;
        }
        let mut end = at;
        while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
            end += 1;
        }
        if end - at == 16 {
            Some(Span::hidden(end - at))
        } else {
            Some(Span::verbatim(end - at))
        }
    })
}

// ---------------------------------------------------------------- 8. IPv4 addresses

fn mask_ipv4(text: &str) -> (String, usize) {
    scan(text, |text, at| {
        let bytes = text.as_bytes();
        if !bytes[at].is_ascii_digit() {
            return None;
        }
        let mut end = at;
        let mut dots = 0;
        let mut in_octet = 0;
        while end < bytes.len()
            && (bytes[end].is_ascii_digit() || (bytes[end] == b'.' && in_octet > 0))
        {
            if bytes[end] == b'.' {
                dots += 1;
                in_octet = 0;
            } else {
                in_octet += 1;
            }
            end += 1;
        }
        if dots == 3 && in_octet > 0 && in_octet <= 3 {
            Some(Span::hidden(end - at))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(input: &str) -> String {
        redact(input).0
    }

    #[test]
    fn an_email_keeps_its_domain_and_hides_the_local_part() {
        let out = one("登录失败：bestbychinas@gmail.com 拒绝了凭据");
        assert!(
            out.contains("@gmail.com"),
            "the domain is the actionable half: {out}"
        );
        assert!(!out.contains("bestbychinas"));
        assert!(one("a.b+tag@example.com").contains("@example.com"));
    }

    #[test]
    fn tokens_and_long_base64_runs_are_hidden() {
        assert!(!one("token ya29.a0AfH6SByX7abcDEFghiJKLmnoPQRs").contains("ya29."));
        assert!(!one("id 1//0eXaMpLeToKeN1234567890abcdefg").contains("1//"));
        assert!(!one(&"A".repeat(64)).contains("AAAA"));
        assert!(
            one("用户点了三次重试").contains("三次"),
            "short words are not secrets"
        );
    }

    #[test]
    fn a_private_key_block_goes_whole() {
        let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA1234567890abcdefghij\nklmnopqrstuvwxyzABCDEFGHIJKLMNOP\n-----END RSA PRIVATE KEY-----\n";
        let out = one(key);
        assert!(
            !out.contains("MIIEpAIBAAKC"),
            "the body went with the block"
        );
        assert!(!out.contains("BEGIN RSA"), "so did the header");
        assert_eq!(out.matches(MASK).count(), 1, "one block, one mask");
    }

    #[test]
    fn a_certificate_is_not_a_private_key_and_stays() {
        let cert = "-----BEGIN CERTIFICATE-----\nMIIEpAIBAAKCAQEA\n-----END CERTIFICATE-----";
        assert!(
            one(cert).contains("CERTIFICATE"),
            "a certificate is public data"
        );
    }

    #[test]
    fn windows_paths_hide_the_user_name() {
        let out = one(r"C:\Users\72492\AppData\Local\Temp\x.log");
        assert!(out.contains("Users\\"), "the prefix stays: {out}");
        assert!(!out.contains("72492"));
        assert!(
            one("/home/myroot/.config/easy-email").contains("/home/myroot"),
            "the Linux path this crate also runs on must survive"
        );
    }

    #[test]
    fn phones_are_hidden_and_other_numbers_are_not() {
        assert!(!one("联系 13800138000 重试").contains("13800138000"));
        assert!(one("+8613800138000").contains(MASK));
        assert!(
            one("同步了 149 封，缓存共 192 封").contains("149"),
            "a count must stay"
        );
    }

    #[test]
    fn identity_and_bank_cards_are_hidden() {
        assert!(!one("11010119900307888X").contains("19900307"));
        assert!(!one("6222021234567890123").contains("62220212"));
        assert!(one("2026-01-01").contains("2026"), "a date is not a card");
    }

    #[test]
    fn an_authorization_code_of_sixteen_characters_is_hidden() {
        let code = "abcdefghij123456";
        assert!(!one(&format!("授权码 {code} 已拒绝")).contains(code));
        assert!(
            one("abcdefghij1234567").contains("abcdefghij1234567"),
            "seventeen characters is not the shape: a false mask here hides real ids"
        );
    }

    #[test]
    fn ipv4_addresses_are_hidden() {
        assert!(!one("连接 111.55.214.44:993 失败").contains("111.55.214.44"));
        assert!(
            one("webview 1.2.3 启动").contains("1.2.3"),
            "a version is not an address"
        );
    }

    #[test]
    fn a_normal_survives_and_redaction_is_idempotent() {
        let line = "同步 INBOX：新增 5 封，缓存共 192 封，用时 1.2 s";
        assert_eq!(one(line), line, "a normal log line must be untouched");
        let once = one("用户 a@b.com 用了 13800138000");
        assert_eq!(
            once,
            redact(&once).0,
            "running it twice must not move anything further"
        );
        assert_eq!(redact("nothing to hide").1, 0);
    }

    #[test]
    fn the_count_says_how_many_things_were_hidden() {
        let (_, n) = redact("a@b.com 和 c@d.com，加上 13800138000");
        assert_eq!(n, 3);
    }

    #[test]
    fn chinese_text_is_not_broken_by_byte_indexing() {
        assert!(one("对账单已同步：账单").contains("对账单"));
        assert!(one("邮箱用户@例子.中国 被拒").contains(MASK));
    }
}
