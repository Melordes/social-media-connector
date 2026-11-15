//! Mastodon and servers with a Mastodon-compatible API.
//!
//! Sign-in: the app registers itself on the user's server
//! (`POST /api/v1/apps`), then uses the OAuth authorization code flow with
//! PKCE and the loopback redirect. Tokens do not expire.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::{Ctx, OAuthBegin, OAuthStart, expect_code, expect_state, js, ju};
use crate::error::{PResult, ProviderError};
use crate::http::{self, host_of, normalize_server, send, send_json, send_ok};
use crate::model::*;
use crate::text::{self, escape_html, html_to_text};

const SCOPES: &str = "read write";
const APP_NAME: &str = "Social Media Connector";

fn auth(req: reqwest::RequestBuilder, creds: &Credentials) -> reqwest::RequestBuilder {
    req.bearer_auth(creds.token())
}

pub async fn begin(ctx: &Ctx, b: &OAuthBegin) -> PResult<OAuthStart> {
    let server = normalize_server(
        b.field("server")
            .ok_or_else(|| ProviderError::invalid("Enter the server your Mastodon account is on."))?,
    )?;
    let req = ctx.http.client.post(format!("{server}/api/v1/apps")).form(&[
        ("client_name", APP_NAME),
        ("redirect_uris", b.redirect_uri.as_str()),
        ("scopes", SCOPES),
    ]);
    let app: Value = match send_json(req, "Mastodon app registration").await {
        Ok(v) => v,
        Err(ProviderError::NotFound(_)) => {
            return Err(ProviderError::invalid(format!(
                "{} does not look like a Mastodon server.",
                host_of(&server)
            )));
        }
        Err(e) => return Err(e),
    };
    let client_id = js(&app, "/client_id").ok_or_else(|| ProviderError::parse("Mastodon app registration", "no client_id"))?;
    let client_secret = js(&app, "/client_secret").unwrap_or_default();
    let auth_url = crate::oauth::with_query(
        &format!("{server}/oauth/authorize"),
        &[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", &b.redirect_uri),
            ("scope", SCOPES),
            ("state", &b.state),
            ("code_challenge", &b.pkce.challenge),
            ("code_challenge_method", "S256"),
        ],
    );
    Ok(OAuthStart {
        auth_url,
        flow: json!({ "server": server, "clientId": client_id, "clientSecret": client_secret }),
    })
}

pub async fn finish(ctx: &Ctx, b: &OAuthBegin, flow: &Value, params: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    expect_state(params, &b.state)?;
    let code = expect_code(params)?;
    let server = js(flow, "/server").unwrap_or_default();
    let client_id = js(flow, "/clientId").unwrap_or_default();
    let client_secret = js(flow, "/clientSecret").unwrap_or_default();
    let req = ctx.http.client.post(format!("{server}/oauth/token")).form(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", &client_id),
        ("client_secret", &client_secret),
        ("redirect_uri", &b.redirect_uri),
        ("code_verifier", &b.pkce.verifier),
        ("scope", SCOPES),
    ]);
    let token: Value = send_json(req, "Mastodon token").await?;
    let access = js(&token, "/access_token").ok_or_else(|| ProviderError::parse("Mastodon token", "no access_token"))?;
    let creds = Credentials {
        access_token: Some(access),
        client_id: Some(client_id),
        client_secret: Some(client_secret),
        ..Default::default()
    };
    let me: Value = send_json(
        auth(ctx.http.client.get(format!("{server}/api/v1/accounts/verify_credentials")), &creds),
        "Mastodon account",
    )
    .await?;
    let meta = instance_limits(ctx, &server).await;
    let host = host_of(&server);
    let username = js(&me, "/username").unwrap_or_default();
    Ok(vec![NewAccount {
        platform: Platform::Mastodon,
        server: server.clone(),
        remote_id: js(&me, "/id").unwrap_or_default(),
        handle: format!("@{username}@{host}"),
        display_name: non_empty(js(&me, "/display_name")).unwrap_or_else(|| username.clone()),
        avatar_url: js(&me, "/avatar_static").or_else(|| js(&me, "/avatar")),
        profile_url: js(&me, "/url"),
        meta: Value::Object(meta),
        credentials: creds,
    }])
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

