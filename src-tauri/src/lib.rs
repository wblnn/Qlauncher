pub mod platform;
pub mod platforms;
pub mod sophon;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use sysinfo::{ProcessesToUpdate, System};
use tauri::{Emitter, Manager};
use winreg::enums::*;
use winreg::RegKey;

use serde::Serialize;
use crate::platform::{GameInfo, DownloadProgress};
use crate::platforms::{get_platforms, platform_for_game};

// ================= 通用工具 =================
pub fn is_cloud_entry(name: &str) -> bool {
    name.contains("云") || name.to_lowercase().contains("cloud")
}

pub fn fmt_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 { v /= 1024.0; i += 1; }
    format!("{:.1} {}", v, UNITS[i])
}

pub fn scan_uninstall_registry() -> Vec<(String, String)> {
    let mut results = Vec::new();
    let roots = [
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        (HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];
    for (root, path) in roots {
        let predef = RegKey::predef(root);
        let uninstall = match predef.open_subkey_with_flags(path, KEY_READ) {
            Ok(k) => k,
            Err(_) => continue,
        };
        for key_name in uninstall.enum_keys().filter_map(|k| k.ok()) {
            if let Ok(sub) = uninstall.open_subkey(&key_name) {
                let name: String = sub.get_value("DisplayName").unwrap_or_default();
                let loc: String = sub.get_value("InstallLocation").unwrap_or_default();
                if !name.is_empty() && !loc.is_empty() { results.push((name, loc)); }
            }
        }
    }
    results
}

pub fn find_game_exe(dir: &Path, exes: &[&str]) -> Option<PathBuf> {
    for exe in exes { let p = dir.join(exe); if p.is_file() { return Some(p); } }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_dir() { for exe in exes { let q = p.join(exe); if q.is_file() { return Some(q); } } }
        }
    }
    None
}

pub fn read_local_version(dir: &str) -> Option<String> {
    let content = std::fs::read_to_string(Path::new(dir).join("config.ini")).ok()?;
    for line in content.lines() {
        if let Some(v) = line.trim().strip_prefix("game_version=") { return Some(v.trim().to_string()); }
    }
    None
}

fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("games.json"))
}

pub fn load_config(app: &tauri::AppHandle) -> HashMap<String, String> {
    config_path(app).ok().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_config(app: &tauri::AppHandle, map: &HashMap<String, String>) -> Result<(), String> {
    let p = config_path(app)?;
    let s = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;
    std::fs::write(p, s).map_err(|e| e.to_string())?;
    Ok(())
}

static CANCEL_FLAGS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
pub fn cancel_flag(game_id: &str) -> Arc<AtomicBool> {
    let map = CANCEL_FLAGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap();
    guard.entry(game_id.to_string()).or_insert_with(|| Arc::new(AtomicBool::new(false))).clone()
}

/// 取进程可比较的名字：优先 exe 文件名，取不到（0 线程残骸/受保护进程）回落 sysinfo 的进程名。
/// 之前只用 p.exe() 会把「已死但锁着游戏文件」的残骸全漏掉 —— 本次事故正是这么发生的。
fn proc_key(p: &sysinfo::Process) -> String {
    let exe_name = p.exe().and_then(|e| e.file_name())
        .map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    let name = if exe_name.is_empty() { p.name().to_string_lossy().to_lowercase() } else { exe_name };
    name.trim_end_matches(".exe").to_string()
}

// ================= 🧟 僵尸/残留进程识别 =================
#[derive(Serialize, Clone)]
pub struct ZombieProcess {
    pub pid: u32,
    pub name: String,
    pub memory: u64,
    pub game_id: String,
}

/// 列出"名字匹配已绑定游戏、但工作集 ≤ 10MB"的残留进程
/// （与 get_running_games 的 >10MB 判活阈值互为反面，口径一致）
#[tauri::command(async)]
fn list_zombie_games(app: tauri::AppHandle) -> Vec<ZombieProcess> {
    let games = get_all_games_inner(&app);
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All);
    let mut out = Vec::new();
    for game in games {
        if !game.installed { continue; }
        if let Some(exe_path) = game.exe {
            let wanted = Path::new(&exe_path).file_name()
                .unwrap_or_default().to_string_lossy().to_lowercase();
            let wanted = wanted.trim_end_matches(".exe").to_string();
            for (pid, p) in sys.processes() {
                if proc_key(p) == wanted && p.memory() <= 10 * 1024 * 1024 {
                    out.push(ZombieProcess {
                        pid: pid.as_u32(), name: proc_key(p),
                        memory: p.memory(), game_id: game.id.clone(),
                    });
                }
            }
        }
    }
    out
}

