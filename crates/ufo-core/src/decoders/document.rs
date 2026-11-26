use std::collections::BTreeMap;
use std::io::{Cursor, Read as _};

use flate2::read::ZlibDecoder;

use crate::decoders::text::{decode_to_utf8, parse_delimited_table};
use crate::model::{DocSection, MetadataEntry, TableSheet, ViewerPayload};

/// Decodes PDF, DOCX/ODT/EPUB/FB2/XPS, XLSX/ODS spreadsheets, PPTX/ODP, CFB/OLE2, RTF, EML/VCF/ICS.
pub fn decode_document(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    match format_id {
        "pdf" => decode_pdf(bytes, metadata),
        "docx" | "docm" | "dotx" | "dotm" => decode_docx(bytes, metadata),
        "xlsx" | "xlsm" | "xltx" | "xltm" => decode_xlsx(bytes, metadata),
        "pptx" | "pptm" | "ppsx" | "potx" => decode_pptx(bytes, metadata),
        "odt" | "ods" | "odp" => decode_odf(bytes, format_id, metadata),
        "epub" => decode_epub(bytes, metadata),
        "fb2" => decode_fb2(bytes, metadata),
        "xps" | "oxps" => decode_xps(bytes, metadata),
        "doc" | "xls" | "ppt" | "msg" | "msi" | "msp" | "pps" => decode_cfb(bytes, format_id, metadata),
        "rtf" => decode_rtf(bytes, metadata),
        "eml" | "vcf" | "ics" | "ical" | "vcs" => decode_structured_message(bytes, format_id, metadata),
        _ => None,
    }
}

fn decode_pdf(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let text_lossy = String::from_utf8_lossy(&bytes[..bytes.len().min(8 * 1024 * 1024)]);
    let version = text_lossy
        .lines()
        .next()
        .unwrap_or("%PDF-1.x")
        .trim_start_matches("%PDF-")
        .trim()
        .to_string();

    let page_markers = text_lossy.matches("/Type /Page").count();
    let pages_fallback = text_lossy.matches("/Type/Page").count();
    let page_count = (page_markers + pages_fallback).max(1);
    let encrypted = text_lossy.contains("/Encrypt");

    let title = extract_pdf_info_field(&text_lossy, "/Title");
    let author = extract_pdf_info_field(&text_lossy, "/Author");
    let producer = extract_pdf_info_field(&text_lossy, "/Producer");

    metadata.push(MetadataEntry { section: "PDF".into(), key: "PDF Version".into(), value: version.clone() });
    metadata.push(MetadataEntry { section: "PDF".into(), key: "Estimated Pages".into(), value: page_count.to_string() });
    metadata.push(MetadataEntry { section: "PDF".into(), key: "Encrypted".into(), value: if encrypted { "Yes" } else { "No" }.into() });
    if let Some(a) = &author {
        metadata.push(MetadataEntry { section: "PDF".into(), key: "Author".into(), value: a.clone() });
    }
    if let Some(p) = &producer {
        metadata.push(MetadataEntry { section: "PDF".into(), key: "Producer".into(), value: p.clone() });
    }

    let extracted_text = extract_pdf_text_streams(bytes);

    Some(ViewerPayload::Document {
        doc_type: format!("PDF v{version}"),
        title,
        page_count,
        use_native_pdf_blob: true,
        sections: if extracted_text.is_empty() {
            Vec::new()
        } else {
            vec![DocSection { title: "Extracted Text Stream".into(), body: extracted_text.clone() }]
        },
        extracted_text,
    })
}

fn extract_pdf_info_field(text: &str, key: &str) -> Option<String> {
    let idx = text.find(key)?;
    let rest = text[idx + key.len()..].trim_start();
    if let Some(inner) = rest.strip_prefix('(') {
        let end = inner.find(')')?;
        let clean = inner[..end].trim().to_string();
        if !clean.is_empty() {
            return Some(clean);
        }
    }
    None
}

