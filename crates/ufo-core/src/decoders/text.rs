use std::borrow::Cow;

use encoding_rs::{UTF_16BE, UTF_16LE};

use crate::model::{
    HighlightedLine, MetadataEntry, NotebookCell, TableSheet, TextPage, TextToken, TreeNode,
    ViewerPayload,
};

const INITIAL_LINE_LIMIT: usize = 500;

/// Decodes arbitrary byte buffer to UTF-8 text with BOM / chardetng encoding detection.
pub fn decode_to_utf8(bytes: &[u8]) -> (Cow<'_, str>, String, bool) {
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        let rest = &bytes[3..];
        return (String::from_utf8_lossy(rest), "UTF-8 (BOM)".into(), true);
    }
    if bytes.starts_with(b"\xFF\xFE") {
        let (cow, _, _) = UTF_16LE.decode(&bytes[2..]);
        return (Cow::Owned(cow.into_owned()), "UTF-16LE".into(), true);
    }
    if bytes.starts_with(b"\xFE\xFF") {
        let (cow, _, _) = UTF_16BE.decode(&bytes[2..]);
        return (Cow::Owned(cow.into_owned()), "UTF-16BE".into(), true);
    }
    if simdutf8::basic::from_utf8(bytes).is_ok() {
        let s = unsafe { std::str::from_utf8_unchecked(bytes) };
        return (Cow::Borrowed(s), "UTF-8".into(), false);
    }
    let sample = &bytes[..bytes.len().min(32 * 1024)];
    let mut det = chardetng::EncodingDetector::new();
    det.feed(sample, bytes.len() <= sample.len());
    let enc = det.guess(None, true);
    let (cow, _, _) = enc.decode(bytes);
    (Cow::Owned(cow.into_owned()), enc.name().to_string(), false)
}

pub fn detect_line_ending(text: &str) -> &'static str {
    let sample = &text[..text.len().min(65536)];
    if sample.contains("\r\n") {
        "CRLF (\\r\\n)"
    } else if sample.contains('\n') {
        "LF (\\n)"
    } else if sample.contains('\r') {
        "CR (\\r)"
    } else {
        "None"
    }
}

pub fn slice_text_page(bytes: &[u8], start_line: usize, line_count: usize, lang: &str) -> TextPage {
    let (text, _, _) = decode_to_utf8(bytes);
    let all_lines: Vec<&str> = text.lines().collect();
    let total = all_lines.len();
    let start = start_line.min(total);
    let end = (start + line_count.clamp(20, 1000)).min(total);
    let lines = all_lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| HighlightedLine {
            line_no: start + i + 1,
            tokens: tokenize_line(line, lang),
        })
        .collect();
    TextPage {
        start_line: start,
        total_lines: total,
        lines,
    }
}

