//! Free-fly camera (HLAE-style bindings), the 16:9 viewport enforcement, and the
//! initial camera + light spawn.

use std::f32::consts::FRAC_PI_2;

use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
use bevy::render::camera::Viewport;
use bevy::window::CursorGrabMode;

use crate::app::AppSet;
use crate::campath::{campath_playback, Campath};
use crate::coords::hammer_to_world;
use crate::demo::{ActiveDemo, Playback};

const FLY_SPEED: f32 = 900.0;
const MOUSE_SENS: f32 = 0.0015;

// HLAE-style camera rates.
const ROLL_RATE: f32 = 1.2; // rad/s
const ZOOM_RATE: f32 = 0.7; // rad/s (fov)
const FOV_MIN: f32 = 0.15;
const FOV_MAX: f32 = 2.2;
pub(crate) const DEFAULT_FOV: f32 = 1.309; // ~75 deg
const SPEED_STEP: f32 = 1.5; // multiplicative per +/- keypress
const SPEED_MIN: f32 = 30.0;
const SPEED_MAX: f32 = 20000.0;

#[derive(Component, Default)]
pub(crate) struct FlyCam {
    pub(crate) yaw: f32,
    pub(crate) pitch: f32,
    pub(crate) roll: f32,
    pub(crate) speed: f32,
}

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, camera_setup)
            // campath_playback drives the camera while following; fly_camera must run
            // after it (it early-returns when following so it doesn't fight the path).
            .add_systems(
                Update,
                fly_camera.after(campath_playback).in_set(AppSet::Sync),
            )
            .add_systems(Update, enforce_16_9_viewport.in_set(AppSet::Draw));
    }
}

fn camera_setup(mut commands: Commands, demo_res: Res<ActiveDemo>) {
    // Aim the camera at the first tick's players so you don't spawn in the void.
    let look_at = demo_res
        .0
        .first_populated_frame()
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
    eprintln!("[previewer] look_at={look_at:?} cam_pos={cam_pos:?}");

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

/// Keep the camera viewport at the largest 16:9 rect that fits the window,
/// centered with letterbox/pillarbox bars filled by the clear color.
fn enforce_16_9_viewport(windows: Query<&Window>, mut cam_q: Query<&mut Camera, With<FlyCam>>) {
    let Ok(window) = windows.get_single() else {
        return;
    };
    let Ok(mut camera) = cam_q.get_single_mut() else {
        return;
    };
    let w = window.physical_width();
    let h = window.physical_height();
    if w == 0 || h == 0 {
        return;
    }
    let (vw, vh) = if w * 9 > h * 16 {
        (h * 16 / 9, h) // wider than 16:9 -> pillarbox
    } else {
        (w, w * 9 / 16) // taller than 16:9 -> letterbox
    };
    let x = (w - vw) / 2;
    let y = (h - vh) / 2;
    camera.viewport = Some(Viewport {
        physical_position: UVec2::new(x, y),
        physical_size: UVec2::new(vw, vh),
        ..default()
    });
}

// HLAE mirv_input camera bindings. Mouse-look while holding RMB (our "input mode").
fn fly_camera(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    mut motion: EventReader<MouseMotion>,
    mut windows: Query<&mut Window>,
    demo_res: Res<ActiveDemo>,
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

    // G: snap onto the current tick's players (our own utility binding).
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
