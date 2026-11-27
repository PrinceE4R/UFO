//! High-fidelity CAD surface & B-rep decoding modeled on Tangency Labs SolidCraft's
//! STEP, IGES, CADIR, and IFC import pipelines.

use std::collections::{HashMap, HashSet};
use std::f64::consts::TAU;
use std::fmt::Write as _;

use solidcore::eval::geometry::{Curve as ECurve, Curve2 as ECurve2, Frame as EFrame, Surface as ESurface};
use solidcore::eval::{CurveEval, SurfaceEval};
use solidcore_ops::pcurve::{exact_pcurves, pcurves_within, EdgeUse, TrimFace, Trims};
use solidcore_record::brep::{Brep, Curve as RCurve, Curve2 as RCurve2, Frame as RFrame, Surface as RSurface};
use solidcore_step::assembly::Solid;
use solidcore_step::p21::{Entity, Exchange, Param};
use solidcore_step::topology::{
    is_body_item, items as step_model_items, named_face, read_faces_with, read_solid_as_sheets,
    read_solid_with, read_solid_without, roots as step_roots,
};
use solidcore_step::units;
use solidcore_tess::MeshTolerance;

use crate::decoders::mesh3d::{CAD_BODY_PALETTE, MAX_RENDER_TRIS, MeshBuild};
use crate::decoders::text::decode_to_utf8;
use crate::model::{MeshBatch, MetadataEntry, SceneLayer3d, SceneMaterialInfo, ViewerPayload};

// ---------------------------------------------------------------------------
// 1. Solidcore Record -> Evaluator conversion + Dropped Curve Pcurve Trims
// ---------------------------------------------------------------------------

fn rframe_to_eval(f: &RFrame) -> EFrame {
    EFrame {
        origin: f.origin,
        x_axis: f.x_axis,
        y_axis: f.y_axis,
    }
}

fn rcurve_to_eval(c: &RCurve, depth: usize) -> Option<ECurve> {
    if depth > 8 {
        return None;
    }
    Some(match c {
        RCurve::Line { start, end } => ECurve::Line {
            start: *start,
            end: *end,
        },
        RCurve::Circle { frame, radius } => ECurve::Circle {
            frame: rframe_to_eval(frame),
            radius: *radius,
        },
        RCurve::Ellipse { frame, radii } => ECurve::Ellipse {
            frame: rframe_to_eval(frame),
            radii: *radii,
        },
        RCurve::Polyline { points } => ECurve::Polyline {
            points: points.clone(),
        },
        RCurve::Spline { knots, points } => ECurve::Spline {
            knots: knots.clone(),
            points: points.clone(),
        },
        RCurve::RationalSpline { knots, homogeneous } => ECurve::RationalSpline {
            knots: knots.clone(),
            homogeneous: homogeneous.clone(),
        },
        RCurve::SurfaceIntersection {
            surfaces,
            leader,
            tolerance,
        } => {
            let s0 = Box::new(rsurface_to_eval(&surfaces[0], depth + 1)?);
            let s1 = Box::new(rsurface_to_eval(&surfaces[1], depth + 1)?);
            let ldr = Box::new(rcurve_to_eval(leader, depth + 1)?);
            ECurve::SurfaceIntersection {
                surfaces: [s0, s1],
                leader: ldr,
                tolerance: *tolerance,
            }
        }
    })
}

fn rsurface_to_eval(s: &RSurface, depth: usize) -> Option<ESurface> {
    if depth > 8 {
        return None;
    }
    Some(match s {
        RSurface::Plane {
            origin,
            u_axis,
            v_axis,
        } => ESurface::Plane {
            origin: *origin,
            u_axis: *u_axis,
            v_axis: *v_axis,
        },
        RSurface::PlaneThrough {
            origin,
            u_point,
            v_point,
        } => ESurface::PlaneThrough {
            origin: *origin,
            u_point: *u_point,
            v_point: *v_point,
        },
        RSurface::Cylinder { frame, radius } => ESurface::Cylinder {
            frame: rframe_to_eval(frame),
            radius: *radius,
        },
        RSurface::Cone {
            frame,
            radius_at_origin,
            half_angle,
        } => ESurface::Cone {
            frame: rframe_to_eval(frame),
            radius_at_origin: *radius_at_origin,
            half_angle: *half_angle,
        },
        RSurface::Sphere { frame, radius } => ESurface::Sphere {
            frame: rframe_to_eval(frame),
            radius: *radius,
        },
        RSurface::Torus {
            frame,
            major_radius,
            minor_radius,
            tube_start,
        } => ESurface::Torus {
            frame: rframe_to_eval(frame),
            major_radius: *major_radius,
            minor_radius: *minor_radius,
            tube_start: *tube_start,
        },
        RSurface::BallPatch { center, radius } => ESurface::BallPatch {
            center: *center,
            radius: *radius,
        },
        RSurface::Spline {
            u_knots,
            v_knots,
            points,
        } => ESurface::Spline {
            u_knots: u_knots.clone(),
            v_knots: v_knots.clone(),
            points: points.clone(),
        },
        RSurface::RationalSpline {
            u_knots,
            v_knots,
            homogeneous,
        } => ESurface::RationalSpline {
            u_knots: u_knots.clone(),
            v_knots: v_knots.clone(),
            homogeneous: homogeneous.clone(),
        },
        RSurface::PlacedRevolution {
            profile,
            origin,
            axis,
            matrix,
            forward,
        } => ESurface::PlacedRevolution {
            profile: Box::new(rcurve_to_eval(profile, depth + 1)?),
            origin: *origin,
            axis: *axis,
            matrix: *matrix,
            forward: *forward,
        },
        RSurface::Blend { .. } => return None,
    })
}

/// Unwrap angle/periodic coordinates smoothly relative to `prev` and `period`.
#[inline]
fn unwrap_periodic(val: f64, prev: Option<f64>, period: Option<f64>) -> f64 {
    let (Some(p), Some(per)) = (prev, period) else {
        return val;
    };
    if !(per > 0.0 && val.is_finite() && p.is_finite()) {
        return val;
    }
    let k = ((p - val) / per).round();
    val + k * per
}

/// Drop an arbitrary 3D edge curve onto `seval` over `interval = [t0, t1]` as a piecewise linear
/// (degree-1 clamped) 2D B-spline `RCurve2::Spline`, refining where lift chord error exceeds `tol`.
fn drop_edge_to_pcurve(
    seval: &SurfaceEval,
    ceval: &CurveEval,
    interval: [f64; 2],
    tol: f64,
    prev_uv: &mut Option<[f64; 2]>,
) -> Option<(RCurve2, f64)> {
    let [t0, t1] = interval;
    if !(t0.is_finite() && t1.is_finite() && t0 < t1) {
        return None;
    }
    let dom = seval.domain();
    let sample_at = |t: f64, seed: Option<[f64; 2]>| -> Option<([f64; 2], f64)> {
        let pt3 = ceval.point(t)?;
        let raw_uv = seval.project(pt3, seed, f64::INFINITY)?;
        let u = unwrap_periodic(raw_uv[0], seed.map(|s| s[0]), dom.period[0]);
        let v = unwrap_periodic(raw_uv[1], seed.map(|s| s[1]), dom.period[1]);
        let s3 = seval.point([u, v])?;
        let d = (pt3 - s3).length();
        Some(([u, v], d))
    };

    let mut samples: Vec<(f64, [f64; 2])> = Vec::with_capacity(65);
    let mut seed = *prev_uv;
    let initial_steps = 16usize;
    let mut max_gap = 0.0f64;
    for i in 0..=initial_steps {
        let t = if i == initial_steps {
            t1
        } else {
            t0 + (t1 - t0) * (i as f64 / initial_steps as f64)
        };
        let (uv, gap) = sample_at(t, seed)?;
        max_gap = max_gap.max(gap);
        seed = Some(uv);
        samples.push((t, uv));
    }

    // Refine segments where midpoint deviation exceeds target tolerance.
    let target_chord = tol.max(1e-4) * 0.5;
    for _pass in 0..3 {
        let mut next: Vec<(f64, [f64; 2])> = Vec::with_capacity(samples.len() * 2);
        let mut added = false;
        for w in samples.windows(2) {
            let (ta, uva) = w[0];
            let (tb, uvb) = w[1];
            next.push((ta, uva));
            let tm = 0.5 * (ta + tb);
            let uvm = [0.5 * (uva[0] + uvb[0]), 0.5 * (uva[1] + uvb[1])];
            if let (Some(c3), Some(s3)) = (ceval.point(tm), seval.point(uvm)) {
                if (c3 - s3).length() > target_chord && next.len() < 120 {
                    if let Some((uv_exact, gap)) = sample_at(tm, Some(uvm)) {
                        max_gap = max_gap.max(gap);
                        next.push((tm, uv_exact));
                        added = true;
                    }
                }
            }
        }
        if let Some(&last) = samples.last() {
            next.push(last);
        }
        samples = next;
        if !added {
            break;
        }
    }

    if samples.len() < 2 {
        return None;
    }
    *prev_uv = samples.last().map(|s| s.1);

    // Degree-1 clamped B-spline knots: [t0, t0, t1, t2, ..., tn-1, t_{n-1}] => len = points + 2.
    let mut knots = Vec::with_capacity(samples.len() + 2);
    knots.push(samples[0].0);
    for &(t, _) in &samples {
        knots.push(t);
    }
    if let Some(&(tn, _)) = samples.last() {
        knots.push(tn);
    }
    let points: Vec<[f64; 2]> = samples.into_iter().map(|(_, uv)| uv).collect();
    Some((RCurve2::Spline { knots, points }, max_gap))
}

/// Custom [`Trims`] implementation replacing simple closed-form-only trims:
/// Uses fast closed-form exact pcurves where valid, and falls back to projecting/dropping
/// arbitrary 3D boundary curves (splines on cylinders/spheres/cones/tori/NURBS, offset circles)
/// directly onto the surface evaluator.
pub(crate) struct SolidcoreTrims;

impl Trims for SolidcoreTrims {
    fn trims(&self, face: &TrimFace<'_>) -> Result<Vec<Vec<RCurve2>>, String> {
        let surf = face.nurbs.unwrap_or(face.surface);
        let build_uses = |given_override: Option<&Vec<Vec<Option<RCurve2>>>>, min_tol: f64| -> Result<Vec<Vec<(EdgeUse<'_>, Option<RCurve2>)>>, String> {
            let mut out = Vec::with_capacity(face.loops.len());
            for (li, lp) in face.loops.iter().enumerate() {
                let mut row = Vec::with_capacity(lp.len());
                for (ui, u) in lp.iter().enumerate() {
                    let edge = face
                        .edges
                        .get(u.edge as usize)
                        .ok_or_else(|| format!("missing trim edge {}", u.edge))?;
                    let curve = edge.nurbs.as_ref().unwrap_or(&edge.curve);
                    let given = given_override
                        .and_then(|g| g.get(li))
                        .and_then(|l| l.get(ui))
                        .cloned()
                        .flatten()
                        .or_else(|| u.pcurve.clone());
                    let use_rec = EdgeUse {
                        edge: u.edge,
                        curve,
                        interval: edge.interval,
                        forward: u.forward,
                        tolerance: edge.tolerance.max(face.tolerance).max(min_tol).min(1.0),
                        given: None,
                    };
                    row.push((use_rec, given));
                }
                out.push(row);
            }
            Ok(out)
        };

        // Pass 1: closed-form exact pcurves (+ already-given pcurves).
        let base_rows = build_uses(None, 0.0)?;
        let base_loops: Vec<Vec<EdgeUse<'_>>> = base_rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|(u, g)| EdgeUse {
                        given: g.as_ref(),
                        ..*u
                    })
                    .collect()
            })
            .collect();

        if let Ok(pcs) = pcurves_within(surf, &base_loops, face.tolerance, Some(face.domain)) {
            return Ok(pcs);
        }
        if let Ok(pcs) = pcurves_within(surf, &base_loops, face.tolerance, None) {
            return Ok(pcs);
        }

        // Pass 2: keep whichever uses have exact pcurves, drop the remaining uses onto the surface.
        let mut partial = exact_pcurves(surf, &base_loops, face.tolerance, Some(face.domain))
            .or_else(|_| exact_pcurves(surf, &base_loops, face.tolerance, None))
            .unwrap_or_else(|_| {
                face.loops
                    .iter()
                    .map(|l| vec![None; l.len()])
                    .collect()
            });

        let eval_surf = rsurface_to_eval(surf, 0)
            .ok_or_else(|| "surface cannot be converted for UV trim drop".to_string())?;
        let seval = SurfaceEval::new(&eval_surf).map_err(|e| e.to_string())?;
        let dom = seval.domain();
        let dom_mid = [
            match face.domain[0] {
                [a, b] if a.is_finite() && b.is_finite() => 0.5 * (a + b),
                _ => dom.range[0].map_or(0.0, |(a, b)| 0.5 * (a + b)),
            },
            match face.domain[1] {
                [a, b] if a.is_finite() && b.is_finite() => 0.5 * (a + b),
                _ => dom.range[1].map_or(0.0, |(a, b)| 0.5 * (a + b)),
            },
        ];

        let mut max_drop_gap = face.tolerance;
        for (li, lp) in face.loops.iter().enumerate() {
            let mut seed = Some(dom_mid);
            for (ui, u) in lp.iter().enumerate() {
                let slot = &mut partial[li][ui];
                if let Some(existing) = slot {
                    // Advance seed along existing pcurve's exit end for continuity.
                    let edge = &face.edges[u.edge as usize];
                    let t_exit = if u.forward { edge.interval[1] } else { edge.interval[0] };
                    if let Some(uv) = eval_rcurve2(existing, t_exit) {
                        seed = Some(uv);
                    }
                    continue;
                }
                let edge = face
                    .edges
                    .get(u.edge as usize)
                    .ok_or_else(|| format!("missing trim edge {}", u.edge))?;
                let rcurve = edge.nurbs.as_ref().unwrap_or(&edge.curve);
                let ecurve = rcurve_to_eval(rcurve, 0)
                    .ok_or_else(|| "curve cannot be converted for UV trim drop".to_string())?;
                let ceval = CurveEval::new(&ecurve).map_err(|e| e.to_string())?;
                if let Some((dropped, gap)) = drop_edge_to_pcurve(
                    &seval,
                    &ceval,
                    edge.interval,
                    edge.tolerance.max(face.tolerance),
                    &mut seed,
                ) {
                    max_drop_gap = max_drop_gap.max(gap);
                    *slot = Some(dropped);
                } else {
                    return Err("failed to project boundary edge onto surface".into());
                }
            }
        }

        let effective_tol = (max_drop_gap * 2.0).max(face.tolerance).min(0.95);
        let dropped_rows = build_uses(Some(&partial), effective_tol)?;
        let dropped_loops: Vec<Vec<EdgeUse<'_>>> = dropped_rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|(u, g)| EdgeUse {
                        given: g.as_ref(),
                        ..*u
                    })
                    .collect()
            })
            .collect();

        if let Ok(pcs) = pcurves_within(surf, &dropped_loops, effective_tol, Some(face.domain)) {
            return Ok(pcs);
        }
        if let Ok(pcs) = pcurves_within(surf, &dropped_loops, effective_tol, None) {
            return Ok(pcs);
        }

        // Fallback: if closing check was strict across a non-periodic gap, validate intervals & return directly.
        Ok(partial
            .into_iter()
            .map(|row| row.into_iter().map(|c| c.unwrap()).collect())
            .collect())
    }
}

