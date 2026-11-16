use super::{disk_error, media_command};
use crate::{progress::TransferControl, transfer};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};
use url::Url;

pub(super) fn video_id(url: &Url) -> Option<String> {
    let id = match (url.host_str()?, url.path()) {
        ("youtube.com" | "www.youtube.com" | "m.youtube.com", "/watch") => url
            .query_pairs()
            .find(|(key, _)| key == "v")?
            .1
            .into_owned(),
        ("youtube.com" | "www.youtube.com" | "m.youtube.com", path) => path
            .strip_prefix("/shorts/")?
            .trim_end_matches('/')
            .to_owned(),
        ("youtu.be", path) => path.strip_prefix('/')?.trim_end_matches('/').to_owned(),
        _ => return None,
    };
    (id.len() == 11
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)))
    .then_some(id)
}

pub(super) fn prepare(
    url: &Url,
    directory: &Path,
    ffmpeg: &mut Command,
    max_height: Option<u32>,
    control: &TransferControl,
) -> Result<f64, String> {
    let id = video_id(url).ok_or("Choose a YouTube video or Shorts link.")?;
    let url = format!("https://www.youtube.com/watch?v={id}");
    let video_format = match max_height {
        Some(height) => format!("bv*[protocol=https][height<={height}]"),
        None => "bv*[protocol=https]".into(),
    };
    let (video, duration) = download_track(&url, directory, "video", &video_format, control)?;
    let (audio, audio_duration) = download_track(
        &url,
        directory,
        "audio",
        "ba[protocol=https][ext=m4a]/ba[protocol=https]",
        control,
    )?;
    if !super::close_duration(duration, audio_duration) {
        return Err("YouTube returned audio and video with different durations.".into());
    }
    ffmpeg
        .args(["-protocol_whitelist", "file", "-i"])
        .arg(video)
        .args(["-protocol_whitelist", "file", "-i"])
        .arg(audio)
        .args(["-map", "0:v:0", "-map", "1:a:0"]);
    Ok(duration)
}

fn download_track(
    url: &str,
    directory: &Path,
    name: &str,
    format: &str,
    control: &TransferControl,
) -> Result<(PathBuf, f64), String> {
    control.check()?;
    let metadata = directory.join(format!("{name}.duration"));
    let stderr = tempfile::tempfile_in(directory).map_err(disk_error)?;
    let mut command = media_command("yt-dlp");
    command
        .current_dir(directory)
        .args([
            "--ignore-config",
            "--no-plugin-dirs",
            "--no-playlist",
            "--no-simulate",
            "--no-progress",
            "--no-allow-unplayable-formats",
            "--match-filters",
            "!is_live",
            "--socket-timeout",
            "20",
            "--retries",
            "3",
            "--js-runtimes",
            "node",
            "--print",
            "before_dl:duration",
            "--format",
            format,
            "--output",
        ])
        .arg(format!("{name}.%(ext)s"))
        .stdout(File::create(&metadata).map_err(disk_error)?)
        .stderr(stderr.try_clone().map_err(disk_error)?);
    if let Some(limit) = control.speed_limit_bytes() {
        command.arg("--limit-rate").arg(limit.to_string());
    }
    let mut child = command.arg("--").arg(url).spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Install yt-dlp and make it available on PATH to download YouTube videos.".to_string()
        } else {
            format!("Cannot start yt-dlp: {error}")
        }
    })?;
    let status = transfer::wait_for_child(&mut child, control);
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    control.check()?;
    if !status.map_err(disk_error)?.success() {
        return Err("YouTube did not provide a downloadable track. Check your connection and yt-dlp version.".into());
    }
    let mut metadata_text = String::new();
    File::open(metadata)
        .map_err(disk_error)?
        .take(128)
        .read_to_string(&mut metadata_text)
        .map_err(disk_error)?;
    let duration = metadata_text
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or("YouTube did not provide a finished video with a verifiable duration.")?;
    let mut track = None;
    for entry in fs::read_dir(directory).map_err(disk_error)? {
        let entry = entry.map_err(disk_error)?;
        let filename = entry.file_name();
        if filename
            .to_str()
            .and_then(|filename| filename.strip_prefix(&format!("{name}.")))
            .is_some_and(|extension| {
                matches!(extension, "mp4" | "webm" | "m4a" | "3gp" | "mkv" | "opus")
            })
            && entry.file_type().map_err(disk_error)?.is_file()
        {
            let bytes = entry.metadata().map_err(disk_error)?.len();
            if bytes == 0 || track.is_some() {
                return Err("YouTube returned incomplete or ambiguous media data.".into());
            }
            track = Some((entry.path(), bytes));
        }
    }
    let (path, bytes) = track.ok_or("YouTube did not return a complete media track.")?;
    control.commit_bytes(bytes);
    Ok((path, duration))
}
