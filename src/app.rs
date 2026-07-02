//! Defines the shared system sets and their ordering; domain plugins register into them.

use bevy::prelude::*;

/// Per-frame phases, chained `Input -> Playback -> Sync -> Draw`.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum AppSet {
    Input,
    /// Move the playhead / recompile the campath before anything reads them.
    Playback,
    /// Push demo + campath state onto entities and the camera.
    Sync,
    /// Gizmos, viewport, fps. Read-only.
    Draw,
}

pub struct AppPlugin;

impl Plugin for AppPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            Update,
            (AppSet::Input, AppSet::Playback, AppSet::Sync, AppSet::Draw).chain(),
        )
        .add_systems(Update, global_input.in_set(AppSet::Input));
    }
}

fn global_input(keys: Res<ButtonInput<KeyCode>>, mut exit: EventWriter<AppExit>) {
    if keys.just_pressed(KeyCode::Escape) {
        exit.send(AppExit::Success);
    }
}
