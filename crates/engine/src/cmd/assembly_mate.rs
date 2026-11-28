//! Mate (`assembly.mate`): one command that joins two parts from a pick on each, the way the
//! picks suggest. Two flat faces mate against each other, a cylinder rests on a flat face, two
//! holes or shafts line up on one axis, two round edges sit on one centre, a point lies on a face.
//! What a pick is (a plane, a cylinder, a round edge, a straight edge, a point) is read from the
//! exact geometry, never the display mesh.
//!
//! Side `a` is the part that moves (the first pick), side `b` the one it moves to. The rule is
//! kept as a joint of the kind it needs, made where the parts are, so the part moves no further
//! than the rule needs and keeps every motion it leaves free. When an alignment (a mate, or two
//! axes on one line) can't be met because of the joints the parts already have, the parts are
//! made parallel instead, at whatever distance they are: `fell_back` says so.

use serde_json::{Value, json};
use solidcore_record::Vec3;
use solidcraft_doc::joints::{self, Constraint, Joint, JointKind, JointOrigin, RefShape};
use solidcraft_geom::Vec3Geom;

use super::{CommandSpec, add_joint, component_of, new_joint, origin_param, value};
use crate::params::{bad, bool_, str_};
use crate::schema::joints::{MateParams, MateResult};

use crate::schema::{TypedContract, output_schema_for};
use crate::{Result, Session};

const CONTRACT: TypedContract<MateParams> = TypedContract::new();

pub static COMMANDS: &[CommandSpec] = &[CommandSpec::new("assembly.mate", "Mate", crate::checked!("assembly.mate", mate))
    .at("SOLID", "ASSEMBLE")
    .icon("constrain")
    .contract(&CONTRACT)
    .output(output_schema_for::<MateResult>)
    .params(
        "a: the pick on the part that moves, b: the pick it moves to; each a joint origin as joint.create takes it, \
         where face: [x,y,z] may be a flat or a cylindrical face and edge: [x,y,z] a straight or round edge (read from the \
         exact geometry); mode?: auto|angle|tangent|axial (auto: two flat faces mate, two cylinders line up on one axis, a \
         straight edge rests on a cylinder; angle: two flat faces at `angle`; tangent: two cylinders side by side; axial: a \
         straight edge on a cylinder's axis); rule?: one of the reply's choices (mate, angle, parallel, tangent, concentric, \
         insert, line_on_plane, point_on_plane, point_on_cylinder, point_on_axis, coincident) in place of mode, default the \
         first; offset?: the gap (mm or an expression); angle?: deg (for angle: between the faces' normals or the edges' \
         or axes' directions; default the angle they are at); reverse?: measure the angle against b's direction reversed \
         (its supplement; default false for faces, for edges and axes whichever is at most 90 deg now); flip?: turn \
         the moving part over (two faces flush instead of against each other); lock?: true stops the turn of two parts on one \
         axis; limits?: [min, max] of the gap (mm) instead of one gap, or with mode angle of the angle (deg) instead of `angle`: the part moves between them; name?; quiet? → \
         joint, name, rule, kind, fell_back, choices, angle?, reverse?, directions?, origins?, lines?, dof, conflicts, fixed",
    )];

/// What a pick is, for choosing the rule.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    Plane,
    /// A cylindrical face: its radius, and whether it is a hole (the material outside it).
    Cylinder {
        radius: f64,
        hole: bool,
    },
    /// A round edge, or a round end given by its centre.
    Circle,
    /// A straight edge or an axis.
    Line,
    Point,
}

impl Shape {
    /// A line the part can turn about: a cylinder's, a round edge's or an axis.
    fn axial(self) -> bool {
        matches!(self, Shape::Cylinder { .. } | Shape::Circle | Shape::Line)
    }
}

/// A pick and what it is.
#[derive(Clone, Debug)]
struct Ref {
    origin: JointOrigin,
    shape: Shape,
}

