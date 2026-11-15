//! OAuth helpers for a desktop app: PKCE, random state, and a small HTTP
//! server on the loopback interface that receives the browser redirect.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::text::escape_html;

pub const CALLBACK_PATH: &str = "/callback";

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("system random number generator");
    buf
}

/// URL-safe random string with `n_bytes` of entropy.
pub fn random_token(n_bytes: usize) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(n_bytes))
}

#[derive(Clone, Debug)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = random_token(32);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Pkce { verifier, challenge }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

/// The redirect URI the browser comes back to.
pub fn redirect_uri(host: &str, port: u16) -> String {
    format!("http://{host}:{port}{CALLBACK_PATH}")
}

/// What the browser sent to the callback, plus a way to answer it once the
/// app has finished connecting the account.
pub struct Callback {
    pub params: HashMap<String, String>,
    responder: Option<oneshot::Sender<CallbackPage>>,
}

impl Callback {
    pub fn respond(&mut self, page: CallbackPage) {
        if let Some(tx) = self.responder.take() {
            let _ = tx.send(page);
        }
    }
}

impl Drop for Callback {
    fn drop(&mut self) {
        if let Some(tx) = self.responder.take() {
            let _ = tx.send(CallbackPage::Done);
        }
    }
}

#[derive(Debug, Clone)]
pub enum CallbackPage {
    Success(String),
    Failure(String),
    Done,
}

/// Listens on 127.0.0.1 (and ::1 when available) for the OAuth redirect.
pub struct LoopbackServer {
    pub port: u16,
    rx: mpsc::Receiver<Callback>,
    tasks: Vec<JoinHandle<()>>,
}

impl LoopbackServer {
    pub async fn bind(port: u16) -> std::io::Result<Self> {
        let v4 = TcpListener::bind(("127.0.0.1", port)).await?;
        let port = v4.local_addr()?.port();
        let v6 = TcpListener::bind(("::1", port)).await.ok();
        let (tx, rx) = mpsc::channel(4);
        let mut tasks = vec![tokio::spawn(accept_loop(v4, tx.clone()))];
        if let Some(v6) = v6 {
            tasks.push(tokio::spawn(accept_loop(v6, tx)));
        }
        Ok(LoopbackServer { port, rx, tasks })
    }

    /// Waits for the browser to arrive at the callback URL.
    pub async fn next(&mut self, timeout: Duration) -> Option<Callback> {
        tokio::time::timeout(timeout, self.rx.recv()).await.ok().flatten()
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn accept_loop(listener: TcpListener, tx: mpsc::Sender<Callback>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = handle_connection(stream, tx).await;
        });
    }
}

async fn handle_connection(mut stream: TcpStream, tx: mpsc::Sender<Callback>) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16 * 1024 {
                break;
            }
        }
        Ok::<_, std::io::Error>(())
    })
    .await;
    if read.is_err() {
        return Ok(());
    }
    let head = String::from_utf8_lossy(&buf);
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    if method != "GET" || path != CALLBACK_PATH {
        return write_page(&mut stream, 404, "Not found", "Nothing to see here.", false).await;
    }
    let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let (resp_tx, resp_rx) = oneshot::channel();
    let callback = Callback { params, responder: Some(resp_tx) };
    if tx.send(callback).await.is_err() {
        return write_page(
            &mut stream,
            410,
            "Sign-in link expired",
            "This sign-in has already finished or was cancelled. Start again from the app.",
            false,
        )
        .await;
    }
    let page = tokio::time::timeout(Duration::from_secs(90), resp_rx).await;
    match page {
        Ok(Ok(CallbackPage::Success(msg))) => write_page(&mut stream, 200, "Account connected", &msg, true).await,
        Ok(Ok(CallbackPage::Failure(msg))) => write_page(&mut stream, 200, "Could not connect", &msg, false).await,
        _ => {
            write_page(
                &mut stream,
                200,
                "Signed in",
                "You can close this tab and return to Social Media Connector.",
                true,
            )
            .await
        }
    }
}

async fn write_page(stream: &mut TcpStream, status: u16, title: &str, message: &str, ok: bool) -> std::io::Result<()> {
    let color = if ok { "#1a7f4b" } else { "#b42318" };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title} – Social Media Connector</title>\
<meta name=\"viewport\" content=\"width=device-width\">\
<style>body{{font-family:system-ui,sans-serif;background:#f4f5f7;color:#1d2433;display:grid;place-items:center;min-height:100vh;margin:0}}\
main{{background:#fff;border-radius:14px;padding:32px 40px;max-width:460px;box-shadow:0 4px 24px rgba(0,0,0,.08)}}\
h1{{font-size:20px;margin:0 0 12px;color:{color}}}p{{line-height:1.5;margin:0 0 8px}}small{{color:#667085}}</style></head>\
<body><main><h1>{title}</h1><p>{message}</p><small>You can close this tab and return to Social Media Connector.</small></main></body></html>",
        title = escape_html(title),
        message = escape_html(message),
    );
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        410 => "Gone",
        _ => "OK",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

/// Appends query parameters to a URL.
pub fn with_query(base: &str, params: &[(&str, &str)]) -> String {
    let mut url = url::Url::parse(base).expect("valid base URL");
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    url.to_string()
}

/// Reads the OAuth error from callback parameters, if the user declined or
/// the network reported a problem.
pub fn callback_error(params: &HashMap<String, String>) -> Option<String> {
    let err = params.get("error")?;
    let desc = params
        .get("error_description")
        .or_else(|| params.get("error_message"))
        .or_else(|| params.get("error_reason"))
        .cloned()
        .unwrap_or_default();
    Some(if err == "access_denied" {
        "Access was declined in the browser.".to_string()
    } else if desc.is_empty() {
        format!("The network reported an error: {err}")
    } else {
        format!("The network reported an error: {desc}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_shape() {
        let p = Pkce::new();
        assert_eq!(p.verifier.len(), 43);
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expected);
        // RFC 7636 appendix B example.
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[tokio::test]
    async fn loopback_receives_callback_and_answers() {
        let mut server = LoopbackServer::bind(0).await.unwrap();
        let port = server.port;
        let client = tokio::spawn(async move {
            reqwest::get(format!("http://127.0.0.1:{port}/callback?code=abc&state=xyz"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        let mut cb = server.next(Duration::from_secs(5)).await.expect("callback");
        assert_eq!(cb.params.get("code").map(String::as_str), Some("abc"));
        assert_eq!(cb.params.get("state").map(String::as_str), Some("xyz"));
        cb.respond(CallbackPage::Success("Connected @me".into()));
        let page = client.await.unwrap();
        assert!(page.contains("Connected @me"), "{page}");

        // Other paths get a 404 and do not reach the app.
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/favicon.ico")).await.unwrap();
        assert_eq!(resp.status().as_u16(), 404);
    }
}
