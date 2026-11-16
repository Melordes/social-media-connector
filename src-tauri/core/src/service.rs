//! The application service: everything the user interface asks for goes
//! through here. It owns storage, credentials, sign-in flows, publishing and
//! the scheduler.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::error::{AppError, AppResult, ProviderError};
use crate::http::Http;
use crate::model::*;
use crate::oauth::{self, CallbackPage, LoopbackServer, Pkce};
use crate::providers::{self, Ctx, Limits, OAuthBegin, PlatformInfo};
use crate::secrets::SecretStore;
use crate::store::*;

/// How the service tells the outside world that something changed.
pub trait EventSink: Send + Sync {
    fn posts_changed(&self);
    fn accounts_changed(&self);
    fn notify(&self, title: &str, body: &str);
}

pub struct NoopSink;

impl EventSink for NoopSink {
    fn posts_changed(&self) {}
    fn accounts_changed(&self) {}
    fn notify(&self, _title: &str, _body: &str) {}
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// "system", "light" or "dark".
    pub theme: String,
    /// Keep running in the tray when the window is closed, so scheduled
    /// posts still go out.
    pub close_to_tray: bool,
    pub notify_published: bool,
    pub notify_failed: bool,
    /// A scheduled post that is overdue by more than this (because the app
    /// was not running) is not published automatically but marked missed.
    /// -1 publishes it however late.
    pub late_grace_minutes: i64,
    /// Port of the local sign-in callback (part of the redirect URI).
    pub oauth_port: u16,
    pub default_refresh_minutes: i64,
    pub week_starts_monday: bool,
    /// Accounts selected in the composer last time.
    pub last_accounts: Vec<String>,
    pub tray_hint_shown: bool,
    pub compact_streams: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: "system".into(),
            close_to_tray: true,
            notify_published: true,
            notify_failed: true,
            late_grace_minutes: 60,
            oauth_port: 38917,
            default_refresh_minutes: 5,
            week_starts_monday: true,
            last_accounts: Vec::new(),
            tray_hint_shown: false,
            compact_streams: false,
        }
    }
}

