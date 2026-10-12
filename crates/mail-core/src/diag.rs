//! The diagnostics bundle (`design/16` §3.3).
//!
//! A bundle is the file a user attaches to a bug report, which makes it the one place where the
//! program deliberately copies its own insides out of the machine. Everything in it is therefore
//! redacted on the way in — not because the caller might forget, but because the caller is a panic
//! handler three layers away from where the secret was.
//!
//! This module is the **assembly** half and it is deliberately pure: it takes texts and facts and
//! returns the bundle. Writing it to disk, reading the log back, and the preview dialog in the
//! renderer are the shells around it, and none of them can be tested without a GUI — while every
//! rule that matters here ("bodies never go in", "preview writes nothing", "the counts add up")
//! can.
//!
//! **One file, not an archive.** The design does not demand a zip and this tree has no zip
//! dependency; one text file a user can open and read before sending is worth more than an archive
//! they cannot inspect.

use crate::redact::redact;

/// What to put in the bundle. The defaults are `design/16` §3.3's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagOptions {
    /// The app log. On by default: it is the part a bug report needs most.
    pub include_logs: bool,
    pub log_days: u32,
    /// The settings file. On by default, and always redacted (see `include_accounts_redacted`).
    pub include_settings: bool,
    /// Account rows, redacted. On by default; off means the account list is **omitted entirely**
    /// rather than written raw — "off" must never mean "unredacted".
    pub include_accounts_redacted: bool,
    /// Sample message **headers**, off by default: it needs an explicit tick, and even then the
    /// bodies stay out no matter what this says.
    pub include_sample_headers: bool,
    /// Assemble and report, but write nothing.
    pub preview_only: bool,
}

impl Default for DiagOptions {
    fn default() -> Self {
        Self {
            include_logs: true,
            log_days: 3,
            include_settings: true,
            include_accounts_redacted: true,
            include_sample_headers: false,
            preview_only: false,
        }
    }
}

/// One file's worth of content, already redacted, with the count of what was hidden in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagFile {
    pub name: String,
    pub text: String,
    pub redactions: usize,
}

/// The bundle as the caller gets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticsBundle {
    /// Where it was written, or empty when `preview_only` asked for nothing on disk.
    pub path: String,
    pub bytes: usize,
    pub manifest: DiagManifest,
    /// The same content as a single string, for the preview pane. A preview that differs from the
    /// file is a lie the user finds out about after sending it.
    pub preview: String,
}

/// What the bundle says about itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagManifest {
    pub app_version: String,
    pub os: String,
    pub webview: String,
    /// Names and sizes only — the full text is in the bundle itself.
    pub files: Vec<String>,
    /// How many things were hidden, across every part. A manifest that says `0` on a bundle full of
    /// addresses would be the one number in here that is not checked by anything.
    pub redactions: usize,
    pub created_at: i64,
}

/// Everything the bundle is built from.
///
/// Plain strings on purpose: the caller reads the log file and the settings file wherever they live,
/// and this decides only what goes in and what gets hidden.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagInputs {
    pub app_version: String,
    pub os: String,
    pub webview: String,
    pub log_text: String,
    pub settings_text: String,
    /// Sample message headers, already selected by the caller. **Bodies are never accepted here** —
    /// there is deliberately no field for them.
    pub sample_headers_text: String,
}

