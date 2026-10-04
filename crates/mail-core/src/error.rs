//! Error type for the mail layer.
//!
//! Errors carry two levels of information: a technical message for logs, and a
//! [`MailError::user_hint`] with actionable Chinese guidance. The hint matters more than the
//! technical message — the most common real-world failure by far is "the user typed their
//! account password instead of the provider-issued authorization code", and a raw
//! `AUTHENTICATIONFAILED` string gives them nothing to act on.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, MailError>;

#[derive(Debug, Error)]
pub enum MailError {
    #[error("无法连接 {host}:{port} — {source}")]
    Connect {
        host: String,
        port: u16,
        #[source]
        source: std::io::Error,
    },

    #[error("TLS 握手失败（{host}）— {source}")]
    Tls {
        host: String,
        // `async-native-tls` re-exports `native_tls::Error`, so we avoid a direct native-tls dep.
        #[source]
        source: async_native_tls::Error,
    },

    #[error("{server} 认证失败 — {raw}")]
    Auth {
        server: String,
        raw: String,
        hint: AuthHint,
    },

    #[error("IMAP 协议错误 — {0}")]
    Imap(String),

    #[error("SMTP 发送失败 — {0}")]
    Smtp(String),

    #[error("配置错误 — {0}")]
    Config(String),

    #[error("凭据存取失败 — {0}")]
    Credential(String),

    #[error("邮件解析失败 — {0}")]
    Parse(String),

    #[error("本地缓存错误 — {0}")]
    Cache(String),

    /// A failure from a local database whose *kind* the raw SQL layer cannot know.
    ///
    /// `From<rusqlite::Error>` serves both databases, and the advice that is right for one is
    /// destructive for the other: deleting `cache.db` costs a re-sync, while deleting `user.db` costs
    /// the user's drafts and outbox. Saying "本地缓存错误 — 可以删掉缓存文件" for a `user.db` problem
    /// (found on a real profile, M1a-60) is therefore not a wording nit: it points at the wrong file.
    /// This variant exists so the message can say what is true of both without inviting that.
    #[error("本地数据库出错 — {0}")]
    LocalDb(String),

    /// The database was locked by another writer. Separate from `Cache` because it is *retryable*:
    /// the caller (see `Db::write_retry`) can run the same transaction again, whereas a corrupt file
    /// or a missing table cannot be fixed by waiting.
    #[error("数据库正忙：{0}")]
    Busy(String),

    #[error("操作超时（{0} 秒）")]
    Timeout(u64),
}

/// So cache code can use `?` instead of mapping at every call site.
impl From<rusqlite::Error> for MailError {
    fn from(error: rusqlite::Error) -> Self {
        // Classify lock contention here, once, so every `?` in the codebase benefits: retry loops
        // need to tell "wait and try again" apart from "this will never work".
        if is_lock_error(&error) {
            MailError::Busy(error.to_string())
        } else {
            MailError::LocalDb(error.to_string())
        }
    }
}

/// Whether a SQLite error is pure lock contention (`SQLITE_BUSY` / `SQLITE_LOCKED`).
///
/// `busy_timeout` only covers `SQLITE_BUSY`; `SQLITE_LOCKED` (another connection writing a different
/// table on a shared-cache database) is not retried by SQLite at all, which is why the connection
/// helper tells callers to use `write_retry`.
fn is_lock_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(err, _)
            if matches!(
                err.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

/// Which piece of guidance the UI should surface for an authentication failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthHint {
    /// Provider needs an app-specific password / authorization code, not the login password.
    UseAppPassword,
    /// The account has not been switched on for IMAP/SMTP in the provider's web settings.
    EnableImapInWebSettings,
    /// The provider now requires OAuth2 and rejects plain passwords outright.
    OAuth2Required,
    /// NetEase-family servers reject clients that do not announce themselves with `ID`.
    NeteaseNeedsId,
    /// QQ / Foxmail rejects the login and lists its own possible causes, including rate limiting.
    QqLoginRejected,
    Unknown,
}

