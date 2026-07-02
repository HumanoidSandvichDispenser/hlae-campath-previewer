//! Native previewer spike: parse a TF2 demo, render the map GLB + players at a tick,
//! free-fly camera, scrub the timeline. Players are team-colored spheres — enough to
//! verify positions line up with the map. GLB player models are a known-easy next step.
//!
//! Usage: previewer-native <demo.dem> <map.glb>
//!   assets are loaded relative to the `assets/` dir next to the binary.
//!
//! Controls: RMB hold = mouselook · WASD/RF fly · Space = play/pause · ←/→ seek 50 ·
//!           ,/. step 1 tick · +/- fly speed · B = toggle AABBs · Esc = quit
//! Campath:  V = add keyframe here · L = delete last · P = follow path · F5 = export

mod campath;
mod demo;

use std::f32::consts::FRAC_PI_2;

use std::collections::HashMap;

use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
use bevy::render::camera::Viewport;
use bevy::window::CursorGrabMode;

// ── Coordinate calibration (tune on artix by eye) ───────────────────────────────
// Demo positions are Hammer/Source coords, Z-up. The map GLB carries those same raw
// coords but glTF is Y-up, so we parent BOTH map and players under a root rotated
// -90° about X — that makes Hammer +Z become engine +Y. If the map looks tipped or
// mirrored on artix, this rotation (and MAP_SCENE handedness) is the first knob.
fn world_root_transform() -> Transform {
    Transform::from_rotation(Quat::from_rotation_x(-FRAC_PI_2))
}

const PLAYER_POOL: usize = 32;
const PROJECTILE_POOL: usize = 64;
/// How many ticks of history to draw behind each live projectile as a trail.
const TRAIL_TICKS: u32 = 40;
const FLY_SPEED: f32 = 900.0;
const MOUSE_SENS: f32 = 0.0015;

// HLAE-style camera rates.
const ROLL_RATE: f32 = 1.2; // rad/s
const ZOOM_RATE: f32 = 0.7; // rad/s (fov)
const FOV_MIN: f32 = 0.15;
const FOV_MAX: f32 = 2.2;
const DEFAULT_FOV: f32 = 1.309; // ~75°
const SPEED_STEP: f32 = 1.5; // multiplicative per +/- keypress
const SPEED_MIN: f32 = 30.0;
const SPEED_MAX: f32 = 20000.0;

// Player models are glTF Y-up; standing them up in our Hammer-Z-up player space needs
// the same +90° X as the map. Yaw offset tunes which way "yaw=0" faces — live-adjust
// with [ and ] (PlayerYaw resource); default +90° corrects the observed rightward facing.

#[derive(Resource)]
struct DemoRes(demo::DemoData);

/// Toggle the per-player collision AABB wireframe (B).
#[derive(Resource)]
struct ShowAabb(bool);

/// The campath being built: keyframes (Source Z-up), interp mode, and the compiled
/// spline (rebuilt whenever `dirty`). `following` slaves the camera to the path.
#[derive(Resource, Default)]
struct Campath {
    keyframes: Vec<campath::Keyframe>,
    interp: campath::CampathInterp,
    compiled: Option<campath::CompiledCampath>,
    dirty: bool,
    following: bool,
}

#[derive(Resource)]
struct Playback {
    tick: f32,
    playing: bool,
}

/// Preloaded player scenes keyed by (class, team). class 1..=9, team 2=red / 3=blue.
#[derive(Resource)]
struct PlayerModels(HashMap<(u8, u8), Handle<Scene>>);

#[derive(Component)]
struct PlayerSlot(usize);

/// Which model a rig slot currently has spawned, so we only re-spawn on class/team change.
#[derive(Component, Default)]
struct ModelState {
    key: Option<(u8, u8)>,
    child: Option<Entity>,
    /// Per-instance material clones (so alpha can fade this player independently).
    cloned: bool,
    mats: Vec<Handle<StandardMaterial>>,
    blend: bool,
}

/// A slot in the projectile render pool.
#[derive(Component)]
struct ProjectileSlot(usize);

/// Preloaded render assets per projectile type (indexed by ProjectileType u8).
/// Rocket/pipe/sticky have real GLB scenes (from dribble.tf); every other type
/// falls back to a primitive mesh. team_idx: 0 = red, 1 = blue.
#[derive(Resource)]
struct ProjectileAssets {
    /// Primitive fallback mesh per type.
    mesh: Vec<Handle<Mesh>>,
    /// Primitive fallback material per type, per team.
    mat: Vec<[Handle<StandardMaterial>; 2]>,
    /// Real GLB scene per type, per team. `None` = no model, use the primitive.
    scene: Vec<[Option<Handle<Scene>>; 2]>,
    /// Whether this type is elongated (rocket/arrow) and should aim along travel.
    oriented: Vec<bool>,
}

