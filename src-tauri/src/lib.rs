pub mod digest;
pub mod path_safety;
pub mod trust;
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

// ================= QL-04：每游戏任务互斥 + 独立取消令牌 =================
/// 会写游戏目录 / 长时间占用游戏目录的重任务类型
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskKind { Download, Patch, Verify, Repair, Rollback }

impl TaskKind {
    fn label(self) -> &'static str {
        match self {
            TaskKind::Download => "下载",
            TaskKind::Patch    => "应用差分",
            TaskKind::Verify   => "校验",
            TaskKind::Repair   => "修复",
            TaskKind::Rollback => "回滚",
        }
    }
}

struct TaskSlot { kind: TaskKind, token: Arc<AtomicBool> }

static TASKS: OnceLock<Mutex<HashMap<String, TaskSlot>>> = OnceLock::new();
static IDLE_FLAGS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();

fn tasks() -> &'static Mutex<HashMap<String, TaskSlot>> {
    TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// QL-04：取号。同一游戏同一时间只允许一个重任务。
///
/// 旧实现每个游戏共用一个 Arc<AtomicBool>，而 start_download 又在 spawn 之前把它 store(false)：
/// A 还在跑时点 B，B 会把 A 的取消位清掉（A 再也停不下来），两个线程同时改名/删除同一批文件。
/// 现在每次任务新建独立令牌，且同一游戏只允许一个任务。
pub fn acquire_task(game_id: &str, kind: TaskKind) -> Result<TaskGuard, String> {
    let mut map = tasks().lock().unwrap();
    if let Some(slot) = map.get(game_id) {
        return Err(format!(
            "该游戏已有「{}」任务在进行中，请等它结束或先取消，再执行「{}」",
            slot.kind.label(), kind.label()
        ));
    }
    let token = Arc::new(AtomicBool::new(false));
    map.insert(game_id.to_string(), TaskSlot { kind, token: token.clone() });
    Ok(TaskGuard { game_id: game_id.to_string(), token })
}

fn release_task(game_id: &str) { tasks().lock().unwrap().remove(game_id); }

/// QL-04：任务守卫。移进执行线程，线程结束（成功 / 失败 / 提前 return）自动释放占用。
pub struct TaskGuard { game_id: String, pub token: Arc<AtomicBool> }

impl Drop for TaskGuard {
    fn drop(&mut self) { release_task(&self.game_id); }
}