/// The way of joining the two picks, tried in turn until one is met.
#[derive(Clone, Debug)]
struct Try {
    kind: JointKind,
    /// The joint's side a (fixed by the rule) and b.
    a: JointOrigin,
    b: JointOrigin,
    flip: bool,
    /// What the gap is added to (a radius, or two).
    base: f64,
    /// The gap goes the other way (inside a hole).
    inward: bool,
    tag: Option<Constraint>,
    rule: &'static str,
    /// An angle between two lines (edges or axes) rather than two faces: a line has no side, so
    /// the angle measured is the one at most a right angle unless `reverse` says.
    lines: bool,
}

fn mate(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "assembly.mate";
    let moving = pick(s, p.get("a"), cmd)?;
    let target = pick(s, p.get("b"), cmd)?;
    if moving.origin.occurrence == target.origin.occurrence && moving.origin.inside == target.origin.inside {
        return Err(bad(cmd, "the two picks are on one part: pick a face, edge or point on another part"));
    }
    let gap = match p.get("offset") {
        Some(v) => value(s, v, false, cmd)?,
        None => 0.0,
    };
    let angle = p.get("angle").map(|v| value(s, v, true, cmd)).transpose()?;
    let mode = str_(p, "mode").unwrap_or("auto");
    // An angle's limits are angles (degrees), a gap's lengths.
    let angular = mode == "angle" || str_(p, "rule") == Some("angle");
    let limits = match p.get("limits") {
        None | Some(Value::Null) => None,
        Some(Value::Array(pair)) if pair.len() == 2 => {
            let lo = pair.first().map(|v| value(s, v, angular, cmd)).transpose()?.unwrap_or(0.0);
            let hi = pair.get(1).map(|v| value(s, v, angular, cmd)).transpose()?.unwrap_or(0.0);
            if lo > hi {
                return Err(bad(cmd, "`limits` must be [min, max] with min no larger than max"));
            }
            Some((lo, hi))
        }
        Some(_) => return Err(bad(cmd, "`limits` must be [min, max]")),
    };
    if !matches!(mode, "auto" | "angle" | "tangent" | "axial") {
        return Err(bad(cmd, "`mode` must be auto, angle, tangent or axial"));
    }
    let flip = bool_(p, "flip") == Some(true);
    let lock = bool_(p, "lock") == Some(true);
    let choices = choices(moving.shape, target.shape);
    if choices.is_empty() {
        return Err(bad(cmd, "these two picks can't be mated"));
    }
    if mode == "angle" && !choices.contains(&"angle") {
        return Err(bad(cmd, "the angle mode takes two flat faces"));
    }
    if mode == "tangent" && !choices.contains(&"tangent") {
        return Err(bad(cmd, "the tangent mode takes two cylinders, or a cylinder and a flat face"));
    }
    // The rule asked for, or the mode's, or the one the picks suggest (the first that fits).
    let asked = match str_(p, "rule") {
        Some(r) => Some(r),
        None => match mode {
            "angle" => Some("angle"),
            "tangent" => Some("tangent"),
            "axial" if choices.contains(&"concentric") => Some("concentric"),
            _ => None,
        },
    };
    let rule = match asked {
        Some(r) if choices.contains(&r) => r,
        Some(r) => return Err(bad(cmd, format!("`rule` {r} doesn't fit these picks: they take {}", choices.join(", ")))),
        None => choices.first().copied().unwrap_or("mate"),
    };
    let tries = ways(rule, &moving, &target, flip, lock);
    if limits.is_some() && !tries.first().is_some_and(|t| matches!(t.kind, JointKind::Planar | JointKind::Angle)) {
        return Err(bad(cmd, "limits are taken for the gap or the angle between two flat faces"));
    }
    // A gap between limits is a parallel joint's free distance, held between them; an angle
    // between limits, an angle range's tilt.
    let mut tries = tries;
    if limits.is_some()
        && let Some(t) = tries.first_mut()
    {
        t.kind = if t.kind == JointKind::Angle { JointKind::AngleRange } else { JointKind::Parallel };
    }
    let saved = s.doc.clone();
    let last = tries.len().saturating_sub(1);
    for (i, t) in tries.into_iter().enumerate() {
        let fell_back = i > 0;
        let reply = attempt(s, p, &t, gap, angle, limits, cmd)?;
        let id = reply.get("joint").and_then(Value::as_u64).unwrap_or(0);
        let stuck =
            reply.get("conflicts").and_then(Value::as_array).is_some_and(|c| c.iter().any(|c| c.get("joint").and_then(Value::as_u64) == Some(id)));
        if stuck && i < last {
            // Met by nothing the parts can do: back to where they were, and the next way.
            s.set_doc((*saved).clone());
            continue;
        }
        // joint.create's answer (`MateResult` flattens it), with the rule kept.
        let mut out = reply;
        if let Some(m) = out.as_object_mut() {
            m.insert("rule".into(), json!(t.rule));
            m.insert("kind".into(), serde_json::to_value(t.kind).unwrap_or_default());
            m.insert("fell_back".into(), json!(fell_back));
            m.insert("choices".into(), json!(choices));
        }
        return Ok(out);
    }
    Err(bad(cmd, "these two picks can't be mated"))
}