pub fn decode_text_viewer(
    bytes: &[u8],
    format_id: &str,
    lang: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    // Handle CSV / TSV tables directly
    if matches!(format_id, "csv" | "tsv") {
        let (text, enc, _) = decode_to_utf8(bytes);
        let delim = if format_id == "tsv" { b'\t' } else { detect_csv_delimiter(&text) };
        let sheet = parse_delimited_table(&text, format_id.to_ascii_uppercase(), delim);
        metadata.push(MetadataEntry { section: "Text".into(), key: "Encoding".into(), value: enc });
        metadata.push(MetadataEntry { section: "Table".into(), key: "Columns".into(), value: sheet.columns.len().to_string() });
        metadata.push(MetadataEntry { section: "Table".into(), key: "Rows".into(), value: sheet.total_rows.to_string() });
        return ViewerPayload::Table {
            format_label: format!("{} Delimited Table", format_id.to_ascii_uppercase()),
            sheets: vec![sheet],
            active_sheet: 0,
        };
    }

    let (text, encoding, has_bom) = decode_to_utf8(bytes);
    let line_ending = detect_line_ending(&text).to_string();

    // Handle JSON / GeoJSON / HAR / Jupyter / TOML / RON / PLIST structured trees
    if format_id == "ipynb" {
        if let Some(cells) = parse_ipynb(&text) {
            metadata.push(MetadataEntry { section: "Notebook".into(), key: "Cells".into(), value: cells.len().to_string() });
            return build_text_payload(&text, "json", encoding, has_bom, line_ending, None, None, Some(cells), metadata);
        }
    }

    if matches!(format_id, "json" | "geojson" | "har" | "webmanifest") {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
            if text.len() <= 2 * 1024 * 1024 {
                let pretty = serde_json::to_string_pretty(&val).unwrap_or_else(|_| text.to_string());
                let mut nodes = Vec::new();
                collect_json_tree("root", &val, 0, &mut nodes, 350);
                metadata.push(MetadataEntry { section: "JSON".into(), key: "Encoding".into(), value: encoding });
                return ViewerPayload::Structured {
                    format_label: format_id.to_ascii_uppercase(),
                    formatted_text: pretty,
                    tree_nodes: nodes,
                };
            }
        }
    }

    if format_id == "toml" {
        if let Ok(val) = toml::from_str::<toml::Value>(&text) {
            let pretty = toml::to_string_pretty(&val).unwrap_or_else(|_| text.to_string());
            let mut nodes = Vec::new();
            collect_toml_tree("root", &val, 0, &mut nodes, 300);
            metadata.push(MetadataEntry { section: "TOML".into(), key: "Encoding".into(), value: encoding });
            return ViewerPayload::Structured {
                format_label: "TOML Configuration".into(),
                formatted_text: pretty,
                tree_nodes: nodes,
            };
        }
    }

    if format_id == "ron" {
        if let Ok(val) = ron::from_str::<ron::Value>(&text) {
            let pretty = format!("{val:#?}");
            metadata.push(MetadataEntry { section: "RON".into(), key: "Encoding".into(), value: encoding });
            return ViewerPayload::Structured {
                format_label: "Rusty Object Notation (RON)".into(),
                formatted_text: pretty,
                tree_nodes: Vec::new(),
            };
        }
    }

    if format_id == "plist" {
        if let Ok(val) = plist::from_bytes::<plist::Value>(bytes) {
            let pretty = format!("{val:#?}");
            metadata.push(MetadataEntry { section: "Plist".into(), key: "Encoding".into(), value: encoding });
            return ViewerPayload::Structured {
                format_label: "Apple Property List".into(),
                formatted_text: pretty,
                tree_nodes: Vec::new(),
            };
        }
    }

    let md_html = if matches!(lang, "markdown") {
        Some(render_markdown_html(&text))
    } else {
        None
    };

    let raw_html = if matches!(format_id, "html" | "htm" | "xhtml") && text.len() <= 4 * 1024 * 1024 {
        Some(text.to_string())
    } else {
        None
    };

    build_text_payload(&text, lang, encoding, has_bom, line_ending, md_html, raw_html, None, metadata)
}

#[allow(clippy::too_many_arguments)]
fn build_text_payload(
    text: &str,
    lang: &str,
    encoding: String,
    has_bom: bool,
    line_ending: String,
    markdown_html: Option<String>,
    raw_html: Option<String>,
    notebook_cells: Option<Vec<NotebookCell>>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let mut lines = Vec::new();
    let mut total_lines = 0usize;
    for (idx, line) in text.lines().enumerate() {
        total_lines += 1;
        if idx < INITIAL_LINE_LIMIT {
            lines.push(HighlightedLine {
                line_no: idx + 1,
                tokens: tokenize_line(line, lang),
            });
        }
    }
    if text.is_empty() {
        total_lines = 0;
    }

    metadata.push(MetadataEntry { section: "Text".into(), key: "Encoding".into(), value: encoding.clone() });
    metadata.push(MetadataEntry { section: "Text".into(), key: "Lines".into(), value: total_lines.to_string() });
    metadata.push(MetadataEntry { section: "Text".into(), key: "Line Endings".into(), value: line_ending.clone() });
    metadata.push(MetadataEntry { section: "Text".into(), key: "Syntax Mode".into(), value: lang.to_string() });

    ViewerPayload::Text {
        language: lang.to_string(),
        total_lines,
        total_bytes: text.len() as u64,
        encoding,
        has_bom,
        line_ending,
        lines,
        truncated: total_lines > INITIAL_LINE_LIMIT,
        markdown_html,
        raw_html,
        notebook_cells,
    }
}

