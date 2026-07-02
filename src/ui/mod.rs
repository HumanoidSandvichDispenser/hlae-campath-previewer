//! egui overlay: a floating timeline bar along the bottom (scrubber, play/pause, seek
//! buttons), a right-side campath panel (keyframe list + export), and a floating window
//! that edits the selected keyframe. The 3D view keeps rendering underneath.

use bevy::prelude::*;
use bevy_egui::{egui, EguiContexts, EguiPlugin};

use crate::app::AppSet;
use crate::camera::FlyCam;
use crate::campath::spline::{FovInterp, PositionInterp, RotationInterp};
use crate::campath::{capture_pose, export_vdm_to, export_xml_to, import_campath, Campath};
use crate::coords::hammer_to_world_quat;
use crate::demo::{ActiveDemo, DemoPath, Playback, SeekTo};

pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin)
            .init_resource::<UiState>()
            .init_resource::<ViewOptions>()
            .add_systems(
                Update,
                (
                    timeline_bar,
                    campath_panel,
                    options_panel,
                    composition_overlay,
                    player_name_overlay,
                )
                    .in_set(AppSet::Draw),
            );
    }
}

#[derive(Resource)]
struct UiState {
    show_campath: bool,
    show_options: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            show_campath: true,
            show_options: true,
        }
    }
}

/// Composition guide drawn over the 16:9 framed view.
#[derive(Default, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Composition {
    #[default]
    None,
    Thirds,
    /// Phi grid: divisions at 0.382 / 0.618.
    Golden,
    /// Fibonacci / golden spiral (orientation set by `spiral_rot`).
    Spiral,
    /// Corner-to-corner diagonals.
    Diagonal,
    /// Dead-center crosshair.
    Center,
}

