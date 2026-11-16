#[path = "../src/media.rs"]
mod media;
#[path = "../src/network.rs"]
mod network;
#[path = "../src/progress.rs"]
mod progress;
#[path = "../src/proxy.rs"]
mod proxy;
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
        proxy: proxy::ProxySettings::Direct,
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
            proxy: proxy::ProxySettings::Direct,
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
            false,
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
fn only_completed_downloads_can_be_opened_and_completed_files_survive_list_removal() {
    let (_root, _destination, _, settings, store) = setup(None);
    let (url, server) = serve(Response::Complete(1024, 11));
    store
        .add(url, "complete.bin".into(), String::new())
        .unwrap();
    let id = job(&store).id;
    assert!(store.completed_path(&id).is_err());
    store.process_next().unwrap();
    server
        .join()
        .unwrap()
        .expect("download request reached the server");
    let path = store.completed_path(&id).unwrap();
    assert_eq!(path, settings.destination_folder.join("complete.bin"));
    assert_eq!(std::fs::read(&path).unwrap(), vec![11; 1024]);
    store.remove_download(&id).unwrap();
    assert!(store.snapshot().unwrap().jobs.is_empty());
    assert!(path.exists());
}

#[test]
fn youtube_watch_and_short_links_queue_as_media() {
    let (_root, _destination, _, _, store) = setup(None);
    for url in [
        "https://www.youtube.com/watch?v=BaW_jenozKc&list=ignore",
        "https://youtu.be/BaW_jenozKc",
        "https://m.youtube.com/shorts/BaW_jenozKc",
    ] {
        let name = format!("video-{}", store.snapshot().unwrap().jobs.len());
        store.add(url.into(), name.clone(), String::new()).unwrap();
        let queued = job(&store);
        assert!(queued.format == media::Format::Youtube);
        assert_eq!(queued.filename, format!("{name}.mkv"));
    }
    store
        .add(
            "https://www.youtube.com.example.test/watch?v=BaW_jenozKc".into(),
            "ordinary.bin".into(),
            String::new(),
        )
        .unwrap();
    assert!(job(&store).format == media::Format::File);
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
fn legacy_downloads_migrate_to_the_main_queue() {
    let (_root, _destination, database, settings, store) = setup(None);
    store
        .add(
            "https://example.test/file.bin".into(),
            "legacy.bin".into(),
            String::new(),
        )
        .unwrap();
    let id = job(&store).id;
    drop(store);
    let database_cleanup = rusqlite::Connection::open(&database).unwrap();
    let snapshot: String = database_cleanup
        .query_row("SELECT snapshot FROM app_state WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    let mut snapshot: serde_json::Value = serde_json::from_str(&snapshot).unwrap();
    snapshot.as_object_mut().unwrap().remove("queues");
    snapshot["settings"]["downloadSlots"] = 3.into();
    let legacy_job = snapshot["jobs"][0].as_object_mut().unwrap();
    legacy_job.remove("queueId");
    legacy_job.remove("addedAt");
    database_cleanup
        .execute(
            "UPDATE app_state SET snapshot = ?1 WHERE id = 1",
            [snapshot.to_string()],
        )
        .unwrap();
    drop(database_cleanup);
    let migrated = Store::open(&database, settings)
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(migrated.queues.len(), 1);
    assert_eq!(migrated.queues[0].id, store::DEFAULT_QUEUE);
    assert_eq!(migrated.queues[0].slots, 3);
    assert!(migrated.queues[0].running);
    assert_eq!(migrated.jobs[0].id, id);
    assert_eq!(migrated.jobs[0].queue_id, store::DEFAULT_QUEUE);
    assert!(migrated.jobs[0].added_at.is_none());
}

#[test]
fn queues_limit_workers_independently_and_can_stop_and_resume() {
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    let (_root, _destination, database, settings, store) = setup(None);
    let store = Arc::new(store);
    let other = store.save_queue(None, "Wi-Fi".into(), 2, None).unwrap();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let response_gate = Arc::clone(&gate);
    let (request, requests) = mpsc::channel();
    let server = Fixture::start(move |mut stream, raw| {
        if raw.starts_with("HEAD ") {
            let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        let path = raw.split_whitespace().nth(1).unwrap().to_owned();
        let _ = request.send(path);
        let (open, ready) = &*response_gate;
        let _guard = ready
            .wait_timeout_while(open.lock().unwrap(), Duration::from_secs(5), |open| !*open)
            .unwrap();
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata");
    });
    for name in ["main-1", "main-2"] {
        store
            .add(server.url(name), name.into(), String::new())
            .unwrap();
    }
    for name in ["wifi-1", "wifi-2", "wifi-3"] {
        store
            .add_to_queue(server.url(name), name.into(), String::new(), &other)
            .unwrap();
    }
    let pending = store
        .snapshot()
        .unwrap()
        .jobs
        .into_iter()
        .find(|job| job.filename == "wifi-3")
        .unwrap();
    store
        .move_download(&pending.id, store::DEFAULT_QUEUE)
        .unwrap();
    assert_eq!(job(&store).queue_id, store::DEFAULT_QUEUE);
    store.move_download(&pending.id, &other).unwrap();
    assert!(store.start_next(|_| {}).unwrap());
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(5)).unwrap(),
        "/main-1"
    );
    assert!(store
        .while_workers_idle::<()>(|| panic!("install started during a download"))
        .is_err());
    assert!(!store.start_next(|_| {}).unwrap());
    store.set_queue_running(&other, true).unwrap();
    assert!(store.start_next(|_| {}).unwrap());
    assert!(store.start_next(|_| {}).unwrap());
    let mut active = [
        requests.recv_timeout(Duration::from_secs(5)).unwrap(),
        requests.recv_timeout(Duration::from_secs(5)).unwrap(),
    ];
    active.sort();
    assert_eq!(active, ["/wifi-1", "/wifi-2"]);
    assert!(!store.start_next(|_| {}).unwrap());
    store.set_queue_running(&other, false).unwrap();
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();

    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let state = store.snapshot().unwrap();
        if state
            .jobs
            .iter()
            .any(|job| job.filename == "main-1" && job.status == Status::Complete)
            && state
                .jobs
                .iter()
                .filter(|job| job.queue_id == other)
                .all(|job| job.status == Status::Queued)
            && store.while_workers_idle(|| Ok(())).is_ok()
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "workers did not stop");
        thread::sleep(Duration::from_millis(25));
    }
    assert!(store.process_next().unwrap());
    assert!(!store.process_next().unwrap());
    let reopened = Store::open(&database, settings).unwrap();
    assert!(!reopened.process_next().unwrap());
    reopened.set_queue_running(&other, true).unwrap();
    for _ in 0..3 {
        assert!(reopened.process_next().unwrap());
    }
    assert!(reopened.while_workers_idle(|| Ok(())).is_ok());
    let finished = reopened.snapshot().unwrap();
    assert!(finished
        .jobs
        .iter()
        .all(|job| job.status == Status::Complete && job.added_at.is_some()));
    assert_eq!(
        finished
            .queues
            .iter()
            .find(|queue| queue.id == other)
            .unwrap()
            .slots,
        2
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bound_http_client_uses_the_selected_source_address() {
    assert!(progress::TransferControl::new(None).binding().is_none());
    let (peer, connected) = std::sync::mpsc::channel();
    let server = Fixture::start(move |mut stream, _| {
        peer.send(stream.peer_addr().unwrap().ip()).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata")
            .unwrap();
    });
    let address: std::net::IpAddr = "127.0.0.2".parse().unwrap();
    let control = progress::TransferControl::with_network(
        None,
        Some(network::Binding {
            name: "lo".into(),
            address,
        }),
        &proxy::ProxySettings::Direct,
    );
    let response = transfer::http_client(true, &control)
        .unwrap()
        .get(server.url("fixture"))
        .send()
        .unwrap();
    assert_eq!(&response.bytes().unwrap()[..], b"data");
    assert_eq!(
        connected.recv_timeout(Duration::from_secs(5)).unwrap(),
        address
    );
    let unavailable = progress::TransferControl::with_network(
        None,
        Some(network::Binding {
            name: "outernet-missing".into(),
            address,
        }),
        &proxy::ProxySettings::Direct,
    );
    assert!(transfer::http_client(true, &unavailable)
        .unwrap()
        .get(server.url("fixture"))
        .send()
        .is_err());
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

#[test]
fn custom_proxy_downloads_and_never_falls_back_to_origin() {
    let (_root, destination, _database, mut settings, store) = setup(None);
    let (proxy_url, proxy_server) = serve(Response::CompleteChunked);
    settings.proxy = proxy::ProxySettings::Custom(proxy_url.trim_end_matches("fixture.bin").into());
    store.settings(settings.clone()).unwrap();
    let source = "http://outernet-proxy-test.invalid/file.bin";
    store
        .add(source.into(), "proxied.bin".into(), String::new())
        .unwrap();
    assert!(store.process_next().unwrap());
    assert_eq!(
        fs::read(destination.path().join("proxied.bin")).unwrap(),
        b"example"
    );
    let request = proxy_server.join().unwrap().unwrap();
    assert!(request.starts_with(&format!("GET {source} HTTP/")));

    let (origin_url, origin) = serve(Response::CompleteChunked);
    let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
    settings.proxy =
        proxy::ProxySettings::Custom(format!("http://{}", unavailable.local_addr().unwrap()));
    drop(unavailable);
    store.settings(settings.clone()).unwrap();
    store
        .add(origin_url.clone(), "unavailable.bin".into(), String::new())
        .unwrap();
    assert!(store.process_next().unwrap());
    assert!(matches!(job(&store).status, Status::Failed));

    settings.proxy = proxy::ProxySettings::Direct;
    store.settings(settings).unwrap();
    store
        .add(origin_url, "direct.bin".into(), String::new())
        .unwrap();
    assert!(store.process_next().unwrap());
    assert_eq!(
        fs::read(destination.path().join("direct.bin")).unwrap(),
        b"example"
    );
    assert!(origin.join().unwrap().unwrap().starts_with("GET /"));
}

#[test]
fn system_proxy_uses_environment_while_direct_ignores_it() {
    const PROXY_TEST_CHILD: &str = "OUTERNET_SYSTEM_PROXY_TEST_CHILD";
    if let Ok(origin) = std::env::var(PROXY_TEST_CHILD) {
        for (mode, expected) in [
            (proxy::ProxySettings::System, b"example".as_slice()),
            (proxy::ProxySettings::Direct, b"dddd".as_slice()),
        ] {
            let control = progress::TransferControl::with_network(None, None, &mode);
            let response = transfer::http_client(true, &control)
                .unwrap()
                .get(&origin)
                .send()
                .unwrap();
            assert_eq!(response.bytes().unwrap().as_ref(), expected);
        }
        return;
    }
    let (proxy, proxy_server) = serve(Response::CompleteChunked);
    let (origin, origin_server) = serve(Response::Complete(4, b'd'));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "system_proxy_uses_environment_while_direct_ignores_it",
        ])
        .env(PROXY_TEST_CHILD, &origin)
        .env("HTTP_PROXY", proxy.trim_end_matches("fixture.bin"))
        .env("http_proxy", proxy.trim_end_matches("fixture.bin"))
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .env_remove("REQUEST_METHOD")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let request = proxy_server.join().unwrap().unwrap();
    assert!(request.starts_with(&format!("GET {origin} HTTP/")));
    assert!(origin_server.join().unwrap().unwrap().starts_with("GET /"));
}

