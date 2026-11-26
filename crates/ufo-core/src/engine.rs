use crate::decoders::{
    archive, data, design, document, font, hex_bin, image, media, mesh3d, text,
};
use crate::magic::{compute_hashes, looks_like_text, sniff_format};
use crate::model::{
    ExportResult, FormatCategory, HexPage, MetadataEntry, OpenFileReport, TextPage, ViewerPayload,
};
use crate::registry::{lookup_extension, lookup_special_filename};

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

/// Primary entry point: identifies and opens any file buffer into an OpenFileReport.
pub fn open_bytes(file_name: &str, bytes: &[u8], total_size_override: Option<u64>) -> OpenFileReport {
    #[cfg(not(target_arch = "wasm32"))]
    let start = Instant::now();

    let total_size = total_size_override.unwrap_or(bytes.len() as u64);
    let (ext, reg_spec) = lookup_extension(file_name);
    let detected = sniff_format(file_name, bytes, total_size);
    let hashes = compute_hashes(bytes);
    let hex_preview = hex_bin::build_hex_rows(bytes, 0, 4096);
    let preview_strings = hex_bin::extract_strings(bytes, 120);

    let mut metadata = vec![
        MetadataEntry { section: "Identity".into(), key: "File Name".into(), value: file_name.to_string() },
        MetadataEntry { section: "Identity".into(), key: "Category".into(), value: detected.category.label().to_string() },
        MetadataEntry { section: "Identity".into(), key: "MIME Type".into(), value: detected.mime.clone() },
        MetadataEntry { section: "Identity".into(), key: "Detection Basis".into(), value: detected.detection_basis.clone() },
        MetadataEntry { section: "Integrity".into(), key: "Size".into(), value: format!("{total_size} bytes") },
        MetadataEntry { section: "Integrity".into(), key: "CRC-32".into(), value: hashes.crc32.clone() },
        MetadataEntry { section: "Integrity".into(), key: "SHA-256".into(), value: hashes.sha256.clone() },
        MetadataEntry { section: "Integrity".into(), key: "Entropy".into(), value: format!("{:.3} bits/byte", hashes.entropy_bits) },
    ];

    let lang_hint = lookup_special_filename(file_name)
        .map(|s| s.4)
        .or_else(|| reg_spec.and_then(|r| r.language))
        .unwrap_or_else(|| match detected.id.as_str() {
            "json" | "geojson" | "har" => "json",
            "xml" | "svg" | "dae" | "gpx" | "kml" | "fb2" | "plist" => "xml",
            "html" | "htm" => "html",
            "md" | "markdown" => "markdown",
            "ts" => "typescript",
            "obj" => "text",
            _ => "text",
        });

    let viewer = dispatch_viewer(bytes, &detected, lang_hint, &mut metadata);

    #[cfg(not(target_arch = "wasm32"))]
    let parse_time_us = start.elapsed().as_micros() as u64;
    #[cfg(target_arch = "wasm32")]
    let parse_time_us = 0u64;

    OpenFileReport {
        file_name: file_name.to_string(),
        file_size: total_size,
        extension: ext,
        detected,
        hashes,
        metadata,
        parse_time_us,
        viewer,
        hex_preview,
        preview_strings,
    }
}