/// Which representation a projectile slot currently shows, so we only respawn the
/// child on a type/team change. `(ty, team, is_scene)`.
#[derive(Component, Default)]
struct ProjectileModelState {
    key: Option<(u8, u8, bool)>,
    child: Option<Entity>,
}

fn projectile_type_name(ty: u8) -> &'static str {
    match ty {
        0 => "rocket",
        1 => "healing arrow",
        2 => "sticky",
        3 => "pipe",
        4 => "flare",
        5 => "loose cannon",
        _ => "unknown",
    }
}

fn class_name(class: u8) -> Option<&'static str> {
    Some(match class {
        1 => "scout",
        2 => "sniper",
        3 => "soldier",
        4 => "demoman",
        5 => "medic",
        6 => "heavy",
        7 => "pyro",
        8 => "spy",
        9 => "engineer",
        _ => return None,
    })
}

/// Parent node holding the map mesh at its calibrated orientation.
#[derive(Component)]
struct MapRoot;

/// Constant yaw offset applied to every player model (radians).
#[derive(Resource)]
struct PlayerYaw(f32);

/// Path to the loaded demo, used to name exported campath files.
#[derive(Resource)]
struct DemoPath(String);

#[derive(Component, Default)]
struct FlyCam {
    yaw: f32,
    pitch: f32,
    roll: f32,
    speed: f32,
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let demo_path = args.next().unwrap_or_else(|| "assets/demo.dem".into());
    let map_asset = args.next().unwrap_or_else(|| "map_snakewater.glb".into());

    eprintln!("[spike] parsing {demo_path} ...");
    let bytes = std::fs::read(&demo_path)?;
    let data = demo::parse(&bytes)?;
    eprintln!(
        "[spike] parsed: {} frames, {:.4}s/tick, max_tick={}",
        data.frames.len(),
        data.interval_per_tick,
        data.max_tick
    );

    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "previewer-native spike".into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(FrameTimeDiagnosticsPlugin)
        .insert_resource(ClearColor(Color::srgb(0.05, 0.06, 0.08)))
        // Untextured map surfaces facing away from the sun would be near-black; lift them.
        .insert_resource(AmbientLight {
            color: Color::WHITE,
            brightness: 600.0,
        })
        .insert_resource(Playback {
            tick: 1.0,
            playing: false,
        })
        .insert_resource(ShowAabb(true))
        .insert_resource(Campath::default())
        .insert_resource(DemoPath(demo_path.clone()))
        .insert_resource(MapAssetPath(map_asset))
        .insert_resource(DemoRes(data))
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                input_controls,
                advance_playback,
                update_players,
                update_projectiles,
                draw_projectile_trails,
                draw_player_aabb,
                campath_input,
                campath_recompile,
                campath_playback,
                draw_campath,
                enforce_16_9_viewport,
                clone_player_materials,
                fade_dead_players,
                fly_camera,
                report_fps,
                report_memory,
            )
                .chain(),
        )
        .run();

    Ok(())
}

#[derive(Resource)]
struct MapAssetPath(String);

