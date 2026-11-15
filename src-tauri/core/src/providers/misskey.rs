//! Misskey and its forks (Sharkey, Firefish, CherryPick...).
//!
//! Sign-in uses MiAuth: the user approves a session on their server's page,
//! the browser returns to the loopback callback with `?session=...`, and the
//! app exchanges the session for a token. Every API call is a POST with the
//! token in the JSON body (`i`).

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{Ctx, OAuthBegin, OAuthStart, js, ju};
use crate::error::{PResult, ProviderError};
use crate::http::{host_of, normalize_server, send_json};
use crate::model::*;
use crate::text;

const PERMISSIONS: &str =
    "read:account,write:notes,read:notifications,write:notifications,read:reactions,write:reactions,read:drive,write:drive";
const LIKE_REACTION: &str = "❤️";

async fn api(ctx: &Ctx, server: &str, creds: &Credentials, endpoint: &str, mut body: Value) -> PResult<Value> {
    if let Value::Object(m) = &mut body {
        m.insert("i".into(), json!(creds.token()));
    }
    let req = ctx.http.client.post(format!("{server}/api/{endpoint}")).json(&body);
    let resp = crate::http::send_ok(req).await?;
    let bytes = resp.bytes().await.map_err(ProviderError::from)?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| ProviderError::parse("Misskey", e))
}

pub async fn begin(_ctx: &Ctx, b: &OAuthBegin) -> PResult<OAuthStart> {
    let server = normalize_server(
        b.field("server")
            .ok_or_else(|| ProviderError::invalid("Enter the server your Misskey account is on."))?,
    )?;
    let session = uuid::Uuid::new_v4().to_string();
    let auth_url = crate::oauth::with_query(
        &format!("{server}/miauth/{session}"),
        &[
            ("name", "Social Media Connector"),
            ("callback", &b.redirect_uri),
            ("permission", PERMISSIONS),
        ],
    );
    Ok(OAuthStart { auth_url, flow: json!({ "server": server, "session": session }) })
}

pub async fn finish(ctx: &Ctx, _b: &OAuthBegin, flow: &Value, params: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    let server = js(flow, "/server").unwrap_or_default();
    let session = js(flow, "/session").unwrap_or_default();
    if params.get("session").map(String::as_str) != Some(session.as_str()) {
        return Err(ProviderError::invalid("The sign-in answer did not match this request. Please try again."));
    }
    let v: Value = send_json(
        ctx.http.client.post(format!("{server}/api/miauth/{session}/check")).json(&json!({})),
        "Misskey sign-in",
    )
    .await?;
    if !v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Err(ProviderError::Forbidden("Access was not approved on the server.".into()));
    }
    let token = js(&v, "/token").ok_or_else(|| ProviderError::parse("Misskey sign-in", "no token"))?;
    let creds = Credentials::bearer(token);
    let me = api(ctx, &server, &creds, "i", json!({})).await?;
    let host = host_of(&server);
    let username = js(&me, "/username").unwrap_or_default();
    let meta = server_limits(ctx, &server).await;
    Ok(vec![NewAccount {
        platform: Platform::Misskey,
        server: server.clone(),
        remote_id: js(&me, "/id").unwrap_or_default(),
        handle: format!("@{username}@{host}"),
        display_name: js(&me, "/name").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| username.clone()),
        avatar_url: js(&me, "/avatarUrl"),
        profile_url: Some(format!("{server}/@{username}")),
        meta: Value::Object(meta),
        credentials: creds,
    }])
}

async fn server_limits(ctx: &Ctx, server: &str) -> serde_json::Map<String, Value> {
    let mut meta = serde_json::Map::new();
    if let Ok(v) = send_json::<Value>(ctx.http.client.post(format!("{server}/api/meta")).json(&json!({"detail": false})), "Misskey meta").await {
        if let Some(n) = ju(&v, "/maxNoteTextLength") {
            meta.insert("maxChars".into(), n.into());
        }
        if let Some(n) = js(&v, "/version") {
            meta.insert("serverVersion".into(), n.into());
        }
    }
    meta
}

