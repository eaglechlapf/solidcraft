//! The Mate tool (`assembly.mate`): one tool for putting parts together. Pick something on the
//! part to move (a face, an edge, a point), then something on another part: the tool works out
//! the mate from the two picks (two flat faces against each other, two cylinders on one axis, a
//! cylinder on a face…), and the first part slides into place. The mate is made at once, as one
//! undo step, and a small box beside it (the pill) changes it in place: the distance, an angle
//! instead, flipped, the turn locked, tangent instead of on one axis. Picking the next part keeps
//! the mate and starts the next one; Esc takes the mate back, and a second Esc closes the tool.
//!
//! While the tool is open, dragging a part moves it within the motion its mates leave free (no mate
//! is made), and right-clicking a part shows its planes, axes and sketches to pick from.

use serde_json::{Value, json};
use solidcore_record::Vec3;
use solidcraft_engine::Sel;
use solidcraft_engine::doc::{IDENTITY, Mat};

use crate::SolveApp;
use crate::dialogs_assembly::{Asm, Hover, picked_snap, placement_of_body, snap_of};
use crate::selection::SelInput;
use crate::ui_kit::{Container, StatusTone, Surface, expr_row, status_line};
use crate::viewport::Proj;

/// How long the part takes to slide into place.
const SLIDE_MS: f64 = 350.0;

/// How the two picks are joined, when not as they suggest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Auto,
    Angle,
    Tangent,
    Axial,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Angle => "angle",
            Mode::Tangent => "tangent",
            Mode::Axial => "axial",
        }
    }
}

/// A mate made and shown: its picks, what it was made with and the engine's answer (its rule,
/// joint type, the freedom left, conflicts).
#[derive(Clone, Debug)]
pub struct Applied {
    pub picks: [Sel; 2],
    pub params: Value,
    pub reply: Value,
    /// The undo history's length with the mate made: the pill makes it again only while its step
    /// is the last one (a drag after it keeps it as it is).
    pub depth: usize,
}

/// The part sliding into place before the mate is made: each occurrence that moves, from where
/// it is to where the mate puts it.
#[derive(Clone, Debug)]
pub struct Slide {
    started: f64,
    moves: Vec<(u64, Mat, Mat)>,
    picks: [Sel; 2],
    params: Value,
    /// The undo history's length before the slide: the slide's steps go, the mate's stays.
    depth: usize,
}

#[derive(Clone, Debug, Default)]
pub struct MateForm {
    pub mode: Mode,
    /// The gap, and the angle mode's angle (lengths, angles or expressions).
    pub distance: String,
    pub angle: String,
    pub flip: bool,
    pub lock: bool,
    /// The gap between two flat faces slides between `min` and `max` instead.
    pub limits: bool,
    pub min: String,
    pub max: String,
    pub error: Option<String>,
    pub hover: Hover,
    pub applied: Option<Applied>,
    pub slide: Option<Slide>,
    /// Asked by the pill: make the mate again with the values now, keep it and go on, take it back.
    pub changed: bool,
    pub done: bool,
    pub undo: bool,
    /// Put the cursor in the pill's value (a mate was just made).
    pub focus: bool,
    pub made: usize,
}

fn start_form() -> MateForm {
    MateForm { distance: "0 mm".into(), angle: "90 deg".into(), min: "0 mm".into(), max: "10 mm".into(), ..Default::default() }
}

/// What a Mate pick takes: a face, an edge or a corner as itself (an edge is not its middle and
/// a hole's wall not its centre, as a joint origin's snap would make them), a datum plane or
/// axis, the origin, or a point. No snap points: the engine reads what a pick is from the
/// exact geometry, so a pick needs no glyph to say where it lands.
pub const PICKS: crate::selection::Accept = crate::selection::FACES
    | crate::selection::EDGES
    | crate::selection::VERTICES
    | crate::selection::ORIGIN
    | crate::selection::POINTS
    | crate::selection::PLANES
    | crate::selection::AXES
    | crate::selection::CONSTRUCTION_POINTS;

pub fn start(_app: &SolveApp) -> Option<(Asm, Vec<SelInput>)> {
    Some((Asm::Mate(Box::new(start_form())), vec![SelInput::new("Moves", PICKS, false), SelInput::new("To", PICKS, false)]))
}

