//! X (formerly Twitter), through the user's own developer app.
//!
//! Sign-in: OAuth 2.0 authorization code flow with PKCE (S256) and the
//! loopback redirect `http://127.0.0.1:PORT/callback` (X wants the loopback
//! IP, not `localhost`). A "Native App" is a public client and sends its
//! client ID in the token request; a "Web App, Automated App or Bot" is a
//! confidential client and authenticates with HTTP Basic (ID and secret).
//! Access tokens last two hours; `offline.access` adds a refresh token, which
//! X replaces on every refresh.
//!
//! X bills the developer account for every request and every post or user it
//! returns (pay-per-use credits), so stream pages are kept small. Since 2026
//! self-serve apps can't like posts, and may only reply to posts that mention
//! or quote the account, or to the account's own posts.
//!
//! Reference (checked 2026-11-15): docs.x.com — OAuth 2.0 authorization code
//! flow, users/me, timelines (reverse_chronological, mentions, tweets),
//! POST /2/tweets, POST /2/media/upload, POST /2/media/metadata, retweets,
//! likes, DELETE /2/tweets/:id, and the changelog.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde_json::{Value, json};

use super::{Ctx, OAuthBegin, OAuthStart, expect_code, expect_state, js, ju};
use crate::error::{PResult, ProviderError};
use crate::http;
use crate::media::PreparedImage;
use crate::model::*;
use crate::text::{self, escape_html};

const API: &str = "https://api.x.com";
const AUTH: &str = "https://x.com";
const WEB: &str = "https://x.com";
/// Read posts and users, post and repost, like, upload media, stay signed in.
const SCOPES: &str = "tweet.read tweet.write users.read like.write media.write offline.access";
/// Posts per stream page: X bills every post and expanded object it returns.
const PAGE_SIZE: u32 = 20;
/// `metadata.alt_text.text` limit of POST /2/media/metadata.
const ALT_TEXT_MAX: usize = 1000;
const STANDARD_MAX_CHARS: u64 = 280;
/// X Premium accounts may post long text.
const PREMIUM_MAX_CHARS: u64 = 25_000;

const TWEET_FIELDS: &str = "author_id,created_at,conversation_id,in_reply_to_user_id,referenced_tweets,attachments,entities,public_metrics,possibly_sensitive,note_tweet,lang";
const EXPANSIONS: &str = "author_id,attachments.media_keys,referenced_tweets.id,referenced_tweets.id.author_id,in_reply_to_user_id";
const USER_FIELDS: &str = "name,username,profile_image_url,protected";
const MEDIA_FIELDS: &str = "media_key,type,url,preview_image_url,alt_text,width,height,variants";
const ME_FIELDS: &str = "name,username,profile_image_url,public_metrics,subscription_type";

const NO_CREDITS: &str = "Your X developer account has run out of API credits (or reached its spending limit). Add credits in the X Developer Console at console.x.com, then try again.";
const REPLY_RESTRICTED: &str =
    "X only lets apps reply to posts that mention you or quote one of your posts, and to your own posts.";

// ------------------------------------------------------------------ http

fn api(ctx: &Ctx) -> String {
    ctx.base("x.api", API)
}

fn auth(req: RequestBuilder, creds: &Credentials) -> RequestBuilder {
    req.bearer_auth(creds.token())
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

fn username(account: &Account) -> &str {
    account.handle.trim_start_matches('@')
}

fn post_url(username: &str, id: &str) -> String {
    format!("{WEB}/{username}/status/{id}")
}

/// X ids are decimal numbers; anything else must not reach a URL path.
fn x_id(s: &str) -> Option<&str> {
    let s = s.trim();
    (!s.is_empty() && s.len() <= 19 && s.bytes().all(|b| b.is_ascii_digit())).then_some(s)
}

fn own_id(account: &Account) -> PResult<&str> {
    x_id(&account.remote_id).ok_or_else(|| ProviderError::invalid("This X account has no valid user id. Reconnect it."))
}

/// Sends a request and maps X's error answers to friendly errors.
async fn send(req: RequestBuilder) -> PResult<Response> {
    let resp = http::send(req).await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry = http::retry_after_secs(&resp);
    let body = resp.text().await.unwrap_or_default();
    Err(x_error(status, &body, retry))
}

async fn send_json(req: RequestBuilder, what: &str) -> PResult<Value> {
    http::read_json(send(req).await?, what).await
}

/// Turns an X error answer into an error with a sentence the user can act on.
fn x_error(status: StatusCode, body: &str, retry_after: Option<u64>) -> ProviderError {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let detail = http::error_message(body).unwrap_or_default();
    let lower = detail.to_lowercase();
    let problem = ["/type", "/title", "/reason"]
        .iter()
        .filter_map(|p| js(&v, p))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    match status.as_u16() {
        401 => ProviderError::Unauthorized("X did not accept the saved sign-in. It may have expired or been revoked.".into()),
        402 => ProviderError::Forbidden(NO_CREDITS.into()),
        403 if problem.contains("credits") || lower.contains("credits") => ProviderError::Forbidden(NO_CREDITS.into()),
        403 if problem.contains("client-forbidden") || problem.contains("client-not-enrolled") => ProviderError::Forbidden(
            "Your X developer app is not allowed to use this part of the X API. Check the app and its API credits in the X Developer Console at console.x.com."
                .into(),
        ),
        403 if lower.contains("duplicate content") => {
            ProviderError::Invalid("X rejected this post because you posted the same text recently.".into())
        }
        403 if lower.contains("reply to this conversation is not allowed") || lower.contains("not been mentioned") => {
            ProviderError::Forbidden(REPLY_RESTRICTED.into())
        }
        403 => ProviderError::Forbidden(if detail.is_empty() || lower == "forbidden" {
            "X refused this action (403 Forbidden). Your X API access may not include it.".into()
        } else {
            format!("X refused this action: {detail}")
        }),
        429 if problem.contains("usage-capped") || problem.contains("usagecap") => ProviderError::RateLimited {
            message: if detail.is_empty() {
                "Your X API usage cap has been reached.".into()
            } else {
                format!("Your X API usage cap has been reached ({detail}).")
            },
            retry_after,
        },
        _ => http::status_error(status, body, retry_after),
    }
}

// --------------------------------------------------------------- sign-in

fn secret(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// A request to the token endpoint, authenticated the way the app type needs.
fn token_request(ctx: &Ctx, client_id: &str, client_secret: Option<&str>, params: &[(&str, &str)]) -> RequestBuilder {
    let req = ctx.http.client.post(format!("{}/2/oauth2/token", api(ctx)));
    let mut form = params.to_vec();
    match client_secret {
        // Confidential client ("Web App, Automated App or Bot").
        Some(secret) => req.basic_auth(client_id, Some(secret)).form(&form),
        // Public client ("Native App"): the client ID goes in the body.
        None => {
            form.push(("client_id", client_id));
            req.form(&form)
        }
    }
}

fn token_credentials(
    token: &Value,
    client_id: &str,
    client_secret: Option<&str>,
    previous_refresh: Option<&str>,
) -> PResult<Credentials> {
    let access = js(token, "/access_token").ok_or_else(|| ProviderError::parse("X token", "no access_token"))?;
    Ok(Credentials {
        access_token: Some(access),
        refresh_token: non_empty(js(token, "/refresh_token")).or_else(|| previous_refresh.map(str::to_string)),
        expires_at: ju(token, "/expires_in").map(|s| now_secs() + s as i64),
        client_id: Some(client_id.to_string()),
        client_secret: client_secret.map(str::to_string),
        extra: serde_json::Map::new(),
    })
}

pub async fn begin(ctx: &Ctx, b: &OAuthBegin) -> PResult<OAuthStart> {
    let client_id = b.require_client_id()?;
    let auth_url = crate::oauth::with_query(
        &format!("{}/i/oauth2/authorize", ctx.base("x.auth", AUTH)),
        &[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", &b.redirect_uri),
            ("scope", SCOPES),
            ("state", &b.state),
            ("code_challenge", &b.pkce.challenge),
            ("code_challenge_method", "S256"),
        ],
    )
    // Spaces as %20, as in X's examples. The form encoder writes them as '+'
    // and a literal '+' as %2B, so this only touches spaces.
    .replace('+', "%20");
    Ok(OAuthStart { auth_url, flow: json!({}) })
}

pub async fn finish(ctx: &Ctx, b: &OAuthBegin, _flow: &Value, params: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    expect_state(params, &b.state)?;
    let code = expect_code(params)?;
    let client_id = b.require_client_id()?;
    let client_secret = secret(b.client_secret.as_deref());
    let req = token_request(
        ctx,
        client_id,
        client_secret,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &b.redirect_uri),
            ("code_verifier", &b.pkce.verifier),
        ],
    );
    let token: Value = match http::send_json(req, "X sign-in").await {
        Ok(v) => v,
        Err(ProviderError::Unauthorized(m)) => {
            return Err(ProviderError::invalid(format!(
                "X did not accept the developer app's client ID or secret ({m}). Check them in Settings → Developer apps; leave the secret empty for a \"Native App\"."
            )));
        }
        Err(e) => return Err(e),
    };
    let creds = token_credentials(&token, client_id, client_secret, None)?;
    let me = fetch_me(ctx, &creds).await?;
    let remote_id = js(&me, "/id").ok_or_else(|| ProviderError::parse("X account", "no id"))?;
    let p = profile_update(&me);
    Ok(vec![NewAccount {
        platform: Platform::X,
        server: String::new(),
        remote_id,
        handle: p.handle.unwrap_or_default(),
        display_name: p.display_name.unwrap_or_default(),
        avatar_url: p.avatar_url,
        profile_url: p.profile_url,
        meta: Value::Object(p.meta),
        credentials: creds,
    }])
}

