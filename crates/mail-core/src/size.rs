//! Size parsing for the `larger:` / `smaller:` search prefixes.
//!
//! Split out from `query.rs` for the same reason the parser was: it is pure, it is testable without
//! a database, and it has one decision in it that a reader should be able to find — what a "MB"
//! means. The answer here is decimal (10^6), because that is what every mail client's attachment
//! dialog shows and what a user comparing "10 MB" against a provider's "25 MB limit" is thinking.
//! `MiB` and friends are the binary ones, spelled the way the standards spell them.

/// Parse a size the way a user types one, into bytes.
///
/// Accepts a bare number of bytes, and a number with a unit (`B`, `K`/`KB`, `KiB`, `M`/`MB`, `MiB`,
/// `G`/`GB`, `GiB`), with or without a space, in any letter case. Returns `None` for anything else,
/// which the caller turns into a hint rather than a silently dropped condition.
pub fn parse_size(input: &str) -> Option<u64> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let digits = digits.trim();
    if digits.is_empty() {
        return None;
    }
    let value: f64 = digits.parse().ok()?;
    if value < 0.0 {
        return None;
    }
    let unit = unit.trim().to_ascii_lowercase();
    let multiplier: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1_000.0,
        "kib" => 1_024.0,
        "m" | "mb" => 1_000_000.0,
        "mib" => 1_048_576.0,
        "g" | "gb" => 1_000_000_000.0,
        "gib" => 1_073_741_824.0,
        _ => return None,
    };
    Some((value * multiplier).round() as u64)
}

/// The units the parser understands, for the hint the search box shows.
pub fn known_size_units() -> Vec<&'static str> {
    vec!["B", "KB", "MB", "GB", "KiB", "MiB", "GiB"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_come_in_bare_bytes_and_every_unit() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1k"), Some(1_000));
        assert_eq!(parse_size("1KB"), Some(1_000));
        assert_eq!(parse_size("1kb"), Some(1_000));
        assert_eq!(parse_size("1 KB"), Some(1_000));
        assert_eq!(parse_size("1KiB"), Some(1_024));
        assert_eq!(parse_size("10MB"), Some(10_000_000));
        assert_eq!(parse_size("10 mb"), Some(10_000_000));
        assert_eq!(
            parse_size("2.5MB"),
            Some(2_500_000),
            "a decimal amount is normal in a mail size"
        );
        assert_eq!(parse_size("1GB"), Some(1_000_000_000));
        assert_eq!(parse_size("1GiB"), Some(1_073_741_824));
    }

    #[test]
    fn a_megabyte_is_a_million_and_a_mebibyte_is_a_million_and_a_bit() {
        // The one decision in this file, pinned: decimal units by default, binary when spelled the
        // binary way. A user comparing against "25 MB limit" is counting millions.
        assert_eq!(parse_size("1MB"), Some(1_000_000));
        assert_eq!(parse_size("1MiB"), Some(1_048_576));
        assert_ne!(parse_size("1MB"), parse_size("1MiB"));
    }

    #[test]
    fn nonsense_is_none_rather_than_zero() {
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("big"), None);
        assert_eq!(
            parse_size("10MB/s"),
            None,
            "a unit we do not know must not become 10 bytes"
        );
        assert_eq!(parse_size("-1MB"), None);
        assert_eq!(parse_size(".."), None);
    }
}
