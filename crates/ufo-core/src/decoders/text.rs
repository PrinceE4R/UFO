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
    let lines: Vec<&str> = md.lines().take(5000).collect();
    let mut html = String::with_capacity(md.len().saturating_mul(2) + 256);
    let mut list_stack: Vec<(usize, &'static str)> = Vec::new();
    let mut i = 0usize;

    let close_lists_to = |target: Option<usize>, stack: &mut Vec<(usize, &'static str)>, out: &mut String| {
        while let Some(&(ind, tag)) = stack.last() {
            if target.is_some_and(|t| ind < t) {
                break;
            }
            out.push_str(&format!("</{tag}>\n"));
            stack.pop();
        }
    };
    let close_all_lists = |stack: &mut Vec<(usize, &'static str)>, out: &mut String| {
        while let Some((_, tag)) = stack.pop() {
            out.push_str(&format!("</{tag}>\n"));
        }
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed_end = line.trim_end();
        let trimmed = trimmed_end.trim_start();

        // Fenced code blocks ``` or ~~~
        if let Some((fence_ch, fence_len, info)) = parse_md_fence(trimmed) {
            close_all_lists(&mut list_stack, &mut html);
            let lang = escape_html(info.trim());
            html.push_str(&format!("<pre class=\"md-code\" data-lang=\"{lang}\"><code>"));
            i += 1;
            while i < lines.len() {
                let code_line = lines[i];
                let ct = code_line.trim();
                let close_len = ct.bytes().take_while(|&b| b == fence_ch as u8).count();
                if close_len >= fence_len && ct.len() == close_len {
                    i += 1;
                    break;
                }
                html.push_str(&escape_html(code_line));
                html.push('\n');
                i += 1;
            }
            html.push_str("</code></pre>\n");
            continue;
        }

        // Blank lines
        if trimmed.is_empty() {
            close_all_lists(&mut list_stack, &mut html);
            i += 1;
            continue;
        }

        // Horizontal rule (---, ***, ___)
        if is_md_hr(trimmed) {
            close_all_lists(&mut list_stack, &mut html);
            html.push_str("<hr/>\n");
            i += 1;
            continue;
        }

        // ATX Headings (# .. ######)
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes)
            && trimmed
                .as_bytes()
                .get(hashes)
                .is_none_or(|&b| b == b' ' || b == b'\t')
        {
            close_all_lists(&mut list_stack, &mut html);
            let inner = trimmed[hashes..].trim().trim_end_matches('#').trim_end();
            let slug = md_heading_slug(inner);
            let content = render_inline_md(inner);
            html.push_str(&format!(
                "<a id=\"{slug}\" class=\"md-anchor\" aria-hidden=\"true\"></a><h{hashes}>{content}</h{hashes}>\n"
            ));
            i += 1;
            continue;
        }

        // GFM Pipe Table (header row + separator row |---|:---:|)
        if i + 1 < lines.len() && is_md_table_candidate(trimmed) {
            if let Some(aligns) = parse_md_table_aligns(lines[i + 1].trim()) {
                let headers = split_md_table_row(trimmed);
                if !headers.is_empty() && headers.len() == aligns.len() {
                    close_all_lists(&mut list_stack, &mut html);
                    html.push_str("<div class=\"md-table-wrap\"><table class=\"md-table\">\n<thead><tr>");
                    for (ci, cell) in headers.iter().enumerate() {
                        let align = aligns.get(ci).copied().unwrap_or("");
                        let style = if align.is_empty() {
                            String::new()
                        } else {
                            format!(" style=\"text-align:{align}\"")
                        };
                        html.push_str(&format!("<th{style}>{}</th>", render_inline_md(cell.trim())));
                    }
                    html.push_str("</tr></thead>\n<tbody>\n");
                    i += 2;
                    while i < lines.len() {
                        let row_t = lines[i].trim();
                        if row_t.is_empty() || !row_t.contains('|') || is_md_fence(row_t) {
                            break;
                        }
                        let cols = split_md_table_row(row_t);
                        html.push_str("<tr>");
                        for ci in 0..aligns.len() {
                            let align = aligns.get(ci).copied().unwrap_or("");
                            let style = if align.is_empty() {
                                String::new()
                            } else {
                                format!(" style=\"text-align:{align}\"")
                            };
                            let val = cols.get(ci).map(|s| s.trim()).unwrap_or("");
                            html.push_str(&format!("<td{style}>{}</td>", render_inline_md(val)));
                        }
                        html.push_str("</tr>\n");
                        i += 1;
                    }
                    html.push_str("</tbody></table></div>\n");
                    continue;
                }
            }
        }

        // Multi-line Blockquote
        if trimmed.starts_with('>') {
            close_all_lists(&mut list_stack, &mut html);
            let mut quote_lines = Vec::new();
            while i < lines.len() {
                let qt = lines[i].trim_start();
                if let Some(rest) = qt.strip_prefix('>') {
                    quote_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
                    i += 1;
                } else {
                    break;
                }
            }
            let inner_md = quote_lines.join("\n");
            let inner_html = render_markdown_html(&inner_md);
            html.push_str(&format!("<blockquote>{inner_html}</blockquote>\n"));
            continue;
        }

        // Unordered, Task (- [ ] / - [x]), or Ordered (1. / 1)) list item
        if let Some((tag, content, task_state)) = parse_md_list_marker(trimmed) {
            let indent = line.len() - trimmed.len();
            // Close deeper lists
            while let Some(&(top_ind, top_tag)) = list_stack.last() {
                if top_ind > indent {
                    html.push_str(&format!("</{top_tag}>\n"));
                    list_stack.pop();
                } else {
                    break;
                }
            }
            if list_stack.last().is_none_or(|&(top_ind, top_tag)| top_ind < indent || top_tag != tag) {
                if list_stack.last().is_some_and(|&(top_ind, top_tag)| top_ind == indent && top_tag != tag) {
                    if let Some((_, prev_tag)) = list_stack.pop() {
                        html.push_str(&format!("</{prev_tag}>\n"));
                    }
                }
                html.push_str(&format!("<{tag}>\n"));
                list_stack.push((indent, tag));
            }
            let _ = close_lists_to;
            let rendered = render_inline_md(content);
            match task_state {
                Some(checked) => {
                    let chk = if checked { " checked" } else { "" };
                    html.push_str(&format!(
                        "<li class=\"md-task-item\"><input type=\"checkbox\" disabled{chk}/> <span>{rendered}</span></li>\n"
                    ));
                }
                None => {
                    html.push_str(&format!("<li>{rendered}</li>\n"));
                }
            }
            i += 1;
            continue;
        }

        close_all_lists(&mut list_stack, &mut html);

        // Setext Heading (Title \n === or ---)
        if i + 1 < lines.len() {
            let next_t = lines[i + 1].trim();
            if next_t.len() >= 2 && next_t.bytes().all(|b| b == b'=') {
                let slug = md_heading_slug(trimmed);
                html.push_str(&format!(
                    "<a id=\"{slug}\" class=\"md-anchor\" aria-hidden=\"true\"></a><h1>{}</h1>\n",
                    render_inline_md(trimmed)
                ));
                i += 2;
                continue;
            }
            if next_t.len() >= 2 && next_t.bytes().all(|b| b == b'-') && !trimmed.contains('|') {
                let slug = md_heading_slug(trimmed);
                html.push_str(&format!(
                    "<a id=\"{slug}\" class=\"md-anchor\" aria-hidden=\"true\"></a><h2>{}</h2>\n",
                    render_inline_md(trimmed)
                ));
                i += 2;
                continue;
            }
        }

        // Paragraph (accumulates contiguous non-block lines)
        let mut para_lines: Vec<String> = Vec::new();
        while i < lines.len() {
            let cur_raw = lines[i];
            let cur_t = cur_raw.trim();
            if cur_t.is_empty()
                || parse_md_fence(cur_t).is_some()
                || is_md_hr(cur_t)
                || cur_t.starts_with('>')
                || parse_md_list_marker(cur_t).is_some()
                || (1..=6).any(|h| {
                    cur_t.starts_with(&"#".repeat(h))
                        && cur_t.as_bytes().get(h).is_some_and(|&b| b == b' ' || b == b'\t')
                })
                || (i + 1 < lines.len()
                    && is_md_table_candidate(cur_t)
                    && parse_md_table_aligns(lines[i + 1].trim()).is_some())
            {
                break;
            }
            let has_hard_break = cur_raw.ends_with("  ") || cur_raw.trim_end().ends_with('\\');
            let piece = if cur_raw.trim_end().ends_with('\\') {
                cur_t.trim_end_matches('\\').trim_end()
            } else {
                cur_t
            };
            let mut rendered = render_inline_md(piece);
            if has_hard_break {
                rendered.push_str("<br/>");
            }
            para_lines.push(rendered);
            i += 1;
        }
        if !para_lines.is_empty() {
            html.push_str(&format!("<p>{}</p>\n", para_lines.join("\n")));
        } else {
            i += 1;
        }
    }

    close_all_lists(&mut list_stack, &mut html);
    html
}

