use crate::{progress::TransferControl, transfer};
use reqwest::{
    blocking::{Client, Response},
    header, StatusCode,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    thread,
};
use url::Url;

const CHUNK_BYTES: u64 = 4 * 1024 * 1024;
const MAX_WORKERS: usize = 4;
const CHECKPOINT: &str = "ranges.json";

#[derive(Serialize, Deserialize, PartialEq)]
struct Identity {
    url: Url,
    etag: String,
    length: u64,
}

#[derive(Debug)]
enum PartError {
    Changed,
    Interrupted,
    Write(io::Error),
}

pub fn has_checkpoint(directory: &Path) -> bool {
    directory.join(CHECKPOINT).is_file()
}

pub fn completed_bytes(directory: &Path) -> Option<u64> {
    let identity: Identity =
        serde_json::from_reader(File::open(directory.join(CHECKPOINT)).ok()?).ok()?;
    let mut bytes = 0;
    for entry in fs::read_dir(directory).ok()?.flatten() {
        let name = entry.file_name();
        let Some(index) = parse_part_name(&name.to_string_lossy()) else {
            continue;
        };
        if index < identity.length.div_ceil(CHUNK_BYTES) {
            bytes += bounds(index, identity.length).1;
        }
    }
    Some(bytes)
}

fn parse_part_name(name: &str) -> Option<u64> {
    name.strip_prefix("part-")?
        .strip_suffix(".sha256")?
        .parse()
        .ok()
}

pub fn try_download(
    url: &str,
    directory: &Path,
    has_expected_hash: bool,
    control: &TransferControl,
) -> Result<bool, String> {
    control.check()?;
    let client = transfer::http_client(true)?;
    let response = match client
        .head(url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
    {
        Ok(response) => response,
        Err(_) if !has_checkpoint(directory) => return Ok(false),
        Err(_) => return Err("Cannot check the remote file. Completed parts were kept.".into()),
    };
    let Some(identity) = identity(&response) else {
        if source_unavailable(response.status()) && has_checkpoint(directory) {
            return Err(
                "The source is unavailable. Completed parts were kept. Retry or replace the link."
                    .into(),
            );
        }
        return Ok(false);
    };
    let client = transfer::http_client(false)?;
    let payload = directory.join("payload");
    let checkpoint = directory.join(CHECKPOINT);
    let previous = File::open(&checkpoint)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, Identity>(file).ok());
    let same_version = previous.as_ref().is_some_and(|previous| {
        previous == &identity
            || (has_expected_hash
                && previous.length == identity.length
                && previous.etag == identity.etag)
    }) && fs::metadata(&payload).map(|file| file.len()).ok()
        == Some(identity.length);
    if !same_version {
        transfer::reset_working_directory(directory)?;
        fs::create_dir(directory).map_err(write_error)?;
        let file = File::create(&payload).map_err(write_error)?;
        file.set_len(identity.length).map_err(write_error)?;
        file.sync_all().map_err(write_error)?;
    }
    if !same_version || previous.as_ref() != Some(&identity) {
        let bytes = serde_json::to_vec(&identity).map_err(|error| error.to_string())?;
        save_marker(directory, &checkpoint, &bytes).map_err(write_error)?;
    }
    let payload = Mutex::new(
        File::options()
            .read(true)
            .write(true)
            .open(&payload)
            .map_err(write_error)?,
    );
    control.set_total(Some(identity.length));
    let count = identity.length.div_ceil(CHUNK_BYTES);
    let next = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let failure = Mutex::new(None::<PartError>);

    let mut workers_failed = false;
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..MAX_WORKERS.min(usize::try_from(count).unwrap_or(MAX_WORKERS)) {
            handles.push(scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) && !control.is_paused() {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    if let Err(error) = fetch_part(
                        &client, directory, &identity, &payload, index, control, &stop,
                    ) {
                        stop.store(true, Ordering::Relaxed);
                        let mut failure = failure.lock().expect("part error lock");
                        // A version mismatch invalidates all saved pieces even if another request failed first.
                        if failure.is_none() || matches!(error, PartError::Changed) {
                            *failure = Some(error);
                        }
                    }
                }
            }));
        }
        for worker in handles {
            workers_failed |= worker.join().is_err();
        }
    });
    control.check()?;
    if workers_failed {
        return Err(
            "A download worker stopped unexpectedly. Retry will recheck completed parts.".into(),
        );
    }
    match failure.into_inner().expect("part error lock") {
        None => Ok(true),
        Some(PartError::Changed) => Ok(false),
        Some(PartError::Interrupted) => {
            Err("A byte-range request was interrupted. Retry can reuse verified parts.".into())
        }
        Some(PartError::Write(error)) => Err(write_error(error)),
    }
}

