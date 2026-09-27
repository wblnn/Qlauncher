# qlauncher

A cross-platform game launcher / download manager built with **Tauri 2 + Vue 3 + TypeScript**. It unifies multiple distribution platforms (miHoYo, Steam, Riot) behind a single interface: it auto-scans your machine for installed games, checks remote versions, supports full & differential downloads with speed and memory-usage controls, and launches games with one click.

> The backend currently relies heavily on the Windows registry (`winreg`); it is primarily developed and verified on **Windows**.

📖 Chinese documentation: [README.zh-CN.md](./README.zh-CN.md)

## Features

- **Multi-platform aggregation**: each platform is abstracted behind a unified `GamePlatform` trait, with capability tiers:
  - `Aggregate` (entry point only, e.g. Riot)
  - `DirectLaunch` (can be launched directly, e.g. Steam)
  - `Download` / `Full` (version check + download + launch, e.g. miHoYo)
- **Installed-game scanning**: reads the Windows uninstall registry keys plus per-platform directory rules to detect install paths, executables and local version numbers
- **Manual binding**: when the registry scan comes up empty, bind a game directory manually via a folder-picker dialog
- **Version guard (P0)**: semantic version comparison — downloads are only allowed when the state is `fresh` (not installed) or `ahead` (remote has an update); if the API's package lags behind the local version it is marked `behind` and downloading is blocked, preventing "updates that make your game older"
- **Full / differential downloads**: patches are preferred (with patch size shown), falling back to full packages; multi-part packages (`parts`) are supported
- **Download UX**: real-time progress events (downloaded / total / speed / ETA), cancel support, streaming buffering with a disk-usage safety line (`download_limit_mb`), optional download speed limit
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
  "download_limit_mb": 0,           // Disk-usage "safety line" for downloads, 0 = unlimited
  "download_buffer_mb": 4,          // Streaming write buffer size (MB), controls peak memory usage
  "download_speed_limit_mbps": 10   // Download speed limit (MB/s), 0 = unlimited
}
```

An invalid `launcher_id` causes miHoYo API calls to return `retcode != 0`. In that case, click the 🔬 self-check button in the UI to view the report and replace this field.

## Download Pipeline

1. `check_remote` fetches the version and part list (`parts`: url / md5 / size) and reports the `version_relation`
2. `start_download` streams each part into the target directory while computing MD5 on the fly; aborts when `download_limit_mb` would be exceeded to protect the disk
3. Each part is MD5-verified as soon as it finishes writing; on failure the part is deleted and an error is raised to avoid dirty files
4. Differential packages (`.7z`) are automatically extracted into `extracted/` after download, with the inner structure printed, ready for later merging
5. Throughout the process, `download-progress` events report progress, speed and ETA

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
| `start_download(game_id, dest, use_patch)` | Start a download (full or differential) |
| `cancel_download(game_id)` | Cancel an in-progress download |
| `probe_api` | API self-diagnosis, returns an array of report lines |

The frontend listens via `listen("download-progress", ...)` to receive `DownloadProgress` events and update the progress bar, speed and time remaining.

## Known Limitations

- Registry scanning and parts of the launch logic are Windows-only; macOS / Linux require additional platform branches in `platform.rs`
- miHoYo downloads rely on the non-public HYP API; API changes must be mirrored in `mihoyo.rs`
- There is no one-click post-download install / verification flow yet (extraction and MD5 verification are handled ad hoc inside platform implementations)

## Git Commit Convention

Commit messages describe the actual change in Chinese, e.g.: `优化了下载占用，完善了一下下载交互`.
