//! Campath authoring: capture keyframes, recompile the spline, follow it with the
//! camera, draw it. Math lives in `spline`, HLAE/VDM output in `export`.

pub mod export;
pub mod spline;

use bevy::prelude::*;

use crate::app::AppSet;
use crate::camera::{FlyCam, DEFAULT_FOV};
use crate::coords::{hammer_to_world_quat, world_to_hammer_quat};
use crate::demo::{ActiveDemo, DemoPath, Playback};

pub struct CampathPlugin;

impl Plugin for CampathPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Campath>()
            .add_systems(Update, campath_input.in_set(AppSet::Input))
            .add_systems(Update, campath_recompile.in_set(AppSet::Playback))
            .add_systems(Update, campath_playback.in_set(AppSet::Sync))
            .add_systems(Update, draw_campath.in_set(AppSet::Draw));
    }
}

/// The path being built. Keyframes are Source Z-up; `compiled` is rebuilt whenever
/// `dirty`; `following` slaves the camera to the path.
#[derive(Resource, Default)]
pub(crate) struct Campath {
    pub(crate) keyframes: Vec<spline::Keyframe>,
    pub(crate) interp: spline::CampathInterp,
    pub(crate) compiled: Option<spline::CompiledCampath>,
    pub(crate) dirty: bool,
    pub(crate) following: bool,
}

/// V add keyframe, L delete highest-tick, P follow, F5 export XML+VDM.
fn campath_input(
    keys: Res<ButtonInput<KeyCode>>,
    pb: Res<Playback>,
    demo_res: Res<ActiveDemo>,
    demo_path: Res<DemoPath>,
    mut path: ResMut<Campath>,
    cam_q: Query<(&Transform, &Projection), With<FlyCam>>,
) {
    if keys.just_pressed(KeyCode::KeyV) {
        if let Ok((tf, proj)) = cam_q.get_single() {
            let tick = pb.tick.round() as u32;
            let inv = world_to_hammer_quat();
            let pos = inv * tf.translation;
            let q = inv * tf.rotation;
            let fov = match proj {
                Projection::Perspective(p) => p.fov.to_degrees(),
                _ => DEFAULT_FOV.to_degrees(),
            };
            let kf = spline::Keyframe {
                tick,
                position: [pos.x, pos.y, pos.z],
                quaternion: [q.x, q.y, q.z, q.w],
                fov,
            };
            // One keyframe per tick: replace if this tick already has one.
            if let Some(i) = path.keyframes.iter().position(|k| k.tick == tick) {
                path.keyframes[i] = kf;
            } else {
                path.keyframes.push(kf);
                path.keyframes.sort_by_key(|k| k.tick);
            }
            path.dirty = true;
            eprintln!(
                "[campath] keyframe @ tick {tick} (total {})",
                path.keyframes.len()
            );
        }
    }

    if keys.just_pressed(KeyCode::KeyL) {
        // Vec is tick-sorted, so pop removes the highest-tick keyframe.
        if let Some(k) = path.keyframes.pop() {
            path.dirty = true;
            eprintln!(
                "[campath] removed keyframe @ tick {} ({} left)",
                k.tick,
                path.keyframes.len()
            );
        }
    }

    if keys.just_pressed(KeyCode::KeyP) {
        path.following = !path.following;
        eprintln!(
            "[campath] follow {}",
            if path.following { "ON" } else { "OFF" }
        );
    }

    if keys.just_pressed(KeyCode::F5) {
        if path.keyframes.len() < 2 {
            eprintln!("[campath] need >= 2 keyframes to export");
        } else {
            let stem = std::path::Path::new(&demo_path.0)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("campath");
            let xml_name = format!("{stem}_campath.xml");
            let vdm_name = format!("{stem}.vdm");
            let xml = export::to_hlae_campath_xml(
                &path.keyframes,
                &path.interp,
                demo_res.0.interval_per_tick(),
            );
            let vdm = export::to_vdm(&path.keyframes, &xml_name);
            let cwd = std::env::current_dir().unwrap_or_default();
            match (
                std::fs::write(&xml_name, xml),
                std::fs::write(&vdm_name, vdm),
            ) {
                (Ok(_), Ok(_)) => eprintln!(
                    "[campath] exported {} + {} to {}",
                    xml_name,
                    vdm_name,
                    cwd.display()
                ),
                (a, b) => eprintln!("[campath] export failed: {a:?} {b:?}"),
            }
        }
    }
}

