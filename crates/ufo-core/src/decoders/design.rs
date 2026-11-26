use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{Cursor, Read as _};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use image::{codecs::png::PngEncoder, ColorType, ImageEncoder as _};
use ruzstd::decoding::StreamingDecoder as ZstdDecoder;
use serde_json::Value;

use crate::model::{DesignLayer, DesignPage, MetadataEntry, ViewerPayload};

/// Maximum canvas preview size when stitching tiled layers (prevents OOM on huge canvases).
const MAX_LAYER_IMG_DIM: usize = 4096;

/// Decodes layered design documents & multi-page projects into `ViewerPayload::LayeredDesign`.
pub fn decode_design(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    match format_id {
        "psd" | "psb" => decode_psd_layered(bytes, metadata),
        "fig" => decode_fig_layered(bytes, metadata),
        "figcraft" => decode_figcraft(bytes, metadata),
        "vectorcraft" => decode_vectorcraft(bytes, metadata),
        "pcraft" | "photocraft" => decode_photocraft(bytes, metadata),
        "ora" | "kra" => decode_ora_or_kra(bytes, format_id, metadata),
        _ => None,
    }
}

fn encode_rgba_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || rgba.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    let mut out = Vec::new();
    let enc = PngEncoder::new(&mut out);
    enc.write_image(rgba, w, h, ColorType::Rgba8.into()).ok()?;
    Some(out)
}

fn rgba_to_data_url(w: u32, h: u32, rgba: &[u8]) -> Option<String> {
    let png = encode_rgba_png(w, h, rgba)?;
    Some(format!("data:image/png;base64,{}", B64.encode(&png)))
}

fn esc_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Adobe Photoshop PSD / PSB (Layers + Masks + Merged Composite)
// ─────────────────────────────────────────────────────────────────────────────

fn decode_psd_layered(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 26 || &bytes[0..4] != b"8BPS" {
        return None;
    }
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    let channels = u16::from_be_bytes([bytes[12], bytes[13]]) as usize;
    let height = u32::from_be_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    let width = u32::from_be_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]);
    let depth = u16::from_be_bytes([bytes[22], bytes[23]]);
    let color_mode = u16::from_be_bytes([bytes[24], bytes[25]]);

    if width == 0 || height == 0 || width > 30_000 || height > 30_000 {
        return None;
    }

    metadata.push(MetadataEntry { section: "Photoshop".into(), key: "Version / Canvas".into(), value: format!("{} · {width} × {height} px", if version == 2 { "PSB" } else { "PSD" }) });
    metadata.push(MetadataEntry { section: "Photoshop".into(), key: "Channels / Depth".into(), value: format!("{channels} ch · {depth}-bit") });

    let mut pos = 26usize;
    // Section 1: Color Mode Data
    if pos + 4 > bytes.len() { return None; }
    let color_len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().ok()?) as usize;
    pos = pos.saturating_add(4 + color_len);
    // Section 2: Image Resources
    if pos + 4 > bytes.len() { return None; }
    let res_len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().ok()?) as usize;
    pos = pos.saturating_add(4 + res_len);

    // Section 3: Layer and Mask Information
    let (lm_len, is_psb) = if version == 2 {
        if pos + 8 > bytes.len() { return None; }
        let l = u64::from_be_bytes(bytes[pos..pos + 8].try_into().ok()?) as usize;
        pos += 8;
        (l, true)
    } else {
        if pos + 4 > bytes.len() { return None; }
        let l = u32::from_be_bytes(bytes[pos..pos + 4].try_into().ok()?) as usize;
        pos += 4;
        (l, false)
    };
    let lm_start = pos;
    let lm_end = lm_start.saturating_add(lm_len).min(bytes.len());

    let mut design_layers = Vec::new();
    if lm_len > 4 && lm_start < lm_end && depth == 8 && matches!(color_mode, 1 | 3) {
        design_layers = parse_psd_layers(&bytes[lm_start..lm_end], is_psb, color_mode);
    }

    // Section 4: Merged Composite Image Data
    pos = lm_end;
    let composite_url = if depth == 8 && matches!(color_mode, 1 | 3) && width <= 16384 && height <= 16384 {
        decode_psd_composite(bytes, pos, version, channels, width, height, color_mode)
    } else {
        None
    };

    if design_layers.is_empty() {
        if let Some(ref url) = composite_url {
            design_layers.push(DesignLayer {
                id: "psd_composite".into(),
                name: "Merged Composite".into(),
                kind: "Raster".into(),
                depth: 0,
                visible: true,
                opacity: 1.0,
                blend_mode: "normal".into(),
                x: 0.0,
                y: 0.0,
                width: width as f64,
                height: height as f64,
                svg_fragment: None,
                image_data_url: Some(url.clone()),
            });
        }
    }

    if composite_url.is_none() && design_layers.is_empty() {
        return None;
    }

    metadata.push(MetadataEntry { section: "Photoshop".into(), key: "Extracted Layers".into(), value: design_layers.len().to_string() });

    let label = if version == 2 {
        "Adobe Photoshop Large Document (.psb)"
    } else {
        "Adobe Photoshop Document (.psd)"
    };
    let page = DesignPage {
        id: "psd_canvas".into(),
        name: format!("Canvas ({width} × {height})"),
        width: width as f64,
        height: height as f64,
        background: None,
        composite_data_url: composite_url,
        layers: design_layers,
    };

    Some(ViewerPayload::LayeredDesign {
        format_label: label.into(),
        pages: vec![page],
        active_page: 0,
        note: Some("Toggle layer visibility or hover layers/page to inspect composite & individual layers".into()),
    })
}

