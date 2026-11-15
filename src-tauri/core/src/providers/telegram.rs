//! Telegram channels and groups, posting through a bot the user owns.
//!
//! The bot token is part of every request URL, so it is never logged or put
//! into error messages (see `ProviderError::from(reqwest::Error)`).

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{Ctx, js, ju};
use crate::error::{PResult, ProviderError};
use crate::http::send_json;
use crate::model::*;

const API: &str = "https://api.telegram.org";

fn token(creds: &Credentials) -> PResult<&str> {
    creds
        .extra_str("botToken")
        .ok_or_else(|| ProviderError::Unauthorized("The bot token is missing. Reconnect the account.".into()))
}

fn method_url(ctx: &Ctx, token: &str, method: &str) -> String {
    format!("{}/bot{token}/{method}", ctx.base("telegram.api", API))
}

/// Calls a Bot API method and returns `result`.
async fn call(ctx: &Ctx, token: &str, method: &str, body: Value) -> PResult<Value> {
    let req = ctx.http.client.post(method_url(ctx, token, method)).json(&body);
    let v: Value = map_auth(send_json(req, "Telegram").await)?;
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// Telegram answers 401/404 for a revoked or wrong token.
fn map_auth<T>(r: PResult<T>) -> PResult<T> {
    match r {
        Err(ProviderError::NotFound(_)) => Err(ProviderError::Unauthorized(
            "Telegram did not accept the bot token.".into(),
        )),
        other => other,
    }
}

fn normalize_chat(chat: &str) -> String {
    let c = chat.trim();
    if let Some(rest) = c.strip_prefix("https://t.me/").or_else(|| c.strip_prefix("t.me/")) {
        format!("@{}", rest.trim_end_matches('/'))
    } else if c.starts_with('@') || c.starts_with('-') || c.chars().all(|ch| ch.is_ascii_digit()) {
        c.to_string()
    } else {
        format!("@{c}")
    }
}

pub async fn connect(ctx: &Ctx, fields: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    let token = fields
        .get("botToken")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProviderError::invalid("Paste the bot token from @BotFather."))?;
    if !token.contains(':') {
        return Err(ProviderError::invalid("That does not look like a bot token (it should contain a colon)."));
    }
    let chat = fields
        .get("chat")
        .map(|s| normalize_chat(s))
        .filter(|s| !s.is_empty() && s != "@")
        .ok_or_else(|| ProviderError::invalid("Enter the channel's @username or the chat ID."))?;
    let bot = call(ctx, &token, "getMe", json!({})).await?;
    let bot_id = ju(&bot, "/id").unwrap_or_default();
    let chat_info = match call(ctx, &token, "getChat", json!({ "chat_id": chat })).await {
        Ok(c) => c,
        Err(ProviderError::Invalid(m)) | Err(ProviderError::Forbidden(m)) => {
            return Err(ProviderError::invalid(format!(
                "The bot can't see {chat}: {m}. Add the bot to the channel or group first."
            )));
        }
        Err(e) => return Err(e),
    };
    let chat_type = js(&chat_info, "/type").unwrap_or_default();
    let member = call(ctx, &token, "getChatMember", json!({ "chat_id": chat, "user_id": bot_id })).await?;
    let status = js(&member, "/status").unwrap_or_default();
    let can_post = match chat_type.as_str() {
        "channel" => {
            status == "creator"
                || (status == "administrator" && member.get("can_post_messages").and_then(Value::as_bool).unwrap_or(false))
        }
        _ => matches!(status.as_str(), "creator" | "administrator" | "member"),
    };
    if !can_post {
        return Err(ProviderError::Forbidden(if chat_type == "channel" {
            "Make the bot an admin of the channel with the \"Post messages\" permission, then try again.".into()
        } else {
            "The bot is not allowed to post in this chat.".into()
        }));
    }
    let chat_id = js(&chat_info, "/id").unwrap_or_default();
    let username = js(&chat_info, "/username");
    let title = js(&chat_info, "/title")
        .or_else(|| js(&chat_info, "/first_name"))
        .unwrap_or_else(|| chat.clone());
    let mut creds = Credentials::default();
    creds.set_extra("botToken", token);
    Ok(vec![NewAccount {
        platform: Platform::Telegram,
        server: String::new(),
        remote_id: chat_id.clone(),
        handle: username.as_ref().map(|u| format!("@{u}")).unwrap_or_else(|| format!("chat {chat_id}")),
        display_name: title,
        avatar_url: None,
        profile_url: username.as_ref().map(|u| format!("https://t.me/{u}")),
        meta: json!({
            "chatType": chat_type,
            "chatUsername": username,
            "botUsername": js(&bot, "/username"),
        }),
        credentials: creds,
    }])
}

