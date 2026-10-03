//! Built-in mail provider database.
//!
//! Goal: the user types their email address and nothing else. No server names, no ports, no
//! "is it SSL or STARTTLS".
//!
//! Two things are deliberate here:
//!
//! 1. `setup_note` is not decoration. Almost every Chinese provider requires the user to switch
//!    IMAP on in the web client and then generate a *separate* authorization code. If a user
//!    types their account password, the login fails and they will blame the app. The note is
//!    the fix, so it lives in the data rather than in a doc comment.
//!
//! 2. `quirks` exist because servers do not behave like the RFCs. NetEase refuses clients that
//!    do not announce themselves with `ID`; QQ and NetEase throttle concurrent connections.
//!    Encoding that here keeps it out of the protocol code.
//!
//! `verified` records whether the endpoint data has been confirmed against the live service
//! (TCP + TLS + protocol banner). `tests/provider_reachability.rs` performs exactly that check,
//! so the flag is evidence rather than optimism. Last full run: 2026-10-02 — all 40 endpoints
//! answered correctly except Gmail's SMTP port, whose timeout is the mainland-China network
//! rather than bad data (Gmail's IMAP endpoint answered fine in the same run).

use serde::Serialize;

use crate::model::{AuthKind, Endpoint, Region, Security};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Quirks {
    /// Send RFC 2971 `ID` before `LOGIN`. NetEase (163/126/yeah) rejects clients otherwise.
    pub needs_id: bool,
    /// Servers throttle hard; staying at 1-2 avoids "too many connections" rejections.
    pub max_connections: u8,
}

impl Quirks {
    const STANDARD: Quirks = Quirks {
        needs_id: false,
        max_connections: 3,
    };
    const NETEASE: Quirks = Quirks {
        needs_id: true,
        max_connections: 2,
    };
}

/// One entry in the built-in database.
///
/// Hosts are stored as `&'static str` so the whole table can be a `const`, which keeps it in
/// read-only memory and makes lookups allocation-free. Use [`Provider::imap_endpoint`] and
/// [`Provider::smtp_endpoint`] to get owned [`Endpoint`]s.
#[derive(Debug, Clone, Serialize)]
pub struct Provider {
    pub id: &'static str,
    pub display_name: &'static str,
    /// Domains used to auto-select this provider from the user's email address.
    pub domains: &'static [&'static str],
    pub region: Region,
    pub imap_host: &'static str,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: &'static str,
    pub smtp_port: u16,
    pub smtp_security: Security,
    pub auth: AuthKind,
    /// False for services with no IMAP at all — better to say so than to fail mysteriously.
    pub supports_imap: bool,
    pub quirks: Quirks,
    /// What the user must do to obtain a working credential.
    pub setup_note: &'static str,
    /// Deep link to the page where the credential is generated.
    pub enable_url: &'static str,
    /// Whether the endpoint data has been confirmed against the live service.
    ///
    /// Confirmed means: TCP connect succeeded, the TLS mode matched, and the server greeted us
    /// with the protocol we expect (`* OK` for IMAP, `220` for SMTP). It does **not** mean a login
    /// was completed — that needs a real account and is covered by the manual checklist in the
    /// README. Reachability is also network-dependent: Gmail answers correctly but is not
    /// reachable from mainland China.
    pub verified: bool,
}

impl Provider {
    pub fn imap_endpoint(&self) -> Endpoint {
        Endpoint::new(self.imap_host, self.imap_port, self.imap_security)
    }

    pub fn smtp_endpoint(&self) -> Endpoint {
        Endpoint::new(self.smtp_host, self.smtp_port, self.smtp_security)
    }

    /// True when the provider never accepts a plain password (OAuth2 only).
    pub fn needs_oauth(&self) -> bool {
        self.auth == AuthKind::OAuth2
    }

