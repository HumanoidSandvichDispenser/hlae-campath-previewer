//! HLAE campath XML + VDM export. Ported 1:1 from the web previewer's
//! `campathExport.ts`. Consumes keyframes (Source Z-up) from `super::spline`.

use bevy::math::{Mat3, Quat, Vec3};

use super::spline::{CampathInterp, FovInterp, Keyframe, PositionInterp, RotationInterp};

/// Change of basis between HLAE's point quaternion and ours.
///
/// HLAE stores the rotation in Quake camera-local axes (+X forward, +Y left, +Z up).
/// Our keyframe quaternion is OpenGL camera-local (-Z forward, +Y up, +X right), the
/// frame `source_angles_from_quaternion` and the Bevy camera both use. This maps a
/// Quake-local basis vector to the OpenGL-local one, so `q_hlae = q_ours * basis` and
/// `q_ours = q_hlae * basis.inverse()`.
fn quake_to_opengl_basis() -> Quat {
    Quat::from_mat3(&Mat3::from_cols(
        Vec3::new(0.0, 0.0, -1.0), // Quake +X (forward) -> OpenGL -Z
        Vec3::new(-1.0, 0.0, 0.0), // Quake +Y (left)    -> OpenGL -X
        Vec3::new(0.0, 1.0, 0.0),  // Quake +Z (up)      -> OpenGL +Y
    ))
}

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

/// `mirv_campath` XML. `t` is HLAE's engine `curtime` in seconds, so each keyframe's demo
/// tick is mapped to a server tick by `demo_to_server` before converting with
/// `interval_per_tick`.
pub fn to_hlae_campath_xml(
    keyframes: &[Keyframe],
    interp: &CampathInterp,
    interval_per_tick: f32,
    demo_to_server: impl Fn(u32) -> i64,
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
            // Euler angles come from our (OpenGL-local) quaternion; the written quaternion
            // is rebased into HLAE's Quake-local convention.
            let (pitch, yaw, roll) = source_angles_from_quaternion(q);
            let qh = q * quake_to_opengl_basis();
            let t = demo_to_server(kf.tick).max(0) as f32 * interval_per_tick;
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
                qh.w,
                qh.x,
                qh.y,
                qh.z,
            )
        })
        .collect();

    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{cam_open}\n\t<points>\n\t\t{comment}\n{}\n\t</points>\n</campath>\n",
        points.join("\n")
    )
}

