use crate::progress::TransferControl;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ExitStatus},
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

pub fn download(
    url: &str,
    directory: &Path,
    has_expected_hash: bool,
    control: &TransferControl,
) -> Result<(), String> {
    use reqwest::{header, StatusCode, Version};
    control.check()?;
    fs::create_dir(directory)
        .map_err(|error| format!("Cannot create the working folder: {error}"))?;
    let client = http_client(true, control)?;
    let mut response = client
        .get(url)
        .send()
        .map_err(|error| format!("Cannot start the file transfer: {}", error.without_url()))?;
    let headers = response.headers();
    if response.status() != StatusCode::OK
        || !unencoded(headers)
        || headers.contains_key(header::CONTENT_RANGE)
    {
        return Err(format!(
            "The source did not return a complete file response ({}).",
            response.status()
        ));
    }
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let chunked = headers
        .get(header::TRANSFER_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("chunked"));
    if length.is_none()
        && !chunked
        && !matches!(response.version(), Version::HTTP_2 | Version::HTTP_3)
        && !has_expected_hash
    {
        return Err("The server did not provide a verifiable response length or framing. A publisher SHA-256 is required for this link.".into());
    }
    control.set_total(length);
    let mut file = File::options()
        .create_new(true)
        .write(true)
        .open(directory.join("payload"))
        .map_err(|error| format!("Cannot open the temporary file: {error}"))?;
    let mut written = 0_u64;
    let mut buffer = vec![0; control.chunk_bytes(128 * 1024)];
    loop {
        control.check()?;
        let read = response
            .read(&mut buffer)
            .map_err(|_| "The file transfer ended before the response was complete.")?;
        if read == 0 {
            break;
        }
        control.account_network(read)?;
        file.write_all(&buffer[..read])
            .map_err(|error| format!("Cannot write the temporary file: {error}"))?;
        written += read as u64;
        control.commit_bytes(read as u64);
    }
    if length.is_some_and(|length| length != written) {
        return Err("The downloaded size differs from the server’s reported length.".into());
    }
    Ok(())
}

pub fn strong_etag(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    let etag = headers.get(reqwest::header::ETAG)?.to_str().ok()?;
    let tag = etag.strip_prefix('"')?.strip_suffix('"')?;
    tag.bytes()
        .all(|byte| byte == b'!' || (0x23..=0x7e).contains(&byte))
        .then_some(etag)
}

pub fn unencoded(headers: &reqwest::header::HeaderMap) -> bool {
    let mut encodings = headers.get_all(reqwest::header::CONTENT_ENCODING).iter();
    encodings
        .next()
        .is_none_or(|value| value.as_bytes().eq_ignore_ascii_case(b"identity"))
        && encodings.next().is_none()
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
    Ok(path)
}

pub fn http_client(
    follow_redirects: bool,
    control: &TransferControl,
) -> Result<reqwest::blocking::Client, String> {
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
    let client = reqwest::blocking::Client::builder();
    let client = if let Some(binding) = control.binding() {
        let client = client.no_proxy().local_address(binding.address);
        #[cfg(target_os = "linux")]
        let client = client.interface(&binding.name);
        client
    } else {
        client
    };
    client
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

pub fn wait_for_child(child: &mut Child, control: &TransferControl) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if control.is_paused() {
            let _ = child.kill();
            return child.wait();
        }
        thread::sleep(Duration::from_millis(100));
    }
}
