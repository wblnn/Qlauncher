//! 库洛游戏（《鸣潮》）下载引擎 —— 2026-10 实测协议（国服 / 国际服均验证通过）
//!
//! 三步走，全部用官方地址，不做任何私有格式逆向：
//!   ① 配置索引  GET {api}/{gameId}/{appId_appKey}/index.json
//!        → default.config { version, size, baseUrl, indexFile, indexFileMd5 } + default.cdnList[]
//!   ② 文件清单  GET {cdn}{indexFile}
//!        → { resource:[ { dest, md5, size, fromFolder?, chunkInfos?[{start,end,md5}] } ] }
//!   ③ 文件内容  GET {cdn}{fromFolder | baseUrl}{dest}      ← dest 里可能有空格，要 %20 编码
//!
//! 官方整包是「按文件裸传」：清单里的 size 就是落盘大小，没有解压步骤；CDN 支持 Range，
//! 大文件还自带 100MiB 分块的逐块 md5 → 天然可做分块续传 + 逐块校验。
//! 增量包是私有格式 `.krpdiff`（不逆向），这里改成「同一份整包清单 + 只下本地对不上的文件」，
//! 下载量接近增量、且每个字节都用官方 md5 校验。
//!
//! 实测数据（3.7.0）：国服 G152 清单 915 文件 / 80.52 GB；国际服 G153 清单 719 文件 / 79.98 GB。

use crate::platform::{
    compare_versions, DownloadProgress, GameInfo, GamePlatform, PlatformLevel, RemoteGameInfo,
};
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;

/// 国际服配置端点（实测 200，G153）
const KURO_API_GLOBAL: &str = "https://prod-alicdn-gamestarter.kurogame.com/launcher/game";
/// 国服配置端点（实测 200，G152）
const KURO_API_CN: &str = "https://prod-cn-alicdn-gamestarter.kurogame.com/launcher/game";
/// 清单 temp 后缀 / 备份后缀
const TMP_EXT: &str = "qkuro";
const BAK_EXT: &str = "qold";
/// 我们自己写的本地版本标记（官方装在别处时读不到，会退回"关键文件对账"）
const LOCAL_VERSION_FILE: &str = "ql_kuro_version.txt";

pub struct KuroGame {
    pub id: &'static str,
    pub name: &'static str,
    /// 配置路径里的 gameId（G152=国服鸣潮 / G153=国际服鸣潮）
    pub game_id: &'static str,
    /// 配置路径里的 appId_appKey
    pub app_key: &'static str,
    /// true = 走国服端点
    pub cn: bool,
    pub exes: &'static [&'static str],
    /// 注册表/目录名匹配关键字
    pub hints: &'static [&'static str],
    pub website: &'static str,
}

pub const KURO_GAMES: &[KuroGame] = &[
    KuroGame {
        id: "wuthering_waves",
        name: "鸣潮（国服）",
        game_id: "G152",
        app_key: "10003_Y8xXrXk65DqFHEDgApn3cpK5lfczpFx5",
        cn: true,
        exes: &["Wuthering Waves.exe", "Client-Win64-Shipping.exe"],
        hints: &["Wuthering Waves", "鸣潮"],
        website: "https://mc.kurogames.com/",
    },
    KuroGame {
        id: "wuthering_waves_global",
        name: "鸣潮（国际服）",
        game_id: "G153",
        app_key: "50004_obOHXFrFanqsaIEOmuKroCcbZkQRBC7c",
        cn: false,
        exes: &["Wuthering Waves.exe", "Client-Win64-Shipping.exe"],
        hints: &["Wuthering Waves"],
        website: "https://wutheringwaves.kurogames.com/",
    },
    KuroGame {
        id: "pgr",
        name: "战双帕弥什",
        game_id: "",
        app_key: "",
        cn: true,
        exes: &["PGR.exe", "Punishing.exe"],
        hints: &["战双", "Punishing", "PGR"],
        website: "https://pgr.kurogames.com/",
    },
];

/// ⚠️ 所有 blocking 网络调用都必须落到"我们自己开的系统线程"上。
///
/// Tauri 的 `#[tauri::command(async)]` 函数体跑在 tokio worker 线程里，在那里创建/析构
/// `reqwest::blocking` 客户端会 panic：
///   `Cannot drop a runtime in a context where blocking is not allowed`
/// 表现就是"命令既不返回也不报错，前端毫无反馈"（查版本点了没反应就是这个）。
/// 所以库洛这边的对外入口统一从这里过一道，谁调用都安全。
fn on_worker<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    match std::thread::spawn(f).join() {
        Ok(r) => r,
        Err(_) => Err("内部线程 panic（已捕获，避免整个命令挂死；请把终端日志发我）".into()),
    }
}

fn game_def(game_id: &str) -> Option<&'static KuroGame> {
    KURO_GAMES.iter().find(|g| g.id == game_id)
}

// ================= channels.json 覆盖（常量可改，不用重编译）=================
#[derive(Deserialize, Default)]
struct KuroOverride {
    game_id: Option<String>,
    app_key: Option<String>,
    api: Option<String>,
    cn: Option<bool>,
}
#[derive(Deserialize, Default)]
struct KuroChannels {
    kuro_games: Option<HashMap<String, KuroOverride>>,
}

fn read_override(app: &tauri::AppHandle, game_id: &str) -> Option<KuroOverride> {
    let p = crate::platforms::mihoyo::channels_path(app)?;
    let s = std::fs::read_to_string(p).ok()?;
    let cfg = serde_json::from_str::<KuroChannels>(&s).ok()?;
    cfg.kuro_games.and_then(|m| m.get(game_id).map(|o| KuroOverride {
        game_id: o.game_id.clone(), app_key: o.app_key.clone(), api: o.api.clone(), cn: o.cn,
    }))
}

/// 解析出最终要用的配置端点
fn resolve_endpoint(app: &tauri::AppHandle, g: &KuroGame) -> (String, String, String) {
    let ov = read_override(app, g.id).unwrap_or_default();
    let gid = ov.game_id.unwrap_or_else(|| g.game_id.to_string());
    let key = ov.app_key.unwrap_or_else(|| g.app_key.to_string());
    let api = ov.api.unwrap_or_else(|| {
        let cn = ov.cn.unwrap_or(g.cn);
        if cn { KURO_API_CN } else { KURO_API_GLOBAL }.to_string()
    });
    (api, gid, key)
}

