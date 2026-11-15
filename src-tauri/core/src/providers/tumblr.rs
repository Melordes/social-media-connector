//! Tumblr, through the user's own Tumblr app (OAuth 2).
//!
//! Docs: <https://github.com/tumblr/docs/blob/master/api.md> and the Neue Post
//! Format spec <https://github.com/tumblr/docs/blob/master/npf-spec.md>.
//!
//! Sign-in: authorization code flow. The browser goes to
//! `www.tumblr.com/oauth2/authorize` with the scopes
//! `basic write offline_access`; the code is exchanged at
//! `api.tumblr.com/v2/oauth2/token` with the app's consumer key and secret.
//! Access tokens are short-lived (`expires_in` is 2520 seconds in the docs);
//! the refresh grant returns a new access token *and* a new refresh token,
//! and both are stored.
//!
//! The account is the user's primary blog. Its UUID (`t:...`) is the remote
//! id because, unlike the blog name, it survives renames, and the API accepts
//! it wherever it takes a blog identifier. The name and UUID are kept in
//! `meta` (`blog`, `blogUuid`) for posting and links.
//!
//! Answers are wrapped as `{"meta": {...}, "response": ...}`; errors carry
//! `errors: [{"title", "code", "detail"}]`. Every request uses the client's
//! single, consistent User-Agent, as Tumblr requires.

use std::collections::{HashMap, HashSet};

use reqwest::{RequestBuilder, StatusCode};
use serde_json::{Value, json};

use super::{Ctx, OAuthBegin, OAuthStart, expect_code, expect_state, js, ju};
use crate::error::{PResult, ProviderError};
use crate::http;
use crate::model::*;
use crate::text::{self, decode_entities, escape_html};

const WWW: &str = "https://www.tumblr.com";
const API: &str = "https://api.tumblr.com";
/// Links shown to the user (never overridden in tests).
const SITE: &str = "https://www.tumblr.com";
const PUBLIC_API: &str = "https://api.tumblr.com";
const SCOPES: &str = "basic write offline_access";
const PAGE_SIZE: usize = 20;
/// NPF limits (npf-spec.md, "Post Content Limits").
const BLOCK_MAX_CHARS: usize = 4096;
const MAX_TEXT_BLOCKS: usize = 1000;
const MAX_INLINE_LINKS: usize = 100;

// ------------------------------------------------------------------ http

fn www(ctx: &Ctx) -> String {
    ctx.base("tumblr.www", WWW)
}

fn api(ctx: &Ctx) -> String {
    ctx.base("tumblr.api", API)
}

fn get(ctx: &Ctx, creds: &Credentials, path: &str) -> RequestBuilder {
    ctx.http.client.get(format!("{}{path}", api(ctx))).bearer_auth(creds.token())
}

fn post(ctx: &Ctx, creds: &Credentials, path: &str) -> RequestBuilder {
    ctx.http.client.post(format!("{}{path}", api(ctx))).bearer_auth(creds.token())
}

/// Sends a request and returns the `response` part of the answer.
async fn call(req: RequestBuilder, what: &str) -> PResult<Value> {
    let resp = http::send(req).await?;
    let status = resp.status();
    let retry = http::retry_after_secs(&resp);
    let bytes = resp.bytes().await.map_err(ProviderError::from)?;
    if !status.is_success() {
        return Err(error_for(status, &String::from_utf8_lossy(&bytes), retry));
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    match serde_json::from_slice(&bytes).map_err(|e| ProviderError::parse(what, e))? {
        Value::Object(mut m) => Ok(m.remove("response").unwrap_or(Value::Object(m))),
        other => Ok(other),
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

/// Maps Tumblr's error bodies to errors with a readable message.
fn error_for(status: StatusCode, body: &str, retry: Option<u64>) -> ProviderError {
    let v: Option<Value> = serde_json::from_str(body).ok();
    let first = v.as_ref().and_then(|v| v.pointer("/errors/0"));
    let code = first.and_then(|e| ju(e, "/code"));
    let detail = first
        .and_then(|e| non_empty(js(e, "/detail")).or_else(|| non_empty(js(e, "/title"))))
        .or_else(|| v.as_ref().and_then(|v| non_empty(js(v, "/meta/msg"))));
    // Daily upload and posting limits are 403s with these subcodes.
    if status == StatusCode::FORBIDDEN && matches!(code, Some(8004 | 8011 | 8023)) {
        return ProviderError::RateLimited {
            message: detail.unwrap_or_else(|| "Tumblr's daily limit for this account was reached.".into()),
            retry_after: None,
        };
    }
    if status == StatusCode::UNAUTHORIZED {
        return ProviderError::Unauthorized("Tumblr did not accept the saved sign-in.".into());
    }
    let body = match detail {
        Some(d) => json!({ "message": d }).to_string(),
        None => body.to_string(),
    };
    http::status_error(status, &body, retry)
}

/// Requests a token. A successful answer holds the tokens, so it is never
/// logged or echoed.
async fn token_request(ctx: &Ctx, form: &[(&str, &str)]) -> PResult<Value> {
    let req = ctx.http.client.post(format!("{}/v2/oauth2/token", api(ctx))).form(form);
    let resp = http::send(req).await?;
    let status = resp.status();
    let retry = http::retry_after_secs(&resp);
    let body = resp.text().await.map_err(ProviderError::from)?;
    if status.is_success() {
        let v: Value =
            serde_json::from_str(&body).map_err(|_| ProviderError::parse("Tumblr token", "the answer is not JSON"))?;
        // Accept the token object bare (as documented) or wrapped in `response`.
        let v = match v.get("response").filter(|r| r.get("access_token").is_some()) {
            Some(r) => r.clone(),
            None => v,
        };
        if non_empty(js(&v, "/access_token")).is_none() {
            return Err(ProviderError::parse("Tumblr token", "no access_token"));
        }
        return Ok(v);
    }
    Err(error_for(status, &body, retry))
}

fn credentials_from(token: &Value, client_id: &str, client_secret: &str, previous: Option<&Credentials>) -> PResult<Credentials> {
    let access = non_empty(js(token, "/access_token")).ok_or_else(|| ProviderError::parse("Tumblr token", "no access_token"))?;
    Ok(Credentials {
        access_token: Some(access),
        refresh_token: non_empty(js(token, "/refresh_token")).or_else(|| previous.and_then(|p| p.refresh_token.clone())),
        expires_at: ju(token, "/expires_in").map(|s| now_secs() + s as i64),
        client_id: Some(client_id.to_string()),
        client_secret: Some(client_secret.to_string()),
        extra: previous.map(|p| p.extra.clone()).unwrap_or_default(),
    })
}

// --------------------------------------------------------------- sign-in

pub async fn begin(ctx: &Ctx, b: &OAuthBegin) -> PResult<OAuthStart> {
    let client_id = b.require_client_id()?;
    b.require_client_secret()?;
    let auth_url = crate::oauth::with_query(
        &format!("{}/oauth2/authorize", www(ctx)),
        &[
            ("client_id", client_id),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("state", &b.state),
            // Required when the app has several redirect URLs; always sent so
            // the token request can repeat it.
            ("redirect_uri", &b.redirect_uri),
        ],
    );
    Ok(OAuthStart { auth_url, flow: json!({}) })
}

pub async fn finish(ctx: &Ctx, b: &OAuthBegin, _flow: &Value, params: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    expect_state(params, &b.state)?;
    let code = expect_code(params)?;
    let client_id = b.require_client_id()?;
    let client_secret = b.require_client_secret()?;
    let token = match token_request(
        ctx,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", &b.redirect_uri),
        ],
    )
    .await
    {
        Err(ProviderError::Unauthorized(_)) => {
            return Err(ProviderError::Unauthorized(
                "Tumblr did not accept the sign-in. Check the app's consumer key and secret in Settings → Developer apps, then try again.".into(),
            ));
        }
        other => other?,
    };
    let creds = credentials_from(&token, client_id, client_secret, None)?;
    let info = call(get(ctx, &creds, "/v2/user/info"), "Tumblr account").await?;
    let blog = pick_blog(&info, None, None).ok_or_else(|| ProviderError::invalid("This Tumblr account has no blog to post to."))?;
    let b = BlogInfo::from(blog);
    if b.name.is_empty() {
        return Err(ProviderError::parse("Tumblr account", "the blog has no name"));
    }
    Ok(vec![NewAccount {
        platform: Platform::Tumblr,
        server: String::new(),
        remote_id: b.uuid.clone().unwrap_or_else(|| b.name.clone()),
        handle: b.name.clone(),
        display_name: b.display_name(),
        avatar_url: avatar_of(Some(blog), &b.name),
        profile_url: Some(b.profile_url()),
        meta: Value::Object(b.meta()),
        credentials: creds,
    }])
}

pub async fn refresh(ctx: &Ctx, _account: &Account, creds: &Credentials) -> PResult<Credentials> {
    let reconnect = || ProviderError::Unauthorized("The Tumblr sign-in has expired. Reconnect the account.".into());
    let refresh_token = creds.refresh_token.as_deref().filter(|t| !t.is_empty()).ok_or_else(reconnect)?;
    let client_id = creds.client_id.as_deref().filter(|c| !c.is_empty()).ok_or_else(reconnect)?;
    let client_secret = creds.client_secret.as_deref().filter(|c| !c.is_empty()).ok_or_else(reconnect)?;
    let token = match token_request(
        ctx,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
        ],
    )
    .await
    {
        Ok(t) => t,
        // 401 means the refresh token expired; 400 that it is unknown.
        Err(ProviderError::Unauthorized(_)) | Err(ProviderError::Invalid(_)) => return Err(reconnect()),
        Err(e) => return Err(e),
    };
    credentials_from(&token, client_id, client_secret, Some(creds))
}

/// The blog to use: the one matching `uuid` or `name` when known, else the
/// primary blog.
fn pick_blog<'a>(info: &'a Value, uuid: Option<&str>, name: Option<&str>) -> Option<&'a Value> {
    let blogs = info.pointer("/user/blogs")?.as_array()?;
    uuid.and_then(|u| blogs.iter().find(|b| js(b, "/uuid").as_deref() == Some(u)))
        .or_else(|| name.and_then(|n| blogs.iter().find(|b| js(b, "/name").as_deref() == Some(n))))
        .or_else(|| blogs.iter().find(|b| b.get("primary").and_then(Value::as_bool).unwrap_or(false)))
        .or_else(|| blogs.first())
}