/// Reads the server's posting limits (v2 instance API, falling back to v1
/// and to Pleroma/Akkoma fields). Failures leave the defaults in place.
async fn instance_limits(ctx: &Ctx, server: &str) -> serde_json::Map<String, Value> {
    let mut meta = serde_json::Map::new();
    let get = |path: &str| ctx.http.client.get(format!("{server}{path}")).timeout(Duration::from_secs(20));
    let inst: Option<Value> = match send_json::<Value>(get("/api/v2/instance"), "instance").await {
        Ok(v) => Some(v),
        Err(_) => send_json::<Value>(get("/api/v1/instance"), "instance").await.ok(),
    };
    let Some(inst) = inst else { return meta };
    let max_chars = ju(&inst, "/configuration/statuses/max_characters")
        .or_else(|| ju(&inst, "/max_toot_chars"))
        .or_else(|| ju(&inst, "/pleroma/metadata/fields_limits/max_toot_chars"));
    if let Some(v) = max_chars {
        meta.insert("maxChars".into(), v.into());
    }
    if let Some(v) = ju(&inst, "/configuration/statuses/max_media_attachments") {
        meta.insert("maxImages".into(), v.into());
    }
    if let Some(v) = ju(&inst, "/configuration/media_attachments/image_size_limit").or_else(|| ju(&inst, "/upload_limit")) {
        meta.insert("imageMaxBytes".into(), v.into());
    }
    if let Some(v) = ju(&inst, "/configuration/media_attachments/description_limit") {
        meta.insert("altTextMax".into(), v.into());
    }
    if let Some(v) = js(&inst, "/version") {
        meta.insert("serverVersion".into(), v.into());
    }
    meta
}

pub async fn profile(ctx: &Ctx, account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    let server = &account.server;
    let me: Value = send_json(
        auth(ctx.http.client.get(format!("{server}/api/v1/accounts/verify_credentials")), creds),
        "Mastodon account",
    )
    .await?;
    let username = js(&me, "/username").unwrap_or_default();
    Ok(ProfileUpdate {
        handle: Some(format!("@{username}@{}", host_of(server))),
        display_name: Some(non_empty(js(&me, "/display_name")).unwrap_or(username)),
        avatar_url: js(&me, "/avatar_static").or_else(|| js(&me, "/avatar")),
        profile_url: js(&me, "/url"),
        stats: ProfileStats {
            followers: ju(&me, "/followers_count"),
            following: ju(&me, "/following_count"),
            posts: ju(&me, "/statuses_count"),
        },
        meta: instance_limits(ctx, server).await,
    })
}

pub fn validate(account: &Account, post: &OutgoingPost) -> Vec<String> {
    // Mastodon counts the content warning towards the limit.
    let mut problems = Vec::new();
    if let Some(cw) = post.options.content_warning.as_deref().filter(|s| !s.trim().is_empty()) {
        let max = super::account_limits(account).max_chars as usize;
        let total = text::count(&post.text, text::CountMode::Mastodon) + text::graphemes(cw);
        if total > max {
            problems.push(format!(
                "Text and content warning together are {total} characters; this server allows {max}."
            ));
        }
    }
    problems
}

async fn upload_media(ctx: &Ctx, account: &Account, creds: &Credentials, post: &OutgoingPost) -> PResult<Vec<String>> {
    let server = &account.server;
    let mut ids = Vec::new();
    for (img, alt) in super::prepare_images(account, post)? {
        let part = reqwest::multipart::Part::bytes(img.bytes)
            .file_name(img.file_name.clone())
            .mime_str(&img.mime)
            .map_err(|e| ProviderError::other(e.to_string()))?;
        let mut form = reqwest::multipart::Form::new().part("file", part);
        if !alt.trim().is_empty() {
            form = form.text("description", alt.clone());
        }
        let resp = send_ok(auth(ctx.http.client.post(format!("{server}/api/v2/media")), creds).multipart(form)).await?;
        let status = resp.status().as_u16();
        let media: Value = http::read_json(resp, "Mastodon media").await?;
        let id = js(&media, "/id").ok_or_else(|| ProviderError::parse("Mastodon media", "no id"))?;
        if status == 202 || media.get("url").map(Value::is_null).unwrap_or(false) {
            wait_for_media(ctx, server, creds, &id).await?;
        }
        ids.push(id);
    }
    Ok(ids)
}

