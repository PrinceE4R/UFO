use std::fmt::Write as _;

use crate::decoders::text::decode_to_utf8;
use crate::model::{MetadataEntry, TableSheet, TreeNode, ViewerPayload};

/// Decodes pure-Rust SQLite 3 databases, SafeTensors, GGUF LLM models, NumPy .npy/.npz, GeoJSON/GPX/KML, Parquet/HDF5.
pub fn decode_data_or_scientific(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    match format_id {
        "sqlite" | "sqlite3" | "db3" | "db" | "aup3" if bytes.starts_with(b"SQLite format 3\0") => {
            decode_sqlite3(bytes, metadata)
        }
        "safetensors" => decode_safetensors(bytes, metadata),
        "gguf" => decode_gguf(bytes, metadata),
        "npy" => decode_npy(bytes, metadata),
        "geojson" => decode_geojson_map(bytes, metadata),
        "gpx" | "kml" => decode_gpx_kml_map(bytes, format_id, metadata),
        "parquet" => decode_parquet_header(bytes, metadata),
        _ => None,
    }
}

/// Pure-Rust SQLite 3 B-tree database reader (reads schema + table rows with zero C FFI).
fn decode_sqlite3(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 100 || !bytes.starts_with(b"SQLite format 3\0") {
        return None;
    }
    let raw_ps = u16::from_be_bytes([bytes[16], bytes[17]]);
    let page_size = if raw_ps == 1 { 65536usize } else { (raw_ps as usize).max(512) };
    let text_enc = u32::from_be_bytes([bytes[56], bytes[57], bytes[58], bytes[59]]);
    let schema_rows = read_btree_table_rows(bytes, page_size, 1, 200);

    let mut tables = Vec::new();
    let mut sheets = Vec::new();
    for row in &schema_rows {
        let obj_type = row.first().map(String::as_str).unwrap_or("");
        let name = row.get(1).map(String::as_str).unwrap_or("");
        let root_page = row.get(3).and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
        let sql = row.get(4).map(String::as_str).unwrap_or("");
        if obj_type == "table" && !name.starts_with("sqlite_") && root_page >= 1 {
            let cols = parse_create_table_columns(sql);
            tables.push((name.to_string(), root_page, cols));
        }
    }

    metadata.push(MetadataEntry { section: "SQLite 3".into(), key: "Page Size".into(), value: format!("{page_size} bytes") });
    metadata.push(MetadataEntry { section: "SQLite 3".into(), key: "Text Encoding".into(), value: match text_enc { 1 => "UTF-8", 2 => "UTF-16LE", 3 => "UTF-16BE", _ => "UTF-8" }.into() });
    metadata.push(MetadataEntry { section: "SQLite 3".into(), key: "User Tables".into(), value: tables.len().to_string() });

    for (tname, root_page, col_names) in tables.into_iter().take(16) {
        let rows = read_btree_table_rows(bytes, page_size, root_page, 500);
        let width = col_names
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(1));
        let columns: Vec<String> = (0..width)
            .map(|i| col_names.get(i).cloned().unwrap_or_else(|| format!("col_{i}")))
            .collect();
        sheets.push(TableSheet {
            name: tname,
            columns,
            total_rows: rows.len(),
            truncated: rows.len() >= 500,
            rows,
        });
    }

    if sheets.is_empty() {
        sheets.push(TableSheet {
            name: "sqlite_schema".into(),
            columns: vec!["type".into(), "name".into(), "tbl_name".into(), "rootpage".into(), "sql".into()],
            total_rows: schema_rows.len(),
            truncated: false,
            rows: schema_rows,
        });
    }

    Some(ViewerPayload::Table {
        format_label: "SQLite 3 Database".into(),
        sheets,
        active_sheet: 0,
    })
}