fn parse_md_fence(t: &str) -> Option<(char, usize, &str)> {
    let ch = t.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = t.bytes().take_while(|&b| b == ch as u8).count();
    (len >= 3).then_some((ch, len, &t[len..]))
}

fn is_md_fence(t: &str) -> bool {
    parse_md_fence(t).is_some()
}

fn is_md_hr(t: &str) -> bool {
    let non_space: Vec<char> = t.chars().filter(|c| !c.is_whitespace()).collect();
    non_space.len() >= 3
        && matches!(non_space[0], '-' | '*' | '_')
        && non_space.iter().all(|&c| c == non_space[0])
}

fn md_heading_slug(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if (ch.is_whitespace() || ch == '-' || ch == '_') && !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn is_md_table_candidate(t: &str) -> bool {
    t.contains('|') && !t.starts_with('>')
}

fn split_md_table_row(row: &str) -> Vec<String> {
    let mut s = row.trim();
    if let Some(stripped) = s.strip_prefix('|') {
        s = stripped;
    }
    if !s.ends_with("\\|") {
        if let Some(stripped) = s.strip_suffix('|') {
            s = stripped;
        }
    }
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut in_ticks = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if next == '|' {
                    cur.push('|');
                    chars.next();
                    continue;
                }
            }
            cur.push(c);
        } else if c == '`' {
            in_ticks = !in_ticks;
            cur.push(c);
        } else if c == '|' && !in_ticks {
            cells.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    cells.push(cur);
    cells
}

