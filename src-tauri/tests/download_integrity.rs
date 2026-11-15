#[path = "../src/media.rs"]
mod media;
#[path = "../src/ranges.rs"]
mod ranges;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/transfer.rs"]
mod transfer;

use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
use store::{Job, Settings, Status, Store};
use tempfile::TempDir;

enum Response {
    Complete(u64, u8),
    Truncated,
    TruncatedChunked,
    CompleteChunked,
    CloseDelimited,
    UnsolicitedPartial,
}

fn serve(response: Response) -> (String, thread::JoinHandle<Option<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let start = std::time::Instant::now();
        let (mut stream, request) = loop {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if start.elapsed() > Duration::from_secs(15) {
                            return None;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            // Winsock inherits the listener's nonblocking mode on accepted sockets.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_request(&mut stream);
            if request.starts_with("HEAD ") {
                stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                break (stream, request);
            }
        };
        let raw = match response {
            Response::CompleteChunked => Some("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n7\r\nexample\r\n0\r\n\r\n"),
            Response::CloseDelimited => Some("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nexample"),
            Response::TruncatedChunked => Some("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n1000\r\npartial data"),
            Response::UnsolicitedPartial => Some("HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/12\r\nConnection: close\r\n\r\ndata"),
            _ => None,
        };
        if let Some(raw) = raw {
            stream.write_all(raw.as_bytes()).unwrap();
            return Some(request);
        }
        let (length, value) = match response {
            Response::Complete(length, value) => (length, value),
            Response::Truncated => (1024 * 1024, 1),
            _ => unreachable!(),
        };
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut left = if matches!(response, Response::Truncated) {
            1024
        } else {
            length
        };
        let block = vec![value; 128 * 1024];
        while left > 0 {
            let size = left.min(block.len() as u64) as usize;
            stream.write_all(&block[..size]).unwrap();
            left -= size as u64;
        }
        Some(request)
    });
    (format!("http://{address}/fixture.bin"), server)
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
        assert!(request.len() <= 16 * 1024);
    }
    String::from_utf8(request).unwrap()
}

fn expected(length: u64, value: u8) -> String {
    let mut hash = Sha256::new();
    let block = vec![value; 128 * 1024];
    let mut left = length;
    while left != 0 {
        let size = left.min(block.len() as u64) as usize;
        hash.update(&block[..size]);
        left -= size as u64;
    }
    format!("{:x}", hash.finalize())
}

fn setup(destination_root: Option<&Path>) -> (TempDir, TempDir, PathBuf, Settings, Store) {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("working");
    fs::create_dir(&work).unwrap();
    let destination = match destination_root {
        Some(other) => tempfile::tempdir_in(other).unwrap(),
        None => tempfile::tempdir_in(root.path()).unwrap(),
    };
    let database = root.path().join("queue.sqlite");
    let settings = Settings {
        working_folder: work,
        destination_folder: destination.path().to_owned(),
    };
    let store = Store::open(&database, settings.clone()).unwrap();
    (root, destination, database, settings, store)
}

fn job(store: &Store) -> Job {
    store.snapshot().unwrap().jobs[0].clone()
}

#[test]
fn downloads_exact_bytes_after_reopening_the_queue() {
    let (_root, _destination, database, settings, store) = setup(None);
    let (url, server) = serve(Response::Complete(4 * 1024 * 1024, 7));
    let hash = expected(4 * 1024 * 1024, 7);
    store.add(url, "verified.bin".into(), hash.clone()).unwrap();
    let new_work = settings.working_folder.join("new");
    fs::create_dir(&new_work).unwrap();
    store
        .settings(Settings {
            working_folder: new_work.clone(),
            destination_folder: new_work.clone(),
        })
        .unwrap();
    drop(store);
    let store = Store::open(&database, settings.clone()).unwrap();
    store.recover().unwrap();
    assert!(store.process_next().unwrap());
    let request = server.join().unwrap().expect("the engine made the request");
    assert!(!request.to_ascii_lowercase().contains("\r\nrange:"));
    assert!(
        job(&store).status == Status::Complete,
        "{:?}",
        job(&store).error
    );
    assert_eq!(job(&store).bytes, 4 * 1024 * 1024);
    assert_eq!(
        transfer::sha256(&settings.destination_folder.join("verified.bin")).unwrap(),
        hash
    );
    assert!(!new_work.join("verified.bin").exists());
}