impl AuthHint {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthHint::UseAppPassword => {
                "认证被拒绝。这个邮箱要求使用「授权码 / 应用专用密码」，不是你的登录密码。\
                 请到邮箱网页版的设置里开启 IMAP/SMTP 服务并生成授权码，然后把它填到密码框。"
            }
            AuthHint::EnableImapInWebSettings => {
                "认证被拒绝。请先登录邮箱网页版，在「设置 → 账户」里手动开启 IMAP/SMTP 服务，\
                 然后生成授权码再重试。"
            }
            AuthHint::OAuth2Required => {
                // This used to end with "当前版本尚未支持 OAuth2", which stopped being true when the
                // auth axis landed and is the one sentence a user hitting it would act on. `design/05`
                // §7 names it as a stale string to fix; it points at the feature now.
                "该服务商已停用基本认证，只能使用 OAuth2 授权登录。\
                 请在添加账号时选「用浏览器登录」；向导里没有出现这条路径，说明这个服务商\
                 还没有可用的 OAuth 端点。"
            }
            AuthHint::NeteaseNeedsId => {
                "网易邮箱要求客户端先发送 IMAP ID 命令。请确认已勾选「网易邮箱（需要 ID 命令）」选项，\
                 并确保使用的是授权码而不是登录密码。"
            }
            AuthHint::QqLoginRejected => {
                "QQ 邮箱拒绝了登录，它自己给出的可能原因是：IMAP/SMTP 服务未开启、\
                 密码不正确、或登录频率受限。请依次确认：\n\
                 ① 在 QQ 邮箱网页版「设置 → 账号」里，「IMAP/SMTP 服务」确实处于已开启状态；\n\
                 ② 密码框里填的是那 16 位授权码（不是 QQ 密码），且前后没有多余空格；\n\
                 ③ 如果刚才已经连续失败了好几次，QQ 会临时限制登录 —— 等 10~15 分钟再试，\
                 这期间反复重试只会延长限制。\n\
                 QQ 的帮助页地址在上面的服务器原文里。"
            }
            AuthHint::Unknown => {
                "认证被拒绝。请确认：1) 账号填写的是完整邮箱地址；\
                 2) 密码框里填的是授权码/应用专用密码而不是登录密码；\
                 3) 已在网页版开启 IMAP/SMTP。"
            }
        }
    }
}

/// Reduce a server response to the sentence the server actually wrote.
///
/// `async-imap` renders a rejected command as `no response: code: None, info: Some("...")`.
/// The wrapper and the `Some("` quoting are noise for a user, and they push the useful part —
/// which for QQ includes a help URL — into a poorly-wrapped paragraph. The server's own text is
/// the part worth showing.
fn tidy_server_message(raw: &str) -> String {
    let trimmed = raw.trim();

    let Some(start) = trimmed.find("info: Some(\"") else {
        return trimmed.to_string();
    };
    let rest = &trimmed[start + "info: Some(\"".len()..];
    let Some(end) = rest.rfind("\")") else {
        return trimmed.to_string();
    };

    let inner = rest[..end].replace("\\\"", "\"").replace("\\n", " ");
    let inner = inner.trim();

    if inner.is_empty() {
        trimmed.to_string()
    } else {
        inner.to_string()
    }
}

