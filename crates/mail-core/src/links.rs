//! Link classification for the reading pane (`design/14` §4.2 and §5.3).
//!
//! **Why the app does not simply let the frame follow links.** A sandboxed `iframe` without
//! `allow-top-navigation` can still navigate *itself*, so a click on `<a href="https://evil.example">`
//! used to paint the attacker's page inside the reading pane — no URL shown, nothing for the user to
//! check, and a page that looks like it belongs to the mail. The sanitiser now strips every `href`
//! and keeps the target as `data-ee-href` (`sanitize::sanitize_mail_html`), and the renderer offers
//! the link back as a labelled action after asking this module what it really is.
//!
//! **What it answers.** Not "is this link safe" — nobody can answer that — but the four things a
//! reader can actually act on:
//!
//! * the host that will really be connected to (punycode included, because `аpple.com` with a
//!   Cyrillic `а` renders identically to `apple.com` and is not the same host);
//! * whether the visible text claims a *different* domain from the one it will visit;
//! * whether the scheme is one that must never reach the shell (`javascript:`, `file:`, `data:`);
//! * whether it is a URL shortener, where the real destination is unknowable before the click.
//!
//! It does not fetch anything and does not maintain a blocklist: a verdict is a *fact plus a
//! sentence for the user*, which is what the confirmation dialog needs.

use crate::sanitize::is_dangerous_url;

/// What kind of risk a link carries, in the order the UI escalates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlRisk {
    /// An ordinary `http`/`https`/`mailto` link whose text does not contradict it.
    Safe,
    /// The text names a different domain from the one the link goes to — the classic phishing shape.
    Mismatch,
    /// The host contains non-ASCII characters and is really an `xn--…` name.
    Punycode,
    /// `javascript:`, `data:`, `file:`, `ms-*` … — never opened, and the reason is shown.
    DangerousScheme,
    /// A known URL shortener: the destination is not knowable until it is visited.
    Shortener,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LinkVerdict {
    pub risk: UrlRisk,
    /// The host that would actually be connected to, in the form the resolver sees.
    pub host: String,
    /// The domain the *visible text* names, when it names one that differs.
    pub claimed_host: Option<String>,
    /// The full address, for the dialog to show.
    pub full: String,
    /// One sentence, ready to display.
    pub reason: String,
}

impl LinkVerdict {
    /// Whether the shell may hand this to the system browser at all.
    ///
    /// Everything except a dangerous scheme may be opened — **after the user has seen the verdict**.
    /// Refusing the merely-suspicious ones would train people to copy them into a browser by hand,
    /// which is strictly worse: no reason shown, no domain shown.
    pub fn openable(&self) -> bool {
        self.risk != UrlRisk::DangerousScheme
    }
}

/// Classify one link. `text` is the anchor's visible text, which may name a domain of its own.
pub fn classify_link(raw: &str, text: &str) -> LinkVerdict {
    let trimmed = raw.trim();
    let full = trimmed.to_string();

    let make = |risk: UrlRisk, host: &str, claimed: Option<String>, reason: &str| LinkVerdict {
        risk,
        host: host.to_string(),
        claimed_host: claimed,
        full: full.clone(),
        reason: reason.to_string(),
    };

    if trimmed.is_empty() {
        return make(UrlRisk::Safe, "", None, "空链接。");
    }
    if is_dangerous_url(trimmed) {
        return make(
            UrlRisk::DangerousScheme,
            "",
            None,
            "这是一条可执行脚本或本地文件链接，本应用不会打开它。",
        );
    }

    let parsed = match url::Url::parse(trimmed) {
        Ok(parsed) => parsed,
        Err(_) => {
            // A relative URL (`/login`) or a bare fragment. It cannot reach the network from the
            // reading pane, so it is not a risk — it is simply not a link the app can open.
            return make(
                UrlRisk::Safe,
                "",
                None,
                "这不是一个完整网址，本应用无法打开。",
            );
        }
    };

    let scheme = parsed.scheme().to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
        return make(
            UrlRisk::DangerousScheme,
            parsed.host_str().unwrap_or(""),
            None,
            &format!("本应用不打开 {scheme}: 链接。"),
        );
    }
    if scheme == "mailto" {
        return make(UrlRisk::Safe, "", None, "这是一个邮件地址。");
    }

    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();

    // Punycode first: the whole point of an IDN lookalike is that the *text* looks right, so a
    // mismatch check would pass and the user would be told "safe".
    let as_written = host_as_written(trimmed);
    if !as_written.is_ascii() || host.split('.').any(|label| label.starts_with("xn--")) {
        return make(
            UrlRisk::Punycode,
            &host,
            None,
            &format!(
                "这个域名使用了非英文字符（真实域名是 {host}），显示的字母可能不是它真正的样子。"
            ),
        );
    }

    // The text claims a domain: if it is not the same registrable domain, say so.
    let link_domain = registrable(&host);
    if let Some(claimed) = hosts_in_text(text)
        .into_iter()
        .find(|candidate| registrable(candidate) != link_domain)
    {
        let reason = format!("链接文字写的是 {claimed}，实际会打开 {host}。");
        return make(UrlRisk::Mismatch, &host, Some(claimed), &reason);
    }

    if is_shortener(&host) {
        return make(
            UrlRisk::Shortener,
            &host,
            None,
            &format!("{host} 是短链接服务，真实目的地只有点开之后才知道。"),
        );
    }

    make(UrlRisk::Safe, &host, None, &format!("将打开 {host}。"))
}

