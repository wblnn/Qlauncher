use crate::platform::{
    compare_versions, DownloadProgress, GameInfo, GamePlatform, PlatformLevel,
    RemoteGameInfo, RemotePackagePart,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

const HYP_BASE_CN: &str = "https://hyp-api.mihoyo.com/hyp/hyp-connect/api";
const HYP_LAUNCHER_ID_CN: &str = "jGHBHlcOq1";
const HYP_LANGUAGE: &str = "zh-cn";

const MIHOYO_GAMES: &[(&str, &str, &str, &[&str])] = &[
    ("genshin", "原神", "hk4e_cn", &["YuanShen.exe", "GenshinImpact.exe"]),
    ("starrail", "崩坏：星穹铁道", "hkrpg_cn", &["StarRail.exe"]),
    ("zenless", "绝区零", "nap_cn", &["ZenlessZoneZero.exe"]),
];

#[derive(Deserialize)]
pub struct ChannelsFile {
    launcher_id: Option<String>,
    // P1：下载保命线（MB），0 或缺省 = 不限制
    download_limit_mb: Option<u64>,
    // 新增：下载缓冲区大小(MB)，默认 4MB。越大 CPU 占用越低
    download_buffer_mb: Option<u64>,
    // 新增：下载限速(MB/s)，0 或不填为不限速。限速会自动让出 CPU
    download_speed_limit_mbps: Option<u64>,
    // 新增：hpatchz.exe 路径，缺省 = 项目 src-tauri/hpatchz.exe
    hpatchz_path: Option<String>,
    /// 整包解压成功后是否删掉分卷（缺省 true = 保留；硬盘紧张时设 false 省空间）
    keep_volumes_after_extract: Option<bool>,
    /// launcher_id 候选列表（按优先级；每个都会被 getGames 验证后才采用）
    launcher_ids: Option<Vec<String>>,
    /// 渠道匹配规则：本地游戏键 → biz 前缀或中文名（米哈游改渠道名时改这里即可）
    biz_patterns: Option<HashMap<String, Vec<String>>>,

}

/// 配置三级来源：项目源码（开发期主配置，改文件即时生效）→ 用户目录（生产覆盖）→ 内置默认
fn channels_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("channels.json");
    if project.is_file() { return Some(project); }
    if let Ok(dir) = app.path().app_data_dir() {
        let user = dir.join("channels.json");
        if user.is_file() { return Some(user); }
    }
    None
}
fn read_channels(app: &tauri::AppHandle) -> ChannelsFile {
    if let Some(p) = channels_path(app) {
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(cfg) = serde_json::from_str::<ChannelsFile>(&s) {
                return cfg;
            }
        }
    }
    ChannelsFile { launcher_id: None, download_limit_mb: None, download_buffer_mb: None, download_speed_limit_mbps: None, hpatchz_path: None, keep_volumes_after_extract: None, launcher_ids: None, biz_patterns: None }
}

// ================= 版本源自适应：候选 launcher_id / 渠道匹配 / 公告推断 =================

fn epoch_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// 拼接口地址（带 cache-buster，避免边缘缓存给旧数据）。
/// extra 里的参数会拼成 `&k=v`，所以 game_id 这类查询参数必须走这里，不能塞进 path。
fn hyp_url(path: &str, launcher_id: &str, extra: &[(&str, &str)]) -> String {
    let mut u = format!("{}/{}?launcher_id={}&language={}&_={}", HYP_BASE_CN, path, launcher_id, HYP_LANGUAGE, epoch_ms());
    for (k, v) in extra { u.push_str(&format!("&{}={}", k, v)); }
    u
}

/// 从本机米哈游启动器日志里读当前 LauncherId（官方客户端自己打印的，是最贴近现实的来源）
fn local_launcher_id() -> Option<String> {
    let base = std::env::var("APPDATA").ok().map(|p| PathBuf::from(p).join("miHoYo").join("HYP"))?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.filter_map(|e| e.ok()) {
            let log = e.path().join("logs").join("launcher.log");
            if let Ok(m) = std::fs::metadata(&log) {
                if let Ok(t) = m.modified() {
                    if newest.as_ref().map(|(nt, _)| t > *nt).unwrap_or(true) { newest = Some((t, log)); }
                }
            }
        }
    }
    let (_, log) = newest?;
    let s = std::fs::read_to_string(log).ok()?;
    s.lines().rev()
        .find_map(|l| l.split("AppConfig.App.LauncherId:").nth(1))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// launcher_id 候选（按优先级去重）：channels.json → 本机 HoYoPlay 日志 → 内置常量
fn launcher_candidates(app: &tauri::AppHandle) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    fn push(id: &str, src: &str, out: &mut Vec<(String, String)>) {
        let id = id.trim().to_string();
        if !id.is_empty() && !out.iter().any(|(x, _)| *x == id) { out.push((id, src.to_string())); }
    }
    let cfg = read_channels(app);
    if let Some(ids) = cfg.launcher_ids { for id in ids { push(&id, "channels.json(launcher_ids)", &mut out); } }
    if let Some(id) = cfg.launcher_id { push(&id, "channels.json(launcher_id)", &mut out); }
    if let Some(id) = local_launcher_id() { push(&id, "本机 HoYoPlay 日志", &mut out); }
    push(HYP_LAUNCHER_ID_CN, "内置常量", &mut out);
    out
}

/// 逐个候选打 getGames，选第一个能用的；顺带拿到 biz↔game_id 映射
fn hyp_resolve(app: &tauri::AppHandle) -> Result<(String, String, Vec<HypGameEntry>), String> {
    let mut tried: Vec<String> = Vec::new();
    for (id, src) in launcher_candidates(app) {
        match hyp_get::<HypRoot<HypGamesData>>(&hyp_url("getGames", &id, &[])) {
            Ok(root) if root.retcode == 0 => {
                let games = root.data.map(|d| d.games).unwrap_or_default();
                if !games.is_empty() { return Ok((id, src, games)); }
                tried.push(format!("{} -> retcode=0 但没有游戏列表", id));
            }
            Ok(root) => tried.push(format!("{} -> retcode={} {}", id, root.retcode, root.message)),
            Err(e) => tried.push(format!("{} -> {}", id, e)),
        }
    }
    Err(format!("所有 launcher_id 候选都失败了：\n{}", tried.join("\n")))
}

/// 按匹配规则在 getGames 结果里找游戏（米哈游改渠道名/加马甲也能跟上）
fn match_biz(app: &tauri::AppHandle, game_id: &str, games: &[HypGameEntry]) -> Option<(String, String)> {
    let cfg = read_channels(app);
    let mut pats: Vec<String> = cfg.biz_patterns.as_ref().and_then(|m| m.get(game_id)).cloned().unwrap_or_default();
    if pats.is_empty() {
        pats = MIHOYO_GAMES.iter().find(|(id, _, _, _)| *id == game_id)
            .map(|(_, name, biz, _)| vec![biz.to_string(), name.to_string()])
            .unwrap_or_default();
    }
    let lower: Vec<String> = pats.iter().map(|p| p.to_lowercase()).collect();
    games.iter()
        .find(|g| { let b = g.biz.to_lowercase(); lower.iter().any(|p| !p.is_empty() && b.starts_with(p.as_str())) })
        .map(|g| (g.biz.clone(), g.id.clone()))
}

/// "4.6版本更新说明" 里的 4.6.0 → (4,6,0)
fn version_before_version_word(s: &str) -> Option<(u64, u64, u64)> {
    let bytes = s.as_bytes();
    let mut best: Option<(u64, u64, u64)> = None;
    let mut from = 0usize;
    while let Some(rel) = s[from..].find("版本") {
        let at = from + rel;
        let mut start = at;
        while start > 0 {
            let c = bytes[start - 1] as char;
            if c.is_ascii_digit() || c == '.' { start -= 1; } else { break; }
        }
        if start < at {
            let nums: Vec<u64> = s[start..at].split('.').filter_map(|x| x.parse::<u64>().ok()).collect();
            if nums.len() >= 2 && nums[0] >= 1 && nums[0] <= 30 {
                let v = (nums[0], nums[1], nums.get(2).copied().unwrap_or(0));
                if best.map(|b| v > b).unwrap_or(true) { best = Some(v); }
            }
        }
        from = at + "版本".len();
    }
    best
}

/// 只从"公告/资讯"里推断版本（活动/攻略类不算，避免抓到旧版本号）
fn announced_from_posts(posts: &[HypPost]) -> Option<String> {
    let mut best: Option<(u64, u64, u64)> = None;
    for p in posts {
        if p.post_type != "POST_TYPE_ANNOUNCE" && p.post_type != "POST_TYPE_INFO" { continue; }
        if let Some(v) = version_before_version_word(&p.title) {
            if best.map(|b| v > b).unwrap_or(true) { best = Some(v); }
        }
    }
    best.map(|(a, b, c)| format!("{}.{}.{}", a, b, c))
}

/// 保留旧签名（自检报告等处仍用它做"当前配置里写的 launcher_id"展示）
fn hyp_launcher_id(app: &tauri::AppHandle) -> (String, &'static str) {
    let cfg = read_channels(app);
    if let Some(id) = cfg.launcher_id.filter(|v| !v.trim().is_empty()) {
        return (id, "channels.json 覆盖");
    }
    (HYP_LAUNCHER_ID_CN.to_string(), "内置 CN 常量")
}

/// P1：下载字节上限，0 = 不限制
fn download_limit_bytes(app: &tauri::AppHandle) -> u64 {
    read_channels(app).download_limit_mb.unwrap_or(0) * 1024 * 1024
}

/// hpatchz.exe 位置：channels.json 覆盖 → 项目目录默认
fn hpatchz_exe(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let cfg = read_channels(app);
    if let Some(p) = cfg.hpatchz_path.filter(|v| !v.trim().is_empty()) {
        return Ok(PathBuf::from(p));
    }
    Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join("hpatchz.exe"))
}