impl MailError {
    /// Actionable guidance for the end user, when we have any.
    pub fn user_hint(&self) -> Option<&'static str> {
        match self {
            MailError::Auth { hint, .. } => Some(hint.as_str()),
            MailError::Connect { .. } => Some(
                "连不上服务器。请检查网络、防火墙和代理设置；\
                 部分企业网络会封锁 993/465 端口。",
            ),
            MailError::Tls { .. } => Some(
                "TLS 握手失败。可能是网络中间设备拦截了加密连接，\
                 或服务器地址/端口填错了。",
            ),
            MailError::Timeout(_) => Some("服务器响应超时，请稍后重试或检查网络。"),
            MailError::Busy(_) => Some("本地数据库正忙（另一个操作正在写入），稍后会自动重试。"),
            MailError::Cache(_) => Some(
                "本地缓存数据库读写失败。可以删掉缓存文件后重启 —— \
                 服务器上的邮件不受影响，只是需要重新同步一次。",
            ),
            MailError::LocalDb(_) => Some(
                "本地数据库读写失败。**不要**删除 user.db：草稿和发件箱都在那里，删了就找不回来。\
                 缓存文件（cache.db）可以删，代价只是重新同步一次。",
            ),
            MailError::Credential(_) => Some("无法读写 Windows 凭据管理器，请检查账户权限。"),
            _ => None,
        }
    }

    /// Turn a raw IMAP LOGIN failure into a classified, actionable error.
    ///
    /// The heuristics here are deliberately fuzzy: providers return wildly different text
    /// (`AUTHENTICATIONFAILED`, `LOGIN failed`, `Login fail`, `Unsafe Login`,
    /// `AUTHORIZATION failed`, `Invalid credentials`, ...) and none of it is standardized.
    /// Classification runs on the unmodified response; what gets *stored* is the tidied version,
    /// because that is what the user ends up reading.
    pub fn classify_login(server: &str, raw: impl Into<String>, needs_id: bool) -> Self {
        let raw = raw.into();
        let lower = raw.to_ascii_lowercase();

        let hint = if lower.contains("unsafe login") {
            AuthHint::NeteaseNeedsId
        } else if lower.contains("help.mail.qq.com") || lower.contains("mail.qq.com/detail") {
            // QQ writes its own three-way explanation (service off / bad code / rate limited)
            // plus a help link, so point at that rather than repeating a generic guess.
            AuthHint::QqLoginRejected
        } else if lower.contains("oauth") || lower.contains("basic authentication is disabled") {
            AuthHint::OAuth2Required
        } else if needs_id
            && (lower.contains("authenticationfailed") || lower.contains("login fail"))
        {
            // On NetEase the ID command and the authorization code fail in the same way, so
            // mention both rather than guessing wrong.
            AuthHint::NeteaseNeedsId
        } else if lower.contains("authenticationfailed")
            || lower.contains("invalid credentials")
            || lower.contains("authorization failed")
            // Both spellings: NetEase says "LOGIN failed", QQ says "Login fail".
            || lower.contains("login fail")
        {
            AuthHint::UseAppPassword
        } else {
            AuthHint::Unknown
        };

        MailError::Auth {
            server: server.to_string(),
            raw: tidy_server_message(&raw),
            hint,
        }
    }

    /// Classify an SMTP failure. The interesting case is an authentication rejection, which has
    /// the same root cause as IMAP: the user pasted their account password instead of the
    /// provider-issued authorization code.
    pub fn classify_smtp(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let lower = raw.to_ascii_lowercase();

        let auth_related = lower.contains("authentication")
            || lower.contains("535")
            || lower.contains("username and password not accepted")
            || lower.contains("invalid credentials")
            || lower.contains("5.7.8");

        if auth_related {
            return MailError::Auth {
                server: "SMTP".to_string(),
                raw,
                hint: AuthHint::UseAppPassword,
            };
        }

        if lower.contains("relay") || lower.contains("554") {
            return MailError::Smtp(format!(
                "{raw}（服务器拒绝为该发件人转发，通常是尚未在网页版开启 SMTP 服务）"
            ));
        }

        MailError::Smtp(raw)
    }
}

impl From<async_imap::error::Error> for MailError {
    fn from(e: async_imap::error::Error) -> Self {
        // Tidy here too: `imap_err` funnels every protocol error through this, and users should
        // see the server's sentence rather than a `Some("...")` wrapper.
        MailError::Imap(tidy_server_message(&e.to_string()))
    }
}

impl From<lettre::error::Error> for MailError {
    fn from(e: lettre::error::Error) -> Self {
        MailError::Smtp(e.to_string())
    }
}

