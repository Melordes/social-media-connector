//! SQLite storage for accounts, streams, posts, settings and statistics.
//! Secrets are not stored here (see `secrets.rs`).

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::model::*;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    pub id: String,
    pub account_id: String,
    pub kind: String,
    pub param: Option<String>,
    pub title: Option<String>,
    pub position: i64,
    pub refresh_minutes: i64,
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PostStatus {
    Draft,
    Scheduled,
    Publishing,
    Published,
    Partial,
    Failed,
    Missed,
}

impl PostStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PostStatus::Draft => "draft",
            PostStatus::Scheduled => "scheduled",
            PostStatus::Publishing => "publishing",
            PostStatus::Published => "published",
            PostStatus::Partial => "partial",
            PostStatus::Failed => "failed",
            PostStatus::Missed => "missed",
        }
    }

    pub fn parse(s: &str) -> PostStatus {
        match s {
            "scheduled" => PostStatus::Scheduled,
            "publishing" => PostStatus::Publishing,
            "published" => PostStatus::Published,
            "partial" => PostStatus::Partial,
            "failed" => PostStatus::Failed,
            "missed" => PostStatus::Missed,
            _ => PostStatus::Draft,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TargetStatus {
    Pending,
    Publishing,
    Published,
    Failed,
}

impl TargetStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TargetStatus::Pending => "pending",
            TargetStatus::Publishing => "publishing",
            TargetStatus::Published => "published",
            TargetStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> TargetStatus {
        match s {
            "publishing" => TargetStatus::Publishing,
            "published" => TargetStatus::Published,
            "failed" => TargetStatus::Failed,
            _ => TargetStatus::Pending,
        }
    }
}

/// A reply being composed: which account replies, and to what.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReplyRef {
    pub account_id: String,
    pub target: Value,
    pub author_handle: String,
    pub text_preview: String,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PostTarget {
    pub account_id: String,
    pub platform: Platform,
    pub account_label: String,
    pub body_override: Option<String>,
    pub status: TargetStatus,
    pub remote_id: Option<String>,
    pub remote_url: Option<String>,
    pub error: Option<String>,
    pub published_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Post {
    pub id: String,
    pub body: String,
    pub media: Vec<MediaAttachment>,
    pub options: PostOptions,
    pub reply_to: Option<ReplyRef>,
    pub status: PostStatus,
    /// Unix milliseconds.
    pub scheduled_at: Option<i64>,
    pub published_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_error: Option<String>,
    pub targets: Vec<PostTarget>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StatsSnapshot {
    pub account_id: String,
    pub day: String,
    pub followers: Option<u64>,
    pub following: Option<u64>,
    pub posts: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppCredentialInfo {
    pub platform: Platform,
    pub client_id: String,
    pub has_secret: bool,
    pub updated_at: i64,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    platform TEXT NOT NULL,
    server TEXT NOT NULL DEFAULT '',
    remote_id TEXT NOT NULL,
    handle TEXT NOT NULL,
    display_name TEXT NOT NULL,
    avatar_url TEXT,
    profile_url TEXT,
    meta TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'ok',
    status_message TEXT,
    position INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (platform, server, remote_id)
);
CREATE TABLE IF NOT EXISTS streams (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    param TEXT,
    title TEXT,
    position INTEGER NOT NULL DEFAULT 0,
    refresh_minutes INTEGER NOT NULL DEFAULT 5,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS posts (
    id TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    media TEXT NOT NULL DEFAULT '[]',
    options TEXT NOT NULL DEFAULT '{}',
    reply_to TEXT,
    status TEXT NOT NULL,
    scheduled_at INTEGER,
    published_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_error TEXT
);
CREATE INDEX IF NOT EXISTS posts_status_time ON posts(status, scheduled_at);
CREATE TABLE IF NOT EXISTS post_targets (
    post_id TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    account_id TEXT NOT NULL,
    platform TEXT NOT NULL,
    account_label TEXT NOT NULL,
    body_override TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    remote_id TEXT,
    remote_url TEXT,
    error TEXT,
    published_at INTEGER,
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (post_id, account_id)
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS app_credentials (
    platform TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    has_secret INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS stats_snapshots (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    day TEXT NOT NULL,
    followers INTEGER,
    following INTEGER,
    posts INTEGER,
    PRIMARY KEY (account_id, day)
);
CREATE TABLE IF NOT EXISTS inbox_state (
    account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    seen_at TEXT NOT NULL
);
"#;

const ACCOUNT_COLS: &str = "id, platform, server, remote_id, handle, display_name, avatar_url, profile_url, meta, status, status_message, position, created_at, updated_at";

fn account_from_row(r: &Row) -> rusqlite::Result<Account> {
    let platform: String = r.get(1)?;
    let meta: String = r.get(8)?;
    let status: String = r.get(9)?;
    Ok(Account {
        id: r.get(0)?,
        platform: Platform::parse(&platform).unwrap_or(Platform::Mastodon),
        server: r.get(2)?,
        remote_id: r.get(3)?,
        handle: r.get(4)?,
        display_name: r.get(5)?,
        avatar_url: r.get(6)?,
        profile_url: r.get(7)?,
        meta: serde_json::from_str(&meta).unwrap_or(Value::Object(Default::default())),
        status: AccountStatus::parse(&status),
        status_message: r.get(10)?,
        position: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

const STREAM_COLS: &str = "id, account_id, kind, param, title, position, refresh_minutes, created_at";

fn stream_from_row(r: &Row) -> rusqlite::Result<Stream> {
    Ok(Stream {
        id: r.get(0)?,
        account_id: r.get(1)?,
        kind: r.get(2)?,
        param: r.get(3)?,
        title: r.get(4)?,
        position: r.get(5)?,
        refresh_minutes: r.get(6)?,
        created_at: r.get(7)?,
    })
}

const POST_COLS: &str =
    "id, body, media, options, reply_to, status, scheduled_at, published_at, created_at, updated_at, last_error";

fn post_from_row(r: &Row) -> rusqlite::Result<Post> {
    let media: String = r.get(2)?;
    let options: String = r.get(3)?;
    let reply: Option<String> = r.get(4)?;
    let status: String = r.get(5)?;
    Ok(Post {
        id: r.get(0)?,
        body: r.get(1)?,
        media: serde_json::from_str(&media).unwrap_or_default(),
        options: serde_json::from_str(&options).unwrap_or_default(),
        reply_to: reply.and_then(|s| serde_json::from_str(&s).ok()),
        status: PostStatus::parse(&status),
        scheduled_at: r.get(6)?,
        published_at: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
        last_error: r.get(10)?,
        targets: Vec::new(),
    })
}

const TARGET_COLS: &str =
    "account_id, platform, account_label, body_override, status, remote_id, remote_url, error, published_at";

fn target_from_row(r: &Row) -> rusqlite::Result<PostTarget> {
    let platform: String = r.get(1)?;
    let status: String = r.get(4)?;
    Ok(PostTarget {
        account_id: r.get(0)?,
        platform: Platform::parse(&platform).unwrap_or(Platform::Mastodon),
        account_label: r.get(2)?,
        body_override: r.get(3)?,
        status: TargetStatus::parse(&status),
        remote_id: r.get(5)?,
        remote_url: r.get(6)?,
        error: r.get(7)?,
        published_at: r.get(8)?,
    })
}

impl Store {
    pub fn open(path: &Path) -> AppResult<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> AppResult<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> AppResult<Store> {
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", 1)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> AppResult<T> {
        let conn = self.conn.lock().map_err(|_| AppError::Db("database lock poisoned".into()))?;
        f(&conn).map_err(AppError::from)
    }

    // ---------------------------------------------------------------- accounts

    pub fn list_accounts(&self) -> AppResult<Vec<Account>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {ACCOUNT_COLS} FROM accounts ORDER BY position, created_at"))?;
            let rows = st.query_map([], account_from_row)?;
            rows.collect()
        })
    }

    pub fn get_account(&self, id: &str) -> AppResult<Option<Account>> {
        self.with(|c| {
            c.query_row(&format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE id = ?1"), [id], account_from_row)
                .optional()
        })
    }

    pub fn find_account(&self, platform: Platform, server: &str, remote_id: &str) -> AppResult<Option<Account>> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE platform = ?1 AND server = ?2 AND remote_id = ?3"),
                params![platform.as_str(), server, remote_id],
                account_from_row,
            )
            .optional()
        })
    }

    /// Inserts a new account or updates the existing one with the same
    /// network identity. Returns the stored account and whether it was new.
    pub fn upsert_account(&self, new: &NewAccount) -> AppResult<(Account, bool)> {
        let now = now_secs();
        let existing = self.find_account(new.platform, &new.server, &new.remote_id)?;
        let is_new = existing.is_none();
        let id = match &existing {
            Some(a) => a.id.clone(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        let mut meta = existing.as_ref().map(|a| a.meta.clone()).unwrap_or(Value::Object(Default::default()));
        if let (Value::Object(m), Value::Object(n)) = (&mut meta, &new.meta) {
            for (k, v) in n {
                m.insert(k.clone(), v.clone());
            }
        }
        let meta_s = serde_json::to_string(&meta)?;
        self.with(|c| {
            if is_new {
                let pos: i64 = c.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM accounts", [], |r| r.get(0))?;
                c.execute(
                    "INSERT INTO accounts (id, platform, server, remote_id, handle, display_name, avatar_url, profile_url, meta, status, status_message, position, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'ok', NULL, ?10, ?11, ?11)",
                    params![
                        id,
                        new.platform.as_str(),
                        new.server,
                        new.remote_id,
                        new.handle,
                        new.display_name,
                        new.avatar_url,
                        new.profile_url,
                        meta_s,
                        pos,
                        now
                    ],
                )?;
            } else {
                c.execute(
                    "UPDATE accounts SET handle = ?2, display_name = ?3, avatar_url = ?4, profile_url = ?5, meta = ?6,
                     status = 'ok', status_message = NULL, updated_at = ?7 WHERE id = ?1",
                    params![id, new.handle, new.display_name, new.avatar_url, new.profile_url, meta_s, now],
                )?;
            }
            Ok(())
        })?;
        let acc = self.get_account(&id)?.ok_or_else(|| AppError::Db("account vanished".into()))?;
        Ok((acc, is_new))
    }

    pub fn update_account_profile(&self, id: &str, update: &ProfileUpdate) -> AppResult<()> {
        let Some(mut acc) = self.get_account(id)? else { return Ok(()) };
        if let Some(h) = &update.handle {
            acc.handle = h.clone();
        }
        if let Some(n) = &update.display_name {
            acc.display_name = n.clone();
        }
        if update.avatar_url.is_some() {
            acc.avatar_url = update.avatar_url.clone();
        }
        if update.profile_url.is_some() {
            acc.profile_url = update.profile_url.clone();
        }
        if let Value::Object(m) = &mut acc.meta {
            for (k, v) in &update.meta {
                m.insert(k.clone(), v.clone());
            }
        }
        let meta_s = serde_json::to_string(&acc.meta)?;
        self.with(|c| {
            c.execute(
                "UPDATE accounts SET handle = ?2, display_name = ?3, avatar_url = ?4, profile_url = ?5, meta = ?6, updated_at = ?7 WHERE id = ?1",
                params![id, acc.handle, acc.display_name, acc.avatar_url, acc.profile_url, meta_s, now_secs()],
            )
            .map(|_| ())
        })
    }

    pub fn set_account_meta(&self, id: &str, key: &str, value: Value) -> AppResult<()> {
        let Some(mut acc) = self.get_account(id)? else { return Ok(()) };
        if let Value::Object(m) = &mut acc.meta {
            if value.is_null() {
                m.remove(key);
            } else {
                m.insert(key.to_string(), value);
            }
        }
        let meta_s = serde_json::to_string(&acc.meta)?;
        self.with(|c| {
            c.execute("UPDATE accounts SET meta = ?2, updated_at = ?3 WHERE id = ?1", params![id, meta_s, now_secs()])
                .map(|_| ())
        })
    }

    pub fn set_account_status(&self, id: &str, status: AccountStatus, message: Option<&str>) -> AppResult<()> {
        self.with(|c| {
            c.execute(
                "UPDATE accounts SET status = ?2, status_message = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, status.as_str(), message, now_secs()],
            )
            .map(|_| ())
        })
    }

    pub fn delete_account(&self, id: &str) -> AppResult<()> {
        self.with(|c| c.execute("DELETE FROM accounts WHERE id = ?1", [id]).map(|_| ()))
    }

    pub fn reorder_accounts(&self, ids: &[String]) -> AppResult<()> {
        self.with(|c| {
            for (i, id) in ids.iter().enumerate() {
                c.execute("UPDATE accounts SET position = ?2 WHERE id = ?1", params![id, i as i64])?;
            }
            Ok(())
        })
    }

    // ----------------------------------------------------------------- streams

    pub fn list_streams(&self) -> AppResult<Vec<Stream>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {STREAM_COLS} FROM streams ORDER BY position, created_at"))?;
            let rows = st.query_map([], stream_from_row)?;
            rows.collect()
        })
    }

    pub fn get_stream(&self, id: &str) -> AppResult<Option<Stream>> {
        self.with(|c| {
            c.query_row(&format!("SELECT {STREAM_COLS} FROM streams WHERE id = ?1"), [id], stream_from_row)
                .optional()
        })
    }

    pub fn add_stream(
        &self,
        account_id: &str,
        kind: &str,
        param: Option<&str>,
        title: Option<&str>,
        refresh_minutes: i64,
    ) -> AppResult<Stream> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        self.with(|c| {
            let pos: i64 = c.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM streams", [], |r| r.get(0))?;
            c.execute(
                "INSERT INTO streams (id, account_id, kind, param, title, position, refresh_minutes, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, account_id, kind, param, title, pos, refresh_minutes, now],
            )?;
            Ok(())
        })?;
        self.get_stream(&id)?.ok_or_else(|| AppError::Db("stream vanished".into()))
    }

    pub fn update_stream(&self, id: &str, title: Option<&str>, refresh_minutes: i64) -> AppResult<()> {
        self.with(|c| {
            c.execute(
                "UPDATE streams SET title = ?2, refresh_minutes = ?3 WHERE id = ?1",
                params![id, title, refresh_minutes],
            )
            .map(|_| ())
        })
    }

    pub fn delete_stream(&self, id: &str) -> AppResult<()> {
        self.with(|c| c.execute("DELETE FROM streams WHERE id = ?1", [id]).map(|_| ()))
    }

    pub fn reorder_streams(&self, ids: &[String]) -> AppResult<()> {
        self.with(|c| {
            for (i, id) in ids.iter().enumerate() {
                c.execute("UPDATE streams SET position = ?2 WHERE id = ?1", params![id, i as i64])?;
            }
            Ok(())
        })
    }

    // ------------------------------------------------------------------- posts

    fn load_targets(c: &Connection, post: &mut Post) -> rusqlite::Result<()> {
        let mut st = c.prepare(&format!(
            "SELECT {TARGET_COLS} FROM post_targets WHERE post_id = ?1 ORDER BY position, rowid"
        ))?;
        post.targets = st.query_map([&post.id], target_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(())
    }

    pub fn get_post(&self, id: &str) -> AppResult<Option<Post>> {
        self.with(|c| {
            let post = c
                .query_row(&format!("SELECT {POST_COLS} FROM posts WHERE id = ?1"), [id], post_from_row)
                .optional()?;
            match post {
                Some(mut p) => {
                    Self::load_targets(c, &mut p)?;
                    Ok(Some(p))
                }
                None => Ok(None),
            }
        })
    }

    /// Posts with one of `statuses` (all when empty), newest first by
    /// schedule/publish time, optionally limited to a time window (ms).
    pub fn list_posts(&self, statuses: &[PostStatus], from: Option<i64>, to: Option<i64>, limit: i64) -> AppResult<Vec<Post>> {
        self.with(|c| {
            let mut sql = format!(
                "SELECT {POST_COLS} FROM posts WHERE 1=1"
            );
            if !statuses.is_empty() {
                let list = statuses.iter().map(|s| format!("'{}'", s.as_str())).collect::<Vec<_>>().join(",");
                sql.push_str(&format!(" AND status IN ({list})"));
            }
            let time_expr = "COALESCE(scheduled_at, published_at, updated_at * 1000)";
            if let Some(f) = from {
                sql.push_str(&format!(" AND {time_expr} >= {f}"));
            }
            if let Some(t) = to {
                sql.push_str(&format!(" AND {time_expr} < {t}"));
            }
            sql.push_str(&format!(" ORDER BY {time_expr} DESC LIMIT {limit}"));
            let mut st = c.prepare(&sql)?;
            let mut posts: Vec<Post> = st.query_map([], post_from_row)?.collect::<rusqlite::Result<_>>()?;
            for p in posts.iter_mut() {
                Self::load_targets(c, p)?;
            }
            Ok(posts)
        })
    }

    pub fn due_posts(&self, now_ms: i64) -> AppResult<Vec<Post>> {
        let ids: Vec<String> = self.with(|c| {
            let mut st = c.prepare(
                "SELECT id FROM posts WHERE status = 'scheduled' AND scheduled_at IS NOT NULL AND scheduled_at <= ?1 ORDER BY scheduled_at",
            )?;
            st.query_map([now_ms], |r| r.get::<_, String>(0))?.collect()
        })?;
        let mut out = Vec::new();
        for id in ids {
            if let Some(p) = self.get_post(&id)? {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Writes a post and replaces its targets (keeping the publish results of
    /// targets that stay).
    pub fn save_post(&self, post: &Post) -> AppResult<()> {
        let media = serde_json::to_string(&post.media)?;
        let options = serde_json::to_string(&post.options)?;
        let reply = post.reply_to.as_ref().map(serde_json::to_string).transpose()?;
        let mut conn = self.conn.lock().map_err(|_| AppError::Db("database lock poisoned".into()))?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO posts (id, body, media, options, reply_to, status, scheduled_at, published_at, created_at, updated_at, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET body = excluded.body, media = excluded.media, options = excluded.options,
               reply_to = excluded.reply_to, status = excluded.status, scheduled_at = excluded.scheduled_at,
               published_at = excluded.published_at, updated_at = excluded.updated_at, last_error = excluded.last_error",
            params![
                post.id,
                post.body,
                media,
                options,
                reply,
                post.status.as_str(),
                post.scheduled_at,
                post.published_at,
                post.created_at,
                post.updated_at,
                post.last_error
            ],
        )?;
        let keep: Vec<String> = post.targets.iter().map(|t| t.account_id.clone()).collect();
        {
            let mut existing = tx.prepare("SELECT account_id FROM post_targets WHERE post_id = ?1")?;
            let ids: Vec<String> = existing.query_map([&post.id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            for id in ids {
                if !keep.contains(&id) {
                    tx.execute("DELETE FROM post_targets WHERE post_id = ?1 AND account_id = ?2", params![post.id, id])?;
                }
            }
        }
        for (i, t) in post.targets.iter().enumerate() {
            tx.execute(
                "INSERT INTO post_targets (post_id, account_id, platform, account_label, body_override, status, remote_id, remote_url, error, published_at, position)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(post_id, account_id) DO UPDATE SET platform = excluded.platform, account_label = excluded.account_label,
                   body_override = excluded.body_override, status = excluded.status, remote_id = excluded.remote_id,
                   remote_url = excluded.remote_url, error = excluded.error, published_at = excluded.published_at, position = excluded.position",
                params![
                    post.id,
                    t.account_id,
                    t.platform.as_str(),
                    t.account_label,
                    t.body_override,
                    t.status.as_str(),
                    t.remote_id,
                    t.remote_url,
                    t.error,
                    t.published_at,
                    i as i64
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Moves a post to `to` only if it currently has one of `from`. Returns
    /// whether it did, so two publishers never handle the same post.
    pub fn transition_post(&self, id: &str, from: &[PostStatus], to: PostStatus) -> AppResult<bool> {
        let list = from.iter().map(|s| format!("'{}'", s.as_str())).collect::<Vec<_>>().join(",");
        self.with(|c| {
            let n = c.execute(
                &format!("UPDATE posts SET status = ?2, updated_at = ?3 WHERE id = ?1 AND status IN ({list})"),
                params![id, to.as_str(), now_secs()],
            )?;
            Ok(n == 1)
        })
    }

    pub fn set_post_result(&self, id: &str, status: PostStatus, published_at: Option<i64>, last_error: Option<&str>) -> AppResult<()> {
        self.with(|c| {
            c.execute(
                "UPDATE posts SET status = ?2, published_at = COALESCE(?3, published_at), last_error = ?4, updated_at = ?5 WHERE id = ?1",
                params![id, status.as_str(), published_at, last_error, now_secs()],
            )
            .map(|_| ())
        })
    }

    pub fn set_target_status(
        &self,
        post_id: &str,
        account_id: &str,
        status: TargetStatus,
        result: Option<&PublishResult>,
        error: Option<&str>,
    ) -> AppResult<()> {
        let published_at = if status == TargetStatus::Published { Some(now_millis()) } else { None };
        self.with(|c| {
            c.execute(
                "UPDATE post_targets SET status = ?3,
                    remote_id = COALESCE(?4, remote_id), remote_url = COALESCE(?5, remote_url),
                    error = ?6, published_at = COALESCE(?7, published_at)
                 WHERE post_id = ?1 AND account_id = ?2",
                params![
                    post_id,
                    account_id,
                    status.as_str(),
                    result.map(|r| r.remote_id.clone()),
                    result.and_then(|r| r.url.clone()),
                    error,
                    published_at
                ],
            )
            .map(|_| ())
        })
    }

    pub fn clear_target_remote(&self, post_id: &str, account_id: &str) -> AppResult<()> {
        self.with(|c| {
            c.execute(
                "UPDATE post_targets SET remote_url = NULL, remote_id = NULL, error = 'Deleted from the network' WHERE post_id = ?1 AND account_id = ?2",
                params![post_id, account_id],
            )
            .map(|_| ())
        })
    }

    pub fn delete_post(&self, id: &str) -> AppResult<()> {
        self.with(|c| c.execute("DELETE FROM posts WHERE id = ?1", [id]).map(|_| ()))
    }

    /// Posts left in "publishing" by a crash or forced quit.
    pub fn interrupted_posts(&self) -> AppResult<Vec<Post>> {
        self.list_posts(&[PostStatus::Publishing], None, None, 1000)
    }

    pub fn referenced_media(&self) -> AppResult<std::collections::HashSet<String>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT media FROM posts")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            let mut set = std::collections::HashSet::new();
            for row in rows {
                let media: Vec<MediaAttachment> = serde_json::from_str(&row?).unwrap_or_default();
                for m in media {
                    set.insert(m.path);
                }
            }
            Ok(set)
        })
    }

    /// Number of targets published per day (local dates given as ms ranges
    /// are computed by the caller) for the analytics page.
    pub fn published_targets_since(&self, since_ms: i64) -> AppResult<Vec<(String, Platform, i64)>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT account_id, platform, published_at FROM post_targets WHERE status = 'published' AND published_at >= ?1",
            )?;
            st.query_map([since_ms], |r| {
                let p: String = r.get(1)?;
                Ok((r.get(0)?, Platform::parse(&p).unwrap_or(Platform::Mastodon), r.get(2)?))
            })?
            .collect()
        })
    }

    // ---------------------------------------------------------------- settings

    pub fn get_setting(&self, key: &str) -> AppResult<Option<Value>> {
        self.with(|c| {
            let v: Option<String> = c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?;
            Ok(v.and_then(|s| serde_json::from_str(&s).ok()))
        })
    }

    pub fn set_setting(&self, key: &str, value: &Value) -> AppResult<()> {
        let s = serde_json::to_string(value)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, s],
            )
            .map(|_| ())
        })
    }

    pub fn all_settings(&self) -> AppResult<serde_json::Map<String, Value>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT key, value FROM settings")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut map = serde_json::Map::new();
            for row in rows {
                let (k, v) = row?;
                if let Ok(v) = serde_json::from_str(&v) {
                    map.insert(k, v);
                }
            }
            Ok(map)
        })
    }

    // --------------------------------------------------------- app credentials

    pub fn list_app_credentials(&self) -> AppResult<Vec<AppCredentialInfo>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT platform, client_id, has_secret, updated_at FROM app_credentials")?;
            let rows = st.query_map([], |r| {
                let p: String = r.get(0)?;
                Ok((p, r.get::<_, String>(1)?, r.get::<_, i64>(2)? != 0, r.get::<_, i64>(3)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (p, client_id, has_secret, updated_at) = row?;
                if let Some(platform) = Platform::parse(&p) {
                    out.push(AppCredentialInfo { platform, client_id, has_secret, updated_at });
                }
            }
            Ok(out)
        })
    }

    pub fn get_app_credential(&self, platform: Platform) -> AppResult<Option<AppCredentialInfo>> {
        Ok(self.list_app_credentials()?.into_iter().find(|c| c.platform == platform))
    }

    pub fn set_app_credential(&self, platform: Platform, client_id: &str, has_secret: bool) -> AppResult<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO app_credentials (platform, client_id, has_secret, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(platform) DO UPDATE SET client_id = excluded.client_id, has_secret = excluded.has_secret, updated_at = excluded.updated_at",
                params![platform.as_str(), client_id, has_secret as i64, now_secs()],
            )
            .map(|_| ())
        })
    }

    pub fn delete_app_credential(&self, platform: Platform) -> AppResult<()> {
        self.with(|c| c.execute("DELETE FROM app_credentials WHERE platform = ?1", [platform.as_str()]).map(|_| ()))
    }

    // ------------------------------------------------------------------- stats

    pub fn save_snapshot(&self, account_id: &str, day: &str, stats: &ProfileStats) -> AppResult<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO stats_snapshots (account_id, day, followers, following, posts) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account_id, day) DO UPDATE SET followers = excluded.followers, following = excluded.following, posts = excluded.posts",
                params![
                    account_id,
                    day,
                    stats.followers.map(|v| v as i64),
                    stats.following.map(|v| v as i64),
                    stats.posts.map(|v| v as i64)
                ],
            )
            .map(|_| ())
        })
    }

    pub fn snapshots(&self, since_day: &str) -> AppResult<Vec<StatsSnapshot>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT account_id, day, followers, following, posts FROM stats_snapshots WHERE day >= ?1 ORDER BY day",
            )?;
            st.query_map([since_day], |r| {
                Ok(StatsSnapshot {
                    account_id: r.get(0)?,
                    day: r.get(1)?,
                    followers: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    following: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    posts: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                })
            })?
            .collect()
        })
    }

    pub fn last_snapshot_day(&self, account_id: &str) -> AppResult<Option<String>> {
        self.with(|c| {
            c.query_row("SELECT MAX(day) FROM stats_snapshots WHERE account_id = ?1", [account_id], |r| r.get(0))
        })
    }

    // ------------------------------------------------------------------- inbox

    pub fn inbox_seen(&self, account_id: &str) -> AppResult<Option<String>> {
        self.with(|c| {
            c.query_row("SELECT seen_at FROM inbox_state WHERE account_id = ?1", [account_id], |r| r.get(0))
                .optional()
        })
    }

    pub fn set_inbox_seen(&self, account_id: &str, seen_at: &str) -> AppResult<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO inbox_state (account_id, seen_at) VALUES (?1, ?2) ON CONFLICT(account_id) DO UPDATE SET seen_at = excluded.seen_at",
                params![account_id, seen_at],
            )
            .map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new_acc(remote: &str) -> NewAccount {
        NewAccount {
            platform: Platform::Mastodon,
            server: "https://example.social".into(),
            remote_id: remote.into(),
            handle: format!("@{remote}@example.social"),
            display_name: remote.into(),
            avatar_url: None,
            profile_url: None,
            meta: json!({"maxChars": 500}),
            credentials: Credentials::default(),
        }
    }

    #[test]
    fn accounts_upsert_by_identity() {
        let s = Store::open_in_memory().unwrap();
        let (a, new1) = s.upsert_account(&new_acc("1")).unwrap();
        assert!(new1);
        let mut again = new_acc("1");
        again.display_name = "Renamed".into();
        again.meta = json!({"other": true});
        let (b, new2) = s.upsert_account(&again).unwrap();
        assert!(!new2);
        assert_eq!(a.id, b.id);
        assert_eq!(b.display_name, "Renamed");
        // Meta is merged, not replaced.
        assert_eq!(b.meta, json!({"maxChars": 500, "other": true}));
        s.upsert_account(&new_acc("2")).unwrap();
        assert_eq!(s.list_accounts().unwrap().len(), 2);
    }

    #[test]
    fn posts_and_targets_round_trip() {
        let s = Store::open_in_memory().unwrap();
        let (acc, _) = s.upsert_account(&new_acc("1")).unwrap();
        let post = Post {
            id: "p1".into(),
            body: "Hello".into(),
            media: vec![],
            options: PostOptions { visibility: Some("unlisted".into()), ..Default::default() },
            reply_to: None,
            status: PostStatus::Scheduled,
            scheduled_at: Some(1000),
            published_at: None,
            created_at: 1,
            updated_at: 1,
            last_error: None,
            targets: vec![PostTarget {
                account_id: acc.id.clone(),
                platform: Platform::Mastodon,
                account_label: acc.label(),
                body_override: Some("Hi Mastodon".into()),
                status: TargetStatus::Pending,
                remote_id: None,
                remote_url: None,
                error: None,
                published_at: None,
            }],
        };
        s.save_post(&post).unwrap();
        assert_eq!(s.get_post("p1").unwrap().unwrap(), post);
        assert_eq!(s.due_posts(999).unwrap().len(), 0);
        assert_eq!(s.due_posts(1000).unwrap().len(), 1);
        // Only one caller wins the transition.
        assert!(s.transition_post("p1", &[PostStatus::Scheduled], PostStatus::Publishing).unwrap());
        assert!(!s.transition_post("p1", &[PostStatus::Scheduled], PostStatus::Publishing).unwrap());
        s.set_target_status(
            "p1",
            &acc.id,
            TargetStatus::Published,
            Some(&PublishResult { remote_id: "r1".into(), url: Some("https://x/1".into()) }),
            None,
        )
        .unwrap();
        let p = s.get_post("p1").unwrap().unwrap();
        assert_eq!(p.targets[0].status, TargetStatus::Published);
        assert_eq!(p.targets[0].remote_url.as_deref(), Some("https://x/1"));
        // Removing the account keeps the post history.
        s.delete_account(&acc.id).unwrap();
        assert_eq!(s.get_post("p1").unwrap().unwrap().targets.len(), 1);
    }

    #[test]
    fn streams_cascade_with_account() {
        let s = Store::open_in_memory().unwrap();
        let (acc, _) = s.upsert_account(&new_acc("1")).unwrap();
        s.add_stream(&acc.id, "home", None, None, 5).unwrap();
        s.add_stream(&acc.id, "hashtag", Some("rust"), None, 5).unwrap();
        assert_eq!(s.list_streams().unwrap().len(), 2);
        s.delete_account(&acc.id).unwrap();
        assert_eq!(s.list_streams().unwrap().len(), 0);
    }

    #[test]
    fn settings_round_trip() {
        let s = Store::open_in_memory().unwrap();
        s.set_setting("theme", &json!("dark")).unwrap();
        assert_eq!(s.get_setting("theme").unwrap(), Some(json!("dark")));
        assert_eq!(s.all_settings().unwrap().get("theme"), Some(&json!("dark")));
    }
}
