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
         straight edge on a cylinder's axis); offset?: the gap (mm or an expression); angle?: deg (for angle); flip?: turn \
         the moving part over (two faces flush instead of against each other); lock?: true stops the turn of two parts on one \
         axis; limits?: [min, max] of the gap (mm) instead of one gap: the part slides between them; name?; quiet? → \
         joint, name, rule, kind, fell_back, dof, conflicts, fixed",
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
    let limits = match p.get("limits") {
        None | Some(Value::Null) => None,
        Some(Value::Array(pair)) if pair.len() == 2 => {
            let lo = pair.first().map(|v| value(s, v, false, cmd)).transpose()?.unwrap_or(0.0);
            let hi = pair.get(1).map(|v| value(s, v, false, cmd)).transpose()?.unwrap_or(0.0);
            if lo > hi {
                return Err(bad(cmd, "`limits` must be [min, max] with min no larger than max"));
            }
            Some((lo, hi))
        }
        Some(_) => return Err(bad(cmd, "`limits` must be [min, max]")),
    };
    let mode = str_(p, "mode").unwrap_or("auto");
    if !matches!(mode, "auto" | "angle" | "tangent" | "axial") {
        return Err(bad(cmd, "`mode` must be auto, angle, tangent or axial"));
    }
    let flip = bool_(p, "flip") == Some(true);
    let lock = bool_(p, "lock") == Some(true);
    let tries = rules(&moving, &target, mode, flip, lock, cmd)?;
    if mode == "angle" && angle.is_none() {
        return Err(bad(cmd, "the angle mode needs `angle`"));
    }
    if limits.is_some() && !tries.first().is_some_and(|t| matches!(t.kind, JointKind::Planar)) {
        return Err(bad(cmd, "limits are taken for the gap between two flat faces"));
    }
    // A gap between limits is a parallel joint's free distance, held between them.
    let mut tries = tries;
    if limits.is_some()
        && let Some(t) = tries.first_mut()
    {
        t.kind = JointKind::Parallel;
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
    let sideless = matches!(t.kind, JointKind::Cylindrical | JointKind::Slider) || (t.kind == JointKind::Parallel && t.rule == "parallel");
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
    match t.kind {
        JointKind::Angle => j.angle = angle.unwrap_or(0.0),
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
    add_joint(s, j, &q)
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

/// The world direction of a joint origin's z, where its part sits now.
fn world_z(s: &Session, st: &solidcraft_doc::ModelState, o: &JointOrigin) -> Option<Vec3> {
    let frame = joints::resolve_origin(&s.doc, st, o).ok()?;
    let world = solidcraft_doc::mat_mul(&frame, &joints::occurrence_world(&s.doc, o.occurrence));
    let z = world.get(2)?;
    Some(Vec3::new(z[0], z[1], z[2]))
}

/// The ways of mating `moving` to `target`, the one asked for first and the fallback after it.
fn rules(moving: &Ref, target: &Ref, mode: &str, flip: bool, lock: bool, cmd: &str) -> Result<Vec<Try>> {
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
    };
    let parallel = |flip| make(JointKind::Parallel, t, m, flip, 0.0, false, Some(Constraint::Parallel), "parallel");
    let axial = || {
        let kind = if lock { JointKind::Slider } else { JointKind::Cylindrical };
        vec![make(kind, t, m, flip, 0.0, false, Some(Constraint::Concentric), "concentric"), parallel(flip)]
    };
    // The side that is the cylinder (or plane, or axis) the other rests on, as side a.
    let onto = |base: &Ref, other: &Ref| (base.origin.clone(), other.origin.clone());
    let out = match (moving.shape, target.shape) {
        // Two flat faces (a round edge's plane counts as one): against each other, flush, or at an angle.
        (Plane, Plane) | (Plane, Circle) | (Circle, Plane) if mode == "angle" => {
            vec![make(JointKind::Angle, t, m, flip, 0.0, false, Some(Constraint::Angle), "angle")]
        }
        (Plane, Plane) | (Plane, Circle) | (Circle, Plane) => {
            let rule = if flip { "flush" } else { "mate" };
            let tag = if flip { Constraint::Flush } else { Constraint::Mate };
            vec![make(JointKind::Planar, t, m, flip, 0.0, false, Some(tag), rule), parallel(flip)]
        }
        // A cylinder on a flat face.
        (Plane, Cylinder { .. }) | (Cylinder { .. }, Plane) => {
            let (plane, round) = if moving.shape == Plane { (moving, target) } else { (target, moving) };
            let (a, b) = onto(plane, round);
            let radius = match round.shape {
                Cylinder { radius, .. } => radius,
                _ => 0.0,
            };
            vec![make(JointKind::LineOnPlane, &a, &b, false, radius, false, Some(Constraint::Tangent), "tangent")]
        }
        // A straight edge flat on a face, a point on a face.
        (Plane, Line) | (Line, Plane) => {
            let (plane, line) = if moving.shape == Plane { (moving, target) } else { (target, moving) };
            let (a, b) = onto(plane, line);
            vec![make(JointKind::LineOnPlane, &a, &b, false, 0.0, false, None, "line_on_plane")]
        }
        (Plane, Point) | (Point, Plane) => {
            let (plane, point) = if moving.shape == Plane { (moving, target) } else { (target, moving) };
            let (a, b) = onto(plane, point);
            vec![make(JointKind::PointOnPlane, &a, &b, false, 0.0, false, Some(Constraint::Distance), "point_on_plane")]
        }
        // Two cylinders: on one axis, or side by side.
        (Cylinder { radius: ra, hole: ha }, Cylinder { radius: rb, hole: hb }) if mode == "tangent" => {
            // One inside the other (a shaft in a hole) touches inside: the radii's difference.
            let (base, inward) = if ha != hb { ((ra - rb).abs(), true) } else { (ra + rb, false) };
            vec![make(JointKind::LineOnCylinder, t, m, false, base, inward, None, "tangent")]
        }
        // A straight edge on a cylinder: resting on it, or on its axis.
        (Line, Cylinder { radius, hole }) | (Cylinder { radius, hole }, Line) if mode != "axial" => {
            let (round, line) = if moving.shape == Line { (target, moving) } else { (moving, target) };
            let (a, b) = onto(round, line);
            vec![make(JointKind::LineOnCylinder, &a, &b, false, radius, hole, None, "tangent")]
        }
        // A point on a cylinder's wall.
        (Point, Cylinder { radius, hole }) | (Cylinder { radius, hole }, Point) => {
            let (round, point) = if moving.shape == Point { (target, moving) } else { (moving, target) };
            let (a, b) = onto(round, point);
            vec![make(JointKind::PointOnCylinder, &a, &b, false, radius, hole, None, "point_on_cylinder")]
        }
        // Two round edges: one centre on one axis, turning (or locked).
        (Circle, Circle) => {
            let kind = if lock { JointKind::Rigid } else { JointKind::Revolute };
            let mut out = vec![make(kind, t, m, flip, 0.0, false, Some(Constraint::Insert), "insert")];
            out.extend(axial());
            out
        }
        // Any two other lines: one axis.
        (a, b) if a.axial() && b.axial() => axial(),
        // A point on an axis.
        (Point, b) if b.axial() => vec![make(JointKind::PointOnLine, t, m, false, 0.0, false, Some(Constraint::PointOnAxis), "point_on_axis")],
        (a, Point) if a.axial() => vec![make(JointKind::PointOnLine, m, t, false, 0.0, false, Some(Constraint::PointOnAxis), "point_on_axis")],
        // Two points: on one spot, turning any way.
        (Point, Point) => vec![make(JointKind::Ball, t, m, false, 0.0, false, None, "coincident")],
        _ => return Err(bad(cmd, "these two picks can't be mated")),
    };
    if mode == "angle" && !out.first().is_some_and(|t| t.kind == JointKind::Angle) {
        return Err(bad(cmd, "the angle mode takes two flat faces"));
    }
    if mode == "tangent" && !out.first().is_some_and(|t| matches!(t.kind, JointKind::LineOnCylinder | JointKind::LineOnPlane)) {
        return Err(bad(cmd, "the tangent mode takes two cylinders, or a cylinder and a flat face"));
    }
    Ok(out)
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
