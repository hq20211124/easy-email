//! The send spool: the bytes of a message, frozen at enqueue time.
//!
//! `design/07` §5.3 settles this after the second review: the message is built **once**, when it is
//! queued, and written to disk. The worker only reads it. That is what makes "the user approved
//! exactly these bytes" true across a retry, a crash, a restart, or a GET of a 20 MB attachment from
//! a slow disk — rebuilding at send time would mean a second chance to produce a different message.
//!
//! Files live in their own directory (not in SQLite): a 25 MB message inside `user.db` would make
//! every backup, vacuum and checkpoint pay for it, and the spool is disposable once the message is
//! out or the user cancels.

use std::path::{Path, PathBuf};

use crate::error::{MailError, Result};

/// A directory of frozen messages, named `<outbox id>.eml`.
#[derive(Debug, Clone)]
pub struct Spool {
    dir: PathBuf,
}

impl Spool {
    /// Use `dir` as the spool, creating it if needed.
    ///
    /// Created eagerly rather than lazily so a permissions problem surfaces at startup, where it can
    /// be logged once, instead of failing somebody's send later.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| {
            MailError::Config(format!("无法创建发件暂存目录 {}：{e}", dir.display()))
        })?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the message for outbox row `id` is kept.
    pub fn path_for(&self, id: i64) -> PathBuf {
        self.dir.join(format!("{id}.eml"))
    }

    /// Write the frozen bytes and report where they went, plus their size.
    pub fn write(&self, id: i64, bytes: &[u8]) -> Result<(PathBuf, u64)> {
        let path = self.path_for(id);
        let tmp = path.with_extension("eml.tmp");
        std::fs::write(&tmp, bytes)
            .map_err(|e| MailError::Config(format!("无法写入发件暂存 {}：{e}", tmp.display())))?;
        // Crash between write and rename must not leave a truncated .eml visible.
        if let Ok(f) = std::fs::File::open(&tmp) {
            let _ = f.sync_all();
        }
        std::fs::rename(&tmp, &path)
            .map_err(|e| MailError::Config(format!("无法提交发件暂存 {}：{e}", path.display())))?;
        Ok((path, bytes.len() as u64))
    }

    /// Read a frozen message back.
    pub fn read(&self, path: &Path) -> Result<Vec<u8>> {
        std::fs::read(path)
            .map_err(|e| MailError::Config(format!("读不到发件暂存 {}：{e}", path.display())))
    }

    /// Delete a frozen message. Missing is success — the point is that it is gone.
    pub fn remove(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    /// Delete spool files that no outbox row points at and that are older than `max_age_secs`.
    ///
    /// The settle order is "record, drop the row, drop the bytes" (`design/07` §14.7), and a crash
    /// between the last two leaves a file nobody references. Without a sweep it stays for ever — one
    /// file per interrupted send, up to the 25 MB per-message ceiling.
    ///
    /// **Both conditions are required.** Unreferenced alone would delete the file a send is writing
    /// right now (the row is inserted before the bytes land), so the age is what makes this safe: a
    /// file younger than `max_age_secs` is left alone even if it looks orphaned at this instant.
    ///
    /// Returns how many were removed.
    pub fn sweep_orphans(
        &self,
        referenced: &std::collections::HashSet<PathBuf>,
        now: i64,
        max_age_secs: i64,
    ) -> u64 {
        let Ok(entries) = std::fs::read_dir(self.dir()) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if referenced.contains(&path) {
                continue;
            }
            // Only this program's own files: the directory is ours, but "ours" is not a reason to
            // delete something a user dropped in there.
            if path.extension().and_then(|e| e.to_str()) != Some("eml") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let age = meta
                .modified()
                .ok()
                .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| now - since.as_secs() as i64)
                .unwrap_or(0);
            if age <= max_age_secs {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "easy-email-spool-{}-{}-{}",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn bytes_round_trip_and_removal_is_idempotent() {
        let dir = temp_dir("round-trip");
        let spool = Spool::open(&dir).expect("open");

        let (path, size) = spool.write(7, b"Subject: hi\r\n\r\nbody").expect("write");
        assert_eq!(path, spool.path_for(7));
        assert_eq!(size, 19, "the size is recorded for the UI's size budget");
        assert_eq!(spool.read(&path).expect("read"), b"Subject: hi\r\n\r\nbody");

        spool.remove(&path);
        assert!(!path.exists());
        // Cancelling a message twice, or after the file was cleaned up, must not error.
        spool.remove(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_spool_file_is_an_error_not_empty_bytes() {
        // Sending zero bytes would "succeed" at the SMTP level and deliver an empty message; the
        // worker has to be able to tell "gone" from "empty" so it can rebuild or ask the user.
        let dir = temp_dir("missing");
        let spool = Spool::open(&dir).expect("open");
        assert!(spool.read(&spool.path_for(99)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The startup sweep: unreferenced **and** old. Both, never either.
    ///
    /// A file with no outbox row is the shape a crash between "row dropped" and "file dropped" leaves
    /// behind (`design/07` §14.7), and nothing else would ever collect it. But the row is inserted
    /// *before* the bytes are written, so "no row right now" is also what a send in progress looks
    /// like — which is why the age is what makes deleting safe.
    #[test]
    fn the_sweep_removes_only_old_unreferenced_files() {
        let dir = temp_dir("sweep");
        let spool = Spool::open(&dir).expect("open");
        let (orphan, _) = spool.write(1, b"Subject: a\r\n\r\n").expect("write");
        let (in_use, _) = spool.write(2, b"Subject: b\r\n\r\n").expect("write");
        let (young, _) = spool.write(3, b"Subject: c\r\n\r\n").expect("write");

        // A file that is not ours must never be swept, however old it looks.
        let foreign = spool.dir().join("notes.txt");
        std::fs::write(&foreign, b"not a message").expect("write");

        let referenced = std::collections::HashSet::from([in_use.clone()]);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;

        // One hour: nothing is old enough, so nothing goes — including the one with no row.
        assert_eq!(spool.sweep_orphans(&referenced, now + 3600, 24 * 3600), 0);
        assert!(young.exists(), "too new to judge: a send may be writing it");

        // Twenty-five hours: both unreferenced files go, the referenced one and the foreign file stay.
        assert_eq!(
            spool.sweep_orphans(&referenced, now + 25 * 3600, 24 * 3600),
            2
        );
        assert!(!orphan.exists(), "an unreferenced old file is a leak");
        assert!(!young.exists());
        assert!(in_use.exists(), "a referenced file is evidence, not litter");
        assert!(foreign.exists(), "and this directory is not ours to empty");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
