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
    UnsolicitedPartial,
}

fn serve(response: Response) -> (String, thread::JoinHandle<Option<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let start = std::time::Instant::now();
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
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() <= 16 * 1024);
        }
        let raw = match response {
            Response::TruncatedChunked => Some("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n1000\r\npartial data"),
            Response::UnsolicitedPartial => Some("HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 0-3/12\r\nConnection: close\r\n\r\ndata"),
            _ => None,
        };
        if let Some(raw) = raw {
            stream.write_all(raw.as_bytes()).unwrap();
            return Some(String::from_utf8(request).unwrap());
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
        Some(String::from_utf8(request).unwrap())
    });
    (format!("http://{address}/fixture.bin"), server)
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
    let request = server.join().unwrap().expect("aria2 made the request");
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
        server.join().unwrap().expect("aria2 made the request");
        assert!(job(&store).status == Status::Failed);
        assert!(!settings.destination_folder.join("invalid.bin").exists());
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
    server.join().unwrap().expect("aria2 made the request");
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
    server.join().unwrap().expect("aria2 made the request");
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