// ================= 协议数据结构 =================
#[derive(Deserialize, Default, Clone)]
struct KuroConfig {
    #[serde(default)]
    version: String,
    #[serde(default)]
    size: u64,
    #[serde(default, rename = "baseUrl")]
    base_url: String,
    #[serde(default, rename = "indexFile")]
    index_file: String,
    #[serde(default, rename = "indexFileMd5")]
    index_md5: String,
}
#[derive(Deserialize, Default)]
struct KuroCdn {
    #[serde(default)]
    url: String,
}
#[derive(Deserialize, Default)]
struct KuroIndex {
    #[serde(default)]
    default: KuroIndexDefault,
}
#[derive(Deserialize, Default)]
struct KuroIndexDefault {
    #[serde(default)]
    config: KuroConfig,
    #[serde(default, rename = "cdnList")]
    cdn_list: Vec<KuroCdn>,
}
#[derive(Deserialize, Default)]
struct KuroManifest {
    #[serde(default)]
    resource: Vec<KuroEntry>,
}
#[derive(Deserialize, Clone, Default)]
struct KuroEntry {
    #[serde(default)]
    dest: String,
    #[serde(default)]
    md5: String,
    #[serde(default)]
    size: u64,
    #[serde(default, rename = "fromFolder")]
    from_folder: Option<String>,
    #[serde(default, rename = "chunkInfos")]
    chunk_infos: Vec<KuroChunk>,
}
#[derive(Deserialize, Clone, Default)]
struct KuroChunk {
    #[serde(default)]
    start: u64,
    #[serde(default)]
    end: u64,
    #[serde(default)]
    md5: String,
}

fn meta_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}
fn chunk_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// 元数据 GET：显式声明 identity，尽量让 CDN 别压
fn meta_get(url: &str) -> Result<reqwest::blocking::Response, String> {
    meta_client().get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .map_err(|e| format!("请求失败: {}", e))
}

/// 读响应体 —— 库洛 CDN 会对 JSON 强行 gzip（实测 Content-Encoding: gzip，2818B 压 18534B）。
/// 我们的 reqwest 没开 gzip 特性，直接 serde 就会得到
/// `解析配置失败: error decoding response body`，所以这里手动解。
/// 用已有的 flate2，不引新依赖。
fn read_body(resp: reqwest::blocking::Response) -> Result<Vec<u8>, String> {
    let enc = resp.headers().get(reqwest::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let raw = resp.bytes().map_err(|e| format!("读响应失败: {}", e))?;
    let out = if enc.contains("gzip") {
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut o = Vec::new();
        d.read_to_end(&mut o).map_err(|e| format!("gzip 解压失败: {}", e))?;
        o
    } else if enc.contains("deflate") {
        let mut d = flate2::read::ZlibDecoder::new(&raw[..]);
        let mut o = Vec::new();
        d.read_to_end(&mut o).map_err(|e| format!("deflate 解压失败: {}", e))?;
        o
    } else if enc.contains("br") {
        return Err("CDN 返回了 brotli 压缩（当前未支持）—— 请把这条反馈给我".into());
    } else {
        raw.to_vec()
    };
    // 防御性去 BOM（serde 遇到 BOM 会直接报 expected value）
    Ok(if out.starts_with(&[0xEF, 0xBB, 0xBF]) { out[3..].to_vec() } else { out })
}

fn norm_base(u: &str) -> String {
    if u.ends_with('/') { u.to_string() } else { format!("{}/", u) }
}

/// 拉配置索引（含 CDN 列表）
fn fetch_config(app: &tauri::AppHandle, g: &KuroGame) -> Result<(KuroConfig, Vec<String>, String), String> {
    let (api, gid, key) = resolve_endpoint(app, g);
    if gid.is_empty() || key.is_empty() {
        return Err(format!(
            "{} 的分发常量还没接入（需要 gameId + appId_appKey）。\n→ 拿到后填进 channels.json 的 kuro_games.{} 即可，不用重编译。",
            g.name, g.id));
    }
    let url = format!("{}/{}/{}/index.json", api.trim_end_matches('/'), gid, key);
    println!("[kuro] 配置索引: {}", url);
    let r = meta_get(&url)?;
    if !r.status().is_success() {
        return Err(format!("配置索引 HTTP {}（gameId/appKey 可能已更换，改 channels.json）", r.status()));
    }
    let body = read_body(r)?;
    let idx: KuroIndex = serde_json::from_slice(&body)
        .map_err(|e| format!("解析配置失败: {}（原始 {} 字节）", e, body.len()))?;
    if idx.default.config.version.is_empty() {
        return Err("配置里没有 version（端点族可能变了）".into());
    }
    let cdns: Vec<String> = idx.default.cdn_list.iter().map(|c| norm_base(&c.url)).filter(|u| u.len() > 8).collect();
    if cdns.is_empty() { return Err("配置里没有可用 CDN".into()); }
    Ok((idx.default.config, cdns, url))
}

/// 拉文件清单（{cdn}{indexFile}）
fn fetch_manifest(cfg: &KuroConfig, cdn: &str) -> Result<Vec<KuroEntry>, String> {
    let url = format!("{}{}", cdn, cfg.index_file.trim_start_matches('/'));
    println!("[kuro] 文件清单: {}", url);
    let r = meta_get(&url)?;
    if !r.status().is_success() { return Err(format!("清单 HTTP {}", r.status())); }
    let raw = read_body(r)?;
    // 清单本身也要验（官方给了 indexFileMd5）：不符说明 CDN 给了旧版本，宁可失败也别按旧清单下
    if !cfg.index_md5.is_empty() {
        let got = format!("{:x}", md5::compute(&raw));
        if !got.eq_ignore_ascii_case(&cfg.index_md5) {
            return Err(format!("清单 md5 不符（期望 {} 实际 {}）→ 这个 CDN 可能还是旧版本，换 CDN 重试", cfg.index_md5, got));
        }
        println!("[kuro] 清单 md5 校验通过: {}", got);
    }
    let m: KuroManifest = serde_json::from_slice(&raw).map_err(|e| format!("解析清单失败: {}", e))?;
    if m.resource.is_empty() { return Err("清单里没有文件条目".into()); }
    Ok(m.resource)
}

/// 只保留 URL 安全字符（清单里有 "Wuthering Waves.exe" 这类带空格的路径）
fn url_encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len() + 8);
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 文件真实地址：{cdn}{fromFolder | baseUrl}{dest}
fn file_url(cdn: &str, cfg: &KuroConfig, e: &KuroEntry) -> String {
    let folder = e.from_folder.clone().unwrap_or_else(|| cfg.base_url.clone());
    format!("{}{}{}", cdn, folder.trim_start_matches('/'), url_encode_path(&e.dest))
}

