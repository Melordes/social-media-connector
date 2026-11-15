//! Facebook Pages, through the user's own Meta developer app and the Graph API.
//!
//! Sign-in is the manual Facebook Login (for Business) flow with the loopback
//! redirect `http://localhost:PORT/callback`, which Meta accepts while the
//! app is in Development mode. The code is exchanged with the app secret (and
//! the PKCE verifier), the short-lived user token is swapped for a long-lived
//! one, and `/me/accounts` lists the Pages the user chose. Each Page becomes
//! an account holding its own Page access token. Page tokens obtained from a
//! long-lived user token have no expiry date; Meta ends them when the user
//! changes their password, removes the app or loses their role on the Page,
//! and the user then reconnects (they cannot be renewed silently).
//!
//! Tokens travel in the `Authorization` header or in form bodies, never in
//! URLs. Every Graph call carries an `appsecret_proof`, so the app keeps
//! working when "Require App Secret" is switched on in the Meta app.
//!
//! Docs checked against Graph API v27.0 (the current version in November 2026):
//! - developers.facebook.com/docs/facebook-login/guides/advanced/manual-flow
//! - developers.facebook.com/docs/facebook-login/guides/advanced/oidc-token (PKCE)
//! - developers.facebook.com/docs/facebook-login/guides/access-tokens/get-long-lived
//! - developers.facebook.com/docs/pages-api/posts, .../graph-api/reference/page/photos,
//!   .../graph-api/reference/page (edge `posts`), .../graph-api/reference/object/comments
//! - developers.facebook.com/docs/graph-api/guides/error-handling

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use reqwest::multipart::{Form, Part};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Ctx, OAuthBegin, OAuthStart, expect_code, expect_state, js, ju};
use crate::error::{PResult, ProviderError};
use crate::http::{self, send};
use crate::media::PreparedImage;
use crate::model::*;
use crate::text::{self, escape_html};

/// Graph API version for every call (v27.0 was released on 4 November 2026).
pub const GRAPH_VERSION: &str = "v27.0";
const GRAPH: &str = "https://graph.facebook.com";
const WWW: &str = "https://www.facebook.com";

/// Permissions asked for at sign-in. All of them are available with
/// Standard Access, so the app's admins can grant them without App Review.
const SCOPES: &[&str] = &[
    // List the Pages the person manages (`/me/accounts`).
    "pages_show_list",
    // Read the Page's own posts, followers and picture.
    "pages_read_engagement",
    // Comments on the Page's posts; required for `/posts` reads and by
    // pages_manage_engagement.
    "pages_read_user_content",
    // Publish and delete posts as the Page.
    "pages_manage_posts",
    // Replies: comment as the Page.
    "pages_manage_engagement",
    // Pages that belong to a business portfolio; already part of the
    // "Manage everything on your Page" use case.
    "business_management",
];

/// Without these a Page can't be used at all.
const REQUIRED: &[&str] = &["pages_show_list", "pages_read_engagement", "pages_manage_posts"];

const ACCOUNT_FIELDS: &str = "id,name,username,link,picture.type(large),access_token,tasks";
const PAGE_FIELDS: &str = "id,name,username,link,picture.type(large),followers_count,fan_count";
const POST_FIELDS: &str = "id,message,story,created_time,permalink_url,full_picture,status_type,\
attachments{type,media_type,url,unshimmed_url,title,description,media,target,subattachments},\
from{id,name,picture},shares,comments.summary(true).limit(0),reactions.summary(true).limit(0)";
const PAGE_SIZE: &str = "25";

static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"EAA[A-Za-z0-9]{20,}").expect("token regex"));
static PERMISSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(pages_[a-z_]+|business_management|publish_video|read_insights)\b").expect("permission regex")
});
static CODE_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\(#\d+\)\s*").expect("prefix regex"));

// ------------------------------------------------------------------ http

fn graph_url(ctx: &Ctx, path: &str) -> String {
    format!("{}/{GRAPH_VERSION}/{}", ctx.base("facebook.graph", GRAPH), path.trim_start_matches('/'))
}

/// HMAC-SHA256 of `msg` with `key`, as lowercase hex (RFC 2104).
fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `appsecret_proof`: the access token signed with the app secret.
fn appsecret_proof(token: &str, secret: &str) -> String {
    hmac_sha256_hex(secret.as_bytes(), token.as_bytes())
}

/// Graph calls with one access token.
struct Api<'a> {
    ctx: &'a Ctx,
    token: &'a str,
    secret: Option<&'a str>,
}

impl<'a> Api<'a> {
    fn for_page(ctx: &'a Ctx, creds: &'a Credentials) -> PResult<Self> {
        let token = creds.access_token.as_deref().filter(|t| !t.is_empty()).ok_or_else(|| {
            ProviderError::Unauthorized("The Facebook Page token is missing. Reconnect the Page.".into())
        })?;
        Ok(Api { ctx, token, secret: creds.client_secret.as_deref().filter(|s| !s.is_empty()) })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let req = self.ctx.http.client.request(method, graph_url(self.ctx, path)).bearer_auth(self.token);
        match self.secret {
            Some(secret) => req.query(&[("appsecret_proof", appsecret_proof(self.token, secret))]),
            None => req,
        }
    }

    fn get(&self, path: &str) -> RequestBuilder {
        self.request(Method::GET, path)
    }

    fn post(&self, path: &str) -> RequestBuilder {
        self.request(Method::POST, path)
    }

    fn delete(&self, path: &str) -> RequestBuilder {
        self.request(Method::DELETE, path)
    }

    async fn call(&self, req: RequestBuilder, what: &str) -> PResult<Value> {
        call(req, what, &[self.token, self.secret.unwrap_or_default()]).await
    }
}

/// Sends a Graph request and returns its JSON body. Graph errors become the
/// matching `ProviderError` (see [`graph_error`]); `secrets` are removed
/// from any message.
async fn call(req: RequestBuilder, what: &str, secrets: &[&str]) -> PResult<Value> {
    let resp = send(req).await?;
    let status = resp.status();
    let retry_after = http::retry_after_secs(&resp);
    let bytes = resp.bytes().await.map_err(ProviderError::from)?;
    let body = String::from_utf8_lossy(&bytes);
    if !status.is_success() {
        return Err(graph_error(status, &body, retry_after, secrets));
    }
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| ProviderError::parse(what, e))?;
    if v.get("error").is_some_and(Value::is_object) {
        return Err(graph_error(status, &body, retry_after, secrets));
    }
    Ok(v)
}

/// Removes tokens and secrets from a message (Graph echoes malformed tokens).
fn scrub(s: &str, secrets: &[&str]) -> String {
    let mut out = s.to_string();
    for secret in secrets.iter().filter(|x| x.len() >= 8) {
        out = out.replace(secret, "[hidden]");
    }
    TOKEN_RE.replace_all(&out, "[hidden]").into_owned()
}

/// `(#506) Duplicate status message` → `Duplicate status message.`
fn sentence(s: &str) -> String {
    let s = CODE_PREFIX_RE.replace(s.trim(), "").trim().to_string();
    if s.is_empty() || s.ends_with(['.', '!', '?']) { s } else { format!("{s}.") }
}

fn or(msg: String, fallback: &str) -> String {
    if msg.is_empty() { fallback.to_string() } else { msg }
}

