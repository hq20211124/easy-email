//! Where a desktop client's OAuth registration comes from.
//!
//! A1-F37 is blunt about this: a distributed client cannot keep a secret, so the *registration* is
//! what identifies the app — and it has to be the app's own. This module is how "our own" gets in
//! without being compiled into the binary or committed to the repository:
//!
//! * Drop the JSON that Google Cloud gives you ("Desktop app" client) into the app's data directory,
//!   **without renaming it**, and it is used. Accepting Google's own shape and filename means the
//!   instructions are "download, drop it there" rather than "download, edit, rename".
//! * Nothing found ⇒ the caller decides what to fall back to. For a build that has no registration of
//!   its own, that is the *published* Thunderbird registration with the notice that goes with it
//!   (borrowed, revocable, and not ours — see `PROGRESS`, M1a-63).
//!
//! The secret is not treated as one (Google says outright that a desktop `client_secret` is not a
//! secret), but it is still *the user's* registration: this reads it from their data directory rather
//! than from a repository they might publish.

use crate::error::{MailError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which kind of client the downloaded file describes.
///
/// Google's "Desktop app" clients can use a **loopback** redirect with any port; "Web application"
/// clients cannot — they require a pre-registered https redirect. Dropping a web client in therefore
/// fails, and it fails at the provider with `redirect_uri_mismatch`, which says nothing about the real
/// cause (the wrong client *type*). Naming it here is what turns that into one sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// `{"installed": …}` — what the loopback flow needs.
    Desktop,
    /// `{"web": …}` — cannot use a loopback redirect at all.
    Web,
}

/// One OAuth client registration.
///
/// The endpoints come from Google's file too, and they are honoured rather than ignored: the file is
/// the client's own statement of where its authorization and token endpoints live, so a registration
/// for a different environment (a test tenant, a self-hosted IdP that speaks the same flow) works
/// without a code change. When they are absent — a hand-written file — the provider's built-in
/// endpoints are used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRegistration {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default = "desktop")]
    pub kind: ClientKind,
    #[serde(default)]
    pub auth_uri: Option<String>,
    #[serde(default)]
    pub token_uri: Option<String>,
}

fn desktop() -> ClientKind {
    ClientKind::Desktop
}

/// Google's downloaded file, which nests everything under `installed` (or `web`, for the type that
/// cannot use a loopback redirect at all).
#[derive(Debug, Deserialize)]
struct GoogleFile {
    #[serde(default)]
    installed: Option<ClientRegistration>,
    #[serde(default)]
    web: Option<ClientRegistration>,
}

/// The registration to use, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum Registration {
    /// The user's own, read from their data directory.
    Own { registration: ClientRegistration },
    /// Nobody's own registration is available.
    Missing,
}

/// The file we look for first; the user may also just drop Google's file in unchanged.
const PREFERRED: &str = "oauth-client.json";

/// Read the registration from `dir`, if there is one.
///
/// A malformed file is an **error**, not a silent "no registration": the user put it there on purpose,
/// and "it did nothing" is the least useful answer. Renaming the file is never required.
pub fn client_registration(dir: &Path) -> Result<Registration> {
    let preferred = dir.join(PREFERRED);
    if preferred.exists() {
        return read(&preferred).map(|registration| Registration::Own { registration });
    }

    // Google's file is named `client_secret_<id>.apps.googleusercontent.com.json`.
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|error| MailError::Config(format!("无法读取 {}：{error}", dir.display())))?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("client_secret_") && name.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();

    match candidates.first() {
        Some(path) => read(path).map(|registration| Registration::Own { registration }),
        None => Ok(Registration::Missing),
    }
}

/// One file, in either shape.
fn read(path: &Path) -> Result<ClientRegistration> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| MailError::Config(format!("无法读取 {}：{error}", path.display())))?;
    parse(&raw).map_err(|error| {
        MailError::Config(format!(
            "{} 不是有效的 OAuth 客户端文件（{error}）。请放入 Google Cloud 下载的 JSON 原件。",
            path.display()
        ))
    })
}