// ================= 本地状态 =================
fn local_version_of(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join(LOCAL_VERSION_FILE)).ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
fn write_local_version(root: &Path, ver: &str) {
    let _ = std::fs::write(root.join(LOCAL_VERSION_FILE), ver);
}
/// 本地文件大小（不存在 = None）
fn local_size(root: &Path, dest: &str) -> Option<u64> {
    std::fs::metadata(root.join(dest.replace('/', "\\"))).ok().map(|m| m.len())
}
fn file_md5(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut ctx = md5::Context::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 { break; }
        ctx.consume(&buf[..n]);
    }
    Ok(format!("{:x}", ctx.compute()))
}

/// 关键文件（官方 keyFileCheckList 的核心项）：拿它判断"本地到底是哪个版本"
const KEY_FILES: &[&str] = &[
    "Wuthering Waves.exe",
    "Client/Binaries/Win64/Client-Win64-Shipping.exe",
];

/// 差异集（**只比 size**）：首次安装/快速校验用。
/// ⚠️ 同尺寸但内容变了的文件会被它判成"一致" —— 所以下载/修复一律走 `diff_entries_deep`（内容级）。
fn diff_entries(root: &Path, manifest: &[KuroEntry]) -> Vec<KuroEntry> {
    manifest.iter()
        .filter(|e| !e.dest.is_empty() && !e.dest.ends_with('/') && e.size > 0)
        .filter(|e| local_size(root, &e.dest) != Some(e.size))
        .cloned()
        .collect()
}

// ================= 内容级对账（同尺寸不同内容也要发现）=================
/// 官方清单给大文件带了 100MiB 分块的逐块 md5（chunkInfos），这里就用它做**内容级**核对：
/// ① 能发现"同尺寸不同内容"（原地改配置、同尺寸 pak 重打包都能抓）；
/// ② 能只重下真正坏掉的那几块，而不是整个 18GB 的 pak（真正的分块级增量）。
struct FileDiff {
    entry: KuroEntry,
    /// 本地根本没有
    missing: bool,
    /// 本地长度 != 官方 size（需要截断/补长）
    size_mismatch: bool,
    /// 内容不符的分块（本地存在时的精确坏块表）
    bad_chunks: Vec<KuroChunk>,
    /// 实际需要下载的**压缩**字节（进度条、空间预检、报告都用它）
    need_bytes: u64,
}

/// 内容级核对单个文件。Ok(None) = 完全一致（跳过）；cancel → Err("已取消…")
fn verify_local_file(root: &Path, e: &KuroEntry, cancel: &AtomicBool) -> Result<Option<FileDiff>, String> {
    let p = root.join(e.dest.replace('/', "\\"));
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(_) => {
            // 文件是裸传（官方 size 就是落盘/下载字节数），整文件缺失 = 下 e.size
            return Ok(Some(FileDiff { entry: e.clone(), missing: true, size_mismatch: true, bad_chunks: Vec::new(), need_bytes: e.size }));
        }
    };
    let local_len = meta.len();
    let size_mismatch = local_len != e.size;

    // 小文件（官方没给分块信息）：整文件 md5
    if e.chunk_infos.is_empty() {
        if !size_mismatch && (e.md5.is_empty() || file_md5(&p).map(|h| h.eq_ignore_ascii_case(&e.md5)).unwrap_or(false)) {
            return Ok(None);
        }
        return Ok(Some(FileDiff { entry: e.clone(), missing: false, size_mismatch, bad_chunks: Vec::new(), need_bytes: e.size }));
    }

    // 大文件：逐块核对官方 md5（短读/文件比块短都算这一块坏）
    // ⚠️ 打不开的文件（被游戏占用/权限）不能让它把整盘对账带崩：按"内容不符"处理，
    //    交给下载阶段去逐文件报错（那里有清晰的 per-file 错误信息）。
    let mut f = match std::fs::File::open(&p) {
        Ok(f) => f,
        Err(x) => {
            println!("[kuro] 打不开 {}（按内容不符处理）: {}", e.dest, x);
            let need: u64 = e.chunk_infos.iter().map(|c| c.end.saturating_sub(c.start) + 1).sum();
            return Ok(Some(FileDiff {
                entry: e.clone(), missing: false, size_mismatch,
                bad_chunks: e.chunk_infos.clone(), need_bytes: need }));
        }
    };
    let mut bad: Vec<KuroChunk> = Vec::new();
    let mut need_bytes = 0u64;
    for c in &e.chunk_infos {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Err("已取消（对账中断，未改动任何文件）".into());
        }
        let len = c.end.saturating_sub(c.start) + 1;
        let mut ok = local_len >= c.end + 1;
        if ok && len > 0 {
            let mut buf = vec![0u8; len as usize];
            ok = match (f.seek(SeekFrom::Start(c.start)), f.read_exact(&mut buf)) {
                (Ok(_), Ok(_)) => c.md5.is_empty() || format!("{:x}", md5::compute(&buf)).eq_ignore_ascii_case(&c.md5),
                _ => false,
            };
        }
        if !ok {
            // chunkInfos 里没有压缩大小（只有 start/end/md5）：文件是裸传，块的下载量 ≈ 块长度
            need_bytes += len;
            bad.push(c.clone());
        }
    }
    if bad.is_empty() && !size_mismatch { return Ok(None); }
    // 所有块都对、只是长度不对 → 不用下载，截断即可（need_bytes 归零）
    if bad.is_empty() { need_bytes = 0; }
    Ok(Some(FileDiff { entry: e.clone(), missing: false, size_mismatch, bad_chunks: bad, need_bytes }))
}

