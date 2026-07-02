//! Projectiles: same fixed-pool pattern as players, plus a fading trail behind each.
//! Real GLB models where we have them, primitive shapes otherwise.

use std::collections::HashMap;

use bevy::prelude::*;

use crate::app::AppSet;
use crate::coords::{gltf_stand_up, hammer_to_world_quat};
use crate::demo::{interp_fraction, lerp_angle_deg, ActiveDemo, Playback};
use crate::map::WorldRoot;

const PROJECTILE_POOL: usize = 64;
/// How many ticks of history to draw behind each live projectile as a trail.
const TRAIL_TICKS: u32 = 40;

#[derive(Component)]
struct ProjectileSlot(usize);

/// Render assets indexed by ProjectileType u8. Rocket/pipe/sticky have real GLB scenes
/// (from dribble.tf); the rest fall back to a primitive mesh. team_idx: 0 red, 1 blue.
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

/// What a slot currently shows (`(ty, team, is_scene)`), so the child only respawns
/// when that changes.
#[derive(Component, Default)]
struct ProjectileModelState {
    key: Option<(u8, u8, bool)>,
    child: Option<Entity>,
}

#[allow(dead_code)]
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

pub(crate) struct ProjectilesPlugin;

impl Plugin for ProjectilesPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, projectiles_setup.after(crate::map::map_setup))
            .add_systems(Update, update_projectiles.in_set(AppSet::Sync))
            .add_systems(Update, draw_projectile_trails.in_set(AppSet::Draw));
    }
}

fn projectiles_setup(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    root: Res<WorldRoot>,
) {
    // Primitive fallback shapes, keyed by ProjectileType u8 (0..=7): capsules for
    // rockets/arrows (oriented to travel), spheres for the grenades/flares. Real GLB
    // scenes loaded below take precedence where they exist.
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

    commands.entity(root.0).with_children(|parent| {
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
}

/// Place the pool at the current tick, hiding unused slots. Slots are children of the
/// world root, so positions stay in Hammer coords.
fn update_projectiles(
    mut commands: Commands,
    pb: Res<Playback>,
    demo_res: Res<ActiveDemo>,
    assets: Res<ProjectileAssets>,
    mut q: Query<(
        Entity,
        &ProjectileSlot,
        &mut Transform,
        &mut Visibility,
        &mut ProjectileModelState,
    )>,
) {
    let before = demo_res.0.frame_at(pb.tick as u32);
    let after = pb
        .interpolate
        .then(|| demo_res.0.frame_after(pb.tick as u32))
        .flatten();
    let projs = before.map(|f| f.projectiles.as_slice()).unwrap_or(&[]);
    let t = interp_fraction(pb.tick, before, after);
    // glTF-Y-up -> Hammer-Z-up, same correction the player models get.
    let stand_up = gltf_stand_up();

    for (slot_ent, slot, mut tf, mut vis, mut ms) in &mut q {
        let Some(pr) = projs.get(slot.0) else {
            *vis = Visibility::Hidden;
            continue;
        };
        let ty = pr.ty.min(7) as usize;
        let team_idx = if pr.team == 3 { 1 } else { 0 };
        let has_scene = assets.scene[ty][team_idx].is_some();

        let mut pos = Vec3::from_array(pr.pos);
        let mut rot = pr.rotation;
        if t > 0.0 {
            if let Some(np) = after.and_then(|a| a.projectiles.iter().find(|q| q.entity == pr.entity))
            {
                pos = pos.lerp(Vec3::from_array(np.pos), t);
                for i in 0..3 {
                    rot[i] = lerp_angle_deg(pr.rotation[i], np.rotation[i], t);
                }
            }
        }
        tf.translation = pos;
        tf.rotation = if assets.oriented[ty] {
            // Source angles -> Hammer-space forward (Z-up), then aim the model's
            // long axis (+Y) along it.
            let pitch = rot[0].to_radians();
            let yaw = rot[1].to_radians();
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

/// Fading trail behind each live projectile, rebuilt from the demo cache every frame
/// (which is what keeps it correct across seeks).
fn draw_projectile_trails(pb: Res<Playback>, demo_res: Res<ActiveDemo>, mut gizmos: Gizmos) {
    let cur = pb.tick as u32;
    let lo = cur.saturating_sub(TRAIL_TICKS);
    // Same rotation the world root applies to Hammer-space children.
    let root_rot = hammer_to_world_quat();

    // entity -> ordered (tick, world_pos, team) samples across the trail window.
    let mut trails: HashMap<u32, Vec<(u32, Vec3, u8)>> = HashMap::new();
    demo_res.0.frames_in_range(lo, cur, &mut |f| {
        for pr in &f.projectiles {
            trails
                .entry(pr.entity)
                .or_default()
                .push((f.tick, root_rot * Vec3::from_array(pr.pos), pr.team));
        }
    });

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
