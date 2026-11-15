# Outernet Download Manager

Outernet is a Tauri desktop download manager for Linux and Windows. The first
development milestone handles direct HTTP and HTTPS links, a persistent queue,
and separate in-progress and completed download folders.

## Run locally

Install Node.js 24, stable Rust, the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/),
and [aria2](https://aria2.github.io/). Ensure `aria2c --version` works on your PATH.
On Windows that also applies when launching the installed app from Explorer.
The Linux `.deb` declares aria2 as a package dependency.

```sh
npm ci
npm run tauri dev
```

Before adding a download, open **Settings** to select the in-progress and
completed folders. Place the in-progress folder on an HDD to keep the downloaded
payload off your SSD during transfer. Selecting an SSD as the final destination
writes the completed file to that disk. Folder changes apply to new downloads.

Paste a direct URL into **Add download**. If the publisher provides a SHA-256,
enter it in the same dialog. Outernet compares the received file with that
reference before saving it as completed. When copying across filesystems,
Outernet verifies the staged destination copy before publishing it.

This milestone uses a single connection. An interrupted network transfer starts
over on retry. If the transfer finished and saving to the final destination
failed, retry uses the verified working file. Browser integration, segmented
video, and safe resume are upcoming milestones.

## Tests and builds

```sh
npm run build
cargo test --locked --manifest-path src-tauri/Cargo.toml --test download_integrity
cargo test --release --locked --manifest-path src-tauri/Cargo.toml --test download_integrity downloads_over_four_gib -- --ignored --exact
```

The tests serve real data to aria2 over loopback. To exercise a copy between
filesystems in the large test, set `OUTERNET_TEST_DESTINATION` to an existing
directory on a different volume from the system temporary directory. Each volume
needs more than 4 GiB of free space.

GitHub Actions builds x86-64 Linux and Windows installers on Ubuntu 24.04 and
Windows Server 2025. Pushing a
`v*` tag matching the version in `src-tauri/tauri.conf.json` runs the large
test on both platforms before creating a draft release.