fn parse_md_table_aligns(sep_row: &str) -> Option<Vec<&'static str>> {
    if !sep_row.contains('|') {
        return None;
    }
    let cols = split_md_table_row(sep_row);
    if cols.is_empty() {
        return None;
    }
    let mut aligns = Vec::with_capacity(cols.len());
    for col in cols {
        let t = col.trim();
        let left = t.starts_with(':');
        let right = t.ends_with(':');
        let mid = t.trim_matches(':');
        if mid.is_empty() || !mid.bytes().all(|b| b == b'-') {
            return None;
        }
        aligns.push(match (left, right) {
            (true, true) => "center",
            (false, true) => "right",
            (true, false) => "left",
            (false, false) => "",
        });
    }
    Some(aligns)
}

fn parse_md_list_marker(t: &str) -> Option<(&'static str, &str, Option<bool>)> {
    if let Some(rest) = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
    {
        let trim_r = rest.trim_start();
        if let Some(body) = trim_r.strip_prefix("[ ] ") {
            return Some(("ul", body, Some(false)));
        }
        if let Some(body) = trim_r.strip_prefix("[x] ").or_else(|| trim_r.strip_prefix("[X] ")) {
            return Some(("ul", body, Some(true)));
        }
        return Some(("ul", trim_r, None));
    }
    let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    if (1..=9).contains(&digits) {
        let after = &t[digits..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return Some(("ol", rest.trim_start(), None));
        }
    }
    None
}

fn sanitize_md_url(raw: &str) -> Option<String> {
    let u = raw.trim();
    if u.is_empty() {
        return None;
    }
    let trimmed = if u.starts_with('<') && u.ends_with('>') && u.len() >= 2 {
        &u[1..u.len() - 1]
    } else {
        u
    };
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("vbscript:") {
        return None;
    }
    if lower.starts_with("data:") && !lower.starts_with("data:image/") {
        return None;
    }
    Some(trimmed.to_string())
}