fn eval_rcurve2(c: &RCurve2, t: f64) -> Option<[f64; 2]> {
    let ec = match c {
        RCurve2::Spline { knots, points } => ECurve2::Spline {
            knots: knots.clone(),
            points: points.clone(),
        },
        RCurve2::RationalSpline { knots, homogeneous } => ECurve2::RationalSpline {
            knots: knots.clone(),
            homogeneous: homogeneous.clone(),
        },
    };
    ec.eval(t)
}

// ---------------------------------------------------------------------------
// 2. Bounding-box size including curved edges (replicates SolidCraft body::span_of)
// ---------------------------------------------------------------------------

pub(crate) fn brep_span_size(brep: &Brep) -> f64 {
    let mut bmin = [f64::INFINITY; 3];
    let mut bmax = [f64::NEG_INFINITY; 3];
    let mut add = |p: [f64; 3]| {
        if p[0].is_finite() && p[1].is_finite() && p[2].is_finite() {
            for k in 0..3 {
                bmin[k] = bmin[k].min(p[k]);
                bmax[k] = bmax[k].max(p[k]);
            }
        }
    };
    for v in &brep.vertices {
        add(v.point);
    }
    for e in &brep.edges {
        let rcurve = e.nurbs.as_ref().unwrap_or(&e.curve);
        if let Some(ec) = rcurve_to_eval(rcurve, 0) {
            if let Ok(ce) = CurveEval::new(&ec) {
                let [t0, t1] = e.interval;
                for i in 1..4 {
                    if let Some(p) = ce.point(t0 + (t1 - t0) * (i as f64) / 4.0) {
                        add([p.x, p.y, p.z]);
                    }
                }
            }
        }
    }
    if !bmin[0].is_finite() {
        return 100.0;
    }
    let dx = bmax[0] - bmin[0];
    let dy = bmax[1] - bmin[1];
    let dz = bmax[2] - bmin[2];
    (dx * dx + dy * dy + dz * dz).sqrt().max(1.0)
}

// ---------------------------------------------------------------------------
// 3. STEP Product Structure, Styles, AP242 Tessellation & Per-Solid Recovery
// ---------------------------------------------------------------------------

type Mat4 = [[f64; 4]; 4];

