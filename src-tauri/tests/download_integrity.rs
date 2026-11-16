#[path = "../src/media.rs"]
mod media;
#[path = "../src/progress.rs"]
mod progress;
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
        speed_limit_kib: None,
        download_slots: 1,
        keep_running: false,
        notify_when_finished: false,
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
            speed_limit_kib: None,
            download_slots: 1,
            keep_running: false,
            notify_when_finished: false,
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
fn schedule_survives_a_restart_and_holds_queued_transfers() {
    let (_root, _destination, database, settings, store) = setup(None);
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000;
    let (url, server) = serve(Response::Complete(32, 12));
    store
        .add(url, "scheduled.bin".into(), expected(32, 12))
        .unwrap();
    let recurring = store::DailyWindow {
        start_minute: 1320,
        end_minute: 240,
    };
    store.schedule(Some(start), Some(recurring)).unwrap();
    assert!(!store.process_next().unwrap());
    drop(store);

    let store = Store::open(&database, settings.clone()).unwrap();
    store.recover().unwrap();
    let persisted = store.snapshot().unwrap();
    assert_eq!(persisted.run_after, Some(start));
    assert_eq!(persisted.daily_window.unwrap().start_minute, 1320);
    assert_eq!(persisted.daily_window.unwrap().end_minute, 240);
    assert!(!store.process_next().unwrap());
    // A completed browser transfer can be published while new desktop transfers are waiting.
    let staged = settings.working_folder.join(".outernet-browser-fixture");
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("payload"), [17; 8]).unwrap();
    store
        .complete_browser_transfer(
            "https://example.com/received.bin".into(),
            "received.bin".into(),
            settings.clone(),
            "browser-fixture".into(),
            8,
            expected(8, 17),
        )
        .unwrap();
    assert!(store.process_next().unwrap());
    assert_eq!(
        fs::read(settings.destination_folder.join("received.bin")).unwrap(),
        [17; 8]
    );
    assert!(!store.process_next().unwrap());
    store.schedule(None, None).unwrap();
    assert!(store.process_next().unwrap());
    server.join().unwrap().unwrap();
    assert_eq!(
        fs::read(settings.destination_folder.join("scheduled.bin")).unwrap(),
        [12; 32]
    );
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
fn disconnected_working_folder_is_not_recreated_and_can_be_retried() {
    let (_root, _destination, database, settings, store) = setup(None);
    let (url, server) = serve(Response::Complete(1024, 27));
    let hash = expected(1024, 27);
    store
        .add(url, "reconnected.bin".into(), hash.clone())
        .unwrap();
    fs::remove_dir(&settings.working_folder).unwrap();
    store.process_next().unwrap();
    let failed = job(&store);
    assert!(failed.status == Status::Failed);
    assert!(failed
        .error
        .unwrap()
        .contains("in-progress folder is unavailable"));
    assert!(!settings.working_folder.exists());
    let destination = settings.destination_folder.join("reconnected.bin");
    assert!(!destination.exists());

    drop(store);
    fs::create_dir(&settings.working_folder).unwrap();
    let store = Store::open(&database, settings).unwrap();
    store.recover().unwrap();
    store.retry(failed.id).unwrap();
    store.process_next().unwrap();
    server.join().unwrap().expect("the engine made the request");
    assert!(job(&store).status == Status::Complete);
    assert_eq!(transfer::sha256(&destination).unwrap(), hash);
}

