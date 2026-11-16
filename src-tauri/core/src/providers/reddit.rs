//! Reddit, through the user's own Reddit app: an "installed app" (no secret),
//! or a "web app" / "script" together with its secret.
//!
//! Sign-in: OAuth 2 authorization code flow
//! (<https://github.com/reddit-archive/reddit/wiki/OAuth2>). The browser goes to
//! `www.reddit.com/api/v1/authorize` with `duration=permanent` so Reddit hands
//! out a refresh token; the code is exchanged at `/api/v1/access_token` with
//! HTTP Basic auth made of the client ID and the secret (an empty password for
//! installed apps). Access tokens last about an hour and are renewed with
//! `grant_type=refresh_token`; a new refresh token in that answer replaces the
//! old one. Note that the token endpoint can answer `200 OK` with
//! `{"error": "invalid_grant"}`.
//!
//! API calls go to `oauth.reddit.com` with the bearer token, a descriptive
//! User-Agent (Reddit's API rules ask for `<platform>:<app ID>:<version>`)
//! and `raw_json=1`, so text and URLs come back without HTML escaping.
//!
//! Things are addressed by their "fullname" (`t1_` comment, `t2_` account,
//! `t3_` post, `t4_` message). Posts and replies return the fullname as
//! their remote id, which is what `/api/del` and `/api/vote` expect.
//!
//! Endpoints and scopes: `/api/v1/me` (identity), `/api/submit` and
//! `/api/comment` (submit; privatemessages for message replies), `/best` and
//! `/r/{sr}/new` (read), `/user/{name}/submitted` (history), `/message/inbox`
//! (privatemessages), `/api/vote` (vote), `/api/del` (edit).

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use reqwest::{RequestBuilder, StatusCode};
use serde_json::{Value, json};

use super::{Ctx, OAuthBegin, OAuthStart, expect_code, expect_state, js, ju};
use crate::error::{PResult, ProviderError};
use crate::http;
use crate::model::*;
use crate::text::{decode_entities, html_to_text};

const WWW: &str = "https://www.reddit.com";
const API: &str = "https://oauth.reddit.com";
/// Links shown to the user (never overridden in tests).
const SITE: &str = "https://www.reddit.com";
const SCOPES: &str = "identity submit read history privatemessages vote edit";
/// Unique and descriptive, as Reddit asks; it carries no user name.
const USER_AGENT: &str = concat!("desktop:social-media-connector:v", env!("CARGO_PKG_VERSION"));
const PAGE_SIZE: &str = "25";
const TITLE_MAX: usize = 300;
const COMMENT_MAX: usize = 10_000;

// ------------------------------------------------------------------ http

fn www(ctx: &Ctx) -> String {
    ctx.base("reddit.www", WWW)
}

fn api(ctx: &Ctx) -> String {
    ctx.base("reddit.api", API)
}

fn ua(req: RequestBuilder) -> RequestBuilder {
    req.header(reqwest::header::USER_AGENT, USER_AGENT)
}

fn get(ctx: &Ctx, creds: &Credentials, path: &str) -> RequestBuilder {
    ua(ctx.http.client.get(format!("{}{path}", api(ctx))))
        .bearer_auth(creds.token())
        .query(&[("raw_json", "1")])
}

fn post(ctx: &Ctx, creds: &Credentials, path: &str) -> RequestBuilder {
    ua(ctx.http.client.post(format!("{}{path}", api(ctx))))
        .bearer_auth(creds.token())
        .query(&[("raw_json", "1")])
}

/// Sends an API request and returns the JSON body. Error statuses and the
/// `{"json": {"errors": [...]}}` that Reddit sends with `200 OK` both become
/// errors.
async fn call(req: RequestBuilder, what: &str) -> PResult<Value> {
    let resp = http::send(req).await?;
    let status = resp.status();
    let retry = http::retry_after_secs(&resp);
    let missing_scope = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("insufficient_scope"))
        .unwrap_or(false);
    let bytes = resp.bytes().await.map_err(ProviderError::from)?;
    if !status.is_success() {
        return Err(error_for(status, &String::from_utf8_lossy(&bytes), retry, missing_scope));
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| ProviderError::parse(what, e))?;
    check_json_errors(&v)?;
    Ok(v)
}

fn error_for(status: StatusCode, body: &str, retry: Option<u64>, missing_scope: bool) -> ProviderError {
    if missing_scope {
        return ProviderError::Forbidden(
            "The Reddit sign-in does not include the permission this needs. Reconnect the account.".into(),
        );
    }
    // Reddit's own explanation is more useful than the generic "message".
    let explanation = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| js(&v, "/explanation"))
        .filter(|e| !e.trim().is_empty());
    let body = match explanation {
        Some(e) => json!({ "message": e }).to_string(),
        None => body.to_string(),
    };
    match http::status_error(status, &body, retry) {
        ProviderError::Unauthorized(m) if m.eq_ignore_ascii_case("unauthorized") => {
            ProviderError::Unauthorized("Reddit did not accept the saved sign-in.".into())
        }
        ProviderError::Forbidden(m) if m.eq_ignore_ascii_case("forbidden") => {
            ProviderError::Forbidden("Reddit refused this action (403 Forbidden).".into())
        }
        ProviderError::NotFound(m) if m.eq_ignore_ascii_case("not found") => {
            ProviderError::NotFound("Reddit could not find this (404).".into())
        }
        e => e,
    }
}

static WAIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+)\s*(second|minute|hour)").expect("wait regex"));

/// Turns `{"json": {"errors": [["CODE", "message", "field"]]}}` into an error.
fn check_json_errors(v: &Value) -> PResult<()> {
    let Some(errors) = v.pointer("/json/errors").and_then(Value::as_array).filter(|e| !e.is_empty()) else {
        return Ok(());
    };
    let entries: Vec<(String, String)> = errors
        .iter()
        .filter_map(Value::as_array)
        .map(|e| {
            let code = e.first().and_then(Value::as_str).unwrap_or_default().to_string();
            let msg = e.get(1).and_then(Value::as_str).filter(|m| !m.trim().is_empty()).unwrap_or(&code).to_string();
            (code, msg)
        })
        .collect();
    let message = if entries.is_empty() {
        "Reddit rejected the request.".to_string()
    } else {
        entries.iter().map(|(_, m)| m.as_str()).collect::<Vec<_>>().join("; ")
    };
    let has = |code: &str| entries.iter().any(|(c, _)| c == code);
    if has("RATELIMIT") {
        let retry_after = v
            .pointer("/json/ratelimit")
            .and_then(Value::as_f64)
            .map(|s| s.ceil().max(0.0) as u64)
            .or_else(|| {
                let c = WAIT_RE.captures(&message)?;
                let n: u64 = c[1].parse().ok()?;
                Some(match &c[2] {
                    "hour" => n * 3600,
                    "minute" => n * 60,
                    _ => n,
                })
            });
        return Err(ProviderError::RateLimited { message, retry_after });
    }
    if has("USER_REQUIRED") {
        return Err(ProviderError::Unauthorized(message));
    }
    if has("SUBMIT_VALIDATION_FLAIR_REQUIRED") {
        return Err(ProviderError::Invalid(format!(
            "{message} This subreddit requires post flair, which this app can't set; post there from reddit.com."
        )));
    }
    if ["SUBREDDIT_NOTALLOWED", "SUBREDDIT_NOTALLOWED_BANNED", "USER_BLOCKED", "THREAD_LOCKED"]
        .iter()
        .any(|c| has(c))
    {
        return Err(ProviderError::Forbidden(message));
    }
    Err(ProviderError::Invalid(message))
}