/// Maps a Graph API error body (`{"error": {"message", "type", "code",
/// "error_subcode", "error_user_msg"}}`) to a `ProviderError`, following
/// the codes in the Graph API error-handling guide.
fn graph_error(status: StatusCode, body: &str, retry_after: Option<u64>, secrets: &[&str]) -> ProviderError {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let Some(err) = parsed.as_ref().and_then(|v| v.get("error")).filter(|e| e.is_object()) else {
        return http::status_error(status, &scrub(body, secrets), retry_after);
    };
    let code = ju(err, "/code").unwrap_or(0);
    let subcode = ju(err, "/error_subcode").unwrap_or(0);
    let technical = scrub(&js(err, "/message").unwrap_or_default(), secrets);
    let shown = js(err, "/error_user_msg")
        .map(|m| scrub(&m, secrets))
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| technical.clone());
    let msg = sentence(&shown);
    let lower = technical.to_lowercase();
    if lower.contains("appsecret_proof") {
        return ProviderError::Unauthorized(
            "Facebook rejected the app secret saved with this Page (it may have been reset). Reconnect the Page.".into(),
        );
    }
    if lower.contains("client secret") {
        return ProviderError::invalid(
            "Facebook did not accept the App secret. Copy it again from App settings → Basic into Settings → Developer apps.",
        );
    }
    if lower.contains("error validating application") || lower.contains("invalid application id") {
        return ProviderError::invalid("Facebook did not accept the App ID. Check it in Settings → Developer apps.");
    }
    match code {
        // Invalid, expired or revoked token (subcodes 458-467, 492) and API sessions.
        190 | 102 => ProviderError::Unauthorized(format!(
            "{} Reconnect the Page.",
            or(msg, "The Facebook sign-in is no longer valid.")
        )),
        10 | 200..=299 => ProviderError::Forbidden(match PERMISSION_RE.captures(&technical) {
            Some(c) => format!(
                "Facebook needs the {} permission for this. Check that it is added to your Meta app's use case, then reconnect the Page and allow it.",
                &c[1]
            ),
            None => format!(
                "Facebook refused this: {} If a permission is missing, reconnect the Page and allow everything the app asks for.",
                or(msg, "permission denied.")
            ),
        }),
        // App, user, Page and business use case rate limits.
        4 | 17 | 32 | 341 | 613 | 80001..=80014 => {
            let msg = or(msg, "Facebook's rate limit was reached.");
            ProviderError::RateLimited {
                message: match retry_after {
                    Some(s) => format!("{msg} Try again in {}.", http::human_duration(s)),
                    None => format!("{msg} Try again later."),
                },
                retry_after,
            }
        }
        368 => ProviderError::Forbidden(format!("Facebook blocked this for now: {}", or(msg, "policy restriction."))),
        506 => ProviderError::invalid("Facebook rejected this as a duplicate of a recent post. Change the text and try again."),
        1609005 => ProviderError::invalid(format!("Facebook could not use the link: {}", or(msg, "check the address."))),
        324 => ProviderError::invalid(format!("Facebook could not read the image: {}", or(msg, "missing or invalid file."))),
        803 => ProviderError::NotFound(or(msg, "Not found on Facebook.")),
        100 if subcode == 33 => ProviderError::NotFound(or(msg, "Not found on Facebook.")),
        1 | 2 => ProviderError::Server(or(msg, "Facebook had a temporary problem. Try again later.")),
        _ => http::status_error(status, &json!({ "error": { "message": msg } }).to_string(), retry_after),
    }
}

/// Ids that go into URL paths: Page, post (`page_post`), photo and comment ids.
fn object_id(id: &str) -> PResult<&str> {
    let id = id.trim();
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(id)
    } else {
        Err(ProviderError::invalid("That is not a Facebook post or Page id."))
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

fn handle_for(username: Option<&str>, name: &str) -> String {
    match username.filter(|u| !u.trim().is_empty()) {
        Some(u) => format!("@{u}"),
        None => name.to_string(),
    }
}

// --------------------------------------------------------------- sign-in

pub async fn begin(ctx: &Ctx, b: &OAuthBegin) -> PResult<OAuthStart> {
    let client_id = b.require_client_id()?;
    b.require_client_secret()?;
    let scope = SCOPES.join(",");
    let auth_url = crate::oauth::with_query(
        &format!("{}/{GRAPH_VERSION}/dialog/oauth", ctx.base("facebook.www", WWW)),
        &[
            ("client_id", client_id),
            ("redirect_uri", &b.redirect_uri),
            ("state", &b.state),
            ("response_type", "code"),
            ("scope", &scope),
            // Ask again for permissions declined in an earlier sign-in.
            ("auth_type", "rerequest"),
            ("code_challenge", &b.pkce.challenge),
            ("code_challenge_method", "S256"),
        ],
    );
    Ok(OAuthStart { auth_url, flow: json!({ "graphVersion": GRAPH_VERSION }) })
}

/// POSTs to `oauth/access_token` and returns the new token.
async fn exchange(ctx: &Ctx, form: &[(&str, &str)], what: &str, secrets: &[&str]) -> PResult<String> {
    let req = ctx.http.client.post(graph_url(ctx, "oauth/access_token")).form(form);
    let v = call(req, what, secrets).await?;
    non_empty(js(&v, "/access_token")).ok_or_else(|| ProviderError::parse(what, "no access_token"))
}

/// Fails when a permission the app can't work without was declined.
async fn check_permissions(user: &Api<'_>) -> PResult<()> {
    let v = match user.call(user.get("me/permissions"), "Facebook permissions").await {
        Ok(v) => v,
        Err(e) => {
            log::warn!("could not read the granted Facebook permissions: {e}");
            return Ok(());
        }
    };
    let list = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
    if list.is_empty() {
        return Ok(());
    }
    let granted: Vec<String> = list
        .iter()
        .filter(|p| js(p, "/status").as_deref() == Some("granted"))
        .filter_map(|p| js(p, "/permission"))
        .collect();
    let missing: Vec<&str> = REQUIRED.iter().copied().filter(|p| !granted.iter().any(|g| g == p)).collect();
    let optional: Vec<&str> = SCOPES
        .iter()
        .copied()
        .filter(|p| !REQUIRED.contains(p) && !granted.iter().any(|g| g == p))
        .collect();
    if !optional.is_empty() {
        log::warn!("Facebook permissions not granted: {}", optional.join(", "));
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(ProviderError::Forbidden(format!(
        "Facebook did not grant {}. Connect again and allow everything the app asks for; in your Meta app the permission must be added to the \"Manage everything on your Page\" use case.",
        missing.join(", ")
    )))
}

/// Every Page in `/me/accounts`, following the cursors.
async fn list_pages(user: &Api<'_>) -> PResult<Vec<Value>> {
    let mut pages = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..20 {
        let mut q = vec![("fields", ACCOUNT_FIELDS.to_string()), ("limit", "100".to_string())];
        if let Some(a) = &after {
            q.push(("after", a.clone()));
        }
        let v = user.call(user.get("me/accounts").query(&q), "Facebook Pages").await?;
        pages.extend(v.get("data").and_then(Value::as_array).cloned().unwrap_or_default());
        let has_next = v.pointer("/paging/next").and_then(Value::as_str).is_some();
        match js(&v, "/paging/cursors/after") {
            Some(a) if has_next => after = Some(a),
            _ => break,
        }
    }
    Ok(pages)
}

fn page_account(page: &Value, client_id: &str, client_secret: &str) -> Option<NewAccount> {
    let id = non_empty(js(page, "/id"))?;
    let token = non_empty(js(page, "/access_token"))?;
    let name = non_empty(js(page, "/name")).unwrap_or_else(|| format!("Page {id}"));
    let username = non_empty(js(page, "/username"));
    Some(NewAccount {
        platform: Platform::Facebook,
        server: String::new(),
        handle: handle_for(username.as_deref(), &name),
        display_name: name,
        avatar_url: js(page, "/picture/data/url"),
        profile_url: js(page, "/link").or_else(|| Some(format!("{WWW}/{id}"))),
        meta: json!({ "username": username, "tasks": page.get("tasks").cloned().unwrap_or(Value::Null) }),
        remote_id: id,
        credentials: Credentials {
            access_token: Some(token),
            client_id: Some(client_id.to_string()),
            client_secret: Some(client_secret.to_string()),
            ..Default::default()
        },
    })
}

pub async fn finish(ctx: &Ctx, b: &OAuthBegin, _flow: &Value, params: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    expect_state(params, &b.state)?;
    let code = expect_code(params)?;
    let client_id = b.require_client_id()?;
    let secret = b.require_client_secret()?;
    let short = exchange(
        ctx,
        &[
            ("client_id", client_id),
            ("client_secret", secret),
            ("redirect_uri", &b.redirect_uri),
            ("code", code),
            ("code_verifier", &b.pkce.verifier),
        ],
        "Facebook sign-in",
        &[secret, code],
    )
    .await?;
    // Tokens from the web login last an hour or two; the long-lived one
    // (about 60 days) yields Page tokens without an expiry date.
    let long = exchange(
        ctx,
        &[
            ("grant_type", "fb_exchange_token"),
            ("client_id", client_id),
            ("client_secret", secret),
            ("fb_exchange_token", &short),
        ],
        "Facebook token exchange",
        &[secret, &short],
    )
    .await?;
    let user = Api { ctx, token: &long, secret: Some(secret) };
    check_permissions(&user).await?;
    let accounts: Vec<NewAccount> = list_pages(&user)
        .await?
        .iter()
        .filter_map(|p| page_account(p, client_id, secret))
        .collect();
    if accounts.is_empty() {
        return Err(ProviderError::Forbidden(
            "Facebook did not share any Page with the app. Connect again and choose at least one Page you manage.".into(),
        ));
    }
    Ok(accounts)
}

/// Page tokens can't be renewed without the user signing in again.
pub async fn refresh(_ctx: &Ctx, _account: &Account, _creds: &Credentials) -> PResult<Credentials> {
    Err(ProviderError::Unauthorized(
        "The Facebook Page sign-in is no longer valid. Reconnect the Page.".into(),
    ))
}

pub async fn profile(ctx: &Ctx, account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    let api = Api::for_page(ctx, creds)?;
    let page = object_id(&account.remote_id)?;
    let v = api.call(api.get(page).query(&[("fields", PAGE_FIELDS)]), "Facebook Page").await?;
    let name = non_empty(js(&v, "/name"));
    let username = non_empty(js(&v, "/username"));
    let mut meta = serde_json::Map::new();
    meta.insert("username".into(), json!(username));
    Ok(ProfileUpdate {
        handle: name.as_deref().map(|n| handle_for(username.as_deref(), n)),
        display_name: name,
        avatar_url: js(&v, "/picture/data/url"),
        profile_url: js(&v, "/link"),
        stats: ProfileStats {
            followers: ju(&v, "/followers_count").or_else(|| ju(&v, "/fan_count")),
            following: None,
            posts: None,
        },
        meta,
    })
}

// ---------------------------------------------------------------- posting

fn post_link(post: &OutgoingPost) -> Option<&str> {
    post.options.link.as_deref().map(str::trim).filter(|l| !l.is_empty())
}

fn is_web_link(link: &str) -> bool {
    url::Url::parse(link)
        .map(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty()))
        .unwrap_or(false)
}

/// Photo posts and comments have no link preview: the link goes in the text.
fn with_link(text: &str, link: Option<&str>) -> String {
    match link {
        Some(l) if !text.contains(l) => {
            if text.is_empty() {
                l.to_string()
            } else {
                format!("{text}\n\n{l}")
            }
        }
        _ => text.to_string(),
    }
}

pub fn validate(_account: &Account, post: &OutgoingPost) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(link) = post_link(post)
        && !is_web_link(link)
    {
        problems.push("The link must be a full web address starting with https:// or http://.".to_string());
    }
    if let Some(reply) = &post.reply_to {
        if js(reply, "/id").filter(|id| object_id(id).is_ok()).is_none() {
            problems.push("The Facebook post being replied to is missing its reference. Refresh the stream and try again.".to_string());
        }
        if post.media.len() > 1 {
            problems.push("A Facebook reply is a comment, which can include only one image.".to_string());
        }
    }
    problems
}