/// 单文件 hdiff 合成：hpatchz <旧文件> <增量文件> <新文件>
fn run_hpatchz(app: &tauri::AppHandle, old: &Path, diff: &Path, new: &Path) -> Result<(), String> {
    let exe = hpatchz_exe(app)?;
    if !exe.is_file() {
        return Err(format!("找不到 hpatchz.exe: {}（去 HDiffPatch release 下载放到这里）", exe.display()));
    }
    let out = std::process::Command::new(&exe)
        .arg(old).arg(diff).arg(new)
        .output()
        .map_err(|e| format!("调用 hpatchz 失败: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("hpatchz 退出码 {:?}: {}", out.status.code(), String::from_utf8_lossy(&out.stderr)))
    }
}

// ================= 差分合成 (hdiff) =================
#[derive(Deserialize)]
struct HDiffMap { #[serde(default)] diff_map: Vec<HDiffEntry> }

#[derive(Serialize, Clone)]
pub struct PatchProgress {
    pub game_id: String,
    pub done: u64,
    pub total: u64,
    pub current: String,
    pub status: String, // patching | done:... | error:...
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct HDiffEntry {
    #[serde(default)] source_file_name: String,
    #[serde(default)] source_file_md5: String,
    #[serde(default)] source_file_size: u64,
    #[serde(default)] target_file_name: String,
    #[serde(default)] target_file_md5: String,
    #[serde(default)] target_file_size: u64,
    #[serde(default)] patch_file_name: String,
    #[serde(default)] patch_file_md5: String,
    #[serde(default)] patch_file_size: u64,
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

/// 递归收集 extracted 里的"直接替换文件"（排除 .hdiff 增量和两个元文件）
fn walk_replace_files(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() { walk_replace_files(&p, base, out); continue; }
            let fname = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            if fname == "hdiffmap.json" || fname == "deletefiles.txt" { continue; }
            if fname.ends_with(".hdiff") { continue; }
            out.push(p);
        }
    }
}

fn update_config_version(dir: &str, ver: &str) -> Result<(), String> {
    let p = Path::new(dir).join("config.ini");
    let s = std::fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for line in s.lines() {
        if line.trim().starts_with("game_version=") { out.push(format!("game_version={}", ver)); }
        else { out.push(line.to_string()); }
    }
    std::fs::write(&p, out.join("\r\n")).map_err(|e| e.to_string())?;
    Ok(())
}

/// 只认"长得像版本号"的字符串：1~4 段数字，如 4.4.0（挡掉上万行的清单文件）
fn is_version_like(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 24 { return false; }
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
}

/// 目标版本号三级来源：包内 pkg_version（仅当像版本号）→ 目录名 patch_a_b 的 b → 接口 latest_version
fn resolve_target_version(app: &tauri::AppHandle, game_id: &str, patch_dir: &str) -> Option<String> {
    if let Ok(s) = std::fs::read_to_string(Path::new(patch_dir).join("extracted").join("pkg_version")) {
        if is_version_like(&s) { return Some(s.trim().to_string()); }
    }
    if let Some(name) = Path::new(patch_dir).file_name().and_then(|s| s.to_str()) {
        if let Some(rest) = name.strip_prefix("patch_") {
            if let Some((_, to)) = rest.rsplit_once('_') {
                if is_version_like(to) { return Some(to.to_string()); }
            }
        }
    }
    let p = crate::platforms::platform_for_game(game_id)?;
    p.remote_info(app, game_id).ok().map(|i| i.latest_version).filter(|v| is_version_like(v))
}

// ================= 断点续跑 / 回滚 日志 (journal) =================
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PatchJournal {
    pub game_id: String,
    pub patch_dir: String,
    pub from_version: String,
    pub to_version: String,
    pub phase: String,          // preflight|synthesize|replace|delete|verify|commit|done|failed
    pub diff_total: usize,
    pub diff_done: usize,
    pub replace_total: usize,
    pub replace_done: usize,
    pub delete_total: usize,
    pub delete_done: usize,
    pub new_files: Vec<String>, // 本次新建的文件（绝对路径，回滚时删除）
    pub backups: Vec<String>,   // xxx.qold（回滚时改名还原）
    pub config_backup: Option<String>,
    pub message: String,
    pub updated_at: String,
    pub finished: bool,
}

impl PatchJournal {
    fn save(&mut self, app: &tauri::AppHandle) {
        self.updated_at = format!("{:?}", std::time::SystemTime::now());
        if let Ok(dir) = app.path().app_data_dir() {
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(s) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(dir.join(format!("qpatch_{}.json", self.game_id)), s);
            }
        }
    }
    fn clear(app: &tauri::AppHandle, game_id: &str) {
        if let Ok(dir) = app.path().app_data_dir() {
            let _ = std::fs::remove_file(dir.join(format!("qpatch_{}.json", game_id)));
        }
    }
}

fn load_journal(app: &tauri::AppHandle, game_id: &str) -> Option<PatchJournal> {
    let dir = app.path().app_data_dir().ok()?;
    let s = std::fs::read_to_string(dir.join(format!("qpatch_{}.json", game_id))).ok()?;
    serde_json::from_str::<PatchJournal>(&s).ok()
}

/// 给 UI 查：某游戏是否有未完成的更新（有 = 该显示「继续 / 回滚」）
pub fn patch_status(app: &tauri::AppHandle, game_id: &str) -> Option<PatchJournal> {
    load_journal(app, game_id).filter(|j| !j.finished)
}

/// 备份 = 把原文件改名成 <path>.qold（同卷 rename，零拷贝成本），并记进 journal
fn backup_file(j: &mut PatchJournal, p: &Path) -> Result<(), String> {
    if !p.exists() { return Ok(()); }
    let qold = PathBuf::from(format!("{}.qold", p.display()));
    if qold.exists() { let _ = std::fs::remove_file(&qold); }
    std::fs::rename(p, &qold).map_err(|e| format!("备份 {} 失败: {}（文件被占用？）", p.display(), e))?;
    j.backups.push(qold.display().to_string());
    Ok(())
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) { v.push(s.to_string()); }
}

/// 目标路径所在卷的可用空间（用 sysinfo::Disks，不引新依赖）
fn free_space_of(path: &Path) -> Option<u64> {
    use sysinfo::Disks;
    let p = path.to_string_lossy().to_lowercase();
    let disks = Disks::new_with_refreshed_list();
    disks.list().iter()
        .filter(|d| p.starts_with(&d.mount_point().to_string_lossy().to_lowercase()))
        .max_by_key(|d| d.mount_point().to_string_lossy().len())
        .map(|d| d.available_space())
}

/// 一键回滚到补丁前：还原所有 .qold、删掉本次新建的文件、复原 config.ini
pub fn rollback_patch(app: &tauri::AppHandle, game_id: &str) -> Result<String, String> {
    let j = load_journal(app, game_id).ok_or("没有找到该游戏的更新日志，无法回滚")?;
    let game_dir = crate::load_config(app).get(game_id).cloned().ok_or("游戏未绑定目录")?;
    let mut restored = 0usize; let mut removed = 0usize; let mut failed: Vec<String> = Vec::new();

    for q in &j.backups {
        let qold = PathBuf::from(q);
        let orig = PathBuf::from(q.trim_end_matches(".qold"));
        if !qold.exists() { continue; }
        if orig.exists() { let _ = std::fs::remove_file(&orig); }
        match std::fs::rename(&qold, &orig) {
            Ok(_) => restored += 1,
            Err(e) => failed.push(format!("{} ({})", orig.display(), e)),
        }
    }
    let restored_set: std::collections::HashSet<&String> = j.backups.iter().collect();
    for n in &j.new_files {
        let qold_of_n = format!("{}.qold", n);
        if restored_set.contains(&qold_of_n) || j.backups.iter().any(|q| q == &qold_of_n) { continue; }
        if std::fs::remove_file(n).is_ok() { removed += 1; }
    }
    if let Some(cfg) = &j.config_backup {
        let _ = std::fs::write(Path::new(&game_dir).join("config.ini"), cfg);
    }
    PatchJournal::clear(app, game_id);
    if !failed.is_empty() {
        return Err(format!("回滚部分失败（{} 个文件被占用），已还原 {} 个：\n{}",
            failed.len(), restored, failed.iter().take(5).cloned().collect::<Vec<_>>().join("\n")));
    }
    Ok(format!("已回滚到 {}：还原 {} 个文件 / 删除 {} 个新增文件 / config.ini 已复原",
        if j.from_version.is_empty() { "补丁前状态".to_string() } else { j.from_version.clone() },
        restored, removed))
}

// ================= 🩹 校验修复（按官方清单逐文件校验 + 单文件补全） =================
// 数据源：游戏目录里的官方清单 pkg_version（NDJSON，逐行 {remoteName, md5, fileSize}）
// 单文件地址：res_list_url + "/" + remoteName（已实测：星铁 .../PC/unzip/<路径> 返回 200）
// 这条路的目的是：删掉/损坏几个文件时，只补这几个，而不是重下 80 GB 整包。

#[derive(Serialize, Clone)]
pub struct VerifyReport {
    pub manifest: String,
    pub total: usize,
    pub ok: usize,
    pub missing: usize,
    pub size_bad: usize,
    pub md5_bad: usize,
    pub deep: bool,
    pub broken_bytes: u64,
    pub sample: Vec<String>,
    pub res_list_url: Option<String>,
    pub local_version: String,
    pub remote_version: String,
    pub version_match: bool,
}

#[derive(Deserialize)]
struct ManifestEntry {
    #[serde(rename = "remoteName", default)] remote_name: String,
    #[serde(default)] md5: String,
    #[serde(rename = "fileSize", default)] file_size: u64,
}

/// 校验结果缓存：repair 直接用，不必再扫一遍
static BROKEN_CACHE: OnceLock<Mutex<HashMap<String, Vec<(String, String, u64)>>>> = OnceLock::new();

fn broken_cache() -> &'static Mutex<HashMap<String, Vec<(String, String, u64)>>> {
    BROKEN_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn manifest_entries(dir: &str) -> Result<Vec<ManifestEntry>, String> {
    let p = Path::new(dir).join("pkg_version");
    let s = std::fs::read_to_string(&p)
        .map_err(|e| format!("读不到官方清单 {}：{}", p.display(), e))?;
    let mut out = Vec::new();
    for line in s.lines() {
        let l = line.trim();
        if l.is_empty() { continue; }
        if let Ok(e) = serde_json::from_str::<ManifestEntry>(l) {
            if !e.remote_name.is_empty() { out.push(e); }
        }
    }
    if out.is_empty() { return Err(format!("清单里没解析出条目：{}", p.display())); }
    Ok(out)
}

/// 快速校验 = 只比大小（秒级）；deep = 逐文件算 md5（82GB 约几分钟）
pub fn verify_files(app: &tauri::AppHandle, game_id: &str, deep: bool) -> Result<VerifyReport, String> {
    let game_dir = crate::load_config(app).get(game_id).cloned().ok_or("游戏未绑定目录")?;
    let entries = manifest_entries(&game_dir)?;
    let (res_list_url, remote_version) = match crate::platforms::platform_for_game(game_id) {
        Some(p) => match p.remote_info(app, game_id) {
            Ok(i) => (Some(i.res_list_url.clone()).filter(|s| !s.is_empty()), i.latest_version),
            Err(_) => (None, String::new()),
        },
        None => (None, String::new()),
    };
    let local_version = crate::read_local_version(&game_dir).unwrap_or_default();
    let total = entries.len();
    let total_bytes: u64 = entries.iter().map(|e| e.file_size).sum();
    let (mut ok, mut missing, mut size_bad, mut md5_bad, mut bytes) = (0usize, 0usize, 0usize, 0usize, 0u64);
    let mut scanned: u64 = 0;
    let mut broken: Vec<(String, String, u64)> = Vec::new();
    let mut sample: Vec<String> = Vec::new();
    let started = Instant::now();

    for (i, e) in entries.iter().enumerate() {
        let p = Path::new(&game_dir).join(e.remote_name.replace('/', "\\"));
        let mut reason = "";
        match std::fs::metadata(&p) {
            Err(_) => { missing += 1; reason = "缺失"; }
            Ok(m) if e.file_size > 0 && m.len() != e.file_size => { size_bad += 1; reason = "大小不符"; }
            Ok(_) => {
                if deep && !e.md5.is_empty() {
                    if let Ok(h) = file_md5(&p) { if h != e.md5 { md5_bad += 1; reason = "md5 不符"; } }
                }
                if reason.is_empty() { ok += 1; }
            }
        }
        scanned += e.file_size;
        if !reason.is_empty() {
            bytes += e.file_size;
            if sample.len() < 30 { sample.push(format!("[{}] {} ({})", reason, e.remote_name, crate::fmt_size(e.file_size))); }
            broken.push((e.remote_name.clone(), e.md5.clone(), e.file_size));
        }
        if i % 100 == 0 || i + 1 == total {
            let el = started.elapsed().as_secs_f64();
            let speed = if el > 0.0 { (scanned as f64 / el) as u64 } else { 0 };
            let eta = if speed > 0 { Some(total_bytes.saturating_sub(scanned) / speed) } else { None };
            let _ = app.emit("verify-progress", PatchProgress {
                game_id: game_id.into(), done: (i + 1) as u64, total: total as u64,
                current: format!("{}{}", if deep { "深度校验 " } else { "快速校验 " }, e.remote_name),
                status: "patching".into() });
            // 字节级进度 → 界面那条进度条 + 速度 + 倒计时
            let _ = app.emit("download-progress", DownloadProgress {
                game_id: game_id.into(), downloaded: scanned, total: total_bytes,
                speed, eta_seconds: eta, status: "verifying".into() });
        }
    }
    broken_cache().lock().unwrap().insert(game_id.to_string(), broken);
    let version_match = !remote_version.is_empty() && !local_version.is_empty() && remote_version == local_version;
    Ok(VerifyReport {
        manifest: Path::new(&game_dir).join("pkg_version").display().to_string(),
        total, ok, missing, size_bad, md5_bad, deep, broken_bytes: bytes, sample,
        res_list_url, local_version, remote_version, version_match,
    })
}