#[test]
fn refuses_truncated_response_and_wrong_hash() {
    for (response, hash) in [
        (Response::Truncated, String::new()),
        (Response::TruncatedChunked, String::new()),
        (Response::UnsolicitedPartial, String::new()),
        (Response::Complete(1024 * 1024, 5), expected(1024 * 1024, 9)),
    ] {
        let (_root, _destination, _, settings, store) = setup(None);
        let (url, server) = serve(response);
        store.add(url, "invalid.bin".into(), hash).unwrap();
        store.process_next().unwrap();
        server.join().unwrap().expect("the engine made the request");
        assert!(job(&store).status == Status::Failed);
        assert!(!settings.destination_folder.join("invalid.bin").exists());
    }
}

#[test]
fn verifies_chunked_and_connection_delimited_responses() {
    let hash = format!("{:x}", Sha256::digest(b"example"));
    for (response, reference, complete) in [
        (Response::CompleteChunked, String::new(), true),
        (Response::CloseDelimited, String::new(), false),
        (Response::CloseDelimited, hash.clone(), true),
    ] {
        let (_root, _destination, _, settings, store) = setup(None);
        let (url, server) = serve(response);
        store.add(url, "framing.bin".into(), reference).unwrap();
        store.process_next().unwrap();
        server.join().unwrap().expect("the engine made the request");
        let file = settings.destination_folder.join("framing.bin");
        assert_eq!(
            job(&store).status == Status::Complete,
            complete,
            "{:?}",
            job(&store).error
        );
        if complete {
            assert_eq!(transfer::sha256(&file).unwrap(), hash);
        } else {
            assert!(!file.exists());
        }
    }
}

#[test]
fn tests_completed_download_retry_without_reaccessing_network() {
    let (_root, _destination, database, settings, store) = setup(None);
    let (url, server) = serve(Response::Complete(1024, 4));
    let hash = expected(1024, 4);
    store.add(url, "retry.bin".into(), hash.clone()).unwrap();
    fs::remove_dir(&settings.destination_folder).unwrap();
    store.process_next().unwrap();
    server.join().unwrap().expect("the engine made the request");
    let failed = job(&store);
    assert!(failed.status == Status::Failed);
    assert!(failed.sha256.is_some());
    drop(store);
    let store = Store::open(&database, settings.clone()).unwrap();
    store.recover().unwrap();
    fs::create_dir(&settings.destination_folder).unwrap();
    let destination = settings.destination_folder.join("retry.bin");
    fs::write(&destination, b"existing").unwrap();
    store.retry(failed.id.clone()).unwrap();
    store.process_next().unwrap();
    assert!(job(&store).status == Status::Failed);
    assert_eq!(fs::read(&destination).unwrap(), b"existing");
    fs::remove_file(&destination).unwrap();
    store.retry(failed.id).unwrap();
    store.process_next().unwrap();
    assert!(job(&store).status == Status::Complete);
    assert_eq!(transfer::sha256(&destination).unwrap(), hash);
}

#[test]
#[ignore = "streams more than 4 GiB; run before release"]
fn downloads_over_four_gib() {
    let other_volume = std::env::var_os("OUTERNET_TEST_DESTINATION").map(PathBuf::from);
    let (_root, _destination, _, settings, store) = setup(other_volume.as_deref());
    let length = 4 * 1024 * 1024 * 1024 + 257;
    let hash = expected(length, 19);
    let (url, server) = serve(Response::Complete(length, 19));
    store.add(url, "large.bin".into(), hash.clone()).unwrap();
    store.process_next().unwrap();
    server.join().unwrap().expect("the engine made the request");
    assert!(
        job(&store).status == Status::Complete,
        "{:?}",
        job(&store).error
    );
    assert_eq!(job(&store).bytes, length);
    assert_eq!(
        transfer::sha256(&settings.destination_folder.join("large.bin")).unwrap(),
        hash
    );
}

