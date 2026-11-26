use std::io::{Cursor, Read as _};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use flate2::read::GzDecoder;
use image::{codecs::png::PngEncoder, ColorType, GenericImageView as _, ImageEncoder as _};

use crate::model::{MetadataEntry, ViewerPayload};

/// Decodes raster images, HDR/EXR, PSD/PSB, PCX, XPM, Camera RAW previews, ICNS, and SVG/SVGZ.
pub fn decode_image(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    if format_id == "svg" {
        return Some(build_svg_payload(bytes, metadata, false));
    }
    if format_id == "svgz" {
        let mut gz = GzDecoder::new(bytes);
        let mut decoded = Vec::new();
        if gz.read_to_end(&mut decoded).is_ok() {
            metadata.push(MetadataEntry { section: "Vector".into(), key: "Compression".into(), value: "Gzip SVGZ".into() });
            return Some(build_svg_payload(&decoded, metadata, true));
        }
    }

    // Fast pass-through for browser-native raster images without re-encoding
    if matches!(format_id, "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "ico") {
        if let Ok(reader) = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
            if let Ok((w, h)) = reader.into_dimensions() {
                let mime = match format_id {
                    "png" => "image/png",
                    "jpg" | "jpeg" => "image/jpeg",
                    "webp" => "image/webp",
                    "gif" => "image/gif",
                    "bmp" => "image/bmp",
                    "ico" => "image/x-icon",
                    _ => "image/png",
                };
                metadata.push(MetadataEntry { section: "Image".into(), key: "Dimensions".into(), value: format!("{w} × {h} px") });
                let data_url = format!("data:{mime};base64,{}", B64.encode(bytes));
                return Some(ViewerPayload::Image {
                    width: w,
                    height: h,
                    channels: "Native".into(),
                    bit_depth: 8,
                    data_url,
                    frames: 1,
                    note: None,
                });
            }
        }
    }

    // OpenEXR HDR -> ACES/Reinhard tone-mapped RGBA PNG
    if format_id == "exr" {
        if let Some(payload) = decode_exr(bytes, metadata) {
            return Some(payload);
        }
    }

    // Adobe PSD / PSB composite preview
    if matches!(format_id, "psd" | "psb") {
        if let Some(payload) = decode_psd(bytes, metadata) {
            return Some(payload);
        }
    }

    // ZSoft PCX
    if format_id == "pcx" {
        if let Some(payload) = decode_pcx(bytes, metadata) {
            return Some(payload);
        }
    }

    // X PixMap XPM
    if format_id == "xpm" {
        if let Some(payload) = decode_xpm(bytes, metadata) {
            return Some(payload);
        }
    }

    // Apple ICNS
    if format_id == "icns" {
        if let Some(payload) = decode_icns(bytes, metadata) {
            return Some(payload);
        }
    }

    // Camera RAW (.cr2, .cr3, .nef, .arw, .dng, .orf, .rw2, .raf, .pef, .srw, .3fr)
    if matches!(
        format_id,
        "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "raf" | "pef" | "srw" | "3fr"
    ) {
        if let Some(payload) = extract_camera_raw_preview(bytes, format_id, metadata) {
            return Some(payload);
        }
    }

    // Generic `image` crate fallback (TIFF, TGA, PNM, QOI, HDR, DDS, CUR, etc.)
    let guessed = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let dyn_img = guessed.decode().ok()?;
    let (w, h) = dyn_img.dimensions();
    let rgba = dyn_img.to_rgba8();
    let png_bytes = encode_rgba_png(w, h, &rgba)?;
    metadata.push(MetadataEntry { section: "Image".into(), key: "Dimensions".into(), value: format!("{w} × {h} px") });
    Some(ViewerPayload::Image {
        width: w,
        height: h,
        channels: "RGBA".into(),
        bit_depth: 8,
        data_url: format!("data:image/png;base64,{}", B64.encode(&png_bytes)),
        frames: 1,
        note: None,
    })
}

