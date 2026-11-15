#[path = "media/dash.rs"]
mod dash;

use crate::{progress::TransferControl, transfer};
use reqwest::{
    blocking::{Client, Response},
    header, StatusCode,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use url::Url;

const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SEGMENTS: usize = 20_000;

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    File,
    Hls,
    Dash,
}

impl Format {
    pub fn from_url(url: &Url) -> Self {
        let path = url.path().to_ascii_lowercase();
        if path.ends_with(".m3u8") {
            Self::Hls
        } else if path.ends_with(".mpd") {
            Self::Dash
        } else {
            Self::File
        }
    }
}

pub fn download(
    url: &str,
    directory: &Path,
    format: Format,
    max_height: Option<u32>,
    control: &TransferControl,
) -> Result<(), String> {
    control.check()?;
    fs::create_dir(directory).map_err(disk_error)?;
    let client = transfer::http_client(true)?;
    let url = transfer::parse_url(url)?;
    let (manifest_url, manifest) = read_manifest(&client, &url)?;
    let mut ffmpeg = media_command("ffmpeg");
    ffmpeg.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "warning",
        "-xerror",
        "-abort_on",
        "empty_output+empty_output_stream",
    ]);
    let duration = match format {
        Format::Hls => prepare_hls(
            &client,
            directory,
            manifest_url,
            manifest,
            &mut ffmpeg,
            max_height,
            control,
        )?,
        Format::Dash => dash::prepare(
            &client,
            directory,
            &manifest_url,
            &manifest,
            &mut ffmpeg,
            max_height,
            control,
        )?,
        Format::File => return Err("Select a video stream format.".into()),
    };
    ffmpeg
        .args(["-sn", "-dn", "-c", "copy", "-f", "matroska", "-n"])
        .arg(directory.join("payload"));
    run_mux(&mut ffmpeg, control)?;
    let result = directory.join("payload");
    verify_output(&result, duration)?;
    let mut decode = media_command("ffmpeg");
    decode
        .args([
            "-nostdin",
            "-v",
            "error",
            "-xerror",
            "-err_detect",
            "explode",
            "-protocol_whitelist",
            "file",
            "-i",
        ])
        .arg(result)
        // Preserve Matroska's millisecond timestamps in the null muxer during decoding.
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-fps_mode:v",
            "passthrough",
            "-enc_time_base:v",
            "1:1000",
            "-f",
            "null",
            "-",
        ]);
    run_mux(&mut decode, control)
}

fn open_http(client: &Client, url: &Url) -> Result<Response, String> {
    let response = client
        .get(url.clone())
        .send()
        .map_err(|_| "Cannot retrieve the media source.")?;
    let headers = response.headers();
    if response.status() != StatusCode::OK
        || !transfer::unencoded(headers)
        || headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"))
    {
        return Err(format!(
            "The media source returned an unexpected response ({}).",
            response.status()
        ));
    }
    Ok(response)
}

fn read_manifest(client: &Client, url: &Url) -> Result<(Url, String), String> {
    let response = open_http(client, url)?;
    let resolved = response.url().clone();
    let mut text = String::new();
    let size = response
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| "Cannot read the media manifest.")? as u64;
    if size > MAX_MANIFEST_BYTES {
        return Err("The media manifest is larger than supported.".into());
    }
    Ok((resolved, text))
}

#[derive(Default)]
struct HlsVariant {
    video: Option<Url>,
    audio: Option<Url>,
}

fn hls_lines(text: &str) -> Result<Vec<&str>, String> {
    let lines: Vec<_> = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.first() != Some(&"#EXTM3U") {
        return Err("The URL did not return an HLS playlist.".into());
    }
    for line in &lines {
        if let Some(key) = line
            .strip_prefix("#EXT-X-KEY:")
            .or_else(|| line.strip_prefix("#EXT-X-SESSION-KEY:"))
        {
            if attributes(key).get("METHOD") != Some(&"NONE") {
                return Err(
                    "This HLS playlist uses encryption. Choose an unencrypted stream.".into(),
                );
            }
        }
    }
    Ok(lines)
}

