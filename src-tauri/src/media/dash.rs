use super::{copy_segment, disk_error, iso_duration, quality_rank, source_url, MAX_SEGMENTS};
use reqwest::blocking::Client;
use roxmltree::Node;
use std::{fs::File, path::Path, process::Command, sync::atomic::AtomicBool};
use url::Url;

type Xml<'a, 'input> = Node<'a, 'input>;

pub fn prepare(
    client: &Client,
    directory: &Path,
    base: &Url,
    manifest: &str,
    ffmpeg: &mut Command,
    max_height: Option<u32>,
    paused: &AtomicBool,
) -> Result<f64, String> {
    let xml = roxmltree::Document::parse(manifest)
        .map_err(|_| "The URL did not return a valid DASH manifest.")?;
    let root = xml.root_element();
    if !root.has_tag_name("MPD") || !matches!(root.attribute("type"), None | Some("static")) {
        return Err("Choose a finished DASH video. Live streams are currently unsupported.".into());
    }
    if root
        .descendants()
        .any(|node| node.has_tag_name("ContentProtection"))
    {
        return Err("This DASH stream uses content protection.".into());
    }
    let mut periods = root.children().filter(|node| node.has_tag_name("Period"));
    let period = periods
        .next()
        .ok_or("The DASH manifest is missing its period.")?;
    if periods.next().is_some() {
        return Err("Choose a DASH manifest with one continuous period.".into());
    }
    let duration = period
        .attribute("duration")
        .or_else(|| root.attribute("mediaPresentationDuration"))
        .and_then(iso_duration)
        .ok_or("The DASH manifest is missing a supported duration.")?;
    if period
        .attribute("start")
        .is_some_and(|start| !matches!(start, "PT0S" | "PT0.0S"))
    {
        return Err("The DASH period has an unsupported start time.".into());
    }

    let mut video = None;
    let mut audio = None;
    for adaptation in period
        .children()
        .filter(|node| node.has_tag_name("AdaptationSet"))
    {
        for representation in adaptation
            .children()
            .filter(|node| node.has_tag_name("Representation"))
        {
            let mime = representation
                .attribute("mimeType")
                .or_else(|| adaptation.attribute("mimeType"));
            let bandwidth = representation
                .attribute("bandwidth")
                .and_then(|bitrate| bitrate.parse::<u64>().ok())
                .unwrap_or(0);
            match mime {
                Some("video/mp4") => {
                    let height = representation
                        .attribute("height")
                        .and_then(|height| height.parse::<u32>().ok())
                        .unwrap_or(0);
                    let rank = quality_rank(height, bandwidth, max_height);
                    if video
                        .as_ref()
                        .is_none_or(|(previous, _, _)| rank > *previous)
                    {
                        video = Some((rank, adaptation, representation));
                    }
                }
                Some("audio/mp4") => {
                    if audio
                        .as_ref()
                        .is_none_or(|(previous, _, _)| bandwidth > *previous)
                    {
                        audio = Some((bandwidth, adaptation, representation));
                    }
                }
                _ => {}
            }
        }
    }
    let Some((_, set, representation)) = video else {
        return Err("This DASH manifest has no supported MP4 video track.".into());
    };
    let video = directory.join("dash-video.mp4");
    stage_track(
        client,
        base,
        [root, period, set, representation],
        duration,
        &video,
        paused,
    )?;
    ffmpeg
        .args(["-protocol_whitelist", "file", "-f", "mov", "-i"])
        .arg(video);
    if let Some((_, set, representation)) = audio {
        let audio = directory.join("dash-audio.mp4");
        stage_track(
            client,
            base,
            [root, period, set, representation],
            duration,
            &audio,
            paused,
        )?;
        ffmpeg
            .args(["-protocol_whitelist", "file", "-f", "mov", "-i"])
            .arg(audio);
        ffmpeg.args(["-map", "0:v:0", "-map", "1:a:0"]);
    }
    Ok(duration)
}

fn stage_track<'a, 'input>(
    client: &Client,
    base: &Url,
    nodes: [Xml<'a, 'input>; 4],
    duration: f64,
    destination: &Path,
    paused: &AtomicBool,
) -> Result<(), String> {
    let mut base = base.clone();
    for node in nodes {
        if let Some(uri) = child(node, "BaseURL").and_then(|node| node.text()) {
            base = source_url(&base, uri.trim())?;
        }
        if child(node, "SegmentList").is_some() || child(node, "SegmentBase").is_some() {
            return Err("This DASH stream uses an unsupported segment layout.".into());
        }
    }

    let templates: Vec<_> = nodes
        .into_iter()
        .rev()
        .filter_map(|node| child(node, "SegmentTemplate"))
        .collect();
    let attribute = |name: &str| templates.iter().find_map(|node| node.attribute(name));
    let initialization =
        attribute("initialization").ok_or("The DASH track lacks an initialization template.")?;
    let segment = attribute("media").ok_or("The DASH track lacks a media template.")?;
    let timescale = integer_attribute(attribute("timescale"), 1)?;
    if timescale == 0 {
        return Err("The DASH track has an invalid timescale.".into());
    }
    let first_number = integer_attribute(attribute("startNumber"), 1)?;
    let offset = integer_attribute(attribute("presentationTimeOffset"), 0)?;
    let total_ticks = (duration * timescale as f64).ceil();
    if total_ticks >= u64::MAX as f64 {
        return Err("The DASH track duration is outside the supported range.".into());
    }
    let period_end = offset
        .checked_add(total_ticks as u64)
        .ok_or("The DASH track is too long.")?;
    let timeline = templates
        .iter()
        .find_map(|node| child(*node, "SegmentTimeline"));
    let timestamps = match timeline {
        Some(timeline) => timeline_times(timeline, period_end)?,
        None => fixed_times(
            integer_attribute(attribute("duration"), 0)?,
            offset,
            period_end,
        )?,
    };

    let representation = nodes[3];
    let id = representation.attribute("id").unwrap_or("");
    let bandwidth = representation.attribute("bandwidth").unwrap_or("");
    let mut output = File::create(destination).map_err(disk_error)?;
    let initial_name = substitute(initialization, id, bandwidth, first_number, 0)?;
    copy_segment(
        client,
        &source_url(&base, &initial_name)?,
        &mut output,
        paused,
    )?;
    for (index, time) in timestamps.into_iter().enumerate() {
        let number = first_number
            .checked_add(index as u64)
            .ok_or("DASH segment numbering overflow.")?;
        let name = substitute(segment, id, bandwidth, number, time)?;
        copy_segment(client, &source_url(&base, &name)?, &mut output, paused)?;
    }
    Ok(())
}