pub async fn refresh(ctx: &Ctx, _account: &Account, creds: &Credentials) -> PResult<Credentials> {
    let expired = || ProviderError::Unauthorized("The X sign-in has expired. Reconnect the account.".into());
    let refresh_token = creds.refresh_token.as_deref().filter(|s| !s.is_empty()).ok_or_else(expired)?;
    let client_id = creds.client_id.as_deref().filter(|s| !s.is_empty()).ok_or_else(expired)?;
    let client_secret = secret(creds.client_secret.as_deref());
    let req = token_request(
        ctx,
        client_id,
        client_secret,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh_token)],
    );
    match http::send_json::<Value>(req, "X token refresh").await {
        Ok(token) => token_credentials(&token, client_id, client_secret, Some(refresh_token)),
        // The refresh token was revoked or already used, or the app changed.
        Err(ProviderError::Invalid(m) | ProviderError::Unauthorized(m) | ProviderError::Forbidden(m)) => Err(
            ProviderError::Unauthorized(format!("X did not renew the sign-in ({m}). Reconnect the account.")),
        ),
        Err(e) => Err(e),
    }
}

async fn fetch_me(ctx: &Ctx, creds: &Credentials) -> PResult<Value> {
    let req = auth(ctx.http.client.get(format!("{}/2/users/me", api(ctx))), creds).query(&[("user.fields", ME_FIELDS)]);
    let v = send_json(req, "X account").await?;
    v.get("data")
        .filter(|d| d.is_object())
        .cloned()
        .ok_or_else(|| ProviderError::parse("X account", "no data"))
}

fn profile_update(me: &Value) -> ProfileUpdate {
    let username = js(me, "/username").unwrap_or_default();
    let premium = matches!(js(me, "/subscription_type").as_deref(), Some("Premium" | "PremiumPlus"));
    let mut meta = serde_json::Map::new();
    meta.insert("maxChars".into(), json!(if premium { PREMIUM_MAX_CHARS } else { STANDARD_MAX_CHARS }));
    ProfileUpdate {
        handle: Some(format!("@{username}")),
        display_name: Some(non_empty(js(me, "/name")).unwrap_or_else(|| username.clone())),
        avatar_url: js(me, "/profile_image_url"),
        profile_url: Some(format!("{WEB}/{username}")),
        stats: ProfileStats {
            followers: ju(me, "/public_metrics/followers_count"),
            following: ju(me, "/public_metrics/following_count"),
            // Guides say tweet_count; the generated reference says post_count.
            posts: ju(me, "/public_metrics/tweet_count").or_else(|| ju(me, "/public_metrics/post_count")),
        },
        meta,
    }
}

pub async fn profile(ctx: &Ctx, _account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    Ok(profile_update(&fetch_me(ctx, creds).await?))
}

// ---------------------------------------------------------------- posting

pub fn validate(account: &Account, post: &OutgoingPost) -> Vec<String> {
    let mut problems = Vec::new();
    let gifs: Vec<&MediaAttachment> = post.media.iter().filter(|m| m.mime == "image/gif").collect();
    if !gifs.is_empty() && post.media.len() > 1 {
        problems.push("X allows up to 4 images, or a single GIF on its own.".to_string());
    }
    // Other images are scaled down to fit; GIFs can't be without losing the animation.
    let max = super::account_limits(account).image_max_bytes;
    for g in gifs.iter().filter(|g| g.size > max) {
        problems.push(format!(
            "{} is {}; X accepts GIFs up to {} from this app.",
            g.file_name,
            crate::media::human_size(g.size),
            crate::media::human_size(max)
        ));
    }
    if let Some(reply) = &post.reply_to
        && js(reply, "/id").as_deref().and_then(x_id).is_none()
    {
        problems.push("The post being replied to is missing its X id.".to_string());
    }
    problems
}

/// Uploads one image with the v2 media endpoint (one multipart request) and
/// returns its media id.
async fn upload_media(ctx: &Ctx, creds: &Credentials, img: PreparedImage) -> PResult<String> {
    let category = if img.mime == "image/gif" { "tweet_gif" } else { "tweet_image" };
    let part = reqwest::multipart::Part::bytes(img.bytes)
        .file_name(img.file_name)
        .mime_str(&img.mime)
        .map_err(|e| ProviderError::other(e.to_string()))?;
    let form = reqwest::multipart::Form::new().text("media_category", category).part("media", part);
    let req = auth(ctx.http.client.post(format!("{}/2/media/upload", api(ctx))), creds).multipart(form);
    let v = send_json(req, "X media upload").await?;
    let id = js(&v, "/data/id").ok_or_else(|| ProviderError::parse("X media upload", "no media id"))?;
    if let Some(info) = v.pointer("/data/processing_info").filter(|i| i.is_object()) {
        wait_for_media(ctx, creds, &id, info.clone()).await?;
    }
    Ok(id)
}

/// GIFs are processed after the upload; a post can use them once X is done.
async fn wait_for_media(ctx: &Ctx, creds: &Credentials, id: &str, mut info: Value) -> PResult<()> {
    for _ in 0..30 {
        match js(&info, "/state").as_deref() {
            None | Some("succeeded") => return Ok(()),
            Some("failed") => {
                let why = js(&info, "/error/message").unwrap_or_else(|| "unknown error".into());
                return Err(ProviderError::invalid(format!("X could not process the image: {why}")));
            }
            _ => {}
        }
        let wait = ju(&info, "/check_after_secs").unwrap_or(1).clamp(1, 10);
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let req = auth(ctx.http.client.get(format!("{}/2/media/upload", api(ctx))), creds)
            .query(&[("command", "STATUS"), ("media_id", id)]);
        let v = send_json(req, "X media status").await?;
        info = v.pointer("/data/processing_info").cloned().unwrap_or(Value::Null);
    }
    Err(ProviderError::Server("X is still processing the image. Try again in a moment.".into()))
}