/// Add the joint one way of mating takes.
fn attempt(s: &mut Session, p: &Value, t: &Try, gap: f64, angle: Option<f64>, limits: Option<(f64, f64)>, cmd: &str) -> Result<Value> {
    let mut q = json!({ "flip": t.flip });
    if let (Some(m), Some(name)) = (q.as_object_mut(), str_(p, "name").filter(|n| !n.trim().is_empty())) {
        m.insert("name".into(), json!(name));
    }
    if let (Some(m), Some(quiet)) = (q.as_object_mut(), p.get("quiet")) {
        m.insert("quiet".into(), quiet.clone());
    }
    let parallel_gap = t.kind == JointKind::Parallel && matches!(t.rule, "mate" | "flush");
    let mut j: Joint = new_joint(s, &q, t.kind, t.a.clone(), t.b.clone(), cmd)?;
    // An axis has no side: a hole's z is its rim's face normal, whichever rim the pick was nearer.
    // Lined up, or parallel after all, the parts keep the way they face now (else the moving part
    // would turn over, against its other mates), and Flip turns it over from there.
    // Side by side (a line on a cylinder) is the same: turned over, a pin standing by a hole would
    // turn end for end about the pick and stand up out of it.
    let sideless = matches!(t.kind, JointKind::Cylindrical | JointKind::Slider | JointKind::LineOnCylinder)
        || (t.kind == JointKind::Parallel && t.rule == "parallel");
    if sideless {
        let st = s.model.state();
        if let (Some(za), Some(zb)) = (world_z(s, &st, &j.a), world_z(s, &st, &j.b)) {
            let datums = |o: &JointOrigin| matches!(o.snap, joints::Snap::Plane { .. } | joints::Snap::Axis { .. });
            // Turned over (z against z) exactly when `datums == flip` (`joints::flip_of`).
            let aligned = za.dot(zb) > 0.0;
            j.flip = (datums(&j.a) && datums(&j.b)) != aligned;
            j.flip ^= t.flip;
        }
    }
    let signed = if t.inward { -gap } else { gap };
    // An angle is measured between the picks' directions as they are (two faces' normals, two
    // lines' directions), or with `reverse` against the target's reversed: its supplement.
    let mut measured: Option<(bool, Option<f64>, JointOrigin, JointOrigin)> = None;
    let mut tilt_range = None;
    if matches!(t.kind, JointKind::Angle | JointKind::AngleRange) {
        let st = s.model.state();
        let now = match (world_z(s, &st, &j.a), world_z(s, &st, &j.b)) {
            (Some(a), Some(b)) => a.dot(b).clamp(-1.0, 1.0).acos(),
            _ => 0.0,
        };
        let reverse = bool_(p, "reverse").unwrap_or(t.lines && now > std::f64::consts::FRAC_PI_2);
        // The joint's tilt for an angle so measured: its frames meet turned over (z against z)
        // unless flipped (`joints::flip_of`), and a tilt is the same either way round.
        let datums = |o: &JointOrigin| matches!(o.snap, joints::Snap::Plane { .. } | joints::Snap::Axis { .. });
        let turned = (datums(&j.a) && datums(&j.b)) == j.flip;
        let pi = std::f64::consts::PI;
        let tilt_of = |v: f64| if reverse != turned { pi - v } else { v };
        // The way the part leans now: the tilt keeps its sign, so the part turns the least.
        let lean = if tilt_now(s, &j) < 0.0 { -1.0 } else { 1.0 };
        let shown = if reverse { pi - now } else { now };
        match t.kind {
            JointKind::Angle => {
                let v = angle.unwrap_or(shown);
                j.angle = lean * tilt_of(v);
                measured = Some((reverse, Some(v), j.a.clone(), j.b.clone()));
            }
            _ => {
                j.angle = 0.0;
                if let Some((lo, hi)) = limits {
                    let (a, b) = (lean * tilt_of(lo), lean * tilt_of(hi));
                    let range = (a.min(b), a.max(b));
                    j.limits = vec![None, None, None, None, Some(range), None];
                    tilt_range = Some(range);
                }
                measured = Some((reverse, None, j.a.clone(), j.b.clone()));
            }
        }
    }
    match t.kind {
        JointKind::Angle | JointKind::AngleRange => {}
        // A gap that slides between limits: the parallel joint's z, not a fixed offset.
        JointKind::Parallel if parallel_gap => {
            j.offset = 0.0;
            if let Some((lo, hi)) = limits {
                j.limits = vec![None, None, Some((lo, hi)), None];
            }
        }
        // Parallel at whatever distance the parts are.
        JointKind::Parallel => j.offset = 0.0,
        _ => j.offset = t.base + signed,
    }
    // A typed gap stays an expression the joint follows, where the gap is the whole offset.
    if t.base == 0.0 && !parallel_gap && t.kind != JointKind::Parallel {
        j.exprs.offset = super::typed(p.get("offset"));
    } else {
        j.exprs.offset = None;
    }
    if t.tag == Some(Constraint::Tangent) && t.kind == JointKind::LineOnPlane {
        // As assembly.constraint.create keeps a tangent: the gap, the radius added at each solve.
        j.exprs.offset = Some(super::typed(p.get("offset")).unwrap_or_else(|| format!("{gap} mm")));
    }
    j.constraint = t.tag;
    // Where the parts are: the values that move the part least.
    if let Some(v) = joints::relation_values(&s.doc, &s.model.state(), &j) {
        j.values = v;
    }
    if let Some((lo, hi)) = limits.filter(|_| parallel_gap)
        && let Some(z) = j.values.get_mut(2)
    {
        *z = z.clamp(lo, hi);
    }
    if let Some((lo, hi)) = tilt_range
        && let Some(tilt) = j.values.get_mut(4)
    {
        // A turn is the same a whole turn on: the one nearest the range (a half turn is as much
        // +180 as -180 degrees), then into it.
        let off = |a: f64| {
            if a < lo {
                lo - a
            } else if a > hi {
                a - hi
            } else {
                0.0
            }
        };
        let tau = std::f64::consts::TAU;
        let near = [*tilt - tau, *tilt, *tilt + tau].into_iter().min_by(|a, b| off(*a).total_cmp(&off(*b))).unwrap_or(*tilt);
        *tilt = near.clamp(lo, hi);
        // Kept as found: `add_joint` would find them again and clamp the half turn the other way.
        if let Some(m) = q.as_object_mut() {
            m.insert("values".into(), json!(j.values));
        }
    }
    let mut reply = add_joint(s, j, &q)?;
    // How an angle is measured, and the two directions it is measured between and a point on each
    // pick (on its face, edge or axis) where the parts are now: what the view draws its arcs from,
    // face to face or edge to edge.
    if let (Some((reverse, v, a, b)), Some(m)) = (measured, reply.as_object_mut()) {
        let st = s.model.state();
        m.insert("reverse".into(), json!(reverse));
        m.insert("lines".into(), json!(t.lines));
        if let Some(v) = v {
            m.insert("angle".into(), json!(v.to_degrees()));
        }
        if let (Some(fa), Some(fb)) = (world_frame(s, &st, &a), world_frame(s, &st, &b)) {
            let row = |f: &solidcraft_doc::Mat, i: usize| f.get(i).map_or(json!([0, 0, 0]), |r| json!([r[0], r[1], r[2]]));
            m.insert("directions".into(), json!({"target": row(&fa, 2), "moving": row(&fb, 2)}));
            m.insert("origins".into(), json!({"target": row(&fa, 3), "moving": row(&fb, 3)}));
        }
    }
    Ok(reply)
}