/// 按 PID 精准清理（只杀僵尸，不误伤活游戏）
#[tauri::command]
fn kill_process(pid: u32) -> Result<String, String> {
    let output = std::process::Command::new("cmd")
        .args(["/C", "taskkill", "/F", "/PID", &pid.to_string()])
        .output()
        .map_err(|e| format!("调用 taskkill 失败: {}", e))?;
    if output.status.success() {
        Ok(format!("已结束 PID {}", pid))
    } else {
        Err(format!("结束失败: {}", String::from_utf8_lossy(&output.stderr)))
    }
}

// ================= 官方启动器识别（本地优先） =================
/// (platform_id, 注册表 DisplayName 关键词, 官启主程序候选名)
const OFFICIAL_LAUNCHER_HINTS: &[(&str, &[&str], &[&str])] = &[
    ("mihoyo", &["米哈游启动器", "HoYoPlay"], &["HoYoPlay.exe", "launcher.exe"]),
    ("steam",  &["Steam"],                    &["steam.exe"]),
    ("riot",   &["Riot Client"],              &["RiotClientServices.exe", "RiotClientUx.exe"]),
];

#[derive(Serialize)]
pub struct OfficialInfo {
    pub local: Option<String>,     // 本机官启 exe 路径（None = 没识别到）
    pub web: Option<String>,       // 官网网页
    pub fallback: Option<String>,  // auto 模式兜底地址（协议或官网）
}

pub fn find_local_launcher(platform_id: &str) -> Option<String> {
    let (_, keywords, exes) = OFFICIAL_LAUNCHER_HINTS.iter().find(|(id, _, _)| *id == platform_id)?;
    for (name, loc) in scan_uninstall_registry() {
        if is_cloud_entry(&name) { continue; }
        if !keywords.iter().any(|k| name.contains(k)) { continue; }
        let dir = Path::new(&loc);
        if !dir.exists() { continue; }
        if let Some(exe) = find_game_exe(dir, exes) { return Some(exe.to_string_lossy().into_owned()); }
    }
    None
}

#[tauri::command(async)]
fn official_info(game_id: String) -> Result<OfficialInfo, String> {
    let platform = platform_for_game(&game_id).ok_or("无平台支持")?;
    Ok(OfficialInfo {
        local: find_local_launcher(platform.id()),
        web: platform.official_website(&game_id),
        fallback: platform.official_launcher(),
    })
}

/// mode = "auto"（本地官启优先，识别不到开兜底）| "web"（只开官网）
#[tauri::command]
fn open_official(game_id: String, mode: String) -> Result<String, String> {
    let platform = platform_for_game(&game_id).ok_or("无平台支持")?;
    if mode == "web" {
        let web = platform.official_website(&game_id).ok_or("该平台未配置官网")?;
        std::process::Command::new("cmd").args(["/C", "start", &web]).spawn().map_err(|e| e.to_string())?;
        return Ok(format!("已打开官网: {}", web));
    }
    if let Some(exe) = find_local_launcher(platform.id()) {
        let dir = Path::new(&exe).parent().map(|p| p.to_path_buf());
        let mut cmd = std::process::Command::new(&exe);
        if let Some(d) = dir { cmd.current_dir(d); }
        cmd.spawn().map_err(|e| format!("拉起本地官启失败: {}", e))?;
        return Ok(format!("已拉起本地官方启动器: {}", exe));
    }
    let fb = platform.official_website(&game_id)
        .or_else(|| platform.official_launcher())
        .ok_or("未识别到本地官启，且无兜底地址")?;
    std::process::Command::new("cmd").args(["/C", "start", &fb]).spawn().map_err(|e| e.to_string())?;
    Ok(format!("未识别到本地官启，已打开兜底地址: {}", fb))
}

// ================= Tauri 命令 (调度层) =================
fn get_all_games_inner(app: &tauri::AppHandle) -> Vec<GameInfo> {
    get_platforms().iter().flat_map(|p| p.detect(app)).collect()
}

#[tauri::command(async)]
fn get_installed_games(app: tauri::AppHandle) -> Vec<GameInfo> { get_all_games_inner(&app) }

#[tauri::command(async)]
fn bind_game(app: tauri::AppHandle, id: String, dir: String) -> Result<GameInfo, String> {
    let platform = platform_for_game(&id).ok_or("无平台支持")?;
    // P3：exe 候选名由平台自己声明
    let def_exes = platform.exe_candidates(&id);
    if def_exes.is_empty() { return Err("该平台不支持手动绑定目录".into()); }
    let exe = find_game_exe(Path::new(&dir), &def_exes).ok_or("没找到主程序")?;
    let mut cfg = load_config(&app);
    cfg.insert(id.clone(), dir.clone());
    save_config(&app, &cfg)?;
    let local_version = read_local_version(&dir);
    let mut game = platform.detect(&app).into_iter().find(|g| g.id == id).ok_or_else(|| format!("平台未报告该游戏: {}", id))?;
    game.installed = true; game.path = Some(dir); game.exe = Some(exe.to_string_lossy().into_owned()); game.local_version = local_version;
    Ok(game)
}