/// Large uploads are processed in the background; wait until ready.
async fn wait_for_media(ctx: &Ctx, server: &str, creds: &Credentials, id: &str) -> PResult<()> {
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let resp = send(auth(ctx.http.client.get(format!("{server}/api/v1/media/{id}")), creds)).await?;
        match resp.status().as_u16() {
            200 => return Ok(()),
            206 => continue,
            _ => {
                http::check(resp).await?;
            }
        }
    }
    Err(ProviderError::Server("The server is still processing the image. Try again in a moment.".into()))
}

fn visibility(v: Option<&str>) -> &'static str {
    match v {
        Some("unlisted") => "unlisted",
        Some("followers") | Some("private") => "private",
        Some("direct") => "direct",
        _ => "public",
    }
}

pub async fn publish(ctx: &Ctx, account: &Account, creds: &Credentials, post: &OutgoingPost) -> PResult<PublishResult> {
    let server = &account.server;
    let media_ids = upload_media(ctx, account, creds, post).await?;
    let mut body = json!({
        "status": post.text,
        "visibility": visibility(post.options.visibility.as_deref()),
    });
    if !media_ids.is_empty() {
        body["media_ids"] = json!(media_ids);
        if post.options.sensitive {
            body["sensitive"] = json!(true);
        }
    }
    if let Some(cw) = post.options.content_warning.as_deref().filter(|s| !s.trim().is_empty()) {
        body["spoiler_text"] = json!(cw);
        body["sensitive"] = json!(true);
    }
    if let Some(lang) = post.options.language.as_deref().filter(|s| !s.is_empty()) {
        body["language"] = json!(lang);
    }
    if let Some(reply) = &post.reply_to
        && let Some(id) = js(reply, "/id") {
            body["in_reply_to_id"] = json!(id);
        }
    let req = auth(ctx.http.client.post(format!("{server}/api/v1/statuses")), creds)
        .header("Idempotency-Key", &post.idempotency_key)
        .json(&body);
    let status: Value = send_json(req, "Mastodon post").await?;
    Ok(PublishResult {
        remote_id: js(&status, "/id").unwrap_or_default(),
        url: js(&status, "/url").or_else(|| js(&status, "/uri")),
    })
}

pub async fn stream(
    ctx: &Ctx,
    account: &Account,
    creds: &Credentials,
    kind: &str,
    param: Option<&str>,
    cursor: Option<&str>,
) -> PResult<FeedPage> {
    let server = &account.server;
    let mut query: Vec<(String, String)> = vec![("limit".into(), "30".into())];
    if let Some(c) = cursor {
        query.push(("max_id".into(), c.to_string()));
    }
    let path = match kind {
        "home" => "/api/v1/timelines/home".to_string(),
        "local" => {
            query.push(("local".into(), "true".into()));
            "/api/v1/timelines/public".to_string()
        }
        "federated" => "/api/v1/timelines/public".to_string(),
        "hashtag" => {
            let tag = param.unwrap_or_default().trim().trim_start_matches('#');
            if tag.is_empty() {
                return Err(ProviderError::invalid("Enter a hashtag."));
            }
            format!("/api/v1/timelines/tag/{}", urlencode(tag))
        }
        "me" => format!("/api/v1/accounts/{}/statuses", account.remote_id),
        "notifications" => "/api/v1/notifications".to_string(),
        "mentions" => {
            query.push(("types[]".into(), "mention".into()));
            "/api/v1/notifications".to_string()
        }
        other => return Err(ProviderError::unsupported(format!("Unknown stream {other}"))),
    };
    let req = auth(ctx.http.client.get(format!("{server}{path}")), creds).query(&query);
    let list: Vec<Value> = send_json(req, "Mastodon timeline").await?;
    let own_id = account.remote_id.as_str();
    let host = host_of(server);
    let items: Vec<FeedItem> = if kind == "notifications" || kind == "mentions" {
        list.iter().map(|n| notification_item(n, own_id, &host)).collect()
    } else {
        list.iter().map(|s| status_item(s, own_id, &host)).collect()
    };
    let cursor = list.last().and_then(|v| js(v, "/id"));
    Ok(FeedPage { cursor: if list.is_empty() { None } else { cursor }, items })
}

fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn author(a: &Value, host: &str) -> Author {
    let acct = js(a, "/acct").unwrap_or_default();
    let handle = if acct.contains('@') { format!("@{acct}") } else { format!("@{acct}@{host}") };
    Author {
        name: non_empty(js(a, "/display_name")).unwrap_or_else(|| js(a, "/username").unwrap_or_default()),
        handle,
        avatar: js(a, "/avatar_static").or_else(|| js(a, "/avatar")),
        url: js(a, "/url"),
    }
}