fn attributes(mut value: &str) -> HashMap<&str, &str> {
    let mut result = HashMap::new();
    while let Some((name, rest)) = value.split_once('=') {
        let (field, following) = if let Some(quoted) = rest.strip_prefix('"') {
            let Some((end, following)) = quoted.split_once('"') else {
                break;
            };
            (end, following.trim_start_matches(','))
        } else {
            rest.split_once(',').unwrap_or((rest, ""))
        };
        result.insert(name, field);
        value = following;
    }
    result
}

fn source_url(base: &Url, relative: &str) -> Result<Url, String> {
    if relative.contains("{$") {
        return Err("Variable-based HLS URLs are currently unsupported.".into());
    }
    let joined = base
        .join(relative)
        .map_err(|_| "The playlist contains an invalid media URL.")?;
    transfer::parse_url(joined.as_str())
}

fn quality_rank(height: u32, bandwidth: u64, cap: Option<u32>) -> (u8, u32, u64) {
    match cap {
        None => (2, height, bandwidth),
        Some(limit) if height > limit => (1, u32::MAX - height, bandwidth),
        Some(_) if height == 0 => (0, 0, bandwidth),
        Some(_) => (2, height, bandwidth),
    }
}

fn select_hls(base: &Url, lines: &[&str], max_height: Option<u32>) -> Result<HlsVariant, String> {
    let mut best = None;
    for pair in lines.windows(2) {
        let Some(variant) = pair[0].strip_prefix("#EXT-X-STREAM-INF:") else {
            continue;
        };
        if pair[1].starts_with('#') {
            return Err("An HLS variant is missing its playlist URL.".into());
        }
        let attrs = attributes(variant);
        let bandwidth = attrs
            .get("AVERAGE-BANDWIDTH")
            .or_else(|| attrs.get("BANDWIDTH"))
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or("An HLS variant is missing its bandwidth.")?;
        let height = attrs
            .get("RESOLUTION")
            .and_then(|value| value.split_once('x'))
            .and_then(|(_, height)| height.parse::<u32>().ok())
            .unwrap_or(0);
        let rank = quality_rank(height, bandwidth, max_height);
        if best
            .as_ref()
            .is_none_or(|(previous, _, _)| rank > *previous)
        {
            best = Some((rank, pair[1], attrs.get("AUDIO").copied()));
        }
    }
    let Some((_, video, audio_group)) = best else {
        return Ok(HlsVariant::default());
    };
    let mut audio = None;
    if let Some(group) = audio_group {
        let selected = lines
            .iter()
            .filter_map(|line| line.strip_prefix("#EXT-X-MEDIA:"))
            .map(attributes)
            .filter(|attrs| {
                attrs.get("TYPE") == Some(&"AUDIO") && attrs.get("GROUP-ID") == Some(&group)
            })
            .max_by_key(|attrs| attrs.get("DEFAULT") == Some(&"YES"))
            .ok_or("The selected HLS variant is missing its audio group.")?;
        if let Some(uri) = selected.get("URI") {
            audio = Some(source_url(base, uri)?);
        }
    }
    Ok(HlsVariant {
        video: Some(source_url(base, video)?),
        audio,
    })
}

