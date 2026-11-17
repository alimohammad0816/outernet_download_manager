//! Downloads video from a web page or a stream URL with yt-dlp, which knows how to read
//! thousands of sites and handles HLS/DASH variations the built-in readers reject.

use super::{cookie_domain, cookie_pairs, disk_error, engine_path, media_command, Headers};
use crate::{progress::TransferControl, transfer};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};
use url::Url;

const OUTPUT_STEM: &str = "site";
const MEDIA_EXTENSIONS: [&str; 10] = [
    "mp4", "mkv", "webm", "mov", "m4v", "ts", "flv", "3gp", "ogv", "avi",
];

pub(super) fn prepare(
    url: &Url,
    directory: &Path,
    ffmpeg: &mut Command,
    max_height: Option<u32>,
    headers: &Headers,
    control: &TransferControl,
) -> Result<Option<f64>, String> {
    control.check()?;
    // Best quality up to the cap, otherwise the smallest one above it.
    let format = match max_height {
        Some(height) => format!("bv*[height<=?{height}]+ba/b[height<=?{height}]/wv*+ba/w"),
        None => "bv*+ba/b".into(),
    };
    let stderr = tempfile::tempfile_in(directory).map_err(disk_error)?;
    let mut command = media_command("yt-dlp");
    command
        .current_dir(directory)
        .args([
            "--ignore-config",
            "--no-plugin-dirs",
            "--no-playlist",
            "--no-simulate",
            "--newline",
            "--progress",
            "--progress-template",
            "download:outernet-progress %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s",
            "--print",
            "before_dl:outernet-duration %(duration)s",
            "--no-allow-unplayable-formats",
            "--abort-on-unavailable-fragments",
            "--match-filters",
            "!is_live",
            "--concurrent-fragments",
            "8",
            "--retries",
            "10",
            "--fragment-retries",
            "10",
            "--socket-timeout",
            "20",
            "--merge-output-format",
            "mkv",
            "--format",
            &format,
            "--output",
        ])
        .arg(format!("{OUTPUT_STEM}.%(ext)s"))
        .stdout(Stdio::piped())
        .stderr(stderr.try_clone().map_err(disk_error)?);
    let ffmpeg_path = engine_path("ffmpeg");
    if ffmpeg_path.is_absolute() {
        command.arg("--ffmpeg-location").arg(ffmpeg_path);
    }
    let node = engine_path("node");
    command.args([
        "--js-runtimes",
        &if node.is_absolute() {
            format!("node:{}", node.display())
        } else {
            "node".into()
        },
    ]);
    for (name, label) in [
        ("user-agent", "User-Agent"),
        ("referer", "Referer"),
        ("origin", "Origin"),
    ] {
        if let Some(value) = headers.get(name) {
            command.arg("--add-header").arg(format!("{label}:{value}"));
        }
    }
    if let Some(cookies) = write_cookies(directory, url, headers)? {
        command.arg("--cookies").arg(cookies);
    }
    control.proxy().configure_command(&mut command);
    if let Some(binding) = control.binding() {
        command
            .arg("--source-address")
            .arg(binding.address.to_string());
    }
    if let Some(limit) = control.speed_limit_bytes() {
        command.arg("--limit-rate").arg(limit.to_string());
    }
    let mut child = command
        .arg("--")
        .arg(url.as_str())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "The site video engine is missing. Repair or reinstall Outernet.".to_string()
            } else {
                format!("Cannot start yt-dlp: {error}")
            }
        })?;
    let output = child.stdout.take().expect("piped stdout");
    let (status, duration) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| read_progress(output, control));
        let status = transfer::wait_for_child(&mut child, control);
        if status.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        (status, reader.join().expect("progress reader"))
    });
    control.check()?;
    if !status.map_err(disk_error)?.success() {
        return Err(engine_error(stderr));
    }
    let track = find_output(directory)?.ok_or(
        "No downloadable video was found. The video may be live, private, or DRM-protected.",
    )?;
    ffmpeg
        .args(["-protocol_whitelist", "file", "-i"])
        .arg(track);
    Ok(duration)
}

