pub mod mihoyo;
pub mod steam;
pub mod riot;
pub mod kuro; // 必须是 pub mod，或者至少是 mod

use crate::platform::GamePlatform;
use std::sync::OnceLock;

static PLATFORMS: OnceLock<Vec<Box<dyn GamePlatform>>> = OnceLock::new();

pub fn get_platforms() -> &'static Vec<Box<dyn GamePlatform>> {
    PLATFORMS.get_or_init(|| {
        vec![
            Box::new(mihoyo::MihoyoPlatform),
            Box::new(steam::SteamPlatform),
            Box::new(riot::RiotPlatform),
            Box::new(kuro::KuroPlatform),
        ]
    })
}

pub fn platform_for_game(game_id: &str) -> Option<&'static dyn GamePlatform> {
    get_platforms().iter().map(|p| p.as_ref()).find(|p| p.game_ids().contains(&game_id))
}