#[path = "media/dash.rs"]
mod dash;
#[path = "media/live.rs"]
mod live;
#[path = "media/site.rs"]
mod site;
#[path = "media/youtube.rs"]
mod youtube;

use crate::{progress::TransferControl, transfer};
use aws_lc_rs::{
    cipher::{DecryptionContext, StreamingDecryptingKey, UnboundCipherKey, AES_128},
    iv::FixedLength,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{
    blocking::{Client, Response},
    cookie::Jar,
    header::{self, HeaderMap, HeaderName, HeaderValue},
    StatusCode,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::Duration,
};
use url::Url;

const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SEGMENTS: usize = 20_000;
const SEGMENT_WORKERS: usize = 6;
const ATTEMPTS: u32 = 5;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const FORWARDED_HEADERS: [&str; 4] = ["cookie", "origin", "referer", "user-agent"];
const LIVE_ERROR: &str = "Use Record live in the browser extension for an ongoing HLS stream.";
const DRM_ERROR: &str =
    "This video is protected by DRM (such as Widevine or FairPlay), which Outernet cannot download.";
pub const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

/// Request headers the browser sent for a media source, keyed by lowercase name.
pub type Headers = BTreeMap<String, String>;

/// Keeps only the headers needed to fetch media the way the browser did.
pub fn clean_headers(input: Headers) -> Headers {
    let mut total = 0;
    input
        .into_iter()
        .filter_map(|(name, value)| {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_owned();
            (FORWARDED_HEADERS.contains(&name.as_str())
                && !value.is_empty()
                && HeaderValue::from_str(&value).is_ok())
            .then_some((name, value))
        })
        .filter(|(name, value)| {
            total += name.len() + value.len();
            total <= MAX_HEADER_BYTES
        })
        .collect()
}

/// Cookies from a `Cookie` header apply to the source host, or to the whole site for `www.` hosts.
fn cookie_domain(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(host.strip_prefix("www.").unwrap_or(host).to_owned())
}

fn cookie_pairs(headers: &Headers) -> impl Iterator<Item = (&str, &str)> {
    headers
        .get("cookie")
        .map(String::as_str)
        .unwrap_or_default()
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| !name.trim().is_empty())
        .map(|(name, value)| (name.trim(), value.trim()))
}

fn media_client(url: &Url, headers: &Headers, control: &TransferControl) -> Result<Client, String> {
    let jar = Arc::new(Jar::default());
    let www = url.host_str().is_some_and(|host| host.starts_with("www."));
    for (name, value) in cookie_pairs(headers) {
        let scope = match cookie_domain(url) {
            Some(domain) if www => format!("; Domain={domain}"),
            _ => String::new(),
        };
        jar.add_cookie_str(&format!("{name}={value}; Path=/{scope}"), url);
    }
    let mut defaults = HeaderMap::new();
    for name in ["origin", "referer"] {
        if let Some(value) = headers
            .get(name)
            .and_then(|value| HeaderValue::from_str(value).ok())
        {
            defaults.insert(HeaderName::from_static(name), value);
        }
    }
    let agent = headers
        .get("user-agent")
        .map_or(BROWSER_USER_AGENT, String::as_str)
        .to_owned();
    transfer::http_client_with(true, control, move |builder| {
        builder
            .cookie_provider(jar)
            .default_headers(defaults)
            .user_agent(agent)
    })
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    #[default]
    File,
    Hls,
    HlsLive,
    Dash,
    Youtube,
    /// A web page whose video is found by the site extractor (yt-dlp).
    Site,
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

fn ffmpeg_command() -> Command {
    let mut ffmpeg = media_command("ffmpeg");
    ffmpeg.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-abort_on",
        "empty_output+empty_output_stream",
    ]);
    ffmpeg
}

fn fresh_directory(directory: &Path) -> Result<(), String> {
    transfer::reset_working_directory(directory)?;
    fs::create_dir(directory).map_err(disk_error)
}