fn image_part(img: PreparedImage) -> PResult<Part> {
    Part::bytes(img.bytes)
        .file_name(img.file_name)
        .mime_str(&img.mime)
        .map_err(|e| ProviderError::other(e.to_string()))
}

/// `page_post` → `https://www.facebook.com/{page}/posts/{post}`, used when
/// the permalink can't be read.
fn fallback_url(post_id: &str) -> String {
    match post_id.split_once('_') {
        Some((page, post)) => format!("{WWW}/{page}/posts/{post}"),
        None => format!("{WWW}/{post_id}"),
    }
}

async fn permalink(api: &Api<'_>, id: &str) -> Option<String> {
    match api.call(api.get(id).query(&[("fields", "permalink_url")]), "Facebook permalink").await {
        Ok(v) => js(&v, "/permalink_url").filter(|u| u.starts_with("http")),
        Err(e) => {
            log::warn!("could not read the Facebook permalink: {e}");
            None
        }
    }
}

/// Uploads a photo to the Page. With a caption it is published as a post;
/// without one it stays unpublished, to be attached to a post.
async fn upload_photo(api: &Api<'_>, page: &str, img: PreparedImage, alt: &str, caption: Option<&str>) -> PResult<Value> {
    let mut form = Form::new().part("source", image_part(img)?);
    match caption {
        Some(c) if !c.is_empty() => form = form.text("caption", c.to_string()),
        Some(_) => {}
        None => form = form.text("published", "false"),
    }
    if !alt.trim().is_empty() {
        form = form.text("alt_text_custom", alt.trim().to_string());
    }
    api.call(api.post(&format!("{page}/photos")).multipart(form), "Facebook photo").await
}

/// A reply: a comment by the Page on the post (text and at most one image).
async fn comment(api: &Api<'_>, reply: &Value, message: &str, images: Vec<(PreparedImage, String)>) -> PResult<PublishResult> {
    let target = js(reply, "/id").ok_or_else(|| {
        ProviderError::invalid("The Facebook post being replied to is missing its reference. Refresh the stream and try again.")
    })?;
    let target = object_id(&target)?;
    if images.len() > 1 {
        return Err(ProviderError::invalid("A Facebook reply is a comment, which can include only one image."));
    }
    let path = format!("{target}/comments");
    let v = match images.into_iter().next() {
        Some((img, _)) => {
            let mut form = Form::new().part("source", image_part(img)?);
            if !message.is_empty() {
                form = form.text("message", message.to_string());
            }
            api.call(api.post(&path).multipart(form), "Facebook comment").await?
        }
        None => api.call(api.post(&path).form(&[("message", message)]), "Facebook comment").await?,
    };
    let id = non_empty(js(&v, "/id")).ok_or_else(|| ProviderError::parse("Facebook comment", "no id"))?;
    let url = match permalink(api, &id).await {
        Some(u) => Some(u),
        None => js(reply, "/url"),
    };
    Ok(PublishResult { remote_id: id, url })
}

