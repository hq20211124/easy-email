//! IMAP modified UTF-7 (RFC 3501 §5.1.3).
//!
//! Mailbox names are not UTF-8 on the wire. Printable ASCII passes through unchanged, `&` is
//! written as `&-`, and everything else is UTF-16BE in a base64 variant that uses `,` where
//! standard base64 uses `/`, omits padding, and is wrapped in `&`…`-`.
//!
//! Skipping this is not a cosmetic problem: a Chinese folder arrives as `&UXZO1mWHTvZZOQ-`
//! (其他文件夹), which is unreadable in the folder list.
//!
//! Only decoding lives here. The wire form stays authoritative for commands — see
//! [`crate::model::MailboxInfo`], which carries both.

/// Base64 variant used inside `&`…`-`: `,` replaces `/` and there is no padding.
fn base64_value(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some(u32::from(byte - b'A')),
        b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
        b'+' => Some(62),
        b',' => Some(63),
        _ => None,
    }
}

/// Decode one `&`…`-` run (the contents between the delimiters) into a string.
fn decode_run(encoded: &str) -> Option<String> {
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    let mut bytes: Vec<u8> = Vec::with_capacity(encoded.len());

    for byte in encoded.bytes() {
        let value = base64_value(byte)?;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push(((accumulator >> bits) & 0xFF) as u8);
        }
    }

    // Leftover bits are padding; a stray byte means the run was malformed.
    if !bytes.len().is_multiple_of(2) {
        return None;
    }

    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect();

    String::from_utf16(&units).ok()
}

/// Decode an IMAP mailbox name for display.
///
/// Always safe to call, including on plain ASCII names, which pass through unchanged. Do **not**
/// try to skip it with `is_ascii()`: the encoded form is itself pure ASCII
/// (`&UXZO1mWHTvZZOQ-`), so that test silently skips every name that actually needed decoding.
/// The only reliable signal is the presence of `&`, which is what this function looks for.
///
/// Malformed input is returned as-is rather than dropped or replaced: showing the raw encoded
/// name is strictly better than showing nothing, and it keeps a decoding bug visible instead of
/// silently mangling a folder the user needs to click.
pub fn decode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);

        let after = &rest[amp + 1..];
        let Some(dash) = after.find('-') else {
            // No terminator: emit the remainder verbatim.
            out.push('&');
            out.push_str(after);
            return out;
        };

        let encoded = &after[..dash];
        if encoded.is_empty() {
            // "&-" is an escaped literal ampersand.
            out.push('&');
        } else {
            match decode_run(encoded) {
                Some(text) => out.push_str(&text),
                None => {
                    out.push('&');
                    out.push_str(encoded);
                    out.push('-');
                }
            }
        }

        rest = &after[dash + 1..];
    }

    out.push_str(rest);
    out
}

/// True when the name contains something that needs decoding.
///
/// Note this tests for `&`, **not** for non-ASCII: an encoded run is made of ASCII characters,
/// so a non-ASCII test would answer "no decoding needed" for exactly the names that need it.
pub fn needs_decoding(name: &str) -> bool {
    name.contains('&')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_real_chinese_folder_name() {
        // Captured from a live QQ Mail account.
        assert_eq!(decode("&UXZO1mWHTvZZOQ-"), "其他文件夹");
        // A separator and plain ASCII following an encoded run must survive untouched.
        assert_eq!(decode("&UXZO1mWHTvZZOQ-/INBOX"), "其他文件夹/INBOX");
    }

    #[test]
    fn ascii_passes_through_untouched() {
        for name in [
            "INBOX",
            "Sent",
            "Deleted Messages",
            "Drafts",
            "[Gmail]/All Mail",
        ] {
            assert_eq!(decode(name), name, "should not touch {name}");
        }
    }

    #[test]
    fn escaped_ampersand_becomes_a_literal_one() {
        assert_eq!(decode("A&-B"), "A&B");
        assert_eq!(decode("&-"), "&");
        assert_eq!(decode("Tom &- Jerry"), "Tom & Jerry");
    }

    #[test]
    fn mixes_ascii_and_encoded_runs() {
        // Verified vector: 其他文件夹 is &UXZO1mWHTvZZOQ-. The ASCII on either side must survive.
        assert_eq!(decode("A&UXZO1mWHTvZZOQ-B"), "A其他文件夹B");
        // Two encoded runs separated by ASCII.
        assert_eq!(
            decode("&UXZO1mWHTvZZOQ- / &UXZO1mWHTvZZOQ-"),
            "其他文件夹 / 其他文件夹"
        );
    }

    #[test]
    fn empty_input_is_empty() {
        assert_eq!(decode(""), "");
    }

    #[test]
    fn malformed_input_is_preserved_not_dropped() {
        // Unterminated run: keep everything, including the ampersand.
        assert_eq!(decode("&UXZO1mWHTvZZOQ"), "&UXZO1mWHTvZZOQ");
        // Invalid base64 character inside a run: keep the original text.
        assert_eq!(decode("&!!!!-"), "&!!!!-");
        // "AA" is 12 bits, i.e. one and a half bytes — cannot be UTF-16.
        assert_eq!(decode("&AA-"), "&AA-");
    }

    #[test]
    fn invalid_utf16_is_preserved() {
        // 0xD800 is a lone high surrogate and cannot stand alone.
        assert_eq!(decode("&2AA-"), "&2AA-");
    }

    #[test]
    fn base64_alphabet_uses_comma_for_sixty_three() {
        assert_eq!(base64_value(b','), Some(63));
        assert_eq!(
            base64_value(b'/'),
            None,
            "standard base64 / must be rejected"
        );
        assert_eq!(base64_value(b'='), None, "padding must be rejected");
        assert_eq!(base64_value(b'+'), Some(62));
    }

    /// The regression this guards: `needs_decoding` must not be implemented as `!is_ascii()`,
    /// because an encoded run is pure ASCII and would then be skipped.
    #[test]
    fn needs_decoding_is_about_the_ampersand_not_ascii() {
        assert!(!needs_decoding("INBOX"));
        assert!(needs_decoding("&UXZO1mWHTvZZOQ-"));
        assert!(
            "&UXZO1mWHTvZZOQ-".is_ascii(),
            "an encoded run really is ASCII — which is why the old is_ascii() guard was wrong"
        );
    }
}
