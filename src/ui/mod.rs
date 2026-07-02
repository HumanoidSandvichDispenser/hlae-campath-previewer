//! egui overlay: a floating timeline bar along the bottom (scrubber, play/pause, seek
//! buttons), a right-side campath panel (keyframe list + export), and a floating window
//! that edits the selected keyframe. The 3D view keeps rendering underneath.

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPlugin};

use crate::app::AppSet;
use crate::camera::FlyCam;
use crate::campath::spline::{FovInterp, PositionInterp, RotationInterp};
use crate::campath::{capture_pose, export_campath, Campath};
use crate::demo::{ActiveDemo, DemoPath, Playback, SeekTo};

pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin)
            .init_resource::<UiState>()
            .add_systems(Update, (timeline_bar, campath_panel).in_set(AppSet::Draw));
    }
}

#[derive(Resource)]
struct UiState {
    show_campath: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self { show_campath: true }
    }
}

fn timeline_bar(
    mut contexts: EguiContexts,
    mut pb: ResMut<Playback>,
    demo_res: Res<ActiveDemo>,
    mut seek: EventWriter<SeekTo>,
    // Some(buf) while the tick readout is being typed into.
    mut tick_edit: Local<Option<String>>,
    mut edit_opened: Local<bool>,
) {
    let max = demo_res.0.max_tick().max(1) as f32;
    let ctx = contexts.ctx_mut();

    // Floats over the viewport rather than docking, so it doesn't shrink the 3D view.
    egui::Area::new(egui::Id::new("timeline"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -12.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button(if pb.playing { "pause" } else { "play" }).clicked() {
                        pb.playing = !pb.playing;
                    }
                    if ui.button("<< 50").clicked() {
                        seek.send(SeekTo(pb.tick - 50.0));
                    }
                    if ui.button("< 1").clicked() {
                        seek.send(SeekTo(pb.tick - 1.0));
                    }
                    if ui.button("1 >").clicked() {
                        seek.send(SeekTo(pb.tick + 1.0));
                    }
                    if ui.button("50 >>").clicked() {
                        seek.send(SeekTo(pb.tick + 50.0));
                    }

                    // Dragging the slider seeks; the value tracks pb.tick otherwise.
                    let mut tick = pb.tick;
                    ui.spacing_mut().slider_width = 320.0;
                    let slider = ui.add(
                        egui::Slider::new(&mut tick, 1.0..=max)
                            .integer()
                            .show_value(false),
                    );
                    if slider.changed() {
                        seek.send(SeekTo(tick));
                    }

                    // Click the tick to type an exact value; Enter or click-away commits.
                    match tick_edit.as_mut() {
                        None => {
                            if ui
                                .button(format!("{}", pb.tick as u32))
                                .on_hover_text("Click to set tick")
                                .clicked()
                            {
                                *tick_edit = Some((pb.tick as u32).to_string());
                                *edit_opened = true;
                            }
                        }
                        Some(buf) => {
                            let resp = ui.add(
                                egui::TextEdit::singleline(buf).desired_width(64.0),
                            );
                            if *edit_opened {
                                resp.request_focus();
                                *edit_opened = false;
                            }
                            if resp.lost_focus() {
                                if let Ok(v) = buf.trim().parse::<f32>() {
                                    seek.send(SeekTo(v));
                                }
                                *tick_edit = None;
                            }
                        }
                    }
                    ui.monospace(format!("/ {}", max as u32));
                });
            });
        });
}

