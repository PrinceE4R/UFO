use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read as _};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use flate2::read::GzDecoder;
use image::{codecs::png::PngEncoder, ColorType, ImageEncoder as _};
use ruzstd::decoding::StreamingDecoder as ZstdDecoder;

use crate::decoders::anim::{euler_deg_xyz_to_quat, infer_species_hint};
use crate::decoders::mesh3d::MeshBuild;
use crate::model::{
    AnimationClip, BoneChannel, MeshBatch, MetadataEntry, SceneLayer3d, SceneMaterialInfo,
    SkeletonAnimation, SkeletonBone, TreeNode, ViewerPayload,
};

#[derive(Debug, Clone)]
struct BlendBlock<'a> {
    code: [u8; 4],
    old: u64,
    data: &'a [u8],
}

#[derive(Debug, Clone)]
struct SdnaField {
    type_idx: usize,
    name: String,
    offset: usize,
    size: usize,
}

#[derive(Debug, Clone)]
struct SdnaStruct {
    name: String,
    size: usize,
    fields: Vec<SdnaField>,
}

struct Sdna {
    structs: Vec<SdnaStruct>,
    by_name: HashMap<String, usize>,
}

#[derive(Clone)]
struct BlendMaterialData {
    name: String,
    base_color: [f32; 4],
    metallic: f32,
    roughness: f32,
    texture_index: Option<usize>,
    texture_name: Option<String>,
}

struct ParsedBlendMesh {
    name: String,
    verts: Vec<[f32; 3]>,
    /// Each face is `(corner_vert_indices, corner_uvs, mat_slot_index)`
    faces: Vec<(Vec<u32>, Vec<[f32; 2]>, usize)>,
    wire_edges: Vec<[u32; 2]>,
    mat_ptrs: Vec<u64>,
}