    /// Roughly how many bytes a day this service lets an IMAP client download, when that is known.
    ///
    /// Only Gmail documents a number: about 2.5 GB a day, after which the account is throttled for
    /// the next 24 hours. `design/04` §5.7 keeps 8% back and uses 2.3 GB. Everywhere else the limit
    /// is undocumented or generous, and **inventing one would stop somebody's mail for no reason**,
    /// so unknown is `None` — which the ledger reads as "no limit".
    pub fn daily_download_budget(&self) -> Option<u64> {
        match self.id {
            "gmail" => Some(2_300_000_000),
            _ => None,
        }
    }

    /// Whether this service is one that caps how far back a third-party client may read.
    ///
    /// A1-F2: NetEase's web setting decides the range a third-party client may fetch, and for the
    /// enterprise product it defaults to the last 30 days. Nothing in the protocol says so — IMAP
    /// simply stops having older mail — which is why the client has to *know* which services behave
    /// this way before it says anything. Getting this wrong in the "yes" direction would tell people
    /// to change a setting that does not exist.
    pub fn caps_third_party_range(&self) -> bool {
        matches!(self.id, "163" | "126" | "yeah" | "qiye163")
    }

    /// Whether the cap is documented as an *enterprise* policy rather than a per-account setting.
    ///
    /// The distinction is only for the wording: "企业策略" is right for 网易企业邮 and wrong for a
    /// personal 163 mailbox, where the same symptom comes from the account's own 收取范围 setting.
    pub fn range_cap_is_enterprise_policy(&self) -> bool {
        matches!(self.id, "qiye163")
    }
}

use AuthKind::{AppPassword, OAuth2, Password};
use Security::{Plain, Ssl, StartTls};