fn read_btree_table_rows(
    bytes: &[u8],
    page_size: usize,
    page_no: usize,
    limit: usize,
) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut stack = vec![page_no];
    let mut visited = 0usize;
    while let Some(pno) = stack.pop() {
        if rows.len() >= limit || visited > 512 || pno == 0 {
            break;
        }
        visited += 1;
        let page_off = (pno - 1).saturating_mul(page_size);
        let Some(page) = bytes.get(page_off..page_off.saturating_add(page_size).min(bytes.len())) else {
            continue;
        };
        let hdr_off = if pno == 1 { 100 } else { 0 };
        if hdr_off + 8 > page.len() {
            continue;
        }
        let page_type = page[hdr_off];
        let cell_count = u16::from_be_bytes([page[hdr_off + 3], page[hdr_off + 4]]) as usize;
        let ptr_start = hdr_off + if page_type == 0x05 { 12 } else { 8 };

        if page_type == 0x05 {
            // Interior table B-tree page
            if let Some(right_child) = page.get(hdr_off + 8..hdr_off + 12) {
                let rc = u32::from_be_bytes([right_child[0], right_child[1], right_child[2], right_child[3]]) as usize;
                stack.push(rc);
            }
            for i in (0..cell_count).rev() {
                let po = ptr_start + i * 2;
                let Some(ptr_bytes) = page.get(po..po + 2) else { continue };
                let coff = u16::from_be_bytes([ptr_bytes[0], ptr_bytes[1]]) as usize;
                if let Some(child_b) = page.get(coff..coff + 4) {
                    let child = u32::from_be_bytes([child_b[0], child_b[1], child_b[2], child_b[3]]) as usize;
                    stack.push(child);
                }
            }
        } else if page_type == 0x0D {
            // Leaf table B-tree page
            for i in 0..cell_count {
                if rows.len() >= limit { break; }
                let po = ptr_start + i * 2;
                let Some(ptr_bytes) = page.get(po..po + 2) else { continue };
                let coff = u16::from_be_bytes([ptr_bytes[0], ptr_bytes[1]]) as usize;
                if coff < page.len() {
                    if let Some(rec) = parse_sqlite_leaf_cell(&page[coff..]) {
                        rows.push(rec);
                    }
                }
            }
        }
    }
    rows
}

fn parse_sqlite_leaf_cell(cell: &[u8]) -> Option<Vec<String>> {
    let (_payload_len, p1) = read_sqlite_varint(cell, 0)?;
    let (_rowid, p2) = read_sqlite_varint(cell, p1)?;
    let rec = cell.get(p2..)?;
    let (hdr_len, mut hp) = read_sqlite_varint(rec, 0)?;
    let hdr_end = (hdr_len as usize).min(rec.len());
    let mut serial_types = Vec::new();
    while hp < hdr_end {
        let (st, next) = read_sqlite_varint(rec, hp)?;
        serial_types.push(st);
        hp = next;
    }
    let mut dp = hdr_end;
    let mut out = Vec::with_capacity(serial_types.len());
    for st in serial_types {
        let (val, len) = match st {
            0 => ("NULL".into(), 0),
            1 => (rec.get(dp).map(|&b| (b as i8).to_string()).unwrap_or_default(), 1),
            2 => (rec.get(dp..dp + 2).map(|s| i16::from_be_bytes([s[0], s[1]]).to_string()).unwrap_or_default(), 2),
            3 => (rec.get(dp..dp + 3).map(|s| {
                let sign = if s[0] & 0x80 != 0 { 0xFF } else { 0 };
                i32::from_be_bytes([sign, s[0], s[1], s[2]]).to_string()
            }).unwrap_or_default(), 3),
            4 => (rec.get(dp..dp + 4).map(|s| i32::from_be_bytes([s[0], s[1], s[2], s[3]]).to_string()).unwrap_or_default(), 4),
            5 => (rec.get(dp..dp + 6).map(|s| {
                let sign = if s[0] & 0x80 != 0 { 0xFF } else { 0 };
                i64::from_be_bytes([sign, sign, s[0], s[1], s[2], s[3], s[4], s[5]]).to_string()
            }).unwrap_or_default(), 6),
            6 => (rec.get(dp..dp + 8).map(|s| i64::from_be_bytes(s.try_into().unwrap_or([0; 8])).to_string()).unwrap_or_default(), 8),
            7 => (rec.get(dp..dp + 8).map(|s| format!("{:.4}", f64::from_be_bytes(s.try_into().unwrap_or([0; 8])))).unwrap_or_default(), 8),
            8 => ("0".into(), 0),
            9 => ("1".into(), 0),
            s if s >= 12 && s % 2 == 0 => {
                let len = ((s - 12) / 2) as usize;
                (format!("<blob {len} B>"), len)
            }
            s if s >= 13 && s % 2 == 1 => {
                let len = ((s - 13) / 2) as usize;
                let end = (dp + len).min(rec.len());
                let txt = String::from_utf8_lossy(&rec[dp..end]).to_string();
                (txt, len)
            }
            _ => (String::new(), 0),
        };
        dp = dp.saturating_add(len);
        out.push(val);
    }
    Some(out)
}

