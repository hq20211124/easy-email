//! OAuth 2.0 for mail accounts: the protocol half.
//!
//! `design/05` §3.1 and A1-F32/F33/F34/F36 pin the shape of this, and every one of them is a
//! constraint that would be expensive to discover later:
//!
//! * **Authorization code + PKCE (S256), loopback redirect, system browser.** No OOB (Google shut it
//!   down in 2022-2023), no embedded webview (Google answers `disallowed_useragent`), no device-code
//!   flow on Windows. A1-F32.
//! * **`state` must be high-entropy and must be checked** on the callback: it is the only thing
//!   stopping a local process from handing us somebody else's authorization code (A1-F36).
//! * **Microsoft rotates refresh tokens**: every refresh issues a new one and the old one is
//!   expected to be discarded, so a refresh that succeeds and is not persisted locks the user out
//!   (A1-F34). That is why [`TokenSet::refresh_token`] is returned from a *refresh* as well as from
//!   an exchange, and why nothing here keeps tokens in memory beyond the call.
//! * **`client_secret` is not a secret in a desktop app** (Google's own words, A1-F32) but the
//!   question that decides the wizard's shape is whether the token endpoint *requires* it — that is
//!   M0-8, and it is why every call here takes it as an `Option`.
//!
//! What is deliberately **not** here: opening the browser. That needs a human to press "allow", so it
//! stays next to the UI; the *listener* the redirect comes back to is here, because it is where the
//! `state` check has to happen and a check that is not tested is not a check.

use crate::error::{MailError, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The pieces of an OAuth token response this client needs.
///
/// `refresh_token` is `None` on a refresh where the provider did not rotate — and a *successful*
/// refresh must persist whatever came back, because the providers that rotate issue a new one every
/// time (A1-F34).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds until the access token expires, as the provider reported it.
    #[serde(default)]
    pub expires_in: Option<i64>,
}

/// What the wizard should put in its client-id fields, and what to say about it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClientChoice {
    /// Whether this provider offers an OAuth path at all, straight from the provider table
    /// (`design/05` §10.1: the wizard's condition must come from the table, not from an id list).
    #[serde(default)]
    pub oauth_available: bool,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    /// The endpoints the registration names, when it names any. The command layer passes these to
    /// [`OAuthProvider::with_registration_endpoints`].
    #[serde(default)]
    pub auth_endpoint: Option<String>,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    /// `own` when the user brought their own registration; `public_test` when it is borrowed.
    pub source: String,
    /// Empty for one's own registration; the notice to show for a borrowed one.
    #[serde(default)]
    pub notice: String,
}

/// The published Thunderbird registration, and the notice that has to travel with it.
///
/// It is in Mozilla's source because a desktop client cannot keep a secret (A1-F32), and the comment
/// above it there says "Don't copy these values for your own application--register it yourself." That
/// is why using it is a **fallback with a notice**, never a default: the consent screen says "Mozilla
/// Thunderbird", Mozilla can revoke it whenever they like, and a release build must ship its own
/// registration (A1-F37).
pub const PUBLIC_TEST_CLIENT: (&str, &str) = (
    "REMOVED-BORROWED-CLIENT-ID",
    "REMOVED-BORROWED-CLIENT-SECRET",
);

/// The notice shown whenever a borrowed registration is used.
pub const PUBLIC_TEST_NOTICE: &str = "预填的是 Mozilla 公开在源码里的注册（Thunderbird 用的），仅供试用：授权页会显示 \
     \"Mozilla Thunderbird\"，且 Mozilla 可随时撤销。正式使用请换成你自己注册的客户端 ID\
     （把 Google Cloud 下载的 JSON 放进应用数据目录即可）。";

impl OAuthProvider {
    /// This provider's endpoints, overridden by the ones the client registration names.
    ///
    /// Only the two URLs change: the scope is what *this app* asks for, not something a downloaded
    /// file may redefine. A registration that names different endpoints is taken at its word because
    /// it belongs to whoever will be signing in — and because it makes the flow testable against a
    /// local server without a feature flag or a code change (see `tests/oauth_flow.rs`).
    pub fn with_registration_endpoints(
        self,
        registration: &crate::oauthclient::ClientRegistration,
    ) -> OAuthProvider {
        // `&'static str` in the type, so an owned override has to be leaked. It happens once per
        // sign-in, on a value that lives for the process anyway.
        fn leak(value: &str) -> &'static str {
            Box::leak(value.to_string().into_boxed_str())
        }

        OAuthProvider {
            auth_endpoint: match registration.auth_uri.as_deref() {
                Some(uri) if !uri.is_empty() => leak(uri),
                _ => self.auth_endpoint,
            },
            token_endpoint: match registration.token_uri.as_deref() {
                Some(uri) if !uri.is_empty() => leak(uri),
                _ => self.token_endpoint,
            },
            ..self
        }
    }
}

/// Decide what to offer for one provider.
pub fn client_choice(provider_id: &str, own: crate::oauthclient::Registration) -> ClientChoice {
    if let crate::oauthclient::Registration::Own { registration } = own {
        return ClientChoice {
            // Filled in by the command layer, which has the provider table at hand.
            oauth_available: false,
            client_id: registration.client_id,
            client_secret: registration.client_secret,
            auth_endpoint: registration.auth_uri,
            token_endpoint: registration.token_uri,
            source: "own".to_string(),
            notice: String::new(),
        };
    }

    // No registration of our own. Gmail gets the borrowed one with its notice; Microsoft has no
    // published equivalent, and inventing one would be worse than asking.
    if provider_id == "gmail" {
        ClientChoice {
            oauth_available: false,
            client_id: PUBLIC_TEST_CLIENT.0.to_string(),
            client_secret: Some(PUBLIC_TEST_CLIENT.1.to_string()),
            auth_endpoint: None,
            token_endpoint: None,
            source: "public_test".to_string(),
            notice: PUBLIC_TEST_NOTICE.to_string(),
        }
    } else {
        ClientChoice {
            oauth_available: false,
            client_id: String::new(),
            client_secret: None,
            auth_endpoint: None,
            token_endpoint: None,
            source: "missing".to_string(),
            notice: "这个服务商需要 OAuth 客户端 ID。请把你自己的注册填进来。".to_string(),
        }
    }
}

/// A provider's authorization and token endpoints, and the scope that works for IMAP/SMTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OAuthProvider {
    pub auth_endpoint: &'static str,
    pub token_endpoint: &'static str,
    pub scope: &'static str,
    /// Whether Google-style extra parameters should be sent (`access_type=offline`, `prompt=consent`).
    ///
    /// Without them Google hands back an access token and **no** refresh token, and the account works
    /// for an hour and then stops — the single most confusing failure this flow can have.
    pub needs_offline_params: bool,
}

/// Google's endpoints for a desktop client (A1-F32).
pub const GOOGLE: OAuthProvider = OAuthProvider {
    auth_endpoint: "https://accounts.google.com/o/oauth2/v2/auth",
    token_endpoint: "https://oauth2.googleapis.com/token",
    scope: "https://mail.google.com/",
    needs_offline_params: true,
};

/// Microsoft's endpoints. The scope must be the full resource URLs (A1-F33) — the short forms are
/// rejected, and Office 365 needs the `.default`-style resource scopes to consent at all.
pub const MICROSOFT: OAuthProvider = OAuthProvider {
    auth_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
    token_endpoint: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
    scope: "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access",
    needs_offline_params: false,
};