/// Writes the browser's cookies in the Netscape format yt-dlp reads.
fn write_cookies(
    directory: &Path,
    url: &Url,
    headers: &Headers,
) -> Result<Option<PathBuf>, String> {
    let Some(domain) = cookie_domain(url) else {
        return Ok(None);
    };
    let www = url.host_str().is_some_and(|host| host.starts_with("www."));
    let (scope, subdomains) = if www {
        (format!(".{domain}"), "TRUE")
    } else {
        (domain, "FALSE")
    };
    let expires = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |now| now.as_secs())
        + 86_400;
    let mut text = String::from("# Netscape HTTP Cookie File\n");
    for (name, value) in cookie_pairs(headers) {
        if [name, value]
            .iter()
            .any(|part| part.contains(['\t', '\n', '\r']))
        {
            continue;
        }
        text.push_str(&format!(
            "{scope}\t{subdomains}\t/\tFALSE\t{expires}\t{name}\t{value}\n"
        ));
    }
    if !text.contains('\t') {
        return Ok(None);
    }
    let path = directory.join("cookies.txt");
    File::create(&path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(disk_error)?;
    Ok(Some(path))
}

fn read_progress(output: impl Read, control: &TransferControl) -> Option<f64> {
    let mut duration = None;
    // Bytes of earlier files (video before audio) and of the current file so far.
    let mut finished = 0_u64;
    let mut current = 0_u64;
    for line in BufReader::new(output).lines().map_while(Result::ok) {
        if let Some(value) = line.strip_prefix("outernet-duration ") {
            duration = value
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value > 0.0);
        } else if let Some(values) = line.strip_prefix("outernet-progress ") {
            let mut fields = values
                .split_whitespace()
                .map(|value| value.parse::<f64>().ok().map(|value| value as u64));
            let Some(Some(downloaded)) = fields.next() else {
                continue;
            };
            let total = fields.next().flatten().or(fields.next().flatten());
            if downloaded < current {
                finished += current;
                current = 0;
            }
            control.commit_bytes(downloaded - current);
            control.note_received(downloaded - current);
            current = downloaded;
            if let Some(total) = total.filter(|total| *total >= downloaded) {
                control.update_total(Some(finished + total));
            }
        }
    }
    duration
}

fn find_output(directory: &Path) -> Result<Option<PathBuf>, String> {
    let mut best: Option<(PathBuf, u64)> = None;
    for entry in fs::read_dir(directory).map_err(disk_error)? {
        let entry = entry.map_err(disk_error)?;
        let name = entry.file_name();
        let Some(extension) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&format!("{OUTPUT_STEM}.")))
        else {
            continue;
        };
        if !MEDIA_EXTENSIONS.contains(&extension)
            || !entry.file_type().map_err(disk_error)?.is_file()
        {
            continue;
        }
        let bytes = entry.metadata().map_err(disk_error)?.len();
        if bytes > 0 && best.as_ref().is_none_or(|(_, size)| bytes > *size) {
            best = Some((entry.path(), bytes));
        }
    }
    Ok(best.map(|(path, _)| path))
}

fn engine_error(mut stderr: File) -> String {
    let mut text = String::new();
    let _ = stderr.seek(SeekFrom::Start(0));
    let _ = stderr.take(256 * 1024).read_to_string(&mut text);
    let lower = text.to_ascii_lowercase();
    if lower.contains("drm") {
        return super::DRM_ERROR.into();
    }
    match text
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("ERROR:"))
    {
        Some(reason) => format!(
            "The site video engine could not download this video: {}",
            reason.trim().chars().take(300).collect::<String>()
        ),
        None => "The site video engine could not download this video.".into(),
    }
}
