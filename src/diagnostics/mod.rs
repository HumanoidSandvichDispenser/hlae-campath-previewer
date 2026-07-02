//! Diagnostics: FPS in the window title + a periodic stderr line, and an F6 memory
//! breakdown of decoded asset bytes held in CPU RAM.

use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::prelude::*;

use crate::app::AppSet;
use crate::demo::{ActiveDemo, Playback};

pub struct DiagnosticsPlugin;

impl Plugin for DiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(FrameTimeDiagnosticsPlugin)
            .add_systems(Update, (report_fps, report_memory).in_set(AppSet::Draw));
    }
}

/// FPS + tick in the window title (bevy_ui is disabled, so the title stands in for
/// on-screen text), plus a stderr line each second.
fn report_fps(
    time: Res<Time>,
    diagnostics: Res<DiagnosticsStore>,
    mut windows: Query<&mut Window>,
    pb: Res<Playback>,
    mut acc: Local<f32>,
) {
    *acc += time.delta_seconds();
    if *acc < 0.5 {
        return;
    }
    *acc = 0.0;
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);
    let title = format!(
        "hlae-campath-previewer · {:.0} fps · tick {} {}",
        fps,
        pb.tick as u32,
        if pb.playing { "▶" } else { "⏸" }
    );
    if let Ok(mut w) = windows.get_single_mut() {
        w.title = title;
    }
    eprintln!("[previewer] {:.1} fps  tick {}", fps, pb.tick as u32);
}

/// F6: dump a breakdown of decoded asset bytes held in CPU RAM, to see where process
/// memory is going. Textures (Image.data) and mesh vertex/index buffers keep a MAIN_WORLD
/// copy in addition to the GPU upload unless stripped, and are usually the largest chunk.
fn report_memory(
    keys: Res<ButtonInput<KeyCode>>,
    images: Res<Assets<Image>>,
    meshes: Res<Assets<Mesh>>,
    demo_res: Res<ActiveDemo>,
) {
    if !keys.just_pressed(KeyCode::F6) {
        return;
    }
    let img_bytes: usize = images.iter().map(|(_, i)| i.data.len()).sum();
    let mesh_bytes: usize = meshes
        .iter()
        .map(|(_, m)| {
            let v: usize = m.attributes().map(|(_, a)| a.get_bytes().len()).sum();
            let i = m.indices().map(|i| i.len() * 4).unwrap_or(0);
            v + i
        })
        .sum();
    let demo_bytes = demo_res.0.approx_bytes();
    let mb = |b: usize| b as f64 / 1_048_576.0;
    eprintln!(
        "[mem] images: {} assets, {:.1} MB (CPU copy) · meshes: {} assets, {:.1} MB · demo cache: {:.1} MB",
        images.len(),
        mb(img_bytes),
        meshes.len(),
        mb(mesh_bytes),
        mb(demo_bytes),
    );
}