// --------- HLAE IMPORT ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// Parse an HLAE `mirv_campath` XML back into keyframes and interp settings. The inverse
/// of `to_hlae_campath_xml`: quaternion is the source of truth (the euler `r*` attrs are
/// ignored), `t` seconds convert to ticks via `interval_per_tick`, and position round-trips
/// verbatim. Keyframe ids are left 0 for the caller to assign. Cubic is the default when a
/// channel's interp attribute is absent.
pub fn from_hlae_campath_xml(
    xml: &str,
    interval_per_tick: f32,
    server_to_demo: impl Fn(i64) -> i64,
) -> anyhow::Result<(Vec<Keyframe>, CampathInterp)> {
    let doc = roxmltree::Document::parse(xml)?;
    let root = doc.root_element();
    if root.tag_name().name() != "campath" {
        anyhow::bail!("not a campath XML (root is <{}>)", root.tag_name().name());
    }

    let interp = CampathInterp {
        position: match root.attribute("positionInterp") {
            Some("linear") => PositionInterp::Linear,
            _ => PositionInterp::Cubic,
        },
        rotation: match root.attribute("rotationInterp") {
            Some("sLinear") => RotationInterp::SLinear,
            _ => RotationInterp::SCubic,
        },
        fov: match root.attribute("fovInterp") {
            Some("linear") => FovInterp::Linear,
            _ => FovInterp::Cubic,
        },
    };

    let per_tick = if interval_per_tick > 0.0 {
        interval_per_tick
    } else {
        0.015
    };
    let attr = |n: &roxmltree::Node, name: &str| -> anyhow::Result<f32> {
        n.attribute(name)
            .ok_or_else(|| anyhow::anyhow!("<p> missing '{name}'"))?
            .parse::<f32>()
            .map_err(|e| anyhow::anyhow!("<p> bad '{name}': {e}"))
    };

    let mut keyframes = Vec::new();
    for p in doc.descendants().filter(|n| n.has_tag_name("p")) {
        let t = attr(&p, "t")?;
        // t is engine curtime seconds -> server tick -> demo tick.
        let server_tick = (t / per_tick).round() as i64;
        let demo_tick = server_to_demo(server_tick);
        // XML quaternion is Quake-local; rebase into our OpenGL-local convention.
        let qh = Quat::from_xyzw(
            attr(&p, "qx")?,
            attr(&p, "qy")?,
            attr(&p, "qz")?,
            attr(&p, "qw")?,
        );
        let q = qh * quake_to_opengl_basis().inverse();
        keyframes.push(Keyframe {
            id: 0,
            tick: demo_tick.max(0) as u32,
            position: [attr(&p, "x")?, attr(&p, "y")?, attr(&p, "z")?],
            quaternion: [q.x, q.y, q.z, q.w],
            fov: attr(&p, "fov")?,
        });
    }
    keyframes.sort_by_key(|k| k.tick);
    Ok((keyframes, interp))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_t_uses_server_tick_and_round_trips() {
        // ~22 min of server uptime before recording, TF2 tick interval.
        let offset = 88_666_i64;
        let interval = 0.015_f32;
        let kfs = vec![
            Keyframe {
                id: 1,
                tick: 148_000,
                position: [10.0, -20.0, 30.0],
                quaternion: [0.0, 0.0, 0.0, 1.0],
                fov: 90.0,
            },
            Keyframe {
                id: 2,
                tick: 148_500,
                position: [11.0, -21.0, 31.0],
                quaternion: [0.0, 0.0, 0.0, 1.0],
                fov: 75.0,
            },
        ];
        let interp = CampathInterp::default();
        let d2s = |t: u32| t as i64 + offset;
        let s2d = |s: i64| s - offset;
        let xml = to_hlae_campath_xml(&kfs, &interp, interval, d2s);

        // t is engine curtime seconds = (demo tick + offset) * interval.
        let want_t = (148_000 + offset) as f32 * interval; // ~3549.99
        assert!(
            xml.contains(&format!("t=\"{want_t:.6}\"")),
            "xml t mismatch:\n{xml}"
        );

        // Importing with the same offset recovers the original demo ticks.
        let (back, _) = from_hlae_campath_xml(&xml, interval, s2d).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].tick, 148_000);
        assert_eq!(back[1].tick, 148_500);
        assert_eq!(back[0].fov, 90.0);
    }

    #[test]
    fn quaternion_rebased_to_hlae_convention() {
        // A real HLAE point with nonzero roll (rx=roll, ry=pitch, rz=yaw).
        let (qw, qx, qy, qz) = (0.530535_f32, 0.068643, 0.201336, -0.820539);
        let (rx, ry, rz) = (-15.812207_f32, 19.043198, -116.897751);
        let xml = format!(
            "<?xml version=\"1.0\"?><campath><points>\
             <p t=\"0\" x=\"0\" y=\"0\" z=\"0\" fov=\"90\" \
             qw=\"{qw}\" qx=\"{qx}\" qy=\"{qy}\" qz=\"{qz}\"/></points></campath>"
        );

        let (kfs, _) = from_hlae_campath_xml(&xml, 0.015, |s| s).unwrap();
        let q = Quat::from_xyzw(
            kfs[0].quaternion[0],
            kfs[0].quaternion[1],
            kfs[0].quaternion[2],
            kfs[0].quaternion[3],
        );

        // Our angle extraction on the rebased quaternion must recover HLAE's angles.
        let (pitch, yaw, roll) = source_angles_from_quaternion(q);
        let norm = |a: f32| (a + 540.0).rem_euclid(360.0) - 180.0;
        assert!(norm(roll - rx).abs() < 0.05, "roll {roll} vs {rx}");
        assert!(norm(pitch - ry).abs() < 0.05, "pitch {pitch} vs {ry}");
        assert!(norm(yaw - rz).abs() < 0.05, "yaw {yaw} vs {rz}");

        // Exporting it again reproduces the original HLAE quaternion (up to sign).
        let kf = Keyframe {
            id: 1,
            tick: 0,
            position: [0.0; 3],
            quaternion: kfs[0].quaternion,
            fov: 90.0,
        };
        let out = to_hlae_campath_xml(&[kf], &CampathInterp::default(), 0.015, |t| t as i64);
        let dot = qw.abs(); // sanity that the source value is what we expect
        assert!(dot > 0.0);
        assert!(
            out.contains(&format!("qw=\"{qw:.6}\"")) || out.contains(&format!("qw=\"{:.6}\"", -qw)),
            "exported quaternion should match HLAE's:\n{out}"
        );
    }
}
