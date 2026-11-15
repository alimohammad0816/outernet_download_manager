#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod store;
mod transfer;

use std::{io, sync::Arc, time::Duration};
use store::{Settings, Snapshot, Store};
use tauri::{Manager, State};

#[tauri::command]
async fn snapshot(store: State<'_, Arc<Store>>) -> Result<Snapshot, String> {
    store.snapshot()
}

#[tauri::command]
async fn save_settings(store: State<'_, Arc<Store>>, settings: Settings) -> Result<(), String> {
    store.settings(settings)
}

#[tauri::command]
async fn add_download(
    store: State<'_, Arc<Store>>,
    url: String,
    filename: String,
    sha256: String,
) -> Result<(), String> {
    store.add(url, filename, sha256)
}

#[tauri::command]
async fn retry_download(store: State<'_, Arc<Store>>, id: String) -> Result<(), String> {
    store.retry(id)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data = app.path().app_local_data_dir()?;
            std::fs::create_dir_all(&data)?;
            let working_folder = data.join("working");
            std::fs::create_dir_all(&working_folder)?;
            let destination_folder = app
                .path()
                .download_dir()
                .ok()
                .filter(|path| path.is_dir())
                .unwrap_or_else(|| data.join("completed"));
            std::fs::create_dir_all(&destination_folder)?;
            let defaults = Settings {
                working_folder,
                destination_folder,
            };
            let store = Arc::new(
                Store::open(&data.join("downloads.sqlite"), defaults).map_err(io::Error::other)?,
            );
            store.recover().map_err(io::Error::other)?;
            app.manage(Arc::clone(&store));
            std::thread::Builder::new()
                .name("downloads".into())
                .spawn(move || loop {
                    match store.process_next() {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(error) => eprintln!("{error}"),
                    }
                    std::thread::sleep(Duration::from_millis(400));
                })?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            save_settings,
            add_download,
            retry_download
        ])
        .run(tauri::generate_context!())
        .expect("Outernet could not start");
}