#[test]
fn quick_download_honors_destination_and_can_run_now_or_later() {
    let (root, _destination, _database, _settings, store) = setup(None);
    let custom = root.path().join("other destination");
    fs::create_dir(&custom).unwrap();
    let queue = store.save_queue(None, "Manual".into(), 1, None).unwrap();
    let (url, server) = serve(Response::CompleteChunked);
    let id = store
        .prepare_download(url, "single.bin".into(), None, None)
        .unwrap();
    assert!(!store.process_next().unwrap());
    let discarded = store
        .prepare_download(
            "https://example.invalid/cancel".into(),
            "cancelled.bin".into(),
            None,
            None,
        )
        .unwrap();
    store.discard_draft(&discarded).unwrap();
    assert_eq!(store.snapshot().unwrap().jobs.len(), 1);
    assert_eq!(
        store
            .submit_download(
                Some(&id),
                String::new(),
                "single.bin".into(),
                String::new(),
                &queue,
                custom.clone(),
                true
            )
            .unwrap(),
        id
    );
    assert!(store.process_next().unwrap());
    assert!(server.join().unwrap().is_some());
    assert_eq!(fs::read(custom.join("single.bin")).unwrap(), b"example");

    let (url, later) = serve(Response::CompleteChunked);
    store.set_queue_running(&queue, true).unwrap();
    let id = store
        .submit_download(
            None,
            url,
            "later.bin".into(),
            String::new(),
            &queue,
            custom.clone(),
            false,
        )
        .unwrap();
    assert!(!store.process_next().unwrap());
    assert!(
        store
            .snapshot()
            .unwrap()
            .jobs
            .iter()
            .find(|job| job.id == id)
            .unwrap()
            .deferred
    );
    store.set_queue_running(&queue, true).unwrap();
    assert!(store.process_next().unwrap());
    assert!(later.join().unwrap().is_some());
    assert_eq!(fs::read(custom.join("later.bin")).unwrap(), b"example");
}