/// Overlay/gizmo toggles shared between the UI and the renderers. The view is already
/// locked to 16:9 by the camera viewport, so `aspect` is a further cinematic crop drawn
/// inside that frame, not a first letterbox.
#[derive(Resource)]
pub(crate) struct ViewOptions {
    /// Player collision AABB wireframes (also toggled with B).
    pub(crate) show_aabb: bool,
    /// The 3D campath polyline + keyframe frustums.
    pub(crate) show_campath: bool,
    /// Floating name tags above each live player.
    pub(crate) show_names: bool,
    pub(crate) composition: Composition,
    /// Golden-spiral orientation, 0..3 (the four corner flips).
    pub(crate) spiral_rot: u8,
    /// Draw the phi grid under the golden spiral.
    pub(crate) spiral_grid: bool,
    /// Preview letterbox to a narrower ratio (2.39 etc.), or `None` for the full 16:9.
    /// Preview only: it does not change FOV or the exported campath.
    pub(crate) aspect: Option<f32>,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            show_aabb: true,
            show_campath: true,
            show_names: true,
            composition: Composition::None,
            spiral_rot: 0,
            spiral_grid: false,
            aspect: None,
        }
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

                ui.horizontal(|ui| {
                    ui.label("speed");
                    for preset in [0.25, 0.5, 1.0, 2.0, 4.0] {
                        let on = (pb.speed - preset).abs() < 1e-3;
                        if ui.selectable_label(on, format!("{preset}x")).clicked() {
                            pb.speed = preset;
                        }
                    }
                    ui.add(
                        egui::DragValue::new(&mut pb.speed)
                            .speed(0.01)
                            .range(0.05..=16.0)
                            .suffix("x"),
                    );
                    ui.separator();
                    ui.checkbox(&mut pb.interpolate, "smooth")
                        .on_hover_text("Interpolate positions between ticks");
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

            ui.separator();
            let stem = std::path::Path::new(&demo_path.0)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("campath")
                .to_string();
            ui.horizontal(|ui| {
                if ui.button("Import XML").clicked() {
                    if let Some(file) = rfd::FileDialog::new()
                        .add_filter("campath xml", &["xml"])
                        .pick_file()
                    {
                        match import_campath(&mut path, &file, demo_res.0.as_ref()) {
                            Ok(n) => eprintln!("[campath] imported {n} keyframes from {}", file.display()),
                            Err(e) => eprintln!("[campath] import failed: {e}"),
                        }
                    }
                }
                ui.add_enabled_ui(path.keyframes.len() >= 2, |ui| {
                    if ui.button("Export XML").clicked() {
                        if let Some(file) = rfd::FileDialog::new()
                            .add_filter("campath xml", &["xml"])
                            .set_file_name(format!("{stem}_campath.xml"))
                            .save_file()
                        {
                            match export_xml_to(&path, &file, demo_res.0.as_ref()) {
                                Ok(_) => eprintln!("[campath] wrote {}", file.display()),
                                Err(e) => eprintln!("[campath] export failed: {e}"),
                            }
                        }
                    }
                    if ui.button("Export VDM").clicked() {
                        if let Some(file) = rfd::FileDialog::new()
                            .add_filter("vdm", &["vdm"])
                            .set_file_name(format!("{stem}.vdm"))
                            .save_file()
                        {
                            // The VDM loads a campath file named after the VDM's own stem.
                            let xml_name = file
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .map(|s| format!("{s}_campath.xml"))
                                .unwrap_or_else(|| format!("{stem}_campath.xml"));
                            match export_vdm_to(&path, &file, &xml_name) {
                                Ok(_) => eprintln!("[campath] wrote {} (loads {xml_name})", file.display()),
                                Err(e) => eprintln!("[campath] vdm export failed: {e}"),
                            }
                        }
                    }
                });
            });

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

fn options_panel(
    mut contexts: EguiContexts,
    keys: Res<ButtonInput<KeyCode>>,
    mut ui_state: ResMut<UiState>,
    mut opts: ResMut<ViewOptions>,
) {
    if keys.just_pressed(KeyCode::F3) {
        ui_state.show_options = !ui_state.show_options;
    }
    if !ui_state.show_options {
        return;
    }
    let ctx = contexts.ctx_mut();

    egui::Window::new("View options")
        .resizable(false)
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 12.0))
        .show(ctx, |ui| {
            ui.checkbox(&mut opts.show_aabb, "Player boxes (B)");
            ui.checkbox(&mut opts.show_names, "Player names");
            ui.checkbox(&mut opts.show_campath, "Campath path");

            ui.separator();
            ui.label("Composition guide");
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut opts.composition, Composition::None, "Off");
                ui.selectable_value(&mut opts.composition, Composition::Thirds, "Thirds");
                ui.selectable_value(&mut opts.composition, Composition::Golden, "Golden");
                ui.selectable_value(&mut opts.composition, Composition::Spiral, "Spiral");
                ui.selectable_value(&mut opts.composition, Composition::Diagonal, "Diagonal");
                ui.selectable_value(&mut opts.composition, Composition::Center, "Center");
            });
            if opts.composition == Composition::Spiral {
                ui.horizontal(|ui| {
                    if ui.button("Rotate spiral").clicked() {
                        opts.spiral_rot = (opts.spiral_rot + 1) % 4;
                    }
                    ui.checkbox(&mut opts.spiral_grid, "with grid");
                });
            }

            ui.separator();
            ui.label("Aspect mask")
                .on_hover_text("Preview letterbox inside the 16:9 view. Does not change FOV or export.");
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut opts.aspect, None, "16:9");
                ui.selectable_value(&mut opts.aspect, Some(1.85), "1.85");
                ui.selectable_value(&mut opts.aspect, Some(2.0), "2.00");
                ui.selectable_value(&mut opts.aspect, Some(2.39), "2.39");
            });
        });
}

