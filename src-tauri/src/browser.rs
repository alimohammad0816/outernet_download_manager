use crate::{media::Format, open_store, progress::TransferControl, store::Store, transfer, APP_ID};
use base64::{engine::general_purpose::STANDARD, Engine};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::NamedTempFile;

const HOST: &str = "io.outernet.downloadmanager";
const EXTENSION_ID: &str = include_str!("../browser-extension-id");
const FIREFOX_ID: &str = include_str!("../firefox-extension-id");
const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    Ping,
    Receive {
        url: String,
        filename: Option<String>,
        length: u64,
        #[serde(default)]
        interactive: bool,
    },
    Chunk {
        bytes: String,
    },
    Finish,
    Abort,
    Download {
        url: String,
        filename: Option<String>,
        format: Option<Format>,
        max_height: Option<u32>,
        #[serde(default)]
        headers: crate::media::Headers,
        #[serde(default)]
        interactive: bool,
    },
}

fn allowed_origins() -> Vec<String> {
    let mut origins = vec![format!("chrome-extension://{}/", EXTENSION_ID.trim())];
    for id in [
        option_env!("OUTERNET_CHROME_EXTENSION_ID"),
        option_env!("OUTERNET_EDGE_EXTENSION_ID"),
    ]
    .into_iter()
    .flatten()
    .filter(|id| id.len() == 32 && id.bytes().all(|byte| (b'a'..=b'p').contains(&byte)))
    {
        let origin = format!("chrome-extension://{id}/");
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    origins
}

pub fn launch_origin(mut args: impl Iterator<Item = OsString>) -> Option<String> {
    let first = args.next()?;
    if first.to_string_lossy().starts_with("chrome-extension://") {
        return Some(first.to_string_lossy().into_owned());
    }
    let manifest = Path::new(&first);
    if manifest.is_absolute()
        && manifest
            .file_name()
            .is_some_and(|file| file == format!("{HOST}.json").as_str())
    {
        return Some(
            args.next()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        );
    }
    None
}

pub fn run_host(origin: &str) -> io::Result<()> {
    if !allowed_origins().iter().any(|allowed| allowed == origin) && origin != FIREFOX_ID.trim() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unknown extension origin.",
        ));
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let outcome = match read_request(&mut input) {
        Ok(Request::Receive {
            url,
            filename,
            length,
            interactive,
        }) => browser_store()
            .and_then(|store| {
                receive(
                    &store,
                    &url,
                    filename,
                    length,
                    interactive,
                    &mut input,
                    &mut output,
                )
            })
            .map(|id| {
                if interactive {
                    Launch::Popup(id)
                } else {
                    Launch::Main
                }
            }),
        Ok(request) => handle_request(request),
        Err(error) => Err(error),
    };
    let message = match outcome {
        Ok(launch) => {
            let popup = matches!(launch, Launch::Popup(_));
            let result = launch.start();
            match result {
                Ok(()) => json!({ "ok": true, "popup": popup }),
                Err(error) if popup => {
                    json!({ "ok": false, "error": format!("Cannot open Outernet: {error}") })
                }
                Err(_) => json!({ "ok": true, "popup": false }),
            }
        }
        Err(error) => json!({ "ok": false, "error": error }),
    };
    write_response(&mut output, &message)
}

enum Launch {
    None,
    Main,
    Popup(String),
}

impl Launch {
    fn start(self) -> io::Result<()> {
        let args = match &self {
            Self::None => return Ok(()),
            Self::Main => vec![],
            Self::Popup(id) => vec!["--download-request", id],
        };
        Command::new(std::env::current_exe()?)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
    }
}

fn browser_store() -> Result<Store, String> {
    let data = dirs::data_local_dir()
        .ok_or("Cannot locate the Outernet data directory.")?
        .join(APP_ID);
    open_store(&data).map_err(|error| error.to_string())
}