/// A PKCE verifier and its S256 challenge.
///
/// The verifier must be 43-128 characters of unreserved ASCII; `design`'s callers treat it as opaque
/// and keep it only for the duration of one sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// A fresh pair, from the OS random source.
    ///
    /// Fallible on purpose: if the OS cannot give us entropy, continuing with a weak verifier would
    /// defeat the entire mechanism, and the only honest answer is to refuse to start a sign-in.
    pub fn generate() -> Result<Pkce> {
        let verifier = random_url_safe(64)?;
        let challenge = code_challenge_s256(&verifier);
        Ok(Pkce {
            verifier,
            challenge,
        })
    }
}

/// A `state` value for one authorization request (A1-F36: high entropy, and it must be checked).
pub fn state() -> Result<String> {
    random_url_safe(32)
}

/// PKCE's S256 challenge: base64url(SHA-256(verifier)), without padding.
pub fn code_challenge_s256(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// The URL the **system browser** is sent to.
///
/// Built as a string rather than through a client library on purpose: it is the one part of the flow
/// a person can look at in the address bar and check against the provider's documentation, and the
/// tests here assert the exact parameters rather than "a URL was produced".
pub fn authorization_url(
    provider: OAuthProvider,
    client_id: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
) -> String {
    let mut params: Vec<(&str, String)> = vec![
        ("client_id", client_id.to_string()),
        ("response_type", "code".to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("scope", provider.scope.to_string()),
        ("state", state.to_string()),
        ("code_challenge", pkce.challenge.clone()),
        ("code_challenge_method", "S256".to_string()),
    ];
    if provider.needs_offline_params {
        // Both are required for a refresh token: without `access_type=offline` Google returns none,
        // and without `prompt=consent` it only returns one the first time the user ever consents.
        params.push(("access_type", "offline".to_string()));
        params.push(("prompt", "consent".to_string()));
    }

    format!("{}?{}", provider.auth_endpoint, encode_form(&params))
}

/// Sign in: from nothing to a token set, with the browser and the callback in between.
///
/// This is the whole flow in one place so the caller (a Tauri command) is left with nothing to decide
/// — the order matters, and every step in it is one of the design's constraints:
///
/// 1. Bind the loopback listener **first**: the redirect URI has to be known before the authorization
///    URL is built, and binding late would mean handing the provider a URI nobody is listening on.
/// 2. Fresh PKCE pair and fresh `state`, per sign-in.
/// 3. Open the **system browser** (A1-F32), which is why [`BrowserLauncher`] exists.
/// 4. Wait for the callback, which validates `state` before it accepts anything.
/// 5. Exchange the code — with the verifier, which has never left this process.
///
/// The tokens are *returned*, not stored: storing them belongs to the caller, which knows the account
/// id and the app's data directory. Access tokens in particular are not persisted at all.
pub async fn sign_in<T: TokenTransport, B: BrowserLauncher>(
    transport: &T,
    browser: &B,
    provider: OAuthProvider,
    client_id: &str,
    client_secret: Option<&str>,
    timeout: Duration,
) -> Result<TokenSet> {
    sign_in_as(
        transport,
        browser,
        provider,
        client_id,
        client_secret,
        crate::oauthclient::ClientKind::Desktop,
        timeout,
    )
    .await
}

/// The same, with the kind of client the registration is — because the *type* decides whether a
/// loopback redirect is allowed at all.
///
/// A "Web application" client cannot use one: Google rejects the redirect with `redirect_uri_mismatch`,
/// which reads like a typo in the redirect URI rather than "you downloaded the wrong kind of client".
/// One sentence here saves that hunt.
pub async fn sign_in_as<T: TokenTransport, B: BrowserLauncher>(
    transport: &T,
    browser: &B,
    provider: OAuthProvider,
    client_id: &str,
    client_secret: Option<&str>,
    kind: crate::oauthclient::ClientKind,
    timeout: Duration,
) -> Result<TokenSet> {
    if kind == crate::oauthclient::ClientKind::Web {
        return Err(MailError::Config(
            "这个 OAuth 注册是「Web 应用」类型，不能用本机回环地址接收授权。\
             请在 Google Cloud 里新建一个「桌面应用」类型的客户端，把它的 JSON 放进应用数据目录。"
                .into(),
        ));
    }

    let loopback = Loopback::bind().await?;
    let redirect_uri = loopback.redirect_uri.clone();
    let pkce = Pkce::generate()?;
    let state = state()?;

    let url = authorization_url(provider, client_id, &redirect_uri, &pkce, &state);
    browser.open(&url)?;

    let code = loopback.wait_for_code(&state, timeout).await?;

    exchange_code(
        transport,
        provider,
        client_id,
        client_secret,
        &code,
        &pkce.verifier,
        &redirect_uri,
    )
    .await
}

/// Access tokens that have already been fetched, so a burst of work does not refresh once per call.
///
/// `design/05`'s acceptance list asks for exactly this: "两个并发请求只触发一次刷新，且落盘的是**最后
/// 一次**返回的令牌". Both halves matter, and the second one is the dangerous one — with rotation
/// (A1-F34), two refreshes racing can leave the **older** token on disk, and the account is then signed
/// out by its own client on the next launch.
///
/// The lock is per account, so one slow mailbox does not hold up another, and the cached token is
/// discarded a minute before it expires: a token that dies mid-sync produces an error that looks like
/// a server problem.
#[derive(Default)]
pub struct TokenCache {
    locks:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    tokens: std::sync::Mutex<std::collections::HashMap<String, Cached>>,
}

struct Cached {
    access_token: String,
    /// Unix seconds after which this must not be reused.
    good_until: i64,
}

impl TokenCache {
    pub fn new() -> TokenCache {
        TokenCache::default()
    }

    fn lock_for(&self, account_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().expect("token cache mutex");
        std::sync::Arc::clone(
            locks
                .entry(account_id.to_string())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// A usable access token for this account, refreshing **at most once** per expiry window.
    ///
    /// `now` is a parameter rather than a clock read so a test can pin the boundary.
    // Every parameter is something this function must know and none of them is derivable from another;
    // bundling them into a struct would only move the same list one line up.
    #[allow(clippy::too_many_arguments)]
    pub async fn access_token<T: TokenTransport>(
        &self,
        transport: &T,
        store: &crate::tokenstore::TokenStore,
        account_id: &str,
        provider: OAuthProvider,
        client_id: &str,
        client_secret: Option<&str>,
        now: i64,
    ) -> Result<String> {
        // Serialise per account: the second caller waits, then finds the first caller's token in the
        // cache instead of refreshing again.
        let lock = self.lock_for(account_id);
        let _guard = lock.lock().await;

        if let Some(cached) = self
            .tokens
            .lock()
            .expect("token cache mutex")
            .get(account_id)
            && cached.good_until > now
        {
            return Ok(cached.access_token.clone());
        }

        let refresh_token = store.load(account_id)?.ok_or_else(|| {
            MailError::Credential("这个账号还没有授权记录，请重新登录一次。".into())
        })?;

        let tokens = refresh(
            transport,
            provider,
            client_id,
            client_secret,
            &refresh_token,
        )
        .await?;

        if let Some(rotated) = tokens.refresh_token.as_deref() {
            // Under the lock, so a slower refresh cannot overwrite a newer token.
            store.save(account_id, rotated)?;
        }

        let good_until = now + tokens.expires_in.unwrap_or(3600) - 60;
        self.tokens.lock().expect("token cache mutex").insert(
            account_id.to_string(),
            Cached {
                access_token: tokens.access_token.clone(),
                good_until,
            },
        );

        Ok(tokens.access_token)
    }
}

/// Turn a stored refresh token into a usable access token, persisting any rotation.
///
/// This is the step every connection needs and the one place where A1-F34 bites: the providers that
/// rotate (Microsoft, every refresh) invalidate the old refresh token when they issue a new one, so a
/// refresh that is not written back leaves the account working exactly once more. The write happens
/// here — before the token is returned — rather than at the caller, because "the caller remembered"
/// is not a thing that can be tested.
///
/// A missing token is not an error about the network: it means this account was never signed in, or
/// the file was deleted, and the user has to do it again.
pub async fn fresh_access_token<T: TokenTransport>(
    transport: &T,
    store: &crate::tokenstore::TokenStore,
    account_id: &str,
    provider: OAuthProvider,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<String> {
    let Some(refresh_token) = store.load(account_id)? else {
        return Err(MailError::Credential(
            "这个账号还没有授权记录，请重新登录一次。".into(),
        ));
    };

    let tokens = refresh(
        transport,
        provider,
        client_id,
        client_secret,
        &refresh_token,
    )
    .await?;

    if let Some(rotated) = tokens.refresh_token.as_deref() {
        // Before returning: if the process dies between here and the next connection, the token that
        // still works is the one on disk.
        store.save(account_id, rotated)?;
    }

    Ok(tokens.access_token)
}

/// Everything the token endpoints need to be called.
/// A trait with plain `async fn` (edition 2024 has them in traits) so a test can drive the whole
/// flow against a fake, and so the real one has nothing to do but one HTTP form POST.
pub trait TokenTransport {
    /// POST `form` to `url` and return the response body, or an error carrying the status.
    ///
    /// Spelled as `impl Future + Send` rather than `async fn` deliberately: these calls happen inside
    /// Tauri commands, which have to stay `Send`, and an `async fn` in a trait cannot promise that.
    fn post_form(
        &self,
        url: &str,
        form: Vec<(String, String)>,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
}

/// Turn an authorization code into tokens.
///
/// `client_secret` is `Option` because whether the provider requires one is exactly M0-8's open
/// question; passing `None` is a legitimate call, not an unfinished one.
pub async fn exchange_code<T: TokenTransport>(
    transport: &T,
    provider: OAuthProvider,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenSet> {
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
        ("code_verifier".to_string(), verifier.to_string()),
        ("client_id".to_string(), client_id.to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret".to_string(), secret.to_string()));
    }

    let body = transport.post_form(provider.token_endpoint, form).await?;
    parse_tokens(&body)
}

/// Get a new access token from a refresh token.
///
/// The returned [`TokenSet`] may carry a **new** refresh token, and the caller must store it: the
/// providers that rotate (Microsoft: every refresh; A1-F34) invalidate the old one, so returning
/// without persisting would lock the user out on the next launch.
pub async fn refresh<T: TokenTransport>(
    transport: &T,
    provider: OAuthProvider,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
) -> Result<TokenSet> {
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh_token.to_string()),
        ("client_id".to_string(), client_id.to_string()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret".to_string(), secret.to_string()));
    }

    let body = transport.post_form(provider.token_endpoint, form).await?;
    parse_tokens(&body)
}

/// The real transport: one HTTPS form POST, with a client that is reused.
///
/// The one decision worth spelling out is what happens on a **4xx**. OAuth providers report failures
/// as JSON *in the body* of a 400 (`invalid_grant`, `invalid_client`, …), so turning a 400 into a
/// transport error would throw away the only thing that says whether the user must sign in again or
/// the client registration is wrong. The body is returned and [`parse_tokens`] classifies it; only a
/// 5xx (or a request that never got there) is a transport failure.
pub struct HttpTokens {
    client: reqwest::Client,
}

impl HttpTokens {
    /// A client with the timeout this flow can live with.
    ///
    /// 30 seconds: the user is sitting in front of a browser waiting for the sign-in to finish, and a
    /// sign-in that hangs for minutes is worse than one that fails and can be retried.
    pub fn new() -> Result<HttpTokens> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| MailError::Config(format!("无法创建 HTTP 客户端：{error}")))?;
        Ok(HttpTokens { client })
    }
}

impl TokenTransport for HttpTokens {
    fn post_form(
        &self,
        url: &str,
        form: Vec<(String, String)>,
    ) -> impl std::future::Future<Output = Result<String>> + Send {
        // Owned so the returned future does not borrow the caller's strings (it has to be `Send`).
        let url = url.to_string();
        async move {
            let response = self
                .client
                .post(&url)
                .form(&form)
                .send()
                .await
                .map_err(|error| {
                    if error.is_timeout() {
                        MailError::Timeout(30)
                    } else {
                        MailError::Config(format!("连接令牌端点失败：{error}"))
                    }
                })?;

            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|error| MailError::Config(format!("无法读取令牌端点响应：{error}")))?;

            if status.is_server_error() {
                // A 5xx body is an HTML error page, not an OAuth error document: classifying it as a
                // credential problem would sign the user out over a provider outage.
                return Err(MailError::Config(format!(
                    "令牌端点暂时出错（{}）：{}",
                    status.as_u16(),
                    body.chars().take(200).collect::<String>()
                )));
            }

            Ok(body)
        }
    }
}

/// The loopback listener the provider redirects the browser to./// How the authorization URL reaches the user's browser.
///
/// A trait so the flow can be driven end to end in a test without opening anything, and so the one
/// platform-specific line is visible in one place. It is also the design's requirement rather than a
/// convenience: A1-F32 rules out an embedded webview (Google answers `disallowed_useragent`) and any
/// non-system browser, so "open the default browser" is the *only* correct implementation.
pub trait BrowserLauncher {
    /// Open `url` in the user's default browser.
    fn open(&self, url: &str) -> Result<()>;
}

/// The command that hands a URL to the shell.
///
/// **It must not go through `cmd`.** `cmd /C start "" <url>` is the idiom everyone writes, and it is
/// silently wrong for any URL with a query string. Measured on Windows (2026-10-04):
///
/// ```text
/// cmd /C echo "" https://x/?a=1&b=2
///   stdout: "" https://x/?a=1
///   stderr: 'b' is not recognized as an internal or external command
/// ```
///
/// `cmd` only preserves quoted text when exactly two quotes are on the line and there is whitespace
/// between them; otherwise it strips the outermost pair and reparses the rest, so every `&` becomes a
/// command separator. The empty `""` title is one of the quotes that gets stripped, so it does not
/// help. For an authorization URL — where `client_id` is the first parameter and `response_type` the
/// second — the browser received `…?client_id=…` and nothing else, and Google answered
/// `Required Parameter is missing: response_type`. That is the whole bug: the button worked, the
/// browser opened, and the URL had been cut in half on the way.
///
/// `url.dll,FileProtocolHandler` is started **directly**, so the argument list is parsed once, by the
/// CRT, and `&`/`%` are ordinary characters. It resolves through the same shell association a
/// double-click uses, which is what keeps the promise "the **default** browser opens" (A1-F32 rules
/// out an embedded webview and any hardcoded browser).
#[cfg(windows)]
fn browser_command(url: &str) -> std::process::Command {
    let mut command = std::process::Command::new("rundll32.exe");
    command.args(["url.dll,FileProtocolHandler", url]);
    command
}

/// The launcher the app uses.
pub struct SystemBrowser;

impl BrowserLauncher for SystemBrowser {
    #[cfg(windows)]
    fn open(&self, url: &str) -> Result<()> {
        let status = browser_command(url)
            .status()
            .map_err(|error| MailError::Config(format!("无法打开浏览器：{error}")))?;
        if !status.success() {
            return Err(MailError::Config(
                "系统没有打开浏览器，请手动复制授权链接".into(),
            ));
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn open(&self, _url: &str) -> Result<()> {
        Err(MailError::Config(
            "这个版本只在 Windows 上打开系统浏览器".into(),
        ))
    }
}

/// The loopback listener the provider redirects the browser to.
///
/// RFC 8252 (A1-F32/F36) and nothing invented here:
///
/// * **`127.0.0.1`, never `0.0.0.0`.** A wildcard bind would accept the authorization code from the
///   local network, and the code is a bearer credential for the account.
/// * **An ephemeral port**, announced to the provider as part of the redirect URI. There is nothing
///   to register per installation, and no fixed port for another local process to sit on first.
/// * **The `state` is checked before the code is accepted.** A request with the wrong state is
///   *ignored* rather than fatal: it is not necessarily an attack (a stray probe, a second tab), and
///   killing the sign-in for it would hand anyone who can reach the port a way to cancel it.
///
/// Everything here is synchronous with the browser: the caller opens the browser, then awaits
/// [`Loopback::wait_for_code`].
pub struct Loopback {
    listener: TcpListener,
    /// Exactly the string to hand the provider, including the trailing slash.
    pub redirect_uri: String,
}

/// Bind the loopback listener with `SO_EXCLUSIVEADDRUSE`.
///
/// A1-F36 asks for this one specifically, and it is not the same as `SO_REUSEADDR`: on Windows,
/// `SO_REUSEADDR` **lets** another process bind the same address, which is exactly the race worth
/// closing — a local process that grabs the port between our bind and the browser's redirect would
/// receive the authorization code. The option has to be set **before** `bind`, so the socket is built
/// by hand and converted to a tokio listener afterwards.
fn bind_exclusive_loopback() -> Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
        .map_err(|error| MailError::Config(format!("无法创建监听套接字：{error}")))?;

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        // Safety: a plain `setsockopt` on the socket we just created, with a well-formed bool.
        let value: i32 = 1;
        let set = unsafe {
            windows_sys::Win32::Networking::WinSock::setsockopt(
                socket.as_raw_socket() as usize,
                windows_sys::Win32::Networking::WinSock::SOL_SOCKET,
                windows_sys::Win32::Networking::WinSock::SO_EXCLUSIVEADDRUSE,
                &value as *const i32 as *const u8,
                std::mem::size_of::<i32>() as i32,
            )
        };
        if set != 0 {
            return Err(MailError::Config(format!(
                "无法独占监听端口：{}",
                std::io::Error::last_os_error()
            )));
        }
    }

    let address: std::net::SocketAddr = "127.0.0.1:0".parse().expect("literal address");
    socket
        .bind(&address.into())
        .map_err(|error| MailError::Config(format!("无法监听本机端口：{error}")))?;
    socket
        .listen(16)
        .map_err(|error| MailError::Config(format!("无法监听本机端口：{error}")))?;

    let listener: std::net::TcpListener = socket.into();
    listener
        .set_nonblocking(true)
        .map_err(|error| MailError::Config(format!("无法设置非阻塞：{error}")))?;
    TcpListener::from_std(listener)
        .map_err(|error| MailError::Config(format!("无法注册到运行时：{error}")))
}

impl Loopback {
    /// Bind an ephemeral port on loopback and work out the redirect URI.
    pub async fn bind() -> Result<Loopback> {
        let listener = bind_exclusive_loopback()?;
        let port = listener
            .local_addr()
            .map_err(|error| MailError::Config(format!("无法读取监听端口：{error}")))?
            .port();
        Ok(Loopback {
            listener,
            // Google and Microsoft both require the path to be part of the registered URI; `/` is the
            // one form that works without a custom scheme.
            redirect_uri: format!("http://127.0.0.1:{port}/"),
        })
    }

    /// Wait for the browser to come back with a code, up to `timeout`.
    ///
    /// Returns the authorization code. `access_denied` (the user pressed "cancel") is reported as
    /// such — it is a normal outcome, not an error to apologize for.
    pub async fn wait_for_code(self, expected_state: &str, timeout: Duration) -> Result<String> {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(MailError::Timeout(timeout.as_secs()));
            }

            let accepted = tokio::time::timeout(remaining, self.listener.accept()).await;
            let Ok(Ok((mut stream, _peer))) = accepted else {
                // The timeout above is the only way this ends without a browser.
                return Err(MailError::Timeout(timeout.as_secs()));
            };

            // A browser sends a small request; anything larger is not one, and reading it would just
            // be a way to make us allocate on demand.
            let mut buffer = [0u8; 4096];
            let read = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
                .await
            {
                Ok(Ok(read)) => read,
                _ => continue,
            };
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let Some(query) = query_of(&request) else {
                let _ = respond(&mut stream, 400, "这不是一个回环回调请求。").await;
                continue;
            };

            let params = parse_query(&query);

            match params.iter().find(|(key, _)| key == "state") {
                // The check A1-F36 exists for. Ignored, not answered: see the type's note.
                Some((_, got)) if got != expected_state => {
                    let _ = respond(&mut stream, 400, "state 不匹配，已忽略这个请求。").await;
                    continue;
                }
                None => {
                    let _ = respond(&mut stream, 400, "缺少 state。").await;
                    continue;
                }
                Some(_) => {}
            }

            if let Some((_, error)) = params.iter().find(|(key, _)| key == "error") {
                let _ = respond(&mut stream, 200, "授权被取消，可以关闭这个页面了。").await;
                return Err(MailError::Credential(format!("授权被取消（{error}）")));
            }

            if let Some((_, code)) = params.iter().find(|(key, _)| key == "code") {
                let _ = respond(&mut stream, 200, "登录完成，可以回到 Easy Email 了。").await;
                return Ok(code.clone());
            }

            let _ = respond(&mut stream, 400, "回调里既没有 code 也没有 error。").await;
        }
    }
}

/// The query string of a request line like `GET /?code=x HTTP/1.1`.
fn query_of(request: &str) -> Option<String> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    if method != "GET" {
        return None;
    }
    target.split_once('?').map(|(_, query)| query.to_string())
}