#[tauri::command]
fn launch_game(app: tauri::AppHandle, id: String, use_official: bool) -> Result<String, String> {
    if id == "test_notepad" { std::process::Command::new("notepad.exe").spawn().map_err(|e| e.to_string())?; return Ok("记事本已启动".into()); }
    let platform = platform_for_game(&id).ok_or("无平台支持")?;
    if use_official {
        if let Some(uri) = platform.official_launcher() {
            std::process::Command::new("cmd").args(["/C", "start", &uri]).spawn().map_err(|e| e.to_string())?;
            return Ok(format!("已唤起官方启动器: {}", uri));
        }
        return Err("该平台无官方启动器配置".into());
    }
    let game = get_all_games_inner(&app).into_iter().find(|g| g.id == id).ok_or("找不到游戏")?;
    platform.launch_direct(&game)
}

#[tauri::command]
fn kill_game(app: tauri::AppHandle, game_id: String) -> Result<String, String> {
    if game_id == "test_notepad" {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "taskkill", "/F", "/IM", "notepad.exe", "/T"])
            .output();
        return Ok("已发送关闭记事本指令".into());
    }

    let games = get_all_games_inner(&app);
    let game = games.into_iter().find(|g| g.id == game_id)
        .ok_or("找不到该游戏配置")?;
    
    let exe_path = game.exe.ok_or("该游戏未绑定 exe 路径")?;
    let exe_name = Path::new(&exe_path).file_name()
        .ok_or("无法解析 exe 文件名")?
        .to_string_lossy();

    // /F = 强制结束, /IM = 镜像名(exe名), /T = 结束进程树(防反作弊残留)
    let output = std::process::Command::new("cmd")
        .args(["/C", "taskkill", "/F", "/IM", &exe_name, "/T"])
        .output()
        .map_err(|e| format!("调用 taskkill 失败: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    
    if output.status.success() {
        Ok(format!("已强制关闭 {} 及其子进程", exe_name))
    } else if stderr.contains("没有找到进程") || stdout.contains("没有找到进程") {
        Ok(format!("{} 当前未在运行", exe_name))
    } else {
        Err(format!("关闭失败: {} {}", stdout, stderr))
    }
}

#[tauri::command(async)]
fn get_running_games(app: tauri::AppHandle) -> HashMap<String, bool> {
    let games = get_all_games_inner(&app);
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All);
    let mut status_map = HashMap::new();
    for game in games {
        if !game.installed { continue; }
        if let Some(exe_path) = game.exe {
            let wanted = Path::new(&exe_path).file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            let wanted = wanted.trim_end_matches(".exe").to_string();
            let is_running = sys.processes().values().any(|p| {
                proc_key(p) == wanted && p.memory() > 10 * 1024 * 1024
            });
            status_map.insert(game.id, is_running);
        }
    }
    let notepad_running = sys.processes().values().any(|p| {
        proc_key(p) == "notepad" && p.memory() > 1024 * 1024
    });
    status_map.insert("test_notepad".to_string(), notepad_running);
    status_map
}

#[tauri::command(async)]
fn check_remote(app: tauri::AppHandle, game_id: String) -> Result<crate::platform::RemoteGameInfo, String> {
    platform_for_game(&game_id).ok_or("无平台支持")?.remote_info(&app, &game_id)
}

#[tauri::command]
fn start_download(app: tauri::AppHandle, game_id: String, dest: String, use_patch: bool, allow_old: bool) -> Result<String, String> {
    let platform = platform_for_game(&game_id).ok_or("无平台支持")?;
    let flag = cancel_flag(&game_id);
    flag.store(false, Ordering::SeqCst);
    let app2 = app.clone(); let gid = game_id.clone();
    std::thread::spawn(move || {
        if let Err(e) = platform.download(&app2, &gid, &dest, use_patch, allow_old, flag) {
            let _ = app2.emit("download-progress", DownloadProgress { 
                game_id: gid, downloaded: 0, total: 0, speed: 0, eta_seconds: None, status: format!("error:{}", e) 
            });
        }
    });
    Ok("下载任务已启动".into())
}


#[tauri::command]
fn cancel_download(game_id: String) -> Result<(), String> {
    cancel_flag(&game_id).store(true, Ordering::SeqCst);
    Ok(())
}

#[tauri::command(async)]
fn probe_api(app: tauri::AppHandle) -> Vec<String> {
    crate::platforms::mihoyo::probe_report(&app)
}