fn source_unavailable(status: StatusCode) -> bool {
    status.is_server_error() || matches!(status.as_u16(), 401 | 403 | 404 | 410 | 429)
}

fn identity(response: &Response) -> Option<Identity> {
    if response.status() != StatusCode::OK || !transfer::unencoded(response.headers()) {
        return None;
    }
    let etag = transfer::strong_etag(response.headers())?.to_string();
    let length = response
        .headers()
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    if length < CHUNK_BYTES * 2
        || transfer::parse_url(response.url().as_str()).is_err()
        || !response
            .headers()
            .get(header::ACCEPT_RANGES)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"bytes"))
    {
        return None;
    }
    Some(Identity {
        url: response.url().clone(),
        etag,
        length,
    })
}

fn bounds(index: u64, length: u64) -> (u64, u64) {
    let start = index * CHUNK_BYTES;
    (start, CHUNK_BYTES.min(length - start))
}

// Bytes in a range are visible as they arrive, while the checkpoint is written only after validation.
struct RangeProgress<'a> {
    control: &'a TransferControl,
    pending: u64,
}

impl Drop for RangeProgress<'_> {
    fn drop(&mut self) {
        self.control.discard_bytes(self.pending);
    }
}

fn fetch_part(
    client: &Client,
    directory: &Path,
    identity: &Identity,
    payload: &Mutex<File>,
    index: u64,
    control: &TransferControl,
    stop: &AtomicBool,
) -> Result<(), PartError> {
    let (start, size) = bounds(index, identity.length);
    let marker = directory.join(format!("part-{index}.sha256"));
    if let Ok(recorded) = fs::read_to_string(&marker) {
        let mut file = payload.lock().expect("working file lock");
        file.seek(SeekFrom::Start(start))
            .map_err(PartError::Write)?;
        if recorded == hash_part(&mut file, size).map_err(PartError::Write)? {
            control.commit_bytes(size);
            return Ok(());
        }
        fs::remove_file(&marker).map_err(PartError::Write)?;
    }

    let end = start + size - 1;
    let mut response = client
        .get(identity.url.clone())
        .header(header::RANGE, format!("bytes={start}-{end}"))
        .header(header::IF_RANGE, &identity.etag)
        .send()
        .map_err(|_| PartError::Interrupted)?;
    if source_unavailable(response.status()) {
        return Err(PartError::Interrupted);
    }
    let range = format!("bytes {start}-{end}/{}", identity.length);
    let headers = response.headers();
    if response.status() != StatusCode::PARTIAL_CONTENT
        || transfer::strong_etag(headers) != Some(&identity.etag)
        || !transfer::unencoded(headers)
        || headers
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            != Some(range.as_str())
        || headers.get(header::CONTENT_LENGTH).is_some_and(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                != Some(size)
        })
    {
        return Err(PartError::Changed);
    }
    let mut data = Vec::with_capacity(size as usize);
    let mut progress = RangeProgress {
        control,
        pending: 0,
    };
    let mut block = vec![0; control.chunk_bytes(64 * 1024)];
    let mut remaining = size;
    while remaining > 0 {
        if control.is_paused() || stop.load(Ordering::Relaxed) {
            return Err(PartError::Interrupted);
        }
        let read = block.len().min(remaining as usize);
        let received = response
            .read(&mut block[..read])
            .map_err(|_| PartError::Interrupted)?;
        if received == 0 {
            return Err(PartError::Interrupted);
        }
        control
            .account_network_while(received, || !stop.load(Ordering::Relaxed))
            .map_err(|_| PartError::Interrupted)?;
        data.extend_from_slice(&block[..received]);
        progress.pending += received as u64;
        control.commit_bytes(received as u64);
        remaining -= received as u64;
    }
    if response
        .read(&mut block[..1])
        .map_err(|_| PartError::Interrupted)?
        > 0
    {
        return Err(PartError::Changed);
    }
    {
        let mut file = payload.lock().expect("working file lock");
        file.seek(SeekFrom::Start(start))
            .map_err(PartError::Write)?;
        file.write_all(&data).map_err(PartError::Write)?;
        file.sync_data().map_err(PartError::Write)?;
    }
    let digest = format!("{:x}", Sha256::digest(data));
    save_marker(directory, &marker, digest.as_bytes()).map_err(PartError::Write)?;
    progress.pending = 0;
    Ok(())
}

fn hash_part(payload: &mut File, size: u64) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut reader = payload.take(size);
    if io::copy(&mut reader, &mut hash)? != size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "A saved byte range is incomplete.",
        ));
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn save_marker(directory: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut marker = tempfile::NamedTempFile::new_in(directory)?;
    marker.write_all(bytes)?;
    marker.as_file().sync_all()?;
    marker.persist(path)?;
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn write_error(error: io::Error) -> String {
    format!("Cannot save download data: {error}")
}