/// 只保留 URL 安全字符，其余百分号编码（清单里有空格/括号/#/+ 的文件名必须编码，否则取到的是错内容）
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

/// 按缓存里的坏文件清单逐文件补全（md5 校验后才落位）。
/// 进度走 download-progress{status:"repairing"}：字节 + 速度 + 剩余秒数 → 界面直接画进度条和倒计时。
/// 支持中途取消（复用界面的『✖ 取消』按钮）、单个文件失败自动重试 3 次。
pub fn repair_files(app: &tauri::AppHandle, game_id: &str) -> Result<String, String> {
    let game_dir = crate::load_config(app).get(game_id).cloned().ok_or("游戏未绑定目录")?;
    let platform = crate::platforms::platform_for_game(game_id).ok_or("无平台支持")?;
    let info = platform.remote_info(app, game_id)?;
    if info.res_list_url.is_empty() {
        return Err("该版本没有散列文件地址（res_list_url 为空），无法单文件补全 —— 请走官方启动器修复或整包重装".into());
    }
    let local = crate::read_local_version(&game_dir).unwrap_or_default();
    if !local.is_empty() && !info.latest_version.is_empty() && local != info.latest_version {
        return Err(format!(
            "本地 v{} 与接口整包 v{} 不一致：单文件地址指向的是接口那个版本，直接补可能把文件搞乱。\n→ 先用官方启动器对齐版本，或用整包更新。",
            local, info.latest_version));
    }
    let broken = broken_cache().lock().unwrap().get(game_id).cloned().unwrap_or_default();
    if broken.is_empty() { return Err("没有待修复的文件（请先点一次『🩹 校验修复』做校验）".into()); }

    let app2 = app.clone();
    let gid = game_id.to_string();
    let base = info.res_list_url.trim_end_matches('/').to_string();
    let dir = game_dir.clone();
    let total = broken.len();
    let total_bytes: u64 = broken.iter().map(|(_, _, s)| *s).sum();
    let cancel = crate::cancel_flag(game_id);
    cancel.store(false, AtomicOrdering::SeqCst);
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build() {
            Ok(c) => c,
            Err(e) => { let _ = app2.emit("patch-progress", PatchProgress { game_id: gid, done: 0, total: 0, current: String::new(), status: format!("error:构建 HTTP 客户端失败: {}", e) }); return; }
        };
        let mut done = 0u64;
        let mut downloaded: u64 = 0;
        let mut last_emit = Instant::now();
        let mut last_bytes: u64 = 0;

        for (idx, (name, md5, _size)) in broken.iter().enumerate() {
            if cancel.load(AtomicOrdering::Relaxed) {
                let _ = app2.emit("patch-progress", PatchProgress { game_id: gid.clone(), done, total: total as u64,
                    current: String::new(), status: "error:已取消（已补完的文件都保留，重跑会接着补）".into() });
                return;
            }
            let url = format!("{}/{}", base, url_encode_path(name));
            let dst = Path::new(&dir).join(name.replace('/', "\\"));
            if let Some(par) = dst.parent() { let _ = std::fs::create_dir_all(par); }

            let mut last_err = String::new();
            let mut succeeded = false;
            for attempt in 1..=3u32 {
                let r = (|| -> Result<(), String> {
                    let mut resp = client.get(&url).send().map_err(|e| format!("请求失败: {}", e))?;
                    if !resp.status().is_success() { return Err(format!("HTTP {}", resp.status())); }
                    let tmp = PathBuf::from(format!("{}.qrepair", dst.display()));
                    let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
                    let mut buf = vec![0u8; 256 * 1024];
                    loop {
                        if cancel.load(AtomicOrdering::Relaxed) {
                            drop(f); let _ = std::fs::remove_file(&tmp);
                            return Err("已取消".into());
                        }
                        let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
                        if n == 0 { break; }
                        f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                        downloaded += n as u64;
                        if last_emit.elapsed() > Duration::from_millis(500) {
                            let now = Instant::now();
                            let el = now.duration_since(last_emit).as_secs_f64();
                            let speed = if el > 0.0 { ((downloaded - last_bytes) as f64 / el) as u64 } else { 0 };
                            let eta = if speed > 0 { Some(total_bytes.saturating_sub(downloaded) / speed) } else { None };
                            let _ = app2.emit("download-progress", DownloadProgress {
                                game_id: gid.clone(), downloaded, total: total_bytes,
                                speed, eta_seconds: eta, status: "repairing".into() });
                            last_emit = now; last_bytes = downloaded;
                        }
                    }
                    drop(f);
                    if !md5.is_empty() {
                        let got = file_md5(&tmp)?;
                        if &got != md5 {
                            let _ = std::fs::remove_file(&tmp);
                            return Err(format!("md5 不符（期望 {} 实际 {}）", md5, got));
                        }
                    }
                    if dst.exists() { let _ = std::fs::remove_file(&dst); }
                    std::fs::rename(&tmp, &dst).map_err(|e| e.to_string())?;
                    Ok(())
                })();
                match r {
                    Ok(_) => { succeeded = true; break; }
                    Err(e) => {
                        last_err = e.clone();
                        if e == "已取消" { break; }
                        println!("[repair] {}/{} 第 {} 次失败：{} ({})", idx + 1, total, attempt, name, e);
                        if attempt < 3 { std::thread::sleep(Duration::from_millis(700)); }
                    }
                }
            }
            if !succeeded {
                let _ = app2.emit("patch-progress", PatchProgress { game_id: gid.clone(), done, total: total as u64,
                    current: String::new(),
                    status: format!("error:补全 {} 失败（已试 3 次）: {}\n→ 已完成的部分保留，网络/占用恢复后重跑会接着补。", name, last_err) });
                return;
            }
            done += 1;
            println!("[repair] {}/{} 已补全 {}", done, total, name);
            let _ = app2.emit("patch-progress", PatchProgress { game_id: gid.clone(), done, total: total as u64,
                current: format!("补全 {}", name), status: "patching".into() });
        }
        let _ = app2.emit("download-progress", DownloadProgress { game_id: gid.clone(), downloaded, total: total_bytes,
            speed: 0, eta_seconds: Some(0), status: "repairing".into() });
        let _ = app2.emit("patch-progress", PatchProgress { game_id: gid, done, total: total as u64,
            current: String::new(), status: format!("done:按清单补全了 {} 个文件（{}）", done, crate::fmt_size(downloaded)) });
    });
    Ok(format!("修复已在后台启动（{} 个文件 / {}）", total, crate::fmt_size(total_bytes)))
}

// ================= 整包：分卷合并 + 自动解压 =================
// 米哈游整包是"分卷单档"：a.zip.001/.002/… 或 a.7z.001/…，按顺序拼接就是一个完整压缩包
// （7-Zip 打开 .001 能解就是这个原理）。这里**不**先拼出一个大文件（那要多占一倍空间），
// 而是做一个"虚拟连续流"直接喂给解压器；读取进度走 download-progress{status:"extracting"}。

struct VolumeReader {
    vols: Vec<PathBuf>,
    starts: Vec<u64>,
    total: u64,
    pos: u64,
    cur_idx: usize,
    cur: Option<std::fs::File>,
    app: tauri::AppHandle,
    game_id: String,
    read_bytes: u64,
    last_emit: Instant,
    last_bytes: u64,
}

impl VolumeReader {
    fn new(app: &tauri::AppHandle, game_id: &str, vols: &[PathBuf]) -> Result<Self, String> {
        let mut starts = Vec::new();
        let mut total = 0u64;
        for v in vols {
            starts.push(total);
            total += std::fs::metadata(v).map_err(|e| format!("读取分卷 {} 失败: {}", v.display(), e))?.len();
        }
        if total == 0 { return Err("分卷都是空文件".into()); }
        Ok(Self {
            vols: vols.to_vec(), starts, total, pos: 0, cur_idx: usize::MAX, cur: None,
            app: app.clone(), game_id: game_id.to_string(), read_bytes: 0,
            last_emit: Instant::now(), last_bytes: 0,
        })
    }
    fn open_idx(&mut self, idx: usize) -> std::io::Result<()> {
        if self.cur_idx == idx && self.cur.is_some() { return Ok(()); }
        self.cur = Some(std::fs::File::open(&self.vols[idx])?);
        self.cur_idx = idx;
        Ok(())
    }
    fn tick(&mut self) {
        if self.last_emit.elapsed() > Duration::from_millis(500) {
            let now = Instant::now();
            let el = now.duration_since(self.last_emit).as_secs_f64();
            let shown = std::cmp::min(self.read_bytes, self.total);
            let speed = if el > 0.0 { ((self.read_bytes - self.last_bytes) as f64 / el) as u64 } else { 0 };
            let eta = if speed > 0 { Some(self.total.saturating_sub(shown) / speed) } else { None };
            let _ = self.app.emit("download-progress", DownloadProgress {
                game_id: self.game_id.clone(), downloaded: shown, total: self.total,
                speed, eta_seconds: eta, status: "extracting".into() });
            self.last_emit = now;
            self.last_bytes = self.read_bytes;
        }
    }
}

impl Read for VolumeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.total || buf.is_empty() { return Ok(0); }
        let idx = match self.starts.binary_search(&self.pos) { Ok(i) => i, Err(i) => i.saturating_sub(1) };
        let off = self.pos - self.starts[idx];
        let end = *self.starts.get(idx + 1).unwrap_or(&self.total);
        let want = std::cmp::min(buf.len() as u64, end - self.pos) as usize;
        if want == 0 { return Ok(0); }
        self.open_idx(idx)?;
        let f = self.cur.as_mut().unwrap();
        f.seek(std::io::SeekFrom::Start(off))?;
        let n = f.read(&mut buf[..want])?;
        self.pos += n as u64;
        self.read_bytes += n as u64;
        self.tick();
        Ok(n)
    }
}

impl Seek for VolumeReader {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        let np: i64 = match from {
            std::io::SeekFrom::Start(x) => x as i64,
            std::io::SeekFrom::Current(d) => self.pos as i64 + d,
            std::io::SeekFrom::End(d) => self.total as i64 + d,
        };
        if np < 0 { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek 越界")); }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

fn u16le(b: &[u8]) -> u16 { u16::from_le_bytes([b[0], b[1]]) }
fn u32le(b: &[u8]) -> u32 { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) }
fn u64le(b: &[u8]) -> u64 { u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) }