/// Replaces `:shortcode:` custom emoji with images.
fn emojify(html: &str, emojis: Option<&Value>) -> String {
    let Some(list) = emojis.and_then(Value::as_array) else { return html.to_string() };
    let mut out = html.to_string();
    for e in list {
        let (Some(code), Some(url)) = (js(e, "/shortcode"), js(e, "/static_url").or_else(|| js(e, "/url"))) else {
            continue;
        };
        let tag = format!(
            "<img class=\"emoji\" src=\"{}\" alt=\":{}:\" title=\":{}:\">",
            escape_html(&url),
            escape_html(&code),
            escape_html(&code)
        );
        out = out.replace(&format!(":{code}:"), &tag);
    }
    out
}

fn media_view(m: &Value) -> MediaView {
    let kind = match js(m, "/type").as_deref() {
        Some("image") => "image",
        Some("gifv") => "gif",
        Some("video") => "video",
        Some("audio") => "audio",
        _ => "other",
    };
    MediaView {
        kind: kind.into(),
        url: js(m, "/url").or_else(|| js(m, "/remote_url")).unwrap_or_default(),
        preview_url: js(m, "/preview_url"),
        alt: js(m, "/description"),
        width: ju(m, "/meta/original/width").map(|v| v as u32),
        height: ju(m, "/meta/original/height").map(|v| v as u32),
    }
}

pub(crate) fn status_item(s: &Value, own_id: &str, host: &str) -> FeedItem {
    let reblog = s.get("reblog").filter(|r| r.is_object());
    let st = reblog.unwrap_or(s);
    let content = js(st, "/content").unwrap_or_default();
    let reply_ctx = js(st, "/in_reply_to_account_id").map(|reply_acc| {
        let acct = st
            .get("mentions")
            .and_then(Value::as_array)
            .and_then(|m| m.iter().find(|x| js(x, "/id").as_deref() == Some(reply_acc.as_str())))
            .and_then(|m| js(m, "/acct"));
        match acct {
            Some(a) => format!("Replying to @{a}"),
            None if js(st, "/account/id").as_deref() == Some(reply_acc.as_str()) => "Continued thread".to_string(),
            None => "Reply".to_string(),
        }
    });
    let visibility = js(st, "/visibility").unwrap_or_else(|| "public".into());
    let card = st.get("card").filter(|c| c.is_object()).map(|c| LinkCard {
        url: js(c, "/url").unwrap_or_default(),
        title: non_empty(js(c, "/title")),
        description: non_empty(js(c, "/description")),
        image: js(c, "/image"),
    });
    let quote = st
        .pointer("/quote/quoted_status")
        .filter(|q| q.is_object())
        .map(|q| Box::new(status_item(q, own_id, host)));
    FeedItem {
        id: js(s, "/id").unwrap_or_default(),
        account_id: String::new(),
        url: js(st, "/url").or_else(|| js(st, "/uri")),
        author: author(&st["account"], host),
        created_at: js(st, "/created_at").unwrap_or_default(),
        text: html_to_text(&content),
        html: Some(emojify(&content, st.get("emojis"))),
        title: None,
        content_warning: non_empty(js(st, "/spoiler_text")),
        sensitive: st.get("sensitive").and_then(Value::as_bool).unwrap_or(false),
        media: st
            .get("media_attachments")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(media_view).collect())
            .unwrap_or_default(),
        link: card,
        quote,
        reposted_by: reblog.map(|_| author(&s["account"], host)),
        context: reply_ctx,
        counts: Counts {
            replies: ju(st, "/replies_count"),
            reposts: ju(st, "/reblogs_count"),
            likes: ju(st, "/favourites_count"),
            quotes: ju(st, "/quotes_count"),
            score: None,
        },
        viewer: ViewerState {
            liked: st.get("favourited").and_then(Value::as_bool).unwrap_or(false),
            reposted: st.get("reblogged").and_then(Value::as_bool).unwrap_or(false),
        },
        actions: ItemActions {
            like: true,
            repost: visibility == "public" || visibility == "unlisted",
            reply: true,
            delete: js(st, "/account/id").as_deref() == Some(own_id),
        },
        target: json!({
            "id": js(st, "/id"),
            "url": js(st, "/url"),
            "acct": js(st, "/account/acct"),
        }),
        notification: None,
    }
}

