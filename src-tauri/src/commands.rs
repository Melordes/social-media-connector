//! Commands called from the interface. Each one is a thin wrapper around
//! the service; errors reach the interface as `{ kind, message }`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use connector_core::error::AppError;
use connector_core::model::{FeedPage, Interaction, MediaAttachment, Platform};
use connector_core::service::{
    AccountView, AnalyticsOverview, AppInfo, ConnectStart, InboxPage, SavePostInput, Service, Settings, TargetProblem,
};
use connector_core::store::{Post, PostStatus, Stream};
use serde_json::Value;
use tauri::{AppHandle, State};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_opener::OpenerExt;

type Svc<'a> = State<'a, Arc<Service>>;
type R<T> = Result<T, AppError>;

#[tauri::command]
pub async fn app_info(svc: Svc<'_>) -> R<AppInfo> {
    svc.app_info()
}

#[tauri::command]
pub async fn get_settings(svc: Svc<'_>) -> R<Settings> {
    Ok(svc.settings())
}

#[tauri::command]
pub async fn update_settings(svc: Svc<'_>, patch: Value) -> R<Settings> {
    svc.update_settings(patch)
}

#[tauri::command]
pub async fn set_app_credentials(
    svc: Svc<'_>,
    platform: Platform,
    client_id: String,
    client_secret: Option<String>,
) -> R<()> {
    svc.set_app_credentials(platform, &client_id, client_secret).await
}

#[tauri::command]
pub async fn clear_app_credentials(svc: Svc<'_>, platform: Platform) -> R<()> {
    svc.clear_app_credentials(platform).await
}

#[tauri::command]
pub async fn list_accounts(svc: Svc<'_>) -> R<Vec<AccountView>> {
    svc.list_accounts()
}

/// Starts a browser sign-in and opens the network's page in the browser.
#[tauri::command]
pub async fn begin_connect(
    app: AppHandle,
    svc: Svc<'_>,
    platform: Platform,
    fields: HashMap<String, String>,
) -> R<ConnectStart> {
    let start = svc.begin_connect(platform, fields).await?;
    if let Err(e) = app.opener().open_url(start.auth_url.clone(), None::<&str>) {
        log::warn!("could not open the browser: {e}");
    }
    Ok(start)
}

#[tauri::command]
pub async fn complete_connect(svc: Svc<'_>, flow_id: String) -> R<Vec<AccountView>> {
    let svc: Arc<Service> = svc.inner().clone();
    svc.complete_connect(&flow_id).await
}

#[tauri::command]
pub async fn cancel_connect(svc: Svc<'_>) -> R<()> {
    svc.cancel_connect().await;
    Ok(())
}

#[tauri::command]
pub async fn connect_form(svc: Svc<'_>, platform: Platform, fields: HashMap<String, String>) -> R<Vec<AccountView>> {
    let svc: Arc<Service> = svc.inner().clone();
    svc.connect_form(platform, fields).await
}

#[tauri::command]
pub async fn remove_account(svc: Svc<'_>, id: String) -> R<()> {
    svc.remove_account(&id).await
}

#[tauri::command]
pub async fn refresh_account(svc: Svc<'_>, id: String) -> R<AccountView> {
    svc.refresh_profile(&id).await
}

#[tauri::command]
pub async fn reorder_accounts(svc: Svc<'_>, ids: Vec<String>) -> R<()> {
    svc.reorder_accounts(&ids)
}

#[tauri::command]
pub async fn list_streams(svc: Svc<'_>) -> R<Vec<Stream>> {
    svc.list_streams()
}

#[tauri::command]
pub async fn add_stream(svc: Svc<'_>, account_id: String, kind: String, param: Option<String>) -> R<Stream> {
    svc.add_stream(&account_id, &kind, param)
}

#[tauri::command]
pub async fn update_stream(svc: Svc<'_>, id: String, title: Option<String>, refresh_minutes: i64) -> R<()> {
    svc.update_stream(&id, title, refresh_minutes)
}

#[tauri::command]
pub async fn remove_stream(svc: Svc<'_>, id: String) -> R<()> {
    svc.remove_stream(&id)
}

#[tauri::command]
pub async fn reorder_streams(svc: Svc<'_>, ids: Vec<String>) -> R<()> {
    svc.reorder_streams(&ids)
}

#[tauri::command]
pub async fn load_stream(svc: Svc<'_>, stream_id: String, cursor: Option<String>) -> R<FeedPage> {
    svc.load_stream(&stream_id, cursor).await
}

#[tauri::command]
pub async fn interact(svc: Svc<'_>, account_id: String, target: Value, action: Interaction) -> R<Value> {
    svc.interact(&account_id, target, action).await
}