/// Decodes Blender `.blend` files (v2.x–v5.3+, uncompressed/Gzip/Zstd) into multi-layer 3D meshes,
/// materials, packed image textures, and rigged armature animations.
pub fn decode_blend_payload(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let decompressed: Vec<u8>;
    let raw: &[u8] = if bytes.starts_with(b"BLENDER") {
        bytes
    } else if bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        let zstd = ZstdDecoder::new(Cursor::new(bytes)).ok()?;
        let mut buf = Vec::new();
        zstd.take(128 * 1024 * 1024).read_to_end(&mut buf).ok()?;
        decompressed = buf;
        &decompressed
    } else if bytes.starts_with(&[0x1F, 0x8B, 0x08]) {
        let gz = GzDecoder::new(bytes);
        let mut buf = Vec::new();
        gz.take(128 * 1024 * 1024).read_to_end(&mut buf).ok()?;
        decompressed = buf;
        &decompressed
    } else {
        return None;
    };

    if raw.len() < 12 || !raw.starts_with(b"BLENDER") {
        return None;
    }

    let (hdr_len, ptr_size, is_large, is_le, version_str) =
        if raw.len() >= 17 && raw[7].is_ascii_digit() && raw[8].is_ascii_digit() {
            let hlen = std::str::from_utf8(&raw[7..9])
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(17)
                .clamp(12, 32);
            let ptr_sz = if raw.get(9) == Some(&b'_') { 4 } else { 8 };
            let large = raw.get(10..12) == Some(b"01");
            let le = raw.get(12) != Some(&b'V');
            let ver = String::from_utf8_lossy(&raw[13..hlen.min(raw.len())]).to_string();
            (hlen, ptr_sz, large, le, format_blend_version(&ver))
        } else {
            let ptr_sz = if raw[7] == b'_' { 4 } else { 8 };
            let le = raw[8] != b'V';
            let ver = String::from_utf8_lossy(&raw[9..12]).to_string();
            (12usize, ptr_sz, false, le, format_blend_version(&ver))
        };

    metadata.push(MetadataEntry { section: "Blender".into(), key: "Version".into(), value: version_str.clone() });
    metadata.push(MetadataEntry { section: "Blender".into(), key: "Pointer Size".into(), value: format!("{}-bit ({})", ptr_size * 8, if is_large { "LargeBHead8" } else { "Standard BHead" }) });

    let mut blocks = Vec::new();
    let mut data_map: HashMap<u64, &[u8]> = HashMap::new();
    let mut dna_bytes: Option<&[u8]> = None;
    let mut block_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut pos = hdr_len;

    while pos < raw.len() {
        let (code, old, len, bhead_sz) = if is_large {
            if pos + 32 > raw.len() { break; }
            let code = [raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]];
            let old = r_u64(&raw[pos + 8..], is_le);
            let len = r_i64(&raw[pos + 16..], is_le).max(0) as usize;
            (code, old, len, 32usize)
        } else if ptr_size == 8 {
            if pos + 24 > raw.len() { break; }
            let code = [raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]];
            let len = r_u32(&raw[pos + 4..], is_le) as usize;
            let old = r_u64(&raw[pos + 8..], is_le);
            (code, old, len, 24usize)
        } else {
            if pos + 20 > raw.len() { break; }
            let code = [raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]];
            let len = r_u32(&raw[pos + 4..], is_le) as usize;
            let old = r_u32(&raw[pos + 8..], is_le) as u64;
            (code, old, len, 20usize)
        };

        pos += bhead_sz;
        let code_str = String::from_utf8_lossy(&code).trim_end_matches('\0').to_string();
        if code_str == "ENDB" || pos.saturating_add(len) > raw.len() {
            break;
        }
        let payload = &raw[pos..pos + len];
        pos += len;

        *block_counts.entry(code_str).or_default() += 1;
        if &code == b"DNA1" {
            dna_bytes = Some(payload);
        } else if old != 0 {
            if &code == b"DATA" {
                data_map.insert(old, payload);
            } else {
                data_map.entry(old).or_insert(payload);
            }
        }
        blocks.push(BlendBlock { code, old, data: payload });
    }

    metadata.push(MetadataEntry { section: "Blender".into(), key: "Blocks".into(), value: blocks.len().to_string() });

    let sdna = dna_bytes.and_then(|d| parse_sdna(d, ptr_size, is_le));
    let mut parts = Vec::new();
    let mut tree_nodes = Vec::new();

    let mut build = MeshBuild {
        format_label: format!("Blender {version_str} Scene"),
        ..Default::default()
    };

    if let Some(ref dna) = sdna {
        metadata.push(MetadataEntry { section: "Blender SDNA".into(), key: "Struct Types".into(), value: dna.structs.len().to_string() });

        let id_name_off = dna
            .find_field("ID", "name")
            .map(|f| f.offset)
            .unwrap_or(if ptr_size == 8 { 24 } else { 12 });

        for blk in &blocks {
            let prefix = String::from_utf8_lossy(&blk.code);
            let p2 = prefix.trim_end_matches('\0');
            if matches!(p2, "OB" | "ME" | "MA" | "CA" | "LA" | "SC" | "AR" | "AC" | "WO" | "IM") {
                if let Some(name) = read_id_name(blk.data, id_name_off) {
                    if parts.len() < 128 {
                        parts.push(name.clone());
                    }
                    if tree_nodes.len() < 240 {
                        tree_nodes.push(TreeNode {
                            key: p2.to_string(),
                            value_preview: name,
                            kind: "object".into(),
                            depth: 1,
                        });
                    }
                }
            }
        }

        // 1. Decode Packed Textures (`IM` blocks)
        let mut im_map: HashMap<u64, (usize, String)> = HashMap::new();
        for blk in &blocks {
            if &blk.code == b"IM\0\0" {
                if let Some((data_url, tex_name)) =
                    extract_packed_image(blk, dna, &data_map, ptr_size, is_le, id_name_off)
                {
                    let tidx = build.textures.len();
                    build.textures.push(data_url);
                    im_map.insert(blk.old, (tidx, tex_name));
                }
            }
        }

        // 2. Decode Materials (`MA` blocks, linking packed `IM` textures if node tree references them)
        let mut ma_map: HashMap<u64, BlendMaterialData> = HashMap::new();
        for blk in &blocks {
            if &blk.code == b"MA\0\0" {
                let mat = extract_material(blk, dna, &data_map, &im_map, ptr_size, is_le, id_name_off);
                build.materials.push(SceneMaterialInfo {
                    name: mat.name.clone(),
                    base_color: mat.base_color,
                    metallic: mat.metallic,
                    roughness: mat.roughness,
                    texture_name: mat.texture_name.clone(),
                    texture_index: mat.texture_index,
                });
                ma_map.insert(blk.old, mat);
            }
        }

        // 3. Parse all Mesh (`ME`) geometry by pointer
        let mut mesh_map: HashMap<u64, ParsedBlendMesh> = HashMap::new();
        for blk in &blocks {
            if &blk.code == b"ME\0\0" {
                if let Some(pm) = parse_blend_mesh(blk, dna, &data_map, ptr_size, is_le, id_name_off) {
                    mesh_map.insert(blk.old, pm);
                }
            }
        }

        // 4. Emit per-Object (`OB`) layered batches with exact world transforms & visibility
        let mut used_meshes: HashMap<u64, bool> = HashMap::new();
        let ob_blocks: Vec<&BlendBlock<'_>> = blocks.iter().filter(|b| &b.code == b"OB\0\0").collect();
        for (obj_idx, ob) in ob_blocks.iter().enumerate() {
            let ob_type = dna
                .find_field("Object", "type")
                .and_then(|f| read_i16_or_i32(ob.data, f.offset, f.size, is_le))
                .unwrap_or(0);
            if ob_type != 1 {
                continue;
            }
            let data_ptr = dna
                .find_field("Object", "data")
                .map(|f| read_ptr(ob.data, f.offset, ptr_size, is_le))
                .unwrap_or(0);
            let Some(mesh) = mesh_map.get(&data_ptr) else { continue };
            used_meshes.insert(data_ptr, true);

            let obj_name = read_id_name(ob.data, id_name_off)
                .map(|n| clean_blend_id(&n))
                .unwrap_or_else(|| format!("Object_{obj_idx}"));
            let parent_ptr = dna
                .find_field("Object", "parent")
                .map(|f| read_ptr(ob.data, f.offset, ptr_size, is_le))
                .unwrap_or(0);
            let visible = extract_object_visibility(ob.data, dna, is_le);
            let mat4 = extract_object_matrix(ob.data, dna, is_le);

            emit_mesh_instance(
                &format!("blend_ob_{}", ob.old),
                obj_name,
                "Blender Object",
                visible,
                (parent_ptr != 0).then(|| format!("blend_ob_{parent_ptr}")),
                mesh,
                &mat4,
                &ma_map,
                &mut build,
            );
        }

        // Any orphan `ME` datablocks not linked to an `OB` still appear as layers
        let identity = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        for blk in &blocks {
            if &blk.code == b"ME\0\0" && !used_meshes.contains_key(&blk.old) {
                if let Some(mesh) = mesh_map.get(&blk.old) {
                    emit_mesh_instance(
                        &format!("blend_me_{}", blk.old),
                        mesh.name.clone(),
                        "Blender Mesh",
                        true,
                        None,
                        mesh,
                        &identity,
                        &ma_map,
                        &mut build,
                    );
                }
            }
        }

        // 5. Extract Armatures (`AR`) and Actions (`AC`) for skeletal animation playback
        build.skeleton = extract_blend_skeleton(&blocks, dna, &data_map, ptr_size, is_le, id_name_off);
    }

    let tri_count = build.positions.len() / 9;
    let vert_count = build.positions.len() / 3;
    let tex_count = build.textures.len();
    let mat_count = build.materials.len();
    let anim_count = build.skeleton.as_ref().map(|s| s.clips.len()).unwrap_or(0);

    let mut summary_lines = vec![
        format!("Blender {version_str} ({}-bit, {})", ptr_size * 8, if is_large { "LargeBHead8" } else { "BHead" }),
        format!(
            "Layers: {} · Tris: {tri_count} · Materials: {mat_count} · Textures: {tex_count} · Clips: {anim_count}",
            build.layers.len()
        ),
        String::new(),
        "Block Counts:".into(),
    ];
    for (k, v) in &block_counts {
        summary_lines.push(format!("  {k:<4} : {v}"));
    }
    let summary = summary_lines.join("\n");

    metadata.push(MetadataEntry { section: "Blender Scene".into(), key: "Object Layers".into(), value: build.layers.len().to_string() });
    metadata.push(MetadataEntry { section: "Blender Scene".into(), key: "Materials / Textures".into(), value: format!("{mat_count} / {tex_count}") });
    metadata.push(MetadataEntry { section: "Blender Scene".into(), key: "Triangles / Vertices".into(), value: format!("{tri_count} / {vert_count}") });
    if let Some(ref skel) = build.skeleton {
        metadata.push(MetadataEntry { section: "Blender Animation".into(), key: "Bones / Action Clips".into(), value: format!("{} / {}", skel.bones.len(), skel.clips.len()) });
    }

    build.parts = parts;
    build.cad_summary = Some(format!(
        "Blender {version_str} · {} layers · {mat_count} materials · {tex_count} textures · {anim_count} clips",
        build.layers.len()
    ));

    if !build.positions.is_empty() || !build.wireframe_lines.is_empty() || build.skeleton.is_some() {
        return Some(build.finalize());
    }

    Some(ViewerPayload::Structured {
        format_label: format!("Blender {version_str} Scene"),
        formatted_text: summary,
        tree_nodes,
    })
}

fn clean_blend_id(raw: &str) -> String {
    if raw.len() > 2 && raw.as_bytes()[0].is_ascii_uppercase() && raw.as_bytes()[1].is_ascii_uppercase() {
        raw[2..].to_string()
    } else {
        raw.to_string()
    }
}