fn notification_item(n: &Value, own_id: &str, host: &str) -> FeedItem {
    let kind_s = js(n, "/type").unwrap_or_default();
    let status = n.get("status").filter(|s| s.is_object());
    let (kind, label) = match kind_s.as_str() {
        "mention" => {
            let is_reply = status.and_then(|s| js(s, "/in_reply_to_account_id")).as_deref() == Some(own_id);
            if is_reply {
                (NotificationKind::Reply, "replied to you")
            } else {
                (NotificationKind::Mention, "mentioned you")
            }
        }
        "favourite" => (NotificationKind::Like, "favourited your post"),
        "reblog" => (NotificationKind::Repost, "boosted your post"),
        "follow" => (NotificationKind::Follow, "followed you"),
        "follow_request" => (NotificationKind::FollowRequest, "asked to follow you"),
        "poll" => (NotificationKind::Poll, "a poll has ended"),
        "update" => (NotificationKind::Update, "edited a post"),
        "status" => (NotificationKind::Other, "posted"),
        "quote" => (NotificationKind::Quote, "quoted your post"),
        "quoted_update" => (NotificationKind::Update, "edited a post that quotes you"),
        "admin.sign_up" => (NotificationKind::Other, "signed up"),
        "admin.report" => (NotificationKind::Other, "filed a report"),
        "severed_relationships" => (NotificationKind::Other, "some of your connections were removed"),
        "moderation_warning" => (NotificationKind::Other, "you received a moderation warning"),
        _ => (NotificationKind::Other, "sent a notification"),
    };
    let actor = author(&n["account"], host);
    let mut item = match status {
        Some(s) if matches!(kind, NotificationKind::Mention | NotificationKind::Reply | NotificationKind::Quote | NotificationKind::Other | NotificationKind::Update) => {
            let mut it = status_item(s, own_id, host);
            it.reposted_by = None;
            it
        }
        _ => FeedItem {
            author: actor.clone(),
            url: status.and_then(|s| js(s, "/url")).or_else(|| actor.url.clone()),
            actions: ItemActions::default(),
            target: Value::Null,
            ..Default::default()
        },
    };
    let subject = match kind {
        NotificationKind::Like | NotificationKind::Repost | NotificationKind::Poll => {
            status.map(|s| text::preview(&html_to_text(&js(s, "/content").unwrap_or_default()), 280))
        }
        _ => None,
    };
    item.id = js(n, "/id").unwrap_or_default();
    item.author = actor;
    item.created_at = js(n, "/created_at").unwrap_or_default();
    item.notification = Some(NotificationInfo { kind, label: label.into(), subject });
    item
}

pub async fn interact(ctx: &Ctx, account: &Account, creds: &Credentials, target: &Value, action: Interaction) -> PResult<Value> {
    let id = js(target, "/id").ok_or_else(|| ProviderError::invalid("This post can't be acted on."))?;
    let verb = match action {
        Interaction::Like => "favourite",
        Interaction::Unlike => "unfavourite",
        Interaction::Repost => "reblog",
        Interaction::Unrepost => "unreblog",
    };
    let req = auth(
        ctx.http.client.post(format!("{}/api/v1/statuses/{id}/{verb}", account.server)),
        creds,
    );
    send_ok(req).await?;
    Ok(target.clone())
}

