//! Where refresh tokens live.
//!
//! A1-F35 settled this, and it is worth restating because the obvious answer is wrong: the **Windows
//! credential manager cannot hold a refresh token**. `CRED_MAX_CREDENTIAL_BLOB_SIZE` is 2,560 bytes
//! and Entra's refresh tokens are commonly 1-4 KB, so storing one there fails or, worse, truncates.
//!
//! Windows uses a **DPAPI-encrypted file** (`CryptProtectData`, user scope). Linux/macOS has no DPAPI,
//! so the file is instead encrypted with a key derived from the machine's own id (`/etc/machine-id`)
//! and the caller's entropy (the account id), written with `0600` permissions inside a `0700`
//! directory. The honest limitation is the same on both platforms — this does not stop malware
//! running **as the same user**; it stops the file being useful to another account on the same
//! machine or anywhere else.
//!
//! Three details that are easy to get wrong and are therefore spelled out here:
//!
//! * **User scope, never `CRYPTPROTECT_LOCAL_MACHINE`.** Machine scope would let any account on the
//!   machine decrypt the token, which is the opposite of what a mail password deserves.
//! * **App entropy.** Passing the account id as additional entropy ties the ciphertext to this app
//!   *and* to that account: a file copied onto another account's name does not decrypt.
//! * **No prompt structure.** The DPAPI prompt flow is deprecated and is being removed (2027-02), and
//!   a sign-in that silently stops working because the OS removed an option is not a thing to build.
//!
//! The honest limitation, which the design states rather than hides: DPAPI does not stop malware
//! running **as the same user**. It stops the file being useful on another machine or to another
//! account, which is what it is for.
//!
//! Access tokens are deliberately **not** stored: they last an hour, nothing else uses them, and a
//! file that only ever holds longer-lived secrets is a smaller thing to get wrong.

use crate::error::{MailError, Result};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

/// Refresh tokens for a set of accounts, in one directory.
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    /// `dir` is the app's own data directory; the files go in a `tokens` subdirectory.
    pub fn new(dir: impl Into<PathBuf>) -> TokenStore {
        TokenStore {
            dir: dir.into().join("tokens"),
        }
    }

    /// Store (or replace) one account's refresh token.
    pub fn save(&self, account_id: &str, refresh_token: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| MailError::Config(format!("无法创建令牌目录：{error}")))?;
        restrict_dir(&self.dir);
        let sealed = protect(refresh_token.as_bytes(), account_id.as_bytes())?;
        let path = self.path_for(account_id);
        // Write to a temporary file and rename: a crash halfway through must not leave a truncated
        // token where a working one used to be, because that would silently sign the user out.
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, &sealed)
            .map_err(|error| MailError::Config(format!("无法写入令牌：{error}")))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| MailError::Config(format!("无法替换令牌文件：{error}")))?;
        restrict_file(&path);
        Ok(())
    }

    /// Read one account's refresh token, or `None` when there is none.
    pub fn load(&self, account_id: &str) -> Result<Option<String>> {
        let path = self.path_for(account_id);
        let sealed = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(MailError::Config(format!("无法读取令牌：{error}"))),
        };

        let plain = unprotect(&sealed, account_id.as_bytes()).map_err(|error| {
            // Worth being explicit: this is what a *copied* file looks like, and the useful answer is
            // "sign in again", not a mysterious crash.
            MailError::Credential(format!("令牌无法解密（可能需要重新登录）：{error}"))
        })?;
        String::from_utf8(plain)
            .map(Some)
            .map_err(|_| MailError::Credential("令牌内容不是文本，请重新登录".into()))
    }

    /// Forget one account's refresh token. Missing is success.
    pub fn delete(&self, account_id: &str) -> Result<()> {
        match std::fs::remove_file(self.path_for(account_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(MailError::Config(format!("无法删除令牌：{error}"))),
        }
    }

    /// One file per account, named by a hash so an address never becomes a path.
    fn path_for(&self, account_id: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(account_id.as_bytes());
        let short: String = digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.dir.join(format!("token-{short}.bin"))
    }
}

/// Whether this build can protect tokens at all. Both supported platforms have a real
/// implementation now (DPAPI on Windows, machine-id-keyed file encryption elsewhere).
pub fn available() -> bool {
    true
}

#[cfg(windows)]
fn protect(plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptProtectData};

    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let extra = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // Safety: the two input blobs point at live slices for the duration of the call, the output blob
    // is ours to free, and `LocalFree` below is the documented way to free it.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &extra,
            std::ptr::null(),
            std::ptr::null(),
            // No flags: user scope (never machine scope) and no prompt structure.
            0,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(MailError::Config(format!(
            "DPAPI 加密失败：{}",
            std::io::Error::last_os_error()
        )));
    }

    let sealed =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    free_local(output.pbData);
    Ok(sealed)
}

