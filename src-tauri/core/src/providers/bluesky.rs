//! Bluesky and other AT Protocol servers.
//!
//! Sign-in uses an app password (`com.atproto.server.createSession`). The
//! account's PDS is found from its handle (handle → DID → DID document), and
//! all later requests go to that PDS, which forwards `app.bsky.*` calls to
//! the Bluesky AppView. Access tokens last about two hours and are renewed
//! with the refresh token, or with the app password if that has expired too.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use reqwest::RequestBuilder;
use serde_json::{Value, json};

use super::{Ctx, js, ju};
use crate::error::{PResult, ProviderError};
use crate::http::{self, normalize_server, send};
use crate::model::*;
use crate::text::{self, escape_html};

const DEFAULT_PDS: &str = "https://bsky.social";
const APPVIEW: &str = "https://public.api.bsky.app";
const PLC: &str = "https://plc.directory";
const DISCOVER_FEED: &str = "at://did:plc:z72i7hdynmk6r22z27h6tvur/app.bsky.feed.generator/whats-hot";
const MAX_TEXT_BYTES: usize = 3000;

// ------------------------------------------------------------------ http

/// Sends an XRPC request and maps Bluesky's token errors (which come back
/// as 400 `ExpiredToken`) to `Unauthorized`.
async fn xrpc(req: RequestBuilder, what: &str) -> PResult<Value> {
    let resp = send(req).await?;
    let status = resp.status();
    if status.is_success() {
        let bytes = resp.bytes().await.map_err(ProviderError::from)?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_slice(&bytes).map_err(|e| ProviderError::parse(what, e));
    }
    let retry = http::retry_after_secs(&resp);
    let body = resp.text().await.unwrap_or_default();
    let err_code = serde_json::from_str::<Value>(&body).ok().and_then(|v| js(&v, "/error")).unwrap_or_default();
    if matches!(err_code.as_str(), "ExpiredToken" | "InvalidToken" | "AuthenticationRequired" | "AuthMissing") {
        let msg = http::error_message(&body).unwrap_or_else(|| "The Bluesky session expired.".into());
        return Err(ProviderError::Unauthorized(msg));
    }
    Err(http::status_error(status, &body, retry))
}

fn get(ctx: &Ctx, account: &Account, creds: &Credentials, method: &str) -> RequestBuilder {
    ctx.http
        .client
        .get(format!("{}/xrpc/{method}", account.server))
        .bearer_auth(creds.token())
}

fn post(ctx: &Ctx, account: &Account, creds: &Credentials, method: &str) -> RequestBuilder {
    ctx.http
        .client
        .post(format!("{}/xrpc/{method}", account.server))
        .bearer_auth(creds.token())
}

fn did(account: &Account) -> &str {
    &account.remote_id
}

// --------------------------------------------------------------- sign-in

/// Finds the PDS that hosts `handle` (or the DID itself).
async fn resolve_pds(ctx: &Ctx, identifier: &str) -> PResult<(String, String)> {
    let did = if identifier.starts_with("did:") {
        identifier.to_string()
    } else {
        let appview = ctx.base("bsky.appview", APPVIEW);
        let v = xrpc(
            ctx.http
                .client
                .get(format!("{appview}/xrpc/com.atproto.identity.resolveHandle"))
                .query(&[("handle", identifier)])
                .timeout(Duration::from_secs(20)),
            "handle resolution",
        )
        .await?;
        js(&v, "/did").ok_or_else(|| ProviderError::invalid(format!("Could not find the account {identifier}.")))?
    };
    let doc_url = if let Some(rest) = did.strip_prefix("did:web:") {
        format!("https://{}/.well-known/did.json", rest.replace("%3A", ":"))
    } else {
        format!("{}/{did}", ctx.base("bsky.plc", PLC))
    };
    let doc: Value = http::send_json(ctx.http.client.get(doc_url).timeout(Duration::from_secs(20)), "DID document").await?;
    let pds = pds_from_doc(&doc).ok_or_else(|| ProviderError::invalid("The account has no hosting provider listed."))?;
    Ok((did, pds))
}

fn pds_from_doc(doc: &Value) -> Option<String> {
    doc.get("service")?.as_array()?.iter().find_map(|s| {
        let id = js(s, "/id")?;
        if id.ends_with("#atproto_pds") {
            js(s, "/serviceEndpoint").map(|e| e.trim_end_matches('/').to_string())
        } else {
            None
        }
    })
}

/// Expiry (`exp`) of a JWT, without verifying it.
fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("exp")?.as_i64()
}

fn session_credentials(session: &Value, password: &str, identifier: &str) -> Credentials {
    let access = js(session, "/accessJwt").unwrap_or_default();
    let mut creds = Credentials {
        expires_at: jwt_exp(&access),
        access_token: Some(access),
        refresh_token: js(session, "/refreshJwt"),
        ..Default::default()
    };
    if !password.is_empty() {
        creds.set_extra("appPassword", password);
    }
    creds.set_extra("identifier", identifier);
    creds
}

async fn create_session(ctx: &Ctx, server: &str, identifier: &str, password: &str) -> PResult<Value> {
    let req = ctx
        .http
        .client
        .post(format!("{server}/xrpc/com.atproto.server.createSession"))
        .json(&json!({ "identifier": identifier, "password": password }));
    match xrpc(req, "Bluesky sign-in").await {
        Ok(v) => Ok(v),
        Err(ProviderError::Unauthorized(m)) if m.contains("AuthFactorTokenRequired") => Err(ProviderError::Forbidden(
            "This account asks for an email code at sign-in. Use an app password instead of your main password.".into(),
        )),
        Err(ProviderError::Unauthorized(_)) => Err(ProviderError::Unauthorized(
            "Bluesky did not accept this handle and app password.".into(),
        )),
        Err(e) => Err(e),
    }
}

