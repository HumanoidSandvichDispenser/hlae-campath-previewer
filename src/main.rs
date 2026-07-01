//! Native previewer spike: parse a TF2 demo, render the map GLB + players at a tick,
//! free-fly camera, scrub the timeline. Players are team-colored spheres — enough to
//! verify positions line up with the map. GLB player models are a known-easy next step.
//!
//! Usage: previewer-native <demo.dem> <map.glb>
//!   assets are loaded relative to the `assets/` dir next to the binary.
//!
//! Controls: RMB hold = mouselook + WASD/QE fly · Space = play/pause · ←/→ seek 50 ·
//!           ,/. step 1 tick · Shift = fly faster · Esc = quit

mod demo;

use std::f32::consts::FRAC_PI_2;

use std::collections::HashMap;

use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
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

/// Independently-rotatable parent for the map mesh, so calibration can align the map
/// to the players without moving the players.
#[derive(Component)]
struct MapRoot;

#[derive(Resource)]
struct MapRot(Quat);

/// Map handedness. The demo's team labels are authoritative and our player transform is
/// a pure rotation, so teams are correct *unless* the map GLB was mirror-converted.
/// Toggle with M to test against a known-correct render (dribble.tf).
#[derive(Resource)]
struct MapMirror(bool);

/// Constant yaw offset applied to every player model (radians). Calibrate with [ and ].
#[derive(Resource)]
struct PlayerYaw(f32);

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
    let map_asset = args.next().unwrap_or_else(|| "map.glb".into());

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
        .insert_resource(MapAssetPath(map_asset))
        .insert_resource(DemoRes(data))
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                input_controls,
                advance_playback,
                update_players,
                draw_player_aabb,
                clone_player_materials,
                fade_dead_players,
                fly_camera,
                report_fps,
                calibrate_map,
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

    // Everything that lives in Hammer coords goes under this rotated root.
    let root = commands.spawn((SpatialBundle::from_transform(world_root_transform()),)).id();

    // Calibrated on artix: +90° about X cancels the world root's -90° for the map
    // (the map GLB is already Y-up; only the players needed the Hammer->Y-up rotation).
    let map_rot = Quat::from_rotation_x(FRAC_PI_2);
    commands.insert_resource(MapRot(map_rot));
    commands.insert_resource(MapMirror(false));

    commands.entity(root).with_children(|parent| {
        // Map lives under its own rotatable node (calibrate with U/J/I/K/O/L, M mirror).
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

/// Live map-orientation calibration: rotate the map in 90° steps to find the transform
/// that lines it up with the players (which are already correct). U/J = ±X, I/K = ±Y,
/// O/L = ±Z. Prints the resulting euler + quaternion so the final value can be baked in.
fn calibrate_map(
    keys: Res<ButtonInput<KeyCode>>,
    mut rot: ResMut<MapRot>,
    mut mirror: ResMut<MapMirror>,
    mut player_yaw: ResMut<PlayerYaw>,
    mut q: Query<&mut Transform, With<MapRoot>>,
) {
    // Player yaw offset: [ / ] rotate every model ±90°.
    if keys.just_pressed(KeyCode::BracketLeft) {
        player_yaw.0 -= FRAC_PI_2;
        eprintln!("[spike] player yaw offset: {:.0}°", player_yaw.0.to_degrees());
    }
    if keys.just_pressed(KeyCode::BracketRight) {
        player_yaw.0 += FRAC_PI_2;
        eprintln!("[spike] player yaw offset: {:.0}°", player_yaw.0.to_degrees());
    }

    let step = FRAC_PI_2;
    let mut changed = true;
    if keys.just_pressed(KeyCode::KeyU) {
        rot.0 = (Quat::from_rotation_x(step) * rot.0).normalize();
    } else if keys.just_pressed(KeyCode::KeyJ) {
        rot.0 = (Quat::from_rotation_x(-step) * rot.0).normalize();
    } else if keys.just_pressed(KeyCode::KeyI) {
        rot.0 = (Quat::from_rotation_y(step) * rot.0).normalize();
    } else if keys.just_pressed(KeyCode::KeyK) {
        rot.0 = (Quat::from_rotation_y(-step) * rot.0).normalize();
    } else if keys.just_pressed(KeyCode::KeyO) {
        rot.0 = (Quat::from_rotation_z(step) * rot.0).normalize();
    } else if keys.just_pressed(KeyCode::KeyL) {
        rot.0 = (Quat::from_rotation_z(-step) * rot.0).normalize();
    } else if keys.just_pressed(KeyCode::KeyM) {
        mirror.0 = !mirror.0;
    } else {
        changed = false;
    }
    if !changed {
        return;
    }

    // Mirror via a negative X scale (flips handedness across the YZ plane).
    let scale = Vec3::new(if mirror.0 { -1.0 } else { 1.0 }, 1.0, 1.0);
    if let Ok(mut tf) = q.get_single_mut() {
        *tf = Transform::from_rotation(rot.0).with_scale(scale);
    }
    let (ry, rx, rz) = rot.0.to_euler(EulerRot::YXZ);
    eprintln!(
        "[spike] map rot: euler(x={:.0}° y={:.0}° z={:.0}°) mirror={} quat={:?}",
        rx.to_degrees(),
        ry.to_degrees(),
        rz.to_degrees(),
        mirror.0,
        rot.0
    );
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

/// Hammer coords -> engine world (matches the -90°-about-X root applied to children).
fn hammer_to_world(p: [f32; 3]) -> Vec3 {
    let q = Quat::from_rotation_x(-FRAC_PI_2);
    q * Vec3::new(p[0], p[1], p[2])
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
    mut q: Query<(&mut Transform, &mut FlyCam, &mut Projection)>,
) {
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