#[test]
fn removing_active_download_waits_for_the_worker_and_preserves_existing_destination_files() {
    use std::io::Write;
    let (_root, destination, _database, settings, store) = setup(None);
    let store = std::sync::Arc::new(store);
    let (entered, request) = std::sync::mpsc::channel();
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let response_gate = gate.clone();
    let server = Fixture::start(move |mut stream, raw| {
        if raw.starts_with("HEAD ") {
            let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        let _ = entered.send(());
        let (open, ready) = &*response_gate;
        let _guard = ready
            .wait_timeout_while(open.lock().unwrap(), Duration::from_secs(5), |open| !*open)
            .unwrap();
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata");
    });
    store
        .add(server.url("active"), "active.bin".into(), String::new())
        .unwrap();
    assert!(store.start_next(|_| {}).unwrap());
    request.recv_timeout(Duration::from_secs(5)).unwrap();
    let id = job(&store).id;
    store.remove_download(&id).unwrap();
    assert!(job(&store).removing);
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !store.snapshot().unwrap().jobs.is_empty() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(25));
    }
    assert!(!destination.path().join("active.bin").exists());
    assert!(fs::read_dir(settings.working_folder)
        .unwrap()
        .next()
        .is_none());

    store
        .add(
            "https://example.invalid/queued".into(),
            "queued.bin".into(),
            String::new(),
        )
        .unwrap();
    store.remove_download(&job(&store).id).unwrap();
    assert!(store.snapshot().unwrap().jobs.is_empty());
}

