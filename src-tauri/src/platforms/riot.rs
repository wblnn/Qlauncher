use crate::platform::{GameInfo, GamePlatform, PlatformLevel};

pub struct RiotPlatform;

impl GamePlatform for RiotPlatform {
    fn id(&self) -> &str { "riot" }
    fn name(&self) -> &str { "Riot (L0)" }
    fn level(&self) -> PlatformLevel { PlatformLevel::Aggregate }
    fn game_ids(&self) -> Vec<&'static str> { vec!["valorant"] }
    fn official_launcher(&self) -> Option<String> { Some("https://playvalorant.com/".into()) }
    fn official_website(&self, _game_id: &str) -> Option<String> {
        Some("https://playvalorant.com/".into())
    }

    fn detect(&self, _app: &tauri::AppHandle) -> Vec<GameInfo> {
        vec![GameInfo {
            id: "valorant".into(), name: "Valorant (无畏契约)".into(), installed: false,
            path: None, exe: None, local_version: None, platform: "riot".into(),
            platform_level: PlatformLevel::Aggregate, launcher_uri: self.official_launcher(),
        }]
    }

    fn launch_direct(&self, _game: &GameInfo) -> Result<String, String> {
        Err("L0 平台不支持直启，请使用官方启动器".into())
    }
}