fn extract_packed_image(
    blk: &BlendBlock<'_>,
    dna: &Sdna,
    data_map: &HashMap<u64, &[u8]>,
    ptr_size: usize,
    is_le: bool,
    id_name_off: usize,
) -> Option<(String, String)> {
    let im_name = read_id_name(blk.data, id_name_off)
        .map(|n| clean_blend_id(&n))
        .unwrap_or_else(|| format!("Texture_{}", blk.old));

    // Legacy `Image.packedfile` or newer `Image.packedfiles.first -> ImagePackedFile.packedfile`
    let mut pf_ptr = dna
        .find_field("Image", "packedfile")
        .map(|f| read_ptr(blk.data, f.offset, ptr_size, is_le))
        .unwrap_or(0);
    if pf_ptr == 0 {
        if let Some(pfs_f) = dna.find_field("Image", "packedfiles") {
            let first_ptr = read_ptr(blk.data, pfs_f.offset, ptr_size, is_le);
            if let Some(ipf_buf) = data_map.get(&first_ptr) {
                pf_ptr = dna
                    .find_field("ImagePackedFile", "packedfile")
                    .map(|f| read_ptr(ipf_buf, f.offset, ptr_size, is_le))
                    .unwrap_or(0);
            }
        }
    }
    if pf_ptr == 0 {
        return None;
    }
    let pf_buf = data_map.get(&pf_ptr)?;
    let size = dna
        .find_field("PackedFile", "size")
        .and_then(|f| read_int(pf_buf, f.offset, f.size, is_le))
        .unwrap_or(0)
        .max(0) as usize;
    let dptr = dna
        .find_field("PackedFile", "data")
        .map(|f| read_ptr(pf_buf, f.offset, ptr_size, is_le))
        .unwrap_or(0);
    let raw = data_map.get(&dptr)?;
    let slice = if size > 0 && size <= raw.len() { &raw[..size] } else { *raw };
    if slice.is_empty() {
        return None;
    }

    if let Ok(dyn_img) = image::load_from_memory(slice) {
        let rgba = dyn_img.to_rgba8();
        let (w, h) = rgba.dimensions();
        let mut out = Vec::new();
        if PngEncoder::new(&mut out).write_image(&rgba, w, h, ColorType::Rgba8.into()).is_ok() {
            return Some((format!("data:image/png;base64,{}", B64.encode(&out)), im_name));
        }
    }
    if slice.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((format!("data:image/png;base64,{}", B64.encode(slice)), im_name));
    }
    if slice.starts_with(b"\xFF\xD8\xFF") {
        return Some((format!("data:image/jpeg;base64,{}", B64.encode(slice)), im_name));
    }
    None
}

fn extract_material(
    blk: &BlendBlock<'_>,
    dna: &Sdna,
    data_map: &HashMap<u64, &[u8]>,
    im_map: &HashMap<u64, (usize, String)>,
    ptr_size: usize,
    is_le: bool,
    id_name_off: usize,
) -> BlendMaterialData {
    let name = read_id_name(blk.data, id_name_off)
        .map(|n| clean_blend_id(&n))
        .unwrap_or_else(|| format!("Material_{}", blk.old));

    let read_f = |fname: &str, def: f32| -> f32 {
        dna.find_field("Material", fname)
            .and_then(|f| (f.offset + 4 <= blk.data.len()).then(|| r_f32(&blk.data[f.offset..], is_le)))
            .filter(|v| v.is_finite())
            .unwrap_or(def)
    };
    let r = read_f("r", 0.78).clamp(0.0, 1.0);
    let g = read_f("g", 0.85).clamp(0.0, 1.0);
    let b = read_f("b", 0.88).clamp(0.0, 1.0);
    let a = read_f("a", 1.0).clamp(0.05, 1.0);
    let metallic = read_f("metallic", 0.0).clamp(0.0, 1.0);
    let roughness = read_f("roughness", 0.5).clamp(0.0, 1.0);

    let mut tex_idx = None;
    let mut tex_name = None;

    // Inspect Material nodetree `bNodeTree.nodes` for linked `IM` image texture
    if let Some(nt_f) = dna.find_field("Material", "nodetree") {
        let nt_ptr = read_ptr(blk.data, nt_f.offset, ptr_size, is_le);
        if let Some(nt_buf) = data_map.get(&nt_ptr) {
            if let Some(nodes_f) = dna.find_field("bNodeTree", "nodes") {
                let mut cur = read_ptr(nt_buf, nodes_f.offset, ptr_size, is_le);
                let next_f = dna.find_field("bNode", "next");
                let id_f = dna.find_field("bNode", "id");
                for _ in 0..128 {
                    if cur == 0 { break; }
                    let Some(nbuf) = data_map.get(&cur) else { break };
                    if let Some(idf) = id_f {
                        let id_ptr = read_ptr(nbuf, idf.offset, ptr_size, is_le);
                        if let Some((ti, tn)) = im_map.get(&id_ptr) {
                            tex_idx = Some(*ti);
                            tex_name = Some(tn.clone());
                            break;
                        }
                    }
                    cur = next_f.map(|nf| read_ptr(nbuf, nf.offset, ptr_size, is_le)).unwrap_or(0);
                }
            }
        }
    }

    BlendMaterialData {
        name,
        base_color: [r, g, b, a],
        metallic,
        roughness,
        texture_index: tex_idx,
        texture_name: tex_name,
    }
}

fn extract_object_visibility(data: &[u8], dna: &Sdna, is_le: bool) -> bool {
    if let Some(vf) = dna.find_field("Object", "visibility_flag").or_else(|| dna.find_field("Object", "restrictflag")) {
        if let Some(flag) = read_int(data, vf.offset, vf.size, is_le) {
            // In Blender, bit 0 (`OB_HIDE_VIEWPORT` / `OB_RESTRICT_VIEWPORT`) hides viewport display
            return (flag & 1) == 0;
        }
    }
    true
}

fn extract_object_matrix(data: &[u8], dna: &Sdna, is_le: bool) -> [[f32; 4]; 4] {
    if let Some(mf) = dna.find_field("Object", "obmat").or_else(|| dna.find_field("Object", "constinv")) {
        if mf.offset + 64 <= data.len() {
            let mut m = [[0.0f32; 4]; 4];
            let mut norm_sum = 0.0f32;
            for c in 0..4 {
                for r in 0..4 {
                    let v = r_f32(&data[mf.offset + (c * 4 + r) * 4..], is_le);
                    m[c][r] = v;
                    norm_sum += v.abs();
                }
            }
            if norm_sum > 1e-4 && m.iter().flatten().all(|v| v.is_finite()) {
                return m;
            }
        }
    }
    // Fallback from `loc` + `scale`
    let mut m = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    if let Some(lf) = dna.find_field("Object", "loc") {
        if lf.offset + 12 <= data.len() {
            m[3][0] = r_f32(&data[lf.offset..], is_le);
            m[3][1] = r_f32(&data[lf.offset + 4..], is_le);
            m[3][2] = r_f32(&data[lf.offset + 8..], is_le);
        }
    }
    if let Some(sf) = dna.find_field("Object", "scale").or_else(|| dna.find_field("Object", "size")) {
        if sf.offset + 12 <= data.len() {
            m[0][0] = r_f32(&data[sf.offset..], is_le);
            m[1][1] = r_f32(&data[sf.offset + 4..], is_le);
            m[2][2] = r_f32(&data[sf.offset + 8..], is_le);
        }
    }
    m
}

