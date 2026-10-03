use crate::platform::{GamePlatform, GameInfo, RemoteGameInfo, PlatformLevel};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub struct KuroPlatform;

impl GamePlatform for KuroPlatform {
    fn id(&self) -> &str {
        "kuro"
    }

    fn name(&self) -> &str {
        "库洛游戏"
    }

    fn level(&self) -> PlatformLevel {
        PlatformLevel::Full
    }

    fn game_ids(&self) -> Vec<&'static str> {
        vec!["wuthering_waves", "pgr"]
    }

    fn official_launcher(&self) -> Option<String> {
        None // 等待你提供官启路径
    }

    fn detect(&self, _app: &tauri::AppHandle) -> Vec<GameInfo> {
        // 暂时返回空，等待你提供《鸣潮》本机安装路径
        vec![]
    }

    fn launch_direct(&self, _game: &GameInfo) -> Result<String, String> {
        Err("库洛平台暂不支持直接启动".to_string())
    }

    fn remote_info(&self, _app: &tauri::AppHandle, game_id: &str) -> Result<RemoteGameInfo, String> {
        // 占位：等待拿到真实 API 后，按你的硬规矩在这里用 thread+mpsc 发请求并 Dump JSON
        Err(format!("Kuro {} API 尚未接入，等待 Dump", game_id))
    }

    fn download(
        &self,
        _app: &tauri::AppHandle,
        _game_id: &str,
        _dest: &str,
        _use_patch: bool,
        _allow_old: bool,
        _cancel: Arc<AtomicBool>,
    ) -> Result<String, String> {
        Err("库洛平台下载逻辑尚未实现".to_string())
    }
}