fn parse_ipynb(text: &str) -> Option<Vec<NotebookCell>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let arr = v.get("cells")?.as_array()?;
    let mut cells = Vec::with_capacity(arr.len());
    for c in arr {
        let cell_type = c.get("cell_type").and_then(|s| s.as_str()).unwrap_or("code").to_string();
        let execution_count = c.get("execution_count").and_then(|n| n.as_u64());
        let source = match c.get("source") {
            Some(serde_json::Value::Array(lines)) => lines
                .iter()
                .filter_map(|l| l.as_str())
                .collect::<String>(),
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let mut outputs = Vec::new();
        if let Some(outs) = c.get("outputs").and_then(|o| o.as_array()) {
            for o in outs {
                if let Some(t) = o.get("text").and_then(|t| t.as_array()) {
                    outputs.push(t.iter().filter_map(|s| s.as_str()).collect::<String>());
                } else if let Some(t) = o.get("data").and_then(|d| d.get("text/plain")) {
                    if let Some(arr) = t.as_array() {
                        outputs.push(arr.iter().filter_map(|s| s.as_str()).collect::<String>());
                    } else if let Some(s) = t.as_str() {
                        outputs.push(s.to_string());
                    }
                }
            }
        }
        cells.push(NotebookCell { cell_type, execution_count, source, outputs });
    }
    Some(cells)
}

fn collect_json_tree(
    key: &str,
    val: &serde_json::Value,
    depth: usize,
    out: &mut Vec<TreeNode>,
    limit: usize,
) {
    if out.len() >= limit {
        return;
    }
    match val {
        serde_json::Value::Object(map) => {
            out.push(TreeNode {
                key: key.to_string(),
                value_preview: format!("{{ {} keys }}", map.len()),
                kind: "object".into(),
                depth,
            });
            for (k, v) in map.iter().take(80) {
                collect_json_tree(k, v, depth + 1, out, limit);
            }
        }
        serde_json::Value::Array(arr) => {
            out.push(TreeNode {
                key: key.to_string(),
                value_preview: format!("[ {} items ]", arr.len()),
                kind: "array".into(),
                depth,
            });
            for (i, v) in arr.iter().take(40).enumerate() {
                collect_json_tree(&format!("[{i}]"), v, depth + 1, out, limit);
            }
        }
        serde_json::Value::String(s) => {
            let prev = if s.len() > 100 { format!("\"{}...\"", &s[..100]) } else { format!("\"{s}\"") };
            out.push(TreeNode { key: key.to_string(), value_preview: prev, kind: "string".into(), depth });
        }
        serde_json::Value::Number(n) => {
            out.push(TreeNode { key: key.to_string(), value_preview: n.to_string(), kind: "number".into(), depth });
        }
        serde_json::Value::Bool(b) => {
            out.push(TreeNode { key: key.to_string(), value_preview: b.to_string(), kind: "bool".into(), depth });
        }
        serde_json::Value::Null => {
            out.push(TreeNode { key: key.to_string(), value_preview: "null".into(), kind: "null".into(), depth });
        }
    }
}

fn collect_toml_tree(
    key: &str,
    val: &toml::Value,
    depth: usize,
    out: &mut Vec<TreeNode>,
    limit: usize,
) {
    if out.len() >= limit {
        return;
    }
    match val {
        toml::Value::Table(map) => {
            out.push(TreeNode { key: key.to_string(), value_preview: format!("{{ {} keys }}", map.len()), kind: "object".into(), depth });
            for (k, v) in map {
                collect_toml_tree(k, v, depth + 1, out, limit);
            }
        }
        toml::Value::Array(arr) => {
            out.push(TreeNode { key: key.to_string(), value_preview: format!("[ {} items ]", arr.len()), kind: "array".into(), depth });
            for (i, v) in arr.iter().take(30).enumerate() {
                collect_toml_tree(&format!("[{i}]"), v, depth + 1, out, limit);
            }
        }
        other => {
            out.push(TreeNode { key: key.to_string(), value_preview: other.to_string(), kind: "value".into(), depth });
        }
    }
}

fn detect_csv_delimiter(text: &str) -> u8 {
    let first = text.lines().next().unwrap_or("");
    let candidates = [(b',', first.matches(',').count()), (b'\t', first.matches('\t').count()), (b';', first.matches(';').count()), (b'|', first.matches('|').count())];
    candidates.into_iter().max_by_key(|c| c.1).filter(|c| c.1 > 0).map(|c| c.0).unwrap_or(b',')
}