#[tauri::command(async)]
fn apply_patch(app: tauri::AppHandle, game_id: String, patch_dir: String, dry_run: bool) -> Result<String, String> {
    if dry_run {
        return crate::platforms::mihoyo::apply_patch(&app, &game_id, &patch_dir, true);
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        let gid = game_id.clone();
        let res = crate::platforms::mihoyo::apply_patch(&app2, &gid, &patch_dir, false);
        let status = match res { Ok(m) => format!("done:{}", m), Err(e) => format!("error:{}", e) };
        let _ = app2.emit("patch-progress", crate::platforms::mihoyo::PatchProgress {
            game_id, done: 0, total: 0, current: String::new(), status,
        });
    });
    Ok("合成已在后台启动".into())
}

// ================= 更新应变：继续 / 回滚 =================
#[tauri::command]
fn patch_status(app: tauri::AppHandle, game_id: String) -> Option<crate::platforms::mihoyo::PatchJournal> {
    crate::platforms::mihoyo::patch_status(&app, &game_id)
}

/// 断点续跑：用 journal 里记的 patch_dir 再跑一遍（每步幂等 + 可逆）
#[tauri::command]
fn resume_patch(app: tauri::AppHandle, game_id: String) -> Result<String, String> {
    let j = crate::platforms::mihoyo::patch_status(&app, &game_id).ok_or("没有未完成的更新记录")?;
    if j.patch_dir.is_empty() { return Err("更新记录里没有补丁目录，无法续跑（可改用『↩ 回滚』）".into()); }
    let app2 = app.clone();
    std::thread::spawn(move || {
        let gid = game_id.clone();
        let res = crate::platforms::mihoyo::apply_patch(&app2, &gid, &j.patch_dir, false);
        let status = match res { Ok(m) => format!("done:{}", m), Err(e) => format!("error:{}", e) };
        let _ = app2.emit("patch-progress", crate::platforms::mihoyo::PatchProgress {
            game_id: gid, done: 0, total: 0, current: String::new(), status });
    });
    Ok("继续更新已在后台启动".into())
}

/// 一键回滚到补丁前：还原 .qold、删掉本次新增文件、复原 config.ini
#[tauri::command(async)]
fn rollback_patch(app: tauri::AppHandle, game_id: String) -> Result<String, String> {
    crate::platforms::mihoyo::rollback_patch(&app, &game_id)
}

// ================= 🩹 校验修复 =================
/// 按官方清单（pkg_version）逐文件校验本地：快速=只比大小，deep=逐文件算 md5。
/// 重活丢进独立线程并带超时 —— 同 hyp_get 的理由：命令是 (async)，函数体会跑在运行时线程上。
#[tauri::command(async)]
fn verify_game_files(app: tauri::AppHandle, game_id: String, deep: bool) -> Result<crate::platforms::mihoyo::VerifyReport, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let app2 = app.clone();
    std::thread::spawn(move || {
        let _ = tx.send(crate::platforms::mihoyo::verify_files(&app2, &game_id, deep));
    });
    rx.recv_timeout(std::time::Duration::from_secs(1800))
        .map_err(|_| "校验超时（30 分钟）".to_string())?
}

/// 按校验结果只补缺失/损坏的文件（不等同整包重装）
#[tauri::command(async)]
fn repair_game_files(app: tauri::AppHandle, game_id: String) -> Result<String, String> {
    crate::platforms::mihoyo::repair_files(&app, &game_id)
}

// src-tauri/src/lib.rs
use sysinfo::Disks;

/// 检查目标路径所在磁盘的可用空间 (单位: MB)
pub fn check_available_space_mb(path: &Path) -> Result<u64, String> {
    let disks = Disks::new_with_refreshed_list();
    // 规范化路径为绝对路径，确保能正确匹配挂载点
    let abs_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    
    for disk in disks.list() {
        if abs_path.starts_with(disk.mount_point()) {
            return Ok(disk.available_space() / 1024 / 1024);
        }
    }
    Err("无法识别目标路径所在磁盘分区".to_string())
}

/// hpatchz.exe 外置解析：优先 exe 同目录（打包版），回退项目目录（dev）
pub fn find_hpatchz() -> Result<PathBuf, String> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("hpatchz.exe"));
        }
    }
    cands.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hpatchz.exe"));
    for c in cands {
        if c.exists() {
            return Ok(c);
        }
    }
    Err("未找到 hpatchz.exe：请把它放在 qlauncher.exe 同目录".into())
}


#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_installed_games, bind_game, launch_game, get_running_games,
            check_remote, start_download, cancel_download, probe_api,official_info, 
            open_official,
            apply_patch,
            kill_game,list_zombie_games, kill_process,
            patch_status, resume_patch, rollback_patch,
            verify_game_files, repair_game_files,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}