fn setup(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    map_path: Res<MapAssetPath>,
    demo_res: Res<DemoRes>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let map_scene: Handle<Scene> = asset_server.load(format!("{}#Scene0", map_path.0));

    // Preload every class/team model.
    let mut models = HashMap::new();
    for class in 1..=9u8 {
        let name = class_name(class).unwrap();
        for (team, tn) in [(2u8, "red"), (3u8, "blue")] {
            let h: Handle<Scene> = asset_server.load(format!("players/{name}_{tn}.glb#Scene0"));
            models.insert((class, team), h);
        }
    }
    commands.insert_resource(PlayerModels(models));
    commands.insert_resource(PlayerYaw(FRAC_PI_2)); // +90°: corrects observed rightward facing

    // Projectile meshes/materials, keyed by ProjectileType u8 (0..=7). No dedicated GLBs
    // exist, so build primitive shapes: capsules for rockets/arrows (oriented to travel),
    // spheres for the various grenades/flares.
    let mut proj_mesh = Vec::with_capacity(8);
    let mut proj_oriented = Vec::with_capacity(8);
    for ty in 0..8u8 {
        let (mesh, oriented): (Mesh, bool) = match ty {
            0 => (Capsule3d::new(3.0, 14.0).into(), true),  // rocket
            1 => (Capsule3d::new(1.5, 30.0).into(), true),  // healing arrow
            2 => (Sphere::new(5.0).into(), false),          // sticky
            3 => (Sphere::new(5.0).into(), false),          // pipe
            4 => (Sphere::new(3.5).into(), false),          // flare
            5 => (Sphere::new(6.0).into(), false),          // loose cannon
            _ => (Sphere::new(4.0).into(), false),          // unknown
        };
        proj_mesh.push(meshes.add(mesh));
        proj_oriented.push(oriented);
    }
    // Per-team materials. Flares glow orange regardless of team.
    let mut proj_mat: Vec<[Handle<StandardMaterial>; 2]> = Vec::with_capacity(8);
    for ty in 0..8u8 {
        let mut make = |base: Color, emissive: LinearRgba| {
            materials.add(StandardMaterial {
                base_color: base,
                emissive,
                perceptual_roughness: 0.6,
                ..default()
            })
        };
        let pair = if ty == 4 {
            let flare = LinearRgba::new(3.0, 1.2, 0.1, 1.0);
            [
                make(Color::srgb(1.0, 0.6, 0.1), flare),
                make(Color::srgb(1.0, 0.6, 0.1), flare),
            ]
        } else {
            [
                make(Color::srgb(1.0, 0.25, 0.2), LinearRgba::new(0.3, 0.05, 0.04, 1.0)),
                make(Color::srgb(0.3, 0.5, 1.0), LinearRgba::new(0.04, 0.08, 0.3, 1.0)),
            ]
        };
        proj_mat.push(pair);
    }
    // Real GLB models for rocket/pipe/sticky (dribble.tf assets); others stay primitive.
    // Rocket is team-neutral (one shared model used for both).
    let mut proj_scene: Vec<[Option<Handle<Scene>>; 2]> = vec![[None, None]; 8];
    let load_scene = |s: &AssetServer, p: &str| -> Handle<Scene> { s.load(format!("{p}#Scene0")) };
    let rocket = load_scene(&asset_server, "projectiles/rocket_shared.glb");
    proj_scene[0] = [Some(rocket.clone()), Some(rocket)]; // rocket
    proj_scene[2] = [
        Some(load_scene(&asset_server, "projectiles/stickybomb_red.glb")),
        Some(load_scene(&asset_server, "projectiles/stickybomb_blue.glb")),
    ]; // sticky
    proj_scene[3] = [
        Some(load_scene(&asset_server, "projectiles/pipebomb_red.glb")),
        Some(load_scene(&asset_server, "projectiles/pipebomb_blue.glb")),
    ]; // pipe

    commands.insert_resource(ProjectileAssets {
        mesh: proj_mesh,
        mat: proj_mat,
        scene: proj_scene,
        oriented: proj_oriented,
    });

    // Everything that lives in Hammer coords goes under this rotated root.
    let root = commands.spawn((SpatialBundle::from_transform(world_root_transform()),)).id();

    // The map GLB is exported Y-up, so it needs +90° about X to cancel the world root's
    // -90° (players live in raw Hammer coords and want the root rotation as-is).
    let map_rot = Quat::from_rotation_x(FRAC_PI_2);

    commands.entity(root).with_children(|parent| {
        parent
            .spawn((
                SpatialBundle::from_transform(Transform::from_rotation(map_rot)),
                MapRoot,
            ))
            .with_children(|map_parent| {
                map_parent.spawn(SceneBundle {
                    scene: map_scene,
                    ..default()
                });
            });
        for i in 0..PLAYER_POOL {
            parent.spawn((
                SpatialBundle {
                    visibility: Visibility::Hidden,
                    ..default()
                },
                PlayerSlot(i),
                ModelState::default(),
            ));
        }
        // Projectile render pool: hidden spatial nodes; each gets a mesh/scene child
        // assigned per tick (mirrors the player rig pattern).
        for i in 0..PROJECTILE_POOL {
            parent.spawn((
                SpatialBundle {
                    visibility: Visibility::Hidden,
                    ..default()
                },
                ProjectileSlot(i),
                ProjectileModelState::default(),
            ));
        }
    });

    // Aim the camera at the first tick's players so you don't spawn in the void.
    let look_at = demo_res
        .0
        .frames
        .iter()
        .find(|f| !f.players.is_empty())
        .map(|f| {
            let n = f.players.len() as f32;
            let sum = f.players.iter().fold([0.0f32; 3], |mut a, p| {
                a[0] += p.pos[0];
                a[1] += p.pos[1];
                a[2] += p.pos[2];
                a
            });
            hammer_to_world([sum[0] / n, sum[1] / n, sum[2] / n])
        })
        .unwrap_or(Vec3::ZERO);

    let cam_pos = look_at + Vec3::new(0.0, 800.0, 1600.0);
    let mut cam_tf = Transform::from_translation(cam_pos);
    cam_tf.look_at(look_at, Vec3::Y);
    let (yaw, pitch, _) = cam_tf.rotation.to_euler(EulerRot::YXZ);
    eprintln!("[spike] look_at={look_at:?} cam_pos={cam_pos:?}");

    commands.spawn((
        Camera3dBundle {
            transform: cam_tf,
            // Hammer maps span thousands of units; the default far=1000 clips the
            // whole map. Push near/far way out.
            projection: Projection::Perspective(PerspectiveProjection {
                near: 1.0,
                far: 200_000.0,
                fov: DEFAULT_FOV,
                ..default()
            }),
            ..default()
        },
        FlyCam {
            yaw,
            pitch,
            roll: 0.0,
            speed: FLY_SPEED,
        },
    ));

    commands.spawn(DirectionalLightBundle {
        directional_light: DirectionalLight {
            illuminance: 12_000.0,
            shadows_enabled: false,
            ..default()
        },
        transform: Transform::from_xyz(1.0, 3.0, 1.5).looking_at(Vec3::ZERO, Vec3::Y),
        ..default()
    });
}

