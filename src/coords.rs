//! All Z-up (Hammer/Source) <-> Y-up (glTF/Bevy) coordinate conversions.
//!
//! Demo positions are Hammer/Source coords, Z-up. The map GLB carries those same raw
//! coords, and player/projectile snapshots too. glTF/Bevy is Y-up, so we parent
//! everything under a world root rotated -90 deg about X, which makes Hammer +Z become
//! engine +Y. If the map looks tipped or mirrored, these rotations are the first knob.
//!
//! Two quats, pls do not confuse the signs:
//! -90 deg X = `hammer_to_world_quat` (world root; also trail/AABB "root_rot")
//! +90 deg X = `gltf_stand_up` (map/model assets authored Y-up, "stand_up"/"map_rot")

use bevy::prelude::*;
use std::f32::consts::FRAC_PI_2;

pub fn world_root_transform() -> Transform {
    Transform::from_rotation(hammer_to_world_quat())
}

pub fn hammer_to_world_quat() -> Quat {
    Quat::from_rotation_x(-FRAC_PI_2)
}

/// Inverse of `hammer_to_world_quat`.
pub fn world_to_hammer_quat() -> Quat {
    Quat::from_rotation_x(FRAC_PI_2)
}

pub fn hammer_to_world(p: [f32; 3]) -> Vec3 {
    hammer_to_world_quat() * Vec3::new(p[0], p[1], p[2])
}

/// glTF assets are Y-up, so under the -90 deg world root they end up on their side.
/// Apply this to stand them back up.
pub fn gltf_stand_up() -> Quat {
    Quat::from_rotation_x(FRAC_PI_2)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compare orientations by how far a composed rotation moves the basis vectors.
    // Quat::angle_between hits acos(dot>1)->NaN for near-identical quats; this doesn't.
    fn is_identity(q: Quat) -> bool {
        [Vec3::X, Vec3::Y, Vec3::Z]
            .iter()
            .all(|&v| (q * v - v).length() < 1e-5)
    }

    #[test]
    fn hammer_world_round_trips() {
        assert!(is_identity(world_to_hammer_quat() * hammer_to_world_quat()));
    }

    #[test]
    fn hammer_z_becomes_world_y() {
        // Hammer +Z (up) should map to engine +Y (up).
        let up = hammer_to_world([0.0, 0.0, 1.0]);
        assert!((up - Vec3::Y).length() < 1e-5, "got {up:?}");
    }

    #[test]
    fn stand_up_cancels_world_root() {
        // The +90 deg stand-up on a Y-up asset cancels the -90 deg world root back to identity.
        assert!(is_identity(gltf_stand_up() * hammer_to_world_quat()));
    }
}
