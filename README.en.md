# qlauncher

A cross-platform game launcher / download manager built with **Tauri 2 + Vue 3 + TypeScript**. It unifies multiple distribution platforms (miHoYo, Kuro Games, Steam, Riot) behind a single interface: it auto-scans your machine for installed games, checks remote versions, and supports several update paths — full packages, differential patches (hpatchz synthesis), Sophon chunked updates and the Kuro official-manifest engine — plus one-click launching.

> The backend currently relies heavily on the Windows registry (`winreg`); it is primarily developed and verified on **Windows**.

📖 中文文档：[README.md](./README.md)

## Features

- **Multi-platform aggregation**: each platform is abstracted behind a unified `GamePlatform` trait, with capability tiers:
  - `Aggregate` (entry point only, e.g. Riot)
  - `DirectLaunch` (can be launched directly, e.g. Steam)
  - `Download` / `Full` (version check + download + launch, e.g. miHoYo, Kuro)
- **Region-merged cards**: CN and global servers of the same game are two backend IDs; the frontend merges them into one card with a region switcher (choice persisted in localStorage)
- **Installed-game scanning**: reads the Windows uninstall registry keys plus per-platform directory rules to detect install paths, executables and local version numbers; cloud-gaming entries are filtered out automatically
- **Manual binding**: when the registry scan comes up empty, bind a game directory manually via a folder-picker dialog (exe candidate names are declared by each platform)
- **Official-launcher detection**: locates the locally installed official launcher (HoYoPlay / Steam / Riot Client) from the registry and launches it with one click; falls back to the official website when not found, and a "web-only" mode is also available
- **Version guard & multi-source version resolution**: semantic version comparison — downloads allowed only for `fresh` / `ahead` (or `announced` inferred from official posts); the effective "latest version" is the max of full package / pre-download / announcement·branch tag, with source and check time shown in the UI; a lagging API package is marked `behind` and blocked to prevent "updates that make your game older"
- **Four update paths (three engines + legacy channels)**:
  - **Kuro official-manifest engine (Wuthering Waves CN G152 / global G153, newly added)**: three-step official protocol (config index → file manifest → per-file raw transfer) with content-level reconciliation (official 100 MiB chunk md5s for large files, whole-file md5 for small ones); only mismatched files/bad chunks are re-downloaded, giving natural chunk-level resume and automatic CDN rotation retries
  - **Sophon chunk engine (Genshin / Honkai: Star Rail, CN)**: parses the official protobuf chunk manifest, reconciles every file locally (size + md5), then downloads only the changed chunks (zstd decode → staging assembly → whole-file verification → `.qlold` backup swap). No full-package archive needed; unaffected by full-package deprecation
  - **Full / differential downloads (ZZZ and other HYP channels)**: patch archives are synthesized in place with `hpatchz.exe`; full multi-volume packages (`.zip.001…` / `.7z.001…`) are decompressed directly through a virtual concatenated stream (custom ZIP64/deflate reader + sevenz-rust), with optional volume retention after extraction
  - **Legacy pkg_version verify & repair (other miHoYo channels)**: per-file comparison against the official hash list in the game directory, re-downloading only missing/corrupted files from res_list instead of the whole package
- **Reversible patch transactions (journal)**: patch synthesis advances through preflight → synthesize → replace → delete → verify → commit, journaling each phase; any failure or power loss can be resumed (`resume_patch`, idempotent steps) or rolled back (`rollback_patch`: restore `.qold`, delete newly added files, restore config.ini). All irreversible actions happen only after the commit point
- **🩹 Verify & repair**: validates local files against the official manifest (fast = size + key-exe md5; deep = content-level md5) and repairs only missing/corrupted files instead of reinstalling; Genshin/Star Rail automatically switch to the Sophon manifest basis, Kuro uses chunk-md5 content reconciliation
- **Low-resource streaming downloads**: configurable buffer (`download_buffer_mb`), smooth CPU-yielding speed limit (`download_speed_limit_mbps`), disk-usage safety line (`download_limit_mb`)
- **Real-time progress**: a dedicated reporter thread emits at a fixed 1 Hz (downloaded / total / smoothed instantaneous speed / ETA); reconciliation phases report separate `verifying` progress; cancel anytime — already-downloaded chunks stay in temp files and continue on the next run
- **Running state & zombie processes**: `sysinfo` polling distinguishes "running / installed / not installed"; leftover processes with working set ≤ 10 MB are flagged as zombies and can be killed precisely by PID (`list_zombie_games` / `kill_process`) so they no longer lock game files
- **API self-diagnosis**: built-in 🔬 self-check command printing HYP API connectivity, `launcher_id` source, safety-line settings and package-parsing results to help debug broken APIs