const IDENT4: Mat4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// Row-vector 4x4 matrix multiply (`a` followed by `b`, translation in row 3).
fn mat4_concat(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0; 4]; 4];
    for r in 0..4 {
        for c in 0..4 {
            out[r][c] = (0..4).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    out
}

fn representation_parts(e: &Entity) -> Option<(Vec<u64>, Option<u64>)> {
    let p = if e.is_complex() {
        e.record("REPRESENTATION")
            .or_else(|| {
                e.records
                    .iter()
                    .find(|r| r.name.ends_with("REPRESENTATION") && r.params.len() >= 3)
                    .map(|r| r.params.as_slice())
            })?
    } else if e.name().ends_with("REPRESENTATION") {
        e.params()
    } else {
        return None;
    };
    let items = p.get(1)?.as_list()?.iter().filter_map(Param::as_ref_id).collect();
    Some((items, p.get(2).and_then(Param::as_ref_id)))
}

fn predefined_step_color(name: &str) -> [f32; 3] {
    match name.to_ascii_lowercase().as_str() {
        "red" => [0.85, 0.16, 0.16],
        "green" => [0.16, 0.72, 0.24],
        "blue" => [0.18, 0.44, 0.88],
        "yellow" => [0.92, 0.80, 0.15],
        "magenta" => [0.82, 0.22, 0.78],
        "cyan" => [0.16, 0.78, 0.84],
        "white" => [0.92, 0.94, 0.96],
        "black" => [0.14, 0.15, 0.17],
        _ => [0.76, 0.85, 0.90],
    }
}

fn extract_step_styles(ex: &Exchange) -> (HashMap<u64, [f32; 4]>, HashSet<u64>) {
    let mut colors: HashMap<u64, [f32; 4]> = HashMap::new();
    let mut invisible: HashSet<u64> = HashSet::new();

    for e in ex.entities.values() {
        if e.name() == "INVISIBILITY" {
            if let Some(list) = e.params().first().and_then(Param::as_list) {
                for p in list {
                    if let Some(id) = p.as_ref_id() {
                        invisible.insert(id);
                    }
                }
            }
            continue;
        }
        let is_style = e.name().ends_with("STYLED_ITEM") || e.has("STYLED_ITEM");
        if !is_style {
            continue;
        }
        let p = e.params();
        let Some(target) = p.get(2).and_then(Param::as_ref_id) else {
            continue;
        };
        let Some(style_list) = p.get(1).and_then(Param::as_list) else {
            continue;
        };
        let mut stack: Vec<u64> = style_list.iter().filter_map(Param::as_ref_id).collect();
        let mut visited = 0usize;
        let mut rgb: Option<[f32; 3]> = None;
        let mut alpha: f32 = 1.0;

        while let Some(sid) = stack.pop() {
            visited += 1;
            if visited > 64 {
                break;
            }
            let Some(se) = ex.get(sid) else {
                continue;
            };
            match se.name() {
                "COLOUR_RGB" => {
                    let sp = se.params();
                    if let (Some(r), Some(g), Some(b)) = (
                        sp.get(1).and_then(Param::as_f64),
                        sp.get(2).and_then(Param::as_f64),
                        sp.get(3).and_then(Param::as_f64),
                    ) {
                        rgb = Some([
                            r.clamp(0.0, 1.0) as f32,
                            g.clamp(0.0, 1.0) as f32,
                            b.clamp(0.0, 1.0) as f32,
                        ]);
                    }
                }
                "DRAUGHTING_PRE_DEFINED_COLOUR" => {
                    if let Some(n) = se.params().first().and_then(Param::as_str) {
                        rgb = Some(predefined_step_color(n));
                    }
                }
                "SURFACE_STYLE_TRANSPARENT" => {
                    if let Some(t) = se.params().first().and_then(Param::as_f64) {
                        alpha = (1.0 - t.clamp(0.0, 0.95)) as f32;
                    }
                }
                "CURVE_STYLE" | "POINT_STYLE" | "SURFACE_STYLE_BOUNDARY" | "SURFACE_STYLE_SILHOUETTE" => {}
                _ => {
                    for r in &se.records {
                        for param in &r.params {
                            match param {
                                Param::Ref(id) => stack.push(*id),
                                Param::List(l) | Param::Typed(_, l) => {
                                    for x in l {
                                        if let Some(id) = x.as_ref_id() {
                                            stack.push(id);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        if let Some(c) = rgb {
            colors.insert(target, [c[0], c[1], c[2], alpha]);
        }
    }

    (colors, invisible)
}

#[derive(Clone)]
struct PlacedStepRef {
    item: u64,
    ctx: Option<u64>,
    name: String,
    placement: Option<Mat4>,
}

fn step_is_tessellated_item(name: &str) -> bool {
    matches!(
        name,
        "TRIANGULATED_SURFACE_SET"
            | "COMPLEX_TRIANGULATED_SURFACE_SET"
            | "TRIANGULATED_FACE"
            | "COMPLEX_TRIANGULATED_FACE"
            | "TESSELLATED_FACE"
            | "TESSELLATED_SHELL"
            | "TESSELLATED_SOLID"
    )
}

fn collect_step_placements(ex: &Exchange) -> (Vec<PlacedStepRef>, Vec<PlacedStepRef>) {
    let mut brep_items = Vec::new();
    let mut tess_items = Vec::new();
    let mut seen_from_products: HashSet<u64> = HashSet::new();

    // Index representations -> context & items, and product names
    let mut rep_ctx: HashMap<u64, u64> = HashMap::new();
    let mut rep_items: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut first_ctx: Option<u64> = None;

    let mut ids: Vec<u64> = ex.entities.keys().copied().collect();
    ids.sort_unstable();
    for &id in &ids {
        let Some(e) = ex.get(id) else {
            continue;
        };
        if let Some((items, ctx)) = representation_parts(e) {
            if let Some(c) = ctx {
                rep_ctx.insert(id, c);
                if first_ctx.is_none() {
                    first_ctx = Some(c);
                }
            }
            rep_items.insert(id, items);
        }
    }

    let mut item_ctx: HashMap<u64, u64> = HashMap::new();
    for (&rep, items) in &rep_items {
        if let Some(&c) = rep_ctx.get(&rep) {
            for &it in items {
                item_ctx.entry(it).or_insert(c);
            }
        }
    }

    // Try solidcore_record::step::product + solidcore_ops::step_assembly first (handles AP214/AP242 trees cleanly).
    if let Ok(ps) = solidcore_record::step::product::read(ex) {
        if let Ok(placed) = solidcore_ops::step_assembly::placed_bodies(&ps) {
            let mut item_name: HashMap<u64, String> = HashMap::new();
            for rep in &ps.representations {
                for it in &rep.items {
                    if !it.name.is_empty() {
                        item_name.entry(it.entity).or_insert_with(|| it.name.clone());
                    }
                }
            }
            for pb in placed {
                seen_from_products.insert(pb.entity);
                let xf = (pb.transform != IDENT4).then_some(pb.transform);
                for piece in step_model_items(ex, pb.entity) {
                    let ctx = item_ctx.get(&piece).or_else(|| item_ctx.get(&pb.entity)).copied().or(first_ctx);
                    let name = item_name
                        .get(&piece)
                        .or_else(|| item_name.get(&pb.entity))
                        .cloned()
                        .unwrap_or_else(|| format!("STEP Body #{piece}"));
                    brep_items.push(PlacedStepRef {
                        item: piece,
                        ctx,
                        name,
                        placement: xf,
                    });
                }
            }
        }
    }

    // Collect AP242 tessellated items in representations (or if product reader skipped custom representations).
    let mut visited_rep_items = Vec::new();
    for (&rep, items) in &rep_items {
        let ctx = rep_ctx.get(&rep).copied().or(first_ctx);
        for &it in items {
            if let Some(e) = ex.get(it) {
                if step_is_tessellated_item(e.name()) {
                    let n = e
                        .params()
                        .first()
                        .and_then(Param::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    tess_items.push(PlacedStepRef {
                        item: it,
                        ctx,
                        name: if n.is_empty() {
                            format!("STEP Tessellated #{it}")
                        } else {
                            n
                        },
                        placement: None,
                    });
                } else if !seen_from_products.contains(&it) && is_body_item(e.name()) {
                    visited_rep_items.push((it, ctx));
                }
            }
        }
    }

    // Add loose representation items not already visited by product tree
    for (it, ctx) in visited_rep_items {
        seen_from_products.insert(it);
        for piece in step_model_items(ex, it) {
            let name = ex
                .get(piece)
                .and_then(|e| e.params().first().and_then(Param::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("STEP Body #{piece}"));
            brep_items.push(PlacedStepRef {
                item: piece,
                ctx: ctx.or(first_ctx),
                name,
                placement: None,
            });
        }
    }

    // Unlisted loose topology (bare SHELL, CLOSED_SHELL, OPEN_SHELL, ADVANCED_FACE, FACE_SURFACE) via roots(&ex)
    for root in step_roots(ex) {
        if seen_from_products.contains(&root) {
            continue;
        }
        if let Some(e) = ex.get(root) {
            if step_is_tessellated_item(e.name()) {
                tess_items.push(PlacedStepRef {
                    item: root,
                    ctx: first_ctx,
                    name: format!("STEP Tessellated #{root}"),
                    placement: None,
                });
                continue;
            }
        }
        for piece in step_model_items(ex, root) {
            let name = ex
                .get(piece)
                .and_then(|e| e.params().first().and_then(Param::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("STEP Surface #{piece}"));
            brep_items.push(PlacedStepRef {
                item: piece,
                ctx: item_ctx.get(&piece).copied().or(first_ctx),
                name,
                placement: None,
            });
        }
    }

    (brep_items, tess_items)
}

/// Resilient per-solid/sheet/face decoder mirroring SolidCraft `step_in/parallel.rs::read_step_item`.
fn read_step_item_resilient(
    ex: &Exchange,
    item: u64,
    context: units::Context,
) -> (Vec<Solid>, usize) {
    match read_solid_with(ex, item, context, &SolidcoreTrims) {
        Ok(mut s) => {
            if let Ok(Some((repaired, _map))) = solidcore_ops::pcurve::complete_periodic_seams(&s.brep) {
                s.brep = repaired;
            }
            return (vec![s], 0);
        }
        Err(why) => {
            // Inspect per-face readability so we never discard valid surfaces when one face breaks.
            let faces_res = read_faces_with(ex, item, context, &SolidcoreTrims);
            let mut pieces: Vec<Solid> = Vec::new();
            let mut bad_faces: HashSet<u64> = HashSet::new();

            if let Ok(face_list) = faces_res {
                for fr in face_list {
                    match fr.record {
                        Ok(brep) => {
                            pieces.push(Solid {
                                item,
                                name: String::new(),
                                product: None,
                                brep,
                                face_origin: vec![fr.face],
                                sheet: true,
                                notes: Vec::new(),
                                turned: Vec::new(),
                            });
                        }
                        Err(_) => {
                            bad_faces.insert(fr.face);
                        }
                    }
                }
            }

            if bad_faces.is_empty() && !why.contains("face #") {
                if let Ok(sheet) = read_solid_as_sheets(ex, item, context, &SolidcoreTrims) {
                    return (vec![sheet], 0);
                }
            }

            let mut skip = bad_faces.clone();
            if let Some(fid) = named_face(&why) {
                skip.insert(fid);
            }
            if !skip.is_empty() {
                for _round in 0..8 {
                    match read_solid_without(ex, item, context, &SolidcoreTrims, &skip) {
                        Ok(partial) => {
                            return (vec![partial], skip.len());
                        }
                        Err(w2) => {
                            if let Some(fid) = named_face(&w2) {
                                if skip.insert(fid) {
                                    continue;
                                }
                            }
                            if w2.contains("not closed") {
                                if let Ok(sheet) =
                                    read_solid_as_sheets(ex, item, context, &SolidcoreTrims)
                                {
                                    return (vec![sheet], skip.len());
                                }
                            }
                            break;
                        }
                    }
                }
            }

            if !pieces.is_empty() {
                let err_cnt = bad_faces.len().max(1);
                return (pieces, err_cnt);
            }
            (Vec::new(), 1)
        }
    }
}

/// Extract AP242 tessellated coordinates and triangles.
fn append_ap242_tessellated(
    ex: &Exchange,
    tref: &PlacedStepRef,
    context: units::Context,
    color: [f32; 4],
    visible: bool,
    idx: usize,
    build: &mut MeshBuild,
) -> bool {
    let Some(e) = ex.get(tref.item) else {
        return false;
    };
    let len_scale = context.units.len;
    let mut coords: Vec<[f64; 3]> = Vec::new();
    let mut tri_indices: Vec<[usize; 3]> = Vec::new();

    let read_coords_list = |cid: u64, out: &mut Vec<[f64; 3]>| {
        if let Some(ce) = ex.get(cid) {
            if let Some(list) = ce.params().get(1).and_then(Param::as_list) {
                for pt in list {
                    if let Some(xyz) = pt.as_list() {
                        if xyz.len() >= 3 {
                            if let (Some(x), Some(y), Some(z)) = (
                                xyz[0].as_f64(),
                                xyz[1].as_f64(),
                                xyz[2].as_f64(),
                            ) {
                                out.push([x * len_scale, y * len_scale, z * len_scale]);
                            }
                        }
                    }
                }
            }
        }
    };

    let mut stack = vec![tref.item];
    let mut visited = 0usize;
    while let Some(id) = stack.pop() {
        visited += 1;
        if visited > 10_000 {
            break;
        }
        let Some(ent) = ex.get(id) else {
            continue;
        };
        let p = ent.params();
        match ent.name() {
            "TESSELLATED_SOLID" | "TESSELLATED_SHELL" | "TESSELLATED_CONNECTING_EDGE" => {
                for prm in p {
                    if let Param::List(l) = prm {
                        for x in l {
                            if let Some(r) = x.as_ref_id() {
                                stack.push(r);
                            }
                        }
                    } else if let Some(r) = prm.as_ref_id() {
                        stack.push(r);
                    }
                }
            }
            "TRIANGULATED_SURFACE_SET"
            | "COMPLEX_TRIANGULATED_SURFACE_SET"
            | "TRIANGULATED_FACE"
            | "COMPLEX_TRIANGULATED_FACE"
            | "TESSELLATED_FACE" => {
                coords.clear();
                for prm in p {
                    if let Some(cid) = prm.as_ref_id() {
                        if ex.get(cid).is_some_and(|c| c.name().contains("COORDINATES_LIST")) {
                            read_coords_list(cid, &mut coords);
                        }
                    }
                }
                for prm in p {
                    if let Some(rows) = prm.as_list() {
                        for row in rows {
                            if let Some(tup) = row.as_list() {
                                if tup.len() >= 3 {
                                    if let (Some(a), Some(b), Some(c)) = (
                                        tup[0].as_i64().and_then(|v| usize::try_from(v - 1).ok()),
                                        tup[1].as_i64().and_then(|v| usize::try_from(v - 1).ok()),
                                        tup[2].as_i64().and_then(|v| usize::try_from(v - 1).ok()),
                                    ) {
                                        tri_indices.push([a, b, c]);
                                    }
                                }
                            }
                        }
                    }
                }
                if !coords.is_empty() && !tri_indices.is_empty() {
                    let xf = tref.placement;
                    let start_v = (build.positions.len() / 3) as u32;
                    for tri in tri_indices.drain(..).take(MAX_RENDER_TRIS) {
                        if let (Some(a), Some(b), Some(c)) = (
                            coords.get(tri[0]).copied(),
                            coords.get(tri[1]).copied(),
                            coords.get(tri[2]).copied(),
                        ) {
                            let ta = xform_arr(xf.as_ref(), a);
                            let tb = xform_arr(xf.as_ref(), b);
                            let tc = xform_arr(xf.as_ref(), c);
                            let n = tri_normal(ta, tb, tc);
                            for pt in [ta, tb, tc] {
                                build.positions.extend_from_slice(&pt);
                                build.normals.extend_from_slice(&n);
                            }
                        }
                    }
                    let end_v = (build.positions.len() / 3) as u32;
                    let vcount = (end_v - start_v) as usize;
                    if vcount > 0 {
                        let bi = build.batches.len();
                        build.batches.push(MeshBatch {
                            start_vertex: start_v,
                            vertex_count: vcount as u32,
                            base_color: color,
                            texture_index: None,
                        });
                        build.layers.push(SceneLayer3d {
                            id: format!("step_tess_{idx}_{}", build.layers.len()),
                            name: tref.name.clone(),
                            kind: "AP242 Tessellated".into(),
                            visible,
                            parent_id: None,
                            material_names: Vec::new(),
                            texture_names: Vec::new(),
                            vertex_count: vcount,
                            triangle_count: vcount / 3,
                            batch_indices: vec![bi],
                        });
                    }
                }
            }
            _ => {}
        }
    }
    let _ = e;
    !build.positions.is_empty()
}

#[inline]
fn xform_arr(m: Option<&Mat4>, p: [f64; 3]) -> [f32; 3] {
    match m {
        Some(m) => [
            (m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0]) as f32,
            (m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1]) as f32,
            (m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2]) as f32,
        ],
        None => [p[0] as f32, p[1] as f32, p[2] as f32],
    }
}

#[inline]
fn tri_normal(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let nx = ab[1] * ac[2] - ab[2] * ac[1];
    let ny = ab[2] * ac[0] - ab[0] * ac[2];
    let nz = ab[0] * ac[1] - ab[1] * ac[0];
    let len = (nx * nx + ny * ny + nz * nz).sqrt().max(1e-12);
    [nx / len, ny / len, nz / len]
}

// ---------------------------------------------------------------------------
// 4. Unified Solidcore Tessellator (supports instancing, per-face fallback & colors)
// ---------------------------------------------------------------------------

pub(crate) struct SolidcoreBodyItem<'a> {
    pub(crate) cache_key: usize,
    pub(crate) name: String,
    pub(crate) solid: &'a Solid,
    pub(crate) placement: Option<Mat4>,
    pub(crate) visible: bool,
    pub(crate) color: [f32; 4],
    pub(crate) face_colors: Option<&'a HashMap<u64, [f32; 4]>>,
}

#[derive(Clone)]
struct EvaluatedMesh {
    positions: Vec<solidcore_record::Vec3>,
    normals: Vec<solidcore_record::Vec3>,
    triangles: Vec<[u32; 3]>,
    tri_face: Vec<u32>,
    edges: Vec<Vec<solidcore_record::Vec3>>,
    edge_visible: Vec<bool>,
    refused_faces: usize,
}

#[inline]
fn xform_pos_mat4(m: Option<&Mat4>, p: solidcore_record::Vec3) -> [f32; 3] {
    match m {
        Some(m) => [
            (m[0][0] * p.x + m[1][0] * p.y + m[2][0] * p.z + m[3][0]) as f32,
            (m[0][1] * p.x + m[1][1] * p.y + m[2][1] * p.z + m[3][1]) as f32,
            (m[0][2] * p.x + m[1][2] * p.y + m[2][2] * p.z + m[3][2]) as f32,
        ],
        None => [p.x as f32, p.y as f32, p.z as f32],
    }
}

#[inline]
fn xform_norm_mat4(m: Option<&Mat4>, n: solidcore_record::Vec3) -> [f32; 3] {
    match m {
        Some(m) => {
            let x = m[0][0] * n.x + m[1][0] * n.y + m[2][0] * n.z;
            let y = m[0][1] * n.x + m[1][1] * n.y + m[2][1] * n.z;
            let z = m[0][2] * n.x + m[1][2] * n.y + m[2][2] * n.z;
            let len = (x * x + y * y + z * z).sqrt().max(1e-12);
            [(x / len) as f32, (y / len) as f32, (z / len) as f32]
        }
        None => [n.x as f32, n.y as f32, n.z as f32],
    }
}

fn evaluate_solid_cached(solid: &Solid, coarse_mult: f64) -> Option<EvaluatedMesh> {
    let size = brep_span_size(&solid.brep);
    let tol = if coarse_mult <= 1.01 {
        MeshTolerance::display_of(size)
    } else {
        MeshTolerance::chord((size * 1e-3 * coarse_mult).max(5e-3))
    };

    let (mut tmesh, mut refused) = match solidcore_tess::plan(&solid.brep, &tol, size) {
        Ok((plan, _stats)) => plan.evaluate(),
        Err(_) => {
            let coarse = MeshTolerance::chord((size * 6e-3).max(5e-3));
            let (plan, _) = solidcore_tess::plan(&solid.brep, &coarse, size).ok()?;
            plan.evaluate()
        }
    };

    // If any faces were refused at display tolerance, retry coarser so no surface holes appear.
    if !refused.is_empty() && tmesh.triangles.is_empty() {
        let coarse = MeshTolerance::chord((size * 8e-3).max(1e-2));
        if let Ok((plan, _)) = solidcore_tess::plan(&solid.brep, &coarse, size) {
            let (cmesh, cref) = plan.evaluate();
            if !cmesh.triangles.is_empty() {
                tmesh = cmesh;
                refused = cref;
            }
        }
    }

    let edge_visible: Vec<bool> = (0..tmesh.edges.len())
        .map(|ei| tmesh.edge_visible(ei))
        .collect();

    Some(EvaluatedMesh {
        positions: tmesh.positions,
        normals: tmesh.normals,
        triangles: tmesh.triangles,
        tri_face: tmesh.tri_face,
        edges: tmesh.edges,
        edge_visible,
        refused_faces: refused.len(),
    })
}

pub(crate) fn tessellate_solidcore_items(
    label: &str,
    items: &[SolidcoreBodyItem<'_>],
    kind_label: &str,
    extra_notes: usize,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let mut build = MeshBuild {
        format_label: label.into(),
        ..Default::default()
    };
    append_solidcore_items_to_build(items, kind_label, extra_notes, &mut build, metadata)?;
    Some(build.finalize())
}

fn append_solidcore_items_to_build(
    items: &[SolidcoreBodyItem<'_>],
    kind_label: &str,
    extra_notes: usize,
    build: &mut MeshBuild,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<()> {
    // Estimate face pressure so huge multi-hundred-part assemblies smoothly trade triangle density
    // rather than cutting off half the parts when MAX_RENDER_TRIS is approached.
    let total_faces_all: usize = items.iter().map(|i| i.solid.brep.faces.len()).sum();
    let coarse_mult = if total_faces_all > 15_000 {
        4.0
    } else if total_faces_all > 6_000 {
        2.5
    } else {
        1.0
    };

    let mut mesh_cache: HashMap<usize, EvaluatedMesh> = HashMap::new();
    let mut total_faces = 0usize;
    let mut refused_faces = 0usize;
    let max_tris_per_scene = MAX_RENDER_TRIS.max(600_000);

    for (idx, item) in items.iter().enumerate() {
        let eval = if let Some(cached) = mesh_cache.get(&item.cache_key) {
            cached
        } else {
            let Some(made) = evaluate_solid_cached(item.solid, coarse_mult) else {
                continue;
            };
            mesh_cache.entry(item.cache_key).or_insert(made)
        };

        total_faces += item.solid.brep.faces.len();
        refused_faces += eval.refused_faces;
        if build.positions.len() / 9 + eval.triangles.len() > max_tris_per_scene && idx > 0 {
            continue;
        }

        let placement_ref = item.placement.as_ref();
        let det_neg = placement_ref.is_some_and(|m| {
            let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
                - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
                + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
            det < 0.0
        });

        let start_v = (build.positions.len() / 3) as u32;
        let mut batch_indices = Vec::new();
        let mut current_color = item.color;
        let mut batch_start = start_v;

        for (ti, tri) in eval.triangles.iter().enumerate() {
            let (Some(p0), Some(p1), Some(p2)) = (
                eval.positions.get(tri[0] as usize).copied(),
                eval.positions.get(tri[1] as usize).copied(),
                eval.positions.get(tri[2] as usize).copied(),
            ) else {
                continue;
            };
            let (n0, n1, n2) = (
                eval.normals.get(tri[0] as usize).copied().unwrap_or(solidcore_record::Vec3::Z),
                eval.normals.get(tri[1] as usize).copied().unwrap_or(solidcore_record::Vec3::Z),
                eval.normals.get(tri[2] as usize).copied().unwrap_or(solidcore_record::Vec3::Z),
            );

            // Check per-face STEP style if present
            let tri_color = eval
                .tri_face
                .get(ti)
                .and_then(|&fi| item.solid.face_origin.get(fi as usize))
                .and_then(|origin_id| item.face_colors.and_then(|m| m.get(origin_id).copied()))
                .unwrap_or(item.color);

            if (tri_color[0] - current_color[0]).abs() > 1e-4
                || (tri_color[1] - current_color[1]).abs() > 1e-4
                || (tri_color[2] - current_color[2]).abs() > 1e-4
                || (tri_color[3] - current_color[3]).abs() > 1e-4
            {
                let cur_v = (build.positions.len() / 3) as u32;
                if cur_v > batch_start {
                    let bi = build.batches.len();
                    build.batches.push(MeshBatch {
                        start_vertex: batch_start,
                        vertex_count: cur_v - batch_start,
                        base_color: current_color,
                        texture_index: None,
                    });
                    batch_indices.push(bi);
                    batch_start = cur_v;
                }
                current_color = tri_color;
            }

            let order = if det_neg {
                [(p0, n0), (p2, n2), (p1, n1)]
            } else {
                [(p0, n0), (p1, n1), (p2, n2)]
            };
            for (p, n) in order {
                build.positions.extend_from_slice(&xform_pos_mat4(placement_ref, p));
                build.normals.extend_from_slice(&xform_norm_mat4(placement_ref, n));
            }
        }

        let end_v = (build.positions.len() / 3) as u32;
        if end_v > batch_start {
            let bi = build.batches.len();
            build.batches.push(MeshBatch {
                start_vertex: batch_start,
                vertex_count: end_v - batch_start,
                base_color: current_color,
                texture_index: None,
            });
            batch_indices.push(bi);
        }

        for (ei, poly) in eval.edges.iter().enumerate() {
            if !eval.edge_visible.get(ei).copied().unwrap_or(false)
                || build.wireframe_lines.len() >= 1_500_000
            {
                continue;
            }
            for pair in poly.windows(2) {
                build.wireframe_lines.extend_from_slice(&xform_pos_mat4(placement_ref, pair[0]));
                build.wireframe_lines.extend_from_slice(&xform_pos_mat4(placement_ref, pair[1]));
            }
        }

        let vcount = (end_v - start_v) as usize;
        build.parts.push(item.name.clone());
        let mat_name = format!("{} Surface", item.name);
        build.materials.push(SceneMaterialInfo {
            name: mat_name.clone(),
            base_color: item.color,
            metallic: 0.25,
            roughness: 0.45,
            texture_name: None,
            texture_index: None,
        });
        build.layers.push(SceneLayer3d {
            id: format!("cad_body_{idx}"),
            name: item.name.clone(),
            kind: kind_label.into(),
            visible: item.visible,
            parent_id: None,
            material_names: vec![mat_name],
            texture_names: Vec::new(),
            vertex_count: vcount,
            triangle_count: vcount / 3,
            batch_indices,
        });
    }

    if build.positions.is_empty() && build.wireframe_lines.is_empty() {
        return None;
    }

    let tri_count = build.positions.len() / 9;
    metadata.push(MetadataEntry {
        section: "Solidcore Kernel".into(),
        key: "Engine".into(),
        value: "solidcore + solidcore-tess".into(),
    });
    metadata.push(MetadataEntry {
        section: "Solidcore Kernel".into(),
        key: "Bodies / Instances".into(),
        value: items.len().to_string(),
    });
    metadata.push(MetadataEntry {
        section: "Solidcore Kernel".into(),
        key: "B-Rep Faces".into(),
        value: total_faces.to_string(),
    });
    metadata.push(MetadataEntry {
        section: "Solidcore Kernel".into(),
        key: "Tessellated Triangles".into(),
        value: tri_count.to_string(),
    });
    if refused_faces > 0 || extra_notes > 0 {
        metadata.push(MetadataEntry {
            section: "Solidcore Kernel".into(),
            key: "Recovered / Refused Faces".into(),
            value: format!("{refused_faces} / {extra_notes}"),
        });
    }
    build.cad_summary = Some(format!(
        "Solidcore B-Rep · {} bodies · {} faces · {} tris",
        items.len(),
        total_faces,
        tri_count
    ));
    Some(())
}

/// Primary STEP decoder entry point (used for `.step`, `.stp`, and translated `.iges`/`.igs`).
pub(crate) fn decode_solidcore_step_text(
    text: &str,
    label: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let ex = solidcore_step::p21::parse(text).ok()?;
    let (item_colors, invisible) = extract_step_styles(&ex);
    let (brep_refs, tess_refs) = collect_step_placements(&ex);

    // Deduplicate & read each unique (item, context) B-rep once, then instantiate placed instances.
    let mut unique_solids: HashMap<(u64, u64), (Vec<Solid>, usize)> = HashMap::new();
    let mut ctx_cache: HashMap<u64, units::Context> = HashMap::new();

    for bref in &brep_refs {
        let key = (bref.item, bref.ctx.unwrap_or(0));
        if unique_solids.contains_key(&key) {
            continue;
        }
        let context = match bref.ctx {
            Some(c) => *ctx_cache.entry(c).or_insert_with(|| units::context(&ex, c)),
            None => units::Context::default(),
        };
        unique_solids.insert(key, read_step_item_resilient(&ex, bref.item, context));
    }

    let mut total_errors = 0usize;
    let mut seen_err_keys = HashSet::new();
    for (key, (_, errs)) in &unique_solids {
        if seen_err_keys.insert(*key) {
            total_errors += errs;
        }
    }

    let mut owned_items: Vec<(usize, String, &Solid, Option<Mat4>, bool, [f32; 4])> = Vec::new();
    for (idx, bref) in brep_refs.iter().enumerate() {
        let key = (bref.item, bref.ctx.unwrap_or(0));
        let Some((solids, _)) = unique_solids.get(&key) else {
            continue;
        };
        let visible = !invisible.contains(&bref.item);
        let base_color = item_colors
            .get(&bref.item)
            .copied()
            .unwrap_or(CAD_BODY_PALETTE[idx % CAD_BODY_PALETTE.len()]);
        for (si, s) in solids.iter().enumerate() {
            let name = if solids.len() > 1 {
                if !s.name.trim().is_empty() {
                    s.name.clone()
                } else {
                    format!("{} · Face {}", bref.name, si + 1)
                }
            } else if !s.name.trim().is_empty() {
                s.name.clone()
            } else {
                bref.name.clone()
            };
            let col = item_colors.get(&s.item).copied().unwrap_or(base_color);
            // Unique cache key per (item_id, sub_solid_index)
            let cache_key = ((bref.item as usize) << 12) ^ si;
            owned_items.push((cache_key, name, s, bref.placement, visible, col));
        }
    }

    let mut build = MeshBuild {
        format_label: label.into(),
        ..Default::default()
    };

    let body_items: Vec<SolidcoreBodyItem<'_>> = owned_items
        .iter()
        .map(|(cache_key, name, s, placement, visible, color)| SolidcoreBodyItem {
            cache_key: *cache_key,
            name: name.clone(),
            solid: s,
            placement: *placement,
            visible: *visible,
            color: *color,
            face_colors: Some(&item_colors),
        })
        .collect();

    if !body_items.is_empty() {
        let _ = append_solidcore_items_to_build(
            &body_items,
            "STEP Solid / Surface",
            total_errors,
            &mut build,
            metadata,
        );
    }

    // Append AP242 tessellated geometry sets if present
    for (ti, tref) in tess_refs.iter().enumerate() {
        let ctx = match tref.ctx {
            Some(c) => units::context(&ex, c),
            None => units::Context::default(),
        };
        let vis = !invisible.contains(&tref.item);
        let col = item_colors
            .get(&tref.item)
            .copied()
            .unwrap_or(CAD_BODY_PALETTE[(body_items.len() + ti) % CAD_BODY_PALETTE.len()]);
        append_ap242_tessellated(&ex, tref, ctx, col, vis, ti, &mut build);
    }

    if build.positions.is_empty() && build.wireframe_lines.is_empty() {
        return None;
    }
    Some(build.finalize())
}

// ---------------------------------------------------------------------------
// 5. IGES 5.3 -> STEP Exchange Translator (SolidCraft crates/kernel/src/iges_in.rs)
// ---------------------------------------------------------------------------

mod iges_step {
    use super::*;

    type V3 = [f64; 3];
    type R<T> = Result<T, String>;

    #[inline]
    fn vsub(a: V3, b: V3) -> V3 {
        [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
    }
    #[inline]
    fn vadd(a: V3, b: V3) -> V3 {
        [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
    }
    #[inline]
    fn vmul(a: V3, k: f64) -> V3 {
        [a[0] * k, a[1] * k, a[2] * k]
    }
    #[inline]
    fn vdot(a: V3, b: V3) -> f64 {
        a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
    }
    #[inline]
    fn vnorm(a: V3) -> f64 {
        vdot(a, a).sqrt()
    }
    fn vunit(a: V3) -> Option<V3> {
        let n = vnorm(a);
        (n > 1e-300 && n.is_finite()).then(|| vmul(a, 1.0 / n))
    }
    fn perp(z: V3) -> V3 {
        let a = if z[0].abs() < 0.9 {
            [1.0, 0.0, 0.0]
        } else {
            [0.0, 1.0, 0.0]
        };
        vunit(vsub(a, vmul(z, vdot(a, z)))).unwrap_or([1.0, 0.0, 0.0])
    }

    #[derive(Clone, Copy, Debug)]
    struct Xf([[f64; 4]; 3]);
    impl Xf {
        const ID: Xf = Xf([[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]]);
        fn point(&self, p: V3) -> V3 {
            let m = &self.0;
            [0, 1, 2].map(|i| m[i][0] * p[0] + m[i][1] * p[1] + m[i][2] * p[2] + m[i][3])
        }
        fn dir(&self, d: V3) -> V3 {
            let m = &self.0;
            [0, 1, 2].map(|i| m[i][0] * d[0] + m[i][1] * d[1] + m[i][2] * d[2])
        }
        fn then(&self, inner: &Xf) -> Xf {
            let (a, b) = (&self.0, &inner.0);
            let mut m = [[0.0; 4]; 3];
            for i in 0..3 {
                for j in 0..4 {
                    m[i][j] = (0..3).map(|k| a[i][k] * b[k][j]).sum::<f64>()
                        + if j == 3 { a[i][3] } else { 0.0 };
                }
            }
            Xf(m)
        }
        fn scale(&self) -> f64 {
            let m = &self.0;
            let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
                - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
                + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
            det.abs().cbrt().max(1e-12)
        }
    }

    #[derive(Clone, Debug, Default)]
    struct De {
        typ: i64,
        xform: i64,
        label: String,
    }

    struct Iges {
        des: HashMap<usize, De>,
        params: HashMap<usize, Vec<String>>,
        unit: f64,
        resolution: f64,
    }

    fn tokens(s: &str, delim: char, rdelim: char) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == delim || c == rdelim {
                out.push(cur.trim().to_string());
                cur.clear();
                if c == rdelim {
                    return out;
                }
                i += 1;
                continue;
            }
            if (c == 'H' || c == 'h')
                && !cur.trim().is_empty()
                && cur.trim().chars().all(|d| d.is_ascii_digit())
            {
                let n: usize = cur.trim().parse().unwrap_or(0).min(chars.len());
                let start = i + 1;
                let end = (start + n).min(chars.len());
                cur = chars.get(start..end).map(|x| x.iter().collect::<String>()).unwrap_or_default();
                out.push(cur.clone());
                cur.clear();
                i = end;
                while i < chars.len() && chars[i] != delim && chars[i] != rdelim {
                    i += 1;
                }
                if i < chars.len() {
                    if chars[i] == rdelim {
                        return out;
                    }
                    i += 1;
                }
                continue;
            }
            cur.push(c);
            i += 1;
        }
        if !cur.trim().is_empty() {
            out.push(cur.trim().to_string());
        }
        out
    }

    fn num(s: &str) -> Option<f64> {
        let t = s.trim().replace(['D', 'd'], "E");
        if t.is_empty() {
            return None;
        }
        t.parse::<f64>().ok().filter(|x| x.is_finite())
    }

    fn int(s: &str) -> Option<i64> {
        let t = s.trim();
        if t.is_empty() {
            return Some(0);
        }
        t.parse::<i64>()
            .ok()
            .or_else(|| num(t).filter(|x| x.fract() == 0.0 && x.abs() < 1e15).map(|x| x as i64))
    }

    fn unit_mm(flag: i64, name: &str) -> f64 {
        match flag {
            1 => 25.4,
            2 => 1.0,
            4 => 304.8,
            5 => 1_609_344.0,
            6 => 1000.0,
            7 => 1_000_000.0,
            8 => 0.0254,
            9 => 0.001,
            10 => 10.0,
            11 => 1e-6,
            _ => match name.trim().to_ascii_uppercase().as_str() {
                "IN" | "INCH" => 25.4,
                "M" | "METER" => 1000.0,
                "CM" => 10.0,
                "FT" => 304.8,
                _ => 1.0,
            },
        }
    }

    fn parse_iges(text: &str) -> R<Iges> {
        let mut g_buf = String::new();
        let mut d_lines: Vec<String> = Vec::new();
        let mut p_lines: HashMap<usize, String> = HashMap::new();

        for raw in text.lines() {
            let line = raw.trim_end_matches(&['\r', '\n']);
            if line.len() < 73 {
                continue;
            }
            let sec = line.as_bytes()[72] as char;
            match sec {
                'G' => g_buf.push_str(&line[..72]),
                'D' => d_lines.push(line.to_string()),
                'P' => {
                    if let Ok(seq) = line[73..line.len().min(80)].trim().parse::<usize>() {
                        p_lines.entry(seq).or_default().push_str(line[..64].trim_end());
                    }
                    // Also capture chained Parameter rows pointed by DE line # in cols 64..72
                    if let Ok(de_ptr) = line[64..72].trim().parse::<usize>() {
                        p_lines.entry(10_000_000 + de_ptr).or_default().push_str(line[..64].trim_end());
                    }
                }
                _ => {}
            }
        }
        if d_lines.len() < 2 {
            return Err("IGES file has no directory entries".into());
        }

        // Parse global section delimiters & units
        let gc: Vec<char> = g_buf.chars().collect();
        let (mut delim, mut rdelim, mut pos) = (',', ';', 0usize);
        if gc.len() >= 3 && gc[0] == '1' && (gc[1] == 'H' || gc[1] == 'h') {
            delim = gc[2];
            pos = 3;
        }
        if gc.get(pos) == Some(&delim) {
            pos += 1;
        }
        if gc.len() >= pos + 3 && gc[pos] == '1' && (gc[pos + 1] == 'H' || gc[pos + 1] == 'h') {
            rdelim = gc[pos + 2];
            pos += 3;
        }
        if gc.get(pos) == Some(&delim) {
            pos += 1;
        }
        let g_rest: String = gc.get(pos..).map(|x| x.iter().collect()).unwrap_or_default();
        let gf = tokens(&g_rest, delim, rdelim);
        let uflag = gf.get(11).and_then(|s| int(s)).unwrap_or(2);
        let uname = gf.get(12).map(String::as_str).unwrap_or("");
        let unit = unit_mm(uflag, uname);
        let resolution = gf.get(16).and_then(|s| num(s)).unwrap_or(1e-4).max(1e-6);

        let de_field = |l: &str, col0: usize| -> i64 {
            let end = (col0 + 8).min(l.len());
            if col0 >= l.len() {
                return 0;
            }
            l[col0..end].trim().parse::<i64>().unwrap_or(0)
        };

        let mut des = HashMap::new();
        let mut params = HashMap::new();
        let mut idx = 0usize;
        while idx + 1 < d_lines.len() {
            let l1 = &d_lines[idx];
            let l2 = &d_lines[idx + 1];
            let seq = l1.get(73..80.min(l1.len())).and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(idx + 1);
            let typ = de_field(l1, 0);
            let pd = de_field(l1, 8).max(0) as usize;
            let xform = de_field(l1, 48);
            let lines = de_field(l2, 16).max(1) as usize;
            let label = l2.get(56..64.min(l2.len())).unwrap_or("").trim().to_string();

            des.insert(seq, De { typ, xform, label });

            // Prefer DE-pointer-keyed p_lines; fallback to sequential PD lines
            let ptxt = if let Some(s) = p_lines.get(&(10_000_000 + seq)) {
                s.clone()
            } else {
                let mut acc = String::new();
                for k in 0..lines {
                    if let Some(chunk) = p_lines.get(&(pd + k)) {
                        acc.push_str(chunk);
                    }
                }
                acc
            };
            params.insert(seq, tokens(&ptxt, delim, rdelim));
            idx += 2;
        }

        Ok(Iges { des, params, unit, resolution })
    }

    struct Out {
        lines: Vec<String>,
    }
    impl Out {
        fn add(&mut self, s: String) -> u64 {
            self.lines.push(s);
            self.lines.len() as u64
        }
        fn point(&mut self, p: V3) -> u64 {
            self.add(format!("CARTESIAN_POINT('',({},{},{}))", r(p[0]), r(p[1]), r(p[2])))
        }
        fn dir(&mut self, d: V3) -> u64 {
            self.add(format!("DIRECTION('',({},{},{}))", r(d[0]), r(d[1]), r(d[2])))
        }
        fn placement(&mut self, o: V3, z: V3, x: V3) -> u64 {
            let (a, b, c) = (self.point(o), self.dir(z), self.dir(x));
            self.add(format!("AXIS2_PLACEMENT_3D('',#{a},#{b},#{c})"))
        }
    }

    fn r(x: f64) -> String {
        let s = format!("{x:?}");
        if s.contains('.') || s.contains('e') || s.contains('E') {
            s.replace('e', "E")
        } else {
            format!("{s}.")
        }
    }

    #[derive(Clone, Copy)]
    enum Shape {
        Other,
        Line,
        Spline,
    }

    #[derive(Clone, Copy)]
    struct Seg {
        curve: u64,
        a: V3,
        b: V3,
        q: V3,
        mid: V3,
        rev: bool,
        shape: Shape,
    }
    impl Seg {
        fn along(curve: u64, f: impl Fn(f64) -> V3) -> Self {
            Seg {
                curve,
                a: f(0.0),
                b: f(1.0),
                q: f(0.25),
                mid: f(0.5),
                rev: false,
                shape: Shape::Other,
            }
        }
        fn with(mut self, s: Shape) -> Self {
            self.shape = s;
            self
        }
        fn reversed(mut self) -> Self {
            std::mem::swap(&mut self.a, &mut self.b);
            self.q = self.mid; // q approximated when reversed
            self.rev = !self.rev;
            self
        }
    }

    #[derive(Clone)]
    struct Nurbs {
        deg: usize,
        knots: Vec<f64>,
        pts: Vec<[f64; 4]>,
    }
    impl Nurbs {
        fn eval_h(&self, t: f64) -> Option<[f64; 4]> {
            let p = self.deg;
            let n = self.pts.len();
            if n == 0 || self.knots.len() != n + p + 1 {
                return None;
            }
            let (lo, hi) = (self.knots[p], self.knots[n]);
            let t = t.clamp(lo, hi);
            let mut k = p;
            while k + 1 < n && self.knots[k + 1] <= t {
                k += 1;
            }
            let mut d: Vec<[f64; 4]> = (0..=p)
                .map(|j| self.pts[(k + j).saturating_sub(p).min(n - 1)])
                .collect();
            for rr in 1..=p {
                for j in (rr..=p).rev() {
                    let i = k + j - p;
                    let (a0, a1) = (
                        self.knots[i],
                        self.knots[(i + p + 1 - rr).min(self.knots.len() - 1)],
                    );
                    let alpha = if a1 > a0 { (t - a0) / (a1 - a0) } else { 0.0 };
                    let (x, y) = (d[j - 1], d[j]);
                    d[j] = [0, 1, 2, 3].map(|c| (1.0 - alpha) * x[c] + alpha * y[c]);
                }
            }
            d.get(p).copied()
        }
        fn eval(&self, t: f64) -> Option<V3> {
            let h = self.eval_h(t)?;
            (h[3].abs() > 1e-300).then(|| [h[0] / h[3], h[1] / h[3], h[2] / h[3]])
        }
        fn range(&self) -> (f64, f64) {
            (
                self.knots.get(self.deg).copied().unwrap_or(0.0),
                self.knots.get(self.pts.len()).copied().unwrap_or(0.0),
            )
        }
    }

    struct Tr<'a> {
        f: &'a Iges,
        o: Out,
        tol: f64,
        surf_cache: HashMap<usize, u64>,
        splines: Vec<Nurbs>,
    }

    impl<'a> Tr<'a> {
        fn de(&self, seq: i64) -> R<&'a De> {
            usize::try_from(seq)
                .ok()
                .and_then(|s| self.f.des.get(&s))
                .ok_or_else(|| format!("missing entity {seq}"))
        }
        fn p(&self, seq: i64) -> R<&'a [String]> {
            usize::try_from(seq)
                .ok()
                .and_then(|s| self.f.params.get(&s))
                .map(Vec::as_slice)
                .ok_or_else(|| format!("missing params {seq}"))
        }
        fn n(&self, seq: i64, i: usize) -> R<f64> {
            self.p(seq)?.get(i).and_then(|s| num(s)).ok_or_else(|| "bad num".into())
        }
        fn i(&self, seq: i64, i: usize) -> R<i64> {
            self.p(seq)?.get(i).and_then(|s| int(s)).ok_or_else(|| "bad int".into())
        }
        fn pt(&self, seq: i64, i: usize) -> R<V3> {
            let u = self.f.unit;
            Ok([self.n(seq, i)? * u, self.n(seq, i + 1)? * u, self.n(seq, i + 2)? * u])
        }
        fn pt_raw(&self, seq: i64, i: usize, unit: f64) -> R<V3> {
            Ok([self.n(seq, i)? * unit, self.n(seq, i + 1)? * unit, self.n(seq, i + 2)? * unit])
        }
        fn own_xf(&self, seq: i64, depth: usize, unit: f64) -> R<Xf> {
            let de = self.de(seq)?;
            if de.xform <= 0 || depth > 24 {
                return Ok(Xf::ID);
            }
            let t = de.xform;
            if self.de(t)?.typ != 124 {
                return Ok(Xf::ID);
            }
            let mut m = [[0.0; 4]; 3];
            for (row, rr) in m.iter_mut().enumerate() {
                for (col, cell) in rr.iter_mut().enumerate() {
                    *cell = self.n(t, 1 + row * 4 + col)?;
                }
                rr[3] *= unit;
            }
            let parent = self.own_xf(t, depth + 1, unit)?;
            Ok(parent.then(&Xf(m)))
        }

        fn knot_text(knots: &[f64]) -> (String, String) {
            let mut ks: Vec<f64> = Vec::new();
            let mut ms: Vec<usize> = Vec::new();
            for &k in knots {
                match ks.last() {
                    Some(&l) if (k - l).abs() <= 1e-14 * (1.0 + k.abs()) => {
                        if let Some(m) = ms.last_mut() {
                            *m += 1;
                        }
                    }
                    _ => {
                        ks.push(k);
                        ms.push(1);
                    }
                }
            }
            (
                ms.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(","),
                ks.iter().map(|k| r(*k)).collect::<Vec<_>>().join(","),
            )
        }

        fn write_nurbs_curve(&mut self, nb: &Nurbs) -> u64 {
            let ids: Vec<String> = nb
                .pts
                .iter()
                .map(|h| format!("#{}", self.o.point([h[0] / h[3], h[1] / h[3], h[2] / h[3]])))
                .collect();
            let (ms, ks) = Self::knot_text(&nb.knots);
            let rational = nb.pts.iter().any(|h| (h[3] - 1.0).abs() > 1e-12);
            if rational {
                let ws: Vec<String> = nb.pts.iter().map(|h| r(h[3])).collect();
                self.o.add(format!(
                    "(BOUNDED_CURVE()B_SPLINE_CURVE({},({}),.UNSPECIFIED.,.U.,.U.)B_SPLINE_CURVE_WITH_KNOTS(({ms}),({ks}),.UNSPECIFIED.)CURVE()GEOMETRIC_REPRESENTATION_ITEM()RATIONAL_B_SPLINE_CURVE(({}))REPRESENTATION_ITEM(''))",
                    nb.deg,
                    ids.join(","),
                    ws.join(",")
                ))
            } else {
                self.o.add(format!(
                    "B_SPLINE_CURVE_WITH_KNOTS('',{},({}),.UNSPECIFIED.,.U.,.U.,({ms}),({ks}),.UNSPECIFIED.)",
                    nb.deg,
                    ids.join(",")
                ))
            }
        }

        fn nurbs_curve(&self, seq: i64, x: &Xf, unit: f64) -> R<Nurbs> {
            let k = self.i(seq, 1)?.clamp(0, 65_536) as usize;
            let m = self.i(seq, 2)?.clamp(1, 32) as usize;
            let nk = k + m + 2;
            let mut knots = Vec::with_capacity(nk);
            for j in 0..nk {
                knots.push(self.n(seq, 7 + j)?);
            }
            let base_w = 7 + nk;
            let base_p = base_w + k + 1;
            let mut pts = Vec::with_capacity(k + 1);
            for j in 0..=k {
                let w = self.n(seq, base_w + j)?.max(1e-12);
                let p = x.point(self.pt_raw(seq, base_p + 3 * j, unit)?);
                pts.push([p[0] * w, p[1] * w, p[2] * w, w]);
            }
            let mut nb = Nurbs { deg: m, knots, pts };
            let (a, b) = nb.range();
            if b > a && b - a < 1e-4 {
                let sc = 1.0 / (b - a);
                for kv in &mut nb.knots {
                    *kv = (*kv - a) * sc;
                }
            }
            Ok(nb)
        }

        fn curve(&mut self, seq: i64, xf: &Xf, depth: usize) -> R<Vec<Seg>> {
            if depth > 24 {
                return Err("curve nesting".into());
            }
            let de = self.de(seq)?;
            let x = xf.then(&self.own_xf(seq, 0, self.f.unit)?);
            match de.typ {
                110 => {
                    let (a, b) = (x.point(self.pt(seq, 1)?), x.point(self.pt(seq, 4)?));
                    let dvec = vsub(b, a);
                    let len = vnorm(dvec);
                    if len <= 1e-9 {
                        return Ok(Vec::new());
                    }
                    let (pa, d) = (self.o.point(a), vunit(dvec).unwrap_or([1.0, 0.0, 0.0]));
                    let dd = self.o.dir(d);
                    let vec_id = self.o.add(format!("VECTOR('',#{dd},{})", r(len)));
                    let cid = self.o.add(format!("LINE('',#{pa},#{vec_id})"));
                    Ok(vec![Seg::along(cid, |t| vadd(a, vmul(dvec, t))).with(Shape::Line)])
                }
                100 => {
                    let u = self.f.unit;
                    let zt = self.n(seq, 1)? * u;
                    let c = [self.n(seq, 2)? * u, self.n(seq, 3)? * u, zt];
                    let s = [self.n(seq, 4)? * u, self.n(seq, 5)? * u, zt];
                    let e = [self.n(seq, 6)? * u, self.n(seq, 7)? * u, zt];
                    let rad = vnorm(vsub(s, c));
                    if !(rad > 0.0) {
                        return Err("zero radius arc".into());
                    }
                    let a0 = (s[1] - c[1]).atan2(s[0] - c[0]);
                    let mut a1 = (e[1] - c[1]).atan2(e[0] - c[0]);
                    while a1 <= a0 + 1e-12 {
                        a1 += TAU;
                    }
                    let z = vunit(x.dir([0.0, 0.0, 1.0])).ok_or("arc frame")?;
                    let xa = vunit(x.dir([1.0, 0.0, 0.0])).ok_or("arc frame")?;
                    let ax = self.o.placement(x.point(c), z, xa);
                    let cid = self.o.add(format!("CIRCLE('',#{ax},{})", r(rad * x.scale())));
                    let at = |t: f64| x.point([c[0] + rad * t.cos(), c[1] + rad * t.sin(), zt]);
                    Ok(vec![Seg::along(cid, |t| at(a0 + (a1 - a0) * t))])
                }
                102 => {
                    let n = self.i(seq, 1)?.clamp(0, 100_000) as usize;
                    let mut out = Vec::new();
                    for k in 0..n {
                        out.extend(self.curve(self.i(seq, 2 + k)?, &x, depth + 1)?);
                    }
                    Ok(out)
                }
                126 => {
                    let nb = self.nurbs_curve(seq, &x, self.f.unit)?;
                    let (t0, t1) = nb.range();
                    let cid = self.write_nurbs_curve(&nb);
                    let seg = Seg::along(cid, |t| nb.eval(t0 + (t1 - t0) * t).unwrap_or([0.0; 3]));
                    self.splines.push(nb);
                    Ok(vec![seg.with(Shape::Spline)])
                }
                t => Err(format!("unsupported IGES curve {t}")),
            }
        }

        fn surface(&mut self, seq: i64, xf: &Xf) -> R<u64> {
            let key = usize::try_from(seq).unwrap_or(0);
            if let Some(&id) = self.surf_cache.get(&key) {
                return Ok(id);
            }
            let de = self.de(seq)?;
            let x = xf.then(&self.own_xf(seq, 0, self.f.unit)?);
            let u = self.f.unit;
            let k = x.scale();
            let id = match de.typ {
                128 => {
                    let k1 = self.i(seq, 1)?.clamp(0, 4096) as usize;
                    let k2 = self.i(seq, 2)?.clamp(0, 4096) as usize;
                    let m1 = self.i(seq, 3)?.clamp(1, 32) as usize;
                    let m2 = self.i(seq, 4)?.clamp(1, 32) as usize;
                    let (n1, n2) = (k1 + m1 + 2, k2 + m2 + 2);
                    let sk: Vec<f64> = (0..n1).map(|j| self.n(seq, 10 + j)).collect::<R<_>>()?;
                    let tk: Vec<f64> = (0..n2).map(|j| self.n(seq, 10 + n1 + j)).collect::<R<_>>()?;
                    let bw = 10 + n1 + n2;
                    let bp = bw + (k1 + 1) * (k2 + 1);
                    let mut rows = Vec::new();
                    let mut wrows = Vec::new();
                    let mut rat = false;
                    for i in 0..=k1 {
                        let mut row = Vec::new();
                        let mut wrow = Vec::new();
                        for j in 0..=k2 {
                            let idx = j * (k1 + 1) + i;
                            let w = self.n(seq, bw + idx)?.max(1e-12);
                            rat |= (w - 1.0).abs() > 1e-12;
                            let p = x.point(self.pt(seq, bp + 3 * idx)?);
                            row.push(format!("#{}", self.o.point(p)));
                            wrow.push(r(w));
                        }
                        rows.push(format!("({})", row.join(",")));
                        wrows.push(format!("({})", wrow.join(",")));
                    }
                    let (ms, ks) = Self::knot_text(&sk);
                    let (mt, kt) = Self::knot_text(&tk);
                    if rat {
                        self.o.add(format!(
                            "(BOUNDED_SURFACE()B_SPLINE_SURFACE({m1},{m2},({}),.UNSPECIFIED.,.U.,.U.,.U.)B_SPLINE_SURFACE_WITH_KNOTS(({ms}),({mt}),({ks}),({kt}),.UNSPECIFIED.)GEOMETRIC_REPRESENTATION_ITEM()RATIONAL_B_SPLINE_SURFACE(({}))REPRESENTATION_ITEM('')SURFACE())",
                            rows.join(","),
                            wrows.join(",")
                        ))
                    } else {
                        self.o.add(format!(
                            "B_SPLINE_SURFACE_WITH_KNOTS('',{m1},{m2},({}),.UNSPECIFIED.,.U.,.U.,.U.,({ms}),({mt}),({ks}),({kt}),.UNSPECIFIED.)",
                            rows.join(",")
                        ))
                    }
                }
                108 => {
                    let nn = vunit([self.n(seq, 1)?, self.n(seq, 2)?, self.n(seq, 3)?])
                        .ok_or("bad plane normal")?;
                    let d = self.n(seq, 4)? * u;
                    let (o, z) = (x.point(vmul(nn, d)), vunit(x.dir(nn)).ok_or("plane dir")?);
                    let ax = self.o.placement(o, z, perp(z));
                    self.o.add(format!("PLANE('',#{ax})"))
                }
                190 => {
                    let p_id = self.i(seq, 1)?;
                    let d_id = self.i(seq, 2)?;
                    let o = x.then(&self.own_xf(p_id, 0, u)?).point(self.pt(p_id, 1)?);
                    let z = vunit(x.dir([self.n(d_id, 1)?, self.n(d_id, 2)?, self.n(d_id, 3)?]))
                        .ok_or("plane z")?;
                    let ax = self.o.placement(o, z, perp(z));
                    self.o.add(format!("PLANE('',#{ax})"))
                }
                192 | 194 => {
                    let p_id = self.i(seq, 1)?;
                    let d_id = self.i(seq, 2)?;
                    let o = x.then(&self.own_xf(p_id, 0, u)?).point(self.pt(p_id, 1)?);
                    let z = vunit(x.dir([self.n(d_id, 1)?, self.n(d_id, 2)?, self.n(d_id, 3)?]))
                        .ok_or("cyl z")?;
                    let rad = self.n(seq, 3)? * u * k;
                    let ax = self.o.placement(o, z, perp(z));
                    if de.typ == 192 {
                        self.o.add(format!("CYLINDRICAL_SURFACE('',#{ax},{})", r(rad)))
                    } else {
                        let ang = self.n(seq, 4)?.to_radians();
                        self.o.add(format!("CONICAL_SURFACE('',#{ax},{},{})", r(rad), r(ang)))
                    }
                }
                196 => {
                    let p_id = self.i(seq, 1)?;
                    let o = x.then(&self.own_xf(p_id, 0, u)?).point(self.pt(p_id, 1)?);
                    let rad = self.n(seq, 2)? * u * k;
                    let ax = self.o.placement(o, [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
                    self.o.add(format!("SPHERICAL_SURFACE('',#{ax},{})", r(rad)))
                }
                198 => {
                    let p_id = self.i(seq, 1)?;
                    let d_id = self.i(seq, 2)?;
                    let o = x.then(&self.own_xf(p_id, 0, u)?).point(self.pt(p_id, 1)?);
                    let z = vunit(x.dir([self.n(d_id, 1)?, self.n(d_id, 2)?, self.n(d_id, 3)?]))
                        .ok_or("torus z")?;
                    let (big, small) = (self.n(seq, 3)? * u * k, self.n(seq, 4)? * u * k);
                    let ax = self.o.placement(o, z, perp(z));
                    self.o.add(format!("TOROIDAL_SURFACE('',#{ax},{},{})", r(big), r(small)))
                }
                120 => {
                    let axis = self.i(seq, 1)?;
                    let generatrix = self.i(seq, 2)?;
                    let ax_x = x.then(&self.own_xf(axis, 0, u)?);
                    let (a, b) = (ax_x.point(self.pt(axis, 1)?), ax_x.point(self.pt(axis, 4)?));
                    let d = vunit(vsub(b, a)).ok_or("rev axis")?;
                    let segs = self.curve(generatrix, &x, 1)?;
                    let seg = segs.first().ok_or("rev gen")?;
                    let (pa, dd) = (self.o.point(a), self.o.dir(d));
                    let a1 = self.o.add(format!("AXIS1_PLACEMENT('',#{pa},#{dd})"));
                    self.o.add(format!("SURFACE_OF_REVOLUTION('',#{},#{a1})", seg.curve))
                }
                122 => {
                    let segs = self.curve(self.i(seq, 1)?, &x, 1)?;
                    let seg = segs.first().ok_or("extrude dir")?;
                    let end = x.point(self.pt(seq, 2)?);
                    let v = vsub(end, seg.a);
                    let d = vunit(v).ok_or("extrude zero")?;
                    let dd = self.o.dir(d);
                    let vec_id = self.o.add(format!("VECTOR('',#{dd},{})", r(vnorm(v))));
                    self.o.add(format!("SURFACE_OF_LINEAR_EXTRUSION('',#{},#{vec_id})", seg.curve))
                }
                t => return Err(format!("unsupported IGES surface {t}")),
            };
            self.surf_cache.insert(key, id);
            Ok(id)
        }

        fn natural_boundary_128(&mut self, seq: i64, xf: &Xf) -> R<Vec<Seg>> {
            let de = self.de(seq)?;
            if de.typ != 128 {
                return Err("not a NURBS surface".into());
            }
            let x = xf.then(&self.own_xf(seq, 0, self.f.unit)?);
            let k1 = self.i(seq, 1)?.clamp(0, 4096) as usize;
            let k2 = self.i(seq, 2)?.clamp(0, 4096) as usize;
            let m1 = self.i(seq, 3)?.clamp(1, 32) as usize;
            let m2 = self.i(seq, 4)?.clamp(1, 32) as usize;
            let (n1, n2) = (k1 + m1 + 2, k2 + m2 + 2);
            let sk: Vec<f64> = (0..n1).map(|j| self.n(seq, 10 + j)).collect::<R<_>>()?;
            let tk: Vec<f64> = (0..n2).map(|j| self.n(seq, 10 + n1 + j)).collect::<R<_>>()?;
            let bw = 10 + n1 + n2;
            let bp = bw + (k1 + 1) * (k2 + 1);
            let h = |i: usize, j: usize| -> R<[f64; 4]> {
                let idx = j * (k1 + 1) + i;
                let w = self.n(seq, bw + idx)?.max(1e-12);
                let p = x.point(self.pt(seq, bp + 3 * idx)?);
                Ok([p[0] * w, p[1] * w, p[2] * w, w])
            };
            let row = |j: usize| -> R<Nurbs> {
                Ok(Nurbs {
                    deg: m1,
                    knots: sk.clone(),
                    pts: (0..=k1).map(|i| h(i, j)).collect::<R<_>>()?,
                })
            };
            let col = |i: usize| -> R<Nurbs> {
                Ok(Nurbs {
                    deg: m2,
                    knots: tk.clone(),
                    pts: (0..=k2).map(|j| h(i, j)).collect::<R<_>>()?,
                })
            };
            let mut out = Vec::new();
            for (nb, rev) in [(row(0)?, false), (col(k1)?, false), (row(k2)?, true), (col(0)?, true)] {
                let (t0, t1) = nb.range();
                let ev = |t: f64| nb.eval(t0 + (t1 - t0) * t);
                let (Some(a), Some(b), Some(mid)) = (ev(0.0), ev(1.0), ev(0.5)) else {
                    continue;
                };
                if vnorm(vsub(a, b)) <= self.tol && vnorm(vsub(a, mid)) <= self.tol {
                    continue;
                }
                let cid = self.write_nurbs_curve(&nb);
                let seg = Seg::along(cid, |t| ev(t).unwrap_or([0.0; 3]))
                    .with(Shape::Spline);
                self.splines.push(nb);
                out.push(if rev { seg.reversed() } else { seg });
            }
            Ok(out)
        }
    }

    pub(super) fn translate(text: &str) -> Option<String> {
        let f = parse_iges(text).ok()?;
        let tol = (f.resolution * f.unit).clamp(1e-5, 0.05);
        let mut t = Tr {
            f: &f,
            o: Out { lines: Vec::new() },
            tol,
            surf_cache: HashMap::new(),
            splines: Vec::new(),
        };

        let mm = t.o.add("(LENGTH_UNIT()NAMED_UNIT(*)SI_UNIT(.MILLI.,.METRE.))".into());
        let rad = t.o.add("(NAMED_UNIT(*)PLANE_ANGLE_UNIT()SI_UNIT($,.RADIAN.))".into());
        let sr = t.o.add("(NAMED_UNIT(*)SI_UNIT($,.STERADIAN.)SOLID_ANGLE_UNIT())".into());
        let unc = t.o.add(format!(
            "UNCERTAINTY_MEASURE_WITH_UNIT(LENGTH_MEASURE({}),#{mm},'distance_accuracy_value','')",
            r(tol)
        ));
        let _ctx = t.o.add(format!(
            "(GEOMETRIC_REPRESENTATION_CONTEXT(3)GLOBAL_UNCERTAINTY_ASSIGNED_CONTEXT((#{unc}))GLOBAL_UNIT_ASSIGNED_CONTEXT((#{mm},#{rad},#{sr}))REPRESENTATION_CONTEXT('',''))"
        ));

        let mut seqs: Vec<usize> = f.des.keys().copied().collect();
        seqs.sort_unstable();

        let mut referenced_128 = HashSet::new();
        for &s in &seqs {
            if let Some(de) = f.des.get(&s) {
                if de.typ == 144 {
                    if let Ok(surf) = t.i(s as i64, 1) {
                        referenced_128.insert(surf as usize);
                    }
                } else if de.typ == 143 {
                    if let Ok(surf) = t.i(s as i64, 2) {
                        referenced_128.insert(surf as usize);
                    }
                }
            }
        }

        let mut face_ids = Vec::new();
        for &s in &seqs {
            let Some(de) = f.des.get(&s) else {
                continue;
            };
            let seq = s as i64;
            if !matches!(de.typ, 144 | 143 | 128) || (de.typ == 128 && referenced_128.contains(&s)) {
                continue;
            }
            let xf = t.own_xf(seq, 0, f.unit).unwrap_or(Xf::ID);
            let mut build_face = || -> R<u64> {
                let (surf_id, loops): (u64, Vec<Vec<Seg>>) = match de.typ {
                    144 => {
                        let surf_seq = t.i(seq, 1)?;
                        let surf_id = t.surface(surf_seq, &xf)?;
                        let n1 = t.i(seq, 2)?;
                        let n2 = t.i(seq, 3)?.clamp(0, 1024) as usize;
                        let mut ls = Vec::new();
                        let outer = if n1 == 0 {
                            t.natural_boundary_128(surf_seq, &xf)?
                        } else {
                            let cseq = t.i(seq, 4)?;
                            let c = if t.de(cseq)?.typ == 142 {
                                let mc = t.i(cseq, 4)?;
                                let cx = xf.then(&t.own_xf(cseq, 0, f.unit)?);
                                t.curve(mc, &cx, 0)?
                            } else {
                                t.curve(cseq, &xf, 0)?
                            };
                            c
                        };
                        ls.push(outer);
                        for k in 0..n2 {
                            let cseq = t.i(seq, 5 + k)?;
                            let c = if t.de(cseq)?.typ == 142 {
                                let mc = t.i(cseq, 4)?;
                                let cx = xf.then(&t.own_xf(cseq, 0, f.unit)?);
                                t.curve(mc, &cx, 0)?
                            } else {
                                t.curve(cseq, &xf, 0)?
                            };
                            if !c.is_empty() {
                                ls.push(c);
                            }
                        }
                        (surf_id, ls)
                    }
                    143 => {
                        let surf_seq = t.i(seq, 2)?;
                        let surf_id = t.surface(surf_seq, &xf)?;
                        let n = t.i(seq, 3)?.clamp(0, 1024) as usize;
                        let mut ls = Vec::new();
                        for k in 0..n {
                            let bseq = t.i(seq, 4 + k)?;
                            if t.de(bseq)?.typ == 141 {
                                let ne = t.i(bseq, 4)?.clamp(0, 4096) as usize;
                                let mut bi = 5;
                                let mut segs = Vec::new();
                                for _ in 0..ne {
                                    let c = t.i(bseq, bi)?;
                                    let sense = t.i(bseq, bi + 1)?;
                                    let kp = t.i(bseq, bi + 2)?.clamp(0, 1024) as usize;
                                    bi += 3 + kp;
                                    let mut cs = t.curve(c, &xf, 0)?;
                                    if sense == 2 {
                                        cs = cs.into_iter().rev().map(Seg::reversed).collect();
                                    }
                                    segs.extend(cs);
                                }
                                if !segs.is_empty() {
                                    ls.push(segs);
                                }
                            }
                        }
                        (surf_id, ls)
                    }
                    _ => {
                        let surf_id = t.surface(seq, &Xf::ID)?;
                        let outer = t.natural_boundary_128(seq, &Xf::ID)?;
                        (surf_id, vec![outer])
                    }
                };
                let mut bounds = Vec::new();
                for (li, segs) in loops.into_iter().enumerate() {
                    if segs.is_empty() {
                        continue;
                    }
                    // Turn segments head-to-tail so loop closes cleanly
                    let mut chained = Vec::with_capacity(segs.len());
                    let mut prev_end: Option<V3> = None;
                    for mut sg in segs {
                        if let Some(pe) = prev_end {
                            if vnorm(vsub(sg.a, sg.b)) > tol
                                && vnorm(vsub(sg.b, pe)) < vnorm(vsub(sg.a, pe))
                            {
                                sg = sg.reversed();
                            }
                        }
                        prev_end = Some(sg.b);
                        chained.push(sg);
                    }
                    let verts: Vec<u64> = chained.iter().map(|s| t.o.point(s.a)).collect();
                    let vps: Vec<u64> = verts
                        .iter()
                        .map(|&pt| t.o.add(format!("VERTEX_POINT('',#{pt})")))
                        .collect();
                    let mut oes = Vec::new();
                    for (k, sg) in chained.iter().enumerate() {
                        let va = vps[k];
                        let vb = vps[(k + 1) % vps.len()];
                        let ec = t.o.add(format!(
                            "EDGE_CURVE('',#{va},#{vb},#{},{})",
                            sg.curve,
                            if sg.rev { ".F." } else { ".T." }
                        ));
                        let oe = t.o.add(format!("ORIENTED_EDGE('',*,*,#{ec},.T.)"));
                        oes.push(format!("#{oe}"));
                    }
                    if oes.is_empty() {
                        continue;
                    }
                    let el = t.o.add(format!("EDGE_LOOP('',({}))", oes.join(",")));
                    let bkind = if li == 0 {
                        "FACE_OUTER_BOUND"
                    } else {
                        "FACE_BOUND"
                    };
                    bounds.push(format!("#{}", t.o.add(format!("{bkind}('',#{el},.T.)"))));
                }
                if bounds.is_empty() {
                    return Err("empty loop".into());
                }
                let af = t.o.add(format!(
                    "ADVANCED_FACE('{}',({}),#{surf_id},.T.)",
                    de.label.replace('\'', "''"),
                    bounds.join(",")
                ));
                Ok(af)
            };
            if let Ok(fid) = build_face() {
                face_ids.push(format!("#{fid}"));
            }
        }

        if face_ids.is_empty() {
            return None;
        }
        let sh = t.o.add(format!("OPEN_SHELL('',({}))", face_ids.join(",")));
        let _sm = t.o.add(format!("SHELL_BASED_SURFACE_MODEL('IGES',(#{sh}))"));

        let mut step = String::from(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('IGES import'),'2;1');\nFILE_NAME('','',(''),(''),'UFO IGES','','');\nFILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\nENDSEC;\nDATA;\n",
        );
        for (i, l) in t.o.lines.iter().enumerate() {
            let _ = writeln!(step, "#{}={l};", i + 1);
        }
        step.push_str("ENDSEC;\nEND-ISO-10303-21;\n");
        Some(step)
    }
}

pub(crate) fn decode_iges_solidcraft(
    bytes: &[u8],
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    let step_text = iges_step::translate(&text)?;
    let payload = decode_solidcore_step_text(&step_text, "IGES CAD (Solidcore)", metadata)?;
    if let ViewerPayload::Mesh3d { triangle_count, .. } = &payload {
        if *triangle_count == 0 {
            return None;
        }
    }
    Some(payload)
}

// ---------------------------------------------------------------------------
// 6. IFC BIM Surface / Solid Extractor (FacetedBrep, Polygonal/TriangulatedFaceSet, ExtrudedAreaSolid)
// ---------------------------------------------------------------------------

fn ifc_length_scale_mm(ex: &Exchange) -> f64 {
    for e in ex.entities.values() {
        if e.name() == "IFCSIUNIT" {
            let p = e.params();
            if p.get(1).and_then(Param::as_enum) == Some("LENGTHUNIT") {
                let prefix = p.get(2).and_then(Param::as_enum);
                return match prefix {
                    Some("MILLI") => 1.0,
                    Some("CENTI") => 10.0,
                    Some("KILO") => 1_000_000.0,
                    None => 1000.0, // IFC SI metre default -> mm
                    _ => 1000.0,
                };
            }
        }
    }
    1000.0
}

fn ifc_pt(ex: &Exchange, id: u64, scale: f64) -> Option<[f64; 3]> {
    let e = ex.get(id)?;
    let list = e.params().first()?.as_list()?;
    let x = list.first()?.as_f64()? * scale;
    let y = list.get(1)?.as_f64()? * scale;
    let z = list.get(2).and_then(Param::as_f64).unwrap_or(0.0) * scale;
    Some([x, y, z])
}

fn ifc_dir(ex: &Exchange, id: u64) -> Option<[f64; 3]> {
    let e = ex.get(id)?;
    let list = e.params().first()?.as_list()?;
    let x = list.first()?.as_f64()?;
    let y = list.get(1)?.as_f64()?;
    let z = list.get(2).and_then(Param::as_f64).unwrap_or(0.0);
    let len = (x * x + y * y + z * z).sqrt();
    (len > 1e-12).then_some([x / len, y / len, z / len])
}

fn ifc_axis2(ex: &Exchange, id: u64, scale: f64) -> Option<Mat4> {
    let e = ex.get(id)?;
    let p = e.params();
    match e.name() {
        "IFCAXIS2PLACEMENT3D" => {
            let loc = ifc_pt(ex, p.first()?.as_ref_id()?, scale)?;
            let axis = p.get(1).and_then(Param::as_ref_id).and_then(|d| ifc_dir(ex, d));
            let refd = p.get(2).and_then(Param::as_ref_id).and_then(|d| ifc_dir(ex, d));
            solidcore::geom::step_matrix::frame_matrix(loc, axis, refd)
        }
        "IFCAXIS2PLACEMENT2D" => {
            let loc = ifc_pt(ex, p.first()?.as_ref_id()?, scale)?;
            let refd = p.get(1).and_then(Param::as_ref_id).and_then(|d| ifc_dir(ex, d));
            solidcore::geom::step_matrix::frame_matrix(loc, Some([0.0, 0.0, 1.0]), refd)
        }
        _ => None,
    }
}

fn ifc_local_placement(ex: &Exchange, id: u64, scale: f64, depth: usize) -> Mat4 {
    if depth > 24 {
        return IDENT4;
    }
    let Some(e) = ex.get(id) else {
        return IDENT4;
    };
    if e.name() != "IFCLOCALPLACEMENT" {
        return IDENT4;
    }
    let p = e.params();
    let parent = p
        .first()
        .and_then(Param::as_ref_id)
        .map_or(IDENT4, |pid| ifc_local_placement(ex, pid, scale, depth + 1));
    let rel = p
        .get(1)
        .and_then(Param::as_ref_id)
        .and_then(|aid| ifc_axis2(ex, aid, scale))
        .unwrap_or(IDENT4);
    mat4_concat(&rel, &parent)
}

fn project_polygon_to_plane(pts: &[[f64; 3]]) -> Option<([f64; 3], [f64; 3], [f64; 3], [f64; 3])> {
    if pts.len() < 3 {
        return None;
    }
    let o = pts[0];
    // Newell's normal
    let mut n = [0.0f64; 3];
    for i in 0..pts.len() {
        let a = pts[i];
        let b = pts[(i + 1) % pts.len()];
        n[0] += (a[1] - b[1]) * (a[2] + b[2]);
        n[1] += (a[2] - b[2]) * (a[0] + b[0]);
        n[2] += (a[0] - b[0]) * (a[1] + b[1]);
    }
    let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if !(nl > 1e-12) {
        return None;
    }
    let n = [n[0] / nl, n[1] / nl, n[2] / nl];
    let cand = if n[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let dot_nc = n[0] * cand[0] + n[1] * cand[1] + n[2] * cand[2];
    let tx = [cand[0] - n[0] * dot_nc, cand[1] - n[1] * dot_nc, cand[2] - n[2] * dot_nc];
    let xl = (tx[0] * tx[0] + tx[1] * tx[1] + tx[2] * tx[2]).sqrt();
    let u = [tx[0] / xl, tx[1] / xl, tx[2] / xl];
    let v = [
        n[1] * u[2] - n[2] * u[1],
        n[2] * u[0] - n[0] * u[2],
        n[0] * u[1] - n[1] * u[0],
    ];
    Some((o, u, v, n))
}

fn push_planar_loops(loops3d: &[Vec<[f64; 3]>], xf: Option<&Mat4>, build: &mut MeshBuild) {
    let Some(outer) = loops3d.first() else {
        return;
    };
    let Some((o, u, v, n)) = project_polygon_to_plane(outer) else {
        return;
    };
    let to_2d = |p: [f64; 3]| -> [f64; 2] {
        let d = [p[0] - o[0], p[1] - o[1], p[2] - o[2]];
        [d[0] * u[0] + d[1] * u[1] + d[2] * u[2], d[0] * v[0] + d[1] * v[1] + d[2] * v[2]]
    };
    let loops2d: Vec<Vec<[f64; 2]>> = loops3d
        .iter()
        .map(|l| l.iter().copied().map(to_2d).collect())
        .collect();
    if let Ok((pts2d, tris)) = solidcore_tess::triangulate_planar(&loops2d) {
        let p3: Vec<[f32; 3]> = pts2d
            .iter()
            .map(|&[pu, pv]| {
                xform_arr(
                    xf,
                    [o[0] + pu * u[0] + pv * v[0], o[1] + pu * u[1] + pv * v[1], o[2] + pu * u[2] + pv * v[2]],
                )
            })
            .collect();
        let nf = xform_arr(
            xf.map(|m| [
                [m[0][0], m[0][1], m[0][2], 0.0],
                [m[1][0], m[1][1], m[1][2], 0.0],
                [m[2][0], m[2][1], m[2][2], 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ])
            .as_ref(),
            n,
        );
        for [a, b, c] in tris {
            for &idx in &[a, b, c] {
                if let Some(&pt) = p3.get(idx as usize) {
                    build.positions.extend_from_slice(&pt);
                    build.normals.extend_from_slice(&nf);
                }
            }
        }
    }
    // Add crisp boundary wireframe lines for every loop
    for lp in loops3d {
        for i in 0..lp.len() {
            let a = xform_arr(xf, lp[i]);
            let b = xform_arr(xf, lp[(i + 1) % lp.len()]);
            build.wireframe_lines.extend_from_slice(&a);
            build.wireframe_lines.extend_from_slice(&b);
        }
    }
}

fn ifc_profile_loops(ex: &Exchange, pid: u64, scale: f64) -> Option<Vec<Vec<[f64; 3]>>> {
    let e = ex.get(pid)?;
    let p = e.params();
    match e.name() {
        "IFCRECTANGLEPROFILEDEF" | "IFCRECTANGLEHOLLOWPROFILEDEF" => {
            let pos = p.get(2).and_then(Param::as_ref_id).and_then(|aid| ifc_axis2(ex, aid, scale)).unwrap_or(IDENT4);
            let hx = p.get(3)?.as_f64()? * scale * 0.5;
            let hy = p.get(4)?.as_f64()? * scale * 0.5;
            let corner = |x: f64, y: f64| -> [f64; 3] {
                [
                    pos[0][0] * x + pos[1][0] * y + pos[3][0],
                    pos[0][1] * x + pos[1][1] * y + pos[3][1],
                    pos[0][2] * x + pos[1][2] * y + pos[3][2],
                ]
            };
            let outer = vec![
                corner(-hx, -hy),
                corner(hx, -hy),
                corner(hx, hy),
                corner(-hx, hy),
            ];
            Some(vec![outer])
        }
        "IFCCIRCLEPROFILEDEF" | "IFCCIRCLEHOLLOWPROFILEDEF" => {
            let pos = p.get(2).and_then(Param::as_ref_id).and_then(|aid| ifc_axis2(ex, aid, scale)).unwrap_or(IDENT4);
            let rad = p.get(3)?.as_f64()? * scale;
            let mut ring = Vec::with_capacity(24);
            for k in 0..24 {
                let th = TAU * (k as f64) / 24.0;
                let (x, y) = (rad * th.cos(), rad * th.sin());
                ring.push([
                    pos[0][0] * x + pos[1][0] * y + pos[3][0],
                    pos[0][1] * x + pos[1][1] * y + pos[3][1],
                    pos[0][2] * x + pos[1][2] * y + pos[3][2],
                ]);
            }
            Some(vec![ring])
        }
        "IFCARBITRARYCLOSEDPROFILEDEF" | "IFCARBITRARYPROFILEDEFWITHVOIDS" => {
            let mut res = Vec::new();
            let extract_poly = |cid: u64| -> Vec<[f64; 3]> {
                ex.get(cid)
                    .and_then(|ce| ce.params().first().and_then(Param::as_list))
                    .map(|pts| pts.iter().filter_map(|pr| pr.as_ref_id().and_then(|id| ifc_pt(ex, id, scale))).collect())
                    .unwrap_or_default()
            };
            if let Some(cid) = p.get(2).and_then(Param::as_ref_id) {
                let outer = extract_poly(cid);
                if outer.len() >= 3 {
                    res.push(outer);
                }
            }
            if e.name() == "IFCARBITRARYPROFILEDEFWITHVOIDS" {
                if let Some(inners) = p.get(3).and_then(Param::as_list) {
                    for in_ref in inners {
                        if let Some(cid) = in_ref.as_ref_id() {
                            let inner = extract_poly(cid);
                            if inner.len() >= 3 {
                                res.push(inner);
                            }
                        }
                    }
                }
            }
            (!res.is_empty()).then_some(res)
        }
        _ => None,
    }
}

pub(crate) fn decode_ifc_solidcore(
    bytes: &[u8],
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    if let Some(p) = decode_solidcore_step_text(&text, "IFC BIM (Solidcore B-Rep)", metadata) {
        return Some(p);
    }
    let ex = solidcore_step::p21::parse(&text).ok()?;
    let scale = ifc_length_scale_mm(&ex);

    // Map geometric items to product placement + names where referenced
    let mut item_placement: HashMap<u64, Mat4> = HashMap::new();
    let mut item_name: HashMap<u64, String> = HashMap::new();
    for e in ex.entities.values() {
        if matches!(e.name(), "IFCWALL" | "IFCWALLSTANDARDCASE" | "IFCSLAB" | "IFCBEAM" | "IFCCOLUMN" | "IFCDOOR" | "IFCWINDOW" | "IFCPLATE" | "IFCMEMBER" | "IFCBUILDINGELEMENTPROXY" | "IFCROOF" | "IFCSTAIR") {
            let p = e.params();
            let pname = p.get(2).and_then(Param::as_str).unwrap_or("").trim().to_string();
            let place = p
                .get(5)
                .and_then(Param::as_ref_id)
                .map(|pid| ifc_local_placement(&ex, pid, scale, 0))
                .unwrap_or(IDENT4);
            let Some(pds_id) = p.get(6).and_then(Param::as_ref_id) else {
                continue;
            };
            let Some(pds) = ex.get(pds_id) else {
                continue;
            };
            if let Some(reps) = pds.params().get(2).and_then(Param::as_list) {
                for rref in reps {
                    let Some(rid) = rref.as_ref_id() else { continue };
                    let Some(re) = ex.get(rid) else { continue };
                    if let Some(items) = re.params().get(3).and_then(Param::as_list) {
                        for it in items {
                            if let Some(iid) = it.as_ref_id() {
                                item_placement.insert(iid, place);
                                if !pname.is_empty() {
                                    item_name.insert(iid, pname.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut build = MeshBuild {
        format_label: "IFC BIM CAD Model".into(),
        ..Default::default()
    };

    let mut ids: Vec<u64> = ex.entities.keys().copied().collect();
    ids.sort_unstable();
    let mut item_idx = 0usize;

    for &id in &ids {
        let Some(e) = ex.get(id) else {
            continue;
        };
        let xf = item_placement.get(&id).copied();
        let name = item_name
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("{} #{id}", e.name()));
        let start_v = (build.positions.len() / 3) as u32;

        match e.name() {
            "IFCTRIANGULATEDFACESET" => {
                let p = e.params();
                let Some(pts_id) = p.first().and_then(Param::as_ref_id) else { continue };
                let Some(pts_list) = ex.get(pts_id).and_then(|ce| ce.params().first().and_then(Param::as_list)) else { continue };
                let coords: Vec<[f64; 3]> = pts_list
                    .iter()
                    .filter_map(|row| {
                        let l = row.as_list()?;
                        Some([l.first()?.as_f64()? * scale, l.get(1)?.as_f64()? * scale, l.get(2)?.as_f64()? * scale])
                    })
                    .collect();
                if let Some(tris) = p.get(3).and_then(Param::as_list) {
                    for row in tris {
                        if let Some(tup) = row.as_list() {
                            if let (Some(a), Some(b), Some(c)) = (
                                tup.first().and_then(Param::as_i64).and_then(|v| usize::try_from(v - 1).ok()),
                                tup.get(1).and_then(Param::as_i64).and_then(|v| usize::try_from(v - 1).ok()),
                                tup.get(2).and_then(Param::as_i64).and_then(|v| usize::try_from(v - 1).ok()),
                            ) {
                                if let (Some(&pa), Some(&pb), Some(&pc)) = (coords.get(a), coords.get(b), coords.get(c)) {
                                    let ta = xform_arr(xf.as_ref(), pa);
                                    let tb = xform_arr(xf.as_ref(), pb);
                                    let tc = xform_arr(xf.as_ref(), pc);
                                    let n = tri_normal(ta, tb, tc);
                                    for pt in [ta, tb, tc] {
                                        build.positions.extend_from_slice(&pt);
                                        build.normals.extend_from_slice(&n);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "IFCPOLYGONALFACESET" => {
                let p = e.params();
                let Some(pts_id) = p.first().and_then(Param::as_ref_id) else { continue };
                let Some(pts_list) = ex.get(pts_id).and_then(|ce| ce.params().first().and_then(Param::as_list)) else { continue };
                let coords: Vec<[f64; 3]> = pts_list
                    .iter()
                    .filter_map(|row| {
                        let l = row.as_list()?;
                        Some([l.first()?.as_f64()? * scale, l.get(1)?.as_f64()? * scale, l.get(2)?.as_f64()? * scale])
                    })
                    .collect();
                if let Some(faces) = p.get(2).and_then(Param::as_list) {
                    for fref in faces {
                        let Some(fid) = fref.as_ref_id() else { continue };
                        let Some(fe) = ex.get(fid) else { continue };
                        let mut loops3d = Vec::new();
                        if let Some(outer_idx) = fe.params().first().and_then(Param::as_list) {
                            let lp: Vec<[f64; 3]> = outer_idx
                                .iter()
                                .filter_map(|v| v.as_i64().and_then(|i| usize::try_from(i - 1).ok()).and_then(|k| coords.get(k).copied()))
                                .collect();
                            if lp.len() >= 3 {
                                loops3d.push(lp);
                            }
                        }
                        if fe.name() == "IFCINDEXEDPOLYGONALFACEWITHVOIDS" {
                            if let Some(inners) = fe.params().get(1).and_then(Param::as_list) {
                                for in_l in inners {
                                    if let Some(idxs) = in_l.as_list() {
                                        let lp: Vec<[f64; 3]> = idxs
                                            .iter()
                                            .filter_map(|v| v.as_i64().and_then(|i| usize::try_from(i - 1).ok()).and_then(|k| coords.get(k).copied()))
                                            .collect();
                                        if lp.len() >= 3 {
                                            loops3d.push(lp);
                                        }
                                    }
                                }
                            }
                        }
                        push_planar_loops(&loops3d, xf.as_ref(), &mut build);
                    }
                }
            }
            "IFCFACETEDBREP" | "IFCSHELLBASEDSURFACEMODEL" | "IFCFACEBASEDSURFACEMODEL" => {
                let p = e.params();
                let mut shells = Vec::new();
                for prm in p {
                    if let Some(sid) = prm.as_ref_id() {
                        shells.push(sid);
                    } else if let Some(l) = prm.as_list() {
                        for x in l {
                            if let Some(sid) = x.as_ref_id() {
                                shells.push(sid);
                            }
                        }
                    }
                }
                for sid in shells {
                    let Some(se) = ex.get(sid) else { continue };
                    let Some(flist) = se.params().first().and_then(Param::as_list) else { continue };
                    for fref in flist {
                        let Some(fid) = fref.as_ref_id() else { continue };
                        let Some(fe) = ex.get(fid) else { continue };
                        let Some(bounds) = fe.params().first().and_then(Param::as_list) else { continue };
                        let mut loops3d = Vec::new();
                        for bref in bounds {
                            let Some(bid) = bref.as_ref_id() else { continue };
                            let Some(be) = ex.get(bid) else { continue };
                            let Some(lid) = be.params().first().and_then(Param::as_ref_id) else { continue };
                            let Some(le) = ex.get(lid) else { continue };
                            let Some(pts) = le.params().first().and_then(Param::as_list) else { continue };
                            let mut ring: Vec<[f64; 3]> = pts
                                .iter()
                                .filter_map(|pr| pr.as_ref_id().and_then(|pid| ifc_pt(&ex, pid, scale)))
                                .collect();
                            if be.params().get(1).and_then(Param::as_bool) == Some(false) {
                                ring.reverse();
                            }
                            if ring.len() >= 3 {
                                loops3d.push(ring);
                            }
                        }
                        push_planar_loops(&loops3d, xf.as_ref(), &mut build);
                    }
                }
            }
            "IFCEXTRUDEDAREASOLID" => {
                let p = e.params();
                let Some(prof_id) = p.first().and_then(Param::as_ref_id) else { continue };
                let Some(loops_bot) = ifc_profile_loops(&ex, prof_id, scale) else { continue };
                let local_pos = p
                    .get(1)
                    .and_then(Param::as_ref_id)
                    .and_then(|aid| ifc_axis2(&ex, aid, scale))
                    .unwrap_or(IDENT4);
                let combined_xf = match xf {
                    Some(m) => mat4_concat(&local_pos, &m),
                    None => local_pos,
                };
                let dir = p
                    .get(2)
                    .and_then(Param::as_ref_id)
                    .and_then(|did| ifc_dir(&ex, did))
                    .unwrap_or([0.0, 0.0, 1.0]);
                let depth = p.get(3).and_then(Param::as_f64).unwrap_or(100.0) * scale;
                let shift = [dir[0] * depth, dir[1] * depth, dir[2] * depth];

                let loops_top: Vec<Vec<[f64; 3]>> = loops_bot
                    .iter()
                    .map(|l| {
                        l.iter()
                            .map(|pt| [pt[0] + shift[0], pt[1] + shift[1], pt[2] + shift[2]])
                            .collect()
                    })
                    .collect();

                push_planar_loops(&loops_bot, Some(&combined_xf), &mut build);
                push_planar_loops(&loops_top, Some(&combined_xf), &mut build);

                for (bot, top) in loops_bot.iter().zip(&loops_top) {
                    let npts = bot.len();
                    for k in 0..npts {
                        let kn = (k + 1) % npts;
                        let p00 = xform_arr(Some(&combined_xf), bot[k]);
                        let p10 = xform_arr(Some(&combined_xf), bot[kn]);
                        let p11 = xform_arr(Some(&combined_xf), top[kn]);
                        let p01 = xform_arr(Some(&combined_xf), top[k]);
                        let n1 = tri_normal(p00, p10, p11);
                        for pt in [p00, p10, p11, p00, p11, p01] {
                            build.positions.extend_from_slice(&pt);
                            build.normals.extend_from_slice(&n1);
                        }
                    }
                }
            }
            _ => continue,
        }

        let end_v = (build.positions.len() / 3) as u32;
        let vcount = (end_v - start_v) as usize;
        if vcount > 0 {
            let color = CAD_BODY_PALETTE[item_idx % CAD_BODY_PALETTE.len()];
            item_idx += 1;
            let bi = build.batches.len();
            build.batches.push(MeshBatch {
                start_vertex: start_v,
                vertex_count: vcount as u32,
                base_color: color,
                texture_index: None,
            });
            build.layers.push(SceneLayer3d {
                id: format!("ifc_elem_{id}"),
                name,
                kind: "IFC BIM Solid".into(),
                visible: true,
                parent_id: None,
                material_names: Vec::new(),
                texture_names: Vec::new(),
                vertex_count: vcount,
                triangle_count: vcount / 3,
                batch_indices: vec![bi],
            });
        }
    }

    if build.positions.is_empty() && build.wireframe_lines.is_empty() {
        return None;
    }

    let tris = build.positions.len() / 9;
    metadata.push(MetadataEntry {
        section: "IFC BIM".into(),
        key: "BIM Elements".into(),
        value: item_idx.to_string(),
    });
    metadata.push(MetadataEntry {
        section: "IFC BIM".into(),
        key: "Tessellated Triangles".into(),
        value: tris.to_string(),
    });
    build.cad_summary = Some(format!("IFC BIM · {item_idx} elements · {tris} tris"));
    Some(build.finalize())
}