fn parse_psd_layers(sec: &[u8], is_psb: bool, color_mode: u16) -> Vec<DesignLayer> {
    let mut pos = 0usize;
    let layer_info_len = if is_psb {
        if pos + 8 > sec.len() { return Vec::new(); }
        let v = u64::from_be_bytes(sec[pos..pos + 8].try_into().unwrap_or([0; 8])) as usize;
        pos += 8;
        v
    } else {
        if pos + 4 > sec.len() { return Vec::new(); }
        let v = u32::from_be_bytes(sec[pos..pos + 4].try_into().unwrap_or([0; 4])) as usize;
        pos += 4;
        v
    };
    if layer_info_len == 0 || pos + 2 > sec.len() {
        return Vec::new();
    }
    let layer_info_end = pos.saturating_add(layer_info_len).min(sec.len());
    let raw_count = i16::from_be_bytes([sec[pos], sec[pos + 1]]);
    pos += 2;
    let count = (raw_count.unsigned_abs() as usize).min(256);
    if count == 0 {
        return Vec::new();
    }

    struct RawLayer {
        top: i32,
        left: i32,
        bottom: i32,
        right: i32,
        channels: Vec<(i16, usize)>,
        blend: String,
        opacity: f32,
        visible: bool,
        name: String,
        section_type: Option<u32>,
    }

    let mut raw_layers = Vec::with_capacity(count);
    for idx in 0..count {
        if pos + 18 > layer_info_end { break; }
        let top = i32::from_be_bytes([sec[pos], sec[pos + 1], sec[pos + 2], sec[pos + 3]]);
        let left = i32::from_be_bytes([sec[pos + 4], sec[pos + 5], sec[pos + 6], sec[pos + 7]]);
        let bottom = i32::from_be_bytes([sec[pos + 8], sec[pos + 9], sec[pos + 10], sec[pos + 11]]);
        let right = i32::from_be_bytes([sec[pos + 12], sec[pos + 13], sec[pos + 14], sec[pos + 15]]);
        let num_ch = u16::from_be_bytes([sec[pos + 16], sec[pos + 17]]) as usize;
        pos += 18;

        let mut chs = Vec::with_capacity(num_ch);
        let ch_entry_sz = if is_psb { 10 } else { 6 };
        for _ in 0..num_ch {
            if pos + ch_entry_sz > layer_info_end { break; }
            let ch_id = i16::from_be_bytes([sec[pos], sec[pos + 1]]);
            let ch_len = if is_psb {
                u64::from_be_bytes(sec[pos + 2..pos + 10].try_into().unwrap_or([0; 8])) as usize
            } else {
                u32::from_be_bytes(sec[pos + 2..pos + 6].try_into().unwrap_or([0; 4])) as usize
            };
            pos += ch_entry_sz;
            chs.push((ch_id, ch_len));
        }

        if pos + 16 > layer_info_end { break; }
        // `8BIM` sig at pos..pos+4
        let blend_key = String::from_utf8_lossy(&sec[pos + 4..pos + 8]).trim().to_string();
        let opacity = sec[pos + 8] as f32 / 255.0;
        let _clipping = sec[pos + 9];
        let flags = sec[pos + 10];
        let visible = (flags & 0x02) == 0;
        let extra_len = u32::from_be_bytes(sec[pos + 12..pos + 16].try_into().unwrap_or([0; 4])) as usize;
        pos += 16;

        let extra_end = pos.saturating_add(extra_len).min(layer_info_end);
        let mut ep = pos;
        // Skip layer mask & blending ranges
        if ep + 4 <= extra_end {
            let mask_len = u32::from_be_bytes(sec[ep..ep + 4].try_into().unwrap_or([0; 4])) as usize;
            ep = ep.saturating_add(4 + mask_len).min(extra_end);
        }
        if ep + 4 <= extra_end {
            let ranges_len = u32::from_be_bytes(sec[ep..ep + 4].try_into().unwrap_or([0; 4])) as usize;
            ep = ep.saturating_add(4 + ranges_len).min(extra_end);
        }
        let mut name = format!("Layer {}", idx + 1);
        if ep < extra_end {
            let plen = sec[ep] as usize;
            let padded = (plen + 4) & !3;
            if ep + 1 + plen <= extra_end {
                let ascii_name = String::from_utf8_lossy(&sec[ep + 1..ep + 1 + plen]).trim().to_string();
                if !ascii_name.is_empty() {
                    name = ascii_name;
                }
            }
            ep = ep.saturating_add(padded).min(extra_end);
        }

        // Parse tagged additional layer info (`luni` Unicode name, `lsct` section divider)
        let mut section_type = None;
        while ep + 12 <= extra_end {
            let sig = &sec[ep..ep + 4];
            if sig != b"8BIM" && sig != b"8B64" {
                break;
            }
            let key = &sec[ep + 4..ep + 8];
            let long_len = is_psb
                && matches!(
                    key,
                    b"LMsk" | b"Lr16" | b"Lr32" | b"Layr" | b"Mt16" | b"Mt32" | b"Mtrn" | b"Alph" | b"FMsk" | b"lnk2" | b"FEid" | b"FXid" | b"PxSD"
                );
            ep += 8;
            let tlen = if long_len {
                if ep + 8 > extra_end { break; }
                let l = u64::from_be_bytes(sec[ep..ep + 8].try_into().unwrap_or([0; 8])) as usize;
                ep += 8;
                l
            } else {
                if ep + 4 > extra_end { break; }
                let l = u32::from_be_bytes(sec[ep..ep + 4].try_into().unwrap_or([0; 4])) as usize;
                ep += 4;
                l
            };
            let tdata_end = ep.saturating_add(tlen).min(extra_end);
            if key == b"luni" && ep + 4 <= tdata_end {
                let ulen = u32::from_be_bytes(sec[ep..ep + 4].try_into().unwrap_or([0; 4])) as usize;
                if ep + 4 + ulen * 2 <= tdata_end {
                    let units: Vec<u16> = (0..ulen)
                        .map(|k| u16::from_be_bytes([sec[ep + 4 + k * 2], sec[ep + 5 + k * 2]]))
                        .collect();
                    let uni = String::from_utf16_lossy(&units).trim_end_matches('\0').to_string();
                    if !uni.is_empty() {
                        name = uni;
                    }
                }
            } else if key == b"lsct" && ep + 4 <= tdata_end {
                section_type = Some(u32::from_be_bytes(sec[ep..ep + 4].try_into().unwrap_or([0; 4])));
            }
            ep = tdata_end;
        }

        pos = extra_end;
        raw_layers.push(RawLayer {
            top,
            left,
            bottom,
            right,
            channels: chs,
            blend: psd_blend_name(&blend_key),
            opacity,
            visible,
            name,
            section_type,
        });
    }

    // Channel Image Data follows immediately after layer records
    let mut layer_imgs: Vec<Option<String>> = Vec::with_capacity(raw_layers.len());
    for rl in &raw_layers {
        let lw = (rl.right - rl.left).max(0) as usize;
        let lh = (rl.bottom - rl.top).max(0) as usize;
        let valid_dims = lw > 0 && lh > 0 && lw <= 8192 && lh <= 8192;
        let plane_sz = lw * lh;
        let mut r_plane = None;
        let mut g_plane = None;
        let mut b_plane = None;
        let mut a_plane = None;

        for &(ch_id, ch_len) in &rl.channels {
            let cend = pos.saturating_add(ch_len).min(layer_info_end);
            let ch_slice = &sec[pos..cend];
            pos = cend;
            if !valid_dims || ch_slice.len() < 2 || !matches!(ch_id, -1 | 0 | 1 | 2) {
                continue;
            }
            if let Some(decoded) = decode_psd_channel_data(ch_slice, lw, lh, is_psb) {
                if decoded.len() == plane_sz {
                    match ch_id {
                        0 => r_plane = Some(decoded),
                        1 => g_plane = Some(decoded),
                        2 => b_plane = Some(decoded),
                        -1 => a_plane = Some(decoded),
                        _ => {}
                    }
                }
            }
        }

        let data_url = if valid_dims && (r_plane.is_some() || g_plane.is_some() || b_plane.is_some()) {
            let mut rgba = vec![255u8; plane_sz * 4];
            for i in 0..plane_sz {
                if color_mode == 1 {
                    let gray = r_plane.as_ref().map(|p| p[i]).unwrap_or(255);
                    rgba[i * 4] = gray;
                    rgba[i * 4 + 1] = gray;
                    rgba[i * 4 + 2] = gray;
                } else {
                    rgba[i * 4] = r_plane.as_ref().map(|p| p[i]).unwrap_or(0);
                    rgba[i * 4 + 1] = g_plane.as_ref().map(|p| p[i]).unwrap_or(0);
                    rgba[i * 4 + 2] = b_plane.as_ref().map(|p| p[i]).unwrap_or(0);
                }
                rgba[i * 4 + 3] = a_plane.as_ref().map(|p| p[i]).unwrap_or(255);
            }
            rgba_to_data_url(lw as u32, lh as u32, &rgba)
        } else {
            None
        };
        layer_imgs.push(data_url);
    }

    // PSD stores layers bottom-to-top with section closing divider (`section_type == 3`) before children.
    // Convert to clean top-to-bottom UI tree with hierarchical indentation depth.
    let mut out = Vec::new();
    let mut group_depth = 0usize;
    for idx in (0..raw_layers.len()).rev() {
        let rl = &raw_layers[idx];
        match rl.section_type {
            Some(3) => {
                group_depth = group_depth.saturating_sub(1);
                continue;
            }
            Some(1) | Some(2) => {
                out.push(DesignLayer {
                    id: format!("psd_layer_{idx}"),
                    name: rl.name.clone(),
                    kind: "Group".into(),
                    depth: group_depth,
                    visible: rl.visible,
                    opacity: rl.opacity,
                    blend_mode: rl.blend.clone(),
                    x: rl.left as f64,
                    y: rl.top as f64,
                    width: (rl.right - rl.left).max(0) as f64,
                    height: (rl.bottom - rl.top).max(0) as f64,
                    svg_fragment: None,
                    image_data_url: None,
                });
                group_depth += 1;
                continue;
            }
            _ => {}
        }
        out.push(DesignLayer {
            id: format!("psd_layer_{idx}"),
            name: rl.name.clone(),
            kind: "Raster".into(),
            depth: group_depth,
            visible: rl.visible,
            opacity: rl.opacity,
            blend_mode: rl.blend.clone(),
            x: rl.left as f64,
            y: rl.top as f64,
            width: (rl.right - rl.left).max(0) as f64,
            height: (rl.bottom - rl.top).max(0) as f64,
            svg_fragment: None,
            image_data_url: layer_imgs[idx].clone(),
        });
    }
    out
}