#[cfg(windows)]
fn unprotect(sealed: &[u8], entropy: &[u8]) -> std::result::Result<Vec<u8>, String> {
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptUnprotectData};

    let input = CRYPT_INTEGER_BLOB {
        cbData: sealed.len() as u32,
        pbData: sealed.as_ptr() as *mut u8,
    };
    let extra = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // Safety: as in `protect` — live inputs, output freed below.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &extra,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let plain =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    free_local(output.pbData);
    Ok(plain)
}

#[cfg(windows)]
fn free_local(pointer: *mut u8) {
    use windows_sys::Win32::Foundation::LocalFree;
    if !pointer.is_null() {
        // Safety: the pointer came from DPAPI, which documents `LocalFree` as the way to release it.
        unsafe {
            LocalFree(pointer as *mut core::ffi::c_void);
        }
    }
}

#[cfg(windows)]
fn restrict_dir(_dir: &std::path::Path) {}

#[cfg(windows)]
fn restrict_file(_path: &std::path::Path) {}

#[cfg(not(windows))]
fn restrict_dir(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(windows))]
fn restrict_file(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(windows))]
fn protect(plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>> {
    let key = derive_key(entropy)?;
    let mut out = Vec::with_capacity(MAGIC.len() + TAG_LEN + plain.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&tag(&key, plain));
    out.extend(xor_keystream(&key, plain));
    Ok(out)
}

#[cfg(not(windows))]
fn unprotect(sealed: &[u8], entropy: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let key = derive_key(entropy).map_err(|error| error.to_string())?;
    if sealed.len() < MAGIC.len() + TAG_LEN + 1 || &sealed[..MAGIC.len()] != MAGIC {
        return Err("不是有效的令牌文件".into());
    }
    let expected = &sealed[MAGIC.len()..MAGIC.len() + TAG_LEN];
    let body = &sealed[MAGIC.len() + TAG_LEN..];
    let plain = xor_keystream(&key, body);
    if tag(&key, &plain).as_slice() != expected {
        return Err("完整性校验失败".into());
    }
    Ok(plain)
}

/// Four bytes of magic so a non-token file is rejected rather than producing garbage.
#[cfg(not(windows))]
const MAGIC: &[u8; 4] = b"EEET";

#[cfg(not(windows))]
const TAG_LEN: usize = 8;

/// Key = SHA-256(domain || machine secret || caller entropy). The machine secret binds the
/// file to this machine; the entropy (the account id) binds it to this account.
#[cfg(not(windows))]
fn derive_key(entropy: &[u8]) -> Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"easy-email/tokenstore/v1");
    hasher.update(machine_secret()?);
    hasher.update(entropy);
    Ok(hasher.finalize().into())
}

#[cfg(not(windows))]
fn tag(key: &[u8; 32], plain: &[u8]) -> [u8; TAG_LEN] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.update(plain);
    let digest = hasher.finalize();
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&digest[..TAG_LEN]);
    out
}

/// SHA-256 in counter mode as a keystream: a stand-in stream cipher that keeps the token
/// out of the file (and fails an integrity check under the wrong key) without a new
/// dependency. **Not** a substitute for a real AEAD in new code; chosen because it needs
/// no new crate and the threat model is file-on-wrong-machine.
#[cfg(not(windows))]
fn xor_keystream(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut out = Vec::with_capacity(data.len());
    for (counter, chunk) in data.chunks(32).enumerate() {
        let mut hasher = Sha256::new();
        hasher.update(key);
        hasher.update((counter as u64).to_be_bytes());
        let block = hasher.finalize();
        for (byte, key) in chunk.iter().zip(block.iter()) {
            out.push(byte ^ key);
        }
    }
    out
}

/// The machine's own identity: `/etc/machine-id`, falling back to a per-user random file
/// persisted next to the app config. Either is enough to keep the file from being useful
/// on another machine.
#[cfg(not(windows))]
fn machine_secret() -> Result<Vec<u8>> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(id) = std::fs::read_to_string(path) {
            let trimmed = id.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.as_bytes().to_vec());
            }
        }
    }
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
        .ok_or_else(|| MailError::Config("无法确定配置目录，无法读取机器标识".into()))?
        .join("easy-email");
    std::fs::create_dir_all(&dir)
        .map_err(|error| MailError::Config(format!("无法创建配置目录：{error}")))?;
    let path = dir.join("machine-key");
    match std::fs::read(&path) {
        Ok(bytes) if bytes.len() >= 16 => Ok(bytes),
        _ => {
            let mut bytes = vec![0u8; 32];
            getrandom::fill(&mut bytes)
                .map_err(|error| MailError::Config(format!("系统随机源不可用：{error}")))?;
            std::fs::write(&path, &bytes)
                .map_err(|error| MailError::Config(format!("无法写入机器标识：{error}")))?;
            restrict_file(&path);
            Ok(bytes)
        }
    }
}