fn parse_md_dest_and_title(s: &str) -> Option<(String, Option<String>, usize)> {
    // Parse inside `(...)` starting after `(`. Handles nested parens and `"title"`.
    let bytes = s.as_bytes();
    let mut depth = 1usize;
    let mut in_quote: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if let Some(q) = in_quote {
            if b == q {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            in_quote = Some(b);
            i += 1;
            continue;
        }
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth == 0 {
                let inner = s[..i].trim();
                let (dest, title) = if inner.starts_with('<') {
                    if let Some(end_angle) = inner.find('>') {
                        let d = &inner[1..end_angle];
                        let t = inner[end_angle + 1..]
                            .trim()
                            .trim_matches('"')
                            .trim_matches('\'')
                            .trim();
                        (d, (!t.is_empty()).then_some(t.to_string()))
                    } else {
                        (inner, None)
                    }
                } else if let Some(sp) = inner.find(char::is_whitespace) {
                    let d = inner[..sp].trim();
                    let t = inner[sp..]
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .trim();
                    (d, (!t.is_empty()).then_some(t.to_string()))
                } else {
                    (inner, None)
                };
                return Some((dest.to_string(), title, i + 1));
            }
        }
        i += 1;
    }
    None
}

fn find_matching_bracket(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 1usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if b == b'[' {
            depth += 1;
        } else if b == b']' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn render_inline_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 32);
    let b = s.as_bytes();
    let mut i = 0usize;

    while i < s.len() {
        // Escaped character
        if b[i] == b'\\' && i + 1 < s.len() {
            let next_ch = s[i + 1..].chars().next().unwrap_or('\\');
            out.push_str(&escape_html(&next_ch.to_string()));
            i += 1 + next_ch.len_utf8();
            continue;
        }

        // Inline code span (` or ``)
        if b[i] == b'`' {
            let ticks = b[i..].iter().take_while(|&&x| x == b'`').count();
            let delim = &s[i..i + ticks];
            if let Some(end_rel) = s[i + ticks..].find(delim) {
                let code_inner = &s[i + ticks..i + ticks + end_rel];
                let trimmed_code = if code_inner.starts_with(' ')
                    && code_inner.ends_with(' ')
                    && code_inner.len() > 2
                {
                    &code_inner[1..code_inner.len() - 1]
                } else {
                    code_inner
                };
                out.push_str("<code>");
                out.push_str(&escape_html(trimmed_code));
                out.push_str("</code>");
                i += ticks + end_rel + ticks;
                continue;
            }
        }

        // Image ![alt](src "title")
        if b[i] == b'!' && b.get(i + 1) == Some(&b'[') {
            if let Some(close_br) = find_matching_bracket(&s[i + 2..]) {
                let alt = &s[i + 2..i + 2 + close_br];
                let after_br = &s[i + 2 + close_br + 1..];
                if let Some(rest_paren) = after_br.strip_prefix('(') {
                    if let Some((raw_url, title, consumed)) = parse_md_dest_and_title(rest_paren) {
                        if let Some(safe_url) = sanitize_md_url(&raw_url) {
                            let esc_src = escape_html(&safe_url);
                            let esc_alt = escape_html(alt);
                            let title_attr = title
                                .map(|t| format!(" title=\"{}\"", escape_html(&t)))
                                .unwrap_or_default();
                            out.push_str(&format!(
                                "<img class=\"md-img\" src=\"{esc_src}\" data-ufo-src=\"{esc_src}\" alt=\"{esc_alt}\"{title_attr}/>"
                            ));
                            i += 2 + close_br + 1 + 1 + consumed;
                            continue;
                        }
                    }
                }
            }
        }

        // Link [label](url "title")
        if b[i] == b'[' {
            if let Some(close_br) = find_matching_bracket(&s[i + 1..]) {
                let label = &s[i + 1..i + 1 + close_br];
                let after_br = &s[i + 1 + close_br + 1..];
                if let Some(rest_paren) = after_br.strip_prefix('(') {
                    if let Some((raw_url, title, consumed)) = parse_md_dest_and_title(rest_paren) {
                        if let Some(safe_url) = sanitize_md_url(&raw_url) {
                            let esc_href = escape_html(&safe_url);
                            let title_attr = title
                                .map(|t| format!(" title=\"{}\"", escape_html(&t)))
                                .unwrap_or_default();
                            let rendered_label = render_inline_md(label);
                            out.push_str(&format!(
                                "<a class=\"md-link\" href=\"{esc_href}\" data-ufo-href=\"{esc_href}\"{title_attr}>{rendered_label}</a>"
                            ));
                            i += 1 + close_br + 1 + 1 + consumed;
                            continue;
                        }
                    }
                }
            }
        }

        // Autolink <https://...> or <http://...>
        if b[i] == b'<' {
            if let Some(close) = s[i + 1..].find('>') {
                let inner = &s[i + 1..i + 1 + close];
                if inner.starts_with("http://") || inner.starts_with("https://") {
                    if let Some(safe) = sanitize_md_url(inner) {
                        let esc = escape_html(&safe);
                        out.push_str(&format!(
                            "<a class=\"md-link\" href=\"{esc}\" data-ufo-href=\"{esc}\">{esc}</a>"
                        ));
                        i += 1 + close + 1;
                        continue;
                    }
                }
            }
        }

        // Strikethrough ~~text~~
        if s[i..].starts_with("~~") {
            if let Some(end_rel) = s[i + 2..].find("~~") {
                if end_rel > 0 {
                    out.push_str("<del>");
                    out.push_str(&render_inline_md(&s[i + 2..i + 2 + end_rel]));
                    out.push_str("</del>");
                    i += 2 + end_rel + 2;
                    continue;
                }
            }
        }

        // Bold + Italic ***text***
        if s[i..].starts_with("***") {
            if let Some(end_rel) = find_closing_delim(&s[i + 3..], "***") {
                if end_rel > 0 {
                    out.push_str("<strong><em>");
                    out.push_str(&render_inline_md(&s[i + 3..i + 3 + end_rel]));
                    out.push_str("</em></strong>");
                    i += 3 + end_rel + 3;
                    continue;
                }
            }
        }

        // Bold **text** or __text__
        if s[i..].starts_with("**") || s[i..].starts_with("__") {
            let delim = &s[i..i + 2];
            let prev_alnum = i > 0 && s[..i].chars().next_back().is_some_and(char::is_alphanumeric);
            if !(delim == "__" && prev_alnum) {
                if let Some(end_rel) = find_closing_delim(&s[i + 2..], delim) {
                    if end_rel > 0 {
                        out.push_str("<strong>");
                        out.push_str(&render_inline_md(&s[i + 2..i + 2 + end_rel]));
                        out.push_str("</strong>");
                        i += 2 + end_rel + 2;
                        continue;
                    }
                }
            }
        }

        // Italic *text* or _text_ (underscore respects snake_case identifier boundaries)
        if b[i] == b'*' || b[i] == b'_' {
            let ch = if b[i] == b'*' { "*" } else { "_" };
            let prev_alnum = i > 0 && s[..i].chars().next_back().is_some_and(char::is_alphanumeric);
            let next_space = b.get(i + 1).is_none_or(|&x| x.is_ascii_whitespace());
            if !next_space && !(ch == "_" && prev_alnum) {
                if let Some(end_rel) = find_closing_single_emph(&s[i + 1..], ch.as_bytes()[0]) {
                    if end_rel > 0 {
                        out.push_str("<em>");
                        out.push_str(&render_inline_md(&s[i + 1..i + 1 + end_rel]));
                        out.push_str("</em>");
                        i += 1 + end_rel + 1;
                        continue;
                    }
                }
            }
        }

        // Emit next UTF-8 character safely HTML-escaped
        let ch = s[i..].chars().next().unwrap();
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
        i += ch.len_utf8();
    }
    out
}

fn find_closing_delim(s: &str, delim: &str) -> Option<usize> {
    let mut i = 0usize;
    while i < s.len() {
        if s.as_bytes()[i] == b'\\' && i + 1 < s.len() {
            i += 2;
            continue;
        }
        if s.as_bytes()[i] == b'`' {
            if let Some(end_code) = s[i + 1..].find('`') {
                i += 1 + end_code + 1;
                continue;
            }
        }
        if s[i..].starts_with(delim) {
            return Some(i);
        }
        let ch = s[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}

fn find_closing_single_emph(s: &str, delim: u8) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < s.len() {
        if bytes[i] == b'\\' && i + 1 < s.len() {
            i += 2;
            continue;
        }
        if bytes[i] == b'`' {
            if let Some(end_code) = s[i + 1..].find('`') {
                i += 1 + end_code + 1;
                continue;
            }
        }
        if bytes[i] == delim && bytes.get(i + 1) != Some(&delim) {
            let prev_space = i == 0 || bytes[i - 1].is_ascii_whitespace();
            let next_alnum = delim == b'_'
                && bytes.get(i + 1).is_some_and(|b| b.is_ascii_alphanumeric());
            if !prev_space && !next_alnum {
                return Some(i);
            }
        }
        let ch = s[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}