/// The last two labels: `mail.example.co.uk` → `co.uk`.
///
/// A deliberate approximation. The real answer needs the public-suffix list, and pulling one in to
/// decide whether to *warn* would be the tail wagging the dog: two hosts that share their last two
/// labels are treated as the same site, which is right for the cases that matter here and errs
/// towards *not* crying wolf. The full address is always shown in the dialog anyway.
fn registrable(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    match labels.len() {
        0 => String::new(),
        1 => labels[0].to_string(),
        _ => labels[labels.len() - 2..].join("."),
    }
}

/// The authority as the sender typed it, before `url` normalised it to punycode.
fn host_as_written(raw: &str) -> String {
    let after_scheme = raw.split_once("://").map(|(_, rest)| rest).unwrap_or(raw);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Drop any userinfo, which is itself a good way to make a URL look like another site:
    // `https://good.example@evil.example/` goes to `evil.example`.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    host.trim_matches('.').to_ascii_lowercase()
}

/// Hosts mentioned in the visible text.
///
/// Hand-rolled rather than a regex dependency: the shape is "a run of letters, digits, dots and
/// hyphens with at least one dot", and being slightly conservative costs nothing — a missed host
/// means one fewer warning, while a false one would train people to click through warnings.
fn hosts_in_text(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();

    let flush = |current: &mut String, found: &mut Vec<String>| {
        let candidate = current.trim_matches('.').to_ascii_lowercase();
        current.clear();
        if !candidate.contains('.') {
            return;
        }
        let labels: Vec<&str> = candidate.split('.').collect();
        let tld = labels.last().copied().unwrap_or_default();
        let plausible = labels.len() >= 2
            && labels.iter().all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-')
                    && !label.starts_with('-')
                    && !label.ends_with('-')
            })
            // A real TLD is at least two letters, and not all digits (which would be an IP, and an
            // IP in the text is not a claim about a domain).
            && tld.len() >= 2
            && !tld.chars().all(|c| c.is_ascii_digit());
        if plausible && !found.contains(&candidate) {
            found.push(candidate);
        }
    };

    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
            current.push(c);
        } else {
            flush(&mut current, &mut found);
        }
    }
    flush(&mut current, &mut found);

    found
}