fn dispatch_viewer(
    bytes: &[u8],
    detected: &crate::model::DetectedFormat,
    lang_hint: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let id = detected.id.as_str();

    // 0. Layered Design & Multi-Page Projects (.psd, .psb, .fig, .figcraft, .vectorcraft, .pcraft, .ora, .kra)
    if matches!(
        id,
        "psd" | "psb" | "fig" | "figcraft" | "vectorcraft" | "pcraft" | "photocraft" | "ora" | "kra"
    ) {
        if let Some(v) = design::decode_design(bytes, id, metadata) {
            return v;
        }
    }

    // 1. SVG / Raster Images / Camera RAW / PSD / EXR / AVIF / HEIC / JXL / JP2
    if matches!(
        id,
        "svg" | "svgz" | "png" | "jpg" | "jpeg" | "jpe" | "jfif" | "webp" | "gif" | "bmp" | "dib"
            | "tif" | "tiff" | "ico" | "cur" | "tga" | "icb" | "pcx" | "ppm" | "pgm" | "pbm"
            | "pnm" | "pam" | "qoi" | "exr" | "hdr" | "dds" | "xpm" | "psd" | "psb" | "icns"
            | "avif" | "heic" | "heif" | "jxl" | "jp2" | "j2k" | "kra" | "ora"
            | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "raf" | "pef" | "srw"
            | "3fr"
    ) {
        if let Some(v) = image::decode_image(bytes, id, metadata) {
            return v;
        }
    }

    // 2. Fonts & Typography
    if matches!(id, "ttf" | "otf" | "ttc" | "otc" | "woff" | "woff2") {
        if let Some(v) = font::decode_font(bytes, metadata) {
            return v;
        }
    }

    // 3. 3D Models, CAD, CNC, Fabrication, PCB/Gerber/KiCad
    if matches!(
        id,
        "stl" | "obj" | "ply" | "splat" | "glb" | "gltf" | "3mf" | "step" | "stp" | "ifc"
            | "iges" | "igs" | "3dm" | "fcstd" | "fcstd1" | "blend" | "gcode" | "gco"
            | "nc" | "ngc" | "tap" | "cnc" | "dxf" | "pcd" | "pts" | "xyz"
            | "gerber" | "gbr" | "gbx" | "gtl" | "gbl" | "gts" | "gbs" | "gto" | "gbo"
            | "kicad_pcb" | "kicad_sch" | "kicad_mod" | "fbx" | "bvh" | "usd" | "usda" | "usdc"
            | "usdz"
    ) {
        if let Some(v) = mesh3d::decode_3d_or_cad(bytes, id, metadata) {
            return v;
        }
    }

    // 4. Databases, ML Models & Geo Data
    if matches!(
        id,
        "sqlite" | "sqlite3" | "db3" | "db" | "aup3" | "safetensors" | "gguf" | "npy"
            | "geojson" | "gpx" | "kml" | "parquet"
    ) {
        if let Some(v) = data::decode_data_or_scientific(bytes, id, metadata) {
            return v;
        }
    }

    // 5. Audio, Video, MIDI, Subtitles
    if matches!(
        id,
        "wav" | "wave" | "mp3" | "flac" | "ogg" | "oga" | "opus" | "spx" | "ogv" | "mp4"
            | "m4a" | "m4v" | "mov" | "f4v" | "alac" | "3gp" | "3g2" | "mkv" | "webm" | "mka"
            | "avi" | "mid" | "midi" | "kar" | "srt" | "vtt" | "ass" | "ssa" | "lrc" | "aac"
            | "aiff" | "aif" | "au" | "snd" | "voc" | "wma" | "wv" | "ape" | "ac3" | "dts"
            | "caf" | "dsf" | "dff" | "mpegts" | "mpg" | "mpeg" | "flv"
    ) {
        if let Some(v) = media::decode_media(bytes, id, metadata) {
            return v;
        }
    }

    // 6. Documents & Office
    if matches!(
        id,
        "pdf" | "docx" | "docm" | "dotx" | "dotm" | "xlsx" | "xlsm" | "xltx" | "xltm"
            | "pptx" | "pptm" | "ppsx" | "potx" | "odt" | "ods" | "odp" | "epub" | "fb2"
            | "xps" | "oxps" | "doc" | "xls" | "ppt" | "msg" | "msi" | "msp" | "pps" | "rtf"
            | "eml" | "vcf" | "ics" | "ical" | "vcs"
    ) {
        if let Some(v) = document::decode_document(bytes, id, metadata) {
            return v;
        }
    }

    // 7. Archives, Packages & Disk Images
    if matches!(
        id,
        "zip" | "apk" | "aab" | "ipa" | "jar" | "war" | "ear" | "whl" | "egg" | "nupkg"
            | "cbz" | "apks" | "xapk" | "aar" | "ipsw" | "tar" | "gz" | "tgz" | "tar.gz"
            | "zst" | "tar.zst" | "iso" | "ar" | "deb" | "7z" | "rar" | "cab" | "crate"
            | "unitypackage" | "ova" | "cbt" | "gem" | "sketch" | "idml" | "sl1" | "sl2"
            | "prproj"
    ) {
        if let Some(v) = archive::decode_archive(bytes, id, metadata) {
            return v;
        }
    }

    // 8. Compiled Executables, Object Files, Firmware, Network Captures
    if matches!(
        id,
        "exe" | "dll" | "sys" | "efi" | "com" | "elf" | "so" | "o" | "ko" | "out" | "coff"
            | "macho" | "dylib" | "wasm" | "dex" | "pcap" | "pcapng" | "torrent" | "lnk"
            | "uf2" | "der" | "crt" | "cer" | "cert"
    ) {
        return hex_bin::decode_binary(bytes, detected, metadata);
    }

    // 9. Source code, Configs, Markup, Logs, CSV/TSV, and any readable text
    if matches!(
        detected.category,
        FormatCategory::SourceCode
            | FormatCategory::WebData
            | FormatCategory::BuildMetadata
            | FormatCategory::Document
            | FormatCategory::GenericMisc
    ) || matches!(id, "csv" | "tsv" | "ipynb" | "json" | "toml" | "plist")
        || looks_like_text(bytes)
    {
        return text::decode_text_viewer(bytes, id, lang_hint, metadata);
    }

    // 10. Universal Binary & Hex Fallback — NEVER fails to open any file!
    hex_bin::decode_binary(bytes, detected, metadata)
}

pub fn slice_hex_page(bytes: &[u8], offset: u64, length: usize) -> HexPage {
    hex_bin::slice_hex_page(bytes, offset, length)
}

pub fn slice_text_page(bytes: &[u8], start_line: usize, line_count: usize, lang: &str) -> TextPage {
    text::slice_text_page(bytes, start_line, line_count, lang)
}

pub fn extract_archive_entry(bytes: &[u8], entry_path: &str) -> Result<Vec<u8>, String> {
    archive::extract_archive_entry(bytes, entry_path)
}

/// Prepares a Re-Save export payload (original copy or metadata JSON report).
pub fn prepare_resave(
    file_name: &str,
    bytes: &[u8],
    as_report_json: bool,
) -> Result<ExportResult, String> {
    if as_report_json {
        let report = open_bytes(file_name, bytes, None);
        let json = serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?;
        Ok(ExportResult {
            suggested_name: format!("{file_name}.ufo-report.json"),
            mime: "application/json".into(),
            bytes: json,
        })
    } else {
        Ok(ExportResult {
            suggested_name: file_name.to_string(),
            mime: "application/octet-stream".into(),
            bytes: bytes.to_vec(),
        })
    }
}