/// 从虚拟流里解 zip（支持 ZIP64；压缩方式只支持 store(0) 与 deflate(8)）
fn unzip_virtual<R: Read + Seek>(r: &mut R, total: u64, out_dir: &Path) -> Result<usize, String> {
    // 从尾部找 EOCD（PK\x05\x06）
    let tail_len = std::cmp::min(total, 256 * 1024) as usize;
    let mut tail = vec![0u8; tail_len];
    r.seek(std::io::SeekFrom::End(-(tail_len as i64))).map_err(|e| e.to_string())?;
    r.read_exact(&mut tail).map_err(|e| e.to_string())?;
    let eocd_pos = (0..tail.len().saturating_sub(3)).rev()
        .find(|&i| &tail[i..i + 4] == b"PK\x05\x06")
        .ok_or("找不到 ZIP 结尾记录（EOCD）—— 可能不是标准 zip 分卷")?;
    if tail.len() - eocd_pos < 22 { return Err("EOCD 不完整".into()); }
    let e = &tail[eocd_pos..];
    let mut entries = u16le(&e[10..12]) as u64;
    let mut cd_size = u32le(&e[12..16]) as u64;
    let mut cd_off = u32le(&e[16..20]) as u64;
    // ZIP64：EOCD 前 20 字节是 locator（PK\x06\x07）
    if entries == 0xFFFF || cd_off == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
        if eocd_pos >= 20 && &tail[eocd_pos - 20..eocd_pos - 16] == b"PK\x06\x07" {
            let z64 = u64le(&tail[eocd_pos - 12..eocd_pos - 4]);
            r.seek(std::io::SeekFrom::Start(z64)).map_err(|e| e.to_string())?;
            let mut z = [0u8; 56];
            r.read_exact(&mut z).map_err(|e| e.to_string())?;
            if &z[0..4] != b"PK\x06\x06" { return Err("ZIP64 结尾记录损坏".into()); }
            entries = u64le(&z[32..40]);
            cd_size = u64le(&z[40..48]);
            cd_off = u64le(&z[48..56]);
        }
    }
    if entries == 0 || cd_size == 0 { return Err("zip 中央目录是空的".into()); }
    r.seek(std::io::SeekFrom::Start(cd_off)).map_err(|e| e.to_string())?;
    let mut cd = vec![0u8; cd_size as usize];
    r.read_exact(&mut cd).map_err(|e| e.to_string())?;

    let mut p = 0usize;
    let mut ok = 0usize;
    let mut buf = vec![0u8; 256 * 1024];
    for _ in 0..entries {
        if p + 46 > cd.len() || &cd[p..p + 4] != b"PK\x01\x02" { break; }
        let method = u16le(&cd[p + 10..p + 12]);
        let crc_expect = u32le(&cd[p + 16..p + 20]);
        let mut csize = u32le(&cd[p + 20..p + 24]) as u64;
        let mut uncompressed = u32le(&cd[p + 24..p + 28]) as u64;
        let name_len = u16le(&cd[p + 28..p + 30]) as usize;
        let extra_len = u16le(&cd[p + 30..p + 32]) as usize;
        let comment_len = u16le(&cd[p + 32..p + 34]) as usize;
        let mut lho = u32le(&cd[p + 42..p + 46]) as u64;
        if p + 46 + name_len > cd.len() { break; }
        let name = String::from_utf8_lossy(&cd[p + 46..p + 46 + name_len]).to_string();
        // ZIP64 扩展字段 0x0001
        let mut ep = p + 46 + name_len;
        let eend = std::cmp::min(ep + extra_len, cd.len());
        while ep + 4 <= eend {
            let hid = u16le(&cd[ep..ep + 2]);
            let hsz = u16le(&cd[ep + 2..ep + 4]) as usize;
            if hid == 0x0001 {
                let mut q = ep + 4;
                if uncompressed == 0xFFFF_FFFF && q + 8 <= eend { uncompressed = u64le(&cd[q..q + 8]); q += 8; }
                if csize == 0xFFFF_FFFF && q + 8 <= eend { csize = u64le(&cd[q..q + 8]); q += 8; }
                if lho == 0xFFFF_FFFF && q + 8 <= eend { lho = u64le(&cd[q..q + 8]); }
            }
            ep += 4 + hsz;
        }
        p = eend + comment_len;

        if name.contains("..") || name.starts_with('/') || name.starts_with('\\') {
            return Err(format!("压缩包里有可疑路径，已中止: {}", name));
        }
        if name.ends_with('/') {
            let _ = std::fs::create_dir_all(out_dir.join(&name));
            continue;
        }
        // 跳到本地头之后的数据区
        r.seek(std::io::SeekFrom::Start(lho)).map_err(|e| e.to_string())?;
        let mut lh = [0u8; 30];
        r.read_exact(&mut lh).map_err(|e| e.to_string())?;
        if &lh[0..4] != b"PK\x03\x04" { return Err(format!("本地文件头损坏: {}", name)); }
        let skip = u16le(&lh[26..28]) as u64 + u16le(&lh[28..30]) as u64;
        r.seek(std::io::SeekFrom::Start(lho + 30 + skip)).map_err(|e| e.to_string())?;

        let out_path = out_dir.join(name.replace('\\', "/"));
        if let Some(par) = out_path.parent() { std::fs::create_dir_all(par).map_err(|e| e.to_string())?; }
        let mut out = std::fs::File::create(&out_path).map_err(|e| format!("创建 {} 失败: {}", name, e))?;
        let mut hasher = crc32fast::Hasher::new();
        let mut written: u64 = 0;
        match method {
            0 => {
                let mut left = csize;
                while left > 0 {
                    let want = std::cmp::min(buf.len() as u64, left) as usize;
                    let n = r.read(&mut buf[..want]).map_err(|e| e.to_string())?;
                    if n == 0 { break; }
                    hasher.update(&buf[..n]);
                    out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                    written += n as u64;
                    left -= n as u64;
                }
            }
            8 => {
                let mut dec = flate2::read::DeflateDecoder::new(r.by_ref().take(csize));
                loop {
                    let n = dec.read(&mut buf).map_err(|e| format!("{} 解压失败: {}", name, e))?;
                    if n == 0 { break; }
                    hasher.update(&buf[..n]);
                    out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                    written += n as u64;
                }
            }
            m => return Err(format!("不支持的压缩方式 {}（{}）—— 请用 7-Zip 手动解压这些分卷", m, name)),
        }
        drop(out);
        if uncompressed > 0 && written != uncompressed {
            return Err(format!("解压大小不符: {}（期望 {} 实际 {}）", name, uncompressed, written));
        }
        if crc_expect != 0 {
            let got = hasher.finalize();
            if got != crc_expect { return Err(format!("CRC 校验失败: {}（期望 {:08x} 实际 {:08x}）", name, crc_expect, got)); }
        }
        ok += 1;
    }
    Ok(ok)
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() { n += count_files(&p); } else { n += 1; }
        }
    }
    n
}

/// 整包下载完成后的收尾：把分卷当连续流解压到 <dest>/<game_id>_<version>/
pub fn extract_full_package(app: &tauri::AppHandle, game_id: &str, dest: &str, version: &str, vols: &[PathBuf], keep_volumes: bool) -> Result<String, String> {
    if vols.is_empty() { return Err("没有分卷可解压".into()); }
    let first = vols[0].file_name().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let is_7z = first.contains(".7z");
    let is_zip = first.contains(".zip");
    if !is_7z && !is_zip { return Err(format!("不认识的分卷格式: {}", first)); }

    let out_dir = Path::new(dest).join(format!("{}_{}", game_id, version));
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let _ = app.emit("download-progress", DownloadProgress {
        game_id: game_id.into(), downloaded: 0, total: 0, speed: 0, eta_seconds: None, status: "extracting".into() });

    let mut reader = VolumeReader::new(app, game_id, vols)?;
    let total = reader.total;
    if is_7z {
        // 7z 的元数据在包尾，解压器会 seek，所以必须走"虚拟连续流"而不是 decompress_file
        sevenz_rust::decompress(&mut reader, &out_dir).map_err(|e| format!("7z 解压失败: {}", e))?;
    } else {
        unzip_virtual(&mut reader, total, &out_dir)?;
    }

    let mut note = String::new();
    if !keep_volumes {
        let mut freed = 0u64;
        for v in vols {
            if let Ok(m) = std::fs::metadata(v) { freed += m.len(); }
            let _ = std::fs::remove_file(v);
        }
        note = format!("；已删除分卷释放 {}", crate::fmt_size(freed));
    }
    Ok(format!("已解压 {} 个文件 → {}{}", count_files(&out_dir), out_dir.display(), note))
}

