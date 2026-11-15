//! Data types shared between the service, the providers and the user interface.
//!
//! Everything that reaches the frontend is serialized in camelCase.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Mastodon,
    Bluesky,
    Misskey,
    X,
    Reddit,
    Linkedin,
    Facebook,
    Tumblr,
    Telegram,
    Discord,
}

impl Platform {
    pub const ALL: [Platform; 10] = [
        Platform::Mastodon,
        Platform::Bluesky,
        Platform::Misskey,
        Platform::X,
        Platform::Facebook,
        Platform::Linkedin,
        Platform::Reddit,
        Platform::Tumblr,
        Platform::Telegram,
        Platform::Discord,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Mastodon => "mastodon",
            Platform::Bluesky => "bluesky",
            Platform::Misskey => "misskey",
            Platform::X => "x",
            Platform::Reddit => "reddit",
            Platform::Linkedin => "linkedin",
            Platform::Facebook => "facebook",
            Platform::Tumblr => "tumblr",
            Platform::Telegram => "telegram",
            Platform::Discord => "discord",
        }
    }

    pub fn parse(s: &str) -> Option<Platform> {
        Platform::ALL.into_iter().find(|p| p.as_str() == s)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Platform::Mastodon => "Mastodon",
            Platform::Bluesky => "Bluesky",
            Platform::Misskey => "Misskey",
            Platform::X => "X",
            Platform::Reddit => "Reddit",
            Platform::Linkedin => "LinkedIn",
            Platform::Facebook => "Facebook",
            Platform::Tumblr => "Tumblr",
            Platform::Telegram => "Telegram",
            Platform::Discord => "Discord",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A connected account, as stored (without its secrets).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub platform: Platform,
    /// Base URL of the server for federated networks (Mastodon, Misskey,
    /// the Bluesky PDS). Empty for centralised networks.
    pub server: String,
    /// The account's id on the network (user id, DID, page id, chat id...).
    pub remote_id: String,
    /// Human-readable handle (`@me@mastodon.social`, `me.bsky.social`, `u/me`).
    pub handle: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub profile_url: Option<String>,
    /// Non-secret, network-specific data (limits, blog name, chat id...).
    pub meta: Value,
    pub status: AccountStatus,
    pub status_message: Option<String>,
    pub position: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Account {
    pub fn meta_str(&self, key: &str) -> Option<&str> {
        self.meta.get(key).and_then(Value::as_str)
    }

    pub fn meta_u64(&self, key: &str) -> Option<u64> {
        self.meta.get(key).and_then(Value::as_u64)
    }

    pub fn label(&self) -> String {
        format!("{} ({})", self.display_name, self.handle)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AccountStatus {
    Ok,
    NeedsReconnect,
}

impl AccountStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AccountStatus::Ok => "ok",
            AccountStatus::NeedsReconnect => "needsReconnect",
        }
    }

    pub fn parse(s: &str) -> AccountStatus {
        match s {
            "needsReconnect" => AccountStatus::NeedsReconnect,
            _ => AccountStatus::Ok,
        }
    }
}

/// What a provider returns when an account has been connected. The service
/// turns it into an [`Account`] and stores the credentials separately.
#[derive(Clone, Debug)]
pub struct NewAccount {
    pub platform: Platform,
    pub server: String,
    pub remote_id: String,
    pub handle: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub profile_url: Option<String>,
    pub meta: Value,
    pub credentials: Credentials,
}

/// Secret material for one account. Stored in the system keyring (or the
/// protected fallback file), never sent to the user interface.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Credentials {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    /// Unix seconds when the access token stops working, if known.
    pub expires_at: Option<i64>,
    /// The OAuth client the token was issued to (needed to refresh it).
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// Anything else a provider needs (bot token, webhook URL, app password...).
    pub extra: serde_json::Map<String, Value>,
}

impl Credentials {
    pub fn bearer(token: impl Into<String>) -> Self {
        Credentials {
            access_token: Some(token.into()),
            ..Default::default()
        }
    }

    pub fn token(&self) -> &str {
        self.access_token.as_deref().unwrap_or("")
    }

    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }

    pub fn set_extra(&mut self, key: &str, value: impl Into<Value>) {
        self.extra.insert(key.to_string(), value.into());
    }

    /// True when the access token expires within `margin_secs` and a refresh
    /// token is available to get a new one.
    pub fn should_refresh(&self, now: i64, margin_secs: i64) -> bool {
        match (self.expires_at, &self.refresh_token) {
            (Some(exp), Some(_)) => exp - margin_secs <= now,
            _ => false,
        }
    }
}

