//! HTTP helpers shared by all providers: one client, consistent timeouts and
//! a uniform way to turn error responses into [`ProviderError`]s.

use std::time::Duration;

use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{PResult, ProviderError};

pub const USER_AGENT: &str = concat!(
    "SocialMediaConnector/",
    env!("CARGO_PKG_VERSION"),
    " (desktop app)"
);

#[derive(Clone)]
pub struct Http {
    pub client: reqwest::Client,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(90))
            .build()
            .expect("HTTP client");
        Http { client }
    }
}

/// Sends a request and maps transport failures.
pub async fn send(req: RequestBuilder) -> PResult<Response> {
    req.send().await.map_err(ProviderError::from)
}

/// Sends a request, and returns the response if the status is a success.
pub async fn send_ok(req: RequestBuilder) -> PResult<Response> {
    check(send(req).await?).await
}

/// Sends a request and parses a successful JSON response.
pub async fn send_json<T: DeserializeOwned>(req: RequestBuilder, what: &str) -> PResult<T> {
    let resp = send_ok(req).await?;
    read_json(resp, what).await
}

pub async fn read_json<T: DeserializeOwned>(resp: Response, what: &str) -> PResult<T> {
    let bytes = resp.bytes().await.map_err(ProviderError::from)?;
    serde_json::from_slice(&bytes).map_err(|e| {
        let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).to_string();
        log::warn!("could not parse {what}: {e}; body starts with: {snippet}");
        ProviderError::parse(what, e)
    })
}

/// Turns a non-success response into an error carrying the server's message.
pub async fn check(resp: Response) -> PResult<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry_after = retry_after_secs(&resp);
    let body = resp.text().await.unwrap_or_default();
    Err(status_error(status, &body, retry_after))
}

pub fn retry_after_secs(resp: &Response) -> Option<u64> {
    let headers = resp.headers();
    if let Some(v) = headers.get("retry-after").and_then(|v| v.to_str().ok())
        && let Ok(secs) = v.trim().parse::<u64>() {
            return Some(secs);
        }
    // X and others send the reset time as a unix timestamp.
    for name in ["x-rate-limit-reset", "x-ratelimit-reset", "ratelimit-reset"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok())
            && let Ok(n) = v.trim().parse::<f64>() {
                let n = n as i64;
                let now = chrono::Utc::now().timestamp();
                // Either an absolute unix time or a number of seconds.
                return Some(if n > 1_000_000_000 { (n - now).max(0) as u64 } else { n.max(0) as u64 });
            }
    }
    None
}

/// Builds the error for an unsuccessful status and response body.
pub fn status_error(status: StatusCode, body: &str, retry_after: Option<u64>) -> ProviderError {
    let detail = error_message(body);
    let with = |fallback: &str| -> String {
        match &detail {
            Some(d) if !d.is_empty() => d.clone(),
            _ => fallback.to_string(),
        }
    };
    match status.as_u16() {
        401 => ProviderError::Unauthorized(with("The network rejected the saved sign-in.")),
        403 => ProviderError::Forbidden(with("The network refused this action (403 Forbidden).")),
        404 => ProviderError::NotFound(with("Not found (404).")),
        429 => ProviderError::RateLimited {
            message: match retry_after {
                Some(s) => format!(
                    "{} Try again in {}.",
                    with("Rate limit reached."),
                    human_duration(s)
                ),
                None => with("Rate limit reached. Try again later."),
            },
            retry_after,
        },
        400 | 409 | 410 | 413 | 415 | 422 => ProviderError::Invalid(with(&format!(
            "The network rejected the request ({status})."
        ))),
        500..=599 => ProviderError::Server(with(&format!(
            "The network had a problem ({status}). Try again later."
        ))),
        _ => ProviderError::Other(with(&format!("Unexpected answer from the network ({status})."))),
    }
}

pub fn human_duration(secs: u64) -> String {
    if secs < 90 {
        format!("{secs} seconds")
    } else if secs < 90 * 60 {
        format!("{} minutes", secs.div_ceil(60))
    } else {
        format!("{} hours", secs.div_ceil(3600))
    }
}

/// Pulls a readable message out of the common error body shapes.
pub fn error_message(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if let Some(m) = message_from_json(&v) {
            return Some(truncate(&m, 400));
        }
        return Some(truncate(trimmed, 300));
    }
    // HTML or plain text: strip tags and squeeze whitespace.
    let text = crate::text::html_to_text(trimmed);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() { None } else { Some(truncate(&text, 300)) }
}