struct BlogInfo {
    name: String,
    uuid: Option<String>,
    title: Option<String>,
    url: Option<String>,
}

impl BlogInfo {
    fn from(blog: &Value) -> Self {
        BlogInfo {
            name: js(blog, "/name").unwrap_or_default(),
            uuid: non_empty(js(blog, "/uuid")),
            title: non_empty(js(blog, "/title")).map(|t| decode_entities(t.trim())),
            url: non_empty(js(blog, "/url")),
        }
    }

    fn display_name(&self) -> String {
        self.title.clone().unwrap_or_else(|| self.name.clone())
    }

    fn profile_url(&self) -> String {
        self.url.clone().unwrap_or_else(|| format!("{SITE}/{}", self.name))
    }

    fn meta(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("blog".into(), json!(self.name));
        if let Some(u) = &self.uuid {
            m.insert("blogUuid".into(), json!(u));
        }
        m
    }
}

/// A blog's avatar: the listed size closest to 128 px, else the public avatar
/// endpoint, which answers with the image itself (usable in `<img>`).
fn avatar_of(blog: Option<&Value>, name: &str) -> Option<String> {
    let listed = blog
        .and_then(|b| b.get("avatar"))
        .and_then(Value::as_array)
        .and_then(|list| list.iter().min_by_key(|a| (ju(a, "/width").unwrap_or(0) as i64 - 128).abs()))
        .and_then(|a| non_empty(js(a, "/url")));
    listed.or_else(|| (!name.is_empty()).then(|| format!("{PUBLIC_API}/v2/blog/{name}/avatar/128")))
}

/// The blog identifier for API paths: the UUID when known, else the name.
fn blog_id(account: &Account) -> String {
    account
        .meta_str("blogUuid")
        .filter(|s| !s.is_empty())
        .or_else(|| Some(account.remote_id.as_str()).filter(|s| !s.is_empty()))
        .or_else(|| account.meta_str("blog"))
        .unwrap_or(account.handle.as_str())
        .to_string()
}

fn blog_name(account: &Account) -> String {
    account.meta_str("blog").filter(|s| !s.is_empty()).unwrap_or(account.handle.as_str()).to_string()
}

pub async fn profile(ctx: &Ctx, account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    let info = call(get(ctx, creds, "/v2/user/info"), "Tumblr account").await?;
    let uuid = account.meta_str("blogUuid").or(Some(account.remote_id.as_str()).filter(|r| r.starts_with("t:")));
    let blog = pick_blog(&info, uuid, account.meta_str("blog"))
        .ok_or_else(|| ProviderError::NotFound("The Tumblr blog of this account was not found.".into()))?;
    let b = BlogInfo::from(blog);
    let mut followers = ju(blog, "/followers");
    if followers.is_none() {
        // Not always part of /user/info; the follower list has the total.
        let id = b.uuid.clone().unwrap_or_else(|| b.name.clone());
        followers = call(get(ctx, creds, &format!("/v2/blog/{id}/followers")).query(&[("limit", "1")]), "Tumblr followers")
            .await
            .ok()
            .and_then(|v| ju(&v, "/total_users"));
    }
    Ok(ProfileUpdate {
        handle: Some(b.name.clone()),
        display_name: Some(b.display_name()),
        avatar_url: avatar_of(Some(blog), &b.name),
        profile_url: Some(b.profile_url()),
        stats: ProfileStats { followers, following: ju(&info, "/user/following"), posts: ju(blog, "/posts") },
        meta: b.meta(),
    })
}

// ---------------------------------------------------------------- posting

/// Each non-empty line becomes one NPF text block (a paragraph).
fn paragraphs(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).collect()
}

