pub mod platform;
pub mod platforms;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use sysinfo::{ProcessesToUpdate, System};
use tauri::{Emitter, Manager};
use winreg::enums::*;
use winreg::RegKey;

use serde::Serialize;
use crate::platform::{GameInfo, DownloadProgress, GamePlatform};
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
fn cancel_flag(game_id: &str) -> Arc<AtomicBool> {
    let map = CANCEL_FLAGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap();
    guard.entry(game_id.to_string()).or_insert_with(|| Arc::new(AtomicBool::new(false))).clone()
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

#[tauri::command]
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

#[tauri::command]
fn get_installed_games(app: tauri::AppHandle) -> Vec<GameInfo> { get_all_games_inner(&app) }

#[tauri::command]
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
                let name = p.exe().and_then(|e| e.file_name()).unwrap_or_default().to_string_lossy().to_lowercase();
                name.trim_end_matches(".exe") == wanted && p.memory() > 10 * 1024 * 1024
            });
            status_map.insert(game.id, is_running);
        }
    }
    let notepad_running = sys.processes().values().any(|p| {
        let name = p.exe().and_then(|e| e.file_name()).unwrap_or_default().to_string_lossy().to_lowercase();
        name.trim_end_matches(".exe") == "notepad" && p.memory() > 1024 * 1024
    });
    status_map.insert("test_notepad".to_string(), notepad_running);
    status_map
}

#[tauri::command]
fn check_remote(app: tauri::AppHandle, game_id: String) -> Result<crate::platform::RemoteGameInfo, String> {
    platform_for_game(&game_id).ok_or("无平台支持")?.remote_info(&app, &game_id)
}

#[tauri::command]
fn start_download(app: tauri::AppHandle, game_id: String, dest: String, use_patch: bool) -> Result<String, String> {
    let platform = platform_for_game(&game_id).ok_or("无平台支持")?;
    let flag = cancel_flag(&game_id);
    flag.store(false, Ordering::SeqCst);
    let app2 = app.clone(); let gid = game_id.clone();
    std::thread::spawn(move || {
        if let Err(e) = platform.download(&app2, &gid, &dest, use_patch, flag) {
            let _ = app2.emit("download-progress", DownloadProgress { game_id: gid, downloaded: 0, total: 0, status: format!("error:{}", e) });
        }
    });
    Ok("下载任务已启动".into())
}


#[tauri::command]
fn cancel_download(game_id: String) -> Result<(), String> {
    cancel_flag(&game_id).store(true, Ordering::SeqCst);
    Ok(())
}

#[tauri::command]
fn probe_api(app: tauri::AppHandle) -> Vec<String> {
    crate::platforms::mihoyo::probe_report(&app)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_installed_games, bind_game, launch_game, get_running_games,
            check_remote, start_download, cancel_download, probe_api,official_info, open_official,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}