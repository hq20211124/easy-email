//! Attachment identity: the hash that says "this is still the file the user chose".
//!
//! `design/07` §5.5. A draft stores each attachment as a **path**, not bytes — a 100 MB attachment
//! must not make `user.db` a second copy of the mail — and a path is not identity: the file can be
//! edited, replaced or deleted between adding it and sending. Sending the new contents would deliver
//! something the user never approved, and it is not a thing they could notice from the composer,
//! which still shows the old name and size.
//!
//! So each attachment carries the SHA-256 of the bytes that were there when it was added, and the
//! send path compares. The comparison is deliberately **one-sided on purpose**:
//!
//! * a recorded hash that no longer matches ⇒ the file changed,
//! * a recorded hash that cannot be recomputed ⇒ the file is gone,
//! * **no recorded hash** ⇒ nothing to compare, so nothing is claimed: an old draft written before
//!   this existed is completed by hashing the file at send time rather than being refused.
//!
//! Hashing streams in chunks so a large attachment costs a constant amount of memory.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{MailError, Result};
use crate::model::{OutgoingAttachment, OutgoingMail};

/// How much is read at a time. `design/07` §5.5 asks for 256 KB chunks; the number matters because
/// it is the whole reason hashing a 100 MB file is safe to do on the UI's thread budget.
const CHUNK: usize = 256 * 1024;

/// The SHA-256 of a file's contents, lower-case hex.
pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .map_err(|e| MailError::Config(format!("读不到附件「{}」：{e}", path.display())))?;

    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| MailError::Config(format!("读取附件「{}」失败：{e}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(to_hex(&hasher.finalize()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// What is wrong with an attachment the user asked to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentIssue {
    /// The file is no longer readable at the recorded path — deleted, renamed, or unmounted.
    Missing,
    /// The file is there but its contents are not what was approved.
    Changed,
}

impl AttachmentIssue {
    /// The reason key `design/07` §5.3 uses for "this needs a human".
    pub fn reason(self) -> &'static str {
        match self {
            Self::Missing => "attachment_missing",
            Self::Changed => "attachment_changed",
        }
    }

    /// What the user reads. Names the file, because they have to make a decision about *that* file.
    pub fn message(self, name: &str) -> String {
        match self {
            Self::Missing => format!(
                "附件「{name}」找不到了（被删除、改名或移动过）。\
                 请重新添加这个文件，或把它从这个草稿里去掉。"
            ),
            Self::Changed => format!(
                "附件「{name}」在添加之后被改动过，内容和当初选择时不一样。\
                 确认要用现在这个版本发送，或重新添加。"
            ),
        }
    }
}

/// Compare one attachment against the file on disk.
///
/// `None` means "nothing to report": the hash matches, or none was ever recorded.
pub fn check_attachment(meta: &OutgoingAttachment) -> Option<AttachmentIssue> {
    let recorded = meta.sha256.as_deref().filter(|h| !h.is_empty())?;

    match file_sha256(Path::new(&meta.path)) {
        Ok(current) if current.eq_ignore_ascii_case(recorded) => None,
        Ok(_) => Some(AttachmentIssue::Changed),
        Err(_) => Some(AttachmentIssue::Missing),
    }
}