/// The values one mate had, back to the defaults for the next.
fn reset(f: &mut MateForm) {
    let next = start_form();
    f.mode = next.mode;
    f.distance = next.distance;
    f.angle = next.angle;
    f.flip = false;
    f.lock = false;
    f.limits = false;
    f.error = None;
}

fn first(inputs: &[SelInput], i: usize) -> Option<Sel> {
    inputs.get(i).and_then(|x| x.items.first().cloned())
}

fn pt(p: Vec3) -> Value {
    json!([p.x, p.y, p.z])
}

/// A pick as `assembly.mate` takes it: a face, an edge or a corner where it was clicked (the
/// engine reads what it is from the exact geometry), on the placement it was picked on; a plane,
/// an axis or a point as a joint origin.
fn pick_param(app: &SolveApp, sel: &Sel) -> Option<Value> {
    let s = &app.session;
    let (body, mut v) = match sel {
        Sel::Face { body, point, .. } => (body, json!({"face": pt(*point)})),
        Sel::Edge { body, point, .. } => (body, json!({"edge": pt(*point)})),
        Sel::Vertex { body, point } => (body, json!({"point": pt(*point)})),
        other => return snap_of(s, other).map(|x| x.param),
    };
    if let (Some(path), Value::Object(m)) = (placement_of_body(s, body), &mut v) {
        m.insert("occurrence".into(), json!(path.last().copied().unwrap_or(0)));
        if !path.is_empty() {
            m.insert("path".into(), json!(path));
        }
    }
    Some(v)
}

/// Whether the gap can take limits: two flat faces mated.
fn limitable(f: &MateForm) -> bool {
    f.mode == Mode::Auto
        && f.applied.as_ref().is_some_and(|a| {
            let rule = a.reply["rule"].as_str().unwrap_or_default();
            matches!(rule, "mate" | "flush") || (a.params.get("limits").is_some() && a.reply["kind"] == "parallel")
        })
}

/// `assembly.mate`'s parameters for `picks` with the form's values.
fn params(app: &SolveApp, f: &MateForm, picks: &[Sel; 2]) -> Result<Value, String> {
    let hint = || solidcraft_ui_text::tr("mate.pick_moving").to_string();
    let a = pick_param(app, &picks[0]).ok_or_else(hint)?;
    let b = pick_param(app, &picks[1]).ok_or_else(hint)?;
    let mut p = json!({"a": a, "b": b});
    if f.mode != Mode::Auto {
        p["mode"] = json!(f.mode.name());
    }
    if f.mode == Mode::Angle {
        p["angle"] = json!(f.angle.trim());
    } else if f.limits && limitable(f) {
        p["limits"] = json!([f.min.trim(), f.max.trim()]);
    } else {
        p["offset"] = json!(f.distance.trim());
    }
    if f.flip {
        p["flip"] = json!(true);
    }
    if f.lock {
        p["lock"] = json!(true);
    }
    Ok(p)
}

/// `t` eased in and out.
fn ease(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// A rotation matrix's quaternion (x, y, z, w).
fn quat(m: &Mat) -> [f64; 4] {
    let (r00, r11, r22) = (m[0][0], m[1][1], m[2][2]);
    // Row-vector matrices: m[i][j] maps axis i onto j; the column-vector rotation is its transpose.
    let (r01, r02, r10, r12, r20, r21) = (m[1][0], m[2][0], m[0][1], m[2][1], m[0][2], m[1][2]);
    let trace = r00 + r11 + r22;
    let q = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        [(r21 - r12) / s, (r02 - r20) / s, (r10 - r01) / s, 0.25 * s]
    } else if r00 > r11 && r00 > r22 {
        let s = (1.0 + r00 - r11 - r22).sqrt() * 2.0;
        [0.25 * s, (r01 + r10) / s, (r02 + r20) / s, (r21 - r12) / s]
    } else if r11 > r22 {
        let s = (1.0 + r11 - r00 - r22).sqrt() * 2.0;
        [(r01 + r10) / s, 0.25 * s, (r12 + r21) / s, (r02 - r20) / s]
    } else {
        let s = (1.0 + r22 - r00 - r11).sqrt() * 2.0;
        [(r02 + r20) / s, (r12 + r21) / s, 0.25 * s, (r10 - r01) / s]
    };
    let n = q.iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-12);
    q.map(|v| v / n)
}

