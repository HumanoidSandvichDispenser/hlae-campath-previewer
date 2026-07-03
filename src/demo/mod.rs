//! Playhead + seeking. Parsing/storage lives in the submodules.

pub mod data;
pub mod load;
pub mod source;

use bevy::prelude::*;

use crate::app::AppSet;

pub use load::load_demo_bytes;
pub use source::{parse, ActiveDemo, DemoSource};

/// How far `tick` sits between the `before` and `after` snapshots, in `[0, 1]`. Returns
/// 0 when there's nothing to interpolate toward (no `after`, or same tick).
pub(crate) fn interp_fraction(
    tick: f32,
    before: Option<&data::Frame>,
    after: Option<&data::Frame>,
) -> f32 {
    match (before, after) {
        (Some(b), Some(a)) if a.tick > b.tick => {
            ((tick - b.tick as f32) / (a.tick - b.tick) as f32).clamp(0.0, 1.0)
        }
        _ => 0.0,
    }
}

/// The playhead tick to sample entities at, pushed `delay_ms` behind `tick` to emulate
/// TF2's entity-interpolation lag. `interval_s` is the demo's seconds-per-tick. Clamped
/// to the first tick so early frames don't underflow. The camera never uses this.
pub(crate) fn delayed_tick(tick: f32, delay_ms: f32, interval_s: f32) -> f32 {
    let per_tick_ms = interval_s * 1000.0;
    let dt = if per_tick_ms > 0.0 {
        delay_ms / per_tick_ms
    } else {
        0.0
    };
    (tick - dt).max(1.0)
}

/// Shortest-arc interpolation between two angles in degrees.
pub(crate) fn lerp_angle_deg(a: f32, b: f32, t: f32) -> f32 {
    let mut d = (b - a) % 360.0;
    if d > 180.0 {
        d -= 360.0;
    } else if d < -180.0 {
        d += 360.0;
    }
    a + d * t
}

/// The playhead. `tick` is fractional so playback can accumulate sub-tick.
#[derive(Resource)]
pub(crate) struct Playback {
    pub(crate) tick: f32,
    pub(crate) playing: bool,
    /// Playback rate multiplier. 1.0 = realtime, <1 slow-mo, >1 fast-forward.
    pub(crate) speed: f32,
    /// Lerp entity positions between snapshots so slow-mo doesn't step tick-by-tick.
    pub(crate) interpolate: bool,
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
            speed: 1.0,
            interpolate: true,
        })
        .add_event::<SeekTo>()
        .add_systems(Update, playback_input.in_set(AppSet::Input))
        // Apply seeks before advancing so a seek lands on its exact target tick.
        .add_systems(
            Update,
            (handle_seek, advance_playback)
                .chain()
                .in_set(AppSet::Playback),
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

fn handle_seek(mut ev: EventReader<SeekTo>, mut pb: ResMut<Playback>, demo_res: Res<ActiveDemo>) {
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
    let ticks = dt / demo_res.0.interval_per_tick().max(0.001) * pb.speed;
    pb.tick = (pb.tick + ticks).clamp(1.0, demo_res.0.max_tick() as f32);
}