fn child<'a, 'input>(node: Xml<'a, 'input>, name: &str) -> Option<Xml<'a, 'input>> {
    node.children().find(|node| node.has_tag_name(name))
}

fn integer_attribute(value: Option<&str>, fallback: u64) -> Result<u64, String> {
    value.map_or(Ok(fallback), |number| {
        number
            .parse()
            .map_err(|_| "Invalid DASH segment metadata.".into())
    })
}

fn timeline_times(timeline: Xml<'_, '_>, period_end: u64) -> Result<Vec<u64>, String> {
    let mut entries = timeline
        .children()
        .filter(|node| node.has_tag_name("S"))
        .peekable();
    let mut times = Vec::new();
    let mut time = 0;
    while let Some(entry) = entries.next() {
        time = integer_attribute(entry.attribute("t"), time)?;
        let duration = integer_attribute(entry.attribute("d"), 0)?;
        if duration == 0 {
            return Err("A DASH segment has no duration.".into());
        }
        let repetitions = entry
            .attribute("r")
            .unwrap_or("0")
            .parse::<i64>()
            .map_err(|_| "Invalid DASH segment repetition.")?;
        let count = match repetitions {
            -1 => {
                let end = match entries.peek() {
                    Some(next) => integer_attribute(
                        Some(
                            next.attribute("t")
                                .ok_or("The DASH timeline has an ambiguous repetition.")?,
                        ),
                        0,
                    )?,
                    None => period_end,
                };
                end.checked_sub(time)
                    .filter(|remaining| *remaining > 0)
                    .ok_or("Invalid DASH segment timeline.")?
                    .div_ceil(duration)
            }
            0..=i64::MAX => repetitions as u64 + 1,
            _ => return Err("Invalid DASH segment repetition.".into()),
        };
        add_times(&mut times, &mut time, duration, count)?;
    }
    if times.is_empty() {
        return Err("The DASH track has no media segments.".into());
    }
    Ok(times)
}

fn fixed_times(segment_duration: u64, offset: u64, end: u64) -> Result<Vec<u64>, String> {
    if segment_duration == 0 || end <= offset {
        return Err("The DASH track has no supported segment duration.".into());
    }
    let mut times = Vec::new();
    add_times(
        &mut times,
        &mut 0,
        segment_duration,
        (end - offset).div_ceil(segment_duration),
    )?;
    Ok(times)
}

fn add_times(
    times: &mut Vec<u64>,
    time: &mut u64,
    duration: u64,
    count: u64,
) -> Result<(), String> {
    if count > (MAX_SEGMENTS - times.len()) as u64 {
        return Err("The DASH track has more segments than supported.".into());
    }
    for _ in 0..count {
        times.push(*time);
        *time = time
            .checked_add(duration)
            .ok_or("Invalid DASH segment timeline.")?;
    }
    Ok(())
}

fn substitute(
    mut pattern: &str,
    id: &str,
    bandwidth: &str,
    number: u64,
    time: u64,
) -> Result<String, String> {
    let mut result = String::new();
    while let Some((prefix, tail)) = pattern.split_once('$') {
        result.push_str(prefix);
        let (identifier, rest) = tail
            .split_once('$')
            .ok_or("The DASH template is incomplete.")?;
        let (field, width) = match identifier.split_once('%') {
            Some((field, format)) => {
                let width = format
                    .strip_prefix('0')
                    .and_then(|format| format.strip_suffix('d'))
                    .and_then(|width| width.parse::<usize>().ok())
                    .filter(|width| *width <= 20)
                    .ok_or("Unsupported DASH template number format.")?;
                (field, width)
            }
            None => (identifier, 0),
        };
        match field {
            "" if width == 0 => result.push('$'),
            "RepresentationID" if width == 0 && !id.is_empty() => result.push_str(id),
            "Bandwidth" if !bandwidth.is_empty() => {
                result.push_str(&format!("{bandwidth:0>width$}"))
            }
            "Number" => result.push_str(&format!("{number:0width$}")),
            "Time" => result.push_str(&format!("{time:0width$}")),
            _ => return Err("Unsupported DASH template variable.".into()),
        }
        pattern = rest;
    }
    result.push_str(pattern);
    Ok(result)
}
