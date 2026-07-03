//! Native previewer: parse a TF2 demo, render the map GLB + players/projectiles at a
//! tick, free-fly camera, scrub the timeline, author + export an HLAE campath.
//!
//! Usage: hlae-campath-previewer <demo.dem|-> [map.glb] [--import <campath.xml>] [--data-dir <dir>]
//!
//!   Demo, map, and import paths resolve against the data dir (default
//!   $XDG_DATA_HOME/hlae-campath-previewer, or ~/.local/share/hlae-campath-previewer,
//!   overridden with --data-dir). Absolute paths and `-` (stdin) bypass it.
//!   `--import` loads an existing campath XML on startup.
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

use std::path::{Path, PathBuf};

use bevy::prelude::*;
use clap::Parser;

use app::AppPlugin;
use camera::CameraPlugin;
use campath::{CampathPlugin, ImportOnStartup};
use demo::{ActiveDemo, DemoPath, DemoPlugin};
use diagnostics::DiagnosticsPlugin;
use entities::EntitiesPlugin;
use map::{MapAssetPath, MapPlugin};
use ui::UiPlugin;

/// Native TF2 demo previewer + HLAE campath author.
#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    /// Demo to load (bare name resolves against --data-dir); `-` reads it from stdin.
    #[arg(default_value = "demo.dem")]
    demo: String,
    /// Map GLB to render (bare name resolves against --data-dir); auto-resolved from
    /// the demo header if omitted.
    map: Option<String>,
    /// Campath XML to import on startup (e.g. one authored earlier for this demo).
    #[arg(short, long, value_name = "XML")]
    import: Option<PathBuf>,
    /// Directory holding demos, map GLBs, and player models. Defaults to
    /// $XDG_DATA_HOME/hlae-campath-previewer (or ~/.local/share/hlae-campath-previewer).
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Canonicalize + create the data dir up front so Bevy's AssetPlugin can read it.
    let data_dir = prepare_data_dir(args.data_dir.as_deref().unwrap_or(&default_data_dir()))?;

    // Relative demo/map names resolve under the data dir; absolute paths and `-`
    // (stdin) pass through untouched. --import is a user-authored XML, so it resolves
    // normally against the cwd like any other CLI path.
    let demo_path = resolve_in(&args.demo, &data_dir);
    let import_path = args.import;

    // `-` reads the demo from stdin; give exports a sensible stem in that case since
    // there's no filename to derive one from.
    let export_stem = if args.demo == "-" {
        "campath".to_string()
    } else {
        demo_path.to_string_lossy().into_owned()
    };
    let src_label = if args.demo == "-" {
        "<stdin>"
    } else {
        &args.demo
    };

    eprintln!("[previewer] data dir: {}", data_dir.display());
    eprintln!("[previewer] parsing {src_label} ...");
    let bytes = demo::load_demo_bytes(&demo_path.to_string_lossy())?;
    let data = demo::parse(&bytes)?;
    eprintln!(
        "[previewer] parsed: {:.4}s/tick, max_tick={}, map={}, first_curtime_tick={}",
        demo::DemoSource::interval_per_tick(&data),
        demo::DemoSource::max_tick(&data),
        data.map_name(),
        demo::DemoSource::demo_to_server_tick(&data, 1),
    );

    // Prefer an explicit CLI map; otherwise resolve one from the demo header against the
    // GLBs in the data dir, tolerating version suffixes. Fall back if nothing matches.
    let map_asset = args
        .map
        .or_else(|| resolve_map_asset(data.map_name(), &data_dir))
        .unwrap_or_else(|| {
            eprintln!(
                "[previewer] no map asset for '{}', rendering without geometry",
                data.map_name()
            );
            "map_snakewater.glb".into()
        });
    eprintln!("[previewer] map asset: {map_asset}");

    App::new()
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "hlae-campath-previewer".into(),
                        ..default()
                    }),
                    ..default()
                })
                // Mount maps/player models from the data dir instead of a hard-coded
                // `assets/` next to the cwd.
                .set(AssetPlugin {
                    file_path: data_dir.to_string_lossy().into_owned(),
                    ..default()
                }),
        )
        .insert_resource(ClearColor(Color::srgb(0.05, 0.06, 0.08)))
        // Untextured map surfaces facing away from the sun would be near-black; lift them.
        .insert_resource(AmbientLight {
            color: Color::WHITE,
            brightness: 600.0,
        })
        .insert_resource(DemoPath(export_stem))
        .insert_resource(MapAssetPath(map_asset))
        .insert_resource(ActiveDemo(Box::new(data)))
        .insert_resource(ImportOnStartup(import_path))
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

/// Default data dir: $XDG_DATA_HOME/hlae-campath-previewer, falling back to
/// ~/.local/share/hlae-campath-previewer, and finally the in-tree `assets/` dir if
/// neither env var is set (e.g. running as a service).
fn default_data_dir() -> PathBuf {
    fn nonempty_env(name: &str) -> Option<PathBuf> {
        let v = std::env::var(name).ok()?;
        (!v.is_empty()).then(|| PathBuf::from(v))
    }

    if let Some(xdg) = nonempty_env("XDG_DATA_HOME") {
        return xdg.join("hlae-campath-previewer");
    }
    if let Some(home) = nonempty_env("HOME") {
        return home.join(".local/share/hlae-campath-previewer");
    }
    PathBuf::from("assets")
}

/// Resolve `dir` to an absolute path and make sure it exists before Bevy's
/// `AssetPlugin` reads it. Relative paths are resolved against the cwd first.
fn prepare_data_dir(dir: &Path) -> anyhow::Result<PathBuf> {
    let abs = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()?.join(dir)
    };
    std::fs::create_dir_all(&abs)?;
    Ok(abs)
}

/// Resolve `input` against `data_dir`. `-` (stdin) and absolute paths pass through
/// untouched; bare/relative names resolve under `data_dir` so `demo.dem` loads from
/// `<data_dir>/demo.dem`.
fn resolve_in(input: impl AsRef<Path>, data_dir: &Path) -> PathBuf {
    let input = input.as_ref();
    if input.as_os_str() == "-" {
        return PathBuf::from("-");
    }
    if input.is_absolute() {
        input.to_path_buf()
    } else {
        data_dir.join(input)
    }
}

/// Find the map GLB in `data_dir` for the demo header's `map` name, tolerating version
/// suffixes. An exact `<map>.glb` wins; otherwise the asset sharing the longest common
/// prefix, but only if that prefix covers the base map name (everything before the
/// trailing version token). So `koth_bagel_rc11` still finds `koth_bagel_rc12`, while
/// `cp_process_f12` never grabs `cp_prolands_rc2ta`. Returns a name relative to data_dir.
fn resolve_map_asset(map: &str, data_dir: &Path) -> Option<String> {
    if map.is_empty() {
        return None;
    }
    let exact = format!("{map}.glb");

    let names: Vec<String> = std::fs::read_dir(data_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".glb"))
        .collect();

    if names.contains(&exact) {
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
