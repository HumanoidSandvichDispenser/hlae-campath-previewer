//! Native previewer: parse a TF2 demo, render the map GLB + players/projectiles at a
//! tick, free-fly camera, scrub the timeline, author + export an HLAE campath.
//!
//! Usage: hlae-campath-previewer <demo.dem|-> [map.glb]
//!   `-` reads the demo from stdin. Assets load relative to the `assets/` dir.
//!
//! Controls: RMB hold = mouselook, WASD/RF fly, Space = play/pause, arrows seek 50,
//!           ,/. step 1 tick, +/- fly speed, B = toggle AABBs, Esc = quit
//! Campath:  V = add keyframe here, L = delete last, P = follow path, F5 = export

mod app;
mod camera;
mod campath;
mod coords;
mod demo;
mod diagnostics;
mod entities;
mod map;
mod ui;

use bevy::prelude::*;

use app::AppPlugin;
use camera::CameraPlugin;
use campath::CampathPlugin;
use demo::{ActiveDemo, DemoPath, DemoPlugin};
use diagnostics::DiagnosticsPlugin;
use entities::EntitiesPlugin;
use map::{MapAssetPath, MapPlugin};
use ui::UiPlugin;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let demo_path = args.next().unwrap_or_else(|| "assets/demo.dem".into());
    let map_asset = args.next().unwrap_or_else(|| "map_snakewater.glb".into());

    // "-" reads the demo from stdin; give exports a sensible stem in that case since
    // there's no filename to derive one from.
    let export_stem = if demo_path == "-" { "campath".to_string() } else { demo_path.clone() };
    let src_label = if demo_path == "-" { "<stdin>" } else { &demo_path };

    eprintln!("[previewer] parsing {src_label} ...");
    let bytes = demo::load_demo_bytes(&demo_path)?;
    let data = demo::parse(&bytes)?;
    eprintln!(
        "[previewer] parsed: {:.4}s/tick, max_tick={}",
        demo::DemoSource::interval_per_tick(&data),
        demo::DemoSource::max_tick(&data)
    );

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "hlae-campath-previewer".into(),
                ..default()
            }),
            ..default()
        }))
        .insert_resource(ClearColor(Color::srgb(0.05, 0.06, 0.08)))
        // Untextured map surfaces facing away from the sun would be near-black; lift them.
        .insert_resource(AmbientLight {
            color: Color::WHITE,
            brightness: 600.0,
        })
        .insert_resource(DemoPath(export_stem))
        .insert_resource(MapAssetPath(map_asset))
        .insert_resource(ActiveDemo(Box::new(data)))
        .add_plugins((
            MapPlugin,
            EntitiesPlugin,
            CameraPlugin,
            CampathPlugin,
            DemoPlugin,
            DiagnosticsPlugin,
            UiPlugin,
            AppPlugin,
        ))
        .run();

    Ok(())
}