#[allow(clippy::too_many_arguments)]
fn campath_panel(
    mut contexts: EguiContexts,
    mut path: ResMut<Campath>,
    mut ui_state: ResMut<UiState>,
    keys: Res<ButtonInput<KeyCode>>,
    pb: Res<Playback>,
    demo_res: Res<ActiveDemo>,
    demo_path: Res<DemoPath>,
    mut seek: EventWriter<SeekTo>,
    cam_q: Query<(&Transform, &Projection), With<FlyCam>>,
) {
    if keys.just_pressed(KeyCode::F2) {
        ui_state.show_campath = !ui_state.show_campath;
    }
    if !ui_state.show_campath {
        return;
    }

    let interval = demo_res.0.interval_per_tick();
    let cur_tick = pb.tick.round() as u32;
    let ctx = contexts.ctx_mut();

    egui::Window::new("Campath")
        .resizable(true)
        .default_width(280.0)
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("{} keyframes", path.keyframes.len()));
                if !path.keyframes.is_empty() && ui.button("Clear all").clicked() {
                    path.clear();
                }
            });

            ui.separator();

            // Add-at-tick + drive toggle.
            ui.horizontal(|ui| {
                if ui
                    .button(format!("+ Add keyframe (tick {cur_tick})"))
                    .clicked()
                {
                    if let Ok((tf, proj)) = cam_q.get_single() {
                        let (pos, quat, fov) = capture_pose(tf, proj);
                        let id = path.set_at_tick(cur_tick, pos, quat, fov);
                        path.selected = Some(id);
                    }
                }
                ui.checkbox(&mut path.following, "Drive");
            });

            if path.following && path.keyframes.len() < 2 {
                ui.colored_label(
                    egui::Color32::from_rgb(230, 200, 120),
                    "Add at least 2 keyframes for the path to drive the camera.",
                );
            }

            ui.separator();
            interp_controls(ui, &mut path);
            ui.separator();

            keyframe_list(ui, &mut path, &mut seek, interval, cur_tick, &cam_q);

            if path.keyframes.len() >= 2 {
                ui.separator();
                if ui.button("Export .xml + .vdm").clicked() {
                    export_campath(&path, &demo_path.0, interval);
                }
            }

            // Editor for the selected keyframe, in the same window below the list.
            if let Some(id) = path.selected {
                keyframe_detail(ui, &mut path, id, &cam_q);
            }
        });
}

fn interp_controls(ui: &mut egui::Ui, path: &mut Campath) {
    let before = path.interp;
    egui::Grid::new("interp").num_columns(2).show(ui, |ui| {
        ui.label("Position");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut path.interp.position, PositionInterp::Cubic, "Cubic");
            ui.selectable_value(&mut path.interp.position, PositionInterp::Linear, "Linear");
        });
        ui.end_row();

        ui.label("Rotation");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut path.interp.rotation, RotationInterp::SCubic, "Cubic");
            ui.selectable_value(&mut path.interp.rotation, RotationInterp::SLinear, "Linear");
        });
        ui.end_row();

        ui.label("FOV");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut path.interp.fov, FovInterp::Cubic, "Cubic");
            ui.selectable_value(&mut path.interp.fov, FovInterp::Linear, "Linear");
        });
        ui.end_row();
    });

    let cubic = path.interp.position == PositionInterp::Cubic
        || path.interp.rotation == RotationInterp::SCubic
        || path.interp.fov == FovInterp::Cubic;
    let n = path.keyframes.len();
    if cubic && n > 0 && n < 4 {
        ui.colored_label(
            egui::Color32::from_rgb(230, 200, 120),
            "Cubic needs 4+ keyframes; using linear until then.",
        );
    }
    // Interp changes need a recompile.
    if path.interp != before {
        path.dirty = true;
    }
}

fn keyframe_list(
    ui: &mut egui::Ui,
    path: &mut Campath,
    seek: &mut EventWriter<SeekTo>,
    interval: f32,
    cur_tick: u32,
    cam_q: &Query<(&Transform, &Projection), With<FlyCam>>,
) {
    if path.keyframes.is_empty() {
        ui.weak("No keyframes yet. Frame a shot, then add one at the current tick.");
        return;
    }

    // Snapshot so we can mutate `path` inside the row loop without aliasing.
    let rows: Vec<(u64, u32, f32)> = path
        .keyframes
        .iter()
        .map(|k| (k.id, k.tick, k.fov))
        .collect();

    egui::ScrollArea::vertical()
        .max_height(240.0)
        .show(ui, |ui| {
            for (id, tick, mut fov) in rows {
                let selected = path.selected == Some(id);
                ui.horizontal(|ui| {
                    let label = format!("{}  tick {}", format_time(tick, interval), tick);
                    if ui.selectable_label(selected, label).clicked() {
                        path.selected = Some(id);
                        seek.send(SeekTo(tick as f32));
                    }
                    if tick == cur_tick && !selected {
                        ui.weak("<");
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("delete").clicked() {
                            path.delete(id);
                        }
                        if ui
                            .button("recapture")
                            .on_hover_text("Overwrite from the current camera")
                            .clicked()
                        {
                            if let Ok((tf, proj)) = cam_q.get_single() {
                                let (pos, quat, f) = capture_pose(tf, proj);
                                path.with_keyframe(id, |k| {
                                    k.position = pos;
                                    k.quaternion = quat;
                                    k.fov = f;
                                });
                            }
                        }
                        if ui
                            .add(egui::DragValue::new(&mut fov).range(1.0..=179.0).suffix(" fov"))
                            .changed()
                        {
                            path.with_keyframe(id, |k| k.fov = fov);
                        }
                    });
                });
            }
        });
}