fn encode_rgba_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let enc = PngEncoder::new(&mut out);
    enc.write_image(rgba, w, h, ColorType::Rgba8.into()).ok()?;
    Some(out)
}

fn build_svg_payload(bytes: &[u8], metadata: &mut Vec<MetadataEntry>, is_compressed: bool) -> ViewerPayload {
    let full = String::from_utf8_lossy(bytes).into_owned();
    let head = &full[..full.len().min(8192)];
    let mut width = extract_svg_attr(head, "width");
    let mut height = extract_svg_attr(head, "height");
    let view_box = extract_svg_attr(head, "viewBox");

    if (width.is_none() || height.is_none()) && view_box.is_some() {
        let parts: Vec<&str> = view_box
            .as_deref()
            .unwrap_or("")
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|s| !s.is_empty())
            .collect();
        if parts.len() == 4 {
            if width.is_none() {
                width = Some(format!("{} (viewBox)", parts[2]));
            }
            if height.is_none() {
                height = Some(format!("{} (viewBox)", parts[3]));
            }
        }
    }

    if let Some(w) = &width {
        metadata.push(MetadataEntry { section: "SVG Frame".into(), key: "Width".into(), value: w.clone() });
    }
    if let Some(h) = &height {
        metadata.push(MetadataEntry { section: "SVG Frame".into(), key: "Height".into(), value: h.clone() });
    }
    if let Some(vb) = &view_box {
        metadata.push(MetadataEntry { section: "SVG Frame".into(), key: "viewBox".into(), value: vb.clone() });
    }

    // For very large SVGs (>1.5 MB uncompressed), stream bytes from native URI / Blob instead of cloning twice in JSON.
    let use_stream_url = !is_compressed && bytes.len() > 1_500_000;
    let source_text = if full.len() > 262_144 {
        let mut cut = 262_144;
        while !full.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n\n<!-- ... truncated at 256 KB for responsive source preview ... -->", &full[..cut])
    } else {
        full.clone()
    };

    ViewerPayload::Svg {
        svg_markup: if use_stream_url { String::new() } else { full },
        width,
        height,
        view_box,
        source_text,
        use_stream_url,
    }
}