pub async fn profile(ctx: &Ctx, account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    let token = token(creds)?;
    let chat = call(ctx, token, "getChat", json!({ "chat_id": account.remote_id })).await?;
    let members = call(ctx, token, "getChatMemberCount", json!({ "chat_id": account.remote_id }))
        .await
        .ok()
        .and_then(|v| v.as_u64());
    let username = js(&chat, "/username");
    Ok(ProfileUpdate {
        handle: username.as_ref().map(|u| format!("@{u}")),
        display_name: js(&chat, "/title"),
        avatar_url: None,
        profile_url: username.map(|u| format!("https://t.me/{u}")),
        stats: ProfileStats { followers: members, following: None, posts: None },
        meta: serde_json::Map::new(),
    })
}

pub fn validate(_account: &Account, _post: &OutgoingPost) -> Vec<String> {
    Vec::new()
}

fn message_url(account: &Account, message_id: &str) -> Option<String> {
    if let Some(u) = account.meta_str("chatUsername") {
        return Some(format!("https://t.me/{u}/{message_id}"));
    }
    // Private supergroups and channels: -100<id> → t.me/c/<id>/<message>.
    account
        .remote_id
        .strip_prefix("-100")
        .map(|internal| format!("https://t.me/c/{internal}/{message_id}"))
}

pub async fn publish(ctx: &Ctx, account: &Account, creds: &Credentials, post: &OutgoingPost) -> PResult<PublishResult> {
    let token = token(creds)?;
    let chat_id = account.remote_id.as_str();
    let images = super::prepare_images(account, post)?;
    let text = post.text.trim().to_string();
    let message_ids: Vec<String> = if images.is_empty() {
        let r = call(
            ctx,
            token,
            "sendMessage",
            json!({ "chat_id": chat_id, "text": text, "disable_notification": post.options.silent }),
        )
        .await?;
        vec![js(&r, "/message_id").unwrap_or_default()]
    } else if images.len() == 1 {
        let (img, _) = images.into_iter().next().unwrap();
        let part = reqwest::multipart::Part::bytes(img.bytes)
            .file_name(img.file_name)
            .mime_str(&img.mime)
            .map_err(|e| ProviderError::other(e.to_string()))?;
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("disable_notification", post.options.silent.to_string())
            .part("photo", part);
        if !text.is_empty() {
            form = form.text("caption", text);
        }
        let v: Value = map_auth(send_json(ctx.http.client.post(method_url(ctx, token, "sendPhoto")).multipart(form), "Telegram").await)?;
        vec![js(&v, "/result/message_id").unwrap_or_default()]
    } else {
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .text("disable_notification", post.options.silent.to_string());
        let mut media = Vec::new();
        for (i, (img, _)) in images.into_iter().enumerate() {
            let name = format!("file{i}");
            let mut entry = json!({ "type": "photo", "media": format!("attach://{name}") });
            if i == 0 && !text.is_empty() {
                entry["caption"] = json!(text);
            }
            media.push(entry);
            let part = reqwest::multipart::Part::bytes(img.bytes)
                .file_name(img.file_name)
                .mime_str(&img.mime)
                .map_err(|e| ProviderError::other(e.to_string()))?;
            form = form.part(name, part);
        }
        form = form.text("media", serde_json::to_string(&media).unwrap_or_default());
        let v: Value =
            map_auth(send_json(ctx.http.client.post(method_url(ctx, token, "sendMediaGroup")).multipart(form), "Telegram").await)?;
        v.get("result")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|m| js(m, "/message_id")).collect())
            .unwrap_or_default()
    };
    let first = message_ids.first().cloned().unwrap_or_default();
    Ok(PublishResult { url: message_url(account, &first), remote_id: message_ids.join(",") })
}

