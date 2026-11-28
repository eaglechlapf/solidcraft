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
use crate::ui_kit::BackingTooltipExt as _;
use crate::ui_kit::{Container, StatusTone, Surface, expr_row, status_line};
use crate::viewport::Proj;

/// How long the part takes to slide into place.
const SLIDE_MS: f64 = 350.0;

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
    /// The rule picked on the pill (`assembly.mate`'s `rule`); none: the one the picks suggest.
    pub rule: Option<String>,
    /// The gap, and the angle rule's angle (lengths, angles or expressions).
    pub distance: String,
    pub angle: String,
    /// The angle was typed: without it, the angle rule keeps the angle the faces are at.
    pub angle_set: bool,
    /// The angle rule measures the supplement of the angle between the directions (`assembly.mate`'s
    /// `reverse`), picked on its arcs; none: as the engine measures it by default.
    pub measure: Option<bool>,
    /// Which of the four arcs (`angle_arcs`) shows the angle measured: two show each angle.
    pub sector: Option<usize>,
    pub flip: bool,
    pub lock: bool,
    /// The gap between two flat faces slides between `min` and `max` instead (or the angle
    /// between `min_angle` and `max_angle`).
    pub limits: bool,
    pub min: String,
    pub max: String,
    pub min_angle: String,
    pub max_angle: String,
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
    MateForm {
        distance: "0 mm".into(),
        angle: "90 deg".into(),
        min: "0 mm".into(),
        max: "10 mm".into(),
        min_angle: "0 deg".into(),
        max_angle: "90 deg".into(),
        ..Default::default()
    }
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
    f.rule = None;
    f.distance = next.distance;
    f.angle = next.angle;
    f.angle_set = false;
    f.measure = None;
    f.sector = None;
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

/// The rule the shown mate is asked to keep: the one picked on the pill, or the one the picks
/// suggest (the first of the engine's `choices`).
fn wanted(f: &MateForm) -> Option<String> {
    f.rule.clone().or_else(|| f.applied.as_ref().and_then(|a| a.reply["choices"].get(0)?.as_str().map(str::to_string)))
}

/// Whether the gap or the angle can take limits: two flat faces mated, or at an angle.
fn limitable(f: &MateForm) -> bool {
    f.applied.is_some() && matches!(wanted(f).as_deref(), Some("mate" | "angle"))
}