pub async fn connect(ctx: &Ctx, fields: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    let identifier = fields
        .get("identifier")
        .map(|s| s.trim().trim_start_matches('@').to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProviderError::invalid("Enter your Bluesky handle or email."))?;
    let password = fields
        .get("password")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProviderError::invalid("Enter an app password."))?;
    let explicit = fields.get("server").map(|s| s.trim()).filter(|s| !s.is_empty());
    let login_server = match explicit {
        Some(s) => normalize_server(s)?,
        None if identifier.contains('@') => ctx.base("bsky.pds", DEFAULT_PDS),
        None => match resolve_pds(ctx, &identifier).await {
            Ok((_, pds)) => pds,
            Err(e) => {
                log::warn!("could not resolve PDS for {identifier}: {e}; using the default");
                ctx.base("bsky.pds", DEFAULT_PDS)
            }
        },
    };
    let session = create_session(ctx, &login_server, &identifier, &password).await?;
    let did = js(&session, "/did").ok_or_else(|| ProviderError::parse("Bluesky sign-in", "no did"))?;
    // Talk to the PDS that actually hosts the account from now on.
    let server = session
        .get("didDoc")
        .and_then(pds_from_doc)
        .filter(|_| explicit.is_none())
        .unwrap_or(login_server);
    let creds = session_credentials(&session, &password, &identifier);
    let mut account = NewAccount {
        platform: Platform::Bluesky,
        server: server.clone(),
        remote_id: did.clone(),
        handle: format!("@{}", js(&session, "/handle").unwrap_or_else(|| identifier.clone())),
        display_name: js(&session, "/handle").unwrap_or_else(|| identifier.clone()),
        avatar_url: None,
        profile_url: Some(format!("https://bsky.app/profile/{did}")),
        meta: json!({ "did": did }),
        credentials: creds.clone(),
    };
    let tmp = Account {
        id: String::new(),
        platform: Platform::Bluesky,
        server,
        remote_id: did,
        handle: account.handle.clone(),
        display_name: account.display_name.clone(),
        avatar_url: None,
        profile_url: None,
        meta: Value::Null,
        status: AccountStatus::Ok,
        status_message: None,
        position: 0,
        created_at: 0,
        updated_at: 0,
    };
    if let Ok(p) = profile(ctx, &tmp, &creds).await {
        if let Some(n) = p.display_name {
            account.display_name = n;
        }
        account.avatar_url = p.avatar_url;
        if let Some(h) = p.handle {
            account.handle = h;
        }
        account.profile_url = p.profile_url.or(account.profile_url);
    }
    Ok(vec![account])
}

pub async fn refresh(ctx: &Ctx, account: &Account, creds: &Credentials) -> PResult<Credentials> {
    let password = creds.extra_str("appPassword").unwrap_or_default().to_string();
    let identifier = creds
        .extra_str("identifier")
        .map(str::to_string)
        .unwrap_or_else(|| account.handle.trim_start_matches('@').to_string());
    if let Some(refresh) = &creds.refresh_token {
        let req = ctx
            .http
            .client
            .post(format!("{}/xrpc/com.atproto.server.refreshSession", account.server))
            .bearer_auth(refresh);
        match xrpc(req, "Bluesky session refresh").await {
            Ok(session) => {
                let mut new = session_credentials(&session, &password, &identifier);
                if new.refresh_token.is_none() {
                    new.refresh_token = creds.refresh_token.clone();
                }
                return Ok(new);
            }
            Err(ProviderError::Unauthorized(_)) | Err(ProviderError::Invalid(_)) => {}
            Err(e) => return Err(e),
        }
    }
    if password.is_empty() {
        return Err(ProviderError::Unauthorized(
            "The Bluesky session expired. Reconnect the account.".into(),
        ));
    }
    let session = create_session(ctx, &account.server, &identifier, &password).await?;
    Ok(session_credentials(&session, &password, &identifier))
}

pub async fn profile(ctx: &Ctx, account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    let v = xrpc(
        get(ctx, account, creds, "app.bsky.actor.getProfile").query(&[("actor", did(account))]),
        "Bluesky profile",
    )
    .await?;
    let handle = js(&v, "/handle").unwrap_or_default();
    Ok(ProfileUpdate {
        handle: Some(format!("@{handle}")),
        display_name: Some(js(&v, "/displayName").filter(|s| !s.trim().is_empty()).unwrap_or(handle.clone())),
        avatar_url: js(&v, "/avatar"),
        profile_url: Some(format!("https://bsky.app/profile/{handle}")),
        stats: ProfileStats {
            followers: ju(&v, "/followersCount"),
            following: ju(&v, "/followsCount"),
            posts: ju(&v, "/postsCount"),
        },
        meta: serde_json::Map::new(),
    })
}

// ---------------------------------------------------------------- posting

pub fn validate(_account: &Account, post: &OutgoingPost) -> Vec<String> {
    let (text, _) = text::shorten_links(&post.text);
    if text.len() > MAX_TEXT_BYTES {
        vec![format!("The text is too long for Bluesky ({} bytes, limit {MAX_TEXT_BYTES}).", text.len())]
    } else {
        Vec::new()
    }
}

/// Text with long links shortened, plus rich-text facets for links,
/// mentions and hashtags (byte offsets into the UTF-8 text).
async fn build_text(ctx: &Ctx, account: &Account, creds: &Credentials, input: &str) -> (String, Vec<Value>) {
    let (text, links) = text::shorten_links(input);
    let mut facets = Vec::new();
    let mut taken: Vec<(usize, usize)> = Vec::new();
    for (s, e, url) in links {
        facets.push(json!({
            "index": { "byteStart": s, "byteEnd": e },
            "features": [{ "$type": "app.bsky.richtext.facet#link", "uri": url }]
        }));
        taken.push((s, e));
    }
    let overlaps = |s: usize, e: usize, taken: &[(usize, usize)]| taken.iter().any(|(a, b)| s < *b && e > *a);
    for (s, e, handle) in text::find_handles(&text) {
        if overlaps(s, e, &taken) {
            continue;
        }
        let resolved = xrpc(
            get(ctx, account, creds, "com.atproto.identity.resolveHandle").query(&[("handle", handle.as_str())]),
            "handle",
        )
        .await
        .ok()
        .and_then(|v| js(&v, "/did"));
        if let Some(did) = resolved {
            facets.push(json!({
                "index": { "byteStart": s, "byteEnd": e },
                "features": [{ "$type": "app.bsky.richtext.facet#mention", "did": did }]
            }));
            taken.push((s, e));
        }
    }
    for (s, e, tag) in text::find_tags(&text) {
        if overlaps(s, e, &taken) {
            continue;
        }
        facets.push(json!({
            "index": { "byteStart": s, "byteEnd": e },
            "features": [{ "$type": "app.bsky.richtext.facet#tag", "tag": tag }]
        }));
    }
    (text, facets)
}

