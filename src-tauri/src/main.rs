#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod media;
mod network;
mod progress;
mod ranges;
mod store;
mod transfer;

use std::{fs, io, path::Path, sync::Arc, time::Duration};
use store::{DailyWindow, Priority, Settings, Snapshot, Store};
use tauri::{
    menu::MenuBuilder,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, State, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;

#[tauri::command]
async fn snapshot(store: State<'_, Arc<Store>>) -> Result<Snapshot, String> {
    store.snapshot()
}

#[tauri::command]
async fn save_settings(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    settings: Settings,
) -> Result<(), String> {
    if settings.keep_running && app.tray_by_id(TRAY_ID).is_none() {
        return Err("The system tray is unavailable on this desktop.".into());
    }
    store.settings(settings)
}

#[tauri::command]
async fn add_download(
    store: State<'_, Arc<Store>>,
    url: String,
    filename: String,
    sha256: String,
    queue_id: Option<String>,
) -> Result<(), String> {
    match queue_id {
        Some(id) => store.add_to_queue(url, filename, sha256, &id),
        None => store.add(url, filename, sha256),
    }
}

#[tauri::command]
async fn add_download_batch(
    store: State<'_, Arc<Store>>,
    links: Vec<(String, String)>,
    queue_id: Option<String>,
) -> Result<(), String> {
    match queue_id {
        Some(id) => store.add_batch_to_queue(links, &id),
        None => store.add_batch(links),
    }
}

#[tauri::command]
async fn list_network_interfaces() -> Result<Vec<network::NetworkInterface>, String> {
    network::interfaces()
}

#[tauri::command]
async fn save_download_queue(
    store: State<'_, Arc<Store>>,
    id: Option<String>,
    name: String,
    slots: u8,
    interface: Option<String>,
) -> Result<String, String> {
    store.save_queue(id, name, slots, interface)
}

#[tauri::command]
async fn set_download_queue_running(
    store: State<'_, Arc<Store>>,
    id: String,
    running: bool,
) -> Result<(), String> {
    store.set_queue_running(&id, running)
}

#[tauri::command]
async fn move_download_to_queue(
    store: State<'_, Arc<Store>>,
    id: String,
    queue_id: String,
) -> Result<(), String> {
    store.move_download(&id, &queue_id)
}

#[tauri::command]
async fn replace_download_link(
    store: State<'_, Arc<Store>>,
    id: String,
    url: String,
    sha256: Option<String>,
) -> Result<(), String> {
    store.replace_link(id, url, sha256)
}

#[tauri::command]
async fn open_download(
    store: State<'_, Arc<Store>>,
    id: String,
    reveal: bool,
) -> Result<(), String> {
    let path = store.completed_path(&id)?;
    if reveal {
        if path.is_file() && tauri_plugin_opener::reveal_item_in_dir(&path).is_ok() {
            return Ok(());
        }
        let folder = path
            .parent()
            .filter(|folder| folder.is_dir())
            .ok_or("The download folder is unavailable.")?;
        tauri_plugin_opener::open_path(folder, None::<&str>)
            .map_err(|error| format!("Cannot open the download folder: {error}"))
    } else {
        if !path.is_file() {
            return Err("The downloaded file was moved, deleted, or is unavailable.".into());
        }
        tauri_plugin_opener::open_path(&path, None::<&str>)
            .map_err(|error| format!("Cannot open the downloaded file: {error}"))
    }
}

#[tauri::command]
async fn remove_completed_download(store: State<'_, Arc<Store>>, id: String) -> Result<(), String> {
    store.remove_completed(&id)
}

#[tauri::command]
async fn retry_download(store: State<'_, Arc<Store>>, id: String) -> Result<(), String> {
    store.retry(id)
}

#[tauri::command]
async fn stop_recording(store: State<'_, Arc<Store>>, id: String) -> Result<(), String> {
    store.stop_recording(id)
}

#[tauri::command]
async fn pause_download(store: State<'_, Arc<Store>>, id: String) -> Result<(), String> {
    store.pause(id)
}

const TRAY_ID: &str = "outernet-tray";
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
            download_slots: 1,
            keep_running: false,
            notify_when_finished: false,
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
    daily_window: Option<DailyWindow>,
) -> Result<(), String> {
    store.schedule(run_after, daily_window)
}