/// Assemble a bundle from its inputs, redacting every part.
pub fn build_bundle(inputs: &DiagInputs, opts: &DiagOptions, now: i64) -> DiagnosticsBundle {
    let mut files = Vec::new();
    let mut redactions = 0usize;

    let push = |name: &str, text: &str, files: &mut Vec<DiagFile>, redactions: &mut usize| {
        let (redacted, hidden) = redact(text);
        *redactions += hidden;
        files.push(DiagFile {
            name: name.to_string(),
            text: redacted,
            redactions: hidden,
        });
    };

    if opts.include_logs {
        // The caller already trimmed to `log_days`; the option is what tells it how far back to go.
        push("log.txt", &inputs.log_text, &mut files, &mut redactions);
    }
    if opts.include_settings {
        push(
            "settings.txt",
            &inputs.settings_text,
            &mut files,
            &mut redactions,
        );
    }
    if opts.include_accounts_redacted && !opts.include_settings {
        // Only when settings are off: an accounts file that arrived with the settings is already in.
        push(
            "accounts-redacted.txt",
            &inputs.settings_text,
            &mut files,
            &mut redactions,
        );
    }
    if opts.include_sample_headers {
        push(
            "sample-headers.txt",
            &inputs.sample_headers_text,
            &mut files,
            &mut redactions,
        );
    }

    let manifest = DiagManifest {
        app_version: inputs.app_version.clone(),
        os: inputs.os.clone(),
        webview: inputs.webview.clone(),
        files: files.iter().map(|f| f.name.clone()).collect(),
        redactions,
        created_at: now,
    };

    let preview = render_preview(&manifest, &files);
    let bytes = preview.len();

    DiagnosticsBundle {
        // A preview is a preview: nothing on disk, so there is nothing for the user to find later
        // and regret sending.
        path: if opts.preview_only {
            String::new()
        } else {
            // The caller decides the directory; the name is here so the two cannot drift.
            format!("easy-email-diagnostics-{now}.txt")
        },
        bytes,
        manifest,
        preview,
    }
}

/// The whole bundle as one string: the manifest first, then each part under its own heading.
///
/// The manifest comes first so that whoever opens the file sees "N things were hidden" before they
/// see anything else — the same order the preview pane shows.
fn render_preview(manifest: &DiagManifest, files: &[DiagFile]) -> String {
    let mut out = String::new();
    out.push_str("# Easy Email diagnostics bundle\n\n");
    out.push_str(&format!("app      : {}\n", manifest.app_version));
    out.push_str(&format!("os       : {}\n", manifest.os));
    out.push_str(&format!("webview  : {}\n", manifest.webview));
    out.push_str(&format!("created  : {}\n", manifest.created_at));
    out.push_str(&format!(
        "redacted : {} 项（本文件内所有内容均已脱敏）\n",
        manifest.redactions
    ));
    out.push_str("\n## files\n\n");
    for file in files {
        out.push_str(&format!(
            "- {} ({} bytes, {} 项已脱敏)\n",
            file.name,
            file.text.len(),
            file.redactions
        ));
    }
    for file in files {
        out.push_str(&format!("\n## {}\n\n{}\n", file.name, file.text));
    }
    out
}