async fn set_alt_text(ctx: &Ctx, creds: &Credentials, media_id: &str, alt: &str) -> PResult<()> {
    let text: String = alt.trim().chars().take(ALT_TEXT_MAX).collect();
    let body = json!({ "id": media_id, "metadata": { "alt_text": { "text": text } } });
    send(auth(ctx.http.client.post(format!("{}/2/media/metadata", api(ctx))), creds).json(&body))
        .await
        .map(|_| ())
}

pub async fn publish(ctx: &Ctx, account: &Account, creds: &Credentials, post: &OutgoingPost) -> PResult<PublishResult> {
    if let Some(problem) = validate(account, post).into_iter().next() {
        return Err(ProviderError::Invalid(problem));
    }
    let mut media_ids = Vec::new();
    for (img, alt) in super::prepare_images(account, post)? {
        let id = upload_media(ctx, creds, img).await?;
        if !alt.trim().is_empty() {
            set_alt_text(ctx, creds, &id, &alt).await?;
        }
        media_ids.push(id);
    }
    let mut body = json!({});
    if !post.text.trim().is_empty() {
        body["text"] = json!(post.text);
    }
    if !media_ids.is_empty() {
        body["media"] = json!({ "media_ids": media_ids });
    }
    if let Some(id) = post.reply_to.as_ref().and_then(|r| js(r, "/id")) {
        body["reply"] = json!({ "in_reply_to_tweet_id": id });
    }
    let req = auth(ctx.http.client.post(format!("{}/2/tweets", api(ctx))), creds).json(&body);
    let v = send_json(req, "X post").await?;
    let id = js(&v, "/data/id").ok_or_else(|| ProviderError::parse("X post", "no id"))?;
    Ok(PublishResult { url: Some(post_url(username(account), &id)), remote_id: id })
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
    let me = own_id(account)?;
    let path = match kind {
        "home" => format!("/2/users/{me}/timelines/reverse_chronological"),
        "mentions" => format!("/2/users/{me}/mentions"),
        "me" => format!("/2/users/{me}/tweets"),
        other => return Err(ProviderError::unsupported(format!("Unknown stream {other}"))),
    };
    let mut query: Vec<(&str, String)> = vec![
        ("max_results", PAGE_SIZE.to_string()),
        ("tweet.fields", TWEET_FIELDS.into()),
        ("expansions", EXPANSIONS.into()),
        ("user.fields", USER_FIELDS.into()),
        ("media.fields", MEDIA_FIELDS.into()),
    ];
    if let Some(c) = cursor.filter(|c| !c.is_empty()) {
        query.push(("pagination_token", c.to_string()));
    }
    let req = auth(ctx.http.client.get(format!("{}{path}", api(ctx))), creds).query(&query);
    let v = send_json(req, "X timeline").await?;
    let inc = Includes::new(&v);
    let own = Own { id: me, username: username(account).to_lowercase() };
    let items = v
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|t| if kind == "mentions" { mention_item(t, &inc, &own) } else { timeline_item(t, &inc, &own) })
                .collect()
        })
        .unwrap_or_default();
    Ok(FeedPage { items, cursor: non_empty(js(&v, "/meta/next_token")) })
}

/// The objects a response expanded, by id.
struct Includes<'a> {
    users: HashMap<&'a str, &'a Value>,
    tweets: HashMap<&'a str, &'a Value>,
    media: HashMap<&'a str, &'a Value>,
}

fn index<'a>(list: Option<&'a Value>, key: &str) -> HashMap<&'a str, &'a Value> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| Some((x.get(key)?.as_str()?, x)))
        .collect()
}

impl<'a> Includes<'a> {
    fn new(v: &'a Value) -> Self {
        let inc = v.get("includes");
        let get = |key: &str| inc.and_then(|i| i.get(key));
        Includes {
            users: index(get("users"), "id"),
            // `posts` in case the naming of the generated reference reaches the API.
            tweets: index(get("tweets").or_else(|| get("posts")), "id"),
            media: index(get("media"), "media_key"),
        }
    }

    fn user(&self, id: Option<&str>) -> Option<&'a Value> {
        id.and_then(|id| self.users.get(id).copied())
    }

    fn tweet(&self, id: Option<&str>) -> Option<&'a Value> {
        id.and_then(|id| self.tweets.get(id).copied())
    }
}

/// The connected account.
struct Own<'a> {
    id: &'a str,
    username: String,
}

/// Id of the post `t` refers to as `kind` ("retweeted", "quoted", "replied_to").
fn referenced<'a>(t: &'a Value, kind: &str) -> Option<&'a str> {
    t.get("referenced_tweets")
        .or_else(|| t.get("referenced_posts"))?
        .as_array()?
        .iter()
        .find(|r| r.get("type").and_then(Value::as_str) == Some(kind))?
        .get("id")?
        .as_str()
}

fn author(user: Option<&Value>) -> Author {
    let Some(u) = user else {
        return Author { name: "X user".into(), ..Default::default() };
    };
    let username = js(u, "/username").unwrap_or_default();
    Author {
        name: non_empty(js(u, "/name")).unwrap_or_else(|| username.clone()),
        avatar: js(u, "/profile_image_url"),
        url: Some(format!("{WEB}/{username}")),
        handle: format!("@{username}"),
    }
}

fn media_view(m: &Value) -> MediaView {
    let best_mp4 = || {
        m.get("variants")
            .and_then(Value::as_array)?
            .iter()
            .filter(|v| js(v, "/content_type").as_deref() == Some("video/mp4"))
            .max_by_key(|v| ju(v, "/bit_rate").unwrap_or(0))
            .and_then(|v| js(v, "/url"))
    };
    let preview = js(m, "/preview_image_url");
    let (kind, url) = match js(m, "/type").as_deref() {
        Some("photo") => ("image", js(m, "/url")),
        Some("animated_gif") => ("gif", best_mp4().or_else(|| preview.clone())),
        Some("video") => ("video", best_mp4().or_else(|| preview.clone())),
        _ => ("other", js(m, "/url").or_else(|| preview.clone())),
    };
    MediaView {
        kind: kind.into(),
        url: url.unwrap_or_default(),
        preview_url: preview.or_else(|| js(m, "/url")),
        alt: non_empty(js(m, "/alt_text")),
        width: ju(m, "/width").map(|v| v as u32),
        height: ju(m, "/height").map(|v| v as u32),
    }
}

struct Rendered {
    text: String,
    html: String,
    link: Option<LinkCard>,
}

/// Plain text and HTML of a post: t.co links expanded (links to attached
/// media and to the quoted post removed), mentions, hashtags and links
/// turned into anchors, and a card for the first link X has a title for.
fn render_text(raw: &str, entities: Option<&Value>, has_media: bool, quoted_id: Option<&str>) -> Rendered {
    let mut text = text::decode_entities(raw);
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut link = None;
    let urls = entities.and_then(|e| e.get("urls")).and_then(Value::as_array);
    for u in urls.into_iter().flatten() {
        let Some(short) = js(u, "/url") else { continue };
        let expanded = js(u, "/expanded_url").unwrap_or_else(|| short.clone());
        let display = js(u, "/display_url").unwrap_or_default();
        let is_media = u.get("media_key").is_some()
            || (has_media && (display.starts_with("pic.x.com") || display.starts_with("pic.twitter.com")));
        let is_quote = quoted_id.is_some_and(|q| expanded.contains(&format!("/status/{q}")));
        if is_media || is_quote {
            text = text.replace(&short, "");
            continue;
        }
        text = text.replace(&short, &expanded);
        if !display.is_empty() {
            labels.insert(expanded.clone(), display);
        }
        if link.is_none()
            && let Some(title) = non_empty(js(u, "/title"))
        {
            link = Some(LinkCard {
                url: expanded.clone(),
                title: Some(title),
                description: non_empty(js(u, "/description")),
                image: js(u, "/images/0/url"),
            });
        }
    }
    let text = text.trim().to_string();
    let html = linkify(&text, &labels);
    Rendered { text, html, link }
}

static MENTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^A-Za-z0-9_@])(@[A-Za-z0-9_]{1,15})\b").expect("mention regex"));

/// Byte range, link target and link text.
type Span = (usize, usize, String, String);

fn overlaps(s: usize, e: usize, spans: &[Span]) -> bool {
    spans.iter().any(|x| s < x.1 && e > x.0)
}

fn linkify(text: &str, labels: &HashMap<String, String>) -> String {
    let mut spans: Vec<Span> = text::find_urls(text)
        .into_iter()
        .map(|(s, e)| {
            let url = &text[s..e];
            (s, e, url.to_string(), labels.get(url).cloned().unwrap_or_else(|| url.to_string()))
        })
        .collect();
    for c in MENTION_RE.captures_iter(text) {
        let Some(m) = c.get(1) else { continue };
        if !overlaps(m.start(), m.end(), &spans) {
            spans.push((m.start(), m.end(), format!("{WEB}/{}", &m.as_str()[1..]), m.as_str().to_string()));
        }
    }
    for (s, e, tag) in text::find_tags(text) {
        if !overlaps(s, e, &spans) {
            let tag: String = url::form_urlencoded::byte_serialize(tag.as_bytes()).collect();
            spans.push((s, e, format!("{WEB}/hashtag/{tag}"), text[s..e].to_string()));
        }
    }
    spans.sort_by_key(|x| x.0);
    let mut html = String::with_capacity(text.len() + 64);
    let mut last = 0;
    for (s, e, href, label) in spans {
        html.push_str(&escape_html(&text[last..s]));
        html.push_str(&format!("<a href=\"{}\">{}</a>", escape_html(&href), escape_html(&label)));
        last = e;
    }
    html.push_str(&escape_html(&text[last..]));
    format!("<p>{}</p>", html.replace('\n', "<br>"))
}

fn mentions_account(entities: Option<&Value>, own: &Own) -> bool {
    entities
        .and_then(|e| e.get("mentions"))
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.iter().any(|m| {
                js(m, "/id").as_deref() == Some(own.id)
                    || js(m, "/username").is_some_and(|u| u.to_lowercase() == own.username)
            })
        })
}

fn reply_context(t: &Value, inc: &Includes, author_id: &str) -> Option<String> {
    let parent = referenced(t, "replied_to");
    let to_user = js(t, "/in_reply_to_user_id");
    if parent.is_none() && to_user.is_none() {
        return None;
    }
    if to_user.as_deref() == Some(author_id) {
        return Some("Continued thread".into());
    }
    let name = inc.user(to_user.as_deref()).and_then(|u| js(u, "/username")).or_else(|| {
        let parent_author = inc.tweet(parent).and_then(|p| js(p, "/author_id"))?;
        inc.user(Some(parent_author.as_str())).and_then(|u| js(u, "/username"))
    });
    Some(match name {
        Some(n) => format!("Replying to @{n}"),
        None => "Reply".into(),
    })
}

/// One post. `with_quote` adds the quoted post (one level deep).
fn tweet_item(t: &Value, inc: &Includes, own: &Own, with_quote: bool) -> FeedItem {
    let id = js(t, "/id").unwrap_or_default();
    let author_id = js(t, "/author_id").unwrap_or_default();
    let user = inc.user(Some(author_id.as_str()));
    let handle = user.and_then(|u| js(u, "/username"));
    // Long posts carry their full text in note_tweet.
    let note = t
        .get("note_tweet")
        .or_else(|| t.get("note_post"))
        .filter(|n| n.get("text").and_then(Value::as_str).is_some());
    let (raw, entities) = match note {
        Some(n) => (js(n, "/text").unwrap_or_default(), n.get("entities")),
        None => (js(t, "/text").unwrap_or_default(), t.get("entities")),
    };
    let media: Vec<MediaView> = t
        .pointer("/attachments/media_keys")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|k| inc.media.get(k.as_str()?).copied())
        .map(media_view)
        .collect();
    let quoted_id = referenced(t, "quoted");
    let quoted = inc.tweet(quoted_id);
    let rendered = render_text(&raw, entities, !media.is_empty(), quoted_id);
    let is_own = author_id == own.id;
    let summoned = js(t, "/in_reply_to_user_id").as_deref() == Some(own.id)
        || mentions_account(t.get("entities"), own)
        || mentions_account(note.and_then(|n| n.get("entities")), own)
        || quoted.and_then(|q| js(q, "/author_id")).as_deref() == Some(own.id);
    let protected = user.and_then(|u| u.get("protected")).and_then(Value::as_bool).unwrap_or(false);
    FeedItem {
        url: handle.as_deref().map(|h| post_url(h, &id)),
        author: author(user),
        created_at: js(t, "/created_at").unwrap_or_default(),
        text: rendered.text,
        html: Some(rendered.html),
        sensitive: t.get("possibly_sensitive").and_then(Value::as_bool).unwrap_or(false),
        media,
        link: rendered.link,
        quote: if with_quote { quoted.map(|q| Box::new(tweet_item(q, inc, own, false))) } else { None },
        context: reply_context(t, inc, &author_id),
        counts: Counts {
            replies: ju(t, "/public_metrics/reply_count"),
            reposts: ju(t, "/public_metrics/retweet_count"),
            likes: ju(t, "/public_metrics/like_count"),
            quotes: ju(t, "/public_metrics/quote_count"),
            score: None,
        },
        actions: ItemActions {
            // Self-serve X API apps can't like posts (since April 2026).
            like: false,
            repost: !protected,
            // X only accepts replies to the account's own posts and to posts
            // that mention or quote it.
            reply: is_own || summoned,
            delete: is_own,
        },
        target: json!({ "id": id, "authorId": author_id, "username": handle }),
        id,
        ..Default::default()
    }
}

/// A timeline entry: a post, or a repost shown as the original post.
fn timeline_item(t: &Value, inc: &Includes, own: &Own) -> FeedItem {
    if let Some(original) = inc.tweet(referenced(t, "retweeted")) {
        let reposter = t.get("author_id").and_then(Value::as_str);
        let mut item = tweet_item(original, inc, own, true);
        item.id = js(t, "/id").unwrap_or_default();
        item.reposted_by = Some(author(inc.user(reposter)));
        item.viewer.reposted = reposter == Some(own.id);
        return item;
    }
    tweet_item(t, inc, own, true)
}

/// An entry of the mentions timeline, as a notification.
fn mention_item(t: &Value, inc: &Includes, own: &Own) -> FeedItem {
    if let Some(original) = inc.tweet(referenced(t, "retweeted"))
        && js(original, "/author_id").as_deref() == Some(own.id)
    {
        // "RT @you: ..." counts as a mention: someone reposted your post.
        let mine = tweet_item(original, inc, own, false);
        return FeedItem {
            id: js(t, "/id").unwrap_or_default(),
            url: mine.url,
            author: author(inc.user(t.get("author_id").and_then(Value::as_str))),
            created_at: js(t, "/created_at").unwrap_or_default(),
            notification: Some(NotificationInfo {
                kind: NotificationKind::Repost,
                label: "reposted your post".into(),
                subject: Some(text::preview(&mine.text, 280)),
            }),
            ..Default::default()
        };
    }
    let mut item = timeline_item(t, inc, own);
    let reply = js(t, "/in_reply_to_user_id").as_deref() == Some(own.id);
    item.notification = Some(NotificationInfo {
        kind: if reply { NotificationKind::Reply } else { NotificationKind::Mention },
        label: if reply { "replied to you" } else { "mentioned you" }.into(),
        subject: None,
    });
    item
}