## Tech Stack

| Layer | Technology |
| --- | --- |
| Frontend | Vue 3 `<script setup>`, TypeScript, Vite |
| Desktop framework | Tauri 2 (`@tauri-apps/api` v2) |
| Plugins | `tauri-plugin-dialog` (folder picker), `tauri-plugin-opener` (links/programs) |
| Backend | Rust: `reqwest` (HTTP; blocking calls always run on dedicated OS threads to avoid tokio-context panics), `md5` (checksums), `prost` + `zstd` (Sophon manifests/chunks), `flate2` (manual gunzip of Kuro CDN responses + zip deflate), `sevenz-rust` + `crc32fast` (full-package volume extraction), `sysinfo` (processes/disks), `winreg` (registry) |
| External tool | `hpatchz.exe` (official HDiffPatch binary, required for patch synthesis; put it in `src-tauri/` or point `hpatchz_path` at it) |

## Requirements

- Node.js 18+ (20+ recommended) and npm
- Rust toolchain (stable) + Tauri 2 prerequisites — see the [official guide](https://tauri.app/start/prerequisites/)
- Windows: WebView2 Runtime, MSVC build tools
- `hpatchz.exe` from HDiffPatch releases (for differential synthesis)
- Recommended IDE: VS Code + Volar (Vue - Official), rust-analyzer

## Quick Start

```bash
npm install          # install dependencies
npm run tauri dev    # dev mode (Vite dev server + Tauri window)
npm run dev          # frontend only (http://localhost:1420)
npm run build        # type-check + frontend build
npm run tauri build  # package installers (targets = all: msi / exe / nsis …)
```

Vite uses fixed port `1420` (`strictPort`) and ignores `src-tauri/` file watching; set `TAURI_DEV_HOST` for real-device / remote HMR.

## Project Structure

```
.
├── index.html                     # frontend entry
├── vite.config.ts                 # Vite + Tauri dev config (port 1420 / HMR)
├── src/                           # Vue frontend
│   ├── main.ts                    # app bootstrap
│   └── App.vue                    # main UI: region-merged cards, version compare, download/patch progress, verify-repair, zombie cleanup
├── src-tauri/                     # Rust backend
│   ├── tauri.conf.json            # Tauri config (window, bundle, FE/BE wiring)
│   ├── channels.json              # runtime config: launcher_id, buffer/speed-limit/disk-cap, hpatchz, Sophon & Kuro overrides
│   ├── capabilities/default.json  # permission declarations (core / opener / dialog)
│   └── src/
│       ├── main.rs                # binary entry
│       ├── lib.rs                 # command registration, registry scan, official-launcher detection, process/zombie checks
│       ├── platform.rs            # platform abstraction trait & data types (GameInfo / RemoteGameInfo / DownloadProgress)
│       ├── sophon.rs              # Sophon chunk engine: protobuf manifest, chunk diff/download (zstd), staging swap, config.ini rewrite
│       └── platforms/
│           ├── mod.rs             # platform registry & game_id → platform routing
│           ├── mihoyo.rs          # miHoYo: HYP version sources, virtual-stream volume extraction, hpatchz patch transactions (journal), verify/repair
│           ├── kuro.rs            # Kuro: Wuthering Waves CN/global official-manifest engine (content-level reconcile, chunk resume, CDN rotation)
│           ├── steam.rs           # Steam (protocol direct launch)
│           └── riot.rs            # Riot (aggregated entry)
└── public/                        # static assets
```

## Configuration (`src-tauri/channels.json`)

```jsonc
{
  "launcher_id": "jGHBHlcOq1",          // HYP launcher_id; falls back to the built-in default when empty
  "launcher_ids": ["jGHBHlcOq1"],       // candidate list, validated one by one during self-check
  "biz_patterns": {                      // name-matching rules for getGames (add a rule when miHoYo renames a channel — no rebuild needed)
    "genshin": ["hk4e", "原神"], "starrail": ["hkrpg", "星穹铁道"], "zenless": ["nap", "绝区零"]
  },
  "sophon_categories": ["游戏资源", "语音包-中文"],  // Sophon categories to apply
  "hpatchz_path": "",                    // hpatchz.exe path; default = next to the executable → project src-tauri/ fallback
  "keep_volumes_after_extract": true,    // keep downloaded volumes after full-package extraction
  "download_limit_mb": 0,                // disk-usage safety line, 0 = unlimited (truncates safely when hit; useful for pipeline tests)
  "download_buffer_mb": 4,               // streaming write buffer (MB); larger = lower CPU usage
  "download_speed_limit_mbps": 10,       // speed limit (MB/s); 0/absent = unlimited; implemented by yielding CPU time slices
  "kuro_games": {                        // override Kuro distribution constants (gameId/appKey/endpoint changes need no rebuild)
    "wuthering_waves": { "game_id": "G152", "app_key": "...", "cn": true }
  }
}
```

Config resolution order: **project `src-tauri/channels.json` (main dev config, read at the start of each task) → user-data-dir override (production) → built-in defaults**.

A stale `launcher_id` makes miHoYo APIs return `retcode != 0`; run the 🔬 self-check in the UI and replace the field accordingly.

## Download / Update Flow Details

### Sophon chunk engine (Genshin / Star Rail, CN)
1. `getGameBranches` → `getBuild` yields the latest tag and manifest URL; the protobuf manifest decodes into the full file/chunk list
2. Per-file local reconciliation (size + md5); only changed files contribute chunks to download (progress counted in compressed bytes)
3. Chunks download from CDN (zstd-decoded) into `ql_staging/`; on resume, already-correct staged regions are verified and skipped
4. After whole-file md5 passes, files swap into the game dir (old files renamed `.qlold` first, deleted only once everything succeeds)
5. The actually-applied tag is written back to `config.ini`; space preflight = changed-file staging size + 5 GB headroom

### hpatchz differential synthesis (ZZZ and other HYP channels)
1. Patch archives download into `patch_<from>_<to>/`; `.7z` files auto-extract into its `extracted/` subdirectory
2. `apply_patch` six-phase transaction: preflight (process locks / writability / space) → synthesize (original renamed to `.qold` backup, `hpatchz old diff new`) → replace → delete (rename only, never hard-delete yet) → verification gate (per-entry md5) → ★commit★ (drop `.qold`s, write config.ini version)
3. The journal persists every phase: after interruption use `resume_patch` (idempotent) or `rollback_patch`; `dry_run` only preflights and reports the plan

### Full-package download & virtual-stream extraction
1. Multi-volume packages (`.zip.001…` / `.7z.001…`) stream to disk volume by volume: buffer = `download_buffer_mb`, MD5 computed while writing; when a speed limit is active each block's expected duration is computed and the thread sleeps to yield CPU
2. Volumes already present on disk with matching size are skipped (resume-friendly); every read loop checks the cancel flag and the `download_limit_mb` safety line
3. Once all volumes verify, a virtual concatenated stream joins them across volume boundaries and extracts directly into `<game_id>_<version>/` (ZIP via the custom ZIP64/deflate reader, 7z via sevenz-rust); volumes are kept or deleted per `keep_volumes_after_extract`

### Kuro official-manifest engine (Wuthering Waves)
1. Config index `{api}/{gameId}/{appId_appKey}/index.json` → version / baseUrl / indexFile(+md5) / cdnList (CDN force-gzips JSON responses and they are decoded manually; spaces in `dest` paths are %20-encoded automatically)
2. Fetch the file manifest and verify `indexFileMd5` (a mismatch means a stale CDN — fail rather than download from an outdated manifest)
3. Content-level reconciliation: large files checked chunk-by-chunk against official 100 MiB md5s, small files by whole-file md5 — catches "same size, different content"; near-instant on fresh installs (reconciliation reports `verifying` progress every ~300 ms)
4. Only the diff set downloads: missing files fetch chunk-by-chunk from scratch; content-mismatched files **copy the local file as a base and refill only bad chunks** (no re-downloading an entire 18 GB pak); an interrupted `.qkuro` temp file doubles as the resume base; Range requests with per-chunk md5 verification (bad chunks truncated for retry); compressed server responses are rejected outright to keep offsets sane
5. Space preflight = actual download bytes + largest base-copy file + 5 GB headroom; small files first, big paks last; each file retries across up to 2 CDNs (manifest re-checked before switching); 5 consecutive failures stop the run gracefully (completed files stay); placement uses `.qold` backup + rename with automatic restore on failure
6. On success a local version marker `ql_kuro_version.txt` is written; version checks use a fast size scan plus md5 of two key executables; the official incremental format `.krpdiff` is proprietary (not reverse-engineered) — this engine approximates it via "whole-manifest diff download". Measured on 3.7.0: CN 915 files / 80.52 GB, global 719 files / 79.98 GB

## Supported Games & IDs

| game_id | Name | Platform | Tier |
| --- | --- | --- | --- |
| `genshin` / `genshin_global` | Genshin Impact (CN / global) | miHoYo | Full (CN via Sophon; global currently official-launcher only) |
| `starrail` / `starrail_global` | Honkai: Star Rail (CN / global) | miHoYo | Full (same as above) |
| `zenless` / `zenless_global` | Zenless Zone Zero (CN / global) | miHoYo | Full (package / differential) |
| `wuthering_waves` / `wuthering_waves_global` | Wuthering Waves (CN G152 / global G153) | Kuro | Full (official-manifest engine) |
| `pgr` | Punishing: Gray Raven | Kuro | Full (distribution constants pending; fill in channels.json) |
| `terraria` | Terraria | Steam | DirectLaunch |
| `ravenfield` | Ravenfield | Steam | DirectLaunch |
| `valorant` | VALORANT | Riot | Aggregate |
| `test_notepad` | Test: Notepad | — | Full (integration testing) |

To add a game, register its ID in the platform's `game_ids()` and implement `remote_info` / `download` as needed.

## Tauri Commands

| Command | Purpose |
| --- | --- |
| `get_installed_games` | All games scanned across platforms, with install state |
| `bind_game(id, dir)` | Manually bind a game directory (exe candidates declared by platform) |
| `launch_game(id, use_official)` | Launch the game (or invoke the official launcher protocol) |
| `kill_game(game_id)` | Terminate the game's process |
| `get_running_games` | Whether each game is running (`sysinfo` polling) |
| `list_zombie_games` | List leftover/zombie processes (working set ≤ 10 MB) |
| `kill_process(pid)` | Precisely kill a zombie process by PID |
| `official_info(game_id)` | Local official-launcher exe path, website and fallback URLs |
| `open_official(game_id, mode)` | `auto` launches the local official launcher (falls back to web) / `web` opens the website only |
| `check_remote(game_id)` | Latest remote version, package sizes, patch info, version relation & source |
| `start_download(game_id, dest, use_patch, allow_old)` | Start download/update (auto-routes to Sophon / Kuro / full-patch engines) |
| `cancel_download(game_id)` | Cancel an in-flight download or reconciliation (keeps completed chunks) |
| `probe_api` | API self-diagnosis, returns report lines |
| `apply_patch(game_id, patch_dir, dry_run)` | Differential synthesis (reversible transaction; `dry_run` = preflight plan only) |
| `patch_status(game_id)` | Read the journal of an unfinished update (phase, progress, backups) |
| `resume_patch(game_id)` | Resume a synthesis from the journal |
| `rollback_patch(game_id)` | One-click rollback to pre-patch state (restore `.qold`, remove new files, restore config.ini) |
| `verify_game_files(game_id, deep)` | Verify against the official manifest (fast/deep; Sophon basis for Genshin & SR, chunk-md5 for Kuro) |
| `repair_game_files(game_id)` | Repair: re-download only missing/corrupted files (Kuro titles reuse the official-manifest engine for the diff re-download, "fire-and-report" via events) |

The frontend listens to `download-progress` / `patch-progress` / `verify-progress` events to drive progress bars, phase steppers and ETAs.

## Known Limitations

- Registry scanning and parts of the launch logic are Windows-only; macOS / Linux need platform-specific branches
- miHoYo downloads rely on non-public HYP / Sophon APIs and Kuro downloads on the non-public official manifest endpoints; upstream changes require updating the corresponding modules (most constants can be hot-overridden via channels.json)
- Global-server Genshin / Star Rail Sophon build entry points are not wired yet — use the official launcher for those updates; global ZZZ full packages work
- Punishing: Gray Raven's distribution constants (gameId + appKey) are not yet obtained; fill them into channels.json once available
- Patch synthesis requires the external `hpatchz.exe`; a clear error tells you where to place it if missing
- Speed-limit / buffer / safety-line settings are read at the start of each download task; changes don't affect a task in progress

## Git Commit Convention

Commit messages describe the actual change in Chinese, e.g.: `优化了下载占用，完善了一下下载交互`.