pub fn download(
    url: &str,
    directory: &Path,
    format: Format,
    max_height: Option<u32>,
    headers: &Headers,
    control: &TransferControl,
) -> Result<(), String> {
    control.check()?;
    fs::create_dir(directory).map_err(disk_error)?;
    let url = transfer::parse_url(url)?;
    let mut ffmpeg = ffmpeg_command();
    let duration = match format {
        Format::Youtube => Some(youtube::prepare(
            &url,
            directory,
            &mut ffmpeg,
            max_height,
            control,
        )?),
        Format::Site => site::prepare(&url, directory, &mut ffmpeg, max_height, headers, control)?,
        Format::Hls => {
            let native = media_client(&url, headers, control).and_then(|client| {
                let (manifest_url, manifest) = read_manifest(&client, &url)?;
                prepare_hls(
                    &client,
                    directory,
                    manifest_url,
                    manifest,
                    &mut ffmpeg,
                    max_height,
                    control,
                )
            });
            match native {
                Ok(duration) => Some(duration),
                Err(error) if control.is_paused() || error == LIVE_ERROR || error == DRM_ERROR => {
                    return Err(error)
                }
                Err(error) => {
                    // The site extractor handles HLS variations the built-in reader rejects.
                    control.set_total(None);
                    fresh_directory(directory)?;
                    ffmpeg = ffmpeg_command();
                    site::prepare(&url, directory, &mut ffmpeg, max_height, headers, control)
                        .map_err(|fallback| {
                            if control.is_paused() {
                                fallback
                            } else {
                                format!("{error} The backup engine also failed: {fallback}")
                            }
                        })?
                }
            }
        }
        Format::HlsLive | Format::Dash => {
            let client = media_client(&url, headers, control)?;
            let (manifest_url, manifest) = read_manifest(&client, &url)?;
            Some(if format == Format::HlsLive {
                live::record(
                    &client,
                    directory,
                    manifest_url,
                    manifest,
                    &mut ffmpeg,
                    max_height,
                    control,
                )?
            } else {
                dash::prepare(
                    &client,
                    directory,
                    &manifest_url,
                    &manifest,
                    &mut ffmpeg,
                    max_height,
                    control,
                )?
            })
        }
        Format::File => return Err("Select a video stream format.".into()),
    };
    ffmpeg
        .args(["-sn", "-dn", "-c", "copy", "-f", "matroska", "-n"])
        .arg(directory.join("payload"));
    run_mux(&mut ffmpeg, control)?;
    verify_output(&directory.join("payload"), duration)
}

/// A failed request, and whether trying it again could succeed.
struct Failure {
    message: String,
    retry: bool,
}

impl Failure {
    fn retry(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry: true,
        }
    }

    fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retry: false,
        }
    }
}

fn retrying<T>(
    control: &TransferControl,
    mut attempt: impl FnMut() -> Result<T, Failure>,
) -> Result<T, String> {
    let mut delay = Duration::from_millis(500);
    let mut remaining = ATTEMPTS;
    loop {
        control.check()?;
        remaining -= 1;
        match attempt() {
            Ok(value) => return Ok(value),
            Err(_) if control.is_paused() => return Err(control.check().unwrap_err()),
            Err(failure) if !failure.retry || remaining == 0 => return Err(failure.message),
            Err(_) => {
                let mut waited = Duration::ZERO;
                while waited < delay {
                    control.check()?;
                    thread::sleep(Duration::from_millis(50));
                    waited += Duration::from_millis(50);
                }
                delay *= 2;
            }
        }
    }
}

fn request(client: &Client, url: &Url) -> Result<Response, Failure> {
    let host = url.host_str().unwrap_or_default();
    let response = client
        .get(url.clone())
        .send()
        .map_err(|_| Failure::retry(format!("Cannot connect to the media server ({host}).")))?;
    let status = response.status();
    if status != StatusCode::OK {
        let message = match status.as_u16() {
            401 | 403 => format!(
                "The media server ({host}) refused access ({status}). Start the video in your browser, then send it again so Outernet uses the page's session."
            ),
            404 | 410 => format!(
                "The media server ({host}) no longer has this video ({status}). Reload the page and send it again."
            ),
            _ => format!("The media server ({host}) returned an unexpected response ({status})."),
        };
        let retry = matches!(status.as_u16(), 408 | 425 | 429) || status.is_server_error();
        return Err(Failure { message, retry });
    }
    if !transfer::unencoded(response.headers()) {
        return Err(Failure::fatal(format!(
            "The media server ({host}) returned a compressed response that cannot be used as video."
        )));
    }
    Ok(response)
}

fn open_http(client: &Client, url: &Url) -> Result<Response, String> {
    request(client, url).map_err(|failure| failure.message)
}

