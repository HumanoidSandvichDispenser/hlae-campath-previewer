//! One-pass demo parse into a dense per-tick cache.
//!
//! GameStateAnalyser is forward-only and delta-based: it mutates a single running
//! GameState as it walks packets, with no random access and no memoization. So we do
//! the one sequential pass it forces and snapshot the roster every tick into a Vec.
//! After this, any tick is O(1) — which is what seeking + campath sampling needs.

use tf_demo_parser::demo::parser::gamestateanalyser::{GameStateAnalyser, PlayerState};
use tf_demo_parser::{Demo, DemoParser};

/// A single player's transform at one tick. Positions are raw TF2 world units
/// (Hammer coords, Z-up); the renderer applies the world->engine transform.
#[derive(Clone, Copy, Debug)]
pub struct PlayerSnap {
    pub entity: u32,
    pub pos: [f32; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub team: u8,  // 2 = red, 3 = blue
    pub class: u8, // 1..=9, 0 = unknown
    pub health: u16,
    /// Collision AABB corners, relative to the player origin (feet), raw Hammer units.
    /// From m_vecMaxsPreScaled etc. `max.z` shrinks ~82->62 when ducking.
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub alive: bool,
    /// Ticks since this player last died (0 while alive). Drives the death fade,
    /// tick-based so it's stable under seeking. u32::MAX = never seen alive.
    pub death_age: u32,
}

/// A projectile's transform at one tick. Positions are raw Hammer units (Z-up).
#[derive(Clone, Copy, Debug)]
pub struct ProjectileSnap {
    pub entity: u32,
    pub pos: [f32; 3],
    /// Source euler angles in degrees: [pitch, yaw, roll].
    pub rotation: [f32; 3],
    pub team: u8, // 2 = red, 3 = blue
    /// ProjectileType as u8: 0 rocket, 1 healing arrow, 2 sticky, 3 pipe, 4 flare,
    /// 5 loose cannon, 7 unknown.
    pub ty: u8,
    pub critical: bool,
}

pub struct Frame {
    pub tick: u32,
    pub players: Vec<PlayerSnap>,
    pub projectiles: Vec<ProjectileSnap>,
}

pub struct DemoData {
    pub frames: Vec<Frame>,
    pub interval_per_tick: f32,
    pub max_tick: u32,
}

impl DemoData {
    /// Nearest frame at or before `tick` (frames are tick-sorted). Cheap binary search.
    pub fn frame_at(&self, tick: u32) -> Option<&Frame> {
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
}

pub fn parse(bytes: &[u8]) -> anyhow::Result<DemoData> {
    let demo = Demo::new(bytes);
    let (_header, mut ticker) =
        DemoParser::new_with_analyser(demo.get_stream(), GameStateAnalyser::default()).ticker()?;

    let mut frames: Vec<Frame> = Vec::new();
    let mut interval_per_tick = 0.0_f32;
    let mut last_tick = u32::MAX;
    // entity -> last tick it was alive, for computing death age.
    let mut last_alive: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();

    loop {
        let more = ticker.tick()?;
        let state = ticker.state();
        interval_per_tick = state.interval_per_tick;

        let tick: u32 = u32::from(state.tick);
        if tick != last_tick {
            let players = state
                .players
                .iter()
                .filter(|p| matches!(p.team as u8, 2 | 3)) // red/blue only
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

    Ok(DemoData {
        frames,
        interval_per_tick,
        max_tick,
    })
}