pub async fn delete(ctx: &Ctx, account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    let token = token(creds)?;
    for id in remote_id.split(',').filter(|s| !s.is_empty()) {
        call(ctx, token, "deleteMessage", json!({ "chat_id": account.remote_id, "message_id": id.parse::<i64>().unwrap_or(0) }))
            .await
            .map_err(|e| match e {
                ProviderError::Invalid(m) => ProviderError::Invalid(format!(
                    "{m}. Bots can only delete messages from the last 48 hours."
                )),
                other => other,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn connects_and_posts_to_channel() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot1:abc/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {"id": 42, "username": "mybot"}})))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/bot1:abc/getChat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {
                "id": -1001234, "type": "channel", "title": "My Channel", "username": "mychan"
            }})))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/bot1:abc/getChatMember"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {
                "status": "administrator", "can_post_messages": true, "user": {"id": 42}
            }})))
            .mount(&api)
            .await;
        Mock::given(method("POST"))
            .and(path("/bot1:abc/sendMessage"))
            .and(body_string_contains("\"disable_notification\":true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {"message_id": 77}})))
            .mount(&api)
            .await;
        let ctx = Ctx::default().with_override("telegram.api", &api.uri());
        let fields = HashMap::from([
            ("botToken".to_string(), "1:abc".to_string()),
            ("chat".to_string(), "https://t.me/mychan".to_string()),
        ]);
        let new = connect(&ctx, &fields).await.unwrap().remove(0);
        assert_eq!(new.remote_id, "-1001234");
        assert_eq!(new.handle, "@mychan");
        let account = Account {
            id: "a".into(),
            platform: Platform::Telegram,
            server: String::new(),
            remote_id: new.remote_id.clone(),
            handle: new.handle.clone(),
            display_name: new.display_name.clone(),
            avatar_url: None,
            profile_url: None,
            meta: new.meta.clone(),
            status: AccountStatus::Ok,
            status_message: None,
            position: 0,
            created_at: 0,
            updated_at: 0,
        };
        let post = OutgoingPost {
            text: "Hi channel".into(),
            media: vec![],
            options: PostOptions { silent: true, ..Default::default() },
            reply_to: None,
            idempotency_key: "k".into(),
        };
        let res = publish(&ctx, &account, &new.credentials, &post).await.unwrap();
        assert_eq!(res.remote_id, "77");
        assert_eq!(res.url.as_deref(), Some("https://t.me/mychan/77"));
    }

    #[tokio::test]
    async fn rejects_bot_without_post_rights_and_hides_token() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot9:zzz/getMe"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"ok": false, "error_code": 401, "description": "Unauthorized"})))
            .mount(&api)
            .await;
        let ctx = Ctx::default().with_override("telegram.api", &api.uri());
        let fields = HashMap::from([
            ("botToken".to_string(), "9:zzz".to_string()),
            ("chat".to_string(), "@c".to_string()),
        ]);
        let err = connect(&ctx, &fields).await.unwrap_err();
        assert!(matches!(err, ProviderError::Unauthorized(_)));
        assert!(!err.to_string().contains("9:zzz"));

        // A connection error must not reveal the token either.
        let ctx = Ctx::default().with_override("telegram.api", "http://127.0.0.1:9");
        let err = connect(&ctx, &fields).await.unwrap_err();
        assert!(!err.to_string().contains("zzz"), "{err}");
    }

    #[test]
    fn private_channel_links() {
        let mut acc = Account {
            id: "a".into(),
            platform: Platform::Telegram,
            server: String::new(),
            remote_id: "-1009876".into(),
            handle: String::new(),
            display_name: String::new(),
            avatar_url: None,
            profile_url: None,
            meta: json!({}),
            status: AccountStatus::Ok,
            status_message: None,
            position: 0,
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(message_url(&acc, "5").as_deref(), Some("https://t.me/c/9876/5"));
        acc.remote_id = "12345".into();
        assert_eq!(message_url(&acc, "5"), None);
        assert_eq!(normalize_chat("mychannel"), "@mychannel");
        assert_eq!(normalize_chat("-100123"), "-100123");
    }
}
