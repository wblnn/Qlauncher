# qlauncher

A cross-platform game launcher / download manager built with **Tauri 2 + Vue 3 + TypeScript**. It unifies multiple distribution platforms (miHoYo, Kuro, Steam, Riot) behind a single interface: it auto-scans your machine for installed games, checks remote versions, supports full-package downloads, differential patching and Sophon chunk-based reconciliation updates, provides verify / repair / rollback capabilities, and launches or kills games with one click.

> The backend currently relies heavily on the Windows registry (`winreg`); it is primarily developed and verified on **Windows**.

📖 中文文档：[README.md](./README.md)

## Features

- **Multi-platform aggregation**: each platform is abstracted behind a unified `GamePlatform` trait, with capability tiers:
  - `Aggregate` (entry point only, e.g. Riot)
  - `DirectLaunch` (protocol launch, e.g. Steam)
  - `Download` / `Full` (version check + download + launch + verify/repair, e.g. miHoYo, Kuro)
- **Region-merged cards**: CN and global editions of the same game are two separate backend ids but rendered as a single card in the frontend with a region switcher (the choice is persisted in `localStorage`)
- **Installed-game scanning**: reads the Windows uninstall registry keys plus per-platform directory rules to detect install paths, executables and local version numbers; cloud-gaming entries are filtered out automatically
- **Manual binding**: when the registry scan comes up empty, bind a game directory manually via a folder-picker dialog (exe candidate names are declared by each platform)
- **Official-launcher detection**: locates the locally installed official launcher (HoYoPlay / Steam / Riot Client) from the registry and launches it with one click; falls back to the official website when not found, and a "web-only" mode is also available
- **Version guard (P0)**: semantic version comparison — regular downloads are only allowed when the state is `fresh` (not installed) or `ahead` (remote has an update); if the API's package lags behind the local version it is marked `behind` and downloading is blocked, preventing "updates that make your game older". When the full package stops being updated, the real version can be inferred from announcements / pre-download info (labeled "inferred" in the UI)
- **Three update engines**:
  - **HYP full / differential** (e.g. Zenless Zone Zero): fetches the multi-part list (`parts`: url / md5 / size), streams each part to disk; a differential `.7z` is auto-extracted and then applied file-by-file with `hpatchz.exe` (old file + delta → new file)
  - **Sophon chunk reconciliation** (Genshin Impact / Honkai: Star Rail): parses the official zstd + protobuf manifests (`sophon.rs`), compares local files chunk by chunk against the manifest, and downloads only the changed chunks, assembling them directly into place — no full package required
  - **Kuro official-manifest reconciliation** (Wuthering Waves CN/global, PGR): fetches the official file manifest (validating the manifest's own md5 first to guard against stale CDNs), reconciles locally with a fast size scan plus content-level chunk-md5 verification, re-downloads only mismatched files, with multi-CDN failover
- **Resume / rollback (journal)**: patch application writes a `PatchJournal` throughout; interrupted patches can be continued with `resume_patch` or reverted with `rollback_patch`; backups use same-volume renames to `.qold` (zero-copy), temp files use the `.qkuro` / `.qtmp` suffixes
- **Verify & repair**: `verify_game_files` produces reports in two modes — quick (size) or deep (content md5); `repair_game_files` fills in the differences file-by-file according to the official hash manifest (`res_list_url`)
- **Low-resource streaming downloads**: configurable streaming buffer (`download_buffer_mb`, larger = lower CPU usage), smooth CPU-yielding speed limit (`download_speed_limit_mbps`), a disk-usage safety line (`download_limit_mb`) and a pre-download free-space check (`install_size`)
- **Resume-friendly**: parts already fully present on disk (matching size) are skipped when restarting a download
- **Real-time progress**: progress events (downloaded / total / instantaneous speed / ETA) emitted ~every 500 ms via `download-progress`; cancel anytime with partial-file cleanup
- **Running-state detection & process management**: polls processes with `sysinfo`, distinguishing "running / installed / not installed" in the UI; supports `kill_game`, scanning leftover zombie processes (`list_zombie_games`) and killing them by pid
- **API self-diagnosis**: built-in 🔬 self-check command (`probe_api`) printing HYP API connectivity, `launcher_id` provenance, safety-line settings and package-parsing results to help debug broken APIs

## Tech Stack

| Layer | Technology |
| --- | --- |
| Frontend | Vue 3 `<script setup>`, TypeScript, Vite |
| Desktop framework | Tauri 2 (`@tauri-apps/api` v2) |
| Plugins | `tauri-plugin-dialog` (folder picker), `tauri-plugin-opener` (open links/programs) |
| Backend | Rust: `reqwest` (HTTP), `md5` (hashing), `sevenz-rust` / `flate2` + `crc32fast` (extraction), `sysinfo` (processes), `winreg` (registry), `prost` + `zstd` (Sophon manifest decoding) |
| External tool | `hpatchz.exe` (HDiffPatch, single-file delta synthesis, shipped alongside the app) |

## Requirements

- Node.js 18+ (20+ recommended) and npm
- Rust toolchain (stable) + Tauri 2 prerequisites, see the [official Tauri guide](https://tauri.app/start/prerequisites/)
- Windows: WebView2 Runtime, MSVC build tools
- `hpatchz.exe` is required for delta patching (place it next to `qlauncher.exe` or under `src-tauri/`; already included in the repo)
- Recommended IDE: VS Code + Volar (Vue - Official), tauri-vscode, rust-analyzer

## Quick Start

```bash
# install dependencies
npm install

# development mode (starts the Vite dev server and the Tauri window together)
npm run tauri dev

# frontend only (http://localhost:1420)
npm run dev

# type check + frontend build
npm run build

# build installers (targets = all: msi / exe / nsis, etc.)
npm run tauri build
```

Vite uses a fixed port `1420` (`strictPort`) and ignores file-watching on `src-tauri/`; for physical-device / remote HMR, set the `TAURI_DEV_HOST` environment variable.

## Project Structure

```
.
├── index.html                     # frontend entry
├── vite.config.ts                 # Vite + Tauri dev config (port 1420 / HMR)
├── src/                           # Vue frontend
│   ├── main.ts                    # app bootstrap
│   └── App.vue                    # main UI: region-merged cards, version comparison, download/patch
│                                  #   progress, official-launcher/website, verify & repair,
│                                  #   zombie-process panel, patch resume/rollback
├── src-tauri/                     # Rust backend
│   ├── tauri.conf.json            # Tauri config (window, bundle, frontend-backend wiring)
│   ├── channels.json              # runtime config: launcher_id, download buffer/speed limit, hpatchz path, etc.
│   ├── capabilities/default.json  # permission declarations (core / opener / dialog)
│   ├── hpatchz.exe                # external delta-synthesis tool (shipped with the package)
│   └── src/
│       ├── main.rs                # binary entry
│       ├── lib.rs                 # Tauri command registration, registry scanning, official-launcher detection, process kill/detect
│       ├── platform.rs            # platform abstraction trait & data types (GameInfo / RemoteGameInfo / DownloadProgress)
│       ├── sophon.rs              # Sophon engine: zstd+protobuf manifest parsing, local chunk diff, chunk download & assembly
│       └── platforms/
│           ├── mod.rs             # platform registry and game_id → platform routing
│           ├── mihoyo.rs          # miHoYo HYP: version query, full/patch download, hpatchz synthesis, journal, verify/repair, self-check
│           ├── kuro.rs            # Kuro: official config/manifest APIs, raw per-file downloads, content-level reconciliation, multi-CDN failover
│           ├── steam.rs           # Steam (direct launch via steam://run)
│           └── riot.rs            # Riot (aggregate entry only)
└── public/                        # static assets
```

## Configuration (`src-tauri/channels.json`)

```jsonc
{
  "launcher_id": "jGHBHlcOq1",              // HYP API launcher_id; falls back to the built-in default when empty
  "launcher_ids": ["jGHBHlcOq1"],           // backup launcher_id list (for self-check / rotation)
  "download_limit_mb": 0,                   // disk-usage safety line for downloads, 0 = unlimited (download truncates safely at the line)
  "download_buffer_mb": 4,                  // streaming write buffer size (MB), default 4, larger = lower CPU usage
  "download_speed_limit_mbps": 10,          // download speed limit (MB/s), 0 or absent = unlimited; implemented by yielding CPU time slices
  "hpatchz_path": "",                       // override path to hpatchz.exe; default = next to the exe → src-tauri/ fallback
  "keep_volumes_after_extract": true,       // whether to keep original volumes after full-package extraction
  "biz_patterns": {                         // channel adaptation: game_id → registry/directory match keywords
    "genshin": ["hk4e", "原神"],
    "starrail": ["hkrpg", "星穹铁道"],
    "zenless": ["nap", "绝区零"]
  },
  "sophon_categories": ["游戏资源", "语音包-中文"]  // categories the Sophon updater should handle
}
```

Config is resolved from three sources in order: **project source `src-tauri/channels.json` (main dev config, edits take effect immediately) → user data-directory override (production) → built-in defaults**.

An invalid `launcher_id` makes miHoYo APIs return `retcode != 0`; click the 🔬 self-check in the UI to view the report and replace this field accordingly.

## Update Flow Details

### HYP full / differential (Zenless Zone Zero, etc.)

1. `check_remote` fetches the version and the part list (`parts`: url / md5 / size) and computes `version_relation` (fresh / ahead / equal / behind)
2. The frontend automatically picks the mode based on patch availability: differential when the local version equals `patch_from` (the confirm dialog shows the patch size), otherwise full package
3. `start_download` applies a second guard on the backend: refuses `equal` (already latest), refuses `behind` (prevents downgrades), refuses when the patch's base version doesn't match
4. Each part is streamed to the target directory (buffer size = `download_buffer_mb`) while its MD5 is computed on the fly; parts whose size already matches are skipped (resume-friendly); every read loop iteration checks the cancel flag and the safety line; when a speed limit is active, the expected duration per block is computed and any shortfall is slept away to yield the CPU
5. After each part finishes, its MD5 is verified; on failure the part is deleted and an error is raised, leaving no dirty files behind
6. `apply_patch` stage: the extracted `.7z` payload is applied file-by-file via `hpatchz old file + delta → new file`; old files are first renamed to `.qold` backups recorded in the journal; once everything succeeds, backups are deleted and the version is written to config.ini

### Sophon chunk reconciliation (Genshin Impact / Honkai: Star Rail)

1. Locate the manifest via `getGameBranches → getBuild`, decompress with zstd and decode the protobuf manifest with prost (`sophon.rs`)
2. `diff_local` compares local files chunk by chunk against the manifest (only chunks whose md5 differs need fetching), producing an exact download queue — total bytes are unknown upfront, so progress is reported as chunk counts
3. Changed chunks are streamed down and assembled directly into the target files (landed via `.qtmp` temp names); the whole process is cancellable
4. When the full package stops being updated, the version comes from announcement / pre-download inference (`announced_version`, clearly labeled in the UI); chunk updates remain allowed in that state
5. Verification goes through `verify_sophon`: the Sophon manifest is authoritative (the local pkg_version becomes stale after chunk updates), and per-file URL repair is deliberately disabled for the chunk system to prevent downgrades from outdated full-package URLs

### Kuro official-manifest reconciliation (Wuthering Waves)

1. Fetch the official launcher config to obtain the version, CDN and `indexFile`, then fetch the file manifest and validate the manifest's own md5 first (mismatch = the CDN is still serving the old version; better to fail than to download against a stale manifest)
2. Local reconciliation has two tiers: a fast size scan (used for version checks — no reading 80 GB for that) and content-level official chunk-md5 verification (used for downloads / verification)
3. Only mismatched files are downloaded (the official CDN serves files raw, so they land at their final size — no extraction step), with Range resume + multi-CDN failover (the manifest is re-fetched and re-checked before switching CDNs)
4. The private incremental format `.krpdiff` is not reverse-engineered; it is replaced by a "same full manifest + re-download the differing files" strategy

## Supported Games & IDs

| game_id | Name | Platform | Capability | Update engine |
| --- | --- | --- | --- | --- |
| `genshin` / `genshin_global` | Genshin Impact (CN/global) | miHoYo | Full | Sophon chunks |
| `starrail` / `starrail_global` | Honkai: Star Rail (CN/global) | miHoYo | Full | Sophon chunks |
| `zenless` / `zenless_global` | Zenless Zone Zero (CN/global) | miHoYo | Full | HYP full/patch |
| `wuthering_waves` / `wuthering_waves_global` | Wuthering Waves (CN/global) | Kuro | Full | official-manifest reconciliation |
| `pgr` | Punishing: Gray Raven | Kuro | Full | official-manifest reconciliation |
| `terraria` | Terraria | Steam | DirectLaunch | — (`steam://run`) |
| `ravenfield` | Ravenfield | Steam | DirectLaunch | — (`steam://run`) |
| `valorant` | VALORANT | Riot | Aggregate | — (official launcher / website fallback) |
| `test_notepad` | Test: Notepad | — | Full (dev integration) | — |

Adding a game: append a row to the respective `*_GAMES` table in `mihoyo.rs` / `kuro.rs` (id, biz/game_id, exe candidate names, match keywords, official site); register Steam / Riot games in their platform lists. `game_ids()` is generated from the tables automatically — just implement `remote_info` / `download` and friends as needed.

## Tauri Commands

| Command | Purpose |
| --- | --- |
| `get_installed_games` | Returns all games scanned across platforms with install status |
| `bind_game(id, dir)` | Manually bind a game directory (exe candidates declared by the platform) |
| `launch_game(id, use_official)` | Launch the game (or trigger the official-launcher protocol) |
| `kill_game(game_id)` | Kill all processes belonging to a game |
| `get_running_games` | Whether each game is currently running (`sysinfo` polling) |
| `list_zombie_games` / `kill_process(pid)` | Scan leftover game processes / kill by pid |
| `official_info(game_id)` | Local official-launcher exe path, official site and fallback URL |
| `open_official(game_id, mode)` | `auto` launches the local official launcher (falls back to web) / `web` opens the website only |
| `check_remote(game_id)` | Query the latest remote version, package size, patch availability and version relation (incl. version-source explanation) |
| `start_download(game_id, dest, use_patch, allow_old)` | Start a download (full / patch / chunk reconciliation; completed parts auto-skipped) |
| `cancel_download(game_id)` | Cancel an in-progress download (cleans up partial files) |
| `apply_patch(game_id, patch_dir, dry_run)` | Synthesize a patch (hpatchz / Sophon assembly), supports dry-run |
| `patch_status(game_id)` | Query the journal of an unfinished patch (basis for resuming) |
| `resume_patch(game_id)` / `rollback_patch(game_id)` | Resume an interrupted patch / roll back per the journal |
| `verify_game_files(game_id, deep)` | Verify local files (quick size mode / deep content mode), returns a report |
| `repair_game_files(game_id)` | Repair differing files according to the official manifest |
| `probe_api` | API self-check, returns diagnostic report lines |

The frontend listens to `listen("download-progress", ...)` for `DownloadProgress` events to update the progress bar, speed and remaining time; the patch-synthesis stage reuses the same event (chunk-count basis).

## Known Limitations

- Registry scanning and some launch logic are Windows-specific; macOS / Linux require additional platform branches
- miHoYo / Kuro downloads rely on non-public APIs; API changes must be mirrored in `mihoyo.rs` / `kuro.rs` / `sophon.rs`
- Delta patching depends on the external `hpatchz.exe`; related commands fail with a clear message about where to place it when missing
- Kuro's private incremental format `.krpdiff` is unsupported; replaced by full-manifest-diff re-downloads
- Speed-limit and buffer configuration changes require an app restart to take effect