fn extract_svg_attr(head: &str, attr: &str) -> Option<String> {
    let svg_start = head.find("<svg")?;
    let tag_end = head[svg_start..].find('>')?;
    let tag = &head[svg_start..svg_start + tag_end];
    for quote in ['"', '\''] {
        let pat = format!("{attr}={quote}");
        if let Some(idx) = tag.find(&pat) {
            let rest = &tag[idx + pat.len()..];
            if let Some(end) = rest.find(quote) {
                let val = rest[..end].trim();
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

fn decode_exr(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    use exr::prelude::*;
    let loaded = exr::prelude::read()
        .no_deep_data()
        .largest_resolution_level()
        .rgba_channels(
            |resolution, _| (resolution.width(), vec![0u8; resolution.width() * resolution.height() * 4]),
            |(stride, buf), pos, (r, g, b, a): (f32, f32, f32, f32)| {
                let idx = (pos.y() * *stride + pos.x()) * 4;
                if idx + 3 < buf.len() {
                    buf[idx] = tone_map_u8(r);
                    buf[idx + 1] = tone_map_u8(g);
                    buf[idx + 2] = tone_map_u8(b);
                    buf[idx + 3] = (a.clamp(0.0, 1.0) * 255.0) as u8;
                }
            },
        )
        .first_valid_layer()
        .all_attributes()
        .from_buffered(Cursor::new(bytes))
        .ok()?;
    let size = loaded.layer_data.size;
    let w = size.width() as u32;
    let h = size.height() as u32;
    let (_, rgba) = loaded.layer_data.channel_data.pixels;
    let png_bytes = encode_rgba_png(w, h, &rgba)?;
    metadata.push(MetadataEntry { section: "OpenEXR".into(), key: "Dimensions".into(), value: format!("{w} × {h} px") });
    Some(ViewerPayload::Image {
        width: w,
        height: h,
        channels: "HDR RGBA (Tone-mapped)".into(),
        bit_depth: 16,
        data_url: format!("data:image/png;base64,{}", B64.encode(&png_bytes)),
        frames: 1,
        note: Some("OpenEXR HDR radiance tone-mapped with Reinhard sRGB curve".into()),
    })
}

fn tone_map_u8(v: f32) -> u8 {
    let mapped = (v.max(0.0) / (1.0 + v.max(0.0))).powf(1.0 / 2.2);
    (mapped.clamp(0.0, 1.0) * 255.0) as u8
}

fn decode_psd(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 26 || &bytes[0..4] != b"8BPS" {
        return None;
    }
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    let channels = u16::from_be_bytes([bytes[12], bytes[13]]) as usize;
    let height = u32::from_be_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    let width = u32::from_be_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]);
    let depth = u16::from_be_bytes([bytes[22], bytes[23]]);
    let color_mode = u16::from_be_bytes([bytes[24], bytes[25]]);

    metadata.push(MetadataEntry { section: "Photoshop".into(), key: "Dimensions".into(), value: format!("{width} × {height} px") });
    metadata.push(MetadataEntry { section: "Photoshop".into(), key: "Channels".into(), value: format!("{channels} ({depth}-bit)") });

    let mut pos = 26usize;
    // Skip Color Mode Data, Image Resources, Layer & Mask Info
    for i in 0..3 {
        let len = if i == 2 && version == 2 {
            if pos + 8 > bytes.len() { return None; }
            let l = u64::from_be_bytes(bytes[pos..pos + 8].try_into().ok()?) as usize;
            pos += 8;
            l
        } else {
            if pos + 4 > bytes.len() { return None; }
            let l = u32::from_be_bytes(bytes[pos..pos + 4].try_into().ok()?) as usize;
            pos += 4;
            l
        };
        pos = pos.saturating_add(len);
    }

    if pos + 2 > bytes.len() || width == 0 || height == 0 || width > 16384 || height > 16384 {
        return None;
    }
    let compression = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]);
    pos += 2;
    if depth != 8 || !matches!(color_mode, 1 | 3) {
        return None;
    }

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
        // Skip scanline byte counts
        let count_entry = if version == 2 { 4 } else { 2 };
        pos += h * channels * count_entry;
        for p in 0..channels {
            let mut out = Vec::with_capacity(plane_size);
            while out.len() < plane_size && pos < bytes.len() {
                let n = bytes[pos] as i8;
                pos += 1;
                if n >= 0 {
                    let cnt = (n as usize) + 1;
                    let take = cnt.min(bytes.len() - pos).min(plane_size - out.len());
                    out.extend_from_slice(&bytes[pos..pos + take]);
                    pos += cnt.min(bytes.len() - pos);
                } else if n != -128 {
                    let cnt = ((-n) as usize) + 1;
                    if pos < bytes.len() {
                        let val = bytes[pos];
                        pos += 1;
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
    let png = encode_rgba_png(width, height, &rgba)?;
    Some(ViewerPayload::Image {
        width,
        height,
        channels: if channels >= 4 { "RGBA".into() } else { "RGB".into() },
        bit_depth: 8,
        data_url: format!("data:image/png;base64,{}", B64.encode(&png)),
        frames: 1,
        note: Some("Rendered from Photoshop merged composite channel".into()),
    })
}

fn decode_pcx(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 128 || bytes[0] != 0x0A {
        return None;
    }
    let bpp = bytes[3];
    let xmin = u16::from_le_bytes([bytes[4], bytes[5]]);
    let ymin = u16::from_le_bytes([bytes[6], bytes[7]]);
    let xmax = u16::from_le_bytes([bytes[8], bytes[9]]);
    let ymax = u16::from_le_bytes([bytes[10], bytes[11]]);
    let w = (xmax.saturating_sub(xmin) + 1) as usize;
    let h = (ymax.saturating_sub(ymin) + 1) as usize;
    let planes = bytes[65] as usize;
    let bpl = u16::from_le_bytes([bytes[66], bytes[67]]) as usize;
    if w == 0 || h == 0 || w > 8192 || h > 8192 || bpp != 8 {
        return None;
    }

    let mut pos = 128usize;
    let scan_bytes = planes * bpl;
    let mut decoded_rows = Vec::with_capacity(h * scan_bytes);
    while decoded_rows.len() < h * scan_bytes && pos < bytes.len() {
        let b = bytes[pos];
        pos += 1;
        if b >= 0xC0 {
            let count = (b & 0x3F) as usize;
            let val = *bytes.get(pos)?;
            pos += 1;
            decoded_rows.resize((decoded_rows.len() + count).min(h * scan_bytes), val);
        } else {
            decoded_rows.push(b);
        }
    }
    if decoded_rows.len() < h * scan_bytes {
        return None;
    }

    let mut rgba = vec![255u8; w * h * 4];
    if planes == 1 && bytes.len() >= 769 && bytes[bytes.len() - 769] == 0x0C {
        let pal = &bytes[bytes.len() - 768..];
        for y in 0..h {
            for x in 0..w {
                let idx = decoded_rows[y * scan_bytes + x] as usize;
                let dst = (y * w + x) * 4;
                rgba[dst] = pal[idx * 3];
                rgba[dst + 1] = pal[idx * 3 + 1];
                rgba[dst + 2] = pal[idx * 3 + 2];
            }
        }
    } else if planes >= 3 {
        for y in 0..h {
            let row = &decoded_rows[y * scan_bytes..];
            for x in 0..w {
                let dst = (y * w + x) * 4;
                rgba[dst] = row[x];
                rgba[dst + 1] = row[bpl + x];
                rgba[dst + 2] = row[2 * bpl + x];
                rgba[dst + 3] = if planes >= 4 { row[3 * bpl + x] } else { 255 };
            }
        }
    } else {
        return None;
    }
    let png = encode_rgba_png(w as u32, h as u32, &rgba)?;
    metadata.push(MetadataEntry { section: "PCX".into(), key: "Dimensions".into(), value: format!("{w} × {h} px") });
    Some(ViewerPayload::Image {
        width: w as u32,
        height: h as u32,
        channels: "RGBA".into(),
        bit_depth: 8,
        data_url: format!("data:image/png;base64,{}", B64.encode(&png)),
        frames: 1,
        note: None,
    })
}

fn decode_xpm(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let text = String::from_utf8_lossy(bytes);
    let mut strings = Vec::new();
    let mut rest = text.as_ref();
    while let Some(s) = rest.find('"') {
        rest = &rest[s + 1..];
        if let Some(e) = rest.find('"') {
            strings.push(&rest[..e]);
            rest = &rest[e + 1..];
        } else {
            break;
        }
    }
    let header: Vec<usize> = strings.first()?.split_whitespace().filter_map(|n| n.parse().ok()).collect();
    if header.len() < 4 {
        return None;
    }
    let (w, h, ncolors, cpp) = (header[0], header[1], header[2], header[3]);
    if w == 0 || h == 0 || cpp == 0 || strings.len() < 1 + ncolors + h {
        return None;
    }
    let mut palette = std::collections::HashMap::new();
    for line in &strings[1..1 + ncolors] {
        if line.len() >= cpp {
            let key = &line[..cpp];
            let color_part = line[cpp..].split("c ").nth(1).unwrap_or("#000000").trim();
            let rgba = parse_hex_color(color_part);
            palette.insert(key, rgba);
        }
    }
    let mut rgba = vec![0u8; w * h * 4];
    for (y, row) in strings[1 + ncolors..1 + ncolors + h].iter().enumerate() {
        for x in 0..w {
            let k = row.get(x * cpp..x * cpp + cpp).unwrap_or("");
            let px = palette.get(k).copied().unwrap_or([234, 255, 255, 255]);
            let dst = (y * w + x) * 4;
            rgba[dst..dst + 4].copy_from_slice(&px);
        }
    }
    let png = encode_rgba_png(w as u32, h as u32, &rgba)?;
    metadata.push(MetadataEntry { section: "XPM".into(), key: "Dimensions".into(), value: format!("{w} × {h} px") });
    Some(ViewerPayload::Image {
        width: w as u32,
        height: h as u32,
        channels: "RGBA".into(),
        bit_depth: 8,
        data_url: format!("data:image/png;base64,{}", B64.encode(&png)),
        frames: 1,
        note: None,
    })
}

fn parse_hex_color(c: &str) -> [u8; 4] {
    let t = c.trim();
    if t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("transparent") {
        return [0, 0, 0, 0];
    }
    if let Some(hex) = t.strip_prefix('#') {
        if hex.len() >= 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
            let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
            let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
            return [r, g, b, 255];
        }
    }
    if t.eq_ignore_ascii_case("white") { return [255, 255, 255, 255]; }
    [234, 255, 255, 255]
}

fn decode_icns(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 8 || &bytes[0..4] != b"icns" {
        return None;
    }
    let mut pos = 8usize;
    let mut best_png: Option<&[u8]> = None;
    let mut count = 0usize;
    while pos + 8 <= bytes.len() {
        let sz = u32::from_be_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        if sz < 8 || pos + sz > bytes.len() { break; }
        let payload = &bytes[pos + 8..pos + sz];
        count += 1;
        if payload.starts_with(b"\x89PNG\r\n\x1a\n") {
            if best_png.is_none_or(|b| payload.len() > b.len()) {
                best_png = Some(payload);
            }
        }
        pos += sz;
    }
    let png = best_png?;
    let (w, h) = image::ImageReader::new(Cursor::new(png)).with_guessed_format().ok()?.into_dimensions().ok()?;
    metadata.push(MetadataEntry { section: "ICNS".into(), key: "Icon Representations".into(), value: count.to_string() });
    Some(ViewerPayload::Image {
        width: w,
        height: h,
        channels: "RGBA".into(),
        bit_depth: 8,
        data_url: format!("data:image/png;base64,{}", B64.encode(png)),
        frames: count,
        note: Some("Extracted highest-resolution PNG frame from Apple ICNS bundle".into()),
    })
}

/// Extracts embedded full-res JPEG preview stream from Camera RAW files (CR2, CR3, NEF, ARW, DNG, ORF, RW2, RAF).
fn extract_camera_raw_preview(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let mut best: Option<&[u8]> = None;
    let mut idx = 0usize;
    let limit = bytes.len().min(24 * 1024 * 1024);
    while idx + 4 < limit {
        if bytes[idx] == 0xFF && bytes[idx + 1] == 0xD8 && bytes[idx + 2] == 0xFF {
            // Find matching EOI FF D9
            if let Some(rel_end) = bytes[idx + 3..limit]
                .windows(2)
                .position(|w| w[0] == 0xFF && w[1] == 0xD9)
            {
                let end = idx + 3 + rel_end + 2;
                let cand = &bytes[idx..end];
                if cand.len() > 8192 && best.is_none_or(|b| cand.len() > b.len()) {
                    if image::ImageReader::new(Cursor::new(cand))
                        .with_guessed_format()
                        .ok()
                        .and_then(|r| r.into_dimensions().ok())
                        .is_some()
                    {
                        best = Some(cand);
                    }
                }
                idx = end;
                continue;
            }
        }
        idx += 1;
    }

    let jpeg = best?;
    let (w, h) = image::ImageReader::new(Cursor::new(jpeg))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    metadata.push(MetadataEntry { section: "Camera RAW".into(), key: "RAW Type".into(), value: format_id.to_ascii_uppercase() });
    metadata.push(MetadataEntry { section: "Camera RAW".into(), key: "Preview Dimensions".into(), value: format!("{w} × {h} px") });
    Some(ViewerPayload::Image {
        width: w,
        height: h,
        channels: "RGB (Embedded RAW Preview)".into(),
        bit_depth: 8,
        data_url: format!("data:image/jpeg;base64,{}", B64.encode(jpeg)),
        frames: 1,
        note: Some(format!("Extracted embedded full-res preview from {} RAW container", format_id.to_ascii_uppercase())),
    })
}
