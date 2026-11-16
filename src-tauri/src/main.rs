// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// WebKitGTK's DMA-BUF renderer breaks on NVIDIA's proprietary driver under
/// Wayland: the compositor drops the connection ("Error 71 (Protocol error)
/// dispatching to Wayland display") and the window closes right away. With
/// NVIDIA's driver loaded, render through shared memory instead. Variables
/// the user set themselves are left alone.
#[cfg(target_os = "linux")]
fn nvidia_wayland_workaround() {
    if !std::path::Path::new("/sys/module/nvidia").exists() {
        return;
    }
    for (key, value) in [("WEBKIT_DISABLE_DMABUF_RENDERER", "1"), ("__NV_DISABLE_EXPLICIT_SYNC", "1")] {
        if std::env::var_os(key).is_none() {
            // SAFETY: called first thing in `main`, before any other thread exists.
            unsafe { std::env::set_var(key, value) };
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    nvidia_wayland_workaround();
    social_media_connector_lib::run()
}
