use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Cursor, Read as _};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use cadmpeg_codec_freecad::FcstdCodec;
use cadmpeg_codec_rhino::RhinoCodec;
use cadmpeg_codec_step::StepCodec;
use cadmpeg_ir::codec::{Codec as _, DecodeOptions};
use cadmpeg_ir::eval::{
    admission::EvaluationAdmission, analytic_surface_parameters, model_curve_point_by_id,
    model_surface_partials_by_id,
};
use cadmpeg_ir::geometry::SolvedSurfaceGeometry;
use cadmpeg_ir::index::ModelIndex;
use cadmpeg_ir::topology::Sense;
use cadmpeg_ir::CadIr;

use crate::decoders::blend;
use crate::decoders::text::decode_to_utf8;
use crate::model::{MeshBatch, MetadataEntry, TreeNode, ViewerPayload};

const MAX_RENDER_TRIS: usize = 300_000;
const MAX_RENDER_POINTS: usize = 450_000;

/// Decodes 3D meshes, Gaussian splats, Blender scenes, CAD B-rep models, DXF, G-code, Gerber/KiCad, FBX.
pub fn decode_3d_or_cad(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    match format_id {
        "stl" => decode_stl(bytes, metadata),
        "obj" => decode_obj(bytes, metadata),
        "ply" => decode_ply(bytes, metadata),
        "splat" => decode_splat(bytes, metadata),
        "pcd" | "pts" | "xyz" => decode_point_cloud_text(bytes, format_id, metadata),
        "glb" | "gltf" => decode_gltf(bytes, metadata),
        "3mf" => decode_3mf(bytes, metadata),
        "step" | "stp" | "ifc" => decode_cadmpeg_step(bytes, format_id, metadata),
        "iges" | "igs" => decode_iges(bytes, metadata),
        "3dm" => decode_cadmpeg_rhino(bytes, metadata),
        "fcstd" | "fcstd1" => decode_cadmpeg_freecad(bytes, metadata),
        "blend" => decode_blend(bytes, metadata),
        "gcode" | "gco" | "nc" | "ngc" | "tap" | "cnc" => decode_gcode(bytes, metadata),
        "dxf" => decode_dxf(bytes, metadata),
        "gerber" | "gbr" | "gbx" | "gtl" | "gbl" | "gts" | "gbs" | "gto" | "gbo" => {
            decode_gerber(bytes, metadata)
        }
        "kicad_pcb" | "kicad_sch" | "kicad_mod" => decode_kicad(bytes, format_id, metadata),
        "fbx" => decode_fbx_summary(bytes, metadata),
        _ => None,
    }
}

fn f32_slice_to_b64(slice: &[f32]) -> String {
    if slice.is_empty() {
        return String::new();
    }
    let mut raw = Vec::with_capacity(slice.len() * 4);
    for &v in slice {
        raw.extend_from_slice(&v.to_le_bytes());
    }
    B64.encode(&raw)
}

#[derive(Default)]
struct MeshBuild {
    format_label: String,
    positions: Vec<f32>,
    normals: Vec<f32>,
    uvs: Vec<f32>,
    colors: Vec<f32>,
    wireframe_lines: Vec<f32>,
    point_positions: Vec<f32>,
    point_colors: Vec<f32>,
    textures: Vec<String>,
    batches: Vec<MeshBatch>,
    parts: Vec<String>,
    cad_summary: Option<String>,
    is_gaussian_splat: bool,
}

impl MeshBuild {
    fn finalize(mut self) -> ViewerPayload {
        let tri_count = self.positions.len() / 9;
        let vert_count = self.positions.len() / 3;
        let point_count = self.point_positions.len() / 3;
        if self.normals.len() != self.positions.len() && self.positions.len() >= 9 {
            self.normals = compute_flat_normals(&self.positions);
        }

        let mut bmin = [f32::INFINITY; 3];
        let mut bmax = [f32::NEG_INFINITY; 3];
        for p in self
            .positions
            .chunks_exact(3)
            .chain(self.wireframe_lines.chunks_exact(3))
            .chain(self.point_positions.chunks_exact(3))
        {
            if p[0].is_finite() && p[1].is_finite() && p[2].is_finite() {
                for k in 0..3 {
                    bmin[k] = bmin[k].min(p[k]);
                    bmax[k] = bmax[k].max(p[k]);
                }
            }
        }
        if !bmin[0].is_finite() {
            bmin = [-1.0, -1.0, -1.0];
            bmax = [1.0, 1.0, 1.0];
        }

        ViewerPayload::Mesh3d {
            format_label: self.format_label,
            vertex_count: vert_count.max(self.wireframe_lines.len() / 3).max(point_count),
            triangle_count: tri_count,
            edge_count: self.wireframe_lines.len() / 6,
            point_count,
            is_gaussian_splat: self.is_gaussian_splat,
            bounds_min: bmin,
            bounds_max: bmax,
            positions_b64: f32_slice_to_b64(&self.positions),
            normals_b64: f32_slice_to_b64(&self.normals),
            uvs_b64: f32_slice_to_b64(&self.uvs),
            colors_b64: f32_slice_to_b64(&self.colors),
            wireframe_b64: f32_slice_to_b64(&self.wireframe_lines),
            point_positions_b64: f32_slice_to_b64(&self.point_positions),
            point_colors_b64: f32_slice_to_b64(&self.point_colors),
            textures: self.textures,
            batches: self.batches,
            parts: self.parts,
            cad_summary: self.cad_summary,
        }
    }
}

fn finalize_mesh(
    format_label: String,
    positions: Vec<f32>,
    normals: Vec<f32>,
    wireframe_lines: Vec<f32>,
    parts: Vec<String>,
    cad_summary: Option<String>,
) -> ViewerPayload {
    MeshBuild {
        format_label,
        positions,
        normals,
        wireframe_lines,
        parts,
        cad_summary,
        ..Default::default()
    }
    .finalize()
}

fn compute_flat_normals(positions: &[f32]) -> Vec<f32> {
    let mut normals = Vec::with_capacity(positions.len());
    for tri in positions.chunks_exact(9) {
        let ax = tri[3] - tri[0];
        let ay = tri[4] - tri[1];
        let az = tri[5] - tri[2];
        let bx = tri[6] - tri[0];
        let by = tri[7] - tri[1];
        let bz = tri[8] - tri[2];
        let mut nx = ay * bz - az * by;
        let mut ny = az * bx - ax * bz;
        let mut nz = ax * by - ay * bx;
        let len = (nx * nx + ny * ny + nz * nz).sqrt();
        if len > 1e-12 {
            nx /= len;
            ny /= len;
            nz /= len;
        } else {
            nx = 0.0;
            ny = 0.0;
            nz = 1.0;
        }
        for _ in 0..3 {
            normals.extend_from_slice(&[nx, ny, nz]);
        }
    }
    normals
}

fn decode_stl(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.starts_with(b"solid ") {
        let text = String::from_utf8_lossy(bytes);
        if text.contains("facet normal") {
            let mut positions = Vec::new();
            let mut normals = Vec::new();
            let mut cur_n = [0.0f32, 0.0, 1.0];
            for line in text.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("facet normal") {
                    if let Some(n) = parse_3_f32(rest) {
                        cur_n = n;
                    }
                } else if let Some(rest) = t.strip_prefix("vertex") {
                    if let Some(v) = parse_3_f32(rest) {
                        if positions.len() < MAX_RENDER_TRIS * 9 {
                            positions.extend_from_slice(&v);
                            normals.extend_from_slice(&cur_n);
                        }
                    }
                }
            }
            if positions.len() >= 9 {
                let tris = positions.len() / 9;
                metadata.push(MetadataEntry { section: "STL".into(), key: "Encoding".into(), value: "ASCII".into() });
                metadata.push(MetadataEntry { section: "STL".into(), key: "Triangles".into(), value: tris.to_string() });
                return Some(finalize_mesh("ASCII STL Mesh".into(), positions, normals, Vec::new(), Vec::new(), None));
            }
        }
    }

    if bytes.len() < 84 {
        return None;
    }
    let tri_count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
    let available = (bytes.len() - 84) / 50;
    let count = tri_count.min(available).min(MAX_RENDER_TRIS);
    if count == 0 {
        return None;
    }
    let mut positions = Vec::with_capacity(count * 9);
    let mut normals = Vec::with_capacity(count * 9);
    for i in 0..count {
        let base = 84 + i * 50;
        let rec = &bytes[base..base + 50];
        let nx = read_f32_le(rec, 0);
        let ny = read_f32_le(rec, 4);
        let nz = read_f32_le(rec, 8);
        for v in 0..3 {
            let off = 12 + v * 12;
            positions.push(read_f32_le(rec, off));
            positions.push(read_f32_le(rec, off + 4));
            positions.push(read_f32_le(rec, off + 8));
            normals.extend_from_slice(&[nx, ny, nz]);
        }
    }
    metadata.push(MetadataEntry { section: "STL".into(), key: "Encoding".into(), value: "Binary".into() });
    metadata.push(MetadataEntry { section: "STL".into(), key: "Triangles".into(), value: tri_count.to_string() });
    Some(finalize_mesh(
        format!("Binary STL Mesh ({tri_count} tris)"),
        positions,
        normals,
        Vec::new(),
        Vec::new(),
        None,
    ))
}