type Handler = dyn Fn(std::net::TcpStream, String) + Send + Sync;

struct Fixture {
    address: std::net::SocketAddr,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start(handler: impl Fn(std::net::TcpStream, String) + Send + Sync + 'static) -> Self {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let handler: Arc<Handler> = Arc::new(handler);
        let worker = thread::spawn(move || {
            let mut connections = Vec::new();
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let handler = Arc::clone(&handler);
                        connections.retain(|client: &thread::JoinHandle<()>| !client.is_finished());
                        connections.push(thread::spawn(move || {
                            stream.set_nonblocking(false).unwrap();
                            stream
                                .set_read_timeout(Some(Duration::from_secs(5)))
                                .unwrap();
                            stream
                                .set_write_timeout(Some(Duration::from_secs(5)))
                                .unwrap();
                            let request = read_request(&mut stream);
                            handler(stream, request);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(3));
                    }
                    Err(error) => panic!("Listener error: {error}"),
                }
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            address,
            stop,
            worker: Some(worker),
        }
    }

    fn url(&self, file: &str) -> String {
        format!("http://{}/{file}", self.address)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

const PART: u64 = 4 * 1024 * 1024;
const RANGE_LENGTH: u64 = PART * 3;

#[derive(Clone, Copy, PartialEq)]
enum Scenario {
    Interrupted,
    Stable,
    Replaced,
    Slow,
}

struct RangeState {
    mode: Scenario,
    requests: Vec<String>,
}

fn range_fixture(state: std::sync::Arc<std::sync::Mutex<RangeState>>) -> Fixture {
    Fixture::start(move |mut stream, request| {
        let mode = {
            let mut state = state.lock().unwrap();
            state.requests.push(request.clone());
            state.mode
        };
        if request.starts_with("HEAD ") {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nETag: \"original\"\r\nAccept-Ranges: bytes\r\nContent-Length: {RANGE_LENGTH}\r\nConnection: close\r\n\r\n");
            return;
        }
        let lower = request.to_ascii_lowercase();
        let part = lower
            .lines()
            .find_map(|line| line.strip_prefix("range: bytes="))
            .and_then(|range| range.split_once('-'))
            .map(|(start, end)| (start.parse::<u64>().unwrap(), end.parse::<u64>().unwrap()));
        let value = if mode == Scenario::Replaced { 51 } else { 29 };
        if let Some((start, end)) = part.filter(|_| mode != Scenario::Replaced) {
            assert!(lower.contains("\r\nif-range: \"original\"\r\n"));
            let count = end - start + 1;
            let _ = write!(stream, "HTTP/1.1 206 Partial Content\r\nETag: \"original\"\r\nContent-Range: bytes {start}-{end}/{RANGE_LENGTH}\r\nContent-Length: {count}\r\nConnection: close\r\n\r\n");
            let bytes = if mode == Scenario::Interrupted && start == PART {
                256
            } else {
                count
            };
            if mode == Scenario::Slow {
                for _ in 0..(bytes / 65536) {
                    thread::sleep(Duration::from_millis(3));
                    if stream.write_all(&[value; 65536]).is_err() {
                        break;
                    }
                }
            } else {
                write_bytes(&mut stream, bytes, value);
            }
            if mode == Scenario::Interrupted && start == PART {
                thread::sleep(Duration::from_millis(150));
            }
        } else {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nETag: \"replacement\"\r\nContent-Length: {RANGE_LENGTH}\r\nConnection: close\r\n\r\n");
            // Conditional range clients close this response immediately. They only
            // accept 206, so even an equal-length replacement cannot be appended.
            write_bytes(&mut stream, RANGE_LENGTH, value);
        }
    })
}

fn write_bytes(stream: &mut std::net::TcpStream, mut bytes: u64, value: u8) {
    let data = vec![value; 64 * 1024];
    while bytes > 0 {
        let count = bytes.min(data.len() as u64) as usize;
        if stream.write_all(&data[..count]).is_err() {
            return;
        }
        bytes -= count as u64;
    }
}

#[test]
fn reuses_only_verified_ranges_after_restart() {
    let (_root, _destination, database, settings, store) = setup(None);
    let state = std::sync::Arc::new(std::sync::Mutex::new(RangeState {
        mode: Scenario::Interrupted,
        requests: Vec::new(),
    }));
    let fixture = range_fixture(state.clone());
    let hash = expected(RANGE_LENGTH, 29);
    store
        .add(fixture.url("range.bin"), "range.bin".into(), hash.clone())
        .unwrap();
    store.process_next().unwrap();
    let interrupted = job(&store);
    assert!(
        interrupted.status == Status::Failed,
        "{:?}",
        interrupted.error
    );
    let working = fs::read_dir(&settings.working_folder)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(working.join("part-0.sha256").exists());
    let mut payload = fs::OpenOptions::new()
        .write(true)
        .open(working.join("payload"))
        .unwrap();
    // A saved marker is insufficient when its corresponding bytes changed.
    payload.write_all(b"changed on disk").unwrap();
    drop(payload);
    {
        let mut server = state.lock().unwrap();
        server.mode = Scenario::Stable;
        server.requests.clear();
    }
    drop(store);
    let store = Store::open(&database, settings.clone()).unwrap();
    store.recover().unwrap();
    store.retry(interrupted.id).unwrap();
    store.process_next().unwrap();
    assert!(
        job(&store).status == Status::Complete,
        "{:?}",
        job(&store).error
    );
    assert_eq!(
        transfer::sha256(&settings.destination_folder.join("range.bin")).unwrap(),
        hash
    );
    let server = state.lock().unwrap();
    let get: Vec<_> = server
        .requests
        .iter()
        .filter(|request| request.starts_with("GET "))
        .collect();
    assert_eq!(get.len(), 2);
    assert!(get
        .iter()
        .any(|request| request.to_ascii_lowercase().contains("range: bytes=0-")));
    assert!(get.iter().any(|request| request
        .to_ascii_lowercase()
        .contains(&format!("range: bytes={PART}-"))));
}

#[test]
fn pauses_queued_and_active_downloads_then_continues() {
    let (_root, _destination, _, settings, store) = setup(None);
    let state = std::sync::Arc::new(std::sync::Mutex::new(RangeState {
        mode: Scenario::Slow,
        requests: Vec::new(),
    }));
    let fixture = range_fixture(state.clone());
    let hash = expected(RANGE_LENGTH, 29);
    store
        .add(fixture.url("pause.bin"), "pause.bin".into(), hash.clone())
        .unwrap();
    let id = job(&store).id;
    store.pause(id.clone()).unwrap();
    assert!(job(&store).status == Status::Paused);
    assert!(!store.process_next().unwrap());
    store.retry(id.clone()).unwrap();
    thread::scope(|scope| {
        let worker = scope.spawn(|| store.process_next().unwrap());
        let start = std::time::Instant::now();
        loop {
            if state
                .lock()
                .unwrap()
                .requests
                .iter()
                .any(|request| request.starts_with("GET "))
            {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(3));
        }
        store.pause(id.clone()).unwrap();
        assert!(worker.join().unwrap());
    });
    assert!(job(&store).status == Status::Paused);
    let destination = settings.destination_folder.join("pause.bin");
    assert!(!destination.exists());
    state.lock().unwrap().mode = Scenario::Stable;
    store.retry(id).unwrap();
    store.process_next().unwrap();
    assert!(
        job(&store).status == Status::Complete,
        "{:?}",
        job(&store).error
    );
    assert_eq!(transfer::sha256(&destination).unwrap(), hash);
}

#[test]
fn replacing_a_remote_file_with_same_length_never_mixes_bytes() {
    let (_root, _destination, _, settings, store) = setup(None);
    let state = std::sync::Arc::new(std::sync::Mutex::new(RangeState {
        mode: Scenario::Interrupted,
        requests: Vec::new(),
    }));
    let fixture = range_fixture(state.clone());
    store
        .add(
            fixture.url("replace.bin"),
            "replace.bin".into(),
            String::new(),
        )
        .unwrap();
    store.process_next().unwrap();
    let interrupted = job(&store);
    assert!(interrupted.status == Status::Failed);
    state.lock().unwrap().mode = Scenario::Replaced;
    store.retry(interrupted.id).unwrap();
    store.process_next().unwrap();
    assert!(
        job(&store).status == Status::Complete,
        "{:?}",
        job(&store).error
    );
    assert_eq!(
        transfer::sha256(&settings.destination_folder.join("replace.bin")).unwrap(),
        expected(RANGE_LENGTH, 51)
    );
}

#[test]
fn assembles_hls_and_dash_video_and_rejects_missing_segments() {
    let (_root, _destination, _, settings, store) = setup(None);
    let local = tempfile::tempdir().unwrap();
    for (format, arguments, filename) in [
        (
            "hls",
            &["-hls_time", "1", "-hls_playlist_type", "vod"][..],
            "video.m3u8",
        ),
        ("dash", &["-seg_duration", "1"][..], "video.mpd"),
    ] {
        let path = local.path().join(format);
        fs::create_dir(&path).unwrap();
        let status = std::process::Command::new("ffmpeg")
            .current_dir(&path)
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x120:rate=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "3",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-g",
                "24",
                "-sc_threshold",
                "0",
                "-c:a",
                "aac",
                "-f",
                format,
            ])
            .args(arguments)
            .arg(filename)
            .status()
            .unwrap();
        assert!(status.success(), "could not generate {format} fixture");
    }
    fs::write(local.path().join("hls/master.m3u8"),
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=160x120\nvideo.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1920x1080\nmissing-1080p.m3u8\n"
    ).unwrap();
    let root = local.path().to_owned();
    let fixture = Fixture::start(move |mut stream, request| {
        let path = request.split_whitespace().nth(1).unwrap();
        let path = root.join(path.trim_start_matches('/'));
        if let Ok(bytes) = fs::read(path) {
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            let _ = stream.write_all(&bytes);
        } else {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    for (input, output) in [
        ("hls/master.m3u8", "hls.mkv"),
        ("dash/video.mpd", "dash.mkv"),
    ] {
        store
            .add_from_browser(fixture.url(input), output.into(), None, Some(480))
            .unwrap();
        store.process_next().unwrap();
        let completed = job(&store);
        assert!(
            completed.status == Status::Complete,
            "{output}: {:?}",
            completed.error
        );
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "csv=p=0",
            ])
            .arg(settings.destination_folder.join(output))
            .output()
            .unwrap();
        let streams = String::from_utf8(probe.stdout).unwrap();
        assert!(probe.status.success() && streams.contains("video") && streams.contains("audio"));
    }
    let dash_segment = fs::read_dir(local.path().join("dash"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("chunk-stream0-")
        })
        .unwrap();
    fs::remove_file(dash_segment).unwrap();
    fs::remove_file(local.path().join("hls/video1.ts")).unwrap();
    for (url, filename) in [
        ("dash/video.mpd", "dash-incomplete.mkv"),
        ("hls/video.m3u8", "hls-incomplete.mkv"),
    ] {
        store
            .add(fixture.url(url), filename.into(), String::new())
            .unwrap();
        store.process_next().unwrap();
        assert!(job(&store).status == Status::Failed);
        assert!(!settings.destination_folder.join(filename).exists());
    }
}