// ---------------------------------------------------------------- actions

pub async fn interact(ctx: &Ctx, account: &Account, creds: &Credentials, target: &Value, action: Interaction) -> PResult<Value> {
    let id = js(target, "/id")
        .filter(|s| x_id(s).is_some())
        .ok_or_else(|| ProviderError::invalid("This post can't be acted on."))?;
    let base = format!("{}/2/users/{}", api(ctx), own_id(account)?);
    let client = &ctx.http.client;
    let (req, field, key) = match action {
        Interaction::Like => (client.post(format!("{base}/likes")).json(&json!({ "tweet_id": id })), "liked", "liked"),
        Interaction::Unlike => (client.delete(format!("{base}/likes/{id}")), "liked", "liked"),
        Interaction::Repost => {
            (client.post(format!("{base}/retweets")).json(&json!({ "tweet_id": id })), "retweeted", "reposted")
        }
        Interaction::Unrepost => (client.delete(format!("{base}/retweets/{id}")), "retweeted", "reposted"),
    };
    let v = match send_json(auth(req, creds), "X action").await {
        Ok(v) => v,
        Err(ProviderError::Forbidden(m)) if matches!(action, Interaction::Like | Interaction::Unlike) => {
            return Err(ProviderError::Forbidden(format!(
                "{m} (X stopped offering likes to self-serve API apps in April 2026.)"
            )));
        }
        Err(e) => return Err(e),
    };
    let state = v
        .pointer(&format!("/data/{field}"))
        .and_then(Value::as_bool)
        .unwrap_or(matches!(action, Interaction::Like | Interaction::Repost));
    let mut out = target.clone();
    out[key] = json!(state);
    Ok(out)
}