/// A path's direction, start to end, when it runs straight (in any form: an imported part's
/// straight edge may be a spline): every point along it on the chord.
fn straight(path: &[solidcraft_kernel::PathSeg]) -> Option<Vec3> {
    let at = |t: f64| solidcraft_kernel::along_path(path, t).ok().map(|(p, _)| p);
    let (a, b) = (at(0.0)?, at(1.0)?);
    let chord = b - a;
    let tol = (chord.len() * 1e-6).max(1e-9);
    let on = (1..8).all(|i| at(f64::from(i) / 8.0).is_some_and(|p| p.dist_to_segment(a, b) <= tol));
    (chord.len() > tol && on).then_some(chord)
}

/// A joint origin's frame in the world, where its part sits now: z its direction, the last row
/// where it is.
fn world_frame(s: &Session, st: &solidcraft_doc::ModelState, o: &JointOrigin) -> Option<solidcraft_doc::Mat> {
    let frame = joints::resolve_origin(&s.doc, st, o).ok()?;
    Some(solidcraft_doc::mat_mul(&frame, &joints::occurrence_world(&s.doc, o.occurrence)))
}

/// The world direction of a joint origin's z, where its part sits now.
fn world_z(s: &Session, st: &solidcraft_doc::ModelState, o: &JointOrigin) -> Option<Vec3> {
    let z = *world_frame(s, st, o)?.get(2)?;
    Some(Vec3::new(z[0], z[1], z[2]))
}