fn non_empty_str(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn is_http_url(s: &str) -> bool {
    url::Url::parse(s).map(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some()).unwrap_or(false)
}

pub fn validate(_account: &Account, post: &OutgoingPost) -> Vec<String> {
    let mut problems = Vec::new();
    if post.reply_to.is_some() {
        problems.push("Tumblr's API does not allow replying to posts.".to_string());
    }
    let title = non_empty_str(post.options.title.as_deref());
    if let Some(n) = title.map(|t| t.chars().count()).filter(|n| *n > BLOCK_MAX_CHARS) {
        problems.push(format!("The title has {n} characters; Tumblr allows 4,096."));
    }
    let paras = paragraphs(&post.text);
    if let Some(n) = paras.iter().map(|p| p.chars().count()).max().filter(|n| *n > BLOCK_MAX_CHARS) {
        problems.push(format!(
            "Tumblr paragraphs can have up to 4,096 characters; one has {n}. Add line breaks to split it."
        ));
    }
    if paras.len() + usize::from(title.is_some()) > MAX_TEXT_BLOCKS {
        problems.push("Tumblr posts can have up to 1,000 paragraphs.".to_string());
    }
    if text::find_urls(&post.text).len() > MAX_INLINE_LINKS {
        problems.push("Tumblr posts can contain up to 100 links.".to_string());
    }
    if non_empty_str(post.options.link.as_deref()).is_some_and(|link| !is_http_url(link)) {
        problems.push("The link must be a web address starting with http:// or https://.".to_string());
    }
    problems
}

/// An NPF text block, with links in the text marked as links (NPF ranges
/// count Unicode code points).
fn text_block(text: &str, subtype: Option<&str>) -> Value {
    let mut block = json!({ "type": "text", "text": text });
    if let Some(s) = subtype {
        block["subtype"] = json!(s);
    }
    let links: Vec<Value> = text::find_urls(text)
        .into_iter()
        .map(|(s, e)| {
            json!({
                "start": text[..s].chars().count(),
                "end": text[..e].chars().count(),
                "type": "link",
                "url": &text[s..e],
            })
        })
        .collect();
    if !links.is_empty() {
        block["formatting"] = Value::Array(links);
    }
    block
}

/// Tags as Tumblr wants them: comma-separated, without `#` and duplicates.
fn tags_param(tags: &[String]) -> String {
    let mut seen = HashSet::new();
    tags.iter()
        .map(|t| t.trim().trim_start_matches(['#', '＃']).replace(',', " ").trim().to_string())
        .filter(|t| !t.is_empty() && seen.insert(t.to_lowercase()))
        .collect::<Vec<_>>()
        .join(",")
}

pub async fn publish(ctx: &Ctx, account: &Account, creds: &Credentials, outgoing: &OutgoingPost) -> PResult<PublishResult> {
    if outgoing.reply_to.is_some() {
        return Err(ProviderError::unsupported("Tumblr's API does not allow replying to posts."));
    }
    // Order: heading, images, paragraphs, link card.
    let mut content = Vec::new();
    if let Some(title) = non_empty_str(outgoing.options.title.as_deref()) {
        content.push(text_block(title, Some("heading1")));
    }
    let mut uploads = Vec::new();
    for (i, (img, alt)) in super::prepare_images(account, outgoing)?.into_iter().enumerate() {
        let identifier = format!("image{i}");
        let mut block = json!({
            "type": "image",
            "media": [{ "type": img.mime, "identifier": identifier, "width": img.width, "height": img.height }],
        });
        if !alt.trim().is_empty() {
            block["alt_text"] = json!(alt.trim());
        }
        content.push(block);
        uploads.push((identifier, img));
    }
    for para in paragraphs(&outgoing.text) {
        content.push(text_block(para, None));
    }
    if let Some(link) = non_empty_str(outgoing.options.link.as_deref()) {
        // Tumblr fills in the title, description and image from the page.
        content.push(json!({ "type": "link", "url": link }));
    }
    if content.is_empty() {
        return Err(ProviderError::invalid("The post is empty."));
    }
    let mut body = json!({ "content": content, "state": "published" });
    let tags = tags_param(&outgoing.options.tags);
    if !tags.is_empty() {
        body["tags"] = json!(tags);
    }
    let req = post(ctx, creds, &format!("/v2/blog/{}/posts", blog_id(account)));
    let req = if uploads.is_empty() {
        req.json(&body)
    } else {
        // Multipart: the JSON first, then one part per image, named by the
        // identifier used in its block.
        let json_part = reqwest::multipart::Part::text(body.to_string())
            .mime_str("application/json")
            .map_err(|e| ProviderError::other(e.to_string()))?;
        let mut form = reqwest::multipart::Form::new().part("json", json_part);
        for (identifier, img) in uploads {
            let part = reqwest::multipart::Part::bytes(img.bytes)
                .file_name(img.file_name)
                .mime_str(&img.mime)
                .map_err(|e| ProviderError::other(e.to_string()))?;
            form = form.part(identifier, part);
        }
        req.multipart(form)
    };
    let v = call(req, "Tumblr post").await?;
    let id = non_empty(js(&v, "/id")).ok_or_else(|| ProviderError::parse("Tumblr post", "no id"))?;
    Ok(PublishResult { url: Some(format!("{SITE}/{}/{id}", blog_name(account))), remote_id: id })
}

// ---------------------------------------------------------------- reading
//
// Cursors are query strings (`offset=20`, `before=1700000000`, ...). When an
// answer has a `_links.next` object its parameters are used as is; otherwise
// the dashboard and blog posts page with `offset`, and tags and activity with
// the `before` timestamp of the last item.

type Params = Vec<(String, String)>;

fn parse_cursor(cursor: Option<&str>) -> Params {
    cursor
        .map(|c| url::form_urlencoded::parse(c.as_bytes()).into_owned().collect())
        .unwrap_or_default()
}

fn encode(params: &[(String, String)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(params).finish()
}

/// Adds the cursor's parameters to ours, without overriding the ones that
/// define the stream.
fn merge(base: &mut Params, extra: &Params) {
    for (k, v) in extra {
        if matches!(k.as_str(), "npf" | "limit" | "tag" | "api_key" | "rollups") {
            continue;
        }
        match base.iter_mut().find(|(bk, _)| bk == k) {
            Some(entry) => entry.1 = v.clone(),
            None => base.push((k.clone(), v.clone())),
        }
    }
}

/// The parameters of `_links.next`, if the answer has one.
fn next_link(v: &Value) -> Option<String> {
    let next = v.pointer("/_links/next")?;
    let mut params: Params = Vec::new();
    if let Some(q) = next.get("query_params").and_then(Value::as_object) {
        for (k, val) in q {
            let s = match val {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => continue,
            };
            params.push((k.clone(), s));
        }
    } else if let Some(href) = js(next, "/href") {
        let query = href.split_once('?').map(|(_, q)| q).unwrap_or_default();
        params = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    }
    params.retain(|(k, _)| !matches!(k.as_str(), "npf" | "limit" | "tag" | "api_key" | "rollups"));
    if params.is_empty() { None } else { Some(encode(&params)) }
}

fn before_cursor(ts: Option<u64>) -> Option<String> {
    ts.map(|t| encode(&[("before".to_string(), t.to_string())]))
}

pub async fn stream(
    ctx: &Ctx,
    account: &Account,
    creds: &Credentials,
    kind: &str,
    param: Option<&str>,
    cursor: Option<&str>,
) -> PResult<FeedPage> {
    let blog = blog_id(account);
    let own = OwnBlog {
        uuid: account
            .meta_str("blogUuid")
            .map(str::to_string)
            .or_else(|| Some(account.remote_id.clone()).filter(|r| r.starts_with("t:"))),
        name: blog_name(account),
    };
    let from_cursor = parse_cursor(cursor);
    let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
    match kind {
        "home" | "me" => {
            let path = if kind == "home" { "/v2/user/dashboard".to_string() } else { format!("/v2/blog/{blog}/posts") };
            let mut q = vec![pair("npf", "true"), pair("limit", &PAGE_SIZE.to_string())];
            merge(&mut q, &from_cursor);
            let v = call(get(ctx, creds, &path).query(&q), "Tumblr posts").await?;
            let posts = v.get("posts").and_then(Value::as_array).cloned().unwrap_or_default();
            let offset = from_cursor
                .iter()
                .find(|(k, _)| k == "offset")
                .and_then(|(_, v)| v.parse::<usize>().ok())
                .unwrap_or(0);
            let cursor = if posts.is_empty() {
                None
            } else {
                next_link(&v).or_else(|| Some(encode(&[("offset".to_string(), (offset + posts.len()).to_string())])))
            };
            Ok(FeedPage { items: posts.iter().map(|p| post_item(p, &own)).collect(), cursor })
        }
        "tagged" => {
            let tag = param.unwrap_or_default().trim().trim_start_matches(['#', '＃']).trim();
            if tag.is_empty() {
                return Err(ProviderError::invalid("Enter a tag."));
            }
            let mut q = vec![pair("tag", tag), pair("npf", "true"), pair("limit", &PAGE_SIZE.to_string())];
            merge(&mut q, &from_cursor);
            let v = call(get(ctx, creds, "/v2/tagged").query(&q), "Tumblr tag").await?;
            // This route answers with the list of posts itself.
            let posts = v.as_array().cloned().unwrap_or_default();
            // Featured tags page by `featured_timestamp`, others by `timestamp`.
            let cursor = posts
                .last()
                .and_then(|p| before_cursor(ju(p, "/featured_timestamp").or_else(|| ju(p, "/timestamp"))));
            Ok(FeedPage { items: posts.iter().map(|p| post_item(p, &own)).collect(), cursor })
        }
        "notifications" => {
            // Individual items rather than "X and 5 others liked your post".
            let mut q = vec![pair("rollups", "false")];
            merge(&mut q, &from_cursor);
            let v = call(get(ctx, creds, &format!("/v2/blog/{blog}/notifications")).query(&q), "Tumblr activity").await?;
            let list = v.get("notifications").and_then(Value::as_array).cloned().unwrap_or_default();
            let cursor = if list.is_empty() {
                None
            } else {
                next_link(&v).or_else(|| before_cursor(list.last().and_then(|n| ju(n, "/timestamp"))))
            };
            Ok(FeedPage { items: list.iter().map(|n| notification_item(n, &own)).collect(), cursor })
        }
        other => Err(ProviderError::unsupported(format!("Unknown stream {other}"))),
    }
}

struct OwnBlog {
    uuid: Option<String>,
    name: String,
}

fn iso(secs: Option<u64>) -> String {
    secs.and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// An author from a "short blog info" object (or just a name).
fn blog_author(blog: Option<&Value>, name: &str) -> Author {
    let title = blog
        .and_then(|b| non_empty(js(b, "/title")))
        .map(|t| decode_entities(t.trim()))
        .filter(|t| t != "Untitled");
    Author {
        name: title.unwrap_or_else(|| name.to_string()),
        handle: name.to_string(),
        avatar: avatar_of(blog, name),
        url: blog
            .and_then(|b| non_empty(js(b, "/url")))
            .or_else(|| (!name.is_empty()).then(|| format!("{SITE}/{name}"))),
    }
}

fn trail_author(t: &Value) -> Author {
    match t.get("blog").filter(|b| b.is_object()) {
        Some(b) => blog_author(Some(b), &js(b, "/name").unwrap_or_default()),
        None => {
            // "Broken" trail items only have the old blog name.
            let name = non_empty(js(t, "/broken_blog_name")).unwrap_or_else(|| "deleted blog".into());
            Author { name: name.clone(), handle: name, avatar: None, url: None }
        }
    }
}

/// HTML, text, media and link card collected from NPF blocks.
#[derive(Default)]
struct Rendered {
    html: String,
    text: Vec<String>,
    media: Vec<MediaView>,
    link: Option<LinkCard>,
}

/// One text block as HTML: links and mentions become links, subtypes become
/// headings, quotes and list items.
fn text_html(b: &Value, text: &str) -> String {
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let at = |ci: u64| starts.get(ci as usize).copied().unwrap_or(text.len());
    let mut spans: Vec<(usize, usize, String)> = b
        .get("formatting")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|f| {
                    let href = match js(f, "/type")?.as_str() {
                        "link" => js(f, "/url")?,
                        "mention" => js(f, "/blog/url").or_else(|| js(f, "/blog/name").map(|n| format!("{SITE}/{n}")))?,
                        _ => return None,
                    };
                    let (s, e) = (at(ju(f, "/start")?), at(ju(f, "/end")?));
                    (s < e).then_some((s, e, href))
                })
                .collect()
        })
        .unwrap_or_default();
    spans.sort_by_key(|s| s.0);
    let mut html = String::with_capacity(text.len() + 16);
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
    let html = html.replace('\n', "<br>");
    match js(b, "/subtype").as_deref() {
        Some("heading1") => format!("<h3>{html}</h3>"),
        Some("heading2") => format!("<h4>{html}</h4>"),
        Some("quote") | Some("indented") => format!("<blockquote>{html}</blockquote>"),
        Some("chat") => format!("<p><code>{html}</code></p>"),
        Some("ordered-list-item") => format!("<ol><li>{html}</li></ol>"),
        Some("unordered-list-item") => format!("<ul><li>{html}</li></ul>"),
        _ => format!("<p>{html}</p>"),
    }
}