#[tauri::command]
pub async fn delete_item(svc: Svc<'_>, account_id: String, remote_id: String) -> R<()> {
    svc.delete_item(&account_id, &remote_id).await
}

#[tauri::command]
pub async fn own_posts(svc: Svc<'_>, account_id: String) -> R<FeedPage> {
    svc.own_posts(&account_id).await
}

#[tauri::command]
pub async fn load_inbox(
    svc: Svc<'_>,
    account_ids: Option<Vec<String>>,
    cursors: Option<HashMap<String, String>>,
) -> R<InboxPage> {
    svc.inbox(account_ids, cursors.unwrap_or_default()).await
}

#[tauri::command]
pub async fn mark_inbox_seen(svc: Svc<'_>, account_ids: Option<Vec<String>>) -> R<()> {
    svc.mark_inbox_seen(account_ids)
}

#[tauri::command]
pub async fn import_media(svc: Svc<'_>, paths: Vec<String>) -> R<Vec<MediaAttachment>> {
    let svc: Arc<Service> = svc.inner().clone();
    tauri::async_runtime::spawn_blocking(move || svc.import_files(&paths))
        .await
        .map_err(|e| AppError::other(e.to_string()))?
}

/// Pasted images arrive as a raw body with the file name in a header.
#[tauri::command]
pub async fn import_media_bytes(svc: Svc<'_>, request: tauri::ipc::Request<'_>) -> R<MediaAttachment> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err(AppError::invalid("Expected image data."));
    };
    let name = request
        .headers()
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "pasted-image".into());
    let bytes = bytes.clone();
    let svc: Arc<Service> = svc.inner().clone();
    tauri::async_runtime::spawn_blocking(move || svc.import_bytes(&name, &bytes))
        .await
        .map_err(|e| AppError::other(e.to_string()))?
}

#[tauri::command]
pub async fn list_posts(svc: Svc<'_>, statuses: Vec<PostStatus>, from: Option<i64>, to: Option<i64>) -> R<Vec<Post>> {
    svc.list_posts(statuses, from, to)
}

#[tauri::command]
pub async fn get_post(svc: Svc<'_>, id: String) -> R<Post> {
    svc.get_post(&id)
}

#[tauri::command]
pub async fn validate_post(svc: Svc<'_>, input: SavePostInput) -> R<Vec<TargetProblem>> {
    svc.validate_post(&input)
}

#[tauri::command]
pub async fn save_post(svc: Svc<'_>, input: SavePostInput) -> R<Post> {
    let svc: Arc<Service> = svc.inner().clone();
    svc.save_post(input).await
}

#[tauri::command]
pub async fn publish_post(svc: Svc<'_>, id: String) -> R<Post> {
    let svc: Arc<Service> = svc.inner().clone();
    svc.publish_post(&id, connector_core::service::Trigger::Manual).await
}

#[tauri::command]
pub async fn delete_post(svc: Svc<'_>, id: String) -> R<()> {
    svc.delete_post(&id)
}

#[tauri::command]
pub async fn unschedule_post(svc: Svc<'_>, id: String) -> R<Post> {
    svc.unschedule_post(&id)
}

#[tauri::command]
pub async fn delete_remote(svc: Svc<'_>, post_id: String, account_id: String) -> R<Post> {
    svc.delete_remote(&post_id, &account_id).await
}

#[tauri::command]
pub async fn analytics(svc: Svc<'_>) -> R<AnalyticsOverview> {
    svc.analytics()
}

/// Opens a web link in the default browser (http and https only).
#[tauri::command]
pub async fn open_url(app: AppHandle, url: String) -> R<()> {
    let parsed = url::Url::parse(&url).map_err(|_| AppError::invalid("Not a valid link."))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AppError::invalid("Only web links can be opened."));
    }
    app.opener()
        .open_url(parsed.to_string(), None::<&str>)
        .map_err(|e| AppError::other(format!("Could not open the browser: {e}")))
}

#[tauri::command]
pub async fn get_autostart(app: AppHandle) -> R<bool> {
    app.autolaunch().is_enabled().map_err(|e| AppError::other(e.to_string()))
}

#[tauri::command]
pub async fn set_autostart(app: AppHandle, enabled: bool) -> R<bool> {
    let al = app.autolaunch();
    if enabled { al.enable() } else { al.disable() }.map_err(|e| AppError::other(e.to_string()))?;
    al.is_enabled().map_err(|e| AppError::other(e.to_string()))
}

#[tauri::command]
pub async fn tray_available(state: State<'_, crate::TrayState>) -> R<bool> {
    Ok(state.0.load(Ordering::SeqCst))
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}