pub async fn profile(ctx: &Ctx, account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    let me = api(ctx, &account.server, creds, "i", json!({})).await?;
    let username = js(&me, "/username").unwrap_or_default();
    Ok(ProfileUpdate {
        handle: Some(format!("@{username}@{}", host_of(&account.server))),
        display_name: Some(js(&me, "/name").filter(|s| !s.trim().is_empty()).unwrap_or(username.clone())),
        avatar_url: js(&me, "/avatarUrl"),
        profile_url: Some(format!("{}/@{username}", account.server)),
        stats: ProfileStats {
            followers: ju(&me, "/followersCount"),
            following: ju(&me, "/followingCount"),
            posts: ju(&me, "/notesCount"),
        },
        meta: server_limits(ctx, &account.server).await,
    })
}

pub fn validate(_account: &Account, _post: &OutgoingPost) -> Vec<String> {
    Vec::new()
}

pub async fn publish(ctx: &Ctx, account: &Account, creds: &Credentials, post: &OutgoingPost) -> PResult<PublishResult> {
    let server = &account.server;
    let mut file_ids = Vec::new();
    for (img, alt) in super::prepare_images(account, post)? {
        let part = reqwest::multipart::Part::bytes(img.bytes)
            .file_name(img.file_name.clone())
            .mime_str(&img.mime)
            .map_err(|e| ProviderError::other(e.to_string()))?;
        let mut form = reqwest::multipart::Form::new()
            .text("i", creds.token().to_string())
            .text("name", img.file_name.clone())
            .text("isSensitive", if post.options.sensitive { "true" } else { "false" })
            .part("file", part);
        if !alt.trim().is_empty() {
            form = form.text("comment", alt.clone());
        }
        let file: Value = send_json(
            ctx.http.client.post(format!("{server}/api/drive/files/create")).multipart(form),
            "Misskey upload",
        )
        .await?;
        file_ids.push(js(&file, "/id").ok_or_else(|| ProviderError::parse("Misskey upload", "no id"))?);
    }
    let visibility = match post.options.visibility.as_deref() {
        Some("unlisted") => "home",
        Some("followers") => "followers",
        _ => "public",
    };
    let mut body = json!({ "visibility": visibility });
    body["text"] = if post.text.trim().is_empty() { Value::Null } else { json!(post.text) };
    if !file_ids.is_empty() {
        body["fileIds"] = json!(file_ids);
    }
    if let Some(cw) = post.options.content_warning.as_deref().filter(|s| !s.trim().is_empty()) {
        body["cw"] = json!(cw);
    }
    if let Some(id) = post.reply_to.as_ref().and_then(|r| js(r, "/id")) {
        body["replyId"] = json!(id);
    }
    let v = api(ctx, server, creds, "notes/create", body).await?;
    let id = js(&v, "/createdNote/id").ok_or_else(|| ProviderError::parse("Misskey post", "no note id"))?;
    Ok(PublishResult { url: Some(format!("{server}/notes/{id}")), remote_id: id })
}

pub async fn stream(
    ctx: &Ctx,
    account: &Account,
    creds: &Credentials,
    kind: &str,
    _param: Option<&str>,
    cursor: Option<&str>,
) -> PResult<FeedPage> {
    let mut body = json!({ "limit": 30 });
    if let Some(c) = cursor {
        body["untilId"] = json!(c);
    }
    let endpoint = match kind {
        "home" => "notes/timeline",
        "local" => "notes/local-timeline",
        "social" => "notes/hybrid-timeline",
        "global" => "notes/global-timeline",
        "mentions" => "notes/mentions",
        "me" => {
            body["userId"] = json!(account.remote_id);
            "users/notes"
        }
        "notifications" => "i/notifications",
        other => return Err(ProviderError::unsupported(format!("Unknown stream {other}"))),
    };
    let v = api(ctx, &account.server, creds, endpoint, body).await?;
    let list = v.as_array().cloned().unwrap_or_default();
    let host = host_of(&account.server);
    let items = if kind == "notifications" {
        list.iter().map(|n| notification_item(n, account, &host)).collect()
    } else {
        list.iter().map(|n| note_item(n, account, &host)).collect()
    };
    Ok(FeedPage { cursor: list.last().and_then(|n| js(n, "/id")), items })
}