fn receive(
    store: &Store,
    url: &str,
    filename: Option<String>,
    length: u64,
    interactive: bool,
    mut input: impl Read,
    mut output: impl Write,
) -> Result<String, String> {
    let mut parsed = transfer::parse_url(url)?;
    let filename = download_name(&parsed, filename);
    // The received bytes, rather than a credential-bearing URL, are the retry source.
    parsed.set_query(None);
    if length > 9_007_199_254_740_991 {
        return Err("This response length is outside the supported range.".into());
    }
    let settings = store.current_settings()?;
    let control = TransferControl::new(settings.speed_limit_kib);
    let chunk_bytes = control.chunk_bytes(32 * 1024);
    let temporary = tempfile::Builder::new()
        .prefix(".outernet-")
        .tempdir_in(&settings.working_folder)
        .map_err(receive_error)?;
    let attempt = temporary
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(".outernet-"))
        .ok_or("Cannot identify the working folder.")?
        .to_string();
    let mut payload = File::create(temporary.path().join("payload")).map_err(receive_error)?;
    write_response(
        &mut output,
        &json!({ "ok": true, "chunk_bytes": chunk_bytes }),
    )
    .map_err(receive_error)?;
    let mut written = 0;
    let mut hash = Sha256::new();
    loop {
        match read_request(&mut input)? {
            Request::Chunk { bytes } => {
                let bytes = STANDARD
                    .decode(bytes)
                    .map_err(|_| "Invalid browser data chunk.")?;
                if bytes.len() > chunk_bytes && bytes.len() <= 32 * 1024 {
                    return Err(
                        "Update the Outernet browser extension to use the configured speed limit."
                            .into(),
                    );
                }
                if bytes.is_empty()
                    || bytes.len() > chunk_bytes
                    || bytes.len() as u64 > length - written
                {
                    return Err("The browser sent more bytes than expected.".into());
                }
                control.account_network(bytes.len())?;
                payload.write_all(&bytes).map_err(receive_error)?;
                hash.update(&bytes);
                written += bytes.len() as u64;
                write_response(&mut output, &json!({ "ok": true })).map_err(receive_error)?;
            }
            Request::Abort => return Err("The browser ended the transfer.".into()),
            Request::Finish if written == length => break,
            Request::Finish => {
                return Err("The browser response ended before the declared length.".into())
            }
            _ => return Err("Unexpected browser transfer message.".into()),
        }
    }
    payload.sync_all().map_err(receive_error)?;
    drop(payload);
    #[cfg(unix)]
    {
        File::open(temporary.path())
            .and_then(|directory| directory.sync_all())
            .map_err(receive_error)?;
        File::open(&settings.working_folder)
            .and_then(|directory| directory.sync_all())
            .map_err(receive_error)?;
    }
    let id = store.complete_browser_transfer(
        parsed.into(),
        filename,
        settings,
        attempt,
        written,
        format!("{:x}", hash.finalize()),
        interactive,
    )?;
    let _ = temporary.keep();
    Ok(id)
}

fn receive_error(error: io::Error) -> String {
    format!("Cannot save the browser transfer: {error}")
}

fn read_request(mut input: impl Read) -> Result<Request, String> {
    let mut size = [0; 4];
    input
        .read_exact(&mut size)
        .map_err(|_| "Incomplete browser message.")?;
    let size = u32::from_ne_bytes(size) as usize;
    if size == 0 || size > MAX_MESSAGE {
        return Err("Browser message size is outside the supported range.".into());
    }
    let mut body = vec![0; size];
    input
        .read_exact(&mut body)
        .map_err(|_| "Incomplete browser message.")?;
    serde_json::from_slice(&body).map_err(|_| "Invalid browser message.".into())
}

fn write_response(mut output: impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    output.write_all(&(body.len() as u32).to_ne_bytes())?;
    output.write_all(&body)?;
    output.flush()
}

fn handle_request(request: Request) -> Result<Launch, String> {
    match request {
        Request::Ping => Ok(Launch::None),
        Request::Download {
            url,
            filename,
            format,
            max_height,
            headers,
            interactive,
        } => {
            let parsed = transfer::parse_url(&url)?;
            let filename = download_name(&parsed, filename);
            let headers = crate::media::clean_headers(headers);
            let store = browser_store()?;
            if interactive {
                store
                    .prepare_download(parsed.into(), filename, format, max_height, headers)
                    .map(Launch::Popup)
            } else {
                store.add_from_browser(parsed.into(), filename, format, max_height, headers)?;
                Ok(Launch::Main)
            }
        }
        _ => Err("Start a browser transfer before sending data.".into()),
    }
}

fn download_name(url: &url::Url, filename: Option<String>) -> String {
    let leaf = url
        .path_segments()
        .and_then(|mut parts| parts.next_back())
        .unwrap_or_default();
    filename
        .filter(|name| transfer::valid_name(name))
        .or_else(|| {
            let decoded = percent_decode_str(leaf).decode_utf8().ok()?;
            transfer::valid_name(&decoded).then(|| decoded.into_owned())
        })
        .unwrap_or_else(|| "download".into())
}

