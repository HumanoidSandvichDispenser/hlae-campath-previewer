//! Per-tick access to a parsed demo.

use bevy::prelude::*;
use std::collections::HashMap;

use tf_demo_parser::demo::parser::gamestateanalyser::{GameStateAnalyser, PlayerState};
use tf_demo_parser::{Demo, DemoParser};

use super::data::{Frame, PlayerSnap, ProjectileSnap};

/// Per-tick access to demo state. Methods take `&self` so systems reading the demo can
/// run in parallel; implementations that need to mutate (e.g. a lazy decode cache) do
/// so behind interior mutability.
pub trait DemoSource: Send + Sync + 'static {
    /// Nearest frame at or before `tick`.
    fn frame_at(&self, tick: u32) -> Option<&Frame>;
    fn max_tick(&self) -> u32;
    /// Seconds per tick (~0.015 for TF2).
    fn interval_per_tick(&self) -> f32;
    /// First frame with players in it.
    fn first_populated_frame(&self) -> Option<&Frame>;
    /// Approximate memory held by cached frames, in bytes.
    fn approx_bytes(&self) -> usize;
    /// Visit frames with tick in `[lo, hi]`, in order.
    fn frames_in_range(&self, lo: u32, hi: u32, f: &mut dyn FnMut(&Frame));
}

#[derive(Resource)]
pub struct ActiveDemo(pub Box<dyn DemoSource>);

/// The whole demo in memory, one `Frame` per tick. `frame_at` is a binary search.
pub struct DenseDemoSource {
    frames: Vec<Frame>,
    interval_per_tick: f32,
    max_tick: u32,
}

impl DemoSource for DenseDemoSource {
    fn frame_at(&self, tick: u32) -> Option<&Frame> {
        if self.frames.is_empty() {
            return None;
        }
        let idx = match self.frames.binary_search_by_key(&tick, |f| f.tick) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        };
        self.frames.get(idx)
    }

    fn max_tick(&self) -> u32 {
        self.max_tick
    }

    fn interval_per_tick(&self) -> f32 {
        self.interval_per_tick
    }

    fn first_populated_frame(&self) -> Option<&Frame> {
        self.frames.iter().find(|f| !f.players.is_empty())
    }

    fn approx_bytes(&self) -> usize {
        self.frames
            .iter()
            .map(|f| {
                f.players.len() * std::mem::size_of::<PlayerSnap>()
                    + f.projectiles.len() * std::mem::size_of::<ProjectileSnap>()
            })
            .sum()
    }

    fn frames_in_range(&self, lo: u32, hi: u32, f: &mut dyn FnMut(&Frame)) {
        for frame in self.frames.iter().filter(|fr| fr.tick >= lo && fr.tick <= hi) {
            f(frame);
        }
    }
}

/// One-pass parse into a `DenseDemoSource`. GameStateAnalyser only walks forward
/// (entities are delta-compressed), so: one sequential pass, snapshot every tick.
pub fn parse(bytes: &[u8]) -> anyhow::Result<DenseDemoSource> {
    let demo = Demo::new(bytes);
    let (_header, mut ticker) =
        DemoParser::new_with_analyser(demo.get_stream(), GameStateAnalyser::default()).ticker()?;

    let mut frames: Vec<Frame> = Vec::new();
    let mut interval_per_tick = 0.0_f32;
    let mut last_tick = u32::MAX;
    // entity -> last tick it was alive, for computing death age.
    let mut last_alive: HashMap<u32, u32> = HashMap::new();

    loop {
        let more = ticker.tick()?;
        let state = ticker.state();
        interval_per_tick = state.interval_per_tick;

        let tick: u32 = u32::from(state.tick);
        if tick != last_tick {
            let players = state
                .players
                .iter()
                .filter(|p| matches!(p.team as u8, 2 | 3))
                .map(|p| {
                    let entity = u32::from(p.entity);
                    // m_lifeState is the authoritative death signal; health goes stale
                    // when a dead player's entity stops updating.
                    let alive = p.state == PlayerState::Alive;
                    let death_age = if alive {
                        last_alive.insert(entity, tick);
                        0
                    } else {
                        last_alive.get(&entity).map(|&a| tick - a).unwrap_or(u32::MAX)
                    };
                    PlayerSnap {
                        entity,
                        pos: [p.position.x, p.position.y, p.position.z],
                        yaw: p.view_angle,
                        pitch: p.pitch_angle,
                        team: p.team as u8,
                        class: p.class as u8,
                        health: p.health,
                        bounds_min: [p.bounds.min.x, p.bounds.min.y, p.bounds.min.z],
                        bounds_max: [p.bounds.max.x, p.bounds.max.y, p.bounds.max.z],
                        alive,
                        death_age,
                    }
                })
                .collect();
            let projectiles = state
                .projectiles
                .iter()
                .map(|(_, pr)| ProjectileSnap {
                    entity: u32::from(pr.id),
                    pos: [pr.position.x, pr.position.y, pr.position.z],
                    rotation: [pr.rotation.x, pr.rotation.y, pr.rotation.z],
                    team: pr.team as u8,
                    ty: pr.ty as u8,
                    critical: pr.critical,
                })
                .collect();
            frames.push(Frame {
                tick,
                players,
                projectiles,
            });
            last_tick = tick;
        }

        if !more {
            break;
        }
    }

    let max_tick = frames.last().map(|f| f.tick).unwrap_or(0);
    if interval_per_tick <= 0.0 {
        interval_per_tick = 0.015; // TF2 nominal fallback
    }

    Ok(DenseDemoSource {
        frames,
        interval_per_tick,
        max_tick,
    })
}