/// Google's nested shape, or a flat `{client_id, client_secret}` someone wrote by hand.
fn parse(raw: &str) -> std::result::Result<ClientRegistration, String> {
    if let Ok(file) = serde_json::from_str::<GoogleFile>(raw) {
        if let Some(mut registration) = file.installed {
            registration.kind = ClientKind::Desktop;
            return Ok(registration);
        }
        if let Some(mut registration) = file.web {
            // Accepted, with the kind recorded: refusing here would leave the user with "the JSON I
            // downloaded does not work", while the useful sentence is "you downloaded the wrong
            // *type*" — and that one can only be said at the moment somebody tries to sign in.
            registration.kind = ClientKind::Web;
            return Ok(registration);
        }
    }

    let mut flat: ClientRegistration =
        serde_json::from_str(raw).map_err(|error| format!("无法解析：{error}"))?;
    if flat.client_id.trim().is_empty() {
        return Err("client_id 为空".into());
    }
    // A hand-written file has no type to read; the desktop shape is the one this app can use.
    flat.kind = ClientKind::Desktop;
    Ok(flat)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> TempDir {
            let path = std::env::temp_dir().join(format!(
                "easy-email-oauth-client-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }

        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.0.join(name), contents).expect("write");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_empty_directory_means_no_registration_of_our_own() {
        let dir = TempDir::new("empty");
        assert_eq!(
            client_registration(&dir.0).expect("read"),
            Registration::Missing
        );
    }

    #[test]
    fn googles_own_file_works_unchanged_and_unrenamed() {
        // The whole point of accepting Google's shape and filename: the instruction is "download it
        // and drop it in", which is a thing people actually finish.
        let dir = TempDir::new("google-shape");
        dir.write(
            "client_secret_762379360770-abc.apps.googleusercontent.com.json",
            r#"{"installed":{"client_id":"762379360770-abc.apps.googleusercontent.com",
                 "project_id":"x","auth_uri":"https://accounts.google.com/o/oauth2/auth",
                 "token_uri":"https://oauth2.googleapis.com/token",
                 "client_secret":"GOCSPX-not-a-real-secret",
                 "redirect_uris":["http://localhost"]}}"#,
        );

        match client_registration(&dir.0).expect("read") {
            Registration::Own { registration } => {
                assert_eq!(
                    registration.client_id,
                    "762379360770-abc.apps.googleusercontent.com"
                );
                assert_eq!(
                    registration.client_secret.as_deref(),
                    Some("GOCSPX-not-a-real-secret")
                );
            }
            other => panic!("expected the user's registration, got {other:?}"),
        }
    }

    #[test]
    fn a_web_client_is_recorded_as_such_rather_than_refused_here() {
        // Refusing at this point would say "the JSON you downloaded does not work"; the useful sentence
        // names the wrong *type*, and that can only be said when somebody tries to sign in.
        let dir = TempDir::new("web-kind");
        dir.write(
            PREFERRED,
            r#"{"web":{"client_id":"web-client","client_secret":"s"}}"#,
        );
        match client_registration(&dir.0).expect("read") {
            Registration::Own { registration } => {
                assert_eq!(registration.kind, ClientKind::Web);
            }
            other => panic!("expected a registration, got {other:?}"),
        }
    }

    #[test]
    fn the_preferred_name_wins_over_googles() {
        let dir = TempDir::new("preferred");
        dir.write(
            "client_secret_old.apps.googleusercontent.com.json",
            r#"{"installed":{"client_id":"old","client_secret":"old"}}"#,
        );
        dir.write(
            PREFERRED,
            r#"{"installed":{"client_id":"chosen","client_secret":"chosen"}}"#,
        );

        match client_registration(&dir.0).expect("read") {
            Registration::Own { registration } => assert_eq!(registration.client_id, "chosen"),
            other => panic!("expected the preferred file, got {other:?}"),
        }
    }

    #[test]
    fn a_flat_registration_is_accepted_too() {
        let dir = TempDir::new("flat");
        dir.write(
            PREFERRED,
            r#"{"client_id":"hand-written","client_secret":null}"#,
        );
        match client_registration(&dir.0).expect("read") {
            Registration::Own { registration } => {
                assert_eq!(registration.client_id, "hand-written");
                assert_eq!(registration.client_secret, None);
            }
            other => panic!("expected a registration, got {other:?}"),
        }
    }

    #[test]
    fn a_broken_file_is_reported_rather_than_ignored() {
        // "It did nothing" is the least useful answer to somebody who put a file there on purpose.
        let dir = TempDir::new("broken");
        dir.write(PREFERRED, "{not json");
        let error = client_registration(&dir.0).expect_err("must not be silently ignored");
        assert!(
            error.to_string().contains("oauth-client.json"),
            "the message has to name the file: {error}"
        );
        assert!(
            error.to_string().contains("Google Cloud"),
            "and say where a good one comes from: {error}"
        );
    }
}