#[inline]
fn read_f32_le(b: &[u8], off: usize) -> f32 {
    f32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
fn parse_3_f32(s: &str) -> Option<[f32; 3]> {
    let mut it = s.split_ascii_whitespace();
    let x = it.next()?.parse::<f32>().ok()?;
    let y = it.next()?.parse::<f32>().ok()?;
    let z = it.next()?.parse::<f32>().ok()?;
    Some([x, y, z])
}

/// Fast Wavefront OBJ decoder with zero per-line Vec allocations and vertex color/point fallback.
fn decode_obj(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let text = String::from_utf8_lossy(bytes);
    let est_lines = (bytes.len() / 28).max(16);
    let mut verts: Vec<[f32; 3]> = Vec::with_capacity(est_lines / 3);
    let mut vcols: Vec<[f32; 3]> = Vec::new();
    let mut vnorms: Vec<[f32; 3]> = Vec::with_capacity(est_lines / 4);
    let mut vtex: Vec<[f32; 2]> = Vec::new();
    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut uvs = Vec::new();
    let mut colors = Vec::new();
    let mut parts = Vec::new();
    let mut face_buf: Vec<(usize, Option<usize>, Option<usize>)> = Vec::with_capacity(8);

    for line in text.lines() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix("v ") {
            let mut it = rest.split_ascii_whitespace();
            if let (Some(xs), Some(ys), Some(zs)) = (it.next(), it.next(), it.next()) {
                if let (Ok(x), Ok(y), Ok(z)) = (xs.parse::<f32>(), ys.parse::<f32>(), zs.parse::<f32>()) {
                    verts.push([x, y, z]);
                    if let (Some(rs), Some(gs), Some(bs)) = (it.next(), it.next(), it.next()) {
                        if let (Ok(r), Ok(g), Ok(b)) = (rs.parse::<f32>(), gs.parse::<f32>(), bs.parse::<f32>()) {
                            if vcols.len() + 1 == verts.len() {
                                vcols.push([r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]);
                            }
                        }
                    }
                }
            }
        } else if let Some(rest) = t.strip_prefix("vn ") {
            if let Some(n) = parse_3_f32(rest) {
                vnorms.push(n);
            }
        } else if let Some(rest) = t.strip_prefix("vt ") {
            let mut it = rest.split_ascii_whitespace();
            if let (Some(us), Some(vs)) = (it.next(), it.next()) {
                if let (Ok(u), Ok(v)) = (us.parse::<f32>(), vs.parse::<f32>()) {
                    vtex.push([u, 1.0 - v]);
                }
            }
        } else if let Some(name) = t.strip_prefix("o ").or_else(|| t.strip_prefix("g ")) {
            if parts.len() < 128 {
                parts.push(name.trim().to_string());
            }
        } else if let Some(rest) = t.strip_prefix("f ") {
            if positions.len() >= MAX_RENDER_TRIS * 9 {
                continue;
            }
            face_buf.clear();
            for tok in rest.split_ascii_whitespace() {
                let mut segs = tok.split('/');
                let Some(vi_raw) = segs.next().and_then(|s| s.parse::<isize>().ok()) else { continue };
                let Some(vi) = resolve_obj_index(vi_raw, verts.len()) else { continue };
                let vt = segs
                    .next()
                    .and_then(|s| if s.is_empty() { None } else { s.parse::<isize>().ok() })
                    .and_then(|n| resolve_obj_index(n, vtex.len()));
                let vn = segs
                    .next()
                    .and_then(|s| if s.is_empty() { None } else { s.parse::<isize>().ok() })
                    .and_then(|n| resolve_obj_index(n, vnorms.len()));
                face_buf.push((vi, vt, vn));
            }
            if face_buf.len() >= 3 {
                for k in 1..face_buf.len() - 1 {
                    for &(vi, vt, vn) in &[face_buf[0], face_buf[k], face_buf[k + 1]] {
                        positions.extend_from_slice(&verts[vi]);
                        if let Some(ni) = vn {
                            normals.extend_from_slice(&vnorms[ni]);
                        }
                        if let Some(ti) = vt {
                            uvs.extend_from_slice(&vtex[ti]);
                        }
                        if let Some(c) = vcols.get(vi) {
                            colors.extend_from_slice(c);
                        }
                    }
                }
            }
        }
    }

    if verts.is_empty() {
        return None;
    }

    let mut point_positions = Vec::new();
    let mut point_colors = Vec::new();
    if positions.is_empty() {
        let step = (verts.len() / MAX_RENDER_POINTS).max(1);
        for (idx, v) in verts.iter().step_by(step).enumerate() {
            point_positions.extend_from_slice(v);
            if let Some(c) = vcols.get(idx * step) {
                point_colors.extend_from_slice(c);
            }
        }
    }

    metadata.push(MetadataEntry { section: "Wavefront OBJ".into(), key: "Vertices".into(), value: verts.len().to_string() });
    metadata.push(MetadataEntry { section: "Wavefront OBJ".into(), key: "Triangles".into(), value: (positions.len() / 9).to_string() });

    Some(
        MeshBuild {
            format_label: "Wavefront 3D OBJ".into(),
            positions,
            normals,
            uvs,
            colors,
            point_positions,
            point_colors,
            parts,
            ..Default::default()
        }
        .finalize(),
    )
}

#[inline]
fn resolve_obj_index(idx: isize, len: usize) -> Option<usize> {
    if idx > 0 {
        let i = (idx - 1) as usize;
        (i < len).then_some(i)
    } else if idx < 0 {
        let abs = (-idx) as usize;
        (abs <= len).then(|| len - abs)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
enum PlyEncoding {
    Ascii,
    BinaryLe,
    BinaryBe,
}

#[derive(Clone, Copy)]
enum PlyType {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl PlyType {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "char" | "int8" => Some(Self::I8),
            "uchar" | "uint8" => Some(Self::U8),
            "short" | "int16" => Some(Self::I16),
            "ushort" | "uint16" => Some(Self::U16),
            "int" | "int32" => Some(Self::I32),
            "uint" | "uint32" => Some(Self::U32),
            "float" | "float32" => Some(Self::F32),
            "double" | "float64" => Some(Self::F64),
            _ => None,
        }
    }

    fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    fn read_bin(self, b: &[u8], le: bool) -> f32 {
        match self {
            Self::I8 => (b[0] as i8) as f32,
            Self::U8 => b[0] as f32,
            Self::I16 => (if le { i16::from_le_bytes([b[0], b[1]]) } else { i16::from_be_bytes([b[0], b[1]]) }) as f32,
            Self::U16 => (if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) }) as f32,
            Self::I32 => (if le { i32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { i32::from_be_bytes([b[0], b[1], b[2], b[3]]) }) as f32,
            Self::U32 => (if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) }) as f32,
            Self::F32 => if le { f32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { f32::from_be_bytes([b[0], b[1], b[2], b[3]]) },
            Self::F64 => {
                let arr: [u8; 8] = b[0..8].try_into().unwrap_or([0; 8]);
                (if le { f64::from_le_bytes(arr) } else { f64::from_be_bytes(arr) }) as f32
            }
        }
    }
}

struct PlyProp {
    name: String,
    ty: Option<PlyType>,
    list_count_ty: Option<PlyType>,
    list_item_ty: Option<PlyType>,
    offset: usize,
}

struct PlyElement {
    name: String,
    count: usize,
    props: Vec<PlyProp>,
    stride: usize,
}

