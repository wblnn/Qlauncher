# qlauncher

A cross-platform game launcher / download manager built with **Tauri 2 + Vue 3 + TypeScript**. It unifies multiple distribution platforms (miHoYo, Steam, Riot) behind a single interface: it auto-scans your machine for installed games, checks remote versions, supports full & differential downloads with speed and memory-usage controls, and launches games with one click.

> The backend currently relies heavily on the Windows registry (`winreg`); it is primarily developed and verified on **Windows**.

📖 中文文档：[README.md](./README.md)

## Features

- **Multi-platform aggregation**: each platform is abstracted behind a unified `GamePlatform` trait, with capability tiers:
  - `Aggregate` (entry point only, e.g. Riot)
  - `DirectLaunch` (can be launched directly, e.g. Steam)
  - `Download` / `Full` (version check + download + launch, e.g. miHoYo)
- **Installed-game scanning**: reads the Windows uninstall registry keys plus per-platform directory rules to detect install paths, executables and local version numbers
- **Manual binding**: when the registry scan comes up empty, bind a game directory manually via a folder-picker dialog
- **Version guard (P0)**: semantic version comparison — downloads are only allowed when the state is `fresh` (not installed) or `ahead` (remote has an update); if the API's package lags behind the local version it is marked `behind` and downloading is blocked, preventing "updates that make your game older"
- **Full / differential downloads**: patches are preferred (with patch size shown; refused when the local version doesn't match `patch_from`), falling back to full packages; both support multi-part packages (`parts`); patch `.7z` archives are auto-extracted into `extracted/`
- **Download UX**: real-time progress events (downloaded / total / instantaneous speed / ETA, emitted ~every 500 ms), cancel with partial-file cleanup, configurable streaming buffer (`download_buffer_mb`, larger = lower CPU usage), smooth CPU-yielding speed limit (`download_speed_limit_mbps`) and a disk-usage safety line (`download_limit_mb`)
- **Resume-friendly**: parts already fully present on disk (matching size) are skipped when restarting a download
- **Running-state detection**: polls processes via `sysinfo`, distinguishing "running / installed / not installed" in the UI
- **Official entry points**: open the official website or invoke the official launcher (`launcher_uri`) with one click
- **API self-diagnosis**: built-in 🔬 self-check command printing HYP API connectivity, `launcher_id` source and package-parsing results to help debug broken APIs

## Tech Stack

| Layer | Technology |
| --- | --- |
| Frontend | Vue 3 `<script setup>`, TypeScript, Vite |
| Desktop framework | Tauri 2 (`@tauri-apps/api` v2) |
| Plugins | `tauri-plugin-dialog` (folder picker), `tauri-plugin-opener` (open links / programs) |
| Backend (Rust) | `reqwest` (HTTP), `md5` (checksums), `sevenz-rust` (7z extraction), `sysinfo` (processes), `winreg` (registry) |

## Prerequisites

- Node.js 18+ (20+ recommended) and npm
- Rust toolchain (stable) + Tauri 2 prerequisites, see the [official guide](https://tauri.app/start/prerequisites/)
- Windows: WebView2 Runtime and MSVC build tools
- Recommended IDE: VS Code + Volar (Vue - Official), tauri-vscode, rust-analyzer (this repo ships with `.vscode` config)

## Quick Start

```bash
# Install dependencies
npm install

# Dev mode (starts the Vite dev server and the Tauri window together)
npm run tauri dev

# Frontend only (http://localhost:1420)
npm run dev

# Type check + frontend build
npm run build

# Build installer packages (targets = all: msi / exe / nsis, etc.)
npm run tauri build
```

Vite uses a fixed port `1420` (`strictPort`) and ignores file watching under `src-tauri/`; for physical-device / remote HMR, set the `TAURI_DEV_HOST` environment variable.

## Project Structure

```
.
├── index.html                # Frontend entry
├── vite.config.ts            # Vite + Tauri dev config (port 1420 / HMR)
├── src/                      # Vue frontend
│   ├── main.ts               # App bootstrap
│   └── App.vue               # Main UI: game list, version comparison, download progress, buttons
├── src-tauri/                # Rust backend
│   ├── tauri.conf.json       # Tauri config (window, bundle, commands)
│   ├── channels.json         # Runtime config: launcher_id, download buffer / speed limit / usage cap
│   ├── capabilities/default.json  # Permission declarations (core / opener / dialog)
│   └── src/
│       ├── lib.rs            # Tauri command registration, registry scanning, launching & process detection
│       ├── platform.rs       # Platform abstraction trait & data structures (GameInfo / RemoteGameInfo / DownloadProgress)
│       └── platforms/
│           ├── mod.rs        # Platform registry & game_id → platform routing
│           ├── mihoyo.rs     # miHoYo HYP API: version check, full/differential download, self-diagnosis
│           ├── steam.rs      # Steam (direct launch)
│           └── riot.rs       # Riot (aggregated entry)
└── public/                   # Static assets
```

## Configuration (`src-tauri/channels.json`)

```jsonc
{
  "launcher_id": "jGHBHlcOq1",      // HYP API launcher_id; falls back to the built-in default if empty
  "download_limit_mb": 0,           // Disk-usage "safety line" for downloads, 0 = unlimited (truncates safely when reached; used for pipeline testing)
  "download_buffer_mb": 4,          // Streaming write buffer size (MB), default 4; larger = lower CPU usage
  "download_speed_limit_mbps": 10   // Download speed limit (MB/s), 0 or omitted = unlimited; implemented by yielding CPU time slices
}
```

Config is resolved in three tiers: **project source `src-tauri/channels.json` (primary dev config, takes effect immediately) → user app-data directory override (production) → built-in defaults**.

An invalid `launcher_id` causes miHoYo API calls to return `retcode != 0`. In that case, click the 🔬 self-check button in the UI to view the report and replace this field.

## Download Pipeline

1. `check_remote` fetches the version and part list (`parts`: url / md5 / size) and reports the `version_relation` (fresh / ahead / equal / behind)
2. The frontend picks the mode automatically: differential when local version == `patch_from` (confirmation shows patch size), otherwise full package
3. `start_download` re-guards on the backend: refuses `equal` (already up to date), `behind` (prevents downgrade) and patch/full version mismatches
4. Each part is streamed into the target directory (buffer = `download_buffer_mb`) with MD5 computed on the fly:
   - Parts whose files already exist on disk at full size are skipped (resume-friendly)
   - Every read loop checks the cancel flag and the `download_limit_mb` safety line; exceeding it truncates the download and emits `done_test`
   - When a speed limit is active, the expected duration per chunk is computed and any surplus time is spent sleeping, so the CPU yields instead of busy-waiting
5. Each part is MD5-verified as soon as it finishes writing; on failure the part is deleted and an error is raised to avoid dirty files
6. Differential packages (`.7z`) are automatically extracted into `patch_<from>_<to>/extracted/` after download, with the inner structure printed, ready for later merging
7. Throughout the process, `download-progress` events report progress, instantaneous speed and ETA roughly every 500 ms; cancelling deletes the partial file and emits `error:已取消`

## Supported Games & IDs

| game_id | Name | Platform | Capability |
| --- | --- | --- | --- |
| `genshin` | Genshin Impact | miHoYo | Full |
| `starrail` | Honkai: Star Rail | miHoYo | Full |
| `zenless` | Zenless Zone Zero | miHoYo | Full |
| `terraria` | Terraria | Steam | DirectLaunch |
| `ravenfield` | Ravenfield | Steam | DirectLaunch |
| `valorant` | VALORANT | Riot | Aggregate |
| `test_notepad` | Test: Notepad | — | Full (for integration testing) |

To add a game, simply register its ID in the corresponding platform's `game_ids()` and implement `remote_info` / `download` etc. as needed.

## Tauri Commands

| Command | Purpose |
| --- | --- |
| `get_installed_games` | Returns all games detected across platforms with install status |
| `bind_game(id, dir)` | Manually bind a game directory |
| `launch_game(id, use_official)` | Launch a game (or invoke the official launcher) |
| `get_running_games` | Returns whether each game is currently running |
| `official_info(game_id)` / `open_official(game_id, mode)` | Official-site info / open the official site |
| `check_remote(game_id)` | Query the latest remote version, package size, patch availability & version relation |
| `start_download(game_id, dest, use_patch)` | Start a download (full or differential; already-complete parts are skipped) |
| `cancel_download(game_id)` | Cancel an in-progress download (partial files are cleaned up) |
| `probe_api` | API self-diagnosis, returns an array of report lines |

The frontend listens via `listen("download-progress", ...)` to receive `DownloadProgress` events and update the progress bar, speed and time remaining.

## Known Limitations

- Registry scanning and parts of the launch logic are Windows-only; macOS / Linux require additional platform branches in `platform.rs`
- miHoYo downloads rely on the non-public HYP API; API changes must be mirrored in `mihoyo.rs`
- There is no one-click post-download install / verification flow yet (extraction and MD5 verification are handled ad hoc inside platform implementations)