/// 内容级差异集（下载 / 修复 / 深度校验都走这里）。回调报进度：(已核对文件数, 总数)
fn diff_entries_deep<F: FnMut(usize, usize)>(
    root: &Path,
    manifest: &[KuroEntry],
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<Vec<FileDiff>, String> {
    let cand: Vec<&KuroEntry> = manifest.iter()
        .filter(|e| !e.dest.is_empty() && !e.dest.ends_with('/') && e.size > 0)
        .collect();
    let total = cand.len();
    let mut out = Vec::new();
    for (i, e) in cand.iter().enumerate() {
        if i % 20 == 0 { on_progress(i, total); }
        if cancel.load(AtomicOrdering::Relaxed) {
            return Err("已取消（对账中断，未改动任何文件）".into());
        }
        if let Some(d) = verify_local_file(root, e, cancel)? { out.push(d); }
    }
    on_progress(total, total);
    Ok(out)
}

// ================= 下载引擎 =================
/// 进度共享态：下载线程只更新计数，由**独立的上报线程**按固定节拍（1 秒）发事件。
/// 以前是"跟着读盘循环发"，观感就是"一个文件才跳一次"；现在与下载完全解耦，稳定 1Hz，
/// 速度用轻度平滑（0.6 新 + 0.4 旧）避免数字乱跳。
struct ProgShared {
    downloaded: AtomicU64,
    total: u64,
    current: Mutex<String>,
    stop: AtomicBool,
}

impl ProgShared {
    fn new(total: u64) -> Arc<Self> {
        Arc::new(Self {
            downloaded: AtomicU64::new(0),
            total,
            current: Mutex::new("准备下载…".to_string()),
            stop: AtomicBool::new(false),
        })
    }
    fn add(&self, n: u64) {
        self.downloaded.fetch_add(n, AtomicOrdering::Relaxed);
    }
    fn done(&self) -> u64 {
        self.downloaded.load(AtomicOrdering::Relaxed)
    }
    fn set_current(&self, s: &str) {
        if let Ok(mut c) = self.current.lock() { *c = s.to_string(); }
    }
    fn cur(&self) -> String {
        self.current.lock().map(|c| c.clone()).unwrap_or_default()
    }
    /// 固定 1 秒节拍上报（第一帧立刻发），直到 stop 置位
    fn spawn_reporter(self: &Arc<Self>, app: tauri::AppHandle, game_id: String) -> std::thread::JoinHandle<()> {
        let sh = self.clone();
        std::thread::spawn(move || {
            let mut last_bytes = 0u64;
            let mut last = Instant::now();
            let mut smooth = 0f64;
            loop {
                let done = sh.done();
                let now = Instant::now();
                let el = now.duration_since(last).as_secs_f64();
                let inst = if el > 0.0 { done.saturating_sub(last_bytes) as f64 / el } else { 0.0 };
                smooth = if smooth <= 0.0 { inst } else { inst * 0.6 + smooth * 0.4 };
                let speed = smooth as u64;
                last_bytes = done;
                last = now;
                let eta = if speed > 0 { Some(sh.total.saturating_sub(done) / speed) } else { None };
                let _ = app.emit("download-progress", DownloadProgress {
                    game_id: game_id.clone(), downloaded: done, total: sh.total,
                    speed, eta_seconds: eta, status: "downloading".into(),
                });
                let _ = app.emit("patch-progress", crate::platforms::mihoyo::PatchProgress {
                    game_id: game_id.clone(), done, total: sh.total,
                    current: sh.cur(), status: "patching".into(),
                });
                if sh.stop.load(AtomicOrdering::Relaxed) { break; }
                std::thread::sleep(Duration::from_millis(1000));
            }
        })
    }
}

/// 收工：停上报线程（最多等它 1 秒把最后一帧发完）
fn stop_reporter(sh: &Arc<ProgShared>, reporter: std::thread::JoinHandle<()>) {
    sh.stop.store(true, AtomicOrdering::Relaxed);
    let _ = reporter.join();
}

/// 下载并校验单个文件（分块续传 → 整文件 md5 → 落位）
/// 下载/修复单个文件：
///   ① 本地缺 → 空底稿，从零分块下（仍是断点续传）
///   ② 本地有但内容不符 → **先复制本地当底稿，只补坏块**（分块级增量，不用重下整个 18GB pak）
///   ③ 上次中断留下的 .qkuro → 直接当续跑底稿，逐块校验跳过已好的
fn download_entry(
    cdn: &str,
    cfg: &KuroConfig,
    diff: &FileDiff,
    root: &Path,
    sh: &Arc<ProgShared>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let e = &diff.entry;
    sh.set_current(&e.dest);
    let target = root.join(e.dest.replace('/', "\\"));
    if let Some(par) = target.parent() { std::fs::create_dir_all(par).map_err(|x| x.to_string())?; }
    let tmp = PathBuf::from(format!("{}.{}", target.display(), TMP_EXT));
    let url = file_url(cdn, cfg, e);

    // 分块表：官方给就用官方的（100MiB 一块 + 逐块 md5），没给就整文件一块
    let chunks: Vec<KuroChunk> = if e.chunk_infos.is_empty() {
        vec![KuroChunk { start: 0, end: e.size.saturating_sub(1), md5: e.md5.clone() }]
    } else {
        e.chunk_infos.clone()
    };

    // 底稿选择
    let resume = tmp.exists();
    let mut only: Option<Vec<KuroChunk>> = None;
    if !resume && !diff.missing {
        std::fs::copy(&target, &tmp).map_err(|x| format!("复制本地文件做底稿失败 {}: {}", e.dest, x))?;
        // ⚠️ 只有"精确坏块表非空"时才启用跳过：小文件走整文件 md5 分支、bad_chunks 恒为空，
        //    那时若也设成 Some(空表)，下面会把所有块都 continue 掉（一个字节不下）→ 必然校验失败。
        if !diff.bad_chunks.is_empty() {
            only = Some(diff.bad_chunks.clone());
        }
    } else if !resume {
        let _ = std::fs::remove_file(&tmp);
    }

    let mut f = std::fs::OpenOptions::new().create(true).read(true).write(true)
        .open(&tmp).map_err(|x| format!("开临时文件失败 {}: {}", tmp.display(), x))?;
    // 长度对齐（只有"块全对、单纯长度不符"的情况才会走到这）
    if diff.size_mismatch && !diff.missing {
        f.set_len(e.size).map_err(|x| format!("{} 调整长度失败: {}", e.dest, x))?;
    }
    let mut have = f.metadata().map(|m| m.len()).unwrap_or(0);
    let client = chunk_client();

    for c in &chunks {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Err("已取消（已下好的分块保留在 .qkuro 里，重跑会接着下）".into());
        }
        // "只补坏块"模式：其它块在底稿里是可信的，直接跳过
        if let Some(list) = &only {
            if !list.iter().any(|x| x.start == c.start && x.end == c.end) { continue; }
        }
        let len = c.end.saturating_sub(c.start) + 1;
        if len == 0 { continue; }

        // ① 已下好？读回来校验分块 md5（续跑模式才需要；底稿复制模式下坏块必然要重下）
        if only.is_none() && have >= c.end + 1 {
            let mut buf = vec![0u8; len as usize];
            let ok = match (f.seek(SeekFrom::Start(c.start)), f.read_exact(&mut buf)) {
                (Ok(_), Ok(_)) => c.md5.is_empty() || format!("{:x}", md5::compute(&buf)).eq_ignore_ascii_case(&c.md5),
                _ => false,
            };
            if ok {
                sh.add(len);
                continue;
            }
        }

        // ② 拉这一块（Range；服务端不支持就退回整文件顺序下）
        let mut resp = client.get(&url)
            .header(reqwest::header::RANGE, format!("bytes={}-{}", c.start, c.end))
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send().map_err(|x| format!("{} 分块请求失败: {}", e.dest, x))?;
        let partial = resp.status().as_u16() == 206;
        if !resp.status().is_success() {
            return Err(format!("{} HTTP {} ({}:{})", e.dest, resp.status(), c.start, c.end));
        }
        // 内容被压缩过就没法按偏移写了（Range + gzip 会让偏移全错），宁可失败也别写坏文件
        if let Some(enc) = resp.headers().get(reqwest::header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()) {
            if !enc.eq_ignore_ascii_case("identity") && !enc.is_empty() {
                return Err(format!("{} 返回了 Content-Encoding: {}（无法按偏移落盘）", e.dest, enc));
            }
        }
        if !partial && c.start > 0 {
            return Err(format!("{} 服务端不支持 Range（第 {} 块）", e.dest, c.start / (100 * 1024 * 1024)));
        }
        let mut hasher = md5::Context::new();
        let mut buf = vec![0u8; 1024 * 1024];
        let mut written: u64 = 0;
        if partial { f.seek(SeekFrom::Start(c.start)).map_err(|x| x.to_string())?; }
        else { f.seek(SeekFrom::Start(0)).map_err(|x| x.to_string())?; }
        loop {
            if cancel.load(AtomicOrdering::Relaxed) {
                let _ = f.flush();
                return Err("已取消（已下好的分块保留在 .qkuro 里，重跑会接着下）".into());
            }
            let n = resp.read(&mut buf).map_err(|x| format!("{} 读取失败: {}", e.dest, x))?;
            if n == 0 { break; }
            hasher.consume(&buf[..n]);
            f.write_all(&buf[..n]).map_err(|x| x.to_string())?;
            written += n as u64;
            sh.add(n as u64);
            if written >= len { break; }
        }
        f.flush().map_err(|x| x.to_string())?;
        have = have.max(f.metadata().map(|m| m.len()).unwrap_or(have));
        let got = format!("{:x}", hasher.compute());
        if !c.md5.is_empty() && !got.eq_ignore_ascii_case(&c.md5) {
            // 这一块坏了：截断到块起点，让下次重下
            let _ = f.set_len(c.start);
            return Err(format!("{} 第 {} 块 md5 不符（期望 {} 实际 {}），已丢弃该块", e.dest, c.start / (100 * 1024 * 1024), c.md5, got));
        }
    }
    f.flush().map_err(|x| x.to_string())?;
    drop(f);

    // ③ 整文件 md5
    let whole = file_md5(&tmp)?;
    if !e.md5.is_empty() && !whole.eq_ignore_ascii_case(&e.md5) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{} 整文件 md5 不符（期望 {} 实际 {}），已删除重下", e.dest, e.md5, whole));
    }

    // ④ 落位：旧文件先改名 .qold（零拷贝、可回滚），再 rename 新文件，最后删备份
    let bak = PathBuf::from(format!("{}.{}", target.display(), BAK_EXT));
    if target.exists() {
        let _ = std::fs::remove_file(&bak);
        std::fs::rename(&target, &bak).map_err(|x| format!("移开旧文件失败: {}", x))?;
    }
    match std::fs::rename(&tmp, &target) {
        Ok(_) => { let _ = std::fs::remove_file(&bak); Ok(()) }
        Err(x) => {
            // 落位失败：把旧文件还原回去，别把游戏搞成半截
            if bak.exists() { let _ = std::fs::rename(&bak, &target); }
            Err(format!("{} 落位失败: {}", e.dest, x))
        }
    }
}

