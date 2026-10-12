//! Log rotation (`design/16` §3.4: 8 MB per file, keep seven).
//!
//! `startup-error.log` was append-forever: it only ever grew, and the two consequences are both
//! real. A long-term user's log reaches tens of megabytes, and the diagnostics bundle then reads a
//! file the size of the problem it describes; and nothing ever cleans it up, because the directory
//! belongs to the app and the file has no policy.
//!
//! The rules are pure here and the three filesystem moves stay in the caller. That split is the same
//! one the rest of this crate makes: the decisions a test can check do not need a disk, and the
//! disk does not need to be checked in CI.

/// Per-file size cap. Eight megabytes, from `design/16` §3.4.
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// How many rotated files to keep (`startup-error.1.log` … `.7.log`).
pub const KEEP_LOG_FILES: u32 = 7;

/// The rotated names, oldest-bound first: `.1` is the one that just rotated out.
pub fn rotated_names(keep: u32) -> Vec<String> {
    (1..=keep).map(|n| format!(".{n}.log")).collect()
}

/// Whether a file of this size should rotate before the next write.
///
/// `>=` rather than `>`: the cap is an approximation of "big", and a file that missed the line by
/// one byte is not meaningfully smaller than one that crossed it. The choice is pinned by a test so
/// that changing it is a decision rather than an accident.
pub fn rotation_action(size: u64, max_bytes: u64) -> bool {
    size >= max_bytes
}

/// The last `n` lines, in order.
///
/// From the **tail**, not from the head — "show me the log" means what just happened, and a file
/// that has been rotated away is history the caller can ask for separately. Fewer than `n` lines
/// returns all of them rather than padding.
pub fn tail_lines(text: &str, n: usize) -> Vec<&str> {
    if n == 0 {
        return Vec::new();
    }
    let all: Vec<&str> = text.lines().collect();
    let skip = all.len().saturating_sub(n);
    all.into_iter().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_and_the_keep_are_the_designs_numbers() {
        assert_eq!(MAX_LOG_BYTES, 8 * 1024 * 1024);
        assert_eq!(KEEP_LOG_FILES, 7);
        assert_eq!(rotated_names(7).len(), 7);
        assert_eq!(rotated_names(7)[0], ".1.log", "the newest rotation is .1");
    }

    #[test]
    fn a_file_at_the_cap_rotates_and_one_byte_under_does_not() {
        // The boundary is a decision (`>=`), so it is pinned. Off-by-one here is the difference
        // between "the file stops at 8 MB" and "it stops at 8 MB plus one more write".
        assert!(rotation_action(MAX_LOG_BYTES, MAX_LOG_BYTES));
        assert!(rotation_action(MAX_LOG_BYTES + 1, MAX_LOG_BYTES));
        assert!(!rotation_action(MAX_LOG_BYTES - 1, MAX_LOG_BYTES));
        assert!(rotation_action(MAX_LOG_BYTES * 100, MAX_LOG_BYTES));
        assert!(!rotation_action(0, MAX_LOG_BYTES));
    }

    #[test]
    fn tail_takes_the_end_and_gives_everything_when_there_is_less() {
        let text = "one\ntwo\nthree\nfour";
        assert_eq!(tail_lines(text, 2), vec!["three", "four"]);
        assert_eq!(tail_lines(text, 4), vec!["one", "two", "three", "four"]);
        assert_eq!(tail_lines(text, 99), vec!["one", "two", "three", "four"]);
        assert_eq!(tail_lines(text, 0), Vec::<&str>::new());
        assert_eq!(tail_lines("", 5), Vec::<&str>::new());
        // A file with a trailing newline does not gain an empty last line.
        assert_eq!(tail_lines("a\nb\n", 5), vec!["a", "b"]);
    }
}
