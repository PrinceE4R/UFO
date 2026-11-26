use std::fmt::Write as _;
use std::io::Read as _;

use flate2::read::ZlibDecoder;
use ttf_parser::{Face, OutlineBuilder, Tag};

use crate::model::{MetadataEntry, ViewerPayload};

struct SvgPathBuilder {
    d: String,
}

impl OutlineBuilder for SvgPathBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        let _ = write!(self.d, "M{x:.1},{:.1} ", -y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let _ = write!(self.d, "L{x:.1},{:.1} ", -y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let _ = write!(self.d, "Q{x1:.1},{:.1} {x:.1},{:.1} ", -y1, -y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let _ = write!(
            self.d,
            "C{x1:.1},{:.1} {x2:.1},{:.1} {x:.1},{:.1} ",
            -y1, -y2, -y
        );
    }
    fn close(&mut self) {
        self.d.push_str("Z ");
    }
}

/// Decodes TTF, OTF, TTC, and WOFF 1.0 fonts using ttf-parser, extracting tables and SVG glyph specimens.
pub fn decode_font(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let owned_sfnt: Vec<u8>;
    let (sfnt_bytes, supports_web_font) = if bytes.starts_with(b"wOFF") {
        owned_sfnt = unwrap_woff1(bytes)?;
        (owned_sfnt.as_slice(), true)
    } else if bytes.starts_with(b"wOF2") {
        return Some(ViewerPayload::Font {
            family: "WOFF2 Web Font".into(),
            subfamily: "Brotli-compressed SFNT".into(),
            glyph_count: 0,
            upem: 1000,
            ascender: 800,
            descender: -200,
            tables: vec!["WOFF2".into()],
            preview_glyphs_svg: String::new(),
            supports_web_font: true,
        });
    } else {
        (bytes, true)
    };

    let face = Face::parse(sfnt_bytes, 0).ok()?;
    let mut family = String::new();
    let mut subfamily = String::new();
    for name in face.names() {
        if let Some(s) = name.to_string() {
            if name.name_id == ttf_parser::name_id::FAMILY && family.is_empty() {
                family = s;
            } else if name.name_id == ttf_parser::name_id::SUBFAMILY && subfamily.is_empty() {
                subfamily = s;
            }
        }
    }
    if family.is_empty() {
        family = "Untitled Typeface".into();
    }
    if subfamily.is_empty() {
        subfamily = "Regular".into();
    }

    let upem = face.units_per_em();
    let ascender = face.ascender();
    let descender = face.descender();
    let glyph_count = face.number_of_glyphs() as usize;

    let mut tables = Vec::new();
    for t in face.raw_face().table_records {
        let bytes = t.tag.to_bytes();
        let tag_str = String::from_utf8_lossy(&bytes).trim().to_string();
        if !tag_str.is_empty() {
            tables.push(tag_str);
        }
    }
    // Deduplicate while preserving order
    tables.dedup();

    metadata.push(MetadataEntry { section: "Font Metrics".into(), key: "Family".into(), value: family.clone() });
    metadata.push(MetadataEntry { section: "Font Metrics".into(), key: "Style".into(), value: subfamily.clone() });
    metadata.push(MetadataEntry { section: "Font Metrics".into(), key: "Glyphs".into(), value: glyph_count.to_string() });
    metadata.push(MetadataEntry { section: "Font Metrics".into(), key: "Units Per Em".into(), value: upem.to_string() });

    let preview_glyphs_svg = render_specimen_svg(&face, upem);

    Some(ViewerPayload::Font {
        family,
        subfamily,
        glyph_count,
        upem,
        ascender,
        descender,
        tables,
        preview_glyphs_svg,
        supports_web_font,
    })
}

fn render_specimen_svg(face: &Face<'_>, upem: u16) -> String {
    let sample_chars = "UFO Aa Bb Gg Rr 0123456789";
    let upem_f = (upem as f32).max(256.0);
    let mut cursor_x = 0.0f32;
    let mut paths = String::new();

    for ch in sample_chars.chars() {
        if ch == ' ' {
            cursor_x += upem_f * 0.32;
            continue;
        }
        if let Some(gid) = face.glyph_index(ch) {
            let mut builder = SvgPathBuilder { d: String::new() };
            face.outline_glyph(gid, &mut builder);
            if !builder.d.is_empty() {
                let _ = write!(
                    paths,
                    "<g transform=\"translate({cursor_x:.1},0)\"><path d=\"{}\" fill=\"#EAFFFF\"/></g>",
                    builder.d
                );
            }
            let adv = face.glyph_hor_advance(gid).unwrap_or((upem_f * 0.6) as u16) as f32;
            cursor_x += adv.max(upem_f * 0.25);
        } else {
            cursor_x += upem_f * 0.5;
        }
    }

    let width = cursor_x.max(upem_f);
    let min_y = -(upem_f * 1.15);
    let height = upem_f * 1.45;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"-40 {min_y:.0} {w:.0} {height:.0}\" class=\"ufo-font-svg\">{paths}</svg>",
        w = width + 80.0
    )
}

