//! Campath authoring: capture keyframes, recompile the spline, follow it with the
//! camera, draw it. Math lives in `spline`, HLAE/VDM output in `export`.

pub mod export;
pub mod spline;

use bevy::prelude::*;

use crate::app::AppSet;
use crate::camera::{FlyCam, DEFAULT_FOV};
use crate::coords::{hammer_to_world_quat, world_to_hammer_quat};
use crate::demo::{ActiveDemo, DemoPath, DemoSource, Playback};

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
    pub(crate) selected: Option<u64>,
    next_id: u64,
}

impl Campath {
    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Add a keyframe at `tick`, or overwrite the one already there (keeping its id).
    /// Returns the keyframe's id.
    pub(crate) fn set_at_tick(
        &mut self,
        tick: u32,
        position: [f32; 3],
        quaternion: [f32; 4],
        fov: f32,
    ) -> u64 {
        self.dirty = true;
        if let Some(i) = self.keyframes.iter().position(|k| k.tick == tick) {
            let id = self.keyframes[i].id;
            self.keyframes[i] = spline::Keyframe { id, tick, position, quaternion, fov };
            id
        } else {
            let id = self.alloc_id();
            self.keyframes.push(spline::Keyframe { id, tick, position, quaternion, fov });
            self.keyframes.sort_by_key(|k| k.tick);
            id
        }
    }

    pub(crate) fn delete(&mut self, id: u64) {
        self.keyframes.retain(|k| k.id != id);
        if self.selected == Some(id) {
            self.selected = None;
        }
        self.dirty = true;
    }

    pub(crate) fn clear(&mut self) {
        self.keyframes.clear();
        self.selected = None;
        self.dirty = true;
    }

    /// Replace all keyframes with imported ones, assigning fresh ids, and set interp.
    pub(crate) fn load_imported(
        &mut self,
        keyframes: Vec<spline::Keyframe>,
        interp: spline::CampathInterp,
    ) {
        self.keyframes.clear();
        self.selected = None;
        self.interp = interp;
        for mut kf in keyframes {
            kf.id = self.alloc_id();
            self.keyframes.push(kf);
        }
        self.keyframes.sort_by_key(|k| k.tick);
        self.dirty = true;
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.keyframes.iter().position(|k| k.id == id)
    }

    /// Move a keyframe to `tick`. Fails (returns false) if another keyframe is already
    /// there, leaving the path untouched.
    pub(crate) fn retime(&mut self, id: u64, tick: u32) -> bool {
        if self.keyframes.iter().any(|k| k.id != id && k.tick == tick) {
            return false;
        }
        if let Some(i) = self.index_of(id) {
            self.keyframes[i].tick = tick;
            self.keyframes.sort_by_key(|k| k.tick);
            self.dirty = true;
            true
        } else {
            false
        }
    }

    pub(crate) fn with_keyframe(&mut self, id: u64, f: impl FnOnce(&mut spline::Keyframe)) {
        if let Some(i) = self.index_of(id) {
            f(&mut self.keyframes[i]);
            self.dirty = true;
        }
    }
}

/// Camera pose in Source Z-up: position, quaternion [x,y,z,w], vertical fov in degrees.
/// This is what a keyframe stores.
pub(crate) fn capture_pose(tf: &Transform, proj: &Projection) -> ([f32; 3], [f32; 4], f32) {
    let inv = world_to_hammer_quat();
    let pos = inv * tf.translation;
    let q = inv * tf.rotation;
    let fov = match proj {
        Projection::Perspective(p) => p.fov.to_degrees(),
        _ => DEFAULT_FOV.to_degrees(),
    };
    ([pos.x, pos.y, pos.z], [q.x, q.y, q.z, q.w], fov)
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
            let (pos, quat, fov) = capture_pose(tf, proj);
            let id = path.set_at_tick(tick, pos, quat, fov);
            path.selected = Some(id);
            eprintln!(
                "[campath] keyframe @ tick {tick} (total {})",
                path.keyframes.len()
            );
        }
    }

    if keys.just_pressed(KeyCode::KeyL) {
        // Vec is tick-sorted, so the last one is the highest-tick keyframe.
        if let Some(k) = path.keyframes.last().copied() {
            path.delete(k.id);
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
        export_campath(&path, &demo_path.0, demo_res.0.as_ref());
    }
}

/// Write the campath's XML + VDM next to the demo, named from `demo_stem`. Needs at
/// least 2 keyframes; logs what it wrote (or why it didn't).
pub(crate) fn export_campath(path: &Campath, demo_stem: &str, demo: &dyn DemoSource) {
    if path.keyframes.len() < 2 {
        eprintln!("[campath] need >= 2 keyframes to export");
        return;
    }
    let stem = std::path::Path::new(demo_stem)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("campath");
    let xml_name = format!("{stem}_campath.xml");
    let vdm_name = format!("{stem}.vdm");
    let interval = demo.interval_per_tick();
    let xml = export::to_hlae_campath_xml(&path.keyframes, &path.interp, interval, |t| {
        demo.demo_to_server_tick(t as i64)
    });
    let vdm = export::to_vdm(&path.keyframes, &xml_name);
    let cwd = std::env::current_dir().unwrap_or_default();
    match (std::fs::write(&xml_name, xml), std::fs::write(&vdm_name, vdm)) {
        (Ok(_), Ok(_)) => {
            eprintln!("[campath] exported {} + {} to {}", xml_name, vdm_name, cwd.display())
        }
        (a, b) => eprintln!("[campath] export failed: {a:?} {b:?}"),
    }
}

/// Write just the campath XML to `xml_path`.
pub(crate) fn export_xml_to(
    path: &Campath,
    xml_path: &std::path::Path,
    demo: &dyn DemoSource,
) -> std::io::Result<()> {
    let xml =
        export::to_hlae_campath_xml(&path.keyframes, &path.interp, demo.interval_per_tick(), |t| {
            demo.demo_to_server_tick(t as i64)
        });
    std::fs::write(xml_path, xml)
}

/// Write just the VDM to `vdm_path`. `xml_file_name` is the campath file the VDM tells
/// HLAE to load (bare file name, no path).
pub(crate) fn export_vdm_to(
    path: &Campath,
    vdm_path: &std::path::Path,
    xml_file_name: &str,
) -> std::io::Result<()> {
    let vdm = export::to_vdm(&path.keyframes, xml_file_name);
    std::fs::write(vdm_path, vdm)
}

/// Load keyframes + interp from an HLAE campath XML into `path`, replacing what's there.
pub(crate) fn import_campath(
    path: &mut Campath,
    xml_path: &std::path::Path,
    demo: &dyn DemoSource,
) -> anyhow::Result<usize> {
    let xml = std::fs::read_to_string(xml_path)?;
    let (keyframes, interp) =
        export::from_hlae_campath_xml(&xml, demo.interval_per_tick(), |s| {
            demo.server_to_demo_tick(s)
        })?;
    let n = keyframes.len();
    path.load_imported(keyframes, interp);
    Ok(n)
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
fn draw_campath(path: Res<Campath>, opts: Res<crate::ui::ViewOptions>, mut gizmos: Gizmos) {
    if !opts.show_campath {
        return;
    }
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