/// FPS in the window title (bevy_ui is disabled, so no on-screen text) + a stderr line
/// each second, both so you can read the number that answers the whole spike.
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
        "previewer-native · {:.0} fps · tick {} {}",
        fps,
        pb.tick as u32,
        if pb.playing { "▶" } else { "⏸" }
    );
    if let Ok(mut w) = windows.get_single_mut() {
        w.title = title;
    }
    eprintln!("[spike] {:.1} fps  tick {}", fps, pb.tick as u32);
}

/// F6: dump a breakdown of decoded asset bytes held in CPU RAM, to see where process
/// memory is going. Textures (Image.data) and mesh vertex/index buffers keep a MAIN_WORLD
/// copy in addition to the GPU upload unless stripped, and are usually the largest chunk.
fn report_memory(
    keys: Res<ButtonInput<KeyCode>>,
    images: Res<Assets<Image>>,
    meshes: Res<Assets<Mesh>>,
    demo_res: Res<DemoRes>,
) {
    if !keys.just_pressed(KeyCode::F6) {
        return;
    }
    let img_bytes: usize = images.iter().map(|(_, i)| i.data.len()).sum();
    let mesh_bytes: usize = meshes
        .iter()
        .map(|(_, m)| {
            let v: usize = m
                .attributes()
                .map(|(_, a)| a.get_bytes().len())
                .sum();
            let i = m.indices().map(|i| i.len() * 4).unwrap_or(0);
            v + i
        })
        .sum();
    let demo_bytes: usize = demo_res
        .0
        .frames
        .iter()
        .map(|f| {
            f.players.len() * std::mem::size_of::<demo::PlayerSnap>()
                + f.projectiles.len() * std::mem::size_of::<demo::ProjectileSnap>()
        })
        .sum();
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

// Rotations between Source/Hammer Z-up (where campath data lives, matching the web
// app + HLAE) and bevy's Y-up world. The camera lives directly in bevy world, so we
// convert its pose at capture (world->hammer) and at playback (hammer->world).
fn hammer_to_world_quat() -> Quat {
    Quat::from_rotation_x(-FRAC_PI_2)
}
fn world_to_hammer_quat() -> Quat {
    Quat::from_rotation_x(FRAC_PI_2)
}

/// Hammer coords -> engine world (matches the -90°-about-X root applied to children).
fn hammer_to_world(p: [f32; 3]) -> Vec3 {
    hammer_to_world_quat() * Vec3::new(p[0], p[1], p[2])
}

/// V add keyframe · L delete highest-tick · P follow · F5 export XML+VDM.
fn campath_input(
    keys: Res<ButtonInput<KeyCode>>,
    pb: Res<Playback>,
    demo_res: Res<DemoRes>,
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
            let kf = campath::Keyframe {
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
            let xml = campath::to_hlae_campath_xml(
                &path.keyframes,
                &path.interp,
                demo_res.0.interval_per_tick,
            );
            let vdm = campath::to_vdm(&path.keyframes, &xml_name);
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

/// Rebuild the compiled spline whenever keyframes/interp changed.
fn campath_recompile(mut path: ResMut<Campath>) {
    if !path.dirty {
        return;
    }
    path.dirty = false;
    let compiled = campath::CompiledCampath::compile(&path.keyframes, path.interp);
    path.compiled = compiled;
}

/// When following, slave the camera to the path sampled at the current tick.
fn campath_playback(
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

/// Draw the compiled path as a polyline plus a marker + forward ray at each keyframe.
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

/// Draw a camera frustum: apex at `p`, opening along the camera's forward (-Z) axis,
/// oriented by `q`. Four edges run from the apex to a far rectangle, which is outlined.
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

/// Keep the camera viewport at the largest 16:9 rect that fits the window,
/// centered with letterbox/pillarbox bars filled by the clear color.
fn enforce_16_9_viewport(
    windows: Query<&Window>,
    mut cam_q: Query<&mut Camera, With<FlyCam>>,
) {
    let Ok(window) = windows.get_single() else { return };
    let Ok(mut camera) = cam_q.get_single_mut() else { return };
    let w = window.physical_width();
    let h = window.physical_height();
    if w == 0 || h == 0 {
        return;
    }
    let (vw, vh) = if w * 9 > h * 16 {
        (h * 16 / 9, h) // wider than 16:9 → pillarbox
    } else {
        (w, w * 9 / 16) // taller than 16:9 → letterbox
    };
    let x = (w - vw) / 2;
    let y = (h - vh) / 2;
    camera.viewport = Some(Viewport {
        physical_position: UVec2::new(x, y),
        physical_size: UVec2::new(vw, vh),
        ..default()
    });
}

fn advance_playback(time: Res<Time>, mut pb: ResMut<Playback>, demo_res: Res<DemoRes>) {
    if !pb.playing {
        return;
    }
    let dt = time.delta_seconds();
    let ticks = dt / demo_res.0.interval_per_tick.max(0.001);
    pb.tick = (pb.tick + ticks).min(demo_res.0.max_tick as f32);
}

fn update_players(
    mut commands: Commands,
    pb: Res<Playback>,
    demo_res: Res<DemoRes>,
    models: Res<PlayerModels>,
    yaw_off: Res<PlayerYaw>,
    mut q: Query<(
        Entity,
        &PlayerSlot,
        &mut Transform,
        &mut Visibility,
        &mut ModelState,
    )>,
) {
    let frame = demo_res.0.frame_at(pb.tick as u32);
    let players = frame.map(|f| f.players.as_slice()).unwrap_or(&[]);
    // glTF-Y-up -> our Hammer-Z-up player space (stand the model upright).
    let stand_up = Quat::from_rotation_x(FRAC_PI_2);

    for (rig, slot, mut tf, mut vis, mut ms) in &mut q {
        let Some(p) = players.get(slot.0) else {
            *vis = Visibility::Hidden;
            continue;
        };

        // Children of the rotated root, so keep positions in raw Hammer coords; yaw is
        // about Hammer up (Z). Model origin is at the feet.
        tf.translation = Vec3::new(p.pos[0], p.pos[1], p.pos[2]);
        tf.rotation = Quat::from_rotation_z(p.yaw.to_radians() + yaw_off.0);

        let key = (p.class, p.team);
        if ms.key != Some(key) {
            if let Some(child) = ms.child.take() {
                commands.entity(child).despawn_recursive();
            }
            ms.cloned = false;
            ms.mats.clear();
            ms.blend = false;
            if let Some(scene) = models.0.get(&key).cloned() {
                let child = commands
                    .spawn(SceneBundle {
                        scene,
                        transform: Transform::from_rotation(stand_up),
                        ..default()
                    })
                    .id();
                commands.entity(rig).add_child(child);
                ms.child = Some(child);
                ms.key = Some(key);
            } else {
                ms.key = None; // unknown class (e.g. mid-join) — nothing to show
            }
        }

        *vis = if ms.child.is_some() {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
    }
}

/// Place the projectile pool at the current tick: assign each active projectile a pool
/// slot, swap in the mesh/material for its type, orient elongated types along travel,
/// and hide the unused slots. Children of the world root, so positions stay Hammer-space.
fn update_projectiles(
    mut commands: Commands,
    pb: Res<Playback>,
    demo_res: Res<DemoRes>,
    assets: Res<ProjectileAssets>,
    mut q: Query<(
        Entity,
        &ProjectileSlot,
        &mut Transform,
        &mut Visibility,
        &mut ProjectileModelState,
    )>,
) {
    let frame = demo_res.0.frame_at(pb.tick as u32);
    let projs = frame.map(|f| f.projectiles.as_slice()).unwrap_or(&[]);
    // glTF-Y-up -> Hammer-Z-up, same correction the player models get.
    let stand_up = Quat::from_rotation_x(FRAC_PI_2);

    for (slot_ent, slot, mut tf, mut vis, mut ms) in &mut q {
        let Some(pr) = projs.get(slot.0) else {
            *vis = Visibility::Hidden;
            continue;
        };
        let ty = pr.ty.min(7) as usize;
        let team_idx = if pr.team == 3 { 1 } else { 0 };
        let has_scene = assets.scene[ty][team_idx].is_some();

        tf.translation = Vec3::from_array(pr.pos);
        tf.rotation = if assets.oriented[ty] {
            // Source angles -> Hammer-space forward (Z-up), then aim the model's
            // long axis (+Y) along it.
            let pitch = pr.rotation[0].to_radians();
            let yaw = pr.rotation[1].to_radians();
            let fwd = Vec3::new(pitch.cos() * yaw.cos(), pitch.cos() * yaw.sin(), -pitch.sin());
            Quat::from_rotation_arc(Vec3::Y, fwd.normalize_or_zero())
        } else {
            Quat::IDENTITY
        };

        // Respawn the child only when the representation changes.
        let key = (pr.ty, pr.team, has_scene);
        if ms.key != Some(key) {
            if let Some(child) = ms.child.take() {
                commands.entity(child).despawn_recursive();
            }
            let child = if let Some(scene) = assets.scene[ty][team_idx].clone() {
                commands
                    .spawn(SceneBundle {
                        scene,
                        transform: Transform::from_rotation(stand_up),
                        ..default()
                    })
                    .id()
            } else {
                commands
                    .spawn(PbrBundle {
                        mesh: assets.mesh[ty].clone(),
                        material: assets.mat[ty][team_idx].clone(),
                        ..default()
                    })
                    .id()
            };
            commands.entity(slot_ent).add_child(child);
            ms.child = Some(child);
            ms.key = Some(key);
        }

        *vis = Visibility::Visible;
    }
}

/// Draw a fading polyline behind each live projectile by gathering its recent positions
/// from the demo's per-tick cache (seek-stable — reconstructed, not accumulated).
fn draw_projectile_trails(pb: Res<Playback>, demo_res: Res<DemoRes>, mut gizmos: Gizmos) {
    let cur = pb.tick as u32;
    let lo = cur.saturating_sub(TRAIL_TICKS);
    // Same rotation the world root applies to Hammer-space children.
    let root_rot = Quat::from_rotation_x(-FRAC_PI_2);

    // entity -> ordered (tick, world_pos, team) samples across the trail window.
    let mut trails: HashMap<u32, Vec<(u32, Vec3, u8)>> = HashMap::new();
    for f in &demo_res.0.frames {
        if f.tick < lo || f.tick > cur {
            continue;
        }
        for pr in &f.projectiles {
            trails
                .entry(pr.entity)
                .or_default()
                .push((f.tick, root_rot * Vec3::from_array(pr.pos), pr.team));
        }
    }

    for samples in trails.values() {
        for pair in samples.windows(2) {
            let (_, a, team) = pair[0];
            let (bt, b, _) = pair[1];
            // Fade older segments toward transparent.
            let age = cur.saturating_sub(bt) as f32 / TRAIL_TICKS as f32;
            let alpha = (1.0 - age).clamp(0.05, 1.0);
            let base = if team == 2 {
                Color::srgba(1.0, 0.4, 0.3, alpha)
            } else {
                Color::srgba(0.4, 0.6, 1.0, alpha)
            };
            gizmos.line(a, b, base);
        }
    }
}

/// Draw each live player's collision AABB as a wireframe box. The box is axis-aligned
/// in Hammer space; we rotate it by the same world-root rotation the players get, so it
/// lines up with the model. Team-colored; hidden for dead players.
fn draw_player_aabb(
    show: Res<ShowAabb>,
    pb: Res<Playback>,
    demo_res: Res<DemoRes>,
    mut gizmos: Gizmos,
) {
    if !show.0 {
        return;
    }
    let Some(frame) = demo_res.0.frame_at(pb.tick as u32) else {
        return;
    };
    // Same rotation applied to Hammer-space children of the world root.
    let root_rot = Quat::from_rotation_x(-FRAC_PI_2);
    for p in &frame.players {
        if !p.alive {
            continue;
        }
        let (mn, mx) = (p.bounds_min, p.bounds_max);
        // AABB center + size in Hammer coords (bounds are offsets from the feet origin).
        let center = Vec3::new(
            p.pos[0] + (mn[0] + mx[0]) * 0.5,
            p.pos[1] + (mn[1] + mx[1]) * 0.5,
            p.pos[2] + (mn[2] + mx[2]) * 0.5,
        );
        let size = Vec3::new(mx[0] - mn[0], mx[1] - mn[1], mx[2] - mn[2]);
        let color = if p.team == 2 {
            Color::srgb(1.0, 0.3, 0.25) // red
        } else {
            Color::srgb(0.3, 0.55, 1.0) // blue
        };
        let tf = Transform {
            translation: root_rot * center,
            rotation: root_rot,
            scale: size,
        };
        gizmos.cuboid(tf, color);
    }
}

/// Walk a spawned scene and collect every entity that carries a material handle.
fn collect_material_entities(
    root: Entity,
    children_q: &Query<&Children>,
    mat_q: &Query<&Handle<StandardMaterial>>,
    out: &mut Vec<Entity>,
) {
    if mat_q.contains(root) {
        out.push(root);
    }
    if let Ok(children) = children_q.get(root) {
        for &c in children.iter() {
            collect_material_entities(c, children_q, mat_q, out);
        }
    }
}

/// Once a player's model scene has spawned, clone its materials per-instance so we can
/// fade this player's alpha without touching other players sharing the same class model.
fn clone_player_materials(
    mut commands: Commands,
    mut mats: ResMut<Assets<StandardMaterial>>,
    children_q: Query<&Children>,
    mat_q: Query<&Handle<StandardMaterial>>,
    mut rigs: Query<&mut ModelState>,
) {
    for mut ms in &mut rigs {
        if ms.cloned || ms.child.is_none() {
            continue;
        }
        let mut ents = Vec::new();
        collect_material_entities(ms.child.unwrap(), &children_q, &mat_q, &mut ents);
        if ents.is_empty() {
            continue; // scene hasn't finished spawning yet
        }
        let mut handles = Vec::new();
        for e in ents {
            if let Ok(h) = mat_q.get(e) {
                if let Some(orig) = mats.get(h) {
                    let clone = orig.clone();
                    let nh = mats.add(clone);
                    commands.entity(e).insert(nh.clone());
                    handles.push(nh);
                }
            }
        }
        ms.mats = handles;
        ms.cloned = true;
    }
}

const DEATH_START_ALPHA: f32 = 0.5;

/// Fade dead players: start at 50% alpha and fade to 0 over ~1s (tick-based), then hide.
fn fade_dead_players(
    pb: Res<Playback>,
    demo_res: Res<DemoRes>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    mut rigs: Query<(&PlayerSlot, &mut Visibility, &mut ModelState)>,
) {
    let frame = demo_res.0.frame_at(pb.tick as u32);
    let players = frame.map(|f| f.players.as_slice()).unwrap_or(&[]);
    let fade_ticks = (1.0 / demo_res.0.interval_per_tick.max(0.001)).round().max(1.0);

    for (slot, mut vis, mut ms) in &mut rigs {
        let Some(p) = players.get(slot.0) else { continue };
        if ms.mats.is_empty() {
            continue;
        }

        let (alpha, want_blend) = if p.alive {
            (1.0, false)
        } else {
            let t = (p.death_age as f32 / fade_ticks).min(1.0);
            (DEATH_START_ALPHA * (1.0 - t), true)
        };

        if want_blend && p.death_age as f32 >= fade_ticks {
            *vis = Visibility::Hidden;
        }

        let toggle = ms.blend != want_blend;
        ms.blend = want_blend;
        for h in ms.mats.clone() {
            if let Some(m) = mats.get_mut(&h) {
                m.base_color = m.base_color.with_alpha(alpha);
                if toggle {
                    m.alpha_mode = if want_blend {
                        AlphaMode::Blend
                    } else {
                        AlphaMode::Opaque
                    };
                }
            }
        }
    }
}

fn input_controls(
    keys: Res<ButtonInput<KeyCode>>,
    mut pb: ResMut<Playback>,
    demo_res: Res<DemoRes>,
    mut show_aabb: ResMut<ShowAabb>,
    mut exit: EventWriter<AppExit>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        exit.send(AppExit::Success);
    }
    if keys.just_pressed(KeyCode::KeyB) {
        show_aabb.0 = !show_aabb.0;
    }
    if keys.just_pressed(KeyCode::Space) {
        pb.playing = !pb.playing;
    }
    let max = demo_res.0.max_tick as f32;
    let seek = |t: f32| (t).clamp(1.0, max);
    if keys.just_pressed(KeyCode::ArrowRight) {
        pb.tick = seek(pb.tick + 50.0);
    }
    if keys.just_pressed(KeyCode::ArrowLeft) {
        pb.tick = seek(pb.tick - 50.0);
    }
    if keys.just_pressed(KeyCode::Period) {
        pb.tick = seek(pb.tick + 1.0);
    }
    if keys.just_pressed(KeyCode::Comma) {
        pb.tick = seek(pb.tick - 1.0);
    }
}

// HLAE mirv_input camera bindings. Mouse-look while holding RMB (our "input mode").
fn fly_camera(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut motion: EventReader<MouseMotion>,
    mut windows: Query<&mut Window>,
    demo_res: Res<DemoRes>,
    pb: Res<Playback>,
    path: Res<Campath>,
    mut q: Query<(&mut Transform, &mut FlyCam, &mut Projection)>,
) {
    // While following the campath, the playback system owns the camera; free the
    // cursor and bail so manual controls don't fight it.
    if path.following {
        let mut window = windows.single_mut();
        window.cursor.grab_mode = CursorGrabMode::None;
        window.cursor.visible = true;
        return;
    }
    let Ok((mut tf, mut cam, mut proj)) = q.get_single_mut() else {
        return;
    };
    let dt = time.delta_seconds();
    let held = |cs: &[KeyCode]| cs.iter().any(|c| keys.pressed(*c));
    let hit = |cs: &[KeyCode]| cs.iter().any(|c| keys.just_pressed(*c));

    // Mouse look (hold RMB).
    let mut window = windows.single_mut();
    let looking = mouse.pressed(MouseButton::Right);
    window.cursor.grab_mode = if looking {
        CursorGrabMode::Locked
    } else {
        CursorGrabMode::None
    };
    window.cursor.visible = !looking;
    if looking {
        let mut d = Vec2::ZERO;
        for ev in motion.read() {
            d += ev.delta;
        }
        cam.yaw -= d.x * MOUSE_SENS;
        cam.pitch = (cam.pitch - d.y * MOUSE_SENS).clamp(-FRAC_PI_2 + 0.01, FRAC_PI_2 - 0.01);
    } else {
        motion.clear();
    }

    // Roll: Z left, X / Numpad0 / Numpad. right. Roll rate scales with fly speed (HLAE).
    let speed_factor = cam.speed / FLY_SPEED;
    if held(&[KeyCode::KeyZ]) {
        cam.roll += ROLL_RATE * speed_factor * dt;
    }
    if held(&[KeyCode::KeyX, KeyCode::Numpad0, KeyCode::NumpadDecimal]) {
        cam.roll -= ROLL_RATE * speed_factor * dt;
    }

    // Zoom (fov): in = PageUp / Numpad7, out = PageDown / Numpad1.
    if let Projection::Perspective(p) = &mut *proj {
        if held(&[KeyCode::PageUp, KeyCode::Numpad7]) {
            p.fov = (p.fov - ZOOM_RATE * dt).max(FOV_MIN);
        }
        if held(&[KeyCode::PageDown, KeyCode::Numpad1]) {
            p.fov = (p.fov + ZOOM_RATE * dt).min(FOV_MAX);
        }
    }

    // Speed: + / NumpadAdd faster, - / NumpadSubtract slower. Stepped per keypress.
    if hit(&[KeyCode::Equal, KeyCode::NumpadAdd]) {
        cam.speed = (cam.speed * SPEED_STEP).min(SPEED_MAX);
    }
    if hit(&[KeyCode::Minus, KeyCode::NumpadSubtract]) {
        cam.speed = (cam.speed / SPEED_STEP).max(SPEED_MIN);
    }

    // Reset view/speed: Home / Numpad5.
    if hit(&[KeyCode::Home, KeyCode::Numpad5]) {
        cam.pitch = 0.0;
        cam.roll = 0.0;
        cam.speed = FLY_SPEED;
        if let Projection::Perspective(p) = &mut *proj {
            p.fov = DEFAULT_FOV;
        }
    }

    // G: snap onto the current tick's players (utility, not an HLAE binding).
    if hit(&[KeyCode::KeyG]) {
        if let Some(frame) = demo_res.0.frame_at(pb.tick as u32) {
            if !frame.players.is_empty() {
                let n = frame.players.len() as f32;
                let sum = frame.players.iter().fold([0.0f32; 3], |mut a, p| {
                    a[0] += p.pos[0];
                    a[1] += p.pos[1];
                    a[2] += p.pos[2];
                    a
                });
                let look_at = hammer_to_world([sum[0] / n, sum[1] / n, sum[2] / n]);
                tf.translation = look_at + Vec3::new(0.0, 800.0, 1600.0);
                tf.look_at(look_at, Vec3::Y);
                let (y, x, _) = tf.rotation.to_euler(EulerRot::YXZ);
                cam.yaw = y;
                cam.pitch = x;
                cam.roll = 0.0;
            }
        }
    }

    tf.rotation = Quat::from_euler(EulerRot::YXZ, cam.yaw, cam.pitch, cam.roll);

    // Movement: WASD + numpad. Forward W/8, Back S/2, Left A/4, Right D/6,
    // Up R/9, Down F/3 (up/down are world-relative).
    let speed = cam.speed * dt;
    let fwd = *tf.forward();
    let right = *tf.right();
    let up = Vec3::Y;
    let mut v = Vec3::ZERO;
    if held(&[KeyCode::KeyW, KeyCode::Numpad8]) {
        v += fwd;
    }
    if held(&[KeyCode::KeyS, KeyCode::Numpad2]) {
        v -= fwd;
    }
    if held(&[KeyCode::KeyD, KeyCode::Numpad6]) {
        v += right;
    }
    if held(&[KeyCode::KeyA, KeyCode::Numpad4]) {
        v -= right;
    }
    if held(&[KeyCode::KeyR, KeyCode::Numpad9]) {
        v += up;
    }
    if held(&[KeyCode::KeyF, KeyCode::Numpad3]) {
        v -= up;
    }
    if v != Vec3::ZERO {
        tf.translation += v.normalize() * speed;
    }
}
