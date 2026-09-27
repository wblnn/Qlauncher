use crate::platform::{
    compare_versions, DownloadProgress, GameInfo, GamePlatform, PlatformLevel,
    RemoteGameInfo, RemotePackagePart,
};
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
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
    ChannelsFile { launcher_id: None, download_limit_mb: None, download_buffer_mb: None, download_speed_limit_mbps: None, hpatchz_path: None }
}

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

/// 原地差分升级。dry_run=true 只校验+报计划，不写盘。
pub fn apply_patch(app: &tauri::AppHandle, game_id: &str, patch_dir: &str, dry_run: bool) -> Result<String, String> {
    let game_dir = crate::load_config(app).get(game_id).cloned().ok_or("游戏未绑定目录")?;
    let extracted = Path::new(patch_dir).join("extracted");
    let map: HDiffMap = serde_json::from_str(
        &std::fs::read_to_string(extracted.join("hdiffmap.json")).map_err(|e| e.to_string())?
    ).map_err(|e| format!("解析 hdiffmap.json 失败: {}", e))?;

    let mut replace_files = Vec::new();
    walk_replace_files(&extracted, &extracted, &mut replace_files);
    let delete_list: Vec<String> = std::fs::read_to_string(extracted.join("deletefiles.txt"))
        .unwrap_or_default().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();

    let mut synthesized = 0usize; let mut replaced = 0usize;
    let mut pending_delete: Vec<String> = delete_list.clone();

    // 1) 合成 diff_map：old + hdiff -> temp -> 校验 -> rename 到 target -> 记录删 source
    for (i, e) in map.diff_map.iter().enumerate() {
        let old = Path::new(&game_dir).join(&e.source_file_name);
        let diff = extracted.join(&e.patch_file_name);
        let new = Path::new(&game_dir).join(&e.target_file_name);
        if !old.exists() { return Err(format!("第{}条: 源文件缺失 {}（本地版本不匹配?）", i, e.source_file_name)); }
        if !diff.exists() { return Err(format!("第{}条: 增量缺失 {}", i, e.patch_file_name)); }
        let old_size = std::fs::metadata(&old).map(|m| m.len()).unwrap_or(0);
        if old_size != e.source_file_size {
            return Err(format!("第{}条: 源大小不符 {} 期望{} 实际{}", i, e.source_file_name, e.source_file_size, old_size));
        }
        if !dry_run {
            let temp = PathBuf::from(format!("{}.qtmp", new.display()));
            run_hpatchz(app, &old, &diff, &temp)?;
            let got = file_md5(&temp)?;
            if !e.target_file_md5.is_empty() && got != e.target_file_md5 {
                let _ = std::fs::remove_file(&temp);
                return Err(format!("第{}条: 合成 md5 不符 {} 期望{} 实际{}", i, e.target_file_name, e.target_file_md5, got));
            }
            if let Some(par) = new.parent() { std::fs::create_dir_all(par).map_err(|x| x.to_string())?; }
            if new.exists() { let _ = std::fs::remove_file(&new); }
            std::fs::rename(&temp, &new).map_err(|x| format!("rename 失败: {}", x))?;
        }
        synthesized += 1;
        if e.source_file_name != e.target_file_name { pending_delete.push(e.source_file_name.clone()); }
    }

    // 2) 直接替换文件（dll/exe/pkg_version 等整文件覆盖）
    for p in &replace_files {
        let rel = p.strip_prefix(&extracted).unwrap().to_string_lossy().replace('\\', "/");
        if !dry_run {
            let dst = Path::new(&game_dir).join(&rel);
            if let Some(par) = dst.parent() { std::fs::create_dir_all(par).map_err(|x| x.to_string())?; }
            std::fs::copy(p, &dst).map_err(|x| format!("复制 {} 失败: {}", rel, x))?;
        }
        replaced += 1;
    }

    // 3) 删除（deletefiles + 已取代的 source 旧文件）
    let deleted = pending_delete.len();
    if !dry_run {
        for rel in &pending_delete {
            let f = Path::new(&game_dir).join(rel);
            if f.exists() { let _ = std::fs::remove_file(&f); }
        }
    }

    // 4) 更新 config.ini 版本号
    if !dry_run {
        if let Ok(v) = std::fs::read_to_string(extracted.join("pkg_version")) {
            update_config_version(&game_dir, v.trim())?;
        }
    }

    Ok(format!("{}：合成 {} 条 / 直接替换 {} 个 / 待删 {} 个",
        if dry_run { "[预演] 计划" } else { "[完成]" }, synthesized, replaced, deleted))
}