/// 原地差分升级（**可逆版**）。dry_run=true 只预检+报计划，不写盘。
///
/// 顺序原则 —— 提交点之前每一步都可逆，不可逆动作全部集中在最后：
///   0 预检(进程/可写性/空间) → 1 合成(原件改名 .qold 备份) → 2 替换(同样 .qold 备份)
///   → 3 删除(只改名为 .qold，不真删) → 4 校验门(逐条复核 md5) → ★提交点★
///   → 5 删 .qold + 写 config.ini 版本号 → journal 收尾
/// 任何一步失败/断电：config.ini 未被改、原件都还在（.qold），可「继续更新」或「↩ 回滚」。
pub fn apply_patch(app: &tauri::AppHandle, game_id: &str, patch_dir: &str, dry_run: bool) -> Result<String, String> {
    let game_dir = crate::load_config(app).get(game_id).cloned().ok_or("游戏未绑定目录")?;
    let gdir = PathBuf::from(&game_dir);
    let extracted = Path::new(patch_dir).join("extracted");
    let map: HDiffMap = serde_json::from_str(
        &std::fs::read_to_string(extracted.join("hdiffmap.json")).map_err(|e| format!("读不到 hdiffmap.json: {}", e))?
    ).map_err(|e| format!("解析 hdiffmap.json 失败: {}", e))?;
    let mut replace_files: Vec<PathBuf> = Vec::new();
    walk_replace_files(&extracted, &extracted, &mut replace_files);

    // 立刻给界面一个"已受理"的反馈（预检+预演可能要十几秒）
    let _ = app.emit("patch-progress", PatchProgress {
        game_id: game_id.into(), done: 0, total: 0,
        current: "预检：读取补丁清单 / 检查进程占用…".into(), status: "patching".into() });

    // ---------- 0) 预检 ----------
    // 0.1 进程：正在跑的游戏 / 残留僵尸（僵尸常常取不到 exe 路径，所以优先用进程名匹配）
    {
        use sysinfo::{ProcessesToUpdate, System};
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::All);
        let wanted_name = crate::load_config(app).get(game_id)
            .and_then(|d| Path::new(d).file_name().map(|s| s.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        let dir_lower = game_dir.to_lowercase();
        let (mut live, mut dead): (Vec<String>, Vec<u32>) = (Vec::new(), Vec::new());
        for (pid, p) in sys.processes() {
            let exe_name = p.exe().and_then(|e| e.file_name())
                .map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            let name = if exe_name.is_empty() { p.name().to_string_lossy().to_lowercase() } else { exe_name };
            let is_ours = !wanted_name.is_empty() && name == wanted_name;
            let under_dir = p.exe().map(|e| e.to_string_lossy().to_lowercase().starts_with(&dir_lower)).unwrap_or(false);
            if !(is_ours || under_dir) { continue; }
            if p.memory() > 10 * 1024 * 1024 { live.push(format!("PID {} {}", pid.as_u32(), name)); }
            else { dead.push(pid.as_u32()); }
        }
        if !live.is_empty() {
            return Err(format!("游戏正在运行，请先关闭再应用差分：\n{}", live.join("\n")));
        }
        if !dead.is_empty() {
            return Err(format!(
                "检测到 {} 个残留进程（内存极小、但正锁着游戏文件）：PID {}\n→ 先点界面上的『🧟 僵尸扫描 → 清理』清掉它们，再重试应用差分。",
                dead.len(),
                dead.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    // 0.2 可写性：把将要被覆盖的文件逐个探一遍
    let mut will_write: Vec<PathBuf> = Vec::new();
    for e in &map.diff_map { will_write.push(gdir.join(e.target_file_name.replace('/', "\\"))); }
    for p in &replace_files {
        if let Ok(rel) = p.strip_prefix(&extracted) { will_write.push(gdir.join(rel)); }
    }
    will_write.push(gdir.join("config.ini"));
    let locked: Vec<String> = will_write.iter()
        .filter(|p| p.exists() && std::fs::OpenOptions::new().write(true).open(p).is_err())
        .map(|p| p.display().to_string()).collect();
    if !locked.is_empty() {
        return Err(format!(
            "有 {} 个目标文件正被占用，已中止（避免又写一半）：\n{}\n→ 关掉占用的进程（含 0 线程残留进程），或重启一次再跑。",
            locked.len(), locked.iter().take(5).cloned().collect::<Vec<_>>().join("\n")
        ));
    }
    // 0.3 空间：替换阶段先写 .qtmp 会临时多占一份（备份用 rename，不额外占空间）
    let need: u64 = replace_files.iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
    if let Some(free) = free_space_of(&gdir) {
        if free < need + 512 * 1024 * 1024 {
            return Err(format!("磁盘空间不足：替换阶段需要约 {} 临时空间，当前可用 {}",
                crate::fmt_size(need), crate::fmt_size(free)));
        }
    }

    let delete_list: Vec<String> = std::fs::read_to_string(extracted.join("deletefiles.txt"))
        .unwrap_or_default().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();

    // ---------- journal：断点续跑 / 回滚的依据 ----------
    let to_version = resolve_target_version(app, game_id, patch_dir).unwrap_or_default();
    let from_version = crate::read_local_version(&game_dir).unwrap_or_default();
    let mut j = load_journal(app, game_id).filter(|x| !x.finished).unwrap_or_default();
    if j.game_id.is_empty() { j.game_id = game_id.to_string(); }
    if j.patch_dir.is_empty() { j.patch_dir = patch_dir.to_string(); }
    if j.from_version.is_empty() { j.from_version = from_version.clone(); }
    if j.to_version.is_empty() { j.to_version = to_version.clone(); }
    if j.config_backup.is_none() { j.config_backup = std::fs::read_to_string(gdir.join("config.ini")).ok(); }
    j.diff_total = map.diff_map.len();
    j.replace_total = replace_files.len();
    j.delete_total = delete_list.len();
    j.finished = false;
    let resuming = !j.backups.is_empty() || j.diff_done > 0 || j.replace_done > 0;

    // 预检+预演可能要十几秒（1241 个文件探测 + 696 个文件 md5），先给界面一个反馈，别让它看起来像死了
    let _ = app.emit("patch-progress", PatchProgress {
        game_id: game_id.into(), done: 0, total: 0,
        current: "预检：进程占用 / 文件可写性 / 磁盘空间…".into(), status: "patching".into() });

    // ---------- 预演：只统计，不写盘 ----------
    if dry_run {
        let mut diff_done = 0usize;
        let mut checked = 0usize;
        for e in &map.diff_map {
            let new = gdir.join(e.target_file_name.replace('/', "\\"));
            if std::fs::metadata(&new).map(|m| m.len() == e.target_file_size).unwrap_or(false)
                && file_md5(&new).map(|h| h == e.target_file_md5).unwrap_or(false) { diff_done += 1; }
            checked += 1;
            if checked % 100 == 0 {
                let _ = app.emit("patch-progress", PatchProgress {
                    game_id: game_id.into(), done: checked as u64, total: map.diff_map.len() as u64,
                    current: format!("预演：核对已合成文件 {}/{}", checked, map.diff_map.len()), status: "patching".into() });
            }
        }
        let _ = app.emit("patch-progress", PatchProgress {
            game_id: game_id.into(), done: 0, total: 0,
            current: "预演：核对待替换文件…".into(), status: "patching".into() });
        let rep_done = replace_files.iter().filter(|p| {
            let rel = p.strip_prefix(&extracted).unwrap();
            match (std::fs::metadata(p), std::fs::metadata(gdir.join(rel))) {
                (Ok(a), Ok(b)) => a.len() == b.len(), _ => false }
        }).count();
        return Ok(format!(
            "[预演] 计划：合成 {} 条（已完成 {}）/ 直接替换 {} 个（已完成 {}）/ 待删 {} 个 / 目标版本 {}{}",
            map.diff_map.len(), diff_done, replace_files.len(), rep_done, delete_list.len(),
            if to_version.is_empty() { "未知".to_string() } else { to_version.clone() },
            if resuming { format!(" / ⚠️ 检测到上次未完成（已备份 {} 个文件），本次将续跑", j.backups.len()) } else { String::new() }
        ));
    }

    let mut synthesized = 0usize;
    let mut replaced = 0usize;
    let mut pending_delete: Vec<String> = delete_list.clone();

    // ---------- 1) 合成 diff_map（可逆：原件先改名成 .qold） ----------
    j.phase = "synthesize".into();
    j.save(app);
    for (i, e) in map.diff_map.iter().enumerate() {
        let old = gdir.join(e.source_file_name.replace('/', "\\"));
        let diff = extracted.join(e.patch_file_name.replace('/', "\\"));
        let new = gdir.join(e.target_file_name.replace('/', "\\"));

        // 幂等：目标已正确 → 跳过（续跑时命中这里）
        if std::fs::metadata(&new).map(|m| m.len() == e.target_file_size).unwrap_or(false)
            && file_md5(&new).map(|h| h == e.target_file_md5).unwrap_or(false) {
            synthesized += 1;
            if e.source_file_name != e.target_file_name { push_unique(&mut pending_delete, &e.source_file_name); }
            if synthesized % 50 == 0 || synthesized == map.diff_map.len() {
                let _ = app.emit("patch-progress", PatchProgress {
                    game_id: game_id.into(), done: synthesized as u64, total: map.diff_map.len() as u64,
                    current: format!("已完成校验 {}", e.target_file_name), status: "patching".into() });
            }
            continue;
        }
        // 上次已把源备份成 .qold → 先还原，保证 hpatchz 能读到源
        if !old.exists() {
            let q = PathBuf::from(format!("{}.qold", old.display()));
            if q.exists() { let _ = std::fs::rename(&q, &old); }
        }
        if !old.exists() { return Err(format!("第{}条: 源文件缺失 {}（本地版本不匹配?）", i, e.source_file_name)); }
        if !diff.exists() { return Err(format!("第{}条: 增量缺失 {}", i, e.patch_file_name)); }
        let old_size = std::fs::metadata(&old).map(|m| m.len()).unwrap_or(0);
        if old_size != e.source_file_size {
            j.phase = "failed".into();
            j.message = format!("第{}条源大小不符：{} 期望{} 实际{}", i, e.source_file_name, e.source_file_size, old_size);
            j.save(app);
            return Err(format!("第{}条: 源大小不符 {} 期望{} 实际{}", i, e.source_file_name, e.source_file_size, old_size));
        }
        
        
        
        // 合成到临时文件 → 校验 → 原件改名备份 → 落位
        let temp = PathBuf::from(format!("{}.qtmp", new.display()));
        run_hpatchz(app, &old, &diff, &temp)?;
        let got = file_md5(&temp)?;
        if !e.target_file_md5.is_empty() && got != e.target_file_md5 {
            let _ = std::fs::remove_file(&temp);
            return Err(format!("第{}条: 合成 md5 不符 {} 期望{} 实际{}", i, e.target_file_name, e.target_file_md5, got));
        }
        if let Some(par) = new.parent() { std::fs::create_dir_all(par).map_err(|x| x.to_string())?; }
        let existed = new.exists();
        backup_file(&mut j, &new)?;                 // ← 可逆的关键：原件改名 .qold（零拷贝）
        std::fs::rename(&temp, &new).map_err(|x| format!("rename 失败: {}", x))?;
        if !existed { j.new_files.push(new.display().to_string()); }
        synthesized += 1;
        j.diff_done = synthesized;
        if synthesized % 10 == 0 || synthesized == map.diff_map.len() { j.save(app); }
        let _ = app.emit("patch-progress", PatchProgress {
            game_id: game_id.into(), done: synthesized as u64, total: map.diff_map.len() as u64,
            current: e.target_file_name.clone(), status: "patching".into(),
        });
        if e.source_file_name != e.target_file_name { push_unique(&mut pending_delete, &e.source_file_name); }
    }
    j.diff_done = synthesized;
    j.save(app);

    // ---------- 2) 直接替换文件（可逆：同样先备份成 .qold） ----------
    j.phase = "replace".into();
    j.save(app);
    for (i, p) in replace_files.iter().enumerate() {
        let rel = p.strip_prefix(&extracted).unwrap().to_string_lossy().replace('\\', "/");
        let dst = gdir.join(&rel);
        // 幂等：大小一致即视为已复制（写入走 .qtmp → rename，不会留下半截文件）
        let same = match (std::fs::metadata(p), std::fs::metadata(&dst)) {
            (Ok(a), Ok(b)) => a.len() == b.len(), _ => false };
        if !same {
            if let Some(par) = dst.parent() { std::fs::create_dir_all(par).map_err(|x| x.to_string())?; }
            let temp = PathBuf::from(format!("{}.qtmp", dst.display()));
            std::fs::copy(p, &temp).map_err(|x| format!(
                "复制第 {}/{} 个文件失败：{}\n→ 该文件很可能被进程占用。已完成的保持现状，处理占用后点『▶ 继续更新』即可（已完成项会跳过）。\n→ {}",
                i + 1, replace_files.len(), rel, x))?;
            let existed = dst.exists();
            backup_file(&mut j, &dst)?;
            std::fs::rename(&temp, &dst).map_err(|x| format!("rename 失败: {}", x))?;
            if !existed { j.new_files.push(dst.display().to_string()); }
        }
        replaced += 1;
        j.replace_done = replaced;
        if replaced % 20 == 0 || replaced == replace_files.len() {
            j.save(app);
            let _ = app.emit("patch-progress", PatchProgress {
                game_id: game_id.into(), done: replaced as u64, total: replace_files.len() as u64,
                current: format!("复制 {}", rel), status: "patching".into(),
            });
        }
    }
    j.replace_done = replaced;
    j.save(app);

    // ---------- 3) 删除：只改名为 .qold，真正删除留到提交点 ----------
    j.phase = "delete".into();
    let mut deleted = 0usize;
    for rel in &pending_delete {
        let f = gdir.join(rel.replace('/', "\\"));
        if f.exists() { backup_file(&mut j, &f)?; deleted += 1; }
    }
    j.delete_done = deleted;
    j.save(app);

    // ---------- 4) 校验门：全部对得上才允许进入不可逆的提交点 ----------
    j.phase = "verify".into();
    j.save(app);
    let _ = app.emit("patch-progress", PatchProgress {
        game_id: game_id.into(), done: 0, total: map.diff_map.len() as u64,
        current: "校验中…".into(), status: "patching".into() });
    let mut bad: Vec<String> = Vec::new();
    for e in &map.diff_map {
        let new = gdir.join(e.target_file_name.replace('/', "\\"));
        let ok = std::fs::metadata(&new).map(|m| m.len() == e.target_file_size).unwrap_or(false)
            && file_md5(&new).map(|h| h == e.target_file_md5).unwrap_or(false);
        if !ok { bad.push(e.target_file_name.clone()); if bad.len() >= 10 { break; } }
    }
    if bad.is_empty() {
        for p in replace_files.iter() {
            let rel = p.strip_prefix(&extracted).unwrap();
            let dst = gdir.join(rel);
            let ok = match (std::fs::metadata(p), std::fs::metadata(&dst)) {
                (Ok(a), Ok(b)) => a.len() == b.len() && file_md5(p).ok() == file_md5(&dst).ok(),
                _ => false };
            if !ok { bad.push(rel.to_string_lossy().replace('\\', "/")); if bad.len() >= 10 { break; } }
        }
    }
    if !bad.is_empty() {
        j.phase = "failed".into();
        j.message = format!("校验未通过 {} 项", bad.len());
        j.save(app);
        return Err(format!(
            "⚠️ 校验未通过（{} 项）。文件都在、config.ini 未改动 —— 可点『▶ 继续更新』重跑，或『↩ 回滚』回到补丁前：\n{}",
            bad.len(), bad.join("\n")));
    }

    // ---------- 5) ★提交点★：到这里才做不可逆动作 ----------
    j.phase = "commit".into();
    j.save(app);
    let mut purged = 0usize;
    for q in &j.backups { if std::fs::remove_file(q).is_ok() { purged += 1; } }
    let version_note = match resolve_target_version(app, game_id, patch_dir) {
        Some(v) => { update_config_version(&game_dir, &v)?; format!(" / config.ini → {}", v) }
        None => " / ⚠️ 无法确定目标版本号，config.ini 未改动".to_string(),
    };
    j.phase = "done".into();
    j.finished = true;
    j.backups.clear();
    j.new_files.clear();
    j.message = format!("完成：合成 {} 条 / 替换 {} 个 / 删除 {} 个", synthesized, replaced, deleted);
    j.save(app);

    Ok(format!("[完成]：合成 {} 条 / 直接替换 {} 个 / 删除 {} 个 / 清理备份 {} 个{}",
        synthesized, replaced, deleted, purged, version_note))
}

/// 阻塞 HTTP 一律丢进独立 std::thread，并带硬超时。
/// ⚠️ 不能直接 `reqwest::blocking::get`：命令是 `#[tauri::command(async)]`，函数体会跑在
///    tokio 运行时线程上，blocking 客户端在那里创建/析构会 panic（表现为"点查版本没反应"：
///    命令既不返回也不报错）。另外以前完全没有超时，网络一卡就永远挂着。
fn hyp_get<T: for<'de> Deserialize<'de> + Send + 'static>(url: &str) -> Result<T, String> {
    let url = url.to_string();
    let (tx, rx) = std::sync::mpsc::channel::<Result<T, String>>();
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(25))
            .build()
        {
            Ok(c) => c,
            Err(e) => { let _ = tx.send(Err(format!("构建 HTTP 客户端失败: {}", e))); return; }
        };
        let r = client.get(&url).send()
            .map_err(|e| format!("网络错误: {}", e))
            .and_then(|resp| resp.json::<T>().map_err(|e| format!("解析响应失败: {}", e)));
        let _ = tx.send(r);
    });
    rx.recv_timeout(Duration::from_secs(30))
        .map_err(|_| "请求超时（30 秒无响应）".to_string())?
}

#[derive(Deserialize)] pub struct HypRoot<T> { pub retcode: i64, #[serde(default)] pub message: String, pub data: Option<T> }
#[derive(Deserialize)] pub struct HypGamesData { #[serde(default)] pub games: Vec<HypGameEntry> }
#[derive(Deserialize)] pub struct HypGameEntry { #[serde(default)] pub id: String, #[serde(default)] pub biz: String }
#[derive(Deserialize)] pub struct HypPackagesData { #[serde(default)] pub game_packages: Vec<HypGamePackages> }
#[derive(Deserialize)] pub struct HypGamePackages { pub game: HypGameId, pub main: Option<HypMain>, #[serde(default)] pub pre_download: Option<HypPre> }
/// 预下载（下个版本已宣布时才有内容）
#[derive(Deserialize)] pub struct HypPre { pub major: Option<HypMajor> }
/// 公告/资讯流（版本源自适应用它推断"当前真实版本"）
#[derive(Deserialize)] pub struct HypContentData { pub content: Option<HypContent> }
#[derive(Deserialize)] pub struct HypContent { #[serde(default)] pub posts: Vec<HypPost> }
#[derive(Deserialize)] pub struct HypPost {
    #[serde(default, rename = "type")] pub post_type: String,
    #[serde(default)] pub title: String,
    #[serde(default)] pub date: String,
}
#[derive(Deserialize)] pub struct HypGameId { #[serde(default)] pub id: String, #[serde(default)] pub biz: String }
// P4 修复：同文件自引用不再写全路径
#[derive(Deserialize)] pub struct HypMain { pub major: Option<HypMajor>, #[serde(default)] pub patches: Vec<HypPatch> }
#[derive(Deserialize)] pub struct HypMajor { #[serde(default)] pub version: String, #[serde(default)] pub game_pkgs: Vec<HypPkg>, #[serde(default)] pub audio_pkgs: Vec<HypAudioPkg>, #[serde(default)] pub res_list_url: String }
#[derive(Deserialize)] pub struct HypPatch { #[serde(default)] pub version: String, #[serde(default)] pub game_pkgs: Vec<HypPkg>, #[serde(default)] pub audio_pkgs: Vec<HypAudioPkg>, #[serde(default)] pub res_list_url: String }
#[derive(Deserialize)] pub struct HypPkg { #[serde(default)] pub url: String, #[serde(default)] pub md5: String, #[serde(default)] pub size: String, #[serde(default)] pub decompressed_size: String }
#[derive(Deserialize)] pub struct HypAudioPkg { #[serde(default)] pub language: String, #[serde(default)] pub url: String, #[serde(default)] pub md5: String, #[serde(default)] pub size: String }