async fn upload_blob(ctx: &Ctx, account: &Account, creds: &Credentials, bytes: Vec<u8>, mime: &str) -> PResult<Value> {
    let v = xrpc(
        post(ctx, account, creds, "com.atproto.repo.uploadBlob")
            .header("Content-Type", mime)
            .body(bytes),
        "Bluesky upload",
    )
    .await?;
    v.get("blob").cloned().ok_or_else(|| ProviderError::parse("Bluesky upload", "no blob"))
}

/// Builds a link card for the first link in the post (best effort).
async fn link_card(ctx: &Ctx, account: &Account, creds: &Credentials, url: &str) -> Option<Value> {
    let meta = fetch_link_meta(ctx, url).await?;
    let mut external = json!({
        "uri": url,
        "title": meta.title.clone().unwrap_or_default(),
        "description": meta.description.clone().unwrap_or_default(),
    });
    if let Some(img_url) = &meta.image
        && let Some(bytes) = fetch_bytes(ctx, img_url, 8 * 1024 * 1024).await {
            let limits = crate::media::ImageLimits {
                max_bytes: 1_000_000,
                max_dimension: 2000,
                allowed: &["image/jpeg", "image/png", "image/webp"],
            };
            if let Ok(img) = crate::media::prepare_bytes(bytes, "card.jpg", &limits, "Bluesky")
                && let Ok(blob) = upload_blob(ctx, account, creds, img.bytes, &img.mime).await {
                    external["thumb"] = blob;
                }
        }
    Some(json!({ "$type": "app.bsky.embed.external", "external": external }))
}

pub(crate) struct LinkMeta {
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
}

async fn fetch_bytes(ctx: &Ctx, url: &str, max: usize) -> Option<Vec<u8>> {
    let resp = ctx.http.client.get(url).timeout(Duration::from_secs(10)).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    if resp.content_length().map(|l| l as usize > max).unwrap_or(false) {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() > max { None } else { Some(bytes.to_vec()) }
}

/// Reads the Open Graph title, description and image of a web page.
pub(crate) async fn fetch_link_meta(ctx: &Ctx, url: &str) -> Option<LinkMeta> {
    let bytes = fetch_bytes(ctx, url, 2 * 1024 * 1024).await?;
    let html = String::from_utf8_lossy(&bytes);
    let head = match html.find("</head>") {
        Some(i) => &html[..i],
        None => &html[..html.len().min(300_000)],
    };
    let meta = parse_meta(head);
    let title = meta
        .get("og:title")
        .or_else(|| meta.get("twitter:title"))
        .cloned()
        .or_else(|| {
            let re = regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").ok()?;
            re.captures(head).map(|c| text::decode_entities(c[1].trim()))
        });
    let description = meta
        .get("og:description")
        .or_else(|| meta.get("twitter:description"))
        .or_else(|| meta.get("description"))
        .cloned();
    let image = meta
        .get("og:image")
        .or_else(|| meta.get("og:image:url"))
        .or_else(|| meta.get("twitter:image"))
        .and_then(|i| url::Url::parse(url).ok()?.join(i).ok())
        .map(|u| u.to_string())
        .filter(|u| u.starts_with("http"));
    if title.is_none() && description.is_none() {
        return None;
    }
    Some(LinkMeta {
        title: title.map(|t| crate::http::truncate(&t, 300)),
        description: description.map(|d| crate::http::truncate(&d, 1000)),
        image,
    })
}

fn parse_meta(head: &str) -> HashMap<String, String> {
    let tag_re = regex::Regex::new(r"(?is)<meta\s[^>]*>").unwrap();
    let attr_re = regex::Regex::new(r#"(?is)([a-z:_-]+)\s*=\s*("([^"]*)"|'([^']*)')"#).unwrap();
    let mut out = HashMap::new();
    for tag in tag_re.find_iter(head) {
        let mut key = None;
        let mut content = None;
        for c in attr_re.captures_iter(tag.as_str()) {
            let name = c[1].to_lowercase();
            let value = c.get(3).or_else(|| c.get(4)).map(|m| m.as_str()).unwrap_or("");
            match name.as_str() {
                "property" | "name" => key = Some(value.to_lowercase()),
                "content" => content = Some(text::decode_entities(value.trim())),
                _ => {}
            }
        }
        if let (Some(k), Some(v)) = (key, content)
            && !v.is_empty() {
                out.entry(k).or_insert(v);
            }
    }
    out
}

fn rkey(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub async fn publish(ctx: &Ctx, account: &Account, creds: &Credentials, outgoing: &OutgoingPost) -> PResult<PublishResult> {
    let (text, facets) = build_text(ctx, account, creds, &outgoing.text).await;
    let mut record = json!({
        "$type": "app.bsky.feed.post",
        "text": text,
        "createdAt": now_iso(),
    });
    if !facets.is_empty() {
        record["facets"] = json!(facets);
    }
    if let Some(lang) = outgoing.options.language.as_deref().filter(|s| !s.is_empty()) {
        record["langs"] = json!([lang]);
    }
    if !outgoing.media.is_empty() {
        let mut images = Vec::new();
        for (img, alt) in super::prepare_images(account, outgoing)? {
            let (w, h) = (img.width, img.height);
            let blob = upload_blob(ctx, account, creds, img.bytes, &img.mime).await?;
            images.push(json!({ "alt": alt, "image": blob, "aspectRatio": { "width": w, "height": h } }));
        }
        record["embed"] = json!({ "$type": "app.bsky.embed.images", "images": images });
    } else if let Some((s, e)) = text::find_urls(&outgoing.text).first().copied()
        && let Some(card) = link_card(ctx, account, creds, &outgoing.text[s..e]).await {
            record["embed"] = card;
        }
    if let Some(reply) = &outgoing.reply_to {
        let (Some(uri), Some(cid)) = (js(reply, "/uri"), js(reply, "/cid")) else {
            return Err(ProviderError::invalid("The post being replied to is missing its reference."));
        };
        let root_uri = js(reply, "/rootUri").unwrap_or_else(|| uri.clone());
        let root_cid = js(reply, "/rootCid").unwrap_or_else(|| cid.clone());
        record["reply"] = json!({
            "root": { "uri": root_uri, "cid": root_cid },
            "parent": { "uri": uri, "cid": cid }
        });
    }
    let v = xrpc(
        post(ctx, account, creds, "com.atproto.repo.createRecord").json(&json!({
            "repo": did(account),
            "collection": "app.bsky.feed.post",
            "record": record,
        })),
        "Bluesky post",
    )
    .await?;
    let uri = js(&v, "/uri").ok_or_else(|| ProviderError::parse("Bluesky post", "no uri"))?;
    let handle = account.handle.trim_start_matches('@');
    Ok(PublishResult {
        url: Some(format!("https://bsky.app/profile/{handle}/post/{}", rkey(&uri))),
        remote_id: uri,
    })
}

// ---------------------------------------------------------------- reading

pub async fn stream(
    ctx: &Ctx,
    account: &Account,
    creds: &Credentials,
    kind: &str,
    param: Option<&str>,
    cursor: Option<&str>,
) -> PResult<FeedPage> {
    let mut q: Vec<(&str, String)> = vec![("limit", "40".into())];
    if let Some(c) = cursor {
        q.push(("cursor", c.to_string()));
    }
    let (method, notifications) = match kind {
        "home" => ("app.bsky.feed.getTimeline", false),
        "discover" => {
            q.push(("feed", DISCOVER_FEED.into()));
            ("app.bsky.feed.getFeed", false)
        }
        "me" => {
            q.push(("actor", did(account).to_string()));
            q.push(("filter", "posts_and_author_threads".into()));
            ("app.bsky.feed.getAuthorFeed", false)
        }
        "search" => {
            let term = param.unwrap_or_default().trim();
            if term.is_empty() {
                return Err(ProviderError::invalid("Enter something to search for."));
            }
            q.push(("q", term.to_string()));
            q.push(("sort", "latest".into()));
            ("app.bsky.feed.searchPosts", false)
        }
        "notifications" => ("app.bsky.notification.listNotifications", true),
        "mentions" => {
            for r in ["mention", "reply", "quote"] {
                q.push(("reasons", r.into()));
            }
            ("app.bsky.notification.listNotifications", true)
        }
        other => return Err(ProviderError::unsupported(format!("Unknown stream {other}"))),
    };
    let v = xrpc(get(ctx, account, creds, method).query(&q), "Bluesky feed").await?;
    let cursor = js(&v, "/cursor");
    let own = did(account);
    let items = if notifications {
        let list = v.get("notifications").and_then(Value::as_array).cloned().unwrap_or_default();
        let subjects = subject_texts(ctx, account, creds, &list).await;
        list.iter().map(|n| notification_item(n, &subjects)).collect()
    } else if kind == "search" {
        v.get("posts")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|p| post_item(p, None, None, own)).collect())
            .unwrap_or_default()
    } else {
        v.get("feed")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|f| feed_item(f, own)).collect())
            .unwrap_or_default()
    };
    Ok(FeedPage { items, cursor })
}