fn hyp_get<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, String> {
    let resp = reqwest::blocking::get(url).map_err(|e| format!("网络错误: {}", e))?;
    resp.json::<T>().map_err(|e| format!("解析响应失败: {}", e))
}

#[derive(Deserialize)] pub struct HypRoot<T> { pub retcode: i64, #[serde(default)] pub message: String, pub data: Option<T> }
#[derive(Deserialize)] pub struct HypGamesData { #[serde(default)] pub games: Vec<HypGameEntry> }
#[derive(Deserialize)] pub struct HypGameEntry { #[serde(default)] pub id: String, #[serde(default)] pub biz: String }
#[derive(Deserialize)] pub struct HypPackagesData { #[serde(default)] pub game_packages: Vec<HypGamePackages> }
#[derive(Deserialize)] pub struct HypGamePackages { pub game: HypGameId, pub main: Option<HypMain> }
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
        let biz = MIHOYO_GAMES.iter().find(|(id, _, _, _)| *id == game_id).map(|(_, _, biz, _)| *biz).ok_or("未知的米哈游游戏")?;
        let (launcher_id, _src) = hyp_launcher_id(app);
        let url = format!("{}/getGamePackages?launcher_id={}&language={}", HYP_BASE_CN, launcher_id, HYP_LANGUAGE);
        let root: HypRoot<HypPackagesData> = hyp_get(&url)?;
        if root.retcode != 0 {
            return Err(format!("getGamePackages retcode={} message={}（launcher_id 可能已更换：改 channels.json 或点🔬自检）", root.retcode, root.message));
        }
        let all = root.data.ok_or("响应缺少 data")?;
        let gp = all.game_packages.into_iter().find(|g| g.game.biz == biz).ok_or_else(|| format!("接口未返回 {} 的包信息", biz))?;
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

        // P0：版本守卫 —— 取本地版本并计算关系
        let local = self.detect(app).into_iter().find(|g| g.id == game_id).and_then(|g| g.local_version);
        let relation = match &local {
            None => "fresh",
            Some(lv) => match compare_versions(lv, &major.version) {
                Ordering::Less => "ahead",
                Ordering::Equal => "equal",
                Ordering::Greater => "behind",
            },
        };

        Ok(RemoteGameInfo {
            game_id: game_id.into(), latest_version: major.version.clone(),
            package_url: parts[0].url.clone(), package_size: parts.iter().map(|p| p.size).sum(),
            parts, patch_from, patch_size,
            local_version: local, version_relation: relation.to_string(),
            patch_parts,
        })
    }

    fn download(&self, app: &tauri::AppHandle, game_id: &str, dest: &str, use_patch: bool, cancel: Arc<AtomicBool>) -> Result<String, String> {
        let info = self.remote_info(app, game_id)?;

        // P0：后端双保险守卫
        if info.version_relation == "equal" {
            return Err(format!("本地 v{} 已是最新，无需下载", info.latest_version));
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
        }
        let _ = app.emit("download-progress", DownloadProgress { game_id: game_id.into(), downloaded, total, speed: 0, eta_seconds: Some(0), status: format!("done:{}", dest) });
        Ok(format!("{} 个分卷已下载到 {}", saved.len(), dest))
    }
}