fn extract_pdf_text_streams(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut pos = 0usize;
    let limit = bytes.len().min(6 * 1024 * 1024);
    while pos < limit && out.len() < 32_000 {
        let Some(rel) = find_bytes(&bytes[pos..limit], b"stream") else { break };
        let s_start = pos + rel + 6;
        let stream_data_start = if bytes.get(s_start..s_start + 2) == Some(b"\r\n") {
            s_start + 2
        } else if bytes.get(s_start) == Some(&b'\n') {
            s_start + 1
        } else {
            pos = s_start;
            continue;
        };
        let Some(rel_end) = find_bytes(&bytes[stream_data_start..limit], b"endstream") else { break };
        let raw = &bytes[stream_data_start..stream_data_start + rel_end];
        let trimmed = raw.strip_suffix(b"\r\n").or_else(|| raw.strip_suffix(b"\n")).unwrap_or(raw);

        // Try raw content or FlateDecode zlib decompression
        let mut decompressed = Vec::new();
        let content_bytes = if trimmed.windows(2).any(|w| w == b"BT") {
            trimmed
        } else {
            let mut dec = ZlibDecoder::new(trimmed);
            if dec.read_to_end(&mut decompressed).is_ok() && decompressed.windows(2).any(|w| w == b"BT") {
                decompressed.as_slice()
            } else {
                pos = stream_data_start + rel_end + 9;
                continue;
            }
        };

        let s = String::from_utf8_lossy(content_bytes);
        let mut rest = s.as_ref();
        while let Some(p) = rest.find('(') {
            rest = &rest[p + 1..];
            if let Some(e) = rest.find(')') {
                let frag = &rest[..e];
                if frag.chars().all(|c| c.is_ascii_graphic() || c == ' ') && !frag.is_empty() {
                    out.push_str(frag);
                    out.push(' ');
                }
                rest = &rest[e + 1..];
            } else {
                break;
            }
        }
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        pos = stream_data_start + rel_end + 9;
    }
    out.trim().to_string()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn decode_docx(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut xml = String::new();
    arch.by_name("word/document.xml").ok()?.read_to_string(&mut xml).ok()?;
    let doc = roxmltree::Document::parse(&xml).ok()?;

    let mut sections = Vec::new();
    let mut paragraphs = Vec::new();
    for node in doc.descendants() {
        if node.has_tag_name("p") {
            let mut line = String::new();
            for t in node.descendants().filter(|n| n.has_tag_name("t")) {
                if let Some(txt) = t.text() {
                    line.push_str(txt);
                }
            }
            if !line.trim().is_empty() {
                paragraphs.push(line);
            }
        }
    }
    let body = paragraphs.join("\n\n");
    sections.push(DocSection { title: "Document Body".into(), body: body.clone() });
    metadata.push(MetadataEntry { section: "DOCX".into(), key: "Paragraphs".into(), value: paragraphs.len().to_string() });

    Some(ViewerPayload::Document {
        doc_type: "Microsoft Word OOXML (.docx)".into(),
        title: None,
        page_count: 1,
        use_native_pdf_blob: false,
        sections,
        extracted_text: body,
    })
}

fn decode_xlsx(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    // 1. Read shared strings
    let mut shared = Vec::new();
    if let Ok(mut ss_file) = arch.by_name("xl/sharedStrings.xml") {
        let mut ss_xml = String::new();
        if ss_file.read_to_string(&mut ss_xml).is_ok() {
            if let Ok(doc) = roxmltree::Document::parse(&ss_xml) {
                for si in doc.descendants().filter(|n| n.has_tag_name("si")) {
                    let s: String = si.descendants().filter(|n| n.has_tag_name("t")).filter_map(|t| t.text()).collect();
                    shared.push(s);
                }
            }
        }
    }

    // 2. Collect sheet names from workbook.xml
    let mut sheet_names = Vec::new();
    if let Ok(mut wb) = arch.by_name("xl/workbook.xml") {
        let mut wb_xml = String::new();
        if wb.read_to_string(&mut wb_xml).is_ok() {
            if let Ok(doc) = roxmltree::Document::parse(&wb_xml) {
                for s in doc.descendants().filter(|n| n.has_tag_name("sheet")) {
                    if let Some(n) = s.attribute("name") {
                        sheet_names.push(n.to_string());
                    }
                }
            }
        }
    }

    let mut sheet_paths: Vec<String> = (0..arch.len())
        .filter_map(|i| arch.by_index(i).ok().and_then(|f| f.name().ok().map(|n| n.into_owned())))
        .filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml"))
        .collect();
    sheet_paths.sort();

    let mut sheets = Vec::new();
    for (idx, path) in sheet_paths.iter().take(16).enumerate() {
        let Ok(mut f) = arch.by_name(path) else { continue };
        let mut s_xml = String::new();
        if f.read_to_string(&mut s_xml).is_err() { continue; }
        let Ok(doc) = roxmltree::Document::parse(&s_xml) else { continue };

        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut max_cols = 0usize;
        for row_node in doc.descendants().filter(|n| n.has_tag_name("row")).take(1500) {
            let mut row_vals: BTreeMap<usize, String> = BTreeMap::new();
            for c in row_node.children().filter(|n| n.has_tag_name("c")) {
                let col_idx = c.attribute("r").map(col_ref_to_index).unwrap_or(row_vals.len());
                let t_attr = c.attribute("t").unwrap_or("");
                let val_str = if t_attr == "inlineStr" {
                    c.descendants().filter(|n| n.has_tag_name("t")).filter_map(|t| t.text()).collect::<String>()
                } else {
                    let raw = c.children().find(|n| n.has_tag_name("v")).and_then(|v| v.text()).unwrap_or("");
                    if t_attr == "s" {
                        raw.parse::<usize>().ok().and_then(|i| shared.get(i)).cloned().unwrap_or_default()
                    } else {
                        raw.to_string()
                    }
                };
                max_cols = max_cols.max(col_idx + 1);
                row_vals.insert(col_idx, val_str);
            }
            let mut vec_row = vec![String::new(); max_cols.max(1)];
            for (ci, v) in row_vals {
                if ci < vec_row.len() { vec_row[ci] = v; }
            }
            rows.push(vec_row);
        }
        for r in &mut rows {
            r.resize(max_cols.max(1), String::new());
        }
        let columns = if !rows.is_empty() {
            rows.remove(0)
        } else {
            (0..max_cols.max(1)).map(|c| format!("Col {}", c + 1)).collect()
        };
        let sname = sheet_names.get(idx).cloned().unwrap_or_else(|| format!("Sheet {}", idx + 1));
        sheets.push(TableSheet {
            name: sname,
            columns,
            total_rows: rows.len(),
            truncated: false,
            rows,
        });
    }

    if sheets.is_empty() {
        return None;
    }
    metadata.push(MetadataEntry { section: "Excel".into(), key: "Worksheets".into(), value: sheets.len().to_string() });
    Some(ViewerPayload::Table {
        format_label: "Microsoft Excel Workbook (.xlsx)".into(),
        sheets,
        active_sheet: 0,
    })
}

fn col_ref_to_index(r: &str) -> usize {
    let mut col = 0usize;
    for b in r.bytes() {
        if b.is_ascii_alphabetic() {
            col = col * 26 + ((b.to_ascii_uppercase() - b'A') as usize + 1);
        } else {
            break;
        }
    }
    col.saturating_sub(1)
}

fn decode_pptx(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut slide_paths: Vec<String> = (0..arch.len())
        .filter_map(|i| arch.by_index(i).ok().and_then(|f| f.name().ok().map(|n| n.into_owned())))
        .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
        .collect();
    slide_paths.sort();

    let mut sections = Vec::new();
    let mut all_text = String::new();
    for (idx, path) in slide_paths.iter().enumerate() {
        let Ok(mut f) = arch.by_name(path) else { continue };
        let mut xml = String::new();
        if f.read_to_string(&mut xml).is_err() { continue; }
        let Ok(doc) = roxmltree::Document::parse(&xml) else { continue };
        let mut slide_lines = Vec::new();
        for p in doc.descendants().filter(|n| n.has_tag_name("p")) {
            let line: String = p.descendants().filter(|n| n.has_tag_name("t")).filter_map(|t| t.text()).collect();
            if !line.trim().is_empty() {
                slide_lines.push(line);
            }
        }
        let body = slide_lines.join("\n");
        all_text.push_str(&format!("--- Slide {} ---\n{}\n\n", idx + 1, body));
        sections.push(DocSection { title: format!("Slide {}", idx + 1), body });
    }
    metadata.push(MetadataEntry { section: "PowerPoint".into(), key: "Slides".into(), value: sections.len().to_string() });
    Some(ViewerPayload::Document {
        doc_type: "Microsoft PowerPoint (.pptx)".into(),
        title: None,
        page_count: sections.len(),
        use_native_pdf_blob: false,
        sections,
        extracted_text: all_text,
    })
}

fn decode_odf(bytes: &[u8], format_id: &str, metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut xml = String::new();
    arch.by_name("content.xml").ok()?.read_to_string(&mut xml).ok()?;
    let doc = roxmltree::Document::parse(&xml).ok()?;

    if format_id == "ods" {
        let mut sheets = Vec::new();
        for table in doc.descendants().filter(|n| n.has_tag_name("table")).take(12) {
            let name = table.attribute("name").unwrap_or("Sheet").to_string();
            let mut rows = Vec::new();
            for tr in table.descendants().filter(|n| n.has_tag_name("table-row")).take(1000) {
                let row: Vec<String> = tr
                    .children()
                    .filter(|n| n.has_tag_name("table-cell"))
                    .map(|c| c.descendants().filter_map(|t| t.text()).collect::<Vec<_>>().join(" "))
                    .collect();
                if row.iter().any(|s| !s.is_empty()) {
                    rows.push(row);
                }
            }
            if !rows.is_empty() {
                let cols = rows.remove(0);
                sheets.push(TableSheet { name, columns: cols, total_rows: rows.len(), truncated: false, rows });
            }
        }
        if !sheets.is_empty() {
            return Some(ViewerPayload::Table {
                format_label: "OpenDocument Spreadsheet (.ods)".into(),
                sheets,
                active_sheet: 0,
            });
        }
    }

    let mut lines = Vec::new();
    for p in doc.descendants().filter(|n| n.has_tag_name("p") || n.has_tag_name("h")) {
        let line: String = p.descendants().filter_map(|n| n.text()).collect();
        if !line.trim().is_empty() {
            lines.push(line);
        }
    }
    let body = lines.join("\n\n");
    metadata.push(MetadataEntry { section: "OpenDocument".into(), key: "Format".into(), value: format_id.to_ascii_uppercase() });
    Some(ViewerPayload::Document {
        doc_type: format!("OpenDocument (.{format_id})"),
        title: None,
        page_count: 1,
        use_native_pdf_blob: false,
        sections: vec![DocSection { title: "Document Content".into(), body: body.clone() }],
        extracted_text: body,
    })
}

fn decode_epub(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut html_paths: Vec<String> = (0..arch.len())
        .filter_map(|i| arch.by_index(i).ok().and_then(|f| f.name().ok().map(|n| n.into_owned())))
        .filter(|n| n.ends_with(".xhtml") || n.ends_with(".html") || n.ends_with(".htm"))
        .collect();
    html_paths.sort();

    let mut sections = Vec::new();
    let mut full = String::new();
    for (idx, path) in html_paths.iter().take(40).enumerate() {
        let Ok(mut f) = arch.by_name(path) else { continue };
        let mut raw = String::new();
        if f.read_to_string(&mut raw).is_err() { continue; }
        let stripped = strip_html_tags(&raw);
        if !stripped.trim().is_empty() {
            full.push_str(&stripped);
            full.push_str("\n\n");
            sections.push(DocSection {
                title: path.rsplit('/').next().unwrap_or(&format!("Chapter {}", idx + 1)).to_string(),
                body: stripped,
            });
        }
    }
    metadata.push(MetadataEntry { section: "EPUB".into(), key: "Chapters / HTML Docs".into(), value: html_paths.len().to_string() });
    Some(ViewerPayload::Document {
        doc_type: "EPUB E-Book".into(),
        title: None,
        page_count: sections.len().max(1),
        use_native_pdf_blob: false,
        sections,
        extracted_text: full,
    })
}

fn decode_fb2(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    let doc = roxmltree::Document::parse(&text).ok()?;
    let title = doc.descendants().find(|n| n.has_tag_name("book-title")).and_then(|n| n.text()).map(|s| s.to_string());
    let mut paragraphs = Vec::new();
    for p in doc.descendants().filter(|n| n.has_tag_name("p")) {
        let t: String = p.descendants().filter_map(|n| n.text()).collect();
        if !t.trim().is_empty() {
            paragraphs.push(t);
        }
    }
    let body = paragraphs.join("\n\n");
    metadata.push(MetadataEntry { section: "FictionBook".into(), key: "Paragraphs".into(), value: paragraphs.len().to_string() });
    Some(ViewerPayload::Document {
        doc_type: "FictionBook XML (.fb2)".into(),
        title,
        page_count: 1,
        use_native_pdf_blob: false,
        sections: vec![DocSection { title: "Book Content".into(), body: body.clone() }],
        extracted_text: body,
    })
}

fn decode_xps(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut pages: Vec<String> = (0..arch.len())
        .filter_map(|i| arch.by_index(i).ok().and_then(|f| f.name().ok().map(|n| n.into_owned())))
        .filter(|n| n.ends_with(".fpage"))
        .collect();
    pages.sort();
    let mut sections = Vec::new();
    let mut full = String::new();
    for (idx, p) in pages.iter().take(40).enumerate() {
        let Ok(mut f) = arch.by_name(p) else { continue };
        let mut xml = String::new();
        if f.read_to_string(&mut xml).is_err() { continue; }
        if let Ok(doc) = roxmltree::Document::parse(&xml) {
            let line: Vec<String> = doc
                .descendants()
                .filter(|n| n.has_tag_name("Glyphs"))
                .filter_map(|g| g.attribute("UnicodeString").map(|s| s.to_string()))
                .collect();
            let txt = line.join(" ");
            full.push_str(&txt);
            full.push('\n');
            sections.push(DocSection { title: format!("Page {}", idx + 1), body: txt });
        }
    }
    metadata.push(MetadataEntry { section: "XPS".into(), key: "Fixed Pages".into(), value: pages.len().to_string() });
    Some(ViewerPayload::Document {
        doc_type: "XPS / OpenXPS Document".into(),
        title: None,
        page_count: pages.len().max(1),
        use_native_pdf_blob: false,
        sections,
        extracted_text: full,
    })
}

fn decode_cfb(bytes: &[u8], format_id: &str, metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut comp = cfb::CompoundFile::open(Cursor::new(bytes)).ok()?;
    let mut streams = Vec::new();
    let mut collected_text = String::new();
    let entries: Vec<_> = comp.walk().filter(|e| e.is_stream()).map(|e| (e.path().display().to_string(), e.len())).collect();
    for (path, len) in &entries {
        streams.push(format!("{path} ({len} bytes)"));
        if *len > 0 && *len <= 2 * 1024 * 1024 && collected_text.len() < 16_000 {
            if let Ok(mut s) = comp.open_stream(path) {
                let mut buf = Vec::new();
                if s.read_to_end(&mut buf).is_ok() {
                    let text_frags = crate::decoders::hex_bin::extract_strings(&buf, 80);
                    for f in text_frags {
                        if f.value.len() >= 8 {
                            collected_text.push_str(&f.value);
                            collected_text.push('\n');
                        }
                    }
                }
            }
        }
    }
    metadata.push(MetadataEntry { section: "OLE2 / CFB".into(), key: "Streams".into(), value: streams.len().to_string() });
    Some(ViewerPayload::Document {
        doc_type: format!("Microsoft OLE2/CFB (.{format_id})"),
        title: None,
        page_count: 1,
        use_native_pdf_blob: false,
        sections: vec![
            DocSection { title: "OLE2 Compound Streams".into(), body: streams.join("\n") },
            DocSection { title: "Extracted Text Strings".into(), body: collected_text.clone() },
        ],
        extracted_text: collected_text,
    })
}

fn decode_rtf(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut depth = 0i32;
    let mut skip_group = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                depth += 1;
                let rest: String = chars.clone().take(12).collect();
                if rest.starts_with("\\fonttbl") || rest.starts_with("\\colortbl") || rest.starts_with("\\stylesheet") || rest.starts_with("\\*") {
                    skip_group = true;
                }
            }
            '}' => {
                depth -= 1;
                if depth <= 1 {
                    skip_group = false;
                }
            }
            '\\' if !skip_group => {
                let mut cmd = String::new();
                while let Some(&n) = chars.peek() {
                    if n.is_ascii_alphanumeric() || n == '-' {
                        cmd.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
                if cmd == "par" || cmd == "line" {
                    out.push('\n');
                } else if cmd == "tab" {
                    out.push('\t');
                } else if cmd.is_empty() {
                    if let Some(esc) = chars.next() {
                        out.push(esc);
                    }
                }
            }
            _ if !skip_group => {
                if c != '\r' && c != '\n' {
                    out.push(c);
                }
            }
            _ => {}
        }
    }
    let clean = out.trim().to_string();
    metadata.push(MetadataEntry { section: "RTF".into(), key: "Extracted Chars".into(), value: clean.len().to_string() });
    Some(ViewerPayload::Document {
        doc_type: "Rich Text Format (.rtf)".into(),
        title: None,
        page_count: 1,
        use_native_pdf_blob: false,
        sections: vec![DocSection { title: "Document Text".into(), body: clean.clone() }],
        extracted_text: clean,
    })
}