pub const PROVIDERS: &[Provider] = &[
    // ---------------------------------------------------------------- 中国大陆
    Provider {
        id: "qq",
        display_name: "QQ 邮箱 / Foxmail",
        domains: &["qq.com", "foxmail.com", "vip.qq.com"],
        region: Region::Cn,
        imap_host: "imap.qq.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.qq.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "在 QQ 邮箱网页版「设置 → 账号」中找到「POP3/IMAP/SMTP 服务」，开启 IMAP/SMTP 服务；\
                     按提示用手机发送短信后，会得到一串 16 位授权码。\
                     请把这串授权码填到本应用的密码框里，不是你的 QQ 密码。\n\n\
                     ⚠️ 用 @foxmail.com / @vip.qq.com 别名的话：已确认别名和 QQ 号是同一个邮箱\
                     （发到哪个地址都能收到），但 IMAP 登录只认 QQ 号邮箱地址 —— 直接用别名登录\
                     会被拒（Login fail / Account is not open）。\n\
                     解决：在下方「登录用户名」里填你的 QQ 号邮箱（例如 123456@qq.com），\
                     「邮箱地址」一栏仍然填别名。这样用 QQ 号登录，发信身份还是别名。",
        enable_url: "https://service.mail.qq.com/detail/0/75",
        verified: true,
    },
    Provider {
        id: "163",
        display_name: "网易 163 邮箱",
        domains: &["163.com"],
        region: Region::Cn,
        imap_host: "imap.163.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.163.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::NETEASE,
        setup_note: "在 163 邮箱网页版「设置 → POP3/SMTP/IMAP」中开启 IMAP/SMTP 服务，\
                     然后设置「客户端授权密码」。请把授权密码填到本应用的密码框里。\
                     网易邮箱要求客户端先发送 IMAP ID 命令，本应用已自动处理。",
        enable_url: "https://mail.163.com/",
        verified: true,
    },
    Provider {
        id: "126",
        display_name: "网易 126 邮箱",
        domains: &["126.com"],
        region: Region::Cn,
        imap_host: "imap.126.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.126.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::NETEASE,
        setup_note: "与 163 邮箱相同：在网页版「设置 → POP3/SMTP/IMAP」开启服务并设置\
                     「客户端授权密码」，然后填入本应用的密码框。",
        enable_url: "https://mail.126.com/",
        verified: true,
    },
    Provider {
        id: "yeah",
        display_name: "网易 yeah.net 邮箱",
        domains: &["yeah.net"],
        region: Region::Cn,
        imap_host: "imap.yeah.net",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.yeah.net",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::NETEASE,
        setup_note: "与 163 邮箱相同：开启 IMAP/SMTP 服务并设置客户端授权密码。",
        enable_url: "https://mail.yeah.net/",
        verified: true,
    },
    Provider {
        id: "exmail",
        display_name: "腾讯企业邮箱",
        domains: &["exmail.qq.com"],
        region: Region::Cn,
        imap_host: "imap.exmail.qq.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.exmail.qq.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: Password,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "腾讯企业邮使用自有域名，服务器地址固定为 imap/smtp.exmail.qq.com。\
                     密码通常就是邮箱登录密码；若管理员开启了安全策略，\
                     请在网页版「设置 → 客户端」中生成专用密码。",
        enable_url: "https://exmail.qq.com/",
        verified: true,
    },
    Provider {
        id: "qiye163",
        display_name: "网易企业邮箱",
        domains: &["qiye.163.com"],
        region: Region::Cn,
        imap_host: "imap.qiye.163.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.qiye.163.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: Password,
        supports_imap: true,
        quirks: Quirks::NETEASE,
        setup_note: "网易企业邮使用自有域名，服务器地址固定为 imap/smtp.qiye.163.com。\
                     管理员需在后台为账号开启 IMAP/SMTP，密码为邮箱登录密码。\
                     本应用会自动发送网易要求的 ID 命令。",
        enable_url: "https://qiye.163.com/",
        verified: true,
    },
    Provider {
        id: "aliyun",
        display_name: "阿里邮箱（个人版）",
        domains: &["aliyun.com", "aliyun.cn"],
        region: Region::Cn,
        imap_host: "imap.aliyun.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.aliyun.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: Password,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "在阿里邮箱网页版「设置 → 账户」中确认已开启 IMAP/SMTP 服务，\
                     密码为邮箱登录密码。",
        enable_url: "https://mail.aliyun.com/",
        verified: true,
    },
    Provider {
        id: "aliyun-qiye",
        display_name: "阿里企业邮箱",
        domains: &["qiye.aliyun.com"],
        region: Region::Cn,
        imap_host: "imap.qiye.aliyun.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.qiye.aliyun.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: Password,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "阿里企业邮使用自有域名，服务器地址固定为 imap/smtp.qiye.aliyun.com。\
                     密码为邮箱登录密码。",
        enable_url: "https://qiye.aliyun.com/",
        verified: true,
    },
    Provider {
        id: "sina",
        display_name: "新浪邮箱",
        domains: &["sina.com", "sina.cn"],
        region: Region::Cn,
        imap_host: "imap.sina.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.sina.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: Password,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "在新浪邮箱网页版「设置 → 客户端 POP/IMAP/SMTP」中开启 IMAP/SMTP 服务，\
                     密码为邮箱登录密码。",
        enable_url: "https://mail.sina.com.cn/",
        verified: true,
    },
    Provider {
        id: "139",
        display_name: "中国移动 139 邮箱",
        domains: &["139.com"],
        region: Region::Cn,
        imap_host: "imap.139.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.139.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: Password,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "139 邮箱使用手机号加 @139.com 作为地址。\
                     请在网页版「设置 → 账户」中开启 IMAP/SMTP，密码为邮箱登录密码。",
        enable_url: "https://mail.10086.cn/",
        verified: true,
    },
    // ---------------------------------------------------------------- 国际
    Provider {
        id: "gmail",
        display_name: "Gmail",
        domains: &["gmail.com", "googlemail.com"],
        region: Region::Global,
        imap_host: "imap.gmail.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.gmail.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "Google 已停用「不够安全的应用」访问。请先为账号开启两步验证，\
                     再到「应用专用密码」页面生成一个 16 位密码，填到本应用的密码框。\
                     直接使用 Google 账号密码会认证失败。",
        enable_url: "https://myaccount.google.com/apppasswords",
        verified: true,
    },
    Provider {
        id: "outlook",
        display_name: "Outlook / Microsoft 365",
        domains: &[
            "outlook.com",
            "hotmail.com",
            "live.com",
            "msn.com",
            "outlook.jp",
            "outlook.de",
        ],
        region: Region::Global,
        imap_host: "outlook.office365.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.office365.com",
        smtp_port: 587,
        smtp_security: StartTls,
        auth: OAuth2,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "微软已于 2024 年 9 月彻底关闭个人账号的基本认证，IMAP/SMTP 现在只能使用 \
                     OAuth2 授权登录。当前版本尚未实现 OAuth2，因此暂时无法登录 Outlook 账号；\
                     该支持已在计划中。",
        enable_url: "https://support.microsoft.com/account-billing",
        verified: true,
    },
    Provider {
        id: "yahoo",
        display_name: "Yahoo Mail",
        domains: &["yahoo.com", "ymail.com", "yahoo.co.jp", "yahoo.com.tw"],
        region: Region::Global,
        imap_host: "imap.mail.yahoo.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.mail.yahoo.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "Yahoo 要求使用「应用密码」：登录 Yahoo 账号安全设置，生成应用密码后\
                     填到本应用的密码框。",
        enable_url: "https://login.yahoo.com/account/security",
        verified: true,
    },
    Provider {
        id: "icloud",
        display_name: "iCloud 邮箱",
        domains: &["icloud.com", "me.com", "mac.com"],
        region: Region::Global,
        imap_host: "imap.mail.me.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.mail.me.com",
        smtp_port: 587,
        smtp_security: StartTls,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "Apple 要求使用「App 专用密码」：在 Apple 账户的「登录与安全」中生成，\
                     然后填到本应用的密码框。请使用 @icloud.com 主地址登录。",
        enable_url: "https://account.apple.com/account/manage",
        verified: true,
    },
    Provider {
        id: "zoho",
        display_name: "Zoho Mail",
        domains: &["zoho.com", "zohomail.com"],
        region: Region::Global,
        imap_host: "imap.zoho.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.zoho.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "Zoho 建议使用「应用专用密码」：账号设置 → 安全 → App Passwords 中生成。",
        enable_url: "https://accounts.zoho.com/home#security/app_passwords",
        verified: true,
    },
    Provider {
        id: "yandex",
        display_name: "Yandex Mail",
        domains: &["yandex.com", "yandex.ru"],
        region: Region::Global,
        imap_host: "imap.yandex.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.yandex.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "在 Yandex ID 的「安全 → 应用密码」中生成专用密码，\
                     并确认邮箱设置中已开启 IMAP。",
        enable_url: "https://id.yandex.com/security/app-passwords",
        verified: true,
    },
    Provider {
        id: "aol",
        display_name: "AOL Mail",
        domains: &["aol.com"],
        region: Region::Global,
        imap_host: "imap.aol.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.aol.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "与 Yahoo 同属一家，同样要求生成「应用密码」后使用。",
        enable_url: "https://login.aol.com/account/security",
        verified: true,
    },
    Provider {
        id: "gmx",
        display_name: "GMX Mail",
        domains: &["gmx.com", "gmx.net", "gmx.de"],
        region: Region::Global,
        imap_host: "imap.gmx.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "mail.gmx.com",
        smtp_port: 587,
        smtp_security: StartTls,
        auth: Password,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "在 GMX 网页版「设置 → POP3/IMAP」中开启 IMAP 访问，密码为登录密码。",
        enable_url: "https://www.gmx.com/",
        verified: true,
    },
    Provider {
        id: "fastmail",
        display_name: "Fastmail",
        domains: &["fastmail.com", "fastmail.fm"],
        region: Region::Global,
        imap_host: "imap.fastmail.com",
        imap_port: 993,
        imap_security: Ssl,
        smtp_host: "smtp.fastmail.com",
        smtp_port: 465,
        smtp_security: Ssl,
        auth: AppPassword,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "Fastmail 必须使用「App Password」：设置 → 隐私与安全 → App Passwords，\
                     生成时勾选 IMAP 与 SMTP 权限。",
        enable_url: "https://app.fastmail.com/settings/security/apppasswords",
        verified: true,
    },
    Provider {
        id: "proton",
        display_name: "Proton Mail（需 Bridge）",
        domains: &["proton.me", "protonmail.com", "pm.me"],
        region: Region::Global,
        imap_host: "127.0.0.1",
        imap_port: 1143,
        imap_security: Plain,
        smtp_host: "127.0.0.1",
        smtp_port: 1025,
        smtp_security: Plain,
        auth: Password,
        supports_imap: true,
        quirks: Quirks::STANDARD,
        setup_note: "Proton Mail 不直接提供 IMAP。必须先安装并登录官方「Proton Mail Bridge」，\
                     它会在本机 1143/1025 端口提供本地 IMAP/SMTP 服务。\
                     密码请使用 Bridge 界面中显示的专用密码（不是 Proton 账号密码）。",
        enable_url: "https://proton.me/mail/bridge",
        // Localhost, so it cannot be checked from CI or from a machine without Bridge running.
        verified: false,
    },
    Provider {
        id: "tutanota",
        display_name: "Tuta（原 Tutanota）",
        domains: &["tuta.com", "tutanota.com", "tuta.io"],
        region: Region::Global,
        imap_host: "",
        imap_port: 0,
        imap_security: Ssl,
        smtp_host: "",
        smtp_port: 0,
        smtp_security: Ssl,
        auth: OAuth2,
        supports_imap: false,
        quirks: Quirks::STANDARD,
        setup_note: "Tuta 出于端到端加密的设计，不提供也不计划提供 IMAP/SMTP，\
                     因此任何第三方邮件客户端都无法连接它。",
        enable_url: "https://tuta.com/",
        // No IMAP endpoint exists, so there is nothing to verify.
        verified: false,
    },
];

