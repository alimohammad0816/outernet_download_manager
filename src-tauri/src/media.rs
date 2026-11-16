#[path = "media/dash.rs"]
mod dash;
#[path = "media/live.rs"]
mod live;
#[path = "media/youtube.rs"]
mod youtube;

use crate::{progress::TransferControl, transfer};
use aws_lc_rs::{
    cipher::{DecryptionContext, StreamingDecryptingKey, UnboundCipherKey, AES_128},
    iv::FixedLength,
};
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
    sync::OnceLock,
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
    HlsLive,
    Dash,
    Youtube,
}

impl Format {
    pub fn from_url(url: &Url) -> Self {
        let path = url.path().to_ascii_lowercase();
        if youtube::video_id(url).is_some() {
            Self::Youtube
        } else if path.ends_with(".m3u8") {
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
    let url = transfer::parse_url(url)?;
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
    let duration = if format == Format::Youtube {
        youtube::prepare(&url, directory, &mut ffmpeg, max_height, control)?
    } else {
        let client = transfer::http_client(true, control)?;
        let (manifest_url, manifest) = read_manifest(&client, &url)?;
        match format {
            Format::Hls => prepare_hls(
                &client,
                directory,
                manifest_url,
                manifest,
                &mut ffmpeg,
                max_height,
                control,
            )?,
            Format::HlsLive => live::record(
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
            Format::File | Format::Youtube => return Err("Select a video stream format.".into()),
        }
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
            let attrs = attributes(key);
            if attrs.get("METHOD") != Some(&"NONE")
                && (attrs.get("METHOD") != Some(&"AES-128")
                    || attrs
                        .get("KEYFORMAT")
                        .is_some_and(|format| *format != "identity")
                    || attrs
                        .get("KEYFORMATVERSIONS")
                        .is_some_and(|versions| *versions != "1"))
            {
                return Err("This HLS encryption method or key format is unsupported.".into());
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

struct HlsKey {
    bytes: [u8; 16],
    iv: Option<u128>,
}

impl HlsKey {
    fn load(client: &Client, base: &Url, tag: &str) -> Result<Option<Self>, String> {
        let attrs = attributes(tag);
        if attrs.get("METHOD") == Some(&"NONE") {
            return Ok(None);
        }
        let iv = attrs
            .get("IV")
            .map(|value| {
                value
                    .strip_prefix("0x")
                    .or_else(|| value.strip_prefix("0X"))
                    .filter(|digits| {
                        !digits.is_empty()
                            && digits.len() <= 32
                            && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .and_then(|digits| u128::from_str_radix(digits, 16).ok())
                    .ok_or("The HLS key has an invalid initialization vector.")
            })
            .transpose()?;
        let url = source_url(base, attrs.get("URI").ok_or("The HLS key has no URL.")?)?;
        let response = open_http(client, &url)?;
        if base.scheme() == "https" && response.url().scheme() != "https" {
            return Err("The HLS key URL must use HTTPS for an HTTPS playlist.".into());
        }
        let mut bytes = Vec::with_capacity(17);
        response
            .take(17)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read the HLS key.")?;
        let bytes = bytes
            .try_into()
            .map_err(|_| "The HLS key must contain exactly 16 bytes.")?;
        Ok(Some(Self { bytes, iv }))
    }

    fn decoder(&self, sequence: Option<u64>) -> Result<StreamingDecryptingKey, String> {
        let iv = self
            .iv
            .or_else(|| sequence.map(u128::from))
            .ok_or("An encrypted HLS initialization section requires an explicit IV.")?;
        let key = UnboundCipherKey::new(&AES_128, &self.bytes)
            .map_err(|_| "Cannot initialize the HLS decryption key.")?;
        StreamingDecryptingKey::cbc_pkcs7(
            key,
            DecryptionContext::Iv128(FixedLength::from(iv.to_be_bytes())),
        )
        .map_err(|_| "Cannot initialize HLS decryption.".into())
    }
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
        return Err("Use Record live in the browser extension for an ongoing HLS stream.".into());
    }
    if lines.iter().any(|line| {
        ["#EXT-X-SKIP", "#EXT-X-GAP", "#EXT-X-STREAM-INF"]
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
    let mut pending_range = None;
    let mut previous_range = None;
    let mut resources = HashMap::new();
    let mut key = None;
    let mut sequence = None;
    for line in lines {
        if let Some(tag) = line.strip_prefix("#EXT-X-KEY:") {
            control.check()?;
            key = HlsKey::load(client, base, tag)?;
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            if count != 0 || sequence.is_some() {
                return Err("An HLS media sequence tag is misplaced or duplicated.".into());
            }
            sequence = Some(
                value
                    .parse::<u64>()
                    .map_err(|_| "The HLS media sequence is invalid.")?,
            );
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            let seconds = value
                .split(',')
                .next()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0 && *value < 86_400.0)
                .ok_or("An HLS segment has an invalid duration.")?;
            if pending_duration.replace(seconds).is_some() {
                return Err("An HLS segment is missing its URL.".into());
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            if pending_range.replace(value).is_some() {
                return Err("An HLS segment has more than one byte range.".into());
            }
        } else if let Some(map) = line.strip_prefix("#EXT-X-MAP:") {
            let attrs = attributes(map);
            let url = source_url(
                base,
                attrs
                    .get("URI")
                    .ok_or("An HLS initialization segment is missing its URL.")?,
            )?;
            let name = format!("{prefix}-init-{count}.mp4");
            let range = attrs
                .get("BYTERANGE")
                .map(|value| hls_range(value, None))
                .transpose()?;
            fetch_segment(
                client,
                &url,
                &directory.join(&name),
                range,
                &mut resources,
                key.as_ref().map(|key| key.decoder(None)).transpose()?,
                control,
            )?;
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
            let implicit_start = previous_range
                .take()
                .filter(|(previous, _)| previous == &url)
                .map(|(_, end)| end);
            let range = pending_range
                .take()
                .map(|value| hls_range(value, implicit_start))
                .transpose()?;
            let number = sequence
                .unwrap_or(0)
                .checked_add(count as u64)
                .ok_or("The HLS media sequence overflows.")?;
            fetch_segment(
                client,
                &url,
                &directory.join(&name),
                range,
                &mut resources,
                key.as_ref()
                    .map(|key| key.decoder(Some(number)))
                    .transpose()?,
                control,
            )?;
            previous_range = range.map(|(_, end)| (url, end));
            playlist.push_str(&format!("#EXTINF:{seconds},\n{name}\n"));
            duration += seconds;
            target = target.max(seconds.ceil());
            count += 1;
        }
    }
    if pending_duration.is_some() || pending_range.is_some() || count == 0 || duration <= 0.0 {
        return Err("The HLS playlist is incomplete.".into());
    }
    let list = directory.join(format!("{prefix}.m3u8"));
    let text = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target}\n{playlist}#EXT-X-ENDLIST\n"
    );
    fs::write(&list, text).map_err(disk_error)?;
    Ok((list, duration))
}

fn hls_range(value: &str, implicit_start: Option<u64>) -> Result<(u64, u64), String> {
    let (length, start) = match value.split_once('@') {
        Some((length, start)) => (length, start.parse::<u64>().ok()),
        None => (value, implicit_start),
    };
    start
        .zip(length.parse::<u64>().ok().filter(|length| *length > 0))
        .and_then(|(start, length)| start.checked_add(length).map(|end| (start, end)))
        .ok_or_else(|| "The HLS playlist contains an invalid byte range.".into())
}

#[derive(PartialEq)]
struct HlsResource {
    url: Url,
    etag: String,
    total: u64,
}

fn open_hls_range(
    client: &Client,
    url: &Url,
    start: u64,
    end: u64,
    resources: &mut HashMap<Url, HlsResource>,
) -> Result<Response, String> {
    let last = end - 1;
    let previous = resources.get(url);
    let mut request = client
        .get(url.clone())
        .header(header::RANGE, format!("bytes={start}-{last}"));
    if let Some(previous) = previous {
        request = request.header(header::IF_RANGE, &previous.etag);
    }
    let response = request
        .send()
        .map_err(|_| "Cannot retrieve the HLS byte range.")?;
    let headers = response.headers();
    let prefix = format!("bytes {start}-{last}/");
    let total = headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(&prefix))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|total| *total >= end);
    let resource = match (total, transfer::strong_etag(headers)) {
        (Some(total), Some(etag))
            if response.status() == StatusCode::PARTIAL_CONTENT
                && transfer::unencoded(headers)
                && headers.get(header::CONTENT_LENGTH).is_none_or(|value| {
                    value
                        .to_str()
                        .ok()
                        .and_then(|value| value.parse::<u64>().ok())
                        == Some(end - start)
                }) =>
        {
            HlsResource {
                url: response.url().clone(),
                etag: etag.into(),
                total,
            }
        }
        _ => {
            return Err(
                "The server did not return the requested HLS byte range with a strong ETag.".into(),
            )
        }
    };
    if previous.is_some_and(|previous| *previous != resource) {
        return Err("An HLS resource changed during transfer.".into());
    }
    resources.entry(url.clone()).or_insert(resource);
    Ok(response)
}

fn fetch_segment(
    client: &Client,
    url: &Url,
    destination: &Path,
    range: Option<(u64, u64)>,
    resources: &mut HashMap<Url, HlsResource>,
    decryption: Option<StreamingDecryptingKey>,
    control: &TransferControl,
) -> Result<(), String> {
    control.check()?;
    let mut file = File::options()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(disk_error)?;
    if decryption.is_some() && range.is_some() {
        return Err("Encrypted HLS byte ranges are currently unsupported.".into());
    }
    let (response, expected) = match range {
        Some((start, end)) => (
            open_hls_range(client, url, start, end, resources)?,
            Some(end - start),
        ),
        None => open_segment(client, url)?,
    };
    copy_response(response, &mut file, expected, decryption, control)
}

fn open_segment(client: &Client, url: &Url) -> Result<(Response, Option<u64>), String> {
    let response = open_http(client, url)?;
    let expected = response.content_length();
    Ok((response, expected))
}

fn copy_segment(
    client: &Client,
    url: &Url,
    file: &mut File,
    control: &TransferControl,
) -> Result<(), String> {
    control.check()?;
    let (response, expected) = open_segment(client, url)?;
    copy_response(response, file, expected, None, control)
}

fn copy_response(
    mut response: Response,
    file: &mut File,
    expected: Option<u64>,
    mut decryption: Option<StreamingDecryptingKey>,
    control: &TransferControl,
) -> Result<(), String> {
    let mut bytes = 0;
    let mut block = vec![0; control.chunk_bytes(64 * 1024)];
    let mut plaintext = decryption
        .as_ref()
        .map(|_| vec![0; block.len() + AES_128.block_len()]);
    loop {
        control.check()?;
        let length = response
            .read(&mut block)
            .map_err(|_| "The video segment transfer was interrupted.")?;
        if length == 0 {
            break;
        }
        bytes += length as u64;
        if expected.is_some_and(|expected| bytes > expected) {
            return Err("The server returned more bytes than expected for a video segment.".into());
        }
        control.account_network(length)?;
        if let (Some(decoder), Some(plaintext)) = (&mut decryption, &mut plaintext) {
            let output = decoder
                .update(&block[..length], plaintext)
                .map_err(|_| "Cannot decrypt the HLS segment.")?;
            file.write_all(output.written()).map_err(disk_error)?;
        } else {
            file.write_all(&block[..length]).map_err(disk_error)?;
        }
        control.commit_bytes(length as u64);
    }
    if bytes == 0 || expected.is_some_and(|length| length != bytes) {
        return Err("The server returned an incomplete video segment.".into());
    }
    if let (Some(decoder), Some(plaintext)) = (decryption, plaintext.as_mut()) {
        let output = decoder.finish(plaintext).map_err(|_| {
            "The HLS segment could not be decrypted. The key or segment may be invalid."
        })?;
        file.write_all(output.written()).map_err(disk_error)?;
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

static ENGINE_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

pub fn set_engine_directory(directory: PathBuf) {
    let _ = ENGINE_DIRECTORY.set(directory);
}

fn engine_path(program: &str) -> PathBuf {
    ENGINE_DIRECTORY
        .get()
        .map(|directory| directory.join(format!("{program}{}", std::env::consts::EXE_SUFFIX)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(program))
}

fn media_command(program: &str) -> Command {
    let mut command = Command::new(engine_path(program));
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
        format!("Cannot start the media engine. Repair or reinstall Outernet. ({error})")
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