/// Requests a token. The body of a successful answer holds the tokens, so it
/// is never logged or echoed.
async fn token_request(ctx: &Ctx, client_id: &str, client_secret: &str, form: &[(&str, &str)]) -> PResult<Value> {
    let req = ua(ctx.http.client.post(format!("{}/api/v1/access_token", www(ctx))))
        .basic_auth(client_id, Some(client_secret))
        .form(form);
    let resp = http::send(req).await?;
    let status = resp.status();
    let retry = http::retry_after_secs(&resp);
    let body = resp.text().await.map_err(ProviderError::from)?;
    let parsed: Option<Value> = serde_json::from_str(&body).ok();
    if status.is_success() {
        let v = parsed.ok_or_else(|| ProviderError::parse("Reddit token", "the answer is not JSON"))?;
        if js(&v, "/access_token").filter(|t| !t.is_empty()).is_some() {
            return Ok(v);
        }
        let err = js(&v, "/error").unwrap_or_else(|| "no access token".into());
        return Err(ProviderError::Unauthorized(format!("Reddit did not issue a token ({err}).")));
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err(ProviderError::Unauthorized(
            "Reddit did not accept the app's client ID or secret. Check them in Settings → Developer apps.".into(),
        ));
    }
    Err(http::status_error(status, &body, retry))
}

fn credentials_from(token: &Value, client_id: &str, client_secret: &str, previous: Option<&Credentials>) -> PResult<Credentials> {
    let access = js(token, "/access_token")
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ProviderError::parse("Reddit token", "no access_token"))?;
    Ok(Credentials {
        access_token: Some(access),
        refresh_token: js(token, "/refresh_token")
            .filter(|t| !t.is_empty())
            .or_else(|| previous.and_then(|p| p.refresh_token.clone())),
        expires_at: ju(token, "/expires_in").map(|s| now_secs() + s as i64),
        client_id: Some(client_id.to_string()),
        client_secret: Some(client_secret.to_string()).filter(|s| !s.is_empty()),
        extra: previous.map(|p| p.extra.clone()).unwrap_or_default(),
    })
}

// --------------------------------------------------------------- sign-in

pub async fn begin(ctx: &Ctx, b: &OAuthBegin) -> PResult<OAuthStart> {
    let client_id = b.require_client_id()?;
    let auth_url = crate::oauth::with_query(
        &format!("{}/api/v1/authorize", www(ctx)),
        &[
            ("client_id", client_id),
            ("response_type", "code"),
            ("state", &b.state),
            ("redirect_uri", &b.redirect_uri),
            ("duration", "permanent"),
            ("scope", SCOPES),
        ],
    );
    Ok(OAuthStart { auth_url, flow: json!({}) })
}

pub async fn finish(ctx: &Ctx, b: &OAuthBegin, _flow: &Value, params: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    expect_state(params, &b.state)?;
    let code = expect_code(params)?;
    let client_id = b.require_client_id()?;
    let client_secret = b.client_secret.as_deref().unwrap_or_default();
    let token = match token_request(
        ctx,
        client_id,
        client_secret,
        &[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", &b.redirect_uri)],
    )
    .await
    {
        Err(ProviderError::Unauthorized(m)) if m.contains("invalid_grant") => {
            return Err(ProviderError::invalid(
                "Reddit did not accept the sign-in code (it may have expired). Please try again.",
            ));
        }
        other => other?,
    };
    let creds = credentials_from(&token, client_id, client_secret, None)?;
    let me = call(get(ctx, &creds, "/api/v1/me"), "Reddit account").await?;
    let id = js(&me, "/id").filter(|s| !s.is_empty()).ok_or_else(|| ProviderError::parse("Reddit account", "no id"))?;
    let update = profile_update(&me);
    let name = js(&me, "/name").unwrap_or_default();
    Ok(vec![NewAccount {
        platform: Platform::Reddit,
        server: String::new(),
        remote_id: format!("t2_{id}"),
        handle: update.handle.clone().unwrap_or_default(),
        display_name: update.display_name.clone().unwrap_or_else(|| name.clone()),
        avatar_url: update.avatar_url.clone(),
        profile_url: update.profile_url.clone(),
        meta: Value::Object(update.meta),
        credentials: creds,
    }])
}

pub async fn refresh(ctx: &Ctx, _account: &Account, creds: &Credentials) -> PResult<Credentials> {
    let reconnect = || ProviderError::Unauthorized("The Reddit sign-in has expired. Reconnect the account.".into());
    let refresh_token = creds.refresh_token.as_deref().filter(|t| !t.is_empty()).ok_or_else(reconnect)?;
    let client_id = creds.client_id.as_deref().filter(|c| !c.is_empty()).ok_or_else(reconnect)?;
    let client_secret = creds.client_secret.as_deref().unwrap_or_default();
    let token = match token_request(
        ctx,
        client_id,
        client_secret,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh_token)],
    )
    .await
    {
        Ok(t) => t,
        Err(ProviderError::Unauthorized(m)) if m.contains("invalid_grant") => {
            return Err(ProviderError::Unauthorized(
                "Reddit no longer accepts the saved sign-in (it was revoked or expired). Reconnect the account.".into(),
            ));
        }
        Err(ProviderError::Invalid(m)) => return Err(ProviderError::Unauthorized(m)),
        Err(e) => return Err(e),
    };
    credentials_from(&token, client_id, client_secret, Some(creds))
}

fn profile_update(me: &Value) -> ProfileUpdate {
    let name = js(me, "/name").unwrap_or_default();
    let mut meta = serde_json::Map::new();
    meta.insert("username".into(), json!(name));
    for (key, ptr) in [("karma", "/total_karma"), ("linkKarma", "/link_karma"), ("commentKarma", "/comment_karma")] {
        if let Some(n) = me.pointer(ptr).and_then(Value::as_i64) {
            meta.insert(key.into(), json!(n));
        }
    }
    let avatar = js(me, "/icon_img")
        .or_else(|| js(me, "/subreddit/icon_img"))
        .filter(|u| u.starts_with("http"))
        .map(|u| decode_entities(&u));
    ProfileUpdate {
        handle: Some(format!("u/{name}")),
        display_name: Some(
            js(me, "/subreddit/title")
                .map(|t| decode_entities(t.trim()))
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| name.clone()),
        ),
        avatar_url: avatar,
        profile_url: Some(format!("{SITE}/user/{name}")),
        stats: ProfileStats {
            // Following a Reddit user means subscribing to their profile.
            followers: ju(me, "/subreddit/subscribers"),
            following: None,
            posts: None,
        },
        meta,
    }
}

pub async fn profile(ctx: &Ctx, _account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    let me = call(get(ctx, creds, "/api/v1/me"), "Reddit account").await?;
    Ok(profile_update(&me))
}

// ---------------------------------------------------------------- posting

/// The `sr` value for a subreddit typed by the user: `r/rust`, `/r/rust/`,
/// `rust` and `https://www.reddit.com/r/rust` give `rust`; `u/me`, `/user/me`
/// and `u_me` give `u_me` (the user's profile). Not validated.
pub(crate) fn normalize_subreddit(input: &str) -> String {
    let mut s = input.trim();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest;
        }
    }
    for host in ["www.reddit.com", "old.reddit.com", "new.reddit.com", "np.reddit.com", "reddit.com"] {
        if let Some(rest) = s.strip_prefix(host) {
            s = rest;
            break;
        }
    }
    let s = s.trim_matches('/');
    let (profile, rest) = if let Some(r) = s.strip_prefix("r/") {
        (false, r)
    } else if let Some(r) = s.strip_prefix("u/").or_else(|| s.strip_prefix("user/")) {
        (true, r)
    } else {
        (false, s)
    };
    let name = rest.split('/').next().unwrap_or_default().trim();
    if profile && !name.is_empty() { format!("u_{name}") } else { name.to_string() }
}