pub async fn publish(ctx: &Ctx, account: &Account, creds: &Credentials, post: &OutgoingPost) -> PResult<PublishResult> {
    let api = Api::for_page(ctx, creds)?;
    let page = object_id(&account.remote_id)?;
    let images = super::prepare_images(account, post)?;
    let text = post.text.trim();
    let link = post_link(post);
    if let Some(reply) = &post.reply_to {
        return comment(&api, reply, &with_link(text, link), images).await;
    }
    let id = match images.len() {
        0 => {
            let mut body = json!({});
            if !text.is_empty() {
                body["message"] = json!(text);
            }
            if let Some(l) = link {
                body["link"] = json!(l);
            }
            let v = api.call(api.post(&format!("{page}/feed")).json(&body), "Facebook post").await?;
            js(&v, "/id")
        }
        1 => {
            let (img, alt) = images.into_iter().next().expect("one image");
            let caption = with_link(text, link);
            let v = upload_photo(&api, page, img, &alt, Some(&caption)).await?;
            js(&v, "/post_id").or_else(|| js(&v, "/id"))
        }
        _ => {
            let mut attached = Vec::new();
            for (img, alt) in images {
                let v = upload_photo(&api, page, img, &alt, None).await?;
                let id = non_empty(js(&v, "/id")).ok_or_else(|| ProviderError::parse("Facebook photo", "no id"))?;
                attached.push(json!({ "media_fbid": id }));
            }
            let mut body = json!({ "attached_media": attached });
            let message = with_link(text, link);
            if !message.is_empty() {
                body["message"] = json!(message);
            }
            let v = api.call(api.post(&format!("{page}/feed")).json(&body), "Facebook post").await?;
            js(&v, "/id")
        }
    };
    let id = non_empty(id).ok_or_else(|| ProviderError::parse("Facebook post", "no id"))?;
    let url = match permalink(&api, &id).await {
        Some(u) => u,
        None => fallback_url(&id),
    };
    Ok(PublishResult { remote_id: id, url: Some(url) })
}

// ---------------------------------------------------------------- reading

pub async fn stream(
    ctx: &Ctx,
    account: &Account,
    creds: &Credentials,
    kind: &str,
    _param: Option<&str>,
    cursor: Option<&str>,
) -> PResult<FeedPage> {
    if kind != "me" {
        return Err(ProviderError::unsupported(format!("Facebook Pages have no \"{kind}\" stream.")));
    }
    let api = Api::for_page(ctx, creds)?;
    let page = object_id(&account.remote_id)?;
    let mut q = vec![("fields", POST_FIELDS.to_string()), ("limit", PAGE_SIZE.to_string())];
    if let Some(c) = cursor.filter(|c| !c.is_empty()) {
        q.push(("after", c.to_string()));
    }
    // `/posts` is "This Page's own Posts"; `/feed` also has visitors' posts.
    let v = api.call(api.get(&format!("{page}/posts")).query(&q), "Facebook Page posts").await?;
    let items = v
        .get("data")
        .and_then(Value::as_array)
        .map(|list| list.iter().map(|p| post_item(p, account)).collect())
        .unwrap_or_default();
    let has_next = v.pointer("/paging/next").and_then(Value::as_str).is_some();
    Ok(FeedPage { items, cursor: if has_next { js(&v, "/paging/cursors/after") } else { None } })
}