/// Fill in the attachments whose hash was never computed, and check the ones that have one.
///
/// This is "发送前补算" from `design/07` §7: a draft written before hashes existed — or one whose
/// picker did not manage to hash a file — is completed here rather than refused, and once completed
/// it stays recorded, so the *next* send can tell whether the file changed in the meantime.
///
/// Called on the way into the outbox, where the bytes are about to be frozen: this is the last
/// moment at which "the file is not what you chose" can still be a question instead of a delivery.
pub fn prepare_attachments(mail: &mut OutgoingMail) -> Result<()> {
    for meta in &mut mail.attachments {
        if meta.sha256.as_deref().is_none_or(str::is_empty) {
            meta.sha256 = Some(file_sha256(Path::new(&meta.path))?);
            continue;
        }

        if let Some(issue) = check_attachment(meta) {
            return Err(MailError::Config(issue.message(&meta.name)));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "easy-email-attach-{}-{}-{}",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&path, contents).expect("write fixture");
        path
    }

    fn attachment(path: &std::path::Path, sha256: Option<&str>) -> OutgoingAttachment {
        OutgoingAttachment {
            name: "报表.pdf".to_string(),
            mime: "application/pdf".to_string(),
            path: path.to_string_lossy().to_string(),
            size: 0,
            sha256: sha256.map(str::to_string),
        }
    }

    #[test]
    fn the_hash_is_the_documented_sha256() {
        // An empty file's SHA-256 is a fixed public value, so this pins the algorithm and the
        // lower-case hex encoding rather than merely checking that the function is deterministic.
        let path = temp_file("empty", b"");
        assert_eq!(
            file_sha256(&path).expect("hash"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_larger_than_one_chunk_hashes_the_same_as_a_whole_read() {
        // The chunked loop is the reason a 100 MB attachment is affordable; a loop that dropped or
        // reread a chunk would still produce *a* hash, just not the right one.
        let mut contents = vec![b'x'; CHUNK * 2 + 7];
        contents[CHUNK] = b'y';
        contents[CHUNK * 2] = b'z';
        let path = temp_file("chunked", &contents);

        let expected = {
            let mut hasher = Sha256::new();
            hasher.update(&contents);
            to_hex(&hasher.finalize())
        };
        assert_eq!(file_sha256(&path).expect("hash"), expected);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unchanged_file_is_not_reported() {
        let path = temp_file("same", b"hello");
        let recorded = file_sha256(&path).expect("hash");
        assert_eq!(check_attachment(&attachment(&path, Some(&recorded))), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_changed_is_reported_as_changed() {
        let path = temp_file("changed", b"hello");
        let recorded = file_sha256(&path).expect("hash");
        std::fs::write(&path, b"hello, world").expect("edit fixture");
        assert_eq!(
            check_attachment(&attachment(&path, Some(&recorded))),
            Some(AttachmentIssue::Changed)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_vanished_is_reported_as_missing() {
        let path = temp_file("gone", b"hello");
        let recorded = file_sha256(&path).expect("hash");
        std::fs::remove_file(&path).expect("remove fixture");
        assert_eq!(
            check_attachment(&attachment(&path, Some(&recorded))),
            Some(AttachmentIssue::Missing)
        );
    }

    #[test]
    fn an_attachment_with_no_recorded_hash_claims_nothing() {
        // Old drafts predate hashing. "We cannot check" must not be reported as "it changed".
        let path = temp_file("nohash", b"hello");
        assert_eq!(check_attachment(&attachment(&path, None)), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn preparing_fills_in_missing_hashes_and_then_notices_edits() {
        let path = temp_file("prepare", b"first version");
        let mut mail = OutgoingMail {
            to: vec!["a@example.com".into()],
            cc: vec![],
            bcc: vec![],
            subject: "s".into(),
            body_text: "b".into(),
            body_html: None,
            in_reply_to: None,
            references: vec![],
            attachments: vec![attachment(&path, None)],
            message_id: None,
        };

        prepare_attachments(&mut mail).expect("a missing hash is filled in, not refused");
        let recorded = mail.attachments[0].sha256.clone().expect("now recorded");
        assert_eq!(recorded, file_sha256(&path).expect("hash"));

        // Unchanged a second time: nothing to say.
        prepare_attachments(&mut mail).expect("idempotent");

        // Edited after it was recorded: that is the case the whole module exists for.
        std::fs::write(&path, b"second version").expect("edit fixture");
        let error = prepare_attachments(&mut mail).expect_err("must refuse");
        assert!(
            error.to_string().contains("报表.pdf"),
            "the message has to name the file: {error}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_refused_with_the_path_the_user_knows() {
        let path = std::env::temp_dir().join("easy-email-does-not-exist-attach.pdf");
        let mut mail = OutgoingMail {
            to: vec!["a@example.com".into()],
            cc: vec![],
            bcc: vec![],
            subject: "s".into(),
            body_text: "b".into(),
            body_html: None,
            in_reply_to: None,
            references: vec![],
            attachments: vec![attachment(&path, Some("deadbeef"))],
            message_id: None,
        };

        let error = prepare_attachments(&mut mail).expect_err("must refuse");
        assert!(error.to_string().contains("找不到了"), "{error}");
    }

    #[test]
    fn the_reason_keys_are_the_ones_the_worker_recognises() {
        use crate::outbox::needs_user_decision;
        for issue in [AttachmentIssue::Missing, AttachmentIssue::Changed] {
            assert!(
                needs_user_decision(Some(issue.reason())),
                "{:?} must be a case the worker refuses to retry",
                issue
            );
        }
    }
}