fn read_manifest(client: &Client, url: &Url) -> Result<(Url, String), String> {
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(700));
        }
        let response = match request(client, url) {
            Ok(response) => response,
            Err(failure) if failure.retry => {
                last = failure.message;
                continue;
            }
            Err(failure) => return Err(failure.message),
        };
        let resolved = response.url().clone();
        let mut text = String::new();
        let size = match response
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_string(&mut text)
        {
            Ok(size) => size as u64,
            Err(_) => {
                last = "Cannot read the media manifest.".into();
                continue;
            }
        };
        if size > MAX_MANIFEST_BYTES {
            return Err("The media manifest is larger than supported.".into());
        }
        return Ok((resolved, text));
    }
    Err(last)
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
    let mut identity = false;
    let mut protected = false;
    for line in &lines {
        if let Some(key) = line
            .strip_prefix("#EXT-X-KEY:")
            .or_else(|| line.strip_prefix("#EXT-X-SESSION-KEY:"))
        {
            let attrs = attributes(key);
            match attrs.get("METHOD").copied() {
                Some("NONE") => {}
                Some("AES-128")
                    if attrs
                        .get("KEYFORMAT")
                        .is_none_or(|format| *format == "identity") =>
                {
                    identity = true
                }
                _ => protected = true,
            }
        }
    }
    // A playlist may announce DRM keys next to a usable identity key; only DRM alone blocks it.
    if protected && !identity {
        return Err(DRM_ERROR.into());
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
            .unwrap_or(0);
        let height = attrs
            .get("RESOLUTION")
            .and_then(|value| value.split_once(['x', 'X']))
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
    // A missing audio group usually means the audio is multiplexed into the video.
    let audio = audio_group
        .and_then(|group| {
            lines
                .iter()
                .filter_map(|line| line.strip_prefix("#EXT-X-MEDIA:"))
                .map(attributes)
                .filter(|attrs| {
                    attrs.get("TYPE") == Some(&"AUDIO") && attrs.get("GROUP-ID") == Some(&group)
                })
                .max_by_key(|attrs| attrs.get("DEFAULT") == Some(&"YES"))
        })
        .and_then(|selected| selected.get("URI").map(|uri| source_url(base, uri)))
        .transpose()?;
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
    /// Loads the key named by an `EXT-X-KEY` tag. `Ok(None)` means the segments that follow are clear.
    fn load(
        client: &Client,
        base: &Url,
        tag: &str,
        cache: &mut HashMap<String, [u8; 16]>,
        control: &TransferControl,
    ) -> Result<Option<Self>, String> {
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
        let uri = attrs.get("URI").ok_or("The HLS key has no URL.")?;
        if let Some(bytes) = cache.get(*uri) {
            return Ok(Some(Self { bytes: *bytes, iv }));
        }
        let bytes = if let Some(data) = uri.strip_prefix("data:") {
            inline_key(data)?
        } else {
            let url = source_url(base, uri)?;
            let bytes = retrying(control, || {
                let response = request(client, &url)?;
                if base.scheme() == "https" && response.url().scheme() != "https" {
                    return Err(Failure::fatal(
                        "The HLS key URL must use HTTPS for an HTTPS playlist.",
                    ));
                }
                let mut bytes = Vec::with_capacity(17);
                response
                    .take(17)
                    .read_to_end(&mut bytes)
                    .map_err(|_| Failure::retry("Cannot read the HLS key."))?;
                Ok(bytes)
            })?;
            key_bytes(bytes)?
        };
        cache.insert((*uri).to_owned(), bytes);
        Ok(Some(Self { bytes, iv }))
    }

    fn for_segment(&self, sequence: Option<u64>) -> Result<SegmentKey, String> {
        let iv = self
            .iv
            .or_else(|| sequence.map(u128::from))
            .ok_or("An encrypted HLS initialization section requires an explicit IV.")?;
        Ok(SegmentKey {
            bytes: self.bytes,
            iv,
        })
    }
}

fn key_bytes(bytes: Vec<u8>) -> Result<[u8; 16], String> {
    bytes
        .try_into()
        .map_err(|_| "The HLS key must contain exactly 16 bytes.".into())
}

fn inline_key(data: &str) -> Result<[u8; 16], String> {
    let (meta, payload) = data
        .split_once(',')
        .ok_or("The HLS key data URL is invalid.")?;
    let bytes = if meta.ends_with(";base64") {
        STANDARD
            .decode(payload)
            .map_err(|_| "The HLS key data URL is invalid.")?
    } else {
        percent_encoding::percent_decode_str(payload).collect()
    };
    key_bytes(bytes)
}