pub async fn delete(ctx: &Ctx, account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    let req = auth(
        ctx.http.client.delete(format!("{}/api/v1/statuses/{remote_id}", account.server)),
        creds,
    );
    send_ok(req).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::Pkce;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn account(server: &str) -> Account {
        Account {
            id: "a1".into(),
            platform: Platform::Mastodon,
            server: server.into(),
            remote_id: "100".into(),
            handle: "@me@example".into(),
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

    fn sample_status(id: &str) -> Value {
        json!({
            "id": id, "created_at": "2026-11-01T10:00:00.000Z", "in_reply_to_id": null, "in_reply_to_account_id": null,
            "sensitive": false, "spoiler_text": "", "visibility": "public", "language": "en",
            "uri": format!("https://example/users/me/statuses/{id}"), "url": format!("https://example/@me/{id}"),
            "replies_count": 1, "reblogs_count": 2, "favourites_count": 3, "favourited": true, "reblogged": false,
            "content": "<p>Hello :wave: <a href=\"https://example.com\">example.com</a></p>",
            "reblog": null,
            "account": {"id": "100", "username": "me", "acct": "me", "display_name": "Me", "url": "https://example/@me",
                        "avatar": "https://example/a.png", "avatar_static": "https://example/a.png"},
            "media_attachments": [{"id": "9", "type": "image", "url": "https://example/m.png", "preview_url": "https://example/p.png",
                                   "description": "A cat", "meta": {"original": {"width": 640, "height": 480}}}],
            "mentions": [], "tags": [], "emojis": [{"shortcode": "wave", "url": "https://example/e.png", "static_url": "https://example/e.png"}],
            "card": null, "poll": null
        })
    }

    #[tokio::test]
    async fn full_sign_in_flow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/apps"))
            .and(body_string_contains("redirect_uris=http%3A%2F%2F127.0.0.1%3A5555%2Fcallback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"client_id": "cid", "client_secret": "csec"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("code=thecode"))
            .and(body_string_contains("code_verifier="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "tok", "token_type": "Bearer"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/accounts/verify_credentials"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "100", "username": "me", "acct": "me", "display_name": "", "url": "https://example/@me",
                "avatar": "https://example/a.png", "followers_count": 5
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v2/instance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "4.5.0",
                "configuration": {"statuses": {"max_characters": 1000, "max_media_attachments": 4},
                                   "media_attachments": {"image_size_limit": 10485760, "description_limit": 1500}}
            })))
            .mount(&server)
            .await;

        let ctx = Ctx::default();
        let b = OAuthBegin {
            platform: Platform::Mastodon,
            fields: HashMap::from([("server".to_string(), server.uri())]),
            redirect_uri: "http://127.0.0.1:5555/callback".into(),
            state: "st".into(),
            pkce: Pkce::new(),
            client_id: None,
            client_secret: None,
        };
        let start = begin(&ctx, &b).await.unwrap();
        assert!(start.auth_url.starts_with(&format!("{}/oauth/authorize?", server.uri())));
        assert!(start.auth_url.contains("client_id=cid"));
        assert!(start.auth_url.contains("code_challenge_method=S256"));

        // Wrong state is rejected.
        let bad = HashMap::from([("code".to_string(), "thecode".to_string()), ("state".to_string(), "other".to_string())]);
        assert!(finish(&ctx, &b, &start.flow, &bad).await.is_err());

        let params = HashMap::from([("code".to_string(), "thecode".to_string()), ("state".to_string(), "st".to_string())]);
        let accounts = finish(&ctx, &b, &start.flow, &params).await.unwrap();
        assert_eq!(accounts.len(), 1);
        let a = &accounts[0];
        assert_eq!(a.remote_id, "100");
        assert_eq!(a.display_name, "me");
        assert!(a.handle.starts_with("@me@127.0.0.1:"));
        assert_eq!(a.meta["maxChars"], json!(1000));
        assert_eq!(a.credentials.access_token.as_deref(), Some("tok"));
    }

    #[tokio::test]
    async fn publishes_with_media_and_options() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/media"))
            .and(crate::testutil::bytes_contains("A description"))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({"id": "m1", "url": null})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/media/m1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m1", "url": "https://x/m1.png"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/statuses"))
            .and(header("idempotency-key", "post-1"))
            .and(body_string_contains("\"media_ids\":[\"m1\"]"))
            .and(body_string_contains("\"visibility\":\"private\""))
            .and(body_string_contains("\"spoiler_text\":\"cw\""))
            .and(body_string_contains("\"in_reply_to_id\":\"55\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_status("777")))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let png = {
            let img = image::RgbImage::from_pixel(10, 10, image::Rgb([200, 10, 10]));
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
            buf
        };
        let mut att = crate::media::import_bytes(dir.path(), &png, "red.png").unwrap();
        att.alt = "A description".into();
        let post = OutgoingPost {
            text: "Hello".into(),
            media: vec![att],
            options: PostOptions {
                visibility: Some("followers".into()),
                content_warning: Some("cw".into()),
                ..Default::default()
            },
            reply_to: Some(json!({"id": "55"})),
            idempotency_key: "post-1".into(),
        };
        let acc = account(&server.uri());
        let res = publish(&Ctx::default(), &acc, &Credentials::bearer("tok"), &post).await.unwrap();
        assert_eq!(res.remote_id, "777");
        assert_eq!(res.url.as_deref(), Some("https://example/@me/777"));
    }

    #[tokio::test]
    async fn maps_timeline_and_notifications() {
        let server = MockServer::start().await;
        let mut boosted = sample_status("2");
        boosted["reblog"] = sample_status("1");
        boosted["account"]["acct"] = json!("friend@other.social");
        Mock::given(method("GET"))
            .and(path("/api/v1/timelines/home"))
            .and(query_param("max_id", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([sample_status("3"), boosted])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/notifications"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "n1", "type": "favourite", "created_at": "2026-11-02T10:00:00Z",
                 "account": {"id": "7", "username": "fan", "acct": "fan@far.away", "display_name": "Fan"},
                 "status": sample_status("3")},
                {"id": "n2", "type": "follow", "created_at": "2026-11-02T09:00:00Z",
                 "account": {"id": "8", "username": "new", "acct": "new", "display_name": "New", "url": "https://example/@new"}}
            ])))
            .mount(&server)
            .await;
        let acc = account(&server.uri());
        let ctx = Ctx::default();
        let creds = Credentials::bearer("tok");
        let page = stream(&ctx, &acc, &creds, "home", None, Some("50")).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.cursor.as_deref(), Some("2"));
        let first = &page.items[0];
        assert_eq!(first.text, "Hello :wave: example.com");
        assert!(first.html.as_ref().unwrap().contains("<img class=\"emoji\""));
        assert_eq!(first.counts.likes, Some(3));
        assert!(first.viewer.liked);
        assert!(first.actions.delete);
        assert_eq!(first.media[0].alt.as_deref(), Some("A cat"));
        let boost = &page.items[1];
        assert_eq!(boost.id, "2");
        assert_eq!(boost.target["id"], json!("1"));
        assert_eq!(boost.reposted_by.as_ref().unwrap().handle, "@friend@other.social");

        let notes = stream(&ctx, &acc, &creds, "notifications", None, None).await.unwrap();
        let like = &notes.items[0];
        assert_eq!(like.notification.as_ref().unwrap().kind, NotificationKind::Like);
        assert_eq!(like.author.handle, "@fan@far.away");
        assert_eq!(like.notification.as_ref().unwrap().subject.as_deref(), Some("Hello :wave: example.com"));
        let follow = &notes.items[1];
        assert_eq!(follow.notification.as_ref().unwrap().kind, NotificationKind::Follow);
        assert!(follow.author.handle.starts_with("@new@"));
    }

    #[tokio::test]
    async fn invalid_token_is_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/statuses/5/favourite"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "The access token is invalid"})))
            .mount(&server)
            .await;
        let err = interact(&Ctx::default(), &account(&server.uri()), &Credentials::bearer("x"), &json!({"id": "5"}), Interaction::Like)
            .await
            .unwrap_err();
        assert_eq!(err, ProviderError::Unauthorized("The access token is invalid".into()));
    }

    /// Talks to a real Mastodon server (read-only, public data). Run with
    /// `cargo test -- --ignored live_`.
    #[tokio::test]
    #[ignore]
    async fn live_reads_instance_limits_and_public_statuses() {
        let ctx = Ctx::default();
        let meta = instance_limits(&ctx, "https://mastodon.social").await;
        println!("{meta:?}");
        assert!(meta.get("maxChars").and_then(Value::as_u64).unwrap() >= 500);
        let acct: Value = send_json(
            ctx.http.client.get("https://mastodon.social/api/v1/accounts/lookup").query(&[("acct", "Mastodon")]),
            "lookup",
        )
        .await
        .unwrap();
        let id = js(&acct, "/id").unwrap();
        let list: Vec<Value> = send_json(
            ctx.http.client.get(format!("https://mastodon.social/api/v1/accounts/{id}/statuses")).query(&[("limit", "10")]),
            "statuses",
        )
        .await
        .unwrap();
        assert!(!list.is_empty());
        for s in &list {
            let i = status_item(s, "nobody", "mastodon.social");
            assert!(i.url.is_some());
            assert!(i.html.is_some());
            println!("{} {} | {} | media {} | favs {:?}", i.created_at, i.author.handle, crate::text::preview(&i.text, 60), i.media.len(), i.counts.likes);
        }
    }
}