/// 取当前任务的取消令牌；没有任务在跑时返回一个空闲令牌（兼容平台内部调用点）。
pub fn cancel_flag(game_id: &str) -> Arc<AtomicBool> {
    let existing = { tasks().lock().unwrap().get(game_id).map(|s| s.token.clone()) };
    if let Some(t) = existing { return t; }
    let map = IDLE_FLAGS.get_or_init(|| Mutex::new(HashMap::new()));
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

/// QL-03 归属判定的纯逻辑（便于单测）：主判据是"镜像名属于已绑定游戏主程序"，
/// 辅助判据是"工作集 ≤ 10MB"（与 list_zombie_games 口径一致，不以内存作为唯一判据）。
fn check_kill_allowed(allowed: &[String], pid: u32, key: &str, mem: u64) -> Result<(), String> {
    if !allowed.iter().any(|a| *a == key) {
        return Err(format!(
            "拒绝结束 PID {}（{}）：它不属于任何已绑定游戏的主程序（{}）",
            pid, key, allowed.join(" / ")
        ));
    }
    if mem > 10 * 1024 * 1024 {
        return Err(format!(
            "拒绝结束 PID {}（{}）：工作集 {} 超过 10MB，看起来是正在运行的游戏，请用「关闭游戏」",
            pid, key, crate::fmt_size(mem)
        ));
    }
    Ok(())
}

/// 按 PID 精准清理残留进程。
///
/// QL-03：这是唯一"按 PID 终止进程"的入口，而 PID 来自前端。原先只把 PID 原样交给
/// taskkill，等于把"结束任意进程"的能力暴露给了 WebView（XSS 或被篡改的前端即可滥用）。
/// 后端必须二次校验归属，安全检查放在真正动手的位置：
///   ① 进程镜像名必须等于某个"已绑定游戏"的主程序名（主判据）；
///   ② 工作集必须 ≤ 10MB（与 list_zombie_games 的僵尸口径一致，辅助判据）；
///      正在运行的游戏请走 kill_game。
#[tauri::command]
fn kill_process(app: tauri::AppHandle, pid: u32) -> Result<String, String> {
    let allowed: Vec<String> = get_all_games_inner(&app)
        .into_iter()
        .filter(|g| g.installed)
        .filter_map(|g| g.exe)
        .filter_map(|exe| {
            Path::new(&exe).file_name().map(|n| {
                n.to_string_lossy().to_lowercase().trim_end_matches(".exe").to_string()
            })
        })
        .collect();
    if allowed.is_empty() {
        return Err("当前没有任何已绑定游戏，拒绝结束进程".into());
    }

    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All);
    let target = sysinfo::Pid::from_u32(pid);
    let (key, mem) = match sys.processes().get(&target) {
        Some(p) => (proc_key(p), p.memory()),
        None => return Err(format!("PID {} 不存在或已退出", pid)),
    };
    check_kill_allowed(&allowed, pid, &key, mem)?;

    let output = std::process::Command::new("cmd")
        .args(["/C", "taskkill", "/F", "/PID", &pid.to_string()])
        .output()
        .map_err(|e| format!("调用 taskkill 失败: {}", e))?;
    if output.status.success() {
        Ok(format!("已结束 PID {}（{}）", pid, key))
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

/// QL-07：允许"用系统默认程序打开"的目标白名单。
/// 当前代码里这些值全部来自平台硬编码常量，但这条执行链一旦接进动态值就是命令执行面，
/// 所以这里显式限定：官网域名（https）+ 少量固定协议（steam:）。
const OPEN_HTTPS_DOMAINS: &[&str] = &[
    "mihoyo.com", "hoyoverse.com", "kurogames.com", "steampowered.com", "playvalorant.com",
];
const OPEN_URI_SCHEMES: &[&str] = &["steam:"];

fn trusted_open_target(target: &str) -> Result<(), String> {
    let t = target.trim();
    if t.is_empty() { return Err("打开目标为空".into()); }
    if t.chars().any(|c| c.is_control()) { return Err(format!("打开目标含控制字符: {:?}", t)); }
    if crate::trust::is_trusted_https_host(t, OPEN_HTTPS_DOMAINS) { return Ok(()); }
    let lower = t.to_ascii_lowercase();
    if OPEN_URI_SCHEMES.iter().any(|s| lower.starts_with(s)) { return Ok(()); }
    Err(format!(
        "拒绝打开白名单以外的目标: {}\n→ 如果是新增平台/新域名，请把域名加进 lib.rs 的 OPEN_HTTPS_DOMAINS。",
        t
    ))
}

/// QL-07：统一的"用系统默认程序打开"入口。
/// ① 先过白名单校验；
/// ② 不再走 cmd /C start —— 那是把值交给 cmd.exe 解析，& | ^ " 等元字符会被当作命令语法。
///    改用 ShellExecute 系的 open_url（tauri-plugin-opener），完全不经过 shell。
pub fn open_external(target: &str) -> Result<(), String> {
    trusted_open_target(target)?;
    tauri_plugin_opener::open_url(target, None::<&str>)
        .map_err(|e| format!("打开 {} 失败: {}", target, e))
}

/// mode = "auto"（本地官启优先，识别不到开兜底）| "web"（只开官网）
#[tauri::command]
fn open_official(game_id: String, mode: String) -> Result<String, String> {
    let platform = platform_for_game(&game_id).ok_or("无平台支持")?;
    if mode == "web" {
        let web = platform.official_website(&game_id).ok_or("该平台未配置官网")?;
        open_external(&web)?;
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
    open_external(&fb)?;
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
            open_external(&uri)?;
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
    // ⚠️ 网络请求一律丢到独立系统线程：本命令是 (async)，函数体跑在 tokio worker 上，
    //    在那里创建/析构 reqwest::blocking 客户端会 panic（Cannot drop a runtime…）
    //    → 命令既不返回也不报错、前端「点了没反应」。顺带加硬超时，网络卡住不会挂死。
    let platform = platform_for_game(&game_id).ok_or("无平台支持")?;
    // QL-04 补充：超时不能只回 Err —— 后台线程还在跑，必须置取消位让它自己停下，
    // 否则每次查版本超时都会漏一个线程。库洛会把令牌轮询进它的本地对账里。
    let token = Arc::new(AtomicBool::new(false));
    let token2 = token.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(platform.remote_info_cancellable(&app, &game_id, token2));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(90)) {
        Ok(r) => r,
        Err(_) => {
            token.store(true, Ordering::SeqCst);
            Err("查版本超时（90 秒）：已请求中止后台查询，稍后重试".to_string())
        }
    }
}

#[tauri::command]
fn start_download(app: tauri::AppHandle, game_id: String, dest: String, use_patch: Option<bool>, allow_old: Option<bool>) -> Result<String, String> {
    // 前端可能传 null（JS 的 && 链很容易把 null 原样送过来）—— 这里宽容成 false，
    // 免得好端端的下载被 "invalid args: invalid type: null, expected a boolean" 拦住
    let use_patch = use_patch.unwrap_or(false);
    let allow_old = allow_old.unwrap_or(false);
    let platform = platform_for_game(&game_id).ok_or("无平台支持")?;
    // QL-04：先取号（已有任务直接拒绝）；令牌由本次任务独享，不再共用/重置
    let guard = acquire_task(&game_id, TaskKind::Download)?;
    let flag = guard.token.clone();
    let app2 = app.clone(); let gid = game_id.clone();
    std::thread::spawn(move || {
        let _guard = guard; // 线程结束（成功或失败）自动释放占用
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
    // QL-04：合成会改名/删除游戏目录里的文件，必须独占该游戏
    let guard = acquire_task(&game_id, TaskKind::Patch)?;
    let app2 = app.clone();
    std::thread::spawn(move || {
        let _guard = guard;
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
    // QL-04：续跑同样独占该游戏
    let guard = acquire_task(&game_id, TaskKind::Patch)?;
    let app2 = app.clone();
    std::thread::spawn(move || {
        let _guard = guard;
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
    // QL-04：回滚会还原/删除文件，独占该游戏
    let _guard = acquire_task(&game_id, TaskKind::Rollback)?;
    crate::platforms::mihoyo::rollback_patch(&app, &game_id)
}

// ================= 🩹 校验修复 =================
/// 按官方清单（pkg_version）逐文件校验本地：快速=只比大小，deep=逐文件算 md5。
/// 重活丢进独立线程并带超时 —— 同 hyp_get 的理由：命令是 (async)，函数体会跑在运行时线程上。
#[tauri::command(async)]
fn verify_game_files(app: tauri::AppHandle, game_id: String, deep: bool) -> Result<crate::platforms::mihoyo::VerifyReport, String> {
    // 库洛（鸣潮）：没有 pkg_version，走"官方清单对账"；同样丢线程 + 超时
    if is_kuro(&game_id) {
        // QL-04：校验是重任务，独占该游戏
        let guard = acquire_task(&game_id, TaskKind::Verify)?;
        let gid = game_id.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let app2 = app.clone();
        std::thread::spawn(move || {
            let _guard = guard;
            let _ = tx.send(crate::platforms::kuro::kuro_verify(&app2, &gid, deep));
        });
        // 内容级对账要把安装目录读一遍（80GB 大约几分钟），给足时间
        return match rx.recv_timeout(std::time::Duration::from_secs(3600)) {
            Ok(v) => v,
            Err(_) => {
                // 超时不能只回 Err：后台线程还在读整盘，必须置取消位让它自己停下
                cancel_flag(&game_id).store(true, Ordering::SeqCst);
                Err("库洛对账超时（60 分钟）：已请求中止后台对账（已核对的部分不受影响）".to_string())
            }
        };
    }
    // QL-04：同上，独占该游戏
    let guard = acquire_task(&game_id, TaskKind::Verify)?;
    let gid = game_id.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let app2 = app.clone();
    std::thread::spawn(move || {
        let _guard = guard;
        let _ = tx.send(crate::platforms::mihoyo::verify_files(&app2, &gid, deep));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(1800)) {
        Ok(v) => v,
        Err(_) => {
            // 同理：让后台的逐文件校验看到取消位后自己收尾
            cancel_flag(&game_id).store(true, Ordering::SeqCst);
            Err("校验超时（30 分钟）：已请求中止后台校验（未改动任何文件）".to_string())
        }
    }
}

fn is_kuro(game_id: &str) -> bool {
    platform_for_game(game_id).map(|p| p.id() == "kuro").unwrap_or(false)
}

/// 按校验结果只补缺失/损坏的文件（不等同整包重装）
#[tauri::command(async)]
fn repair_game_files(app: tauri::AppHandle, game_id: String) -> Result<String, String> {
    // 库洛：清单差异集重下（引擎自带分块续传 + 官方 md5 校验），作用在已绑定目录。
    // 鸣潮一次可能要下几十 GB，所以这里只"点火"不等待，进度/结果都走事件（同 start_download）。
    if is_kuro(&game_id) {
        let dir = crate::load_config(&app).get(&game_id).cloned().ok_or("游戏未绑定目录")?;
        // QL-04：修复会写游戏目录，独占该游戏；令牌由本次任务独享
        let guard = acquire_task(&game_id, TaskKind::Repair)?;
        let cancel = guard.token.clone();
        let app2 = app.clone();
        let gid = game_id.clone();
        std::thread::spawn(move || {
            let _guard = guard;
            if let Err(e) = crate::platforms::kuro::kuro_download(&app2, &gid, &dir, cancel) {
                let _ = app2.emit("download-progress", DownloadProgress {
                    game_id: gid, downloaded: 0, total: 0, speed: 0, eta_seconds: None,
                    status: format!("error:{}", e),
                });
            }
        });
        return Ok("库洛清单修复已启动：差异文件会逐个下载并用官方 md5 校验（进度看进度条，可随时 ✖ 取消）".into());
    }
    // 米哈游：repair_files 自己起线程，守卫交给它，由它的线程结束时释放
    let guard = acquire_task(&game_id, TaskKind::Repair)?;
    crate::platforms::mihoyo::repair_files(&app, &game_id, guard)
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

/// hpatchz.exe 外置解析（QL-05）：只认可信根 + 固定 SHA-256，不再"存在即执行"。
pub fn find_hpatchz() -> Result<PathBuf, String> {
    crate::trust::resolve_executable(
        &crate::trust::trusted_exe_roots_plain(),
        None,
        "hpatchz.exe",
        crate::trust::HPATCHZ_SHA256,
    )
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

#[cfg(test)]
mod ql03_tests {
    use super::check_kill_allowed;

    fn allowed() -> Vec<String> {
        vec!["genshinimpact".to_string(), "starrail".to_string()]
    }

    #[test]
    fn allows_bound_game_low_memory_process() {
        assert!(check_kill_allowed(&allowed(), 1234, "genshinimpact", 1024 * 1024).is_ok());
    }

    #[test]
    fn rejects_process_of_unbound_game() {
        let r = check_kill_allowed(&allowed(), 1234, "explorer", 1024 * 1024);
        assert!(r.is_err(), "未绑定游戏的进程必须拒绝");
        assert!(r.unwrap_err().contains("不属于任何已绑定游戏"));
    }

    #[test]
    fn rejects_running_game_by_memory() {
        let r = check_kill_allowed(&allowed(), 1234, "genshinimpact", 100 * 1024 * 1024);
        assert!(r.is_err(), "正在运行的游戏必须拒绝");
        assert!(r.unwrap_err().contains("超过 10MB"));
    }

    #[test]
    fn memory_boundary_is_inclusive() {
        assert!(check_kill_allowed(&allowed(), 1, "starrail", 10 * 1024 * 1024).is_ok());
        assert!(check_kill_allowed(&allowed(), 1, "starrail", 10 * 1024 * 1024 + 1).is_err());
    }

    #[test]
    fn rejects_empty_allowlist() {
        assert!(check_kill_allowed(&[], 1, "genshinimpact", 0).is_err());
    }
}

#[cfg(test)]
mod ql04_tests {
    use super::*;

    #[test]
    fn only_one_task_per_game() {
        let g = "ql04_test_only_one";
        let guard = acquire_task(g, TaskKind::Download).expect("第一次取号应成功");
        let second = acquire_task(g, TaskKind::Patch).err();
        assert!(second.is_some(), "同一游戏第二次取号必须被拒绝");
        assert!(second.unwrap().contains("已有"));
        drop(guard);
        assert!(acquire_task(g, TaskKind::Patch).is_ok(), "释放后应能再次取号");
        release_task(g);
    }

    #[test]
    fn cancel_flag_points_at_running_task_token() {
        let g = "ql04_test_cancel_token";
        let guard = acquire_task(g, TaskKind::Verify).unwrap();
        let via_fn = cancel_flag(g);
        assert!(Arc::ptr_eq(&via_fn, &guard.token), "cancel_flag 必须返回当前任务的令牌");
        via_fn.store(true, Ordering::SeqCst);
        assert!(guard.token.load(Ordering::SeqCst), "取消必须作用到任务令牌");
        drop(guard);
    }

    #[test]
    fn tasks_are_isolated_per_game() {
        let a = acquire_task("ql04_test_iso_a", TaskKind::Download).unwrap();
        let b = acquire_task("ql04_test_iso_b", TaskKind::Download).unwrap();
        assert!(!Arc::ptr_eq(&a.token, &b.token), "不同游戏必须有独立令牌");
        drop(a);
        drop(b);
    }

    #[test]
    fn idle_cancel_does_not_leak_into_next_task() {
        let g = "ql04_test_idle";
        // 没有任务时的取消只影响空闲令牌，不能污染下一个任务
        cancel_flag(g).store(true, Ordering::SeqCst);
        let guard = acquire_task(g, TaskKind::Download).unwrap();
        assert!(!guard.token.load(Ordering::SeqCst), "新任务必须是干净的令牌");
        drop(guard);
    }
}

#[cfg(test)]
mod ql07_tests {
    use super::trusted_open_target;

    #[test]
    fn allows_all_platform_hardcoded_targets() {
        for t in [
            "https://ys.mihoyo.com/",
            "https://genshin.hoyoverse.com/",
            "https://sr.mihoyo.com/",
            "https://hsr.hoyoverse.com/",
            "https://zzz.mihoyo.com/",
            "https://zzz.hoyoverse.com/",
            "https://mc.kurogames.com/",
            "https://wutheringwaves.kurogames.com/",
            "https://pgr.kurogames.com/",
            "https://store.steampowered.com/app/105600/",
            "https://playvalorant.com/",
            "steam://open/main",
            "steam://run/105600",
        ] {
            assert!(trusted_open_target(t).is_ok(), "应放行 {}", t);
        }
    }

    #[test]
    fn rejects_dangerous_schemes_and_hosts() {
        for t in [
            "http://ys.mihoyo.com/",
            "file:///C:/Windows/System32/calc.exe",
            "ms-msdt:/id PCWDiagnostic",
            "javascript:alert(1)",
            "https://evil.com/",
            "https://mihoyo.com.evil.com/",
            "steamx://evil",
        ] {
            assert!(trusted_open_target(t).is_err(), "应拒绝 {}", t);
        }
    }

    #[test]
    fn rejects_empty_and_control_chars() {
        assert!(trusted_open_target("").is_err());
        assert!(trusted_open_target("   ").is_err());
        assert!(trusted_open_target("https://ys.mihoyo.com/\r\ncalc").is_err());
    }

    #[test]
    fn tauri_config_has_csp_and_no_opener_capability() {
        let base = env!("CARGO_MANIFEST_DIR");
        let raw = std::fs::read_to_string(format!("{}/tauri.conf.json", base)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let csp = v["app"]["security"]["csp"].as_str().expect("csp 不能为 null");
        assert!(csp.contains("default-src 'self'"), "csp: {}", csp);
        assert!(csp.contains("object-src 'none'"), "csp: {}", csp);
        assert!(csp.contains("connect-src 'self' ipc: http://ipc.localhost"), "csp: {}", csp);
        assert!(v["app"]["security"]["devCsp"].is_string(), "devCsp 应单独配置");

        let cap = std::fs::read_to_string(format!("{}/capabilities/default.json", base)).unwrap();
        assert!(!cap.contains("opener"), "opener 权限应已移除: {}", cap);
        assert!(cap.contains("dialog:default"), "dialog 权限应保留: {}", cap);
    }
}




