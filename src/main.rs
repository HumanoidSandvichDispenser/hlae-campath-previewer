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
use clap::Parser;

use app::AppPlugin;
use camera::CameraPlugin;
use campath::CampathPlugin;
use demo::{ActiveDemo, DemoPath, DemoPlugin};
use diagnostics::DiagnosticsPlugin;
use entities::EntitiesPlugin;
use map::{MapAssetPath, MapPlugin};
use ui::UiPlugin;

/// Native TF2 demo previewer + HLAE campath author.
#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    /// Demo to load; `-` reads it from stdin.
    #[arg(default_value = "assets/demo.dem")]
    demo: String,
    /// Map GLB to render; auto-resolved from the demo header if omitted.
    map: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let demo_path = args.demo;
    let map_arg = args.map;

    // "-" reads the demo from stdin; give exports a sensible stem in that case since
    // there's no filename to derive one from.
    let export_stem = if demo_path == "-" { "campath".to_string() } else { demo_path.clone() };
    let src_label = if demo_path == "-" { "<stdin>" } else { &demo_path };

    eprintln!("[previewer] parsing {src_label} ...");
    let bytes = demo::load_demo_bytes(&demo_path)?;
    let data = demo::parse(&bytes)?;
    eprintln!(
        "[previewer] parsed: {:.4}s/tick, max_tick={}, map={}, first_curtime_tick={}",
        demo::DemoSource::interval_per_tick(&data),
        demo::DemoSource::max_tick(&data),
        data.map_name(),
        demo::DemoSource::demo_to_server_tick(&data, 1),
    );

    // Prefer an explicit CLI map; otherwise resolve one from the demo header against the
    // GLBs in assets/, tolerating version suffixes. Fall back if nothing matches.
    let map_asset = map_arg
        .or_else(|| resolve_map_asset(data.map_name()))
        .unwrap_or_else(|| {
            eprintln!(
                "[previewer] no map asset for '{}', rendering without geometry",
                data.map_name()
            );
            "map_snakewater.glb".into()
        });
    eprintln!("[previewer] map asset: {map_asset}");

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

/// Find the map GLB in `assets/` for the demo header's `map` name, tolerating version
/// suffixes. An exact `<map>.glb` wins; otherwise the asset sharing the longest common
/// prefix, but only if that prefix covers the base map name (everything before the
/// trailing version token). So `koth_bagel_rc11` still finds `koth_bagel_rc12`, while
/// `cp_process_f12` never grabs `cp_prolands_rc2ta`. Returns a name relative to assets/.
fn resolve_map_asset(map: &str) -> Option<String> {
    if map.is_empty() {
        return None;
    }
    let exact = format!("{map}.glb");

    let names: Vec<String> = std::fs::read_dir("assets")
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".glb"))
        .collect();

    if names.iter().any(|n| *n == exact) {
        return Some(exact);
    }
    best_prefix_match(map, &names)
}

/// The `<name>.glb` sharing the longest common prefix with `map`, so long as the prefix
/// covers `map`'s base name (its part before any trailing version token). Unrelated maps
/// sharing only a game-mode prefix (cp_, koth_) are rejected.
fn best_prefix_match(map: &str, names: &[String]) -> Option<String> {
    let need = strip_version(map).len();
    names
        .iter()
        .filter_map(|n| {
            let stem = n.trim_end_matches(".glb");
            let lcp = common_prefix_len(map, stem);
            (lcp >= need).then_some((lcp, n.clone()))
        })
        .max_by_key(|(lcp, _)| *lcp)
        .map(|(_, n)| n)
}

/// Drop a trailing `_<token>` when the token carries a digit (a version like `rc12`,
/// `f9`, `b06`, `final1`). Versionless names are returned whole.
fn strip_version(name: &str) -> &str {
    match name.rsplit_once('_') {
        Some((base, suffix)) if suffix.bytes().any(|b| b.is_ascii_digit()) => base,
        _ => name,
    }
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| format!("{s}.glb")).collect()
    }

    #[test]
    fn version_suffix_still_matches() {
        let avail = names(&["koth_bagel_rc12", "koth_clearcut_b18", "koth_product_final"]);
        assert_eq!(
            best_prefix_match("koth_bagel_rc11", &avail).as_deref(),
            Some("koth_bagel_rc12.glb")
        );
    }

    #[test]
    fn shared_mode_prefix_does_not_match() {
        // cp_process vs cp_prolands share only "cp_pro"; not enough to cover the base.
        let avail = names(&["cp_prolands_rc2ta", "cp_gullywash_f9"]);
        assert_eq!(best_prefix_match("cp_process_f12", &avail), None);
    }

    #[test]
    fn no_candidate_returns_none() {
        let avail = names(&["cp_process_f12", "koth_bagel_rc12"]);
        assert_eq!(best_prefix_match("ultiduo_swine_b06", &avail), None);
    }

    #[test]
    fn strip_version_drops_only_versionish_suffix() {
        assert_eq!(strip_version("koth_bagel_rc12"), "koth_bagel");
        assert_eq!(strip_version("cp_snakewater_final1"), "cp_snakewater");
        assert_eq!(strip_version("cp_sunshine"), "cp_sunshine");
    }
}