/// The angle two flat faces are at now, as an angle joint `j` between them reads it: what an
/// angle mate given no angle keeps, so it turns the part no further than lining up the faces.
fn tilt_now(s: &Session, j: &Joint) -> f64 {
    let mut range = j.clone();
    range.kind = JointKind::AngleRange;
    range.angle = 0.0;
    joints::relation_values(&s.doc, &s.model.state(), &range).and_then(|v| v.get(4).copied()).filter(|a| a.is_finite()).unwrap_or(0.0)
}

/// The rules two picks can be joined by, the one they suggest first: what the pill offers.
fn choices(moving: Shape, target: Shape) -> Vec<&'static str> {
    use Shape::*;
    let flat = |s: Shape| matches!(s, Plane | Circle);
    let round = |s: Shape| matches!(s, Cylinder { .. });
    match (moving, target) {
        // Two round edges: one centre on one axis; or only on one axis, or their faces mated.
        (Circle, Circle) => vec!["insert", "concentric", "mate", "angle", "parallel"],
        // Two flat faces (a round edge's plane counts as one).
        (a, b) if flat(a) && flat(b) => vec!["mate", "angle", "parallel"],
        // A cylinder on a flat face, or its axis in it.
        (Plane, b) | (b, Plane) if round(b) => vec!["tangent", "line_on_plane"],
        (Plane, Line) | (Line, Plane) => vec!["line_on_plane"],
        (Plane, Point) | (Point, Plane) => vec!["point_on_plane"],
        // Two cylinders: on one axis, side by side, or their axes at an angle.
        (a, b) if round(a) && round(b) => vec!["concentric", "tangent", "angle", "parallel"],
        // A straight edge resting on a cylinder, or on its axis, or at an angle to it.
        (Line, b) | (b, Line) if round(b) => vec!["tangent", "concentric", "angle", "parallel"],
        // A point on a cylinder's wall, or on its axis.
        (Point, b) | (b, Point) if round(b) => vec!["point_on_cylinder", "point_on_axis"],
        // Any two other lines (straight edges, axes): one axis, or at an angle.
        (a, b) if a.axial() && b.axial() => vec!["concentric", "angle", "parallel"],
        (Point, b) | (b, Point) if b.axial() => vec!["point_on_axis"],
        (Point, Point) => vec!["coincident"],
        _ => Vec::new(),
    }
}

