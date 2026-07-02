//! Spawns the rotated world root and mounts the map GLB under it.

use bevy::prelude::*;

use crate::coords::{gltf_stand_up, world_root_transform};

/// The rotated root every Hammer-space entity (map, players, projectiles) parents under.
#[derive(Resource)]
pub(crate) struct WorldRoot(pub(crate) Entity);

/// Map GLB path from the command line.
#[derive(Resource)]
pub(crate) struct MapAssetPath(pub(crate) String);

#[derive(Component)]
struct MapRoot;

pub struct MapPlugin;

impl Plugin for MapPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, map_setup);
    }
}

pub(crate) fn map_setup(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    map_path: Res<MapAssetPath>,
) {
    let map_scene: Handle<Scene> = asset_server.load(format!("{}#Scene0", map_path.0));

    let root = commands
        .spawn((SpatialBundle::from_transform(world_root_transform()),))
        .id();
    commands.insert_resource(WorldRoot(root));

    // The map GLB is exported Y-up, so it needs +90 deg about X to cancel the world root's
    // -90 deg (players live in raw Hammer coords and want the root rotation as-is).
    let map_rot = gltf_stand_up();
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
    });
}
