use crate::platform::{GameInfo, GamePlatform, PlatformLevel};

pub struct SteamPlatform;

const STEAM_GAMES: &[(&str, &str, u32)] = &[
    ("terraria", "Terraria", 105600),
    ("ravenfield", "Ravenfield", 636480),
];

impl GamePlatform for SteamPlatform {
    fn id(&self) -> &str { "steam" }
    fn name(&self) -> &str { "Steam (L1)" }
    fn level(&self) -> PlatformLevel { PlatformLevel::DirectLaunch }
    fn game_ids(&self) -> Vec<&'static str> { vec!["terraria", "ravenfield"] }
    fn official_launcher(&self) -> Option<String> { Some("steam://open/main".into()) }
    fn official_website(&self, game_id: &str) -> Option<String> {
        STEAM_GAMES.iter().find(|(id, _, _)| *id == game_id)
            .map(|(_, _, appid)| format!("https://store.steampowered.com/app/{}/", appid))
    }

    fn detect(&self, _app: &tauri::AppHandle) -> Vec<GameInfo> {
        STEAM_GAMES.iter().map(|(id, name, _appid)| GameInfo {
            id: (*id).into(), name: (*name).into(), installed: false,
            path: None, exe: None, local_version: None, platform: "steam".into(),
            platform_level: PlatformLevel::DirectLaunch, launcher_uri: self.official_launcher(),
        }).collect()
    }

    fn launch_direct(&self, game: &GameInfo) -> Result<String, String> {
        let appid = STEAM_GAMES.iter()
            .find(|(id, _, _)| *id == game.id)
            .map(|(_, _, appid)| *appid)
            .ok_or("未知 Steam 游戏")?;
        let uri = format!("steam://run/{}", appid);
        std::process::Command::new("cmd").args(["/C", "start", &uri]).spawn().map_err(|e| e.to_string())?;
        Ok(format!("已唤起 Steam: {}", uri))
    }
}