/// 对外入口：下载 / 差分更新 / 修复 都走这里（引擎自己算差异集）
pub fn kuro_download(app: &tauri::AppHandle, game_id: &str, dest: &str, cancel: Arc<AtomicBool>) -> Result<String, String> {
    let a = app.clone();
    let g = game_id.to_string();
    let d = dest.to_string();
    on_worker(move || kuro_download_inner(&a, &g, &d, cancel))
}

fn kuro_download_inner(app: &tauri::AppHandle, game_id: &str, dest: &str, cancel: Arc<AtomicBool>) -> Result<String, String> {
    let g = game_def(game_id).ok_or("未知的库洛游戏")?;
    let (cfg, cdns, _api) = fetch_config(app, g)?;
    let mut cdn = cdns[0].clone();
    let mut manifest = fetch_manifest(&cfg, &cdn)?;
    println!("[kuro] 远程版本 {} / 清单 {} 个文件", cfg.version, manifest.len());

    // 目标根目录：优先用户选的目录（首次安装），否则用已绑定目录（更新/修复）
    let bound = crate::load_config(app).get(game_id).cloned();
    let root = if dest.trim().is_empty() {
        bound.clone().map(PathBuf::from).ok_or("没有指定安装目录，且该游戏未绑定目录")?
    } else {
        PathBuf::from(dest)
    };
    std::fs::create_dir_all(&root).map_err(|e| format!("创建目录失败: {}", e))?;
    println!("[kuro] 安装根目录: {}", root.display());

    // 内容级对账：不只看 size —— 同尺寸但内容变了的（原地改配置、同尺寸 pak 重打包）也要抓出来。
    // 首次安装本地没文件，这一步几乎是瞬间的。
    println!("[kuro] 内容级对账中（官方分块 md5，{} 个文件）…", manifest.len());
    let queue: Vec<FileDiff> = {
        let a = app.clone();
        let gid = game_id.to_string();
        let mut last = Instant::now();
        diff_entries_deep(&root, &manifest, &cancel, |i, t| {
            if last.elapsed().as_millis() >= 300 || i == t {
                let _ = a.emit("download-progress", DownloadProgress {
                    game_id: gid.clone(), downloaded: i as u64, total: t as u64,
                    speed: 0, eta_seconds: None, status: "verifying".into() });
                last = Instant::now();
            }
        })?
    };
    let need: u64 = queue.iter().map(|d| d.need_bytes).sum();
    let n_missing = queue.iter().filter(|d| d.missing).count();
    println!("[kuro] 对账结果：需处理 {} / {} 个文件（缺失 {} ｜ 内容或长度不符 {}）｜ 实际需下载 {}",
        queue.len(), manifest.len(), n_missing, queue.len() - n_missing, crate::fmt_size(need));

    if queue.is_empty() {
        write_local_version(&root, &cfg.version);
        let _ = app.emit("download-progress", DownloadProgress {
            game_id: game_id.into(), downloaded: 0, total: 0, speed: 0, eta_seconds: Some(0),
            status: format!("done:{}", root.display()) });
        return Ok(format!("本地已与官方 v{} 逐块一致，无需下载", cfg.version));
    }

    // 空间预检：下载量 + 需要复制底稿的最大文件（分块修复要先复制一份）+ 5GB 余量
    let copy_need: u64 = queue.iter().filter(|d| !d.missing).map(|d| d.entry.size).max().unwrap_or(0);
    if let Some(free) = crate::platforms::mihoyo::free_space_of(&root) {
        if free < need + copy_need + 5 * 1024 * 1024 * 1024 {
            return Err(format!(
                "目标盘空间不足：需要约 {}（下载 {} + 复制底稿最多 {}）+ 5GB 余量，当前可用 {}。\n→ 换个盘，或先清理空间。",
                crate::fmt_size(need + copy_need), crate::fmt_size(need), crate::fmt_size(copy_need), crate::fmt_size(free)));
        }
    }

    // 小文件先下（早一点有"装上了"的实感），大 pak 在后面
    let mut queue = queue;
    queue.sort_by_key(|d| d.entry.size);

    // 进度：共享计数 + 独立上报线程（固定 1 秒一拍，与下载循环解耦）
    let sh = ProgShared::new(need);
    let mut reporter = Some(sh.spawn_reporter(app.clone(), game_id.to_string()));

    let total_files = queue.len();
    let mut done_files = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for d in &queue {
        if cancel.load(AtomicOrdering::Relaxed) {
            if let Some(r) = reporter.take() { stop_reporter(&sh, r); }
            return Err(format!("已取消（已完成 {} / {} 个文件，重跑会接着下）", done_files, total_files));
        }
        let mut last_err = String::new();
        let mut ok = false;
        // 每个文件最多换 2 个 CDN 重试
        for attempt in 0..2usize {
            match download_entry(&cdn, &cfg, d, &root, &sh, &cancel) {
                Ok(_) => { ok = true; break; }
                Err(err) => {
                    last_err = err.clone();
                    if err.contains("已取消") {
                        if let Some(r) = reporter.take() { stop_reporter(&sh, r); }
                        return Err(err);
                    }
                    println!("[kuro] {} 第 {} 次失败: {}", d.entry.dest, attempt + 1, err);
                    if attempt == 0 && cdns.len() > 1 {
                        cdn = cdns[(cdns.iter().position(|c| c == &cdn).unwrap_or(0) + 1) % cdns.len()].clone();
                        println!("[kuro] 换 CDN 重试: {}", cdn);
                        // 换 CDN 前重拉一次清单（不同 CDN 的清单理论上同版本，保险起见）
                        if let Ok(m2) = fetch_manifest(&cfg, &cdn) { if m2.len() == manifest.len() { manifest = m2; } }
                    }
                }
            }
        }
        if !ok {
            failed.push(format!("{} → {}", d.entry.dest, last_err));
            // 连续失败就别硬撑了，把已完成的保住，报清楚
            if failed.len() >= 5 {
                if let Some(r) = reporter.take() { stop_reporter(&sh, r); }
                return Err(format!(
                    "连续 {} 个文件失败，已停止（已下好的文件都在，重跑会接着下）。\n最近错误：\n{}",
                    failed.len(), failed.join("\n")));
            }
            continue;
        }
        done_files += 1;
        sh.set_current(&d.entry.dest);
    }

    if !failed.is_empty() {
        if let Some(r) = reporter.take() { stop_reporter(&sh, r); }
        return Err(format!("有 {} 个文件没下成功（其余已完成，重跑会只补这些）：\n{}", failed.len(), failed.join("\n")));
    }

    if let Some(r) = reporter.take() { stop_reporter(&sh, r); }
    write_local_version(&root, &cfg.version);
    let _ = app.emit("patch-progress", crate::platforms::mihoyo::PatchProgress {
        game_id: game_id.into(), done: need, total: need, current: String::new(), status: "patching".into() });
    let _ = app.emit("download-progress", DownloadProgress {
        game_id: game_id.into(), downloaded: need, total: need, speed: 0, eta_seconds: Some(0),
        status: format!("done:{}", root.display()) });
    Ok(format!("{} 完成：v{}，本次下载 {} 个文件 / {}", g.name, cfg.version, done_files, crate::fmt_size(need)))
}