/// Draw the selected composition guide and any aspect mask over the framed view. The
/// camera already crops to 16:9, so the guide aligns to that rect, not the whole window.
fn composition_overlay(mut contexts: EguiContexts, opts: Res<ViewOptions>) {
    if opts.composition == Composition::None && opts.aspect.is_none() {
        return;
    }
    let ctx = contexts.ctx_mut();
    let screen = ctx.screen_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("composition"),
    ));

    // Largest 16:9 rect inside the window, matching enforce_16_9_viewport.
    let mut frame = fit_aspect(screen, 16.0 / 9.0);
    // A narrower ratio letterboxes inside the 16:9 frame; paint the bars over it.
    if let Some(ar) = opts.aspect {
        let inner = fit_aspect(frame, ar);
        let bar = egui::Color32::from_black_alpha(235);
        for r in bars_around(frame, inner) {
            painter.rect_filled(r, 0.0, bar);
        }
        frame = inner;
    }

    let stroke = egui::Stroke::new(1.0, egui::Color32::from_white_alpha(110));
    let (l, r, t, b) = (frame.left(), frame.right(), frame.top(), frame.bottom());
    let (w, h) = (frame.width(), frame.height());
    let vline = |x: f32, p: &egui::Painter| {
        p.line_segment([egui::pos2(x, t), egui::pos2(x, b)], stroke);
    };
    let hline = |y: f32, p: &egui::Painter| {
        p.line_segment([egui::pos2(l, y), egui::pos2(r, y)], stroke);
    };

    match opts.composition {
        Composition::None => {}
        Composition::Thirds => {
            for i in 1..3 {
                vline(l + w * i as f32 / 3.0, &painter);
                hline(t + h * i as f32 / 3.0, &painter);
            }
        }
        Composition::Golden => {
            for f in [0.382, 0.618] {
                vline(l + w * f, &painter);
                hline(t + h * f, &painter);
            }
        }
        Composition::Spiral => {
            if opts.spiral_grid {
                for f in [0.382, 0.618] {
                    vline(l + w * f, &painter);
                    hline(t + h * f, &painter);
                }
            }
            // Points are normalized [0,1]^2; stretch them to fill the frame.
            let pts: Vec<egui::Pos2> = golden_spiral_norm(opts.spiral_rot)
                .into_iter()
                .map(|p| egui::pos2(l + p.x * w, t + p.y * h))
                .collect();
            painter.add(egui::Shape::line(pts, stroke));
        }
        Composition::Diagonal => {
            painter.line_segment([frame.left_top(), frame.right_bottom()], stroke);
            painter.line_segment([frame.right_top(), frame.left_bottom()], stroke);
        }
        Composition::Center => {
            let c = frame.center();
            vline(c.x, &painter);
            hline(c.y, &painter);
        }
    }
}

/// Golden-spiral polyline in normalized `[0,1]^2`, ready to stretch onto any frame.
///
/// Built the classic way: start from a golden rectangle (phi:1), peel off the largest
/// square each step, and draw the quarter-circle arc inside it. Successive squares shrink
/// by 1/phi and rotate 90 degrees, so the arcs chain into the spiral. The construction is
/// self-similar only in a true golden rectangle, so we build there and normalize; the
/// caller's stretch to a 16:9 (or masked) frame matches how editors show this overlay.
/// `rot` (0..3) flips the result into each corner.
fn golden_spiral_norm(rot: u8) -> Vec<egui::Pos2> {
    const PHI: f32 = 1.618_034;
    const ARC_SEGS: usize = 24;
    const STEPS: usize = 13;

    let (mut x0, mut y0, mut x1, mut y1) = (0.0f32, 0.0, PHI, 1.0);
    let mut pts: Vec<egui::Pos2> = Vec::with_capacity(STEPS * (ARC_SEGS + 1));

    for i in 0..STEPS {
        // Per step: the square's pivot corner, the arc's start/end, and the leftover rect.
        let (pivot, start, end, next) = match i % 4 {
            0 => {
                let s = y1 - y0; // square on the left
                ((x0 + s, y1), (x0, y1), (x0 + s, y0), (x0 + s, y0, x1, y1))
            }
            1 => {
                let s = x1 - x0; // square on top
                ((x0, y0 + s), (x0, y0), (x1, y0 + s), (x0, y0 + s, x1, y1))
            }
            2 => {
                let s = y1 - y0; // square on the right
                ((x1 - s, y0), (x1, y0), (x1 - s, y1), (x0, y0, x1 - s, y1))
            }
            _ => {
                let s = x1 - x0; // square on the bottom
                ((x1, y1 - s), (x1, y1), (x0, y1 - s), (x0, y0, x1, y1 - s))
            }
        };

        let r = ((start.0 - pivot.0).powi(2) + (start.1 - pivot.1).powi(2)).sqrt();
        let a0 = (start.1 - pivot.1).atan2(start.0 - pivot.0);
        let a1 = (end.1 - pivot.1).atan2(end.0 - pivot.0);
        // Sweep the short way; the two corners are always a quarter turn apart.
        let mut d = a1 - a0;
        if d > std::f32::consts::PI {
            d -= std::f32::consts::TAU;
        } else if d < -std::f32::consts::PI {
            d += std::f32::consts::TAU;
        }
        for k in 0..=ARC_SEGS {
            let a = a0 + d * (k as f32 / ARC_SEGS as f32);
            pts.push(egui::pos2(pivot.0 + r * a.cos(), pivot.1 + r * a.sin()));
        }
        (x0, y0, x1, y1) = next;
    }

    let flip_x = rot == 1 || rot == 3;
    let flip_y = rot == 2 || rot == 3;
    for p in pts.iter_mut() {
        p.x /= PHI; // [0, phi] -> [0, 1]
        if flip_x {
            p.x = 1.0 - p.x;
        }
        if flip_y {
            p.y = 1.0 - p.y;
        }
    }
    pts
}

