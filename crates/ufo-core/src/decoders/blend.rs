use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read as _};

use flate2::read::GzDecoder;
use ruzstd::decoding::StreamingDecoder as ZstdDecoder;

use crate::model::{MetadataEntry, TreeNode, ViewerPayload};

#[derive(Debug, Clone)]
struct BlendBlock<'a> {
    code: [u8; 4],
    sdna_nr: usize,
    old: u64,
    nr: usize,
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
    types: Vec<String>,
    type_lens: Vec<usize>,
    structs: Vec<SdnaStruct>,
    by_name: HashMap<String, usize>,
}

/// Decodes Blender `.blend` files from v2.x through v5.3+ (uncompressed, Gzip, or Zstandard).
pub fn decode_blend_info(
    bytes: &[u8],
    metadata: &mut Vec<MetadataEntry>,
) -> Option<(String, Vec<[f32; 3]>, Vec<[u32; 3]>, Vec<f32>, Vec<String>, Vec<TreeNode>, String)> {
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

    // Detect Blender 5.0+ 17-byte header (`BLENDER17-01v0503`) vs legacy 12-byte header (`BLENDER-v402`)
    let (hdr_len, ptr_size, is_large, is_le, version_str) = if raw.len() >= 17
        && raw[7].is_ascii_digit()
        && raw[8].is_ascii_digit()
    {
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

    // Parse all BHead blocks
    let mut blocks = Vec::new();
    let mut data_map: HashMap<u64, &[u8]> = HashMap::new();
    let mut dna_bytes: Option<&[u8]> = None;
    let mut block_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut pos = hdr_len;

    while pos < raw.len() {
        let (code, sdna_nr, old, len, nr, bhead_sz) = if is_large {
            if pos + 32 > raw.len() { break; }
            let code = [raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]];
            let sdna = r_i32(&raw[pos + 4..], is_le).max(0) as usize;
            let old = r_u64(&raw[pos + 8..], is_le);
            let len = r_i64(&raw[pos + 16..], is_le).max(0) as usize;
            let nr = r_i64(&raw[pos + 24..], is_le).max(0) as usize;
            (code, sdna, old, len, nr, 32usize)
        } else if ptr_size == 8 {
            if pos + 24 > raw.len() { break; }
            let code = [raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]];
            let len = r_u32(&raw[pos + 4..], is_le) as usize;
            let old = r_u64(&raw[pos + 8..], is_le);
            let sdna = r_i32(&raw[pos + 16..], is_le).max(0) as usize;
            let nr = r_i32(&raw[pos + 20..], is_le).max(0) as usize;
            (code, sdna, old, len, nr, 24usize)
        } else {
            if pos + 20 > raw.len() { break; }
            let code = [raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]];
            let len = r_u32(&raw[pos + 4..], is_le) as usize;
            let old = r_u32(&raw[pos + 8..], is_le) as u64;
            let sdna = r_i32(&raw[pos + 12..], is_le).max(0) as usize;
            let nr = r_i32(&raw[pos + 16..], is_le).max(0) as usize;
            (code, sdna, old, len, nr, 20usize)
        };

        pos += bhead_sz;
        let code_str = String::from_utf8_lossy(&code).trim_end_matches('\0').to_string();
        if code_str == "ENDB" || pos.saturating_add(len) > raw.len() {
            break;
        }
        let payload = &raw[pos..pos + len];
        pos += len;

        *block_counts.entry(code_str.clone()).or_default() += 1;
        if &code == b"DNA1" {
            dna_bytes = Some(payload);
        } else if &code == b"DATA" && old != 0 {
            data_map.insert(old, payload);
        }
        blocks.push(BlendBlock { code, sdna_nr, old, nr, data: payload });
    }

    metadata.push(MetadataEntry { section: "Blender".into(), key: "Blocks".into(), value: blocks.len().to_string() });

    let sdna = dna_bytes.and_then(|d| parse_sdna(d, ptr_size, is_le));
    let mut parts = Vec::new();
    let mut tree_nodes = Vec::new();
    let mut all_verts: Vec<[f32; 3]> = Vec::new();
    let mut all_tris: Vec<[u32; 3]> = Vec::new();
    let mut wire_lines: Vec<f32> = Vec::new();

    if let Some(ref dna) = sdna {
        metadata.push(MetadataEntry { section: "Blender SDNA".into(), key: "Struct Types".into(), value: dna.structs.len().to_string() });

        let id_name_off = dna
            .find_field("ID", "name")
            .map(|f| f.offset)
            .unwrap_or(if ptr_size == 8 { 24 } else { 12 });

        // Collect named IDs (Objects, Meshes, Materials, Cameras, Lights, Scenes)
        for blk in &blocks {
            let prefix = String::from_utf8_lossy(&blk.code);
            let p2 = prefix.trim_end_matches('\0');
            if matches!(p2, "OB" | "ME" | "MA" | "CA" | "LA" | "SC" | "AR" | "AC" | "WO") {
                if let Some(name) = read_id_name(blk.data, id_name_off) {
                    if parts.len() < 128 {
                        parts.push(name.clone());
                    }
                    if tree_nodes.len() < 200 {
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

        // Extract 3D Mesh geometry from `ME` blocks (supports legacy MVert, CustomData, & Blender 5.0 AttributeStorage)
        for blk in &blocks {
            if &blk.code == b"ME\0\0" && all_verts.len() < 250_000 {
                extract_blend_mesh(
                    blk,
                    dna,
                    &data_map,
                    ptr_size,
                    is_le,
                    &mut all_verts,
                    &mut all_tris,
                    &mut wire_lines,
                );
            }
        }
    }

    let mut summary_lines = vec![
        format!("Blender {version_str} ({}-bit, {})", ptr_size * 8, if is_large { "LargeBHead8" } else { "BHead" }),
        format!("Total Data Blocks: {}", blocks.len()),
        format!("Extracted Mesh Vertices: {} · Triangles: {}", all_verts.len(), all_tris.len()),
        String::new(),
        "Block Counts:".into(),
    ];
    for (k, v) in &block_counts {
        summary_lines.push(format!("  {k:<4} : {v}"));
    }
    if !parts.is_empty() {
        summary_lines.push(String::new());
        summary_lines.push("Scene Datablocks:".into());
        for p in &parts {
            summary_lines.push(format!("  • {p}"));
        }
    }

    metadata.push(MetadataEntry { section: "Blender Mesh".into(), key: "Extracted Vertices".into(), value: all_verts.len().to_string() });
    metadata.push(MetadataEntry { section: "Blender Mesh".into(), key: "Extracted Triangles".into(), value: all_tris.len().to_string() });

    Some((
        format!("Blender {version_str} Scene"),
        all_verts,
        all_tris,
        wire_lines,
        parts,
        tree_nodes,
        summary_lines.join("\n"),
    ))
}

#[allow(clippy::too_many_arguments)]
fn extract_blend_mesh(
    blk: &BlendBlock<'_>,
    dna: &Sdna,
    data_map: &HashMap<u64, &[u8]>,
    ptr_size: usize,
    is_le: bool,
    out_verts: &mut Vec<[f32; 3]>,
    out_tris: &mut Vec<[u32; 3]>,
    out_wire: &mut Vec<f32>,
) {
    let base_vert = out_verts.len() as u32;
    let m = blk.data;
    let totvert = dna
        .find_field("Mesh", "totvert")
        .and_then(|f| read_int(m, f.offset, f.size, is_le))
        .unwrap_or(0)
        .max(0) as usize;
    if totvert == 0 || totvert > 500_000 {
        return;
    }

    let mut mesh_verts: Vec<[f32; 3]> = Vec::new();

    // Strategy 1: Legacy MVert array (`Mesh.mvert`)
    if let Some(mv_f) = dna.find_field("Mesh", "mvert") {
        let ptr = read_ptr(m, mv_f.offset, ptr_size, is_le);
        if let Some(vdata) = data_map.get(&ptr) {
            if let Some(mvert_struct) = dna.struct_by_name("MVert") {
                if let Some(co_f) = dna.find_field("MVert", "co") {
                    for i in 0..totvert {
                        let off = i * mvert_struct.size + co_f.offset;
                        if off + 12 <= vdata.len() {
                            mesh_verts.push([
                                r_f32(&vdata[off..], is_le),
                                r_f32(&vdata[off + 4..], is_le),
                                r_f32(&vdata[off + 8..], is_le),
                            ]);
                        }
                    }
                }
            }
        }
    }

    // Strategy 2: Blender 3.5–4.x CustomData (`Mesh.vert_data` -> layers -> "position")
    if mesh_verts.is_empty() {
        if let Some(vdata_f) = dna.find_field("Mesh", "vert_data") {
            let layers_f = dna.find_field("CustomData", "layers");
            let totlayer_f = dna.find_field("CustomData", "totlayer");
            if let (Some(lf), Some(tf)) = (layers_f, totlayer_f) {
                let nlayers = read_int(m, vdata_f.offset + tf.offset, tf.size, is_le).unwrap_or(0).max(0) as usize;
                let layers_ptr = read_ptr(m, vdata_f.offset + lf.offset, ptr_size, is_le);
                if let (Some(lbuf), Some(layer_st)) = (data_map.get(&layers_ptr), dna.struct_by_name("CustomDataLayer")) {
                    let name_f = dna.find_field("CustomDataLayer", "name");
                    let data_f = dna.find_field("CustomDataLayer", "data");
                    let type_f = dna.find_field("CustomDataLayer", "type");
                    if let (Some(nf), Some(df)) = (name_f, data_f) {
                        for i in 0..nlayers.min(64) {
                            let base = i * layer_st.size;
                            if base + layer_st.size > lbuf.len() { break; }
                            let lname = read_cstr(&lbuf[base + nf.offset..base + nf.offset + nf.size.min(64)]);
                            let ltype = type_f.and_then(|tf2| read_int(lbuf, base + tf2.offset, tf2.size, is_le)).unwrap_or(-1);
                            if lname == "position" || ltype == 48 {
                                let dptr = read_ptr(lbuf, base + df.offset, ptr_size, is_le);
                                if let Some(pbuf) = data_map.get(&dptr) {
                                    for vi in 0..totvert {
                                        let po = vi * 12;
                                        if po + 12 <= pbuf.len() {
                                            mesh_verts.push([
                                                r_f32(&pbuf[po..], is_le),
                                                r_f32(&pbuf[po + 4..], is_le),
                                                r_f32(&pbuf[po + 8..], is_le),
                                            ]);
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Strategy 3: Blender 5.0+ AttributeStorage (`Mesh.attribute_storage.dna_attributes`)
    if mesh_verts.is_empty() {
        if let Some(attr_f) = dna.find_field("Mesh", "attribute_storage") {
            let dna_attrs_f = dna.find_field("AttributeStorage", "dna_attributes");
            let dna_num_f = dna.find_field("AttributeStorage", "dna_attributes_num");
            if let (Some(daf), Some(dnf)) = (dna_attrs_f, dna_num_f) {
                let nattrs = read_int(m, attr_f.offset + dnf.offset, dnf.size, is_le).unwrap_or(0).max(0) as usize;
                let aptr = read_ptr(m, attr_f.offset + daf.offset, ptr_size, is_le);
                if let (Some(abuf), Some(attr_st)) = (data_map.get(&aptr), dna.struct_by_name("Attribute")) {
                    let aname_f = dna.find_field("Attribute", "name");
                    let adata_f = dna.find_field("Attribute", "data");
                    if let (Some(nf), Some(df)) = (aname_f, adata_f) {
                        for i in 0..nattrs.min(64) {
                            let base = i * attr_st.size;
                            if base + attr_st.size > abuf.len() { break; }
                            let name_ptr = read_ptr(abuf, base + nf.offset, ptr_size, is_le);
                            let aname = data_map
                                .get(&name_ptr)
                                .map(|b| read_cstr(b))
                                .unwrap_or_else(|| read_cstr(&abuf[base + nf.offset..base + nf.offset + nf.size.min(64)]));
                            if aname == "position" {
                                // `Attribute.data` points to `AttributeArray` (or inline), whose first `data` ptr holds `[f32; 3]`
                                let dptr = read_ptr(abuf, base + df.offset, ptr_size, is_le);
                                if let Some(arr_struct_buf) = data_map.get(&dptr) {
                                    // Check if `AttributeArray` struct exists in DNA
                                    let pbuf = if let Some(arr_data_f) = dna.find_field("AttributeArray", "data") {
                                        let inner_ptr = read_ptr(arr_struct_buf, arr_data_f.offset, ptr_size, is_le);
                                        data_map.get(&inner_ptr).copied().unwrap_or(arr_struct_buf)
                                    } else {
                                        arr_struct_buf
                                    };
                                    for vi in 0..totvert {
                                        let po = vi * 12;
                                        if po + 12 <= pbuf.len() {
                                            mesh_verts.push([
                                                r_f32(&pbuf[po..], is_le),
                                                r_f32(&pbuf[po + 4..], is_le),
                                                r_f32(&pbuf[po + 8..], is_le),
                                            ]);
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if mesh_verts.is_empty() {
        return;
    }

    // Swap Z/Y for upright Y-up viewport
    for v in &mut mesh_verts {
        *v = [v[0], v[2], -v[1]];
    }
    let vert_count = mesh_verts.len();
    out_verts.extend_from_slice(&mesh_verts);

    // Extract faces (`face_offset_indices` + `corner_verts`, or legacy `mpoly` + `mloop`)
    let mut added_faces = false;
    if let (Some(fo_f), Some(totface_f)) = (
        dna.find_field("Mesh", "face_offset_indices"),
        dna.find_field("Mesh", "faces_num").or_else(|| dna.find_field("Mesh", "totpoly")),
    ) {
        let nfaces = read_int(m, totface_f.offset, totface_f.size, is_le).unwrap_or(0).max(0) as usize;
        let fo_ptr = read_ptr(m, fo_f.offset, ptr_size, is_le);
        // Find `.corner_vert` in `corner_data` or `ldata` or `AttributeStorage`
        let corner_verts_buf = find_mesh_int_layer(m, dna, data_map, ptr_size, is_le, ".corner_vert");
        if let (Some(fo_buf), Some(cv_buf)) = (data_map.get(&fo_ptr), corner_verts_buf) {
            for fi in 0..nfaces.min(200_000) {
                let s_off = fi * 4;
                let e_off = (fi + 1) * 4;
                if e_off + 4 > fo_buf.len() { break; }
                let s = r_i32(&fo_buf[s_off..], is_le).max(0) as usize;
                let e = r_i32(&fo_buf[e_off..], is_le).max(0) as usize;
                if e > s + 2 && e * 4 <= cv_buf.len() {
                    let v0 = r_i32(&cv_buf[s * 4..], is_le) as usize;
                    for k in 1..e - s - 1 {
                        let v1 = r_i32(&cv_buf[(s + k) * 4..], is_le) as usize;
                        let v2 = r_i32(&cv_buf[(s + k + 1) * 4..], is_le) as usize;
                        if v0 < vert_count && v1 < vert_count && v2 < vert_count {
                            out_tris.push([base_vert + v0 as u32, base_vert + v1 as u32, base_vert + v2 as u32]);
                            added_faces = true;
                        }
                    }
                }
            }
        }
    }

    if !added_faces {
        // Fallback edge/point wireframe preview from vertices
        for pair in mesh_verts.chunks_exact(2).take(40_000) {
            out_wire.extend_from_slice(&pair[0]);
            out_wire.extend_from_slice(&pair[1]);
        }
    }
}

fn find_mesh_int_layer<'a>(
    mesh_data: &[u8],
    dna: &Sdna,
    data_map: &HashMap<u64, &'a [u8]>,
    ptr_size: usize,
    is_le: bool,
    target_name: &str,
) -> Option<&'a [u8]> {
    // Try CustomData `corner_data`
    if let Some(cd_f) = dna.find_field("Mesh", "corner_data").or_else(|| dna.find_field("Mesh", "ldata")) {
        if let (Some(lf), Some(tf), Some(layer_st)) = (
            dna.find_field("CustomData", "layers"),
            dna.find_field("CustomData", "totlayer"),
            dna.struct_by_name("CustomDataLayer"),
        ) {
            let n = read_int(mesh_data, cd_f.offset + tf.offset, tf.size, is_le).unwrap_or(0).max(0) as usize;
            let ptr = read_ptr(mesh_data, cd_f.offset + lf.offset, ptr_size, is_le);
            if let Some(lbuf) = data_map.get(&ptr) {
                let nf = dna.find_field("CustomDataLayer", "name")?;
                let df = dna.find_field("CustomDataLayer", "data")?;
                for i in 0..n.min(64) {
                    let base = i * layer_st.size;
                    if base + layer_st.size > lbuf.len() { break; }
                    let nm = read_cstr(&lbuf[base + nf.offset..base + nf.offset + nf.size.min(64)]);
                    if nm == target_name {
                        let dptr = read_ptr(lbuf, base + df.offset, ptr_size, is_le);
                        return data_map.get(&dptr).copied();
                    }
                }
            }
        }
    }
    // Try Blender 5.0+ AttributeStorage
    if let Some(attr_f) = dna.find_field("Mesh", "attribute_storage") {
        let daf = dna.find_field("AttributeStorage", "dna_attributes")?;
        let dnf = dna.find_field("AttributeStorage", "dna_attributes_num")?;
        let nattrs = read_int(mesh_data, attr_f.offset + dnf.offset, dnf.size, is_le).unwrap_or(0).max(0) as usize;
        let aptr = read_ptr(mesh_data, attr_f.offset + daf.offset, ptr_size, is_le);
        if let (Some(abuf), Some(attr_st)) = (data_map.get(&aptr), dna.struct_by_name("Attribute")) {
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
        }
    }
    None
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
    let _ = &structs.first().map(|s| s.fields.first().map(|f| f.type_idx));
    Some(Sdna { types, type_lens, structs, by_name })
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

/// Converts a `.blend` parse into a Mesh3d (if geometry exists) or Structured scene inspector.
pub fn blend_to_viewer(
    label: String,
    verts: Vec<[f32; 3]>,
    tris: Vec<[u32; 3]>,
    wire: Vec<f32>,
    parts: Vec<String>,
    tree_nodes: Vec<TreeNode>,
    summary: String,
    mesh_builder: impl FnOnce(String, Vec<f32>, Vec<f32>, Vec<f32>, Vec<String>, Option<String>) -> ViewerPayload,
) -> ViewerPayload {
    if !tris.is_empty() || !wire.is_empty() {
        let mut pos = Vec::with_capacity(tris.len() * 9);
        for tri in &tris {
            for &idx in tri {
                if let Some(v) = verts.get(idx as usize) {
                    pos.extend_from_slice(v);
                }
            }
        }
        return mesh_builder(label, pos, Vec::new(), wire, parts, Some(summary));
    }
    ViewerPayload::Structured {
        format_label: label,
        formatted_text: summary,
        tree_nodes,
    }
}