/// 校验：fast = 只比 size（秒级，但漏"同尺寸不同内容"）；deep = 内容级（官方 100MiB 分块 md5）
pub fn kuro_verify(app: &tauri::AppHandle, game_id: &str, deep: bool) -> Result<crate::platforms::mihoyo::VerifyReport, String> {
    let a = app.clone();
    let g = game_id.to_string();
    on_worker(move || kuro_verify_inner(&a, &g, deep))
}

fn kuro_verify_inner(app: &tauri::AppHandle, game_id: &str, deep: bool) -> Result<crate::platforms::mihoyo::VerifyReport, String> {
    let g = game_def(game_id).ok_or("未知的库洛游戏")?;
    let root = PathBuf::from(crate::load_config(app).get(game_id).cloned().ok_or("游戏未绑定目录")?);
    let (cfg, cdns, _) = fetch_config(app, g)?;
    let manifest = fetch_manifest(&cfg, &cdns[0])?;
    let cancel = crate::cancel_flag(game_id);
    cancel.store(false, AtomicOrdering::SeqCst);
    let total = manifest.iter()
        .filter(|e| !e.dest.is_empty() && !e.dest.ends_with('/') && e.size > 0).count();

    let (missing, size_bad, md5_bad, broken_bytes, sample) = if deep {
        // 内容级：逐块（大文件）或整文件（小文件）核对官方 md5 —— 同尺寸不同内容也能抓出来
        let started = Instant::now();
        let diffs = diff_entries_deep(&root, &manifest, &cancel, |i, t| {
            if i % 20 == 0 || i == t {
                let el = started.elapsed().as_secs_f64();
                let speed = if el > 0.0 { (i as f64 / el) as u64 } else { 0 };
                let _ = app.emit("download-progress", DownloadProgress {
                    game_id: game_id.into(), downloaded: i as u64, total: t as u64, speed,
                    eta_seconds: if speed > 0 { Some((t.saturating_sub(i) as u64) / speed) } else { None },
                    status: "verifying".into() });
                let _ = app.emit("verify-progress", crate::platforms::mihoyo::PatchProgress {
                    game_id: game_id.into(), done: i as u64, total: t as u64,
                    current: format!("内容级对账 {}/{}", i, t), status: "patching".into() });
            }
        })?;
        let missing = diffs.iter().filter(|d| d.missing).count();
        let size_bad = diffs.iter().filter(|d| !d.missing && d.size_mismatch).count();
        let md5_bad = diffs.iter().filter(|d| !d.missing && !d.size_mismatch).count();
        let bytes: u64 = diffs.iter().map(|d| d.need_bytes).sum();
        let sample: Vec<String> = diffs.iter().take(30).map(|d| {
            let why = if d.missing { "缺失".to_string() }
                else if d.size_mismatch { "大小不符".to_string() }
                else { format!("内容不符（同尺寸！坏 {} 块）", d.bad_chunks.len()) };
            format!("[{}] {} ({})", why, d.entry.dest, crate::fmt_size(d.entry.size))
        }).collect();
        (missing, size_bad, md5_bad, bytes, sample)
    } else {
        // 快速：只比 size（老口径）+ 两个关键 exe 的 md5（便宜，能挡"版本对但被改"）
        let changed = diff_entries(&root, &manifest);
        let missing = changed.iter().filter(|e| local_size(&root, &e.dest).is_none()).count();
        let size_bad = changed.len() - missing;
        let mut key_bad = 0usize;
        for k in KEY_FILES {
            if let Some(e) = manifest.iter().find(|e| e.dest == *k) {
                let p = root.join(k.replace('/', "\\"));
                if file_md5(&p).map(|h| !h.eq_ignore_ascii_case(&e.md5)).unwrap_or(true) { key_bad += 1; }
            }
        }
        let bytes: u64 = changed.iter().map(|e| e.size).sum();
        let sample: Vec<String> = changed.iter().take(30)
            .map(|e| format!("[缺失/大小不符] {} ({})", e.dest, crate::fmt_size(e.size))).collect();
        (missing, size_bad, key_bad, bytes, sample)
    };

    let local_version = local_version_of(&root).unwrap_or_default();
    let version_match = missing + size_bad + md5_bad == 0;
    let mut sample = sample;
    if sample.is_empty() { sample.push("✅ 与官方清单一致".into()); }
    Ok(crate::platforms::mihoyo::VerifyReport {
        manifest: format!("库洛官方清单（{} / {}，{} 个文件，{}；口径：{}）",
            g.name, cfg.version, total, crate::fmt_size(cfg.size),
            if deep { "内容级（官方分块 md5）" } else { "快速（只比大小 + 关键 exe md5）" }),
        total,
        ok: total.saturating_sub(missing + size_bad + md5_bad),
        missing, size_bad, md5_bad, deep,
        broken_bytes, sample,
        res_list_url: None, local_version, remote_version: cfg.version.clone(),
        version_match, sophon_managed: false, sophon_tag: None,
    })
}