#[derive(Clone, Copy)]
struct SegmentKey {
    bytes: [u8; 16],
    iv: u128,
}

impl SegmentKey {
    fn decoder(&self) -> Result<StreamingDecryptingKey, String> {
        let key = UnboundCipherKey::new(&AES_128, &self.bytes)
            .map_err(|_| "Cannot initialize the HLS decryption key.")?;
        StreamingDecryptingKey::cbc_pkcs7(
            key,
            DecryptionContext::Iv128(FixedLength::from(self.iv.to_be_bytes())),
        )
        .map_err(|_| "Cannot initialize HLS decryption.".into())
    }
}

/// One file the playlist needs: a media segment or an initialization section.
struct SegmentTask {
    url: Url,
    path: PathBuf,
    range: Option<(u64, u64)>,
    key: Option<SegmentKey>,
    transport_stream: bool,
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
    if !lines.contains(&"#EXT-X-ENDLIST") && !lines.contains(&"#EXT-X-PLAYLIST-TYPE:VOD") {
        return Err(LIVE_ERROR.into());
    }
    if lines.iter().any(|line| {
        ["#EXT-X-SKIP", "#EXT-X-STREAM-INF"]
            .iter()
            .any(|tag| line.starts_with(tag))
    }) {
        return Err("This HLS playlist uses an unsupported segment layout.".into());
    }

    let mut playlist = String::new();
    let mut tasks = Vec::new();
    let mut pending_duration = None;
    let mut duration = 0.0;
    let mut target = 1.0_f64;
    let mut count = 0;
    let mut mapped = false;
    let mut gap = false;
    let mut pending_range = None;
    let mut previous_range = None;
    let mut keys = HashMap::new();
    let mut key = None;
    let mut sequence = None;
    for line in lines {
        if let Some(tag) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = attributes(tag);
            // Alternative DRM key systems for the same segments are skipped.
            if attrs
                .get("KEYFORMAT")
                .is_none_or(|format| *format == "identity")
            {
                control.check()?;
                key = HlsKey::load(client, base, tag, &mut keys, control)?;
            }
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
                .and_then(|value| value.trim().parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0 && *value < 86_400.0)
                .ok_or("An HLS segment has an invalid duration.")?;
            if pending_duration.replace(seconds).is_some() {
                return Err("An HLS segment is missing its URL.".into());
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            if pending_range.replace(value).is_some() {
                return Err("An HLS segment has more than one byte range.".into());
            }
        } else if line == "#EXT-X-GAP" {
            gap = true;
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
            tasks.push(SegmentTask {
                url,
                path: directory.join(&name),
                range,
                key: key.as_ref().map(|key| key.for_segment(None)).transpose()?,
                transport_stream: false,
            });
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
            previous_range = range.map(|(_, end)| (url.clone(), end));
            count += 1;
            if std::mem::take(&mut gap) {
                // The server marks this segment as unavailable; leave a hole in the timeline.
                playlist.push_str("#EXT-X-DISCONTINUITY\n");
                continue;
            }
            let extension = match url.path().rsplit('.').next() {
                Some(extension @ ("ts" | "m4s" | "mp4" | "aac" | "mp3" | "ac3" | "ec3")) => {
                    extension.to_owned()
                }
                _ if mapped => "m4s".into(),
                _ => "ts".into(),
            };
            let name = format!("{prefix}-{count}.{extension}");
            tasks.push(SegmentTask {
                url,
                path: directory.join(&name),
                range,
                key: key
                    .as_ref()
                    .map(|key| key.for_segment(Some(number)))
                    .transpose()?,
                transport_stream: !mapped && extension == "ts",
            });
            playlist.push_str(&format!("#EXTINF:{seconds},\n{name}\n"));
            duration += seconds;
            target = target.max(seconds.ceil());
        }
    }
    if pending_duration.is_some() || pending_range.is_some() || tasks.is_empty() || duration <= 0.0
    {
        return Err("The HLS playlist is incomplete.".into());
    }
    fetch_segments(client, &tasks, control)?;
    let list = directory.join(format!("{prefix}.m3u8"));
    let text = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target}\n{playlist}#EXT-X-ENDLIST\n"
    );
    fs::write(&list, text).map_err(disk_error)?;
    Ok((list, duration))
}