fn image_view(b: &Value) -> Option<MediaView> {
    // Sizes come widest first.
    let sizes = b.get("media")?.as_array()?;
    let full = sizes.first()?;
    let url = non_empty(js(full, "/url"))?;
    let preview = sizes
        .iter()
        .find(|m| ju(m, "/width").map(|w| w <= 640).unwrap_or(false))
        .and_then(|m| js(m, "/url"))
        .filter(|p| *p != url);
    Some(MediaView {
        kind: "image".into(),
        preview_url: preview,
        alt: non_empty(js(b, "/alt_text")),
        width: ju(full, "/width").map(|v| v as u32),
        height: ju(full, "/height").map(|v| v as u32),
        url,
    })
}

fn poster_url(b: &Value) -> Option<String> {
    let poster = b.get("poster")?;
    let first = poster.as_array().and_then(|a| a.first()).unwrap_or(poster);
    non_empty(js(first, "/url"))
}

/// A link card for the first link; later ones become plain links.
fn add_link(out: &mut Rendered, url: String, title: Option<String>, description: Option<String>, image: Option<String>) {
    if out.link.is_none() {
        out.link = Some(LinkCard { url, title, description, image });
    } else {
        let label = title.unwrap_or_else(|| url.clone());
        out.html.push_str(&format!("<p><a href=\"{}\">{}</a></p>", escape_html(&url), escape_html(&label)));
        out.text.push(url);
    }
}

fn render_block(b: &Value, out: &mut Rendered) {
    match js(b, "/type").as_deref() {
        Some("text") => {
            let t = js(b, "/text").unwrap_or_default();
            out.html.push_str(&text_html(b, &t));
            out.text.push(t);
        }
        Some("image") => {
            if let Some(m) = image_view(b) {
                out.media.push(m);
            }
            if let Some(c) = non_empty(js(b, "/caption")) {
                out.html.push_str(&format!("<p><em>{}</em></p>", escape_html(&c)));
                out.text.push(c);
            }
        }
        Some("link") => {
            if let Some(url) = non_empty(js(b, "/url")) {
                add_link(out, url, non_empty(js(b, "/title")), non_empty(js(b, "/description")), poster_url(b));
            }
        }
        Some(kind @ ("video" | "audio")) => {
            if let Some(url) = non_empty(js(b, "/media/url")) {
                out.media.push(MediaView {
                    kind: kind.to_string(),
                    url,
                    preview_url: poster_url(b),
                    alt: None,
                    width: ju(b, "/media/width").map(|v| v as u32),
                    height: ju(b, "/media/height").map(|v| v as u32),
                });
            } else if let Some(url) = non_empty(js(b, "/url")) {
                let title = non_empty(js(b, "/title")).map(|t| match non_empty(js(b, "/artist")) {
                    Some(a) => format!("{t} – {a}"),
                    None => t,
                });
                add_link(out, url, title, None, poster_url(b));
            }
        }
        Some("poll") => {
            let question = js(b, "/question").unwrap_or_default();
            let answers: Vec<String> = b
                .get("answers")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|x| js(x, "/answer_text")).collect())
                .unwrap_or_default();
            out.html.push_str(&format!("<p>Poll: {}</p>", escape_html(&question)));
            if !answers.is_empty() {
                let items: String = answers.iter().map(|a| format!("<li>{}</li>", escape_html(a))).collect();
                out.html.push_str(&format!("<ul>{items}</ul>"));
            }
            out.text.push(format!("Poll: {question}"));
        }
        _ => {
            // NPF asks clients to show a fallback rather than drop content.
            out.html.push_str("<p><em>This part of the post can't be shown here. Open it on Tumblr.</em></p>");
            out.text.push("[content not shown]".into());
        }
    }
}

/// Renders a list of blocks; blocks listed in an "ask" layout are the
/// question and are shown as a quote attributed to whoever asked.
fn render(blocks: &[Value], layout: Option<&Value>, skip: usize, out: &mut Rendered) {
    let ask = layout
        .and_then(Value::as_array)
        .and_then(|l| l.iter().find(|x| js(x, "/type").as_deref() == Some("ask")));
    let ask_blocks: Vec<usize> = ask
        .and_then(|a| a.get("blocks"))
        .and_then(Value::as_array)
        .map(|b| b.iter().filter_map(Value::as_u64).map(|i| i as usize).collect())
        .unwrap_or_default();
    if !ask_blocks.is_empty() {
        let asker = ask.and_then(|a| non_empty(js(a, "/attribution/blog/name"))).unwrap_or_else(|| "Anonymous".into());
        out.html.push_str(&format!("<p><em>{} asked:</em></p>", escape_html(&asker)));
        out.text.push(format!("{asker} asked:"));
    }
    for (i, b) in blocks.iter().enumerate().skip(skip) {
        if ask_blocks.contains(&i) {
            let mut inner = Rendered::default();
            render_block(b, &mut inner);
            out.html.push_str(&format!("<blockquote>{}</blockquote>", inner.html));
            out.text.extend(inner.text);
            out.media.extend(inner.media);
            if out.link.is_none() {
                out.link = inner.link;
            }
        } else {
            render_block(b, out);
        }
    }
}

fn has_content(blocks: &[Value]) -> bool {
    blocks.iter().any(|b| js(b, "/type").as_deref() != Some("text") || non_empty(js(b, "/text")).is_some())
}