/// Graph returns `2026-11-10T08:30:00+0000`; the app wants RFC 3339 in UTC.
fn rfc3339(t: Option<String>) -> String {
    let Some(t) = t else { return String::new() };
    let utc = |dt: chrono::DateTime<chrono::FixedOffset>| {
        dt.with_timezone(&chrono::Utc).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    if let Ok(dt) = chrono::DateTime::parse_from_str(&t, "%Y-%m-%dT%H:%M:%S%z") {
        return utc(dt);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&t) {
        return utc(dt);
    }
    if let Some(dt) = t.parse::<i64>().ok().and_then(|s| chrono::DateTime::from_timestamp(s, 0)) {
        return dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    }
    t
}

fn author(from: Option<&Value>, account: &Account) -> Author {
    let avatar = from.and_then(|f| js(f, "/picture/data/url"));
    match from.filter(|f| js(f, "/id").is_some_and(|id| id != account.remote_id)) {
        Some(other) => {
            let name = js(other, "/name").unwrap_or_default();
            Author {
                handle: name.clone(),
                name,
                avatar,
                url: js(other, "/id").map(|id| format!("{WWW}/{id}")),
            }
        }
        None => Author {
            name: from
                .and_then(|f| non_empty(js(f, "/name")))
                .unwrap_or_else(|| account.display_name.clone()),
            handle: account.handle.clone(),
            avatar: avatar.or_else(|| account.avatar_url.clone()),
            url: account.profile_url.clone(),
        },
    }
}

/// Photos, videos (including the photos of multi-photo posts in
/// `subattachments`) and link previews of a post.
fn collect_attachment(att: &Value, media: &mut Vec<MediaView>, link: &mut Option<LinkCard>) {
    if let Some(subs) = att.pointer("/subattachments/data").and_then(Value::as_array).filter(|s| !s.is_empty()) {
        for sub in subs {
            collect_attachment(sub, media, link);
        }
        return;
    }
    let kind = js(att, "/type").unwrap_or_default();
    let media_type = js(att, "/media_type").unwrap_or_default();
    let image = att.pointer("/media/image");
    let src = image.and_then(|i| js(i, "/src"));
    let width = image.and_then(|i| ju(i, "/width")).map(|v| v as u32);
    let height = image.and_then(|i| ju(i, "/height")).map(|v| v as u32);
    if media_type == "link" || kind == "share" {
        if link.is_none() {
            *link = Some(LinkCard {
                url: js(att, "/unshimmed_url").or_else(|| js(att, "/url")).unwrap_or_default(),
                title: non_empty(js(att, "/title")),
                description: non_empty(js(att, "/description")),
                image: src,
            });
        }
        return;
    }
    let source = js(att, "/media/source");
    let view_kind = if kind.contains("animated") {
        "gif"
    } else if media_type == "video" || kind.starts_with("video") {
        "video"
    } else {
        "image"
    };
    let url = match view_kind {
        "image" => src.clone(),
        _ => source.or_else(|| js(att, "/url")).or_else(|| src.clone()),
    };
    if let Some(url) = url {
        media.push(MediaView {
            kind: view_kind.into(),
            preview_url: if view_kind == "image" { None } else { src },
            url,
            alt: None,
            width,
            height,
        });
    }
}

/// Post text as HTML with clickable links and hashtags.
fn text_html(text: &str) -> String {
    let mut spans: Vec<(usize, usize, String)> =
        text::find_urls(text).into_iter().map(|(s, e)| (s, e, text[s..e].to_string())).collect();
    for (s, e, tag) in text::find_tags(text) {
        if spans.iter().any(|(a, b, _)| s < *b && e > *a) {
            continue;
        }
        let tag: String = url::form_urlencoded::byte_serialize(tag.as_bytes()).collect();
        spans.push((s, e, format!("{WWW}/hashtag/{tag}")));
    }
    spans.sort_by_key(|s| s.0);
    let mut html = String::with_capacity(text.len() + 32);
    let mut last = 0;
    for (s, e, href) in spans {
        if s < last {
            continue;
        }
        html.push_str(&escape_html(&text[last..s]));
        html.push_str(&format!("<a href=\"{}\">{}</a>", escape_html(&href), escape_html(&text[s..e])));
        last = e;
    }
    html.push_str(&escape_html(&text[last..]));
    format!("<p>{}</p>", html.replace('\n', "<br>"))
}

fn post_item(p: &Value, account: &Account) -> FeedItem {
    let id = js(p, "/id").unwrap_or_default();
    let url = js(p, "/permalink_url");
    let message = js(p, "/message").unwrap_or_default();
    let mut media = Vec::new();
    let mut link = None;
    for att in p.pointer("/attachments/data").and_then(Value::as_array).into_iter().flatten() {
        collect_attachment(att, &mut media, &mut link);
    }
    if media.is_empty()
        && link.is_none()
        && let Some(pic) = non_empty(js(p, "/full_picture"))
    {
        media.push(MediaView::image(pic));
    }
    FeedItem {
        id: id.clone(),
        account_id: String::new(),
        url: url.clone(),
        author: author(p.get("from"), account),
        created_at: rfc3339(js(p, "/created_time")),
        html: if message.trim().is_empty() { None } else { Some(text_html(&message)) },
        text: message,
        // "Cafe One added 2 new photos." and similar.
        context: non_empty(js(p, "/story")),
        media,
        link,
        counts: Counts {
            replies: ju(p, "/comments/summary/total_count"),
            // Graph leaves `shares` out when nobody shared the post.
            reposts: Some(ju(p, "/shares/count").unwrap_or(0)),
            likes: ju(p, "/reactions/summary/total_count"),
            quotes: None,
            score: None,
        },
        actions: ItemActions { like: false, repost: false, reply: true, delete: true },
        target: json!({ "id": id, "url": url }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- actions

pub async fn interact(_ctx: &Ctx, _account: &Account, _creds: &Credentials, _target: &Value, _action: Interaction) -> PResult<Value> {
    Err(ProviderError::unsupported("Liking and sharing are not available for Facebook Pages."))
}

/// Deletes a post (or a reply, which is a comment) made by the Page.
pub async fn delete(ctx: &Ctx, _account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    let api = Api::for_page(ctx, creds)?;
    let id = object_id(remote_id)?;
    let v = api.call(api.delete(id), "Facebook delete").await?;
    if v.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(ProviderError::other("Facebook did not delete the post."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::Pkce;
    use crate::testutil::bytes_contains;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{body_partial_json, body_string_contains, header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const SECRET: &str = "app-secret-0123456789";
    const PAGE_TOKEN: &str = "EAAPageTokenOne1234567890abcdef";
    const SHORT_TOKEN: &str = "EAAShortUserToken1234567890abc";
    const LONG_TOKEN: &str = "EAALongUserToken1234567890abcd";

    fn v(p: &str) -> String {
        format!("/{GRAPH_VERSION}/{p}")
    }

    fn ctx(server: &MockServer) -> Ctx {
        Ctx::default()
            .with_override("facebook.graph", &server.uri())
            .with_override("facebook.www", &server.uri())
    }

    fn account() -> Account {
        Account {
            id: "a1".into(),
            platform: Platform::Facebook,
            server: String::new(),
            remote_id: "111".into(),
            handle: "@cafeone".into(),
            display_name: "Cafe One".into(),
            avatar_url: Some("https://scontent.example/p111.jpg".into()),
            profile_url: Some("https://www.facebook.com/cafeone".into()),
            meta: json!({}),
            status: AccountStatus::Ok,
            status_message: None,
            position: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn creds() -> Credentials {
        Credentials {
            access_token: Some(PAGE_TOKEN.into()),
            client_id: Some("app-1".into()),
            client_secret: Some(SECRET.into()),
            ..Default::default()
        }
    }

    fn proof(token: &str) -> String {
        appsecret_proof(token, SECRET)
    }

    fn outgoing(text: &str) -> OutgoingPost {
        OutgoingPost {
            text: text.into(),
            media: vec![],
            options: PostOptions::default(),
            reply_to: None,
            idempotency_key: "k".into(),
        }
    }

    fn photo(dir: &std::path::Path, name: &str, alt: &str) -> MediaAttachment {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 6, image::Rgb([10, 20, 30])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let mut att = crate::media::import_bytes(dir, &png, name).unwrap();
        att.alt = alt.into();
        att
    }

    fn begin_params() -> OAuthBegin {
        OAuthBegin {
            platform: Platform::Facebook,
            fields: HashMap::new(),
            redirect_uri: "http://localhost:8765/callback".into(),
            state: "st".into(),
            pkce: Pkce::new(),
            client_id: Some("app-1".into()),
            client_secret: Some(SECRET.into()),
        }
    }

    async fn mount_token_exchange(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path(v("oauth/access_token")))
            .and(body_string_contains("code=thecode"))
            .and(body_string_contains("client_id=app-1"))
            .and(body_string_contains(format!("client_secret={SECRET}")))
            .and(body_string_contains("redirect_uri=http%3A%2F%2Flocalhost%3A8765%2Fcallback"))
            .and(body_string_contains("code_verifier="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": SHORT_TOKEN, "token_type": "bearer", "expires_in": 5183
            })))
            .expect(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path(v("oauth/access_token")))
            .and(body_string_contains("grant_type=fb_exchange_token"))
            .and(body_string_contains(format!("fb_exchange_token={SHORT_TOKEN}")))
            .and(body_string_contains(format!("client_secret={SECRET}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": LONG_TOKEN, "token_type": "bearer", "expires_in": 5183944
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_permissions(server: &MockServer, declined: &[&str]) {
        let data: Vec<Value> = SCOPES
            .iter()
            .chain(["public_profile"].iter())
            .map(|p| json!({ "permission": p, "status": if declined.contains(p) { "declined" } else { "granted" } }))
            .collect();
        Mock::given(method("GET"))
            .and(path(v("me/permissions")))
            .and(header("authorization", format!("Bearer {LONG_TOKEN}").as_str()))
            .and(query_param("appsecret_proof", proof(LONG_TOKEN).as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": data })))
            .mount(server)
            .await;
    }

    fn callback() -> HashMap<String, String> {
        HashMap::from([("code".to_string(), "thecode".to_string()), ("state".to_string(), "st".to_string())])
    }

    #[tokio::test]
    async fn sign_in_turns_each_granted_page_into_an_account() {
        let server = MockServer::start().await;
        mount_token_exchange(&server).await;
        mount_permissions(&server, &[]).await;
        // First page of /me/accounts, shaped like the "long-lived Page access
        // token" example, with a cursor to a second page.
        Mock::given(method("GET"))
            .and(path(v("me/accounts")))
            .and(header("authorization", format!("Bearer {LONG_TOKEN}").as_str()))
            .and(query_param("appsecret_proof", proof(LONG_TOKEN).as_str()))
            .and(query_param("fields", ACCOUNT_FIELDS))
            .and(query_param_is_missing("after"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {
                        "access_token": PAGE_TOKEN,
                        "category": "Cafe",
                        "category_list": [{"id": "128673187201735", "name": "Cafe"}],
                        "name": "Cafe One",
                        "id": "111",
                        "tasks": ["ANALYZE", "ADVERTISE", "MODERATE", "CREATE_CONTENT", "MANAGE"],
                        "username": "cafeone",
                        "link": "https://www.facebook.com/cafeone",
                        "picture": {"data": {"height": 200, "is_silhouette": false, "url": "https://scontent.example/p111.jpg", "width": 200}}
                    },
                    {"name": "Insights Only", "id": "333", "tasks": ["ANALYZE"]}
                ],
                "paging": {
                    "cursors": {"before": "QVFIbefore", "after": "QVFIafter"},
                    "next": "https://graph.facebook.com/v27.0/me/accounts?after=QVFIafter"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(v("me/accounts")))
            .and(query_param("after", "QVFIafter"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{
                    "access_token": "EAAPageTokenTwo1234567890abcdef",
                    "category": "Community",
                    "name": "Neighbourhood News",
                    "id": "222",
                    "tasks": ["CREATE_CONTENT", "MODERATE"],
                    "link": "https://www.facebook.com/profile.php?id=222",
                    "picture": {"data": {"url": "https://scontent.example/p222.jpg"}}
                }],
                "paging": {"cursors": {"before": "QVFIafter", "after": "QVFIend"}}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let ctx = ctx(&server);
        let b = begin_params();
        let start = begin(&ctx, &b).await.unwrap();
        let url = url::Url::parse(&start.auth_url).unwrap();
        assert_eq!(url.path(), format!("/{GRAPH_VERSION}/dialog/oauth"));
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "app-1");
        assert_eq!(q["redirect_uri"], "http://localhost:8765/callback");
        assert_eq!(q["state"], "st");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge"], b.pkce.challenge);
        assert_eq!(q["code_challenge_method"], "S256");
        let scopes: Vec<&str> = q["scope"].split(',').collect();
        for s in ["pages_show_list", "pages_read_engagement", "pages_manage_posts", "pages_manage_engagement"] {
            assert!(scopes.contains(&s), "{s} missing from {scopes:?}");
        }
        assert!(!start.auth_url.contains(SECRET));

        // A redirect for another sign-in is rejected before any request.
        let bad = HashMap::from([("code".to_string(), "thecode".to_string()), ("state".to_string(), "other".to_string())]);
        assert!(finish(&ctx, &b, &start.flow, &bad).await.is_err());

        let accounts = finish(&ctx, &b, &start.flow, &callback()).await.unwrap();
        assert_eq!(accounts.len(), 2, "the Page without a token is skipped");
        let a = &accounts[0];
        assert_eq!(a.platform, Platform::Facebook);
        assert_eq!(a.server, "");
        assert_eq!(a.remote_id, "111");
        assert_eq!(a.handle, "@cafeone");
        assert_eq!(a.display_name, "Cafe One");
        assert_eq!(a.avatar_url.as_deref(), Some("https://scontent.example/p111.jpg"));
        assert_eq!(a.profile_url.as_deref(), Some("https://www.facebook.com/cafeone"));
        assert_eq!(a.credentials.access_token.as_deref(), Some(PAGE_TOKEN));
        assert_eq!(a.credentials.client_id.as_deref(), Some("app-1"));
        assert_eq!(a.credentials.client_secret.as_deref(), Some(SECRET));
        assert_eq!(a.credentials.expires_at, None);
        assert_eq!(a.credentials.refresh_token, None);
        let b2 = &accounts[1];
        assert_eq!(b2.remote_id, "222");
        assert_eq!(b2.handle, "Neighbourhood News", "no username: the Page name is the handle");
    }

    #[tokio::test]
    async fn sign_in_reports_declined_permissions() {
        let server = MockServer::start().await;
        mount_token_exchange(&server).await;
        mount_permissions(&server, &["pages_manage_posts"]).await;
        let b = begin_params();
        let err = finish(&ctx(&server), &b, &json!({}), &callback()).await.unwrap_err();
        assert!(matches!(&err, ProviderError::Forbidden(m) if m.contains("pages_manage_posts")), "{err:?}");
    }

    #[tokio::test]
    async fn sign_in_needs_the_app_secret() {
        let mut b = begin_params();
        b.client_secret = None;
        let err = begin(&Ctx::default(), &b).await.unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn profile_reads_followers_and_maps_expired_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(v("111")))
            .and(query_param("fields", PAGE_FIELDS))
            .and(header("authorization", format!("Bearer {PAGE_TOKEN}").as_str()))
            .and(query_param("appsecret_proof", proof(PAGE_TOKEN).as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "111", "name": "Cafe One", "username": "cafeone", "link": "https://www.facebook.com/cafeone",
                "picture": {"data": {"height": 200, "is_silhouette": false, "url": "https://scontent.example/new.jpg", "width": 200}},
                "followers_count": 1234, "fan_count": 1200
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(v("222")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "222", "name": "Neighbourhood News", "fan_count": 77
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(v("444")))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
                "message": "Error validating access token: The session has been invalidated because the user changed their password or Facebook has changed the session for security reasons.",
                "type": "OAuthException", "code": 190, "error_subcode": 460, "fbtrace_id": "AbCd"
            }})))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let p = profile(&ctx, &account(), &creds()).await.unwrap();
        assert_eq!(p.stats.followers, Some(1234));
        assert_eq!(p.handle.as_deref(), Some("@cafeone"));
        assert_eq!(p.display_name.as_deref(), Some("Cafe One"));
        assert_eq!(p.avatar_url.as_deref(), Some("https://scontent.example/new.jpg"));

        let mut other = account();
        other.remote_id = "222".into();
        let p = profile(&ctx, &other, &creds()).await.unwrap();
        assert_eq!(p.stats.followers, Some(77), "falls back to fan_count");
        assert_eq!(p.handle.as_deref(), Some("Neighbourhood News"));

        other.remote_id = "444".into();
        let err = profile(&ctx, &other, &creds()).await.unwrap_err();
        assert!(matches!(&err, ProviderError::Unauthorized(m) if m.contains("changed their password")), "{err:?}");
    }

    #[tokio::test]
    async fn publishes_text_with_link_and_reads_the_permalink() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(v("111/feed")))
            .and(header("authorization", format!("Bearer {PAGE_TOKEN}").as_str()))
            .and(query_param("appsecret_proof", proof(PAGE_TOKEN).as_str()))
            .and(body_partial_json(json!({"message": "Hello", "link": "https://example.com/menu"})))
            .respond_with(|req: &Request| {
                // The token travels in the header, never in the URL.
                assert!(!req.url.as_str().contains(PAGE_TOKEN));
                ResponseTemplate::new(200).set_body_json(json!({"id": "111_222"}))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(v("111_222")))
            .and(query_param("fields", "permalink_url"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "permalink_url": "https://www.facebook.com/cafeone/posts/pfbid02abc", "id": "111_222"
            })))
            .mount(&server)
            .await;
        let mut post = outgoing("  Hello ");
        post.options.link = Some("https://example.com/menu".into());
        let res = publish(&ctx(&server), &account(), &creds(), &post).await.unwrap();
        assert_eq!(res.remote_id, "111_222");
        assert_eq!(res.url.as_deref(), Some("https://www.facebook.com/cafeone/posts/pfbid02abc"));
    }

    #[tokio::test]
    async fn publishes_one_photo_with_caption_and_alt_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(v("111/photos")))
            .and(header("authorization", format!("Bearer {PAGE_TOKEN}").as_str()))
            .and(bytes_contains("name=\"source\"; filename=\"p1.png\""))
            .and(bytes_contains("name=\"caption\""))
            .and(bytes_contains("Fresh bread\n\nhttps://example.com/bread"))
            .and(bytes_contains("name=\"alt_text_custom\""))
            .and(bytes_contains("A loaf on a table"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "333", "post_id": "111_444"})))
            .expect(1)
            .mount(&server)
            .await;
        // Permalink lookup fails: the post URL is built from the id instead.
        Mock::given(method("GET"))
            .and(path(v("111_444")))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": {
                "message": "An unknown error has occurred.", "type": "OAuthException", "code": 1
            }})))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut post = outgoing("Fresh bread");
        post.media = vec![photo(dir.path(), "p1.png", "A loaf on a table")];
        post.options.link = Some("https://example.com/bread".into());
        let res = publish(&ctx(&server), &account(), &creds(), &post).await.unwrap();
        assert_eq!(res.remote_id, "111_444");
        assert_eq!(res.url.as_deref(), Some("https://www.facebook.com/111/posts/444"));
    }

    #[tokio::test]
    async fn publishes_several_photos_as_one_post() {
        let server = MockServer::start().await;
        let uploads = Arc::new(AtomicUsize::new(0));
        let counter = uploads.clone();
        Mock::given(method("POST"))
            .and(path(v("111/photos")))
            .and(bytes_contains("name=\"published\""))
            .and(bytes_contains("false"))
            .respond_with(move |_: &Request| {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                ResponseTemplate::new(200).set_body_json(json!({"id": format!("90{n}")}))
            })
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(v("111/feed")))
            .and(body_partial_json(json!({
                "message": "Two pictures",
                "attached_media": [{"media_fbid": "901"}, {"media_fbid": "902"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "111_555"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(v("111_555")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "permalink_url": "https://www.facebook.com/cafeone/posts/pfbid0555", "id": "111_555"
            })))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut post = outgoing("Two pictures");
        post.media = vec![photo(dir.path(), "a.png", ""), photo(dir.path(), "b.png", "")];
        let res = publish(&ctx(&server), &account(), &creds(), &post).await.unwrap();
        assert_eq!(res.remote_id, "111_555");
        assert_eq!(res.url.as_deref(), Some("https://www.facebook.com/cafeone/posts/pfbid0555"));
        assert_eq!(uploads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn replies_are_comments_with_at_most_one_image() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(v("111_222/comments")))
            .and(bytes_contains("name=\"source\""))
            .and(bytes_contains("name=\"message\""))
            .and(bytes_contains("See you there"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "222_7"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(v("111_222/comments")))
            .and(body_string_contains("message=Thanks%21"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "222_8"})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(v("222_7")))
            .and(query_param("fields", "permalink_url"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "permalink_url": "https://www.facebook.com/cafeone/posts/pfbid0abc?comment_id=7", "id": "222_7"
            })))
            .mount(&server)
            .await;
        let target = json!({"id": "111_222", "url": "https://www.facebook.com/cafeone/posts/pfbid0abc"});
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&server);

        let mut post = outgoing("See you there");
        post.reply_to = Some(target.clone());
        post.media = vec![photo(dir.path(), "r.png", "")];
        let res = publish(&ctx, &account(), &creds(), &post).await.unwrap();
        assert_eq!(res.remote_id, "222_7");
        assert_eq!(res.url.as_deref(), Some("https://www.facebook.com/cafeone/posts/pfbid0abc?comment_id=7"));

        // Without a readable permalink the reply links to the post.
        let mut post = outgoing("Thanks!");
        post.reply_to = Some(target.clone());
        let res = publish(&ctx, &account(), &creds(), &post).await.unwrap();
        assert_eq!(res.remote_id, "222_8");
        assert_eq!(res.url.as_deref(), Some("https://www.facebook.com/cafeone/posts/pfbid0abc"));

        let mut post = outgoing("Two images");
        post.reply_to = Some(target);
        post.media = vec![photo(dir.path(), "x.png", ""), photo(dir.path(), "y.png", "")];
        assert!(validate(&account(), &post).iter().any(|m| m.contains("only one image")));
        let err = publish(&ctx, &account(), &creds(), &post).await.unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{err:?}");
    }

    #[tokio::test]
    async fn maps_page_posts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(v("111/posts")))
            .and(query_param("after", "QVFIcur"))
            .and(query_param("fields", POST_FIELDS))
            .and(query_param("limit", PAGE_SIZE))
            .and(header("authorization", format!("Bearer {PAGE_TOKEN}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {
                        "id": "111_1001",
                        "message": "New menu! https://example.com/menu #coffee",
                        "created_time": "2026-11-10T08:30:00+0000",
                        "permalink_url": "https://www.facebook.com/cafeone/posts/pfbid01",
                        "full_picture": "https://scontent.example/link.jpg",
                        "status_type": "shared_story",
                        "from": {"name": "Cafe One", "id": "111", "picture": {"data": {"url": "https://scontent.example/from.jpg"}}},
                        "attachments": {"data": [{
                            "type": "share", "media_type": "link",
                            "url": "https://l.facebook.com/l.php?u=https%3A%2F%2Fexample.com%2Fmenu",
                            "unshimmed_url": "https://example.com/menu",
                            "title": "Our menu", "description": "Fresh coffee every day",
                            "media": {"image": {"height": 630, "src": "https://scontent.example/link.jpg", "width": 1200}},
                            "target": {"url": "https://l.facebook.com/l.php?u=https%3A%2F%2Fexample.com%2Fmenu"}
                        }]},
                        "shares": {"count": 3},
                        "comments": {"data": [], "summary": {"order": "ranked", "total_count": 4, "can_comment": true}},
                        "reactions": {"data": [], "summary": {"total_count": 12, "viewer_reaction": "NONE"}}
                    },
                    {
                        "id": "111_1002",
                        "message": "Our week",
                        "story": "Cafe One added 2 new photos.",
                        "created_time": "2026-11-09T18:00:00+0100",
                        "permalink_url": "https://www.facebook.com/cafeone/posts/pfbid02",
                        "status_type": "added_photos",
                        "from": {"name": "Cafe One", "id": "111"},
                        "attachments": {"data": [{
                            "type": "album", "media_type": "album", "title": "Our week",
                            "url": "https://www.facebook.com/media/set/?set=a.1",
                            "media": {"image": {"height": 720, "src": "https://scontent.example/cover.jpg", "width": 720}},
                            "subattachments": {"data": [
                                {"type": "photo", "media_type": "photo", "url": "https://www.facebook.com/photo.php?fbid=1",
                                 "media": {"image": {"height": 720, "src": "https://scontent.example/s1.jpg", "width": 960}},
                                 "target": {"id": "1", "url": "https://www.facebook.com/photo.php?fbid=1"}},
                                {"type": "photo", "media_type": "photo", "url": "https://www.facebook.com/photo.php?fbid=2",
                                 "media": {"image": {"height": 960, "src": "https://scontent.example/s2.jpg", "width": 720}},
                                 "target": {"id": "2", "url": "https://www.facebook.com/photo.php?fbid=2"}}
                            ]}
                        }]},
                        "comments": {"data": [], "summary": {"total_count": 0}},
                        "reactions": {"data": [], "summary": {"total_count": 5}}
                    },
                    {
                        "id": "111_1003",
                        "created_time": "2026-11-08T12:00:00+0000",
                        "permalink_url": "https://www.facebook.com/cafeone/videos/77/",
                        "status_type": "added_video",
                        "attachments": {"data": [{
                            "type": "video_inline", "media_type": "video",
                            "url": "https://www.facebook.com/cafeone/videos/77/",
                            "media": {"image": {"height": 720, "src": "https://scontent.example/v.jpg", "width": 1280},
                                      "source": "https://video.example/v.mp4"}
                        }]}
                    }
                ],
                "paging": {
                    "cursors": {"before": "QVFIbefore", "after": "QVFInext"},
                    "next": "https://graph.facebook.com/v27.0/111/posts?after=QVFInext"
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(v("111/posts")))
            .and(query_param("after", "QVFInext"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "paging": {"cursors": {"before": "QVFInext", "after": "QVFInext"}}
            })))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let page = stream(&ctx, &account(), &creds(), "me", None, Some("QVFIcur")).await.unwrap();
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.cursor.as_deref(), Some("QVFInext"));

        let link_post = &page.items[0];
        assert_eq!(link_post.id, "111_1001");
        assert_eq!(link_post.url.as_deref(), Some("https://www.facebook.com/cafeone/posts/pfbid01"));
        assert_eq!(link_post.created_at, "2026-11-10T08:30:00Z");
        assert_eq!(link_post.text, "New menu! https://example.com/menu #coffee");
        let html = link_post.html.as_deref().unwrap();
        assert!(html.contains("<a href=\"https://example.com/menu\">https://example.com/menu</a>"), "{html}");
        assert!(html.contains("<a href=\"https://www.facebook.com/hashtag/coffee\">#coffee</a>"), "{html}");
        let card = link_post.link.as_ref().unwrap();
        assert_eq!(card.url, "https://example.com/menu");
        assert_eq!(card.title.as_deref(), Some("Our menu"));
        assert_eq!(card.image.as_deref(), Some("https://scontent.example/link.jpg"));
        assert!(link_post.media.is_empty());
        assert_eq!(link_post.counts.replies, Some(4));
        assert_eq!(link_post.counts.likes, Some(12));
        assert_eq!(link_post.counts.reposts, Some(3));
        assert_eq!(link_post.author.handle, "@cafeone");
        assert_eq!(link_post.author.avatar.as_deref(), Some("https://scontent.example/from.jpg"));
        assert_eq!(link_post.actions, ItemActions { like: false, repost: false, reply: true, delete: true });
        assert_eq!(link_post.target["id"], json!("111_1001"));

        let album = &page.items[1];
        assert_eq!(album.created_at, "2026-11-09T17:00:00Z");
        assert_eq!(album.context.as_deref(), Some("Cafe One added 2 new photos."));
        let urls: Vec<&str> = album.media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(urls, vec!["https://scontent.example/s1.jpg", "https://scontent.example/s2.jpg"]);
        assert_eq!((album.media[0].width, album.media[0].height), (Some(960), Some(720)));
        assert_eq!(album.counts.reposts, Some(0));
        assert_eq!(album.author.avatar.as_deref(), Some("https://scontent.example/p111.jpg"));

        let video = &page.items[2];
        assert_eq!(video.text, "");
        assert!(video.html.is_none());
        assert_eq!(video.media[0].kind, "video");
        assert_eq!(video.media[0].url, "https://video.example/v.mp4");
        assert_eq!(video.media[0].preview_url.as_deref(), Some("https://scontent.example/v.jpg"));
        assert_eq!(video.author.name, "Cafe One");

        let last = stream(&ctx, &account(), &creds(), "me", None, Some("QVFInext")).await.unwrap();
        assert!(last.items.is_empty());
        assert_eq!(last.cursor, None, "no `next` link: the end of the list");

        let err = stream(&ctx, &account(), &creds(), "home", None, None).await.unwrap_err();
        assert!(matches!(err, ProviderError::Unsupported(_)));
    }

    #[tokio::test]
    async fn deletes_posts_and_has_no_likes() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path(v("111_222")))
            .and(header("authorization", format!("Bearer {PAGE_TOKEN}").as_str()))
            .and(query_param("appsecret_proof", proof(PAGE_TOKEN).as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(v("111_999")))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
                "message": "Unsupported delete request. Object with ID '111_999' does not exist, cannot be loaded due to missing permissions, or does not support this operation. Please read the Graph API documentation at https://developers.facebook.com/docs/graph-api",
                "type": "GraphMethodException", "code": 100, "error_subcode": 33, "fbtrace_id": "Ab"
            }})))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        delete(&ctx, &account(), &creds(), "111_222").await.unwrap();
        let err = delete(&ctx, &account(), &creds(), "111_999").await.unwrap_err();
        assert!(matches!(err, ProviderError::NotFound(_)), "{err:?}");
        let err = delete(&ctx, &account(), &creds(), "../me").await.unwrap_err();
        assert!(matches!(err, ProviderError::Invalid(_)), "{err:?}");

        let err = interact(&ctx, &account(), &creds(), &json!({"id": "111_222"}), Interaction::Like).await.unwrap_err();
        assert!(matches!(err, ProviderError::Unsupported(_)));
        let err = refresh(&ctx, &account(), &creds()).await.unwrap_err();
        assert!(matches!(err, ProviderError::Unauthorized(_)));
    }

    #[test]
    fn maps_graph_errors() {
        let e = |status: u16, err: Value| {
            graph_error(StatusCode::from_u16(status).unwrap(), &json!({ "error": err }).to_string(), None, &[PAGE_TOKEN, SECRET])
        };
        // Graph echoes malformed tokens; the message must not.
        let err = e(400, json!({"message": format!("Malformed access token {PAGE_TOKEN}"), "type": "OAuthException", "code": 190}));
        assert!(matches!(&err, ProviderError::Unauthorized(_)), "{err:?}");
        assert!(!err.to_string().contains("PageTokenOne"), "{err}");
        let err = e(400, json!({"message": "Bad token EAAOtherToken1234567890abcdefgh", "type": "OAuthException", "code": 190}));
        assert!(!err.to_string().contains("OtherToken"), "{err}");
        assert!(matches!(e(400, json!({"message": "API session error", "code": 102})), ProviderError::Unauthorized(_)));

        let err = e(403, json!({
            "message": "(#200) If posting to a page, requires both pages_read_engagement and pages_manage_posts as an admin with sufficient administrative permission",
            "type": "OAuthException", "code": 200
        }));
        assert!(matches!(&err, ProviderError::Forbidden(m) if m.contains("pages_read_engagement permission")), "{err:?}");
        let err = e(400, json!({"message": "(#10) Application does not have permission for this action", "type": "OAuthException", "code": 10}));
        assert!(matches!(&err, ProviderError::Forbidden(m) if m.contains("Application does not have permission")), "{err:?}");

        let err = e(400, json!({"message": "(#32) Page request limit reached", "type": "OAuthException", "code": 32}));
        assert!(matches!(&err, ProviderError::RateLimited { message, .. } if message == "Page request limit reached. Try again later."), "{err:?}");
        assert!(matches!(e(400, json!({"message": "(#80001) There have been too many calls to this Page account.", "code": 80001})), ProviderError::RateLimited { .. }));
        assert!(matches!(e(400, json!({"message": "(#4) Application request limit reached", "code": 4})), ProviderError::RateLimited { .. }));

        let err = e(400, json!({"message": "(#506) Duplicate status message", "type": "OAuthException", "code": 506}));
        assert!(matches!(&err, ProviderError::Invalid(m) if m.contains("duplicate")), "{err:?}");
        assert!(matches!(e(400, json!({"message": "(#368) The action attempted has been deemed abusive", "code": 368})), ProviderError::Forbidden(_)));
        assert!(matches!(e(400, json!({"message": "Error validating client secret.", "type": "OAuthException", "code": 1})), ProviderError::Invalid(m) if m.contains("App secret")));
        assert!(matches!(e(400, json!({"message": "Error validating application. Invalid application ID.", "code": 101})), ProviderError::Invalid(m) if m.contains("App ID")));
        assert!(matches!(e(400, json!({"message": "Invalid appsecret_proof provided in the API argument", "type": "GraphMethodException", "code": 100})), ProviderError::Unauthorized(_)));
        assert!(matches!(e(500, json!({"message": "An unknown error has occurred.", "type": "OAuthException", "code": 1})), ProviderError::Server(_)));
        assert_eq!(
            e(400, json!({"message": "(#100) The parameter url is required", "type": "OAuthException", "code": 100})),
            ProviderError::Invalid("The parameter url is required.".into())
        );
        // A user-facing message is preferred when Graph sends one.
        assert_eq!(
            e(400, json!({"message": "Invalid parameter", "code": 100, "error_user_title": "Link", "error_user_msg": "The link could not be shared"})),
            ProviderError::Invalid("The link could not be shared.".into())
        );
        // Not a Graph error body.
        assert!(matches!(
            graph_error(StatusCode::BAD_GATEWAY, "<html><body>Bad gateway</body></html>", None, &[]),
            ProviderError::Server(_)
        ));
    }

    #[test]
    fn appsecret_proof_is_hmac_sha256() {
        // RFC 4231 test cases 2 and 6.
        assert_eq!(
            appsecret_proof("what do ya want for nothing?", "Jefe"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hmac_sha256_hex(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First"),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn validates_links_and_replies() {
        let acc = account();
        let mut post = outgoing("Hi");
        assert!(validate(&acc, &post).is_empty());
        post.options.link = Some("example.com".into());
        assert_eq!(validate(&acc, &post).len(), 1);
        post.options.link = Some("ftp://example.com/file".into());
        assert_eq!(validate(&acc, &post).len(), 1);
        post.options.link = Some(" https://example.com/page ".into());
        assert!(validate(&acc, &post).is_empty());
        post.reply_to = Some(json!({"url": "https://www.facebook.com/x"}));
        assert!(validate(&acc, &post).iter().any(|m| m.contains("reference")));
        post.reply_to = Some(json!({"id": "111_2"}));
        assert!(validate(&acc, &post).is_empty());
        // A link-only post passes the shared checks too.
        let mut link_only = outgoing("");
        link_only.options.link = Some("https://example.com".into());
        assert!(super::super::validate(&acc, &link_only).is_empty());
        assert_eq!(with_link("Look", Some("https://a.example")), "Look\n\nhttps://a.example");
        assert_eq!(with_link("Look https://a.example", Some("https://a.example")), "Look https://a.example");
        assert_eq!(with_link("", Some("https://a.example")), "https://a.example");
    }
}