fn campath_recompile(mut path: ResMut<Campath>) {
    if !path.dirty {
        return;
    }
    path.dirty = false;
    let compiled = spline::CompiledCampath::compile(&path.keyframes, path.interp);
    path.compiled = compiled;
}

/// When following, slave the camera to the path sampled at the current tick.
pub(crate) fn campath_playback(
    path: Res<Campath>,
    pb: Res<Playback>,
    mut cam_q: Query<(&mut Transform, &mut Projection, &mut FlyCam)>,
) {
    if !path.following {
        return;
    }
    let Some(c) = &path.compiled else { return };
    let Ok((mut tf, mut proj, mut cam)) = cam_q.get_single_mut() else {
        return;
    };
    let (lo, hi) = c.tick_range();
    let s = c.eval((pb.tick as f64).clamp(lo, hi));
    let r = hammer_to_world_quat();
    tf.translation = r * s.position;
    tf.rotation = r * s.quaternion;
    if let Projection::Perspective(p) = &mut *proj {
        p.fov = s.fov.to_radians();
    }
    // Keep FlyCam angles in sync so releasing follow doesn't snap the view.
    let (y, x, z) = tf.rotation.to_euler(EulerRot::YXZ);
    cam.yaw = y;
    cam.pitch = x;
    cam.roll = z;
}

/// Polyline along the compiled path plus a frustum at each keyframe.
fn draw_campath(path: Res<Campath>, mut gizmos: Gizmos) {
    let Some(c) = &path.compiled else { return };
    let r = hammer_to_world_quat();
    let (lo, hi) = c.tick_range();
    let segments = 256usize;
    let mut prev: Option<Vec3> = None;
    for i in 0..=segments {
        let t = lo + (hi - lo) * (i as f64 / segments as f64);
        let p = r * c.eval(t).position;
        if let Some(pp) = prev {
            gizmos.line(pp, p, Color::srgb(1.0, 0.85, 0.2));
        }
        prev = Some(p);
    }
    for kf in &path.keyframes {
        let p = r * Vec3::from_array(kf.position);
        let q = r * Quat::from_xyzw(kf.quaternion[0], kf.quaternion[1], kf.quaternion[2], kf.quaternion[3]);
        draw_frustum(&mut gizmos, p, q, Color::srgb(1.0, 0.4, 0.9));
    }
}

/// Wireframe frustum: apex at `p`, opening along -Z, oriented by `q`.
fn draw_frustum(gizmos: &mut Gizmos, p: Vec3, q: Quat, color: Color) {
    const DEPTH: f32 = 60.0; // distance to the far plane, world units
    const HALF_W: f32 = 26.0; // half width of the far plane (~47deg horizontal FOV)
    const HALF_H: f32 = 16.0; // half height of the far plane

    let fwd = q * Vec3::new(0.0, 0.0, -1.0);
    let up = q * Vec3::Y;
    let right = q * Vec3::X;
    let center = p + fwd * DEPTH;

    // Far-plane corners, CCW: top-left, top-right, bottom-right, bottom-left.
    let corners = [
        center - right * HALF_W + up * HALF_H,
        center + right * HALF_W + up * HALF_H,
        center + right * HALF_W - up * HALF_H,
        center - right * HALF_W - up * HALF_H,
    ];

    for c in corners {
        gizmos.line(p, c, color); // apex edge
    }
    for i in 0..4 {
        gizmos.line(corners[i], corners[(i + 1) % 4], color); // far-plane outline
    }
    // Up indicator: a small marker at the top edge so orientation reads at a glance.
    let top_mid = (corners[0] + corners[1]) * 0.5;
    gizmos.line(top_mid, top_mid + up * HALF_H * 0.5, Color::srgb(0.3, 0.9, 1.0));
}