/// The ways of mating `moving` to `target` by `rule` (one of [`choices`]), the rule first and the
/// fallback after it when the parts' other joints won't let it meet.
fn ways(rule: &str, moving: &Ref, target: &Ref, flip: bool, lock: bool) -> Vec<Try> {
    use Shape::*;
    let (m, t) = (&moving.origin, &target.origin);
    let make = |kind, a: &JointOrigin, b: &JointOrigin, flip, base, inward, tag, rule| Try {
        kind,
        a: a.clone(),
        b: b.clone(),
        flip,
        base,
        inward,
        tag,
        rule,
        lines: false,
    };
    // A face, or a round edge's face, has a side; an edge or an axis does not.
    let sided = |s: Shape| matches!(s, Plane | Circle);
    let parallel = || make(JointKind::Parallel, t, m, flip, 0.0, false, Some(Constraint::Parallel), "parallel");
    let concentric = || {
        let kind = if lock { JointKind::Slider } else { JointKind::Cylindrical };
        make(kind, t, m, flip, 0.0, false, Some(Constraint::Concentric), "concentric")
    };
    // The pick that is the face (or cylinder, or axis) the other rests on, then the other: side a
    // is the one rested on.
    let split = |is: &dyn Fn(Shape) -> bool| if is(moving.shape) { (moving, target) } else { (target, moving) };
    let round = |r: &Ref| match r.shape {
        Cylinder { radius, hole } => (radius, hole),
        _ => (0.0, false),
    };
    match rule {
        "mate" => {
            let (rule, tag) = if flip { ("flush", Constraint::Flush) } else { ("mate", Constraint::Mate) };
            vec![make(JointKind::Planar, t, m, flip, 0.0, false, Some(tag), rule), parallel()]
        }
        "angle" => {
            let mut way = make(JointKind::Angle, t, m, flip, 0.0, false, Some(Constraint::Angle), "angle");
            way.lines = !(sided(moving.shape) && sided(target.shape));
            vec![way]
        }
        "parallel" => vec![parallel()],
        "concentric" => vec![concentric(), parallel()],
        "insert" => {
            let kind = if lock { JointKind::Rigid } else { JointKind::Revolute };
            vec![make(kind, t, m, flip, 0.0, false, Some(Constraint::Insert), "insert"), concentric(), parallel()]
        }
        "tangent" => match (moving.shape, target.shape) {
            // One inside the other (a shaft in a hole) touches inside: the radii's difference.
            (Cylinder { radius: ra, hole: ha }, Cylinder { radius: rb, hole: hb }) => {
                let (base, inward) = if ha != hb { ((ra - rb).abs(), true) } else { (ra + rb, false) };
                vec![make(JointKind::LineOnCylinder, t, m, false, base, inward, None, "tangent")]
            }
            (Plane, _) | (_, Plane) => {
                let (plane, cylinder) = split(&|s| s == Plane);
                vec![make(
                    JointKind::LineOnPlane,
                    &plane.origin,
                    &cylinder.origin,
                    false,
                    round(cylinder).0,
                    false,
                    Some(Constraint::Tangent),
                    "tangent",
                )]
            }
            _ => {
                let (cylinder, line) = split(&|s| matches!(s, Cylinder { .. }));
                let (radius, hole) = round(cylinder);
                vec![make(JointKind::LineOnCylinder, &cylinder.origin, &line.origin, false, radius, hole, None, "tangent")]
            }
        },
        "line_on_plane" => {
            let (plane, line) = split(&|s| s == Plane);
            vec![make(JointKind::LineOnPlane, &plane.origin, &line.origin, false, 0.0, false, None, "line_on_plane")]
        }
        "point_on_plane" => {
            let (plane, point) = split(&|s| s == Plane);
            vec![make(JointKind::PointOnPlane, &plane.origin, &point.origin, false, 0.0, false, Some(Constraint::Distance), "point_on_plane")]
        }
        "point_on_cylinder" => {
            let (cylinder, point) = split(&|s| matches!(s, Cylinder { .. }));
            let (radius, hole) = round(cylinder);
            vec![make(JointKind::PointOnCylinder, &cylinder.origin, &point.origin, false, radius, hole, None, "point_on_cylinder")]
        }
        "point_on_axis" => {
            let (axis, point) = split(&|s| s != Point);
            vec![make(JointKind::PointOnLine, &axis.origin, &point.origin, false, 0.0, false, Some(Constraint::PointOnAxis), "point_on_axis")]
        }
        // Two points: on one spot, turning any way.
        _ => vec![make(JointKind::Ball, t, m, false, 0.0, false, None, "coincident")],
    }
}