/// Unwraps WOFF 1.0 directory & zlib-compressed SFNT tables into a valid TTF/OTF byte buffer.
fn unwrap_woff1(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < 44 {
        return None;
    }
    let flavor = &bytes[4..8];
    let num_tables = u16::from_be_bytes([bytes[12], bytes[13]]) as usize;
    if bytes.len() < 44 + num_tables * 20 || num_tables == 0 || num_tables > 128 {
        return None;
    }

    struct WoffEntry {
        tag: [u8; 4],
        offset: usize,
        comp_len: usize,
        orig_len: usize,
        orig_checksum: u32,
    }

    let mut entries = Vec::with_capacity(num_tables);
    for i in 0..num_tables {
        let base = 44 + i * 20;
        let tag = [bytes[base], bytes[base + 1], bytes[base + 2], bytes[base + 3]];
        let offset = u32::from_be_bytes([bytes[base + 4], bytes[base + 5], bytes[base + 6], bytes[base + 7]]) as usize;
        let comp_len = u32::from_be_bytes([bytes[base + 8], bytes[base + 9], bytes[base + 10], bytes[base + 11]]) as usize;
        let orig_len = u32::from_be_bytes([bytes[base + 12], bytes[base + 13], bytes[base + 14], bytes[base + 15]]) as usize;
        let orig_checksum = u32::from_be_bytes([bytes[base + 16], bytes[base + 17], bytes[base + 18], bytes[base + 19]]);
        if offset.saturating_add(comp_len) > bytes.len() || orig_len > 32 * 1024 * 1024 {
            return None;
        }
        entries.push(WoffEntry { tag, offset, comp_len, orig_len, orig_checksum });
    }

    let header_len = 12 + num_tables * 16;
    let mut out = Vec::with_capacity(header_len + entries.iter().map(|e| e.orig_len + 4).sum::<usize>());
    out.extend_from_slice(flavor);
    out.extend_from_slice(&(num_tables as u16).to_be_bytes());
    let mut max_pow2 = 1u16;
    let mut entry_selector = 0u16;
    while max_pow2 * 2 <= num_tables as u16 {
        max_pow2 *= 2;
        entry_selector += 1;
    }
    let search_range = max_pow2 * 16;
    let range_shift = (num_tables as u16) * 16 - search_range;
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    let mut table_blobs = Vec::with_capacity(num_tables);
    let mut cur_offset = header_len;
    for e in &entries {
        let src = &bytes[e.offset..e.offset + e.comp_len];
        let decoded = if e.comp_len < e.orig_len {
            let mut dec = ZlibDecoder::new(src);
            let mut buf = Vec::with_capacity(e.orig_len);
            dec.read_to_end(&mut buf).ok()?;
            buf
        } else {
            src.to_vec()
        };
        let padded_len = (decoded.len() + 3) & !3;
        out.extend_from_slice(&e.tag);
        out.extend_from_slice(&e.orig_checksum.to_be_bytes());
        out.extend_from_slice(&(cur_offset as u32).to_be_bytes());
        out.extend_from_slice(&(decoded.len() as u32).to_be_bytes());
        cur_offset += padded_len;
        table_blobs.push((decoded, padded_len));
    }

    for (blob, padded_len) in table_blobs {
        let pad = padded_len - blob.len();
        out.extend_from_slice(&blob);
        out.resize(out.len() + pad, 0);
    }
    let _ = Tag::from_bytes(b"head");
    Some(out)
}
