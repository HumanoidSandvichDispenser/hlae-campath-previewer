//! Per-tick snapshot types, pure data (no Bevy). Positions are raw Hammer coords
//! (Z-up); the renderer converts (see `crate::coords`).

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

/// Everything the renderer needs at one demo tick.
pub struct Frame {
    pub tick: u32,
    pub players: Vec<PlayerSnap>,
    pub projectiles: Vec<ProjectileSnap>,
}

#[cfg(test)]
mod tests {
    /// The parser's ticker can't be cloned mid-stream, which is why we cache frames
    /// instead of checkpointing parser state. This breaks if that ever changes upstream.
    #[test]
    fn test_ticker_clone() {
        use tf_demo_parser::demo::parser::gamestateanalyser::GameStateAnalyser;
        use tf_demo_parser::{Demo, DemoParser};

        let bytes = vec![0u8; 100];
        let demo = Demo::new(&bytes);
        let _ = DemoParser::new_with_analyser(demo.get_stream(), GameStateAnalyser::default())
            .ticker();
    }
}