#[test]
fn explicit_download_now_can_start_two_jobs_in_a_one_slot_queue() {
    let (_root, destination, _database, _settings, store) = setup(None);
    let store = std::sync::Arc::new(store);
    let (requests, received) = std::sync::mpsc::channel();
    let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let response_gate = gate.clone();
    let server = Fixture::start(move |mut stream, raw| {
        if raw.starts_with("HEAD ") {
            let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }
        let _ = requests.send(());
        let (open, wake) = &*response_gate;
        let _guard = wake
            .wait_timeout_while(open.lock().unwrap(), Duration::from_secs(5), |open| !*open)
            .unwrap();
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata");
    });
    for name in ["first.bin", "second.bin"] {
        store
            .submit_download(
                None,
                server.url(name),
                name.into(),
                String::new(),
                store::DEFAULT_QUEUE,
                destination.path().to_owned(),
                true,
            )
            .unwrap();
        assert!(store.start_next(|_| {}).unwrap());
        received.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    store
        .add(server.url("queued.bin"), "queued.bin".into(), String::new())
        .unwrap();
    assert!(!store.start_next(|_| {}).unwrap());
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while store.while_workers_idle(|| Ok(())).is_err() {
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::read(destination.path().join("first.bin")).unwrap(),
        b"data"
    );
    assert_eq!(
        fs::read(destination.path().join("second.bin")).unwrap(),
        b"data"
    );
}
