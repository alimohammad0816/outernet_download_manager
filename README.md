# Outernet Download Manager

Outernet downloads HTTP(S) files and supported on-demand video streams on Linux
and Windows. Its local queue, settings, and history use SQLite.

## Screenshots

![Outernet desktop queue with an active, a queued, and a completed download](docs/screenshots/downloads.png)

![Browser extension connected to Outernet with stream quality and HLS download controls](docs/screenshots/browser-extension.png)

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
extension popup lists direct video responses and HLS/DASH manifests detected
in the active tab. For HLS and DASH, choose the quality for this download
beside the player, or set the default from the extension popup. Outernet
picks the best quality up to the preference where offered, or the lowest
known resolution above it.

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

Video support covers finished HLS with discrete segments or byte ranges and
static single-period DASH with fragmented MP4 SegmentTemplates. HLS byte
ranges require a strong ETag and matching range responses. Finished HLS also
supports standard AES-128 with identity keys served by the site, including
explicit IVs and key rotation. The AES-128 path supports complete segments.
DRM and SAMPLE-AES require a different playback path.

For a compatible live HLS stream, choose **Record live in Outernet** next to
the video or **Record live** in the popup. Use **Stop and save recording** in
the desktop queue to finalize the captured portion. Live recording supports
unencrypted, multiplexed streams with discrete MPEG-TS segments. It retries
temporary failures while the segments remain available and fails if the
server window advances past a missing segment. The output is Matroska
(`.mkv`).

## Manage downloads

In **Add download**, select **Multiple links** and paste one URL per line
(up to 500). Check the file preview, then queue the links. Outernet validates
the batch before committing it to the queue.
Existing and conflicting filenames receive a numbered suffix. Use **File type**
to filter downloads by filename extension.

Use **Replace link** on an incomplete failed or paused download. The queue
keeps its filename, destination, priority, and existing publisher checksum.
When the URL changes, Outernet reuses verified byte ranges only if the
publisher SHA-256 is known and the remote size and strong ETag match. The
complete file must still match the publisher checksum before publication.

## Schedule the queue

Open **Queue schedule** to hold the queue until a local date and time, allow
new transfers only during a daily time window, or combine both. A daily
window can cross midnight. Schedules survive app restarts. **Clear schedule**
releases the queue. Active transfers and publication of already received files
continue outside the window.

Set **Simultaneous downloads** in Settings. New jobs start in priority order,
with older jobs first among equal priorities. Lowering the limit lets active
downloads finish before new ones start. Change a queued, paused, or failed
download's priority in its row. The default is one simultaneous desktop
transfer.

Enable **Keep running in the system tray when closed** to let the queue progress
after closing the window. Open Outernet from the tray or launch it again to
restore the window. **Quit Outernet** in the tray exits the application.
**Notify me when a download completes** sends an OS notification once the
verified file has been published. The operating system controls notification
visibility, and Linux desktops need a system-tray host to display the tray icon.

## Storage and integrity

Choose separate **In-progress folder** and **Completed downloads folder** paths
in **Settings**. Place the first folder on an HDD to keep transfer data and
video assembly on that disk. An SSD selected as the final destination receives
the completed file once. If the in-progress folder is unavailable, reconnect
the disk and use **Retry**. Folder changes apply to new downloads.

Outernet requests up to four ranges in parallel when a large file advertises
byte ranges and a strong ETag. Every range must match the requested byte
positions and the same strong ETag. After pause or interruption, Outernet
checks the source identity and the SHA-256 of each saved part before reusing
it. If the resource changes or declines the range request, the app starts a
fresh single-connection transfer. Single-connection transfers and video
assembly restart from the beginning after interruption.

The list shows live progress, speed and an estimated remaining time for active
desktop transfers with a known size. Transfers without a known total still
show the received bytes and speed. In **Settings**, enter a speed limit per
download in KiB/s, or leave it blank for unlimited speed. The limit is read
when a transfer starts and shared by its parallel connections. Browser-assisted
transfers use the configured limit when the matching extension is installed.

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

## Signing and store distribution

Tagged builds can enable two independent Windows signatures through GitHub Actions.

| GitHub setting | Value |
| --- | --- |
| Repository variable `OUTERNET_UPDATER_PUBLIC_KEY` | Contents of the `.pub` file generated by the Tauri signer. |
| Secret `TAURI_SIGNING_PRIVATE_KEY` | Contents of the corresponding private key file. |
| Secret `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | The password for the private key, when set. |
| Secret `WINDOWS_CERTIFICATE` | Base64-encoded, exportable Authenticode PFX with a trusted code-signing chain. |
| Secret `WINDOWS_CERTIFICATE_PASSWORD` | Password for the PFX. |

Generate updater keys with `npm run tauri -- signer generate --write-keys /secure/path/outernet.key`.
Keep a secure backup of the private key outside the repository. Keep the same
updater key across releases so installed copies can verify future updates.
A configured updater build includes the public key and the GitHub release
endpoint. GitHub Actions verifies the installer signature before publishing
`latest.json`. The Windows app checks and installs signed updates from
Settings on request. Installation requires the download workers to be idle.
Install the first updater-enabled build manually. Debian users can continue
installing new `.deb` packages from GitHub Releases. See the
[Tauri updater setup](https://v2.tauri.app/plugin/updater/).

The Windows workflow imports the PFX into the runner's user certificate store
and verifies the timestamp and Authenticode signature on both the app executable
and the installer. A hardware-backed or remote signing provider requires its own
Tauri signing command and runner configuration.
[Windows signing in Tauri](https://v2.tauri.app/distribute/sign/windows/).

Upload `outernet-chromium-store.zip` as a draft in the Chrome Web Store and
Microsoft Edge Add-ons publisher dashboards. Once the stores assign IDs, set
`OUTERNET_CHROME_EXTENSION_ID` and `OUTERNET_EDGE_EXTENSION_ID` as repository
variables before building the desktop installers. In Outernet, connect the
browser again to refresh the native messaging registration. The standard
`outernet-browser-extension.zip` keeps its development extension ID.
[Chrome extension IDs](https://developer.chrome.com/docs/extensions/reference/manifest/key).

Submit `outernet-firefox-extension.zip` to Mozilla Add-ons (AMO) for signing.
Keep the existing Firefox ID when submitting future updates. The workflow builds
the ZIPs and leaves store submission to the publisher accounts.