/// Every provider in the built-in database.
pub fn all() -> &'static [Provider] {
    PROVIDERS
}

/// Look a provider up by its stable id.
pub fn by_id(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Extract the domain part of an email address, lowercased.
pub fn domain_of(email: &str) -> Option<String> {
    email
        .rsplit_once('@')
        .map(|(_, d)| d.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
}

/// Guess the provider from the user's email address.
///
/// Matches the domain exactly first, then falls back to a registrable-suffix match so that
/// `user@mail.qq.com` still resolves. Domain-only matching cannot identify self-hosted
/// corporate mail, which is what the auto-discovery step is for.
pub fn by_email(email: &str) -> Option<&'static Provider> {
    let domain = domain_of(email)?;
    if let Some(p) = PROVIDERS
        .iter()
        .find(|p| p.domains.contains(&domain.as_str()))
    {
        return Some(p);
    }
    PROVIDERS
        .iter()
        .find(|p| p.domains.iter().any(|d| domain.ends_with(&format!(".{d}"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_service_that_documents_a_download_limit_gets_one() {
        // Gmail's ~2.5 GB/day is a documented, enforced limit; the ledger uses 2.3 GB of it.
        let gmail = by_id("gmail").expect("gmail");
        let budget = gmail.daily_download_budget().expect("gmail has a limit");
        assert!(
            (2_000_000_000..2_500_000_000).contains(&budget),
            "the number has to stay inside the documented limit with margin: {budget}"
        );

        // Everything else: unknown, and unknown must mean "no limit" rather than a guess. A made-up
        // budget would stop somebody's mail for a reason that does not exist.
        for provider in all() {
            if provider.id == "gmail" {
                continue;
            }
            assert_eq!(
                provider.daily_download_budget(),
                None,
                "{} must not be throttled by an invented number",
                provider.id
            );
        }
    }

    #[test]
    fn only_the_services_that_cap_the_fetch_range_are_flagged() {
        // A1-F2: the NetEase family. Anyone else must stay unflagged — the hint tells the user to go
        // and change a setting, and telling them to change one that does not exist is worse than
        // saying nothing.
        for id in ["163", "126", "yeah", "qiye163"] {
            let provider = by_id(id).unwrap_or_else(|| panic!("{id} is in the table"));
            assert!(
                provider.caps_third_party_range(),
                "{id} caps how far back a third-party client can read"
            );
        }
        for provider in all() {
            if matches!(provider.id, "163" | "126" | "yeah" | "qiye163") {
                continue;
            }
            assert!(
                !provider.caps_third_party_range(),
                "{} must not be given a range-cap hint",
                provider.id
            );
        }
        // The wording differs between the enterprise product and a personal mailbox.
        assert!(
            by_id("qiye163")
                .expect("qiye163")
                .range_cap_is_enterprise_policy()
        );
        assert!(!by_id("163").expect("163").range_cap_is_enterprise_policy());
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<_> = PROVIDERS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "provider ids must be unique");
    }

    #[test]
    fn domains_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for p in PROVIDERS {
            for d in p.domains {
                assert_eq!(*d, d.to_ascii_lowercase(), "{d} must be lowercase");
                assert!(seen.insert(*d), "domain {d} is claimed by two providers");
            }
        }
    }

    #[test]
    fn every_provider_is_documented() {
        for p in PROVIDERS {
            assert!(!p.display_name.is_empty(), "{} has no name", p.id);
            assert!(
                p.setup_note.chars().count() > 10,
                "{} needs a real setup note, not a placeholder",
                p.id
            );
            if p.supports_imap {
                assert!(!p.imap_host.is_empty(), "{} has no IMAP host", p.id);
                assert!(p.imap_port != 0, "{} has no IMAP port", p.id);
                assert!(!p.smtp_host.is_empty(), "{} has no SMTP host", p.id);
                assert!(p.smtp_port != 0, "{} has no SMTP port", p.id);
            }
        }
    }

    #[test]
    fn credentials_are_never_plain_passwords_on_major_cn_providers() {
        for id in ["qq", "163", "126", "yeah", "gmail"] {
            let p = by_id(id).unwrap();
            assert_eq!(
                p.auth, AppPassword,
                "{id} issues an authorization code, not the account password"
            );
        }
    }

    #[test]
    fn detects_common_providers() {
        for (email, want) in [
            ("someone@qq.com", "qq"),
            ("someone@foxmail.com", "qq"),
            ("someone@163.com", "163"),
            ("a.b@126.com", "126"),
            ("x@yeah.net", "yeah"),
            ("x@gmail.com", "gmail"),
            ("x@hotmail.com", "outlook"),
            ("x@icloud.com", "icloud"),
            ("x@fastmail.com", "fastmail"),
        ] {
            assert_eq!(
                by_email(email).map(|p| p.id),
                Some(want),
                "failed to map {email}"
            );
        }
    }

    #[test]
    fn detects_subdomains() {
        assert_eq!(by_email("a@mail.qq.com").map(|p| p.id), Some("qq"));
    }

    #[test]
    fn unknown_domain_is_none() {
        assert!(by_email("a@some-company.example").is_none());
    }

    #[test]
    fn netease_family_requires_the_id_command() {
        for id in ["163", "126", "yeah", "qiye163"] {
            let p = by_id(id).expect(id);
            assert!(p.quirks.needs_id, "{id} must send the IMAP ID command");
            assert!(
                p.quirks.max_connections <= 2,
                "{id} throttles connections aggressively"
            );
        }
    }
}