/// The rotation matrix of quaternion `q`, in the row-vector layout `quat` reads.
fn from_quat([x, y, z, w]: [f64; 4]) -> Mat {
    let mut m = IDENTITY;
    let c = [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w)],
        [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w)],
        [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y)],
    ];
    for (i, row) in c.iter().enumerate() {
        for (j, v) in row.iter().enumerate() {
            if let Some(cell) = m.get_mut(j).and_then(|r| r.get_mut(i)) {
                *cell = *v;
            }
        }
    }
    m
}

/// The placement `t` of the way from `a` to `b`: the turn by the shortest way, the move straight.
fn between(a: &Mat, b: &Mat, t: f64) -> Mat {
    let (qa, mut qb) = (quat(a), quat(b));
    let mut dot: f64 = qa.iter().zip(&qb).map(|(x, y)| x * y).sum();
    if dot < 0.0 {
        qb = qb.map(|v| -v);
        dot = -dot;
    }
    let q = if dot > 0.9995 {
        let q: Vec<f64> = qa.iter().zip(&qb).map(|(x, y)| x + (y - x) * t).collect();
        let n = q.iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-12);
        [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
    } else {
        let theta = dot.clamp(-1.0, 1.0).acos();
        let (wa, wb) = (((1.0 - t) * theta).sin() / theta.sin(), (t * theta).sin() / theta.sin());
        [qa[0] * wa + qb[0] * wb, qa[1] * wa + qb[1] * wb, qa[2] * wa + qb[2] * wb, qa[3] * wa + qb[3] * wb]
    };
    let mut m = from_quat(q);
    for i in 0..3 {
        m[3][i] = a[3][i] + (b[3][i] - a[3][i]) * t;
    }
    m
}

/// `occurrence.move`'s parameters putting occurrence `id` at `m` (its placement in its parent),
/// shown there and not kept. `to_matrix` is row-major with the translation in the last column;
/// a placement keeps the translation in its last row, so it reads out column by column.
fn move_params(id: u64, m: &Mat) -> Value {
    let to_matrix: Vec<f64> = (0..3).flat_map(|row| m.iter().map(move |col| col[row])).collect();
    json!({"occurrence": id, "space": "local", "to_matrix": to_matrix, "capture": false})
}

/// Make the mate for two new picks: the part slides there first, when the mate moves it.
fn begin(app: &mut SolveApp, ctx: &egui::Context, f: &mut MateForm, picks: [Sel; 2], inputs: &mut [SelInput]) {
    let p = match params(app, f, &picks) {
        Ok(p) => p,
        Err(e) => {
            f.error = Some(e);
            if let Some(i) = inputs.get_mut(1) {
                i.items.clear();
            }
            return;
        }
    };
    // Where the mate puts the parts, worked out on a copy.
    let mut scratch = app.session.scratch();
    if let Err(e) = scratch.execute("assembly.mate", &p) {
        f.error = Some(e.to_string());
        // The second pick didn't fit: pick another.
        if let Some(i) = inputs.get_mut(1) {
            i.items.clear();
        }
        return;
    }
    let moves: Vec<(u64, Mat, Mat)> = scratch
        .doc
        .occurrences
        .iter()
        .filter(|o| !o.grounded)
        .filter_map(|o| {
            let was = app.session.doc.occurrences.iter().find(|x| x.id == o.id)?;
            let moved = was.transform.iter().flatten().zip(o.transform.iter().flatten()).any(|(a, b)| (a - b).abs() > 1e-9);
            moved.then_some((o.id, was.transform, o.transform))
        })
        .collect();
    if moves.is_empty() || !app.preferences.animate_mates {
        apply(app, f, picks, p, inputs);
        return;
    }
    f.slide = Some(Slide { started: crate::now_ms(), moves, picks, params: p, depth: app.session.undo.len() });
    ctx.request_repaint();
}