pub struct KuroPlatform;

impl GamePlatform for KuroPlatform {
    fn id(&self) -> &str { "kuro" }
    fn name(&self) -> &str { "库洛游戏" }
    fn level(&self) -> PlatformLevel { PlatformLevel::Full }
    fn game_ids(&self) -> Vec<&'static str> { KURO_GAMES.iter().map(|g| g.id).collect() }
    /// 没有已知的官启 URI：兜底给官网（open_official 会走"打开网页"这条路，而不是报错）
    fn official_launcher(&self) -> Option<String> { Some(KURO_GAMES[0].website.into()) }
    fn official_website(&self, game_id: &str) -> Option<String> {
        game_def(game_id).map(|g| g.website.to_string())
    }
    fn exe_candidates(&self, game_id: &str) -> Vec<&'static str> {
        game_def(game_id).map(|g| g.exes.to_vec()).unwrap_or_default()
    }

    fn detect(&self, app: &tauri::AppHandle) -> Vec<GameInfo> {
        let cfg = crate::load_config(app);
        let entries = crate::scan_uninstall_registry();
        let mut out = Vec::new();
        for g in KURO_GAMES {
            let mut found: Option<(String, PathBuf)> = None;
            // ① 已绑定目录
            if let Some(dir) = cfg.get(g.id) {
                if let Some(exe) = crate::find_game_exe(Path::new(dir), g.exes) { found = Some((dir.clone(), exe)); }
            }
            // ② 注册表卸载项
            if found.is_none() {
                for (name, loc) in &entries {
                    if crate::is_cloud_entry(name) || !g.hints.iter().any(|h| name.contains(h)) { continue; }
                    let dir = Path::new(loc);
                    if !dir.exists() { continue; }
                    if let Some(exe) = crate::find_game_exe(dir, g.exes) { found = Some((loc.clone(), exe)); break; }
                }
            }
            // ③ 常见路径
            if found.is_none() {
                for root in common_roots() {
                    for hint in g.hints {
                        let p = root.join(hint);
                        if !p.is_dir() { continue; }
                        if let Some(exe) = crate::find_game_exe(&p, g.exes) { found = Some((p.display().to_string(), exe)); break; }
                    }
                    if found.is_some() { break; }
                }
            }
            out.push(match found {
                Some((dir, exe)) => GameInfo {
                    id: g.id.into(), name: g.name.into(), installed: true,
                    path: Some(dir.clone()), exe: Some(exe.to_string_lossy().into_owned()),
                    local_version: local_version_of(Path::new(&dir)), platform: "kuro".into(),
                    platform_level: PlatformLevel::Full, launcher_uri: self.official_launcher(),
                },
                None => GameInfo {
                    id: g.id.into(), name: g.name.into(), installed: false,
                    path: None, exe: None, local_version: None, platform: "kuro".into(),
                    platform_level: PlatformLevel::Full, launcher_uri: self.official_launcher(),
                },
            });
        }
        out
    }

    fn launch_direct(&self, game: &GameInfo) -> Result<String, String> {
        let exe = game.exe.clone().ok_or("缺少 exe 路径")?;
        let dir = game.path.clone().ok_or("缺少安装目录")?;
        std::process::Command::new(&exe).current_dir(&dir).spawn().map_err(|e| e.to_string())?;
        Ok(format!("已直启: {}", exe))
    }

    fn remote_info(&self, app: &tauri::AppHandle, game_id: &str) -> Result<RemoteGameInfo, String> {
        let a = app.clone();
        let g = game_id.to_string();
        on_worker(move || remote_info_inner(&a, &g))
    }

    fn download(&self, app: &tauri::AppHandle, game_id: &str, dest: &str, _use_patch: bool, _allow_old: bool, cancel: Arc<AtomicBool>) -> Result<String, String> {
        kuro_download(app, game_id, dest, cancel)
    }
}

