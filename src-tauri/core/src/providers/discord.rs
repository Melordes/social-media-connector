//! Discord channels, posting through a webhook.
//!
//! The webhook URL contains its token, so it is stored as a secret and never
//! shown or logged.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{Ctx, js};
use crate::error::{PResult, ProviderError};
use crate::http::{send_json, send_ok};
use crate::model::*;

const HOSTS: [&str; 4] = ["discord.com", "discordapp.com", "canary.discord.com", "ptb.discord.com"];

fn webhook_url(ctx: &Ctx, raw: &str) -> PResult<String> {
    let raw = raw.trim();
    let url = url::Url::parse(raw).map_err(|_| ProviderError::invalid("That is not a webhook URL."))?;
    let host = url.host_str().unwrap_or_default();
    let test_host = ctx.overrides.contains_key("discord.test") && crate::http::is_loopback_host(host);
    if !(test_host || (url.scheme() == "https" && HOSTS.contains(&host))) {
        return Err(ProviderError::invalid("Paste a Discord webhook URL (https://discord.com/api/webhooks/...)."));
    }
    let segs: Vec<&str> = url.path_segments().map(|s| s.collect()).unwrap_or_default();
    let ok = segs.len() >= 4 && segs[0] == "api" && segs.contains(&"webhooks");
    if !ok {
        return Err(ProviderError::invalid("Paste a Discord webhook URL (https://discord.com/api/webhooks/...)."));
    }
    // Drop query and fragment; keep the canonical path.
    Ok(format!("{}://{}{}{}", url.scheme(), host, url.port().map(|p| format!(":{p}")).unwrap_or_default(), url.path().trim_end_matches('/')))
}

fn stored_url(creds: &Credentials) -> PResult<&str> {
    creds
        .extra_str("webhookUrl")
        .ok_or_else(|| ProviderError::Unauthorized("The webhook URL is missing. Reconnect the account.".into()))
}

/// Discord answers 401/404 when the webhook was deleted.
fn map_gone<T>(r: PResult<T>) -> PResult<T> {
    match r {
        Err(ProviderError::NotFound(_)) => Err(ProviderError::Unauthorized(
            "This webhook no longer exists. Create a new one and reconnect.".into(),
        )),
        other => other,
    }
}

fn avatar(hook: &Value) -> Option<String> {
    let id = js(hook, "/id")?;
    let hash = js(hook, "/avatar")?;
    Some(format!("https://cdn.discordapp.com/avatars/{id}/{hash}.png?size=128"))
}

pub async fn connect(ctx: &Ctx, fields: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    let url = webhook_url(ctx, fields.get("webhookUrl").map(String::as_str).unwrap_or_default())?;
    let hook: Value = map_gone(send_json(ctx.http.client.get(&url), "Discord webhook").await)?;
    let custom = fields.get("name").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let hook_name = js(&hook, "/name").unwrap_or_else(|| "Webhook".into());
    let mut creds = Credentials::default();
    creds.set_extra("webhookUrl", url);
    Ok(vec![NewAccount {
        platform: Platform::Discord,
        server: String::new(),
        remote_id: js(&hook, "/id").unwrap_or_default(),
        handle: format!("webhook in channel {}", js(&hook, "/channel_id").unwrap_or_default()),
        display_name: custom.clone().unwrap_or(hook_name),
        avatar_url: avatar(&hook),
        profile_url: None,
        meta: json!({
            "guildId": js(&hook, "/guild_id"),
            "channelId": js(&hook, "/channel_id"),
            "username": custom,
        }),
        credentials: creds,
    }])
}

pub async fn profile(ctx: &Ctx, account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    let hook: Value = map_gone(send_json(ctx.http.client.get(stored_url(creds)?), "Discord webhook").await)?;
    Ok(ProfileUpdate {
        display_name: account.meta_str("username").map(str::to_string).or_else(|| js(&hook, "/name")),
        avatar_url: avatar(&hook),
        ..Default::default()
    })
}

pub fn validate(_account: &Account, _post: &OutgoingPost) -> Vec<String> {
    Vec::new()
}

