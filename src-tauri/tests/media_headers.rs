#![allow(dead_code)]
#[path = "../src/media.rs"]
mod media;
#[path = "../src/network.rs"]
mod network;
#[path = "../src/progress.rs"]
mod progress;
#[path = "../src/proxy.rs"]
mod proxy;
#[path = "../src/transfer.rs"]
mod transfer;

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    thread,
    time::Duration,
};

/// Serves `root` only to requests that carry the page's referrer and session cookie,
/// the way hotlink-protected streaming sites do.
fn serve_protected(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let failed_once = Arc::new(AtomicBool::new(false));
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let root = root.clone();
            let failed_once = failed_once.clone();
            thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0; 1];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/')
                    .to_owned();
                let allowed = request.contains("referer: https://site.example/watch\r\n")
                    && request.contains("session=abc")
                    && request.contains("user-agent: mozilla/");
                if !allowed {
                    let _ = stream.write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
                if path == "video1.ts" && !failed_once.swap(true, Ordering::Relaxed) {
                    let _ = stream.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
                let Ok(mut body) = fs::read(root.join(&path)) else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                };
                let mut content_type = "video/mp2t";
                if path.ends_with(".m3u8") {
                    // Some sites mislabel playlists; the reader must not depend on the type.
                    content_type = "text/html";
                } else if path == "video0.ts" {
                    // Disguise the segment as an image, as some streaming sites do.
                    let mut disguised = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
                    disguised.extend_from_slice(&body);
                    body = disguised;
                    content_type = "image/png";
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            });
        }
    });
    format!("http://{address}/video.m3u8")
}

#[test]
fn forwards_browser_headers_and_recovers_disguised_and_flaky_segments() {
    let fixture = tempfile::tempdir().unwrap();
    let status = Command::new("ffmpeg")
        .current_dir(fixture.path())
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=10:duration=3",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=3",
            "-c:v",
            "libx264",
            "-g",
            "10",
            "-c:a",
            "aac",
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_playlist_type",
            "vod",
            "-hls_segment_filename",
            "video%d.ts",
            "video.m3u8",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "could not generate the HLS fixture");
    let url = serve_protected(fixture.path().to_path_buf());
    let work = tempfile::tempdir().unwrap();

    let headers = media::clean_headers(
        [
            ("Referer", "https://site.example/watch"),
            ("Cookie", "session=abc; theme=dark"),
            ("User-Agent", "Mozilla/5.0 Test"),
            ("Authorization", "Bearer must-not-be-forwarded"),
        ]
        .into_iter()
        .map(|(name, value)| (name.into(), value.into()))
        .collect(),
    );
    assert!(!headers.contains_key("authorization"));
    let control = progress::TransferControl::new(None);
    media::download(
        &url,
        &work.path().join("ok"),
        media::Format::Hls,
        None,
        &headers,
        &control,
    )
    .expect("the stream downloads with the page's headers");
    assert!(fs::metadata(work.path().join("ok/payload")).unwrap().len() > 0);

    let control = progress::TransferControl::new(None);
    let error = media::download(
        &url,
        &work.path().join("refused"),
        media::Format::Hls,
        None,
        &Default::default(),
        &control,
    )
    .unwrap_err();
    assert!(error.contains("refused access"), "{error}");
}
