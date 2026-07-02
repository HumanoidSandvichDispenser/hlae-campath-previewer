//! Players: a fixed pool of rig slots, one per roster index, each showing the right
//! class/team model for the current tick. Materials are cloned per instance so dead
//! players can fade independently.

use std::collections::HashMap;
use std::f32::consts::FRAC_PI_2;

use bevy::prelude::*;

use crate::app::AppSet;
use crate::coords::{gltf_stand_up, hammer_to_world_quat};
use crate::demo::{ActiveDemo, Playback};
use crate::map::WorldRoot;

const PLAYER_POOL: usize = 32;
const DEATH_START_ALPHA: f32 = 0.5;

/// AABB wireframe toggle (B).
#[derive(Resource)]
struct ShowAabb(bool);

/// Preloaded player scenes keyed by (class, team). class 1..=9, team 2=red / 3=blue.
#[derive(Resource)]
struct PlayerModels(HashMap<(u8, u8), Handle<Scene>>);

/// Constant yaw offset applied to every player model (radians).
#[derive(Resource)]
struct PlayerYaw(f32);

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

pub(crate) struct PlayersPlugin;

impl Plugin for PlayersPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ShowAabb(true))
            .add_systems(Startup, players_setup.after(crate::map::map_setup))
            .add_systems(Update, toggle_aabb.in_set(AppSet::Input))
            .add_systems(
                Update,
                (update_players, clone_player_materials, fade_dead_players)
                    .chain()
                    .in_set(AppSet::Sync),
            )
            .add_systems(Update, draw_player_aabb.in_set(AppSet::Draw));
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

fn players_setup(mut commands: Commands, asset_server: Res<AssetServer>, root: Res<WorldRoot>) {
    let mut models = HashMap::new();
    for class in 1..=9u8 {
        let name = class_name(class).unwrap();
        for (team, tn) in [(2u8, "red"), (3u8, "blue")] {
            let h: Handle<Scene> = asset_server.load(format!("players/{name}_{tn}.glb#Scene0"));
            models.insert((class, team), h);
        }
    }
    commands.insert_resource(PlayerModels(models));
    commands.insert_resource(PlayerYaw(FRAC_PI_2)); // +90 deg: corrects observed rightward facing

    commands.entity(root.0).with_children(|parent| {
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
}

fn toggle_aabb(keys: Res<ButtonInput<KeyCode>>, mut show_aabb: ResMut<ShowAabb>) {
    if keys.just_pressed(KeyCode::KeyB) {
        show_aabb.0 = !show_aabb.0;
    }
}

fn update_players(
    mut commands: Commands,
    pb: Res<Playback>,
    demo_res: Res<ActiveDemo>,
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
    let stand_up = gltf_stand_up();

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
                ms.key = None; // unknown class (e.g. mid-join): nothing to show
            }
        }

        *vis = if ms.child.is_some() {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
    }
}

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

/// Once a rig's scene has spawned, clone its materials so fading this player's alpha
/// leaves other players on the same class model alone.
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

/// Fade dead players: start at 50% alpha and fade to 0 over ~1s (tick-based), then hide.
fn fade_dead_players(
    pb: Res<Playback>,
    demo_res: Res<ActiveDemo>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    mut rigs: Query<(&PlayerSlot, &mut Visibility, &mut ModelState)>,
) {
    let frame = demo_res.0.frame_at(pb.tick as u32);
    let players = frame.map(|f| f.players.as_slice()).unwrap_or(&[]);
    let fade_ticks = (1.0 / demo_res.0.interval_per_tick().max(0.001)).round().max(1.0);

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

/// Wireframe collision AABB per live player. Axis-aligned in Hammer space, so it gets
/// the same world-root rotation the players do.
fn draw_player_aabb(
    show: Res<ShowAabb>,
    pb: Res<Playback>,
    demo_res: Res<ActiveDemo>,
    mut gizmos: Gizmos,
) {
    if !show.0 {
        return;
    }
    let Some(frame) = demo_res.0.frame_at(pb.tick as u32) else {
        return;
    };
    // Same rotation applied to Hammer-space children of the world root.
    let root_rot = hammer_to_world_quat();
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