fn author(u: &Value, server_host: &str, server: &str) -> Author {
    let username = js(u, "/username").unwrap_or_default();
    let host = js(u, "/host").unwrap_or_else(|| server_host.to_string());
    Author {
        name: js(u, "/name").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| username.clone()),
        handle: format!("@{username}@{host}"),
        avatar: js(u, "/avatarUrl"),
        url: Some(if js(u, "/host").is_some() {
            format!("{server}/@{username}@{host}")
        } else {
            format!("{server}/@{username}")
        }),
    }
}

fn files(n: &Value) -> Vec<MediaView> {
    n.get("files")
        .and_then(Value::as_array)
        .map(|fs| {
            fs.iter()
                .map(|f| {
                    let mime = js(f, "/type").unwrap_or_default();
                    MediaView {
                        kind: if mime == "image/gif" {
                            "gif"
                        } else if mime.starts_with("image/") {
                            "image"
                        } else if mime.starts_with("video/") {
                            "video"
                        } else if mime.starts_with("audio/") {
                            "audio"
                        } else {
                            "other"
                        }
                        .into(),
                        url: js(f, "/url").unwrap_or_default(),
                        preview_url: js(f, "/thumbnailUrl"),
                        alt: js(f, "/comment").filter(|s| !s.is_empty()),
                        width: ju(f, "/properties/width").map(|v| v as u32),
                        height: ju(f, "/properties/height").map(|v| v as u32),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn reaction_total(n: &Value) -> u64 {
    n.get("reactions")
        .and_then(Value::as_object)
        .map(|m| m.values().filter_map(Value::as_u64).sum())
        .unwrap_or(0)
}

fn plain_note(n: &Value, account: &Account, host: &str) -> FeedItem {
    let server = &account.server;
    let id = js(n, "/id").unwrap_or_default();
    let text = js(n, "/text").unwrap_or_default();
    let visibility = js(n, "/visibility").unwrap_or_else(|| "public".into());
    let has_files = n.get("files").and_then(Value::as_array).map(|a| !a.is_empty()).unwrap_or(false);
    let is_sensitive_files = n
        .get("files")
        .and_then(Value::as_array)
        .map(|a| a.iter().any(|f| f.get("isSensitive").and_then(Value::as_bool).unwrap_or(false)))
        .unwrap_or(false);
    FeedItem {
        id: id.clone(),
        account_id: String::new(),
        url: js(n, "/url").or_else(|| js(n, "/uri")).or_else(|| Some(format!("{server}/notes/{id}"))),
        author: author(&n["user"], host, server),
        created_at: js(n, "/createdAt").unwrap_or_default(),
        text,
        html: None,
        title: None,
        content_warning: js(n, "/cw").filter(|s| !s.trim().is_empty()),
        sensitive: is_sensitive_files && has_files,
        media: files(n),
        link: None,
        quote: None,
        reposted_by: None,
        context: n
            .get("reply")
            .filter(|r| r.is_object())
            .map(|r| format!("Replying to @{}", js(r, "/user/username").unwrap_or_default()))
            .or_else(|| js(n, "/replyId").map(|_| "Reply".into())),
        counts: Counts {
            replies: ju(n, "/repliesCount"),
            reposts: ju(n, "/renoteCount"),
            likes: Some(reaction_total(n)),
            quotes: None,
            score: None,
        },
        viewer: ViewerState { liked: js(n, "/myReaction").is_some(), reposted: false },
        actions: ItemActions {
            like: true,
            repost: visibility == "public" || visibility == "home",
            reply: true,
            delete: js(n, "/userId").as_deref() == Some(account.remote_id.as_str()),
        },
        target: json!({ "id": id }),
        notification: None,
    }
}

fn note_item(n: &Value, account: &Account, host: &str) -> FeedItem {
    let renote = n.get("renote").filter(|r| r.is_object());
    let is_pure_renote = renote.is_some()
        && js(n, "/text").is_none()
        && n.get("files").and_then(Value::as_array).map(|a| a.is_empty()).unwrap_or(true);
    if let (true, Some(r)) = (is_pure_renote, renote) {
        let mut item = plain_note(r, account, host);
        item.id = js(n, "/id").unwrap_or_default();
        item.reposted_by = Some(author(&n["user"], host, &account.server));
        return item;
    }
    let mut item = plain_note(n, account, host);
    if let Some(r) = renote {
        item.quote = Some(Box::new(plain_note(r, account, host)));
    }
    item
}

fn notification_item(n: &Value, account: &Account, host: &str) -> FeedItem {
    let t = js(n, "/type").unwrap_or_default();
    let note = n.get("note").filter(|x| x.is_object());
    let reaction = js(n, "/reaction").unwrap_or_default();
    let (kind, label) = match t.as_str() {
        "mention" => (NotificationKind::Mention, "mentioned you".to_string()),
        "reply" => (NotificationKind::Reply, "replied to you".to_string()),
        "quote" => (NotificationKind::Quote, "quoted your note".to_string()),
        "renote" => (NotificationKind::Repost, "renoted your note".to_string()),
        "reaction" => (NotificationKind::Like, format!("reacted {reaction}")),
        "follow" => (NotificationKind::Follow, "followed you".to_string()),
        "receiveFollowRequest" => (NotificationKind::FollowRequest, "asked to follow you".to_string()),
        "followRequestAccepted" => (NotificationKind::Follow, "accepted your follow request".to_string()),
        "pollEnded" => (NotificationKind::Poll, "a poll has ended".to_string()),
        "achievementEarned" => (NotificationKind::Other, "you earned an achievement".to_string()),
        "note" => (NotificationKind::Other, "posted".to_string()),
        _ => (NotificationKind::Other, "sent a notification".to_string()),
    };
    let is_post = matches!(kind, NotificationKind::Mention | NotificationKind::Reply | NotificationKind::Quote) || t == "note";
    let mut item = match note {
        Some(nt) if is_post => plain_note(nt, account, host),
        _ => FeedItem {
            url: note.and_then(|nt| js(nt, "/id")).map(|id| format!("{}/notes/{id}", account.server)),
            ..Default::default()
        },
    };
    let subject = match kind {
        NotificationKind::Like | NotificationKind::Poll => note.and_then(|nt| js(nt, "/text")),
        NotificationKind::Repost => note.and_then(|nt| js(nt, "/renote/text")),
        _ => None,
    };
    item.id = js(n, "/id").unwrap_or_default();
    item.created_at = js(n, "/createdAt").unwrap_or_default();
    if let Some(u) = n.get("user").filter(|u| u.is_object()) {
        item.author = author(u, host, &account.server);
    } else if !is_post {
        item.author = Author { name: "Misskey".into(), handle: host.to_string(), avatar: None, url: None };
    }
    item.notification = Some(NotificationInfo { kind, label, subject: subject.map(|s| text::preview(&s, 280)) });
    item
}

pub async fn interact(ctx: &Ctx, account: &Account, creds: &Credentials, target: &Value, action: Interaction) -> PResult<Value> {
    let id = js(target, "/id").ok_or_else(|| ProviderError::invalid("This note can't be acted on."))?;
    let server = &account.server;
    match action {
        Interaction::Like => {
            api(ctx, server, creds, "notes/reactions/create", json!({ "noteId": id, "reaction": LIKE_REACTION })).await?;
        }
        Interaction::Unlike => {
            api(ctx, server, creds, "notes/reactions/delete", json!({ "noteId": id })).await?;
        }
        Interaction::Repost => {
            api(ctx, server, creds, "notes/create", json!({ "renoteId": id, "visibility": "public" })).await?;
        }
        Interaction::Unrepost => {
            api(ctx, server, creds, "notes/unrenote", json!({ "noteId": id })).await?;
        }
    }
    Ok(target.clone())
}

pub async fn delete(ctx: &Ctx, account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    api(ctx, &account.server, creds, "notes/delete", json!({ "noteId": remote_id })).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::Pkce;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn miauth_flow_and_posting() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex_check())
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "token": "tk", "user": {"id": "u1", "username": "me"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/i"))
            .and(body_string_contains("\"i\":\"tk\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "u1", "username": "me", "name": "Me", "avatarUrl": "https://m/a.png",
                "followersCount": 4, "followingCount": 5, "notesCount": 6
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/meta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"maxNoteTextLength": 5000})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/notes/create"))
            .and(body_string_contains("\"visibility\":\"home\""))
            .and(body_string_contains("\"cw\":\"spoiler\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"createdNote": {"id": "n9"}})))
            .mount(&server)
            .await;

        let ctx = Ctx::default();
        let b = OAuthBegin {
            platform: Platform::Misskey,
            fields: HashMap::from([("server".to_string(), server.uri())]),
            redirect_uri: "http://127.0.0.1:1/callback".into(),
            state: "s".into(),
            pkce: Pkce::new(),
            client_id: None,
            client_secret: None,
        };
        let start = begin(&ctx, &b).await.unwrap();
        assert!(start.auth_url.contains("/miauth/"));
        assert!(start.auth_url.contains("permission=read%3Aaccount"));
        let session = js(&start.flow, "/session").unwrap();
        let wrong = HashMap::from([("session".to_string(), "nope".to_string())]);
        assert!(finish(&ctx, &b, &start.flow, &wrong).await.is_err());
        let ok = HashMap::from([("session".to_string(), session)]);
        let acc = finish(&ctx, &b, &start.flow, &ok).await.unwrap().remove(0);
        assert_eq!(acc.display_name, "Me");
        assert_eq!(acc.meta["maxChars"], json!(5000));

        let account = Account {
            id: "a".into(),
            platform: Platform::Misskey,
            server: server.uri(),
            remote_id: "u1".into(),
            handle: acc.handle.clone(),
            display_name: "Me".into(),
            avatar_url: None,
            profile_url: None,
            meta: json!({}),
            status: AccountStatus::Ok,
            status_message: None,
            position: 0,
            created_at: 0,
            updated_at: 0,
        };
        let post = OutgoingPost {
            text: "hello".into(),
            media: vec![],
            options: PostOptions {
                visibility: Some("unlisted".into()),
                content_warning: Some("spoiler".into()),
                ..Default::default()
            },
            reply_to: None,
            idempotency_key: "k".into(),
        };
        let res = publish(&ctx, &account, &acc.credentials, &post).await.unwrap();
        assert_eq!(res.remote_id, "n9");
        assert_eq!(res.url, Some(format!("{}/notes/n9", server.uri())));
    }

    fn path_regex_check() -> wiremock::matchers::PathRegexMatcher {
        wiremock::matchers::path_regex(r"^/api/miauth/[0-9a-f-]+/check$")
    }

    #[test]
    fn maps_renotes_and_reactions() {
        let account = Account {
            id: "a".into(),
            platform: Platform::Misskey,
            server: "https://misskey.test".into(),
            remote_id: "me".into(),
            handle: "@me@misskey.test".into(),
            display_name: "Me".into(),
            avatar_url: None,
            profile_url: None,
            meta: json!({}),
            status: AccountStatus::Ok,
            status_message: None,
            position: 0,
            created_at: 0,
            updated_at: 0,
        };
        let original = json!({"id": "o1", "createdAt": "2026-01-01T00:00:00Z", "userId": "x", "text": "orig",
            "user": {"id": "x", "username": "bob", "host": "other.test", "name": "Bob"},
            "reactions": {"❤": 2, ":blob@.:": 3}, "myReaction": "❤", "renoteCount": 1, "repliesCount": 0, "visibility": "public", "files": []});
        let renote = json!({"id": "r1", "createdAt": "2026-01-02T00:00:00Z", "userId": "me", "text": null, "files": [],
            "user": {"id": "me", "username": "me", "host": null, "name": "Me"}, "renote": original});
        let item = note_item(&renote, &account, "misskey.test");
        assert_eq!(item.id, "r1");
        assert_eq!(item.text, "orig");
        assert_eq!(item.author.handle, "@bob@other.test");
        assert_eq!(item.reposted_by.unwrap().handle, "@me@misskey.test");
        assert_eq!(item.counts.likes, Some(5));
        assert!(item.viewer.liked);
        assert_eq!(item.target, json!({"id": "o1"}));
    }
}