fn prepare_hls(
    client: &Client,
    directory: &Path,
    base: Url,
    manifest: String,
    ffmpeg: &mut Command,
    max_height: Option<u32>,
    control: &TransferControl,
) -> Result<f64, String> {
    let lines = hls_lines(&manifest)?;
    let selected = select_hls(&base, &lines, max_height)?;
    let (video_base, video_manifest) = match selected.video {
        Some(url) => read_manifest(client, &url)?,
        None => (base, manifest),
    };
    let (video, duration) = stage_hls(
        client,
        directory,
        "video",
        &video_base,
        &video_manifest,
        control,
    )?;
    hls_input(ffmpeg, &video);
    if let Some(audio) = selected.audio {
        let (audio_base, audio_manifest) = read_manifest(client, &audio)?;
        let (audio, audio_duration) = stage_hls(
            client,
            directory,
            "audio",
            &audio_base,
            &audio_manifest,
            control,
        )?;
        if !close_duration(duration, audio_duration) {
            return Err("The HLS video and audio playlists have different durations.".into());
        }
        hls_input(ffmpeg, &audio);
        ffmpeg.args(["-map", "0:v:0", "-map", "1:a:0"]);
    }
    Ok(duration)
}

fn stage_hls(
    client: &Client,
    directory: &Path,
    prefix: &str,
    base: &Url,
    manifest: &str,
    control: &TransferControl,
) -> Result<(PathBuf, f64), String> {
    let lines = hls_lines(manifest)?;
    if !lines.contains(&"#EXT-X-ENDLIST") {
        return Err("Choose a finished HLS video. Live streams are currently unsupported.".into());
    }
    if lines.iter().any(|line| {
        [
            "#EXT-X-BYTERANGE",
            "#EXT-X-SKIP",
            "#EXT-X-GAP",
            "#EXT-X-STREAM-INF",
        ]
        .iter()
        .any(|tag| line.starts_with(tag))
    }) {
        return Err("This HLS playlist uses an unsupported segment layout.".into());
    }

    let mut playlist = String::new();
    let mut pending_duration = None;
    let mut duration = 0.0;
    let mut target = 1.0_f64;
    let mut count = 0;
    let mut mapped = false;
    for line in lines {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            let seconds = value
                .split(',')
                .next()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0 && *value < 86_400.0)
                .ok_or("An HLS segment has an invalid duration.")?;
            if pending_duration.replace(seconds).is_some() {
                return Err("An HLS segment is missing its URL.".into());
            }
        } else if let Some(map) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = attributes(map);
            if attrs.contains_key("BYTERANGE") {
                return Err("HLS initialization byte ranges are currently unsupported.".into());
            }
            let url = source_url(
                base,
                attrs
                    .get("URI")
                    .ok_or("An HLS initialization segment is missing its URL.")?,
            )?;
            let name = format!("{prefix}-init-{count}.mp4");
            fetch_segment(client, &url, &directory.join(&name), control)?;
            playlist.push_str(&format!("#EXT-X-MAP:URI=\"{name}\"\n"));
            mapped = true;
        } else if line == "#EXT-X-DISCONTINUITY" {
            playlist.push_str("#EXT-X-DISCONTINUITY\n");
        } else if !line.starts_with('#') {
            let seconds = pending_duration
                .take()
                .ok_or("An HLS segment is missing its duration.")?;
            if count >= MAX_SEGMENTS {
                return Err("The HLS playlist has more segments than supported.".into());
            }
            let url = source_url(base, line)?;
            let extension = match url.path().rsplit('.').next() {
                Some(extension @ ("ts" | "m4s" | "mp4" | "aac" | "mp3" | "ac3" | "ec3")) => {
                    extension
                }
                _ if mapped => "m4s",
                _ => "ts",
            };
            let name = format!("{prefix}-{count}.{extension}");
            fetch_segment(client, &url, &directory.join(&name), control)?;
            playlist.push_str(&format!("#EXTINF:{seconds},\n{name}\n"));
            duration += seconds;
            target = target.max(seconds.ceil());
            count += 1;
        }
    }
    if pending_duration.is_some() || count == 0 || duration <= 0.0 {
        return Err("The HLS playlist is incomplete.".into());
    }
    let list = directory.join(format!("{prefix}.m3u8"));
    let text = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target}\n{playlist}#EXT-X-ENDLIST\n"
    );
    fs::write(&list, text).map_err(disk_error)?;
    Ok((list, duration))
}

