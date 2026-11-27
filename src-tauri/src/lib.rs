use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use memmap2::Mmap;
use serde::Serialize;
use tauri::{http, ipc::Response, Emitter, Manager, State, Window};

enum FileBacking {
    Mapped { _file: File, mmap: Mmap },
    Memory(Vec<u8>),
}

impl FileBacking {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Mapped { mmap, .. } => mmap,
            Self::Memory(v) => v,
        }
    }
}

struct SessionEntry {
    file_name: String,
    mime: String,
    backing: FileBacking,
    source_dir: Option<PathBuf>,
}

struct AppState {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<String, Arc<SessionEntry>>>,
    launch_paths: Mutex<Vec<String>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
            launch_paths: Mutex::new(Vec::new()),
        }
    }
}

#[derive(Serialize)]
struct OpenSessionResponse {
    session_id: String,
    stream_url: String,
    asset_base_url: Option<String>,
    report: ufo_core::OpenFileReport,
}

#[derive(Serialize)]
struct PlatformInfo {
    os: &'static str,
    is_mobile: bool,
    supports_window_chrome: bool,
    registered_formats: usize,
}

#[derive(Serialize)]
struct FormatCatalogItem {
    ext: &'static str,
    name: &'static str,
    category: &'static str,
    mime: &'static str,
}

fn origin_for_scheme() -> &'static str {
    if cfg!(any(target_os = "windows", target_os = "android")) {
        "http://ufo.localhost"
    } else {
        "ufo://localhost"
    }
}

fn stream_url_for(session_id: &str) -> String {
    format!("{}/stream/{session_id}", origin_for_scheme())
}

fn store_session(
    state: &AppState,
    file_name: String,
    backing: FileBacking,
    source_dir: Option<PathBuf>,
    report: ufo_core::OpenFileReport,
) -> Result<OpenSessionResponse, String> {
    let id_num = state.next_id.fetch_add(1, Ordering::Relaxed);
    let session_id = format!("ufo-{id_num}");
    let mime = report.detected.mime.clone();
    let has_source_dir = source_dir.is_some();
    let entry = Arc::new(SessionEntry {
        file_name,
        mime,
        backing,
        source_dir,
    });
    let mut map = state.sessions.lock().map_err(|_| "Session lock poisoned")?;
    map.insert(session_id.clone(), entry);
    Ok(OpenSessionResponse {
        stream_url: stream_url_for(&session_id),
        asset_base_url: has_source_dir.then(|| format!("{}/asset/{session_id}/", origin_for_scheme())),
        session_id,
        report,
    })
}

#[tauri::command]
fn get_platform_info() -> PlatformInfo {
    let is_mobile = cfg!(any(target_os = "android", target_os = "ios"));
    PlatformInfo {
        os: std::env::consts::OS,
        is_mobile,
        supports_window_chrome: !is_mobile,
        registered_formats: ufo_core::FORMAT_REGISTRY.len(),
    }
}

#[tauri::command]
fn take_launch_files(state: State<'_, Arc<AppState>>) -> Result<Vec<String>, String> {
    let mut paths = state.launch_paths.lock().map_err(|_| "Launch lock poisoned")?;
    Ok(std::mem::take(&mut *paths))
}

#[tauri::command]
fn get_formats_catalog() -> Vec<FormatCatalogItem> {
    ufo_core::FORMAT_REGISTRY
        .iter()
        .map(|f| FormatCatalogItem {
            ext: f.ext,
            name: f.name,
            category: f.category.label(),
            mime: f.mime,
        })
        .collect()
}