fn keyframe_detail(
    ui: &mut egui::Ui,
    path: &mut Campath,
    id: u64,
    cam_q: &Query<(&Transform, &Projection), With<FlyCam>>,
) {
    let Some(kf) = path.keyframes.iter().find(|k| k.id == id).copied() else {
        return;
    };

    ui.separator();
    ui.horizontal(|ui| {
        ui.strong(format!("Keyframe tick {}", kf.tick));
        if ui.button("Deselect").clicked() {
            path.selected = None;
        }
        if ui.button("Recapture").clicked() {
            if let Ok((tf, proj)) = cam_q.get_single() {
                let (pos, quat, fov) = capture_pose(tf, proj);
                path.with_keyframe(id, |k| {
                    k.position = pos;
                    k.quaternion = quat;
                    k.fov = fov;
                });
            }
        }
    });

    // Tick / retime.
    let mut tick = kf.tick as i32;
    ui.horizontal(|ui| {
        ui.label("Tick");
        if ui.add(egui::DragValue::new(&mut tick).range(0..=i32::MAX)).changed()
            && !path.retime(id, tick.max(0) as u32)
        {
            ui.colored_label(egui::Color32::from_rgb(230, 200, 120), "tick taken");
        }
    });

    // Position (Source Z-up).
    let mut pos = kf.position;
    ui.label("Position");
    ui.horizontal(|ui| {
        let mut changed = false;
        for v in pos.iter_mut() {
            changed |= ui.add(egui::DragValue::new(v).speed(1.0)).changed();
        }
        if changed {
            path.with_keyframe(id, |k| k.position = pos);
        }
    });

    // Rotation as pitch/yaw/roll degrees in Source space (yaw about Z, pitch about Y,
    // roll about X), round-tripped through the keyframe quaternion.
    let (mut pitch, mut yaw, mut roll) = euler_from_quat(kf.quaternion);
    ui.label("Rotation (pitch / yaw / roll deg)");
    ui.horizontal(|ui| {
        let mut changed = false;
        changed |= ui.add(egui::DragValue::new(&mut pitch).speed(0.5)).changed();
        changed |= ui.add(egui::DragValue::new(&mut yaw).speed(0.5)).changed();
        changed |= ui.add(egui::DragValue::new(&mut roll).speed(0.5)).changed();
        if changed {
            let quat = quat_from_euler(pitch, yaw, roll);
            path.with_keyframe(id, |k| k.quaternion = quat);
        }
    });

    // FOV.
    let mut fov = kf.fov;
    ui.horizontal(|ui| {
        ui.label("FOV");
        if ui.add(egui::DragValue::new(&mut fov).range(1.0..=179.0)).changed() {
            path.with_keyframe(id, |k| k.fov = fov);
        }
    });
}

fn format_time(tick: u32, interval: f32) -> String {
    let secs = tick as f32 * interval;
    let m = (secs / 60.0) as u32;
    let s = secs % 60.0;
    format!("{m}:{s:04.1}")
}

fn euler_from_quat(q: [f32; 4]) -> (f32, f32, f32) {
    let quat = Quat::from_xyzw(q[0], q[1], q[2], q[3]);
    let (yaw, pitch, roll) = quat.to_euler(EulerRot::ZYX);
    (pitch.to_degrees(), yaw.to_degrees(), roll.to_degrees())
}

fn quat_from_euler(pitch: f32, yaw: f32, roll: f32) -> [f32; 4] {
    let q = Quat::from_euler(
        EulerRot::ZYX,
        yaw.to_radians(),
        pitch.to_radians(),
        roll.to_radians(),
    );
    [q.x, q.y, q.z, q.w]
}
