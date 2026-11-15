# Outernet Download Manager

Outernet downloads HTTP(S) files and supported on-demand video streams on Linux
and Windows. Its local queue, settings, and history use SQLite.

## Install and run

Get the x86-64 desktop installer and browser extension from the
[Desktop builds workflow](https://github.com/alimohammad0816/outernet_download_manager/actions/workflows/build.yml)
or the matching [GitHub Release](https://github.com/alimohammad0816/outernet_download_manager/releases).
The Linux `.deb` depends on FFmpeg. On Windows, install FFmpeg separately and
ensure `ffmpeg` and `ffprobe` are available on PATH
when launching Outernet.

For development, install Node.js 24, stable Rust, CMake, the
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/), and the engines
above.

```sh
npm ci
npm run tauri dev
```

## Connect a browser

1. For Chrome, Edge, or Chromium, extract `outernet-browser-extension.zip`,
   open the Extensions page, turn on Developer mode, and choose **Load unpacked**.
   When working from source, you can select the `browser/` folder directly.
2. For Firefox, extract `outernet-firefox-extension.zip`. Open
   `about:debugging#/runtime/this-firefox`, choose **Load Temporary Add-on**,
   and select the extracted `manifest.json`. Reload the temporary add-on after
   restarting Firefox.
3. In Outernet, open **Browser integration** and select your browser to
   register the desktop connection for your account. Open the extension popup
   to check the connection.

Right-click a link and choose **Download with Outernet**. On pages with a
supported video source, use **Download this video** beside the player. The
extension popup lists HLS and DASH manifests detected in the active tab and
lets you set a preferred resolution. Outernet picks the best quality up to
the preference where offered, or the lowest known resolution above it.

For a direct file behind a sign-in, use **Download with Outernet using site
session**. The browser makes the authenticated request and streams the body
to the configured in-progress folder through Native Messaging. Site cookies
stay in the browser. This mode requires a full response with a known byte
length and an unencoded body. It uses the browser's regular profile session
and supports direct files. Websites that issue a POST, require a page-issued
authorization header, or serve protected segmented media still use the
browser's normal download flow.

Enable **Automatically take over eligible downloads** in the extension popup
to transfer matching browser downloads. This covers attachment GETs of at
least 1 MiB with an allowed extension, a strong ETag, and matching length and
media type. Outernet validates the second response before pausing the original,
and the extension cancels the original only after the desktop confirms that
the complete body is durably queued. If the handoff fails, the extension
resumes the original when the browser can resume it.

Video support covers finished unencrypted HLS playlists with discrete
segments and static single-period DASH with fragmented MP4 SegmentTemplates.
Outernet assembles these streams into Matroska (`.mkv`).

## Schedule the queue

Open **Queue schedule** to hold queued desktop transfers until a local date
and time. The schedule survives app restarts. Keep Outernet running for the
queue to start at the selected time, or choose **Run queue now** to release it
early. Active transfers continue, and browser-session handoffs already in
progress can be saved while the desktop queue is waiting.

## Storage and integrity

Choose separate **In-progress folder** and **Completed downloads folder** paths
in **Settings**. Place the first folder on an HDD to keep transfer data and
video assembly on that disk. An SSD selected as the final destination receives
the completed file once. Folder changes apply to new downloads.

Outernet requests up to four ranges in parallel when a large file advertises
byte ranges and a strong ETag. Every range must match the requested byte
positions and the same strong ETag. After pause or interruption, Outernet
checks the source identity and the SHA-256 of each saved part before reusing
it. If the resource changes or declines the range request, the app starts a
fresh single-connection transfer. Single-connection transfers and video
assembly restart from the beginning after interruption.

An optional publisher SHA-256 can be entered when adding a direct link.
Outernet compares the result with that reference before publication. For
video, it requires all listed segments, checks output duration, and decodes
the assembled audio and video before publishing the file. It verifies copied
bytes before publishing across filesystems. If publication fails after a
successful transfer, **Retry** reuses the verified working file.

## Tests and builds

```sh
npm run build
npm run check:extension
npm run test:extension
cargo test --locked --manifest-path src-tauri/Cargo.toml
cargo test --release --locked --manifest-path src-tauri/Cargo.toml --test download_integrity downloads_over_four_gib -- --ignored --exact
```

The tests use loopback fixtures and require FFmpeg and ffprobe. To test a
cross-filesystem copy of the large fixture, set `OUTERNET_TEST_DESTINATION` to
a directory on a different volume from the system temporary directory. Both
volumes need more than 4 GiB free.

GitHub Actions builds x86-64 installers on Ubuntu 24.04 and Windows Server
2025 and packages both browser extensions. Pushing a version tag that matches
`src-tauri/tauri.conf.json` runs the large-file test before publishing a GitHub Release with the two
installers, the two browser extensions, and a SHA-256 checksum file.
