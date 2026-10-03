//! Account persistence.
//!
//! Only non-secret account settings live on disk; the password goes to the Windows Credential
//! Manager (see `mail_core::secret`). That split is deliberate — a config file is copied around,
//! backed up and inspected far more casually than a credential vault entry.

use std::path::{Path, PathBuf};

use mail_core::{AccountConfig, MailError, Result};
use serde::{Deserialize, Serialize};

/// On-disk shape. Versioned so a future migration has something to branch on.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccountsFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

fn default_version() -> u32 {
    1
}

pub fn load(path: &Path) -> Result<Vec<AccountConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| MailError::Config(format!("读取账号配置失败（{}）：{e}", path.display())))?;

    // A corrupt file must not brick the app: start empty and keep the bad file for inspection.
    match serde_json::from_str::<AccountsFile>(&raw) {
        Ok(file) => Ok(file.accounts),
        Err(e) => {
            let backup = path.with_extension("json.corrupt");
            let _ = std::fs::rename(path, &backup);
            Err(MailError::Config(format!(
                "账号配置已损坏（{e}），已备份到 {}，请重新添加账号。",
                backup.display()
            )))
        }
    }
}

pub fn save(path: &Path, accounts: &[AccountConfig]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MailError::Config(format!("创建配置目录失败：{e}")))?;
    }

    let file = AccountsFile {
        version: default_version(),
        accounts: accounts.to_vec(),
    };
    let json = serde_json::to_string_pretty(&file)
        .map_err(|e| MailError::Config(format!("序列化账号配置失败：{e}")))?;

    // Write-then-rename so an interrupted write cannot truncate the existing config.
    let tmp: PathBuf = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)
        .map_err(|e| MailError::Config(format!("写入账号配置失败（{}）：{e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| MailError::Config(format!("替换账号配置失败（{}）：{e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_core::{Endpoint, Security};

    fn account(email: &str) -> AccountConfig {
        AccountConfig {
            id: email.to_string(),
            email: email.to_string(),
            display_name: "测试".into(),
            username: email.to_string(),
            provider_id: "custom".into(),
            imap: Endpoint::new("imap.example.com", 993, Security::Ssl),
            smtp: Endpoint::new("smtp.example.com", 465, Security::Ssl),
            needs_id_command: false,
            max_connections: 2,
            auth: mail_core::AuthMethod::Password,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("easy-email-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn round_trips_accounts() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("accounts.json");

        assert!(load(&path).expect("missing file is fine").is_empty());

        save(&path, &[account("a@example.com")]).expect("save");
        let loaded = load(&path).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].email, "a@example.com");
        assert_eq!(loaded[0].imap.host, "imap.example.com");

        // Overwriting must replace, not append.
        save(&path, &[account("b@example.com"), account("c@example.com")]).expect("save");
        assert_eq!(load(&path).expect("load").len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_password_never_reaches_the_config_file() {
        let dir = temp_dir("nosecret");
        let path = dir.join("accounts.json");
        save(&path, &[account("a@example.com")]).expect("save");

        let raw = std::fs::read_to_string(&path).expect("read");
        // The property is about the *secret*, not about a word: the file now legitimately carries
        // `"auth":"password"`, which says how the account signs in and is not a credential. What must
        // never appear is anything that is one.
        assert!(
            !raw.contains("client_secret"),
            "config file must not carry OAuth secrets"
        );
        assert!(
            !raw.contains("secret"),
            "config file must not mention secrets: {raw}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_file_is_backed_up_rather_than_fatal() {
        let dir = temp_dir("corrupt");
        let path = dir.join("accounts.json");
        std::fs::write(&path, "{ this is not json").expect("write");

        let err = load(&path).expect_err("corrupt file must report");
        assert!(err.to_string().contains("损坏"));
        assert!(!path.exists(), "the bad file should be moved aside");
        assert!(path.with_extension("json.corrupt").exists());

        // And the app can carry on with an empty list.
        assert!(load(&path).expect("now clean").is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