fn decode_psd_channel_data(data: &[u8], w: usize, h: usize, is_psb: bool) -> Option<Vec<u8>> {
    if data.len() < 2 {
        return None;
    }
    let comp = u16::from_be_bytes([data[0], data[1]]);
    let plane_sz = w * h;
    let body = &data[2..];
    match comp {
        0 => {
            if body.len() >= plane_sz {
                Some(body[..plane_sz].to_vec())
            } else {
                let mut v = vec![0u8; plane_sz];
                let copy = body.len().min(plane_sz);
                v[..copy].copy_from_slice(&body[..copy]);
                Some(v)
            }
        }
        1 => {
            let count_sz = if is_psb { 4 } else { 2 };
            let mut p = h.saturating_mul(count_sz);
            if p > body.len() {
                p = 0;
            }
            Some(decode_packbits(body, p, plane_sz))
        }
        2 => {
            let mut dec = ZlibDecoder::new(body);
            let mut out = Vec::with_capacity(plane_sz);
            dec.read_to_end(&mut out).ok()?;
            Some(out)
        }
        3 => {
            let mut dec = ZlibDecoder::new(body);
            let mut out = Vec::with_capacity(plane_sz);
            dec.read_to_end(&mut out).ok()?;
            for row in out.chunks_exact_mut(w) {
                for x in 1..w {
                    row[x] = row[x].wrapping_add(row[x - 1]);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn decode_packbits(body: &[u8], start: usize, expected: usize) -> Vec<u8> {
    let mut pos = start;
    let mut out = Vec::with_capacity(expected);
    while out.len() < expected && pos < body.len() {
        let n = body[pos] as i8;
        pos += 1;
        if n >= 0 {
            let cnt = (n as usize) + 1;
            let take = cnt.min(body.len() - pos).min(expected - out.len());
            out.extend_from_slice(&body[pos..pos + take]);
            pos += cnt.min(body.len() - pos);
        } else if n != -128 {
            let cnt = ((-n) as usize) + 1;
            if pos < body.len() {
                let val = body[pos];
                pos += 1;
                for _ in 0..cnt.min(expected - out.len()) {
                    out.push(val);
                }
            }
        }
    }
    while out.len() < expected {
        out.push(0);
    }
    out
}

fn decode_psd_composite(
    bytes: &[u8],
    mut pos: usize,
    version: u16,
    channels: usize,
    width: u32,
    height: u32,
    color_mode: u16,
) -> Option<String> {
    if pos + 2 > bytes.len() {
        return None;
    }
    let compression = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]);
    pos += 2;
    let w = width as usize;
    let h = height as usize;
    let plane_size = w * h;
    let total_planes = channels.min(4);
    let mut planes = vec![vec![255u8; plane_size]; 4];

    if compression == 0 {
        for p in 0..total_planes {
            let end = pos + plane_size;
            if end > bytes.len() { break; }
            planes[p].copy_from_slice(&bytes[pos..end]);
            pos = end;
        }
    } else if compression == 1 {
        let count_entry = if version == 2 { 4 } else { 2 };
        let mut rle_pos = pos + h * channels * count_entry;
        for p in 0..channels {
            let mut out = Vec::with_capacity(plane_size);
            while out.len() < plane_size && rle_pos < bytes.len() {
                let n = bytes[rle_pos] as i8;
                rle_pos += 1;
                if n >= 0 {
                    let cnt = (n as usize) + 1;
                    let take = cnt.min(bytes.len() - rle_pos).min(plane_size - out.len());
                    out.extend_from_slice(&bytes[rle_pos..rle_pos + take]);
                    rle_pos += cnt.min(bytes.len() - rle_pos);
                } else if n != -128 {
                    let cnt = ((-n) as usize) + 1;
                    if rle_pos < bytes.len() {
                        let val = bytes[rle_pos];
                        rle_pos += 1;
                        for _ in 0..cnt.min(plane_size - out.len()) {
                            out.push(val);
                        }
                    }
                }
            }
            if p < 4 && out.len() == plane_size {
                planes[p] = out;
            }
        }
    } else {
        return None;
    }

    let mut rgba = vec![255u8; plane_size * 4];
    for i in 0..plane_size {
        if color_mode == 1 {
            let g = planes[0][i];
            rgba[i * 4] = g;
            rgba[i * 4 + 1] = g;
            rgba[i * 4 + 2] = g;
            rgba[i * 4 + 3] = if channels >= 2 { planes[1][i] } else { 255 };
        } else {
            rgba[i * 4] = planes[0][i];
            rgba[i * 4 + 1] = planes[1][i];
            rgba[i * 4 + 2] = planes[2][i];
            rgba[i * 4 + 3] = if channels >= 4 { planes[3][i] } else { 255 };
        }
    }
    rgba_to_data_url(width, height, &rgba)
}

fn psd_blend_name(k: &str) -> String {
    match k {
        "norm" => "normal",
        "mul" => "multiply",
        "scrn" => "screen",
        "over" => "overlay",
        "dark" => "darken",
        "lite" => "lighten",
        "diff" => "difference",
        "hLit" => "hard-light",
        "sLit" => "soft-light",
        "div" => "color-dodge",
        "idiv" => "color-burn",
        "pass" => "pass-through",
        other => other,
    }
    .to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Figma (.fig ZIP / fig-kiwi binary) & Xfig (#FIG)
// ─────────────────────────────────────────────────────────────────────────────

fn decode_fig_layered(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.starts_with(b"#FIG") {
        return decode_xfig_layered(bytes, metadata);
    }

    let mut images: HashMap<String, String> = HashMap::new();
    let mut thumb_url: Option<String> = None;
    let canvas_buf: Vec<u8>;
    let canvas: &[u8] = if bytes.starts_with(b"PK\x03\x04") {
        let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
        let mut found_canvas = Vec::new();
        for i in 0..arch.len() {
            let Ok(mut f) = arch.by_index(i) else { continue };
            let Ok(name) = f.name().map(|n| n.into_owned()) else { continue };
            if name == "canvas.fig" {
                f.read_to_end(&mut found_canvas).ok()?;
            } else if name.starts_with("images/") && f.size() > 0 && f.size() < 24 * 1024 * 1024 {
                let mut ibuf = Vec::new();
                if f.read_to_end(&mut ibuf).is_ok() {
                    let key = name.trim_start_matches("images/").to_string();
                    let mime = guess_image_mime(&ibuf);
                    images.insert(key, format!("data:{mime};base64,{}", B64.encode(&ibuf)));
                }
            } else if (name == "thumbnail.png" || name == "meta.json") && name == "thumbnail.png" {
                let mut tbuf = Vec::new();
                if f.read_to_end(&mut tbuf).is_ok() && !tbuf.is_empty() {
                    thumb_url = Some(format!("data:image/png;base64,{}", B64.encode(&tbuf)));
                }
            }
        }
        if found_canvas.is_empty() {
            return None;
        }
        canvas_buf = found_canvas;
        &canvas_buf
    } else {
        bytes
    };

    if !(canvas.starts_with(b"fig-kiwi")
        || canvas.starts_with(b"fig-jam.")
        || canvas.starts_with(b"fig-deck"))
        || canvas.len() < 16
    {
        return None;
    }

    let ver = u32::from_le_bytes([canvas[8], canvas[9], canvas[10], canvas[11]]);
    let mut chunks = Vec::new();
    let mut pos = 12usize;
    while pos + 4 <= canvas.len() {
        let sz = u32::from_le_bytes([canvas[pos], canvas[pos + 1], canvas[pos + 2], canvas[pos + 3]]) as usize;
        pos += 4;
        if sz == 0 || pos + sz > canvas.len() {
            break;
        }
        chunks.push(&canvas[pos..pos + sz]);
        pos += sz;
    }
    if chunks.len() < 2 {
        return None;
    }

    let schema_raw = decompress_kiwi_chunk(chunks[0])?;
    let msg_raw = decompress_kiwi_chunk(chunks[1])?;
    let schema = KiwiSchema::decode(&schema_raw)?;
    let root = schema.find_def("Message")?;
    let msg = schema.decode_message(root, &msg_raw, 400_000)?;

    let changes = msg.as_obj()?.get("nodeChanges")?.as_arr()?;
    metadata.push(MetadataEntry { section: "Figma (.fig)".into(), key: "Schema Version".into(), value: format!("fig-kiwi v{ver}") });
    metadata.push(MetadataEntry { section: "Figma (.fig)".into(), key: "Node Changes".into(), value: changes.len().to_string() });
    if !images.is_empty() {
        metadata.push(MetadataEntry { section: "Figma (.fig)".into(), key: "Embedded Images".into(), value: images.len().to_string() });
    }

    // Index all nodes by `guid = (sessionID, localID)`
    #[derive(Clone)]
    struct FigNode {
        guid: (i64, i64),
        parent: Option<(i64, i64)>,
        name: String,
        kind: String,
        visible: bool,
        opacity: f32,
        w: f64,
        h: f64,
        x: f64,
        y: f64,
        bg: Option<String>,
        fill: Option<String>,
        stroke: Option<String>,
        stroke_w: f64,
        corner_r: f64,
        text: Option<String>,
        image_ref: Option<String>,
    }

    let parse_guid = |v: &KiwiVal| -> Option<(i64, i64)> {
        let o = v.as_obj()?;
        let s = o.get("sessionID").and_then(KiwiVal::as_i64).unwrap_or(0);
        let l = o.get("localID").and_then(KiwiVal::as_i64)?;
        Some((s, l))
    };

    let mut nodes: HashMap<(i64, i64), FigNode> = HashMap::new();
    let mut children_map: HashMap<(i64, i64), Vec<(i64, i64)>> = HashMap::new();
    let mut page_ids = Vec::new();
    let mut order_list = Vec::new();

    for ch in changes {
        let Some(obj) = ch.as_obj() else { continue };
        let Some(guid) = obj.get("guid").and_then(parse_guid) else { continue };
        let parent = obj
            .get("parentIndex")
            .and_then(|pi| pi.as_obj())
            .and_then(|po| po.get("guid"))
            .and_then(parse_guid);
        let kind = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("FRAME")
            .to_string();
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&kind)
            .to_string();
        let visible = obj.get("visible").and_then(KiwiVal::as_bool).unwrap_or(true);
        let opacity = obj.get("opacity").and_then(KiwiVal::as_f64).unwrap_or(1.0) as f32;

        let (w, h) = obj
            .get("size")
            .and_then(KiwiVal::as_obj)
            .map(|s| {
                (
                    s.get("x").and_then(KiwiVal::as_f64).unwrap_or(0.0),
                    s.get("y").and_then(KiwiVal::as_f64).unwrap_or(0.0),
                )
            })
            .unwrap_or((0.0, 0.0));

        let tf = obj.get("transform").and_then(KiwiVal::as_obj);
        let gf = |k: &str, d: f64| tf.as_ref().and_then(|t| t.get(k)).and_then(KiwiVal::as_f64).unwrap_or(d);

        let mut fill = None;
        let mut image_ref = None;
        if let Some(paints) = obj.get("fillPaints").and_then(KiwiVal::as_arr) {
            for p in paints {
                if let Some(po) = p.as_obj() {
                    if po.get("visible").and_then(KiwiVal::as_bool) == Some(false) { continue; }
                    if let Some(c) = po.get("color").and_then(kiwi_color_css) {
                        fill = Some(c);
                    }
                    if let Some(img) = po.get("image").and_then(KiwiVal::as_obj) {
                        if let Some(KiwiVal::Bytes(b)) = img.get("hash") {
                            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                            if let Some(url) = images.get(&hex) {
                                image_ref = Some(url.clone());
                            }
                        }
                    }
                }
            }
        }
        let bg = obj.get("backgroundColor").and_then(kiwi_color_css).or_else(|| fill.clone());
        let stroke = obj.get("strokePaints").and_then(KiwiVal::as_arr).and_then(|ps| {
            ps.iter().find_map(|p| {
                let po = p.as_obj()?;
                if po.get("visible").and_then(KiwiVal::as_bool) == Some(false) { return None; }
                po.get("color").and_then(kiwi_color_css)
            })
        });
        let stroke_w = obj.get("strokeWeight").and_then(KiwiVal::as_f64).unwrap_or(1.0);
        let corner_r = obj.get("cornerRadius").and_then(KiwiVal::as_f64).unwrap_or(0.0);
        let text = obj
            .get("textData")
            .and_then(KiwiVal::as_obj)
            .and_then(|td| td.get("characters"))
            .and_then(KiwiVal::as_str)
            .map(str::to_string);

        if kind == "CANVAS" {
            page_ids.push(guid);
        }
        if let Some(p) = parent {
            children_map.entry(p).or_default().push(guid);
        }
        order_list.push(guid);
        nodes.insert(
            guid,
            FigNode {
                guid,
                parent,
                name,
                kind,
                visible,
                opacity,
                w,
                h,
                x: gf("m02", 0.0),
                y: gf("m12", 0.0),
                bg,
                fill,
                stroke,
                stroke_w,
                corner_r,
                text,
                image_ref,
            },
        );
    }

    if page_ids.is_empty() {
        // Group top-level frames if file has no explicit CANVAS node
        let roots: Vec<(i64, i64)> = order_list
            .iter()
            .copied()
            .filter(|id| nodes.get(id).and_then(|n| n.parent).is_none_or(|p| !nodes.contains_key(&p)))
            .collect();
        page_ids.extend(roots);
    }

    let mut pages = Vec::new();
    for (p_idx, &pg_id) in page_ids.iter().enumerate() {
        let Some(pg) = nodes.get(&pg_id) else { continue };
        let mut layers = Vec::new();

        #[allow(clippy::too_many_arguments)]
        fn walk_fig(
            id: (i64, i64),
            depth: usize,
            ox: f64,
            oy: f64,
            nodes: &HashMap<(i64, i64), FigNode>,
            children_map: &HashMap<(i64, i64), Vec<(i64, i64)>>,
            layers: &mut Vec<DesignLayer>,
            bounds: &mut (f64, f64, f64, f64),
            visited: &mut HashSet<(i64, i64)>,
        ) {
            if !visited.insert(id) || layers.len() >= 1500 {
                return;
            }
            let Some(n) = nodes.get(&id) else { return };
            let wx = ox + n.x;
            let wy = oy + n.y;
            let w = n.w.max(1.0);
            let h = n.h.max(1.0);
            if n.kind != "CANVAS" && n.kind != "DOCUMENT" {
                bounds.0 = bounds.0.min(wx);
                bounds.1 = bounds.1.min(wy);
                bounds.2 = bounds.2.max(wx + w);
                bounds.3 = bounds.3.max(wy + h);

                let fill_attr = n.fill.as_deref().unwrap_or(match n.kind.as_str() {
                    "FRAME" | "COMPONENT" | "SYMBOL" => "rgba(255,255,255,0.06)",
                    "GROUP" | "SECTION" => "none",
                    "TEXT" => "none",
                    _ => "rgba(234,255,255,0.22)",
                });
                let stroke_attr = n.stroke.as_deref().unwrap_or(match n.kind.as_str() {
                    "FRAME" | "COMPONENT" | "INSTANCE" => "rgba(234,255,255,0.45)",
                    "GROUP" | "SECTION" => "none",
                    _ => "rgba(234,255,255,0.35)",
                });

                let svg_frag = match n.kind.as_str() {
                    "ELLIPSE" => Some(format!(
                        "<ellipse cx=\"{:.2}\" cy=\"{:.2}\" rx=\"{:.2}\" ry=\"{:.2}\" fill=\"{fill_attr}\" stroke=\"{stroke_attr}\" stroke-width=\"{:.2}\"/>",
                        wx + w * 0.5, wy + h * 0.5, w * 0.5, h * 0.5, n.stroke_w
                    )),
                    "TEXT" => {
                        let txt = n.text.as_deref().unwrap_or(&n.name);
                        let fs = (h * 0.65).clamp(11.0, 48.0);
                        Some(format!(
                            "<text x=\"{wx:.2}\" y=\"{:.2}\" font-size=\"{fs:.1}\" fill=\"{}\" font-family=\"sans-serif\">{}</text>",
                            wy + fs,
                            n.fill.as_deref().unwrap_or("#eaffff"),
                            esc_xml(txt)
                        ))
                    }
                    "GROUP" | "SECTION" => None,
                    _ => {
                        let rx = n.corner_r.min(w * 0.5).min(h * 0.5);
                        Some(format!(
                            "<rect x=\"{wx:.2}\" y=\"{wy:.2}\" width=\"{w:.2}\" height=\"{h:.2}\" rx=\"{rx:.2}\" fill=\"{fill_attr}\" stroke=\"{stroke_attr}\" stroke-width=\"{:.2}\"/>",
                            n.stroke_w
                        ))
                    }
                };

                layers.push(DesignLayer {
                    id: format!("fig_{}_{}", n.guid.0, n.guid.1),
                    name: n.name.clone(),
                    kind: prettify_fig_kind(&n.kind),
                    depth,
                    visible: n.visible,
                    opacity: n.opacity,
                    blend_mode: "normal".into(),
                    x: wx,
                    y: wy,
                    width: w,
                    height: h,
                    svg_fragment: svg_frag,
                    image_data_url: n.image_ref.clone(),
                });
            }

            if let Some(kids) = children_map.get(&id) {
                let next_d = if n.kind == "CANVAS" || n.kind == "DOCUMENT" { 0 } else { depth + 1 };
                for &k in kids {
                    walk_fig(k, next_d, wx, wy, nodes, children_map, layers, bounds, visited);
                }
            }
        }

        let mut bounds = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut visited = HashSet::new();
        walk_fig(pg_id, 0, 0.0, 0.0, &nodes, &children_map, &mut layers, &mut bounds, &mut visited);

        let (min_x, min_y, max_x, max_y) = if bounds.0.is_finite() {
            bounds
        } else {
            (0.0, 0.0, 1200.0, 800.0)
        };
        // Normalize layer coordinates relative to page min corner so viewport centers neatly
        if min_x != 0.0 || min_y != 0.0 {
            for l in &mut layers {
                l.x -= min_x;
                l.y -= min_y;
            }
        }
        let pw = (max_x - min_x).max(64.0);
        let ph = (max_y - min_y).max(64.0);

        pages.push(DesignPage {
            id: format!("fig_page_{p_idx}"),
            name: pg.name.clone(),
            width: pw,
            height: ph,
            background: pg.bg.clone(),
            composite_data_url: if p_idx == 0 { thumb_url.clone() } else { None },
            layers,
        });
    }

    metadata.push(MetadataEntry { section: "Figma (.fig)".into(), key: "Pages".into(), value: pages.len().to_string() });

    Some(ViewerPayload::LayeredDesign {
        format_label: format!("Figma Design (.fig · kiwi v{ver})"),
        pages,
        active_page: 0,
        note: Some("Switch between individual Figma pages or inspect all pages and layer visibility toggles".into()),
    })
}

fn prettify_fig_kind(k: &str) -> String {
    match k {
        "CANVAS" => "Page",
        "FRAME" => "Frame",
        "GROUP" => "Group",
        "RECTANGLE" | "ROUNDED_RECTANGLE" => "Rectangle",
        "ELLIPSE" => "Ellipse",
        "VECTOR" => "Vector",
        "TEXT" => "Text",
        "COMPONENT" | "SYMBOL" => "Component",
        "INSTANCE" => "Instance",
        "LINE" => "Line",
        "STAR" => "Star",
        "REGULAR_POLYGON" => "Polygon",
        other => other,
    }
    .to_string()
}

fn kiwi_color_css(v: &KiwiVal) -> Option<String> {
    let o = v.as_obj()?;
    let r = (o.get("r").and_then(KiwiVal::as_f64).unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (o.get("g").and_then(KiwiVal::as_f64).unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (o.get("b").and_then(KiwiVal::as_f64).unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u8;
    let a = o.get("a").and_then(KiwiVal::as_f64).unwrap_or(1.0).clamp(0.0, 1.0);
    Some(format!("rgba({r},{g},{b},{a:.3})"))
}

fn decompress_kiwi_chunk(chunk: &[u8]) -> Option<Vec<u8>> {
    if chunk.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        let mut z = ZstdDecoder::new(Cursor::new(chunk)).ok()?;
        let mut out = Vec::new();
        z.read_to_end(&mut out).ok()?;
        return Some(out);
    }
    if chunk.starts_with(&[0x78, 0x9C]) || chunk.starts_with(&[0x78, 0x01]) || chunk.starts_with(&[0x78, 0xDA]) {
        let mut z = ZlibDecoder::new(chunk);
        let mut out = Vec::new();
        if z.read_to_end(&mut out).is_ok() && !out.is_empty() {
            return Some(out);
        }
    }
    let mut d = DeflateDecoder::new(chunk);
    let mut out = Vec::new();
    if d.read_to_end(&mut out).is_ok() && !out.is_empty() {
        return Some(out);
    }
    Some(chunk.to_vec())
}

// ── Minimal Pure-Rust Kiwi Binary Schema + Message Decoder ───────────────────

#[derive(Clone, Copy, PartialEq)]
enum KiwiDefKind {
    Enum,
    Struct,
    Message,
}

#[derive(Clone)]
struct KiwiField {
    name: String,
    type_id: i32,
    is_array: bool,
    tag: u32,
}

#[derive(Clone)]
struct KiwiDef {
    name: String,
    kind: KiwiDefKind,
    fields: Vec<KiwiField>,
}

struct KiwiSchema {
    defs: Vec<KiwiDef>,
}

#[derive(Clone)]
enum KiwiVal {
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f32),
    Str(String),
    Bytes(Vec<u8>),
    Arr(Vec<KiwiVal>),
    Obj(Vec<(String, KiwiVal)>),
}

impl KiwiVal {
    fn as_bool(&self) -> Option<bool> {
        match self { Self::Bool(b) => Some(*b), _ => None }
    }
    fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            Self::Uint(u) => Some(*u as i64),
            _ => None,
        }
    }
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(f) => Some(*f as f64),
            Self::Int(i) => Some(*i as f64),
            Self::Uint(u) => Some(*u as f64),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self { Self::Str(s) => Some(s.as_str()), _ => None }
    }
    fn as_arr(&self) -> Option<&[KiwiVal]> {
        match self { Self::Arr(a) => Some(a.as_slice()), _ => None }
    }
    fn as_obj(&self) -> Option<KiwiObjView<'_>> {
        match self { Self::Obj(fs) => Some(KiwiObjView(fs)), _ => None }
    }
}

struct KiwiObjView<'a>(&'a [(String, KiwiVal)]);
impl<'a> KiwiObjView<'a> {
    fn get(&self, k: &str) -> Option<&'a KiwiVal> {
        self.0.iter().find(|(n, _)| n == k).map(|(_, v)| v)
    }
}