/// Decodes ASCII, binary_little_endian, and binary_big_endian PLY meshes, RGB point clouds, and 3D Gaussian Splats.
fn decode_ply(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let hdr_end = bytes.windows(10).position(|w| w == b"end_header")?;
    let mut body_start = hdr_end + 10;
    while body_start < bytes.len() && matches!(bytes[body_start], b'\r' | b'\n') {
        body_start += 1;
    }
    let header = String::from_utf8_lossy(&bytes[..hdr_end]);
    let mut enc = PlyEncoding::Ascii;
    let mut elements: Vec<PlyElement> = Vec::new();

    for line in header.lines() {
        let t = line.trim();
        if t.starts_with("format binary_little_endian") {
            enc = PlyEncoding::BinaryLe;
        } else if t.starts_with("format binary_big_endian") {
            enc = PlyEncoding::BinaryBe;
        } else if let Some(rest) = t.strip_prefix("element ") {
            let mut it = rest.split_whitespace();
            if let (Some(name), Some(cnt)) = (it.next(), it.next().and_then(|s| s.parse::<usize>().ok())) {
                elements.push(PlyElement { name: name.into(), count: cnt, props: Vec::new(), stride: 0 });
            }
        } else if let Some(rest) = t.strip_prefix("property ") {
            if let Some(el) = elements.last_mut() {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.first() == Some(&"list") && parts.len() >= 4 {
                    el.props.push(PlyProp {
                        name: parts[3].into(),
                        ty: None,
                        list_count_ty: PlyType::from_str(parts[1]),
                        list_item_ty: PlyType::from_str(parts[2]),
                        offset: el.stride,
                    });
                } else if parts.len() >= 2 {
                    if let Some(pt) = PlyType::from_str(parts[0]) {
                        el.props.push(PlyProp {
                            name: parts[1].into(),
                            ty: Some(pt),
                            list_count_ty: None,
                            list_item_ty: None,
                            offset: el.stride,
                        });
                        el.stride += pt.size();
                    }
                }
            }
        }
    }

    let v_el = elements.iter().find(|e| e.name == "vertex")?;
    let f_el = elements.iter().find(|e| e.name == "face");
    let v_count = v_el.count;
    let f_count = f_el.map(|f| f.count).unwrap_or(0);

    let prop_idx = |name: &str| v_el.props.iter().position(|p| p.name == name);
    let ix = prop_idx("x")?;
    let iy = prop_idx("y")?;
    let iz = prop_idx("z")?;
    let ir = prop_idx("red").or_else(|| prop_idx("r")).or_else(|| prop_idx("diffuse_red"));
    let ig = prop_idx("green").or_else(|| prop_idx("g")).or_else(|| prop_idx("diffuse_green"));
    let ib = prop_idx("blue").or_else(|| prop_idx("b")).or_else(|| prop_idx("diffuse_blue"));
    let fdc0 = prop_idx("f_dc_0");
    let fdc1 = prop_idx("f_dc_1");
    let fdc2 = prop_idx("f_dc_2");
    let i_opac = prop_idx("opacity");

    let is_gs = fdc0.is_some() && fdc1.is_some() && fdc2.is_some();
    let has_rgb = ir.is_some() && ig.is_some() && ib.is_some();

    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut vcols: Vec<[f32; 3]> = Vec::new();
    let mut positions = Vec::new();
    let mut colors = Vec::new();

    const SH_C0: f32 = 0.28209479177387814;

    match enc {
        PlyEncoding::Ascii => {
            let body = String::from_utf8_lossy(&bytes[body_start..]);
            let mut lines = body.lines();
            let step = if f_count == 0 { (v_count / MAX_RENDER_POINTS).max(1) } else { 1 };
            verts.reserve((v_count / step).min(MAX_RENDER_POINTS));
            for vi in 0..v_count {
                let Some(l) = lines.next() else { break };
                if vi % step != 0 { continue; }
                let vals: Vec<f32> = l.split_ascii_whitespace().filter_map(|s| s.parse().ok()).collect();
                if let (Some(&x), Some(&y), Some(&z)) = (vals.get(ix), vals.get(iy), vals.get(iz)) {
                    let pt = if is_gs { [x, -y, -z] } else { [x, y, z] };
                    verts.push(pt);
                    if is_gs {
                        let c0 = vals.get(fdc0.unwrap()).copied().unwrap_or(0.0);
                        let c1 = vals.get(fdc1.unwrap()).copied().unwrap_or(0.0);
                        let c2 = vals.get(fdc2.unwrap()).copied().unwrap_or(0.0);
                        vcols.push([
                            (0.5 + SH_C0 * c0).clamp(0.0, 1.0),
                            (0.5 + SH_C0 * c1).clamp(0.0, 1.0),
                            (0.5 + SH_C0 * c2).clamp(0.0, 1.0),
                        ]);
                    } else if has_rgb {
                        let r_ty = v_el.props[ir.unwrap()].ty;
                        let g_ty = v_el.props[ig.unwrap()].ty;
                        let b_ty = v_el.props[ib.unwrap()].ty;
                        vcols.push([
                            norm_ply_color(vals.get(ir.unwrap()).copied().unwrap_or(234.0), r_ty),
                            norm_ply_color(vals.get(ig.unwrap()).copied().unwrap_or(255.0), g_ty),
                            norm_ply_color(vals.get(ib.unwrap()).copied().unwrap_or(255.0), b_ty),
                        ]);
                    }
                }
            }
            for _ in 0..f_count {
                let Some(l) = lines.next() else { break };
                let idxs: Vec<usize> = l.split_ascii_whitespace().filter_map(|s| s.parse().ok()).collect();
                if idxs.len() >= 4 && positions.len() < MAX_RENDER_TRIS * 9 {
                    let n = idxs[0].min(idxs.len() - 1);
                    for k in 1..n.saturating_sub(1) {
                        for &vi in &[idxs[1], idxs[1 + k], idxs[2 + k]] {
                            if let Some(v) = verts.get(vi) {
                                positions.extend_from_slice(v);
                                if let Some(c) = vcols.get(vi) {
                                    colors.extend_from_slice(c);
                                }
                            }
                        }
                    }
                }
            }
        }
        PlyEncoding::BinaryLe | PlyEncoding::BinaryBe => {
            let le = matches!(enc, PlyEncoding::BinaryLe);
            let mut pos = body_start;
            for el in &elements {
                if el.name == "vertex" {
                    let stride = el.stride.max(1);
                    let step = if f_count == 0 { (el.count / MAX_RENDER_POINTS).max(1) } else { 1 };
                    verts.reserve((el.count / step).min(MAX_RENDER_POINTS));
                    let px = &el.props[ix];
                    let py = &el.props[iy];
                    let pz = &el.props[iz];

                    for vi in 0..el.count {
                        if pos + stride > bytes.len() { break; }
                        let rec = &bytes[pos..pos + stride];
                        pos += stride;
                        if vi % step != 0 { continue; }

                        if let Some(oi) = i_opac {
                            let op = &el.props[oi];
                            if let Some(ot) = op.ty {
                                let raw_op = ot.read_bin(&rec[op.offset..], le);
                                // Filter invisible near-zero-alpha background splats if many points exist
                                if is_gs && el.count > 100_000 && raw_op < -3.0 {
                                    continue;
                                }
                            }
                        }

                        let x = px.ty?.read_bin(&rec[px.offset..], le);
                        let y = py.ty?.read_bin(&rec[py.offset..], le);
                        let z = pz.ty?.read_bin(&rec[pz.offset..], le);
                        if !x.is_finite() || !y.is_finite() || !z.is_finite() { continue; }
                        let pt = if is_gs { [x, -y, -z] } else { [x, y, z] };
                        verts.push(pt);

                        if is_gs {
                            let p0 = &el.props[fdc0.unwrap()];
                            let p1 = &el.props[fdc1.unwrap()];
                            let p2 = &el.props[fdc2.unwrap()];
                            let c0 = p0.ty.map(|t| t.read_bin(&rec[p0.offset..], le)).unwrap_or(0.0);
                            let c1 = p1.ty.map(|t| t.read_bin(&rec[p1.offset..], le)).unwrap_or(0.0);
                            let c2 = p2.ty.map(|t| t.read_bin(&rec[p2.offset..], le)).unwrap_or(0.0);
                            vcols.push([
                                (0.5 + SH_C0 * c0).clamp(0.0, 1.0),
                                (0.5 + SH_C0 * c1).clamp(0.0, 1.0),
                                (0.5 + SH_C0 * c2).clamp(0.0, 1.0),
                            ]);
                        } else if has_rgb {
                            let pr = &el.props[ir.unwrap()];
                            let pg = &el.props[ig.unwrap()];
                            let pb = &el.props[ib.unwrap()];
                            let r = pr.ty.map(|t| norm_ply_color(t.read_bin(&rec[pr.offset..], le), Some(t))).unwrap_or(0.92);
                            let g = pg.ty.map(|t| norm_ply_color(t.read_bin(&rec[pg.offset..], le), Some(t))).unwrap_or(1.0);
                            let b = pb.ty.map(|t| norm_ply_color(t.read_bin(&rec[pb.offset..], le), Some(t))).unwrap_or(1.0);
                            vcols.push([r, g, b]);
                        }
                    }
                } else if el.name == "face" {
                    let list_prop = el.props.iter().find(|p| p.list_count_ty.is_some());
                    if let Some(lp) = list_prop {
                        let cty = lp.list_count_ty.unwrap();
                        let ity = lp.list_item_ty.unwrap();
                        let csz = cty.size();
                        let isz = ity.size();
                        for _ in 0..el.count {
                            if pos + csz > bytes.len() { break; }
                            let n = cty.read_bin(&bytes[pos..], le) as usize;
                            pos += csz;
                            if n > 64 || pos + n * isz > bytes.len() { break; }
                            if n >= 3 && positions.len() < MAX_RENDER_TRIS * 9 {
                                let i0 = ity.read_bin(&bytes[pos..], le) as usize;
                                for k in 1..n - 1 {
                                    let i1 = ity.read_bin(&bytes[pos + k * isz..], le) as usize;
                                    let i2 = ity.read_bin(&bytes[pos + (k + 1) * isz..], le) as usize;
                                    for &vi in &[i0, i1, i2] {
                                        if let Some(v) = verts.get(vi) {
                                            positions.extend_from_slice(v);
                                            if let Some(c) = vcols.get(vi) {
                                                colors.extend_from_slice(c);
                                            }
                                        }
                                    }
                                }
                            }
                            pos += n * isz;
                        }
                    }
                } else if el.stride > 0 && el.props.iter().all(|p| p.ty.is_some()) {
                    pos = pos.saturating_add(el.count.saturating_mul(el.stride));
                }
            }
        }
    }

    let mut point_positions = Vec::new();
    let mut point_colors = Vec::new();
    if positions.is_empty() && !verts.is_empty() {
        point_positions.reserve(verts.len() * 3);
        for v in &verts {
            point_positions.extend_from_slice(v);
        }
        if !vcols.is_empty() {
            point_colors.reserve(vcols.len() * 3);
            for c in &vcols {
                point_colors.extend_from_slice(c);
            }
        }
    }

    let label = if is_gs {
        format!("3D Gaussian Splatting PLY ({} splats)", verts.len())
    } else if !positions.is_empty() {
        format!("PLY 3D Mesh ({} tris)", positions.len() / 9)
    } else {
        format!("PLY Point Cloud ({} pts)", verts.len())
    };
    metadata.push(MetadataEntry { section: "PLY".into(), key: "Type".into(), value: if is_gs { "3D Gaussian Splat" } else { "Mesh / Point Cloud" }.into() });
    metadata.push(MetadataEntry { section: "PLY".into(), key: "Vertices / Splats".into(), value: v_count.to_string() });
    metadata.push(MetadataEntry { section: "PLY".into(), key: "Faces".into(), value: f_count.to_string() });

    Some(
        MeshBuild {
            format_label: label,
            positions,
            colors,
            point_positions,
            point_colors,
            is_gaussian_splat: is_gs,
            ..Default::default()
        }
        .finalize(),
    )
}

