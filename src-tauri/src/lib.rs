//! Tauri shell: window, tray, plugins and the commands the interface calls.
//! All real work happens in `connector_core::service::Service`.

mod commands;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use connector_core::service::{EventSink, Service};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;

/// Whether a tray icon exists, so closing the window may hide it.
pub struct TrayState(pub AtomicBool);

struct TauriSink {
    app: AppHandle,
}

impl EventSink for TauriSink {
    fn posts_changed(&self) {
        let _ = self.app.emit("posts-changed", ());
    }

    fn accounts_changed(&self) {
        let _ = self.app.emit("accounts-changed", ());
    }

    fn notify(&self, title: &str, body: &str) {
        if let Err(e) = self.app.notification().builder().title(title).body(body).show() {
            log::warn!("notification failed: {e}");
        }
    }
}

pub fn show_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn data_dir(app: &AppHandle) -> PathBuf {
    if let Ok(dir) = std::env::var("SMC_DATA_DIR") {
        return PathBuf::from(dir);
    }
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("social-media-connector"))
}

/// On Linux the tray icon needs libayatana-appindicator (or the older
/// libappindicator), which the tray library loads at run time and panics
/// without (and release builds abort on panic). Check for it first, so the
/// app runs without a tray icon instead of crashing. These are the names the
/// tray library tries, with its "backcompat" feature on.
#[cfg(target_os = "linux")]
fn tray_library_available() -> bool {
    const NAMES: [&str; 4] = [
        "libayatana-appindicator3.so.1",
        "libappindicator3.so.1",
        "libayatana-appindicator3.so",
        "libappindicator3.so",
    ];
    NAMES.iter().any(|name| {
        let name = std::ffi::CString::new(*name).expect("library names have no NUL");
        // SAFETY: a valid C string; the handle stays open on purpose, as the
        // tray library opens the same library right after.
        !unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY) }.is_null()
    })
}

#[cfg(not(target_os = "linux"))]
fn tray_library_available() -> bool {
    true
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Social Media Connector", true, None::<&str>)?;
    let compose = MenuItem::with_id(app, "compose", "New post…", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &compose, &separator, &quit])?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Social Media Connector")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "compose" => {
                show_main_window(app);
                let _ = app.emit("open-composer", ());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,tao=warn,wry=warn"))
        .format_timestamp_secs()
        .init();

    let start_hidden = std::env::args().any(|a| a == "--minimized");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .manage(TrayState(AtomicBool::new(false)))
        .setup(move |app| {
            let handle = app.handle().clone();
            let dir = data_dir(&handle);
            log::info!("data folder: {}", dir.display());
            let service = Service::open(&dir, Arc::new(TauriSink { app: handle.clone() }))
                .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;
            if let Err(e) = service.recover() {
                log::warn!("recovery: {e}");
            }
            // Let the interface show attached images from the media folder.
            std::fs::create_dir_all(&service.media_dir)?;
            if let Err(e) = app.asset_protocol_scope().allow_directory(&service.media_dir, true) {
                log::warn!("asset scope: {e}");
            }
            tauri::async_runtime::spawn(service.clone().run_scheduler());
            app.manage(service);

            let tray = if !tray_library_available() {
                log::warn!("no tray icon: libayatana-appindicator is not installed");
                false
            } else {
                match build_tray(&handle) {
                    Ok(()) => true,
                    Err(e) => {
                        log::warn!("no tray icon: {e}");
                        false
                    }
                }
            };
            app.state::<TrayState>().0.store(tray, Ordering::SeqCst);
            // Starting hidden (at login) needs a tray icon to bring the window back.
            if !start_hidden || !tray {
                show_main_window(&handle);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let tray = app.state::<TrayState>().0.load(Ordering::SeqCst);
                let service = app.state::<Arc<Service>>();
                let settings = service.settings();
                if tray && settings.close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                    if !settings.tray_hint_shown {
                        let _ = service.update_settings(serde_json::json!({ "trayHintShown": true }));
                        let _ = app
                            .notification()
                            .builder()
                            .title("Still running in the tray")
                            .body("Social Media Connector keeps running so your scheduled posts go out. Quit it from the tray icon.")
                            .show();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::get_settings,
            commands::update_settings,
            commands::set_app_credentials,
            commands::clear_app_credentials,
            commands::list_accounts,
            commands::begin_connect,
            commands::complete_connect,
            commands::cancel_connect,
            commands::connect_form,
            commands::remove_account,
            commands::refresh_account,
            commands::reorder_accounts,
            commands::list_streams,
            commands::add_stream,
            commands::update_stream,
            commands::remove_stream,
            commands::reorder_streams,
            commands::load_stream,
            commands::interact,
            commands::delete_item,
            commands::own_posts,
            commands::load_inbox,
            commands::mark_inbox_seen,
            commands::import_media,
            commands::import_media_bytes,
            commands::list_posts,
            commands::get_post,
            commands::validate_post,
            commands::save_post,
            commands::publish_post,
            commands::delete_post,
            commands::unschedule_post,
            commands::delete_remote,
            commands::analytics,
            commands::open_url,
            commands::get_autostart,
            commands::set_autostart,
            commands::tray_available,
            commands::quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Social Media Connector");
}