/// Texts of the posts that likes and reposts refer to.
async fn subject_texts(ctx: &Ctx, account: &Account, creds: &Credentials, list: &[Value]) -> HashMap<String, String> {
    let mut uris: Vec<String> = list
        .iter()
        .filter(|n| matches!(js(n, "/reason").as_deref(), Some("like" | "repost" | "like-via-repost" | "repost-via-repost")))
        .filter_map(|n| js(n, "/reasonSubject"))
        .filter(|u| u.contains("/app.bsky.feed.post/"))
        .collect();
    uris.sort();
    uris.dedup();
    let mut out = HashMap::new();
    for chunk in uris.chunks(25) {
        let q: Vec<(&str, &str)> = chunk.iter().map(|u| ("uris", u.as_str())).collect();
        if let Ok(v) = xrpc(get(ctx, account, creds, "app.bsky.feed.getPosts").query(&q), "Bluesky posts").await {
            for p in v.get("posts").and_then(Value::as_array).into_iter().flatten() {
                if let (Some(uri), Some(t)) = (js(p, "/uri"), js(p, "/record/text")) {
                    out.insert(uri, t);
                }
            }
        }
    }
    out
}

fn author(a: &Value) -> Author {
    let handle = js(a, "/handle").unwrap_or_default();
    Author {
        name: js(a, "/displayName").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| handle.clone()),
        avatar: js(a, "/avatar"),
        url: Some(format!("https://bsky.app/profile/{handle}")),
        handle: format!("@{handle}"),
    }
}

fn post_url(uri: &str, handle: &str) -> String {
    format!("https://bsky.app/profile/{handle}/post/{}", rkey(uri))
}

/// Text with facets turned into HTML links.
fn rich_html(text: &str, facets: Option<&Value>) -> Option<String> {
    let facets = facets.and_then(Value::as_array).filter(|f| !f.is_empty());
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize, String)> = facets
        .map(|list| {
            list.iter()
                .filter_map(|f| {
                    let s = ju(f, "/index/byteStart")? as usize;
                    let e = ju(f, "/index/byteEnd")? as usize;
                    if s >= e || e > bytes.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
                        return None;
                    }
                    let feat = f.pointer("/features/0")?;
                    let href = match js(feat, "/$type")?.as_str() {
                        "app.bsky.richtext.facet#link" => js(feat, "/uri")?,
                        "app.bsky.richtext.facet#mention" => format!("https://bsky.app/profile/{}", js(feat, "/did")?),
                        "app.bsky.richtext.facet#tag" => {
                            format!("https://bsky.app/hashtag/{}", url::form_urlencoded::byte_serialize(js(feat, "/tag")?.as_bytes()).collect::<String>())
                        }
                        _ => return None,
                    };
                    Some((s, e, href))
                })
                .collect()
        })
        .unwrap_or_default();
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
    Some(format!("<p>{}</p>", html.replace('\n', "<br>")))
}

