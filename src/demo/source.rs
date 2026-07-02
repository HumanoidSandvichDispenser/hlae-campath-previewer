//! Per-tick access to a parsed demo.

use bevy::prelude::*;
use std::collections::HashMap;

use tf_demo_parser::demo::data::DemoTick;
use tf_demo_parser::demo::message::{Message, MessageType};
use tf_demo_parser::demo::parser::gamestateanalyser::{GameStateAnalyser, PlayerState};
use tf_demo_parser::demo::parser::{MessageHandler, ParserState};
use tf_demo_parser::{Demo, DemoParser};

use super::data::{Frame, PlayerSnap, ProjectileSnap};

/// Per-tick access to demo state. Methods take `&self` so systems reading the demo can
/// run in parallel; implementations that need to mutate (e.g. a lazy decode cache) do
/// so behind interior mutability.
pub trait DemoSource: Send + Sync + 'static {
    /// Nearest frame at or before `tick`.
    fn frame_at(&self, tick: u32) -> Option<&Frame>;
    /// Display name for a player entity, if the demo carried one.
    fn player_name(&self, entity: u32) -> Option<&str>;
    /// Demo tick -> server tick. HLAE's campath time axis is engine `curtime` (server sim
    /// time), which leads demo ticks by the server's uptime and can jump at a pause, so
    /// this is a piecewise map, not a constant offset.
    fn demo_to_server_tick(&self, demo_tick: i64) -> i64;
    /// Server tick -> demo tick, the inverse of `demo_to_server_tick`.
    fn server_to_demo_tick(&self, server_tick: i64) -> i64;
    /// First frame with a tick strictly greater than `tick`, for interpolating between
    /// snapshots. Pairs with `frame_at`.
    fn frame_after(&self, tick: u32) -> Option<&Frame>;
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

/// Piecewise demo-tick <-> server-tick map. Within a segment both advance 1:1, so each
/// anchor `(demo, server)` holds until the next; a pause inserts a new anchor. Both
/// columns are monotonic, so each direction is a binary search + linear step.
#[derive(Default)]
pub struct TickMap {
    anchors: Vec<(i64, i64)>,
}

impl TickMap {
    fn demo_to_server(&self, demo: i64) -> i64 {
        if self.anchors.is_empty() {
            return demo;
        }
        let i = self.anchors.partition_point(|&(d, _)| d <= demo).max(1) - 1;
        let (d, s) = self.anchors[i];
        s + (demo - d)
    }

    fn server_to_demo(&self, server: i64) -> i64 {
        if self.anchors.is_empty() {
            return server;
        }
        let i = self.anchors.partition_point(|&(_, s)| s <= server).max(1) - 1;
        let (d, s) = self.anchors[i];
        d + (server - s)
    }
}

/// The whole demo in memory, one `Frame` per tick. `frame_at` is a binary search.
pub struct DenseDemoSource {
    frames: Vec<Frame>,
    interval_per_tick: f32,
    max_tick: u32,
    map: String,
    /// Player entity id -> display name. Names are per-demo, not per-tick, so they live
    /// here rather than in every `PlayerSnap`.
    names: HashMap<u32, String>,
    /// Demo-tick <-> server-tick map for HLAE curtime conversion.
    tick_map: TickMap,
}

impl DenseDemoSource {
    /// Map name from the demo header, e.g. `ultiduo_swine_b06`. Empty if the header had none.
    pub fn map_name(&self) -> &str {
        &self.map
    }
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

    fn frame_after(&self, tick: u32) -> Option<&Frame> {
        let idx = self.frames.partition_point(|f| f.tick <= tick);
        self.frames.get(idx)
    }

    fn player_name(&self, entity: u32) -> Option<&str> {
        self.names.get(&entity).map(String::as_str)
    }

    fn demo_to_server_tick(&self, demo_tick: i64) -> i64 {
        self.tick_map.demo_to_server(demo_tick)
    }

