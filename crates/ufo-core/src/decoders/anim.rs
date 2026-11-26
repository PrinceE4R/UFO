use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use fbxcel::low::v7400::AttributeValue;
use fbxcel::tree::any::AnyTree;

use crate::model::{
    AnimationClip, BoneChannel, MetadataEntry, SkeletonAnimation, SkeletonBone, ViewerPayload,
};

const FBX_TICKS_PER_SEC: f64 = 46_186_158_000.0;

#[inline]
pub(crate) fn euler_deg_xyz_to_quat(rx_deg: f64, ry_deg: f64, rz_deg: f64) -> [f32; 4] {
    let hx = (rx_deg.to_radians()) * 0.5;
    let hy = (ry_deg.to_radians()) * 0.5;
    let hz = (rz_deg.to_radians()) * 0.5;
    let (sx, cx) = hx.sin_cos();
    let (sy, cy) = hy.sin_cos();
    let (sz, cz) = hz.sin_cos();
    // Intrinsic Z * Y * X (extrinsic X then Y then Z — standard FBX EulerXYZ)
    let x = sx * cy * cz - cx * sy * sz;
    let y = cx * sy * cz + sx * cy * sz;
    let z = cx * cy * sz - sx * sy * cz;
    let w = cx * cy * cz + sx * sy * sz;
    normalize_quat([x as f32, y as f32, z as f32, w as f32])
}

#[inline]
fn axis_angle_quat(axis: char, deg: f64) -> [f64; 4] {
    let half = deg.to_radians() * 0.5;
    let (s, c) = half.sin_cos();
    match axis {
        'x' | 'X' => [s, 0.0, 0.0, c],
        'y' | 'Y' => [0.0, s, 0.0, c],
        _ => [0.0, 0.0, s, c],
    }
}