fn embed_parts(embed: Option<&Value>, own: &str) -> (Vec<MediaView>, Option<LinkCard>, Option<Box<FeedItem>>) {
    let Some(embed) = embed else { return (vec![], None, None) };
    let t = js(embed, "/$type").unwrap_or_default();
    match t.as_str() {
        "app.bsky.embed.images#view" => (
            embed
                .get("images")
                .and_then(Value::as_array)
                .map(|imgs| {
                    imgs.iter()
                        .map(|i| MediaView {
                            kind: "image".into(),
                            url: js(i, "/fullsize").unwrap_or_default(),
                            preview_url: js(i, "/thumb"),
                            alt: js(i, "/alt").filter(|a| !a.is_empty()),
                            width: ju(i, "/aspectRatio/width").map(|v| v as u32),
                            height: ju(i, "/aspectRatio/height").map(|v| v as u32),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            None,
            None,
        ),
        "app.bsky.embed.video#view" => (
            vec![MediaView {
                kind: "video".into(),
                url: js(embed, "/playlist").unwrap_or_default(),
                preview_url: js(embed, "/thumbnail"),
                alt: js(embed, "/alt"),
                width: ju(embed, "/aspectRatio/width").map(|v| v as u32),
                height: ju(embed, "/aspectRatio/height").map(|v| v as u32),
            }],
            None,
            None,
        ),
        "app.bsky.embed.external#view" => (
            vec![],
            embed.get("external").map(|x| LinkCard {
                url: js(x, "/uri").unwrap_or_default(),
                title: js(x, "/title").filter(|s| !s.is_empty()),
                description: js(x, "/description").filter(|s| !s.is_empty()),
                image: js(x, "/thumb"),
            }),
            None,
        ),
        "app.bsky.embed.record#view" => (vec![], None, quoted(embed.get("record"), own)),
        "app.bsky.embed.recordWithMedia#view" => {
            let (media, link, _) = embed_parts(embed.get("media"), own);
            (media, link, quoted(embed.pointer("/record/record"), own))
        }
        _ => (vec![], None, None),
    }
}

fn quoted(rec: Option<&Value>, own: &str) -> Option<Box<FeedItem>> {
    let rec = rec?;
    if js(rec, "/$type").as_deref() != Some("app.bsky.embed.record#viewRecord") {
        return None;
    }
    let uri = js(rec, "/uri")?;
    let a = author(&rec["author"]);
    let text = js(rec, "/value/text").unwrap_or_default();
    let (media, link, _) = embed_parts(rec.get("embeds").and_then(|e| e.get(0)), own);
    Some(Box::new(FeedItem {
        id: uri.clone(),
        url: Some(post_url(&uri, a.handle.trim_start_matches('@'))),
        created_at: js(rec, "/value/createdAt").or_else(|| js(rec, "/indexedAt")).unwrap_or_default(),
        html: rich_html(&text, rec.pointer("/value/facets")),
        text,
        author: a,
        media,
        link,
        target: json!({ "uri": uri, "cid": js(rec, "/cid") }),
        ..Default::default()
    }))
}

fn post_item(p: &Value, reposted_by: Option<Author>, reply_parent: Option<&Value>, own: &str) -> FeedItem {
    let uri = js(p, "/uri").unwrap_or_default();
    let a = author(&p["author"]);
    let record = &p["record"];
    let text = js(record, "/text").unwrap_or_default();
    let (media, link, quote) = embed_parts(p.get("embed"), own);
    let is_reply = record.get("reply").is_some();
    let context = if is_reply {
        Some(match reply_parent.and_then(|r| js(r, "/author/handle")) {
            Some(h) => format!("Replying to @{h}"),
            None => "Reply".to_string(),
        })
    } else {
        None
    };
    let labels = p.get("labels").and_then(Value::as_array);
    let sensitive = labels
        .map(|l| {
            l.iter()
                .any(|x| matches!(js(x, "/val").as_deref(), Some("porn" | "sexual" | "nudity" | "graphic-media" | "gore")))
        })
        .unwrap_or(false);
    FeedItem {
        id: uri.clone(),
        account_id: String::new(),
        url: Some(post_url(&uri, a.handle.trim_start_matches('@'))),
        created_at: js(record, "/createdAt").or_else(|| js(p, "/indexedAt")).unwrap_or_default(),
        html: rich_html(&text, record.get("facets")),
        text,
        title: None,
        content_warning: if sensitive { Some("Sensitive content".into()) } else { None },
        sensitive,
        media,
        link,
        quote,
        reposted_by,
        context,
        counts: Counts {
            replies: ju(p, "/replyCount"),
            reposts: ju(p, "/repostCount"),
            likes: ju(p, "/likeCount"),
            quotes: ju(p, "/quoteCount"),
            score: None,
        },
        viewer: ViewerState {
            liked: p.pointer("/viewer/like").is_some(),
            reposted: p.pointer("/viewer/repost").is_some(),
        },
        actions: ItemActions {
            like: true,
            repost: true,
            reply: !p.pointer("/viewer/replyDisabled").and_then(Value::as_bool).unwrap_or(false),
            delete: js(p, "/author/did").as_deref() == Some(own),
        },
        target: json!({
            "uri": uri,
            "cid": js(p, "/cid"),
            "rootUri": js(record, "/reply/root/uri"),
            "rootCid": js(record, "/reply/root/cid"),
            "likeUri": js(p, "/viewer/like"),
            "repostUri": js(p, "/viewer/repost"),
        }),
        author: a,
        notification: None,
    }
}

fn feed_item(f: &Value, own: &str) -> FeedItem {
    let reason = f.get("reason");
    let reposted_by = reason
        .filter(|r| js(r, "/$type").as_deref() == Some("app.bsky.feed.defs#reasonRepost"))
        .map(|r| author(&r["by"]));
    let mut item = post_item(&f["post"], reposted_by.clone(), f.pointer("/reply/parent"), own);
    if let Some(by) = &reposted_by {
        item.id = format!("{}#repost:{}", item.id, by.handle);
    }
    if reason.map(|r| js(r, "/$type").as_deref() == Some("app.bsky.feed.defs#reasonPin")).unwrap_or(false) {
        item.context = Some("Pinned".into());
    }
    item
}

fn notification_item(n: &Value, subjects: &HashMap<String, String>) -> FeedItem {
    let reason = js(n, "/reason").unwrap_or_default();
    let (kind, label) = match reason.as_str() {
        "like" => (NotificationKind::Like, "liked your post"),
        "like-via-repost" => (NotificationKind::Like, "liked your repost"),
        "repost" => (NotificationKind::Repost, "reposted your post"),
        "repost-via-repost" => (NotificationKind::Repost, "reposted your repost"),
        "follow" => (NotificationKind::Follow, "followed you"),
        "mention" => (NotificationKind::Mention, "mentioned you"),
        "reply" => (NotificationKind::Reply, "replied to you"),
        "quote" => (NotificationKind::Quote, "quoted your post"),
        "starterpack-joined" => (NotificationKind::Follow, "joined via your starter pack"),
        "verified" => (NotificationKind::Other, "verified your account"),
        "unverified" => (NotificationKind::Other, "removed your verification"),
        "subscribed-post" => (NotificationKind::Other, "posted"),
        _ => (NotificationKind::Other, "sent a notification"),
    };
    let a = author(&n["author"]);
    let uri = js(n, "/uri").unwrap_or_default();
    let is_post = matches!(kind, NotificationKind::Mention | NotificationKind::Reply | NotificationKind::Quote)
        || reason == "subscribed-post";
    let record = &n["record"];
    let text = if is_post { js(record, "/text").unwrap_or_default() } else { String::new() };
    let subject = js(n, "/reasonSubject").and_then(|s| subjects.get(&s).cloned()).map(|t| text::preview(&t, 280));
    FeedItem {
        id: format!("{uri}#{reason}"),
        account_id: String::new(),
        url: Some(if is_post {
            post_url(&uri, a.handle.trim_start_matches('@'))
        } else if let Some(subj) = js(n, "/reasonSubject").filter(|s| s.contains("/app.bsky.feed.post/")) {
            // The subject is one of our own posts: link to it via its DID.
            let did = subj.trim_start_matches("at://").split('/').next().unwrap_or_default().to_string();
            post_url(&subj, &did)
        } else {
            a.url.clone().unwrap_or_default()
        }),
        created_at: js(n, "/indexedAt").or_else(|| js(record, "/createdAt")).unwrap_or_default(),
        html: if is_post { rich_html(&text, record.get("facets")) } else { None },
        text,
        actions: ItemActions { like: is_post, repost: is_post, reply: is_post, delete: false },
        target: if is_post {
            json!({
                "uri": uri,
                "cid": js(n, "/cid"),
                "rootUri": js(record, "/reply/root/uri"),
                "rootCid": js(record, "/reply/root/cid"),
            })
        } else {
            Value::Null
        },
        author: a,
        notification: Some(NotificationInfo { kind, label: label.into(), subject }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- actions

pub async fn interact(ctx: &Ctx, account: &Account, creds: &Credentials, target: &Value, action: Interaction) -> PResult<Value> {
    let mut target = target.clone();
    let (collection, key) = match action {
        Interaction::Like | Interaction::Unlike => ("app.bsky.feed.like", "likeUri"),
        Interaction::Repost | Interaction::Unrepost => ("app.bsky.feed.repost", "repostUri"),
    };
    match action {
        Interaction::Like | Interaction::Repost => {
            let (Some(uri), Some(cid)) = (js(&target, "/uri"), js(&target, "/cid")) else {
                return Err(ProviderError::invalid("This post can't be acted on."));
            };
            let v = xrpc(
                post(ctx, account, creds, "com.atproto.repo.createRecord").json(&json!({
                    "repo": did(account),
                    "collection": collection,
                    "record": { "$type": collection, "subject": { "uri": uri, "cid": cid }, "createdAt": now_iso() }
                })),
                "Bluesky action",
            )
            .await?;
            target[key] = json!(js(&v, "/uri"));
        }
        Interaction::Unlike | Interaction::Unrepost => {
            let Some(record_uri) = js(&target, &format!("/{key}")) else {
                return Err(ProviderError::invalid("Refresh the stream and try again."));
            };
            xrpc(
                post(ctx, account, creds, "com.atproto.repo.deleteRecord").json(&json!({
                    "repo": did(account),
                    "collection": collection,
                    "rkey": rkey(&record_uri),
                })),
                "Bluesky action",
            )
            .await?;
            target[key] = Value::Null;
        }
    }
    Ok(target)
}

pub async fn delete(ctx: &Ctx, account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    xrpc(
        post(ctx, account, creds, "com.atproto.repo.deleteRecord").json(&json!({
            "repo": did(account),
            "collection": "app.bsky.feed.post",
            "rkey": rkey(remote_id),
        })),
        "Bluesky delete",
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn jwt(exp: i64) -> String {
        let enc = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        format!("{}.{}.sig", enc(json!({"alg": "ES256K"})), enc(json!({"exp": exp, "sub": "did:plc:me"})))
    }

    fn account(server: &str) -> Account {
        Account {
            id: "a".into(),
            platform: Platform::Bluesky,
            server: server.into(),
            remote_id: "did:plc:me".into(),
            handle: "@me.test".into(),
            display_name: "Me".into(),
            avatar_url: None,
            profile_url: None,
            meta: json!({}),
            status: AccountStatus::Ok,
            status_message: None,
            position: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn connects_with_app_password() {
        let pds = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.identity.resolveHandle"))
            .and(query_param("handle", "me.test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"did": "did:plc:me"})))
            .mount(&pds)
            .await;
        Mock::given(method("GET"))
            .and(path("/did:plc:me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "did:plc:me",
                "service": [{"id": "#atproto_pds", "type": "AtprotoPersonalDataServer", "serviceEndpoint": pds.uri()}]
            })))
            .mount(&pds)
            .await;
        let access = jwt(4_000_000_000);
        Mock::given(method("POST"))
            .and(path("/xrpc/com.atproto.server.createSession"))
            .and(body_string_contains("\"password\":\"abcd-efgh-ijkl-mnop\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "did": "did:plc:me", "handle": "me.test", "accessJwt": access, "refreshJwt": "refresh-1"
            })))
            .mount(&pds)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/app.bsky.actor.getProfile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "did": "did:plc:me", "handle": "me.test", "displayName": "Me!", "avatar": "https://cdn/a.jpg",
                "followersCount": 10, "followsCount": 2, "postsCount": 33
            })))
            .mount(&pds)
            .await;
        let ctx = Ctx::default().with_override("bsky.appview", &pds.uri()).with_override("bsky.plc", &pds.uri());
        let fields = HashMap::from([
            ("identifier".to_string(), "@me.test".to_string()),
            ("password".to_string(), "abcd-efgh-ijkl-mnop".to_string()),
        ]);
        let accounts = connect(&ctx, &fields).await.unwrap();
        let a = &accounts[0];
        assert_eq!(a.server, pds.uri());
        assert_eq!(a.remote_id, "did:plc:me");
        assert_eq!(a.display_name, "Me!");
        assert_eq!(a.handle, "@me.test");
        assert_eq!(a.credentials.expires_at, Some(4_000_000_000));
        assert_eq!(a.credentials.extra_str("appPassword"), Some("abcd-efgh-ijkl-mnop"));
    }

    #[tokio::test]
    async fn expired_token_maps_to_unauthorized_and_refreshes() {
        let pds = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/app.bsky.actor.getProfile"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "ExpiredToken", "message": "Token has expired"})))
            .mount(&pds)
            .await;
        Mock::given(method("POST"))
            .and(path("/xrpc/com.atproto.server.refreshSession"))
            .and(header("authorization", "Bearer refresh-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "did": "did:plc:me", "handle": "me.test", "accessJwt": jwt(5_000_000_000), "refreshJwt": "refresh-2"
            })))
            .mount(&pds)
            .await;
        let acc = account(&pds.uri());
        let mut creds = Credentials::bearer("old");
        creds.refresh_token = Some("refresh-1".into());
        let err = profile(&Ctx::default(), &acc, &creds).await.unwrap_err();
        assert!(matches!(err, ProviderError::Unauthorized(_)), "{err:?}");
        let new = refresh(&Ctx::default(), &acc, &creds).await.unwrap();
        assert_eq!(new.refresh_token.as_deref(), Some("refresh-2"));
        assert_eq!(new.expires_at, Some(5_000_000_000));
    }

    #[tokio::test]
    async fn publishes_with_facets_reply_and_images() {
        let pds = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/xrpc/com.atproto.identity.resolveHandle"))
            .and(query_param("handle", "alice.test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"did": "did:plc:alice"})))
            .mount(&pds)
            .await;
        Mock::given(method("POST"))
            .and(path("/xrpc/com.atproto.repo.uploadBlob"))
            .and(header("content-type", "image/png"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "blob": {"$type": "blob", "ref": {"$link": "bafk"}, "mimeType": "image/png", "size": 100}
            })))
            .expect(1)
            .mount(&pds)
            .await;
        Mock::given(method("POST"))
            .and(path("/xrpc/com.atproto.repo.createRecord"))
            .respond_with(|req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let rec = &body["record"];
                assert_eq!(body["repo"], json!("did:plc:me"));
                assert_eq!(rec["text"], json!("Hi @alice.test see example.com/a/really/long/p... #rust"));
                let facets = rec["facets"].as_array().unwrap();
                let types: Vec<String> = facets.iter().map(|f| js(f, "/features/0/$type").unwrap()).collect();
                assert_eq!(types, vec![
                    "app.bsky.richtext.facet#link",
                    "app.bsky.richtext.facet#mention",
                    "app.bsky.richtext.facet#tag"
                ]);
                let link = &facets[0];
                assert_eq!(link["features"][0]["uri"], json!("https://example.com/a/really/long/path/that/goes/on"));
                let text = rec["text"].as_str().unwrap();
                let (s, e) = (ju(link, "/index/byteStart").unwrap() as usize, ju(link, "/index/byteEnd").unwrap() as usize);
                assert_eq!(&text[s..e], "example.com/a/really/long/p...");
                assert_eq!(rec["embed"]["images"][0]["alt"], json!("alt text"));
                assert_eq!(rec["embed"]["images"][0]["aspectRatio"], json!({"width": 8, "height": 6}));
                assert_eq!(rec["reply"]["root"]["uri"], json!("at://did:plc:x/app.bsky.feed.post/root"));
                assert_eq!(rec["reply"]["parent"]["uri"], json!("at://did:plc:x/app.bsky.feed.post/parent"));
                ResponseTemplate::new(200).set_body_json(json!({"uri": "at://did:plc:me/app.bsky.feed.post/3abc", "cid": "c"}))
            })
            .expect(1)
            .mount(&pds)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 6, image::Rgb([1, 2, 3])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let mut att = crate::media::import_bytes(dir.path(), &png, "p.png").unwrap();
        att.alt = "alt text".into();
        let outgoing = OutgoingPost {
            text: "Hi @alice.test see https://example.com/a/really/long/path/that/goes/on #rust".into(),
            media: vec![att],
            options: PostOptions::default(),
            reply_to: Some(json!({
                "uri": "at://did:plc:x/app.bsky.feed.post/parent", "cid": "pc",
                "rootUri": "at://did:plc:x/app.bsky.feed.post/root", "rootCid": "rc"
            })),
            idempotency_key: "k".into(),
        };
        let res = publish(&Ctx::default(), &account(&pds.uri()), &Credentials::bearer("t"), &outgoing).await.unwrap();
        assert_eq!(res.remote_id, "at://did:plc:me/app.bsky.feed.post/3abc");
        assert_eq!(res.url.as_deref(), Some("https://bsky.app/profile/me.test/post/3abc"));
    }

    #[tokio::test]
    async fn maps_timeline_and_notifications() {
        let pds = MockServer::start().await;
        let post_view = |uri: &str, text: &str| {
            json!({
                "uri": uri, "cid": "cid1",
                "author": {"did": "did:plc:bob", "handle": "bob.test", "displayName": "Bob", "avatar": "https://cdn/b.jpg"},
                "record": {"$type": "app.bsky.feed.post", "text": text, "createdAt": "2026-11-01T00:00:00.000Z",
                           "facets": [{"index": {"byteStart": 0, "byteEnd": 5}, "features": [{"$type": "app.bsky.richtext.facet#tag", "tag": "rust"}]}]},
                "embed": {"$type": "app.bsky.embed.images#view", "images": [{"thumb": "https://cdn/t.jpg", "fullsize": "https://cdn/f.jpg", "alt": "pic"}]},
                "replyCount": 1, "repostCount": 2, "likeCount": 3, "quoteCount": 0,
                "indexedAt": "2026-11-01T00:00:01.000Z",
                "viewer": {"like": "at://did:plc:me/app.bsky.feed.like/l1"}
            })
        };
        Mock::given(method("GET"))
            .and(path("/xrpc/app.bsky.feed.getTimeline"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "cursor": "next",
                "feed": [
                    {"post": post_view("at://did:plc:bob/app.bsky.feed.post/p1", "#rust <3")},
                    {"post": post_view("at://did:plc:bob/app.bsky.feed.post/p2", "boosted"),
                     "reason": {"$type": "app.bsky.feed.defs#reasonRepost", "by": {"did": "did:plc:c", "handle": "carol.test"}}}
                ]
            })))
            .mount(&pds)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/app.bsky.notification.listNotifications"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "notifications": [
                    {"uri": "at://did:plc:bob/app.bsky.feed.like/x", "cid": "c", "reason": "like",
                     "reasonSubject": "at://did:plc:me/app.bsky.feed.post/mine", "isRead": false,
                     "author": {"did": "did:plc:bob", "handle": "bob.test"}, "record": {}, "indexedAt": "2026-11-02T00:00:00Z"},
                    {"uri": "at://did:plc:bob/app.bsky.feed.post/r", "cid": "c2", "reason": "reply", "isRead": false,
                     "author": {"did": "did:plc:bob", "handle": "bob.test"},
                     "record": {"text": "nice", "reply": {"root": {"uri": "at://root", "cid": "rc"}, "parent": {"uri": "at://p", "cid": "pc"}}},
                     "indexedAt": "2026-11-02T00:00:01Z"}
                ]
            })))
            .mount(&pds)
            .await;
        Mock::given(method("GET"))
            .and(path("/xrpc/app.bsky.feed.getPosts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "posts": [{"uri": "at://did:plc:me/app.bsky.feed.post/mine", "record": {"text": "my great post"}}]
            })))
            .mount(&pds)
            .await;
        let acc = account(&pds.uri());
        let ctx = Ctx::default();
        let creds = Credentials::bearer("t");
        let page = stream(&ctx, &acc, &creds, "home", None, None).await.unwrap();
        assert_eq!(page.cursor.as_deref(), Some("next"));
        let first = &page.items[0];
        assert_eq!(first.url.as_deref(), Some("https://bsky.app/profile/bob.test/post/p1"));
        assert_eq!(first.html.as_deref(), Some("<p><a href=\"https://bsky.app/hashtag/rust\">#rust</a> &lt;3</p>"));
        assert!(first.viewer.liked);
        assert_eq!(first.media[0].alt.as_deref(), Some("pic"));
        assert_eq!(first.target["likeUri"], json!("at://did:plc:me/app.bsky.feed.like/l1"));
        assert_eq!(page.items[1].reposted_by.as_ref().unwrap().handle, "@carol.test");

        let notes = stream(&ctx, &acc, &creds, "notifications", None, None).await.unwrap();
        let like = notes.items[0].notification.as_ref().unwrap();
        assert_eq!(like.kind, NotificationKind::Like);
        assert_eq!(like.subject.as_deref(), Some("my great post"));
        let reply = &notes.items[1];
        assert_eq!(reply.text, "nice");
        assert!(reply.actions.reply);
        assert_eq!(reply.target["rootUri"], json!("at://root"));
    }

    #[test]
    fn parses_open_graph() {
        let html = r#"<html><head><title>Fallback</title>
            <meta property="og:title" content="The &amp; Title">
            <meta content="Desc here" name="description">
            <meta property='og:image' content='/img.png'></head>"#;
        let m = parse_meta(html);
        assert_eq!(m.get("og:title").map(String::as_str), Some("The & Title"));
        assert_eq!(m.get("description").map(String::as_str), Some("Desc here"));
        assert_eq!(m.get("og:image").map(String::as_str), Some("/img.png"));
    }

    /// Talks to the real public Bluesky services (read-only). Run with
    /// `cargo test -- --ignored live_`.
    #[tokio::test]
    #[ignore]
    async fn live_resolves_pds_and_maps_real_feed() {
        let ctx = Ctx::default();
        let (did, pds) = resolve_pds(&ctx, "bsky.app").await.unwrap();
        println!("bsky.app -> {did} on {pds}");
        assert!(did.starts_with("did:plc:"));
        assert!(pds.starts_with("https://"));
        let v = xrpc(
            ctx.http
                .client
                .get(format!("{APPVIEW}/xrpc/app.bsky.feed.getAuthorFeed"))
                .query(&[("actor", "bsky.app"), ("limit", "10")]),
            "feed",
        )
        .await
        .unwrap();
        let items: Vec<FeedItem> = v["feed"].as_array().unwrap().iter().map(|f| feed_item(f, &did)).collect();
        assert!(!items.is_empty());
        for i in &items {
            assert!(i.url.as_deref().unwrap().starts_with("https://bsky.app/profile/"));
            assert!(!i.author.handle.is_empty());
            println!("{} {} | {} | media {} | likes {:?}", i.created_at, i.author.handle, crate::text::preview(&i.text, 60), i.media.len(), i.counts.likes);
        }
    }
}