/// A pick and what it is, from the exact geometry under it.
fn pick(s: &Session, v: Option<&Value>, cmd: &str) -> Result<Ref> {
    let v = v.ok_or_else(|| bad(cmd, "the picks `a` and `b` are required"))?;
    let at = |key: &str| v.get(key).and_then(crate::params::vec3);
    let st = s.model.state();
    if let Some(p) = at("face") {
        let (on, local) = located(s, v, p, cmd)?;
        let found = bodies(s, &st, on.part()).into_iter().filter_map(|(body, to_body)| {
            let q = solidcraft_doc::apply_point(&to_body, local);
            body.nearest_face(q).map(|(d, f)| (d, body, f, q, to_body))
        });
        let best = found.min_by(|a, b| a.0.total_cmp(&b.0));
        let Some((_, body, face, q, _)) = best else { return Err(bad(cmd, "no face there")) };
        return match solidcraft_kernel::face_surface(body, face) {
            Some(solidcraft_kernel::FaceSurface::Plane { .. }) => Ok(Ref { origin: origin_param(s, Some(v), cmd)?, shape: Shape::Plane }),
            Some(solidcraft_kernel::FaceSurface::Cylinder { origin, axis, radius }) => {
                // A hole when the material is outside the wall: a point just inside the wall, toward the axis.
                let axis = Vec3::new(axis.x, axis.y, axis.z);
                let foot = origin + axis * (q - origin).dot(axis);
                let inward = foot - q;
                let step = (radius * 0.05).max(1e-3).min(radius * 0.5);
                let probe = q + inward * (step / inward.len().max(1e-12));
                let hole = matches!(body.classify_point(probe), Ok(solidcraft_kernel::PointClass::Outside));
                let mut round = v.clone();
                if let Some(m) = round.as_object_mut() {
                    m.remove("face");
                    m.insert("circle".into(), json!(p));
                }
                Ok(Ref { origin: origin_param(s, Some(&round), cmd)?, shape: Shape::Cylinder { radius, hole } })
            }
            _ => Err(bad(cmd, "only flat and cylindrical faces can be mated so far")),
        };
    }
    if let Some(p) = at("edge") {
        let (on, local) = located(s, v, p, cmd)?;
        let world = joints::occurrence_world(&s.doc, on.part());
        let mut best: Option<(f64, &solidcraft_kernel::Body, usize, Option<Vec3>, solidcraft_doc::Mat)> = None;
        for (body, to_body) in bodies(s, &st, on.part()) {
            let q = solidcraft_doc::apply_point(&to_body, local);
            for e in 0..body.edge_count() {
                // To the edge's own curve, whatever it is (a circle may be a spline in an imported
                // part); a cylinder's seam is no edge to pick.
                let Ok(Some(d)) = solidcraft_kernel::edge_distance(body, e, q) else { continue };
                if d.is_finite() && best.as_ref().is_none_or(|(bd, ..)| d < *bd) {
                    let line = solidcraft_kernel::edge_path(body, e).ok().and_then(|path| straight(&path));
                    // A straight edge's direction, back in the occurrence's (then the world's) frame.
                    let back = solidcraft_doc::mat_inverse(&to_body).unwrap_or(solidcraft_doc::IDENTITY);
                    best = Some((d, body, e, line.map(|l| solidcraft_doc::apply_vector(&world, solidcraft_doc::apply_vector(&back, l))), to_body));
                }
            }
        }
        let Some((_, body, e, line, to_body)) = best else { return Err(bad(cmd, "no edge there")) };
        if let Some(dir) = line {
            let mut straight = v.clone();
            if let Some(m) = straight.as_object_mut() {
                m.insert("z".into(), json!([dir.x, dir.y, dir.z]));
            }
            return Ok(Ref { origin: origin_param(s, Some(&straight), cmd)?, shape: Shape::Line });
        }
        if let Some(circle) = solidcraft_kernel::edge_circle(body, e) {
            let mut round = v.clone();
            if let Some(m) = round.as_object_mut() {
                m.remove("edge");
                m.insert("circle".into(), json!(p));
            }
            // The round end of a cylinder, which follows the cylinder by name.
            if let Ok(origin) = origin_param(s, Some(&round), cmd)
                && joints::resolve_origin(&s.doc, &st, &origin).is_ok()
            {
                return Ok(Ref { origin, shape: Shape::Circle });
            }
            // A rim with no cylinder beside it (a pin's end past its chamfer): its centre, z along
            // its axis out of the material, as a joint origin picked on such a rim stays a point.
            let axis = Vec3::new(circle.axis.x, circle.axis.y, circle.axis.z);
            let step = (circle.radius * 0.05).max(1e-3);
            let inside = |q: Vec3| matches!(body.classify_point(q), Ok(solidcraft_kernel::PointClass::Inside));
            let out = if inside(circle.center + axis * step) && !inside(circle.center - axis * step) { axis * -1.0 } else { axis };
            let back = solidcraft_doc::mat_inverse(&to_body).unwrap_or(solidcraft_doc::IDENTITY);
            let centre = solidcraft_doc::apply_point(&world, solidcraft_doc::apply_point(&back, circle.center));
            let z = solidcraft_doc::apply_vector(&world, solidcraft_doc::apply_vector(&back, out));
            let mut point = v.clone();
            if let Some(m) = point.as_object_mut() {
                m.remove("edge");
                m.insert("point".into(), json!([centre.x, centre.y, centre.z]));
                m.insert("z".into(), json!([z.x, z.y, z.z]));
            }
            return Ok(Ref { origin: origin_param(s, Some(&point), cmd)?, shape: Shape::Circle });
        }
        return Err(bad(cmd, "only straight and round edges can be mated so far"));
    }
    // Anything else as joint.create reads it: a point, a datum plane or axis, a round end, a named origin.
    let origin = origin_param(s, Some(v), cmd)?;
    let shape = if at("circle").is_some() {
        Shape::Circle
    } else {
        match RefShape::of(&origin.snap) {
            RefShape::Plane => Shape::Plane,
            RefShape::Round => Shape::Circle,
            RefShape::Axis | RefShape::Edge => Shape::Line,
            RefShape::Point => Shape::Point,
        }
    };
    Ok(Ref { origin, shape })
}