/// A post in NPF. Reblogs: the trail (root first) comes before the
/// reblogger's own addition; every part is attributed to its blog, as
/// Tumblr's API agreement requires.
fn post_item(p: &Value, own: &OwnBlog) -> FeedItem {
    let id = js(p, "/id_string").or_else(|| js(p, "/id")).unwrap_or_default();
    let blog = p.get("blog").filter(|b| b.is_object());
    let blog_name = non_empty(js(p, "/blog_name")).or_else(|| blog.and_then(|b| js(b, "/name"))).unwrap_or_default();
    let blog_uuid = blog.and_then(|b| non_empty(js(b, "/uuid"))).or_else(|| non_empty(js(p, "/tumblelog_uuid")));
    let poster = blog_author(blog, &blog_name);
    let empty: Vec<Value> = Vec::new();
    let trail = p.get("trail").and_then(Value::as_array).unwrap_or(&empty);
    let content = p.get("content").and_then(Value::as_array).unwrap_or(&empty);
    let adds_content = has_content(content);

    let mut segments: Vec<(Author, &[Value], Option<&Value>)> = trail
        .iter()
        .map(|t| {
            let blocks = t.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
            (trail_author(t), blocks, t.get("layout"))
        })
        .collect();
    if adds_content || segments.is_empty() {
        segments.push((poster.clone(), content.as_slice(), p.get("layout")));
    }
    let is_reblog = !trail.is_empty() || non_empty(js(p, "/parent_post_url")).is_some();
    // A reblog without additions shows the parent post, "reblogged by" the poster.
    let pure_reblog = !trail.is_empty() && !adds_content;
    let (author, reposted_by) = match segments.last() {
        Some((parent, _, _)) if pure_reblog => (parent.clone(), Some(poster.clone())),
        _ => (poster.clone(), None),
    };

    let mut r = Rendered::default();
    let labelled = segments.len() > 1;
    let mut title = None;
    for (a, blocks, layout) in &segments {
        let mut skip = 0;
        if labelled {
            r.html.push_str(&format!("<p><strong>{}</strong></p>", escape_html(&a.handle)));
            r.text.push(format!("{}:", a.handle));
        } else if layout.and_then(Value::as_array).map(|l| l.is_empty()).unwrap_or(true) {
            // A lone post starting with a heading: use it as the title.
            if let Some(first) = blocks.first().filter(|b| js(b, "/subtype").as_deref() == Some("heading1")) {
                title = non_empty(js(first, "/text"));
                skip = usize::from(title.is_some());
            }
        }
        render(blocks, *layout, skip, &mut r);
    }
    let tags: Vec<String> = p
        .get("tags")
        .and_then(Value::as_array)
        .map(|t| t.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if !tags.is_empty() {
        let links: Vec<String> = tags
            .iter()
            .map(|t| {
                let href = format!("{SITE}/tagged/{}", urlencode(t));
                format!("<a href=\"{}\">#{}</a>", escape_html(&href), escape_html(t))
            })
            .collect();
        r.html.push_str(&format!("<p>{}</p>", links.join(" ")));
        r.text.push(tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "));
    }

    let mut context = Vec::new();
    if is_reblog && !pure_reblog {
        let from = non_empty(js(p, "/reblogged_from_name")).or_else(|| trail.last().map(|t| trail_author(t).handle));
        if let Some(from) = from.filter(|f| !f.is_empty()) {
            context.push(format!("Reblogged from {from}"));
        }
    }
    if let Some(src) = non_empty(js(p, "/source_url")) {
        let label = non_empty(js(p, "/source_title")).unwrap_or_else(|| http::host_of(&src));
        context.push(format!("Source: {label}"));
    }
    let labelled_mature = p.get("has_community_label").and_then(Value::as_bool).unwrap_or(false);
    let content_warning = labelled_mature.then(|| {
        let cats: Vec<String> = p
            .get("community_label_categories")
            .and_then(Value::as_array)
            .map(|c| c.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if cats.is_empty() { "Mature content".to_string() } else { format!("Community label: {}", cats.join(", ")) }
    });

    let reblog_key = non_empty(js(p, "/reblog_key"));
    let liked = p.get("liked").and_then(Value::as_bool).unwrap_or(false);
    let is_own = (blog_uuid.is_some() && blog_uuid == own.uuid) || (!blog_name.is_empty() && blog_name.eq_ignore_ascii_case(&own.name));
    let can = |key: &str, default: bool| p.get(key).and_then(Value::as_bool).unwrap_or(default);
    let url = non_empty(js(p, "/post_url"));
    FeedItem {
        id: id.clone(),
        account_id: String::new(),
        url: url.clone(),
        author,
        created_at: iso(ju(p, "/timestamp")),
        text: r.text.join("\n\n"),
        html: if r.html.is_empty() { None } else { Some(r.html) },
        title,
        sensitive: content_warning.is_some(),
        content_warning,
        media: r.media,
        link: r.link,
        quote: None,
        reposted_by,
        context: if context.is_empty() { None } else { Some(context.join(" · ")) },
        // Tumblr only reports "notes": likes, reblogs and replies together.
        counts: Counts { likes: ju(p, "/note_count"), ..Default::default() },
        viewer: ViewerState { liked, reposted: false },
        actions: ItemActions {
            like: reblog_key.is_some() && can("can_like", !is_own),
            repost: reblog_key.is_some() && blog_uuid.is_some() && can("can_reblog", true),
            reply: false,
            delete: is_own,
        },
        target: json!({
            "id": id,
            "reblogKey": reblog_key,
            "blogUuid": blog_uuid,
            "blogName": blog_name,
            "url": url,
            "liked": liked,
        }),
        notification: None,
    }
}

/// An item of the blog's activity feed.
fn notification_item(n: &Value, own: &OwnBlog) -> FeedItem {
    let kind_s = js(n, "/type").unwrap_or_default();
    let (kind, label) = match kind_s.as_str() {
        "like" => (NotificationKind::Like, "liked your post"),
        "reblog_naked" => (NotificationKind::Repost, "reblogged your post"),
        "reblog_with_content" => (NotificationKind::Repost, "reblogged your post and added"),
        "reply" => (NotificationKind::Reply, "replied to your post"),
        "follow" => (NotificationKind::Follow, "followed you"),
        "mention_in_reply" => (NotificationKind::Mention, "mentioned you in a reply"),
        "mention_in_post" => (NotificationKind::Mention, "mentioned you in a post"),
        "ask" => (NotificationKind::Message, "sent you an ask"),
        "answered_ask" => (NotificationKind::Other, "answered your ask"),
        "new_group_blog_member" => (NotificationKind::Other, "joined your group blog"),
        "post_attribution" => (NotificationKind::Other, "used your post"),
        "post_flagged" => (NotificationKind::Other, "your post was flagged"),
        "post_appeal_accepted" => (NotificationKind::Other, "your appeal was accepted"),
        "post_appeal_rejected" => (NotificationKind::Other, "your appeal was rejected"),
        "what_you_missed" => (NotificationKind::Other, "a post you may have missed"),
        "conversational_note" => (NotificationKind::Reply, "commented on a post you follow"),
        _ => (NotificationKind::Other, "sent a notification"),
    };
    let from = non_empty(js(n, "/from_tumblelog_name"));
    let author = match &from {
        Some(name) => blog_author(None, name),
        None => Author { name: "Tumblr".into(), handle: String::new(), avatar: None, url: Some(SITE.into()) },
    };
    let my_post = non_empty(js(n, "/target_post_id")).map(|id| format!("{SITE}/{}/{id}", own.name));
    let their_post = match (&from, non_empty(js(n, "/post_id"))) {
        (Some(f), Some(id)) => Some(format!("{SITE}/{f}/{id}")),
        _ => None,
    };
    let url = match kind {
        NotificationKind::Like | NotificationKind::Reply => my_post.clone(),
        NotificationKind::Follow => author.url.clone(),
        _ => their_post.or_else(|| my_post.clone()),
    };
    let mut text = non_empty(js(n, "/reply_text"))
        .or_else(|| non_empty(js(n, "/added_text")))
        .or_else(|| non_empty(js(n, "/post_summary")))
        .unwrap_or_default();
    let tags: Vec<String> = n
        .get("post_tags")
        .and_then(Value::as_array)
        .map(|t| t.iter().filter_map(Value::as_str).map(|t| format!("#{t}")).collect())
        .unwrap_or_default();
    if !tags.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&tags.join(" "));
    }
    let subject = match kind {
        NotificationKind::Like | NotificationKind::Repost | NotificationKind::Reply => {
            non_empty(js(n, "/target_post_summary")).map(|s| text::preview(&s, 280))
        }
        _ => None,
    };
    let created = ju(n, "/timestamp");
    FeedItem {
        id: non_empty(js(n, "/id")).unwrap_or_else(|| {
            format!("{kind_s}-{}-{}", created.unwrap_or_default(), from.clone().unwrap_or_default())
        }),
        url,
        author,
        created_at: iso(created),
        text,
        actions: ItemActions::default(),
        target: Value::Null,
        notification: Some(NotificationInfo { kind, label: label.into(), subject }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- actions

pub async fn interact(ctx: &Ctx, account: &Account, creds: &Credentials, target: &Value, action: Interaction) -> PResult<Value> {
    if action == Interaction::Unrepost {
        return Err(ProviderError::unsupported(
            "A Tumblr reblog is a post of its own: delete it from your blog to undo it.",
        ));
    }
    let (Some(id), Some(key)) = (non_empty(js(target, "/id")), non_empty(js(target, "/reblogKey"))) else {
        return Err(ProviderError::invalid("This post can't be acted on."));
    };
    let mut target = target.clone();
    match action {
        Interaction::Like | Interaction::Unlike => {
            let path = if action == Interaction::Like { "/v2/user/like" } else { "/v2/user/unlike" };
            call(post(ctx, creds, path).form(&[("id", id.as_str()), ("reblog_key", key.as_str())]), "Tumblr like").await?;
            target["liked"] = json!(action == Interaction::Like);
        }
        Interaction::Repost => {
            let uuid = non_empty(js(&target, "/blogUuid")).ok_or_else(|| ProviderError::invalid("This post can't be reblogged."))?;
            // Post IDs are 64-bit integers; send a number when it parses.
            let parent_id = id.parse::<u64>().map(Value::from).unwrap_or_else(|_| json!(id));
            let body = json!({
                "content": [],
                "state": "published",
                "parent_tumblelog_uuid": uuid,
                "parent_post_id": parent_id,
                "reblog_key": key,
            });
            let v = call(post(ctx, creds, &format!("/v2/blog/{}/posts", blog_id(account))).json(&body), "Tumblr reblog").await?;
            target["reblogged"] = json!(true);
            target["reblogId"] = json!(js(&v, "/id"));
        }
        Interaction::Unrepost => unreachable!(),
    }
    Ok(target)
}

pub async fn delete(ctx: &Ctx, account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    let id = remote_id.trim();
    if id.is_empty() {
        return Err(ProviderError::invalid("This post has no Tumblr id."));
    }
    call(post(ctx, creds, &format!("/v2/blog/{}/post/delete", blog_id(account))).form(&[("id", id)]), "Tumblr delete")
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::Pkce;
    use crate::testutil::bytes_contains;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const UUID: &str = "t:mYbLoGuUiD";

    fn account() -> Account {
        Account {
            id: "a1".into(),
            platform: Platform::Tumblr,
            server: String::new(),
            remote_id: UUID.into(),
            handle: "myblog".into(),
            display_name: "My Blog".into(),
            avatar_url: None,
            profile_url: None,
            meta: json!({ "blog": "myblog", "blogUuid": UUID }),
            status: AccountStatus::Ok,
            status_message: None,
            position: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ctx(server: &MockServer) -> Ctx {
        Ctx::default().with_override("tumblr.www", &server.uri()).with_override("tumblr.api", &server.uri())
    }

    fn ok(response: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({ "meta": { "status": 200, "msg": "OK" }, "response": response }))
    }

    /// Shaped like the documented `/v2/user/info` answer.
    fn user_info() -> Value {
        json!({ "user": {
            "following": 263, "default_post_format": "html", "name": "me", "likes": 606,
            "blogs": [
                {"name": "sideblog", "title": "Side", "url": "https://sideblog.tumblr.com/", "primary": false,
                 "followers": 3, "uuid": "t:sIdE", "type": "public"},
                {"name": "myblog", "title": "My Blog", "url": "https://myblog.tumblr.com/", "tweet": "N", "primary": true,
                 "followers": 2684, "posts": 120, "uuid": UUID, "type": "public",
                 "avatar": [{"width": 512, "height": 512, "url": "https://64.media.tumblr.com/avatar_512.png"},
                            {"width": 128, "height": 128, "url": "https://64.media.tumblr.com/avatar_128.png"},
                            {"width": 64, "height": 64, "url": "https://64.media.tumblr.com/avatar_64.png"}]}
            ]
        }})
    }

    #[tokio::test]
    async fn signs_in_and_picks_the_primary_blog() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/oauth2/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=thecode"))
            .and(body_string_contains("client_id=ckey"))
            .and(body_string_contains("client_secret=csecret"))
            .and(body_string_contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5555%2Fcallback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "acc-1", "expires_in": 2520, "id_token": false, "refresh_token": "ref-1",
                "scope": "basic write offline_access", "token_type": "bearer"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/user/info"))
            .and(header("authorization", "Bearer acc-1"))
            .respond_with(ok(user_info()))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let b = OAuthBegin {
            platform: Platform::Tumblr,
            fields: HashMap::new(),
            redirect_uri: "http://127.0.0.1:5555/callback".into(),
            state: "st".into(),
            pkce: Pkce::new(),
            client_id: Some("ckey".into()),
            client_secret: Some("csecret".into()),
        };
        let start = begin(&ctx, &b).await.unwrap();
        let url = url::Url::parse(&start.auth_url).unwrap();
        assert_eq!(url.path(), "/oauth2/authorize");
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "ckey");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], "basic write offline_access");
        assert_eq!(q["state"], "st");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5555/callback");
        assert!(!start.auth_url.contains("csecret"));

        let no_secret = OAuthBegin { client_secret: None, ..b.clone() };
        assert!(matches!(begin(&ctx, &no_secret).await, Err(ProviderError::Invalid(_))));

        let wrong_state = HashMap::from([("code".to_string(), "thecode".to_string()), ("state".to_string(), "x".to_string())]);
        assert!(finish(&ctx, &b, &start.flow, &wrong_state).await.is_err());

        let params = HashMap::from([("code".to_string(), "thecode".to_string()), ("state".to_string(), "st".to_string())]);
        let accounts = finish(&ctx, &b, &start.flow, &params).await.unwrap();
        assert_eq!(accounts.len(), 1);
        let a = &accounts[0];
        assert_eq!(a.platform, Platform::Tumblr);
        assert_eq!(a.server, "");
        assert_eq!(a.remote_id, UUID);
        assert_eq!(a.handle, "myblog");
        assert_eq!(a.display_name, "My Blog");
        assert_eq!(a.avatar_url.as_deref(), Some("https://64.media.tumblr.com/avatar_128.png"));
        assert_eq!(a.profile_url.as_deref(), Some("https://myblog.tumblr.com/"));
        assert_eq!(a.meta, json!({ "blog": "myblog", "blogUuid": UUID }));
        let c = &a.credentials;
        assert_eq!(c.access_token.as_deref(), Some("acc-1"));
        assert_eq!(c.refresh_token.as_deref(), Some("ref-1"));
        assert_eq!(c.client_id.as_deref(), Some("ckey"));
        assert_eq!(c.client_secret.as_deref(), Some("csecret"));
        let exp = c.expires_at.unwrap();
        assert!((now_secs() + 2510..=now_secs() + 2520).contains(&exp), "{exp}");
    }

    #[tokio::test]
    async fn refresh_stores_the_new_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/oauth2/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=ref-1"))
            .and(body_string_contains("client_secret=csecret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "acc-2", "expires_in": 2520, "refresh_token": "ref-2",
                "scope": "basic write offline_access", "token_type": "bearer"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/oauth2/token"))
            .and(body_string_contains("refresh_token=zz-expired-refresh"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "meta": {"status": 401, "msg": "Unauthorized"}, "response": [],
                "errors": [{"title": "Unauthorized", "code": 1016, "detail": "Unable to authorize"}]
            })))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials {
            access_token: Some("acc-1".into()),
            refresh_token: Some("ref-1".into()),
            expires_at: Some(1),
            client_id: Some("ckey".into()),
            client_secret: Some("csecret".into()),
            extra: Default::default(),
        };
        let new = refresh(&ctx, &account(), &creds).await.unwrap();
        assert_eq!(new.access_token.as_deref(), Some("acc-2"));
        assert_eq!(new.refresh_token.as_deref(), Some("ref-2"));
        assert_eq!(new.client_secret.as_deref(), Some("csecret"));
        let expired = Credentials { refresh_token: Some("zz-expired-refresh".into()), ..creds };
        let err = refresh(&ctx, &account(), &expired).await.unwrap_err();
        assert!(matches!(&err, ProviderError::Unauthorized(m) if m.contains("Reconnect")), "{err:?}");
        assert!(!err.to_string().contains("zz-expired-refresh"));
    }

    #[tokio::test]
    async fn reads_profile_and_falls_back_to_the_follower_list() {
        let server = MockServer::start().await;
        let mut info = user_info();
        info["user"]["blogs"][1].as_object_mut().unwrap().remove("followers");
        info["user"]["blogs"][1]["name"] = json!("renamed");
        Mock::given(method("GET")).and(path("/v2/user/info")).respond_with(ok(info)).mount(&server).await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/blog/{UUID}/followers")))
            .and(query_param("limit", "1"))
            .respond_with(ok(json!({"total_users": 2685, "users": [{"name": "david", "following": true, "url": "https://www.davidslog.com", "updated": 1308781073}]})))
            .expect(1)
            .mount(&server)
            .await;
        let p = profile(&ctx(&server), &account(), &Credentials::bearer("t")).await.unwrap();
        // Found by UUID although the blog was renamed.
        assert_eq!(p.handle.as_deref(), Some("renamed"));
        assert_eq!(p.stats.followers, Some(2685));
        assert_eq!(p.stats.following, Some(263));
        assert_eq!(p.stats.posts, Some(120));
        assert_eq!(p.meta["blog"], json!("renamed"));
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([200, 10, 10])))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[tokio::test]
    async fn publishes_npf_with_images_in_one_multipart_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/v2/blog/{UUID}/posts")))
            .and(header("authorization", "Bearer tok"))
            .and(bytes_contains("name=\"json\""))
            .and(bytes_contains("application/json"))
            .and(bytes_contains("\"subtype\":\"heading1\""))
            .and(bytes_contains("\"identifier\":\"image0\""))
            .and(bytes_contains("\"alt_text\":\"A red square\""))
            .and(bytes_contains("\"tags\":\"art,red square\""))
            .and(bytes_contains("\"state\":\"published\""))
            .and(bytes_contains("name=\"image0\"; filename=\"red.png\""))
            .and(bytes_contains("Content-Type: image/png"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "meta": {"status": 201, "msg": "Created"}, "response": {"id": "1234567891234567"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut att = crate::media::import_bytes(dir.path(), &png(8, 6), "red.png").unwrap();
        att.alt = "A red square".into();
        let post = OutgoingPost {
            text: "First line\n\nSee https://example.com/a now".into(),
            media: vec![att],
            options: PostOptions {
                title: Some("Hello Tumblr".into()),
                tags: vec!["#art".into(), "red square".into(), "Art".into(), " ".into()],
                ..Default::default()
            },
            reply_to: None,
            idempotency_key: "k".into(),
        };
        assert!(validate(&account(), &post).is_empty());
        let res = publish(&ctx(&server), &account(), &Credentials::bearer("tok"), &post).await.unwrap();
        assert_eq!(res.remote_id, "1234567891234567");
        assert_eq!(res.url.as_deref(), Some("https://www.tumblr.com/myblog/1234567891234567"));
    }

    #[tokio::test]
    async fn publishes_text_posts_as_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/v2/blog/{UUID}/posts")))
            .respond_with(|req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                assert_eq!(body["state"], json!("published"));
                assert!(body.get("tags").is_none());
                let content = body["content"].as_array().unwrap();
                assert_eq!(content.len(), 3);
                assert_eq!(content[0], json!({"type": "text", "text": "Grüße 👋"}));
                // Ranges count code points: "Grüße 👋 " is 8 of them.
                assert_eq!(
                    content[1],
                    json!({"type": "text", "text": "Grüße 👋 https://x.org/a.",
                           "formatting": [{"start": 8, "end": 23, "type": "link", "url": "https://x.org/a"}]})
                );
                assert_eq!(content[2], json!({"type": "link", "url": "https://example.com/page"}));
                ResponseTemplate::new(201).set_body_json(json!({"meta": {"status": 201, "msg": "Created"}, "response": {"id": "77"}}))
            })
            .expect(1)
            .mount(&server)
            .await;
        let post = OutgoingPost {
            text: "  Grüße 👋  \n\n\nGrüße 👋 https://x.org/a.\n".into(),
            media: vec![],
            options: PostOptions { link: Some("https://example.com/page".into()), ..Default::default() },
            reply_to: None,
            idempotency_key: "k".into(),
        };
        let res = publish(&ctx(&server), &account(), &Credentials::bearer("tok"), &post).await.unwrap();
        assert_eq!(res.remote_id, "77");
    }

    #[tokio::test]
    async fn maps_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/v2/blog/{UUID}/posts")))
            .and(body_string_contains("bad"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "meta": {"msg": "Bad Request", "status": 400}, "errors": [{"title": "'content' must be an array'", "code": 8001}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/v2/blog/{UUID}/posts")))
            .and(body_string_contains("busy"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "meta": {"msg": "Forbidden", "status": 403},
                "errors": [{"title": "Forbidden", "code": 8023, "detail": "You've hit your daily post limit."}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/user/info"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "meta": {"status": 401, "msg": "Unauthorized"}, "response": [],
                "errors": [{"title": "Unauthorized", "code": 1016, "detail": "Unable to authorize"}]
            })))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials::bearer("tok");
        let text = |t: &str| OutgoingPost {
            text: t.into(),
            media: vec![],
            options: PostOptions::default(),
            reply_to: None,
            idempotency_key: "k".into(),
        };
        let err = publish(&ctx, &account(), &creds, &text("bad")).await.unwrap_err();
        assert_eq!(err, ProviderError::Invalid("'content' must be an array'".into()));
        let err = publish(&ctx, &account(), &creds, &text("busy")).await.unwrap_err();
        assert_eq!(
            err,
            ProviderError::RateLimited { message: "You've hit your daily post limit.".into(), retry_after: None }
        );
        let err = profile(&ctx, &account(), &creds).await.unwrap_err();
        assert!(matches!(err, ProviderError::Unauthorized(_)));
        let mut reply = text("hi");
        reply.reply_to = Some(json!({"id": "1"}));
        assert!(matches!(publish(&ctx, &account(), &creds, &reply).await, Err(ProviderError::Unsupported(_))));
    }

    fn short_blog(name: &str, uuid: &str) -> Value {
        json!({"name": name, "title": "Untitled", "description": "", "url": format!("https://{name}.tumblr.com/"),
               "uuid": uuid, "updated": 1_762_000_000})
    }

    #[tokio::test]
    async fn maps_dashboard_posts_and_reblog_trails() {
        let server = MockServer::start().await;
        let original = json!({
            "object_type": "post", "type": "blocks", "id": 7_100_000_000_000_000_001_u64, "id_string": "7100000000000000001",
            "blog_name": "artist", "blog": short_blog("artist", "t:aRt"), "post_url": "https://www.tumblr.com/artist/7100000000000000001",
            "timestamp": 1_762_000_000, "reblog_key": "rk1", "tags": ["art", "oc"], "note_count": 42, "liked": true,
            "can_like": true, "can_reblog": true, "can_reply": true, "trail": [], "layout": [],
            "content": [
                {"type": "text", "subtype": "heading1", "text": "My painting"},
                {"type": "image", "media": [
                    {"type": "image/jpeg", "url": "https://64.media.tumblr.com/a/s1280x1920/p.jpg", "width": 1280, "height": 960},
                    {"type": "image/jpeg", "url": "https://64.media.tumblr.com/a/s640x960/p.jpg", "width": 640, "height": 480}],
                 "alt_text": "Oil on canvas"},
                {"type": "text", "text": "Made with love for @friend",
                 "formatting": [{"start": 19, "end": 26, "type": "mention",
                                 "blog": {"uuid": "t:fRiEnD", "name": "friend", "url": "https://friend.tumblr.com/"}}]},
                {"type": "link", "url": "https://example.com/shop", "title": "Shop", "description": "Prints",
                 "poster": [{"url": "https://64.media.tumblr.com/poster.jpg", "type": "image/jpeg", "width": 540, "height": 405}]}
            ]
        });
        let pure_reblog = json!({
            "object_type": "post", "type": "blocks", "id_string": "7100000000000000002", "blog_name": "fan",
            "blog": short_blog("fan", "t:fAn"), "post_url": "https://www.tumblr.com/fan/7100000000000000002",
            "timestamp": 1_762_000_100, "reblog_key": "rk2", "tags": [], "note_count": 42, "liked": false,
            "parent_post_url": "https://www.tumblr.com/artist/7100000000000000001",
            "content": [], "layout": [],
            "trail": [{"post": {"id": "7100000000000000001"}, "blog": short_blog("artist", "t:aRt"),
                       "content": [{"type": "text", "text": "Original words"}], "layout": []}]
        });
        let commented = json!({
            "object_type": "post", "type": "blocks", "id_string": "7100000000000000003", "blog_name": "myblog",
            "blog": short_blog("myblog", UUID), "post_url": "https://www.tumblr.com/myblog/7100000000000000003",
            "timestamp": 1_762_000_200, "reblog_key": "rk3", "tags": [], "note_count": 1, "liked": false,
            "content": [{"type": "text", "text": "So true"}], "layout": [],
            "trail": [
                {"broken_blog_name": "gone-blog", "content": [{"type": "text", "text": "Root"}], "layout": []},
                {"post": {"id": "7100000000000000002"}, "blog": short_blog("fan", "t:fAn"),
                 "content": [{"type": "text", "text": "Middle"}], "layout": []}
            ]
        });
        Mock::given(method("GET"))
            .and(path("/v2/user/dashboard"))
            .and(query_param("npf", "true"))
            .and(query_param("limit", "20"))
            .and(query_param("offset", "20"))
            .respond_with(ok(json!({"posts": [original, pure_reblog, commented]})))
            .mount(&server)
            .await;
        let page = stream(&ctx(&server), &account(), &Credentials::bearer("t"), "home", None, Some("offset=20")).await.unwrap();
        assert_eq!(page.cursor.as_deref(), Some("offset=23"));
        let o = &page.items[0];
        assert_eq!(o.id, "7100000000000000001");
        assert_eq!(o.title.as_deref(), Some("My painting"));
        assert_eq!(o.text, "Made with love for @friend\n\n#art #oc");
        assert!(o.html.as_deref().unwrap().contains("<a href=\"https://friend.tumblr.com/\">@friend</a>"));
        assert!(o.html.as_deref().unwrap().contains("<a href=\"https://www.tumblr.com/tagged/art\">#art</a>"));
        assert_eq!(o.author.handle, "artist");
        assert_eq!(o.author.name, "artist", "\"Untitled\" is not used as a name");
        assert_eq!(o.media[0].url, "https://64.media.tumblr.com/a/s1280x1920/p.jpg");
        assert_eq!(o.media[0].preview_url.as_deref(), Some("https://64.media.tumblr.com/a/s640x960/p.jpg"));
        assert_eq!(o.media[0].alt.as_deref(), Some("Oil on canvas"));
        let card = o.link.as_ref().unwrap();
        assert_eq!((card.url.as_str(), card.title.as_deref()), ("https://example.com/shop", Some("Shop")));
        assert_eq!(card.image.as_deref(), Some("https://64.media.tumblr.com/poster.jpg"));
        assert_eq!(o.counts.likes, Some(42));
        assert!(o.viewer.liked);
        assert!(o.actions.like && o.actions.repost && !o.actions.reply && !o.actions.delete);
        assert_eq!(o.created_at, "2025-11-01T12:26:40.000Z");
        assert_eq!(o.target["id"], json!("7100000000000000001"));
        assert_eq!(o.target["reblogKey"], json!("rk1"));
        assert_eq!(o.target["blogUuid"], json!("t:aRt"));

        let r = &page.items[1];
        assert_eq!(r.author.handle, "artist");
        assert_eq!(r.reposted_by.as_ref().unwrap().handle, "fan");
        assert_eq!(r.text, "Original words");
        assert_eq!(r.target["reblogKey"], json!("rk2"));
        assert_eq!(r.target["blogUuid"], json!("t:fAn"));

        let c = &page.items[2];
        assert_eq!(c.author.handle, "myblog");
        assert!(c.reposted_by.is_none());
        assert_eq!(c.text, "gone-blog:\n\nRoot\n\nfan:\n\nMiddle\n\nmyblog:\n\nSo true");
        assert_eq!(c.context.as_deref(), Some("Reblogged from fan"));
        assert!(c.actions.delete);
    }

    #[tokio::test]
    async fn pages_blog_posts_tags_and_activity() {
        let server = MockServer::start().await;
        let simple = |id: &str, ts: u64| {
            json!({"id_string": id, "blog_name": "myblog", "blog": short_blog("myblog", UUID), "timestamp": ts,
                   "reblog_key": "k", "content": [{"type": "text", "text": "hi"}], "layout": [], "trail": []})
        };
        Mock::given(method("GET"))
            .and(path(format!("/v2/blog/{UUID}/posts")))
            .and(query_param("npf", "true"))
            .respond_with(ok(json!({
                "blog": {"name": "myblog"}, "posts": [simple("1", 1_762_000_000)], "total_posts": 40,
                "_links": {"next": {"href": "/v2/blog/myblog/posts?npf=true&offset=20&page_number=2", "method": "GET",
                                    "query_params": {"npf": "true", "offset": "20", "page_number": "2"}}}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/tagged"))
            .and(query_param("tag", "art"))
            .and(query_param("npf", "true"))
            .respond_with(ok(json!([simple("5", 1_762_000_500), simple("4", 1_762_000_400)])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/blog/{UUID}/notifications")))
            .and(query_param("rollups", "false"))
            .and(query_param("before", "1762000000"))
            .respond_with(ok(json!({
                "notifications": [
                    {"id": "n1", "type": "like", "timestamp": 1_761_999_900, "unread": true, "target_post_id": "1",
                     "target_post_summary": "My painting", "from_tumblelog_name": "fan"},
                    {"id": "n2", "type": "reblog_with_content", "timestamp": 1_761_999_800, "unread": true,
                     "target_post_id": "1", "from_tumblelog_name": "fan", "post_id": "9", "added_text": "Wow",
                     "post_tags": ["love"]},
                    {"id": "n3", "type": "reply", "timestamp": 1_761_999_700, "unread": false, "target_post_id": "1",
                     "from_tumblelog_name": "pal", "reply_text": "Nice!"},
                    {"id": "n4", "type": "follow", "timestamp": 1_761_999_600, "unread": false, "from_tumblelog_name": "newbie"}
                ],
                "_links": {"next": {"href": "/v2/blog/myblog/notifications?before=1761999600", "method": "GET",
                                    "query_params": {"before": 1_761_999_600}}}
            })))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials::bearer("t");
        let mine = stream(&ctx, &account(), &creds, "me", None, None).await.unwrap();
        assert_eq!(mine.cursor.as_deref(), Some("offset=20&page_number=2"));
        assert!(mine.items[0].actions.delete);
        assert!(!mine.items[0].actions.like, "own posts can't be liked");

        let tagged = stream(&ctx, &account(), &creds, "tagged", Some("#art"), None).await.unwrap();
        assert_eq!(tagged.items.len(), 2);
        assert_eq!(tagged.cursor.as_deref(), Some("before=1762000400"));
        assert!(matches!(stream(&ctx, &account(), &creds, "tagged", Some(" "), None).await, Err(ProviderError::Invalid(_))));

        let activity = stream(&ctx, &account(), &creds, "notifications", None, Some("before=1762000000")).await.unwrap();
        assert_eq!(activity.cursor.as_deref(), Some("before=1761999600"));
        let like = &activity.items[0];
        let n = like.notification.as_ref().unwrap();
        assert_eq!((n.kind, n.label.as_str()), (NotificationKind::Like, "liked your post"));
        assert_eq!(n.subject.as_deref(), Some("My painting"));
        assert_eq!(like.author.handle, "fan");
        assert_eq!(like.author.avatar.as_deref(), Some("https://api.tumblr.com/v2/blog/fan/avatar/128"));
        assert_eq!(like.url.as_deref(), Some("https://www.tumblr.com/myblog/1"));
        let reblog = &activity.items[1];
        assert_eq!(reblog.notification.as_ref().unwrap().kind, NotificationKind::Repost);
        assert_eq!(reblog.text, "Wow\n\n#love");
        assert_eq!(reblog.url.as_deref(), Some("https://www.tumblr.com/fan/9"));
        let reply = &activity.items[2];
        assert_eq!(reply.notification.as_ref().unwrap().kind, NotificationKind::Reply);
        assert_eq!(reply.text, "Nice!");
        let follow = &activity.items[3];
        assert_eq!(follow.notification.as_ref().unwrap().kind, NotificationKind::Follow);
        assert_eq!(follow.url.as_deref(), Some("https://www.tumblr.com/newbie"));
        assert_eq!(follow.created_at, "2025-11-01T12:20:00.000Z");
    }

    #[tokio::test]
    async fn likes_reblogs_and_deletes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/user/like"))
            .and(body_string_contains("id=7100000000000000001"))
            .and(body_string_contains("reblog_key=rk1"))
            .respond_with(ok(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/user/unlike"))
            .and(body_string_contains("reblog_key=rk1"))
            .respond_with(ok(json!([])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/v2/blog/{UUID}/posts")))
            .respond_with(|req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                assert_eq!(body["content"], json!([]));
                assert_eq!(body["parent_tumblelog_uuid"], json!("t:aRt"));
                assert_eq!(body["parent_post_id"], json!(7_100_000_000_000_000_001_u64));
                assert_eq!(body["reblog_key"], json!("rk1"));
                ResponseTemplate::new(201).set_body_json(json!({"meta": {"status": 201, "msg": "Created"}, "response": {"id": "555"}}))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/v2/blog/{UUID}/post/delete")))
            .and(body_string_contains("id=555"))
            .respond_with(ok(json!({"id": 555})))
            .expect(1)
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        let creds = Credentials::bearer("t");
        let target = json!({"id": "7100000000000000001", "reblogKey": "rk1", "blogUuid": "t:aRt", "blogName": "artist", "liked": false});
        let liked = interact(&ctx, &account(), &creds, &target, Interaction::Like).await.unwrap();
        assert_eq!(liked["liked"], json!(true));
        let unliked = interact(&ctx, &account(), &creds, &liked, Interaction::Unlike).await.unwrap();
        assert_eq!(unliked["liked"], json!(false));
        let reblogged = interact(&ctx, &account(), &creds, &target, Interaction::Repost).await.unwrap();
        assert_eq!(reblogged["reblogId"], json!("555"));
        assert!(matches!(
            interact(&ctx, &account(), &creds, &target, Interaction::Unrepost).await,
            Err(ProviderError::Unsupported(_))
        ));
        assert!(matches!(
            interact(&ctx, &account(), &creds, &json!({"id": "1"}), Interaction::Like).await,
            Err(ProviderError::Invalid(_))
        ));
        delete(&ctx, &account(), &creds, "555").await.unwrap();
    }

    #[test]
    fn renders_asks_and_unknown_blocks() {
        // Shaped like the NPF spec's ask layout example.
        let post = json!({
            "id_string": "9", "blog_name": "myblog", "blog": short_blog("myblog", UUID), "timestamp": 1_762_000_000,
            "reblog_key": "k", "trail": [],
            "content": [
                {"type": "text", "text": "What's your favourite colour?"},
                {"type": "text", "text": "Blue, obviously <3"},
                {"type": "paywall", "subtype": "cta", "url": "https://www.tumblr.com/myblog/subscribe"}
            ],
            "layout": [{"type": "ask", "blocks": [0],
                        "attribution": {"type": "blog", "url": "https://cyle.tumblr.com", "blog": {"name": "cyle", "uuid": "t:cY"}}}]
        });
        let own = OwnBlog { uuid: Some(UUID.into()), name: "myblog".into() };
        let item = post_item(&post, &own);
        assert_eq!(item.title, None);
        assert_eq!(item.text, "cyle asked:\n\nWhat's your favourite colour?\n\nBlue, obviously <3\n\n[content not shown]");
        let html = item.html.unwrap();
        assert!(html.starts_with("<p><em>cyle asked:</em></p><blockquote><p>What&#39;s your favourite colour?</p></blockquote>"), "{html}");
        assert!(html.contains("<p>Blue, obviously &lt;3</p>"), "{html}");
        assert!(html.contains("can't be shown here"), "{html}");
    }

    #[test]
    fn validates_tumblr_rules() {
        let acc = account();
        let post = |text: String| OutgoingPost {
            text,
            media: vec![],
            options: PostOptions::default(),
            reply_to: None,
            idempotency_key: "k".into(),
        };
        assert!(validate(&acc, &post("Short and fine".into())).is_empty());
        // Long posts are fine as long as each paragraph fits in a block.
        assert!(validate(&acc, &post(format!("{}\n{}", "a".repeat(4096), "b".repeat(4096)))).is_empty());
        let long = validate(&acc, &post("é".repeat(4097)));
        assert!(long[0].contains("4,096"), "{long:?}");
        let mut with_link = post("x".into());
        with_link.options.link = Some("javascript:alert(1)".into());
        assert!(validate(&acc, &with_link).iter().any(|m| m.contains("http")));
        let mut reply = post("x".into());
        reply.reply_to = Some(json!({"id": "1"}));
        assert!(!validate(&acc, &reply).is_empty());
        let links = (0..101).map(|i| format!("https://e.com/{i}")).collect::<Vec<_>>().join(" ");
        assert!(validate(&acc, &post(links)).iter().any(|m| m.contains("100 links")));
        assert_eq!(tags_param(&["#a,b".into(), "A b".into(), "c".into()]), "a b,c");
    }

    #[test]
    fn shared_validation_allows_long_posts_with_short_paragraphs() {
        let post = |text: String| OutgoingPost {
            text,
            media: vec![],
            options: PostOptions::default(),
            reply_to: None,
            idempotency_key: "k".into(),
        };
        let long = post(format!("{}\n{}", "a".repeat(4000), "b".repeat(4000)));
        assert!(crate::providers::validate(&account(), &long).is_empty());
        let one_block = crate::providers::validate(&account(), &post("c".repeat(5000)));
        assert_eq!(one_block.len(), 1, "{one_block:?}");
        let info = crate::providers::platform_info(Platform::Tumblr);
        assert!(info.features.link && !info.features.reply);
    }
}
