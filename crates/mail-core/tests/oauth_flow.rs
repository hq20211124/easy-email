//! The whole OAuth flow through the **real** pieces, not the fakes.
//!
//! `crates/mail-core/src/oauth.rs` drives its flow with a fake transport and a fake browser, and the
//! real `reqwest` transport is exercised against a one-answer server. Each half passes on its own —
//! which is exactly the shape that hid the bug M1a-7 found: the pieces were fine and the fit was not.
//!
//! So this file runs the flow the way the app runs it: the **real** `HttpTokens` posts to a local HTTP
//! server standing in for Google's token endpoint, and the browser half is a task that calls back into
//! the **real** loopback listener. What it adds over the unit tests is the seam between them — a form
//! encoded by one layer and parsed by another, a loopback port announced in a URL and dialled back, a
//! `state` generated in one place and compared in another.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mail_core::oauth::{BrowserLauncher, GOOGLE, HttpTokens, OAuthProvider, sign_in_as};
use mail_core::oauthclient::ClientKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A stand-in for `https://oauth2.googleapis.com/token`: answers one POST with a token document and
/// records the form it was sent.
async fn spawn_token_endpoint(body: &str) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let url = format!("http://{}/token", listener.local_addr().expect("addr"));
    let seen: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let recorded = Arc::clone(&seen);
    let body = body.to_string();

    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = vec![0u8; 8192];
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            // Keep only the form: the request line and headers are not what is under test.
            let raw = String::from_utf8_lossy(&buffer[..read]).to_string();
            let form = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            *recorded.lock().expect("record") = form;

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });

    (url, seen)
}

/// The "user presses allow" half: read the state and redirect URI out of the URL we were handed, then
/// come back to the loopback listener carrying the same state.
struct UserBrowser;

impl BrowserLauncher for UserBrowser {
    fn open(&self, url: &str) -> mail_core::Result<()> {
        let state = url
            .split_once("state=")
            .and_then(|(_, rest)| rest.split('&').next())
            .expect("the URL carries a state")
            .to_string();
        let redirect = url
            .split_once("redirect_uri=")
            .and_then(|(_, rest)| rest.split('&').next())
            .expect("the URL carries a redirect_uri")
            .replace("%3A", ":")
            .replace("%2F", "/");

        tokio::spawn(async move {
            let addr = redirect.trim_start_matches("http://").trim_end_matches('/');
            if let Ok(mut stream) = TcpStream::connect(addr).await {
                let request = format!(
                    "GET /?code=the-real-code&state={state} HTTP/1.1\r\nHost: {addr}\r\n\r\n"
                );
                let _ = stream.write_all(request.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        Ok(())
    }
}

#[tokio::test]
async fn the_oauth_flow_runs_through_the_real_transport_and_the_real_loopback() {
    let (token_url, form) = spawn_token_endpoint(
        r#"{"access_token":"real-access","refresh_token":"real-refresh","expires_in":3600}"#,
    )
    .await;
    // `OAuthProvider` holds `&'static str` endpoints; a local server needs a leaked string, which is
    // fine for a test process and keeps the production type free of a `String` it does not need.
    let provider = OAuthProvider {
        token_endpoint: Box::leak(token_url.into_boxed_str()),
        ..GOOGLE
    };

    let transport = HttpTokens::new().expect("http client");
    let tokens = sign_in_as(
        &transport,
        &UserBrowser,
        provider,
        "client-from-test",
        Some("secret-from-test"),
        ClientKind::Desktop,
        Duration::from_secs(10),
    )
    .await
    .expect("the whole flow completes");

    assert_eq!(tokens.access_token, "real-access");
    assert_eq!(tokens.refresh_token.as_deref(), Some("real-refresh"));

    let form = form.lock().expect("form").clone();
    for expected in [
        "grant_type=authorization_code",
        "code=the-real-code",
        "client_id=client-from-test",
        // The verifier has to survive URL-encoding on the way in and form-encoding on the way out;
        // it never appears in the authorization URL itself.
        "code_verifier=",
    ] {
        assert!(
            form.contains(expected),
            "expected {expected} in the form: {form}"
        );
    }
    assert!(
        !form.contains("secret-from-test") || form.contains("client_secret=secret-from-test"),
        "the secret may only travel as `client_secret`: {form}"
    );
}