#[inline]
fn xform_blend_pos(m: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    let wx = m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0];
    let wy = m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1];
    let wz = m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2];
    // Blender Z-up -> WebGL Y-up
    [wx, wz, -wy]
}

fn parse_blend_mesh(
    blk: &BlendBlock<'_>,
    dna: &Sdna,
    data_map: &HashMap<u64, &[u8]>,
    ptr_size: usize,
    is_le: bool,
    id_name_off: usize,
) -> Option<ParsedBlendMesh> {
    let m = blk.data;
    let name = read_id_name(m, id_name_off)
        .map(|n| clean_blend_id(&n))
        .unwrap_or_else(|| format!("Mesh_{}", blk.old));

    let totvert = dna
        .find_field("Mesh", "totvert")
        .or_else(|| dna.find_field("Mesh", "verts_num"))
        .and_then(|f| read_int(m, f.offset, f.size, is_le))
        .unwrap_or(0)
        .max(0) as usize;
    if totvert == 0 || totvert > 500_000 {
        return None;
    }

    let mut verts: Vec<[f32; 3]> = Vec::new();

    // Strategy 1: Legacy MVert array (`Mesh.mvert`)
    if let Some(mv_f) = dna.find_field("Mesh", "mvert") {
        let ptr = read_ptr(m, mv_f.offset, ptr_size, is_le);
        if let (Some(vdata), Some(mvert_struct), Some(co_f)) = (
            data_map.get(&ptr),
            dna.struct_by_name("MVert"),
            dna.find_field("MVert", "co"),
        ) {
            for i in 0..totvert {
                let off = i * mvert_struct.size + co_f.offset;
                if off + 12 <= vdata.len() {
                    verts.push([
                        r_f32(&vdata[off..], is_le),
                        r_f32(&vdata[off + 4..], is_le),
                        r_f32(&vdata[off + 8..], is_le),
                    ]);
                }
            }
        }
    }

    // Strategy 2: CustomData (`Mesh.vert_data` -> "position")
    if verts.is_empty() {
        if let Some(vbuf) = find_customdata_layer(m, dna, data_map, ptr_size, is_le, "vert_data", "position", Some(48))
            .or_else(|| find_attribute_layer(m, dna, data_map, ptr_size, is_le, "position"))
        {
            for vi in 0..totvert {
                let po = vi * 12;
                if po + 12 <= vbuf.len() {
                    verts.push([
                        r_f32(&vbuf[po..], is_le),
                        r_f32(&vbuf[po + 4..], is_le),
                        r_f32(&vbuf[po + 8..], is_le),
                    ]);
                }
            }
        }
    }

    if verts.is_empty() {
        return None;
    }

    // Read Material slot pointer array (`Mesh.mat`, length `totcol`)
    let totcol = dna
        .find_field("Mesh", "totcol")
        .and_then(|f| read_i16_or_i32(m, f.offset, f.size, is_le))
        .unwrap_or(0)
        .max(0) as usize;
    let mut mat_ptrs = Vec::new();
    if totcol > 0 {
        if let Some(mf) = dna.find_field("Mesh", "mat") {
            let mptr = read_ptr(m, mf.offset, ptr_size, is_le);
            if let Some(mbuf) = data_map.get(&mptr) {
                for slot in 0..totcol.min(64) {
                    mat_ptrs.push(read_ptr(mbuf, slot * ptr_size, ptr_size, is_le));
                }
            }
        }
    }

    // Read UVs from `.uv_map` / `UVMap` / `MLoopUV`
    let uv_raw = find_customdata_layer(m, dna, data_map, ptr_size, is_le, "corner_data", "UVMap", None)
        .or_else(|| find_customdata_layer(m, dna, data_map, ptr_size, is_le, "corner_data", ".uv_map", None))
        .or_else(|| find_customdata_layer(m, dna, data_map, ptr_size, is_le, "ldata", "UVMap", None))
        .or_else(|| find_attribute_layer(m, dna, data_map, ptr_size, is_le, "UVMap"))
        .or_else(|| find_attribute_layer(m, dna, data_map, ptr_size, is_le, ".uv_map"));

    let corner_uv_at = |c_idx: usize| -> [f32; 2] {
        if let Some(buf) = uv_raw {
            // Check either tightly packed `[f32; 2]` (8B) or `MLoopUV` struct (`uv[2]` at offset 0)
            let stride = if let Some(st) = dna.struct_by_name("MLoopUV") {
                if buf.len() >= (c_idx + 1) * st.size && st.size > 8 && buf.len() % st.size == 0 {
                    st.size
                } else {
                    8
                }
            } else {
                8
            };
            let off = c_idx * stride;
            if off + 8 <= buf.len() {
                let u = r_f32(&buf[off..], is_le);
                let v = r_f32(&buf[off + 4..], is_le);
                if u.is_finite() && v.is_finite() {
                    return [u, 1.0 - v];
                }
            }
        }
        [0.0, 0.0]
    };

    // Per-face material_index buffer (Blender 3.5+ / 5.0+)
    let mat_idx_buf = find_customdata_layer(m, dna, data_map, ptr_size, is_le, "face_data", "material_index", None)
        .or_else(|| find_customdata_layer(m, dna, data_map, ptr_size, is_le, "pdata", "material_index", None))
        .or_else(|| find_attribute_layer(m, dna, data_map, ptr_size, is_le, "material_index"));

    let mut faces = Vec::new();
    let vlen = verts.len();

    // Strategy A: Blender 3.5–5.x `face_offset_indices` + `.corner_vert`
    if let (Some(fo_f), Some(totface_f)) = (
        dna.find_field("Mesh", "face_offset_indices"),
        dna.find_field("Mesh", "faces_num").or_else(|| dna.find_field("Mesh", "totpoly")),
    ) {
        let nfaces = read_int(m, totface_f.offset, totface_f.size, is_le).unwrap_or(0).max(0) as usize;
        let fo_ptr = read_ptr(m, fo_f.offset, ptr_size, is_le);
        let corner_verts_buf = find_customdata_layer(m, dna, data_map, ptr_size, is_le, "corner_data", ".corner_vert", None)
            .or_else(|| find_attribute_layer(m, dna, data_map, ptr_size, is_le, ".corner_vert"));
        if let (Some(fo_buf), Some(cv_buf)) = (data_map.get(&fo_ptr), corner_verts_buf) {
            for fi in 0..nfaces.min(200_000) {
                let s_off = fi * 4;
                let e_off = (fi + 1) * 4;
                if e_off + 4 > fo_buf.len() { break; }
                let s = r_i32(&fo_buf[s_off..], is_le).max(0) as usize;
                let e = r_i32(&fo_buf[e_off..], is_le).max(0) as usize;
                if e > s + 2 && e * 4 <= cv_buf.len() {
                    let mut fverts = Vec::with_capacity(e - s);
                    let mut fuvs = Vec::with_capacity(e - s);
                    let mut ok = true;
                    for ci in s..e {
                        let vi = r_i32(&cv_buf[ci * 4..], is_le);
                        if vi < 0 || (vi as usize) >= vlen {
                            ok = false;
                            break;
                        }
                        fverts.push(vi as u32);
                        fuvs.push(corner_uv_at(ci));
                    }
                    if ok {
                        let mslot = mat_idx_buf
                            .and_then(|b| (fi * 4 + 4 <= b.len()).then(|| r_i32(&b[fi * 4..], is_le).max(0) as usize))
                            .unwrap_or(0);
                        faces.push((fverts, fuvs, mslot));
                    }
                }
            }
        }
    }

    // Strategy B: Legacy Blender 2.x–3.4 `mpoly` + `mloop`
    if faces.is_empty() {
        if let (Some(mp_f), Some(ml_f), Some(tp_f), Some(mp_st), Some(ml_st)) = (
            dna.find_field("Mesh", "mpoly"),
            dna.find_field("Mesh", "mloop"),
            dna.find_field("Mesh", "totpoly"),
            dna.struct_by_name("MPoly"),
            dna.struct_by_name("MLoop"),
        ) {
            let nfaces = read_int(m, tp_f.offset, tp_f.size, is_le).unwrap_or(0).max(0) as usize;
            let mp_ptr = read_ptr(m, mp_f.offset, ptr_size, is_le);
            let ml_ptr = read_ptr(m, ml_f.offset, ptr_size, is_le);
            if let (Some(mp_buf), Some(ml_buf), Some(ls_f), Some(tl_f), Some(lv_f)) = (
                data_map.get(&mp_ptr),
                data_map.get(&ml_ptr),
                dna.find_field("MPoly", "loopstart"),
                dna.find_field("MPoly", "totloop"),
                dna.find_field("MLoop", "v"),
            ) {
                let mat_nr_f = dna.find_field("MPoly", "mat_nr");
                for fi in 0..nfaces.min(200_000) {
                    let base = fi * mp_st.size;
                    if base + mp_st.size > mp_buf.len() { break; }
                    let s = read_int(mp_buf, base + ls_f.offset, ls_f.size, is_le).unwrap_or(0).max(0) as usize;
                    let cnt = read_int(mp_buf, base + tl_f.offset, tl_f.size, is_le).unwrap_or(0).max(0) as usize;
                    let mslot = mat_nr_f
                        .and_then(|mf| read_i16_or_i32(mp_buf, base + mf.offset, mf.size, is_le))
                        .unwrap_or(0)
                        .max(0) as usize;
                    if cnt >= 3 {
                        let mut fverts = Vec::with_capacity(cnt);
                        let mut fuvs = Vec::with_capacity(cnt);
                        let mut ok = true;
                        for ci in s..s + cnt {
                            let lo = ci * ml_st.size + lv_f.offset;
                            if lo + 4 > ml_buf.len() { ok = false; break; }
                            let vi = r_u32(&ml_buf[lo..], is_le) as usize;
                            if vi >= vlen { ok = false; break; }
                            fverts.push(vi as u32);
                            fuvs.push(corner_uv_at(ci));
                        }
                        if ok {
                            faces.push((fverts, fuvs, mslot));
                        }
                    }
                }
            }
        }
    }

    let mut wire_edges = Vec::new();
    if faces.is_empty() {
        for pair in (0..verts.len() as u32).collect::<Vec<_>>().chunks_exact(2).take(40_000) {
            wire_edges.push([pair[0], pair[1]]);
        }
    }

    Some(ParsedBlendMesh {
        name,
        verts,
        faces,
        wire_edges,
        mat_ptrs,
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_mesh_instance(
    id: &str,
    name: String,
    kind: &str,
    visible: bool,
    parent_id: Option<String>,
    mesh: &ParsedBlendMesh,
    obmat: &[[f32; 4]; 4],
    ma_map: &HashMap<u64, BlendMaterialData>,
    build: &mut MeshBuild,
) {
    let mut by_slot: BTreeMap<usize, Vec<(Vec<u32>, Vec<[f32; 2]>)>> = BTreeMap::new();
    for (fv, fuv, slot) in &mesh.faces {
        by_slot.entry(*slot).or_default().push((fv.clone(), fuv.clone()));
    }

    let obj_start_v = build.positions.len() / 3;
    let mut batch_indices = Vec::new();
    let mut material_names = Vec::new();
    let mut texture_names = Vec::new();

    for (slot, faces) in by_slot {
        let start_v = (build.positions.len() / 3) as u32;
        for (fv, fuv) in faces {
            if build.positions.len() >= 300_000 * 9 || fv.len() < 3 {
                break;
            }
            for k in 1..fv.len() - 1 {
                let p0 = xform_blend_pos(obmat, mesh.verts[fv[0] as usize]);
                let p1 = xform_blend_pos(obmat, mesh.verts[fv[k] as usize]);
                let p2 = xform_blend_pos(obmat, mesh.verts[fv[k + 1] as usize]);
                let ux = p1[0] - p0[0];
                let uy = p1[1] - p0[1];
                let uz = p1[2] - p0[2];
                let vx = p2[0] - p0[0];
                let vy = p2[1] - p0[1];
                let vz = p2[2] - p0[2];
                let nx = uy * vz - uz * vy;
                let ny = uz * vx - ux * vz;
                let nz = ux * vy - uy * vx;
                let nl = (nx * nx + ny * ny + nz * nz).sqrt().max(1e-8);
                let n = [nx / nl, ny / nl, nz / nl];

                for (p, uv) in [(p0, fuv[0]), (p1, fuv[k]), (p2, fuv[k + 1])] {
                    build.positions.extend_from_slice(&p);
                    build.normals.extend_from_slice(&n);
                    build.uvs.extend_from_slice(&uv);
                }
            }
        }
        let end_v = (build.positions.len() / 3) as u32;
        if end_v > start_v {
            let mat_opt = mesh.mat_ptrs.get(slot).and_then(|ptr| ma_map.get(ptr));
            let (base_color, tex_idx) = if let Some(mat) = mat_opt {
                if !material_names.contains(&mat.name) {
                    material_names.push(mat.name.clone());
                }
                if let Some(ref tn) = mat.texture_name {
                    if !texture_names.contains(tn) {
                        texture_names.push(tn.clone());
                    }
                }
                (mat.base_color, mat.texture_index)
            } else {
                ([0.82, 0.90, 0.92, 1.0], None)
            };
            let bi = build.batches.len();
            build.batches.push(MeshBatch {
                start_vertex: start_v,
                vertex_count: end_v - start_v,
                base_color,
                texture_index: tex_idx,
            });
            batch_indices.push(bi);
        }
    }

    if mesh.faces.is_empty() {
        for &[a, b] in &mesh.wire_edges {
            if let (Some(&va), Some(&vb)) = (mesh.verts.get(a as usize), mesh.verts.get(b as usize)) {
                build.wireframe_lines.extend_from_slice(&xform_blend_pos(obmat, va));
                build.wireframe_lines.extend_from_slice(&xform_blend_pos(obmat, vb));
            }
        }
    }

    let vcount = (build.positions.len() / 3).saturating_sub(obj_start_v);
    if vcount > 0 || !mesh.wire_edges.is_empty() {
        build.layers.push(SceneLayer3d {
            id: id.to_string(),
            name,
            kind: kind.into(),
            visible,
            parent_id,
            material_names,
            texture_names,
            vertex_count: vcount,
            triangle_count: vcount / 3,
            batch_indices,
        });
    }
}

fn find_customdata_layer<'a>(
    mesh_data: &[u8],
    dna: &Sdna,
    data_map: &HashMap<u64, &'a [u8]>,
    ptr_size: usize,
    is_le: bool,
    cd_field: &str,
    target_name: &str,
    fallback_type: Option<i64>,
) -> Option<&'a [u8]> {
    let cd_f = dna.find_field("Mesh", cd_field)?;
    let lf = dna.find_field("CustomData", "layers")?;
    let tf = dna.find_field("CustomData", "totlayer")?;
    let layer_st = dna.struct_by_name("CustomDataLayer")?;
    let n = read_int(mesh_data, cd_f.offset + tf.offset, tf.size, is_le).unwrap_or(0).max(0) as usize;
    let ptr = read_ptr(mesh_data, cd_f.offset + lf.offset, ptr_size, is_le);
    let lbuf = data_map.get(&ptr)?;
    let nf = dna.find_field("CustomDataLayer", "name")?;
    let df = dna.find_field("CustomDataLayer", "data")?;
    let type_f = dna.find_field("CustomDataLayer", "type");
    for i in 0..n.min(64) {
        let base = i * layer_st.size;
        if base + layer_st.size > lbuf.len() { break; }
        let nm = read_cstr(&lbuf[base + nf.offset..base + nf.offset + nf.size.min(64)]);
        let ltype = type_f.and_then(|tf2| read_int(lbuf, base + tf2.offset, tf2.size, is_le)).unwrap_or(-1);
        if nm == target_name || fallback_type.is_some_and(|ft| ft == ltype) {
            let dptr = read_ptr(lbuf, base + df.offset, ptr_size, is_le);
            return data_map.get(&dptr).copied();
        }
    }
    None
}

fn find_attribute_layer<'a>(
    mesh_data: &[u8],
    dna: &Sdna,
    data_map: &HashMap<u64, &'a [u8]>,
    ptr_size: usize,
    is_le: bool,
    target_name: &str,
) -> Option<&'a [u8]> {
    let attr_f = dna.find_field("Mesh", "attribute_storage")?;
    let daf = dna.find_field("AttributeStorage", "dna_attributes")?;
    let dnf = dna.find_field("AttributeStorage", "dna_attributes_num")?;
    let nattrs = read_int(mesh_data, attr_f.offset + dnf.offset, dnf.size, is_le).unwrap_or(0).max(0) as usize;
    let aptr = read_ptr(mesh_data, attr_f.offset + daf.offset, ptr_size, is_le);
    let abuf = data_map.get(&aptr)?;
    let attr_st = dna.struct_by_name("Attribute")?;
    let nf = dna.find_field("Attribute", "name")?;
    let df = dna.find_field("Attribute", "data")?;
    for i in 0..nattrs.min(64) {
        let base = i * attr_st.size;
        if base + attr_st.size > abuf.len() { break; }
        let name_ptr = read_ptr(abuf, base + nf.offset, ptr_size, is_le);
        let aname = data_map
            .get(&name_ptr)
            .map(|b| read_cstr(b))
            .unwrap_or_else(|| read_cstr(&abuf[base + nf.offset..base + nf.offset + nf.size.min(64)]));
        if aname == target_name {
            let dptr = read_ptr(abuf, base + df.offset, ptr_size, is_le);
            if let Some(arr_buf) = data_map.get(&dptr) {
                if let Some(arr_df) = dna.find_field("AttributeArray", "data") {
                    let inner_ptr = read_ptr(arr_buf, arr_df.offset, ptr_size, is_le);
                    return data_map.get(&inner_ptr).copied();
                }
                return Some(arr_buf);
            }
        }
    }
    None
}