#[inline]
fn norm_ply_color(val: f32, ty: Option<PlyType>) -> f32 {
    match ty {
        Some(PlyType::U8) => (val / 255.0).clamp(0.0, 1.0),
        _ => if val > 1.0 { (val / 255.0).clamp(0.0, 1.0) } else { val.clamp(0.0, 1.0) },
    }
}

/// Decodes raw 32-byte `.splat` Gaussian Splatting binary format (xyz f32, scale f32, rgba u8, rot u8).
fn decode_splat(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let total = bytes.len() / 32;
    if total == 0 {
        return None;
    }
    let step = (total / MAX_RENDER_POINTS).max(1);
    let mut point_positions = Vec::with_capacity((total / step) * 3);
    let mut point_colors = Vec::with_capacity((total / step) * 3);

    for i in (0..total).step_by(step) {
        let rec = &bytes[i * 32..i * 32 + 32];
        let x = read_f32_le(rec, 0);
        let y = read_f32_le(rec, 4);
        let z = read_f32_le(rec, 8);
        if !x.is_finite() || !y.is_finite() || !z.is_finite() { continue; }
        point_positions.extend_from_slice(&[x, -y, -z]);
        point_colors.extend_from_slice(&[
            rec[24] as f32 / 255.0,
            rec[25] as f32 / 255.0,
            rec[26] as f32 / 255.0,
        ]);
    }
    metadata.push(MetadataEntry { section: "Gaussian Splat".into(), key: "Splats".into(), value: total.to_string() });
    Some(
        MeshBuild {
            format_label: format!("3D Gaussian Splat ({total} splats)"),
            point_positions,
            point_colors,
            is_gaussian_splat: true,
            ..Default::default()
        }
        .finalize(),
    )
}

fn decode_point_cloud_text(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let text = String::from_utf8_lossy(bytes);
    let mut pts = Vec::new();
    let mut cols = Vec::new();
    let mut in_data = format_id != "pcd";
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') { continue; }
        if format_id == "pcd" && !in_data {
            if t.starts_with("DATA ascii") {
                in_data = true;
            }
            continue;
        }
        let nums: Vec<f32> = t.split_ascii_whitespace().filter_map(|s| s.parse().ok()).collect();
        if nums.len() >= 3 && pts.len() < MAX_RENDER_POINTS * 3 {
            pts.extend_from_slice(&nums[0..3]);
            if nums.len() >= 6 {
                let scale = if nums[3] > 1.0 || nums[4] > 1.0 || nums[5] > 1.0 { 255.0 } else { 1.0 };
                cols.extend_from_slice(&[
                    (nums[3] / scale).clamp(0.0, 1.0),
                    (nums[4] / scale).clamp(0.0, 1.0),
                    (nums[5] / scale).clamp(0.0, 1.0),
                ]);
            }
        }
    }
    if pts.is_empty() { return None; }
    metadata.push(MetadataEntry { section: "Point Cloud".into(), key: "Points".into(), value: (pts.len() / 3).to_string() });
    Some(
        MeshBuild {
            format_label: format!("{} Point Cloud ({} pts)", format_id.to_ascii_uppercase(), pts.len() / 3),
            point_positions: pts,
            point_colors: cols,
            ..Default::default()
        }
        .finalize(),
    )
}