#[cfg(target_os = "linux")]
#[test]
fn working_file_write_failure_leaves_no_published_file() {
    const CHILD: &str = "OUTERNET_TEST_FILE_SIZE_LIMITED";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new("bash")
            .args([
                "-c",
                r#"trap '' XFSZ; ulimit -f 128 || exit; exec "$@""#,
                "outernet-test",
            ])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "working_file_write_failure_leaves_no_published_file",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    let (_root, _destination, _, settings, store) = setup(None);
    let (url, server) = serve(Response::Complete(512 * 1024, 11));
    store
        .add(url, "write-error.bin".into(), String::new())
        .unwrap();
    store.process_next().unwrap();
    // The sender may observe a closed socket before writing the entire fixture.
    let _ = server.join();
    let failed = job(&store);
    assert!(failed.status == Status::Failed);
    assert!(failed
        .error
        .unwrap()
        .contains("Cannot write the temporary file"));
    assert!(failed.sha256.is_none());
    assert!(!settings.destination_folder.join("write-error.bin").exists());
    assert_eq!(fs::read_dir(settings.working_folder).unwrap().count(), 0);
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
    Expired,
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
        if mode == Scenario::Expired {
            let _ = stream.write_all(
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        }
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
fn batch_is_atomic_and_reserves_distinct_filenames() {
    let (_root, _destination, _, _, store) = setup(None);
    let first = "https://example.test/first/file.zip".to_string();
    let second = "https://example.test/second/file.zip".to_string();
    assert!(store
        .add_batch(vec![
            (first.clone(), "file.zip".into()),
            ("invalid".into(), "file.zip".into())
        ])
        .is_err());
    assert!(store.snapshot().unwrap().jobs.is_empty());
    store
        .add_batch(vec![
            (first.clone(), "file.zip".into()),
            (second.clone(), "file.zip".into()),
        ])
        .unwrap();
    let jobs = store.snapshot().unwrap().jobs;
    assert_eq!(
        (jobs[1].url.as_str(), jobs[1].filename.as_str()),
        (first.as_str(), "file.zip")
    );
    assert_eq!(
        (jobs[0].url.as_str(), jobs[0].filename.as_str()),
        (second.as_str(), "file (2).zip")
    );
}

#[test]
fn replacing_a_link_reuses_checkpoint_with_publisher_hash() {
    let (_root, _destination, _, settings, store) = setup(None);
    let server = std::sync::Arc::new(std::sync::Mutex::new(RangeState {
        mode: Scenario::Interrupted,
        requests: vec![],
    }));
    let fixture = range_fixture(server.clone());
    let hash = expected(RANGE_LENGTH, 29);
    store
        .add(
            fixture.url("file.zip?token=expired"),
            "file.zip".into(),
            String::new(),
        )
        .unwrap();
    store.process_next().unwrap();
    let failed = job(&store);
    assert!(failed.status == Status::Failed);
    server.lock().unwrap().mode = Scenario::Expired;
    store.retry(failed.id.clone()).unwrap();
    store.process_next().unwrap();
    let expired = job(&store);
    assert!(expired.status == Status::Failed);
    assert!(expired
        .error
        .unwrap()
        .contains("Completed parts were kept."));
    {
        let mut server = server.lock().unwrap();
        server.mode = Scenario::Stable;
        server.requests.clear();
    }
    store
        .replace_link(
            failed.id,
            fixture.url("file.zip?token=fresh"),
            Some(hash.clone()),
        )
        .unwrap();
    store.process_next().unwrap();
    assert!(
        job(&store).status == Status::Complete,
        "{:?}",
        job(&store).error
    );
    assert_eq!(
        transfer::sha256(&settings.destination_folder.join("file.zip")).unwrap(),
        hash
    );
    let server = server.lock().unwrap();
    let gets = server
        .requests
        .iter()
        .filter(|request| request.starts_with("GET "))
        .collect::<Vec<_>>();
    assert_eq!(gets.len(), 1);
    assert!(gets[0].starts_with("GET /file.zip?token=fresh "));
}

#[test]
fn priority_and_slots_keep_parallel_transfers_independent() {
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    use store::Priority;

    let (_root, _destination, database, mut settings, store) = setup(None);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let response_gate = Arc::clone(&gate);
    let (request, arrivals) = mpsc::channel();
    let fixture = Fixture::start(move |mut stream, raw| {
        if raw.starts_with("HEAD ") {
            let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        let name = raw
            .split_whitespace()
            .nth(1)
            .unwrap()
            .trim_start_matches('/');
        let _ = request.send(name.to_string());
        let (open, wake) = &*response_gate;
        let _gate = wake
            .wait_timeout_while(open.lock().unwrap(), Duration::from_secs(5), |open| !*open)
            .unwrap();
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{name}",
            name.len()
        );
    });
    for (name, priority) in [
        ("low.bin", Priority::Low),
        ("normal.bin", Priority::Normal),
        ("high-old.bin", Priority::High),
        ("high-new.bin", Priority::High),
    ] {
        store
            .add(fixture.url(name), name.into(), String::new())
            .unwrap();
        store.set_priority(job(&store).id, priority).unwrap();
    }
    settings.download_slots = 2;
    store.settings(settings.clone()).unwrap();
    drop(store);
    let store = Arc::new(Store::open(&database, settings.clone()).unwrap());
    assert_eq!(store.current_settings().unwrap().download_slots, 2);

    let (complete, finished) = mpsc::channel();
    let launch = || {
        let complete = complete.clone();
        store
            .start_next(move |job| {
                let _ = complete.send(job.filename);
            })
            .unwrap()
    };
    assert!(launch());
    assert_eq!(
        arrivals.recv_timeout(Duration::from_secs(3)).unwrap(),
        "high-old.bin"
    );
    assert!(launch());
    assert_eq!(
        arrivals.recv_timeout(Duration::from_secs(3)).unwrap(),
        "high-new.bin"
    );
    assert!(!launch());

    let mut reduced = settings.clone();
    reduced.download_slots = 1;
    store.settings(reduced).unwrap();
    assert!(!launch());
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    for _ in 0..2 {
        let name = finished.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            fs::read(settings.destination_folder.join(&name)).unwrap(),
            name.as_bytes()
        );
    }
    assert!(launch());
    assert_eq!(
        arrivals.recv_timeout(Duration::from_secs(3)).unwrap(),
        "normal.bin"
    );
    assert_eq!(
        finished.recv_timeout(Duration::from_secs(5)).unwrap(),
        "normal.bin"
    );
    assert!(launch());
    assert_eq!(
        arrivals.recv_timeout(Duration::from_secs(3)).unwrap(),
        "low.bin"
    );
    assert_eq!(
        finished.recv_timeout(Duration::from_secs(5)).unwrap(),
        "low.bin"
    );
    assert!(!launch());
}

#[test]
fn parallel_ranges_share_the_speed_limit_and_report_live_progress() {
    let (_root, _destination, database, mut settings, store) = setup(None);
    let fixture = range_fixture(std::sync::Arc::new(std::sync::Mutex::new(RangeState {
        mode: Scenario::Stable,
        requests: Vec::new(),
    })));
    let url = fixture.url("limited.bin");
    let digest = expected(RANGE_LENGTH, 29);
    store
        .add(url, "limited.bin".into(), digest.clone())
        .unwrap();
    settings.speed_limit_kib = Some(8 * 1024);
    store.settings(settings.clone()).unwrap();
    drop(store);
    let store = Store::open(&database, settings.clone()).unwrap();
    assert_eq!(
        store.snapshot().unwrap().settings.speed_limit_kib,
        Some(8 * 1024)
    );

    let start = std::time::Instant::now();
    thread::scope(|scope| {
        let worker = scope.spawn(|| store.process_next().unwrap());
        let mut observed = false;
        while !worker.is_finished() {
            if let Some(transfer) = job(&store).transfer {
                observed |= transfer.total_bytes == Some(RANGE_LENGTH)
                    && transfer.completed_bytes > 0
                    && transfer.completed_bytes < RANGE_LENGTH
                    && transfer.bytes_per_second > 0;
            }
            thread::sleep(Duration::from_millis(30));
        }
        assert!(worker.join().unwrap());
        assert!(observed, "expected live size, progress and measured speed");
    });
    assert!(start.elapsed() >= Duration::from_millis(1450));
    assert!(
        job(&store).status == Status::Complete,
        "{:?}",
        job(&store).error
    );
    assert!(job(&store).transfer.is_none());
    assert_eq!(
        transfer::sha256(&settings.destination_folder.join("limited.bin")).unwrap(),
        digest
    );
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
    for (folder, format, arguments, filename) in [
        (
            "hls",
            "hls",
            &["-hls_time", "1", "-hls_playlist_type", "vod"][..],
            "video.m3u8",
        ),
        (
            "hls-range",
            "hls",
            &[
                "-hls_time",
                "1",
                "-hls_playlist_type",
                "vod",
                "-hls_segment_type",
                "fmp4",
                "-hls_flags",
                "single_file",
            ][..],
            "video.m3u8",
        ),
        ("dash", "dash", &["-seg_duration", "1"][..], "video.mpd"),
    ] {
        let path = local.path().join(folder);
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
    let hls = local.path().join("hls");
    let encrypted = hls.join("encrypted");
    fs::create_dir(&encrypted).unwrap();
    use aws_lc_rs::{
        cipher::{EncryptionContext, PaddedBlockEncryptingKey, UnboundCipherKey, AES_128},
        iv::FixedLength,
    };
    let original = fs::read_to_string(hls.join("video.m3u8")).unwrap();
    let mut encrypted_manifest =
        original.replace("#EXT-X-MEDIA-SEQUENCE:0", "#EXT-X-MEDIA-SEQUENCE:11");
    assert!(encrypted_manifest.contains("#EXT-X-MEDIA-SEQUENCE:11"));
    for (number, key_byte, iv) in [(0, 23, 11_u128), (1, 49, 42_u128), (2, 49, 42_u128)] {
        let filename = format!("video{number}.ts");
        let mut bytes = fs::read(hls.join(&filename)).unwrap();
        let key_bytes = [key_byte; 16];
        let key = UnboundCipherKey::new(&AES_128, &key_bytes).unwrap();
        let key = PaddedBlockEncryptingKey::cbc_pkcs7(key).unwrap();
        key.less_safe_encrypt(
            &mut bytes,
            EncryptionContext::Iv128(FixedLength::from(iv.to_be_bytes())),
        )
        .unwrap();
        fs::write(encrypted.join(&filename), &bytes).unwrap();
        if number < 2 {
            let iv = if number == 0 {
                String::new()
            } else {
                format!(",IV=0x{iv:x}")
            };
            fs::write(encrypted.join(format!("key{number}")), key_bytes).unwrap();
            let marker = format!("#EXT-X-KEY:METHOD=AES-128,URI=\"key{number}\"{iv}\n");
            // The key tag applies to the segment that follows this duration.
            encrypted_manifest =
                encrypted_manifest.replace(&filename, &format!("{marker}{filename}"));
        }
    }
    fs::write(encrypted.join("video.m3u8"), &encrypted_manifest).unwrap();
    fs::write(encrypted.join("wrong-key"), [73; 16]).unwrap();
    fs::write(
        encrypted.join("wrong.m3u8"),
        encrypted_manifest.replace("key1", "wrong-key"),
    )
    .unwrap();
    let range_directory = local.path().join("hls-range");
    let range_playlist = fs::read_to_string(range_directory.join("video.m3u8")).unwrap();
    assert!(range_playlist.contains("#EXT-X-MAP:") && range_playlist.contains("#EXT-X-BYTERANGE:"));
    let mut first = true;
    let implicit = range_playlist
        .lines()
        .map(|line| {
            if let Some((range, _)) = line.split_once('@') {
                if range.starts_with("#EXT-X-BYTERANGE:") {
                    if !first {
                        return range;
                    }
                    first = false;
                }
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(range_directory.join("implicit.m3u8"), &implicit).unwrap();
    fs::write(
        range_directory.join("changed.m3u8"),
        implicit.replace("video.m4s", "changed.m4s"),
    )
    .unwrap();
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let live_updates = Arc::new(AtomicUsize::new(0));
    let live_requests = Arc::clone(&live_updates);
    let gap_updates = AtomicUsize::new(0);
    let second_attempts = AtomicUsize::new(0);
    let first_live_bytes = fs::metadata(hls.join("video0.ts")).unwrap().len();
    let second_live_bytes = fs::metadata(hls.join("video1.ts")).unwrap().len();
    let root = local.path().to_owned();
    let fixture = Fixture::start(move |mut stream, request| {
        let name = request
            .split_whitespace()
            .nth(1)
            .unwrap()
            .trim_start_matches('/');
        if name == "live.m3u8" || name == "gap.m3u8" {
            let gap = name == "gap.m3u8";
            let requests = if gap { &gap_updates } else { &live_requests };
            let update = requests.fetch_add(1, Ordering::Relaxed);
            let sequence = if gap && update > 0 { 93 } else { 91 };
            let segments = if update == 0 {
                "#EXTINF:1.0,\n/hls/video0.ts\n"
            } else if gap {
                "#EXTINF:1.0,\n/hls/video2.ts\n"
            } else {
                "#EXTINF:1.0,\n/hls/video0.ts\n#EXTINF:1.0,\n/hls/video1.ts?transient\n"
            };
            let body = format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n{segments}"
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            return;
        }
        let name = if name == "hls/video1.ts?transient" {
            if second_attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                return;
            }
            "hls/video1.ts"
        } else {
            name
        };
        let changed = name.ends_with("/changed.m4s");
        let path = root.join(if changed { "hls-range/video.m4s" } else { name });
        if let Ok(bytes) = fs::read(path) {
            if let Some(range) = request.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("range") {
                    value.trim().strip_prefix("bytes=")
                } else {
                    None
                }
            }) {
                let (start, last) = range.split_once('-').unwrap();
                let (start, last) = (
                    start.parse::<usize>().unwrap(),
                    last.parse::<usize>().unwrap(),
                );
                let resumed = request
                    .lines()
                    .any(|line| line.to_ascii_lowercase().starts_with("if-range:"));
                let etag = if changed && resumed {
                    "different"
                } else {
                    "stable"
                };
                write!(stream,
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{last}/{}\r\nETag: \"{etag}\"\r\nConnection: close\r\n\r\n",
                    last + 1 - start, bytes.len()).unwrap();
                let _ = stream.write_all(&bytes[start..=last]);
                return;
            }
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
        ("hls/encrypted/video.m3u8", "hls-encrypted.mkv"),
        ("hls-range/implicit.m3u8", "hls-range.mkv"),
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
    let decoded_hash = |name: &str| {
        let result = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(settings.destination_folder.join(name))
            .args(["-map", "0:v:0", "-map", "0:a:0", "-f", "hash", "-"])
            .output()
            .unwrap();
        assert!(result.status.success());
        result.stdout
    };
    assert_eq!(decoded_hash("hls.mkv"), decoded_hash("hls-encrypted.mkv"));
    store
        .add_from_browser(
            fixture.url("live.m3u8"),
            "record.mkv".into(),
            Some(media::Format::HlsLive),
            None,
        )
        .unwrap();
    let store = Arc::new(store);
    let live_id = job(&store).id;
    let (done, completed) = std::sync::mpsc::channel();
    assert!(store
        .start_next(move |job| {
            let _ = done.send(job);
        })
        .unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    while job(&store).bytes < first_live_bytes + second_live_bytes {
        assert!(
            std::time::Instant::now() < deadline,
            "recording did not receive both segments: {:?}",
            job(&store).error
        );
        thread::sleep(Duration::from_millis(25));
    }
    store.stop_recording(live_id).unwrap();
    assert!(
        completed
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .status
            == Status::Complete
    );
    assert!(live_updates.load(Ordering::Relaxed) >= 3);
    assert!(settings.destination_folder.join("record.mkv").is_file());

    store
        .add_from_browser(
            fixture.url("gap.m3u8"),
            "gap.mkv".into(),
            Some(media::Format::HlsLive),
            None,
        )
        .unwrap();
    store.process_next().unwrap();
    let gap = job(&store);
    assert!(gap.status == Status::Failed);
    assert!(gap.error.unwrap().contains("lost one or more segments"));
    assert!(!settings.destination_folder.join("gap.mkv").exists());
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
        ("hls/encrypted/wrong.m3u8", "wrong-key.mkv"),
        ("hls-range/changed.m3u8", "hls-changed.mkv"),
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
