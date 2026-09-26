pub mod mihoyo;
pub mod steam;
pub mod riot;

use crate::platform::GamePlatform;
use std::sync::OnceLock;

static PLATFORMS: OnceLock<Vec<Box<dyn GamePlatform>>> = OnceLock::new();

pub fn get_platforms() -> &'static Vec<Box<dyn GamePlatform>> {
    PLATFORMS.get_or_init(|| {
        vec![
            Box::new(mihoyo::MihoyoPlatform),
            Box::new(steam::SteamPlatform),
            Box::new(riot::RiotPlatform),
        ]
    })
}

pub fn platform_for_game(game_id: &str) -> Option<&'static dyn GamePlatform> {
    get_platforms().iter().map(|p| p.as_ref()).find(|p| p.game_ids().contains(&game_id))
}