/// 查版本：配置索引 + 官方清单 + 本地对账（size 快扫 + 关键文件 md5 定版本）
fn remote_info_inner(app: &tauri::AppHandle, game_id: &str) -> Result<RemoteGameInfo, String> {
    let g = game_def(game_id).ok_or("未知的库洛游戏")?;
    let (cfg, cdns, api) = fetch_config(app, g)?;
        let manifest = fetch_manifest(&cfg, &cdns[0])?;
        let total: u64 = manifest.iter().map(|e| e.size).sum();

        // 本地对账：size 快扫（不做全量 md5，80GB 太慢）+ 关键文件 md5 定版本
        let root = crate::load_config(app).get(game_id).map(PathBuf::from);
        let (local_version, changed, key_same) = match &root {
            Some(r) if r.is_dir() => {
                let changed = diff_entries(r, &manifest);
                let mut key_same = true;
                for k in KEY_FILES {
                    if let Some(e) = manifest.iter().find(|e| e.dest == *k) {
                        match file_md5(&r.join(k.replace('/', "\\"))) {
                            Ok(h) if h.eq_ignore_ascii_case(&e.md5) => {}
                            _ => { key_same = false; }
                        }
                    }
                }
                (local_version_of(r), changed, key_same)
            }
            _ => (None, Vec::new(), false),
        };
        let changed_bytes: u64 = changed.iter().map(|e| e.size).sum();
        let installed = root.as_ref().map(|r| r.is_dir()).unwrap_or(false);

        let relation = if !installed {
            "fresh"
        } else if key_same && changed.is_empty() {
            "equal"
        } else {
            match local_version.as_deref().map(|lv| compare_versions(lv, &cfg.version)) {
                Some(Ordering::Equal) | None => "ahead",
                Some(Ordering::Less) => "ahead",
                Some(Ordering::Greater) => "behind",
            }
        };

        Ok(RemoteGameInfo {
            game_id: game_id.into(),
            latest_version: cfg.version.clone(),
            // 首次安装的"整包"就是清单全量
            package_url: format!("{}{}", cdns[0], cfg.index_file),
            package_size: total,
            parts: Vec::new(),
            patch_from: if installed { local_version.clone() } else { None },
            patch_size: changed_bytes,
            patch_parts: Vec::new(),
            local_version,
            version_relation: relation.into(),
            install_size: total,
            res_list_url: String::new(),
            announced_version: None,
            version_source: "库洛官方清单".into(),
            checked_at: (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs()).unwrap_or(0)),
            resolved_biz: g.game_id.into(),
            resolved_game_id: g.game_id.into(),
            launcher_id: api,
            launcher_id_source: if g.cn { "内置国服常量".into() } else { "内置国际服常量".into() },
        })
    }

/// 常见安装根（用于未绑定时自动发现）
fn common_roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for d in ["C", "D", "E", "F", "G"] {
        v.push(PathBuf::from(format!("{}:\\", d)));
        v.push(PathBuf::from(format!("{}:\\Program Files", d)));
        v.push(PathBuf::from(format!("{}:\\Games", d)));
        v.push(PathBuf::from(format!("{}:\\Game", d)));
    }
    v
}