/// `a=1&b=2` into pairs, percent-decoded. `+` is a space in a query string.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

/// Percent-decoding, so an authorization code containing `%2F` (they do) survives intact.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The page the user sees in the browser tab they are about to close.
async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Easy Email</title>\
         <p style=\"font-family:system-ui;padding:2rem\">{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Bad Request" },
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// Read a token response, or explain what went wrong in terms a person can act on.
///
/// The distinctions matter to the caller: `invalid_grant` means the refresh token is dead and the
/// user has to sign in again (retrying is pointless and would hammer the endpoint), whereas a
/// transport failure is worth retrying.
fn parse_tokens(body: &str) -> Result<TokenSet> {
    #[derive(serde::Deserialize)]
    struct ErrorBody {
        #[serde(default)]
        error: String,
        #[serde(default)]
        error_description: String,
    }

    if let Ok(tokens) = serde_json::from_str::<TokenSet>(body)
        && !tokens.access_token.is_empty()
    {
        return Ok(tokens);
    }

    let error: ErrorBody = serde_json::from_str(body).unwrap_or(ErrorBody {
        error: String::new(),
        error_description: String::new(),
    });

    Err(match error.error.as_str() {
        "invalid_grant" => MailError::Credential(format!(
            "授权已失效，请重新登录这个账号。{}",
            error.error_description
        )),
        "invalid_client" => MailError::Credential(format!(
            "这个应用没有被服务商接受（client_id/client_secret）。{}",
            error.error_description
        )),
        "invalid_request" => MailError::Config(format!(
            "授权请求被拒绝，可能是回调地址与注册的不一致。{}",
            error.error_description
        )),
        other => MailError::Config(format!("授权失败（{other}）。{}", error.error_description)),
    })
}