impl From<lettre::transport::smtp::Error> for MailError {
    fn from(e: lettre::transport::smtp::Error) -> Self {
        MailError::Smtp(e.to_string())
    }
}

impl From<lettre::address::AddressError> for MailError {
    fn from(e: lettre::address::AddressError) -> Self {
        MailError::Config(format!("邮箱地址格式不正确：{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape async-imap produced for QQ, minus the truncation.
    const QQ_REJECTION: &str = concat!(
        "no response: code: None, info: Some(\"Login fail. Account is not open, ",
        "password is incorrect, login frequency limited, or system busy. For more ",
        "information at https://help.mail.qq.com/detail/108/1023\")"
    );

    #[test]
    fn unwraps_the_async_imap_no_response_wrapper() {
        let tidied = tidy_server_message(QQ_REJECTION);
        assert!(tidied.starts_with("Login fail."), "got: {tidied}");
        assert!(tidied.contains("https://help.mail.qq.com/detail/108/1023"));
        assert!(
            !tidied.contains("Some(") && !tidied.contains("info:"),
            "the wrapper must be gone, got: {tidied}"
        );
    }

    #[test]
    fn leaves_plain_messages_alone() {
        assert_eq!(
            tidy_server_message("  AUTHENTICATIONFAILED  "),
            "AUTHENTICATIONFAILED"
        );
        assert_eq!(tidy_server_message(""), "");
    }

    #[test]
    fn survives_a_truncated_wrapper() {
        // If the closing quote is missing, fall back to the original rather than mangling it.
        let raw = "no response: code: None, info: Some(\"Login fail";
        assert_eq!(tidy_server_message(raw), raw);
    }

    #[test]
    fn qq_rejection_gets_its_own_three_part_hint() {
        let error = MailError::classify_login("imap.qq.com", QQ_REJECTION, false);
        let hint = error.user_hint().expect("hint");
        assert!(hint.contains("IMAP/SMTP 服务未开启"), "got: {hint}");
        assert!(hint.contains("授权码"), "got: {hint}");
        // QQ rate-limits after repeated failures, which is easy to make worse by retrying.
        assert!(
            hint.contains("频率") || hint.contains("等 10"),
            "got: {hint}"
        );

        // And the raw text kept for display must be readable, not the Debug wrapper.
        let MailError::Auth { raw, .. } = &error else {
            panic!("expected an auth error");
        };
        assert!(raw.starts_with("Login fail."), "got: {raw}");
    }

    #[test]
    fn other_providers_still_classify_as_before() {
        assert_eq!(
            MailError::classify_login("x", "AUTHENTICATIONFAILED", false).user_hint(),
            Some(AuthHint::UseAppPassword.as_str())
        );
        // NetEase says "LOGIN failed"; QQ says "Login fail". Both must be recognised.
        for text in [
            "LOGIN failed: Invalid credentials",
            "Login fail. Please check your password",
            "NO [AUTHORIZATION failed]",
        ] {
            let error = MailError::classify_login("x", text, false);
            let MailError::Auth { hint, .. } = &error else {
                panic!("expected auth error for {text}");
            };
            assert_eq!(*hint, AuthHint::UseAppPassword, "misclassified: {text}");
        }
    }

    #[test]
    fn netease_unsafe_login_still_points_at_the_id_command() {
        let error = MailError::classify_login("imap.163.com", "Unsafe Login", true);
        let MailError::Auth { hint, .. } = &error else {
            panic!("expected auth error");
        };
        assert_eq!(*hint, AuthHint::NeteaseNeedsId);
    }

    #[test]
    fn classification_uses_the_original_text_not_the_tidied_one() {
        // The URL lives inside the wrapper; dropping it before classifying would lose the signal.
        let error = MailError::classify_login("imap.qq.com", QQ_REJECTION, false);
        let MailError::Auth { hint, .. } = &error else {
            panic!("expected auth error");
        };
        assert_eq!(*hint, AuthHint::QqLoginRejected);
    }
}