/// A frame of the slide; the mate is made at its end.
fn slide(app: &mut SolveApp, ctx: &egui::Context, f: &mut MateForm, inputs: &mut [SelInput]) {
    let Some(sl) = f.slide.clone() else { return };
    let t = ((crate::now_ms() - sl.started) / SLIDE_MS).clamp(0.0, 1.0);
    if t < 1.0 {
        let e = ease(t);
        let mut stuck = false;
        for (id, from, to) in &sl.moves {
            let m = between(from, to, e);
            // Not captured: shown there, not kept, so the joints don't pull it back on the way.
            stuck |= app.run("occurrence.move", move_params(*id, &m)).is_err();
        }
        if !stuck {
            ctx.request_repaint();
            return;
        }
        // A part that can't be shown on its way isn't slid: the mate is made at once.
    }
    let ids: Vec<u64> = sl.moves.iter().map(|(id, ..)| *id).collect();
    let _ = app.run("component.revert_position", json!({"occurrences": ids}));
    app.session.undo.truncate(sl.depth);
    f.slide = None;
    apply(app, f, sl.picks, sl.params, inputs);
}

/// Make the mate: one undo step.
fn apply(app: &mut SolveApp, f: &mut MateForm, picks: [Sel; 2], p: Value, inputs: &mut [SelInput]) {
    match app.run("assembly.mate", p.clone()) {
        Ok(reply) => {
            f.applied = Some(Applied { picks, params: p, reply, depth: app.session.undo.len() });
            f.error = None;
            f.focus = true;
        }
        Err(e) => {
            f.error = Some(e);
            if let Some(i) = inputs.get_mut(1) {
                i.items.clear();
            }
        }
    }
}

/// Make the shown mate again with the values now: taken back and made anew, still one undo step.
/// A value it refuses keeps the mate as it was.
fn reapply(app: &mut SolveApp, f: &mut MateForm) {
    let Some(was) = f.applied.clone() else { return };
    let Ok(p) = params(app, f, &was.picks) else { return };
    if p == was.params {
        return;
    }
    if app.run("edit.undo", json!({})).is_err() {
        return;
    }
    match app.run("assembly.mate", p.clone()) {
        Ok(reply) => {
            f.applied = Some(Applied { picks: was.picks, params: p, reply, depth: app.session.undo.len() });
            f.error = None;
        }
        Err(e) => {
            f.error = Some(e);
            if let Ok(reply) = app.run("assembly.mate", was.params.clone()) {
                f.applied = Some(Applied { reply, depth: app.session.undo.len(), ..was });
            }
        }
    }
}

/// What the picks and the pill ask, each frame.
fn step(app: &mut SolveApp, ctx: &egui::Context, f: &mut MateForm, inputs: &mut [SelInput], active: &mut usize) {
    if f.slide.is_some() {
        slide(app, ctx, f, inputs);
        return;
    }
    let picks = [first(inputs, 0), first(inputs, 1)];
    if f.undo {
        f.undo = false;
        if f.applied.take().is_some_and(|a| a.depth == app.session.undo.len()) {
            let _ = app.run("edit.undo", json!({}));
        }
        for i in inputs.iter_mut() {
            i.items.clear();
        }
        reset(f);
        *active = 0;
        return;
    }
    if let Some(shown) = f.applied.clone() {
        let new_first = picks[0].as_ref() != Some(&shown.picks[0]);
        // Something else changed the design since (a part dragged): the mate stays as it is.
        let moved_on = app.session.undo.len() != shown.depth || crate::joint_drag::active(app) || crate::occurrence_drag::active(app);
        if new_first || f.done || moved_on {
            // The shown mate stays; a new first pick starts the next one.
            f.applied = None;
            f.done = false;
            f.made += 1;
            reset(f);
            let next = if new_first { picks[0].clone() } else { None };
            if let Some(i) = inputs.get_mut(0) {
                i.items = next.into_iter().collect();
            }
            if let Some(i) = inputs.get_mut(1) {
                i.items.clear();
            }
            *active = usize::from(inputs.first().is_some_and(|i| !i.items.is_empty()));
            return;
        }
        if f.changed {
            f.changed = false;
            reapply(app, f);
        }
        // The next click picks the next part to move.
        *active = 0;
        return;
    }
    if let [Some(a), Some(b)] = picks {
        begin(app, ctx, f, [a, b], inputs);
        *active = 0;
    }
}