/// Options for a post. Each network uses the fields that apply to it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PostOptions {
    /// Content warning / spoiler text (Mastodon, Misskey).
    pub content_warning: Option<String>,
    /// "public", "unlisted" or "followers" (Mastodon, Misskey);
    /// "public" or "connections" (LinkedIn).
    pub visibility: Option<String>,
    /// Mark media as sensitive (Mastodon, Misskey) / NSFW (Reddit).
    pub sensitive: bool,
    /// ISO 639-1 language code.
    pub language: Option<String>,
    /// Title (Reddit, required; Tumblr, optional).
    pub title: Option<String>,
    /// Subreddit for Reddit posts, without the `r/` prefix.
    pub subreddit: Option<String>,
    /// A link to share (Reddit link post, Facebook link).
    pub link: Option<String>,
    /// Tags (Tumblr).
    pub tags: Vec<String>,
    /// Send without a notification sound (Telegram).
    pub silent: bool,
    /// Mark as spoiler (Reddit).
    pub spoiler: bool,
}

/// An image ready to be uploaded with a post.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MediaAttachment {
    pub id: String,
    pub path: String,
    pub file_name: String,
    pub mime: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub alt: String,
}

/// What gets published to one account.
#[derive(Clone, Debug)]
pub struct OutgoingPost {
    pub text: String,
    pub media: Vec<MediaAttachment>,
    pub options: PostOptions,
    /// Provider data of the item being replied to (`FeedItem::target`).
    pub reply_to: Option<Value>,
    /// Stable key so a retried request does not post twice where supported.
    pub idempotency_key: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub remote_id: String,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    pub name: String,
    pub handle: String,
    pub avatar: Option<String>,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MediaView {
    /// "image", "video", "gif", "audio" or "other".
    pub kind: String,
    pub url: String,
    pub preview_url: Option<String>,
    pub alt: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl MediaView {
    pub fn image(url: impl Into<String>) -> Self {
        MediaView {
            kind: "image".into(),
            url: url.into(),
            preview_url: None,
            alt: None,
            width: None,
            height: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LinkCard {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub image: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub replies: Option<u64>,
    pub reposts: Option<u64>,
    pub likes: Option<u64>,
    pub quotes: Option<u64>,
    /// Reddit score (upvotes minus downvotes).
    pub score: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ViewerState {
    pub liked: bool,
    pub reposted: bool,
}

/// Which actions the user can take on an item.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemActions {
    pub like: bool,
    pub repost: bool,
    pub reply: bool,
    pub delete: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum NotificationKind {
    Mention,
    Reply,
    Quote,
    Like,
    Repost,
    Follow,
    FollowRequest,
    Poll,
    Update,
    Message,
    Other,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NotificationInfo {
    pub kind: NotificationKind,
    /// Short sentence such as "liked your post".
    pub label: String,
    /// Text of the post the notification is about (for likes and reposts).
    pub subject: Option<String>,
}

/// One entry in a stream: a post, or a notification.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FeedItem {
    /// Unique within the account and stream.
    pub id: String,
    /// Filled in by the service.
    #[serde(default)]
    pub account_id: String,
    pub url: Option<String>,
    pub author: Author,
    /// RFC 3339 timestamp.
    pub created_at: String,
    /// Plain-text content (always set, may be empty).
    pub text: String,
    /// HTML content from the network; the UI sanitizes it before display.
    pub html: Option<String>,
    pub title: Option<String>,
    pub content_warning: Option<String>,
    pub sensitive: bool,
    pub media: Vec<MediaView>,
    pub link: Option<LinkCard>,
    pub quote: Option<Box<FeedItem>>,
    pub reposted_by: Option<Author>,
    /// "Replying to @someone", "r/rust" and similar context lines.
    pub context: Option<String>,
    pub counts: Counts,
    pub viewer: ViewerState,
    pub actions: ItemActions,
    /// Opaque, provider-specific data needed to act on the item.
    pub target: Value,
    pub notification: Option<NotificationInfo>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FeedPage {
    pub items: Vec<FeedItem>,
    pub cursor: Option<String>,
}

/// An action on an item in a stream.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Interaction {
    Like,
    Unlike,
    Repost,
    Unrepost,
}

/// Counters shown on the analytics page.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileStats {
    pub followers: Option<u64>,
    pub following: Option<u64>,
    pub posts: Option<u64>,
}

/// Fresh profile data fetched from the network.
#[derive(Clone, Debug, Default)]
pub struct ProfileUpdate {
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub profile_url: Option<String>,
    pub stats: ProfileStats,
    /// Keys merged into `Account::meta` (for example updated limits).
    pub meta: serde_json::Map<String, Value>,
}

pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