#[tauri::command]
async fn set_priority(
    store: State<'_, Arc<Store>>,
    id: String,
    priority: Priority,
) -> Result<(), String> {
    store.set_priority(id, priority)
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn install_tray(app: &AppHandle) -> tauri::Result<()> {
    let menu = MenuBuilder::new(app)
        .text("show", "Open Outernet")
        .text("quit", "Quit Outernet")
        .build()?;
    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("Outernet Download Manager")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[tauri::command]
fn updates_enabled(app: tauri::AppHandle) -> bool {
    cfg!(windows) && app.config().plugins.0.contains_key("updater")
}

#[cfg(windows)]
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_updater::UpdaterExt;

    if !updates_enabled(app.clone()) {
        return Err("Updates are unavailable for this build.".into());
    }
    app.updater()
        .map_err(|error| error.to_string())?
        .check()
        .await
        .map(|update| update.map(|update| update.version))
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
async fn install_update(
    app: tauri::AppHandle,
    store: State<'_, Arc<Store>>,
    version: String,
) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    if !updates_enabled(app.clone()) {
        return Err("Updates are unavailable for this build.".into());
    }
    let update = app
        .updater()
        .map_err(|error| error.to_string())?
        .check()
        .await
        .map_err(|error| error.to_string())?
        .filter(|update| update.version == version)
        .ok_or("Check for updates again. The available version has changed.")?;
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|error| error.to_string())?;
    store.while_workers_idle(|| update.install(bytes).map_err(|error| error.to_string()))
}

fn main() {
    if let Some(origin) = browser::launch_origin(std::env::args_os().skip(1)) {
        if let Err(error) = browser::run_host(&origin) {
            eprintln!("{error}");
        }
        return;
    }
    let builder =
        tauri::Builder::default().plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_main(app)
        }));
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if window.label() == "main"
                    && app.tray_by_id(TRAY_ID).is_some()
                    && app
                        .state::<Arc<Store>>()
                        .current_settings()
                        .is_ok_and(|settings| settings.keep_running)
                    && window.hide().is_ok()
                {
                    api.prevent_close();
                }
            }
        })
        .setup(|app| {
            #[cfg(windows)]
            if updates_enabled(app.handle().clone()) {
                app.handle()
                    .plugin(tauri_plugin_updater::Builder::new().build())?;
            }
            let data = app.path().app_local_data_dir()?;
            let store = Arc::new(open_store(&data)?);
            store.recover().map_err(io::Error::other)?;
            app.manage(Arc::clone(&store));
            if let Err(error) = install_tray(app.handle()) {
                eprintln!("System tray unavailable: {error}");
            }
            let app_handle = app.handle().clone();
            std::thread::Builder::new()
                .name("downloads".into())
                .spawn(move || loop {
                    let notifications = app_handle.clone();
                    let settings = Arc::clone(&store);
                    match store.start_next(move |job| {
                        if settings
                            .current_settings()
                            .is_ok_and(|current| current.notify_when_finished)
                        {
                            let _ = notifications
                                .notification()
                                .builder()
                                .title("Download complete")
                                .body(job.filename)
                                .show();
                        }
                    }) {
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
            add_download_batch,
            list_network_interfaces,
            save_download_queue,
            set_download_queue_running,
            move_download_to_queue,
            replace_download_link,
            retry_download,
            open_download,
            remove_completed_download,
            pause_download,
            stop_recording,
            connect_browser,
            schedule_queue,
            set_priority,
            updates_enabled,
            #[cfg(windows)]
            check_update,
            #[cfg(windows)]
            install_update
        ])
        .run(tauri::generate_context!())
        .expect("Outernet could not start");
}
