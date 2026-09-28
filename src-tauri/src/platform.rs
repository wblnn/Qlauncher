use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum PlatformLevel {
    Aggregate,
    DirectLaunch,
    Download,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameInfo {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub path: Option<String>,
    pub exe: Option<String>,
    pub local_version: Option<String>,
    pub platform: String,
    pub platform_level: PlatformLevel,
    pub launcher_uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePackagePart {
    pub url: String,
    pub md5: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteGameInfo {
    pub game_id: String,
    pub latest_version: String,
    pub package_url: String,
    pub package_size: u64,
    pub parts: Vec<RemotePackagePart>,
    pub patch_from: Option<String>,
    pub patch_size: u64,
    /// 差分包分卷列表（空 = 无差分或不可用）
    pub patch_parts: Vec<RemotePackagePart>,
    // P0 版本守卫字段
    pub local_version: Option<String>,
    /// fresh=本地未安装(可整包) ahead=远程更新(可下载) equal=已是最新 behind=接口整包滞后(禁下载)
    pub version_relation: String,
    /// 解压后的安装体积（整包下载前用它做空间预检）
    pub install_size: u64,
    /// 官方散列文件基址：单文件地址 = res_list_url + "/" + 清单里的 remoteName（校验修复用）
    pub res_list_url: String,
    // ===== 版本源自适应：多源版本 + 来源可解释 =====
    /// 官方公告/预下载里推断出的"当前真实版本"（整包停更时用；UI 标注"推断"）
    pub announced_version: Option<String>,
    /// 远程版本来源：整包 / 预下载 / 公告(推断)
    pub version_source: String,
    /// 本次检查时间（unix 秒）
    pub checked_at: u64,
    /// 实际解析到的渠道与 hyp game_id（渠道改名时能自动跟上）
    pub resolved_biz: String,
    pub resolved_game_id: String,
    /// 本次实际使用的 launcher_id 及来源
    pub launcher_id: String,
    pub launcher_id_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub game_id: String,
    pub downloaded: u64,
    pub total: u64,
    pub speed: u64,             // 字节/秒 (B/s)
    pub eta_seconds: Option<u64>, // 预计剩余秒数 (None=计算中, Some(0)=已完成)
    pub status: String,
}

/// 语义化版本比较："7.0.0" vs "5.5.0"
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let parse = |s: &str| -> Vec<u64> {
        s.split('.').map(|x| x.trim().parse::<u64>().unwrap_or(0)).collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    let len = pa.len().max(pb.len());
    for i in 0..len {
        let va = pa.get(i).copied().unwrap_or(0);
        let vb = pb.get(i).copied().unwrap_or(0);
        if va != vb { return va.cmp(&vb); }
    }
    Ordering::Equal
}

pub trait GamePlatform: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn level(&self) -> PlatformLevel;
    fn game_ids(&self) -> Vec<&'static str>;
    fn official_launcher(&self) -> Option<String>;
    /// 纯官网网页地址（前端不硬编，由平台下发）
    fn official_website(&self, _game_id: &str) -> Option<String> { None }
    fn detect(&self, app: &tauri::AppHandle) -> Vec<GameInfo>;
    fn launch_direct(&self, game: &GameInfo) -> Result<String, String>;
    fn remote_info(&self, _app: &tauri::AppHandle, _game_id: &str) -> Result<RemoteGameInfo, String> {
        Err("该平台不支持在线获取信息".into())
    }
    fn download(&self, _app: &tauri::AppHandle, _game_id: &str, _dest: &str, _use_patch: bool, _allow_old: bool, _cancel: Arc<AtomicBool>) -> Result<String, String> {
        Err("该平台不支持在线下载".into())
    }
    // P3：平台自己声明某游戏的 exe 候选名，bind_game 不再硬编码
    fn exe_candidates(&self, _game_id: &str) -> Vec<&'static str> { vec![] }
}