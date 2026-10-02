//! Credential storage backed by the Windows Credential Manager.
//!
//! Passwords never touch the config file or the database. `keyring` maps to the platform vault
//! (Credential Manager on Windows), so the secret is protected by the OS and by the user's
//! login, and it is not readable by simply copying the app's data directory.

use keyring::{Entry, Error as KeyringError};

use crate::error::{MailError, Result};

/// Namespace under which all of this app's credentials are stored.
const SERVICE: &str = "easy-email";

fn entry(account_id: &str) -> Result<Entry> {
    Entry::new(SERVICE, account_id).map_err(|e| MailError::Credential(e.to_string()))
}

/// Store (or overwrite) the password for an account.
pub fn save_password(account_id: &str, password: &str) -> Result<()> {
    entry(account_id)?
        .set_password(password)
        .map_err(|e| MailError::Credential(e.to_string()))
}

/// Read the stored password, or `None` when the account has never been saved.
pub fn load_password(account_id: &str) -> Result<Option<String>> {
    match entry(account_id)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(KeyringError::NoEntry) => Ok(None),
        Err(e) => Err(MailError::Credential(e.to_string())),
    }
}

/// Remove the stored password. Deleting a credential that is not there is not an error.
pub fn delete_password(account_id: &str) -> Result<()> {
    match entry(account_id)?.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(e) => Err(MailError::Credential(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the real Windows Credential Manager. Uses a random id so it never touches a
    /// user's account, and always cleans up.
    #[test]
    fn round_trip_against_the_real_vault() {
        let id = format!("test-{}", std::process::id());
        save_password(&id, "s3cret").expect("save");
        assert_eq!(load_password(&id).expect("load").as_deref(), Some("s3cret"));

        save_password(&id, "rotated").expect("overwrite");
        assert_eq!(
            load_password(&id).expect("load").as_deref(),
            Some("rotated")
        );

        delete_password(&id).expect("delete");
        assert_eq!(load_password(&id).expect("load"), None);
    }

    #[test]
    fn deleting_a_missing_credential_is_not_an_error() {
        delete_password("easy-email-definitely-not-stored").expect("delete must be idempotent");
    }
}