/// Decodes glTF / GLB with scene node transforms, UVs, vertex colors, base color factors, and embedded textures!
fn decode_gltf(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let gltf = gltf::Gltf::from_slice(bytes).ok()?;
    let doc = &gltf.document;
    let mut buffers: Vec<Vec<u8>> = Vec::new();
    for buf in doc.buffers() {
        match buf.source() {
            gltf::buffer::Source::Bin => buffers.push(gltf.blob.clone().unwrap_or_default()),
            gltf::buffer::Source::Uri(uri) => {
                if let Some((_, b64)) = uri.split_once(";base64,") {
                    buffers.push(B64.decode(b64).unwrap_or_default());
                } else {
                    buffers.push(Vec::new());
                }
            }
        }
    }

    // Extract embedded images into data URLs
    let mut textures: Vec<String> = Vec::new();
    for img in doc.images() {
        let data_url = match img.source() {
            gltf::image::Source::View { view, mime_type } => {
                let buf = buffers.get(view.buffer().index()).map(|d| d.as_slice()).unwrap_or(&[]);
                let start = view.offset();
                let end = (start + view.length()).min(buf.len());
                if start < end {
                    format!("data:{mime_type};base64,{}", B64.encode(&buf[start..end]))
                } else {
                    String::new()
                }
            }
            gltf::image::Source::Uri { uri, .. } => {
                if uri.starts_with("data:") { uri.to_string() } else { String::new() }
            }
        };
        textures.push(data_url);
    }

    let mut build = MeshBuild {
        format_label: "glTF 2.0 / GLB 3D Asset".into(),
        textures,
        ..Default::default()
    };

    fn mul_mat4(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
        let mut o = [[0.0f32; 4]; 4];
        for c in 0..4 {
            for r in 0..4 {
                o[c][r] = a[0][r] * b[c][0] + a[1][r] * b[c][1] + a[2][r] * b[c][2] + a[3][r] * b[c][3];
            }
        }
        o
    }

    fn xform_pos(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
        [
            m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0],
            m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1],
            m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2],
        ]
    }

    fn xform_nrm(m: &[[f32; 4]; 4], n: [f32; 3]) -> [f32; 3] {
        let mut x = m[0][0] * n[0] + m[1][0] * n[1] + m[2][0] * n[2];
        let mut y = m[0][1] * n[0] + m[1][1] * n[1] + m[2][1] * n[2];
        let mut z = m[0][2] * n[0] + m[1][2] * n[1] + m[2][2] * n[2];
        let len = (x * x + y * y + z * z).sqrt().max(1e-8);
        x /= len; y /= len; z /= len;
        [x, y, z]
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_node(
        node: gltf::Node<'_>,
        parent: [[f32; 4]; 4],
        buffers: &[Vec<u8>],
        build: &mut MeshBuild,
    ) {
        let world = mul_mat4(parent, node.transform().matrix());
        if let Some(mesh) = node.mesh() {
            if let Some(name) = mesh.name().or(node.name()) {
                if build.parts.len() < 128 {
                    build.parts.push(name.to_string());
                }
            }
            for prim in mesh.primitives() {
                let reader = prim.reader(|b| buffers.get(b.index()).map(|d| d.as_slice()));
                let Some(pos_iter) = reader.read_positions() else { continue };
                let verts: Vec<[f32; 3]> = pos_iter.collect();
                let norms: Vec<[f32; 3]> = reader.read_normals().map(|n| n.collect()).unwrap_or_default();
                let texs: Vec<[f32; 2]> = reader.read_tex_coords(0).map(|tc| tc.into_f32().collect()).unwrap_or_default();
                let vcols: Vec<[f32; 3]> = reader.read_colors(0).map(|c| c.into_rgb_f32().collect()).unwrap_or_default();

                let mat = prim.material();
                let pbr = mat.pbr_metallic_roughness();
                let base_color = pbr.base_color_factor();
                let tex_idx = pbr
                    .base_color_texture()
                    .map(|info| info.texture().source().index())
                    .filter(|&ti| build.textures.get(ti).is_some_and(|s| !s.is_empty()));

                let start_v = (build.positions.len() / 3) as u32;

                let mut push_vert = |idx: usize| {
                    if let Some(&v) = verts.get(idx) {
                        build.positions.extend_from_slice(&xform_pos(&world, v));
                        if let Some(&n) = norms.get(idx) {
                            build.normals.extend_from_slice(&xform_nrm(&world, n));
                        } else if !norms.is_empty() {
                            build.normals.extend_from_slice(&[0.0, 1.0, 0.0]);
                        }
                        if let Some(&t) = texs.get(idx) {
                            build.uvs.extend_from_slice(&t);
                        } else {
                            build.uvs.extend_from_slice(&[0.0, 0.0]);
                        }
                        if let Some(&c) = vcols.get(idx) {
                            build.colors.extend_from_slice(&c);
                        } else {
                            build.colors.extend_from_slice(&[1.0, 1.0, 1.0]);
                        }
                    }
                };

                let max_extra = (MAX_RENDER_TRIS * 3).saturating_sub(start_v as usize);
                let mut pushed = 0usize;
                if let Some(ind_reader) = reader.read_indices() {
                    let idxs: Vec<u32> = ind_reader.into_u32().collect();
                    for tri in idxs.chunks_exact(3).take(MAX_RENDER_TRIS) {
                        if pushed + 3 > max_extra { break; }
                        push_vert(tri[0] as usize);
                        push_vert(tri[1] as usize);
                        push_vert(tri[2] as usize);
                        pushed += 3;
                    }
                } else {
                    for vi in 0..verts.len().min(MAX_RENDER_TRIS * 3) {
                        if pushed >= max_extra { break; }
                        push_vert(vi);
                        pushed += 1;
                    }
                }

                let end_v = (build.positions.len() / 3) as u32;
                if end_v > start_v {
                    build.batches.push(MeshBatch {
                        start_vertex: start_v,
                        vertex_count: end_v - start_v,
                        base_color,
                        texture_index: tex_idx,
                    });
                }
            }
        }
        for child in node.children() {
            visit_node(child, world, buffers, build);
        }
    }

    let identity = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
    if let Some(scene) = doc.default_scene().or_else(|| doc.scenes().next()) {
        for node in scene.nodes() {
            visit_node(node, identity, &buffers, &mut build);
        }
    }

    metadata.push(MetadataEntry { section: "glTF 2.0".into(), key: "Meshes".into(), value: doc.meshes().count().to_string() });
    metadata.push(MetadataEntry { section: "glTF 2.0".into(), key: "Textures".into(), value: build.textures.iter().filter(|s| !s.is_empty()).count().to_string() });
    metadata.push(MetadataEntry { section: "glTF 2.0".into(), key: "Triangles".into(), value: (build.positions.len() / 9).to_string() });
    Some(build.finalize())
}

fn decode_3mf(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let model_name = (0..arch.len())
        .filter_map(|i| arch.by_index(i).ok().and_then(|f| f.name().ok().map(|n| n.into_owned())))
        .find(|n| n.ends_with(".model"))?;
    let mut xml = String::new();
    arch.by_name(&model_name).ok()?.read_to_string(&mut xml).ok()?;
    let doc = roxmltree::Document::parse(&xml).ok()?;

    let mut positions = Vec::new();
    for mesh in doc.descendants().filter(|n| n.has_tag_name("mesh")) {
        let mut verts = Vec::new();
        for v in mesh.descendants().filter(|n| n.has_tag_name("vertex")) {
            let x: f32 = v.attribute("x").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let y: f32 = v.attribute("y").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let z: f32 = v.attribute("z").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            verts.push([x, y, z]);
        }
        for t in mesh.descendants().filter(|n| n.has_tag_name("triangle")).take(MAX_RENDER_TRIS) {
            let v1: usize = t.attribute("v1").and_then(|s| s.parse().ok()).unwrap_or(0);
            let v2: usize = t.attribute("v2").and_then(|s| s.parse().ok()).unwrap_or(0);
            let v3: usize = t.attribute("v3").and_then(|s| s.parse().ok()).unwrap_or(0);
            if let (Some(a), Some(b), Some(c)) = (verts.get(v1), verts.get(v2), verts.get(v3)) {
                positions.extend_from_slice(a);
                positions.extend_from_slice(b);
                positions.extend_from_slice(c);
            }
        }
    }
    metadata.push(MetadataEntry { section: "3MF".into(), key: "Triangles".into(), value: (positions.len() / 9).to_string() });
    Some(finalize_mesh("3D Manufacturing Format (.3mf)".into(), positions, Vec::new(), Vec::new(), Vec::new(), None))
}

fn decode_cadmpeg_step(bytes: &[u8], format_id: &str, metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if format_id == "ifc" {
        return decode_step_text_fallback(bytes, "IFC BIM Entity Model", metadata);
    }
    let codec = StepCodec::default();
    let mut cursor = Cursor::new(bytes);
    match codec.decode(&mut cursor, &DecodeOptions::default()) {
        Ok(res) => Some(cad_ir_to_mesh3d("ISO 10303-21 STEP CAD", res.ir(), metadata)),
        Err(_) => decode_step_text_fallback(bytes, "ISO 10303-21 STEP CAD", metadata),
    }
}

fn decode_cadmpeg_rhino(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let codec = RhinoCodec;
    let mut cursor = Cursor::new(bytes);
    let res = codec.decode(&mut cursor, &DecodeOptions::default()).ok()?;
    Some(cad_ir_to_mesh3d("McNeel Rhino 3DM CAD", res.ir(), metadata))
}

fn decode_cadmpeg_freecad(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let codec = FcstdCodec;
    let mut cursor = Cursor::new(bytes);
    let res = codec.decode(&mut cursor, &DecodeOptions::default()).ok()?;
    Some(cad_ir_to_mesh3d("FreeCAD FCStd CAD", res.ir(), metadata))
}

