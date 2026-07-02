//! HLAE campath XML + VDM export. Ported 1:1 from the web previewer's
//! `campathExport.ts`. Consumes keyframes (Source Z-up) from `super::spline`.

use bevy::math::{Quat, Vec3};

use super::spline::{CampathInterp, FovInterp, Keyframe, PositionInterp, RotationInterp};

// --------- HLAE EXPORT ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------

const RAD2DEG: f32 = 180.0 / std::f32::consts::PI;

/// Source QAngle (pitch, yaw, roll degrees) from a Z-up-world camera quaternion
/// (camera looks down local -Z, +Y up). Port of sourceAnglesFromQuaternion.
fn source_angles_from_quaternion(q: Quat) -> (f32, f32, f32) {
    let forward = q * Vec3::new(0.0, 0.0, -1.0);
    let up = q * Vec3::new(0.0, 1.0, 0.0);
    let mut left = up.cross(forward);
    if left.length_squared() > 0.0 {
        left = left.normalize();
    }
    let xy_dist = forward.x.hypot(forward.y);

    let (pitch, yaw, roll);
    if xy_dist > 0.001 {
        yaw = forward.y.atan2(forward.x);
        pitch = (-forward.z).atan2(xy_dist);
        let up_z = left.y * forward.x - left.x * forward.y;
        roll = left.z.atan2(up_z);
    } else {
        yaw = (-left.x).atan2(left.y);
        pitch = (-forward.z).atan2(xy_dist);
        roll = 0.0;
    }
    (pitch * RAD2DEG, yaw * RAD2DEG, roll * RAD2DEG)
}

fn interp_attrs(interp: &CampathInterp) -> String {
    let mut attrs: Vec<&str> = Vec::new();
    match interp.position {
        PositionInterp::Linear => attrs.push("positionInterp=\"linear\""),
        PositionInterp::Cubic => attrs.push("positionInterp=\"cubic\""),
    }
    match interp.rotation {
        RotationInterp::SLinear => attrs.push("rotationInterp=\"sLinear\""),
        RotationInterp::SCubic => attrs.push("rotationInterp=\"sCubic\""),
    }
    match interp.fov {
        FovInterp::Linear => attrs.push("fovInterp=\"linear\""),
        FovInterp::Cubic => attrs.push("fovInterp=\"cubic\""),
    }
    attrs.join(" ")
}

/// `mirv_campath` XML. `interval_per_tick` converts keyframe tick -> seconds.
pub fn to_hlae_campath_xml(
    keyframes: &[Keyframe],
    interp: &CampathInterp,
    interval_per_tick: f32,
) -> String {
    let mut sorted = keyframes.to_vec();
    sorted.sort_by_key(|k| k.tick);

    let attrs = interp_attrs(interp);
    let cam_open = if attrs.is_empty() {
        "<campath>".to_string()
    } else {
        format!("<campath {attrs}>")
    };

    let comment = "<!--Points are in Quake coordinates, meaning x=forward, y=left, z=up and rotation order is first rx, then ry and lastly rz.\n\
Rotation direction follows the right-hand grip rule.\n\
rx (roll), ry (pitch), rz(yaw) are the Euler angles in degrees.\n\
qw, qx, qy, qz are the quaternion values.\n\
When read it is sufficient that either rx, ry, rz OR qw, qx, qy, qz are present.\n\
If both are present then qw, qx, qy, qz take precedence.-->";

    let points: Vec<String> = sorted
        .iter()
        .map(|kf| {
            let q = Quat::from_xyzw(
                kf.quaternion[0],
                kf.quaternion[1],
                kf.quaternion[2],
                kf.quaternion[3],
            );
            let (pitch, yaw, roll) = source_angles_from_quaternion(q);
            let t = kf.tick as f32 * interval_per_tick;
            format!(
                "\t\t<p t=\"{:.6}\" x=\"{:.6}\" y=\"{:.6}\" z=\"{:.6}\" fov=\"{:.6}\" \
                 rx=\"{:.6}\" ry=\"{:.6}\" rz=\"{:.6}\" \
                 qw=\"{:.6}\" qx=\"{:.6}\" qy=\"{:.6}\" qz=\"{:.6}\"/>",
                t,
                kf.position[0],
                kf.position[1],
                kf.position[2],
                kf.fov,
                roll,
                pitch,
                yaw,
                q.w,
                q.x,
                q.y,
                q.z,
            )
        })
        .collect();

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{cam_open}\n\t<points>\n\t\t{comment}\n{}\n\t</points>\n</campath>\n",
        points.join("\n")
    )
}

/// Minimal VDM that loads + enables the campath at the first keyframe's tick.
pub fn to_vdm(keyframes: &[Keyframe], campath_file_name: &str) -> String {
    let start_tick = keyframes.iter().map(|k| k.tick).min().unwrap_or(0);
    let bare: String = campath_file_name
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '"')
        .collect();
    let commands = format!("mirv_campath clear; mirv_campath load {bare}; mirv_campath enabled 1");
    format!(
        "demoactions\n{{\n\t\"1\"\n\t{{\n\t\tfactory \"PlayCommands\"\n\t\tname \"Load campath\"\n\t\tstarttick \"{start_tick}\"\n\t\tcommands \"{commands}\"\n\t}}\n}}\n"
    )
}
