#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod media;
mod network;
mod progress;
mod proxy;
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
            proxy: Default::default(),
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
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tauri::include_image!("icons/tray.png"))
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
        })
        .build(app)?;
    Ok(())
}

const RELEASES_PAGE: &str =
    "https://github.com/alimohammad0816/outernet_download_manager/releases/latest";
const RELEASES_API: &str =
    "https://api.github.com/repos/alimohammad0816/outernet_download_manager/releases/latest";

fn signed_updates_enabled(app: &AppHandle) -> bool {
    use tauri::utils::{config::BundleType, platform::bundle_type};

    matches!(bundle_type(), Some(BundleType::Deb | BundleType::Nsis))
        && app.config().plugins.0.contains_key("updater")
}

#[derive(serde::Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(serde::Deserialize)]
struct ReleaseAsset {
    name: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AvailableUpdate {
    version: String,
    signed_install: bool,
}

impl GithubRelease {
    fn newer_version(&self, current: &semver::Version) -> Option<semver::Version> {
        if self.draft || self.prerelease {
            return None;
        }
        let version = self
            .tag_name
            .strip_prefix('v')?
            .parse::<semver::Version>()
            .ok()?;
        (version.pre.is_empty() && version > *current).then_some(version)
    }
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<AvailableUpdate>, String> {
    let mut response = reqwest::Client::builder()
        .user_agent("Outernet Download Manager")
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|_| "Cannot initialize the update check.")?
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| "Could not check GitHub for updates. Try again later.")?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Could not read update metadata.")?
    {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err("GitHub returned oversized update metadata.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let release: GithubRelease =
        serde_json::from_slice(&bytes).map_err(|_| "GitHub returned invalid update metadata.")?;
    Ok(release
        .newer_version(&app.package_info().version)
        .map(|version| AvailableUpdate {
            version: version.to_string(),
            signed_install: signed_updates_enabled(&app)
                && release
                    .assets
                    .iter()
                    .any(|asset| asset.name == "latest.json"),
        }))
}

#[tauri::command]
fn open_release_page() -> Result<(), String> {
    tauri_plugin_opener::open_url(RELEASES_PAGE, None::<&str>).map_err(|error| error.to_string())
}

#[tauri::command]
async fn install_update(
    app: AppHandle,
    store: State<'_, Arc<Store>>,
    version: String,
) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    if !signed_updates_enabled(&app) {
        return Err("Signed updates are unavailable for this build.".into());
    }
    store.while_workers_idle(|| Ok(()))?;
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
    let store = Arc::clone(store.inner());
    tauri::async_runtime::spawn_blocking(move || {
        store.while_workers_idle(|| {
            update.install(bytes).map_err(|error| error.to_string())?;
            #[cfg(target_os = "linux")]
            app.restart();
            #[cfg(not(target_os = "linux"))]
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())?
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
            if signed_updates_enabled(app.handle()) {
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
            check_update,
            open_release_page,
            install_update
        ])
        .run(tauri::generate_context!())
        .expect("Outernet could not start");
}

#[cfg(test)]
mod update_tests {
    use super::GithubRelease;

    #[test]
    fn only_newer_stable_published_releases_are_announced() {
        let current = "1.9.0".parse().unwrap();
        for (tag_name, draft, prerelease, expected) in [
            ("v1.10.0", false, false, true),
            ("v1.9.0", false, false, false),
            ("v1.8.99", false, false, false),
            ("v2.0.0", true, false, false),
            ("v2.0.0", false, true, false),
            ("v2.0.0-beta.1", false, false, false),
            ("unknown", false, false, false),
        ] {
            let release = GithubRelease {
                tag_name: tag_name.into(),
                draft,
                prerelease,
                assets: vec![],
            };
            assert_eq!(
                release.newer_version(&current).is_some(),
                expected,
                "{tag_name}"
            );
        }
    }
}
