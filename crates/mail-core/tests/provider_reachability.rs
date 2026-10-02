//! Reachability check for the built-in provider database.
//!
//! The database is only useful if it is correct, so this test proves it against the real
//! servers: TCP connect, TLS handshake where the preset says so, and a readable protocol banner.
//! That is as far as we can go without credentials, and it catches the failure modes that
//! actually happen — a retired hostname, a changed port, a server that moved to STARTTLS.
//!
//! The distinction that matters: a TCP connection that *succeeds* and then speaks the wrong
//! protocol is proof the preset is wrong. A TCP connection that fails may just be this network
//! (a corporate firewall, or the GFW for Gmail/Yahoo/iCloud from mainland China). Only the first
//! kind is a hard failure; the second is reported so it can be re-checked from elsewhere.
//!
//! Ignored by default because it needs the public internet:
//!
//! ```text
//! cargo test -p mail-core --test provider_reachability -- --ignored --nocapture
//! ```

use std::time::Duration;

use mail_core::Security;
use mail_core::providers::{self, Provider};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const BANNER_TIMEOUT: Duration = Duration::from_secs(15);

/// What a probe concluded.
enum Verdict {
    /// Endpoint answered with the protocol we expected.
    Good(String),
    /// Endpoint answered, but not with the expected protocol — the preset is wrong.
    WrongProtocol(String),
    /// Could not reach the endpoint at all. May be this network rather than bad data.
    Unreachable(String),
}

async fn probe(host: &str, port: u16, security: Security, kind: &str) -> Verdict {
    let tcp = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port))).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => return Verdict::Unreachable(format!("连接失败: {e}")),
        Err(_) => return Verdict::Unreachable("连接超时".to_string()),
    };
    let _ = tcp.set_nodelay(true);

    let banner = match security {
        Security::Ssl => {
            let connector = async_native_tls::TlsConnector::new();
            match tokio::time::timeout(BANNER_TIMEOUT, connector.connect(host, tcp)).await {
                Ok(Ok(mut stream)) => read_banner(&mut stream).await,
                // A handshake that *fails* is evidence: the peer told us it is not the service we
                // think it is. A handshake that merely never finishes carries no information about
                // the peer at all, so it must not be reported as bad data — doing that made this
                // test flaky on a real network.
                Ok(Err(e)) => return Verdict::WrongProtocol(format!("TLS 握手失败: {e}")),
                Err(_) => return Verdict::Unreachable("TLS 握手超时".to_string()),
            }
        }
        // Plaintext and STARTTLS both greet in the clear; enough to identify the service.
        Security::StartTls | Security::Plain => {
            let mut stream = tcp;
            read_banner(&mut stream).await
        }
    };

    let banner = match banner {
        Ok(b) => b,
        Err(e) => return Verdict::WrongProtocol(e),
    };

    let ok = match kind {
        // IMAP servers open with an untagged `* OK` (RFC 3501 §7.1.1).
        "IMAP" => banner.starts_with("* OK") || banner.starts_with("* PREAUTH"),
        // SMTP servers open with `220` (RFC 5321 §4.2).
        _ => banner.starts_with("220"),
    };

    if ok {
        Verdict::Good(first_line(&banner))
    } else {
        Verdict::WrongProtocol(format!("不是 {kind} 问候语: {}", first_line(&banner)))
    }
}

/// Probe once, and give a "wrong protocol" conclusion a second chance.
///
/// Calling an endpoint's data stale is a strong claim, and on a real network a transient hiccup
/// can briefly look like a protocol mismatch. Only a verdict that survives both attempts fails
/// the run. A genuinely retired hostname or changed port fails consistently.
async fn probe_with_retry(host: &str, port: u16, security: Security, kind: &str) -> Verdict {
    match probe(host, port, security, kind).await {
        Verdict::WrongProtocol(_) => {
            tokio::time::sleep(Duration::from_millis(500)).await;
            probe(host, port, security, kind).await
        }
        other => other,
    }
}

async fn read_banner<S>(stream: &mut S) -> Result<String, String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf = vec![0u8; 512];
    let n = tokio::time::timeout(BANNER_TIMEOUT, stream.read(&mut buf))
        .await
        .map_err(|_| "读取问候语超时".to_string())?
        .map_err(|e| format!("读取失败: {e}"))?;
    if n == 0 {
        return Err("连接被立即关闭".to_string());
    }
    Ok(String::from_utf8_lossy(&buf[..n]).trim().to_string())
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(60).collect()
}