/// Downloads segments over several connections, stopping at the first lasting failure.
fn fetch_segments(
    client: &Client,
    tasks: &[SegmentTask],
    control: &TransferControl,
) -> Result<(), String> {
    let resources = Mutex::new(HashMap::new());
    // Fetch the first range of each byte-range resource alone so later ranges can pin its ETag.
    let mut seen = std::collections::HashSet::new();
    let (first, rest): (Vec<_>, Vec<_>) = tasks
        .iter()
        .partition(|task| task.range.is_some() && seen.insert(task.url.clone()));
    for task in first {
        fetch_segment(client, task, &resources, control)?;
    }
    let next = AtomicUsize::new(0);
    let failure = Mutex::new(None);
    thread::scope(|scope| {
        for _ in 0..SEGMENT_WORKERS.min(rest.len()) {
            scope.spawn(|| loop {
                if failure.lock().expect("segment failure lock").is_some() {
                    break;
                }
                let Some(task) = rest.get(next.fetch_add(1, Ordering::Relaxed)) else {
                    break;
                };
                if let Err(error) = fetch_segment(client, task, &resources, control) {
                    failure
                        .lock()
                        .expect("segment failure lock")
                        .get_or_insert(error);
                    break;
                }
            });
        }
    });
    control.check()?;
    match failure.into_inner().expect("segment failure lock") {
        Some(error) => Err(error),
        None => Ok(()),
    }
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

#[derive(Clone, PartialEq)]
struct HlsResource {
    url: Url,
    etag: Option<String>,
    total: u64,
}

fn open_hls_range(
    client: &Client,
    url: &Url,
    start: u64,
    end: u64,
    resources: &Mutex<HashMap<Url, HlsResource>>,
) -> Result<Response, Failure> {
    let last = end - 1;
    let previous = resources
        .lock()
        .expect("HLS resource lock")
        .get(url)
        .cloned();
    let mut request = client
        .get(url.clone())
        .header(header::RANGE, format!("bytes={start}-{last}"));
    if let Some(etag) = previous
        .as_ref()
        .and_then(|previous| previous.etag.as_ref())
    {
        request = request.header(header::IF_RANGE, etag);
    }
    let response = request
        .send()
        .map_err(|_| Failure::retry("Cannot retrieve the HLS byte range."))?;
    let headers = response.headers();
    let prefix = format!("bytes {start}-{last}/");
    let total = headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(&prefix))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|total| *total >= end);
    let resource = match total {
        Some(total)
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
                etag: transfer::strong_etag(headers).map(Into::into),
                total,
            }
        }
        _ if response.status().is_server_error() => {
            return Err(Failure::retry(format!(
                "The media server returned {} for an HLS byte range.",
                response.status()
            )))
        }
        _ => {
            return Err(Failure::fatal(
                "The server did not return the requested HLS byte range.",
            ))
        }
    };
    let mut known = resources.lock().expect("HLS resource lock");
    if known.get(url).is_some_and(|previous| *previous != resource) {
        return Err(Failure::fatal("An HLS resource changed during transfer."));
    }
    known.entry(url.clone()).or_insert(resource);
    Ok(response)
}

fn fetch_segment(
    client: &Client,
    task: &SegmentTask,
    resources: &Mutex<HashMap<Url, HlsResource>>,
    control: &TransferControl,
) -> Result<(), String> {
    retrying(control, || {
        let mut file =
            File::create(&task.path).map_err(|error| Failure::fatal(disk_error(error)))?;
        let decryption = task
            .key
            .as_ref()
            .map(SegmentKey::decoder)
            .transpose()
            .map_err(Failure::fatal)?;
        let (response, expected) = match task.range {
            Some((start, end)) => (
                open_hls_range(client, &task.url, start, end, resources)?,
                Some(end - start),
            ),
            None => {
                let response = request(client, &task.url)?;
                let expected = response.content_length();
                (response, expected)
            }
        };
        copy_body(response, &mut file, expected, decryption, control)
    })?;
    if task.transport_stream {
        strip_disguise(&task.path).map_err(disk_error)?;
    }
    Ok(())
}

/// Some sites prepend an image header to MPEG-TS segments; drop everything before the first TS packet.
fn strip_disguise(path: &Path) -> io::Result<()> {
    let mut head = Vec::with_capacity(64 * 1024);
    File::open(path)?.take(64 * 1024).read_to_end(&mut head)?;
    if head.first() == Some(&0x47) || head.len() < 377 {
        return Ok(());
    }
    let Some(offset) = (1..head.len() - 376)
        .find(|&at| head[at] == 0x47 && head[at + 188] == 0x47 && head[at + 376] == 0x47)
    else {
        return Ok(());
    };
    let data = fs::read(path)?;
    fs::write(path, &data[offset..])
}