/// An account as the user interface sees it.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    #[serde(flatten)]
    pub account: Account,
    pub limits: Limits,
    pub stats: Option<StatsSnapshot>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub data_dir: String,
    pub secret_backend: String,
    /// The operating system's credential store, for display.
    pub secret_store_name: String,
    pub platforms: Vec<PlatformInfo>,
    pub app_credentials: Vec<AppCredentialInfo>,
    /// Redirect URI to register with each network's developer app.
    pub redirect_uris: HashMap<Platform, String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ConnectStart {
    pub flow_id: String,
    pub auth_url: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TargetInput {
    pub account_id: String,
    #[serde(default)]
    pub body_override: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SavePostInput {
    pub id: Option<String>,
    pub body: String,
    #[serde(default)]
    pub media: Vec<MediaAttachment>,
    #[serde(default)]
    pub options: PostOptions,
    #[serde(default)]
    pub reply_to: Option<ReplyRef>,
    pub targets: Vec<TargetInput>,
    /// "draft", "schedule" or "publish".
    pub action: String,
    /// Unix milliseconds, for "schedule".
    pub scheduled_at: Option<i64>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TargetProblem {
    pub account_id: String,
    pub problems: Vec<String>,
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct InboxPage {
    pub items: Vec<FeedItem>,
    /// Next page cursor per account (absent: no more items).
    pub cursors: HashMap<String, String>,
    pub errors: Vec<AccountError>,
    /// When the user last looked at each account's notifications (RFC 3339).
    pub seen: HashMap<String, String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AccountError {
    pub account_id: String,
    pub message: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DailyCount {
    pub day: String,
    pub count: u32,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AccountAnalytics {
    pub account_id: String,
    pub history: Vec<StatsSnapshot>,
    pub published_30d: u32,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AnalyticsOverview {
    pub accounts: Vec<AccountAnalytics>,
    pub daily: Vec<DailyCount>,
    pub scheduled: u32,
    pub drafts: u32,
    pub published_30d: u32,
    pub failed_30d: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Manual,
    Scheduled,
}

/// One network operation for an account (so it can be retried after a
/// token refresh without closures).
#[derive(Clone, Debug)]
enum Op {
    Profile,
    Publish(Box<OutgoingPost>),
    Stream { kind: String, param: Option<String>, cursor: Option<String> },
    Interact { target: Value, action: Interaction },
    Delete(String),
}

enum OpOut {
    Profile(ProfileUpdate),
    Published(PublishResult),
    Page(FeedPage),
    Target(Value),
    Done,
}

async fn run_op(ctx: &Ctx, account: &Account, creds: &Credentials, op: &Op) -> Result<OpOut, ProviderError> {
    Ok(match op {
        Op::Profile => OpOut::Profile(providers::profile(ctx, account, creds).await?),
        Op::Publish(post) => OpOut::Published(providers::publish(ctx, account, creds, post).await?),
        Op::Stream { kind, param, cursor } => OpOut::Page(
            providers::stream(ctx, account, creds, kind, param.as_deref(), cursor.as_deref()).await?,
        ),
        Op::Interact { target, action } => OpOut::Target(providers::interact(ctx, account, creds, target, *action).await?),
        Op::Delete(id) => {
            providers::delete(ctx, account, creds, id).await?;
            OpOut::Done
        }
    })
}

struct PendingFlow {
    id: String,
    begin: OAuthBegin,
    flow: Value,
    server: Option<LoopbackServer>,
    cancel: Option<oneshot::Sender<()>>,
    cancel_rx: Option<oneshot::Receiver<()>>,
}

pub struct Service {
    pub store: Store,
    secrets: Arc<SecretStore>,
    pub ctx: Ctx,
    pub data_dir: PathBuf,
    pub media_dir: PathBuf,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    flow: tokio::sync::Mutex<Option<PendingFlow>>,
    sink: Arc<dyn EventSink>,
}

const SETTINGS_KEY: &str = "settings";
const SECRET_BACKEND_KEY: &str = "secretBackend";

fn account_secret_key(id: &str) -> String {
    format!("account:{id}")
}

fn app_secret_key(p: Platform) -> String {
    format!("app:{}", p.as_str())
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn local_day(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

impl Service {
    /// Opens (or creates) the app's data in `data_dir`.
    pub fn open(data_dir: &Path, sink: Arc<dyn EventSink>) -> AppResult<Arc<Service>> {
        std::fs::create_dir_all(data_dir)?;
        let store = Store::open(&data_dir.join("connector.db"))?;
        let remembered = store.get_setting(SECRET_BACKEND_KEY)?.and_then(|v| v.as_str().map(str::to_string));
        let secrets = SecretStore::open(data_dir, remembered.as_deref());
        if remembered.as_deref() != Some(secrets.backend().as_str()) {
            store.set_setting(SECRET_BACKEND_KEY, &json!(secrets.backend().as_str()))?;
        }
        Ok(Self::build(store, secrets, data_dir, Ctx::new(Http::new()), sink))
    }

    /// For tests: in-memory database, file secrets in `data_dir`, custom context.
    pub fn open_for_test(data_dir: &Path, ctx: Ctx) -> Arc<Service> {
        let store = Store::open_in_memory().expect("db");
        let secrets = SecretStore::open(data_dir, Some("file"));
        Self::build(store, secrets, data_dir, ctx, Arc::new(NoopSink))
    }

    fn build(store: Store, secrets: SecretStore, data_dir: &Path, ctx: Ctx, sink: Arc<dyn EventSink>) -> Arc<Service> {
        Arc::new(Service {
            store,
            secrets: Arc::new(secrets),
            ctx,
            data_dir: data_dir.to_path_buf(),
            media_dir: data_dir.join("media"),
            locks: Mutex::new(HashMap::new()),
            flow: tokio::sync::Mutex::new(None),
            sink,
        })
    }

    // ------------------------------------------------------------- secrets

    async fn secret_get(&self, key: String) -> AppResult<Option<String>> {
        let s = self.secrets.clone();
        tokio::task::spawn_blocking(move || s.get(&key))
            .await
            .map_err(|e| AppError::other(e.to_string()))?
    }

    async fn secret_set(&self, key: String, value: String) -> AppResult<()> {
        let s = self.secrets.clone();
        tokio::task::spawn_blocking(move || s.set(&key, &value))
            .await
            .map_err(|e| AppError::other(e.to_string()))?
    }

    async fn secret_delete(&self, key: String) -> AppResult<()> {
        let s = self.secrets.clone();
        tokio::task::spawn_blocking(move || s.delete(&key))
            .await
            .map_err(|e| AppError::other(e.to_string()))?
    }

    async fn load_creds(&self, account_id: &str) -> AppResult<Credentials> {
        match self.secret_get(account_secret_key(account_id)).await? {
            Some(s) => Ok(serde_json::from_str(&s)?),
            None => Err(AppError::Provider(ProviderError::Unauthorized(
                "The saved sign-in for this account is missing. Reconnect it.".into(),
            ))),
        }
    }

    async fn save_creds(&self, account_id: &str, creds: &Credentials) -> AppResult<()> {
        self.secret_set(account_secret_key(account_id), serde_json::to_string(creds)?).await
    }

    // ------------------------------------------------------------ settings

    pub fn settings(&self) -> Settings {
        self.store
            .get_setting(SETTINGS_KEY)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    /// Merges `patch` into the settings and returns the result.
    pub fn update_settings(&self, patch: Value) -> AppResult<Settings> {
        let mut current = serde_json::to_value(self.settings())?;
        if let (Value::Object(cur), Value::Object(p)) = (&mut current, patch) {
            for (k, v) in p {
                cur.insert(k, v);
            }
        }
        let settings: Settings =
            serde_json::from_value(current).map_err(|e| AppError::invalid(format!("Invalid setting: {e}")))?;
        if settings.oauth_port < 1024 {
            return Err(AppError::invalid("Choose a port between 1024 and 65535."));
        }
        self.store.set_setting(SETTINGS_KEY, &serde_json::to_value(&settings)?)?;
        Ok(settings)
    }

    // ---------------------------------------------------------------- info

    pub fn redirect_uri(&self, platform: Platform) -> String {
        let info = providers::platform_info(platform);
        oauth::redirect_uri(info.connect.redirect_host, self.settings().oauth_port)
    }

    pub fn app_info(&self) -> AppResult<AppInfo> {
        Ok(AppInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            data_dir: self.data_dir.to_string_lossy().to_string(),
            secret_backend: self.secrets.backend().as_str().to_string(),
            secret_store_name: crate::secrets::system_store_name().to_string(),
            platforms: providers::all_platforms(),
            app_credentials: self.store.list_app_credentials()?,
            redirect_uris: Platform::ALL.into_iter().map(|p| (p, self.redirect_uri(p))).collect(),
        })
    }

    // ----------------------------------------------------- app credentials

    /// Saves the user's developer app for a network. `client_secret`: `None`
    /// keeps the stored secret, an empty string removes it.
    pub async fn set_app_credentials(&self, platform: Platform, client_id: &str, client_secret: Option<String>) -> AppResult<()> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err(AppError::invalid("Enter the client ID."));
        }
        let mut has_secret = self.store.get_app_credential(platform)?.map(|c| c.has_secret).unwrap_or(false);
        if let Some(secret) = client_secret {
            let secret = secret.trim().to_string();
            if secret.is_empty() {
                self.secret_delete(app_secret_key(platform)).await?;
                has_secret = false;
            } else {
                self.secret_set(app_secret_key(platform), secret).await?;
                has_secret = true;
            }
        }
        self.store.set_app_credential(platform, client_id, has_secret)
    }

    pub async fn clear_app_credentials(&self, platform: Platform) -> AppResult<()> {
        self.secret_delete(app_secret_key(platform)).await?;
        self.store.delete_app_credential(platform)
    }

    async fn app_credentials(&self, platform: Platform) -> AppResult<(Option<String>, Option<String>)> {
        let Some(info) = self.store.get_app_credential(platform)? else { return Ok((None, None)) };
        let secret = if info.has_secret { self.secret_get(app_secret_key(platform)).await? } else { None };
        Ok((Some(info.client_id), secret))
    }

    // ------------------------------------------------------------ accounts

    fn view(&self, account: Account) -> AccountView {
        let limits = providers::account_limits(&account);
        let stats = self
            .store
            .snapshots("0000-00-00")
            .ok()
            .and_then(|s| s.into_iter().rfind(|x| x.account_id == account.id));
        AccountView { account, limits, stats }
    }

    pub fn list_accounts(&self) -> AppResult<Vec<AccountView>> {
        let snaps = self.store.snapshots("0000-00-00")?;
        Ok(self
            .store
            .list_accounts()?
            .into_iter()
            .map(|a| {
                let stats = snaps.iter().rfind(|s| s.account_id == a.id).cloned();
                AccountView { limits: providers::account_limits(&a), account: a, stats }
            })
            .collect())
    }

    pub fn account(&self, id: &str) -> AppResult<Account> {
        self.store
            .get_account(id)?
            .ok_or_else(|| AppError::not_found("This account is no longer connected."))
    }

    pub async fn remove_account(&self, id: &str) -> AppResult<()> {
        let account = self.account(id)?;
        // Take the account out of posts that have not gone out yet.
        let open = [PostStatus::Draft, PostStatus::Scheduled, PostStatus::Missed, PostStatus::Failed, PostStatus::Partial];
        for mut post in self.store.list_posts(&open, None, None, 100_000)? {
            let before = post.targets.len();
            post.targets.retain(|t| t.account_id != id || t.status == TargetStatus::Published);
            if post.targets.len() != before {
                if post.targets.is_empty() && post.status == PostStatus::Scheduled {
                    post.status = PostStatus::Draft;
                }
                post.updated_at = now_secs();
                self.store.save_post(&post)?;
            }
        }
        self.secret_delete(account_secret_key(&account.id)).await?;
        self.store.delete_account(id)?;
        self.sink.accounts_changed();
        self.sink.posts_changed();
        Ok(())
    }

    pub fn reorder_accounts(&self, ids: &[String]) -> AppResult<()> {
        self.store.reorder_accounts(ids)?;
        self.sink.accounts_changed();
        Ok(())
    }

    async fn store_new_accounts(self: &Arc<Self>, new: Vec<NewAccount>) -> AppResult<Vec<AccountView>> {
        if new.is_empty() {
            return Err(AppError::invalid("No account was found to connect."));
        }
        let settings = self.settings();
        let mut out = Vec::new();
        for na in new {
            let (account, is_new) = self.store.upsert_account(&na)?;
            self.save_creds(&account.id, &na.credentials).await?;
            if is_new {
                for s in providers::platform_info(account.platform).streams.iter().filter(|s| s.default) {
                    self.store.add_stream(&account.id, s.id, None, None, settings.default_refresh_minutes)?;
                }
                // Only notifications that arrive from now on count as new.
                self.store.set_inbox_seen(&account.id, &chrono::Utc::now().to_rfc3339())?;
            }
            let svc = self.clone();
            let id = account.id.clone();
            tokio::spawn(async move {
                if let Err(e) = svc.refresh_profile(&id).await {
                    log::warn!("first profile refresh failed: {e}");
                }
            });
            out.push(self.view(account));
        }
        self.sink.accounts_changed();
        Ok(out)
    }

    /// Starts a browser sign-in. The caller opens `auth_url` and then calls
    /// [`Service::complete_connect`] to wait for the result.
    pub async fn begin_connect(&self, platform: Platform, fields: HashMap<String, String>) -> AppResult<ConnectStart> {
        let info = providers::platform_info(platform);
        if info.connect.method != "oauth" {
            return Err(AppError::invalid(format!("{} is connected with a form, not a browser sign-in.", info.name)));
        }
        self.cancel_connect().await;
        let (client_id, client_secret) = if info.connect.needs_app {
            self.app_credentials(platform).await?
        } else {
            (None, None)
        };
        if info.connect.needs_app && client_id.is_none() {
            return Err(AppError::invalid(format!(
                "Add your {} developer app in Settings → Developer apps first.",
                info.name
            )));
        }
        let port = self.settings().oauth_port;
        let mut server = None;
        for _ in 0..20 {
            match LoopbackServer::bind(port).await {
                Ok(s) => {
                    server = Some(s);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let server = server.ok_or_else(|| {
            AppError::invalid(format!(
                "Port {port} on this computer is in use, so the sign-in can't come back to the app. Choose another port in Settings → Advanced (and update the redirect URI in your developer apps)."
            ))
        })?;
        let begin = OAuthBegin {
            platform,
            fields,
            redirect_uri: oauth::redirect_uri(info.connect.redirect_host, server.port),
            state: oauth::random_token(16),
            pkce: Pkce::new(),
            client_id,
            client_secret,
        };
        let start = providers::begin_oauth(&self.ctx, &begin).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        *self.flow.lock().await = Some(PendingFlow {
            id: id.clone(),
            begin,
            flow: start.flow,
            server: Some(server),
            cancel: Some(tx),
            cancel_rx: Some(rx),
        });
        log::info!("sign-in started for {}", platform.name());
        Ok(ConnectStart { flow_id: id, auth_url: start.auth_url })
    }

    /// Waits for the browser to come back, then finishes connecting.
    pub async fn complete_connect(self: &Arc<Self>, flow_id: &str) -> AppResult<Vec<AccountView>> {
        let (mut server, cancel_rx, begin, flow) = {
            let mut guard = self.flow.lock().await;
            let pending = guard
                .as_mut()
                .filter(|f| f.id == flow_id)
                .ok_or_else(|| AppError::Cancelled("This sign-in is no longer active.".into()))?;
            let server = pending
                .server
                .take()
                .ok_or_else(|| AppError::Cancelled("This sign-in is already being completed.".into()))?;
            let rx = pending.cancel_rx.take().ok_or_else(|| AppError::Cancelled("Sign-in cancelled.".into()))?;
            (server, rx, pending.begin.clone(), pending.flow.clone())
        };
        let callback = tokio::select! {
            cb = server.next(Duration::from_secs(15 * 60)) => cb,
            _ = cancel_rx => None,
        };
        let cleanup = async {
            let mut guard = self.flow.lock().await;
            if guard.as_ref().map(|f| f.id == flow_id).unwrap_or(false) {
                *guard = None;
            }
        };
        let Some(mut cb) = callback else {
            cleanup.await;
            return Err(AppError::Cancelled("Sign-in cancelled.".into()));
        };
        let outcome = match providers::finish_oauth(&self.ctx, &begin, &flow, &cb.params).await {
            Ok(new) => self.store_new_accounts(new).await,
            Err(e) => Err(AppError::from(e)),
        };
        match &outcome {
            Ok(accounts) => {
                let names: Vec<String> = accounts.iter().map(|a| a.account.handle.clone()).collect();
                cb.respond(CallbackPage::Success(format!(
                    "{} is now connected to Social Media Connector.",
                    names.join(", ")
                )));
            }
            Err(e) => cb.respond(CallbackPage::Failure(e.to_string())),
        }
        cleanup.await;
        drop(server);
        outcome
    }

    pub async fn cancel_connect(&self) {
        let mut guard = self.flow.lock().await;
        if let Some(mut f) = guard.take()
            && let Some(tx) = f.cancel.take() {
                let _ = tx.send(());
            }
    }

    /// Connects a network that uses a form (app password, bot token, webhook).
    pub async fn connect_form(self: &Arc<Self>, platform: Platform, fields: HashMap<String, String>) -> AppResult<Vec<AccountView>> {
        let new = providers::connect_form(&self.ctx, platform, &fields).await?;
        self.store_new_accounts(new).await
    }

    fn lock_for(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Credentials for `account`, renewed first if they are about to expire.
    async fn fresh_credentials(&self, account: &Account) -> AppResult<Credentials> {
        let lock = self.lock_for(&account.id);
        let _guard = lock.lock().await;
        let creds = self.load_creds(&account.id).await?;
        if creds.should_refresh(now_secs(), 120) {
            match providers::refresh(&self.ctx, account, &creds).await {
                Ok(new) => {
                    self.save_creds(&account.id, &new).await?;
                    return Ok(new);
                }
                Err(ProviderError::Unauthorized(m)) | Err(ProviderError::Invalid(m)) | Err(ProviderError::Forbidden(m)) => {
                    self.mark_reconnect(account, &m);
                    return Err(ProviderError::Unauthorized(m).into());
                }
                // Network trouble: try with the current token anyway.
                Err(e) => log::warn!("token refresh failed for {}: {e}", account.handle),
            }
        }
        Ok(creds)
    }

    /// After a rejected token: renew it unless someone else already did.
    async fn refresh_after_unauthorized(&self, account: &Account, used: &Credentials) -> Option<Credentials> {
        let lock = self.lock_for(&account.id);
        let _guard = lock.lock().await;
        let current = self.load_creds(&account.id).await.ok()?;
        if current.access_token != used.access_token {
            return Some(current);
        }
        if !providers::can_refresh(account, &current) {
            return None;
        }
        match providers::refresh(&self.ctx, account, &current).await {
            Ok(new) => {
                if self.save_creds(&account.id, &new).await.is_ok() {
                    Some(new)
                } else {
                    None
                }
            }
            Err(e) => {
                log::warn!("refresh after 401 failed for {}: {e}", account.handle);
                None
            }
        }
    }

    fn mark_reconnect(&self, account: &Account, message: &str) {
        if self
            .store
            .set_account_status(&account.id, AccountStatus::NeedsReconnect, Some(message))
            .is_ok()
        {
            self.sink.accounts_changed();
        }
    }

    fn mark_ok(&self, account: &Account) {
        if account.status != AccountStatus::Ok
            && self.store.set_account_status(&account.id, AccountStatus::Ok, None).is_ok()
        {
            self.sink.accounts_changed();
        }
    }

    /// Runs `op` for an account, renewing the token and retrying once if the
    /// network rejects it.
    async fn with_account(&self, account_id: &str, op: Op) -> AppResult<OpOut> {
        let account = self.account(account_id)?;
        let creds = self.fresh_credentials(&account).await?;
        match run_op(&self.ctx, &account, &creds, &op).await {
            Ok(v) => {
                self.mark_ok(&account);
                Ok(v)
            }
            Err(ProviderError::Unauthorized(msg)) => match self.refresh_after_unauthorized(&account, &creds).await {
                Some(new) => match run_op(&self.ctx, &account, &new, &op).await {
                    Ok(v) => {
                        self.mark_ok(&account);
                        Ok(v)
                    }
                    Err(ProviderError::Unauthorized(m)) => {
                        self.mark_reconnect(&account, &m);
                        Err(ProviderError::Unauthorized(m).into())
                    }
                    Err(e) => Err(e.into()),
                },
                None => {
                    self.mark_reconnect(&account, &msg);
                    Err(ProviderError::Unauthorized(msg).into())
                }
            },
            Err(e) => Err(e.into()),
        }
    }

    /// Fetches fresh profile data and records today's follower counts.
    pub async fn refresh_profile(&self, account_id: &str) -> AppResult<AccountView> {
        let OpOut::Profile(update) = self.with_account(account_id, Op::Profile).await? else {
            unreachable!()
        };
        self.store.update_account_profile(account_id, &update)?;
        let info = providers::platform_info(self.account(account_id)?.platform);
        if info.features.stats || update.stats.followers.is_some() {
            self.store.save_snapshot(account_id, &today(), &update.stats)?;
        }
        self.sink.accounts_changed();
        Ok(self.view(self.account(account_id)?))
    }

    // ------------------------------------------------------------- streams

    pub fn list_streams(&self) -> AppResult<Vec<Stream>> {
        self.store.list_streams()
    }

    pub fn add_stream(&self, account_id: &str, kind: &str, param: Option<String>) -> AppResult<Stream> {
        let account = self.account(account_id)?;
        let info = providers::platform_info(account.platform);
        let kind_info = info
            .streams
            .iter()
            .find(|s| s.id == kind)
            .ok_or_else(|| AppError::invalid(format!("{} has no such stream.", info.name)))?;
        let param = param.map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
        if let (Some(p), None) = (&kind_info.param, &param) {
            return Err(AppError::invalid(format!("Enter a {}.", p.label.to_lowercase())));
        }
        self.store
            .add_stream(account_id, kind, param.as_deref(), None, self.settings().default_refresh_minutes)
    }

    pub fn update_stream(&self, id: &str, title: Option<String>, refresh_minutes: i64) -> AppResult<()> {
        self.store
            .update_stream(id, title.as_deref().filter(|t| !t.trim().is_empty()), refresh_minutes.clamp(0, 24 * 60))
    }

    pub fn remove_stream(&self, id: &str) -> AppResult<()> {
        self.store.delete_stream(id)
    }

    pub fn reorder_streams(&self, ids: &[String]) -> AppResult<()> {
        self.store.reorder_streams(ids)
    }

    pub async fn load_stream(&self, stream_id: &str, cursor: Option<String>) -> AppResult<FeedPage> {
        let stream = self
            .store
            .get_stream(stream_id)?
            .ok_or_else(|| AppError::not_found("This stream was removed."))?;
        self.fetch_page(&stream.account_id, &stream.kind, stream.param.clone(), cursor).await
    }

    pub async fn fetch_page(&self, account_id: &str, kind: &str, param: Option<String>, cursor: Option<String>) -> AppResult<FeedPage> {
        let op = Op::Stream { kind: kind.to_string(), param, cursor };
        let OpOut::Page(mut page) = self.with_account(account_id, op).await? else { unreachable!() };
        for item in page.items.iter_mut() {
            item.account_id = account_id.to_string();
            if let Some(q) = item.quote.as_mut() {
                q.account_id = account_id.to_string();
            }
        }
        Ok(page)
    }

    pub async fn interact(&self, account_id: &str, target: Value, action: Interaction) -> AppResult<Value> {
        let OpOut::Target(t) = self.with_account(account_id, Op::Interact { target, action }).await? else {
            unreachable!()
        };
        Ok(t)
    }

    /// The account's own recent posts, with their like/repost counts.
    pub async fn own_posts(&self, account_id: &str) -> AppResult<FeedPage> {
        let account = self.account(account_id)?;
        let info = providers::platform_info(account.platform);
        let Some(kind) = info.streams.iter().find(|s| s.own_posts) else {
            return Ok(FeedPage::default());
        };
        self.fetch_page(account_id, kind.id, None, None).await
    }

    // --------------------------------------------------------------- inbox

    pub async fn inbox(&self, account_ids: Option<Vec<String>>, cursors: HashMap<String, String>) -> AppResult<InboxPage> {
        let accounts: Vec<Account> = self
            .store
            .list_accounts()?
            .into_iter()
            .filter(|a| providers::platform_info(a.platform).features.inbox)
            .filter(|a| account_ids.as_ref().map(|ids| ids.contains(&a.id)).unwrap_or(true))
            // When paging, only ask accounts that still have more.
            .filter(|a| cursors.is_empty() || cursors.contains_key(&a.id))
            .collect();
        let futures = accounts.iter().map(|a| {
            let cursor = cursors.get(&a.id).cloned();
            async move { (a.id.clone(), self.fetch_page(&a.id, "notifications", None, cursor).await) }
        });
        let results = futures::future::join_all(futures).await;
        let mut page = InboxPage::default();
        for (id, r) in results {
            match r {
                Ok(p) => {
                    page.items.extend(p.items);
                    if let Some(c) = p.cursor {
                        page.cursors.insert(id.clone(), c);
                    }
                }
                Err(e) => page.errors.push(AccountError { account_id: id.clone(), message: e.to_string() }),
            }
            if let Some(seen) = self.store.inbox_seen(&id)? {
                page.seen.insert(id, seen);
            }
        }
        page.items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(page)
    }

    pub fn mark_inbox_seen(&self, account_ids: Option<Vec<String>>) -> AppResult<()> {
        let now = chrono::Utc::now().to_rfc3339();
        for a in self.store.list_accounts()? {
            if account_ids.as_ref().map(|ids| ids.contains(&a.id)).unwrap_or(true) {
                self.store.set_inbox_seen(&a.id, &now)?;
            }
        }
        Ok(())
    }

    // --------------------------------------------------------------- media

    pub fn import_files(&self, paths: &[String]) -> AppResult<Vec<MediaAttachment>> {
        paths
            .iter()
            .map(|p| crate::media::import_file(&self.media_dir, Path::new(p)))
            .collect()
    }

    pub fn import_bytes(&self, name: &str, bytes: &[u8]) -> AppResult<MediaAttachment> {
        crate::media::import_bytes(&self.media_dir, bytes, name)
    }

    // --------------------------------------------------------------- posts

    pub fn list_posts(&self, statuses: Vec<PostStatus>, from: Option<i64>, to: Option<i64>) -> AppResult<Vec<Post>> {
        self.store.list_posts(&statuses, from, to, 2000)
    }

    pub fn get_post(&self, id: &str) -> AppResult<Post> {
        self.store.get_post(id)?.ok_or_else(|| AppError::not_found("This post no longer exists."))
    }

    fn outgoing(post: &Post, target: &PostTarget) -> OutgoingPost {
        OutgoingPost {
            text: target.body_override.clone().unwrap_or_else(|| post.body.clone()),
            media: post.media.clone(),
            options: post.options.clone(),
            reply_to: post
                .reply_to
                .as_ref()
                .filter(|r| r.account_id == target.account_id)
                .map(|r| r.target.clone()),
            idempotency_key: format!("{}-{}", post.id, target.account_id),
        }
    }

    /// Problems per selected account that would stop the post going out.
    pub fn validate_post(&self, input: &SavePostInput) -> AppResult<Vec<TargetProblem>> {
        let mut out = Vec::new();
        for t in &input.targets {
            let Some(account) = self.store.get_account(&t.account_id)? else {
                out.push(TargetProblem {
                    account_id: t.account_id.clone(),
                    problems: vec!["This account is no longer connected.".into()],
                });
                continue;
            };
            let outgoing = OutgoingPost {
                text: t.body_override.clone().unwrap_or_else(|| input.body.clone()),
                media: input.media.clone(),
                options: input.options.clone(),
                reply_to: input
                    .reply_to
                    .as_ref()
                    .filter(|r| r.account_id == t.account_id)
                    .map(|r| r.target.clone()),
                idempotency_key: String::new(),
            };
            let problems = providers::validate(&account, &outgoing);
            if !problems.is_empty() {
                out.push(TargetProblem { account_id: account.id, problems });
            }
        }
        Ok(out)
    }

    /// Saves a draft, schedules a post, or publishes it now.
    pub async fn save_post(self: &Arc<Self>, input: SavePostInput) -> AppResult<Post> {
        let action = input.action.as_str();
        if !matches!(action, "draft" | "schedule" | "publish") {
            return Err(AppError::invalid("Unknown action."));
        }
        if action != "draft" {
            if input.targets.is_empty() {
                return Err(AppError::invalid("Choose at least one account to post to."));
            }
            let problems = self.validate_post(&input)?;
            if !problems.is_empty() {
                let mut lines = Vec::new();
                for p in problems {
                    let name = self
                        .store
                        .get_account(&p.account_id)?
                        .map(|a| format!("{} ({})", a.platform.name(), a.handle))
                        .unwrap_or_else(|| "Removed account".into());
                    lines.push(format!("{name}: {}", p.problems.join(" ")));
                }
                return Err(AppError::invalid(lines.join("\n")));
            }
        }
        let scheduled_at = if action == "schedule" {
            let at = input.scheduled_at.ok_or_else(|| AppError::invalid("Pick a date and time."))?;
            if at < now_millis() - 60_000 {
                return Err(AppError::invalid("That time has already passed. Pick a time in the future."));
            }
            Some(at)
        } else {
            input.scheduled_at
        };
        let now = now_secs();
        let existing = match &input.id {
            Some(id) => self.store.get_post(id)?,
            None => None,
        };
        if let Some(e) = &existing
            && matches!(e.status, PostStatus::Publishing | PostStatus::Published) {
                return Err(AppError::invalid("This post has already gone out and can't be changed."));
            }
        let mut targets = Vec::new();
        for t in &input.targets {
            let account = self.account(&t.account_id)?;
            let previous = existing
                .as_ref()
                .and_then(|e| e.targets.iter().find(|x| x.account_id == t.account_id))
                .cloned();
            let body_override = t.body_override.clone().filter(|b| *b != input.body);
            let target = match previous {
                Some(p) if p.status == TargetStatus::Published => p,
                _ => PostTarget {
                    account_id: account.id.clone(),
                    platform: account.platform,
                    account_label: format!("{} {}", account.display_name, account.handle),
                    body_override,
                    status: TargetStatus::Pending,
                    remote_id: None,
                    remote_url: None,
                    error: None,
                    published_at: None,
                },
            };
            targets.push(target);
        }
        let post = Post {
            id: existing.as_ref().map(|e| e.id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            body: input.body.clone(),
            media: input.media.clone(),
            options: input.options.clone(),
            reply_to: input.reply_to.clone(),
            status: if action == "schedule" { PostStatus::Scheduled } else { PostStatus::Draft },
            scheduled_at,
            published_at: existing.as_ref().and_then(|e| e.published_at),
            created_at: existing.as_ref().map(|e| e.created_at).unwrap_or(now),
            updated_at: now,
            last_error: None,
            targets,
        };
        self.store.save_post(&post)?;
        // Remember the selection for the next post.
        if !input.targets.is_empty() && input.reply_to.is_none() {
            let ids: Vec<String> = input.targets.iter().map(|t| t.account_id.clone()).collect();
            let _ = self.update_settings(json!({ "lastAccounts": ids }));
        }
        self.sink.posts_changed();
        if action == "publish" {
            return self.publish_post(&post.id, Trigger::Manual).await;
        }
        Ok(post)
    }

    pub fn delete_post(&self, id: &str) -> AppResult<()> {
        let post = self.get_post(id)?;
        if post.status == PostStatus::Publishing {
            return Err(AppError::invalid("This post is being published right now."));
        }
        self.store.delete_post(id)?;
        // Remove image files no other post uses.
        let referenced = self.store.referenced_media()?;
        for m in post.media {
            if !referenced.contains(&m.path) && m.path.starts_with(&*self.media_dir.to_string_lossy()) {
                let _ = std::fs::remove_file(&m.path);
            }
        }
        self.sink.posts_changed();
        Ok(())
    }

    /// Moves a post back to drafts (unschedules it).
    pub fn unschedule_post(&self, id: &str) -> AppResult<Post> {
        if !self.store.transition_post(id, &[PostStatus::Scheduled, PostStatus::Missed], PostStatus::Draft)? {
            return Err(AppError::invalid("Only scheduled or missed posts can be moved to drafts."));
        }
        self.sink.posts_changed();
        self.get_post(id)
    }

    /// Publishes every account of a post that has not received it yet.
    pub async fn publish_post(self: &Arc<Self>, id: &str, trigger: Trigger) -> AppResult<Post> {
        let from: &[PostStatus] = match trigger {
            Trigger::Manual => &[
                PostStatus::Draft,
                PostStatus::Scheduled,
                PostStatus::Failed,
                PostStatus::Partial,
                PostStatus::Missed,
            ],
            Trigger::Scheduled => &[PostStatus::Scheduled],
        };
        if !self.store.transition_post(id, from, PostStatus::Publishing)? {
            return Err(AppError::invalid("This post is already being published or has been published."));
        }
        self.sink.posts_changed();
        let post = self.get_post(id)?;
        let pending: Vec<PostTarget> = post
            .targets
            .iter()
            .filter(|t| t.status != TargetStatus::Published)
            .cloned()
            .collect();
        for t in &pending {
            self.store.set_target_status(id, &t.account_id, TargetStatus::Publishing, None, None)?;
        }
        let jobs = pending.iter().map(|t| {
            let outgoing = Self::outgoing(&post, t);
            let svc = self.clone();
            let account_id = t.account_id.clone();
            async move {
                let result = match svc.store.get_account(&account_id) {
                    Ok(Some(account)) => {
                        let problems = providers::validate(&account, &outgoing);
                        if problems.is_empty() {
                            match svc.with_account(&account_id, Op::Publish(Box::new(outgoing))).await {
                                Ok(OpOut::Published(r)) => Ok(r),
                                Ok(_) => Err("Unexpected result.".to_string()),
                                Err(e) => Err(e.to_string()),
                            }
                        } else {
                            Err(problems.join(" "))
                        }
                    }
                    Ok(None) => Err("This account is no longer connected.".to_string()),
                    Err(e) => Err(e.to_string()),
                };
                (account_id, result)
            }
        });
        let results = futures::future::join_all(jobs).await;
        for (account_id, r) in &results {
            match r {
                Ok(res) => self.store.set_target_status(id, account_id, TargetStatus::Published, Some(res), None)?,
                Err(e) => self.store.set_target_status(id, account_id, TargetStatus::Failed, None, Some(e))?,
            }
        }
        let post = self.get_post(id)?;
        let ok = post.targets.iter().filter(|t| t.status == TargetStatus::Published).count();
        let total = post.targets.len();
        let status = if total > 0 && ok == total {
            PostStatus::Published
        } else if ok > 0 {
            PostStatus::Partial
        } else {
            PostStatus::Failed
        };
        let failures: Vec<String> = post
            .targets
            .iter()
            .filter(|t| t.status == TargetStatus::Failed)
            .map(|t| format!("{}: {}", t.platform.name(), t.error.clone().unwrap_or_default()))
            .collect();
        let last_error = if failures.is_empty() { None } else { Some(failures.join("\n")) };
        let published_at = post
            .targets
            .iter()
            .filter_map(|t| t.published_at)
            .max();
        self.store.set_post_result(id, status, published_at, last_error.as_deref())?;
        self.sink.posts_changed();

        let settings = self.settings();
        let preview = crate::text::preview(&post.body, 80);
        match status {
            PostStatus::Published if settings.notify_published => {
                self.sink.notify("Post published", &format!("Sent to {ok} account{}: {preview}", if ok == 1 { "" } else { "s" }))
            }
            PostStatus::Partial if settings.notify_failed => self.sink.notify(
                "Post partly published",
                &format!("{ok} of {total} accounts received it. {}", failures.join(" ")),
            ),
            PostStatus::Failed if settings.notify_failed => {
                self.sink.notify("Post failed", &format!("{preview}\n{}", failures.join(" ")))
            }
            _ => {}
        }
        if trigger == Trigger::Manual && status == PostStatus::Failed {
            return Err(AppError::invalid(last_error.unwrap_or_else(|| "Publishing failed.".into())));
        }
        self.get_post(id)
    }

    /// Deletes a published post from one network.
    pub async fn delete_remote(&self, post_id: &str, account_id: &str) -> AppResult<Post> {
        let post = self.get_post(post_id)?;
        let target = post
            .targets
            .iter()
            .find(|t| t.account_id == account_id)
            .ok_or_else(|| AppError::not_found("That account is not part of this post."))?;
        let remote = target
            .remote_id
            .clone()
            .filter(|r| !r.is_empty())
            .ok_or_else(|| AppError::invalid("This post was not published to that account."))?;
        self.with_account(account_id, Op::Delete(remote)).await?;
        self.store.clear_target_remote(post_id, account_id)?;
        self.sink.posts_changed();
        self.get_post(post_id)
    }

    /// Deletes an item from a stream (one of the account's own posts).
    pub async fn delete_item(&self, account_id: &str, remote_id: &str) -> AppResult<()> {
        self.with_account(account_id, Op::Delete(remote_id.to_string())).await.map(|_| ())
    }

    // ----------------------------------------------------------- scheduler

    /// Cleans up after a crash or forced quit. Call once at startup.
    pub fn recover(&self) -> AppResult<()> {
        for post in self.store.interrupted_posts()? {
            for t in post.targets.iter().filter(|t| t.status == TargetStatus::Publishing) {
                self.store.set_target_status(
                    &post.id,
                    &t.account_id,
                    TargetStatus::Failed,
                    None,
                    Some("The app closed while this was being sent. Check the account before retrying; it may have been posted."),
                )?;
            }
            let post = self.get_post(&post.id)?;
            let ok = post.targets.iter().filter(|t| t.status == TargetStatus::Published).count();
            let status = if ok == 0 {
                PostStatus::Failed
            } else if ok == post.targets.len() {
                PostStatus::Published
            } else {
                PostStatus::Partial
            };
            self.store.set_post_result(&post.id, status, None, Some("Interrupted while publishing."))?;
        }
        let referenced = self.store.referenced_media()?;
        let removed = crate::media::cleanup_unreferenced(&self.media_dir, &referenced);
        if removed > 0 {
            log::info!("removed {removed} unused media files");
        }
        Ok(())
    }

    /// Publishes due posts and records statistics. Runs every few seconds.
    pub async fn scheduler_tick(self: &Arc<Self>) -> AppResult<()> {
        let settings = self.settings();
        let now = now_millis();
        for post in self.store.due_posts(now)? {
            let late_ms = now - post.scheduled_at.unwrap_or(now);
            if settings.late_grace_minutes >= 0 && late_ms > settings.late_grace_minutes * 60_000 {
                if self.store.transition_post(&post.id, &[PostStatus::Scheduled], PostStatus::Missed)? {
                    self.store.set_post_result(
                        &post.id,
                        PostStatus::Missed,
                        None,
                        Some("The app was not running at the scheduled time."),
                    )?;
                    self.sink.posts_changed();
                    if settings.notify_failed {
                        self.sink.notify(
                            "Scheduled post missed",
                            &format!(
                                "\"{}\" was due while the app was closed. Open the Planner to send or reschedule it.",
                                crate::text::preview(&post.body, 60)
                            ),
                        );
                    }
                }
                continue;
            }
            let svc = self.clone();
            let id = post.id.clone();
            tokio::spawn(async move {
                if let Err(e) = svc.publish_post(&id, Trigger::Scheduled).await {
                    log::warn!("scheduled publish of {id}: {e}");
                }
            });
        }
        self.snapshot_one().await;
        Ok(())
    }

    /// Records follower counts for one account that has none for today.
    async fn snapshot_one(&self) {
        let day = today();
        let Ok(accounts) = self.store.list_accounts() else { return };
        for a in accounts {
            if a.status != AccountStatus::Ok || !providers::platform_info(a.platform).features.stats {
                continue;
            }
            let last = self.store.last_snapshot_day(&a.id).ok().flatten();
            if last.as_deref() == Some(day.as_str()) {
                continue;
            }
            // Avoid hammering an account whose refresh keeps failing.
            let key = format!("snapshotAttempt:{}", a.id);
            let attempted = self.store.get_setting(&key).ok().flatten().and_then(|v| v.as_i64()).unwrap_or(0);
            if now_secs() - attempted < 3600 {
                continue;
            }
            let _ = self.store.set_setting(&key, &json!(now_secs()));
            if let Err(e) = self.refresh_profile(&a.id).await {
                log::info!("daily stats for {} failed: {e}", a.handle);
            }
            return;
        }
    }

    pub async fn run_scheduler(self: Arc<Self>) {
        // Let the window come up first.
        tokio::time::sleep(Duration::from_secs(3)).await;
        loop {
            if let Err(e) = self.scheduler_tick().await {
                log::warn!("scheduler: {e}");
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }

    // ----------------------------------------------------------- analytics

    pub fn analytics(&self) -> AppResult<AnalyticsOverview> {
        let since = chrono::Local::now() - chrono::Duration::days(90);
        let snaps = self.store.snapshots(&since.format("%Y-%m-%d").to_string())?;
        let thirty_ms = now_millis() - 30 * 24 * 3600 * 1000;
        let published = self.store.published_targets_since(thirty_ms)?;
        let mut per_account: HashMap<String, u32> = HashMap::new();
        let mut per_day: HashMap<String, u32> = HashMap::new();
        for (account_id, _, at) in &published {
            *per_account.entry(account_id.clone()).or_default() += 1;
            *per_day.entry(local_day(*at)).or_default() += 1;
        }
        let mut daily = Vec::new();
        for i in (0..30).rev() {
            let d = (chrono::Local::now() - chrono::Duration::days(i)).format("%Y-%m-%d").to_string();
            daily.push(DailyCount { count: per_day.get(&d).copied().unwrap_or(0), day: d });
        }
        let accounts = self
            .store
            .list_accounts()?
            .into_iter()
            .map(|a| AccountAnalytics {
                history: snaps.iter().filter(|s| s.account_id == a.id).cloned().collect(),
                published_30d: per_account.get(&a.id).copied().unwrap_or(0),
                account_id: a.id,
            })
            .collect();
        let count = |s: &[PostStatus], from: Option<i64>| -> AppResult<u32> {
            Ok(self.store.list_posts(s, from, None, 100_000)?.len() as u32)
        };
        Ok(AnalyticsOverview {
            accounts,
            daily,
            scheduled: count(&[PostStatus::Scheduled], None)?,
            drafts: count(&[PostStatus::Draft], None)?,
            published_30d: published.len() as u32,
            failed_30d: count(&[PostStatus::Failed, PostStatus::Partial, PostStatus::Missed], Some(thirty_ms))?,
        })
    }

    pub fn known_accounts(&self) -> HashSet<String> {
        self.store
            .list_accounts()
            .map(|a| a.into_iter().map(|a| a.id).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn mastodon_account(svc: &Arc<Service>, server: &str, token: &str) -> String {
        let new = NewAccount {
            platform: Platform::Mastodon,
            server: server.into(),
            remote_id: "1".into(),
            handle: "@me@test".into(),
            display_name: "Me".into(),
            avatar_url: None,
            profile_url: None,
            meta: json!({}),
            credentials: Credentials::bearer(token),
        };
        svc.store_new_accounts(vec![new]).await.unwrap()[0].account.id.clone()
    }

    fn input(account_id: &str, action: &str, body: &str) -> SavePostInput {
        SavePostInput {
            id: None,
            body: body.into(),
            media: vec![],
            options: PostOptions::default(),
            reply_to: None,
            targets: vec![TargetInput { account_id: account_id.into(), body_override: None }],
            action: action.into(),
            scheduled_at: None,
        }
    }

    #[tokio::test]
    async fn publish_now_success_and_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/statuses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "9", "url": "https://test/@me/9"})))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open_for_test(dir.path(), Ctx::default());
        let id = mastodon_account(&svc, &server.uri(), "tok").await;
        // Default streams were created for the new account.
        assert_eq!(svc.list_streams().unwrap().len(), 2);

        let post = svc.save_post(input(&id, "publish", "hello")).await.unwrap();
        assert_eq!(post.status, PostStatus::Published);
        assert_eq!(post.targets[0].remote_url.as_deref(), Some("https://test/@me/9"));

        // Too long for the 500 character limit: rejected before sending.
        let err = svc.save_post(input(&id, "publish", &"x".repeat(501))).await.unwrap_err();
        assert!(err.to_string().contains("allows 500"), "{err}");
    }

    #[tokio::test]
    async fn unauthorized_marks_account_for_reconnect() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/statuses"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "The access token was revoked"})))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open_for_test(dir.path(), Ctx::default());
        let id = mastodon_account(&svc, &server.uri(), "tok").await;
        let err = svc.save_post(input(&id, "publish", "hello")).await.unwrap_err();
        assert!(err.to_string().contains("revoked"), "{err}");
        let acc = svc.account(&id).unwrap();
        assert_eq!(acc.status, AccountStatus::NeedsReconnect);
        let posts = svc.list_posts(vec![PostStatus::Failed], None, None).unwrap();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].targets[0].error.as_deref().unwrap().contains("revoked"));
    }

    #[tokio::test]
    async fn scheduler_publishes_due_posts_and_marks_missed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/statuses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "9", "url": "https://test/@me/9"})))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open_for_test(dir.path(), Ctx::default());
        let id = mastodon_account(&svc, &server.uri(), "tok").await;

        let mut due = input(&id, "schedule", "due now");
        due.scheduled_at = Some(now_millis() + 1000);
        let due = svc.save_post(due).await.unwrap();
        assert_eq!(due.status, PostStatus::Scheduled);
        // Past times are refused when scheduling.
        let mut past = input(&id, "schedule", "past");
        past.scheduled_at = Some(now_millis() - 3_600_000);
        assert!(svc.save_post(past).await.is_err());

        // A post that became overdue while the app was closed.
        let mut old = svc.save_post(input(&id, "draft", "old one")).await.unwrap();
        old.status = PostStatus::Scheduled;
        old.scheduled_at = Some(now_millis() - 5 * 3_600_000);
        svc.store.save_post(&old).unwrap();

        // Not due yet.
        svc.scheduler_tick().await.unwrap();
        assert_eq!(svc.get_post(&due.id).unwrap().status, PostStatus::Scheduled);
        assert_eq!(svc.get_post(&old.id).unwrap().status, PostStatus::Missed);

        tokio::time::sleep(Duration::from_millis(1100)).await;
        svc.scheduler_tick().await.unwrap();
        for _ in 0..50 {
            if svc.get_post(&due.id).unwrap().status == PostStatus::Published {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(svc.get_post(&due.id).unwrap().status, PostStatus::Published);
        // A second tick does not publish it again (the mock expects one call).
        svc.scheduler_tick().await.unwrap();
    }

    #[tokio::test]
    async fn recovers_interrupted_posts() {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open_for_test(dir.path(), Ctx::default());
        let id = mastodon_account(&svc, "https://unused.test", "tok").await;
        let post = svc.save_post(input(&id, "draft", "x")).await.unwrap();
        svc.store.transition_post(&post.id, &[PostStatus::Draft], PostStatus::Publishing).unwrap();
        svc.store.set_target_status(&post.id, &id, TargetStatus::Publishing, None, None).unwrap();
        svc.recover().unwrap();
        let p = svc.get_post(&post.id).unwrap();
        assert_eq!(p.status, PostStatus::Failed);
        assert!(p.targets[0].error.as_deref().unwrap().contains("may have been posted"));
    }

    #[tokio::test]
    async fn removing_account_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open_for_test(dir.path(), Ctx::default());
        let id = mastodon_account(&svc, "https://unused.test", "tok").await;
        let mut sched = input(&id, "schedule", "later");
        sched.scheduled_at = Some(now_millis() + 3_600_000);
        let post = svc.save_post(sched).await.unwrap();
        svc.remove_account(&id).await.unwrap();
        assert!(svc.list_accounts().unwrap().is_empty());
        assert!(svc.list_streams().unwrap().is_empty());
        let p = svc.get_post(&post.id).unwrap();
        assert!(p.targets.is_empty());
        assert_eq!(p.status, PostStatus::Draft);
        assert!(svc.secrets.get(&account_secret_key(&id)).unwrap().is_none());
    }

    #[tokio::test]
    async fn oauth_round_trip_through_loopback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/apps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"client_id": "c", "client_secret": "s"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "tok"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/accounts/verify_credentials"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "5", "username": "me", "display_name": "Me"})))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let svc = Service::open_for_test(dir.path(), Ctx::default());
        // Pick a free port for the callback.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        svc.update_settings(json!({ "oauthPort": port })).unwrap();
        let start = svc
            .begin_connect(Platform::Mastodon, HashMap::from([("server".to_string(), server.uri())]))
            .await
            .unwrap();
        let state = url::Url::parse(&start.auth_url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .to_string();
        let waiter = {
            let svc = svc.clone();
            let flow = start.flow_id.clone();
            tokio::spawn(async move { svc.complete_connect(&flow).await })
        };
        // Play the browser: follow the redirect back to the app.
        let page = reqwest::get(format!("http://127.0.0.1:{port}/callback?code=abc&state={state}"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(page.contains("is now connected"), "{page}");
        let accounts = waiter.await.unwrap().unwrap();
        assert_eq!(accounts[0].account.display_name, "Me");
        assert_eq!(svc.list_accounts().unwrap().len(), 1);
    }
}