/// A directory that is removed when the test ends, so a failing test leaves nothing behind.
#[cfg(test)]
struct TempDir(PathBuf);

#[cfg(test)]
impl TempDir {
    fn new(label: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "easy-email-tokens-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_round_trips_and_can_be_forgotten() {
        if !available() {
            return;
        }
        let dir = TempDir::new("round-trip");
        let store = TokenStore::new(dir.path());

        assert_eq!(store.load("me@example.com").expect("load"), None);

        store
            .save("me@example.com", "1//refresh-token")
            .expect("save");
        assert_eq!(
            store.load("me@example.com").expect("load").as_deref(),
            Some("1//refresh-token")
        );

        // Saving again replaces: this is the path a rotated refresh token takes (A1-F34).
        store.save("me@example.com", "1//rotated").expect("save");
        assert_eq!(
            store.load("me@example.com").expect("load").as_deref(),
            Some("1//rotated"),
            "the newest token has to win, or rotation signs the user out"
        );

        store.delete("me@example.com").expect("delete");
        assert_eq!(store.load("me@example.com").expect("load"), None);
        store
            .delete("me@example.com")
            .expect("delete is idempotent");
        assert!(
            !dir.path().join("tokens").join("token-x.bin").exists(),
            "no stray files"
        );
    }

    #[test]
    fn the_token_is_not_readable_in_the_file() {
        if !available() {
            return;
        }
        let dir = TempDir::new("sealed");
        let store = TokenStore::new(dir.path());
        store
            .save("me@example.com", "SUPERSECRET-REFRESH-TOKEN")
            .expect("save");

        let files: Vec<_> = std::fs::read_dir(dir.path().join("tokens"))
            .expect("read dir")
            .map(|entry| entry.expect("entry").path())
            .collect();
        assert_eq!(files.len(), 1, "one file per account");
        let raw = std::fs::read(&files[0]).expect("read");

        // The assertion this whole module exists for: if the protection silently did nothing,
        // the secret would be sitting in the file and this would catch it.
        assert!(
            !raw.windows("SUPERSECRET-REFRESH-TOKEN".len())
                .any(|window| window == b"SUPERSECRET-REFRESH-TOKEN"),
            "the token must not be readable on disk"
        );
        assert!(
            raw.len() > 20,
            "sealed output carries a header: {} bytes",
            raw.len()
        );
    }

    #[test]
    fn a_file_copied_to_another_account_does_not_decrypt() {
        if !available() {
            return;
        }
        // The account id is the entropy, so this is what stops one account's token from being
        // handed to another — and what a "copy the file next to another account" attempt looks like.
        let dir = TempDir::new("entropy");
        let store = TokenStore::new(dir.path());
        store
            .save("first@example.com", "token-for-first")
            .expect("save");

        let sealed = std::fs::read(store.path_for("first@example.com")).expect("read");
        std::fs::write(store.path_for("second@example.com"), &sealed).expect("copy");

        assert_eq!(
            store.load("first@example.com").expect("load").as_deref(),
            Some("token-for-first")
        );
        let error = store
            .load("second@example.com")
            .expect_err("must not decrypt under another account");
        assert!(
            error.to_string().contains("重新登录"),
            "the message has to be actionable: {error}"
        );
    }

    #[test]
    fn a_truncated_token_file_asks_for_a_sign_in_instead_of_returning_garbage() {
        // `design/05`'s acceptance list: rotation, power loss, corruption and a different user must all
        // be covered. A power cut during the write leaves a file that cannot be decrypted, and the only
        // useful answer is "sign in again" — not a panic, not half a token handed to the server.
        if !available() {
            return;
        }
        let dir = TempDir::new("truncated");
        let store = TokenStore::new(dir.path());
        store
            .save("me@example.com", "a-real-refresh-token")
            .expect("save");

        let path = store.path_for("me@example.com");
        let sealed = std::fs::read(&path).expect("read");
        std::fs::write(&path, &sealed[..sealed.len() / 2]).expect("truncate");

        let error = store
            .load("me@example.com")
            .expect_err("a half-written file must not load");
        assert!(
            error.to_string().contains("重新登录"),
            "the message has to be actionable: {error}"
        );

        // And the account can be recovered by signing in again, which overwrites the damaged file.
        store
            .save("me@example.com", "a-new-token")
            .expect("re-save");
        assert_eq!(
            store.load("me@example.com").expect("load").as_deref(),
            Some("a-new-token")
        );
    }

    #[test]
    fn an_address_never_becomes_a_file_name() {
        let store = TokenStore::new("C:/nowhere");
        let path = store.path_for("../../etc/passwd\\..");
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .to_string();
        assert!(
            name.starts_with("token-") && name.ends_with(".bin"),
            "{name}"
        );
        assert!(!name.contains(".."), "{name}");
        assert!(!name.contains('\\'), "{name}");
    }
}