fn message_from_json(v: &Value) -> Option<String> {
    let s = |v: &Value| v.as_str().map(str::to_string).filter(|s| !s.trim().is_empty());
    // OAuth style.
    if let Some(d) = v.get("error_description").and_then(s) {
        return Some(d);
    }
    // Telegram.
    if let Some(d) = v.get("description").and_then(s)
        && v.get("ok").is_some() {
            return Some(d);
        }
    // Bluesky: {"error": "InvalidRequest", "message": "..."}
    if let (Some(e), Some(m)) = (v.get("error").and_then(s), v.get("message").and_then(s)) {
        return Some(format!("{m} ({e})"));
    }
    if let Some(m) = v.get("message").and_then(s) {
        return Some(m);
    }
    if let Some(m) = v.get("detail").and_then(s) {
        return Some(m);
    }
    if let Some(m) = v.get("title").and_then(s)
        && v.get("type").is_some() {
            return Some(m);
        }
    match v.get("error") {
        Some(Value::String(e)) => return Some(e.clone()),
        Some(obj @ Value::Object(_)) => {
            if let Some(m) = message_from_json(obj) {
                return Some(m);
            }
        }
        _ => {}
    }
    if let Some(errors) = v.get("errors") {
        match errors {
            Value::Array(list) => {
                let msgs: Vec<String> = list
                    .iter()
                    .filter_map(|e| {
                        e.as_str()
                            .map(str::to_string)
                            .or_else(|| message_from_json(e))
                    })
                    .collect();
                if !msgs.is_empty() {
                    return Some(msgs.join("; "));
                }
            }
            Value::Object(map) => {
                let msgs: Vec<String> = map
                    .iter()
                    .map(|(k, v)| match v {
                        Value::Array(a) => format!(
                            "{k}: {}",
                            a.iter()
                                .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        Value::String(t) => format!("{k}: {t}"),
                        other => format!("{k}: {other}"),
                    })
                    .collect();
                if !msgs.is_empty() {
                    return Some(msgs.join("; "));
                }
            }
            _ => {}
        }
    }
    // Reddit: {"json": {"errors": [["CODE", "message", "field"]]}}
    if let Some(list) = v.pointer("/json/errors").and_then(Value::as_array) {
        let msgs: Vec<String> = list
            .iter()
            .filter_map(|e| e.as_array())
            .map(|e| {
                e.get(1)
                    .and_then(Value::as_str)
                    .or_else(|| e.first().and_then(Value::as_str))
                    .unwrap_or("error")
                    .to_string()
            })
            .collect();
        if !msgs.is_empty() {
            return Some(msgs.join("; "));
        }
    }
    None
}

pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Normalises a server address typed by the user into a base URL without a
/// trailing slash. Plain HTTP is only accepted for this computer (tests,
/// self-hosted development servers).
pub fn normalize_server(input: &str) -> PResult<String> {
    let mut s = input.trim().trim_end_matches('/').to_string();
    if s.is_empty() {
        return Err(ProviderError::invalid("Enter the server address."));
    }
    if let Some(rest) = s.strip_prefix('@') {
        // "@user@server" or "user@server": keep the server part.
        s = rest.to_string();
    }
    if !s.contains("://") {
        if let Some((_, host)) = s.rsplit_once('@') {
            s = host.to_string();
        }
        s = format!("https://{s}");
    }
    let url = url::Url::parse(&s).map_err(|_| ProviderError::invalid(format!("\"{input}\" is not a valid server address.")))?;
    let host = url
        .host_str()
        .ok_or_else(|| ProviderError::invalid(format!("\"{input}\" is not a valid server address.")))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(host) => {}
        "http" => {
            return Err(ProviderError::invalid(
                "Only secure (https) servers can be used.",
            ));
        }
        _ => return Err(ProviderError::invalid(format!("\"{input}\" is not a valid server address."))),
    }
    let mut base = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        base.push_str(&format!(":{port}"));
    }
    let path = url.path().trim_end_matches('/');
    if !path.is_empty() && path != "/" {
        base.push_str(path);
    }
    Ok(base)
}

pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h == "localhost" || h == "127.0.0.1" || h == "::1" || h.starts_with("127.")
}

/// Host part of a base URL, for display (`https://mastodon.social` → `mastodon.social`).
pub fn host_of(base: &str) -> String {
    url::Url::parse(base)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_else(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_servers() {
        assert_eq!(normalize_server("mastodon.social").unwrap(), "https://mastodon.social");
        assert_eq!(normalize_server("https://mastodon.social/").unwrap(), "https://mastodon.social");
        assert_eq!(normalize_server("@me@fosstodon.org").unwrap(), "https://fosstodon.org");
        assert_eq!(normalize_server("me@fosstodon.org").unwrap(), "https://fosstodon.org");
        assert_eq!(normalize_server("http://127.0.0.1:8080").unwrap(), "http://127.0.0.1:8080");
        assert!(normalize_server("http://example.com").is_err());
        assert!(normalize_server("").is_err());
        assert!(normalize_server("ftp://x.org").is_err());
    }

    #[test]
    fn extracts_messages() {
        assert_eq!(error_message(r#"{"error":"Record not found"}"#).unwrap(), "Record not found");
        assert_eq!(
            error_message(r#"{"error":"ExpiredToken","message":"Token has expired"}"#).unwrap(),
            "Token has expired (ExpiredToken)"
        );
        assert_eq!(
            error_message(r#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#).unwrap(),
            "Bad Request: chat not found"
        );
        assert_eq!(
            error_message(r#"{"errors":[{"message":"Your Tweet text is too long."}]}"#).unwrap(),
            "Your Tweet text is too long."
        );
        assert_eq!(
            error_message(r#"{"json":{"errors":[["SUBREDDIT_NOEXIST","that subreddit doesn't exist","sr"]]}}"#)
                .unwrap(),
            "that subreddit doesn't exist"
        );
        assert_eq!(
            error_message(r#"{"error":{"message":"Invalid OAuth access token.","type":"OAuthException","code":190}}"#)
                .unwrap(),
            "Invalid OAuth access token."
        );
        assert_eq!(error_message("<html><body><h1>Bad gateway</h1></body></html>").unwrap(), "Bad gateway");
    }

    #[test]
    fn maps_statuses() {
        assert!(matches!(
            status_error(StatusCode::UNAUTHORIZED, "", None),
            ProviderError::Unauthorized(_)
        ));
        match status_error(StatusCode::TOO_MANY_REQUESTS, "", Some(120)) {
            ProviderError::RateLimited { retry_after, message } => {
                assert_eq!(retry_after, Some(120));
                assert!(message.contains("2 minutes"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            status_error(StatusCode::UNPROCESSABLE_ENTITY, r#"{"error":"Validation failed: Text can't be blank"}"#, None),
            ProviderError::Invalid(m) if m.contains("blank")
        ));
    }
}
