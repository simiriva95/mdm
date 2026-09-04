<h1 align="center">MDM — Mini Download Manager</h1>

<p align="center"><em>A single-binary Rust download manager for Windows that hijacks large Chrome downloads and pulls them over up to 16 parallel HTTP Range connections, with crash-proof resume.</em></p>

<p align="center">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-2021-000000?logo=rust&logoColor=white">
  <img alt="Tokio" src="https://img.shields.io/badge/Tokio-1.x-0B7261?logo=rust&logoColor=white">
  <img alt="egui" src="https://img.shields.io/badge/eframe%20%2F%20egui-0.29-4B32C3">
  <img alt="Windows" src="https://img.shields.io/badge/Windows-0078D6?logo=windows&logoColor=white">
  <img alt="Chrome MV3" src="https://img.shields.io/badge/Chrome-MV3%20extension-4285F4?logo=googlechrome&logoColor=white">
</p>

<!-- SCREENSHOT: hero shot of the MDM window with two active downloads, segment map and network sparkline visible, 1280px wide -->

Chrome downloads big files over a single connection and forgets everything if the browser or the machine dies. MDM replaces that path: a tiny MV3 extension cancels any download above a configurable size threshold and hands the URL — plus cookies, referer and the real Chrome user agent — to a native Rust app that downloads it segmented, resumable and rate-limited. One `mdm.exe`, no runtime, no service, no admin rights: it installs entirely under `%LOCALAPPDATA%`.

## Features

- **Segmented downloads** — up to 16 parallel HTTP `Range` connections per file (8 by default), with a live torrent-style segment map in the progress bar and work stealing, so the last slow segment gets split instead of holding the file hostage.
- **Crash-proof resume** — each segment records its *flushed* byte offset in a `.mdm.json` sidecar next to the `.part` file every 2 seconds; the write buffer is force-flushed every 4 MB, so a kill, crash or reboot costs at most that.
- **Integrity guarded by `If-Range`** — the server's ETag / `Last-Modified` is pinned; if the remote file changes mid-download (CDN variant, resume after days) the download fails loudly instead of stitching two versions together.
- **Adaptive per-host concurrency** — `429 Too Many Requests` shrinks the connection count (AIMD), quiet periods ramp it back up, and the learned per-host limit is persisted in `config.json` across sessions.
- **FIFO queue and global bandwidth cap** — only *N* downloads run at once; a shared token-bucket limiter enforces a KB/s ceiling that can be moved live from the settings slider.
- **Never loses a download** — if the app is missing or fails to answer, the extension hands the URL straight back to Chrome, with a short-lived handback memo so the fallback is not re-intercepted into a loop.
- **Usable without the browser** — paste a URL, drag and drop into the window, or enable the optional clipboard watcher; the persistent history offers `[ open ]` and re-download on every past entry.
- **Windows-native niceties** — tray icon with live speed tooltip, taskbar progress bar (red on failure, yellow on pause), toasts under a registered AUMID, run-at-login toggle, and system proxy auto-detection from the registry.

<!-- SCREENSHOT: the Chrome side — the extension's ON badge on the toolbar icon and the "download with MDM" context menu on a link -->

## Tech stack

| Layer | What it uses |
| --- | --- |
| Core / engine | Rust 2021, `tokio` 1 (multi-thread runtime), `anyhow` |
| HTTP | `reqwest` 0.12 with `rustls-tls` + `stream` — HTTP/2 deliberately disabled so segments get real, separate TCP connections |
| GUI | `eframe` / `egui` 0.29, custom "terminal desktop" theme |
| Windows integration | `tray-icon` 0.19, `notify-rust` 4, `winreg` 0.52, `windows-sys` 0.59 (ITaskbarList3 vtable declared by hand) |
| Browser side | Chrome MV3 extension: service worker, `nativeMessaging`, `downloads`, `cookies`, `contextMenus` |
| Packaging | Inno Setup 6 installer, PowerShell fallback installer, GitHub Actions on `windows-latest` |

## Getting started

### Install on Windows

1. Grab **`mdm-setup.exe`** from the [latest release](../../releases/latest) and double-click it. No admin prompt: it installs to `%LOCALAPPDATA%\MDM` and registers the native messaging host itself.
2. One time only: open `chrome://extensions`, turn on **Developer mode**, click **Load unpacked** and pick `%LOCALAPPDATA%\MDM\extension`. The installer opens that page for you at the end.

Without the installer: download `mdm-windows.zip` from the same release, unzip it and double-click `install.bat` (it runs `install.ps1`, which copies the exe, writes `com.sriva.downloader.json` and the `HKCU\Software\Google\Chrome\NativeMessagingHosts` key).

### Build from source

Prerequisites: a stable Rust toolchain — plus Inno Setup 6 only if you want to produce the installer.

```bat
cd app
cargo build --release
..\install\install.bat
```