#[tauri::command]
async fn open_path(path: String, state: State<'_, Arc<AppState>>) -> Result<OpenSessionResponse, String> {
    let app_state = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        let file = File::open(&p).map_err(|e| format!("Failed to open {}: {e}", p.display()))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        let file_name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".into());

        let backing = if meta.len() == 0 {
            FileBacking::Memory(Vec::new())
        } else {
            // SAFETY: read-only memory map for instant zero-copy opening across all file sizes.
            match unsafe { Mmap::map(&file) } {
                Ok(mmap) => FileBacking::Mapped { _file: file, mmap },
                Err(_) => {
                    let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
                    FileBacking::Memory(bytes)
                }
            }
        };
        let source_dir = p.parent().map(|par| {
            std::fs::canonicalize(par).unwrap_or_else(|_| par.to_path_buf())
        });
        let bytes = backing.as_bytes();
        let report = ufo_core::open_bytes(&file_name, bytes, Some(bytes.len() as u64));
        store_session(&app_state, file_name, backing, source_dir, report)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn open_bytes_ipc(
    file_name: String,
    bytes: Vec<u8>,
    state: State<'_, Arc<AppState>>,
) -> Result<OpenSessionResponse, String> {
    let app_state = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || {
        let report = ufo_core::open_bytes(&file_name, &bytes, Some(bytes.len() as u64));
        store_session(&app_state, file_name, FileBacking::Memory(bytes), None, report)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn open_archive_entry_session(
    session_id: String,
    entry_path: String,
    state: State<'_, Arc<AppState>>,
) -> Result<OpenSessionResponse, String> {
    let app_state = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || {
        let entry = {
            let map = app_state.sessions.lock().map_err(|_| "Session lock poisoned")?;
            Arc::clone(map.get(&session_id).ok_or("Session not found")?)
        };
        let entry_bytes = ufo_core::extract_archive_entry(entry.backing.as_bytes(), &entry_path)?;
        let name = Path::new(&entry_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(entry_path);
        let report = ufo_core::open_bytes(&name, &entry_bytes, Some(entry_bytes.len() as u64));
        store_session(&app_state, name, FileBacking::Memory(entry_bytes), None, report)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn close_session(session_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let mut map = state.sessions.lock().map_err(|_| "Session lock poisoned")?;
    map.remove(&session_id);
    Ok(())
}

#[tauri::command]
async fn fetch_hex_page(
    session_id: String,
    offset: u64,
    length: usize,
    state: State<'_, Arc<AppState>>,
) -> Result<ufo_core::HexPage, String> {
    let app_state = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || {
        let entry = {
            let map = app_state.sessions.lock().map_err(|_| "Session lock poisoned")?;
            Arc::clone(map.get(&session_id).ok_or("Session not found")?)
        };
        Ok(ufo_core::slice_hex_page(entry.backing.as_bytes(), offset, length))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn fetch_text_page(
    session_id: String,
    start_line: usize,
    line_count: usize,
    lang: String,
    state: State<'_, Arc<AppState>>,
) -> Result<ufo_core::TextPage, String> {
    let app_state = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || {
        let entry = {
            let map = app_state.sessions.lock().map_err(|_| "Session lock poisoned")?;
            Arc::clone(map.get(&session_id).ok_or("Session not found")?)
        };
        Ok(ufo_core::slice_text_page(
            entry.backing.as_bytes(),
            start_line,
            line_count,
            &lang,
        ))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn read_raw_bytes(session_id: String, state: State<'_, Arc<AppState>>) -> Result<Response, String> {
    let app_state = Arc::clone(&state);
    tauri::async_runtime::spawn_blocking(move || {
        let entry = {
            let map = app_state.sessions.lock().map_err(|_| "Session lock poisoned")?;
            Arc::clone(map.get(&session_id).ok_or("Session not found")?)
        };
        let bytes = entry.backing.as_bytes();
        let len = bytes.len().min(256 * 1024 * 1024);
        Ok(Response::new(bytes[..len].to_vec()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn pick_files_dialog() -> Vec<String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        tauri::async_runtime::spawn_blocking(|| {
            rfd::FileDialog::new()
                .set_title("Open Any File — Universal File Opener")
                .pick_files()
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        })
        .await
        .unwrap_or_default()
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        Vec::new()
    }
}

#[tauri::command]
async fn resave_session_dialog(
    session_id: String,
    as_report_json: bool,
    state: State<'_, Arc<AppState>>,
) -> Result<Option<String>, String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let app_state = Arc::clone(&state);
        tauri::async_runtime::spawn_blocking(move || {
            let entry = {
                let map = app_state.sessions.lock().map_err(|_| "Session lock poisoned")?;
                Arc::clone(map.get(&session_id).ok_or("Session not found")?)
            };
            let export = ufo_core::prepare_resave(
                &entry.file_name,
                entry.backing.as_bytes(),
                as_report_json,
            )?;
            if let Some(dest) = rfd::FileDialog::new()
                .set_file_name(&export.suggested_name)
                .save_file()
            {
                std::fs::write(&dest, &export.bytes).map_err(|e| e.to_string())?;
                return Ok(Some(dest.to_string_lossy().to_string()));
            }
            Ok(None)
        })
        .await
        .map_err(|e| e.to_string())?
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (session_id, as_report_json, state);
        Ok(None)
    }
}

#[tauri::command]
fn window_minimize(window: Window) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    window.minimize().map_err(|e| e.to_string())?;
    let _ = window;
    Ok(())
}

#[tauri::command]
fn window_toggle_maximize(window: Window) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if window.is_maximized().unwrap_or(false) {
            window.unmaximize().map_err(|e| e.to_string())?;
        } else {
            window.maximize().map_err(|e| e.to_string())?;
        }
    }
    let _ = window;
    Ok(())
}

#[tauri::command]
fn window_toggle_fullscreen(window: Window) -> Result<bool, String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let next = !window.is_fullscreen().unwrap_or(false);
        window.set_fullscreen(next).map_err(|e| e.to_string())?;
        return Ok(next);
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = window;
        Ok(false)
    }
}

#[tauri::command]
fn window_close(window: Window) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    window.close().map_err(|e| e.to_string())?;
    let _ = window;
    Ok(())
}

#[tauri::command]
fn window_start_dragging(window: Window) -> Result<(), String> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    window.start_dragging().map_err(|e| e.to_string())?;
    let _ = window;
    Ok(())
}

fn percent_decode_utf8(raw: &str) -> String {
    let b = raw.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(hex_str) = std::str::from_utf8(&b[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex_str, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn asset_mime_for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "apng" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" | "svgz" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" | "cur" => "image/x-icon",
        "avif" => "image/avif",
        "tif" | "tiff" => "image/tiff",
        "jxl" => "image/jxl",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

fn serve_session_asset(app_state: &AppState, rest: &str) -> http::Response<Vec<u8>> {
    let not_found = || {
        http::Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .header("Access-Control-Allow-Origin", "*")
            .body(Vec::new())
            .unwrap()
    };
    let Some((session_id, rel_raw)) = rest.split_once('/') else {
        return not_found();
    };
    let entry = {
        let Ok(map) = app_state.sessions.lock() else {
            return http::Response::builder().status(500).body(Vec::new()).unwrap();
        };
        map.get(session_id).cloned()
    };
    let Some(base_dir) = entry.as_ref().and_then(|e| e.source_dir.as_ref()) else {
        return not_found();
    };
    let clean_rel = rel_raw.split(['?', '#']).next().unwrap_or("");
    let decoded = percent_decode_utf8(clean_rel).replace('\\', "/");
    let trimmed = decoded.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains('\0') {
        return not_found();
    }
    let rel_path = Path::new(trimmed);
    for comp in rel_path.components() {
        if matches!(
            comp,
            std::path::Component::RootDir | std::path::Component::Prefix(_)
        ) {
            return not_found();
        }
    }
    let Ok(canon_base) = std::fs::canonicalize(base_dir) else {
        return not_found();
    };
    let candidate = canon_base.join(rel_path);
    let Ok(canon_file) = std::fs::canonicalize(&candidate) else {
        return not_found();
    };
    if !canon_file.starts_with(&canon_base) {
        return not_found();
    }
    let Ok(meta) = std::fs::metadata(&canon_file) else {
        return not_found();
    };
    if !meta.is_file() || meta.len() > 64 * 1024 * 1024 {
        return not_found();
    }
    let Ok(bytes) = std::fs::read(&canon_file) else {
        return not_found();
    };
    let mime = asset_mime_for_path(&canon_file);
    http::Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_TYPE, mime)
        .header(http::header::CONTENT_LENGTH, bytes.len().to_string())
        .header("Access-Control-Allow-Origin", "*")
        .body(bytes)
        .unwrap()
}

/// Parses HTTP Range: bytes=START-END and serves 206 Partial Content chunks from zero-copy mmap.
fn handle_stream_request(
    app_state: &AppState,
    request: &http::Request<Vec<u8>>,
) -> http::Response<Vec<u8>> {
    if request.method() == http::Method::OPTIONS {
        return http::Response::builder()
            .status(http::StatusCode::NO_CONTENT)
            .header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS")
            .header("Access-Control-Allow-Headers", "Range")
            .body(Vec::new())
            .unwrap();
    }

    let path = request.uri().path();
    if let Some(rest) = path.strip_prefix("/asset/") {
        return serve_session_asset(app_state, rest);
    }

    let session_id = path.strip_prefix("/stream/").unwrap_or("").trim_matches('/');
    let entry = {
        let Ok(map) = app_state.sessions.lock() else {
            return http::Response::builder().status(500).body(Vec::new()).unwrap();
        };
        map.get(session_id).cloned()
    };
    let Some(entry) = entry else {
        return http::Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .header("Access-Control-Allow-Origin", "*")
            .body(Vec::new())
            .unwrap();
    };

    let bytes = entry.backing.as_bytes();
    let total = bytes.len();
    let mime = entry.mime.as_str();

    if total == 0 {
        return http::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_TYPE, mime)
            .header(http::header::ACCEPT_RANGES, "bytes")
            .header("Access-Control-Allow-Origin", "*")
            .body(Vec::new())
            .unwrap();
    }

    if let Some(range_hdr) = request
        .headers()
        .get(http::header::RANGE)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(spec) = range_hdr.strip_prefix("bytes=") {
            let (s_str, e_str) = spec.split_once('-').unwrap_or((spec, ""));
            let start = s_str.trim().parse::<usize>().unwrap_or(0).min(total - 1);
            let max_chunk = 4 * 1024 * 1024;
            let requested_end = e_str
                .trim()
                .parse::<usize>()
                .unwrap_or_else(|_| (start + max_chunk - 1).min(total - 1));
            let end = requested_end.min(total - 1).min(start + max_chunk - 1);
            let slice = bytes[start..=end].to_vec();
            return http::Response::builder()
                .status(http::StatusCode::PARTIAL_CONTENT)
                .header(http::header::CONTENT_TYPE, mime)
                .header(http::header::ACCEPT_RANGES, "bytes")
                .header(http::header::CONTENT_LENGTH, slice.len().to_string())
                .header(
                    http::header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .header("Access-Control-Allow-Origin", "*")
                .body(slice)
                .unwrap();
        }
    }

    let cap = total.min(64 * 1024 * 1024);
    http::Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::CONTENT_TYPE, mime)
        .header(http::header::ACCEPT_RANGES, "bytes")
        .header(http::header::CONTENT_LENGTH, cap.to_string())
        .header("Access-Control-Allow-Origin", "*")
        .body(bytes[..cap].to_vec())
        .unwrap()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let initial_files: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| Path::new(a).exists())
        .collect();

    let state = Arc::new(AppState::default());
    if let Ok(mut lp) = state.launch_paths.lock() {
        *lp = initial_files;
    }
    let proto_state = Arc::clone(&state);

    let mut builder = tauri::Builder::default();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let new_files: Vec<String> = argv
                .into_iter()
                .skip(1)
                .filter(|a| Path::new(a).exists())
                .collect();
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
            if !new_files.is_empty() {
                if let Some(st) = app.try_state::<Arc<AppState>>() {
                    if let Ok(mut pending) = st.launch_paths.lock() {
                        pending.extend(new_files.iter().cloned());
                    }
                }
                let _ = app.emit("single-instance-files", new_files);
            }
        }));
    }

    builder
        .manage(state)
        .register_asynchronous_uri_scheme_protocol("ufo", move |_ctx, request, responder| {
            let st = Arc::clone(&proto_state);
            tauri::async_runtime::spawn_blocking(move || {
                let resp = handle_stream_request(&st, &request);
                responder.respond(resp);
            });
        })
        .setup(|app| {
            let _ = app.get_webview_window("main");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_platform_info,
            take_launch_files,
            get_formats_catalog,
            open_path,
            open_bytes_ipc,
            open_archive_entry_session,
            close_session,
            fetch_hex_page,
            fetch_text_page,
            read_raw_bytes,
            pick_files_dialog,
            resave_session_dialog,
            window_minimize,
            window_toggle_maximize,
            window_toggle_fullscreen,
            window_close,
            window_start_dragging,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Universal File Opener");
}
