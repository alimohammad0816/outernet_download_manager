use crate::{media::Format, open_store, transfer, APP_ID};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
};
use tempfile::NamedTempFile;

const HOST: &str = "io.outernet.downloadmanager";
const EXTENSION_ID: &str = include_str!("../browser-extension-id");
const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    Ping,
    Download {
        url: String,
        filename: Option<String>,
        format: Option<Format>,
        max_height: Option<u32>,
    },
}

fn allowed_origin() -> String {
    format!("chrome-extension://{}/", EXTENSION_ID.trim())
}

pub fn run_host(origin: &str) -> io::Result<()> {
    if origin != allowed_origin() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unknown extension origin.",
        ));
    }
    let request = read_request(io::stdin().lock());
    let outcome = request.and_then(handle_request);
    let success = outcome.as_ref().is_ok_and(|launch| *launch);
    let message = match outcome {
        Ok(_) => json!({ "ok": true }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    write_response(io::stdout().lock(), &message)?;
    if success {
        Command::new(std::env::current_exe()?)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
    }
    Ok(())
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

fn handle_request(request: Request) -> Result<bool, String> {
    let Request::Download {
        url,
        filename,
        format,
        max_height,
    } = request
    else {
        return Ok(false);
    };
    let parsed = transfer::parse_url(&url)?;
    let leaf = parsed
        .path_segments()
        .and_then(|mut parts| parts.next_back())
        .unwrap_or_default();
    let filename = filename
        .filter(|name| transfer::valid_name(name))
        .or_else(|| {
            let decoded = percent_decode_str(leaf).decode_utf8().ok()?;
            transfer::valid_name(&decoded).then(|| decoded.into_owned())
        })
        .unwrap_or_else(|| "download".into());
    let data = dirs::data_local_dir()
        .ok_or("Cannot locate the Outernet data directory.")?
        .join(APP_ID);
    let store = open_store(&data).map_err(|error| error.to_string())?;
    store.add_from_browser(parsed.into(), filename, format, max_height)?;
    Ok(true)
}

pub fn register(browser: &str) -> Result<(), String> {
    let profile = match browser {
        "chrome" => ("google-chrome", "Google\\Chrome"),
        "edge" => ("microsoft-edge", "Microsoft\\Edge"),
        "chromium" => ("chromium", "Chromium"),
        _ => return Err("Choose Chrome, Edge or Chromium.".into()),
    };
    let directory: PathBuf;
    #[cfg(target_os = "linux")]
    {
        directory = dirs::config_dir()
            .ok_or("Cannot locate browser configuration.")?
            .join(profile.0)
            .join("NativeMessagingHosts");
    }
    #[cfg(target_os = "windows")]
    {
        directory = dirs::data_local_dir()
            .ok_or("Cannot locate Outernet configuration.")?
            .join(APP_ID)
            .join("browser");
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = profile;
        return Err("Browser connection is available on Linux and Windows.".into());
    }
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Cannot create browser configuration: {error}"))?;
    let path = directory.join(format!("{HOST}.json"));
    let manifest = json!({
        "name": HOST,
        "description": "Outernet Download Manager",
        "path": std::env::current_exe().map_err(|error| format!("Cannot locate Outernet: {error}"))?,
        "type": "stdio",
        "allowed_origins": [allowed_origin()],
    });
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
}
