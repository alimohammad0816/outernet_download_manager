use super::{
    copy_segment, disk_error, hls_input, hls_lines, read_manifest, select_hls, source_url,
    MAX_SEGMENTS,
};
use crate::progress::TransferControl;
use reqwest::blocking::Client;
use std::{
    fs::{self, File},
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use url::Url;

struct Segment {
    url: Url,
    seconds: f64,
    discontinuity: bool,
}

struct Playlist {
    first: u64,
    target: u64,
    ended: bool,
    segments: Vec<Segment>,
}

impl Playlist {
    fn parse(base: &Url, manifest: &str) -> Result<Self, String> {
        let lines = hls_lines(manifest)?;
        let mut first = None;
        let mut target = None;
        let mut ended = false;
        let mut segments = Vec::new();
        let mut duration = None;
        let mut discontinuity = false;
        for line in lines {
            if line == "#EXT-X-ENDLIST" {
                ended = true;
            } else if line == "#EXT-X-DISCONTINUITY" {
                discontinuity = true;
            } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                if !segments.is_empty() || first.is_some() {
                    return Err("The live HLS sequence tag is misplaced or duplicated.".into());
                }
                first = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| "Invalid live HLS sequence.")?,
                );
            } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
                target = value
                    .parse::<u64>()
                    .ok()
                    .filter(|value| (1..=86_400).contains(value));
            } else if let Some(value) = line.strip_prefix("#EXTINF:") {
                if duration.is_some() {
                    return Err("A live HLS segment is missing its URL.".into());
                }
                duration = Some(
                    value
                        .split(',')
                        .next()
                        .and_then(|value| value.parse::<f64>().ok())
                        .filter(|seconds| *seconds > 0.0 && *seconds < 86_400.0)
                        .ok_or("A live HLS segment has an invalid duration.")?,
                );
            } else if [
                "#EXT-X-MAP:",
                "#EXT-X-KEY:",
                "#EXT-X-BYTERANGE:",
                "#EXT-X-SKIP:",
                "#EXT-X-GAP",
                "#EXT-X-STREAM-INF:",
            ]
            .iter()
            .any(|tag| line.starts_with(tag))
            {
                return Err(
                    "This live HLS playlist uses an unsupported layout or has missing segments."
                        .into(),
                );
            } else if !line.starts_with('#') {
                segments.push(Segment {
                    url: source_url(base, line)?,
                    seconds: duration
                        .take()
                        .ok_or("A live HLS segment is missing its duration.")?,
                    discontinuity: std::mem::take(&mut discontinuity),
                });
                if segments.len() > MAX_SEGMENTS {
                    return Err("The live HLS window has too many segments.".into());
                }
            }
        }
        if duration.is_some() {
            return Err("The live HLS playlist ends in an incomplete segment.".into());
        }
        let first = first.unwrap_or(0);
        first
            .checked_add(segments.len() as u64)
            .ok_or("The live HLS sequence overflows.")?;
        Ok(Self {
            first,
            target: target.ok_or("The live HLS playlist has no valid target duration.")?,
            ended,
            segments,
        })
    }
}

pub(super) fn record(
    client: &Client,
    directory: &Path,
    base: Url,
    manifest: String,
    ffmpeg: &mut Command,
    max_height: Option<u32>,
    control: &TransferControl,
) -> Result<f64, String> {
    let selected = select_hls(&base, &hls_lines(&manifest)?, max_height)?;
    if selected.audio.is_some() {
        return Err(
            "Live recording currently requires a stream with audio and video together.".into(),
        );
    }
    let (url, mut snapshot) = match selected.video {
        Some(url) => read_manifest(client, &url)?,
        None => (base, manifest),
    };
    let mut next = None;
    let mut local = String::new();
    let mut recorded = Vec::<(Url, f64, bool)>::new();
    let mut start_sequence = None;
    let mut duration = 0.0_f64;
    let mut target = 1_u64;
    let mut unavailable_since = None;

    loop {
        control.check()?;
        let playlist = Playlist::parse(&url, &snapshot)?;
        let start_sequence = *start_sequence.get_or_insert(playlist.first);
        let next_needed = *next.get_or_insert(playlist.first);
        if playlist.first > next_needed && !control.recording_finished() {
            return Err("Live recording lost one or more segments from the server window.".into());
        }
        if playlist.ended && recorded.is_empty() {
            return Err("This stream has ended. Use Download for the finished video.".into());
        }
        target = target.max(playlist.target);
        let mut segment_failed = false;
        for (index, segment) in playlist.segments.into_iter().enumerate() {
            let number = playlist.first + index as u64;
            let next_number = next.expect("recording sequence is initialized");
            if control.recording_finished() {
                break;
            }
            if number < next_number {
                if let Some(previous) = number
                    .checked_sub(start_sequence)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| recorded.get(index))
                {
                    if *previous != (segment.url, segment.seconds, segment.discontinuity) {
                        return Err("The live server changed a previously recorded segment.".into());
                    }
                }
                continue;
            }
            if recorded.len() == MAX_SEGMENTS {
                return Err("The live recording reached its segment limit.".into());
            }
            let name = format!("live-{}.ts", recorded.len());
            let path = directory.join(&name);
            let mut output = File::create(&path).map_err(disk_error)?;
            let previous_bytes = control.progress().completed_bytes;
            if let Err(error) = copy_segment(client, &segment.url, &mut output, control) {
                control.discard_bytes(
                    control
                        .progress()
                        .completed_bytes
                        .saturating_sub(previous_bytes),
                );
                drop(output);
                fs::remove_file(path).map_err(disk_error)?;
                control.check()?;
                if error.starts_with("Cannot save") {
                    return Err(error);
                }
                segment_failed = true;
                break;
            }
            if segment.discontinuity {
                local.push_str("#EXT-X-DISCONTINUITY\n");
            }
            local.push_str(&format!("#EXTINF:{},\n{name}\n", segment.seconds));
            duration += segment.seconds;
            recorded.push((segment.url, segment.seconds, segment.discontinuity));
            next = Some(number + 1);
            unavailable_since = None;
        }
        if control.recording_finished() || (playlist.ended && !segment_failed) {
            break;
        }
        let refresh = Duration::from_millis((playlist.target * 500).clamp(250, 5000));
        loop {
            let start = Instant::now();
            while start.elapsed() < refresh {
                control.check()?;
                if control.recording_finished() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if control.recording_finished() {
                break;
            }
            if let Ok((resolved, latest)) = read_manifest(client, &url) {
                if resolved != url {
                    return Err(
                        "The live playlist redirected to a different source during recording."
                            .into(),
                    );
                }
                snapshot = latest;
                break;
            }
            let since = unavailable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(60) {
                return Err("The live playlist was unavailable for too long.".into());
            }
        }
        if control.recording_finished() {
            break;
        }
        if segment_failed {
            let since = unavailable_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(60) {
                return Err("A live HLS segment could not be recovered.".into());
            }
        }
    }
    if recorded.is_empty() || duration <= 0.0 {
        return Err("The recording ended before any complete video segment was saved.".into());
    }
    let list = directory.join("live.m3u8");
    fs::write(
        &list,
        format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{target}\n{local}#EXT-X-ENDLIST\n"
        ),
    )
    .map_err(disk_error)?;
    hls_input(ffmpeg, &list);
    Ok(duration)
}
