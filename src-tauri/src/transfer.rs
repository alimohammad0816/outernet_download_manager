use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};
use tempfile::NamedTempFile;
use url::Url;

pub fn parse_url(input: &str) -> Result<Url, String> {
    let mut url = Url::parse(input.trim()).map_err(|_| "Enter a valid HTTP or HTTPS URL.")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Enter an HTTP or HTTPS URL without embedded credentials.".into());
    }
    url.set_fragment(None);
    Ok(url)
}

pub fn valid_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    !name.is_empty()
        && name.len() <= 240
        && !name.ends_with([' ', '.'])
        && !name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        && !matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        && !["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        })
}

pub fn download(url: &str, directory: &Path, paused: &AtomicBool) -> Result<(), String> {
    check_paused(paused)?;
    fs::create_dir(directory)
        .map_err(|error| format!("Cannot create the working folder: {error}"))?;
    let mut command = Command::new("aria2c");
    command.args([
        "--no-conf",
        "--input-file=-",
        "--file-allocation=none",
        "--auto-file-renaming=false",
        "--allow-overwrite=false",
        "--continue=false",
        "--always-resume=true",
        "--split=1",
        "--max-connection-per-server=1",
        "--max-tries=1",
        "--follow-torrent=false",
        "--follow-metalink=false",
        "--http-accept-gzip=false",
        "--check-certificate=true",
        "--connect-timeout=15",
        "--timeout=30",
        &format!("--stop-with-process={}", std::process::id()),
    ]);
    command.arg("--dir").arg(directory);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!("Cannot start aria2c. Install aria2 and make it available on PATH. ({error})")
        })?;
    let send = writeln!(
        child.stdin.take().expect("piped stdin"),
        "{url}\n  out=payload"
    );
    if let Err(error) = send {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("Cannot send the URL to aria2: {error}"));
    }
    let status = wait_for_child(&mut child, paused)
        .map_err(|error| format!("Cannot wait for aria2: {error}"))?;
    let file = directory.join("payload");
    if !status.success() || !file.is_file() || directory.join("payload.aria2").exists() {
        let detail = match status.code() {
            Some(8) => "the server rejected the byte range",
            Some(9) => "the working disk is full",
            Some(13 | 14 | 15 | 16 | 17 | 18) => "aria2 could not write the download",
            Some(3) => "the resource was unavailable",
            _ => "the transfer did not finish",
        };
        return Err(format!("Download failed: {detail} ({status})."));
    }
    Ok(())
}

pub fn sha256(path: &Path) -> io::Result<String> {
    let mut input = File::open(path)?;
    let mut hash = Sha256::new();
    let mut block = vec![0; 128 * 1024];
    loop {
        let size = input.read(&mut block)?;
        if size == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&block[..size]);
    }
}

pub fn finalize(source: &Path, destination: &Path, digest: &str) -> io::Result<()> {
    if sha256(source)? != digest {
        return Err(io::Error::other(
            "The working file no longer matches its SHA-256.",
        ));
    }
    if destination.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "A file with this name already exists in the destination.",
        ));
    }
    // A hard link publishes the verified file instantly on the same filesystem.
    if fs::hard_link(source, destination).is_err() {
        let parent = destination.parent().expect("destination has a directory");
        let mut staging = NamedTempFile::with_prefix_in(".outernet-copy-", parent)?;
        io::copy(&mut File::open(source)?, staging.as_file_mut())?;
        staging.as_file().sync_all()?;
        if sha256(staging.path())? != digest {
            return Err(io::Error::other(
                "The destination copy failed SHA-256 verification.",
            ));
        }
        staging.persist_noclobber(destination)?;
    }
    #[cfg(unix)]
    File::open(destination.parent().expect("destination has a directory"))?.sync_all()?;
    Ok(())
}

pub fn normalize_folder(path: PathBuf) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("Choose an absolute folder path.".into());
    }
    let folder = path
        .canonicalize()
        .map_err(|error| format!("Cannot access the folder: {error}"))?;
    if !folder.is_dir() {
        return Err("Choose an existing folder.".into());
    }
    NamedTempFile::with_prefix_in(".outernet-check-", &folder)
        .map_err(|error| format!("Cannot write to the folder: {error}"))?;
    // Preserve the user's normal path spelling, including ordinary Windows paths for aria2.
    Ok(path)
}

pub fn http_client(follow_redirects: bool) -> Result<reqwest::blocking::Client, String> {
    use reqwest::{header, redirect::Policy};
    use std::time::Duration;

    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::ACCEPT_ENCODING,
        header::HeaderValue::from_static("identity"),
    );
    let redirects = if follow_redirects {
        Policy::limited(8)
    } else {
        Policy::none()
    };
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .redirect(redirects)
        .default_headers(headers)
        .user_agent("Outernet Download Manager")
        .build()
        .map_err(|error| format!("Cannot initialize HTTP: {error}"))
}

pub fn reset_working_directory(directory: &Path) -> Result<(), String> {
    match fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Cannot reset the working folder: {error}")),
    }
}

pub fn check_paused(paused: &AtomicBool) -> Result<(), String> {
    if paused.load(Ordering::Relaxed) {
        Err("Download paused.".into())
    } else {
        Ok(())
    }
}

pub fn wait_for_child(child: &mut Child, paused: &AtomicBool) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if paused.load(Ordering::Relaxed) {
            let _ = child.kill();
            return child.wait();
        }
        thread::sleep(Duration::from_millis(100));
    }
}
