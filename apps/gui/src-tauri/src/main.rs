//! DataReco desktop app: a Tauri shell around the engine crates.

// Release builds run without a console window on Windows. Do not remove.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;

/// Basic facts about the running app, shown in the status bar.
#[derive(Serialize)]
struct AppInfo {
    name: &'static str,
    version: &'static str,
    os: &'static str,
    arch: &'static str,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        name: "DataReco",
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_info])
        .run(tauri::generate_context!())
        .expect("error while running DataReco");
}