`install.ps1` runs `cargo build --release` for you if no `mdm.exe` is found next to it.

### Run it from macOS / Linux

The engine and the UI are cross-platform; the tray, taskbar progress, toasts, autostart and Chrome integration are Windows-only.

```bash
cd app
cargo run
# queue a download without the extension:
echo '{"url":"https://example.com/big.iso"}' | nc 127.0.0.1 48666
```

### Tests

```bash
cd app
cargo test
```

Besides unit tests on the pure parts (config clamping, queue, limiter), `tests/engine_http.rs` drives the real engine against an in-process fake HTTP server with no extra dependencies: byte-exact segmented download, fallback when the server has no `Range` support, `429` with and without `Retry-After`, truncated stream, pause/resume, sidecar recovery after a simulated crash, ETag changed mid-flight, queue limit enforcement, and a dead link that fails once without retrying.

## Configuration

Everything lives in the app's **Settings** tab, persisted (debounced, written atomically) to `%LOCALAPPDATA%\MDM\config.json`.

| Key | Default | What it does |
| --- | --- | --- |
| `download_dir` | `""` | Destination folder; empty means the system Downloads folder |
| `max_connections` | `8` | Parallel Range connections per download (1–16) |
| `max_concurrent_downloads` | `3` | Downloads running at once; the rest wait in FIFO order |
| `speed_limit_kbps` | `0` | Global bandwidth cap in KB/s; `0` = unlimited |
| `size_threshold_mb` | `10` | Size above which the extension hands the download to MDM |
| `auto_retry` | `3` | Automatic retries after a failure (max 10) |
| `notify_on_complete` | `true` | System toast when a download ends |
| `clipboard_watch` | `false` | Offer to download a file link as soon as it is copied |
| `autostart` | `false` | Start with Windows (HKCU `Run` key) |
| `host_conc` | `{}` | Per-host connection limits learned from `429` responses |

Out-of-range values from a hand-edited file are clamped on load and missing fields fall back to defaults, so an old config never breaks a new build.

The size threshold is shared: the extension asks the app for it at startup and every 5 minutes, falling back to the value in its own options page (`chrome://extensions` → MDM → Details → Extension options), where a domain blocklist can also be set. Downloads whose size Chrome does not know are judged by file extension against a built-in list of "big file" types (`zip`, `iso`, `mkv`, `exe`, …).

Other files under `%LOCALAPPDATA%\MDM`: `mdm.log` (full log, rotated at 2 MB, reachable from the Console tab) and `history.jsonl` (append-only history, pruned to the last 200 entries).

## How it works

```
Chrome ──onDeterminingFilename──▶ extension/background.js
   (cancel + erase in Chrome, collect url + cookies + referer + UA)
                │ sendNativeMessage (stdio, 4-byte length prefix)
                ▼
        mdm.exe --host  ──TCP 127.0.0.1:48666──▶  mdm.exe (GUI + engine)
        (spawns the app if it is not running)          └──▶ Downloads folder
```

One binary, two modes. Launched with `--host` (or with a `chrome-extension://` origin argument, as Chrome does) it is a stdio native-messaging bridge; launched normally it binds port 48666 — which doubles as the single-instance lock — and opens the UI. The line protocol on that port is one JSON object per connection: `{"url":…,"cookies":…}` enqueues a job, `{"cmd":"resume_all"}` restarts everything paused or failed, `{"cmd":"config"}` returns `{"ok":true,"sizeThresholdMb":10}`. Config queries deliberately do *not* spawn the app, so the extension polling for the threshold never makes a window appear out of nowhere. The app also polls GitHub Releases every 6 hours and can download the new installer, verify its sha256 and run it silently.

## Project structure

```
app/                 Rust crate "mdm" (binary + internal lib, so tests can drive the engine)
  src/engine.rs      segmented downloader, sidecar resume, work stealing, AIMD
  src/ui.rs          egui interface (Downloads / Console / Settings)
  src/host.rs        native messaging mode
  src/server.rs      localhost TCP job listener (port 48666)
  src/queue.rs       FIFO admission control
  src/limiter.rs     global token-bucket bandwidth cap
  src/update.rs      GitHub Releases auto-update
  tests/             integration tests against an in-process HTTP server
extension/           Chrome MV3 extension (background service worker + options page)
install/             install.ps1 / install.bat, native host manifest, Inno Setup script
.github/workflows/   test + build on every push; installer and GitHub release on v* tags
```

## Limitations

- Chrome only for now. Firefox speaks the same native messaging protocol, so the extension is the only missing piece.
- The extension is not on the Web Store: it must be loaded unpacked with Developer mode on.
- POST-generated downloads and one-shot token URLs can fail; the extension hands those back to Chrome, or you can switch it OFF from the toolbar icon.

## License

No license file is declared in this repository yet.