fn fetch_segment(
    client: &Client,
    url: &Url,
    destination: &Path,
    control: &TransferControl,
) -> Result<(), String> {
    let mut file = File::options()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(disk_error)?;
    copy_segment(client, url, &mut file, control)
}

fn copy_segment(
    client: &Client,
    url: &Url,
    file: &mut File,
    control: &TransferControl,
) -> Result<(), String> {
    control.check()?;
    let mut response = open_http(client, url)?;
    let expected = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|length| length.parse::<u64>().ok());
    let mut bytes = 0;
    let mut block = vec![0; control.chunk_bytes(64 * 1024)];
    loop {
        control.check()?;
        let length = response
            .read(&mut block)
            .map_err(|_| "The video segment transfer was interrupted.")?;
        if length == 0 {
            break;
        }
        control.account_network(length)?;
        file.write_all(&block[..length]).map_err(disk_error)?;
        control.commit_bytes(length as u64);
        bytes += length as u64;
    }
    if bytes == 0 || expected.is_some_and(|length| length != bytes) {
        return Err("The server returned an incomplete video segment.".into());
    }
    Ok(())
}

fn hls_input(ffmpeg: &mut Command, list: &Path) {
    ffmpeg
        .args([
            "-protocol_whitelist",
            "file",
            "-allowed_extensions",
            "ALL",
            "-f",
            "hls",
            "-i",
        ])
        .arg(list);
}

fn iso_duration(value: &str) -> Option<f64> {
    let mut remaining = value.strip_prefix("PT")?;
    let mut duration = 0.0;
    for (unit, multiplier) in [('H', 3600.0), ('M', 60.0), ('S', 1.0)] {
        if let Some((number, rest)) = remaining.split_once(unit) {
            let number: f64 = number.parse().ok()?;
            if number < 0.0 || !number.is_finite() {
                return None;
            }
            duration += number * multiplier;
            remaining = rest;
        }
    }
    (remaining.is_empty() && duration > 0.0 && duration.is_finite()).then_some(duration)
}

fn media_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

fn run_mux(ffmpeg: &mut Command, control: &TransferControl) -> Result<(), String> {
    let mut child = ffmpeg.spawn().map_err(|error| {
        format!("Cannot start FFmpeg. Install ffmpeg and add it to PATH. ({error})")
    })?;
    let mut diagnostics = child.stderr.take().expect("piped stderr");
    let (status, warnings) = std::thread::scope(|scope| {
        let warnings = scope.spawn(|| io::copy(&mut diagnostics, &mut io::sink()));
        let status = transfer::wait_for_child(&mut child, control);
        if status.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        (status, warnings.join().expect("diagnostic worker"))
    });
    let status = status.map_err(disk_error)?;
    if !status.success() || warnings.map_err(disk_error)? != 0 {
        return Err("FFmpeg reported a media error or warning. The video was rejected.".into());
    }
    Ok(())
}

fn verify_output(path: &Path, expected: f64) -> Result<(), String> {
    let output = media_command("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .output()
        .map_err(|error| format!("Cannot start ffprobe: {error}"))?;
    let info: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Cannot read the completed video's metadata.")?;
    let length = info["format"]["duration"]
        .as_str()
        .and_then(|duration| duration.parse::<f64>().ok());
    let video = info["streams"]
        .as_array()
        .is_some_and(|streams| streams.iter().any(|stream| stream["codec_type"] == "video"));
    if !output.status.success()
        || !video
        || !length.is_some_and(|length| close_duration(expected, length))
    {
        return Err(
            "The completed video is unreadable or its duration differs from the manifest.".into(),
        );
    }
    Ok(())
}

fn close_duration(expected: f64, actual: f64) -> bool {
    actual.is_finite() && (expected - actual).abs() <= 1.0
}

fn disk_error(error: io::Error) -> String {
    format!("Cannot save or process the temporary video: {error}")
}