fn decode_structured_message(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    if format_id == "eml" {
        let split = text.find("\r\n\r\n").or_else(|| text.find("\n\n"));
        let (hdr, body) = match split {
            Some(i) => (&text[..i], text[i..].trim()),
            None => (text.as_ref(), ""),
        };
        let mut subject = None;
        for line in hdr.lines() {
            if let Some((k, v)) = line.split_once(':') {
                let kl = k.trim();
                if matches!(kl.to_ascii_lowercase().as_str(), "subject" | "from" | "to" | "date") {
                    metadata.push(MetadataEntry { section: "Email".into(), key: kl.into(), value: v.trim().into() });
                    if kl.eq_ignore_ascii_case("subject") {
                        subject = Some(v.trim().to_string());
                    }
                }
            }
        }
        return Some(ViewerPayload::Document {
            doc_type: "RFC-822 Email (.eml)".into(),
            title: subject,
            page_count: 1,
            use_native_pdf_blob: false,
            sections: vec![
                DocSection { title: "Headers".into(), body: hdr.to_string() },
                DocSection { title: "Message Body".into(), body: body.to_string() },
            ],
            extracted_text: text.into_owned(),
        });
    }
    // VCF / ICS key-value table
    let csv_like = text
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| format!("{k}\t{v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let sheet = parse_delimited_table(&format!("Property\tValue\n{csv_like}"), format_id.to_ascii_uppercase(), b'\t');
    Some(ViewerPayload::Table {
        format_label: format!("{} Structured Record", format_id.to_ascii_uppercase()),
        sheets: vec![sheet],
        active_sheet: 0,
    })
}

fn strip_html_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                if !out.ends_with('\n') && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}