/// Known URL shorteners.
///
/// A short list of the ones that actually appear in mail, not an attempt at completeness: the point
/// is to say "the destination is unknowable", and a false negative here is a link that opens after
/// the user has seen its real host, which is the normal case anyway.
fn is_shortener(host: &str) -> bool {
    const SHORTENERS: [&str; 12] = [
        "bit.ly",
        "t.co",
        "tinyurl.com",
        "goo.gl",
        "is.gd",
        "buff.ly",
        "ow.ly",
        "rebrand.ly",
        "cutt.ly",
        "shorturl.at",
        "rb.gy",
        "s.id",
    ];
    let host = host.to_ascii_lowercase();
    SHORTENERS
        .iter()
        .any(|known| host == *known || host.ends_with(&format!(".{known}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk(raw: &str, text: &str) -> UrlRisk {
        classify_link(raw, text).risk
    }

    #[test]
    fn an_ordinary_link_is_safe_and_names_its_host() {
        let verdict = classify_link("https://example.com/invoice?id=7", "账单");
        assert_eq!(verdict.risk, UrlRisk::Safe);
        assert_eq!(verdict.host, "example.com");
        assert!(verdict.openable());
        assert!(verdict.reason.contains("example.com"));
    }

    #[test]
    fn text_that_names_another_domain_is_a_mismatch() {
        let verdict = classify_link("https://evil.example/login", "登录 www.example.com");
        assert_eq!(verdict.risk, UrlRisk::Mismatch);
        assert_eq!(verdict.claimed_host.as_deref(), Some("www.example.com"));
        assert_eq!(verdict.host, "evil.example");
        // Still openable: the user is told, and then decides.
        assert!(verdict.openable());
    }

    #[test]
    fn a_link_to_the_same_domain_is_not_a_mismatch() {
        assert_eq!(
            risk("https://mail.example.com/x", "example.com 的邮件"),
            UrlRisk::Safe
        );
        assert_eq!(
            risk("https://www.example.com/x", "登录 example.com"),
            UrlRisk::Safe
        );
    }

    #[test]
    fn an_internationalised_host_is_punycode_not_safe() {
        // The Cyrillic а: renders as `аpple.com`, resolves to xn--80ak6aa92e.com.
        let verdict = classify_link("https://xn--80ak6aa92e.com/login", "apple.com");
        assert_eq!(verdict.risk, UrlRisk::Punycode);
        assert!(verdict.reason.contains("xn--80ak6aa92e.com"));

        // The same thing written with the actual Unicode characters.
        assert_eq!(
            risk("https://аpple.com/login", "apple.com"),
            UrlRisk::Punycode
        );
    }

    #[test]
    fn a_dangerous_scheme_is_never_openable() {
        for bad in [
            "javascript:alert(1)",
            "data:text/html,<script>x</script>",
            "file:///C:/Windows/System32/calc.exe",
            "ms-its:help.chm::/x.htm",
        ] {
            let verdict = classify_link(bad, "点我");
            assert_eq!(verdict.risk, UrlRisk::DangerousScheme, "{bad}");
            assert!(!verdict.openable(), "{bad}");
        }
        // An unusual but harmless-looking scheme is refused too, because the shell cannot open it
        // and pretending otherwise would be a lie.
        let verdict = classify_link("ftp://example.com/x", "ftp");
        assert_eq!(verdict.risk, UrlRisk::DangerousScheme);
    }

    #[test]
    fn userinfo_cannot_disguise_the_real_host() {
        // `https://apple.com@evil.example/` goes to evil.example, and the text is not lying about
        // where it goes only if it says so.
        let verdict = classify_link("https://apple.com@evil.example/", "apple.com");
        assert_eq!(verdict.host, "evil.example");
        assert_eq!(verdict.risk, UrlRisk::Mismatch);
    }

    #[test]
    fn a_shortener_says_the_destination_is_unknown() {
        let verdict = classify_link("https://bit.ly/3xYz", "账单");
        assert_eq!(verdict.risk, UrlRisk::Shortener);
        assert!(verdict.openable());
    }

    #[test]
    fn a_relative_or_empty_link_is_not_a_risk_and_not_openable_as_a_page() {
        assert_eq!(risk("/account/login", "登录"), UrlRisk::Safe);
        assert_eq!(classify_link("", "x").host, "");
        assert_eq!(risk("https://example.com", "mailto:a@b.c"), UrlRisk::Safe);
    }

    #[test]
    fn hosts_in_text_finds_domains_and_ignores_everything_else() {
        assert_eq!(
            hosts_in_text("登录 www.example.com 查看"),
            vec!["www.example.com"]
        );
        assert_eq!(hosts_in_text("see example.com."), vec!["example.com"]);
        // Not domains: a version, an IP, a bare word, a time.
        assert_eq!(hosts_in_text("版本 2.0 发布"), Vec::<String>::new());
        assert_eq!(hosts_in_text("192.168.1.1"), Vec::<String>::new());
        assert_eq!(hosts_in_text("hello world"), Vec::<String>::new());
        assert_eq!(hosts_in_text("12:30"), Vec::<String>::new());
    }

    #[test]
    fn a_host_shaped_run_inside_a_longer_word_is_still_found_once() {
        // Nothing here is a domain claim, but the scanner must not return duplicates either.
        let found = hosts_in_text("a.example.com and a.example.com again");
        assert_eq!(found, vec!["a.example.com"]);
    }
}