#[inline]
fn mul_quat_f64(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

#[inline]
fn normalize_quat(q: [f32; 4]) -> [f32; 4] {
    let len = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if len > 1e-6 {
        [q[0] / len, q[1] / len, q[2] / len, q[3] / len]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

pub(crate) fn infer_species_hint<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let lower: Vec<String> = names.into_iter().map(|s| s.to_ascii_lowercase()).collect();
    let count = lower.len();
    let has = |pat: &str| lower.iter().any(|n| n.contains(pat));

    if has("wing") || has("feather") || has("beak") || has("talon") || has("avian") {
        return format!("Avian / Bird Skeleton ({count} bones)");
    }
    if has("quadruped")
        || has("hoof")
        || has("paw")
        || has("muzzle")
        || has("hind")
        || has("frontleg")
        || has("backleg")
        || (has("tail") && (has("horse") || has("dog") || has("cat") || has("deer") || has("wolf")))
        || (has("leg_fl") || has("leg_fr") || has("leg_bl") || has("leg_br"))
    {
        return format!("Quadruped / Creature Skeleton ({count} bones)");
    }
    if (has("hips") || has("pelvis") || has("spine"))
        && (has("arm") || has("hand") || has("clavicle") || has("shoulder") || has("head"))
    {
        if has("tail") && !has("ponytail") && !has("coattail") {
            return format!("Tailed Biped / Creature Skeleton ({count} bones)");
        }
        return format!("Human / Biped Skeleton ({count} bones)");
    }
    if has("tail") {
        return format!("Creature / Articulated Skeleton ({count} bones)");
    }
    format!("Skeletal Rig ({count} bones)")
}

/// Topologically orders raw nodes `[id, name, parent_id, pos, rot]` into `Vec<SkeletonBone>`
/// and returns `(ordered_bones, id_to_new_index)`.
fn sort_bones_topologically(
    nodes: &[(i64, String, Option<i64>, [f32; 3], [f32; 4])],
) -> (Vec<SkeletonBone>, HashMap<i64, usize>) {
    let id_set: HashSet<i64> = nodes.iter().map(|n| n.0).collect();
    let mut children: HashMap<Option<i64>, Vec<usize>> = HashMap::new();
    for (idx, n) in nodes.iter().enumerate() {
        let p = n.2.filter(|pid| id_set.contains(pid) && *pid != n.0);
        children.entry(p).or_default().push(idx);
    }

    let mut visited = vec![false; nodes.len()];
    let mut order = Vec::with_capacity(nodes.len());

    fn dfs(
        idx: usize,
        nodes: &[(i64, String, Option<i64>, [f32; 3], [f32; 4])],
        children: &HashMap<Option<i64>, Vec<usize>>,
        visited: &mut [bool],
        order: &mut Vec<usize>,
    ) {
        if visited[idx] {
            return;
        }
        visited[idx] = true;
        order.push(idx);
        if let Some(kids) = children.get(&Some(nodes[idx].0)) {
            for &k in kids {
                dfs(k, nodes, children, visited, order);
            }
        }
    }

    if let Some(roots) = children.get(&None) {
        for &r in roots {
            dfs(r, nodes, &children, &mut visited, &mut order);
        }
    }
    for i in 0..nodes.len() {
        if !visited[i] {
            dfs(i, nodes, &children, &mut visited, &mut order);
        }
    }

    let mut id_to_idx = HashMap::new();
    for (new_i, &old_i) in order.iter().enumerate() {
        id_to_idx.insert(nodes[old_i].0, new_i);
    }

    let bones = order
        .iter()
        .enumerate()
        .map(|(new_i, &old_i)| {
            let (_, ref name, parent_id, pos, rot) = nodes[old_i];
            SkeletonBone {
                id: new_i,
                name: name.clone(),
                parent: parent_id.and_then(|pid| id_to_idx.get(&pid).copied()),
                rest_local_pos: pos,
                rest_local_rot: rot,
            }
        })
        .collect();

    (bones, id_to_idx)
}

#[inline]
fn sample_curve(times: &[f32], vals: &[f32], t: f32, default_val: f32) -> f32 {
    if times.is_empty() || vals.is_empty() {
        return default_val;
    }
    if times.len() == 1 || t <= times[0] {
        return vals[0];
    }
    let last = times.len().min(vals.len()) - 1;
    if t >= times[last] {
        return vals[last];
    }
    let idx = times[..=last].partition_point(|&x| x <= t).saturating_sub(1);
    let next = (idx + 1).min(last);
    let span = (times[next] - times[idx]).max(1e-6);
    let alpha = ((t - times[idx]) / span).clamp(0.0, 1.0);
    vals[idx] + (vals[next] - vals[idx]) * alpha
}

/// Decodes BVH Biovision Hierarchy skeletal motion capture files into a looping animated skeleton.
pub fn decode_bvh(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());

    #[derive(Clone)]
    struct BvhJoint {
        id: i64,
        name: String,
        parent: Option<i64>,
        offset: [f32; 3],
        channels: Vec<String>,
    }

    let mut joints: Vec<BvhJoint> = Vec::new();
    let mut stack: Vec<i64> = Vec::new();
    let mut next_id = 1i64;
    let mut frame_count = 0usize;
    let mut frame_time = 1.0f32 / 30.0;
    let mut in_motion = false;
    let mut frame_rows: Vec<Vec<f32>> = Vec::new();

    while let Some(line) = lines.next() {
        if !in_motion {
            if line.eq_ignore_ascii_case("MOTION") {
                in_motion = true;
                continue;
            }
            if let Some(name) = line.strip_prefix("ROOT ").or_else(|| line.strip_prefix("JOINT ")) {
                let id = next_id;
                next_id += 1;
                joints.push(BvhJoint {
                    id,
                    name: name.trim().to_string(),
                    parent: stack.last().copied(),
                    offset: [0.0; 3],
                    channels: Vec::new(),
                });
                stack.push(id);
            } else if line.eq_ignore_ascii_case("End Site") {
                let id = next_id;
                next_id += 1;
                let pname = stack
                    .last()
                    .and_then(|pid| joints.iter().find(|j| j.id == *pid))
                    .map(|j| format!("{}_Tip", j.name))
                    .unwrap_or_else(|| "EndSite".into());
                joints.push(BvhJoint {
                    id,
                    name: pname,
                    parent: stack.last().copied(),
                    offset: [0.0; 3],
                    channels: Vec::new(),
                });
                stack.push(id);
            } else if let Some(rest) = line.strip_prefix("OFFSET") {
                if let Some(cur) = joints.last_mut() {
                    let nums: Vec<f32> = rest.split_whitespace().filter_map(|s| s.parse().ok()).collect();
                    if nums.len() >= 3 {
                        cur.offset = [nums[0], nums[1], nums[2]];
                    }
                }
            } else if let Some(rest) = line.strip_prefix("CHANNELS") {
                if let Some(cur) = joints.last_mut() {
                    let mut it = rest.split_whitespace();
                    let _cnt = it.next();
                    cur.channels = it.map(|s| s.to_string()).collect();
                }
            } else if line == "}" {
                stack.pop();
            }
        } else if let Some(fc) = line.strip_prefix("Frames:") {
            frame_count = fc.trim().parse().unwrap_or(0);
        } else if let Some(ft) = line.strip_prefix("Frame Time:") {
            frame_time = ft.trim().parse::<f32>().unwrap_or(1.0 / 30.0).max(0.001);
        } else {
            let row: Vec<f32> = line.split_whitespace().filter_map(|s| s.parse().ok()).collect();
            if !row.is_empty() && frame_rows.len() < 4096 {
                frame_rows.push(row);
            }
        }
    }

    if joints.is_empty() {
        return None;
    }

    let raw_nodes: Vec<(i64, String, Option<i64>, [f32; 3], [f32; 4])> = joints
        .iter()
        .map(|j| (j.id, j.name.clone(), j.parent, j.offset, [0.0, 0.0, 0.0, 1.0]))
        .collect();
    let (bones, id_to_idx) = sort_bones_topologically(&raw_nodes);
    let hint = infer_species_hint(bones.iter().map(|b| b.name.as_str()));

    let n_frames = frame_rows.len();
    let step = (n_frames / 360).max(1);
    let mut times = Vec::new();
    let mut sampled_rows = Vec::new();
    for (fi, row) in frame_rows.iter().enumerate().step_by(step) {
        times.push(fi as f32 * frame_time);
        sampled_rows.push(row);
    }
    let duration = (n_frames.saturating_sub(1) as f32 * frame_time).max(frame_time);

    let mut channels = Vec::new();
    let mut col_offset = 0usize;
    for j in &joints {
        let ch_len = j.channels.len();
        if ch_len == 0 {
            continue;
        }
        if let Some(&bone_idx) = id_to_idx.get(&j.id) {
            let has_pos = j.channels.iter().any(|c| c.ends_with("position"));
            let has_rot = j.channels.iter().any(|c| c.ends_with("rotation"));
            let mut translations = Vec::new();
            let mut rotations = Vec::new();

            for row in &sampled_rows {
                let mut pos = j.offset;
                let mut q = [0.0f64, 0.0, 0.0, 1.0];
                for (ci, ch_name) in j.channels.iter().enumerate() {
                    let v = row.get(col_offset + ci).copied().unwrap_or(0.0) as f64;
                    match ch_name.as_str() {
                        "Xposition" => pos[0] = v as f32,
                        "Yposition" => pos[1] = v as f32,
                        "Zposition" => pos[2] = v as f32,
                        "Xrotation" => q = mul_quat_f64(q, axis_angle_quat('x', v)),
                        "Yrotation" => q = mul_quat_f64(q, axis_angle_quat('y', v)),
                        "Zrotation" => q = mul_quat_f64(q, axis_angle_quat('z', v)),
                        _ => {}
                    }
                }
                if has_pos {
                    translations.push(pos);
                }
                if has_rot {
                    rotations.push(normalize_quat([q[0] as f32, q[1] as f32, q[2] as f32, q[3] as f32]));
                }
            }

            channels.push(BoneChannel {
                bone_index: bone_idx,
                times: times.clone(),
                translations,
                rotations,
                scales: Vec::new(),
            });
        }
        col_offset += ch_len;
    }

    metadata.push(MetadataEntry { section: "Skeleton Animation".into(), key: "Format".into(), value: "BVH Motion Capture".into() });
    metadata.push(MetadataEntry { section: "Skeleton Animation".into(), key: "Rig Classification".into(), value: hint.clone() });
    metadata.push(MetadataEntry { section: "Skeleton Animation".into(), key: "Bones".into(), value: bones.len().to_string() });
    metadata.push(MetadataEntry { section: "Skeleton Animation".into(), key: "Frames / Duration".into(), value: format!("{} frames ({duration:.2}s @ {:.1} fps)", frame_count.max(n_frames), 1.0 / frame_time) });

    let parts = bones.iter().take(64).map(|b| b.name.clone()).collect();
    let clip = AnimationClip {
        name: "Motion Capture Loop".into(),
        duration_secs: duration,
        channels,
    };
    let skeleton = SkeletonAnimation {
        species_hint: Some(hint.clone()),
        bones,
        clips: vec![clip],
    };

    Some(
        crate::decoders::mesh3d::MeshBuild {
            format_label: format!("BVH Skeleton Animation · {hint}"),
            parts,
            cad_summary: Some(format!("{hint} · {frame_count} frames · {duration:.2}s loop")),
            skeleton: Some(skeleton),
            ..Default::default()
        }
        .finalize(),
    )
}

#[derive(Default)]
struct FbxCurve {
    times: Vec<f32>,
    values: Vec<f32>,
}

#[derive(Default)]
struct FbxCurveNode {
    attr: String,
    default_xyz: [f64; 3],
    x_curve: Option<i64>,
    y_curve: Option<i64>,
    z_curve: Option<i64>,
}

#[derive(Clone)]
struct FbxModelNode {
    id: i64,
    name: String,
    subtype: String,
    lcl_pos: [f64; 3],
    lcl_rot: [f64; 3],
}

fn clean_fbx_name(raw: &str) -> String {
    raw.split('\x00')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_string()
}

fn attr_as_i64(a: &AttributeValue) -> Option<i64> {
    match a {
        AttributeValue::I16(v) => Some(*v as i64),
        AttributeValue::I32(v) => Some(*v as i64),
        AttributeValue::I64(v) => Some(*v),
        _ => None,
    }
}

fn attr_as_f64(a: &AttributeValue) -> Option<f64> {
    match a {
        AttributeValue::F32(v) => Some(*v as f64),
        AttributeValue::F64(v) => Some(*v),
        AttributeValue::I32(v) => Some(*v as f64),
        AttributeValue::I16(v) => Some(*v as f64),
        AttributeValue::I64(v) => Some(*v as f64),
        _ => None,
    }
}

fn attr_as_str(a: &AttributeValue) -> Option<&str> {
    match a {
        AttributeValue::String(s) => Some(s.as_str()),
        _ => None,
    }
}

/// Decodes Autodesk FBX (binary or ASCII) into an articulated looping skeleton animation (+ telemetry).
pub fn decode_fbx_animation(
    bytes: &[u8],
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    if bytes.starts_with(b"Kaydara FBX Binary") {
        if let Some(payload) = decode_fbx_binary_skeleton(bytes, metadata) {
            return Some(payload);
        }
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        if text.contains("FBXHeaderExtension") || text.contains("Objects:") {
            if let Some(payload) = decode_fbx_ascii_skeleton(text, metadata) {
                return Some(payload);
            }
        }
    }
    None
}

fn decode_fbx_binary_skeleton(
    bytes: &[u8],
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let AnyTree::V7400(_ver, tree, _) = AnyTree::from_seekable_reader(&mut Cursor::new(bytes)).ok()?
    else {
        return None;
    };
    let root = tree.root();

    let mut models: HashMap<i64, FbxModelNode> = HashMap::new();
    let mut anim_stacks: Vec<(i64, String)> = Vec::new();
    let mut anim_layers: HashSet<i64> = HashSet::new();
    let mut curve_nodes: HashMap<i64, FbxCurveNode> = HashMap::new();
    let mut curves: HashMap<i64, FbxCurve> = HashMap::new();
    let mut mesh_count = 0usize;

    for obj_root in root.children_by_name("Objects") {
        for node in obj_root.children() {
            let attrs = node.attributes();
            let id = attrs.first().and_then(attr_as_i64).unwrap_or(0);
            let raw_name = attrs.get(1).and_then(attr_as_str).unwrap_or("");
            let name = clean_fbx_name(raw_name);
            let subtype = attrs.get(2).and_then(attr_as_str).unwrap_or("").to_string();

            match node.name() {
                "Geometry" => {
                    if subtype.eq_ignore_ascii_case("Mesh") {
                        mesh_count += 1;
                    }
                }
                "Model" => {
                    let mut m = FbxModelNode {
                        id,
                        name: if name.is_empty() { format!("Node_{id}") } else { name },
                        subtype,
                        lcl_pos: [0.0; 3],
                        lcl_rot: [0.0; 3],
                    };
                    for props in node.children_by_name("Properties70") {
                        for p in props.children_by_name("P") {
                            let pa = p.attributes();
                            let Some(pname) = pa.first().and_then(attr_as_str) else { continue };
                            if pname == "Lcl Translation" && pa.len() >= 7 {
                                m.lcl_pos = [
                                    pa.get(4).and_then(attr_as_f64).unwrap_or(0.0),
                                    pa.get(5).and_then(attr_as_f64).unwrap_or(0.0),
                                    pa.get(6).and_then(attr_as_f64).unwrap_or(0.0),
                                ];
                            } else if pname == "Lcl Rotation" && pa.len() >= 7 {
                                m.lcl_rot = [
                                    pa.get(4).and_then(attr_as_f64).unwrap_or(0.0),
                                    pa.get(5).and_then(attr_as_f64).unwrap_or(0.0),
                                    pa.get(6).and_then(attr_as_f64).unwrap_or(0.0),
                                ];
                            }
                        }
                    }
                    models.insert(id, m);
                }
                "AnimationStack" => {
                    let cname = if name.is_empty() {
                        format!("Take_{}", anim_stacks.len() + 1)
                    } else {
                        name
                    };
                    anim_stacks.push((id, cname));
                }
                "AnimationLayer" => {
                    anim_layers.insert(id);
                }
                "AnimationCurveNode" => {
                    let mut cn = FbxCurveNode {
                        attr: name,
                        ..Default::default()
                    };
                    for props in node.children_by_name("Properties70") {
                        for p in props.children_by_name("P") {
                            let pa = p.attributes();
                            let Some(pname) = pa.first().and_then(attr_as_str) else { continue };
                            let v = pa.get(4).and_then(attr_as_f64).unwrap_or(0.0);
                            match pname {
                                "d|X" => cn.default_xyz[0] = v,
                                "d|Y" => cn.default_xyz[1] = v,
                                "d|Z" => cn.default_xyz[2] = v,
                                _ => {}
                            }
                        }
                    }
                    curve_nodes.insert(id, cn);
                }
                "AnimationCurve" => {
                    let mut fc = FbxCurve::default();
                    for child in node.children() {
                        if child.name() == "KeyTime" {
                            if let Some(AttributeValue::ArrI64(ts)) = child.attributes().first() {
                                fc.times = ts.iter().map(|&t| (t as f64 / FBX_TICKS_PER_SEC) as f32).collect();
                            }
                        } else if child.name() == "KeyValueFloat" {
                            match child.attributes().first() {
                                Some(AttributeValue::ArrF32(vs)) => fc.values = vs.clone(),
                                Some(AttributeValue::ArrF64(vs)) => {
                                    fc.values = vs.iter().map(|&v| v as f32).collect();
                                }
                                _ => {}
                            }
                        }
                    }
                    curves.insert(id, fc);
                }
                _ => {}
            }
        }
    }

    let mut model_parent: HashMap<i64, i64> = HashMap::new();
    let mut layer_to_stack: HashMap<i64, i64> = HashMap::new();
    let mut cnode_layers: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut cnode_target: HashMap<i64, (i64, String)> = HashMap::new();

    for conn_root in root.children_by_name("Connections") {
        for c in conn_root.children_by_name("C") {
            let attrs = c.attributes();
            let ctype = attrs.first().and_then(attr_as_str).unwrap_or("");
            let src = attrs.get(1).and_then(attr_as_i64).unwrap_or(0);
            let dst = attrs.get(2).and_then(attr_as_i64).unwrap_or(0);
            let prop = attrs.get(3).and_then(attr_as_str).unwrap_or("");

            if ctype == "OO" {
                if models.contains_key(&src) && models.contains_key(&dst) {
                    model_parent.insert(src, dst);
                } else if anim_layers.contains(&src) {
                    layer_to_stack.insert(src, dst);
                } else if curve_nodes.contains_key(&src) && anim_layers.contains(&dst) {
                    cnode_layers.entry(src).or_default().push(dst);
                }
            } else if ctype == "OP" {
                if curves.contains_key(&src) {
                    if let Some(cn) = curve_nodes.get_mut(&dst) {
                        if prop.ends_with("X") {
                            cn.x_curve = Some(src);
                        } else if prop.ends_with("Y") {
                            cn.y_curve = Some(src);
                        } else if prop.ends_with("Z") {
                            cn.z_curve = Some(src);
                        }
                    }
                } else if curve_nodes.contains_key(&src) && models.contains_key(&dst) {
                    cnode_target.insert(src, (dst, prop.to_string()));
                }
            }
        }
    }

    let raw_ver = if bytes.len() >= 27 {
        u32::from_le_bytes([bytes[23], bytes[24], bytes[25], bytes[26]])
    } else {
        7400
    };
    metadata.push(MetadataEntry { section: "Autodesk FBX".into(), key: "FBX Version".into(), value: format!("{:.1}", raw_ver as f32 / 1000.0) });
    metadata.push(MetadataEntry { section: "Autodesk FBX".into(), key: "Models / Geometries".into(), value: format!("{} / {}", models.len(), mesh_count) });

    build_fbx_skeleton_payload(
        format!("Autodesk FBX v{:.1}", raw_ver as f32 / 1000.0),
        &models,
        &model_parent,
        &anim_stacks,
        &layer_to_stack,
        &cnode_layers,
        &cnode_target,
        &curve_nodes,
        &curves,
        metadata,
    )
}

fn decode_fbx_ascii_skeleton(
    text: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let mut models: HashMap<i64, FbxModelNode> = HashMap::new();
    let mut anim_stacks: Vec<(i64, String)> = Vec::new();
    let mut anim_layers: HashSet<i64> = HashSet::new();
    let mut curve_nodes: HashMap<i64, FbxCurveNode> = HashMap::new();
    let mut curves: HashMap<i64, FbxCurve> = HashMap::new();
    let mut model_parent: HashMap<i64, i64> = HashMap::new();
    let mut layer_to_stack: HashMap<i64, i64> = HashMap::new();
    let mut cnode_layers: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut cnode_target: HashMap<i64, (i64, String)> = HashMap::new();

    #[derive(Clone, Copy)]
    enum Block {
        Model(i64),
        CurveNode(i64),
        Curve(i64),
        Other,
    }
    let mut stack: Vec<Block> = Vec::new();
    let mut synth_id = 1_000_000i64;
    let mut name_to_id: HashMap<String, i64> = HashMap::new();

    let parse_quoted = |s: &str| -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut inside = false;
        for ch in s.chars() {
            if ch == '"' {
                if inside {
                    out.push(cur.clone());
                    cur.clear();
                }
                inside = !inside;
            } else if inside {
                cur.push(ch);
            }
        }
        out
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with("Model:") && line.contains('{') {
            let qs = parse_quoted(line);
            let head_id = line
                .strip_prefix("Model:")
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or_else(|| {
                    synth_id += 1;
                    synth_id
                });
            let name = qs.first().map(|s| clean_fbx_name(s)).unwrap_or_else(|| format!("Node_{head_id}"));
            let subtype = qs.get(1).cloned().unwrap_or_default();
            if let Some(raw_full) = qs.first() {
                name_to_id.insert(raw_full.clone(), head_id);
            }
            models.insert(
                head_id,
                FbxModelNode {
                    id: head_id,
                    name,
                    subtype,
                    lcl_pos: [0.0; 3],
                    lcl_rot: [0.0; 3],
                },
            );
            stack.push(Block::Model(head_id));
            continue;
        }
        if line.starts_with("AnimationStack:") && line.contains('{') {
            let qs = parse_quoted(line);
            let id = line
                .strip_prefix("AnimationStack:")
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or_else(|| {
                    synth_id += 1;
                    synth_id
                });
            let nm = qs.first().map(|s| clean_fbx_name(s)).unwrap_or_else(|| "Take".into());
            anim_stacks.push((id, nm));
            stack.push(Block::Other);
            continue;
        }
        if line.starts_with("AnimationLayer:") && line.contains('{') {
            let id = line
                .strip_prefix("AnimationLayer:")
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or_else(|| {
                    synth_id += 1;
                    synth_id
                });
            anim_layers.insert(id);
            stack.push(Block::Other);
            continue;
        }
        if line.starts_with("AnimationCurveNode:") && line.contains('{') {
            let qs = parse_quoted(line);
            let id = line
                .strip_prefix("AnimationCurveNode:")
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or_else(|| {
                    synth_id += 1;
                    synth_id
                });
            curve_nodes.insert(
                id,
                FbxCurveNode {
                    attr: qs.first().map(|s| clean_fbx_name(s)).unwrap_or_default(),
                    ..Default::default()
                },
            );
            stack.push(Block::CurveNode(id));
            continue;
        }
        if line.starts_with("AnimationCurve:") && line.contains('{') {
            let id = line
                .strip_prefix("AnimationCurve:")
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or_else(|| {
                    synth_id += 1;
                    synth_id
                });
            curves.insert(id, FbxCurve::default());
            stack.push(Block::Curve(id));
            continue;
        }
        if line.ends_with('{') {
            stack.push(Block::Other);
            continue;
        }
        if line == "}" {
            stack.pop();
            continue;
        }

        if let Some(&top) = stack.last() {
            match top {
                Block::Model(id) if line.starts_with("P:") => {
                    let qs = parse_quoted(line);
                    if let Some(pname) = qs.first() {
                        let tail: Vec<f64> = line
                            .split(',')
                            .rev()
                            .take(3)
                            .filter_map(|s| s.trim().parse::<f64>().ok())
                            .collect();
                        if tail.len() == 3 {
                            if let Some(m) = models.get_mut(&id) {
                                if pname == "Lcl Translation" {
                                    m.lcl_pos = [tail[2], tail[1], tail[0]];
                                } else if pname == "Lcl Rotation" {
                                    m.lcl_rot = [tail[2], tail[1], tail[0]];
                                }
                            }
                        }
                    }
                }
                Block::CurveNode(id) if line.starts_with("P:") => {
                    let qs = parse_quoted(line);
                    if let (Some(pname), Some(val)) = (
                        qs.first(),
                        line.split(',').last().and_then(|s| s.trim().parse::<f64>().ok()),
                    ) {
                        if let Some(cn) = curve_nodes.get_mut(&id) {
                            match pname.as_str() {
                                "d|X" => cn.default_xyz[0] = val,
                                "d|Y" => cn.default_xyz[1] = val,
                                "d|Z" => cn.default_xyz[2] = val,
                                _ => {}
                            }
                        }
                    }
                }
                Block::Curve(id) => {
                    if let Some(rest) = line.strip_prefix("KeyTime:") {
                        let nums_str = rest.split(':').nth(1).unwrap_or(rest);
                        if let Some(fc) = curves.get_mut(&id) {
                            fc.times = nums_str
                                .split(',')
                                .filter_map(|s| s.trim().parse::<i64>().ok())
                                .map(|t| (t as f64 / FBX_TICKS_PER_SEC) as f32)
                                .collect();
                        }
                    } else if let Some(rest) = line.strip_prefix("KeyValueFloat:") {
                        let nums_str = rest.split(':').nth(1).unwrap_or(rest);
                        if let Some(fc) = curves.get_mut(&id) {
                            fc.values = nums_str
                                .split(',')
                                .filter_map(|s| s.trim().parse::<f32>().ok())
                                .collect();
                        }
                    }
                }
                _ => {}
            }
        }

        if line.starts_with("C:") {
            let rest = line.strip_prefix("C:").unwrap_or("").trim();
            let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
            let ctype = parts.first().map(|s| s.trim_matches('"')).unwrap_or("");
            if let (Ok(src), Ok(dst)) = (
                parts.get(1).unwrap_or(&"").parse::<i64>(),
                parts.get(2).unwrap_or(&"").parse::<i64>(),
            ) {
                let prop = parts.get(3).map(|s| s.trim_matches('"')).unwrap_or("");
                if ctype == "OO" {
                    if models.contains_key(&src) && models.contains_key(&dst) {
                        model_parent.insert(src, dst);
                    } else if anim_layers.contains(&src) {
                        layer_to_stack.insert(src, dst);
                    } else if curve_nodes.contains_key(&src) && anim_layers.contains(&dst) {
                        cnode_layers.entry(src).or_default().push(dst);
                    }
                } else if ctype == "OP" {
                    if curves.contains_key(&src) {
                        if let Some(cn) = curve_nodes.get_mut(&dst) {
                            if prop.ends_with("X") {
                                cn.x_curve = Some(src);
                            } else if prop.ends_with("Y") {
                                cn.y_curve = Some(src);
                            } else if prop.ends_with("Z") {
                                cn.z_curve = Some(src);
                            }
                        }
                    } else if curve_nodes.contains_key(&src) && models.contains_key(&dst) {
                        cnode_target.insert(src, (dst, prop.to_string()));
                    }
                }
            } else {
                let qs = parse_quoted(rest);
                if qs.len() >= 3 && qs[0] == "OO" {
                    if let (Some(&s), Some(&d)) = (name_to_id.get(&qs[1]), name_to_id.get(&qs[2])) {
                        model_parent.insert(s, d);
                    }
                }
            }
        }
    }

    metadata.push(MetadataEntry { section: "Autodesk FBX".into(), key: "Encoding".into(), value: "ASCII FBX".into() });
    metadata.push(MetadataEntry { section: "Autodesk FBX".into(), key: "Models".into(), value: models.len().to_string() });

    build_fbx_skeleton_payload(
        "Autodesk FBX (ASCII)".into(),
        &models,
        &model_parent,
        &anim_stacks,
        &layer_to_stack,
        &cnode_layers,
        &cnode_target,
        &curve_nodes,
        &curves,
        metadata,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_fbx_skeleton_payload(
    format_label: String,
    models: &HashMap<i64, FbxModelNode>,
    model_parent: &HashMap<i64, i64>,
    anim_stacks: &[(i64, String)],
    layer_to_stack: &HashMap<i64, i64>,
    cnode_layers: &HashMap<i64, Vec<i64>>,
    cnode_target: &HashMap<i64, (i64, String)>,
    curve_nodes: &HashMap<i64, FbxCurveNode>,
    curves: &HashMap<i64, FbxCurve>,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    if models.is_empty() {
        return None;
    }
    let mut keep_ids: HashSet<i64> = models
        .values()
        .filter(|m| {
            let st = m.subtype.to_ascii_lowercase();
            st == "limbnode" || st == "limb" || st == "skeleton"
        })
        .map(|m| m.id)
        .collect();

    // Include animated models + ancestor hierarchy if no explicit LimbNodes were marked
    if keep_ids.is_empty() {
        for (target_id, _) in cnode_target.values() {
            keep_ids.insert(*target_id);
        }
    }
    // Always preserve parent chain up to root for clean bone articulation
    let snapshot: Vec<i64> = keep_ids.iter().copied().collect();
    for mut cur in snapshot {
        let mut hops = 0;
        while let Some(&pid) = model_parent.get(&cur) {
            if !keep_ids.insert(pid) || hops > 128 {
                break;
            }
            cur = pid;
            hops += 1;
        }
    }
    if keep_ids.is_empty() {
        for m in models.values() {
            if !m.subtype.eq_ignore_ascii_case("Camera") && !m.subtype.eq_ignore_ascii_case("Light") {
                keep_ids.insert(m.id);
            }
        }
    }
    if keep_ids.is_empty() {
        return None;
    }

    let raw_nodes: Vec<(i64, String, Option<i64>, [f32; 3], [f32; 4])> = keep_ids
        .iter()
        .filter_map(|id| models.get(id))
        .map(|m| {
            (
                m.id,
                m.name.clone(),
                model_parent.get(&m.id).copied(),
                [m.lcl_pos[0] as f32, m.lcl_pos[1] as f32, m.lcl_pos[2] as f32],
                euler_deg_xyz_to_quat(m.lcl_rot[0], m.lcl_rot[1], m.lcl_rot[2]),
            )
        })
        .collect();

    let (bones, id_to_idx) = sort_bones_topologically(&raw_nodes);
    let hint = infer_species_hint(bones.iter().map(|b| b.name.as_str()));

    // Group animation curves by stack (clip)
    let stacks: Vec<(i64, String)> = if anim_stacks.is_empty() {
        vec![(0, "Default Loop".into())]
    } else {
        anim_stacks.to_vec()
    };

    let mut clips = Vec::new();
    for (stack_id, stack_name) in &stacks {
        // Per-bone (T, R, S) curve nodes belonging to this stack (or all if single default stack)
        let mut bone_curves: HashMap<usize, (Option<&FbxCurveNode>, Option<&FbxCurveNode>, Option<&FbxCurveNode>)> = HashMap::new();
        for (&cnode_id, (target_id, prop)) in cnode_target {
            let belongs = *stack_id == 0
                || cnode_layers
                    .get(&cnode_id)
                    .map(|ls| ls.iter().any(|lid| layer_to_stack.get(lid) == Some(stack_id)))
                    .unwrap_or(false)
                || stacks.len() == 1;
            if !belongs {
                continue;
            }
            let Some(&bidx) = id_to_idx.get(target_id) else { continue };
            let Some(cn) = curve_nodes.get(&cnode_id) else { continue };
            let entry = bone_curves.entry(bidx).or_default();
            if prop.contains("Translation") || cn.attr == "T" {
                entry.0 = Some(cn);
            } else if prop.contains("Rotation") || cn.attr == "R" {
                entry.1 = Some(cn);
            } else if prop.contains("Scaling") || cn.attr == "S" {
                entry.2 = Some(cn);
            }
        }

        let mut clip_dur = 0.0f32;
        let mut channels = Vec::new();
        for (bidx, (t_cn, r_cn, s_cn)) in bone_curves {
            let Some(m) = bones.get(bidx) else { continue };
            let mut time_set: Vec<f32> = Vec::new();
            for cn in [t_cn, r_cn, s_cn].into_iter().flatten() {
                for cid in [cn.x_curve, cn.y_curve, cn.z_curve].into_iter().flatten() {
                    if let Some(c) = curves.get(&cid) {
                        for &t in &c.times {
                            if t.is_finite() {
                                time_set.push(t);
                            }
                        }
                    }
                }
            }
            if time_set.is_empty() {
                continue;
            }
            time_set.sort_by(f32::total_cmp);
            time_set.dedup_by(|a, b| (*a - *b).abs() < 1e-4);
            if time_set.len() > 240 {
                let stride = time_set.len() / 240 + 1;
                let last = *time_set.last().unwrap_or(&0.0);
                time_set = time_set.into_iter().step_by(stride).collect();
                if time_set.last().copied() != Some(last) {
                    time_set.push(last);
                }
            }
            if let Some(&tmax) = time_set.last() {
                clip_dur = clip_dur.max(tmax);
            }

            let eval_xyz = |cn: &FbxCurveNode, t: f32| -> [f64; 3] {
                let get = |cid: Option<i64>, def: f64| {
                    cid.and_then(|id| curves.get(&id))
                        .map(|c| sample_curve(&c.times, &c.values, t, def as f32) as f64)
                        .unwrap_or(def)
                };
                [
                    get(cn.x_curve, cn.default_xyz[0]),
                    get(cn.y_curve, cn.default_xyz[1]),
                    get(cn.z_curve, cn.default_xyz[2]),
                ]
            };

            let mut translations = Vec::new();
            let mut rotations = Vec::new();
            let mut scales = Vec::new();
            if let Some(cn) = t_cn {
                for &t in &time_set {
                    let v = eval_xyz(cn, t);
                    translations.push([v[0] as f32, v[1] as f32, v[2] as f32]);
                }
            }
            if let Some(cn) = r_cn {
                let _ = m;
                for &t in &time_set {
                    let r = eval_xyz(cn, t);
                    rotations.push(euler_deg_xyz_to_quat(r[0], r[1], r[2]));
                }
            }
            if let Some(cn) = s_cn {
                for &t in &time_set {
                    let s = eval_xyz(cn, t);
                    scales.push([s[0] as f32, s[1] as f32, s[2] as f32]);
                }
            }

            channels.push(BoneChannel {
                bone_index: bidx,
                times: time_set,
                translations,
                rotations,
                scales,
            });
        }

        if !channels.is_empty() {
            clips.push(AnimationClip {
                name: stack_name.clone(),
                duration_secs: clip_dur.max(0.1),
                channels,
            });
        }
    }

    metadata.push(MetadataEntry { section: "Skeleton Animation".into(), key: "Rig Classification".into(), value: hint.clone() });
    metadata.push(MetadataEntry { section: "Skeleton Animation".into(), key: "Bones / Joints".into(), value: bones.len().to_string() });
    metadata.push(MetadataEntry { section: "Skeleton Animation".into(), key: "Animation Clips".into(), value: clips.len().to_string() });

    let parts = bones.iter().take(64).map(|b| b.name.clone()).collect();
    let clip_summary = if let Some(first) = clips.first() {
        format!("{hint} · {} clip(s) · {:.2}s loop", clips.len(), first.duration_secs)
    } else {
        format!("{hint} (rest pose)")
    };

    let skeleton = SkeletonAnimation {
        species_hint: Some(hint),
        bones,
        clips,
    };

    Some(
        crate::decoders::mesh3d::MeshBuild {
            format_label: format!("{format_label} · Skeleton"),
            parts,
            cad_summary: Some(clip_summary),
            skeleton: Some(skeleton),
            ..Default::default()
        }
        .finalize(),
    )
}

/// Extracts skeletal skin & looping animation clips from a glTF 2.0 / GLB document.
pub fn extract_gltf_skeleton(doc: &gltf::Document, buffers: &[Vec<u8>]) -> Option<SkeletonAnimation> {
    let mut anim_nodes: HashSet<usize> = HashSet::new();
    for anim in doc.animations() {
        for ch in anim.channels() {
            anim_nodes.insert(ch.target().node().index());
        }
    }
    let mut joint_indices: Vec<usize> = Vec::new();
    let mut joint_set: HashSet<usize> = HashSet::new();
    for skin in doc.skins() {
        for j in skin.joints() {
            if joint_set.insert(j.index()) {
                joint_indices.push(j.index());
            }
        }
    }
    if joint_indices.is_empty() && !anim_nodes.is_empty() {
        for n_idx in &anim_nodes {
            if joint_set.insert(*n_idx) {
                joint_indices.push(*n_idx);
            }
        }
    }
    if joint_indices.is_empty() {
        return None;
    }

    let mut parent_map: HashMap<usize, usize> = HashMap::new();
    let mut node_map: HashMap<usize, gltf::Node<'_>> = HashMap::new();
    for n in doc.nodes() {
        node_map.insert(n.index(), n.clone());
        for c in n.children() {
            parent_map.insert(c.index(), n.index());
        }
    }
    let snapshot = joint_indices.clone();
    for mut cur in snapshot {
        let mut hops = 0;
        while let Some(&pid) = parent_map.get(&cur) {
            if node_map.get(&pid).and_then(|n| n.mesh()).is_some() {
                break;
            }
            if joint_set.insert(pid) {
                joint_indices.push(pid);
            }
            cur = pid;
            hops += 1;
            if hops > 64 {
                break;
            }
        }
    }

    let raw_nodes: Vec<(i64, String, Option<i64>, [f32; 3], [f32; 4])> = joint_indices
        .iter()
        .filter_map(|ni| node_map.get(ni))
        .map(|n| {
            let (t, r, _s) = n.transform().decomposed();
            (
                n.index() as i64,
                n.name().map(str::to_string).unwrap_or_else(|| format!("Joint_{}", n.index())),
                parent_map.get(&n.index()).map(|&p| p as i64),
                t,
                normalize_quat(r),
            )
        })
        .collect();

    let (bones, id_to_idx) = sort_bones_topologically(&raw_nodes);
    let hint = infer_species_hint(bones.iter().map(|b| b.name.as_str()));

    let mut clips = Vec::new();
    for (ai, anim) in doc.animations().enumerate() {
        let cname = anim
            .name()
            .map(str::to_string)
            .unwrap_or_else(|| format!("Clip {}", ai + 1));
        let mut dur = 0.0f32;
        let mut per_bone: HashMap<usize, BoneChannel> = HashMap::new();

        for ch in anim.channels() {
            let target_node = ch.target().node().index() as i64;
            let Some(&bidx) = id_to_idx.get(&target_node) else { continue };
            let reader = ch.reader(|b| buffers.get(b.index()).map(|d| d.as_slice()));
            let Some(inputs) = reader.read_inputs() else { continue };
            let times: Vec<f32> = inputs.collect();
            if times.is_empty() {
                continue;
            }
            if let Some(&tmax) = times.last() {
                dur = dur.max(tmax);
            }
            let entry = per_bone.entry(bidx).or_insert_with(|| BoneChannel {
                bone_index: bidx,
                times: times.clone(),
                translations: Vec::new(),
                rotations: Vec::new(),
                scales: Vec::new(),
            });
            if entry.times.len() < times.len() {
                entry.times = times.clone();
            }
            if let Some(outs) = reader.read_outputs() {
                match outs {
                    gltf::animation::util::ReadOutputs::Translations(it) => {
                        entry.translations = it.collect();
                    }
                    gltf::animation::util::ReadOutputs::Rotations(it) => {
                        entry.rotations = it.into_f32().map(normalize_quat).collect();
                    }
                    gltf::animation::util::ReadOutputs::Scales(it) => {
                        entry.scales = it.collect();
                    }
                    gltf::animation::util::ReadOutputs::MorphTargetWeights(_) => {}
                }
            }
        }

        let channels: Vec<BoneChannel> = per_bone
            .into_values()
            .filter(|c| !c.translations.is_empty() || !c.rotations.is_empty() || !c.scales.is_empty())
            .collect();
        if !channels.is_empty() {
            clips.push(AnimationClip {
                name: cname,
                duration_secs: dur.max(0.1),
                channels,
            });
        }
    }

    if bones.is_empty() && clips.is_empty() {
        return None;
    }

    Some(SkeletonAnimation {
        species_hint: Some(hint),
        bones,
        clips,
    })
}