pub fn register(browser: &str) -> Result<(), String> {
    let profile = match browser {
        "chrome" => ("google-chrome", "Google\\Chrome"),
        "edge" => ("microsoft-edge", "Microsoft\\Edge"),
        "chromium" => ("chromium", "Chromium"),
        "firefox" => (".mozilla", "Mozilla"),
        _ => return Err("Choose Chrome, Edge, Chromium or Firefox.".into()),
    };
    let directory: PathBuf;
    #[cfg(target_os = "linux")]
    {
        directory = if browser == "firefox" {
            dirs::home_dir()
                .ok_or("Cannot locate Firefox configuration.")?
                .join(profile.0)
                .join("native-messaging-hosts")
        } else {
            dirs::config_dir()
                .ok_or("Cannot locate browser configuration.")?
                .join(profile.0)
                .join("NativeMessagingHosts")
        };
    }
    #[cfg(target_os = "windows")]
    {
        directory = dirs::data_local_dir()
            .ok_or("Cannot locate Outernet configuration.")?
            .join(APP_ID)
            .join("browser")
            .join(browser);
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = profile;
        return Err("Browser connection is available on Linux and Windows.".into());
    }
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Cannot create browser configuration: {error}"))?;
    let path = directory.join(format!("{HOST}.json"));
    let mut manifest = json!({
        "name": HOST,
        "description": "Outernet Download Manager",
        "path": std::env::current_exe().map_err(|error| format!("Cannot locate Outernet: {error}"))?,
        "type": "stdio",

    });
    if browser == "firefox" {
        manifest["allowed_extensions"] = json!([FIREFOX_ID.trim()]);
    } else {
        manifest["allowed_origins"] = json!(allowed_origins());
    }
    let mut staged = NamedTempFile::new_in(&directory).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(staged.as_file_mut(), &manifest)
        .map_err(|error| error.to_string())?;
    staged
        .persist(&path)
        .map_err(|error| format!("Cannot save browser configuration: {error}"))?;
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let system =
            std::env::var_os("SystemRoot").ok_or("Cannot locate the Windows registry tool.")?;
        let key = format!(
            "HKCU\\Software\\{}\\NativeMessagingHosts\\{HOST}",
            profile.1
        );
        let status = Command::new(PathBuf::from(system).join("System32/reg.exe"))
            .creation_flags(0x08000000)
            .args(["add", &key, "/ve", "/t", "REG_SZ", "/d"])
            .arg(path)
            .arg("/f")
            .status()
            .map_err(|error| format!("Cannot register the browser host: {error}"))?;
        if !status.success() {
            return Err("Windows rejected the browser host registry entry.".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_ping_and_rejects_unbounded_input() {
        let mut framed = Vec::new();
        write_response(&mut framed, &json!({"action": "ping"})).unwrap();
        assert!(matches!(
            read_request(framed.as_slice()).unwrap(),
            Request::Ping
        ));
        assert!(read_request(u32::MAX.to_ne_bytes().as_slice()).is_err());
        assert!(read_request([4, 0, 0, 0, b'{'].as_slice()).is_err());
    }

    #[test]
    fn firefox_origin_uses_the_manifest_path_and_extension_id() {
        let manifest = std::env::current_dir()
            .unwrap()
            .join(format!("{HOST}.json"));
        let args = [manifest.into_os_string(), FIREFOX_ID.trim().into()];
        assert_eq!(
            launch_origin(args.into_iter()).as_deref(),
            Some(FIREFOX_ID.trim())
        );
        assert!(launch_origin([OsString::from("other.json")].into_iter()).is_none());
    }

    #[test]
    fn browser_response_is_queued_only_after_all_bytes_are_received() {
        let root = tempfile::tempdir().unwrap();
        let settings = crate::store::Settings {
            working_folder: root.path().join("work"),
            destination_folder: root.path().join("destination"),
            speed_limit_kib: None,
            proxy: Default::default(),
            download_slots: 1,
            keep_running: false,
            notify_when_finished: false,
        };
        fs::create_dir(&settings.working_folder).unwrap();
        fs::create_dir(&settings.destination_folder).unwrap();
        let store = Store::open(&root.path().join("queue.sqlite"), settings.clone()).unwrap();
        let input = |bytes: &[u8]| {
            let mut messages = Vec::new();
            write_response(
                &mut messages,
                &json!({ "action": "chunk", "bytes": STANDARD.encode(bytes) }),
            )
            .unwrap();
            write_response(&mut messages, &json!({ "action": "finish" })).unwrap();
            messages
        };
        let url = "https://example.com/session.bin?private=token";
        let mut output = Vec::new();
        assert!(receive(
            &store,
            url,
            None,
            5,
            false,
            input(b"cat").as_slice(),
            &mut output
        )
        .is_err());
        assert!(store.snapshot().unwrap().jobs.is_empty());
        assert!(fs::read_dir(&settings.working_folder)
            .unwrap()
            .next()
            .is_none());

        receive(
            &store,
            url,
            None,
            5,
            false,
            input(b"bytes").as_slice(),
            &mut output,
        )
        .unwrap();
        let job = store.snapshot().unwrap().jobs.remove(0);
        assert!(!job.url.contains("private"));
        assert_eq!(job.bytes, 5);
        assert!(store.process_next().unwrap());
        assert_eq!(
            fs::read(settings.destination_folder.join("session.bin")).unwrap(),
            b"bytes"
        );

        let staged_id = receive(
            &store,
            url,
            None,
            5,
            true,
            input(b"staged").as_slice(),
            &mut output,
        );
        assert!(staged_id.is_err());
        let staged_id = receive(
            &store,
            url,
            None,
            6,
            true,
            input(b"staged").as_slice(),
            &mut output,
        )
        .unwrap();
        assert!(!store.process_next().unwrap());
        store
            .submit_download(
                Some(&staged_id),
                String::new(),
                "session-later.bin".into(),
                String::new(),
                crate::store::DEFAULT_QUEUE,
                settings.destination_folder.clone(),
                false,
            )
            .unwrap();
        assert!(!store.process_next().unwrap());
        store
            .set_queue_running(crate::store::DEFAULT_QUEUE, true)
            .unwrap();
        assert!(store.process_next().unwrap());
        assert_eq!(
            fs::read(settings.destination_folder.join("session-later.bin")).unwrap(),
            b"staged"
        );
    }
}