#[tokio::test]
#[ignore = "requires public internet access"]
async fn every_verified_provider_really_answers() {
    let mut wrong: Vec<String> = Vec::new();
    let mut unreachable: Vec<String> = Vec::new();
    let mut promotable: Vec<String> = Vec::new();
    let mut probed: std::collections::HashSet<(String, u16)> = std::collections::HashSet::new();

    // Built with format! so the column widths stay readable next to the data rows below.
    // Passing the same literals straight to println! trips clippy::print_literal.
    let header = format!(
        "{:<14} {:<9} {:<28} {:<12} {}",
        "provider", "protocol", "endpoint", "结果", "问候语"
    );
    println!("\n{header}");
    println!("{}", "-".repeat(115));

    for provider in providers::all() {
        if !provider.supports_imap {
            continue;
        }
        // Proton Mail Bridge is localhost, not a public endpoint.
        if provider.imap_host == "127.0.0.1" {
            continue;
        }

        for (kind, host, port, security) in [
            (
                "IMAP",
                provider.imap_host,
                provider.imap_port,
                provider.imap_security,
            ),
            (
                "SMTP",
                provider.smtp_host,
                provider.smtp_port,
                provider.smtp_security,
            ),
        ] {
            let key = (host.to_string(), port);
            if !probed.insert(key) {
                continue;
            }

            let endpoint = format!("{host}:{port}");
            let (label, note) = match probe_with_retry(host, port, security, kind).await {
                Verdict::Good(banner) => ("OK", banner),
                Verdict::WrongProtocol(why) => {
                    wrong.push(format!("{} {kind} {endpoint} — {why}", provider.id));
                    ("协议不符", why)
                }
                Verdict::Unreachable(why) => {
                    if provider.verified {
                        unreachable.push(format!("{} {kind} {endpoint} — {why}", provider.id));
                    } else {
                        promotable.push(provider.id.to_string());
                    }
                    ("不可达", why)
                }
            };

            println!(
                "{:<14} {:<9} {:<28} {:<12} {}",
                provider.id, kind, endpoint, label, note
            );
        }
    }

    println!("\n{}", "-".repeat(115));
    if !unreachable.is_empty() {
        println!("\n标记为 verified 但本次网络不可达（可能是本机网络/防火墙，非数据错误）:");
        for line in &unreachable {
            println!("  {line}");
        }
    }
    if !promotable.is_empty() {
        promotable.sort();
        promotable.dedup();
        println!("\n未标记 verified、本次也没连通的 provider: {promotable:?}");
    }

    assert!(
        wrong.is_empty(),
        "\n以下端口的协议不符 —— 内置数据很可能已经过时，必须修正:\n  {}\n",
        wrong.join("\n  ")
    );
}

/// Every endpoint in the database must be a plausible host/port pair, with the port matching the
/// transport. Catches a leftover empty host, a `0` port, or TLS declared on the wrong port.
#[test]
fn every_endpoint_is_well_formed() {
    for provider in providers::all() {
        if !provider.supports_imap {
            continue;
        }
        for (kind, host, port, security) in [
            (
                "IMAP",
                provider.imap_host,
                provider.imap_port,
                provider.imap_security,
            ),
            (
                "SMTP",
                provider.smtp_host,
                provider.smtp_port,
                provider.smtp_security,
            ),
        ] {
            assert!(!host.is_empty(), "{}: {kind} 主机名为空", provider.id);
            assert!(port != 0, "{}: {kind} 端口为 0", provider.id);
            assert!(
                host.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
                "{}: {kind} 主机名含有可疑字符: {host}",
                provider.id
            );

            let expected = match (kind, security) {
                ("IMAP", Security::Ssl) => Some(993),
                ("IMAP", Security::StartTls) => Some(143),
                ("SMTP", Security::Ssl) => Some(465),
                ("SMTP", Security::StartTls) => Some(587),
                _ => None,
            };
            if let Some(expected) = expected {
                assert_eq!(
                    port,
                    expected,
                    "{}: {kind} 使用 {} 时端口通常为 {expected}，实际为 {port}",
                    provider.id,
                    security.label()
                );
            }
        }
    }
}

/// The providers the target users actually have must stay hand-verified.
///
/// Gmail is included even though it is unreachable from mainland China: `imap.gmail.com:993` /
/// `smtp.gmail.com:465` is its documented, unchanged endpoint, and the timeout is a network fact
/// rather than a data problem. The app must therefore treat "cannot connect" as a first-class,
/// explainable outcome rather than as a bug.
#[test]
fn the_core_providers_are_marked_verified() {
    for id in ["qq", "163", "126", "gmail", "yahoo", "icloud"] {
        let provider = providers::by_id(id).expect(id);
        assert!(
            provider.verified,
            "{id} 属于主要目标用户群，必须已经过真机验证"
        );
    }
}

/// Guards against a copy-paste mistake where one provider ends up pointing at another's server.
#[test]
fn no_two_providers_share_an_endpoint() {
    let mut seen: std::collections::HashMap<(&str, u16), &str> = std::collections::HashMap::new();
    for provider in providers::all() {
        if !provider.supports_imap || provider.imap_host == "127.0.0.1" {
            continue;
        }
        if let Some(previous) = seen.insert((provider.imap_host, provider.imap_port), provider.id) {
            panic!(
                "{} 与 {} 使用了同一个 IMAP 端点 {}:{}",
                provider.id, previous, provider.imap_host, provider.imap_port
            );
        }
    }
}

/// `Provider` must round-trip through JSON for the UI, quirks included.
#[test]
fn providers_serialise_for_the_ui() {
    let provider: &Provider = providers::by_id("163").expect("163");
    let json = serde_json::to_value(provider).expect("serialise");
    assert_eq!(json["id"], "163");
    assert_eq!(json["imap_host"], "imap.163.com");
    assert_eq!(json["imap_port"], 993);
    assert_eq!(json["quirks"]["needs_id"], true);
    assert_eq!(json["auth"], "app_password");
}
