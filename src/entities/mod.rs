//! Rendered demo entities: players + projectiles.

pub mod players;
pub mod projectiles;

use bevy::prelude::*;

pub struct EntitiesPlugin;

impl Plugin for EntitiesPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((players::PlayersPlugin, projectiles::ProjectilesPlugin));
    }
}