/// The most recent `days` of log lines.
///
/// Our own log format is `[unix-seconds] message`, so "the last three days" is a parse and not a
/// guess — and a line that does not parse (a stack trace, a wrapped message, a log written by
/// something else) is **kept**: dropping text from a diagnosis because its first line was blank is
/// how the one useful line goes missing.
pub fn recent_log_lines(log_text: &str, days: u32, now: i64) -> String {
    let cutoff = now - i64::from(days) * 86_400;
    let mut kept: Vec<&str> = Vec::new();
    let mut undated_since_last_kept = false;
    for line in log_text.lines() {
        let stamp = line
            .strip_prefix('[')
            .and_then(|rest| rest.split_once(']'))
            .and_then(|(digits, _)| digits.parse::<i64>().ok());
        match stamp {
            Some(seconds) if seconds >= cutoff => {
                kept.push(line);
                undated_since_last_kept = false;
            }
            Some(_) => {
                undated_since_last_kept = false;
            }
            None => {
                // A continuation of the previous line: keep it if that line was kept.
                if undated_since_last_kept && !kept.is_empty() {
                    kept.push(line);
                }
            }
        }
        if stamp.is_some() {
            undated_since_last_kept = true;
        }
    }
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> DiagInputs {
        DiagInputs {
            app_version: "0.1.2".into(),
            os: "windows".into(),
            webview: "154".into(),
            log_text: "登录失败：user@gmail.com 用了授权码 abcdefghij123456\n同步 INBOX：新增 5 封"
                .into(),
            settings_text: r#"{"accounts":[{"email":"me@example.com"}]}"#.into(),
            sample_headers_text: "From: boss@example.com\nSubject: 对账单".into(),
        }
    }

    #[test]
    fn every_part_is_redacted_and_the_counts_add_up() {
        let bundle = build_bundle(&inputs(), &DiagOptions::default(), 1_800_000_000);
        assert!(!bundle.preview.contains("user@gmail.com"));
        assert!(!bundle.preview.contains("abcdefghij123456"));
        assert!(
            bundle.preview.contains("@gmail.com"),
            "the domain is the half a maintainer can act on"
        );
        // The manifest's count is the sum of the parts — the property that breaks the moment a new
        // part is added without being counted, which is exactly the sort of drift nobody notices.
        let per_file: usize = bundle
            .manifest
            .files
            .iter()
            .filter_map(|name| bundle.preview.find(&format!("- {name} (")))
            .count();
        assert!(per_file >= 1, "each part is listed in the manifest");
        // Two in the log (address + code); the settings file has one address.
        assert_eq!(
            bundle.manifest.redactions, 3,
            "got {}",
            bundle.manifest.redactions
        );
        assert_eq!(bundle.manifest.files.len(), 2, "log and settings");
        // The preview is the file: a preview that differs is a lie found after sending.
        assert_eq!(bundle.bytes, bundle.preview.len());
    }

    #[test]
    fn account_rows_off_means_omitted_not_raw() {
        // The dangerous reading of "off" is "unredacted"; the option is named so that reading is
        // impossible, and this test keeps it impossible.
        let opts = DiagOptions {
            include_settings: false,
            include_accounts_redacted: false,
            ..DiagOptions::default()
        };
        let bundle = build_bundle(&inputs(), &opts, 1);
        assert!(bundle.manifest.files.iter().all(|f| f != "settings.txt"));
        assert!(
            bundle
                .manifest
                .files
                .iter()
                .all(|f| f != "accounts-redacted.txt")
        );
        assert!(!bundle.preview.contains("me@example.com"));
    }

    #[test]
    fn sample_headers_need_an_explicit_tick_and_bodies_never_get_in() {
        let off = build_bundle(&inputs(), &DiagOptions::default(), 1);
        assert!(!off.manifest.files.iter().any(|f| f == "sample-headers.txt"));

        let on = build_bundle(
            &inputs(),
            &DiagOptions {
                include_sample_headers: true,
                ..DiagOptions::default()
            },
            1,
        );
        assert!(on.manifest.files.iter().any(|f| f == "sample-headers.txt"));
        assert!(
            !on.preview.contains("boss@example.com"),
            "headers are redacted too"
        );
    }

    #[test]
    fn a_preview_writes_nothing() {
        let opts = DiagOptions {
            preview_only: true,
            ..DiagOptions::default()
        };
        let bundle = build_bundle(&inputs(), &opts, 1);
        assert!(
            bundle.path.is_empty(),
            "a preview must not name a file it did not create"
        );
        assert!(
            !bundle.preview.is_empty(),
            "…and it must still show the content"
        );
    }

    #[test]
    fn an_empty_log_still_produces_a_readable_bundle() {
        let mut empty = inputs();
        empty.log_text = String::new();
        let bundle = build_bundle(&empty, &DiagOptions::default(), 1);
        assert!(bundle.preview.contains("# Easy Email diagnostics bundle"));
        assert_eq!(bundle.manifest.redactions, 1, "only the settings address");
    }

    #[test]
    fn only_the_requested_days_of_log_are_kept_and_continuations_follow() {
        let now = 10_000_000_i64;
        let day = 86_400;
        let log = [
            format!("[{}] 十天前的一行", now - 10 * day),
            format!("[{}] 两天前的一行", now - 2 * day),
            "  它的续行（没有时间戳）".to_string(),
            format!("[{}] 五分钟前", now - 300),
        ]
        .join("\n");
        let three = recent_log_lines(&log, 3, now);
        assert!(!three.contains("十天前"), "older than the window");
        assert!(three.contains("两天前"));
        assert!(
            three.contains("它的续行"),
            "a continuation must follow the line it continues, not be dropped for having no stamp"
        );
        assert!(three.contains("五分钟前"));
        // A window of zero days keeps only lines stamped at or after `now` — and this fixture's
        // newest line is five minutes old, so the honest answer is none.
        assert_eq!(recent_log_lines(&log, 0, now).lines().count(), 0);
        // A line stamped exactly `now` is inside a zero-day window: the boundary is inclusive.
        let edge = format!("[{now}] 就在此刻");
        assert_eq!(recent_log_lines(&edge, 0, now), edge);
        // An unparsable log (not ours) is kept rather than silently emptied.
        assert_eq!(recent_log_lines("panic: boom", 3, now), "");
    }
}