/// The tool's rows (beside the pill): what to pick, the limits, the freedom left.
pub fn rows(app: &mut SolveApp, ui: &mut egui::Ui, f: &mut MateForm, inputs: &mut [SelInput], active: &mut usize) -> bool {
    // Esc with a mate shown takes it back; the next Esc closes the tool.
    if f.applied.is_some() && f.slide.is_none() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        f.undo = true;
    }
    let ctx = ui.ctx().clone();
    step(app, &ctx, f, inputs, active);
    let hint = match (&f.applied, first(inputs, 0)) {
        (Some(_), _) => None,
        (None, None) => Some("mate.pick_moving"),
        (None, Some(_)) => Some("mate.pick_target"),
    };
    if let Some(h) = hint {
        status_line(ui, solidcraft_ui_text::tr(h), StatusTone::Dim);
    }
    if limitable(f) {
        let before = (f.limits, f.min.clone(), f.max.clone());
        crate::ui_kit::options(ui, "mate_options", &mut [(solidcraft_ui_text::tr("mate.limits"), &mut f.limits)]);
        if f.limits {
            expr_row(ui, solidcraft_ui_text::tr("mate.min"), &mut f.min);
            expr_row(ui, solidcraft_ui_text::tr("mate.max"), &mut f.max);
        }
        if before != (f.limits, f.min.clone(), f.max.clone()) {
            f.changed = true;
        }
    }
    if let Some(a) = &f.applied {
        let r = &a.reply;
        if r["fell_back"] == json!(true) {
            status_line(ui, solidcraft_ui_text::tr("mate.fell_back"), StatusTone::Warning);
        }
        if let Some(c) = r["conflicts"].as_array().and_then(|c| c.first()) {
            let other = c["name"].as_str().unwrap_or_default();
            status_line(ui, &solidcraft_ui_text::format("mate.conflict", &[("other", other)]), StatusTone::Warning);
        }
        let moving = a.params["a"]["occurrence"].as_u64().unwrap_or(0);
        let dof = r["dof"].as_array().into_iter().flatten().find(|d| d["occurrence"].as_u64() == Some(moving)).and_then(|d| d["dof"].as_u64());
        if let Some(n) = dof {
            let part = app.session.doc.occurrences.iter().find(|o| o.id == moving).map(|o| o.name.clone()).unwrap_or_default();
            let text = match n {
                0 => solidcraft_ui_text::format("constrain.fixed", &[("part", &part)]),
                1 => solidcraft_ui_text::format("constrain.dof_one", &[("part", &part)]),
                n => solidcraft_ui_text::format("constrain.dof", &[("part", &part), ("count", &n.to_string())]),
            };
            status_line(ui, &text, StatusTone::Dim);
        }
    }
    if f.made > 0 {
        status_line(ui, &solidcraft_ui_text::format("mate.made", &[("count", &f.made.to_string())]), StatusTone::Dim);
    }
    status_line(ui, solidcraft_ui_text::tr("mate.drag_hint"), StatusTone::Dim);
    if let Some(e) = &f.error {
        status_line(ui, e, StatusTone::Error);
    }
    false
}

/// OK keeps the shown mate (it is made already): nothing more to run.
pub fn commands(_app: &SolveApp, _f: &MateForm, _inputs: &[SelInput]) -> Result<Vec<(String, Value)>, String> {
    Ok(Vec::new())
}

/// Whether dragging a part moves it (within its free motion) while the tool is open: not while a
/// part slides into place.
pub fn drags(app: &SolveApp) -> bool {
    app.dialog.as_ref().is_some_and(|d| matches!(&d.kind, crate::dialogs::Kind::Assembly(Asm::Mate(f)) if f.slide.is_none()))
}

/// The rule's name in the user's language.
fn rule_label(rule: &str) -> &'static str {
    solidcraft_ui_text::tr(match rule {
        "mate" => "mate.rule_mate",
        "flush" => "mate.rule_flush",
        "angle" => "mate.rule_angle",
        "parallel" => "mate.rule_parallel",
        "tangent" => "mate.rule_tangent",
        "concentric" => "mate.rule_concentric",
        "insert" => "mate.rule_insert",
        "line_on_plane" => "mate.rule_line_on_plane",
        "point_on_plane" => "mate.rule_point_on_plane",
        "point_on_cylinder" => "mate.rule_point_on_cylinder",
        "point_on_axis" => "mate.rule_point_on_axis",
        _ => "mate.rule_coincident",
    })
}