struct ByteCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn read_byte(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn read_var_uint(&mut self) -> Option<u64> {
        let mut val = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.read_byte()?;
            val |= ((b & 0x7F) as u64) << shift;
            if (b & 0x80) == 0 {
                return Some(val);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }
    fn read_var_int(&mut self) -> Option<i64> {
        let u = self.read_var_uint()?;
        Some(if (u & 1) != 0 { !((u >> 1) as i64) } else { (u >> 1) as i64 })
    }
    fn read_var_float(&mut self) -> Option<f32> {
        let first = self.read_byte()?;
        if first == 0 {
            return Some(0.0);
        }
        let b1 = self.read_byte()? as u32;
        let b2 = self.read_byte()? as u32;
        let b3 = self.read_byte()? as u32;
        let bits = (first as u32) | (b1 << 8) | (b2 << 16) | (b3 << 24);
        Some(f32::from_bits((bits << 23) | (bits >> 9)))
    }
    fn read_cstring(&mut self) -> Option<String> {
        let start = self.pos;
        let rel_end = self.buf[start..].iter().position(|&b| b == 0)?;
        let s = String::from_utf8_lossy(&self.buf[start..start + rel_end]).to_string();
        self.pos = start + rel_end + 1;
        Some(s)
    }
    fn read_var_int_signed_schema(&mut self) -> Option<i32> {
        let u = self.read_var_uint()? as u32;
        Some(if (u & 1) != 0 { !((u >> 1) as i32) } else { (u >> 1) as i32 })
    }
}

impl KiwiSchema {
    fn decode(raw: &[u8]) -> Option<Self> {
        let mut cur = ByteCursor { buf: raw, pos: 0 };
        let def_count = cur.read_var_uint()? as usize;
        if def_count > 10_000 {
            return None;
        }
        let mut defs = Vec::with_capacity(def_count);
        for _ in 0..def_count {
            let name = cur.read_cstring()?;
            let kind_b = cur.read_byte()?;
            let kind = match kind_b {
                0 => KiwiDefKind::Enum,
                1 => KiwiDefKind::Struct,
                2 => KiwiDefKind::Message,
                _ => return None,
            };
            let fcount = cur.read_var_uint()? as usize;
            let mut fields = Vec::with_capacity(fcount.min(4096));
            for _ in 0..fcount {
                let fname = cur.read_cstring()?;
                let type_id = cur.read_var_int_signed_schema()?;
                let is_array = cur.read_byte()? != 0;
                let tag = cur.read_var_uint()? as u32;
                fields.push(KiwiField { name: fname, type_id, is_array, tag });
            }
            defs.push(KiwiDef { name, kind, fields });
        }
        Some(Self { defs })
    }

    fn find_def(&self, name: &str) -> Option<usize> {
        self.defs.iter().position(|d| d.name == name)
    }

    fn decode_message(&self, def_idx: usize, raw: &[u8], budget: usize) -> Option<KiwiVal> {
        let mut cur = ByteCursor { buf: raw, pos: 0 };
        let mut rem = budget;
        self.read_def(def_idx, &mut cur, &mut rem, 0)
    }

    fn read_def(
        &self,
        def_idx: usize,
        cur: &mut ByteCursor<'_>,
        budget: &mut usize,
        depth: usize,
    ) -> Option<KiwiVal> {
        if *budget == 0 || depth > 48 {
            return None;
        }
        *budget -= 1;
        let def = self.defs.get(def_idx)?;
        match def.kind {
            KiwiDefKind::Enum => {
                let ord = cur.read_var_uint()? as u32;
                let label = def
                    .fields
                    .iter()
                    .find(|f| f.tag == ord)
                    .map(|f| f.name.clone())
                    .unwrap_or_else(|| ord.to_string());
                Some(KiwiVal::Str(label))
            }
            KiwiDefKind::Struct => {
                let mut pairs = Vec::with_capacity(def.fields.len());
                for f in &def.fields {
                    let v = self.read_field(f, cur, budget, depth + 1)?;
                    pairs.push((f.name.clone(), v));
                }
                Some(KiwiVal::Obj(pairs))
            }
            KiwiDefKind::Message => {
                let mut pairs = Vec::new();
                loop {
                    let tag = cur.read_var_uint()? as u32;
                    if tag == 0 {
                        break;
                    }
                    let Some(f) = def.fields.iter().find(|f| f.tag == tag) else {
                        return None;
                    };
                    let v = self.read_field(f, cur, budget, depth + 1)?;
                    pairs.push((f.name.clone(), v));
                }
                Some(KiwiVal::Obj(pairs))
            }
        }
    }

    fn read_field(
        &self,
        f: &KiwiField,
        cur: &mut ByteCursor<'_>,
        budget: &mut usize,
        depth: usize,
    ) -> Option<KiwiVal> {
        if !f.is_array {
            return self.read_scalar_or_def(f.type_id, cur, budget, depth);
        }
        let len = cur.read_var_uint()? as usize;
        if len > *budget || len > 500_000 {
            return None;
        }
        // `byte[]` fast path
        if f.type_id == -2 {
            let start = cur.pos;
            let end = start.checked_add(len)?;
            if end > cur.buf.len() {
                return None;
            }
            cur.pos = end;
            return Some(KiwiVal::Bytes(cur.buf[start..end].to_vec()));
        }
        let mut items = Vec::with_capacity(len.min(1024));
        for _ in 0..len {
            items.push(self.read_scalar_or_def(f.type_id, cur, budget, depth)?);
        }
        Some(KiwiVal::Arr(items))
    }

    fn read_scalar_or_def(
        &self,
        type_id: i32,
        cur: &mut ByteCursor<'_>,
        budget: &mut usize,
        depth: usize,
    ) -> Option<KiwiVal> {
        match type_id {
            -1 => Some(KiwiVal::Bool(cur.read_byte()? != 0)),
            -2 => Some(KiwiVal::Uint(cur.read_byte()? as u64)),
            -3 => Some(KiwiVal::Int(cur.read_var_int()?)),
            -4 => Some(KiwiVal::Uint(cur.read_var_uint()?)),
            -5 => Some(KiwiVal::Float(cur.read_var_float()?)),
            -6 => Some(KiwiVal::Str(cur.read_cstring()?)),
            -7 => Some(KiwiVal::Int(cur.read_var_int()?)),
            -8 => Some(KiwiVal::Uint(cur.read_var_uint()?)),
            idx if idx >= 0 => self.read_def(idx as usize, cur, budget, depth),
            _ => None,
        }
    }
}

fn decode_xfig_layered(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let text = String::from_utf8_lossy(bytes);
    let mut layers = Vec::new();
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let mut i = 0;
    while i < lines.len() {
        let toks: Vec<&str> = lines[i].split_whitespace().collect();
        if let Some(first) = toks.first().and_then(|s| s.parse::<u32>().ok()) {
            if first == 2 && toks.len() >= 16 && i + 1 < lines.len() {
                let coords: Vec<f64> = lines[i + 1].split_whitespace().filter_map(|s| s.parse().ok()).collect();
                let mut d = String::new();
                for (ci, pair) in coords.chunks_exact(2).enumerate() {
                    let x = pair[0] / 15.0;
                    let y = pair[1] / 15.0;
                    min_x = min_x.min(x); min_y = min_y.min(y);
                    max_x = max_x.max(x); max_y = max_y.max(y);
                    let _ = write!(d, "{}{x:.1},{y:.1} ", if ci == 0 { "M" } else { "L" });
                }
                if !d.is_empty() {
                    layers.push(DesignLayer {
                        id: format!("xfig_{i}"),
                        name: format!("Polyline #{}", layers.len() + 1),
                        kind: "Vector".into(),
                        depth: 0,
                        visible: true,
                        opacity: 1.0,
                        blend_mode: "normal".into(),
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 100.0,
                        svg_fragment: Some(format!("<path d=\"{d}\" fill=\"none\" stroke=\"#eaffff\" stroke-width=\"1.5\"/>")),
                        image_data_url: None,
                    });
                }
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    let pw = if min_x.is_finite() { (max_x - min_x).max(120.0) } else { 800.0 };
    let ph = if min_y.is_finite() { (max_y - min_y).max(120.0) } else { 600.0 };
    metadata.push(MetadataEntry { section: "Xfig".into(), key: "Vector Objects".into(), value: layers.len().to_string() });
    Some(ViewerPayload::LayeredDesign {
        format_label: "Xfig Vector Drawing (.fig)".into(),
        pages: vec![DesignPage {
            id: "xfig_0".into(),
            name: "Drawing Page".into(),
            width: pw,
            height: ph,
            background: None,
            composite_data_url: None,
            layers,
        }],
        active_page: 0,
        note: None,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. ArtCraft FigCraft (.figcraft)
// ─────────────────────────────────────────────────────────────────────────────

fn decode_figcraft(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut doc_json: Option<Value> = None;
    let mut images: HashMap<String, String> = HashMap::new();
    let mut thumb_url: Option<String> = None;

    if bytes.starts_with(b"PK\x03\x04") {
        let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
        for i in 0..arch.len() {
            let Ok(mut f) = arch.by_index(i) else { continue };
            let Ok(name) = f.name().map(|n| n.into_owned()) else { continue };
            if name == "document.json" {
                let mut s = String::new();
                if f.read_to_string(&mut s).is_ok() {
                    doc_json = serde_json::from_str(&s).ok();
                }
            } else if name.starts_with("images/") && f.size() > 0 && f.size() < 24 * 1024 * 1024 {
                let mut ibuf = Vec::new();
                if f.read_to_end(&mut ibuf).is_ok() {
                    let key = name.trim_start_matches("images/").to_string();
                    let mime = guess_image_mime(&ibuf);
                    images.insert(key, format!("data:{mime};base64,{}", B64.encode(&ibuf)));
                }
            } else if name == "thumbnail.png" {
                let mut tbuf = Vec::new();
                if f.read_to_end(&mut tbuf).is_ok() && !tbuf.is_empty() {
                    thumb_url = Some(format!("data:image/png;base64,{}", B64.encode(&tbuf)));
                }
            }
        }
    } else {
        doc_json = serde_json::from_slice(bytes).ok();
    }

    let root = doc_json?;
    let mut pages = Vec::new();
    let doc_obj = root.get("document").unwrap_or(&root);

    // Support both `nodes` map (FigCraft doc v1) and `pages` list/tree
    if let Some(nodes_map) = doc_obj.get("nodes").and_then(Value::as_object) {
        let page_ids: Vec<String> = doc_obj
            .get("pages")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|u| u.to_string())))
                    .collect()
            })
            .unwrap_or_else(|| {
                nodes_map
                    .iter()
                    .filter(|(_, n)| n.get("kind").and_then(|k| k.get("type")).and_then(Value::as_str) == Some("page"))
                    .map(|(id, _)| id.clone())
                    .collect()
            });

        for (pi, pid) in page_ids.iter().enumerate() {
            let Some(pnode) = nodes_map.get(pid) else { continue };
            let pname = pnode
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("Page")
                .to_string();
            let mut layers = Vec::new();
            let mut bounds = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);

            fn walk_fc_map(
                id: &str,
                depth: usize,
                ox: f64,
                oy: f64,
                nodes: &serde_json::Map<String, Value>,
                images: &HashMap<String, String>,
                layers: &mut Vec<DesignLayer>,
                bounds: &mut (f64, f64, f64, f64),
                skip_self: bool,
            ) {
                if layers.len() >= 1500 { return; }
                let Some(n) = nodes.get(id) else { return };
                let (nx, ny) = read_fc_pos(n);
                let wx = ox + nx;
                let wy = oy + ny;
                let (w, h) = read_fc_size(n);
                if !skip_self {
                    bounds.0 = bounds.0.min(wx);
                    bounds.1 = bounds.1.min(wy);
                    bounds.2 = bounds.2.max(wx + w.max(1.0));
                    bounds.3 = bounds.3.max(wy + h.max(1.0));
                    layers.push(json_node_to_design_layer(id, depth, wx, wy, w, h, n, images));
                }
                if let Some(children) = n.get("children").and_then(Value::as_array) {
                    let next_d = if skip_self { 0 } else { depth + 1 };
                    for c in children {
                        let cid = c.as_str().map(str::to_string).or_else(|| c.as_u64().map(|u| u.to_string()));
                        if let Some(cid) = cid {
                            walk_fc_map(&cid, next_d, if skip_self { 0.0 } else { wx }, if skip_self { 0.0 } else { wy }, nodes, images, layers, bounds, false);
                        }
                    }
                }
            }

            walk_fc_map(pid, 0, 0.0, 0.0, nodes_map, &images, &mut layers, &mut bounds, true);
            let min_x = if bounds.0.is_finite() { bounds.0 } else { 0.0 };
            let min_y = if bounds.1.is_finite() { bounds.1 } else { 0.0 };
            let pw = if bounds.2.is_finite() { (bounds.2 - min_x).max(64.0) } else { 1200.0 };
            let ph = if bounds.3.is_finite() { (bounds.3 - min_y).max(64.0) } else { 800.0 };
            if min_x != 0.0 || min_y != 0.0 {
                for l in &mut layers {
                    l.x -= min_x;
                    l.y -= min_y;
                }
            }
            pages.push(DesignPage {
                id: pid.clone(),
                name: pname,
                width: pw,
                height: ph,
                background: pnode.pointer("/kind/background").and_then(json_color_css),
                composite_data_url: if pi == 0 { thumb_url.clone() } else { None },
                layers,
            });
        }
    } else if let Some(arr) = doc_obj.get("pages").and_then(Value::as_array) {
        for (pi, pval) in arr.iter().enumerate() {
            let pname = pval.get("name").and_then(Value::as_str).unwrap_or("Page").to_string();
            let pw = pval.get("width").and_then(Value::as_f64).unwrap_or(1200.0);
            let ph = pval.get("height").and_then(Value::as_f64).unwrap_or(800.0);
            let mut layers = Vec::new();
            let mut bounds = (0.0, 0.0, pw, ph);
            if let Some(children) = pval.get("children").or_else(|| pval.get("layers")).and_then(Value::as_array) {
                for (ci, c) in children.iter().enumerate() {
                    walk_json_tree(&format!("fc_{pi}_{ci}"), 0, 0.0, 0.0, c, &images, &mut layers, &mut bounds);
                }
            }
            pages.push(DesignPage {
                id: format!("fc_page_{pi}"),
                name: pname,
                width: pw.max(64.0),
                height: ph.max(64.0),
                background: pval.get("background").and_then(json_color_css),
                composite_data_url: if pi == 0 { thumb_url.clone() } else { None },
                layers,
            });
        }
    }

    if pages.is_empty() {
        return None;
    }
    metadata.push(MetadataEntry { section: "FigCraft".into(), key: "Pages".into(), value: pages.len().to_string() });
    let total_layers: usize = pages.iter().map(|p| p.layers.len()).sum();
    metadata.push(MetadataEntry { section: "FigCraft".into(), key: "Layers".into(), value: total_layers.to_string() });

    Some(ViewerPayload::LayeredDesign {
        format_label: "ArtCraft FigCraft Design (.figcraft)".into(),
        pages,
        active_page: 0,
        note: Some("Inspect individual pages, all pages together, and toggle per-layer visibility".into()),
    })
}

fn read_fc_pos(n: &Value) -> (f64, f64) {
    if let Some(arr) = n.get("transform").and_then(Value::as_array) {
        if arr.len() >= 6 {
            return (
                arr[4].as_f64().unwrap_or(0.0),
                arr[5].as_f64().unwrap_or(0.0),
            );
        }
    }
    (
        n.get("x").and_then(Value::as_f64).unwrap_or(0.0),
        n.get("y").and_then(Value::as_f64).unwrap_or(0.0),
    )
}

fn read_fc_size(n: &Value) -> (f64, f64) {
    if let Some(sz) = n.get("size").and_then(Value::as_object) {
        return (
            sz.get("width").or_else(|| sz.get("x")).and_then(Value::as_f64).unwrap_or(100.0),
            sz.get("height").or_else(|| sz.get("y")).and_then(Value::as_f64).unwrap_or(100.0),
        );
    }
    (
        n.get("width").and_then(Value::as_f64).unwrap_or(100.0),
        n.get("height").and_then(Value::as_f64).unwrap_or(100.0),
    )
}

fn json_color_css(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    let o = v.as_object()?;
    let scale = |k: &str, d: f64| {
        let raw = o.get(k).and_then(Value::as_f64).unwrap_or(d);
        if raw > 1.0 { raw.clamp(0.0, 255.0).round() as u8 } else { (raw.clamp(0.0, 1.0) * 255.0).round() as u8 }
    };
    let a = o.get("a").and_then(Value::as_f64).unwrap_or(1.0);
    let alpha = if a > 1.0 { a / 255.0 } else { a }.clamp(0.0, 1.0);
    Some(format!("rgba({},{},{},{alpha:.3})", scale("r", 0.9), scale("g", 1.0), scale("b", 1.0)))
}

fn json_node_to_design_layer(
    id: &str,
    depth: usize,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    n: &Value,
    images: &HashMap<String, String>,
) -> DesignLayer {
    let kind = n
        .pointer("/kind/type")
        .or_else(|| n.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("Shape")
        .to_string();
    let name = n
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&kind)
        .to_string();
    let visible = n.get("visible").and_then(Value::as_bool).unwrap_or(true);
    let opacity = n.get("opacity").and_then(Value::as_f64).unwrap_or(1.0) as f32;

    let fill = n
        .get("fills")
        .and_then(Value::as_array)
        .and_then(|fs| fs.iter().find_map(|f| f.get("color").and_then(json_color_css)))
        .or_else(|| n.pointer("/fill/color").and_then(json_color_css));
    let stroke = n
        .get("strokes")
        .and_then(Value::as_array)
        .and_then(|ss| ss.iter().find_map(|s| s.get("color").and_then(json_color_css)))
        .or_else(|| n.pointer("/stroke/color").and_then(json_color_css));
    let text = n
        .pointer("/kind/text/characters")
        .or_else(|| n.pointer("/text/characters"))
        .or_else(|| n.get("characters"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let image_ref = n
        .pointer("/kind/image")
        .or_else(|| n.get("imageHash"))
        .and_then(Value::as_str)
        .and_then(|h| images.get(h).cloned());

    let svg = match kind.to_ascii_lowercase().as_str() {
        "ellipse" => Some(format!(
            "<ellipse cx=\"{:.2}\" cy=\"{:.2}\" rx=\"{:.2}\" ry=\"{:.2}\" fill=\"{}\" stroke=\"{}\"/>",
            x + w * 0.5,
            y + h * 0.5,
            w * 0.5,
            h * 0.5,
            fill.as_deref().unwrap_or("rgba(234,255,255,0.22)"),
            stroke.as_deref().unwrap_or("rgba(234,255,255,0.45)")
        )),
        "text" => {
            let t = text.as_deref().unwrap_or(&name);
            let fs = (h * 0.65).clamp(11.0, 48.0);
            Some(format!(
                "<text x=\"{x:.2}\" y=\"{:.2}\" font-size=\"{fs:.1}\" fill=\"{}\" font-family=\"sans-serif\">{}</text>",
                y + fs,
                fill.as_deref().unwrap_or("#eaffff"),
                esc_xml(t)
            ))
        }
        "group" | "layer" => None,
        _ => Some(format!(
            "<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{w:.2}\" height=\"{h:.2}\" rx=\"4\" fill=\"{}\" stroke=\"{}\"/>",
            fill.as_deref().unwrap_or("rgba(234,255,255,0.12)"),
            stroke.as_deref().unwrap_or("rgba(234,255,255,0.42)")
        )),
    };

    DesignLayer {
        id: id.into(),
        name,
        kind,
        depth,
        visible,
        opacity,
        blend_mode: "normal".into(),
        x,
        y,
        width: w.max(1.0),
        height: h.max(1.0),
        svg_fragment: svg,
        image_data_url: image_ref,
    }
}

fn walk_json_tree(
    id: &str,
    depth: usize,
    ox: f64,
    oy: f64,
    n: &Value,
    images: &HashMap<String, String>,
    layers: &mut Vec<DesignLayer>,
    bounds: &mut (f64, f64, f64, f64),
) {
    if layers.len() >= 1500 { return; }
    let (nx, ny) = read_fc_pos(n);
    let wx = ox + nx;
    let wy = oy + ny;
    let (w, h) = read_fc_size(n);
    bounds.0 = bounds.0.min(wx);
    bounds.1 = bounds.1.min(wy);
    bounds.2 = bounds.2.max(wx + w.max(1.0));
    bounds.3 = bounds.3.max(wy + h.max(1.0));
    layers.push(json_node_to_design_layer(id, depth, wx, wy, w, h, n, images));
    if let Some(children) = n.get("children").and_then(Value::as_array) {
        for (ci, c) in children.iter().enumerate() {
            walk_json_tree(&format!("{id}_{ci}"), depth + 1, wx, wy, c, images, layers, bounds);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. ArtCraft VectorCraft (.vectorcraft JSON / Gzip)
// ─────────────────────────────────────────────────────────────────────────────

fn decode_vectorcraft(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let decompressed: Vec<u8>;
    let json_bytes: &[u8] = if bytes.starts_with(&[0x1F, 0x8B, 0x08]) {
        let mut gz = GzDecoder::new(bytes);
        let mut buf = Vec::new();
        gz.read_to_end(&mut buf).ok()?;
        decompressed = buf;
        &decompressed
    } else {
        bytes
    };

    // Strip optional v4 `BLOB_MAGIC` trailer after JSON object
    let root: Value = serde_json::from_slice(json_bytes).or_else(|_| {
        let mut de = serde_json::Deserializer::from_slice(json_bytes);
        serde::Deserialize::deserialize(&mut de)
    }).ok()?;

    let doc = root.get("document").unwrap_or(&root);
    let mut images: HashMap<String, String> = HashMap::new();
    if let Some(img_map) = root.get("images").and_then(Value::as_object) {
        for (k, v) in img_map {
            if let (Some(mime), Some(data)) = (
                v.get("mime").and_then(Value::as_str),
                v.get("data").and_then(Value::as_str),
            ) {
                images.insert(k.clone(), format!("data:{mime};base64,{data}"));
            }
        }
    }
    let preview_url = root.pointer("/preview/data").and_then(Value::as_str).map(|d| {
        let mime = root.pointer("/preview/mime").and_then(Value::as_str).unwrap_or("image/png");
        format!("data:{mime};base64,{d}")
    });

    let mut all_layers = Vec::new();
    if let Some(layers) = doc.get("layers").and_then(Value::as_array) {
        for (li, lnode) in layers.iter().enumerate() {
            collect_vc_layers(&format!("vc_{li}"), 0, lnode, &images, &mut all_layers);
        }
    }

    let mut pages = Vec::new();
    if let Some(artboards) = doc.get("artboards").and_then(Value::as_array) {
        for (ai, ab) in artboards.iter().enumerate() {
            let name = ab.get("name").and_then(Value::as_str).unwrap_or("Artboard").to_string();
            let rect = ab.get("rect").unwrap_or(ab);
            let ax = rect.get("x0").or_else(|| rect.get("x")).and_then(Value::as_f64).unwrap_or(0.0);
            let ay = rect.get("y0").or_else(|| rect.get("y")).and_then(Value::as_f64).unwrap_or(0.0);
            let x1 = rect.get("x1").and_then(Value::as_f64);
            let y1 = rect.get("y1").and_then(Value::as_f64);
            let aw = x1.map(|v| (v - ax).max(1.0)).unwrap_or_else(|| rect.get("width").and_then(Value::as_f64).unwrap_or(800.0));
            let ah = y1.map(|v| (v - ay).max(1.0)).unwrap_or_else(|| rect.get("height").and_then(Value::as_f64).unwrap_or(600.0));

            // Filter layers intersecting artboard (or include all on first if single artboard)
            let mut page_layers = Vec::new();
            for l in &all_layers {
                let inside = artboards.len() == 1
                    || (l.x + l.width >= ax && l.x <= ax + aw && l.y + l.height >= ay && l.y <= ay + ah);
                if inside {
                    let mut shifted = l.clone();
                    shifted.x -= ax;
                    shifted.y -= ay;
                    page_layers.push(shifted);
                }
            }
            if page_layers.is_empty() && ai == 0 {
                page_layers = all_layers.clone();
            }
            pages.push(DesignPage {
                id: format!("vc_ab_{ai}"),
                name,
                width: aw,
                height: ah,
                background: None,
                composite_data_url: if ai == 0 { preview_url.clone() } else { None },
                layers: page_layers,
            });
        }
    }

    if pages.is_empty() {
        pages.push(DesignPage {
            id: "vc_ab_0".into(),
            name: doc.get("title").and_then(Value::as_str).unwrap_or("Canvas").to_string(),
            width: 800.0,
            height: 600.0,
            background: None,
            composite_data_url: preview_url,
            layers: all_layers,
        });
    }

    let lcount: usize = pages.iter().map(|p| p.layers.len()).sum();
    metadata.push(MetadataEntry { section: "VectorCraft".into(), key: "Artboards / Pages".into(), value: pages.len().to_string() });
    metadata.push(MetadataEntry { section: "VectorCraft".into(), key: "Layers / Objects".into(), value: lcount.to_string() });

    Some(ViewerPayload::LayeredDesign {
        format_label: "ArtCraft VectorCraft (.vectorcraft)".into(),
        pages,
        active_page: 0,
        note: Some("Inspect artboard pages, hover elements, and toggle vector layer visibility".into()),
    })
}

fn collect_vc_layers(
    id: &str,
    depth: usize,
    node: &Value,
    images: &HashMap<String, String>,
    out: &mut Vec<DesignLayer>,
) {
    if out.len() >= 1500 { return; }
    let visible = node.get("visible").and_then(Value::as_bool).unwrap_or(true);
    let opacity = node.get("opacity").and_then(Value::as_f64).unwrap_or(1.0) as f32;
    let kind_obj = node.get("kind").unwrap_or(node);
    let kind = kind_obj
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| kind_obj.as_object().and_then(|m| m.keys().next().map(String::as_str)))
        .unwrap_or("Layer")
        .to_string();
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&kind)
        .to_string();

    let (mut x, mut y, mut w, mut h) = (0.0f64, 0.0f64, 100.0f64, 100.0f64);
    let mut svg = None;

    // Extract Bezier / anchor paths if present
    let path_data = kind_obj.pointer("/Path/anchors").or_else(|| kind_obj.pointer("/path/anchors")).or_else(|| node.get("anchors"));
    if let Some(anchors) = path_data.and_then(Value::as_array) {
        let mut d = String::new();
        let (mut minx, mut miny, mut maxx, mut maxy) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for (ai, anc) in anchors.iter().enumerate() {
            let pt = anc.get("p").unwrap_or(anc);
            let (px, py) = if let Some(arr) = pt.as_array() {
                (arr.first().and_then(Value::as_f64).unwrap_or(0.0), arr.get(1).and_then(Value::as_f64).unwrap_or(0.0))
            } else {
                (pt.get("x").and_then(Value::as_f64).unwrap_or(0.0), pt.get("y").and_then(Value::as_f64).unwrap_or(0.0))
            };
            minx = minx.min(px); miny = miny.min(py);
            maxx = maxx.max(px); maxy = maxy.max(py);
            let _ = write!(d, "{}{px:.2},{py:.2} ", if ai == 0 { "M" } else { "L" });
        }
        if kind_obj.pointer("/Path/closed").or_else(|| kind_obj.pointer("/path/closed")).and_then(Value::as_bool) == Some(true) {
            d.push('Z');
        }
        if minx.is_finite() {
            x = minx; y = miny; w = (maxx - minx).max(1.0); h = (maxy - miny).max(1.0);
        }
        if !d.is_empty() {
            svg = Some(format!("<path d=\"{d}\" fill=\"rgba(234,255,255,0.2)\" stroke=\"#eaffff\" stroke-width=\"1.5\"/>"));
        }
    }

    let img_url = kind_obj
        .pointer("/Image/image_id")
        .or_else(|| kind_obj.pointer("/image/key"))
        .and_then(Value::as_str)
        .and_then(|k| images.get(k).cloned());

    out.push(DesignLayer {
        id: id.into(),
        name,
        kind,
        depth,
        visible,
        opacity,
        blend_mode: node.get("blend").and_then(Value::as_str).unwrap_or("normal").to_string(),
        x,
        y,
        width: w,
        height: h,
        svg_fragment: svg,
        image_data_url: img_url,
    });

    if let Some(children) = kind_obj
        .pointer("/Group/children")
        .or_else(|| kind_obj.pointer("/Layer/children"))
        .or_else(|| kind_obj.get("children"))
        .or_else(|| node.get("children"))
        .and_then(Value::as_array)
    {
        for (ci, c) in children.iter().enumerate() {
            collect_vc_layers(&format!("{id}_{ci}"), depth + 1, c, images, out);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. ArtCraft PhotoCraft (.pcraft / .photocraft)
// ─────────────────────────────────────────────────────────────────────────────

fn decode_photocraft(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let manifest_raw = {
        let mut f = arch.by_name("manifest.json").ok()?;
        let mut s = String::new();
        f.read_to_string(&mut s).ok()?;
        serde_json::from_str::<Value>(&s).ok()?
    };

    let doc = manifest_raw.get("document").unwrap_or(&manifest_raw);
    let cw = doc.pointer("/size/width").and_then(Value::as_u64).unwrap_or(1024) as u32;
    let ch = doc.pointer("/size/height").and_then(Value::as_u64).unwrap_or(1024) as u32;
    let doc_name = doc.get("name").and_then(Value::as_str).unwrap_or("PhotoCraft Canvas").to_string();

    let mut composite_url = None;
    for cand in ["composite/full.png", "composite/preview.png", "thumb.png"] {
        if let Ok(mut f) = arch.by_name(cand) {
            let mut png = Vec::new();
            if f.read_to_end(&mut png).is_ok() && !png.is_empty() {
                composite_url = Some(format!("data:image/png;base64,{}", B64.encode(&png)));
                break;
            }
        }
    }

    let mut tile_cache: HashMap<String, Vec<u8>> = HashMap::new();
    for i in 0..arch.len() {
        let Ok(mut f) = arch.by_index(i) else { continue };
        let Ok(name) = f.name().map(|n| n.into_owned()) else { continue };
        if name.starts_with("tiles/") && name.ends_with(".zst") && f.size() < 4 * 1024 * 1024 {
            let mut zbuf = Vec::new();
            if f.read_to_end(&mut zbuf).is_ok() {
                if let Ok(mut dec) = ZstdDecoder::new(Cursor::new(&zbuf)) {
                    let mut raw = Vec::new();
                    if dec.read_to_end(&mut raw).is_ok() {
                        let key = name.trim_start_matches("tiles/").trim_end_matches(".zst").to_string();
                        tile_cache.insert(key, raw);
                    }
                }
            }
        }
    }

    let mut layers = Vec::new();
    if let Some(arr) = doc.get("layers").and_then(Value::as_array) {
        for (idx, lval) in arr.iter().rev().enumerate() {
            collect_pcraft_layer(&format!("pc_{idx}"), 0, lval, cw, ch, &tile_cache, &mut layers);
        }
    }

    metadata.push(MetadataEntry { section: "PhotoCraft".into(), key: "Canvas".into(), value: format!("{cw} × {ch} px") });
    metadata.push(MetadataEntry { section: "PhotoCraft".into(), key: "Layers / Tiles".into(), value: format!("{} layers / {} tiles", layers.len(), tile_cache.len()) });

    Some(ViewerPayload::LayeredDesign {
        format_label: "ArtCraft PhotoCraft (.pcraft)".into(),
        pages: vec![DesignPage {
            id: "pc_main".into(),
            name: doc_name,
            width: cw as f64,
            height: ch as f64,
            background: None,
            composite_data_url: composite_url,
            layers,
        }],
        active_page: 0,
        note: Some("Rendered PhotoCraft layers with tile surface reconstruction & visibility toggles".into()),
    })
}

fn collect_pcraft_layer(
    id: &str,
    depth: usize,
    lval: &Value,
    cw: u32,
    ch: u32,
    tile_cache: &HashMap<String, Vec<u8>>,
    out: &mut Vec<DesignLayer>,
) {
    if out.len() >= 512 { return; }
    let name = lval.get("name").and_then(Value::as_str).unwrap_or("Layer").to_string();
    let visible = lval.get("visible").and_then(Value::as_bool).unwrap_or(true);
    let opacity = lval.get("opacity").and_then(Value::as_f64).unwrap_or(1.0) as f32;
    let blend = lval.get("blend").and_then(Value::as_str).unwrap_or("normal").to_string();
    let kind = lval.pointer("/content/kind").and_then(Value::as_str).unwrap_or("raster").to_string();

    let surface = lval
        .pointer("/content/surface")
        .or_else(|| lval.pointer("/content/cache"));
    let img_url = surface.and_then(|s| render_pcraft_surface(s, cw, ch, tile_cache));

    out.push(DesignLayer {
        id: id.into(),
        name,
        kind: kind.to_uppercase(),
        depth,
        visible,
        opacity,
        blend_mode: blend,
        x: 0.0,
        y: 0.0,
        width: cw as f64,
        height: ch as f64,
        svg_fragment: None,
        image_data_url: img_url,
    });

    if let Some(children) = lval.pointer("/content/children").and_then(Value::as_array) {
        for (ci, child) in children.iter().rev().enumerate() {
            collect_pcraft_layer(&format!("{id}_{ci}"), depth + 1, child, cw, ch, tile_cache, out);
        }
    }
}

fn render_pcraft_surface(
    surf: &Value,
    cw: u32,
    ch: u32,
    tile_cache: &HashMap<String, Vec<u8>>,
) -> Option<String> {
    let tiles = surf.get("tiles").and_then(Value::as_array)?;
    if tiles.is_empty() {
        return None;
    }
    let w = (cw as usize).min(MAX_LAYER_IMG_DIM);
    let h = (ch as usize).min(MAX_LAYER_IMG_DIM);
    if w == 0 || h == 0 {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    let tile_sz = 256usize;
    let mut drew = false;

    for t in tiles {
        let tx = t.get("tx").or_else(|| t.get("x")).and_then(Value::as_i64).unwrap_or(0);
        let ty = t.get("ty").or_else(|| t.get("y")).and_then(Value::as_i64).unwrap_or(0);
        let hash = t.get("hash").or_else(|| t.get("blob")).and_then(Value::as_str)?;
        let Some(raw) = tile_cache.get(hash) else { continue };
        let ox = (tx * tile_sz as i64).max(0) as usize;
        let oy = (ty * tile_sz as i64).max(0) as usize;
        if ox >= w || oy >= h || raw.len() < tile_sz * tile_sz * 4 {
            continue;
        }
        drew = true;
        for py in 0..tile_sz.min(h - oy) {
            for px in 0..tile_sz.min(w - ox) {
                let src = (py * tile_sz + px) * 4;
                let dst = ((oy + py) * w + (ox + px)) * 4;
                rgba[dst..dst + 4].copy_from_slice(&raw[src..src + 4]);
            }
        }
    }
    if !drew {
        return None;
    }
    rgba_to_data_url(w as u32, h as u32, &rgba)
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. OpenRaster (.ora) & Krita (.kra)
// ─────────────────────────────────────────────────────────────────────────────

fn decode_ora_or_kra(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut entries: HashMap<String, Vec<u8>> = HashMap::new();
    for i in 0..arch.len() {
        let Ok(mut f) = arch.by_index(i) else { continue };
        let Ok(name) = f.name().map(|n| n.into_owned()) else { continue };
        if f.size() <= 16 * 1024 * 1024 {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                entries.insert(name, buf);
            }
        }
    }

    let xml_bytes = entries.get("stack.xml").or_else(|| entries.get("maindoc.xml"))?;
    let xml = String::from_utf8_lossy(xml_bytes);
    let doc = roxmltree::Document::parse(&xml).ok()?;

    let image_el = doc.descendants().find(|n| n.has_tag_name("image") || n.has_tag_name("IMAGE"));
    let w = image_el
        .and_then(|n| n.attribute("w").or_else(|| n.attribute("width")))
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(1024.0);
    let h = image_el
        .and_then(|n| n.attribute("h").or_else(|| n.attribute("height")))
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(1024.0);

    let composite = entries
        .get("mergedimage.png")
        .or_else(|| entries.get("preview.png"))
        .map(|b| format!("data:image/png;base64,{}", B64.encode(b)));

    let mut layers = Vec::new();
    for (idx, n) in doc
        .descendants()
        .filter(|n| n.has_tag_name("layer") || n.has_tag_name("stack"))
        .enumerate()
    {
        if n.has_tag_name("stack") && idx == 0 {
            continue;
        }
        let lname = n.attribute("name").unwrap_or("Layer").to_string();
        let visible = n.attribute("visibility").or_else(|| n.attribute("visible")) != Some("hidden")
            && n.attribute("visible") != Some("0");
        let opacity = n
            .attribute("opacity")
            .and_then(|s| s.parse::<f32>().ok())
            .map(|v| if v > 1.0 { v / 255.0 } else { v })
            .unwrap_or(1.0);
        let lx = n.attribute("x").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let ly = n.attribute("y").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let src = n.attribute("src").and_then(|s| entries.get(s)).map(|b| {
            format!("data:{};base64,{}", guess_image_mime(b), B64.encode(b))
        });

        layers.push(DesignLayer {
            id: format!("{format_id}_{idx}"),
            name: lname,
            kind: if n.has_tag_name("stack") { "Group".into() } else { "Layer".into() },
            depth: 0,
            visible,
            opacity,
            blend_mode: n.attribute("composite-op").unwrap_or("normal").to_string(),
            x: lx,
            y: ly,
            width: w,
            height: h,
            svg_fragment: None,
            image_data_url: src,
        });
    }

    metadata.push(MetadataEntry { section: format_id.to_ascii_uppercase(), key: "Layers".into(), value: layers.len().to_string() });

    Some(ViewerPayload::LayeredDesign {
        format_label: if format_id == "kra" { "Krita Layered Document (.kra)".into() } else { "OpenRaster Layered Image (.ora)".into() },
        pages: vec![DesignPage {
            id: format!("{format_id}_page"),
            name: format!("Canvas ({:.0} × {:.0})", w, h),
            width: w,
            height: h,
            background: None,
            composite_data_url: composite,
            layers,
        }],
        active_page: 0,
        note: Some("Inspect composited canvas and toggle layer visibility".into()),
    })
}

fn guess_image_mime(b: &[u8]) -> &'static str {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if b.starts_with(b"\xFF\xD8\xFF") {
        "image/jpeg"
    } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if b.starts_with(b"GIF8") {
        "image/gif"
    } else {
        "image/png"
    }
}
