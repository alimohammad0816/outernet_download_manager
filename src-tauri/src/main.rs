#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod media;
mod progress;
mod ranges;
mod store;
mod transfer;

use std::{fs, io, path::Path, sync::Arc, time::Duration};
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

#[tauri::command]
async fn pause_download(store: State<'_, Arc<Store>>, id: String) -> Result<(), String> {
    store.pause(id)
}

const APP_ID: &str = "io.outernet.downloadmanager";

fn open_store(data: &Path) -> Result<Store, io::Error> {
    fs::create_dir_all(data)?;
    let working_folder = data.join("working");
    fs::create_dir_all(&working_folder)?;
    let destination_folder = dirs::download_dir()
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| data.join("completed"));
    fs::create_dir_all(&destination_folder)?;
    Store::open(
        &data.join("downloads.sqlite"),
        Settings {
            working_folder,
            destination_folder,
            speed_limit_kib: None,
        },
    )
    .map_err(io::Error::other)
}

#[tauri::command]
async fn connect_browser(browser: String) -> Result<(), String> {
    browser::register(&browser)
}

#[tauri::command]
async fn schedule_queue(
    store: State<'_, Arc<Store>>,
    run_after: Option<u64>,
) -> Result<(), String> {
    store.schedule(run_after)
}

fn main() {
    if let Some(origin) = browser::launch_origin(std::env::args_os().skip(1)) {
        if let Err(error) = browser::run_host(&origin) {
            eprintln!("{error}");
        }
        return;
    }
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
            let store = Arc::new(open_store(&data)?);
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
            retry_download,
            pause_download,
            connect_browser,
            schedule_queue
        ])
        .run(tauri::generate_context!())
        .expect("Outernet could not start");
}