pub async fn delete(ctx: &Ctx, _account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    let id = x_id(remote_id).ok_or_else(|| ProviderError::invalid("This is not an X post id."))?;
    send(auth(ctx.http.client.delete(format!("{}/2/tweets/{id}", api(ctx))), creds))
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::Pkce;
    use crate::testutil::bytes_contains;
    use base64::Engine;
    use wiremock::matchers::{body_partial_json, body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx(server: &MockServer) -> Ctx {
        Ctx::default().with_override("x.api", &server.uri()).with_override("x.auth", &server.uri())
    }

    fn account() -> Account {
        Account {
            id: "a1".into(),
            platform: Platform::X,
            server: String::new(),
            remote_id: "2244994945".into(),
            handle: "@TwitterDev".into(),
            display_name: "Twitter Dev".into(),
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

    fn oauth_begin(secret: Option<&str>) -> OAuthBegin {
        OAuthBegin {
            platform: Platform::X,
            fields: HashMap::new(),
            redirect_uri: "http://127.0.0.1:5555/callback".into(),
            state: "st".into(),
            pkce: Pkce::new(),
            client_id: Some("cid".into()),
            client_secret: secret.map(str::to_string),
        }
    }

    fn me_response() -> Value {
        json!({"data": {
            "id": "2244994945", "name": "Twitter Dev", "username": "TwitterDev",
            "profile_image_url": "https://pbs.twimg.com/profile_images/1267175364003901441/tBZNFAgA_normal.jpg",
            "public_metrics": {"followers_count": 507902, "following_count": 1863, "tweet_count": 3561, "listed_count": 1550},
            "subscription_type": "None"
        }})
    }

    fn image_file(dir: &std::path::Path, name: &str, format: image::ImageFormat, alt: &str) -> MediaAttachment {
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 6, image::Rgb([10, 20, 30])))
            .write_to(&mut std::io::Cursor::new(&mut bytes), format)
            .unwrap();
        let mut att = crate::media::import_bytes(dir, &bytes, name).unwrap();
        att.alt = alt.into();
        att
    }

    fn outgoing(text: &str, media: Vec<MediaAttachment>, reply_to: Option<Value>) -> OutgoingPost {
        OutgoingPost { text: text.into(), media, options: PostOptions::default(), reply_to, idempotency_key: "k".into() }
    }

    #[tokio::test]
    async fn signs_in_with_pkce_as_public_client() {
        let server = MockServer::start().await;
        let b = oauth_begin(None);
        Mock::given(method("POST"))
            .and(path("/2/oauth2/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=thecode"))
            .and(body_string_contains("client_id=cid"))
            .and(body_string_contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5555%2Fcallback"))
            .and(body_string_contains(format!("code_verifier={}", b.pkce.verifier)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token_type": "bearer", "expires_in": 7200, "access_token": "acc-1",
                "scope": SCOPES, "refresh_token": "ref-1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2/users/me"))
            .and(header("authorization", "Bearer acc-1"))
            .and(query_param("user.fields", ME_FIELDS))
            .respond_with(ResponseTemplate::new(200).set_body_json(me_response()))
            .mount(&server)
            .await;

        let ctx = ctx(&server);
        let start = begin(&ctx, &b).await.unwrap();
        assert!(start.auth_url.starts_with(&format!("{}/i/oauth2/authorize?", server.uri())));
        for part in [
            "response_type=code",
            "client_id=cid",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A5555%2Fcallback",
            "scope=tweet.read%20tweet.write%20users.read%20like.write%20media.write%20offline.access",
            "state=st",
            "code_challenge_method=S256",
        ] {
            assert!(start.auth_url.contains(part), "{part} missing in {}", start.auth_url);
        }
        assert!(start.auth_url.contains(&format!("code_challenge={}", b.pkce.challenge)));
        assert!(!start.auth_url.contains('+'));

        let bad = HashMap::from([("code".to_string(), "thecode".to_string()), ("state".to_string(), "other".to_string())]);
        assert!(finish(&ctx, &b, &start.flow, &bad).await.is_err());
        let denied = HashMap::from([("error".to_string(), "access_denied".to_string()), ("state".to_string(), "st".to_string())]);
        assert!(matches!(finish(&ctx, &b, &start.flow, &denied).await, Err(ProviderError::Forbidden(_))));

        let params = HashMap::from([("code".to_string(), "thecode".to_string()), ("state".to_string(), "st".to_string())]);
        let accounts = finish(&ctx, &b, &start.flow, &params).await.unwrap();
        let a = &accounts[0];
        assert_eq!(a.platform, Platform::X);
        assert_eq!(a.server, "");
        assert_eq!(a.remote_id, "2244994945");
        assert_eq!(a.handle, "@TwitterDev");
        assert_eq!(a.display_name, "Twitter Dev");
        assert_eq!(a.profile_url.as_deref(), Some("https://x.com/TwitterDev"));
        assert_eq!(a.meta["maxChars"], json!(280));
        let c = &a.credentials;
        assert_eq!(c.access_token.as_deref(), Some("acc-1"));
        assert_eq!(c.refresh_token.as_deref(), Some("ref-1"));
        assert_eq!(c.client_id.as_deref(), Some("cid"));
        assert_eq!(c.client_secret, None);
        let exp = c.expires_at.unwrap() - now_secs();
        assert!((7190..=7200).contains(&exp), "{exp}");
        // A public client sends no Authorization header to the token endpoint.
        let requests = server.received_requests().await.unwrap();
        let token_req = requests.iter().find(|r| r.url.path() == "/2/oauth2/token").unwrap();
        assert!(!token_req.headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn confidential_client_uses_basic_auth_and_refresh_replaces_tokens() {
        let server = MockServer::start().await;
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("cid:csecret"));
        Mock::given(method("POST"))
            .and(path("/2/oauth2/token"))
            .and(header("authorization", basic.as_str()))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=ref-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token_type": "bearer", "expires_in": 7200, "access_token": "acc-2",
                "scope": SCOPES, "refresh_token": "ref-2"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let creds = Credentials {
            access_token: Some("acc-1".into()),
            refresh_token: Some("ref-1".into()),
            expires_at: Some(0),
            client_id: Some("cid".into()),
            client_secret: Some("csecret".into()),
            extra: Default::default(),
        };
        let new = refresh(&ctx(&server), &account(), &creds).await.unwrap();
        assert_eq!(new.access_token.as_deref(), Some("acc-2"));
        assert_eq!(new.refresh_token.as_deref(), Some("ref-2"));
        assert_eq!(new.client_secret.as_deref(), Some("csecret"));
        assert!(new.expires_at.unwrap() > now_secs() + 7000);
        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body).to_string();
        assert!(!body.contains("client_id"), "{body}");
    }

    #[tokio::test]
    async fn rejected_refresh_means_reconnect() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/2/oauth2/token"))
            .and(body_string_contains("client_id=cid"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_request", "error_description": "Value passed for the token was invalid."
            })))
            .mount(&server)
            .await;
        let mut creds = Credentials::bearer("acc-1");
        creds.client_id = Some("cid".into());
        assert!(matches!(refresh(&ctx(&server), &account(), &creds).await, Err(ProviderError::Unauthorized(_))));
        creds.refresh_token = Some("old".into());
        match refresh(&ctx(&server), &account(), &creds).await {
            Err(ProviderError::Unauthorized(m)) => assert!(m.contains("Value passed for the token was invalid"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn publishes_images_with_alt_text_as_reply() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/2/media/upload"))
            .and(header("authorization", "Bearer tok"))
            .and(bytes_contains("name=\"media_category\""))
            .and(bytes_contains("tweet_image"))
            .and(bytes_contains("name=\"media\"; filename=\"cat.png\""))
            .and(bytes_contains("IHDR"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "id": "1880028106020515840", "media_key": "3_1880028106020515840", "size": 72,
                "expires_after_secs": 86400, "image": {"image_type": "image/png", "w": 8, "h": 6}
            }})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/2/media/metadata"))
            .and(body_partial_json(json!({"id": "1880028106020515840", "metadata": {"alt_text": {"text": "A grey cat"}}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "id": "1880028106020515840", "associated_metadata": {"alt_text": {"text": "A grey cat"}}
            }})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/2/tweets"))
            .and(body_partial_json(json!({
                "text": "Hello @friend #rust",
                "media": {"media_ids": ["1880028106020515840"]},
                "reply": {"in_reply_to_tweet_id": "1212092627178287104"}
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {
                "id": "1445880548472328192", "text": "Hello @friend #rust",
                "edit_history_tweet_ids": ["1445880548472328192"]
            }})))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let att = image_file(dir.path(), "cat.png", image::ImageFormat::Png, "A grey cat");
        let post = outgoing("Hello @friend #rust", vec![att], Some(json!({"id": "1212092627178287104"})));
        let res = publish(&ctx(&server), &account(), &Credentials::bearer("tok"), &post).await.unwrap();
        assert_eq!(res.remote_id, "1445880548472328192");
        assert_eq!(res.url.as_deref(), Some("https://x.com/TwitterDev/status/1445880548472328192"));
    }

    #[tokio::test]
    async fn gif_upload_waits_for_processing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/2/media/upload"))
            .and(bytes_contains("tweet_gif"))
            .and(bytes_contains("GIF89a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "id": "1880028106020515841", "media_key": "16_1880028106020515841", "size": 100,
                "expires_after_secs": 86400,
                "processing_info": {"state": "pending", "check_after_secs": 1}
            }})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2/media/upload"))
            .and(query_param("command", "STATUS"))
            .and(query_param("media_id", "1880028106020515841"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "id": "1880028106020515841", "media_key": "16_1880028106020515841",
                "processing_info": {"state": "succeeded", "progress_percent": 100}
            }})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/2/tweets"))
            .and(body_partial_json(json!({"media": {"media_ids": ["1880028106020515841"]}})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"data": {"id": "1445880548472328193", "text": ""}})))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let gif = image_file(dir.path(), "dance.gif", image::ImageFormat::Gif, "");
        let post = outgoing("", vec![gif], None);
        let res = publish(&ctx(&server), &account(), &Credentials::bearer("tok"), &post).await.unwrap();
        assert_eq!(res.remote_id, "1445880548472328193");
        // No text field when the post is only a GIF.
        let requests = server.received_requests().await.unwrap();
        let tweet: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert!(tweet.get("text").is_none(), "{tweet}");
    }

    #[test]
    fn validates_gif_and_reply_rules() {
        let dir = tempfile::tempdir().unwrap();
        let gif = image_file(dir.path(), "a.gif", image::ImageFormat::Gif, "");
        let png = image_file(dir.path(), "b.png", image::ImageFormat::Png, "");
        let acc = account();
        assert!(validate(&acc, &outgoing("hi", vec![gif.clone()], None)).is_empty());
        assert!(validate(&acc, &outgoing("hi", vec![png.clone(), png.clone()], None)).is_empty());
        let mixed = validate(&acc, &outgoing("hi", vec![gif.clone(), png], None));
        assert_eq!(mixed, vec!["X allows up to 4 images, or a single GIF on its own.".to_string()]);
        let mut big = gif;
        big.size = 6 * 1024 * 1024;
        let p = validate(&acc, &outgoing("hi", vec![big], None));
        assert!(p[0].contains("up to 5.0 MB"), "{p:?}");
        let p = validate(&acc, &outgoing("hi", vec![], Some(json!({"id": "../2/users"}))));
        assert_eq!(p.len(), 1);
        // The generic validator counts X's weighted length.
        let long = "日".repeat(141);
        assert!(super::super::validate(&acc, &outgoing(&long, vec![], None)).iter().any(|p| p.contains("282")));
    }

    fn timeline_response() -> Value {
        json!({
            "data": [
                {
                    "id": "1212092628029698048", "author_id": "2244994945", "created_at": "2019-12-31T19:26:16.000Z",
                    "edit_history_tweet_ids": ["1212092628029698048"], "lang": "en", "possibly_sensitive": false,
                    "conversation_id": "1212092627178287104", "in_reply_to_user_id": "783214",
                    "referenced_tweets": [{"type": "replied_to", "id": "1212092627178287104"}],
                    "text": "@X We believe the best future version of our API will come from building it with YOU &amp; #devs https://t.co/abc123XYZ0 https://t.co/yvxdK6aOo2",
                    "attachments": {"media_keys": ["3_1211797899316740096"]},
                    "entities": {
                        "mentions": [{"start": 0, "end": 2, "username": "X", "id": "783214"}],
                        "hashtags": [{"start": 85, "end": 90, "tag": "devs"}],
                        "urls": [
                            {"start": 91, "end": 114, "url": "https://t.co/abc123XYZ0", "expanded_url": "https://developer.x.com/en/blog",
                             "display_url": "developer.x.com/en/blog", "status": 200, "title": "Developer blog",
                             "description": "News for developers", "unwound_url": "https://developer.x.com/en/blog",
                             "images": [{"url": "https://pbs.twimg.com/news_img/1.jpg", "width": 800, "height": 400}]},
                            {"start": 115, "end": 138, "url": "https://t.co/yvxdK6aOo2",
                             "expanded_url": "https://twitter.com/TwitterDev/status/1212092628029698048/photo/1",
                             "display_url": "pic.twitter.com/yvxdK6aOo2", "media_key": "3_1211797899316740096"}
                        ]
                    },
                    "public_metrics": {"retweet_count": 7, "reply_count": 3, "like_count": 38, "quote_count": 1, "bookmark_count": 2, "impression_count": 1250}
                },
                {
                    "id": "1300000000000000001", "author_id": "6253282", "created_at": "2026-11-01T10:00:00.000Z",
                    "edit_history_tweet_ids": ["1300000000000000001"],
                    "text": "RT @XDevelopers: A long announcement that X truncates in retweets…",
                    "referenced_tweets": [{"type": "retweeted", "id": "1300000000000000000"}],
                    "public_metrics": {"retweet_count": 12, "reply_count": 0, "like_count": 0, "quote_count": 0}
                },
                {
                    "id": "1300000000000000002", "author_id": "6253282", "created_at": "2026-11-01T11:00:00.000Z",
                    "edit_history_tweet_ids": ["1300000000000000002"],
                    "text": "Worth reading https://t.co/qqqqqqqqqq",
                    "referenced_tweets": [{"type": "quoted", "id": "1212092628029698048"}],
                    "entities": {"urls": [{"start": 14, "end": 37, "url": "https://t.co/qqqqqqqqqq",
                        "expanded_url": "https://twitter.com/TwitterDev/status/1212092628029698048",
                        "display_url": "twitter.com/TwitterDev/st…"}]},
                    "public_metrics": {"retweet_count": 0, "reply_count": 0, "like_count": 2, "quote_count": 0}
                }
            ],
            "includes": {
                "users": [
                    {"id": "2244994945", "name": "Twitter Dev", "username": "TwitterDev",
                     "profile_image_url": "https://pbs.twimg.com/profile_images/1267175364003901441/tBZNFAgA_normal.jpg", "protected": false},
                    {"id": "783214", "name": "X", "username": "X", "protected": false},
                    {"id": "6253282", "name": "X API", "username": "API", "protected": true},
                    {"id": "1500000000", "name": "X Developers", "username": "XDevelopers", "protected": false}
                ],
                "tweets": [
                    {"id": "1300000000000000000", "author_id": "1500000000", "created_at": "2026-10-31T09:00:00.000Z",
                     "edit_history_tweet_ids": ["1300000000000000000"],
                     "text": "A long announcement that X truncates in retweets…",
                     "note_tweet": {"text": "A long announcement that X truncates in retweets, shown in full thanks to note_tweet. #XAPI",
                                    "entities": {"hashtags": [{"start": 82, "end": 87, "tag": "XAPI"}]}},
                     "public_metrics": {"retweet_count": 12, "reply_count": 4, "like_count": 99, "quote_count": 5}},
                    {"id": "1212092627178287104", "author_id": "783214", "text": "Parent post",
                     "edit_history_tweet_ids": ["1212092627178287104"]},
                    {"id": "1212092628029698048", "author_id": "2244994945", "created_at": "2019-12-31T19:26:16.000Z",
                     "text": "@X We believe the best future version of our API will come from building it with YOU",
                     "edit_history_tweet_ids": ["1212092628029698048"]}
                ],
                "media": [
                    {"media_key": "3_1211797899316740096", "type": "photo", "url": "https://pbs.twimg.com/media/EYeX7akWsAIP1_1.jpg",
                     "width": 1920, "height": 1080, "alt_text": "Rugged hills along the Na Pali coast on the island of Kauai"}
                ]
            },
            "meta": {"newest_id": "1300000000000000002", "oldest_id": "1212092628029698048", "result_count": 3,
                     "next_token": "7140dibdnow9c7btw3w29grvxfcgvpb9n9coehpk7xz5i"}
        })
    }

    #[tokio::test]
    async fn maps_home_timeline() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/users/2244994945/timelines/reverse_chronological"))
            .and(header("authorization", "Bearer tok"))
            .and(query_param("max_results", "20"))
            .and(query_param("pagination_token", "page2"))
            .and(query_param("expansions", EXPANSIONS))
            .and(query_param("tweet.fields", TWEET_FIELDS))
            .respond_with(ResponseTemplate::new(200).set_body_json(timeline_response()))
            .mount(&server)
            .await;
        let page = stream(&ctx(&server), &account(), &Credentials::bearer("tok"), "home", None, Some("page2"))
            .await
            .unwrap();
        assert_eq!(page.cursor.as_deref(), Some("7140dibdnow9c7btw3w29grvxfcgvpb9n9coehpk7xz5i"));
        assert_eq!(page.items.len(), 3);

        let own = &page.items[0];
        assert_eq!(own.id, "1212092628029698048");
        assert_eq!(own.url.as_deref(), Some("https://x.com/TwitterDev/status/1212092628029698048"));
        assert_eq!(own.author.handle, "@TwitterDev");
        assert_eq!(own.author.name, "Twitter Dev");
        assert!(own.author.avatar.as_deref().unwrap().ends_with("_normal.jpg"));
        assert_eq!(
            own.text,
            "@X We believe the best future version of our API will come from building it with YOU & #devs https://developer.x.com/en/blog"
        );
        let html = own.html.as_deref().unwrap();
        assert!(html.contains("<a href=\"https://x.com/X\">@X</a>"), "{html}");
        assert!(html.contains("YOU &amp; <a href=\"https://x.com/hashtag/devs\">#devs</a>"), "{html}");
        assert!(html.contains("<a href=\"https://developer.x.com/en/blog\">developer.x.com/en/blog</a>"), "{html}");
        assert_eq!(own.media.len(), 1);
        assert_eq!(own.media[0].kind, "image");
        assert_eq!(own.media[0].alt.as_deref(), Some("Rugged hills along the Na Pali coast on the island of Kauai"));
        let card = own.link.as_ref().unwrap();
        assert_eq!(card.title.as_deref(), Some("Developer blog"));
        assert_eq!(card.image.as_deref(), Some("https://pbs.twimg.com/news_img/1.jpg"));
        assert_eq!(own.context.as_deref(), Some("Replying to @X"));
        assert_eq!(own.counts.likes, Some(38));
        assert_eq!(own.counts.reposts, Some(7));
        assert_eq!(own.counts.quotes, Some(1));
        assert!(own.actions.delete && own.actions.reply && own.actions.repost && !own.actions.like);
        assert_eq!(own.target["id"], json!("1212092628029698048"));

        let repost = &page.items[1];
        assert_eq!(repost.id, "1300000000000000001");
        assert_eq!(repost.author.handle, "@XDevelopers");
        assert_eq!(repost.reposted_by.as_ref().unwrap().handle, "@API");
        assert_eq!(repost.text, "A long announcement that X truncates in retweets, shown in full thanks to note_tweet. #XAPI");
        assert_eq!(repost.counts.likes, Some(99));
        assert_eq!(repost.target["id"], json!("1300000000000000000"));
        assert!(!repost.actions.reply && !repost.actions.delete && repost.actions.repost);

        let quote = &page.items[2];
        assert_eq!(quote.text, "Worth reading");
        assert!(!quote.actions.repost, "protected accounts can't be reposted");
        // Quoting one of the account's posts summons it, so replying is allowed.
        assert!(quote.actions.reply);
        let quoted = quote.quote.as_ref().unwrap();
        assert_eq!(quoted.id, "1212092628029698048");
        assert_eq!(quoted.author.handle, "@TwitterDev");
    }

    #[tokio::test]
    async fn mentions_are_notifications() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/users/2244994945/mentions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    {"id": "1400000000000000001", "author_id": "783214", "created_at": "2026-11-02T10:00:00.000Z",
                     "text": "@TwitterDev hello", "edit_history_tweet_ids": ["1400000000000000001"],
                     "entities": {"mentions": [{"start": 0, "end": 11, "username": "TwitterDev", "id": "2244994945"}]}},
                    {"id": "1400000000000000002", "author_id": "783214", "created_at": "2026-11-02T11:00:00.000Z",
                     "text": "@TwitterDev agreed", "in_reply_to_user_id": "2244994945",
                     "referenced_tweets": [{"type": "replied_to", "id": "1212092628029698048"}],
                     "edit_history_tweet_ids": ["1400000000000000002"]},
                    {"id": "1400000000000000003", "author_id": "783214", "created_at": "2026-11-02T12:00:00.000Z",
                     "text": "RT @TwitterDev: my post", "referenced_tweets": [{"type": "retweeted", "id": "1212092628029698048"}],
                     "edit_history_tweet_ids": ["1400000000000000003"]}
                ],
                "includes": {
                    "users": [{"id": "783214", "name": "X", "username": "X"},
                              {"id": "2244994945", "name": "Twitter Dev", "username": "TwitterDev"}],
                    "tweets": [{"id": "1212092628029698048", "author_id": "2244994945", "text": "my post",
                                "edit_history_tweet_ids": ["1212092628029698048"]}]
                },
                "meta": {"result_count": 3, "newest_id": "1400000000000000003", "oldest_id": "1400000000000000001"}
            })))
            .mount(&server)
            .await;
        let page = stream(&ctx(&server), &account(), &Credentials::bearer("tok"), "mentions", None, None)
            .await
            .unwrap();
        assert_eq!(page.cursor, None);
        let mention = &page.items[0];
        let n = mention.notification.as_ref().unwrap();
        assert_eq!((n.kind, n.label.as_str()), (NotificationKind::Mention, "mentioned you"));
        assert!(mention.actions.reply);
        assert_eq!(mention.author.handle, "@X");
        let reply = page.items[1].notification.as_ref().unwrap();
        assert_eq!((reply.kind, reply.label.as_str()), (NotificationKind::Reply, "replied to you"));
        assert_eq!(page.items[1].context.as_deref(), Some("Replying to @TwitterDev"));
        let repost = &page.items[2];
        let n = repost.notification.as_ref().unwrap();
        assert_eq!(n.kind, NotificationKind::Repost);
        assert_eq!(n.subject.as_deref(), Some("my post"));
        assert_eq!(repost.author.handle, "@X");
        assert_eq!(repost.url.as_deref(), Some("https://x.com/TwitterDev/status/1212092628029698048"));
    }

    #[tokio::test]
    async fn reposts_likes_and_deletes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/2/users/2244994945/retweets"))
            .and(body_partial_json(json!({"tweet_id": "1228393702244134912"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"retweeted": true, "rest_id": "1228393702244134913"}})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/2/users/2244994945/retweets/1228393702244134912"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"retweeted": false}})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/2/users/2244994945/likes"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "title": "Forbidden", "type": "about:blank", "status": 403,
                "detail": "You are not permitted to perform this action."
            })))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/2/users/2244994945/likes/1228393702244134912"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"liked": false}})))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/2/tweets/1346889436626259968"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"deleted": true}})))
            .expect(1)
            .mount(&server)
            .await;
        let (ctx, acc, creds) = (ctx(&server), account(), Credentials::bearer("tok"));
        let target = json!({"id": "1228393702244134912", "authorId": "783214"});
        let t = interact(&ctx, &acc, &creds, &target, Interaction::Repost).await.unwrap();
        assert_eq!(t["reposted"], json!(true));
        assert_eq!(t["authorId"], json!("783214"));
        let t = interact(&ctx, &acc, &creds, &t, Interaction::Unrepost).await.unwrap();
        assert_eq!(t["reposted"], json!(false));
        match interact(&ctx, &acc, &creds, &target, Interaction::Like).await {
            Err(ProviderError::Forbidden(m)) => assert!(m.contains("April 2026"), "{m}"),
            other => panic!("{other:?}"),
        }
        let t = interact(&ctx, &acc, &creds, &target, Interaction::Unlike).await.unwrap();
        assert_eq!(t["liked"], json!(false));
        assert!(interact(&ctx, &acc, &creds, &json!({"id": "1/../../x"}), Interaction::Repost).await.is_err());
        delete(&ctx, &acc, &creds, "1346889436626259968").await.unwrap();
        assert!(delete(&ctx, &acc, &creds, "../users").await.is_err());
    }

    #[tokio::test]
    async fn profile_reads_counts_and_long_post_limit() {
        let server = MockServer::start().await;
        let mut me = me_response();
        me["data"]["subscription_type"] = json!("Premium");
        me["data"]["public_metrics"] = json!({"followers_count": 10, "following_count": 2, "post_count": 33});
        Mock::given(method("GET"))
            .and(path("/2/users/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(me))
            .mount(&server)
            .await;
        let p = profile(&ctx(&server), &account(), &Credentials::bearer("tok")).await.unwrap();
        assert_eq!(p.stats, ProfileStats { followers: Some(10), following: Some(2), posts: Some(33) });
        assert_eq!(p.meta["maxChars"], json!(25_000));
        assert_eq!(p.handle.as_deref(), Some("@TwitterDev"));
    }

    #[tokio::test]
    async fn maps_x_errors_to_clear_messages() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/2/tweets"))
            .respond_with(ResponseTemplate::new(402).set_body_json(json!({
                "title": "CreditsDepleted", "type": "https://api.x.com/2/problems/credits",
                "detail": "Your enrolled account does not have any credits to fulfill this request."
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2/users/me"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "title": "Unauthorized", "type": "about:blank", "status": 401, "detail": "Unauthorized"
            })))
            .mount(&server)
            .await;
        let (ctx, acc, creds) = (ctx(&server), account(), Credentials::bearer("tok"));
        match publish(&ctx, &acc, &creds, &outgoing("hi", vec![], None)).await {
            Err(ProviderError::Forbidden(m)) => assert!(m.contains("console.x.com"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(profile(&ctx, &acc, &creds).await, Err(ProviderError::Unauthorized(_))));

        let forbidden = |body: Value| x_error(StatusCode::FORBIDDEN, &body.to_string(), None);
        let e = forbidden(json!({
            "client_id": "12345678", "title": "Client Forbidden", "reason": "client-not-enrolled",
            "required_enrollment": "Appropriate Level of API Access",
            "registration_url": "https://developer.x.com/en/docs/projects/overview",
            "detail": "When authenticating requests to the Twitter API v2 endpoints, you must use keys and tokens from a Twitter developer App that is attached to a Project. You can create a project via the developer portal.",
            "type": "https://api.twitter.com/2/problems/client-forbidden"
        }));
        assert!(matches!(&e, ProviderError::Forbidden(m) if m.contains("Developer Console")), "{e:?}");
        let e = forbidden(json!({"title": "Forbidden", "type": "about:blank", "status": 403,
            "detail": "You are not allowed to create a Tweet with duplicate content."}));
        assert!(matches!(e, ProviderError::Invalid(_)), "{e:?}");
        let e = forbidden(json!({"title": "Forbidden", "type": "about:blank", "status": 403,
            "detail": "Reply to this conversation is not allowed because you have not been mentioned or otherwise engaged by the author of the post you are replying to."}));
        assert_eq!(e, ProviderError::Forbidden(REPLY_RESTRICTED.into()));
        let e = x_error(
            StatusCode::TOO_MANY_REQUESTS,
            &json!({"title": "UsageCapExceeded", "detail": "Usage cap exceeded: Monthly product cap",
                    "type": "https://api.twitter.com/2/problems/usage-capped", "period": "Monthly", "scope": "Product"})
            .to_string(),
            Some(60),
        );
        assert!(matches!(&e, ProviderError::RateLimited { message, retry_after: Some(60) } if message.contains("usage cap")), "{e:?}");
        let e = x_error(StatusCode::BAD_REQUEST, &json!({"errors": [{"message": "Your Tweet text is too long."}]}).to_string(), None);
        assert_eq!(e, ProviderError::Invalid("Your Tweet text is too long.".into()));
    }
}