pub fn parse_delimited_table(text: &str, name: String, delim: u8) -> TableSheet {
    let delim_ch = delim as char;
    let mut rows_raw: Vec<Vec<String>> = Vec::new();
    let mut total_rows = 0usize;
    let mut cur_row: Vec<String> = Vec::new();
    let mut cur_cell = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur_cell.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cur_cell.push(ch);
            }
        } else if ch == '"' && cur_cell.is_empty() {
            in_quotes = true;
        } else if ch == delim_ch {
            cur_row.push(std::mem::take(&mut cur_cell));
        } else if ch == '\n' || ch == '\r' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            cur_row.push(std::mem::take(&mut cur_cell));
            if !(cur_row.len() == 1 && cur_row[0].is_empty()) {
                total_rows += 1;
                if rows_raw.len() <= 2001 {
                    rows_raw.push(std::mem::take(&mut cur_row));
                } else {
                    cur_row.clear();
                }
            } else {
                cur_row.clear();
            }
        } else {
            cur_cell.push(ch);
        }
    }
    if !cur_cell.is_empty() || !cur_row.is_empty() {
        cur_row.push(cur_cell);
        total_rows += 1;
        if rows_raw.len() <= 2001 {
            rows_raw.push(cur_row);
        }
    }

    let columns = if !rows_raw.is_empty() {
        rows_raw.remove(0)
    } else {
        vec!["Value".into()]
    };
    let data_total = total_rows.saturating_sub(1);
    let truncated = rows_raw.len() < data_total;
    TableSheet {
        name,
        columns,
        rows: rows_raw,
        total_rows: data_total,
        truncated,
    }
}

/// Lightweight fast syntax tokenizer for ~25 programming/config/markup languages.
pub fn tokenize_line(line: &str, lang: &str) -> Vec<TextToken> {
    if line.is_empty() || lang == "text" || lang == "log" || line.len() > 2048 {
        return vec![TextToken { kind: "plain".into(), text: line.to_string() }];
    }

    if lang == "diff" {
        let kind = if line.starts_with('+') {
            "str"
        } else if line.starts_with('-') {
            "kw"
        } else if line.starts_with("@@") {
            "type"
        } else {
            "plain"
        };
        return vec![TextToken { kind: kind.into(), text: line.to_string() }];
    }

    let mut tokens = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        // Line comments
        if (b == b'/' && bytes.get(i + 1) == Some(&b'/'))
            || (b == b'#' && matches!(lang, "python" | "shell" | "yaml" | "toml" | "ruby" | "perl" | "ini" | "markdown"))
            || (b == b';' && matches!(lang, "ini" | "asm" | "lisp" | "gcode"))
            || (b == b'-' && bytes.get(i + 1) == Some(&b'-') && lang == "sql")
        {
            tokens.push(TextToken { kind: "cmt".into(), text: line[i..].to_string() });
            break;
        }
        // Strings
        if b == b'"' || (b == b'\'' && lang != "rust") || b == b'`' {
            let quote = b;
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            tokens.push(TextToken { kind: "str".into(), text: line[start..i].to_string() });
            continue;
        }
        // XML/HTML tags
        if b == b'<' && matches!(lang, "xml" | "html") {
            let start = i;
            if let Some(end) = line[i..].find('>') {
                i += end + 1;
                tokens.push(TextToken { kind: "tag".into(), text: line[start..i].to_string() });
                continue;
            }
        }
        // Numbers
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'.' | b'_')) {
                i += 1;
            }
            tokens.push(TextToken { kind: "num".into(), text: line[start..i].to_string() });
            continue;
        }
        // Identifiers / keywords
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            let kind = if is_keyword(word, lang) {
                "kw"
            } else if word.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && word.len() > 1 {
                "type"
            } else {
                "plain"
            };
            tokens.push(TextToken { kind: kind.into(), text: word.to_string() });
            continue;
        }
        // Plain punctuation/whitespace run
        let start = i;
        i += 1;
        while i < bytes.len()
            && !bytes[i].is_ascii_alphanumeric()
            && !matches!(bytes[i], b'_' | b'"' | b'\'' | b'`' | b'/' | b'#' | b';' | b'<')
        {
            i += 1;
        }
        // Advance to UTF-8 char boundary safely
        while !line.is_char_boundary(i) && i < bytes.len() {
            i += 1;
        }
        tokens.push(TextToken { kind: "plain".into(), text: line[start..i].to_string() });
    }
    tokens
}