fn extract_blend_skeleton(
    blocks: &[BlendBlock<'_>],
    dna: &Sdna,
    data_map: &HashMap<u64, &[u8]>,
    ptr_size: usize,
    is_le: bool,
    id_name_off: usize,
) -> Option<SkeletonAnimation> {
    let mut bones = Vec::new();
    let mut bone_name_to_idx: HashMap<String, usize> = HashMap::new();

    if let (Some(bonebase_f), Some(b_name_f), Some(b_next_f), Some(b_child_f)) = (
        dna.find_field("bArmature", "bonebase"),
        dna.find_field("Bone", "name"),
        dna.find_field("Bone", "next"),
        dna.find_field("Bone", "childbase"),
    ) {
        let head_f = dna.find_field("Bone", "arm_head").or_else(|| dna.find_field("Bone", "head"));

        #[allow(clippy::too_many_arguments)]
        fn walk_bones(
            first_ptr: u64,
            parent_idx: Option<usize>,
            parent_world: [f32; 3],
            dna: &Sdna,
            data_map: &HashMap<u64, &[u8]>,
            ptr_size: usize,
            is_le: bool,
            b_name_f: &SdnaField,
            b_next_f: &SdnaField,
            b_child_f: &SdnaField,
            head_f: Option<&SdnaField>,
            bones: &mut Vec<SkeletonBone>,
            name_map: &mut HashMap<String, usize>,
        ) {
            let mut cur = first_ptr;
            for _ in 0..256 {
                if cur == 0 { break; }
                let Some(buf) = data_map.get(&cur) else { break };
                let bname = read_cstr(&buf[b_name_f.offset..b_name_f.offset + b_name_f.size.min(64)]);
                let world = head_f
                    .and_then(|hf| {
                        (hf.offset + 12 <= buf.len()).then(|| {
                            let x = r_f32(&buf[hf.offset..], is_le);
                            let y = r_f32(&buf[hf.offset + 4..], is_le);
                            let z = r_f32(&buf[hf.offset + 8..], is_le);
                            [x, z, -y]
                        })
                    })
                    .unwrap_or([0.0, 0.0, 0.0]);
                let local = [
                    world[0] - parent_world[0],
                    world[1] - parent_world[1],
                    world[2] - parent_world[2],
                ];
                let idx = bones.len();
                name_map.insert(bname.clone(), idx);
                bones.push(SkeletonBone {
                    id: idx,
                    name: if bname.is_empty() { format!("Bone_{idx}") } else { bname },
                    parent: parent_idx,
                    rest_local_pos: local,
                    rest_local_rot: [0.0, 0.0, 0.0, 1.0],
                });
                let child_first = read_ptr(buf, b_child_f.offset, ptr_size, is_le);
                if child_first != 0 {
                    walk_bones(
                        child_first,
                        Some(idx),
                        world,
                        dna,
                        data_map,
                        ptr_size,
                        is_le,
                        b_name_f,
                        b_next_f,
                        b_child_f,
                        head_f,
                        bones,
                        name_map,
                    );
                }
                cur = read_ptr(buf, b_next_f.offset, ptr_size, is_le);
            }
        }

        for blk in blocks {
            if &blk.code == b"AR\0\0" {
                let first = read_ptr(blk.data, bonebase_f.offset, ptr_size, is_le);
                walk_bones(
                    first,
                    None,
                    [0.0, 0.0, 0.0],
                    dna,
                    data_map,
                    ptr_size,
                    is_le,
                    b_name_f,
                    b_next_f,
                    b_child_f,
                    head_f,
                    &mut bones,
                    &mut bone_name_to_idx,
                );
            }
        }
    }

    // Parse `AC` Action blocks -> FCurve keyframes (`pose.bones["..."]`)
    let mut clips = Vec::new();
    if let (Some(curves_f), Some(fc_next_f), Some(rna_f), Some(arr_idx_f), Some(bezt_f), Some(totvert_f), Some(bezt_st)) = (
        dna.find_field("bAction", "curves"),
        dna.find_field("FCurve", "next"),
        dna.find_field("FCurve", "rna_path"),
        dna.find_field("FCurve", "array_index"),
        dna.find_field("FCurve", "bezt"),
        dna.find_field("FCurve", "totvert"),
        dna.struct_by_name("BezTriple"),
    ) {
        let vec_f = dna.find_field("BezTriple", "vec");
        for blk in blocks {
            if &blk.code != b"AC\0\0" {
                continue;
            }
            let clip_name = read_id_name(blk.data, id_name_off)
                .map(|n| clean_blend_id(&n))
                .unwrap_or_else(|| format!("Action_{}", clips.len() + 1));

            let mut per_bone: HashMap<usize, (Vec<(f32, [f32; 3])>, Vec<(f32, [f32; 3])>)> = HashMap::new();
            let mut fptr = read_ptr(blk.data, curves_f.offset, ptr_size, is_le);
            let mut max_time = 0.0f32;

            for _ in 0..512 {
                if fptr == 0 { break; }
                let Some(fbuf) = data_map.get(&fptr) else { break };
                let rna_ptr = read_ptr(fbuf, rna_f.offset, ptr_size, is_le);
                let rna_str = data_map.get(&rna_ptr).map(|b| read_cstr(b)).unwrap_or_default();
                let aidx = read_int(fbuf, arr_idx_f.offset, arr_idx_f.size, is_le).unwrap_or(0) as usize;
                let totv = read_int(fbuf, totvert_f.offset, totvert_f.size, is_le).unwrap_or(0).max(0) as usize;
                let bz_ptr = read_ptr(fbuf, bezt_f.offset, ptr_size, is_le);

                if let (Some(bname), Some(bz_buf), Some(vf)) = (extract_quoted_bone(&rna_str), data_map.get(&bz_ptr), vec_f) {
                    if let Some(&bidx) = bone_name_to_idx.get(&bname) {
                        let is_rot = rna_str.contains("rotation_euler");
                        let is_loc = rna_str.contains("location");
                        if (is_rot || is_loc) && aidx < 3 {
                            for ki in 0..totv.min(240) {
                                let base = ki * bezt_st.size + vf.offset;
                                // BezTriple.vec[1][0] = frame, vec[1][1] = value (stride = 3*4 bytes per handle)
                                if base + 24 <= bz_buf.len() {
                                    let frame = r_f32(&bz_buf[base + 12..], is_le);
                                    let val = r_f32(&bz_buf[base + 16..], is_le);
                                    let t = (frame / 24.0).max(0.0);
                                    max_time = max_time.max(t);
                                    let entry = per_bone.entry(bidx).or_default();
                                    let vec = if is_loc { &mut entry.0 } else { &mut entry.1 };
                                    if let Some(slot) = vec.iter_mut().find(|(et, _)| (*et - t).abs() < 1e-3) {
                                        slot.1[aidx] = val;
                                    } else {
                                        let mut arr = [0.0f32; 3];
                                        arr[aidx] = val;
                                        vec.push((t, arr));
                                    }
                                }
                            }
                        }
                    }
                }
                fptr = read_ptr(fbuf, fc_next_f.offset, ptr_size, is_le);
            }

            let mut channels = Vec::new();
            for (bidx, (mut locs, mut rots)) in per_bone {
                locs.sort_by(|a, b| a.0.total_cmp(&b.0));
                rots.sort_by(|a, b| a.0.total_cmp(&b.0));
                let times = if rots.len() >= locs.len() {
                    rots.iter().map(|r| r.0).collect::<Vec<_>>()
                } else {
                    locs.iter().map(|l| l.0).collect::<Vec<_>>()
                };
                if times.is_empty() { continue; }
                let translations = locs.into_iter().map(|(_, p)| [p[0], p[2], -p[1]]).collect();
                let rotations = rots
                    .into_iter()
                    .map(|(_, e)| euler_deg_xyz_to_quat(e[0].to_degrees() as f64, e[2].to_degrees() as f64, (-e[1]).to_degrees() as f64))
                    .collect();
                channels.push(BoneChannel {
                    bone_index: bidx,
                    times,
                    translations,
                    rotations,
                    scales: Vec::new(),
                });
            }
            if !channels.is_empty() {
                clips.push(AnimationClip {
                    name: clip_name,
                    duration_secs: max_time.max(0.5),
                    channels,
                });
            }
        }
    }

    if bones.is_empty() {
        return None;
    }
    let hint = infer_species_hint(bones.iter().map(|b| b.name.as_str()));
    Some(SkeletonAnimation {
        species_hint: Some(hint),
        bones,
        clips,
    })
}

fn extract_quoted_bone(rna: &str) -> Option<String> {
    let idx = rna.find("pose.bones[\"")?;
    let rest = &rna[idx + 12..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

impl Sdna {
    fn struct_by_name(&self, name: &str) -> Option<&SdnaStruct> {
        self.by_name.get(name).and_then(|&i| self.structs.get(i))
    }

    fn find_field(&self, struct_name: &str, field_name: &str) -> Option<&SdnaField> {
        let st = self.struct_by_name(struct_name)?;
        st.fields.iter().find(|f| {
            let clean = f.name.trim_start_matches('*').split('[').next().unwrap_or("");
            clean == field_name
        })
    }
}

fn parse_sdna(dna: &[u8], ptr_size: usize, is_le: bool) -> Option<Sdna> {
    if dna.len() < 12 || &dna[0..4] != b"SDNA" {
        return None;
    }
    let mut pos = 4usize;
    let mut names = Vec::new();
    let mut types = Vec::new();
    let mut type_lens = Vec::new();
    let mut structs = Vec::new();

    while pos + 4 <= dna.len() {
        let tag = &dna[pos..pos + 4];
        pos += 4;
        if tag == b"NAME" && pos + 4 <= dna.len() {
            let cnt = r_u32(&dna[pos..], is_le) as usize;
            pos += 4;
            for _ in 0..cnt {
                let s = read_cstr(&dna[pos..]);
                pos += s.len() + 1;
                names.push(s);
            }
            pos = (pos + 3) & !3;
        } else if tag == b"TYPE" && pos + 4 <= dna.len() {
            let cnt = r_u32(&dna[pos..], is_le) as usize;
            pos += 4;
            for _ in 0..cnt {
                let s = read_cstr(&dna[pos..]);
                pos += s.len() + 1;
                types.push(s);
            }
            pos = (pos + 3) & !3;
        } else if tag == b"TLEN" {
            for _ in 0..types.len() {
                if pos + 2 > dna.len() { break; }
                type_lens.push(r_u16(&dna[pos..], is_le) as usize);
                pos += 2;
            }
            pos = (pos + 3) & !3;
        } else if tag == b"STRC" && pos + 4 <= dna.len() {
            let cnt = r_u32(&dna[pos..], is_le) as usize;
            pos += 4;
            for _ in 0..cnt {
                if pos + 4 > dna.len() { break; }
                let type_idx = r_u16(&dna[pos..], is_le) as usize;
                let nf = r_u16(&dna[pos + 2..], is_le) as usize;
                pos += 4;
                let mut fields = Vec::with_capacity(nf);
                let mut offset = 0usize;
                for _ in 0..nf {
                    if pos + 4 > dna.len() { break; }
                    let ft = r_u16(&dna[pos..], is_le) as usize;
                    let fn_idx = r_u16(&dna[pos + 2..], is_le) as usize;
                    pos += 4;
                    let fname = names.get(fn_idx).cloned().unwrap_or_default();
                    let fsz = compute_field_size(&fname, ft, &type_lens, ptr_size);
                    fields.push(SdnaField { type_idx: ft, name: fname, offset, size: fsz });
                    offset += fsz;
                }
                let sname = types.get(type_idx).cloned().unwrap_or_default();
                let ssize = type_lens.get(type_idx).copied().unwrap_or(offset);
                structs.push(SdnaStruct { name: sname, size: ssize, fields });
            }
            break;
        } else {
            break;
        }
    }

    let mut by_name = HashMap::with_capacity(structs.len());
    for (idx, s) in structs.iter().enumerate() {
        by_name.insert(s.name.clone(), idx);
    }
    let _ = structs.first().and_then(|s| s.fields.first().map(|f| f.type_idx));
    Some(Sdna { structs, by_name })
}

fn compute_field_size(name: &str, type_idx: usize, type_lens: &[usize], ptr_size: usize) -> usize {
    let base_sz = if name.starts_with('*') || name.contains("(*") {
        ptr_size
    } else {
        type_lens.get(type_idx).copied().unwrap_or(4)
    };
    let mut mult = 1usize;
    let mut rest = name;
    while let Some(open) = rest.find('[') {
        rest = &rest[open + 1..];
        if let Some(close) = rest.find(']') {
            if let Ok(n) = rest[..close].parse::<usize>() {
                mult *= n;
            }
            rest = &rest[close + 1..];
        } else {
            break;
        }
    }
    base_sz * mult
}

fn format_blend_version(raw: &str) -> String {
    let clean: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if clean.len() == 4 {
        let maj: u32 = clean[0..2].parse().unwrap_or(0);
        let min: u32 = clean[2..4].parse().unwrap_or(0);
        format!("v{maj}.{min}")
    } else if clean.len() == 3 {
        let maj = &clean[0..1];
        let min: u32 = clean[1..3].parse().unwrap_or(0);
        format!("v{maj}.{min}")
    } else {
        format!("v{clean}")
    }
}

fn read_id_name(data: &[u8], off: usize) -> Option<String> {
    let slice = data.get(off..off + 66.min(data.len().saturating_sub(off)))?;
    let s = read_cstr(slice);
    (!s.is_empty()).then_some(s)
}

fn read_cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len().min(128));
    String::from_utf8_lossy(&b[..end]).to_string()
}

fn read_ptr(b: &[u8], off: usize, ptr_size: usize, is_le: bool) -> u64 {
    if ptr_size == 8 && off + 8 <= b.len() {
        r_u64(&b[off..], is_le)
    } else if ptr_size == 4 && off + 4 <= b.len() {
        r_u32(&b[off..], is_le) as u64
    } else {
        0
    }
}

fn read_int(b: &[u8], off: usize, size: usize, is_le: bool) -> Option<i64> {
    if size >= 8 && off + 8 <= b.len() {
        Some(r_i64(&b[off..], is_le))
    } else if size >= 4 && off + 4 <= b.len() {
        Some(r_i32(&b[off..], is_le) as i64)
    } else {
        None
    }
}

fn read_i16_or_i32(b: &[u8], off: usize, size: usize, is_le: bool) -> Option<i64> {
    if size == 2 && off + 2 <= b.len() {
        let v = if is_le {
            i16::from_le_bytes([b[off], b[off + 1]])
        } else {
            i16::from_be_bytes([b[off], b[off + 1]])
        };
        Some(v as i64)
    } else {
        read_int(b, off, size, is_le)
    }
}

fn r_u16(b: &[u8], le: bool) -> u16 {
    if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) }
}
fn r_u32(b: &[u8], le: bool) -> u32 {
    if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) }
}
fn r_i32(b: &[u8], le: bool) -> i32 {
    if le { i32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { i32::from_be_bytes([b[0], b[1], b[2], b[3]]) }
}
fn r_u64(b: &[u8], le: bool) -> u64 {
    let arr: [u8; 8] = b[0..8].try_into().unwrap_or([0; 8]);
    if le { u64::from_le_bytes(arr) } else { u64::from_be_bytes(arr) }
}
fn r_i64(b: &[u8], le: bool) -> i64 {
    let arr: [u8; 8] = b[0..8].try_into().unwrap_or([0; 8]);
    if le { i64::from_le_bytes(arr) } else { i64::from_be_bytes(arr) }
}
fn r_f32(b: &[u8], le: bool) -> f32 {
    let arr: [u8; 4] = b[0..4].try_into().unwrap_or([0; 4]);
    if le { f32::from_le_bytes(arr) } else { f32::from_be_bytes(arr) }
}
