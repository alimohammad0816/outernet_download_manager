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

## Connect Chrome, Edge, or Chromium

1. Extract `outernet-browser-extension.zip`. When working from source, use the
   `browser/` directory directly.
2. Open the browser’s Extensions page, enable Developer mode, then select
   **Load unpacked** and point it at that directory.
3. In the installed Outernet application, open **Browser integration** and
   select the matching browser to register its native host for your account.
   Open the extension popup to check the connection.

Right-click a link and choose **Download with Outernet**. On pages with a
supported video source, use **Download this video** beside the player. The
extension popup also lists HLS and DASH manifests detected in the active tab
and lets you set a preferred resolution. Outernet picks the best quality up to
the preference where offered, or the lowest known resolution above it.

The extension submits the chosen HTTP(S) URL to the desktop app locally.
Current support covers accessible direct links, finished unencrypted HLS
playlists with discrete segments, and static single-period DASH with
fragmented MP4 SegmentTemplates. The video output uses Matroska (`.mkv`).
Site sessions, HLS byte-range layouts, live streams, and DRM-protected streams
are outside the current scope.

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
cargo test --locked --manifest-path src-tauri/Cargo.toml
cargo test --release --locked --manifest-path src-tauri/Cargo.toml --test download_integrity downloads_over_four_gib -- --ignored --exact
```

The tests use loopback fixtures and require FFmpeg and ffprobe. To test a
cross-filesystem copy of the large fixture, set `OUTERNET_TEST_DESTINATION` to
a directory on a different volume from the system temporary directory. Both
volumes need more than 4 GiB free.

GitHub Actions builds x86-64 installers on Ubuntu 24.04 and Windows Server
2025 and packages the extension. Pushing a version tag that matches
`src-tauri/tauri.conf.json` runs the large-file test before creating a draft
release.