/// Float each live player's name above their head, projected from the demo tick through
/// the camera. Players sit in Hammer space under the rotated world root, so a name's
/// world point is that rotation applied to the head position.
fn player_name_overlay(
    mut contexts: EguiContexts,
    opts: Res<ViewOptions>,
    pb: Res<Playback>,
    demo_res: Res<ActiveDemo>,
    cam_q: Query<(&Camera, &GlobalTransform), With<FlyCam>>,
) {
    if !opts.show_names {
        return;
    }
    let Some(frame) = demo_res.0.frame_at(pb.tick as u32) else {
        return;
    };
    let Ok((camera, cam_tf)) = cam_q.get_single() else {
        return;
    };
    let Some(vp) = camera.logical_viewport_rect() else {
        return;
    };
    let root_rot = hammer_to_world_quat();

    let ctx = contexts.ctx_mut();
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("player_names"),
    ));
    let font = egui::FontId::proportional(14.0);

    for p in &frame.players {
        if !p.alive {
            continue;
        }
        let Some(name) = demo_res.0.player_name(p.entity) else {
            continue;
        };
        // A little above the crown so the tag clears the model.
        let head = Vec3::new(p.pos[0], p.pos[1], p.pos[2] + p.bounds_max[2] + 12.0);
        let Some(view) = camera.world_to_viewport(cam_tf, root_rot * head) else {
            continue;
        };
        // world_to_viewport is viewport-relative; shift into window/egui space.
        let pos = egui::pos2(vp.min.x + view.x, vp.min.y + view.y);

        let color = if p.team == 2 {
            egui::Color32::from_rgb(255, 130, 120)
        } else {
            egui::Color32::from_rgb(130, 175, 255)
        };
        painter.text(
            pos + egui::vec2(1.0, 1.0),
            egui::Align2::CENTER_BOTTOM,
            name,
            font.clone(),
            egui::Color32::from_black_alpha(190),
        );
        painter.text(pos, egui::Align2::CENTER_BOTTOM, name, font.clone(), color);
    }
}

/// Largest rect of aspect `ar` (width/height) centered inside `outer`.
fn fit_aspect(outer: egui::Rect, ar: f32) -> egui::Rect {
    let (ow, oh) = (outer.width(), outer.height());
    let (w, h) = if ow / oh > ar {
        (oh * ar, oh)
    } else {
        (ow, ow / ar)
    };
    egui::Rect::from_center_size(outer.center(), egui::vec2(w, h))
}

/// The four border rects of `outer` left uncovered by `inner` (letterbox bars).
fn bars_around(outer: egui::Rect, inner: egui::Rect) -> [egui::Rect; 4] {
    use egui::{pos2, Rect};
    [
        Rect::from_min_max(outer.min, pos2(outer.max.x, inner.min.y)), // top
        Rect::from_min_max(pos2(outer.min.x, inner.max.y), outer.max), // bottom
        Rect::from_min_max(pos2(outer.min.x, inner.min.y), pos2(inner.min.x, inner.max.y)), // left
        Rect::from_min_max(pos2(inner.max.x, inner.min.y), pos2(outer.max.x, inner.max.y)), // right
    ]
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