fn read_sqlite_varint(b: &[u8], pos: usize) -> Option<(u64, usize)> {
    let mut val = 0u64;
    for i in 0..9 {
        let byte = *b.get(pos + i)?;
        if i == 8 {
            val = (val << 8) | byte as u64;
            return Some((val, pos + 9));
        }
        val = (val << 7) | (byte & 0x7F) as u64;
        if byte & 0x80 == 0 {
            return Some((val, pos + i + 1));
        }
    }
    Some((val, pos + 9))
}

fn parse_create_table_columns(sql: &str) -> Vec<String> {
    let Some(open) = sql.find('(') else { return Vec::new() };
    let Some(close) = sql.rfind(')') else { return Vec::new() };
    if close <= open { return Vec::new(); }
    let inner = &sql[open + 1..close];
    let mut cols = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in inner.chars() {
        match ch {
            '(' => { depth += 1; cur.push(ch); }
            ')' => { depth -= 1; cur.push(ch); }
            ',' if depth == 0 => {
                if let Some(name) = extract_col_name(&cur) {
                    cols.push(name);
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if let Some(name) = extract_col_name(&cur) {
        cols.push(name);
    }
    cols
}

fn extract_col_name(def: &str) -> Option<String> {
    let trimmed = def.trim();
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("PRIMARY KEY")
        || upper.starts_with("FOREIGN KEY")
        || upper.starts_with("UNIQUE")
        || upper.starts_with("CHECK")
        || upper.starts_with("CONSTRAINT")
    {
        return None;
    }
    let first = trimmed.split_whitespace().next()?;
    Some(first.trim_matches(|c| matches!(c, '"' | '`' | '[' | ']')).to_string())
}

fn decode_safetensors(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 8 { return None; }
    let hdr_len = u64::from_le_bytes(bytes[0..8].try_into().ok()?) as usize;
    if hdr_len == 0 || 8 + hdr_len > bytes.len() || hdr_len > 16 * 1024 * 1024 {
        return None;
    }
    let json_str = std::str::from_utf8(&bytes[8..8 + hdr_len]).ok()?;
    let val: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let obj = val.as_object()?;

    let mut rows = Vec::new();
    for (k, v) in obj {
        if k == "__metadata__" { continue; }
        let dtype = v.get("dtype").and_then(|d| d.as_str()).unwrap_or("-");
        let shape = v.get("shape").map(|s| s.to_string()).unwrap_or_default();
        let offsets = v.get("data_offsets").map(|s| s.to_string()).unwrap_or_default();
        rows.push(vec![k.clone(), dtype.into(), shape, offsets]);
    }
    metadata.push(MetadataEntry { section: "SafeTensors".into(), key: "Tensors".into(), value: rows.len().to_string() });
    Some(ViewerPayload::Table {
        format_label: "SafeTensors ML Weights".into(),
        sheets: vec![TableSheet {
            name: "tensors".into(),
            columns: vec!["Tensor Name".into(), "DType".into(), "Shape".into(), "Offsets".into()],
            total_rows: rows.len(),
            truncated: false,
            rows,
        }],
        active_sheet: 0,
    })
}

fn decode_gguf(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 24 || &bytes[0..4] != b"GGUF" { return None; }
    let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let tensor_count = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    let kv_count = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
    metadata.push(MetadataEntry { section: "GGUF".into(), key: "Version".into(), value: version.to_string() });
    metadata.push(MetadataEntry { section: "GGUF".into(), key: "Tensors".into(), value: tensor_count.to_string() });
    metadata.push(MetadataEntry { section: "GGUF".into(), key: "Metadata KV Pairs".into(), value: kv_count.to_string() });

    let nodes = vec![
        TreeNode { key: "Format".into(), value_preview: format!("GGUF v{version}"), kind: "string".into(), depth: 0 },
        TreeNode { key: "Tensor Count".into(), value_preview: tensor_count.to_string(), kind: "number".into(), depth: 0 },
        TreeNode { key: "Metadata KV Count".into(), value_preview: kv_count.to_string(), kind: "number".into(), depth: 0 },
    ];
    Some(ViewerPayload::Structured {
        format_label: format!("GGUF LLM Model v{version}"),
        formatted_text: format!("GGUF v{version}\nTensors: {tensor_count}\nMetadata KV pairs: {kv_count}\nTotal Size: {} bytes", bytes.len()),
        tree_nodes: nodes,
    })
}

fn decode_npy(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 12 || &bytes[0..6] != b"\x93NUMPY" { return None; }
    let major = bytes[6];
    let minor = bytes[7];
    let (hdr_len, hdr_start) = if major == 1 {
        (u16::from_le_bytes([bytes[8], bytes[9]]) as usize, 10usize)
    } else {
        (u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize, 12usize)
    };
    let hdr = String::from_utf8_lossy(bytes.get(hdr_start..hdr_start + hdr_len)?).trim().to_string();
    metadata.push(MetadataEntry { section: "NumPy".into(), key: "NPY Version".into(), value: format!("{major}.{minor}") });
    metadata.push(MetadataEntry { section: "NumPy".into(), key: "Descriptor".into(), value: hdr.clone() });
    Some(ViewerPayload::Structured {
        format_label: format!("NumPy Array (.npy v{major}.{minor})"),
        formatted_text: format!("Header: {hdr}\nPayload bytes: {}", bytes.len().saturating_sub(hdr_start + hdr_len)),
        tree_nodes: vec![TreeNode { key: "dtype & shape".into(), value_preview: hdr, kind: "string".into(), depth: 0 }],
    })
}

fn decode_parquet_header(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let valid_footer = bytes.len() >= 12 && &bytes[bytes.len() - 4..] == b"PAR1";
    metadata.push(MetadataEntry { section: "Parquet".into(), key: "Valid Footer PAR1".into(), value: valid_footer.to_string() });
    let footer_len = if valid_footer {
        u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().ok()?)
    } else { 0 };
    metadata.push(MetadataEntry { section: "Parquet".into(), key: "Thrift Footer Size".into(), value: format!("{footer_len} bytes") });
    None
}

fn decode_geojson_map(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    let val: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut coords: Vec<(f64, f64)> = Vec::new();
    let mut polylines: Vec<Vec<(f64, f64)>> = Vec::new();
    collect_geojson_coords(&val, &mut coords, &mut polylines);
    if coords.is_empty() && polylines.is_empty() {
        return None;
    }
    Some(render_geo_svg("GeoJSON Vector Map", &coords, &polylines, metadata))
}

fn collect_geojson_coords(
    v: &serde_json::Value,
    pts: &mut Vec<(f64, f64)>,
    lines: &mut Vec<Vec<(f64, f64)>>,
) {
    if let Some(obj) = v.as_object() {
        if let Some(c) = obj.get("coordinates") {
            extract_coord_arrays(c, pts, lines);
        }
        for val in obj.values() {
            collect_geojson_coords(val, pts, lines);
        }
    } else if let Some(arr) = v.as_array() {
        for item in arr {
            collect_geojson_coords(item, pts, lines);
        }
    }
}

fn extract_coord_arrays(
    c: &serde_json::Value,
    pts: &mut Vec<(f64, f64)>,
    lines: &mut Vec<Vec<(f64, f64)>>,
) {
    let Some(arr) = c.as_array() else { return };
    if arr.len() >= 2 && arr[0].is_f64() && arr[1].is_f64() {
        if let (Some(x), Some(y)) = (arr[0].as_f64(), arr[1].as_f64()) {
            pts.push((x, -y));
        }
    } else if arr.first().and_then(|x| x.as_array()).is_some_and(|a| a.first().is_some_and(|v| v.is_f64())) {
        let mut line = Vec::new();
        for p in arr {
            if let Some(pa) = p.as_array() {
                if let (Some(x), Some(y)) = (pa.first().and_then(|v| v.as_f64()), pa.get(1).and_then(|v| v.as_f64())) {
                    line.push((x, -y));
                }
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    } else {
        for sub in arr {
            extract_coord_arrays(sub, pts, lines);
        }
    }
}

fn decode_gpx_kml_map(bytes: &[u8], format_id: &str, metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    let doc = roxmltree::Document::parse(&text).ok()?;
    let mut track = Vec::new();
    let mut pts = Vec::new();
    if format_id == "gpx" {
        for node in doc.descendants() {
            if node.has_tag_name("trkpt") || node.has_tag_name("wpt") {
                if let (Some(lat), Some(lon)) = (
                    node.attribute("lat").and_then(|s| s.parse::<f64>().ok()),
                    node.attribute("lon").and_then(|s| s.parse::<f64>().ok()),
                ) {
                    if node.has_tag_name("trkpt") {
                        track.push((lon, -lat));
                    } else {
                        pts.push((lon, -lat));
                    }
                }
            }
        }
    } else {
        for node in doc.descendants().filter(|n| n.has_tag_name("coordinates")) {
            if let Some(raw) = node.text() {
                let mut line = Vec::new();
                for tok in raw.split_whitespace() {
                    let parts: Vec<f64> = tok.split(',').filter_map(|s| s.parse().ok()).collect();
                    if parts.len() >= 2 {
                        line.push((parts[0], -parts[1]));
                    }
                }
                if line.len() == 1 {
                    pts.push(line[0]);
                } else if !line.is_empty() {
                    return Some(render_geo_svg("KML Geographic Track", &pts, &[line], metadata));
                }
            }
        }
    }
    if track.is_empty() && pts.is_empty() {
        return None;
    }
    let track_lines = if track.is_empty() { Vec::new() } else { vec![track] };
    Some(render_geo_svg("GPX GPS Track", &pts, &track_lines, metadata))
}

fn render_geo_svg(
    label: &str,
    pts: &[(f64, f64)],
    lines: &[Vec<(f64, f64)>],
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for &(x, y) in pts.iter().chain(lines.iter().flatten()) {
        minx = minx.min(x); maxx = maxx.max(x);
        miny = miny.min(y); maxy = maxy.max(y);
    }
    let w = (maxx - minx).max(0.001);
    let h = (maxy - miny).max(0.001);
    let pad = w.max(h) * 0.08;
    let sw = (w.max(h) * 0.005).max(0.0001);

    let mut body = String::new();
    for line in lines {
        let path_pts: String = line.iter().map(|(x, y)| format!("{x:.6},{y:.6}")).collect::<Vec<_>>().join(" ");
        let _ = write!(body, "<polyline points=\"{path_pts}\" fill=\"none\" stroke=\"#EAFFFF\" stroke-width=\"{sw:.6}\"/>");
    }
    for (x, y) in pts.iter().take(2000) {
        let _ = write!(body, "<circle cx=\"{x:.6}\" cy=\"{y:.6}\" r=\"{:.6}\" fill=\"#EAFFFF\"/>", sw * 2.0);
    }
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{:.6} {:.6} {:.6} {:.6}\">{body}</svg>",
        minx - pad, miny - pad, w + pad * 2.0, h + pad * 2.0
    );
    let count = pts.len() + lines.iter().map(Vec::len).sum::<usize>();
    metadata.push(MetadataEntry { section: "Geo".into(), key: "Coordinates".into(), value: count.to_string() });
    ViewerPayload::Vector2d {
        format_label: label.into(),
        width: w,
        height: h,
        layers: vec!["Vector Geometry".into()],
        svg_markup: svg,
        entity_count: count,
    }
}