    fn server_to_demo_tick(&self, server_tick: i64) -> i64 {
        self.tick_map.server_to_demo(server_tick)
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

/// Collects every `net_Tick`'s `(demo_tick, server_tick)`. Handles only `NetTick`, so it
/// skips entity decode and is much cheaper than the main game-state pass.
#[derive(Default)]
struct TickPairProbe {
    pairs: Vec<(i64, i64)>,
}

impl MessageHandler for TickPairProbe {
    type Output = Vec<(i64, i64)>;

    fn does_handle(message_type: MessageType) -> bool {
        message_type == MessageType::NetTick
    }

    fn handle_message(&mut self, message: &Message, tick: DemoTick, _state: &ParserState) {
        if let Message::NetTick(nt) = message {
            self.pairs
                .push((u32::from(tick) as i64, u32::from(nt.tick) as i64));
        }
    }

    fn into_output(self, _state: &ParserState) -> Self::Output {
        self.pairs
    }
}

/// Build the piecewise map from ordered `(demo, server)` samples. Drops singleton offsets
/// (the first net tick is a signon artifact with a bogus demo tick) and keeps one anchor
/// per offset change, staying demo-monotonic.
fn build_tick_map(pairs: &[(i64, i64)]) -> TickMap {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for &(d, s) in pairs {
        *counts.entry(s - d).or_default() += 1;
    }

    let mut anchors: Vec<(i64, i64)> = Vec::new();
    let mut cur_offset: Option<i64> = None;
    let mut last_demo = i64::MIN;
    for &(d, s) in pairs {
        let off = s - d;
        // A one-off offset is an artifact, not a real segment.
        if counts.get(&off).copied().unwrap_or(0) < 2 {
            continue;
        }
        if d <= last_demo {
            continue; // keep both columns monotonic
        }
        last_demo = d;
        if cur_offset != Some(off) {
            anchors.push((d, s));
            cur_offset = Some(off);
        }
    }
    TickMap { anchors }
}

/// Full `net_Tick`-only pass to learn the demo/server tick mapping.
fn parse_tick_map(bytes: &[u8]) -> TickMap {
    let demo = Demo::new(bytes);
    let Ok((_header, ticker)) =
        DemoParser::new_with_analyser(demo.get_stream(), TickPairProbe::default()).ticker()
    else {
        return TickMap::default();
    };
    let mut ticker = ticker;
    while matches!(ticker.tick(), Ok(true)) {}
    build_tick_map(&ticker.into_state())
}

/// One-pass parse into a `DenseDemoSource`. GameStateAnalyser only walks forward
/// (entities are delta-compressed), so: one sequential pass, snapshot every tick.
pub fn parse(bytes: &[u8]) -> anyhow::Result<DenseDemoSource> {
    let tick_map = parse_tick_map(bytes);
    let demo = Demo::new(bytes);
    let (header, mut ticker) =
        DemoParser::new_with_analyser(demo.get_stream(), GameStateAnalyser::default()).ticker()?;
    let map = header.map.clone();

    let mut frames: Vec<Frame> = Vec::new();
    let mut interval_per_tick = 0.0_f32;
    let mut last_tick = u32::MAX;
    // entity -> last tick it was alive, for computing death age.
    let mut last_alive: HashMap<u32, u32> = HashMap::new();
    // entity -> display name, filled the first time a player's user info shows up.
    let mut names: HashMap<u32, String> = HashMap::new();

    loop {
        let more = ticker.tick()?;
        let state = ticker.state();
        interval_per_tick = state.interval_per_tick;

        let tick: u32 = u32::from(state.tick);
        if tick != last_tick {
            for p in &state.players {
                let id = u32::from(p.entity);
                if !names.contains_key(&id) {
                    if let Some(info) = &p.info {
                        if !info.name.is_empty() {
                            names.insert(id, info.name.clone());
                        }
                    }
                }
            }
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
        map,
        names,
        tick_map,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_map_handles_pause_and_artifact() {
        // Shape mirrors demo 1319572: a leading signon artifact (offset 67, one sample),
        // a pre-pause run at offset 104253, then a post-pause run at offset 86551.
        let pairs = vec![
            (104_186, 104_253), // artifact: offset 67, single -> dropped
            (1, 104_254),
            (2, 104_255),
            (3, 104_256), // offset 104253
            (138_836, 225_387),
            (138_837, 225_388), // offset 86551 (after the ~30 min pause)
        ];
        let m = build_tick_map(&pairs);

        // Post-pause keyframe: t=3552 s -> server 236800 <-> demo 150249 (~37.6 min).
        assert_eq!(m.server_to_demo(236_800), 150_249);
        assert_eq!(m.demo_to_server(150_249), 236_800);

        // Pre-pause keyframe uses the other offset.
        assert_eq!(m.demo_to_server(60_000), 164_253);
        assert_eq!(m.server_to_demo(164_253), 60_000);

        // The one-off artifact offset never becomes an anchor.
        assert_eq!(m.anchors.first(), Some(&(1, 104_254)));
    }
}