/// A toggle in the pill; true when clicked.
fn toggle(ui: &mut egui::Ui, label: &str, on: bool, handle: &str) -> bool {
    let r = crate::ui_kit::button(ui, label, if on { crate::ui_kit::Button::SmallSelected } else { crate::ui_kit::Button::Small });
    crate::scenario::publish_handle(handle, r.rect.center());
    r.clicked()
}

/// The pill beside the mate shown: its rule, its value, and the switches that fit it.
pub fn overlay(app: &SolveApp, painter: &egui::Painter, proj: &Proj, f: &mut MateForm) {
    if f.slide.is_some() || !crate::canvas::boxes_shown(painter.ctx()) {
        return;
    }
    let Some(shown) = f.applied.clone() else { return };
    let Some(anchor) = picked_snap(app, &shown.picks[1]).map(|s| s.at) else { return };
    let Some(screen) = proj.to_screen(anchor) else { return };
    let rule = shown.reply["rule"].as_str().unwrap_or_default().to_string();
    let kind = shown.reply["kind"].as_str().unwrap_or_default().to_string();
    let faces = shown.picks.iter().all(|p| matches!(p, Sel::Face { .. }));
    let ctx = painter.ctx().clone();
    let t = solidcraft_theme::Tokens::get();
    let at = screen + egui::vec2(18.0, -18.0);
    Container::new(&ctx, Surface::Canvas).area(egui::Id::unique("sc_mate_pill")).fixed_pos(at).order(egui::Order::Foreground).show(&ctx, |ui| {
        let frame = Container::popup_frame(ui.style()).inner_margin(egui::Margin::symmetric(6, 3));
        Container::show_backing(ui, frame, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(rule_label(&rule)).color(t.text_dim).size(t.font_caption));
                let planes = matches!(rule.as_str(), "mate" | "flush" | "angle") || (rule == "parallel" && f.mode == Mode::Auto && !faces);
                let valued =
                    matches!(rule.as_str(), "mate" | "flush" | "angle" | "tangent" | "line_on_plane" | "point_on_plane" | "point_on_cylinder");
                if valued && !(f.limits && limitable(f)) {
                    let value = if f.mode == Mode::Angle { &mut f.angle } else { &mut f.distance };
                    let id = egui::Id::unique("sc_mate_value");
                    let r = crate::ui_kit::canvas_value_field(ui, id, value, false);
                    crate::scenario::publish_handle("mate:value", r.rect.center());
                    if f.focus {
                        f.focus = false;
                        r.request_focus();
                        let n = value.chars().count();
                        let mut st = egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap_or_default();
                        st.cursor
                            .set_char_range(Some(egui::text_selection::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
                        st.store(ui.ctx(), id);
                    }
                    if r.changed() {
                        f.changed = true;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        f.done = true;
                    }
                }
                if planes || rule == "angle" {
                    if toggle(ui, solidcraft_ui_text::tr("mate.angle"), f.mode == Mode::Angle, "mate:angle") {
                        f.mode = if f.mode == Mode::Angle { Mode::Auto } else { Mode::Angle };
                        f.changed = true;
                        f.focus = true;
                    }
                    if toggle(ui, solidcraft_ui_text::tr("mate.flip"), f.flip, "mate:flip") {
                        f.flip = !f.flip;
                        f.changed = true;
                    }
                }
                // Two cylinders, or a straight edge and a cylinder: on one axis or side by side.
                let axial = matches!(rule.as_str(), "concentric") && (faces || f.mode == Mode::Axial);
                if axial && toggle(ui, solidcraft_ui_text::tr("mate.tangent"), false, "mate:tangent") {
                    f.mode = if faces { Mode::Tangent } else { Mode::Auto };
                    f.changed = true;
                    f.focus = true;
                }
                if kind == "line_on_cylinder" && toggle(ui, solidcraft_ui_text::tr("mate.axial"), false, "mate:axial") {
                    f.mode = if faces { Mode::Auto } else { Mode::Axial };
                    f.changed = true;
                }
                if matches!(rule.as_str(), "concentric" | "insert") && toggle(ui, solidcraft_ui_text::tr("mate.lock"), f.lock, "mate:lock") {
                    f.lock = !f.lock;
                    f.changed = true;
                }
                if rule == "insert" && toggle(ui, solidcraft_ui_text::tr("mate.flip"), f.flip, "mate:flip") {
                    f.flip = !f.flip;
                    f.changed = true;
                }
                if toggle(ui, solidcraft_ui_text::tr("mate.done"), false, "mate:done") {
                    f.done = true;
                }
            });
        });
    });
}