fn is_keyword(word: &str, lang: &str) -> bool {
    let common = matches!(
        word,
        "if" | "else" | "for" | "while" | "loop" | "return" | "break" | "continue"
            | "match" | "switch" | "case" | "default" | "true" | "false" | "null" | "None"
            | "import" | "export" | "from" | "use" | "include" | "const" | "let" | "var"
            | "fn" | "func" | "function" | "def" | "class" | "struct" | "enum" | "trait"
            | "impl" | "pub" | "public" | "private" | "static" | "async" | "await" | "try" | "catch"
    );
    if common {
        return true;
    }
    match lang {
        "rust" => matches!(word, "mod" | "crate" | "super" | "self" | "Self" | "where" | "type" | "unsafe" | "mut" | "ref" | "as" | "in" | "dyn" | "move"),
        "python" => matches!(word, "elif" | "except" | "finally" | "with" | "as" | "pass" | "lambda" | "yield" | "True" | "False" | "and" | "or" | "not" | "in" | "is"),
        "sql" => matches!(word.to_ascii_uppercase().as_str(), "SELECT" | "FROM" | "WHERE" | "INSERT" | "INTO" | "UPDATE" | "DELETE" | "CREATE" | "TABLE" | "JOIN" | "ON" | "GROUP" | "BY" | "ORDER" | "LIMIT" | "VALUES" | "AS" | "AND" | "OR"),
        "gcode" => word.starts_with('G') || word.starts_with('M') || word.starts_with('T'),
        _ => false,
    }
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn render_markdown_html(md: &str) -> String {
    let mut html = String::with_capacity(md.len() + 256);
    let mut in_code = false;
    let mut in_list = false;

    for line in md.lines().take(2500) {
        let trimmed = line.trim_end();
        if trimmed.starts_with("```") {
            if in_list {
                html.push_str("</ul>\n");
                in_list = false;
            }
            if in_code {
                html.push_str("</code></pre>\n");
                in_code = false;
            } else {
                let lang = escape_html(trimmed.trim_start_matches('`').trim());
                html.push_str(&format!("<pre class=\"md-code\" data-lang=\"{lang}\"><code>"));
                in_code = true;
            }
            continue;
        }
        if in_code {
            html.push_str(&escape_html(line));
            html.push('\n');
            continue;
        }
        if trimmed.is_empty() {
            if in_list {
                html.push_str("</ul>\n");
                in_list = false;
            }
            continue;
        }
        if trimmed == "---" || trimmed == "***" {
            if in_list {
                html.push_str("</ul>\n");
                in_list = false;
            }
            html.push_str("<hr/>\n");
            continue;
        }
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
            if in_list {
                html.push_str("</ul>\n");
                in_list = false;
            }
            let content = render_inline_md(trimmed[hashes + 1..].trim());
            html.push_str(&format!("<h{hashes}>{content}</h{hashes}>\n"));
            continue;
        }
        if trimmed.starts_with("> ") {
            if in_list {
                html.push_str("</ul>\n");
                in_list = false;
            }
            html.push_str(&format!("<blockquote>{}</blockquote>\n", render_inline_md(&trimmed[2..])));
            continue;
        }
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            if !in_list {
                html.push_str("<ul>\n");
                in_list = true;
            }
            html.push_str(&format!("<li>{}</li>\n", render_inline_md(&trimmed[2..])));
            continue;
        }
        if in_list {
            html.push_str("</ul>\n");
            in_list = false;
        }
        html.push_str(&format!("<p>{}</p>\n", render_inline_md(trimmed)));
    }
    if in_list {
        html.push_str("</ul>\n");
    }
    if in_code {
        html.push_str("</code></pre>\n");
    }
    html
}

fn render_inline_md(s: &str) -> String {
    let esc = escape_html(s);
    // Inline code backticks
    let mut out = String::with_capacity(esc.len());
    let parts: Vec<&str> = esc.split('`').collect();
    for (idx, part) in parts.iter().enumerate() {
        if idx % 2 == 1 {
            out.push_str("<code>");
            out.push_str(part);
            out.push_str("</code>");
        } else {
            // Bold **text**
            let mut seg = String::new();
            let bold_parts: Vec<&str> = part.split("**").collect();
            for (bi, bp) in bold_parts.iter().enumerate() {
                if bi % 2 == 1 && bi + 1 < bold_parts.len() {
                    seg.push_str("<strong>");
                    seg.push_str(bp);
                    seg.push_str("</strong>");
                } else {
                    seg.push_str(bp);
                }
            }
            out.push_str(&seg);
        }
    }
    out
}