/// `assembly.mate`'s parameters for `picks` with the form's values.
fn params(app: &SolveApp, f: &MateForm, picks: &[Sel; 2]) -> Result<Value, String> {
    let hint = || solidcraft_ui_text::tr("mate.pick_moving").to_string();
    let a = pick_param(app, &picks[0]).ok_or_else(hint)?;
    let b = pick_param(app, &picks[1]).ok_or_else(hint)?;
    let mut p = json!({"a": a, "b": b});
    if let Some(rule) = &f.rule {
        p["rule"] = json!(rule);
    }
    let angular = f.rule.as_deref() == Some("angle");
    if let Some(reverse) = f.measure.filter(|_| angular) {
        p["reverse"] = json!(reverse);
    }
    if angular && f.limits && limitable(f) {
        p["limits"] = json!([f.min_angle.trim(), f.max_angle.trim()]);
    } else if angular {
        // Untyped, the faces keep the angle they are at.
        if f.angle_set {
            p["angle"] = json!(f.angle.trim());
        }
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
    match params(app, f, &picks) {
        Ok(p) => place(app, ctx, f, picks, p, inputs, None),
        Err(e) => {
            f.error = Some(e);
            if let Some(i) = inputs.get_mut(1) {
                i.items.clear();
            }
        }
    }
}

/// Make mate `p` from where the parts are now: where it puts them is worked out on a copy, and the
/// parts slide there, then the mate is made. `was` is the mate shown before (taken back already, so
/// the parts are where they were before it) and where it showed the parts: they slide from there,
/// and it is made again when `p` doesn't fit.
fn place(
    app: &mut SolveApp,
    ctx: &egui::Context,
    f: &mut MateForm,
    picks: [Sel; 2],
    p: Value,
    inputs: &mut [SelInput],
    was: Option<(Applied, Vec<(u64, Mat)>)>,
) {
    let mut scratch = app.session.scratch();
    if let Err(e) = scratch.execute("assembly.mate", &p) {
        f.error = Some(e.to_string());
        match was {
            Some((was, _)) => restore(app, f, was),
            // The second pick didn't fit: pick another.
            None => {
                if let Some(i) = inputs.get_mut(1) {
                    i.items.clear();
                }
            }
        }
        return;
    }
    let shown = |id: u64| was.as_ref().and_then(|(_, at)| at.iter().find(|(o, _)| *o == id).map(|(_, m)| *m));
    let differs = |a: &Mat, b: &Mat| a.iter().flatten().zip(b.iter().flatten()).any(|(x, y)| (x - y).abs() > 1e-9);
    let moves: Vec<(u64, Mat, Mat)> = scratch
        .doc
        .occurrences
        .iter()
        .filter(|o| !o.grounded)
        .filter_map(|o| {
            let start = app.session.doc.occurrences.iter().find(|x| x.id == o.id)?.transform;
            let from = shown(o.id).unwrap_or(start);
            differs(&from, &o.transform).then_some((o.id, from, o.transform))
        })
        .collect();
    if moves.is_empty() || !app.preferences.animate_mates {
        apply(app, f, picks, p, inputs);
        return;
    }
    f.slide = Some(Slide { started: crate::now_ms(), moves, picks, params: p, depth: app.session.undo.len() });
    // Its first frame now: the parts taken back to where they were must not show there first.
    if was.is_some() {
        slide(app, ctx, f, inputs);
    }
    ctx.request_repaint();
}

/// Make mate `was` again after one asked for on the pill didn't fit, with the pill as it was.
fn restore(app: &mut SolveApp, f: &mut MateForm, was: Applied) {
    f.rule = was.params.get("rule").and_then(Value::as_str).map(str::to_string);
    f.flip = was.params.get("flip") == Some(&json!(true));
    f.lock = was.params.get("lock") == Some(&json!(true));
    if let Ok(reply) = app.run("assembly.mate", was.params.clone()) {
        f.applied = Some(Applied { reply, depth: app.session.undo.len(), ..was });
    }
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
            // An angle not typed is the one the faces were at: the pill shows it, to type over.
            if let Some(deg) = reply["angle"].as_f64().filter(|_| !f.angle_set) {
                f.angle = format!("{} deg", crate::dialogs_coupling::trim(deg));
            }
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

/// Make the shown mate again with the pill's values now, still one undo step: taken back, so the
/// parts are where they were before the second pick, and worked out again from there (each rule
/// moves the part only as far as it needs from there); the parts slide from where they are shown.
/// A rule or value it refuses keeps the mate as it was.
fn reapply(app: &mut SolveApp, ctx: &egui::Context, f: &mut MateForm, inputs: &mut [SelInput]) {
    let Some(was) = f.applied.clone() else { return };
    let Ok(p) = params(app, f, &was.picks) else { return };
    if p == was.params {
        return;
    }
    let shown: Vec<(u64, Mat)> = app.session.doc.occurrences.iter().map(|o| (o.id, o.transform)).collect();
    if app.run("edit.undo", json!({})).is_err() {
        return;
    }
    f.applied = None;
    place(app, ctx, f, was.picks.clone(), p, inputs, Some((was, shown)));
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
            reapply(app, ctx, f, inputs);
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
        let angular = wanted(f).as_deref() == Some("angle");
        let before = (f.limits, f.min.clone(), f.max.clone(), f.min_angle.clone(), f.max_angle.clone());
        crate::ui_kit::options(ui, "mate_options", &mut [(solidcraft_ui_text::tr("mate.limits"), &mut f.limits)]);
        if f.limits {
            let (min, max) = if angular { (&mut f.min_angle, &mut f.max_angle) } else { (&mut f.min, &mut f.max) };
            expr_row(ui, solidcraft_ui_text::tr("mate.min"), min);
            expr_row(ui, solidcraft_ui_text::tr("mate.max"), max);
        }
        if before != (f.limits, f.min.clone(), f.max.clone(), f.min_angle.clone(), f.max_angle.clone()) {
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

/// A rule's icon: the one the Constrain tool shows for it, or a point on something.
fn rule_icon(rule: &str) -> &'static str {
    match rule {
        "mate" => "constraint_mate",
        "flush" => "constraint_flush",
        "angle" => "constraint_angle",
        "parallel" => "constraint_parallel",
        "tangent" => "constraint_tangent",
        "concentric" => "constraint_concentric",
        "insert" => "constraint_insert",
        "point_on_axis" => "constraint_point_on_axis",
        "line_on_plane" | "point_on_plane" | "point_on_cylinder" => "constraint_distance",
        _ => "c_coincident",
    }
}

/// A switch of the pill as an icon, its name on hover (and to screen readers).
fn icon_toggle(ui: &mut egui::Ui, icon: &str, label: &str, on: bool, handle: &str) -> bool {
    icon_button(ui, icon, label, on, handle).clicked()
}

fn icon_button(ui: &mut egui::Ui, icon: &str, label: &str, on: bool, handle: &str) -> egui::Response {
    let t = solidcraft_theme::Tokens::get();
    let side = t.control_height;
    let r = ui.add(egui::Button::new("").selected(on).min_size(egui::vec2(side, side)).sense(egui::Sense::CLICK));
    crate::icons::paint_icon(ui.painter(), r.rect.shrink(t.space_3), icon, f32::from(t.icon.a()) / 255.0);
    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, on, label));
    t.button_bevel(ui, &r, on);
    crate::scenario::publish_handle(handle, r.rect.center());
    crate::scenario::publish_rect(handle, r.rect);
    r.backing_on_hover_text(label)
}

/// The rules the two picks take, one button each: the one kept is on, the one the picks suggest
/// has a dot under it (and says so on hover). A click makes the mate again by that rule, from where
/// the part was before the second pick; the suggested one goes back to letting the picks decide.
fn rule_buttons(ui: &mut egui::Ui, f: &mut MateForm, choices: &[String], kept: &str) {
    if choices.len() < 2 {
        return;
    }
    let t = solidcraft_theme::Tokens::get();
    // Mate turned over is flush: one button, named and drawn as what it is.
    let kept = if kept == "flush" { "mate" } else { kept };
    for (i, choice) in choices.iter().enumerate() {
        let shown = if choice == "mate" && f.flip { "flush" } else { choice.as_str() };
        let name = rule_label(shown);
        let suggested = i == 0;
        let tip = if suggested { solidcraft_ui_text::format("mate.suggested", &[("rule", name)]) } else { name.to_string() };
        let r = icon_button(ui, rule_icon(shown), &tip, choice == kept, &format!("mate:rule:{choice}"));
        // In the button's corner, clear of the icon (a concentric icon ends in a dot of its own).
        if suggested {
            ui.painter().circle_filled(r.rect.right_top() + egui::vec2(-t.space_4, t.space_4), 2.0, t.accent);
        }
        if r.clicked() && choice != kept {
            f.rule = if suggested { None } else { Some(choice.clone()) };
            // What the last rule was set to may not fit this one: turned over, between limits.
            f.flip = false;
            f.limits = false;
            f.angle_set = false;
            f.measure = None;
            f.sector = None;
            f.changed = true;
        }
    }
}

fn toggle(ui: &mut egui::Ui, label: &str, on: bool, handle: &str) -> bool {
    let r = crate::ui_kit::button(ui, label, if on { crate::ui_kit::Button::SmallSelected } else { crate::ui_kit::Button::Small });
    crate::scenario::publish_handle(handle, r.rect.center());
    r.clicked()
}

/// How far from the pick the angle arcs are drawn, in points.
const ARC_RADIUS: f32 = 48.0;

fn unit(v: Vec3) -> Option<Vec3> {
    let l = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
    (l > 1e-9 && l.is_finite()).then(|| Vec3::new(v.x / l, v.y / l, v.z / l))
}

fn dot(a: Vec3, b: Vec3) -> f64 {
    a.x * b.x + a.y * b.y + a.z * b.z
}

/// `at` moved `k` along `d`.
fn along(at: Vec3, d: Vec3, k: f64) -> Vec3 {
    Vec3::new(at.x + d.x * k, at.y + d.y * k, at.z + d.z * k)
}

/// An angle `x deg` (or a plain number) as its supplement, `180 - x deg`; none for an expression.
fn supplement(angle: &str) -> Option<String> {
    let v: f64 = angle.trim().trim_end_matches("deg").trim_end_matches('°').trim().parse().ok()?;
    Some(format!("{} deg", crate::dialogs_coupling::trim(180.0 - v)))
}

/// Measure the angle on arc `i` (`angle_arcs`), which measures the supplement (`supplementary`) or
/// not; the angle measured now is the supplement when `reverse`. Measured the other way, a typed
/// angle and limits turn into their supplements, and the part stays where it is.
fn measure_on(f: &mut MateForm, i: usize, supplementary: bool, reverse: bool) {
    f.sector = Some(i);
    if supplementary == reverse {
        return;
    }
    f.measure = Some(supplementary);
    // The joint is the same either way (60 degrees between the normals is 120 against one
    // reversed): only what the value means changes, so the mate shown is kept as it is, its
    // parameters and answer brought up to date. Made again from where the part was, parallel faces
    // could tip it another way just as near.
    let Some(a) = f.applied.as_mut() else { return };
    let keep = if f.limits {
        match (supplement(&f.max_angle), supplement(&f.min_angle)) {
            (Some(lo), Some(hi)) => {
                (f.min_angle, f.max_angle) = (lo, hi);
                a.params["limits"] = json!([f.min_angle.trim(), f.max_angle.trim()]);
                true
            }
            _ => false,
        }
    } else {
        match supplement(&f.angle) {
            Some(v) => {
                f.angle = v;
                if f.angle_set {
                    a.params["angle"] = json!(f.angle.trim());
                }
                true
            }
            // An expression can't be turned round: the angle the parts are at, measured anew.
            None => {
                f.angle_set = false;
                false
            }
        }
    };
    if keep {
        a.params["reverse"] = json!(supplementary);
        a.reply["reverse"] = json!(supplementary);
        if let Some(deg) = a.reply["angle"].as_f64() {
            a.reply["angle"] = json!(180.0 - deg);
        }
    } else {
        f.changed = true;
    }
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    Vec3::new(a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x)
}

fn minus(a: Vec3, b: Vec3) -> Vec3 {
    Vec3::new(a.x - b.x, a.y - b.y, a.z - b.z)
}

/// Where two flat faces' planes meet, near the picks (`pt` on the target's, normal `nt`; `pm`, `nm`
/// on the moving one's), and from there the way across that line into each face toward its pick:
/// the rays the angle between the faces lies between. The dashed guides run out to the picks.
/// None for parallel faces, which don't meet.
fn faces_meet(pt: Vec3, nt: Vec3, pm: Vec3, nm: Vec3) -> Option<(Vec3, Vec3, Vec3, Vec<[Vec3; 2]>)> {
    let l = cross(nt, nm);
    let ll = dot(l, l);
    if ll < 1e-6 {
        return None;
    }
    let (da, db) = (dot(nt, pt), dot(nm, pm));
    let on_both = along(along(Vec3::new(0.0, 0.0, 0.0), cross(nm, l), da / ll), cross(l, nt), db / ll);
    let dir = unit(l)?;
    let mid = along(pt, minus(pm, pt), 0.5);
    let centre = along(on_both, dir, dot(dir, minus(mid, on_both)));
    // Across the line, in the face, toward the pick (across the line in the face at all, when the
    // pick is on the line).
    let into = |p: Vec3, n: Vec3| {
        let v = minus(p, centre);
        let v = along(v, dir, -dot(dir, v));
        let reach = dot(v, v).sqrt();
        Some((unit(v).or_else(|| unit(cross(dir, n)))?, reach))
    };
    let ((rt, at), (rm, am)) = (into(pt, nt)?, into(pm, nm)?);
    Some((centre, rt, rm, vec![[centre, along(centre, rt, at)], [centre, along(centre, rm, am)]]))
}

/// Where two lines (edges or axes: `pt` on the target's, direction `dt`; `pm`, `dm` on the moving
/// one's) come nearest, and from there the way along each toward its pick. The dashed guides run
/// out to the picks, and across the gap when the lines pass by each other. None for parallel lines.
fn lines_meet(pt: Vec3, dt: Vec3, pm: Vec3, dm: Vec3) -> Option<(Vec3, Vec3, Vec3, Vec<[Vec3; 2]>)> {
    let w = minus(pt, pm);
    let (b, d, e) = (dot(dt, dm), dot(dt, w), dot(dm, w));
    let den = 1.0 - b * b;
    if den < 1e-6 {
        return None;
    }
    let (s, t) = ((b * e - d) / den, (e - b * d) / den);
    let (ct, cm) = (along(pt, dt, s), along(pm, dm, t));
    let centre = along(ct, minus(cm, ct), 0.5);
    let toward = |d: Vec3, k: f64| if k > 1e-9 { d } else { Vec3::new(-d.x, -d.y, -d.z) };
    let mut guides = vec![[ct, pt], [cm, pm]];
    if dot(minus(cm, ct), minus(cm, ct)) > 1e-12 {
        guides.push([ct, cm]);
    }
    // The pick lies -s along the target's line from its nearest point, -t along the moving one's.
    Some((centre, toward(dt, -s), toward(dm, -t), guides))
}

/// An angle's arcs as drawn: four rays from the centre (`ends`), the arc from each ray to the next
/// (`arcs`, `sweeps` round) with its chip's place (`mids`), whether each measures the supplement of
/// the angle between the directions, and dashed guides.
struct Arcs {
    centre: egui::Pos2,
    ends: [egui::Pos2; 4],
    arcs: [Vec<egui::Pos2>; 4],
    mids: [egui::Pos2; 4],
    sweeps: [f64; 4],
    supplementary: [bool; 4],
    guides: Vec<[egui::Pos2; 2]>,
}

/// The arcs between the picks themselves: from face to face about the line their planes meet on,
/// or from edge to edge where the lines come nearest. None where they don't meet (parallel), or
/// meet far from where the user is looking.
fn between_picks(proj: &Proj, reply: &Value) -> Option<Arcs> {
    let v = |a: &str, b: &str| {
        let d = &reply[a][b];
        Some(Vec3::new(d.get(0)?.as_f64()?, d.get(1)?.as_f64()?, d.get(2)?.as_f64()?))
    };
    let (dt, dm) = (unit(v("directions", "target")?)?, unit(v("directions", "moving")?)?);
    let (pt, pm) = (v("origins", "target")?, v("origins", "moving")?);
    let lines = reply["lines"].as_bool().unwrap_or(false);
    let (centre, rt, rm, guides) = if lines { lines_meet(pt, dt, pm, dm)? } else { faces_meet(pt, dt, pm, dm)? };
    let n = unit(cross(rt, rm))?;
    let c = proj.to_screen(centre)?;
    if proj.to_screen(pt).is_none_or(|p| (p - c).length() > 600.0) {
        return None;
    }
    let pi = std::f64::consts::PI;
    let alpha = dot(rt, rm).clamp(-1.0, 1.0).acos();
    // The angle between the faces, as the rays run, is the one between the normals or its
    // supplement: whichever it is nearer.
    let theta = dot(dt, dm).clamp(-1.0, 1.0).acos();
    let first = (alpha - theta).abs() > (alpha - (pi - theta)).abs() + 1e-9;
    let screen_guides: Vec<[egui::Pos2; 2]> = guides.iter().filter_map(|[a, b]| Some([proj.to_screen(*a)?, proj.to_screen(*b)?])).collect();
    let on_screen = |d: Vec3| proj.to_screen(along(centre, d, 1.0)).map_or(egui::Vec2::ZERO, |p| p - c);
    let (a, b) = (on_screen(rt), on_screen(cross(n, rt)));
    let per_mm = a.length().max(b.length());
    if per_mm <= 1e-6 {
        return None;
    }
    // Their plane seen nearly edge on, the arcs would be a line: flat on the screen there instead,
    // the rays still running along the faces (or edges) as they are seen.
    if (a.x * b.y - a.y * b.x).abs() / (per_mm * per_mm) < 0.3 {
        return flat(proj, centre, rt, rm, first, screen_guides);
    }
    let r = f64::from(ARC_RADIUS / per_mm);
    let neg = |v: Vec3| Vec3::new(-v.x, -v.y, -v.z);
    let rays = [rt, rm, neg(rt), neg(rm)];
    let sweeps = [alpha, pi - alpha, alpha, pi - alpha];
    let supplementary = [first, !first, first, !first];
    let at = |i: usize, a: f64, k: f64| {
        let d = rays.get(i).copied().unwrap_or(rt);
        proj.to_screen(along(along(centre, d, k * a.cos()), cross(n, d), k * a.sin()))
    };
    let mut arcs: [Vec<egui::Pos2>; 4] = Default::default();
    let mut mids = [c; 4];
    let mut ends = [c; 4];
    for i in 0..4 {
        let sweep = sweeps.get(i).copied().unwrap_or(0.0);
        if let Some(arc) = arcs.get_mut(i) {
            *arc = (0..=32).filter_map(|k| at(i, sweep * f64::from(k) / 32.0, r)).collect();
        }
        if let (Some(m), Some(p)) = (mids.get_mut(i), at(i, sweep / 2.0, r)) {
            *m = p;
        }
        if let (Some(e), Some(p)) = (ends.get_mut(i), at(i, 0.0, r * 1.35)) {
            *e = p;
        }
    }
    Some(Arcs { centre: c, ends, arcs, mids, sweeps, supplementary, guides: screen_guides })
}

/// The arcs at the target pick, flat on the screen: where the picks don't meet (parallel) or meet
/// far off. Between the directions: the normals, or the edges' or axes' directions.
fn at_pick(proj: &Proj, reply: &Value, at: Vec3) -> Option<Arcs> {
    let dir = |k: &str| {
        let d = &reply["directions"][k];
        unit(Vec3::new(d.get(0)?.as_f64()?, d.get(1)?.as_f64()?, d.get(2)?.as_f64()?))
    };
    flat(proj, at, dir("target")?, dir("moving")?, false, Vec::new())
}

/// Arcs flat on the screen at `at`, between rays `dt` and `dm`: `dt` runs as it does on screen (up
/// when it points at the eye), `dm` on the side it is on, the angles at their true size. The first
/// arc measures the supplement when `first`.
fn flat(proj: &Proj, at: Vec3, dt: Vec3, dm: Vec3, first: bool, guides: Vec<[egui::Pos2; 2]>) -> Option<Arcs> {
    let origin = proj.to_screen(at)?;
    let on_screen = |d: Vec3| proj.to_screen(along(at, d, 1.0)).map(|p| p - origin).filter(|v| v.length() > 1e-4);
    let lead = on_screen(dt).map_or(egui::vec2(0.0, -1.0), egui::Vec2::normalized);
    let sense = on_screen(dm).map(|v| lead.x * v.y - lead.y * v.x).filter(|c| c.abs() > 1e-4).map_or(1.0, f32::signum);
    let base = f64::from(lead.y).atan2(f64::from(lead.x));
    let theta = dot(dt, dm).clamp(-1.0, 1.0).acos();
    let pi = std::f64::consts::PI;
    let starts = [0.0, theta, pi, pi + theta];
    let sweeps = [theta, pi - theta, theta, pi - theta];
    let toward = |a: f64, k: f32| {
        let a = base + f64::from(sense) * a;
        origin + egui::vec2(a.cos() as f32, a.sin() as f32) * k
    };
    let arcs = std::array::from_fn(|i| {
        let (s, w) = (starts.get(i).copied().unwrap_or(0.0), sweeps.get(i).copied().unwrap_or(0.0));
        (0..=32).map(|k| toward(s + w * f64::from(k) / 32.0, ARC_RADIUS)).collect()
    });
    let mids = std::array::from_fn(|i| toward(starts.get(i).copied().unwrap_or(0.0) + sweeps.get(i).copied().unwrap_or(0.0) / 2.0, ARC_RADIUS));
    let ends = std::array::from_fn(|i| toward(starts.get(i).copied().unwrap_or(0.0), ARC_RADIUS * 1.35));
    Some(Arcs { centre: origin, ends, arcs, mids, sweeps, supplementary: [first, !first, first, !first], guides })
}

/// The angle measured, drawn between the picks themselves (`between_picks`: from face to face, or
/// edge to edge), or flat at the target pick where they don't meet (`at_pick`): the two rays the
/// angle lies between and their reverses, and the four angles between them as arcs. The one
/// measured is a solid arrow; the others are faint, each with its value on a chip that measures
/// that one instead.
fn angle_arcs(painter: &egui::Painter, proj: &Proj, f: &mut MateForm, shown: &Applied, at: Vec3) {
    let Some(arcs) = between_picks(proj, &shown.reply).or_else(|| at_pick(proj, &shown.reply, at)) else { return };
    let reverse = shown.reply["reverse"].as_bool().unwrap_or(false);
    let measures = |i: &usize| arcs.supplementary.get(*i) == Some(&reverse);
    let chosen = f.sector.filter(measures).or_else(|| (0..4).find(measures)).unwrap_or(0);
    let t = solidcraft_theme::Tokens::get();
    let faint = t.text_dim.gamma_multiply(0.55);
    for [a, b] in &arcs.guides {
        painter.add(egui::Shape::dashed_line(&[*a, *b], egui::Stroke::new(1.0, t.text_dim), 4.0, 3.0));
    }
    for (i, end) in arcs.ends.iter().enumerate() {
        painter.line_segment([arcs.centre, *end], egui::Stroke::new(1.0, if i < 2 { t.text_dim } else { faint }));
    }
    let ctx = painter.ctx().clone();
    for (i, ((pts, sweep), mid)) in arcs.arcs.iter().zip(arcs.sweeps).zip(arcs.mids).enumerate() {
        let on = i == chosen;
        // Parallel directions: no arc to draw, but the angle measured is still shown (0 degrees).
        let flat = sweep < 1e-3;
        if flat && !on {
            continue;
        }
        let stroke = egui::Stroke::new(if on { 2.0 } else { 1.2 }, if on { t.accent } else { faint });
        if !flat {
            painter.add(egui::Shape::line(pts.clone(), stroke));
            // An arrowhead at each end: the angle runs from one ray to the other.
            let n_pts = pts.len();
            for (tip, from) in [(pts.first(), pts.get(1)), (pts.last(), n_pts.checked_sub(2).and_then(|k| pts.get(k)))] {
                if let (Some(tip), Some(from)) = (tip, from) {
                    let back = (*from - *tip).normalized() * if on { 9.0 } else { 7.0 };
                    let wing = egui::vec2(-back.y, back.x) * 0.45;
                    painter.add(egui::Shape::convex_polygon(vec![*tip, *tip + back + wing, *tip + back - wing], stroke.color, egui::Stroke::NONE));
                }
            }
        }
        // Its value on a chip beyond the arc's middle: a click measures this one.
        let out = (mid - arcs.centre).normalized() * 16.0;
        let label = format!("{}°", crate::dialogs_coupling::trim((sweep.to_degrees() * 10.0).round() / 10.0));
        let handle = format!("mate:arc:{i}");
        let clicked = Container::new(&ctx, Surface::Canvas)
            .area(egui::Id::unique(("sc_mate_arc", i)))
            .pivot(egui::Align2::CENTER_CENTER)
            .fixed_pos(mid + out)
            .order(egui::Order::Foreground)
            .show(&ctx, |ui| {
                // One line: a chip near the pill or the window's edge has little room, and a wrapped
                // value ("6" over "0°") reads as two.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                let r = crate::ui_kit::button(ui, &label, if on { crate::ui_kit::Button::SmallSelected } else { crate::ui_kit::Button::Small });
                crate::scenario::publish_handle(&handle, r.rect.center());
                r.clicked()
            })
            .inner;
        if clicked && !on {
            measure_on(f, i, arcs.supplementary.get(i).copied().unwrap_or(false), reverse);
        }
    }
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
    let choices: Vec<String> = shown.reply["choices"].as_array().into_iter().flatten().filter_map(|c| c.as_str().map(str::to_string)).collect();
    let ctx = painter.ctx().clone();
    let t = solidcraft_theme::Tokens::get();
    // An angle draws its arcs round the pick: the pill sits clear of them.
    let at = if rule == "angle" {
        angle_arcs(painter, proj, f, &shown, anchor);
        screen + egui::vec2(ARC_RADIUS + 40.0, -ARC_RADIUS - 24.0)
    } else {
        screen + egui::vec2(18.0, -18.0)
    };
    Container::new(&ctx, Surface::Canvas).area(egui::Id::unique("sc_mate_pill")).fixed_pos(at).order(egui::Order::Foreground).show(&ctx, |ui| {
        let frame = Container::popup_frame(ui.style()).inner_margin(egui::Margin::symmetric(6, 3));
        Container::show_backing(ui, frame, |ui| {
            ui.horizontal(|ui| {
                // The relationship made, by name, and the others the picks take. The name is
                // centred on the buttons' height: a caption on its own sits on the row's top.
                let name = egui::RichText::new(rule_label(&rule)).color(t.text_dim).size(t.font_caption);
                let width =
                    ui.painter().layout_no_wrap(rule_label(&rule).to_string(), egui::FontId::proportional(t.font_caption), t.text_dim).size().x;
                let r = ui.add_sized(egui::vec2(width, t.control_height), egui::Label::new(name).selectable(false));
                crate::scenario::publish_rect("mate:rule_name", r.rect);
                rule_buttons(ui, f, &choices, &rule);
                let angular = rule == "angle";
                let valued =
                    matches!(rule.as_str(), "mate" | "flush" | "angle" | "tangent" | "line_on_plane" | "point_on_plane" | "point_on_cylinder");
                if valued && !(f.limits && limitable(f)) {
                    let value = if angular { &mut f.angle } else { &mut f.distance };
                    let id = egui::Id::unique("sc_mate_value");
                    let r = crate::ui_kit::canvas_value_field(ui, id, value, false, "");
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
                        f.angle_set |= angular;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        f.done = true;
                    }
                }
                // Turned over: two faces flush, an axis end for end. (An angle picks the angle it
                // measures on its arcs instead.)
                if matches!(rule.as_str(), "mate" | "flush" | "parallel" | "insert")
                    && icon_toggle(ui, "reverse_normal", solidcraft_ui_text::tr("mate.flip"), f.flip, "mate:flip")
                {
                    f.flip = !f.flip;
                    f.changed = true;
                }
                if matches!(rule.as_str(), "concentric" | "insert")
                    && icon_toggle(ui, "c_fix", solidcraft_ui_text::tr("mate.lock"), f.lock, "mate:lock")
                {
                    f.lock = !f.lock;
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

/// Esc in the Mate tool with a mate shown: it is taken back (on the next frame, `step`), and the
/// tool stays open. False when there is no mate to take back: Esc closes the tool.
pub fn take_back(d: &mut crate::dialogs::Dialog) -> bool {
    match &mut d.kind {
        crate::dialogs::Kind::Assembly(Asm::Mate(f)) if f.applied.is_some() && f.slide.is_none() => {
            f.undo = true;
            true
        }
        _ => false,
    }
}

/// The Mate tool shows a mate: made, or sliding into place.
pub fn showing(app: &SolveApp) -> bool {
    app.dialog.as_ref().is_some_and(|d| matches!(&d.kind, crate::dialogs::Kind::Assembly(Asm::Mate(f)) if f.applied.is_some() || f.slide.is_some()))
}

/// The Mate tool is open.
pub fn open(app: &SolveApp) -> bool {
    app.dialog.as_ref().is_some_and(|d| matches!(&d.kind, crate::dialogs::Kind::Assembly(Asm::Mate(_))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Esc with a mate shown takes it back and keeps the tool open for the next; the next Esc
    /// closes the tool. Neither leaves an undo step behind.
    #[test]
    fn esc_takes_the_mate_back_then_closes_the_tool() {
        let mut h = crate::scenario::Harness::new();
        h.app.home.open = false;
        h.app.preferences.animate_mates = false;
        crate::test_support::grounded_boxes(&[("A", 0), ("B", 30)], |id, p| h.app.run(id, p).unwrap());
        let undo = h.app.session.undo.len();
        h.app.start("assembly.mate");
        let (b, a) = (
            crate::dialogs::face_sel(&h.app.session, Vec3::new(40.0, 10.0, 10.0)).unwrap(),
            crate::dialogs::face_sel(&h.app.session, Vec3::new(10.0, 10.0, 10.0)).unwrap(),
        );
        if let Some(d) = h.app.dialog.as_mut() {
            d.inputs[0].items = vec![b];
            d.inputs[1].items = vec![a];
        }
        h.frames(3);
        assert_eq!(h.app.session.doc.assembly.joints.len(), 1, "the mate is made");
        h.step(&json!({"key": "Escape"})).unwrap();
        h.frames(2);
        assert!(h.app.session.doc.assembly.joints.is_empty(), "Esc takes it back");
        assert!(open(&h.app), "and the tool stays open");
        assert_eq!(h.app.session.undo.len(), undo, "nothing left to undo");
        h.step(&json!({"key": "Escape"})).unwrap();
        h.frames(1);
        assert!(!open(&h.app), "the next Esc closes the tool");
        assert_eq!(h.app.session.undo.len(), undo);
    }

    /// The pill's rules: two cylinders on one axis as the picks suggest, side by side when tangent is
    /// picked, and back on the axis when the suggested one is. Each is made from where the roller
    /// was before the second pick, so back on the axis is exactly where the first mate put it, and
    /// the shown mate stays one undo step.
    #[test]
    fn a_rule_picked_on_the_pill_is_made_from_where_the_part_was() {
        use solidcraft_engine::doc::joints::JointKind;
        let mut h = crate::scenario::Harness::new();
        h.app.home.open = false;
        h.app.preferences.animate_mates = false;
        let run = |h: &mut crate::scenario::Harness, id: &str, p: Value| h.app.run(id, p).unwrap();
        run(&mut h, "component.create", json!({"name": "Shaft"}));
        run(&mut h, "solid.cylinder", json!({"base": [0, 0, 0], "axis": [0, 0, 1], "radius": 5, "height": 40, "operation": "new"}));
        run(&mut h, "component.activate", json!({"component": "root"}));
        run(&mut h, "component.create", json!({"name": "Roller"}));
        run(&mut h, "solid.cylinder", json!({"base": [60, 30, 10], "axis": [0, 1, 0], "radius": 8, "height": 20, "operation": "new"}));
        run(&mut h, "component.activate", json!({"component": "root"}));
        let shaft = h.app.session.doc.find_component("Shaft").and_then(|c| h.app.session.doc.occurrence_of(c)).map(|o| o.id).unwrap();
        run(&mut h, "occurrence.ground", json!({"occurrence": shaft, "grounded": true}));
        let roller = h.app.session.doc.find_component("Roller").and_then(|c| h.app.session.doc.occurrence_of(c)).map(|o| o.id).unwrap();
        let placed = |h: &crate::scenario::Harness| h.app.session.doc.occurrences.iter().find(|o| o.id == roller).map(|o| o.transform).unwrap();
        let undo = h.app.session.undo.len();
        h.app.start("assembly.mate");
        let (a, b) = (
            crate::dialogs::face_sel(&h.app.session, Vec3::new(68.0, 40.0, 10.0)).unwrap(),
            crate::dialogs::face_sel(&h.app.session, Vec3::new(5.0, 0.0, 20.0)).unwrap(),
        );
        if let Some(d) = h.app.dialog.as_mut() {
            d.inputs[0].items = vec![a];
            d.inputs[1].items = vec![b];
        }
        h.frames(3);
        let kinds = |h: &crate::scenario::Harness| h.app.session.doc.assembly.joints.iter().map(|j| j.kind).collect::<Vec<_>>();
        assert_eq!(kinds(&h), vec![JointKind::Cylindrical], "on one axis, as the picks suggest");
        // The pill names it, centred on its buttons' height.
        let name = crate::scenario::rect_of("mate:rule_name").expect("the pill names the rule");
        let button = crate::scenario::rect_of("mate:rule:concentric").expect("a button for each rule");
        assert!((name.center().y - button.center().y).abs() < 0.5, "{name:?} vs {button:?}");
        let on_axis = placed(&h);
        let pick = |h: &mut crate::scenario::Harness, rule: Option<&str>| {
            if let Some(crate::dialogs::Kind::Assembly(Asm::Mate(f))) = h.app.dialog.as_mut().map(|d| &mut d.kind) {
                f.rule = rule.map(str::to_string);
                f.changed = true;
            }
            h.frames(3);
        };
        pick(&mut h, Some("tangent"));
        assert_eq!(kinds(&h), vec![JointKind::LineOnCylinder], "side by side");
        assert_ne!(placed(&h), on_axis);
        assert_eq!(h.app.session.undo.len(), undo + 1, "still one step");
        pick(&mut h, None);
        assert_eq!(kinds(&h), vec![JointKind::Cylindrical], "back on the axis");
        for (x, y) in placed(&h).iter().flatten().zip(on_axis.iter().flatten()) {
            assert!((x - y).abs() < 1e-9, "where the first mate put it: {:?} vs {on_axis:?}", placed(&h));
        }
        assert_eq!(h.app.session.undo.len(), undo + 1, "still one step");
    }

    /// Measuring an angle on the other side (a faint arc's chip) changes what its value means, not
    /// the mate: 60 degrees between two tops' normals is 120 measured against one reversed. The
    /// joint stays exactly as it is, tipped the same way; two tops facing the same way could tip
    /// either way just as near, were it made again.
    #[test]
    fn measuring_an_angle_the_other_way_leaves_the_part_where_it_is() {
        let mut h = crate::scenario::Harness::new();
        h.app.home.open = false;
        h.app.preferences.animate_mates = false;
        crate::test_support::grounded_boxes(&[("A", 0), ("B", 30)], |id, p| h.app.run(id, p).unwrap());
        h.app.start("assembly.mate");
        let (b, a) = (
            crate::dialogs::face_sel(&h.app.session, Vec3::new(40.0, 10.0, 10.0)).unwrap(),
            crate::dialogs::face_sel(&h.app.session, Vec3::new(10.0, 10.0, 10.0)).unwrap(),
        );
        if let Some(d) = h.app.dialog.as_mut() {
            d.inputs[0].items = vec![b];
            d.inputs[1].items = vec![a];
        }
        h.frames(3);
        let form = |h: &mut crate::scenario::Harness, set: &dyn Fn(&mut MateForm)| {
            if let Some(crate::dialogs::Kind::Assembly(Asm::Mate(f))) = h.app.dialog.as_mut().map(|d| &mut d.kind) {
                set(f);
            }
            h.frames(3);
        };
        form(&mut h, &|f| {
            f.rule = Some("angle".into());
            f.changed = true;
        });
        form(&mut h, &|f| {
            f.angle = "60 deg".into();
            f.angle_set = true;
            f.changed = true;
        });
        let joint = |h: &crate::scenario::Harness| h.app.session.doc.assembly.joints.last().cloned().unwrap();
        let (before, undo) = (joint(&h), h.app.session.undo.len());
        assert_eq!(before.kind, solidcraft_engine::doc::joints::JointKind::Angle);
        form(&mut h, &|f| measure_on(f, 1, true, false));
        let after = joint(&h);
        assert_eq!((after.id, after.angle, after.values.clone()), (before.id, before.angle, before.values.clone()), "the same joint, as it was");
        assert_eq!(h.app.session.undo.len(), undo, "nothing made again");
        if let Some(crate::dialogs::Kind::Assembly(Asm::Mate(f))) = h.app.dialog.as_ref().map(|d| &d.kind) {
            assert_eq!(f.angle, "120 deg");
            let shown = f.applied.as_ref().unwrap();
            assert_eq!((shown.params["reverse"].as_bool(), shown.reply["reverse"].as_bool()), (Some(true), Some(true)));
        }
        // While the mate is shown no joint glyph is drawn round its arcs; kept (Done), it has its own.
        assert!(crate::dialogs_assembly::drawn_joints(&h.app).is_empty(), "no joint glyphs while the mate is shown");
        h.app.set_origin_shown(true);
        assert!(!h.app.origin_visible(), "nor the origin's planes and axes");
        form(&mut h, &|f| f.done = true);
        assert_eq!(crate::dialogs_assembly::drawn_joints(&h.app), vec![(after.id, false)], "the mate kept draws its glyph");
        assert!(h.app.origin_visible(), "and the origin is back");
    }

    /// An angle's arcs run between the picks: two faces' from where their planes meet, out into each
    /// face toward its pick; two lines' from where they come nearest, along each toward its pick,
    /// with the gap between lines that pass each other shown.
    #[test]
    fn angle_arcs_run_from_face_to_face_and_edge_to_edge() {
        let near = |a: Vec3, b: Vec3| (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9 && (a.z - b.z).abs() < 1e-9;
        // A plate's top (z = 10) and a block's side (x = 30, facing -x) meet on x = 30, z = 10.
        let (top, side) = (Vec3::new(5.0, 5.0, 10.0), Vec3::new(30.0, 45.0, 60.0));
        let (centre, rt, rm, guides) = faces_meet(top, Vec3::new(0.0, 0.0, 1.0), side, Vec3::new(-1.0, 0.0, 0.0)).expect("they meet");
        assert!(near(centre, Vec3::new(30.0, 25.0, 10.0)), "{centre:?}: on the line, beside the picks");
        assert!(near(rt, Vec3::new(-1.0, 0.0, 0.0)) && near(rm, Vec3::new(0.0, 0.0, 1.0)), "{rt:?} {rm:?}: into each face");
        assert_eq!(guides.len(), 2);
        assert!(faces_meet(top, Vec3::new(0.0, 0.0, 1.0), side, Vec3::new(0.0, 0.0, -1.0)).is_none(), "parallel faces don't meet");
        // A line along x through the origin and one along y through (5, 3, 4) pass 4 apart at x = 5.
        let (centre, rt, rm, guides) =
            lines_meet(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 0.0), Vec3::new(5.0, 3.0, 4.0), Vec3::new(0.0, 1.0, 0.0)).expect("they cross");
        assert!(near(centre, Vec3::new(5.0, 0.0, 2.0)), "{centre:?}");
        assert!(near(rt, Vec3::new(-1.0, 0.0, 0.0)) && near(rm, Vec3::new(0.0, 1.0, 0.0)), "{rt:?} {rm:?}: toward each pick");
        assert_eq!(guides.len(), 3, "out to each pick, and across the gap");
    }

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