fn copy_segment(
    client: &Client,
    url: &Url,
    file: &mut File,
    control: &TransferControl,
) -> Result<(), String> {
    control.check()?;
    let response = open_http(client, url)?;
    let expected = response.content_length();
    copy_body(response, file, expected, None, control).map_err(|failure| failure.message)
}

fn copy_body(
    response: Response,
    file: &mut File,
    expected: Option<u64>,
    decryption: Option<StreamingDecryptingKey>,
    control: &TransferControl,
) -> Result<(), Failure> {
    let mut committed = 0;
    let result = copy_counted(
        response,
        file,
        expected,
        decryption,
        control,
        &mut committed,
    );
    if result.is_err() {
        control.discard_bytes(committed);
    }
    result
}

fn copy_counted(
    mut response: Response,
    file: &mut File,
    expected: Option<u64>,
    mut decryption: Option<StreamingDecryptingKey>,
    control: &TransferControl,
    committed: &mut u64,
) -> Result<(), Failure> {
    let disk = |error| Failure::fatal(disk_error(error));
    let mut bytes = 0;
    let mut block = vec![0; control.chunk_bytes(64 * 1024)];
    let mut plaintext = decryption
        .as_ref()
        .map(|_| vec![0; block.len() + AES_128.block_len()]);
    loop {
        control.check().map_err(Failure::fatal)?;
        let length = response
            .read(&mut block)
            .map_err(|_| Failure::retry("The video segment transfer was interrupted."))?;
        if length == 0 {
            break;
        }
        bytes += length as u64;
        if expected.is_some_and(|expected| bytes > expected) {
            return Err(Failure::retry(
                "The server returned more bytes than expected for a video segment.",
            ));
        }
        control.account_network(length).map_err(Failure::fatal)?;
        if let (Some(decoder), Some(plaintext)) = (&mut decryption, &mut plaintext) {
            let output = decoder
                .update(&block[..length], plaintext)
                .map_err(|_| Failure::fatal("Cannot decrypt the HLS segment."))?;
            file.write_all(output.written()).map_err(disk)?;
        } else {
            file.write_all(&block[..length]).map_err(disk)?;
        }
        control.commit_bytes(length as u64);
        *committed += length as u64;
    }
    if bytes == 0 || expected.is_some_and(|length| length != bytes) {
        return Err(Failure::retry(
            "The server returned an incomplete video segment.",
        ));
    }
    if let (Some(decoder), Some(plaintext)) = (decryption, plaintext.as_mut()) {
        let output = decoder.finish(plaintext).map_err(|_| {
            Failure::fatal(
                "The HLS segment could not be decrypted. The key or segment may be invalid.",
            )
        })?;
        file.write_all(output.written()).map_err(disk)?;
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
    let (status, report) = std::thread::scope(|scope| {
        let report = scope.spawn(|| {
            let mut text = Vec::new();
            diagnostics.read_to_end(&mut text).map(|_| text)
        });
        let status = transfer::wait_for_child(&mut child, control);
        if status.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        (status, report.join().expect("diagnostic worker"))
    });
    control.check()?;
    let status = status.map_err(disk_error)?;
    if !status.success() {
        let report = report.map_err(disk_error)?;
        let text = String::from_utf8_lossy(&report);
        let last = text.lines().rev().find(|line| !line.trim().is_empty());
        return Err(match last {
            Some(line) => format!(
                "FFmpeg could not assemble the video: {}",
                line.trim().chars().take(300).collect::<String>()
            ),
            None => "FFmpeg could not assemble the video.".into(),
        });
    }
    Ok(())
}

fn verify_output(path: &Path, expected: Option<f64>) -> Result<(), String> {
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
    if !output.status.success() || !video {
        return Err("The completed video is unreadable or has no video track.".into());
    }
    if let Some(expected) = expected {
        if !length.is_some_and(|length| close_duration(expected, length)) {
            return Err("The completed video is shorter or longer than the source reports.".into());
        }
    }
    Ok(())
}

/// Timestamps in real streams drift from the advertised durations, so allow a small margin.
fn close_duration(expected: f64, actual: f64) -> bool {
    actual.is_finite() && (expected - actual).abs() <= (expected * 0.02).max(2.0)
}

fn disk_error(error: io::Error) -> String {
    format!("Cannot save or process the temporary video: {error}")
}
