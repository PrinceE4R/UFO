use serde::Serialize;
use wasm_bindgen::prelude::*;

#[derive(Serialize)]
struct FormatCatalogItem {
    ext: &'static str,
    name: &'static str,
    category: &'static str,
    mime: &'static str,
}

/// Opens any file byte slice inside WebAssembly and returns a JSON-serialized OpenFileReport.
#[wasm_bindgen]
pub fn ufo_open_bytes(file_name: &str, bytes: &[u8], total_size_hint: Option<u64>) -> Result<String, JsValue> {
    let report = ufo_core::open_bytes(file_name, bytes, total_size_hint);
    serde_json::to_string(&report).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Returns a paged 16-byte hex window at `offset` as JSON.
#[wasm_bindgen]
pub fn ufo_hex_page(bytes: &[u8], offset: u64, length: usize) -> Result<String, JsValue> {
    let page = ufo_core::slice_hex_page(bytes, offset, length);
    serde_json::to_string(&page).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Returns a syntax-highlighted line window for large text/code/log files as JSON.
#[wasm_bindgen]
pub fn ufo_text_page(bytes: &[u8], start_line: usize, line_count: usize, lang: &str) -> Result<String, JsValue> {
    let page = ufo_core::slice_text_page(bytes, start_line, line_count, lang);
    serde_json::to_string(&page).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Extracts raw bytes for an entry inside a ZIP/TAR/TAR.GZ/TAR.ZST/ISO/AR archive.
#[wasm_bindgen]
pub fn ufo_extract_archive_entry(bytes: &[u8], entry_path: &str) -> Result<Vec<u8>, JsValue> {
    ufo_core::extract_archive_entry(bytes, entry_path).map_err(|e| JsValue::from_str(&e))
}

/// Generates a full UFO diagnostic JSON export for Re-Save.
#[wasm_bindgen]
pub fn ufo_export_report_json(file_name: &str, bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
    let res = ufo_core::prepare_resave(file_name, bytes, true).map_err(|e| JsValue::from_str(&e))?;
    Ok(res.bytes)
}

/// Returns the complete 400+ supported format registry and 23 categories.
#[wasm_bindgen]
pub fn ufo_supported_formats_json() -> Result<String, JsValue> {
    let list: Vec<FormatCatalogItem> = ufo_core::FORMAT_REGISTRY
        .iter()
        .map(|f| FormatCatalogItem {
            ext: f.ext,
            name: f.name,
            category: f.category.label(),
            mime: f.mime,
        })
        .collect();
    serde_json::to_string(&list).map_err(|e| JsValue::from_str(&e.to_string()))
}