/// The occurrence a world point `p` is picked on (as joint.create finds it), and the point in that
/// occurrence's own coordinates.
fn located(s: &Session, v: &Value, p: Vec3, cmd: &str) -> Result<(JointOrigin, Vec3)> {
    let mut probe = serde_json::Map::new();
    for key in ["occurrence", "path"] {
        if let Some(x) = v.get(key) {
            probe.insert(key.into(), x.clone());
        }
    }
    probe.insert("point".into(), json!(p));
    let on = origin_param(s, Some(&Value::Object(probe)), cmd)?;
    let world = if on.part() == 0 { solidcraft_doc::IDENTITY } else { joints::occurrence_world(&s.doc, on.part()) };
    let inv = solidcraft_doc::mat_inverse(&world).ok_or_else(|| bad(cmd, "occurrence transform"))?;
    Ok((on, solidcraft_doc::apply_point(&inv, p)))
}

/// The bodies of an occurrence's component, each with the map from the component's coordinates
/// to the body's own (a body moved in from another component keeps its offset).
fn bodies<'a>(s: &Session, st: &'a solidcraft_doc::ModelState, occurrence: u64) -> Vec<(&'a solidcraft_kernel::Body, solidcraft_doc::Mat)> {
    let component = component_of(s, occurrence);
    st.bodies
        .iter()
        .filter(|b| s.doc.model_body_component(b) == component)
        .map(|b| {
            let to_body = match s.doc.model_body_offset(b) {
                Some(m) => solidcraft_doc::mat_inverse(m).unwrap_or(solidcraft_doc::IDENTITY),
                None => solidcraft_doc::IDENTITY,
            };
            (&b.body, to_body)
        })
        .collect()
}