fn emit(report: &mut Vec<String>, line: String) {
    println!("{}", line);
    report.push(line);
}

pub fn probe_report(app: &tauri::AppHandle) -> Vec<String> {
    let (launcher_id, source) = hyp_launcher_id(app);
    let mut report = Vec::new();
    emit(&mut report, "[0] 接口底座 hyp-connect（旧 mdk 接口已失效：retcode -205 invalid key）".into());
    emit(&mut report, format!("[0] launcher_id = {}（来源：{}）", launcher_id, source));
    emit(&mut report, format!("[0] 下载保命线 = {} MB（0=不限）", download_limit_bytes(app) / 1024 / 1024));
    emit(&mut report, format!("[0] channels.json = {}",
        channels_path(app).map(|p| p.display().to_string())
            .unwrap_or_else(|| "（未找到，用内置默认）".into())));

    let games_url = format!("{}/getGames?launcher_id={}&language={}", HYP_BASE_CN, launcher_id, HYP_LANGUAGE);
    let mut id_by_biz: HashMap<String, String> = HashMap::new();
    match hyp_get::<HypRoot<HypGamesData>>(&games_url) {
        Ok(root) if root.retcode == 0 => {
            let games = root.data.map(|d| d.games).unwrap_or_default();
            emit(&mut report, format!("[1] getGames retcode=0 → 返回 {} 个游戏", games.len()));
            for g in &games { id_by_biz.insert(g.biz.clone(), g.id.clone()); }
            for (id, name, biz, _) in MIHOYO_GAMES {
                let gid = id_by_biz.get(*biz).map(|s| s.as_str()).unwrap_or("未返回");
                emit(&mut report, format!("      {} / {} → biz={} id={}", id, name, biz, gid));
            }
        }
        Ok(root) => emit(&mut report, format!("[1] getGames retcode={} message={}", root.retcode, root.message)),
        Err(e) => emit(&mut report, format!("[1] getGames 请求失败: {}", e)),
    }

    let pkgs_url = format!("{}/getGamePackages?launcher_id={}&language={}", HYP_BASE_CN, launcher_id, HYP_LANGUAGE);
    match hyp_get::<HypRoot<HypPackagesData>>(&pkgs_url) {
        Ok(root) if root.retcode == 0 => {
            let list = root.data.map(|d| d.game_packages).unwrap_or_default();
            emit(&mut report, format!("[2] getGamePackages retcode=0 → 返回 {} 个游戏的包信息", list.len()));
            for (id, name, biz, _) in MIHOYO_GAMES {
                match list.iter().find(|g| g.game.biz == *biz) {
                    Some(gp) => match gp.main.as_ref().and_then(|m| m.major.as_ref()) {
                        Some(m) => {
                            let total: u64 = m.game_pkgs.iter().filter_map(|p| p.size.parse::<u64>().ok()).sum();
                            let patch = gp.main.as_ref().and_then(|m| m.patches.first())
                                .map(|p| format!("；差分 from {} ({})", p.version,
                                    crate::fmt_size(p.game_pkgs.iter().filter_map(|x| x.size.parse::<u64>().ok()).sum())))
                                .unwrap_or_else(|| "；无差分包".to_string());
                            emit(&mut report, format!("      {} / {} v{} 整包 {} 卷 / {}{}",
                                id, name, m.version, m.game_pkgs.len(), crate::fmt_size(total), patch));
                        }
                        None => emit(&mut report, format!("      {} / {}：接口未给整包", id, name)),
                    },
                    None => emit(&mut report, format!("      {} / {}：接口未返回 {} 的包", id, name, biz)),
                }
            }
            for gp in &list {
                let major = match gp.main.as_ref().and_then(|m| m.major.as_ref()) { Some(m) => m, None => continue };
                let gid = id_by_biz.get(&gp.game.biz).map(|s| s.as_str()).unwrap_or(gp.game.id.as_str());
                let sources: Vec<String> = gp.main.as_ref()
                    .map(|m| m.patches.iter().map(|p| format!("\"{}\"", p.version)).collect())
                    .unwrap_or_default();
                emit(&mut report, format!("[全量] biz={} id={} major={} 差分源=[{}] res_list={}",
                    gp.game.biz, gid, major.version, sources.join(", "), major.res_list_url));
            }
        }
        Ok(root) => emit(&mut report, format!("[2] getGamePackages retcode={} message={}", root.retcode, root.message)),
        Err(e) => emit(&mut report, format!("[2] getGamePackages 请求失败: {}", e)),
    }

    // [3] 版本源对账：整包（唯一可下载）/ 预下载 / 公告推断 → 最终判定 + 实际用的渠道与 launcher_id
    emit(&mut report, "[3] 版本源对账（整包=唯一可下载；公告=判断是否已落后真实版本）".into());
    match hyp_resolve(app) {
        Ok((lid, lsrc, games)) => {
            emit(&mut report, format!("      launcher_id = {}（来源：{}；候选共 {} 个）", lid, lsrc, launcher_candidates(app).len()));
            let cfg = crate::load_config(app);
            match hyp_get::<HypRoot<HypPackagesData>>(&hyp_url("getGamePackages", &lid, &[])) {
                Ok(root) if root.retcode == 0 => {
                    let list = root.data.map(|d| d.game_packages).unwrap_or_default();
                    for (id, name, _biz, _) in MIHOYO_GAMES {
                        let (biz, gid) = match match_biz(app, id, &games) {
                            Some(v) => v,
                            None => { emit(&mut report, format!("      {} / {}：渠道未匹配到（需改 channels.json 的 biz_patterns）", id, name)); continue; }
                        };
                        let gp = match list.iter().find(|g| g.game.biz == biz) {
                            Some(g) => g,
                            None => { emit(&mut report, format!("      {} / {}：接口未返回 {} 的包", id, name, biz)); continue; }
                        };
                        let pkg = gp.main.as_ref().and_then(|m| m.major.as_ref()).map(|m| m.version.clone()).unwrap_or_default();
                        let pre = gp.pre_download.as_ref().and_then(|p| p.major.as_ref()).map(|m| m.version.clone()).unwrap_or_else(|| "无".into());
                        let ann = hyp_get::<HypRoot<HypContentData>>(&hyp_url("getGameContent", &lid, &[("game_id", gid.as_str())]))
                            .ok().and_then(|r| r.data).and_then(|d| d.content)
                            .and_then(|c| announced_from_posts(&c.posts)).unwrap_or_else(|| "无".into());
                        let local = cfg.get(*id).cloned()
                            .and_then(|d| crate::read_local_version(&d)).unwrap_or_else(|| "未安装".into());
                        let verdict = if local == "未安装" { "未安装（可整包）".to_string() }
                            else if compare_versions(&local, &pkg) == Ordering::Less { "可更新（整包）".to_string() }
                            else if compare_versions(&local, &pkg) == Ordering::Greater { "本地更新 → 接口整包滞后，禁下载".to_string() }
                            else if ann != "无" && compare_versions(&ann, &pkg) == Ordering::Greater { format!("⚠️ 整包停更：官宣 {} > 整包 {} → 请走官启", ann, pkg) }
                            else { "已是最新".to_string() };
                        emit(&mut report, format!("      {} / {} → biz={} id={} ｜ 本地 {} ｜ 整包 {} ｜ 预下载 {} ｜ 公告 {} ｜ 判定：{}",
                            id, name, biz, gid, local, pkg, pre, ann, verdict));
                    }
                }
                Ok(root) => emit(&mut report, format!("      getGamePackages retcode={} {}", root.retcode, root.message)),
                Err(e) => emit(&mut report, format!("      getGamePackages 失败: {}", e)),
            }
        }
        Err(e) => emit(&mut report, format!("      ⚠️ launcher_id 全部候选都失败：{}", e)),
    }
    report
}