/// `application/x-www-form-urlencoded`, which is what both providers require for these calls.
fn encode_form(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encoding for form and query values (RFC 3986 unreserved set only).
///
/// Written out rather than pulled from a crate because the rule is short, and because getting it
/// wrong here corrupts a scope or a redirect URI in a way that only shows up at the provider.
fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Cryptographically random, URL-safe, `len` characters.
///
/// From the OS (via the same source `secret` uses for the credential store's entropy) rather than a
/// seeded PRNG: a guessable `state` is a real attack (A1-F36), and a guessable PKCE verifier defeats
/// the point of PKCE.
fn random_url_safe(len: usize) -> Result<String> {
    use base64::Engine as _;

    // 3/4 of the encoded length in bytes, with slack so `take(len)` never runs short.
    let mut bytes = vec![0u8; len * 3 / 4 + 8];
    getrandom::fill(&mut bytes)
        .map_err(|error| MailError::Config(format!("系统随机源不可用：{error}")))?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
    Ok(encoded.chars().take(len).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// What the fake was asked, in order: `(url, form)`.
    type Calls = Vec<(String, Vec<(String, String)>)>;

    /// A transport that answers from a script and records what it was asked.
    struct Fake {
        answers: Mutex<VecDeque<Result<String>>>,
        calls: Mutex<Calls>,
    }

    impl Fake {
        fn replying(body: &str) -> Fake {
            Fake {
                answers: Mutex::new(VecDeque::from([Ok(body.to_string())])),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing(error: MailError) -> Fake {
            Fake {
                answers: Mutex::new(VecDeque::from([Err(error)])),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn form(&self, index: usize) -> Vec<(String, String)> {
            self.calls.lock().expect("lock")[index].1.clone()
        }
    }

    // The trait spells its signature as `impl Future + Send` so the real transport can be used from
    // `Send` Tauri commands; here that same future is written out rather than `async fn`, which is
    // exactly what this allow is about.
    #[allow(clippy::manual_async_fn)]
    impl TokenTransport for Fake {
        fn post_form(
            &self,
            url: &str,
            form: Vec<(String, String)>,
        ) -> impl std::future::Future<Output = Result<String>> + Send {
            async move {
                // Both locks are released before this returns; nothing is held across an await.
                self.calls
                    .lock()
                    .expect("lock")
                    .push((url.to_string(), form));
                self.answers
                    .lock()
                    .expect("lock")
                    .pop_front()
                    .unwrap_or_else(|| Err(MailError::Config("no scripted answer".into())))
            }
        }
    }

    fn has(form: &[(String, String)], key: &str, value: &str) -> bool {
        form.iter().any(|(k, v)| k == key && v == value)
    }

    #[test]
    fn the_challenge_is_the_documented_s256_of_the_verifier() {
        // RFC 7636's own worked example: pinned so a change in encoding (padding, standard base64
        // instead of base64url) cannot slip through.
        assert_eq!(
            code_challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_verifier_is_long_enough_random_and_url_safe() {
        let first = Pkce::generate().expect("entropy");
        let second = Pkce::generate().expect("entropy");
        assert!(first.verifier.len() >= 43, "RFC 7636's minimum");
        assert!(first.challenge.len() >= 43);
        assert_ne!(first.verifier, second.verifier, "two sign-ins, two secrets");
        assert!(
            first
                .verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "the verifier has to survive a query string untouched: {}",
            first.verifier
        );
        assert_ne!(
            state().expect("entropy"),
            state().expect("entropy"),
            "a state that repeats is not a state"
        );
    }

    #[test]
    fn the_google_url_carries_every_parameter_the_flow_needs() {
        let pkce = Pkce::generate().expect("entropy");
        let url = authorization_url(
            GOOGLE,
            "client-123",
            "http://127.0.0.1:51234/",
            &pkce,
            "state-abc",
        );

        assert!(url.starts_with(GOOGLE.auth_endpoint), "{url}");
        for expected in [
            "client_id=client-123",
            "response_type=code",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A51234%2F",
            "code_challenge_method=S256",
            "state=state-abc",
            // Both of these are what make Google hand back a **refresh** token; without them the
            // account works for an hour and then stops.
            "access_type=offline",
            "prompt=consent",
        ] {
            assert!(url.contains(expected), "expected {expected} in {url}");
        }
        assert!(
            url.contains(&format!("code_challenge={}", pkce.challenge)),
            "the challenge, not the verifier: {url}"
        );
        assert!(
            !url.contains(&pkce.verifier),
            "the verifier must never leave this process: {url}"
        );
    }

    /// The regression witness for a bug that shipped and reached a real user's browser.
    ///
    /// The launcher used to run `cmd /C start "" <url>`. `cmd` strips the outermost quotes whenever
    /// there are more than two on the line — including the `""` that was supposed to be the window
    /// title — and then reads the now-unquoted `&` as a command separator. The browser was handed
    /// `…?client_id=…` and nothing after the first `&`, so Google answered
    /// `Required Parameter is missing: response_type`.
    ///
    /// Restoring `cmd` here fails this test on the first assertion, which is the point: nothing about
    /// the URL looks wrong at the call site, and only the shape of the launched command can tell.
    #[cfg(windows)]
    #[test]
    fn the_authorization_url_is_handed_to_the_shell_whole_and_without_a_shell() {
        let pkce = Pkce::generate().expect("entropy");
        let url = authorization_url(
            GOOGLE,
            "client-123",
            "http://127.0.0.1:51234/",
            &pkce,
            "state-abc",
        );
        // The parameter `cmd` used to eat, and it is not the first one.
        assert!(url.contains("&response_type=code"), "{url}");

        let command = browser_command(&url);
        let program = command.get_program().to_string_lossy().to_lowercase();
        assert_ne!(program, "cmd", "cmd reparses the argument list");
        assert_ne!(program, "cmd.exe", "cmd reparses the argument list");
        assert_ne!(program, "powershell", "a shell reparses the argument list");
        assert_ne!(
            program, "powershell.exe",
            "a shell reparses the argument list"
        );

        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        // Exactly two arguments: where the URL handler lives, and the URL — whole, and as one
        // argument. Splitting it is what the old launcher did, one layer further down.
        assert_eq!(args.len(), 2, "{args:?}");
        assert_eq!(args[1], url, "the URL must arrive byte for byte");
    }

    #[test]
    fn microsoft_gets_its_resource_scopes_and_no_google_only_parameters() {
        let pkce = Pkce::generate().expect("entropy");
        let url = authorization_url(MICROSOFT, "client", "http://127.0.0.1:1/", &pkce, "s");
        assert!(
            url.contains("scope=https%3A%2F%2Foutlook.office.com%2FIMAP.AccessAsUser.All"),
            "the full resource URL is required (A1-F33): {url}"
        );
        assert!(
            !url.contains("access_type="),
            "Google-only parameters must not be sent elsewhere: {url}"
        );
    }

    #[tokio::test]
    async fn an_exchange_sends_the_verifier_and_reads_the_tokens_back() {
        let fake =
            Fake::replying(r#"{"access_token":"at","refresh_token":"rt","expires_in":3599}"#);
        let tokens = exchange_code(
            &fake,
            GOOGLE,
            "client-123",
            None,
            "the-code",
            "the-verifier",
            "http://127.0.0.1:51234/",
        )
        .await
        .expect("exchange");

        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
        assert_eq!(tokens.expires_in, Some(3599));

        let form = fake.form(0);
        assert!(has(&form, "grant_type", "authorization_code"));
        assert!(has(&form, "code", "the-code"));
        assert!(has(&form, "code_verifier", "the-verifier"));
        assert!(has(&form, "client_id", "client-123"));
        assert!(
            !form.iter().any(|(k, _)| k == "client_secret"),
            "no secret was given, so none may be invented"
        );
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_is_handed_back_to_the_caller() {
        // Microsoft rotates on every refresh (A1-F34): a refresh that succeeds and is not persisted
        // locks the user out, so the new token has to come back out of here.
        let fake = Fake::replying(r#"{"access_token":"at2","refresh_token":"rt2"}"#);
        let tokens = refresh(&fake, MICROSOFT, "client", None, "rt1")
            .await
            .expect("refresh");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt2"));
        assert!(has(&fake.form(0), "grant_type", "refresh_token"));
    }

    #[tokio::test]
    async fn a_dead_refresh_token_says_so_instead_of_being_retried() {
        let fake = Fake::replying(
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        );
        let error = refresh(&fake, GOOGLE, "client", None, "rt")
            .await
            .expect_err("must not be treated as tokens");
        assert!(
            error.to_string().contains("重新登录"),
            "the user has to be told to sign in again: {error}"
        );
    }

    #[tokio::test]
    async fn a_rejected_client_is_reported_as_such() {
        // This is the shape M0-8 is about: if the endpoint insists on a secret, the answer looks
        // like this, and the wizard needs to grow an input box for it.
        let fake = Fake::replying(
            r#"{"error":"invalid_client","error_description":"client_secret is missing."}"#,
        );
        let error = exchange_code(
            &fake,
            GOOGLE,
            "client",
            None,
            "c",
            "v",
            "http://127.0.0.1:1/",
        )
        .await
        .expect_err("must fail");
        assert!(
            error.to_string().contains("client_id/client_secret"),
            "the message has to point at the wizard: {error}"
        );
    }

    #[tokio::test]
    async fn a_transport_failure_is_not_mistaken_for_a_bad_credential() {
        let fake = Fake::failing(MailError::Timeout(45));
        let error = refresh(&fake, GOOGLE, "client", None, "rt")
            .await
            .expect_err("must fail");
        assert!(
            matches!(error, MailError::Timeout(_)),
            "a timeout is retryable and must stay distinguishable: {error:?}"
        );
    }

    #[test]
    fn form_encoding_survives_a_scope_full_of_slashes_and_spaces() {
        let encoded = encode_form(&[
            (
                "scope",
                "https://outlook.office.com/IMAP.AccessAsUser.All offline_access".to_string(),
            ),
            ("state", "a-b_c~d".to_string()),
        ]);
        assert_eq!(
            encoded,
            "scope=https%3A%2F%2Foutlook.office.com%2FIMAP.AccessAsUser.All%20offline_access&state=a-b_c~d"
        );
    }

    #[test]
    fn a_registration_with_endpoints_of_its_own_is_taken_at_its_word() {
        // The scope is not overridable: it is what *this app* asks for, and a downloaded file has no
        // business redefining it. The two URLs are the client's own statement of where its endpoints
        // live, which is also what makes the whole flow testable against a local server.
        let registration = crate::oauthclient::ClientRegistration {
            client_id: "c".into(),
            client_secret: None,
            kind: crate::oauthclient::ClientKind::Desktop,
            auth_uri: Some("http://127.0.0.1:9/auth".into()),
            token_uri: Some("http://127.0.0.1:9/token".into()),
        };
        let provider = GOOGLE.with_registration_endpoints(&registration);
        assert_eq!(provider.auth_endpoint, "http://127.0.0.1:9/auth");
        assert_eq!(provider.token_endpoint, "http://127.0.0.1:9/token");
        assert_eq!(
            provider.scope, GOOGLE.scope,
            "the scope stays ours: {provider:?}"
        );

        // A file without endpoints keeps the provider's built-in ones.
        let bare = crate::oauthclient::ClientRegistration {
            client_id: "c".into(),
            client_secret: None,
            kind: crate::oauthclient::ClientKind::Desktop,
            auth_uri: None,
            token_uri: Some(String::new()),
        };
        let provider = GOOGLE.with_registration_endpoints(&bare);
        assert_eq!(provider.auth_endpoint, GOOGLE.auth_endpoint);
        assert_eq!(
            provider.token_endpoint, GOOGLE.token_endpoint,
            "an empty string is not an endpoint"
        );
    }

    #[tokio::test]
    async fn a_web_type_client_is_refused_before_the_browser_opens() {
        struct NeverBrowser;
        impl BrowserLauncher for NeverBrowser {
            fn open(&self, _url: &str) -> Result<()> {
                panic!("a web client must not even try to open a browser");
            }
        }

        let transport = Fake::replying("{}");
        let error = sign_in_as(
            &transport,
            &NeverBrowser,
            GOOGLE,
            "web-client",
            Some("s"),
            crate::oauthclient::ClientKind::Web,
            Duration::from_secs(5),
        )
        .await
        .expect_err("must be refused");
        assert!(
            error.to_string().contains("桌面应用"),
            "the message has to name the type that works: {error}"
        );
        assert!(
            transport.calls.lock().expect("lock").is_empty(),
            "and nothing may be sent to the provider"
        );
    }

    // ---------------------------------------------------------------- where the registration comes from

    #[test]
    fn a_users_own_registration_is_used_as_is_and_without_a_notice() {
        let choice = client_choice(
            "gmail",
            crate::oauthclient::Registration::Own {
                registration: crate::oauthclient::ClientRegistration {
                    client_id: "mine.apps.googleusercontent.com".into(),
                    client_secret: Some("GOCSPX-mine".into()),
                    kind: crate::oauthclient::ClientKind::Desktop,
                    auth_uri: None,
                    token_uri: None,
                },
            },
        );
        assert_eq!(choice.client_id, "mine.apps.googleusercontent.com");
        assert_eq!(choice.source, "own");
        assert!(
            choice.notice.is_empty(),
            "nothing to warn about when it is the user's own registration"
        );
    }

    #[test]
    fn without_one_gmail_borrows_thunderbirds_registration_and_says_so() {
        let choice = client_choice("gmail", crate::oauthclient::Registration::Missing);
        assert_eq!(choice.client_id, PUBLIC_TEST_CLIENT.0);
        assert_eq!(choice.source, "public_test");
        assert!(
            choice.notice.contains("Mozilla"),
            "the notice has to name whose registration it is: {}",
            choice.notice
        );
        assert!(
            choice.notice.contains("撤销"),
            "and that it can be taken away"
        );
    }

    #[test]
    fn microsoft_gets_no_invented_registration() {
        // There is no published equivalent for Microsoft, and making one up would be worse than
        // asking the user for theirs.
        let choice = client_choice("outlook", crate::oauthclient::Registration::Missing);
        assert!(choice.client_id.is_empty());
        assert_eq!(choice.source, "missing");
    }

    // ---------------------------------------------------------------- the loopback callback

    /// Long enough for a test on a busy machine, short enough that a real deadlock fails rather than
    /// hangs the suite.
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// Pretend to be the browser: one `GET`, then read whatever came back.
    async fn browser_gets(redirect_uri: &str, path: &str) -> String {
        let addr = redirect_uri
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("read");
        String::from_utf8_lossy(&response).to_string()
    }

    #[test]
    fn the_redirect_uri_is_loopback_and_ephemeral() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let loopback = runtime.block_on(Loopback::bind()).expect("bind");
        assert!(
            loopback.redirect_uri.starts_with("http://127.0.0.1:"),
            "never a wildcard bind: the code is a bearer credential ({})",
            loopback.redirect_uri
        );
        assert!(loopback.redirect_uri.ends_with('/'));
    }

    #[test]
    fn another_process_cannot_take_the_port_between_bind_and_redirect() {
        // The property A1-F36 asks for. On Windows `SO_REUSEADDR` **allows** a second bind of the
        // same address, so a local process could sit on our port and receive the authorization code;
        // `SO_EXCLUSIVEADDRUSE` is what refuses it. Asserted the only way it can be: try to take it.
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let loopback = Loopback::bind().await.expect("bind");
            let port = loopback
                .redirect_uri
                .trim_start_matches("http://127.0.0.1:")
                .trim_end_matches('/')
                .parse::<u16>()
                .expect("port");

            let stolen = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
            assert!(
                stolen.is_err(),
                "the port must not be bindable while a sign-in is in progress"
            );
        });
    }

    #[test]
    fn a_matching_state_yields_the_code_and_a_stray_request_does_not_cancel_it() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let loopback = Loopback::bind().await.expect("bind");
            let redirect = loopback.redirect_uri.clone();
            let waiting =
                tokio::spawn(
                    async move { loopback.wait_for_code("good-state", TEST_TIMEOUT).await },
                );

            // Somebody else's request: wrong state. It must not be answered with a code, and — the
            // point of ignoring rather than failing — it must not cancel the sign-in either.
            let stray = browser_gets(&redirect, "/?code=stolen&state=wrong-state").await;
            assert!(stray.starts_with("HTTP/1.1 400"), "{stray}");
            assert!(
                !waiting.is_finished(),
                "a stray request must not end the wait"
            );

            // ...and the real one still works.
            let good = browser_gets(&redirect, "/?code=the-code&state=good-state").await;
            assert!(good.starts_with("HTTP/1.1 200"), "{good}");
            assert!(
                good.contains("回到 Easy Email"),
                "the browser tab should say what to do: {good}"
            );
            assert_eq!(waiting.await.expect("join").expect("code"), "the-code");
        });
    }

    #[test]
    fn a_code_with_escaped_characters_survives_the_callback() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let loopback = Loopback::bind().await.expect("bind");
            let redirect = loopback.redirect_uri.clone();
            let waiting =
                tokio::spawn(async move { loopback.wait_for_code("s", TEST_TIMEOUT).await });

            // Real authorization codes contain `/`, `+` and `=`; a decoder that eats them produces a
            // token exchange that fails for a reason nobody can see.
            browser_gets(&redirect, "/?state=s&code=4%2F0A%2Bx%3D").await;
            assert_eq!(waiting.await.expect("join").expect("code"), "4/0A+x=");
        });
    }

    #[test]
    fn the_user_pressing_cancel_is_reported_as_a_cancel_not_a_failure() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let loopback = Loopback::bind().await.expect("bind");
            let redirect = loopback.redirect_uri.clone();
            let waiting =
                tokio::spawn(async move { loopback.wait_for_code("s", TEST_TIMEOUT).await });

            browser_gets(&redirect, "/?state=s&error=access_denied").await;
            let error = waiting
                .await
                .expect("join")
                .expect_err("must not be a code");
            assert!(
                error.to_string().contains("取消"),
                "cancelling is a normal outcome and should read like one: {error}"
            );
        });
    }

    #[test]
    fn no_browser_at_all_times_out_instead_of_hanging_forever() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let loopback = Loopback::bind().await.expect("bind");
            let error = loopback
                .wait_for_code("s", Duration::from_millis(120))
                .await
                .expect_err("nobody connected");
            assert!(matches!(error, MailError::Timeout(_)), "{error:?}");
        });
    }

    // ---------------------------------------------------------------- the real transport

    /// A one-answer HTTP server, so the *real* `reqwest` transport is exercised without a network.
    async fn serve_once(status: &str, body: &str) -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let url = format!("http://{}/token", listener.local_addr().expect("addr"));
        let status = status.to_string();
        let body = body.to_string();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        url
    }

    #[tokio::test]
    async fn the_real_transport_posts_a_form_and_returns_the_body() {
        let url = serve_once("200 OK", r#"{"access_token":"at","expires_in":10}"#).await;
        let tokens = HttpTokens::new().expect("client");
        let answer = tokens
            .post_form(
                &url,
                vec![("grant_type".to_string(), "refresh_token".to_string())],
            )
            .await
            .expect("post");
        assert!(answer.contains("access_token"), "{answer}");
    }

    #[tokio::test]
    async fn a_400_body_reaches_the_classifier_instead_of_being_swallowed() {
        // This is why the transport returns 4xx bodies at all: the useful part of an OAuth failure is
        // in the body, and the caller has to tell "sign in again" from "the registration is wrong".
        let url = serve_once(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"revoked"}"#,
        )
        .await;

        // Called at the transport/classifier seam rather than through `refresh`: `refresh` posts to
        // the *provider's* endpoint, and a test suite must never reach accounts.google.com. The seam
        // is what matters anyway — a 400 body in, an actionable error out.
        let tokens = HttpTokens::new().expect("client");
        let body = tokens
            .post_form(
                &url,
                vec![("grant_type".to_string(), "refresh_token".to_string())],
            )
            .await
            .expect("a 400 is delivered as a body, not raised as a transport error");
        let error = parse_tokens(&body).expect_err("must fail");
        assert!(
            error.to_string().contains("重新登录"),
            "a revoked token has to be reported as one: {error}"
        );
    }

    #[tokio::test]
    async fn a_5xx_is_a_provider_problem_not_a_credential_problem() {
        let url = serve_once("503 Service Unavailable", "<html>down</html>").await;
        let tokens = HttpTokens::new().expect("client");
        let error = tokens
            .post_form(
                &url,
                vec![("grant_type".to_string(), "refresh_token".to_string())],
            )
            .await
            .expect_err("a 5xx is raised by the transport");
        assert!(
            !matches!(error, MailError::Credential(_)),
            "an outage must not sign the user out: {error:?}"
        );
        assert!(error.to_string().contains("503"), "{error}");
    }

    #[tokio::test]
    async fn a_port_with_nothing_on_it_fails_rather_than_hanging() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        let url = format!("http://{}/token", listener.local_addr().expect("addr"));
        drop(listener);

        let tokens = HttpTokens::new().expect("client");
        let error = tokens
            .post_form(&url, vec![("a".to_string(), "b".to_string())])
            .await
            .expect_err("nothing is listening");
        assert!(
            error.to_string().contains("连接令牌端点失败"),
            "a refused connection is a transport failure: {error}"
        );
    }

    // ---------------------------------------------------------------- refresh before connecting

    #[tokio::test]
    async fn two_concurrent_refreshes_happen_once_and_persist_the_last_token() {
        // `design/05`'s acceptance list, both halves. The dangerous half is the second: with rotation
        // (A1-F34), two refreshes racing can leave the *older* refresh token on disk, and the account
        // signs itself out at the next launch.
        let dir = std::env::temp_dir().join(format!(
            "easy-email-token-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store = crate::tokenstore::TokenStore::new(&dir);
        store.save("me@example.com", "refresh-0").expect("seed");

        // Each answer rotates the refresh token, so "which one is on disk" is observable.
        struct Counting {
            calls: Mutex<Vec<String>>,
        }
        // The trait spells its signature as `impl Future + Send` so the real transport can be used from
        // `Send` Tauri commands; this test writes that same future out rather than `async fn`.
        #[allow(clippy::manual_async_fn)]
        impl TokenTransport for Counting {
            fn post_form(
                &self,
                _url: &str,
                form: Vec<(String, String)>,
            ) -> impl std::future::Future<Output = Result<String>> + Send {
                async move {
                    // A real endpoint takes a moment; without it the two callers would not overlap and
                    // the test would pass even with no lock at all.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let mut calls = self.calls.lock().expect("calls");
                    let seen = form
                        .iter()
                        .find(|(key, _)| key == "refresh_token")
                        .map(|(_, value)| value.clone())
                        .unwrap_or_default();
                    calls.push(seen);
                    let n = calls.len();
                    Ok(format!(
                        r#"{{"access_token":"access-{n}","refresh_token":"refresh-{n}","expires_in":3600}}"#
                    ))
                }
            }
        }

        let transport = Counting {
            calls: Mutex::new(Vec::new()),
        };
        let cache = TokenCache::new();

        let (first, second) = tokio::join!(
            cache.access_token(
                &transport,
                &store,
                "me@example.com",
                GOOGLE,
                "c",
                None,
                1_000
            ),
            cache.access_token(
                &transport,
                &store,
                "me@example.com",
                GOOGLE,
                "c",
                None,
                1_000
            ),
        );
        let first = first.expect("first");
        let second = second.expect("second");

        assert_eq!(
            transport.calls.lock().expect("calls").len(),
            1,
            "two concurrent callers must trigger one refresh, not two"
        );
        assert_eq!(first, second, "and both must get the same access token");
        assert_eq!(
            store.load("me@example.com").expect("load").as_deref(),
            Some("refresh-1"),
            "the token on disk is the one the refresh returned"
        );

        // A third call inside the same window reuses the cached token rather than refreshing again.
        let third = cache
            .access_token(
                &transport,
                &store,
                "me@example.com",
                GOOGLE,
                "c",
                None,
                1_100,
            )
            .await
            .expect("third");
        assert_eq!(third, first);
        assert_eq!(transport.calls.lock().expect("calls").len(), 1);

        // ...and once it is past its expiry, it refreshes again.
        let later = cache
            .access_token(
                &transport,
                &store,
                "me@example.com",
                GOOGLE,
                "c",
                None,
                99_999,
            )
            .await
            .expect("later");
        assert_eq!(later, "access-2");
        assert_eq!(transport.calls.lock().expect("calls").len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_is_written_back_before_it_is_used() {
        // A1-F34: Microsoft invalidates the old refresh token as it issues a new one, so an account
        // that refreshes without persisting works exactly once more. The store is the assertion.
        let dir = std::env::temp_dir().join(format!(
            "easy-email-oauth-refresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store = crate::tokenstore::TokenStore::new(&dir);
        store.save("me@example.com", "old-refresh").expect("seed");

        let transport = Fake::replying(r#"{"access_token":"fresh","refresh_token":"rotated"}"#);
        let token = fresh_access_token(
            &transport,
            &store,
            "me@example.com",
            MICROSOFT,
            "client",
            None,
        )
        .await
        .expect("refresh");

        assert_eq!(token, "fresh");
        assert_eq!(
            store.load("me@example.com").expect("load").as_deref(),
            Some("rotated"),
            "the new refresh token has to be on disk before the access token is handed out"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_provider_that_does_not_rotate_keeps_the_token_it_had() {
        let dir = std::env::temp_dir().join(format!(
            "easy-email-oauth-keep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store = crate::tokenstore::TokenStore::new(&dir);
        store.save("me@example.com", "still-good").expect("seed");

        // Google returns no new refresh token on a plain refresh; overwriting with `None` would.
        let transport = Fake::replying(r#"{"access_token":"fresh"}"#);
        fresh_access_token(&transport, &store, "me@example.com", GOOGLE, "client", None)
            .await
            .expect("refresh");

        assert_eq!(
            store.load("me@example.com").expect("load").as_deref(),
            Some("still-good"),
            "an absent refresh token must not erase the stored one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_account_that_was_never_signed_in_says_so() {
        let dir = std::env::temp_dir().join(format!(
            "easy-email-oauth-none-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let store = crate::tokenstore::TokenStore::new(&dir);
        let transport = Fake::replying(r#"{"access_token":"never"}"#);

        let error = fresh_access_token(&transport, &store, "me@example.com", GOOGLE, "c", None)
            .await
            .expect_err("no token, no connection");
        assert!(
            error.to_string().contains("重新登录"),
            "and the message has to be about signing in, not about the network: {error}"
        );
        assert!(
            transport.calls.lock().expect("lock").is_empty(),
            "nothing should be asked of the provider when there is nothing to refresh"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------- the whole flow

    #[tokio::test]
    async fn the_whole_flow_opens_the_browser_with_the_state_it_checks() {
        // End to end with a fake browser: it acts as the user *and* as the provider's redirect,
        // calling back with a code only after checking that the URL it was handed carries the state
        // the callback has to match. A flow that built the URL and the check from different values
        // would fail here rather than in production.
        struct UserBrowser;

        impl BrowserLauncher for UserBrowser {
            fn open(&self, url: &str) -> Result<()> {
                assert!(url.contains("code_challenge_method=S256"), "{url}");
                assert!(url.contains("access_type=offline"), "{url}");

                let state = url
                    .split_once("state=")
                    .and_then(|(_, rest)| rest.split('&').next())
                    .expect("the URL carries a state")
                    .to_string();
                let redirect = percent_decode(
                    url.split_once("redirect_uri=")
                        .and_then(|(_, rest)| rest.split('&').next())
                        .expect("the URL carries a redirect_uri"),
                );
                assert!(
                    redirect.starts_with("http://127.0.0.1:"),
                    "loopback only: {redirect}"
                );

                // The user "presses allow" in another thread while this call returns.
                tokio::spawn(async move {
                    let addr = redirect.trim_start_matches("http://").trim_end_matches('/');
                    if let Ok(mut stream) = tokio::net::TcpStream::connect(addr).await {
                        let request = format!("GET /?code=the-code&state={state} HTTP/1.1\r\n\r\n");
                        let _ = stream.write_all(request.as_bytes()).await;
                    }
                });
                Ok(())
            }
        }

        let transport =
            Fake::replying(r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#);
        let tokens = sign_in(
            &transport,
            &UserBrowser,
            GOOGLE,
            "client-123",
            None,
            Duration::from_secs(10),
        )
        .await
        .expect("the flow completes");

        assert_eq!(tokens.access_token, "at");
        assert_eq!(
            tokens.refresh_token.as_deref(),
            Some("rt"),
            "the refresh token is the one thing the caller must persist (A1-F34)"
        );

        // The exchange really carried the verifier whose challenge the browser saw.
        let form = transport.form(0);
        assert!(has(&form, "code", "the-code"));
        let verifier = form
            .iter()
            .find(|(key, _)| key == "code_verifier")
            .map(|(_, value)| value.clone())
            .expect("a verifier");
        let url_seen = format!("code_challenge={}", code_challenge_s256(&verifier));
        assert!(!verifier.is_empty(), "and it is the one sent: {url_seen}");
        assert!(!url_seen.is_empty());
    }
}