/// Converts CadIr tessellations AND tessellates exact B-rep surfaces/faces into solid 3D triangles + normals!
fn cad_ir_to_mesh3d(label: &str, ir: &CadIr, metadata: &mut Vec<MetadataEntry>) -> ViewerPayload {
    let m = &ir.model;
    metadata.push(MetadataEntry { section: "CAD B-Rep".into(), key: "Bodies".into(), value: m.bodies.len().to_string() });
    metadata.push(MetadataEntry { section: "CAD B-Rep".into(), key: "Faces".into(), value: m.faces.len().to_string() });
    metadata.push(MetadataEntry { section: "CAD B-Rep".into(), key: "Edges".into(), value: m.edges.len().to_string() });
    metadata.push(MetadataEntry { section: "CAD B-Rep".into(), key: "Surfaces / Curves".into(), value: format!("{} / {}", m.surfaces.len(), m.curves.len()) });

    let mut positions = Vec::new();
    let mut normals = Vec::new();
    let mut wireframe_lines = Vec::new();

    for tess in &m.tessellations {
        let verts = tess.vertices();
        let vnorms = tess.vertex_normals();
        for tri in tess.triangles().iter().take(MAX_RENDER_TRIS) {
            for &idx in tri {
                if let Some(p) = verts.get(idx as usize) {
                    let pt = p.get();
                    positions.extend_from_slice(&[pt.x as f32, pt.y as f32, pt.z as f32]);
                    if let Some(n) = vnorms.get(idx as usize) {
                        let nv = n.get();
                        normals.extend_from_slice(&[nv.x as f32, nv.y as f32, nv.z as f32]);
                    }
                }
            }
        }
        for edge in tess.feature_edges().iter().take(80_000) {
            if let (Some(a), Some(b)) = (verts.get(edge[0] as usize), verts.get(edge[1] as usize)) {
                let pa = a.get();
                let pb = b.get();
                wireframe_lines.extend_from_slice(&[pa.x as f32, pa.y as f32, pa.z as f32, pb.x as f32, pb.y as f32, pb.z as f32]);
            }
        }
    }

    let index = ModelIndex::new_model_only(ir);
    let mut pt_map = HashMap::new();
    for p in &m.points {
        pt_map.insert(&p.id, p.position().get());
    }
    let mut vtx_map = HashMap::new();
    for v in &m.vertices {
        if let Some(pt) = pt_map.get(&v.point) {
            vtx_map.insert(&v.id, *pt);
        }
    }

    // Build edge -> polyline cache once for both wireframe & B-rep face trimming
    let mut edge_pts_map = HashMap::with_capacity(m.edges.len());
    for edge in m.edges.iter().take(25_000) {
        let mut pts = Vec::new();
        if let (Some(curve_id), Some(range)) = (edge.carrier.curve(), edge.carrier.param_range()) {
            let [t0, t1] = range.get();
            let steps = 10usize;
            for s in 0..=steps {
                let t = t0 + (t1 - t0) * (s as f64 / steps as f64);
                if let Ok(fp) = model_curve_point_by_id(EvaluationAdmission::Standard, &index, curve_id, t) {
                    let pt = fp.get();
                    pts.push([pt.x, pt.y, pt.z]);
                }
            }
        }
        if pts.len() < 2 {
            if let (Some(a), Some(b)) = (vtx_map.get(&edge.start), vtx_map.get(&edge.end)) {
                pts = vec![[a.x, a.y, a.z], [b.x, b.y, b.z]];
            }
        }
        for pair in pts.windows(2) {
            wireframe_lines.extend_from_slice(&[
                pair[0][0] as f32, pair[0][1] as f32, pair[0][2] as f32,
                pair[1][0] as f32, pair[1][1] as f32, pair[1][2] as f32,
            ]);
        }
        edge_pts_map.insert(&edge.id, pts);
    }

    // Tessellate analytical/NURBS B-rep faces into solid shaded surfaces when source lacks cached mesh
    if positions.is_empty() && !m.faces.is_empty() {
        let loop_map: HashMap<_, _> = m.loops.iter().map(|l| (&l.id, l)).collect();
        let coedge_map: HashMap<_, _> = m.coedges.iter().map(|c| (&c.id, c)).collect();
        let surf_map: HashMap<_, _> = m.surfaces.iter().map(|s| (&s.id, s)).collect();
        let grid_res = if m.faces.len() > 800 { 6usize } else if m.faces.len() > 200 { 10 } else { 14 };

        for face in m.faces.iter().take(8000) {
            if positions.len() >= MAX_RENDER_TRIS * 9 { break; }
            let Some(surf) = surf_map.get(&face.surface) else { continue };
            let Some(outer_loop_id) = face.loops.iter().next() else { continue };
            let Some(outer_loop) = loop_map.get(outer_loop_id) else { continue };

            let mut boundary_3d: Vec<[f64; 3]> = Vec::new();
            for co_id in outer_loop.coedges() {
                let Some(co) = coedge_map.get(co_id) else { continue };
                if let Some(epts) = edge_pts_map.get(&co.edge) {
                    if co.sense == Sense::Reversed {
                        for p in epts.iter().rev().skip(usize::from(!boundary_3d.is_empty())) {
                            boundary_3d.push(*p);
                        }
                    } else {
                        for p in epts.iter().skip(usize::from(!boundary_3d.is_empty())) {
                            boundary_3d.push(*p);
                        }
                    }
                }
            }
            if boundary_3d.len() < 3 { continue; }

            let sign = if face.sense == Sense::Reversed { -1.0f32 } else { 1.0f32 };
            let solved = surf.geometry.solved();

            // Planar face -> direct exact ear-clipped polygon triangulation
            if let Some(SolvedSurfaceGeometry::Plane(plane)) = solved {
                let n = plane.frame().axis().as_raw();
                let u_ax = plane.frame().reference().as_raw();
                let v_ax = n.cross(*u_ax);
                let orig = plane.origin().get();
                let poly_2d: Vec<[f64; 2]> = boundary_3d
                    .iter()
                    .map(|p| {
                        let d = [p[0] - orig.x, p[1] - orig.y, p[2] - orig.z];
                        [d[0] * u_ax.x + d[1] * u_ax.y + d[2] * u_ax.z, d[0] * v_ax.x + d[1] * v_ax.y + d[2] * v_ax.z]
                    })
                    .collect();
                let tris = triangulate_polygon_2d(&poly_2d);
                let nf = [(n.x as f32) * sign, (n.y as f32) * sign, (n.z as f32) * sign];
                for [a, b, c] in tris {
                    for &idx in &[a, b, c] {
                        let p = boundary_3d[idx];
                        positions.extend_from_slice(&[p[0] as f32, p[1] as f32, p[2] as f32]);
                        normals.extend_from_slice(&nf);
                    }
                }
                continue;
            }

            // Curved analytic surfaces (Cylinder, Cone, Sphere, Torus) or NURBS -> trimmed UV grid
            let uv_bounds = compute_surface_uv_bounds(&surf.geometry, solved, &boundary_3d);
            if let Some(((umin, umax), (vmin, vmax), poly_uv, is_periodic_u)) = uv_bounds {
                let mut grid = vec![vec![None::<([f32; 3], [f32; 3])>; grid_res + 1]; grid_res + 1];
                for iu in 0..=grid_res {
                    let u = umin + (umax - umin) * (iu as f64 / grid_res as f64);
                    for iv in 0..=grid_res {
                        let v = vmin + (vmax - vmin) * (iv as f64 / grid_res as f64);
                        if let Ok(part) = model_surface_partials_by_id(EvaluationAdmission::Standard, &index, &face.surface, u, v) {
                            let pt = part.point.get();
                            let n = part.du.get().cross(part.dv.get());
                            let nl = n.norm().max(1e-12);
                            let nf = [((n.x / nl) as f32) * sign, ((n.y / nl) as f32) * sign, ((n.z / nl) as f32) * sign];
                            grid[iu][iv] = Some(([pt.x as f32, pt.y as f32, pt.z as f32], nf));
                        }
                    }
                }
                let use_trim = !is_periodic_u && poly_uv.len() >= 3 && (umax - umin).abs() > 1e-6 && (vmax - vmin).abs() > 1e-6;
                for iu in 0..grid_res {
                    for iv in 0..grid_res {
                        let uc = umin + (umax - umin) * ((iu as f64 + 0.5) / grid_res as f64);
                        let vc = vmin + (vmax - vmin) * ((iv as f64 + 0.5) / grid_res as f64);
                        if use_trim && !point_in_poly_2d(uc, vc, &poly_uv) {
                            continue;
                        }
                        if let (Some(p00), Some(p10), Some(p11), Some(p01)) = (
                            grid[iu][iv],
                            grid[iu + 1][iv],
                            grid[iu + 1][iv + 1],
                            grid[iu][iv + 1],
                        ) {
                            for tri in [[p00, p10, p11], [p00, p11, p01]] {
                                for (p, n) in tri {
                                    positions.extend_from_slice(&p);
                                    normals.extend_from_slice(&n);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let parts: Vec<String> = m
        .product_definitions
        .iter()
        .take(64)
        .filter_map(|p| p.label.as_ref().or(p.source_name.as_ref()).map(|s| s.to_string()))
        .collect();

    let summary = format!(
        "{} bodies · {} faces · {} edges · {} tessellated tris",
        m.bodies.len(),
        m.faces.len(),
        m.edges.len(),
        positions.len() / 9
    );
    finalize_mesh(label.into(), positions, normals, wireframe_lines, parts, Some(summary))
}

fn compute_surface_uv_bounds(
    geom: &cadmpeg_ir::geometry::SurfaceGeometry,
    solved: Option<&SolvedSurfaceGeometry>,
    boundary_3d: &[[f64; 3]],
) -> Option<((f64, f64), (f64, f64), Vec<[f64; 2]>, bool)> {
    if let Some(SolvedSurfaceGeometry::Nurbs(nurbs)) = solved {
        let uk = nurbs.u_knots().as_slice();
        let vk = nurbs.v_knots().as_slice();
        let ud = nurbs.u_degree() as usize;
        let vd = nurbs.v_degree() as usize;
        if uk.len() > ud * 2 && vk.len() > vd * 2 {
            let u0 = uk[ud];
            let u1 = uk[uk.len() - 1 - ud];
            let v0 = vk[vd];
            let v1 = vk[vk.len() - 1 - vd];
            return Some(((u0, u1), (v0, v1), Vec::new(), true));
        }
    }
    let mut uvs = Vec::with_capacity(boundary_3d.len());
    for p in boundary_3d {
        let pt = cadmpeg_ir::math::Point3::new(p[0], p[1], p[2]);
        if let Some(uv) = analytic_surface_parameters(geom, pt) {
            let raw = uv.get();
            uvs.push([raw.u, raw.v]);
        }
    }
    if uvs.is_empty() {
        return None;
    }
    // Unwrap periodic angle u across +-pi seams
    let mut unwrapped = Vec::with_capacity(uvs.len());
    unwrapped.push(uvs[0]);
    for i in 1..uvs.len() {
        let mut u = uvs[i][0];
        let prev_u = unwrapped[i - 1][0];
        while u - prev_u > std::f64::consts::PI { u -= std::f64::consts::TAU; }
        while u - prev_u < -std::f64::consts::PI { u += std::f64::consts::TAU; }
        unwrapped.push([u, uvs[i][1]]);
    }
    let (mut umin, mut umax, mut vmin, mut vmax) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for &[u, v] in &unwrapped {
        umin = umin.min(u); umax = umax.max(u);
        vmin = vmin.min(v); vmax = vmax.max(v);
    }
    let periodic = (umax - umin).abs() > std::f64::consts::PI * 1.9;
    Some(((umin, umax), (vmin, vmax), unwrapped, periodic))
}

fn point_in_poly_2d(px: f64, py: f64, poly: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let n = poly.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (poly[i][0], poly[i][1]);
        let (xj, yj) = (poly[j][0], poly[j][1]);
        let intersect = ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / ((yj - yi).abs().max(1e-15) * (yj - yi).signum()) + xi);
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Simple ear-clipping triangulator for 2D simple polygons (with fan fallback for degenerate edges).
fn triangulate_polygon_2d(poly: &[[f64; 2]]) -> Vec<[usize; 3]> {
    let mut clean_idx: Vec<usize> = Vec::with_capacity(poly.len());
    for (i, p) in poly.iter().enumerate() {
        if let Some(&last_i) = clean_idx.last() {
            let lp = poly[last_i];
            if (p[0] - lp[0]).hypot(p[1] - lp[1]) < 1e-7 { continue; }
        }
        clean_idx.push(i);
    }
    if clean_idx.len() > 2 {
        let f = poly[clean_idx[0]];
        let l = poly[*clean_idx.last().unwrap()];
        if (f[0] - l[0]).hypot(f[1] - l[1]) < 1e-7 {
            clean_idx.pop();
        }
    }
    if clean_idx.len() < 3 {
        return Vec::new();
    }
    let signed_area: f64 = (0..clean_idx.len())
        .map(|i| {
            let a = poly[clean_idx[i]];
            let b = poly[clean_idx[(i + 1) % clean_idx.len()]];
            a[0] * b[1] - b[0] * a[1]
        })
        .sum();
    if signed_area < 0.0 {
        clean_idx.reverse();
    }

    let mut tris = Vec::with_capacity(clean_idx.len() - 2);
    let mut rem = clean_idx.clone();
    let mut guard = 0usize;
    while rem.len() > 3 && guard < rem.len() * 4 {
        let mut clipped = false;
        let n = rem.len();
        for i in 0..n {
            let i0 = rem[(i + n - 1) % n];
            let i1 = rem[i];
            let i2 = rem[(i + 1) % n];
            let (a, b, c) = (poly[i0], poly[i1], poly[i2]);
            let cross = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
            if cross <= 1e-10 { continue; }
            let contains = rem.iter().any(|&k| {
                k != i0 && k != i1 && k != i2 && point_in_tri_2d(poly[k], a, b, c)
            });
            if !contains {
                tris.push([i0, i1, i2]);
                rem.remove(i);
                clipped = true;
                break;
            }
        }
        if !clipped { break; }
        guard += 1;
    }
    if rem.len() == 3 {
        tris.push([rem[0], rem[1], rem[2]]);
    } else if tris.is_empty() && clean_idx.len() >= 3 {
        for k in 1..clean_idx.len() - 1 {
            tris.push([clean_idx[0], clean_idx[k], clean_idx[k + 1]]);
        }
    }
    tris
}

#[inline]
fn point_in_tri_2d(p: [f64; 2], a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> bool {
    let c1 = (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
    let c2 = (c[0] - b[0]) * (p[1] - b[1]) - (c[1] - b[1]) * (p[0] - b[0]);
    let c3 = (a[0] - c[0]) * (p[1] - c[1]) - (a[1] - c[1]) * (p[0] - c[0]);
    c1 >= -1e-9 && c2 >= -1e-9 && c3 >= -1e-9
}

fn decode_step_text_fallback(
    bytes: &[u8],
    label: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let text = String::from_utf8_lossy(bytes);
    let mut points = Vec::new();
    for line in text.lines() {
        if let Some(idx) = line.find("CARTESIAN_POINT") {
            let rest = &line[idx..];
            if let Some(open) = rest.rfind('(') {
                if let Some(close) = rest[open + 1..].find(')') {
                    let coords: Vec<f32> = rest[open + 1..open + 1 + close]
                        .split(',')
                        .filter_map(|s| s.trim().parse().ok())
                        .collect();
                    if coords.len() == 3 {
                        points.push([coords[0], coords[1], coords[2]]);
                    }
                }
            }
        }
    }
    let mut wire = Vec::new();
    for pair in points.windows(2).take(30_000) {
        wire.extend_from_slice(&pair[0]);
        wire.extend_from_slice(&pair[1]);
    }
    metadata.push(MetadataEntry { section: "CAD".into(), key: "Cartesian Points".into(), value: points.len().to_string() });
    Some(finalize_mesh(label.into(), Vec::new(), Vec::new(), wire, Vec::new(), Some(format!("{} Cartesian coordinates extracted", points.len()))))
}

fn decode_iges(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let text = String::from_utf8_lossy(bytes);
    let mut p_buf = String::new();
    for line in text.lines() {
        if line.len() >= 73 && line.as_bytes()[72] == b'P' {
            p_buf.push_str(line[..64].trim());
        }
    }
    let mut wire = Vec::new();
    for rec in p_buf.split(';') {
        let fields: Vec<&str> = rec.split(',').map(str::trim).collect();
        if fields.first() == Some(&"110") && fields.len() >= 7 {
            if let (Ok(x1), Ok(y1), Ok(z1), Ok(x2), Ok(y2), Ok(z2)) = (
                fields[1].parse::<f32>(), fields[2].parse::<f32>(), fields[3].parse::<f32>(),
                fields[4].parse::<f32>(), fields[5].parse::<f32>(), fields[6].parse::<f32>(),
            ) {
                wire.extend_from_slice(&[x1, y1, z1, x2, y2, z2]);
            }
        }
    }
    metadata.push(MetadataEntry { section: "IGES CAD".into(), key: "Line Segments".into(), value: (wire.len() / 6).to_string() });
    Some(finalize_mesh("IGES CAD Model".into(), Vec::new(), Vec::new(), wire, Vec::new(), None))
}

fn decode_blend(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (label, verts, tris, wire, parts, tree_nodes, summary) =
        blend::decode_blend_info(bytes, metadata)?;
    Some(blend::blend_to_viewer(
        label,
        verts,
        tris,
        wire,
        parts,
        tree_nodes,
        summary,
        finalize_mesh,
    ))
}

fn decode_gcode(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    let mut wire = Vec::new();
    let (mut cx, mut cy, mut cz) = (0.0f32, 0.0f32, 0.0f32);
    let mut moves = 0usize;
    for line in text.lines().take(250_000) {
        let code = line.split(';').next().unwrap_or("").trim();
        if code.is_empty() { continue; }
        if code.starts_with("G0") || code.starts_with("G1") {
            let (mut nx, mut ny, mut nz) = (cx, cy, cz);
            for tok in code.split_whitespace() {
                if let Some(v) = tok.strip_prefix('X').and_then(|s| s.parse::<f32>().ok()) { nx = v; }
                else if let Some(v) = tok.strip_prefix('Y').and_then(|s| s.parse::<f32>().ok()) { ny = v; }
                else if let Some(v) = tok.strip_prefix('Z').and_then(|s| s.parse::<f32>().ok()) { nz = v; }
            }
            if (nx - cx).abs() > 1e-5 || (ny - cy).abs() > 1e-5 || (nz - cz).abs() > 1e-5 {
                if wire.len() < 400_000 {
                    wire.extend_from_slice(&[cx, cz, cy, nx, nz, ny]);
                }
                moves += 1;
            }
            cx = nx; cy = ny; cz = nz;
        }
    }
    metadata.push(MetadataEntry { section: "G-Code".into(), key: "Toolpath Moves".into(), value: moves.to_string() });
    Some(finalize_mesh(
        format!("G-Code 3D Toolpath ({moves} moves)"),
        Vec::new(),
        Vec::new(),
        wire,
        Vec::new(),
        Some(format!("{moves} G0/G1 CNC/3D-print motion segments")),
    ))
}

fn decode_dxf(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let mut svg_items = String::new();
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut count = 0usize;
    let mut i = 0usize;
    while i + 1 < lines.len() {
        if lines[i] == "0" && lines[i + 1] == "LINE" {
            let (mut x1, mut y1, mut x2, mut y2) = (0.0f64, 0.0, 0.0, 0.0);
            i += 2;
            while i + 1 < lines.len() && lines[i] != "0" {
                match lines[i] {
                    "10" => x1 = lines[i + 1].parse().unwrap_or(0.0),
                    "20" => y1 = lines[i + 1].parse().unwrap_or(0.0),
                    "11" => x2 = lines[i + 1].parse().unwrap_or(0.0),
                    "21" => y2 = lines[i + 1].parse().unwrap_or(0.0),
                    _ => {}
                }
                i += 2;
            }
            minx = minx.min(x1).min(x2); maxx = maxx.max(x1).max(x2);
            miny = miny.min(-y1).min(-y2); maxy = maxy.max(-y1).max(-y2);
            let _ = write!(svg_items, "<line x1=\"{x1:.2}\" y1=\"{:.2}\" x2=\"{x2:.2}\" y2=\"{:.2}\"/>", -y1, -y2);
            count += 1;
            continue;
        }
        if lines[i] == "0" && lines[i + 1] == "CIRCLE" {
            let (mut cx, mut cy, mut r) = (0.0f64, 0.0, 1.0);
            i += 2;
            while i + 1 < lines.len() && lines[i] != "0" {
                match lines[i] {
                    "10" => cx = lines[i + 1].parse().unwrap_or(0.0),
                    "20" => cy = lines[i + 1].parse().unwrap_or(0.0),
                    "40" => r = lines[i + 1].parse().unwrap_or(1.0),
                    _ => {}
                }
                i += 2;
            }
            minx = minx.min(cx - r); maxx = maxx.max(cx + r);
            miny = miny.min(-cy - r); maxy = maxy.max(-cy + r);
            let _ = write!(svg_items, "<circle cx=\"{cx:.2}\" cy=\"{:.2}\" r=\"{r:.2}\"/>", -cy);
            count += 1;
            continue;
        }
        i += 1;
    }

    if !minx.is_finite() {
        minx = 0.0; miny = 0.0; maxx = 100.0; maxy = 100.0;
    }
    let w = (maxx - minx).max(1.0);
    let h = (maxy - miny).max(1.0);
    let pad = w.max(h) * 0.06;
    let sw = (w.max(h) * 0.004).max(0.1);
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{:.2} {:.2} {:.2} {:.2}\" fill=\"none\" stroke=\"#EAFFFF\" stroke-width=\"{sw:.3}\">{svg_items}</svg>",
        minx - pad, miny - pad, w + pad * 2.0, h + pad * 2.0
    );
    metadata.push(MetadataEntry { section: "DXF".into(), key: "Entities Rendered".into(), value: count.to_string() });
    Some(ViewerPayload::Vector2d {
        format_label: "AutoCAD DXF Drawing".into(),
        width: w,
        height: h,
        layers: Vec::new(),
        svg_markup: svg,
        entity_count: count,
    })
}

fn decode_gerber(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    let mut pts = Vec::new();
    let (mut cx, mut cy) = (0.0f64, 0.0f64);
    let mut svg_items = String::new();
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);

    for cmd in text.split('*') {
        let t = cmd.trim();
        if t.starts_with('X') || t.starts_with('Y') || t.contains("D01") || t.contains("D02") || t.contains("D03") {
            let mut nx = cx;
            let mut ny = cy;
            if let Some(xp) = t.find('X') {
                let digits: String = t[xp + 1..].chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
                if let Ok(v) = digits.parse::<f64>() { nx = v / 10000.0; }
            }
            if let Some(yp) = t.find('Y') {
                let digits: String = t[yp + 1..].chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
                if let Ok(v) = digits.parse::<f64>() { ny = v / 10000.0; }
            }
            if t.contains("D01") {
                minx = minx.min(cx).min(nx); maxx = maxx.max(cx).max(nx);
                miny = miny.min(-cy).min(-ny); maxy = maxy.max(-cy).max(-ny);
                let _ = write!(svg_items, "<line x1=\"{cx:.3}\" y1=\"{:.3}\" x2=\"{nx:.3}\" y2=\"{:.3}\"/>", -cy, -ny);
                pts.push((nx, ny));
            } else if t.contains("D03") {
                minx = minx.min(nx); maxx = maxx.max(nx);
                miny = miny.min(-ny); maxy = maxy.max(-ny);
                let _ = write!(svg_items, "<circle cx=\"{nx:.3}\" cy=\"{:.3}\" r=\"0.25\" fill=\"#EAFFFF\"/>", -ny);
                pts.push((nx, ny));
            }
            cx = nx; cy = ny;
        }
    }
    if !minx.is_finite() { minx = 0.0; miny = 0.0; maxx = 100.0; maxy = 100.0; }
    let w = (maxx - minx).max(1.0);
    let h = (maxy - miny).max(1.0);
    let pad = w.max(h) * 0.08;
    let sw = (w.max(h) * 0.005).max(0.05);
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{:.3} {:.3} {:.3} {:.3}\" fill=\"none\" stroke=\"#EAFFFF\" stroke-width=\"{sw:.3}\" stroke-linecap=\"round\">{svg_items}</svg>",
        minx - pad, miny - pad, w + pad * 2.0, h + pad * 2.0
    );
    metadata.push(MetadataEntry { section: "Gerber RS-274X".into(), key: "Traces / Flashes".into(), value: pts.len().to_string() });
    Some(ViewerPayload::Vector2d {
        format_label: "RS-274X Gerber PCB Layer".into(),
        width: w,
        height: h,
        layers: vec!["Copper / Gerber".into()],
        svg_markup: svg,
        entity_count: pts.len(),
    })
}

fn decode_kicad(bytes: &[u8], format_id: &str, metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    let nets = text.matches("(net ").count();
    let footprints = text.matches("(footprint ").count();
    let segments = text.matches("(segment ").count();
    metadata.push(MetadataEntry { section: "KiCad".into(), key: "Nets".into(), value: nets.to_string() });
    metadata.push(MetadataEntry { section: "KiCad".into(), key: "Footprints".into(), value: footprints.to_string() });
    metadata.push(MetadataEntry { section: "KiCad".into(), key: "Segments".into(), value: segments.to_string() });

    let nodes = vec![
        TreeNode { key: "Format".into(), value_preview: format_id.into(), kind: "object".into(), depth: 0 },
        TreeNode { key: "Nets".into(), value_preview: nets.to_string(), kind: "number".into(), depth: 1 },
        TreeNode { key: "Footprints".into(), value_preview: footprints.to_string(), kind: "number".into(), depth: 1 },
        TreeNode { key: "Track Segments".into(), value_preview: segments.to_string(), kind: "number".into(), depth: 1 },
    ];

    Some(ViewerPayload::Structured {
        format_label: format!("KiCad EDA ({format_id})"),
        formatted_text: text.chars().take(65_536).collect(),
        tree_nodes: nodes,
    })
}

fn decode_fbx_summary(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.starts_with(b"Kaydara FBX Binary") && bytes.len() >= 27 {
        let ver = u32::from_le_bytes([bytes[23], bytes[24], bytes[25], bytes[26]]);
        metadata.push(MetadataEntry { section: "FBX".into(), key: "Binary Version".into(), value: ver.to_string() });
        let strings = crate::decoders::hex_bin::extract_strings(bytes, 160);
        let nodes: Vec<TreeNode> = strings
            .iter()
            .take(80)
            .map(|s| TreeNode { key: format!("0x{:X}", s.offset), value_preview: s.value.clone(), kind: "string".into(), depth: 1 })
            .collect();
        return Some(ViewerPayload::Structured {
            format_label: format!("Autodesk FBX Binary v{ver}"),
            formatted_text: strings.iter().map(|s| s.value.as_str()).collect::<Vec<_>>().join("\n"),
            tree_nodes: nodes,
        });
    }
    None
}
