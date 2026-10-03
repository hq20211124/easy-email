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
        std::fs::write(&path, bytes)
            .map_err(|e| MailError::Config(format!("无法写入发件暂存 {}：{e}", path.display())))?;
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
}
