//! What happened to the last update attempt.
//!
//! `design/17` §5 has two lines that need somewhere to live: an update that installs itself while the
//! app is closed, and the failure case that follows it — "更新失败，已回退到当前版本" plus one line of
//! reason. Neither the download nor the installer is here (they need an update source and a signing
//! key, which are decisions rather than code); what *is* here is the bookkeeping that makes those two
//! lines true:
//!
//! 1. Before starting the installer, write down **which version we are trying to become**.
//! 2. On the next startup, compare that note with the version we are actually running.
//!    * Same ⇒ the update landed. Forget the note and say so.
//!    * Different ⇒ the installer did not take effect. Forget the note and say *that*, because a
//!      silent failure is how a user ends up pressing "restart to update" forever.
//!    * No note ⇒ nothing to say; this is the normal startup.
//!
//! The note is a file next to the account list rather than a row in a database: it is written by the
//! process that is about to exit, read once by the next one, and has to survive the app being replaced
//! under it. A `JSON` file that a failed write leaves malformed is handled by ignoring it — the cost of
//! ignoring is one missing sentence; the cost of trusting a half-written one is a wrong claim about
//! whether the user's app is up to date.

use crate::error::{MailError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The name of the note, inside the app's data directory.
const FILE_NAME: &str = "update-state.json";

/// What a startup found out about the update that was supposed to have happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The installer ran and this is the new version.
    Updated { version: String },
    /// The installer was supposed to run and we are still the old version.
    StillOld { expected: String, running: String },
}

/// The note left by the process that started the installer.
#[derive(Debug, Serialize, Deserialize)]
struct Note {
    /// The version the installer was going to install.
    version: String,
    /// Free text from whoever started it — shown to the user when the update did not take.
    #[serde(default)]
    reason: Option<String>,
}

/// Where the note lives, and the two operations that matter.
pub struct UpdateState {
    path: PathBuf,
}

impl UpdateState {
    /// `dir` is the app's own data directory.
    pub fn new(dir: impl Into<PathBuf>) -> UpdateState {
        UpdateState {
            path: dir.into().join(FILE_NAME),
        }
    }

    /// Record that an update to `version` is about to be installed.
    ///
    /// Called *before* launching the installer, so a crash between the two still leaves the note.
    pub fn begin(&self, version: &str, reason: Option<&str>) -> Result<()> {
        let note = Note {
            version: version.to_string(),
            reason: reason.map(str::to_string),
        };
        let json = serde_json::to_vec(&note)
            .map_err(|error| MailError::Config(format!("无法序列化更新状态：{error}")))?;
        std::fs::write(&self.path, json)
            .map_err(|error| MailError::Config(format!("无法写入更新状态：{error}")))
    }

    /// Read and clear the note, deciding what to tell the user.
    ///
    /// Clearing is part of reading: the sentence is about *this* startup, and repeating it every
    /// launch would turn a one-time report into nagging.
    pub fn settle(&self, running_version: &str) -> Result<Option<Outcome>> {
        let raw = match std::fs::read(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            // An unreadable note must not stop the app from starting; it only costs the sentence.
            Err(_) => return Ok(None),
        };
        let _ = std::fs::remove_file(&self.path);

        let Ok(note) = serde_json::from_slice::<Note>(&raw) else {
            // Malformed means it was never finished being written. Saying nothing beats guessing.
            return Ok(None);
        };

        if note.version == running_version {
            Ok(Some(Outcome::Updated {
                version: note.version,
            }))
        } else {
            Ok(Some(Outcome::StillOld {
                expected: note.version,
                running: running_version.to_string(),
            }))
        }
    }

    /// The note's path, for a log line when something goes wrong.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> TempDir {
            let path = std::env::temp_dir().join(format!(
                "easy-email-update-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_ordinary_startup_has_nothing_to_say() {
        let dir = TempDir::new("quiet");
        let state = UpdateState::new(&dir.0);
        assert_eq!(state.settle("0.1.0").expect("settle"), None);
    }

    #[test]
    fn the_update_that_landed_is_reported_once_and_then_forgotten() {
        let dir = TempDir::new("landed");
        let state = UpdateState::new(&dir.0);

        state.begin("0.2.0", None).expect("begin");
        assert_eq!(
            state.settle("0.2.0").expect("settle"),
            Some(Outcome::Updated {
                version: "0.2.0".into()
            })
        );
        assert_eq!(
            state.settle("0.2.0").expect("settle"),
            None,
            "the second launch must not repeat the good news"
        );
    }

    #[test]
    fn an_update_that_did_not_take_effect_says_so_with_both_versions() {
        // The design's "更新失败，已回退到当前版本" plus one line of reason: without this the user is
        // left pressing "restart to update" and wondering why the version never changes.
        let dir = TempDir::new("still-old");
        let state = UpdateState::new(&dir.0);

        state.begin("0.2.0", Some("安装器退出码 1")).expect("begin");
        assert_eq!(
            state.settle("0.1.0").expect("settle"),
            Some(Outcome::StillOld {
                expected: "0.2.0".into(),
                running: "0.1.0".into()
            })
        );
        assert_eq!(state.settle("0.1.0").expect("settle"), None);
    }

    #[test]
    fn a_half_written_note_is_ignored_rather_than_claimed() {
        let dir = TempDir::new("half");
        let state = UpdateState::new(&dir.0);
        std::fs::write(state.path(), b"{\"version\": \"0.2").expect("write");

        assert_eq!(
            state.settle("0.1.0").expect("settle"),
            None,
            "a note that was never finished says nothing at all"
        );
        assert!(
            !state.path().exists(),
            "and it is still cleared, so it cannot confuse the next launch either"
        );
    }
}