/// A right-click on a part while the tool is open: show its origin planes and axes, its
/// construction planes, axes and points, and its sketches, to mate to (or hide them again when
/// they are shown). False when nothing part of a placement is under the pointer.
pub fn show_references(app: &mut SolveApp, body: &str) -> bool {
    let s = &app.session;
    let Some(path) = placement_of_body(s, body).filter(|p| !p.is_empty()) else { return false };
    let Some(occ) = path.last().copied() else { return false };
    let Some(component) = s.doc.occurrences.iter().find(|o| o.id == occ).map(|o| o.component) else { return false };
    let ids: Vec<String> = path.iter().map(u64::to_string).collect();
    let folder = format!("o:{}/origin", ids.join("/"));
    let show = s.folder_hidden(&folder);
    let items: Vec<String> = s
        .doc
        .features
        .iter()
        .filter(|f| f.component == component && (matches!(f.kind, solidcraft_engine::doc::FeatureKind::Sketch { .. }) || f.kind.is_construction()))
        .map(|f| f.id.to_string())
        .collect();
    let _ = app.run("browser.visibility", json!({"tree_folders": [folder], "visible": show}));
    if !items.is_empty() {
        let _ = app.run("browser.visibility", json!({"items": items, "visible": show}));
    }
    true
}

/// The Mate tool is open.
pub fn open(app: &SolveApp) -> bool {
    app.dialog.as_ref().is_some_and(|d| matches!(&d.kind, crate::dialogs::Kind::Assembly(Asm::Mate(_))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each frame of the slide is a placement `occurrence.move` takes, and puts the part there.
    #[test]
    fn a_slide_frame_puts_the_part_where_the_frame_says() {
        let mut s = solidcraft_engine::Session::default();
        crate::test_support::grounded_boxes(&[("A", 0), ("B", 30)], |id, p| s.execute(id, &p).unwrap());
        let c = s.doc.find_component("B").unwrap();
        let id = s.doc.occurrence_of(c).unwrap().id;
        let mut m = solidcraft_engine::doc::rigid(Vec3::new(5.0, 6.0, 7.0), Vec3::ZERO, Vec3::Z, 0.7);
        m[3] = [5.0, 6.0, 7.0, 1.0];
        s.execute("occurrence.move", &move_params(id, &m)).unwrap();
        // Shown there, not kept: the placement waits in the pending moves.
        let at = s.pending_moves.get(&id).copied().unwrap_or(IDENTITY);
        for (x, y) in at.iter().flatten().zip(m.iter().flatten()) {
            assert!((x - y).abs() < 1e-9, "{at:?} vs {m:?}");
        }
    }

    #[test]
    fn the_slide_turns_by_the_shortest_way_and_ends_where_the_mate_puts_the_part() {
        let a = IDENTITY;
        let mut b = solidcraft_engine::doc::rigid(Vec3::new(10.0, 0.0, 5.0), Vec3::ZERO, Vec3::Z, 1.2);
        b[3] = [10.0, 0.0, 5.0, 1.0];
        let end = between(&a, &b, 1.0);
        for (x, y) in end.iter().flatten().zip(b.iter().flatten()) {
            assert!((x - y).abs() < 1e-9, "{end:?} vs {b:?}");
        }
        let half = between(&a, &b, 0.5);
        let want = solidcraft_engine::doc::rigid(Vec3::ZERO, Vec3::ZERO, Vec3::Z, 0.6);
        for i in 0..3 {
            for j in 0..3 {
                assert!((half[i][j] - want[i][j]).abs() < 1e-9, "{half:?}");
            }
        }
        assert!((half[3][0] - 5.0).abs() < 1e-9);
    }
}