/// Why `sr` can't be a subreddit (or profile) name, if it can't.
fn subreddit_problem(sr: &str) -> Option<String> {
    if sr.is_empty() {
        return Some("Enter the subreddit to post to (or u_yourname for your profile).".into());
    }
    let ok = if let Some(user) = sr.strip_prefix("u_") {
        (3..=20).contains(&user.len()) && user.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    } else {
        (2..=21).contains(&sr.len())
            && !sr.starts_with('_')
            && sr.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if ok { None } else { Some(format!("\"{sr}\" is not a valid subreddit name.")) }
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn is_http_url(s: &str) -> bool {
    url::Url::parse(s).map(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some()).unwrap_or(false)
}

pub fn validate(_account: &Account, post: &OutgoingPost) -> Vec<String> {
    let mut problems = Vec::new();
    if post.reply_to.is_some() {
        // Replies are comments (or message replies): no title or subreddit.
        let len = post.text.trim().chars().count();
        if len == 0 {
            problems.push("Write a reply first.".to_string());
        } else if len > COMMENT_MAX {
            problems.push(format!("Reddit replies can have up to 10,000 characters; this one has {len}."));
        }
        return problems;
    }
    match non_empty(post.options.title.as_deref()) {
        None => problems.push("Reddit posts need a title.".to_string()),
        Some(t) if t.chars().count() > TITLE_MAX => problems.push(format!(
            "Reddit titles can have up to 300 characters; this one has {}.",
            t.chars().count()
        )),
        Some(_) => {}
    }
    let sr = normalize_subreddit(post.options.subreddit.as_deref().unwrap_or_default());
    if let Some(p) = subreddit_problem(&sr) {
        problems.push(p);
    }
    if non_empty(post.options.link.as_deref()).is_some_and(|link| !is_http_url(link)) {
        problems.push("The link must be a web address starting with http:// or https://.".to_string());
    }
    problems
}

/// The fullname of the item a target refers to.
fn fullname_of(target: &Value) -> Option<String> {
    js(target, "/fullname")
        .or_else(|| js(target, "/name"))
        .filter(|f| f.len() > 3 && f.starts_with('t') && f.as_bytes()[2] == b'_')
}

fn absolute(permalink: &str) -> String {
    if permalink.starts_with("http") { permalink.to_string() } else { format!("{SITE}{permalink}") }
}

pub async fn publish(ctx: &Ctx, _account: &Account, creds: &Credentials, outgoing: &OutgoingPost) -> PResult<PublishResult> {
    if let Some(reply) = &outgoing.reply_to {
        let parent = fullname_of(reply)
            .ok_or_else(|| ProviderError::invalid("The item being replied to is missing its Reddit reference."))?;
        let form = [("api_type", "json"), ("thing_id", parent.as_str()), ("text", outgoing.text.as_str())];
        let v = call(post(ctx, creds, "/api/comment").form(&form), "Reddit reply").await?;
        let thing = v
            .pointer("/json/data/things/0/data")
            .ok_or_else(|| ProviderError::parse("Reddit reply", "no created item"))?;
        let name = js(thing, "/name")
            .or_else(|| js(thing, "/id").map(|id| format!("t1_{id}")))
            .ok_or_else(|| ProviderError::parse("Reddit reply", "no name"))?;
        let url = js(thing, "/permalink")
            .filter(|p| !p.is_empty())
            .map(|p| absolute(&p))
            .or_else(|| {
                // Message replies have no permalink.
                name.strip_prefix("t4_").map(|id| format!("{SITE}/message/messages/{id}"))
            });
        return Ok(PublishResult { remote_id: name, url });
    }

    let sr = normalize_subreddit(outgoing.options.subreddit.as_deref().unwrap_or_default());
    if let Some(p) = subreddit_problem(&sr) {
        return Err(ProviderError::Invalid(p));
    }
    let title = non_empty(outgoing.options.title.as_deref())
        .ok_or_else(|| ProviderError::invalid("Reddit posts need a title."))?;
    let link = non_empty(outgoing.options.link.as_deref());
    let text = outgoing.text.trim();
    let flag = |b: bool| if b { "true" } else { "false" };
    let mut form: Vec<(&str, &str)> = vec![
        ("api_type", "json"),
        ("sr", sr.as_str()),
        ("title", title),
        ("kind", if link.is_some() { "link" } else { "self" }),
        ("nsfw", flag(outgoing.options.sensitive)),
        ("spoiler", flag(outgoing.options.spoiler)),
        ("sendreplies", "true"),
        ("resubmit", "true"),
        ("validate_on_submit", "true"),
    ];
    if let Some(url) = link {
        form.push(("url", url));
    }
    // Link posts may carry body text too; send it only when there is some.
    if link.is_none() || !text.is_empty() {
        form.push(("text", text));
    }
    let v = call(post(ctx, creds, "/api/submit").form(&form), "Reddit post").await?;
    let data = v.pointer("/json/data").ok_or_else(|| ProviderError::parse("Reddit post", "no data"))?;
    let name = js(data, "/name")
        .or_else(|| js(data, "/id").map(|id| format!("t3_{id}")))
        .ok_or_else(|| ProviderError::parse("Reddit post", "no name"))?;
    Ok(PublishResult { url: js(data, "/url").map(|u| absolute(&u)), remote_id: name })
}

// ---------------------------------------------------------------- reading

/// The signed-in user's name, as stored at sign-in.
fn username(account: &Account) -> String {
    account
        .meta_str("username")
        .map(str::to_string)
        .unwrap_or_else(|| account.handle.trim_start_matches("u/").to_string())
}

pub async fn stream(
    ctx: &Ctx,
    account: &Account,
    creds: &Credentials,
    kind: &str,
    param: Option<&str>,
    cursor: Option<&str>,
) -> PResult<FeedPage> {
    let mut query: Vec<(&str, String)> = vec![("limit", PAGE_SIZE.into())];
    // Pagination uses the listing's `after` fullname as the cursor.
    if let Some(c) = cursor.filter(|c| !c.is_empty()) {
        query.push(("after", c.to_string()));
    }
    let path = match kind {
        "home" => "/best".to_string(),
        "notifications" => {
            // Reading through the API does not mark messages as read; say so.
            query.push(("mark", "false".into()));
            "/message/inbox".to_string()
        }
        "me" => {
            query.push(("sort", "new".into()));
            format!("/user/{}/submitted", urlencode(&username(account)))
        }
        "subreddit" => {
            let sr = normalize_subreddit(param.unwrap_or_default());
            if sr.is_empty() {
                return Err(ProviderError::invalid("Enter a subreddit."));
            }
            if let Some(p) = subreddit_problem(&sr) {
                return Err(ProviderError::Invalid(p));
            }
            format!("/r/{sr}/new")
        }
        other => return Err(ProviderError::unsupported(format!("Unknown stream {other}"))),
    };
    let v = call(get(ctx, creds, &path).query(&query), "Reddit listing").await?;
    let own = Own { fullname: account.remote_id.clone(), name: username(account) };
    let children = v.pointer("/data/children").and_then(Value::as_array).cloned().unwrap_or_default();
    let items: Vec<FeedItem> = children
        .iter()
        .filter_map(|c| {
            let data = c.get("data")?;
            match js(c, "/kind").as_deref() {
                Some("t3") if kind != "notifications" => Some(link_item(data, &own, true)),
                Some("t1") | Some("t4") if kind == "notifications" => Some(inbox_item(c, data)),
                _ => None,
            }
        })
        .collect();
    let cursor = js(&v, "/data/after").filter(|a| !a.is_empty() && !children.is_empty());
    Ok(FeedPage { items, cursor })
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

struct Own {
    fullname: String,
    name: String,
}

fn iso(secs: Option<f64>) -> String {
    secs.and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

fn author(name: Option<String>) -> Author {
    match name.filter(|n| !n.is_empty()) {
        Some(n) if n != "[deleted]" => Author {
            handle: format!("u/{n}"),
            url: Some(format!("{SITE}/user/{n}")),
            avatar: None,
            name: n,
        },
        _ => Author { name: "[deleted]".into(), handle: String::new(), avatar: None, url: None },
    }
}

/// Reddit's rendered Markdown, without the comment markers it wraps it in.
/// Without `raw_json=1` the HTML arrives entity-escaped; handle that too.
fn md_html(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let html = if raw.trim_start().starts_with("&lt;") { decode_entities(&raw) } else { raw };
    let html = html.replace("<!-- SC_OFF -->", "").replace("<!-- SC_ON -->", "");
    let html = html.trim();
    if html.is_empty() { None } else { Some(html.to_string()) }
}

fn clean_url(u: Option<String>) -> Option<String> {
    u.map(|u| decode_entities(&u)).filter(|u| u.starts_with("http"))
}

/// A preview size suitable for a stream (at most 640 px wide).
fn preview_size(list: Option<&Value>, url_key: &str, width_key: &str) -> Option<String> {
    let list = list?.as_array()?;
    list.iter()
        .rfind(|x| ju(x, width_key).map(|w| w <= 640).unwrap_or(false))
        .or_else(|| list.first())
        .and_then(|x| clean_url(js(x, url_key)))
}

fn looks_like_image(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or_default().to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".gif", ".webp"].iter().any(|e| path.ends_with(e))
}

/// Images, galleries and videos of a post, or a link card for link posts.
fn link_media(d: &Value) -> (Vec<MediaView>, Option<LinkCard>) {
    let preview_src = d.pointer("/preview/images/0/source");
    let full_preview = preview_src.and_then(|s| clean_url(js(s, "/url")));
    let small_preview = preview_size(d.pointer("/preview/images/0/resolutions"), "/url", "/width").or_else(|| full_preview.clone());

    if d.get("is_gallery").and_then(Value::as_bool).unwrap_or(false) {
        let meta = d.get("media_metadata");
        let media = d
            .pointer("/gallery_data/items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let id = js(item, "/media_id")?;
                        let m = meta?.get(&id)?;
                        if js(m, "/status").as_deref().is_some_and(|s| s != "valid") {
                            return None;
                        }
                        let s = m.get("s")?;
                        let animated = js(m, "/e").as_deref() == Some("AnimatedImage");
                        let url = clean_url(js(s, "/u"))
                            .or_else(|| clean_url(js(s, "/gif")))
                            .or_else(|| clean_url(js(s, "/mp4")))?;
                        Some(MediaView {
                            kind: if animated && url.contains(".mp4") { "gif".into() } else { "image".into() },
                            preview_url: preview_size(m.get("p"), "/u", "/x"),
                            alt: js(item, "/caption").filter(|c| !c.trim().is_empty()),
                            width: ju(s, "/x").map(|v| v as u32),
                            height: ju(s, "/y").map(|v| v as u32),
                            url,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        return (media, None);
    }

    let video = d.pointer("/secure_media/reddit_video").or_else(|| d.pointer("/media/reddit_video"));
    let is_video = d.get("is_video").and_then(Value::as_bool).unwrap_or(false);
    if let Some((rv, url)) = video.filter(|_| is_video).and_then(|rv| Some((rv, clean_url(js(rv, "/fallback_url"))?))) {
        let gif = rv.get("is_gif").and_then(Value::as_bool).unwrap_or(false);
        return (
            vec![MediaView {
                kind: if gif { "gif".into() } else { "video".into() },
                url,
                preview_url: small_preview,
                alt: None,
                width: ju(rv, "/width").map(|v| v as u32),
                height: ju(rv, "/height").map(|v| v as u32),
            }],
            None,
        );
    }

    if d.get("is_self").and_then(Value::as_bool).unwrap_or(false) {
        return (vec![], None);
    }
    let Some(url) = clean_url(js(d, "/url_overridden_by_dest").or_else(|| js(d, "/url"))) else {
        return (vec![], None);
    };
    if js(d, "/post_hint").as_deref() == Some("image") || looks_like_image(&url) {
        return (
            vec![MediaView {
                kind: "image".into(),
                preview_url: small_preview.filter(|p| *p != url),
                alt: None,
                width: preview_src.and_then(|s| ju(s, "/width")).map(|v| v as u32),
                height: preview_src.and_then(|s| ju(s, "/height")).map(|v| v as u32),
                url,
            }],
            None,
        );
    }
    let thumb = clean_url(js(d, "/thumbnail"));
    (vec![], Some(LinkCard { url, title: None, description: None, image: small_preview.or(thumb) }))
}

/// A post (`t3`) from a listing.
fn link_item(d: &Value, own: &Own, with_quote: bool) -> FeedItem {
    let name = js(d, "/name").unwrap_or_else(|| format!("t3_{}", js(d, "/id").unwrap_or_default()));
    let url = js(d, "/permalink").map(|p| absolute(&p));
    let html = md_html(js(d, "/selftext_html"));
    let text = match &html {
        Some(h) => html_to_text(h),
        None => js(d, "/selftext").unwrap_or_default(),
    };
    let nsfw = d.get("over_18").and_then(Value::as_bool).unwrap_or(false);
    let spoiler = d.get("spoiler").and_then(Value::as_bool).unwrap_or(false);
    let content_warning = match (nsfw, spoiler) {
        (true, true) => Some("NSFW, spoiler".to_string()),
        (true, false) => Some("NSFW".to_string()),
        (false, true) => Some("Spoiler".to_string()),
        _ => None,
    };
    let (media, link) = link_media(d);
    let quote = if with_quote {
        d.pointer("/crosspost_parent_list/0").filter(|p| p.is_object()).map(|p| Box::new(link_item(p, own, false)))
    } else {
        None
    };
    let likes = d.get("likes").cloned().unwrap_or(Value::Null);
    let archived = d.get("archived").and_then(Value::as_bool).unwrap_or(false);
    let locked = d.get("locked").and_then(Value::as_bool).unwrap_or(false);
    let author_name = js(d, "/author");
    let is_own = js(d, "/author_fullname").as_deref() == Some(own.fullname.as_str())
        || author_name.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(&own.name));
    FeedItem {
        id: name.clone(),
        account_id: String::new(),
        url: url.clone(),
        author: author(author_name),
        created_at: iso(d.get("created_utc").and_then(Value::as_f64)),
        text,
        html,
        title: js(d, "/title").map(|t| decode_entities(&t)),
        content_warning,
        sensitive: nsfw || spoiler,
        media,
        link,
        quote,
        reposted_by: None,
        context: js(d, "/subreddit_name_prefixed").or_else(|| js(d, "/subreddit").map(|s| format!("r/{s}"))),
        counts: Counts {
            replies: ju(d, "/num_comments"),
            reposts: ju(d, "/num_crossposts"),
            likes: None,
            quotes: None,
            score: d.get("score").and_then(Value::as_i64),
        },
        viewer: ViewerState { liked: likes == Value::Bool(true), reposted: false },
        actions: ItemActions { like: !archived, repost: false, reply: !archived && !locked, delete: is_own },
        target: json!({ "fullname": name, "kind": "t3", "url": url, "likes": likes }),
        notification: None,
    }
}

/// A comment reply, mention (`t1`) or private message (`t4`) from the inbox.
fn inbox_item(child: &Value, d: &Value) -> FeedItem {
    let is_message = js(child, "/kind").as_deref() == Some("t4");
    let name = js(d, "/name").unwrap_or_default();
    let html = md_html(js(d, "/body_html"));
    let text = match &html {
        Some(h) => html_to_text(h),
        None => js(d, "/body").unwrap_or_default(),
    };
    let created_at = iso(d.get("created_utc").and_then(Value::as_f64));
    let subreddit = js(d, "/subreddit_name_prefixed").or_else(|| js(d, "/subreddit").map(|s| format!("r/{s}")));
    let mut a = author(js(d, "/author"));
    if a.handle.is_empty() {
        // Messages from a subreddit's moderators or from Reddit itself.
        a.name = subreddit.clone().unwrap_or_else(|| "Reddit".into());
    }
    let likes = d.get("likes").cloned().unwrap_or(Value::Null);
    if is_message {
        let id = js(d, "/id").unwrap_or_default();
        return FeedItem {
            id: name.clone(),
            url: Some(format!("{SITE}/message/messages/{id}")),
            author: a.clone(),
            created_at,
            text,
            html,
            title: js(d, "/subject").filter(|s| !s.trim().is_empty()),
            context: subreddit.map(|s| format!("via {s}")),
            actions: ItemActions { like: false, repost: false, reply: !a.handle.is_empty(), delete: false },
            target: json!({ "fullname": name, "kind": "t4" }),
            notification: Some(NotificationInfo {
                kind: NotificationKind::Message,
                label: "sent you a message".into(),
                subject: None,
            }),
            ..Default::default()
        };
    }
    let (kind, label) = match js(d, "/type").as_deref() {
        Some("comment_reply") => (NotificationKind::Reply, "replied to your comment"),
        Some("post_reply") => (NotificationKind::Reply, "replied to your post"),
        Some("username_mention") => (NotificationKind::Mention, "mentioned you"),
        _ => (NotificationKind::Reply, "replied to you"),
    };
    let url = js(d, "/context").filter(|c| !c.is_empty()).map(|c| absolute(&c));
    let context = match (subreddit, js(d, "/link_title").filter(|t| !t.trim().is_empty())) {
        (Some(s), Some(t)) => Some(format!("{s} · {}", decode_entities(&t))),
        (Some(s), None) => Some(s),
        (None, t) => t,
    };
    FeedItem {
        id: name.clone(),
        url: url.clone(),
        author: a,
        created_at,
        text,
        html,
        context,
        counts: Counts { score: d.get("score").and_then(Value::as_i64), ..Default::default() },
        viewer: ViewerState { liked: likes == Value::Bool(true), reposted: false },
        actions: ItemActions { like: true, repost: false, reply: true, delete: false },
        target: json!({ "fullname": name, "kind": "t1", "url": url, "likes": likes }),
        notification: Some(NotificationInfo { kind, label: label.into(), subject: None }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- actions

pub async fn interact(ctx: &Ctx, _account: &Account, creds: &Credentials, target: &Value, action: Interaction) -> PResult<Value> {
    let dir = match action {
        Interaction::Like => "1",
        Interaction::Unlike => "0",
        Interaction::Repost | Interaction::Unrepost => {
            return Err(ProviderError::unsupported(
                "Reddit has no reposts in this app. Crosspost from reddit.com instead.",
            ));
        }
    };
    let fullname = fullname_of(target).ok_or_else(|| ProviderError::invalid("This item can't be voted on."))?;
    if fullname.starts_with("t4_") {
        return Err(ProviderError::unsupported("Messages can't be upvoted."));
    }
    call(post(ctx, creds, "/api/vote").form(&[("id", fullname.as_str()), ("dir", dir)]), "Reddit vote").await?;
    let mut target = target.clone();
    if let Value::Object(m) = &mut target {
        m.insert("likes".into(), if action == Interaction::Like { json!(true) } else { Value::Null });
    }
    Ok(target)
}

pub async fn delete(ctx: &Ctx, _account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    let id = remote_id.trim();
    if id.starts_with("t4_") {
        return Err(ProviderError::unsupported("Reddit messages can't be deleted."));
    }
    // Remote ids are fullnames; a bare id is a post.
    let fullname = if fullname_of(&json!({ "fullname": id })).is_some() { id.to_string() } else { format!("t3_{id}") };
    call(post(ctx, creds, "/api/del").form(&[("id", fullname.as_str())]), "Reddit delete").await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::Pkce;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const UA: &str = concat!("desktop:social-media-connector:v", env!("CARGO_PKG_VERSION"));

    fn account() -> Account {
        Account {
            id: "a1".into(),
            platform: Platform::Reddit,
            server: String::new(),
            remote_id: "t2_me1".into(),
            handle: "u/me_user".into(),
            display_name: "Me".into(),
            avatar_url: None,
            profile_url: None,
            meta: json!({ "username": "me_user" }),
            status: AccountStatus::Ok,
            status_message: None,
            position: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ctx(server: &MockServer) -> Ctx {
        Ctx::default().with_override("reddit.www", &server.uri()).with_override("reddit.api", &server.uri())
    }

    fn post_opts(title: &str, sr: &str) -> OutgoingPost {
        OutgoingPost {
            text: "Body text".into(),
            media: vec![],
            options: PostOptions { title: Some(title.into()), subreddit: Some(sr.into()), ..Default::default() },
            reply_to: None,
            idempotency_key: "k".into(),
        }
    }

    /// Shaped like a real `/api/v1/me` answer (trimmed).
    fn me_body() -> Value {
        json!({
            "is_employee": false, "name": "me_user", "id": "me1", "created_utc": 1_500_000_000.0,
            "icon_img": "https://styles.redditmedia.com/t5_abc/styles/profileIcon_x.png?width=256&amp;height=256&amp;s=sig",
            "total_karma": 1234, "link_karma": 1000, "comment_karma": 234, "has_verified_email": true,
            "subreddit": {"display_name": "u_me_user", "display_name_prefixed": "u/me_user", "title": "Me &amp; Co",
                          "subscribers": 42, "url": "/user/me_user/", "subreddit_type": "user", "name": "t5_abc"}
        })
    }

    #[tokio::test]
    async fn signs_in_with_installed_app_and_reads_profile() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/access_token"))
            // Basic auth with the client ID and an empty password.
            .and(header("authorization", "Basic Y2lkOg=="))
            .and(header("user-agent", UA))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=thecode"))
            .and(body_string_contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5555%2Fcallback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "acc-1", "token_type": "bearer", "expires_in": 3600,
                "refresh_token": "ref-1", "scope": SCOPES
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .and(header("authorization", "Bearer acc-1"))
            .and(header("user-agent", UA))
            .and(query_param("raw_json", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_body()))
            .expect(1)
            .mount(&server)
            .await;

        let ctx = ctx(&server);
        let b = OAuthBegin {
            platform: Platform::Reddit,
            fields: HashMap::new(),
            redirect_uri: "http://127.0.0.1:5555/callback".into(),
            state: "st8".into(),
            pkce: Pkce::new(),
            client_id: Some("cid".into()),
            client_secret: None,
        };
        let start = begin(&ctx, &b).await.unwrap();
        let url = url::Url::parse(&start.auth_url).unwrap();
        assert_eq!(url.path(), "/api/v1/authorize");
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["state"], "st8");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5555/callback");
        assert_eq!(q["duration"], "permanent");
        assert_eq!(q["scope"], "identity submit read history privatemessages vote edit");

        let denied = HashMap::from([("error".to_string(), "access_denied".to_string()), ("state".to_string(), "st8".to_string())]);
        assert!(matches!(finish(&ctx, &b, &start.flow, &denied).await, Err(ProviderError::Forbidden(_))));

        let params = HashMap::from([("code".to_string(), "thecode".to_string()), ("state".to_string(), "st8".to_string())]);
        let accounts = finish(&ctx, &b, &start.flow, &params).await.unwrap();
        let a = &accounts[0];
        assert_eq!(a.platform, Platform::Reddit);
        assert_eq!(a.server, "");
        assert_eq!(a.remote_id, "t2_me1");
        assert_eq!(a.handle, "u/me_user");
        assert_eq!(a.display_name, "Me & Co");
        assert_eq!(
            a.avatar_url.as_deref(),
            Some("https://styles.redditmedia.com/t5_abc/styles/profileIcon_x.png?width=256&height=256&s=sig")
        );
        assert_eq!(a.profile_url.as_deref(), Some("https://www.reddit.com/user/me_user"));
        assert_eq!(a.meta["username"], json!("me_user"));
        assert_eq!(a.meta["karma"], json!(1234));
        let c = &a.credentials;
        assert_eq!(c.access_token.as_deref(), Some("acc-1"));
        assert_eq!(c.refresh_token.as_deref(), Some("ref-1"));
        assert_eq!(c.client_id.as_deref(), Some("cid"));
        assert_eq!(c.client_secret, None);
        let exp = c.expires_at.unwrap();
        assert!((now_secs() + 3590..=now_secs() + 3600).contains(&exp), "{exp}");
    }

    #[tokio::test]
    async fn reads_profile_stats() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_body()))
            .mount(&server)
            .await;
        let p = profile(&ctx(&server), &account(), &Credentials::bearer("tok")).await.unwrap();
        assert_eq!(p.handle.as_deref(), Some("u/me_user"));
        assert_eq!(p.stats.followers, Some(42));
        assert_eq!(p.meta["commentKarma"], json!(234));
    }

    #[tokio::test]
    async fn refreshes_and_reports_revoked_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/access_token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=good"))
            .and(header("authorization", "Basic Y2lkOg=="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "acc-2", "token_type": "bearer", "expires_in": 3600, "scope": SCOPES
            })))
            .mount(&server)
            .await;
        // Reddit reports a bad refresh token with 200 OK.
        Mock::given(method("POST"))
            .and(path("/api/v1/access_token"))
            .and(body_string_contains("refresh_token=zz-secret-refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error": "invalid_grant"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/me"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("www-authenticate", "Bearer realm=\"reddit\", error=\"invalid_token\"")
                    .set_body_json(json!({"message": "Unauthorized", "error": 401})),
            )
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials {
            access_token: Some("old".into()),
            refresh_token: Some("good".into()),
            expires_at: Some(1),
            client_id: Some("cid".into()),
            client_secret: None,
            extra: Default::default(),
        };
        let err = profile(&ctx, &account(), &creds).await.unwrap_err();
        assert_eq!(err, ProviderError::Unauthorized("Reddit did not accept the saved sign-in.".into()));

        let new = refresh(&ctx, &account(), &creds).await.unwrap();
        assert_eq!(new.access_token.as_deref(), Some("acc-2"));
        // Reddit does not always send a new refresh token: keep the old one.
        assert_eq!(new.refresh_token.as_deref(), Some("good"));
        assert_eq!(new.client_id.as_deref(), Some("cid"));
        assert!(new.expires_at.unwrap() > now_secs());

        let revoked = Credentials { refresh_token: Some("zz-secret-refresh".into()), ..creds.clone() };
        let err = refresh(&ctx, &account(), &revoked).await.unwrap_err();
        assert!(matches!(&err, ProviderError::Unauthorized(m) if m.contains("Reconnect")), "{err:?}");
        assert!(!err.to_string().contains("zz-secret-refresh"), "no secrets in messages");

        let none = Credentials { refresh_token: None, ..creds.clone() };
        assert!(matches!(refresh(&ctx, &account(), &none).await, Err(ProviderError::Unauthorized(_))));
    }

    #[tokio::test]
    async fn refresh_keeps_a_rotated_refresh_token_and_the_secret() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/access_token"))
            .and(body_string_contains("refresh_token=first"))
            // A "web app": Basic auth with the client ID and its secret.
            .and(header("authorization", "Basic d2ViaWQ6d2Vic2VjcmV0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "acc-3", "token_type": "bearer", "expires_in": 86400,
                "refresh_token": "second", "scope": SCOPES
            })))
            .expect(1)
            .mount(&server)
            .await;
        let creds = Credentials {
            access_token: Some("old".into()),
            refresh_token: Some("first".into()),
            expires_at: Some(1),
            client_id: Some("webid".into()),
            client_secret: Some("websecret".into()),
            extra: Default::default(),
        };
        let new = refresh(&ctx(&server), &account(), &creds).await.unwrap();
        assert_eq!(new.refresh_token.as_deref(), Some("second"));
        assert_eq!(new.client_secret.as_deref(), Some("websecret"));
        let exp = new.expires_at.unwrap();
        assert!((now_secs() + 86_390..=now_secs() + 86_400).contains(&exp), "{exp}");
    }

    #[tokio::test]
    async fn maps_scope_and_explained_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/message/inbox"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("www-authenticate", "Bearer realm=\"reddit\", error=\"insufficient_scope\"")
                    .set_body_json(json!({"message": "Forbidden", "error": 403})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/vote"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "explanation": "This post is archived and can no longer be voted on.", "message": "Bad Request",
                "reason": "ARCHIVED"
            })))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials::bearer("tok");
        let err = stream(&ctx, &account(), &creds, "notifications", None, None).await.unwrap_err();
        assert!(matches!(&err, ProviderError::Forbidden(m) if m.contains("Reconnect")), "{err:?}");
        let err = interact(&ctx, &account(), &creds, &json!({"fullname": "t3_old"}), Interaction::Like).await.unwrap_err();
        assert_eq!(err, ProviderError::Invalid("This post is archived and can no longer be voted on.".into()));
    }

    #[tokio::test]
    async fn submits_self_and_link_posts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/submit"))
            .and(header("user-agent", UA))
            .and(header("authorization", "Bearer tok"))
            .and(body_string_contains("api_type=json"))
            .and(body_string_contains("sr=rust"))
            .and(body_string_contains("kind=self"))
            .and(body_string_contains("title=Hello+Reddit"))
            .and(body_string_contains("text=Body+text"))
            .and(body_string_contains("nsfw=true"))
            .and(body_string_contains("spoiler=false"))
            .and(body_string_contains("sendreplies=true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"json": {"errors": [], "data": {
                "url": "https://www.reddit.com/r/rust/comments/abc123/hello_reddit/", "drafts_count": 0,
                "id": "abc123", "name": "t3_abc123"
            }}})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/submit"))
            .and(body_string_contains("sr=u_me_user"))
            .and(body_string_contains("kind=link"))
            .and(body_string_contains("url=https%3A%2F%2Fexample.com%2Fpost"))
            .and(body_string_contains("spoiler=true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"json": {"errors": [], "data": {
                "url": "https://www.reddit.com/user/me_user/comments/def456/a_link/", "id": "def456", "name": "t3_def456"
            }}})))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials::bearer("tok");
        let mut p = post_opts("Hello Reddit", "/r/rust/");
        p.options.sensitive = true;
        let res = publish(&ctx, &account(), &creds, &p).await.unwrap();
        assert_eq!(res.remote_id, "t3_abc123");
        assert_eq!(res.url.as_deref(), Some("https://www.reddit.com/r/rust/comments/abc123/hello_reddit/"));

        let mut link = post_opts("A link", "u/me_user");
        link.text = String::new();
        link.options.link = Some("https://example.com/post".into());
        link.options.spoiler = true;
        let res = publish(&ctx, &account(), &creds, &link).await.unwrap();
        assert_eq!(res.remote_id, "t3_def456");
    }

    #[tokio::test]
    async fn turns_json_errors_into_errors() {
        let server = MockServer::start().await;
        let respond = |sr: &'static str, body: Value| {
            Mock::given(method("POST"))
                .and(path("/api/submit"))
                .and(body_string_contains(format!("sr={sr}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
        };
        respond("nosuchsub", json!({"json": {"errors": [["SUBREDDIT_NOEXIST", "that subreddit doesn't exist", "sr"]]}}))
            .mount(&server)
            .await;
        respond("busy", json!({"json": {"ratelimit": 539.6, "errors": [["RATELIMIT",
            "Looks like you've been doing that a lot. Take a break for 9 minutes before trying again.", "ratelimit"]]}}))
            .mount(&server)
            .await;
        respond("flairy", json!({"json": {"errors": [["SUBMIT_VALIDATION_FLAIR_REQUIRED", "Your post must contain post flair.", "flair"]]}}))
            .mount(&server)
            .await;
        respond("closed", json!({"json": {"errors": [["SUBREDDIT_NOTALLOWED", "you aren't allowed to post there.", "sr"]]}}))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials::bearer("tok");
        let err = publish(&ctx, &account(), &creds, &post_opts("t", "nosuchsub")).await.unwrap_err();
        assert_eq!(err, ProviderError::Invalid("that subreddit doesn't exist".into()));
        match publish(&ctx, &account(), &creds, &post_opts("t", "busy")).await.unwrap_err() {
            ProviderError::RateLimited { retry_after, message } => {
                assert_eq!(retry_after, Some(540));
                assert!(message.contains("9 minutes"));
            }
            other => panic!("{other:?}"),
        }
        let err = publish(&ctx, &account(), &creds, &post_opts("t", "flairy")).await.unwrap_err();
        assert!(matches!(&err, ProviderError::Invalid(m) if m.contains("flair")), "{err:?}");
        let err = publish(&ctx, &account(), &creds, &post_opts("t", "closed")).await.unwrap_err();
        assert!(matches!(err, ProviderError::Forbidden(_)));
        // Rejected before any request.
        let err = publish(&ctx, &account(), &creds, &post_opts("t", "not a sub!")).await.unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)));
    }

    #[tokio::test]
    async fn replies_with_a_comment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/comment"))
            .and(body_string_contains("thing_id=t1_parent"))
            .and(body_string_contains("text=Thanks%21"))
            .and(body_string_contains("api_type=json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"json": {"errors": [], "data": {"things": [
                {"kind": "t1", "data": {"id": "new1", "name": "t1_new1", "parent_id": "t1_parent", "link_id": "t3_abc123",
                                        "body": "Thanks!", "permalink": "/r/rust/comments/abc123/hello_reddit/new1/"}}
            ]}}})))
            .expect(1)
            .mount(&server)
            .await;
        let p = OutgoingPost {
            text: "Thanks!".into(),
            media: vec![],
            options: PostOptions::default(),
            reply_to: Some(json!({"fullname": "t1_parent", "kind": "t1"})),
            idempotency_key: "k".into(),
        };
        assert!(validate(&account(), &p).is_empty(), "replies need no title or subreddit");
        let res = publish(&ctx(&server), &account(), &Credentials::bearer("tok"), &p).await.unwrap();
        assert_eq!(res.remote_id, "t1_new1");
        assert_eq!(res.url.as_deref(), Some("https://www.reddit.com/r/rust/comments/abc123/hello_reddit/new1/"));
    }

    fn listing(children: Vec<Value>, after: Option<&str>) -> Value {
        json!({"kind": "Listing", "data": {"after": after, "dist": children.len(), "modhash": null, "geo_filter": "",
                                           "children": children, "before": null}})
    }

    fn t3(id: &str, extra: Value) -> Value {
        let mut data = json!({
            "subreddit": "rust", "subreddit_name_prefixed": "r/rust", "selftext": "", "selftext_html": null,
            "author": "ferris", "author_fullname": "t2_ferris", "title": "Rust &amp; you", "name": format!("t3_{id}"),
            "id": id, "score": 512, "num_comments": 87, "num_crossposts": 2, "likes": null, "over_18": false, "spoiler": false,
            "archived": false, "locked": false, "is_self": true, "is_video": false, "created_utc": 1_762_000_000.0,
            "permalink": format!("/r/rust/comments/{id}/rust_you/"), "url": format!("https://www.reddit.com/r/rust/comments/{id}/rust_you/"),
            "thumbnail": "self", "domain": "self.rust", "stickied": false
        });
        if let (Value::Object(d), Value::Object(e)) = (&mut data, extra) {
            d.extend(e);
        }
        json!({"kind": "t3", "data": data})
    }

    #[tokio::test]
    async fn maps_listings_and_paginates() {
        let server = MockServer::start().await;
        let self_post = t3("s1", json!({
            "selftext": "Hello **world**",
            "selftext_html": "<!-- SC_OFF --><div class=\"md\"><p>Hello <strong>world</strong></p>\n</div><!-- SC_ON -->",
            "likes": true, "author": "me_user", "author_fullname": "t2_me1"
        }));
        let link_post = t3("l1", json!({
            "is_self": false, "domain": "blog.rust-lang.org", "post_hint": "link", "over_18": true,
            "url": "https://blog.rust-lang.org/2026/11/01/Rust-1.99.html",
            "url_overridden_by_dest": "https://blog.rust-lang.org/2026/11/01/Rust-1.99.html",
            "thumbnail": "https://b.thumbs.redditmedia.com/t.jpg",
            "preview": {"images": [{"source": {"url": "https://external-preview.redd.it/p.png?auto=webp&s=1", "width": 1200, "height": 630},
                                    "resolutions": [{"url": "https://external-preview.redd.it/p.png?width=108&s=2", "width": 108, "height": 56},
                                                    {"url": "https://external-preview.redd.it/p.png?width=640&s=3", "width": 640, "height": 336}]}],
                        "enabled": false},
            "archived": true
        }));
        let gallery = t3("g1", json!({
            "is_self": false, "is_gallery": true, "url": "https://www.reddit.com/gallery/g1",
            "gallery_data": {"items": [{"media_id": "m1", "id": 1, "caption": "First"}, {"media_id": "m2", "id": 2}]},
            "media_metadata": {
                "m1": {"status": "valid", "e": "Image", "m": "image/jpg",
                       "p": [{"y": 108, "x": 108, "u": "https://preview.redd.it/m1.jpg?width=108&s=a"}],
                       "s": {"y": 1024, "x": 1024, "u": "https://preview.redd.it/m1.jpg?width=1024&s=b"}, "id": "m1"},
                "m2": {"status": "valid", "e": "Image", "m": "image/png",
                       "s": {"y": 600, "x": 800, "u": "https://preview.redd.it/m2.png?width=800&s=c"}, "id": "m2"}
            }
        }));
        let crosspost = t3("x1", json!({
            "is_self": false, "crosspost_parent": "t3_s1",
            "crosspost_parent_list": [self_post["data"].clone()]
        }));
        Mock::given(method("GET"))
            .and(path("/best"))
            .and(query_param("raw_json", "1"))
            .and(query_param("limit", "25"))
            .and(query_param("after", "t3_prev"))
            .and(header("user-agent", UA))
            .respond_with(ResponseTemplate::new(200).set_body_json(listing(
                vec![self_post, link_post, gallery, crosspost],
                Some("t3_x1"),
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user/me_user/submitted"))
            .and(query_param("sort", "new"))
            .respond_with(ResponseTemplate::new(200).set_body_json(listing(vec![], None)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/r/rust/new"))
            .respond_with(ResponseTemplate::new(200).set_body_json(listing(vec![t3("n1", json!({}))], None)))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials::bearer("tok");
        let page = stream(&ctx, &account(), &creds, "home", None, Some("t3_prev")).await.unwrap();
        assert_eq!(page.cursor.as_deref(), Some("t3_x1"));
        assert_eq!(page.items.len(), 4);

        let s = &page.items[0];
        assert_eq!(s.id, "t3_s1");
        assert_eq!(s.title.as_deref(), Some("Rust & you"));
        assert_eq!(s.text, "Hello world");
        assert_eq!(s.html.as_deref(), Some("<div class=\"md\"><p>Hello <strong>world</strong></p>\n</div>"));
        assert_eq!(s.url.as_deref(), Some("https://www.reddit.com/r/rust/comments/s1/rust_you/"));
        assert_eq!(s.context.as_deref(), Some("r/rust"));
        assert_eq!(s.counts.score, Some(512));
        assert_eq!(s.counts.replies, Some(87));
        assert_eq!(s.created_at, "2025-11-01T12:26:40.000Z");
        assert!(s.viewer.liked);
        assert!(s.actions.delete && s.actions.like && s.actions.reply && !s.actions.repost);
        assert_eq!(s.author.handle, "u/me_user");
        assert_eq!(s.target, json!({"fullname": "t3_s1", "kind": "t3", "url": s.url, "likes": true}));

        let l = &page.items[1];
        let card = l.link.as_ref().unwrap();
        assert_eq!(card.url, "https://blog.rust-lang.org/2026/11/01/Rust-1.99.html");
        assert_eq!(card.image.as_deref(), Some("https://external-preview.redd.it/p.png?width=640&s=3"));
        assert!(l.sensitive);
        assert_eq!(l.content_warning.as_deref(), Some("NSFW"));
        assert!(!l.actions.like && !l.actions.reply, "archived posts can't be voted on or replied to");
        assert!(!l.actions.delete);

        let g = &page.items[2];
        assert_eq!(g.media.len(), 2);
        assert_eq!(g.media[0].url, "https://preview.redd.it/m1.jpg?width=1024&s=b");
        assert_eq!(g.media[0].preview_url.as_deref(), Some("https://preview.redd.it/m1.jpg?width=108&s=a"));
        assert_eq!(g.media[0].alt.as_deref(), Some("First"));
        assert_eq!(g.media[1].width, Some(800));

        let x = &page.items[3];
        assert_eq!(x.quote.as_ref().unwrap().id, "t3_s1");

        let mine = stream(&ctx, &account(), &creds, "me", None, None).await.unwrap();
        assert!(mine.items.is_empty());
        assert_eq!(mine.cursor, None);
        let sub = stream(&ctx, &account(), &creds, "subreddit", Some("https://www.reddit.com/r/rust/"), None).await.unwrap();
        assert_eq!(sub.items[0].id, "t3_n1");
        assert!(matches!(stream(&ctx, &account(), &creds, "subreddit", Some(""), None).await, Err(ProviderError::Invalid(_))));
    }

    #[tokio::test]
    async fn maps_inbox_replies_mentions_and_messages() {
        let server = MockServer::start().await;
        let comment = |id: &str, typ: &str, subject: &str| {
            json!({"kind": "t1", "data": {
                "first_message": null, "first_message_name": null, "subreddit": "rust", "likes": null, "replies": "",
                "author_fullname": "t2_x", "id": id, "subject": subject, "score": 3, "author": "crab",
                "num_comments": 12, "parent_id": "t3_abc123", "subreddit_name_prefixed": "r/rust", "new": true,
                "type": typ, "body": "Nice post", "dest": "me_user", "was_comment": true,
                "body_html": "<!-- SC_OFF --><div class=\"md\"><p>Nice post</p>\n</div><!-- SC_ON -->",
                "name": format!("t1_{id}"), "created_utc": 1_762_000_100.0, "link_title": "Hello Reddit",
                "context": format!("/r/rust/comments/abc123/hello_reddit/{id}/?context=3"), "distinguished": null
            }})
        };
        let message = json!({"kind": "t4", "data": {
            "first_message": null, "first_message_name": null, "subreddit": null, "likes": null, "replies": "",
            "author_fullname": "t2_y", "id": "msg1", "subject": "Question", "score": 0, "author": "pal",
            "num_comments": null, "parent_id": null, "subreddit_name_prefixed": null, "new": true, "type": "unknown",
            "body": "Hi there", "dest": "me_user", "was_comment": false, "body_html": "<!-- SC_OFF --><div class=\"md\"><p>Hi there</p>\n</div><!-- SC_ON -->",
            "name": "t4_msg1", "created_utc": 1_762_000_200.0, "context": "", "distinguished": null
        }});
        Mock::given(method("GET"))
            .and(path("/message/inbox"))
            .and(query_param("mark", "false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(listing(
                vec![comment("c1", "post_reply", "post reply"), comment("c2", "username_mention", "username mention"), message],
                Some("t4_msg1"),
            )))
            .mount(&server)
            .await;
        let page = stream(&ctx(&server), &account(), &Credentials::bearer("tok"), "notifications", None, None).await.unwrap();
        assert_eq!(page.cursor.as_deref(), Some("t4_msg1"));
        let reply = &page.items[0];
        let n = reply.notification.as_ref().unwrap();
        assert_eq!((n.kind, n.label.as_str()), (NotificationKind::Reply, "replied to your post"));
        assert_eq!(reply.id, "t1_c1");
        assert_eq!(reply.text, "Nice post");
        assert_eq!(reply.author.handle, "u/crab");
        assert_eq!(reply.url.as_deref(), Some("https://www.reddit.com/r/rust/comments/abc123/hello_reddit/c1/?context=3"));
        assert_eq!(reply.context.as_deref(), Some("r/rust · Hello Reddit"));
        assert!(reply.actions.reply && reply.actions.like);
        assert_eq!(reply.target["fullname"], json!("t1_c1"));
        assert_eq!(page.items[1].notification.as_ref().unwrap().kind, NotificationKind::Mention);
        let msg = &page.items[2];
        assert_eq!(msg.notification.as_ref().unwrap().kind, NotificationKind::Message);
        assert_eq!(msg.title.as_deref(), Some("Question"));
        assert_eq!(msg.text, "Hi there");
        assert!(msg.actions.reply && !msg.actions.like);
        assert_eq!(msg.target, json!({"fullname": "t4_msg1", "kind": "t4"}));
    }

    #[tokio::test]
    async fn votes_and_deletes_by_fullname() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/vote"))
            .and(body_string_contains("id=t3_abc"))
            .and(body_string_contains("dir=1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/vote"))
            .and(body_string_contains("id=t3_abc"))
            .and(body_string_contains("dir=0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/del"))
            .and(body_string_contains("id=t3_abc"))
            .and(header("user-agent", UA))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(2)
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials::bearer("tok");
        let target = json!({"fullname": "t3_abc", "kind": "t3", "likes": null});
        let liked = interact(&ctx, &account(), &creds, &target, Interaction::Like).await.unwrap();
        assert_eq!(liked["likes"], json!(true));
        let cleared = interact(&ctx, &account(), &creds, &liked, Interaction::Unlike).await.unwrap();
        assert_eq!(cleared["likes"], Value::Null);
        assert!(matches!(
            interact(&ctx, &account(), &creds, &target, Interaction::Repost).await,
            Err(ProviderError::Unsupported(_))
        ));
        delete(&ctx, &account(), &creds, "t3_abc").await.unwrap();
        delete(&ctx, &account(), &creds, "abc").await.unwrap();
        assert!(matches!(delete(&ctx, &account(), &creds, "t4_m").await, Err(ProviderError::Unsupported(_))));
    }

    #[test]
    fn validates_reddit_rules() {
        let acc = account();
        assert!(validate(&acc, &post_opts("Fine", "r/rust")).is_empty());
        assert!(validate(&acc, &post_opts("Fine", "u_me_user")).is_empty());
        let mut p = post_opts("", "rust");
        p.options.title = None;
        assert_eq!(validate(&acc, &p), vec!["Reddit posts need a title."]);
        let long = post_opts(&"x".repeat(301), "rust");
        assert!(validate(&acc, &long)[0].contains("300"));
        assert!(validate(&acc, &post_opts(&"é".repeat(300), "rust")).is_empty(), "counts characters, not bytes");
        assert!(validate(&acc, &post_opts("t", "")).iter().any(|m| m.contains("Enter the subreddit")));
        assert!(validate(&acc, &post_opts("t", "bad name")).iter().any(|m| m.contains("not a valid subreddit")));
        let mut link = post_opts("t", "rust");
        link.options.link = Some("ftp://example.com/file".into());
        assert!(validate(&acc, &link).iter().any(|m| m.contains("http")));
        let reply = OutgoingPost {
            text: "y".repeat(10_001),
            media: vec![],
            options: PostOptions::default(),
            reply_to: Some(json!({"fullname": "t3_a"})),
            idempotency_key: "k".into(),
        };
        assert!(validate(&acc, &reply)[0].contains("10,000"));
    }

    #[test]
    fn shared_validation_rejects_images() {
        let mut p = post_opts("t", "rust");
        p.media.push(MediaAttachment {
            id: "m".into(),
            path: "/nowhere/a.png".into(),
            file_name: "a.png".into(),
            mime: "image/png".into(),
            size: 10,
            width: 1,
            height: 1,
            alt: String::new(),
        });
        let problems = crate::providers::validate(&account(), &p);
        assert_eq!(problems, vec!["Reddit posts from this app can't include images."]);
    }

    #[test]
    fn normalizes_subreddit_names() {
        for (input, out) in [
            ("rust", "rust"),
            ("r/rust", "rust"),
            ("/r/rust/", "rust"),
            ("https://www.reddit.com/r/rust/comments/x", "rust"),
            ("old.reddit.com/r/rust", "rust"),
            ("u/me_user", "u_me_user"),
            ("/user/me_user/", "u_me_user"),
            ("u_me_user", "u_me_user"),
            ("  ", ""),
        ] {
            assert_eq!(normalize_subreddit(input), out, "{input}");
        }
        assert!(subreddit_problem("de").is_none());
        assert!(subreddit_problem("_x").is_some());
        assert!(subreddit_problem(&"a".repeat(22)).is_some());
    }
}