pub struct MihoyoPlatform;

impl GamePlatform for MihoyoPlatform {
    fn id(&self) -> &str { "mihoyo" }
    fn name(&self) -> &str { "米哈游 (Full)" }
    fn level(&self) -> PlatformLevel { PlatformLevel::Full }
    fn game_ids(&self) -> Vec<&'static str> { vec!["genshin", "starrail", "zenless"] }
    fn official_launcher(&self) -> Option<String> { Some("https://ys.mihoyo.com/".into()) }
    fn official_website(&self, game_id: &str) -> Option<String> {
        Some(match game_id {
            "genshin" => "https://ys.mihoyo.com/",
            "starrail" => "https://sr.mihoyo.com/",
            "zenless" => "https://zzz.mihoyo.com/",
            _ => return None,
        }.into())
    }

    // P3：平台自己声明 exe 候选名
    fn exe_candidates(&self, game_id: &str) -> Vec<&'static str> {
        MIHOYO_GAMES.iter()
            .find(|(id, _, _, _)| *id == game_id)
            .map(|(_, _, _, exes)| exes.to_vec())
            .unwrap_or_default()
    }

    fn detect(&self, app: &tauri::AppHandle) -> Vec<GameInfo> {
        let cfg = crate::load_config(app);
        let entries = crate::scan_uninstall_registry();
        let mut games = Vec::new();
        for (id, cname, _biz, exes) in MIHOYO_GAMES {
            let mut found: Option<(String, PathBuf)> = None;
            if let Some(dir) = cfg.get(*id) {
                if let Some(exe) = crate::find_game_exe(Path::new(dir), exes) { found = Some((dir.clone(), exe)); }
            }
            if found.is_none() {
                for (name, loc) in &entries {
                    if crate::is_cloud_entry(name) || !name.contains(cname) { continue; }
                    let dir = Path::new(loc);
                    if !dir.exists() { continue; }
                    if let Some(exe) = crate::find_game_exe(dir, exes) { found = Some((loc.clone(), exe)); break; }
                }
            }
            games.push(match found {
                Some((dir, exe)) => GameInfo {
                    id: (*id).into(), name: (*cname).into(), installed: true,
                    path: Some(dir.clone()), exe: Some(exe.to_string_lossy().into_owned()),
                    local_version: crate::read_local_version(&dir), platform: "mihoyo".into(),
                    platform_level: PlatformLevel::Full, launcher_uri: self.official_launcher(),
                },
                None => GameInfo {
                    id: (*id).into(), name: (*cname).into(), installed: false,
                    path: None, exe: None, local_version: None, platform: "mihoyo".into(),
                    platform_level: PlatformLevel::Full, launcher_uri: self.official_launcher(),
                },
            });
        }
        games
    }

    fn launch_direct(&self, game: &GameInfo) -> Result<String, String> {
        let exe = game.exe.clone().ok_or("缺少 exe 路径")?;
        let dir = game.path.clone().ok_or("缺少安装目录")?;
        std::process::Command::new(&exe).current_dir(&dir).spawn().map_err(|e| e.to_string())?;
        Ok(format!("已直启: {}", exe))
    }

    fn remote_info(&self, app: &tauri::AppHandle, game_id: &str) -> Result<RemoteGameInfo, String> {
        // ① 解析 launcher_id（逐个候选验证）+ 动态匹配渠道（渠道改名也能跟上）
        let (launcher_id, lid_src, games) = hyp_resolve(app)?;
        let (biz, hyp_game_id) = match match_biz(app, game_id, &games) {
            Some(v) => v,
            None => {
                let list: Vec<String> = games.iter().map(|g| format!("{} / {}", g.biz, g.id)).collect();
                return Err(format!(
                    "getGames 里没匹配到 {}（渠道可能改名/换马甲了）。\n当前接口返回的渠道：\n{}\n→ 去 channels.json 的 biz_patterns 里加一条规则即可。",
                    game_id, list.join("\n")));
            }
        };
        // ② 整包信息（带 cache-buster）
        let root: HypRoot<HypPackagesData> = hyp_get(&hyp_url("getGamePackages", &launcher_id, &[]))?;
        if root.retcode != 0 {
            return Err(format!("getGamePackages retcode={} message={}（launcher_id={} 来源：{}）", root.retcode, root.message, launcher_id, lid_src));
        }
        let all = root.data.ok_or("响应缺少 data")?;
        let gp = all.game_packages.into_iter().find(|g| g.game.biz == biz)
            .ok_or_else(|| format!("接口未返回 {} 的包信息", biz))?;
        let pre_version = gp.pre_download.as_ref().and_then(|p| p.major.as_ref())
            .map(|m| m.version.clone()).filter(|v| !v.is_empty());
        let main = gp.main.ok_or_else(|| format!("{} 暂无可用整包", biz))?;
        let major = main.major.ok_or("缺少 main.major")?;
        let parts: Vec<RemotePackagePart> = major.game_pkgs.iter().filter(|p| !p.url.is_empty()).map(|p| RemotePackagePart {
            url: p.url.clone(), md5: p.md5.clone(), size: p.size.parse::<u64>().unwrap_or(0),
        }).collect();
        if parts.is_empty() { return Err(format!("{} 没有可下载的分卷包", biz)); }
        let (patch_from, patch_size, patch_parts) = match main.patches.first() {
            Some(p) => (
                Some(p.version.clone()),
                p.game_pkgs.iter().filter_map(|x| x.size.parse::<u64>().ok()).sum(),
                p.game_pkgs.iter().filter(|x| !x.url.is_empty()).map(|x| RemotePackagePart {
                    url: x.url.clone(), md5: x.md5.clone(), size: x.size.parse::<u64>().unwrap_or(0),
                }).collect(),
            ),
            None => (None, 0, Vec::new()),
        };

        // ③ 公告/预下载推断"当前真实版本"
        let mut announce_err: Option<String> = None;
        let mut announced = match hyp_get::<HypRoot<HypContentData>>(
                &hyp_url("getGameContent", &launcher_id, &[("game_id", hyp_game_id.as_str())])) {
            Ok(r) if r.retcode == 0 => match r.data.and_then(|d| d.content).and_then(|c| announced_from_posts(&c.posts)) {
                Some(v) => Some(v),
                None => { announce_err = Some("公告里没解析出版本号".into()); None }
            },
            Ok(r) => { announce_err = Some(format!("getGameContent retcode={} {}", r.retcode, r.message)); None }
            Err(e) => { announce_err = Some(format!("getGameContent 请求失败: {}", e)); None }
        };
        
        // 🌟 显式标注 &str 类型，避免生命周期推断报错
        let mut announced_src: &str = "公告(推断)";

                // 🌟 Sophon 权威版本：原神/星铁直接取 getBuild 的 tag（公告解析失败不再阻断更新）
        if biz == "hk4e_cn" || biz == "hkrpg_cn" {
            let biz_clone = biz.clone();
            
            // 🌟 修复：使用 std::thread::spawn 避开 tokio worker 线程，防止 reqwest::blocking 引发 panic
            let tag_result = std::thread::spawn(move || {
                crate::sophon::fetch_latest_tag(&biz_clone)
            }).join().unwrap_or(Err("获取 tag 线程异常".into()));

            match tag_result {
                Ok(tag) => {
                    let better = announced.as_ref()
                        .map(|a| compare_versions(&tag, a) == Ordering::Greater)
                        .unwrap_or(true);
                    if is_version_like(&tag) && better {
                        announced = Some(tag);
                        announced_src = "Sophon(getBuild)";
                        announce_err = None;
                    }
                }
                Err(e) => println!("[sophon] 取 tag 失败(回退公告推断): {}", e),
            }
        }

        // ④ 远程"最新版本" = 整包 / 预下载 / 公告(或Sophon tag) 三者取最大
        let mut best = major.version.clone();
        let mut version_source = "整包".to_string();
        
        // 🌟 显式声明数组类型，彻底解决 Rust 生命周期推断报错
        let candidates: [(Option<String>, &str); 2] = [
            (pre_version.clone(), "预下载"),
            (announced.clone(), announced_src),
        ];
        
        for (cand, name) in candidates {
            if let Some(v) = cand {
                if is_version_like(&v) && compare_versions(&v, &best) == Ordering::Greater {
                    best = v;
                    version_source = name.to_string();
                }
            }
        }
        if let Some(e) = &announce_err {
            if version_source == "整包" { version_source = format!("整包（公告不可用：{}）", e); }
        }

        // ⑤ 版本守卫：本地 vs 整包（能下载的）+ 官宣（判断是否已落后于真实版本）
        let local = self.detect(app).into_iter().find(|g| g.id == game_id).and_then(|g| g.local_version);
        let mut relation = match &local {  // 🌟 改成 let mut relation
            None => "fresh",
            Some(lv) => match compare_versions(lv, &major.version) {
                Ordering::Less => "ahead",
                Ordering::Equal => if compare_versions(&best, &major.version) == Ordering::Greater { "announced" } else { "equal" },
                Ordering::Greater => "behind",
            },
        };

        // 🌟 Sophon 体系特判：原神/星铁整包落后是常态，按 Sophon/公告版本定关系
        if relation == "behind" && (biz == "hk4e_cn" || biz == "hkrpg_cn") {
            if let Some(lv) = &local {
                match compare_versions(&best, lv) {
                    Ordering::Greater => relation = "announced",
                    Ordering::Equal => relation = "equal",
                    _ => {}
                }
            }
        }

        // 解压后的安装体积：米哈游按"分卷"给 decompressed_size（末卷数值不同，可确认是按卷计的），求和即整包安装体积
        let install_size: u64 = major.game_pkgs.iter()
            .filter_map(|p| p.decompressed_size.parse::<u64>().ok())
            .sum();
        let install_size = if install_size > 0 { install_size } else { parts.iter().map(|p| p.size).sum() };



        

        Ok(RemoteGameInfo {
            game_id: game_id.into(), latest_version: major.version.clone(),
            package_url: parts[0].url.clone(), package_size: parts.iter().map(|p| p.size).sum(),
            parts, patch_from, patch_size,
            local_version: local, version_relation: relation.to_string(),
            patch_parts,
            install_size,
            res_list_url: major.res_list_url.clone(),
            announced_version: announced.clone(),
            version_source,
            checked_at: (epoch_ms() / 1000) as u64,
            resolved_biz: biz.clone(),
            resolved_game_id: hyp_game_id.clone(),
            launcher_id,
            launcher_id_source: lid_src,
        })
    }

    fn download(&self, app: &tauri::AppHandle, game_id: &str, dest: &str, use_patch: bool, allow_old: bool, cancel: Arc<AtomicBool>) -> Result<String, String> {
        let info = self.remote_info(app, game_id)?;

        // src-tauri/src/platforms/mihoyo.rs (在 download 函数内)

        // 🌟 新增：原神/星铁 路由到 Sophon 引擎 (同步版)
        if game_id == "genshin" || game_id == "starrail" {
            let biz = if game_id == "genshin" { "hk4e_cn" } else { "hkrpg_cn" };
            println!("[{}] 检测到 Sophon 体系游戏，启动 Chunk 更新引擎", game_id);
            
            let client = reqwest::blocking::Client::new();
            let bundle = crate::sophon::fetch_game_manifest(&client, biz, "游戏资源")
                .map_err(|e| format!("获取 Sophon 清单失败: {}", e))?;
                
            println!("[{}] 远程最新版本: {}", game_id, bundle.tag);

            let dest_path = Path::new(dest);
            let result = crate::sophon::apply_update(
                &client,
                &bundle.manifest,
                &bundle.chunk_prefix,
                dest_path,
                &dest_path.join("ql_staging"),
                move |p| {
                    // 🌟 阶段进度：转换为现有的 DownloadProgress 事件发送给前端
                    let progress = DownloadProgress {
                        game_id: game_id.into(),
                        downloaded: p.bytes_downloaded,
                        total: 0, // Sophon 边下边组装，不预知总下载量
                        speed: 0,   
                        eta_seconds: None,
                        status: format!("chunking:{}/{}|{}", p.chunks_done, p.chunks_total, p.current_file),
                    };
                    let _ = app.emit("download-progress", progress);
                },
            );

            if let Err(e) = result {
                return Err(format!("Sophon 更新失败: {}", e));
            }

            crate::sophon::bump_config_version(dest_path, &bundle.tag)?;
            println!("[{}] 版本号已更新为 {}", game_id, bundle.tag);
            
            let _ = app.emit("download-progress", DownloadProgress { 
                game_id: game_id.into(), downloaded: 0, total: 0, speed: 0, eta_seconds: Some(0), 
                status: format!("done:{}", dest) 
            });
            return Ok(format!("Sophon 更新完成: {}", bundle.tag));
        }

        

// ... (下方保留原有的 hdiff/整包下载逻辑，用于绝区零等) ...
        
        // P0：后端双保险守卫
        if info.version_relation == "equal" {
            return Err(format!("本地 v{} 已是最新，无需下载", info.latest_version));
        }
        // 官宣有新版、但整包停在旧版本：默认拦住（避免用户以为在下新版），允许二次确认强制下旧版
        if info.version_relation == "announced" && !allow_old {
            return Err(format!(
                "官方已发布 v{}（来源：{}），但接口整包仍停在 v{} —— 该游戏已改为分块分发，整包这条路拿不到新版。\n→ 请用『🩺 官方更新』拉起官方启动器更新；若确实要重装这个旧版本，会再问你一次。",
                info.announced_version.clone().unwrap_or_default(), info.version_source, info.latest_version));
        }
        if info.version_relation == "behind" {
            return Err(format!("接口整包 v{} 落后于本地 v{}，拒绝下载降级包（原神已转分块体系，整包停更）",
                info.latest_version, info.local_version.clone().unwrap_or_default()));
        }
        if info.parts.is_empty() { return Err("接口没有返回可下载的分卷".into()); }

        let (parts, out_dir) = if use_patch {
            let from = info.patch_from.clone().ok_or("接口未提供差分包")?;
            if info.patch_parts.is_empty() { return Err("差分包无可用分卷".into()); }
            let local = info.local_version.clone().unwrap_or_default();
            if local != from {
                return Err(format!("差分包从 v{} 升级，但本地是 v{}，版本不匹配", from, local));
            }
            let dir = Path::new(dest).join(format!("patch_{}_{}", from, info.latest_version));
            (info.patch_parts.clone(), dir)
        } else {
            (info.parts.clone(), PathBuf::from(dest))
        };

        // 空间预检：整包是"先下压缩卷、再解压出安装体积"，两者要同时放得下；
        // 不然就是白下 80 GB 然后卡在解压前（这正是我提醒过的坑）。
        {
            let dl: u64 = parts.iter().map(|p| p.size).sum();
            let need = if use_patch { dl * 3 } else { dl + info.install_size + 2 * 1024 * 1024 * 1024 };
            if let Some(free) = free_space_of(Path::new(dest)) {
                if free < need {
                    return Err(format!(
                        "目标盘空间不足：{} 需要约 {}（下载 {} + 解压后安装 {}），当前可用 {}。\n→ 换一个盘，或先清理空间。",
                        if use_patch { "差分下载" } else { "整包安装" },
                        crate::fmt_size(need), crate::fmt_size(dl), crate::fmt_size(info.install_size),
                        crate::fmt_size(free)));
                }
            }
        }

        std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
        let mut last_emit_downloaded: u64 = 0;
        let total: u64 = parts.iter().map(|p| p.size).sum();
        let limit = download_limit_bytes(app); // P1：配置化保命线
        let mut downloaded: u64 = 0;
        let mut last_emit = Instant::now();
        let client = reqwest::blocking::Client::new();
        let mut saved: Vec<PathBuf> = Vec::new();
        let cfg = read_channels(app);
        let buf_size = (cfg.download_buffer_mb.unwrap_or(4) * 1024 * 1024) as usize;
        let limit_bps = cfg.download_speed_limit_mbps.unwrap_or(0) * 1024 * 1024;
        let mut buf = vec![0u8; buf_size]; 

        for (idx, part) in parts.iter().enumerate() {
            let fname = part.url.split('?').next().unwrap_or("").rsplit('/').next()
                .unwrap_or(&format!("{}_{}.part{:03}", game_id, info.latest_version, idx + 1)).to_string();
            let out_path = out_dir.join(&fname);
            let already = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
            if part.size > 0 && already == part.size {
                downloaded += part.size;
                println!("[download] 跳过已下完的第 {}/{} 卷：{}", idx + 1, parts.len(), fname);
                saved.push(out_path);
                continue;
            }
            let mut stream = client.get(&part.url).send().map_err(|e| format!("第 {}/{} 卷请求失败: {}", idx + 1, parts.len(), e))?;
            if !stream.status().is_success() { return Err(format!("第 {}/{} 卷 HTTP {}", idx + 1, parts.len(), stream.status())); }
            println!("[download] 开始第 {}/{} 卷：{}（{}）", idx + 1, parts.len(), fname, crate::fmt_size(part.size));
            let mut file = std::fs::File::create(&out_path).map_err(|e| format!("创建 {} 失败: {}", fname, e))?;
            let mut md5ctx = md5::Context::new();
            loop {

                let loop_start = Instant::now(); // 记录本次循环开始时间
                
                if cancel.load(AtomicOrdering::Relaxed) {
                    drop(file); let _ = std::fs::remove_file(&out_path);
                    return Err("已取消".into());
                }
                if limit > 0 && downloaded >= limit {
                    drop(file);
                    println!("[download] 🧪 已达保命线 {} MB，安全截断", limit / 1024 / 1024);
                    let _ = app.emit("download-progress", DownloadProgress { game_id: game_id.into(), downloaded, total, speed: 0, eta_seconds: Some(0), status: format!("done_test:{}", dest) });
                    return Ok(format!("管道测试成功：截断于 {} MB，文件在 {}", limit / 1024 / 1024, dest));
                }
                let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 { break; }
                file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                md5ctx.consume(&buf[..n]);
                downloaded += n as u64;

                //限速 Sleep 逻辑（通过让出 CPU 时间片防止 100% 占用）
                if limit_bps > 0 {
                    let expected_duration = Duration::from_secs_f64(n as f64 / limit_bps as f64);
                    let elapsed = loop_start.elapsed();
                    if expected_duration > elapsed {
                        std::thread::sleep(expected_duration - elapsed);
                    }
                }

                if last_emit.elapsed() > Duration::from_millis(500) {
                    let now = Instant::now();
                    let elapsed = now.duration_since(last_emit);
                    if elapsed >= Duration::from_millis(500) {
                        // 计算瞬时速度 (B/s)
                        let speed = if elapsed.as_secs_f64() > 0.0 {
                            ((downloaded - last_emit_downloaded) as f64 / elapsed.as_secs_f64()) as u64
                        } else { 0 };
                        // 计算 ETA
                        let eta_seconds = if speed > 0 { 
                            Some((total.saturating_sub(downloaded)) / speed) 
                        } else { None };

                        let _ = app.emit("download-progress", DownloadProgress { 
                            game_id: game_id.into(), downloaded, total, speed, eta_seconds, status: "downloading".into() 
                        });
                        last_emit = now;
                        last_emit_downloaded = downloaded;
                    }
                }
            }
            let digest = format!("{:x}", md5ctx.compute());
            if !part.md5.is_empty() && digest != part.md5 {
                drop(file);
                let _ = std::fs::remove_file(&out_path);
                return Err(format!("第 {}/{} 卷 md5 校验失败：期望 {} 实际 {}", idx + 1, parts.len(), part.md5, digest));
            }
            println!("[download] 第 {}/{} 卷 md5 校验通过：{}", idx + 1, parts.len(), digest);
            saved.push(out_path);
        }
        println!("[download] 完成：{} 个分卷已落盘 → {}", saved.len(), dest);
        let mut extract_note = String::new();
        // 差分包是 .7z：下完即解压并侦察内层结构（合成所需的最后一块情报）
        if use_patch {
            for f in &saved {
                if f.extension().map(|e| e == "7z").unwrap_or(false) {
                    let extract_dir = out_dir.join("extracted");
                    std::fs::create_dir_all(&extract_dir).map_err(|e| e.to_string())?;
                    sevenz_rust::decompress_file(f, &extract_dir)
                        .map_err(|e| format!("7z 解压失败: {}", e))?;
                    println!("[patch] 已解压 {} → {}", f.display(), extract_dir.display());
                    if let Ok(rd) = std::fs::read_dir(&extract_dir) {
                        for e in rd.filter_map(|e| e.ok()) {
                            println!("[patch] 结构: {} ({})",
                                e.file_name().to_string_lossy(),
                                if e.path().is_dir() { "dir" } else { "file" });
                        }
                    }
                }
            }
        } else {
            // 整包：合并分卷 + 自动解压（不然还得用户自己拿 7-Zip 打开 .001 手动解）
            let keep = read_channels(app).keep_volumes_after_extract.unwrap_or(true);
            match extract_full_package(app, game_id, dest, &info.latest_version, &saved, keep) {
                Ok(msg) => { println!("[extract] {}", msg); extract_note = format!("；{}", msg); }
                Err(e) => {
                    return Err(format!(
                        "整包已下完，但自动解压失败：{}\n→ 分卷都还在 {}，可用 7-Zip 打开 .001 手动解压（也可以先把占用/空间问题解决后重跑）。",
                        e, dest));
                }
            }
        }
        let _ = app.emit("download-progress", DownloadProgress { game_id: game_id.into(), downloaded, total, speed: 0, eta_seconds: Some(0), status: format!("done:{}", dest) });
        Ok(format!("{} 个分卷已下载到 {}{}", saved.len(), dest, extract_note))
    }
}