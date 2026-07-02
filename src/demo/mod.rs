//! Playhead + seeking. Parsing/storage lives in the submodules.

pub mod data;
pub mod load;
pub mod source;

use bevy::prelude::*;

use crate::app::AppSet;

pub use load::load_demo_bytes;
pub use source::{parse, ActiveDemo, DemoSource};

/// The playhead. `tick` is fractional so playback can accumulate sub-tick.
#[derive(Resource)]
pub(crate) struct Playback {
    pub(crate) tick: f32,
    pub(crate) playing: bool,
}

/// Demo path, only used to name exported campath files.
#[derive(Resource)]
pub(crate) struct DemoPath(pub(crate) String);

/// Seek the playhead to an absolute tick. `handle_seek` clamps.
#[derive(Event)]
pub(crate) struct SeekTo(pub(crate) f32);

pub struct DemoPlugin;

impl Plugin for DemoPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Playback {
            tick: 1.0,
            playing: false,
        })
        .add_event::<SeekTo>()
        .add_systems(Update, playback_input.in_set(AppSet::Input))
        // Apply seeks before advancing so a seek lands on its exact target tick.
        .add_systems(
            Update,
            (handle_seek, advance_playback).chain().in_set(AppSet::Playback),
        );
    }
}

fn playback_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut pb: ResMut<Playback>,
    mut seek: EventWriter<SeekTo>,
) {
    if keys.just_pressed(KeyCode::Space) {
        pb.playing = !pb.playing;
    }
    if keys.just_pressed(KeyCode::ArrowRight) {
        seek.send(SeekTo(pb.tick + 50.0));
    }
    if keys.just_pressed(KeyCode::ArrowLeft) {
        seek.send(SeekTo(pb.tick - 50.0));
    }
    if keys.just_pressed(KeyCode::Period) {
        seek.send(SeekTo(pb.tick + 1.0));
    }
    if keys.just_pressed(KeyCode::Comma) {
        seek.send(SeekTo(pb.tick - 1.0));
    }
}

fn handle_seek(
    mut ev: EventReader<SeekTo>,
    mut pb: ResMut<Playback>,
    demo_res: Res<ActiveDemo>,
) {
    let max = demo_res.0.max_tick() as f32;
    for e in ev.read() {
        pb.tick = e.0.clamp(1.0, max);
    }
}

fn advance_playback(time: Res<Time>, mut pb: ResMut<Playback>, demo_res: Res<ActiveDemo>) {
    if !pb.playing {
        return;
    }
    let dt = time.delta_seconds();
    let ticks = dt / demo_res.0.interval_per_tick().max(0.001);
    pb.tick = (pb.tick + ticks).min(demo_res.0.max_tick() as f32);
}