pub async fn publish(ctx: &Ctx, account: &Account, creds: &Credentials, post: &OutgoingPost) -> PResult<PublishResult> {
    let url = format!("{}?wait=true", stored_url(creds)?);
    let images = super::prepare_images(account, post)?;
    let mut payload = json!({
        "content": post.text,
        // Never ping @everyone or @here by accident.
        "allowed_mentions": { "parse": ["users", "roles"] },
    });
    if let Some(name) = account.meta_str("username") {
        payload["username"] = json!(name);
    }
    let req = if images.is_empty() {
        ctx.http.client.post(&url).json(&payload)
    } else {
        let mut form = reqwest::multipart::Form::new();
        let mut attachments = Vec::new();
        for (i, (img, alt)) in images.into_iter().enumerate() {
            let mut a = json!({ "id": i, "filename": img.file_name });
            if !alt.trim().is_empty() {
                a["description"] = json!(alt);
            }
            attachments.push(a);
            let part = reqwest::multipart::Part::bytes(img.bytes)
                .file_name(img.file_name.clone())
                .mime_str(&img.mime)
                .map_err(|e| ProviderError::other(e.to_string()))?;
            form = form.part(format!("files[{i}]"), part);
        }
        payload["attachments"] = json!(attachments);
        form = form.text("payload_json", payload.to_string());
        ctx.http.client.post(&url).multipart(form)
    };
    let msg: Value = map_gone(send_json(req, "Discord").await)?;
    let id = js(&msg, "/id").unwrap_or_default();
    let channel = js(&msg, "/channel_id").or_else(|| account.meta_str("channelId").map(str::to_string));
    let guild = account.meta_str("guildId");
    Ok(PublishResult {
        url: match (guild, channel) {
            (Some(g), Some(c)) => Some(format!("https://discord.com/channels/{g}/{c}/{id}")),
            _ => None,
        },
        remote_id: id,
    })
}

pub async fn delete(ctx: &Ctx, _account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    let url = format!("{}/messages/{remote_id}", stored_url(creds)?);
    send_ok(ctx.http.client.delete(url)).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn validates_webhook_urls() {
        let ctx = Ctx::default();
        assert_eq!(
            webhook_url(&ctx, " https://discord.com/api/webhooks/1/abc?x=1 ").unwrap(),
            "https://discord.com/api/webhooks/1/abc"
        );
        assert!(webhook_url(&ctx, "https://evil.example/api/webhooks/1/abc").is_err());
        assert!(webhook_url(&ctx, "http://discord.com/api/webhooks/1/abc").is_err());
        assert!(webhook_url(&ctx, "https://discord.com/channels/1/2").is_err());
    }

    #[tokio::test]
    async fn connects_and_posts_with_images() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/webhooks/11/tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "11", "name": "Announcer", "avatar": "abc", "channel_id": "22", "guild_id": "33"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/webhooks/11/tok"))
            .and(query_param("wait", "true"))
            .and(crate::testutil::bytes_contains("payload_json"))
            .and(crate::testutil::bytes_contains("\"description\":\"alt!\""))
            .and(crate::testutil::bytes_contains("files[0]"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "99", "channel_id": "22"})))
            .mount(&server)
            .await;
        let ctx = Ctx::default().with_override("discord.test", "1");
        let fields = HashMap::from([("webhookUrl".to_string(), format!("{}/api/webhooks/11/tok", server.uri()))]);
        let new = connect(&ctx, &fields).await.unwrap().remove(0);
        assert_eq!(new.display_name, "Announcer");
        assert_eq!(new.avatar_url.as_deref(), Some("https://cdn.discordapp.com/avatars/11/abc.png?size=128"));
        let account = Account {
            id: "a".into(),
            platform: Platform::Discord,
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
        let dir = tempfile::tempdir().unwrap();
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([9, 9, 9])))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let mut att = crate::media::import_bytes(dir.path(), &png, "a.png").unwrap();
        att.alt = "alt!".into();
        let post = OutgoingPost {
            text: "hello".into(),
            media: vec![att],
            options: PostOptions::default(),
            reply_to: None,
            idempotency_key: "k".into(),
        };
        let res = publish(&ctx, &account, &new.credentials, &post).await.unwrap();
        assert_eq!(res.url.as_deref(), Some("https://discord.com/channels/33/22/99"));
    }
}
