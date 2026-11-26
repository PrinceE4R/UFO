use std::collections::HashSet;

use crc32fast::Hasher as Crc32Hasher;
use sha1::{Digest as Sha1Digest, Sha1};
use sha2::Sha256;

use crate::model::{
    AmbiguityNotice, DetectionConfidence, DetectedFormat, FileHashes, FormatCategory,
};
use crate::registry::{
    lookup_ambiguity, lookup_extension, lookup_special_filename, FORMAT_REGISTRY,
};

const MAX_FULL_CRYPTO_HASH_BYTES: usize = 4 * 1024 * 1024;

/// Computes CRC32, SHA-256, SHA-1, Shannon entropy, printable ratio, and a 64-bin byte histogram.
pub fn compute_hashes(bytes: &[u8]) -> FileHashes {
    let (crc32_hex, sha256, sha1) = if bytes.len() <= MAX_FULL_CRYPTO_HASH_BYTES {
        let mut crc = Crc32Hasher::new();
        crc.update(bytes);
        let mut s256 = Sha256::new();
        s256.update(bytes);
        let mut s1 = Sha1::new();
        s1.update(bytes);
        (
            format!("{:08x}", crc.finalize()),
            hex::encode(s256.finalize()),
            hex::encode(s1.finalize()),
        )
    } else {
        // Ultra-fast sampled digest for large media/CAD/archive files (>4 MiB): avoids UI freeze.
        let head = &bytes[..1024 * 1024];
        let tail = &bytes[bytes.len() - 1024 * 1024..];
        let len_le = (bytes.len() as u64).to_le_bytes();
        let mut crc = Crc32Hasher::new();
        crc.update(head);
        crc.update(tail);
        crc.update(&len_le);
        let mut s256 = Sha256::new();
        s256.update(head);
        s256.update(tail);
        s256.update(len_le);
        let mut s1 = Sha1::new();
        s1.update(head);
        s1.update(tail);
        s1.update(len_le);
        (
            format!("{:08x} (sampled)", crc.finalize()),
            format!("{} (sampled)", hex::encode(s256.finalize())),
            format!("{} (sampled)", hex::encode(s1.finalize())),
        )
    };

    // Sample up to 2 MiB evenly across large files for entropy/histogram speed.
    let sample_step = (bytes.len() / (2 * 1024 * 1024)).max(1);
    let mut counts = [0u64; 256];
    let mut sampled_total = 0u64;
    let mut printable = 0u64;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let b = bytes[idx];
        counts[b as usize] += 1;
        sampled_total += 1;
        if (0x20..=0x7E).contains(&b) || matches!(b, b'\n' | b'\r' | b'\t') {
            printable += 1;
        }
        idx += sample_step;
    }

    let mut entropy = 0.0f64;
    if sampled_total > 0 {
        let total_f = sampled_total as f64;
        for &c in &counts {
            if c > 0 {
                let p = (c as f64) / total_f;
                entropy -= p * p.log2();
            }
        }
    }

    let mut histogram_64 = vec![0u32; 64];
    for bin in 0..64 {
        let sum: u64 = counts[bin * 4..bin * 4 + 4].iter().sum();
        histogram_64[bin] = sum.min(u32::MAX as u64) as u32;
    }

    let printable_ratio = if sampled_total > 0 {
        printable as f32 / sampled_total as f32
    } else {
        1.0
    };

    FileHashes {
        crc32: crc32_hex,
        sha256,
        sha1,
        entropy_bits: entropy as f32,
        printable_ratio,
        histogram_64,
    }
}

struct MagicMatch {
    id: &'static str,
    name: &'static str,
    category: FormatCategory,
    mime: &'static str,
    basis: String,
    confidence: DetectionConfidence,
    warnings: Vec<String>,
}

/// Sniffs magic bytes, container structures, text signatures, and extension ambiguity.
pub fn sniff_format(file_name: &str, bytes: &[u8], total_size: u64) -> DetectedFormat {
    let (ext_opt, reg_spec) = lookup_extension(file_name);
    let magic_hex = bytes
        .iter()
        .take(16)
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ");

    let magic_match = detect_by_magic_and_structure(bytes, total_size, ext_opt.as_deref());

    let (id, name, category, mime, confidence, mut basis, mut warnings) =
        if let Some(m) = magic_match {
            (
                m.id.to_string(),
                m.name.to_string(),
                m.category,
                m.mime.to_string(),
                m.confidence,
                m.basis,
                m.warnings,
            )
        } else if let Some((_, s_name, s_cat, s_mime, _)) = lookup_special_filename(file_name) {
            (
                file_name.to_ascii_lowercase(),
                (*s_name).to_string(),
                *s_cat,
                (*s_mime).to_string(),
                DetectionConfidence::ExtensionHeuristic,
                format!("Matched canonical developer/system filename '{file_name}'"),
                Vec::new(),
            )
        } else if let Some(spec) = reg_spec {
            (
                spec.ext.to_string(),
                spec.name.to_string(),
                spec.category,
                spec.mime.to_string(),
                DetectionConfidence::ExtensionHeuristic,
                format!("Identified from registered extension .{}", spec.ext),
                Vec::new(),
            )
        } else if looks_like_text(bytes) {
            (
                "txt".to_string(),
                "Plain / Structured Text".to_string(),
                FormatCategory::Document,
                "text/plain".to_string(),
                DetectionConfidence::TextHeuristic,
                "No binary magic signature; content validates as readable text".to_string(),
                Vec::new(),
            )
        } else {
            (
                "bin".to_string(),
                "Generic Binary / Unknown Format".to_string(),
                FormatCategory::UnknownBinary,
                "application/octet-stream".to_string(),
                DetectionConfidence::BinaryFallback,
                "No known magic signature matched; opened in Universal Binary & Hex Inspector"
                    .to_string(),
                vec!["Unrecognized binary container — inspect hex, strings, and entropy".into()],
            )
        };

    // Check extension ambiguity and extension-vs-magic mismatch (Section 26).
    let mut ambiguity = None;
    if let Some(ext) = ext_opt.as_deref() {
        let mismatch = check_extension_mismatch(ext, &id, category, bytes);
        if let Some(amb) = lookup_ambiguity(ext) {
            let resolved_meaning = format!("{name} ({id})");
            if mismatch {
                warnings.push(format!(
                    "Extension '.{ext}' does not match detected magic signature ({id}); trust magic bytes."
                ));
            }
            ambiguity = Some(AmbiguityNotice {
                extension: ext.to_string(),
                candidates: amb.candidates.iter().map(|s| (*s).to_string()).collect(),
                resolved_meaning,
                extension_mismatch: mismatch,
            });
            basis = format!("{basis} [Disambiguated .{ext}]");
        } else if mismatch {
            warnings.push(format!(
                "Filename extension '.{ext}' contradicts detected format '{name}' ({id})."
            ));
            ambiguity = Some(AmbiguityNotice {
                extension: ext.to_string(),
                candidates: vec![
                    reg_spec
                        .map(|s| format!("{} (.{})", s.name, s.ext))
                        .unwrap_or_else(|| format!(".{ext}")),
                    format!("{name} ({id})"),
                ],
                resolved_meaning: format!("{name} (verified by file signature)"),
                extension_mismatch: true,
            });
        }
    }

    DetectedFormat {
        id,
        name,
        category,
        mime,
        confidence,
        detection_basis: basis,
        magic_hex,
        ambiguity,
        warnings,
    }
}

fn check_extension_mismatch(
    ext: &str,
    detected_id: &str,
    detected_cat: FormatCategory,
    bytes: &[u8],
) -> bool {
    if bytes.is_empty() {
        return false;
    }
    match ext {
        "ts" => detected_id == "mpegts",
        "obj" => matches!(detected_id, "coff" | "elf" | "macho"),
        "pdb" => detected_id == "pdb_msf" || detected_id == "pdb_chem",
        "key" => matches!(detected_id, "keynote" | "pem" | "der"),
        "fig" => matches!(detected_id, "figma" | "xfig" | "mat"),
        "nc" => matches!(detected_id, "netcdf" | "hdf5"),
        "png" => detected_id != "png",
        "jpg" | "jpeg" | "jpe" | "jfif" => detected_id != "jpg",
        "webp" => detected_id != "webp",
        "gif" => detected_id != "gif",
        "pdf" => detected_id != "pdf",
        "zip" => !matches!(
            detected_id,
            "zip"
                | "docx"
                | "xlsx"
                | "pptx"
                | "apk"
                | "ipa"
                | "jar"
                | "epub"
                | "3mf"
                | "fcstd"
                | "fig"
                | "figcraft"
                | "pcraft"
                | "photocraft"
                | "ora"
                | "kra"
        ),
        "c" | "cpp" | "rs" | "py" | "js" | "json" | "xml" | "html" | "md" | "txt" => {
            matches!(
                detected_cat,
                FormatCategory::ExecutableLib
                    | FormatCategory::RasterImage
                    | FormatCategory::ArchivePackage
                    | FormatCategory::VideoSubtitles
                    | FormatCategory::Audio
            ) && !looks_like_text(bytes)
        }
        _ => false,
    }
}

fn detect_by_magic_and_structure(
    bytes: &[u8],
    total_size: u64,
    ext: Option<&str>,
) -> Option<MagicMatch> {
    if bytes.is_empty() {
        return None;
    }

    // 1. Images
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(mm("png", "Portable Network Graphics", FormatCategory::RasterImage, "image/png", "Magic bytes 89 50 4E 47 0D 0A 1A 0A"));
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(mm("jpg", "JPEG Image", FormatCategory::RasterImage, "image/jpeg", "JPEG SOI + marker FF D8 FF"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(mm("gif", "Graphics Interchange Format", FormatCategory::RasterImage, "image/gif", "GIF87a/GIF89a header"));
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" {
        let kind = &bytes[8..12];
        if kind == b"WEBP" {
            return Some(mm("webp", "WebP Image", FormatCategory::RasterImage, "image/webp", "RIFF container WEBP FourCC"));
        }
        if kind == b"WAVE" {
            return Some(mm("wav", "RIFF WAVE Audio", FormatCategory::Audio, "audio/wav", "RIFF container WAVE FourCC"));
        }
        if kind == b"AVI " {
            return Some(mm("avi", "Audio Video Interleave", FormatCategory::VideoSubtitles, "video/x-msvideo", "RIFF container AVI FourCC"));
        }
        if kind == b"ACON" {
            return Some(mm("ani", "Windows Animated Cursor", FormatCategory::VectorDesign, "application/x-navi-animation", "RIFF container ACON FourCC"));
        }
        if kind == b"sfbk" {
            return Some(mm("sf2", "SoundFont 2.0 Bank", FormatCategory::Audio, "application/x-soundfont", "RIFF container sfbk FourCC"));
        }
        if kind == b"CDXA" {
            return Some(mm("mpg", "MPEG Video (RIFF CDXA)", FormatCategory::VideoSubtitles, "video/mpeg", "RIFF CDXA"));
        }
    }
    if bytes.starts_with(b"BM") && bytes.len() >= 14 {
        let declared = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]) as u64;
        if declared == total_size || (declared > 14 && declared < total_size + 1024 && bytes[14] == 40) {
            return Some(mm("bmp", "Windows Bitmap (BMP)", FormatCategory::RasterImage, "image/bmp", "BM header + DIB info"));
        }
    }
    if bytes.starts_with(b"qoif") {
        return Some(mm("qoi", "Quite OK Image (QOI)", FormatCategory::RasterImage, "image/qoi", "Magic bytes 'qoif'"));
    }
    if bytes.starts_with(&[0x76, 0x2F, 0x31, 0x01]) {
        return Some(mm("exr", "OpenEXR HDR Image", FormatCategory::RasterImage, "image/x-exr", "OpenEXR magic 76 2F 31 01"));
    }
    if bytes.starts_with(b"#?RADIANCE") || bytes.starts_with(b"#?RGBE") {
        return Some(mm("hdr", "Radiance RGBE HDR Image", FormatCategory::RasterImage, "image/vnd.radiance", "Radiance #?RADIANCE header"));
    }
    if bytes.starts_with(b"8BPS") {
        let id = if bytes.get(5) == Some(&2) { "psb" } else { "psd" };
        return Some(mm(id, "Adobe Photoshop Document", FormatCategory::RasterImage, "image/vnd.adobe.photoshop", "Magic signature '8BPS'"));
    }
    if bytes.starts_with(b"gimp xcf ") {
        return Some(mm("xcf", "GIMP Project File", FormatCategory::RasterImage, "image/x-xcf", "Magic signature 'gimp xcf'"));
    }
    if bytes.starts_with(b"DDS ") {
        return Some(mm("dds", "DirectDraw Surface GPU Texture", FormatCategory::RasterImage, "image/vnd-ms.dds", "Magic signature 'DDS '"));
    }
    if bytes.starts_with(b"\xABKTX 11\xBB\r\n\x1A\n") {
        return Some(mm("ktx", "Khronos KTX Texture", FormatCategory::RasterImage, "image/ktx", "KTX 1.1 magic"));
    }
    if bytes.starts_with(b"\xABKTX 20\xBB\r\n\x1A\n") {
        return Some(mm("ktx2", "Khronos KTX2 Texture", FormatCategory::RasterImage, "image/ktx2", "KTX 2.0 magic"));
    }
    if bytes.starts_with(b"FUJIFILMCCD-RAW") {
        return Some(mm("raf", "Fujifilm RAW Photo", FormatCategory::RasterImage, "image/x-fuji-raf", "Fujifilm CCD-RAW signature"));
    }
    if bytes.starts_with(b"II*\x00") || bytes.starts_with(b"MM\x00*") || bytes.starts_with(b"IIRO") || bytes.starts_with(b"IIRS") || bytes.starts_with(b"II\x55\x00") {
        if bytes.starts_with(b"II*\x00") && bytes.get(8..10) == Some(b"CR") {
            return Some(mm("cr2", "Canon RAW 2 (CR2)", FormatCategory::RasterImage, "image/x-canon-cr2", "TIFF LE + Canon CR2 header"));
        }
        if matches!(ext, Some("nef" | "arw" | "dng" | "orf" | "rw2" | "pef" | "srw" | "3fr" | "raw")) {
            let e = ext.unwrap_or("dng");
            let e_static = FORMAT_REGISTRY.iter().find(|f| f.ext == e).map(|f| (f.ext, f.name, f.mime));
            if let Some((fid, fname, fmime)) = e_static {
                return Some(mm(fid, fname, FormatCategory::RasterImage, fmime, "TIFF/EXIF IFD container for Camera RAW"));
            }
        }
        return Some(mm("tiff", "Tagged Image File Format (TIFF)", FormatCategory::RasterImage, "image/tiff", "TIFF IFD byte-order header"));
    }
    if bytes.len() >= 6 && (bytes.starts_with(&[0, 0, 1, 0]) || bytes.starts_with(&[0, 0, 2, 0])) {
        let count = u16::from_le_bytes([bytes[4], bytes[5]]);
        if (1..=64).contains(&count) && bytes.len() >= 6 + (count as usize) * 16 {
            let is_cur = bytes[2] == 2;
            return Some(mm(
                if is_cur { "cur" } else { "ico" },
                if is_cur { "Windows Cursor Resource" } else { "Windows Icon Resource" },
                FormatCategory::VectorDesign,
                "image/x-icon",
                "ICO/CUR directory header",
            ));
        }
    }
    if bytes.starts_with(b"icns") {
        return Some(mm("icns", "Apple Icon Image (ICNS)", FormatCategory::VectorDesign, "image/icns", "Magic signature 'icns'"));
    }

    // 2. ISO Base Media (MP4, MOV, HEIC, AVIF, CR3)
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        if brand == b"crx " {
            return Some(mm("cr3", "Canon RAW 3 (CR3)", FormatCategory::RasterImage, "image/x-canon-cr3", "ISO-BMFF ftyp 'crx '"));
        }
        if matches!(brand, b"avif" | b"avis") {
            return Some(mm("avif", "AV1 Image File Format", FormatCategory::RasterImage, "image/avif", "ISO-BMFF ftyp 'avif'"));
        }
        if matches!(brand, b"heic" | b"heix" | b"mif1" | b"msf1") {
            return Some(mm("heic", "HEIF / HEIC Image", FormatCategory::RasterImage, "image/heic", "ISO-BMFF ftyp HEIF/HEIC"));
        }
        if brand == b"qt  " {
            return Some(mm("mov", "Apple QuickTime Movie", FormatCategory::VideoSubtitles, "video/quicktime", "ISO-BMFF ftyp 'qt  '"));
        }
        if brand == b"M4A " || matches!(ext, Some("m4a" | "alac")) {
            return Some(mm("m4a", "MPEG-4 Audio", FormatCategory::Audio, "audio/mp4", "ISO-BMFF ftyp M4A/MP4 audio"));
        }
        return Some(mm("mp4", "MPEG-4 Container (MP4)", FormatCategory::VideoSubtitles, "video/mp4", format!("ISO-BMFF ftyp '{}'", String::from_utf8_lossy(brand))));
    }

    // 3. Audio / Video Containers
    if bytes.starts_with(b"fLaC") {
        return Some(mm("flac", "FLAC Lossless Audio", FormatCategory::Audio, "audio/flac", "Magic bytes 'fLaC'"));
    }
    if bytes.starts_with(b"OggS") {
        let lower_head = String::from_utf8_lossy(&bytes[..bytes.len().min(128)]);
        if lower_head.contains("OpusHead") {
            return Some(mm("opus", "Ogg Opus Audio", FormatCategory::Audio, "audio/opus", "OggS + OpusHead stream"));
        }
        if lower_head.contains("theora") {
            return Some(mm("ogv", "Ogg Theora Video", FormatCategory::VideoSubtitles, "video/ogg", "OggS + Theora stream"));
        }
        return Some(mm("ogg", "Ogg Vorbis / Media Container", FormatCategory::Audio, "audio/ogg", "Magic bytes 'OggS'"));
    }
    if bytes.starts_with(b"ID3") || (bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE6) == 0xE2) {
        return Some(mm("mp3", "MP3 Audio Stream", FormatCategory::Audio, "audio/mpeg", "ID3v2 tag or MPEG audio sync frame"));
    }
    if bytes.starts_with(b"MThd") {
        return Some(mm("mid", "Standard MIDI Sequence", FormatCategory::Audio, "audio/midi", "MIDI header chunk 'MThd'"));
    }
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        let head_str = String::from_utf8_lossy(&bytes[..bytes.len().min(128)]).to_ascii_lowercase();
        if head_str.contains("webm") {
            return Some(mm("webm", "WebM Media Container", FormatCategory::VideoSubtitles, "video/webm", "EBML DocType 'webm'"));
        }
        return Some(mm("mkv", "Matroska Media Container", FormatCategory::VideoSubtitles, "video/x-matroska", "EBML Matroska header 1A 45 DF A3"));
    }
    if bytes.starts_with(b"FLV\x01") {
        return Some(mm("flv", "Flash Video Container", FormatCategory::VideoSubtitles, "video/x-flv", "FLV header 46 4C 56 01"));
    }
    if bytes.starts_with(b"FORM") && bytes.len() >= 12 {
        if &bytes[8..12] == b"AIFF" || &bytes[8..12] == b"AIFC" {
            return Some(mm("aiff", "Audio Interchange File Format", FormatCategory::Audio, "audio/aiff", "IFF FORM AIFF"));
        }
    }
    if bytes.starts_with(b".snd") {
        return Some(mm("au", "Sun/NeXT AU Audio", FormatCategory::Audio, "audio/basic", "Magic '.snd'"));
    }
    if bytes.starts_with(b"Creative Voice File") {
        return Some(mm("voc", "Creative Voice Audio", FormatCategory::Audio, "audio/x-voc", "Creative Voice File signature"));
    }
    // MPEG-TS sync check: 0x47 at 0, 188, 376
    if bytes.len() >= 377 && bytes[0] == 0x47 && bytes[188] == 0x47 && bytes[376] == 0x47 {
        return Some(mm("mpegts", "MPEG-2 Transport Stream (.ts/.m2ts)", FormatCategory::VideoSubtitles, "video/mp2t", "188-byte MPEG-TS 0x47 sync packets verified"));
    }

    // 4. Documents, Archives, Databases, CAD, Fonts, Executables
    if bytes.starts_with(b"%PDF-") {
        return Some(mm("pdf", "Portable Document Format (PDF)", FormatCategory::Document, "application/pdf", "Header signature '%PDF-'"));
    }
    if bytes.starts_with(b"{\\rtf") {
        return Some(mm("rtf", "Rich Text Format", FormatCategory::Document, "application/rtf", "Header signature '{\\rtf'"));
    }
    if bytes.starts_with(b"%!PS-Adobe") {
        if bytes[..bytes.len().min(64)].windows(4).any(|w| w == b"EPSF") {
            return Some(mm("eps", "Encapsulated PostScript", FormatCategory::VectorDesign, "application/postscript", "%!PS-Adobe EPSF header"));
        }
        return Some(mm("ps", "PostScript Document", FormatCategory::VectorDesign, "application/postscript", "%!PS-Adobe header"));
    }
    if bytes.starts_with(b"AT&TFORM") {
        return Some(mm("djvu", "DjVu Document", FormatCategory::Document, "image/vnd.djvu", "AT&TFORM DjVu magic"));
    }
    if bytes.starts_with(b"ITSF") {
        return Some(mm("chm", "Microsoft Compiled HTML Help", FormatCategory::Document, "application/vnd.ms-htmlhelp", "ITSF CHM signature"));
    }
    if bytes.starts_with(b"SQLite format 3\0") {
        return Some(mm("sqlite", "SQLite 3 Relational Database", FormatCategory::SpreadsheetDbSci, "application/vnd.sqlite3", "Header 'SQLite format 3\\0'"));
    }
    if bytes.starts_with(b"PAR1") {
        return Some(mm("parquet", "Apache Parquet Columnar Store", FormatCategory::SpreadsheetDbSci, "application/vnd.apache.parquet", "Parquet magic PAR1"));
    }
    if bytes.starts_with(b"ARROW1") {
        return Some(mm("arrow", "Apache Arrow IPC File", FormatCategory::SpreadsheetDbSci, "application/vnd.apache.arrow.file", "Arrow magic ARROW1"));
    }
    if bytes.starts_with(b"FEA1") {
        return Some(mm("feather", "Apache Feather Table", FormatCategory::SpreadsheetDbSci, "application/octet-stream", "Feather magic FEA1"));
    }
    if bytes.starts_with(b"\x89HDF\r\n\x1a\n") {
        return Some(mm("hdf5", "HDF5 Scientific Container", FormatCategory::SpreadsheetDbSci, "application/x-hdf5", "HDF5 superblock magic"));
    }
    if bytes.starts_with(b"CDF\x01") || bytes.starts_with(b"CDF\x02") || bytes.starts_with(b"CDF\x05") {
        return Some(mm("netcdf", "NetCDF Scientific Dataset", FormatCategory::SpreadsheetDbSci, "application/x-cdf", "NetCDF CDF magic"));
    }
    if bytes.starts_with(b"\x93NUMPY") {
        return Some(mm("npy", "NumPy Binary Array", FormatCategory::SpreadsheetDbSci, "application/octet-stream", "NumPy magic \\x93NUMPY"));
    }
    if bytes.starts_with(b"GGUF") {
        return Some(mm("gguf", "GGUF LLM Model Weights", FormatCategory::SpreadsheetDbSci, "application/octet-stream", "Magic bytes 'GGUF'"));
    }
    if bytes.starts_with(b"bplist00") || bytes.starts_with(b"bplist01") {
        return Some(mm("plist", "Apple Binary Property List", FormatCategory::NetworkingSecurity, "application/x-plist", "Binary plist 'bplist00' header"));
    }
    if bytes.starts_with(b"fig-kiwi") || bytes.starts_with(b"fig-jam.") || bytes.starts_with(b"fig-deck") {
        return Some(mm("fig", "Figma Design Document", FormatCategory::VectorDesign, "application/octet-stream", "Figma binary 'fig-kiwi' / 'fig-jam.' header"));
    }
    if bytes.starts_with(b"#FIG") {
        return Some(mm("fig", "Xfig Vector Drawing", FormatCategory::VectorDesign, "image/x-xfig", "Xfig '#FIG' header"));
    }
    if bytes.starts_with(b"MATLAB 5.0 MAT-file") || bytes.starts_with(b"MATLAB 7.") {
        return Some(mm("mat", "MATLAB MAT-File Binary", FormatCategory::SpreadsheetDbSci, "application/x-matlab-data", "MATLAB MAT-file header"));
    }

    // 5. Fonts
    if bytes.starts_with(b"wOFF") {
        return Some(mm("woff", "Web Open Font Format 1.0", FormatCategory::FontTypography, "font/woff", "Magic bytes 'wOFF'"));
    }
    if bytes.starts_with(b"wOF2") {
        return Some(mm("woff2", "Web Open Font Format 2.0", FormatCategory::FontTypography, "font/woff2", "Magic bytes 'wOF2'"));
    }
    if bytes.starts_with(b"OTTO") {
        return Some(mm("otf", "OpenType Font (CFF)", FormatCategory::FontTypography, "font/otf", "SFNT 'OTTO' signature"));
    }
    if bytes.starts_with(b"ttcf") {
        return Some(mm("ttc", "TrueType / OpenType Font Collection", FormatCategory::FontTypography, "font/collection", "SFNT collection 'ttcf'"));
    }
    if bytes.starts_with(&[0x00, 0x01, 0x00, 0x00, 0x00]) || bytes.starts_with(b"true\x00") {
        return Some(mm("ttf", "TrueType Font (SFNT)", FormatCategory::FontTypography, "font/ttf", "TrueType SFNT sfntVersion"));
    }

    // 6. 3D & CAD Binaries
    if bytes.starts_with(b"glTF") {
        return Some(mm("glb", "Binary glTF 2.0 3D Model", FormatCategory::Model3D, "model/gltf-binary", "Magic signature 'glTF'"));
    }
    if bytes.starts_with(b"Kaydara FBX Binary") {
        return Some(mm("fbx", "Autodesk FBX Binary 3D Scene", FormatCategory::Model3D, "application/octet-stream", "Kaydara FBX Binary header"));
    }
    if bytes.starts_with(b"BLENDER") {
        return Some(mm("blend", "Blender Project File", FormatCategory::Model3D, "application/x-blender", "Magic signature 'BLENDER'"));
    }
    if bytes.starts_with(b"PXR-USDC") {
        return Some(mm("usdc", "Pixar USD Crate Binary Scene", FormatCategory::Model3D, "application/octet-stream", "PXR-USDC binary header"));
    }
    if bytes.starts_with(b"3D Geometry File Format") {
        return Some(mm("3dm", "McNeel Rhino 3DM CAD Model", FormatCategory::CadBim, "model/vnd.rhino3dm", "Rhino '3D Geometry File Format' signature"));
    }
    if bytes.starts_with(b"ISO-10303-21;") {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_ascii_uppercase();
        if head.contains("IFC2X3") || head.contains("IFC4") {
            return Some(mm("ifc", "Industry Foundation Classes (IFC BIM)", FormatCategory::CadBim, "application/x-step", "ISO-10303-21 Part 21 with IFC schema"));
        }
        return Some(mm("step", "ISO 10303-21 STEP CAD", FormatCategory::CadBim, "model/step", "ISO-10303-21 Part 21 signature"));
    }
    if bytes.starts_with(b"ply\n") || bytes.starts_with(b"ply\r\n") {
        return Some(mm("ply", "Polygon File Format (PLY)", FormatCategory::Model3D, "application/x-ply", "PLY ASCII/binary header"));
    }
    if bytes.len() >= 84 && !bytes.starts_with(b"solid ") {
        let tri_count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as u64;
        if tri_count > 0 && 84u64.saturating_add(tri_count.saturating_mul(50)) == total_size {
            return Some(mm("stl", "Binary Stereolithography (STL)", FormatCategory::Model3D, "model/stl", format!("Binary STL structure verified ({tri_count} triangles)")));
        }
    }
    if bytes.starts_with(b"solid ") && ext == Some("stl") {
        return Some(mm("stl", "ASCII Stereolithography (STL)", FormatCategory::Model3D, "model/stl", "ASCII STL 'solid' header"));
    }
    if bytes.starts_with(b"AC10") && bytes.len() >= 6 && bytes[4].is_ascii_digit() && bytes[5].is_ascii_digit() {
        return Some(mm("dwg", "AutoCAD Binary Drawing (DWG)", FormatCategory::CadBim, "image/vnd.dwg", "AutoCAD AC10xx version header"));
    }

    // 7. Executables, Binaries, Firmware, Disk Images, Archives
    if bytes.starts_with(&[0x7F, b'E', b'L', b'F']) {
        return Some(mm("elf", "ELF Binary / Shared Object", FormatCategory::ExecutableLib, "application/x-executable", "ELF magic 7F 45 4C 46"));
    }
    if bytes.starts_with(b"\x00asm") && bytes.len() >= 8 {
        return Some(mm("wasm", "WebAssembly Binary Module", FormatCategory::SourceCode, "application/wasm", "WASM magic 00 61 73 6D"));
    }
    if bytes.starts_with(b"dex\n") {
        return Some(mm("dex", "Android Dalvik Executable (DEX)", FormatCategory::MobileDev, "application/vnd.android.dex", "DEX magic 'dex\\n'"));
    }
    if bytes.starts_with(b"MZ") && bytes.len() >= 64 {
        let lfanew = u32::from_le_bytes([bytes[60], bytes[61], bytes[62], bytes[63]]) as usize;
        if lfanew + 4 <= bytes.len() && &bytes[lfanew..lfanew + 4] == b"PE\x00\x00" {
            let id = match ext {
                Some("dll") => "dll",
                Some("sys") => "sys",
                Some("efi") => "efi",
                _ => "exe",
            };
            return Some(mm(id, "Windows Portable Executable (PE)", FormatCategory::ExecutableLib, "application/vnd.microsoft.portable-executable", "MZ DOS stub + PE\\0\\0 signature"));
        }
        return Some(mm("com", "DOS / MZ Executable", FormatCategory::ExecutableLib, "application/x-dosexec", "MZ executable header"));
    }
    if bytes.starts_with(&[0xFE, 0xED, 0xFA, 0xCE])
        || bytes.starts_with(&[0xFE, 0xED, 0xFA, 0xCF])
        || bytes.starts_with(&[0xCE, 0xFA, 0xED, 0xFE])
        || bytes.starts_with(&[0xCF, 0xFA, 0xED, 0xFE])
    {
        return Some(mm("macho", "Apple Mach-O Binary / dylib", FormatCategory::ExecutableLib, "application/x-mach-binary", "Mach-O magic header"));
    }
    if bytes.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]) && bytes.len() >= 8 {
        let count_or_ver = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if (1..=16).contains(&count_or_ver) {
            return Some(mm("macho", "Apple Universal Mach-O Binary", FormatCategory::ExecutableLib, "application/x-mach-binary", "Fat Mach-O CAFEBABE"));
        }
        return Some(mm("class", "Java JVM Class Bytecode", FormatCategory::SourceCode, "application/java-vm", "JVM class magic CAFEBABE"));
    }
    if bytes.starts_with(b"!<arch>\n") {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(128)]);
        if head.contains("debian-binary") {
            return Some(mm("deb", "Debian Software Package (.deb)", FormatCategory::ExecutableLib, "application/vnd.debian.binary-package", "AR archive with debian-binary"));
        }
        return Some(mm("ar", "Unix AR / Static Library Archive", FormatCategory::ArchivePackage, "application/x-archive", "Magic signature '!<arch>\\n'"));
    }
    if bytes.starts_with(b"Microsoft C/C++ MSF 7.00") {
        return Some(mm("pdb_msf", "Microsoft Program Debug Database (PDB)", FormatCategory::ExecutableLib, "application/octet-stream", "MSF 7.00 debug database header"));
    }
    if bytes.starts_with(&[0x4C, 0x00, 0x00, 0x00, 0x01, 0x14, 0x02, 0x00]) {
        return Some(mm("lnk", "Windows Shell Link Shortcut", FormatCategory::OperatingSystem, "application/x-ms-shortcut", "ShellLink CLSID header"));
    }
    if bytes.starts_with(b"ElfFile\x00") {
        return Some(mm("evtx", "Windows XML Event Log", FormatCategory::OperatingSystem, "application/octet-stream", "EVTX 'ElfFile\\0' signature"));
    }
    if bytes.starts_with(b"UF2\n") {
        return Some(mm("uf2", "Microsoft USB Flashing Format (UF2)", FormatCategory::ElectronicsPcb, "application/octet-stream", "UF2 magic 0x0A324655"));
    }
    if bytes.starts_with(&[0xD4, 0xC3, 0xB2, 0xA1])
        || bytes.starts_with(&[0xA1, 0xB2, 0xC3, 0xD4])
        || bytes.starts_with(&[0x4D, 0x3C, 0xB2, 0xA1])
    {
        return Some(mm("pcap", "libpcap Packet Capture", FormatCategory::NetworkingSecurity, "application/vnd.tcpdump.pcap", "PCAP magic header"));
    }
    if bytes.starts_with(&[0x0A, 0x0D, 0x0D, 0x0A]) && bytes.len() >= 12 && &bytes[8..12] == b"\x4D\x3C\x2B\x1A" {
        return Some(mm("pcapng", "PCAP Next-Gen Capture", FormatCategory::NetworkingSecurity, "application/x-pcapng", "PCAPNG Section Header Block"));
    }
    if bytes.starts_with(&[0x03, 0xD9, 0xA2, 0x9A]) {
        return Some(mm("kdbx", "KeePass 2.x Encrypted Database", FormatCategory::NetworkingSecurity, "application/x-keepass2", "KeePass KDBX signature"));
    }

    // 8. Compound File Binary (DOC, XLS, PPT, MSG, MSI, SolidEdge)
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        let id = match ext {
            Some("xls" | "xlt") => "xls",
            Some("ppt" | "pps") => "ppt",
            Some("msg") => "msg",
            Some("msi" | "msp") => "msi",
            _ => "doc",
        };
        return Some(mm(id, "Microsoft Compound File Binary (OLE2/CFB)", FormatCategory::Document, "application/x-ole-storage", "OLE2/CFB header D0 CF 11 E0 A1 B1 1A E1"));
    }

    // 9. ZIP & ZIP-based containers (DOCX, XLSX, PPTX, APK, IPA, EPUB, ODT, 3MF, FCStd, etc.)
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return Some(inspect_zip_container(bytes, ext));
    }

    // 10. Compression & Disk Archives (check .blend Zstd/Gzip compression first!)
    if bytes.starts_with(&[0x1F, 0x8B, 0x08]) {
        if ext == Some("blend") {
            return Some(mm("blend", "Blender Project File (Gzip)", FormatCategory::Model3D, "application/x-blender", "Gzip-compressed Blender .blend container"));
        }
        if ext == Some("vectorcraft") {
            return Some(mm("vectorcraft", "ArtCraft VectorCraft Document (Gzip)", FormatCategory::VectorDesign, "application/gzip", "Gzip-compressed VectorCraft container"));
        }
        let id = match ext {
            Some("svgz") => "svgz",
            Some("tgz" | "tar.gz" | "crate" | "unitypackage") => "tar.gz",
            _ => "gz",
        };
        return Some(mm(id, "Gzip Compressed Stream / Archive", FormatCategory::ArchivePackage, "application/gzip", "Gzip magic 1F 8B 08"));
    }
    if bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        if ext == Some("blend") {
            return Some(mm("blend", "Blender Project File (Zstd)", FormatCategory::Model3D, "application/x-blender", "Zstandard-compressed Blender .blend container"));
        }
        if let Ok(mut dec) = ruzstd::decoding::StreamingDecoder::new(std::io::Cursor::new(bytes)) {
            let mut probe = [0u8; 16];
            if std::io::Read::read(&mut dec, &mut probe).unwrap_or(0) >= 7 && probe.starts_with(b"BLENDER") {
                return Some(mm("blend", "Blender Project File (Zstd)", FormatCategory::Model3D, "application/x-blender", "Zstandard stream contains BLENDER magic"));
            }
        }
        let id = if matches!(ext, Some("tar.zst" | "tzst")) { "tar.zst" } else { "zst" };
        return Some(mm(id, "Zstandard Compressed Archive", FormatCategory::ArchivePackage, "application/zstd", "Zstd magic 28 B5 2F FD"));
    }
    if bytes.starts_with(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]) {
        return Some(mm("xz", "XZ / LZMA2 Compressed Stream", FormatCategory::ArchivePackage, "application/x-xz", "XZ magic FD 37 7A 58 5A 00"));
    }
    if bytes.starts_with(b"BZh") && bytes.get(3).is_some_and(|b| (b'1'..=b'9').contains(b)) {
        return Some(mm("bz2", "Bzip2 Compressed Stream", FormatCategory::ArchivePackage, "application/x-bzip2", "Bzip2 'BZh' header"));
    }
    if bytes.starts_with(&[0x04, 0x22, 0x4D, 0x18]) {
        return Some(mm("lz4", "LZ4 Frame Archive", FormatCategory::ArchivePackage, "application/x-lz4", "LZ4 magic 04 22 4D 18"));
    }
    if bytes.starts_with(b"7z\xBC\xAF\x27\x1C") {
        return Some(mm("7z", "7-Zip Archive", FormatCategory::ArchivePackage, "application/x-7z-compressed", "7-Zip signature 37 7A BC AF 27 1C"));
    }
    if bytes.starts_with(b"Rar!\x1A\x07") {
        return Some(mm("rar", "RAR Archive", FormatCategory::ArchivePackage, "application/vnd.rar", "RAR archive signature"));
    }
    if bytes.starts_with(b"MSCF") {
        return Some(mm("cab", "Microsoft Cabinet Archive", FormatCategory::ArchivePackage, "application/vnd.ms-cab-compressed", "CAB 'MSCF' signature"));
    }
    if bytes.starts_with(b"QFI\xFB") {
        return Some(mm("qcow2", "QEMU Copy-On-Write v2 Disk", FormatCategory::LinuxFirmwareDisk, "application/x-qcow2", "QCOW2 magic QFI\\xFB"));
    }
    if bytes.starts_with(b"KDMV") {
        return Some(mm("vmdk", "VMware Sparse Disk Image", FormatCategory::LinuxFirmwareDisk, "application/x-vmdk", "VMDK 'KDMV' signature"));
    }
    if bytes.starts_with(b"vhdxfile") {
        return Some(mm("vhdx", "Hyper-V Virtual Hard Disk (VHDX)", FormatCategory::LinuxFirmwareDisk, "application/x-vhdx", "VHDX 'vhdxfile' signature"));
    }
    if bytes.len() > 0x8006 && &bytes[0x8001..0x8006] == b"CD001" {
        return Some(mm("iso", "ISO-9660 Optical Disc Image", FormatCategory::LinuxFirmwareDisk, "application/x-iso9660-image", "ISO-9660 Primary Volume Descriptor 'CD001' at 0x8001"));
    }
    if bytes.len() >= 265 && &bytes[257..262] == b"ustar" {
        return Some(mm("tar", "POSIX TAR Archive", FormatCategory::ArchivePackage, "application/x-tar", "USTAR 'ustar' magic at offset 257"));
    }

    // 11. Text-based structural magic (SVG, XML, JSON, Bencode, PEM, Gerber, KiCad, G-code, etc.)
    if looks_like_text(bytes) {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        let trimmed = head.trim_start_matches('\u{FEFF}').trim_start();
        if trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && head.contains("<svg")) {
            return Some(mm("svg", "Scalable Vector Graphics (SVG)", FormatCategory::VectorDesign, "image/svg+xml", "XML <svg> root element"));
        }
        if trimmed.starts_with("<!DOCTYPE html") || trimmed.starts_with("<html") {
            return Some(mm("html", "HTML Document", FormatCategory::WebData, "text/html", "HTML root signature"));
        }
        if trimmed.starts_with("<?xml") {
            if head.contains("<plist") {
                return Some(mm("plist", "Apple Property List XML", FormatCategory::NetworkingSecurity, "application/x-plist", "XML <plist> root"));
            }
            if head.contains("<gpx") {
                return Some(mm("gpx", "GPS Exchange XML (GPX)", FormatCategory::SpreadsheetDbSci, "application/gpx+xml", "XML <gpx> root"));
            }
            if head.contains("<kml") {
                return Some(mm("kml", "Keyhole Markup Language (KML)", FormatCategory::SpreadsheetDbSci, "application/vnd.google-earth.kml+xml", "XML <kml> root"));
            }
            if head.contains("<FictionBook") {
                return Some(mm("fb2", "FictionBook E-Book XML", FormatCategory::Document, "application/x-fictionbook+xml", "XML <FictionBook> root"));
            }
            if head.contains("<COLLADA") {
                return Some(mm("dae", "COLLADA 3D XML", FormatCategory::Model3D, "model/vnd.collada+xml", "XML <COLLADA> root"));
            }
            return Some(mm("xml", "XML Document", FormatCategory::WebData, "application/xml", "XML declaration <?xml"));
        }
        if trimmed.starts_with("WEBVTT") {
            return Some(mm("vtt", "WebVTT Captions", FormatCategory::VideoSubtitles, "text/vtt", "WebVTT header"));
        }
        if trimmed.starts_with("-----BEGIN ") {
            return Some(mm("pem", "PEM Encoded Certificate / Key", FormatCategory::NetworkingSecurity, "application/x-pem-file", "PEM ASCII-armor header"));
        }
        if trimmed.starts_with("(kicad_pcb") {
            return Some(mm("kicad_pcb", "KiCad PCB Layout", FormatCategory::ElectronicsPcb, "text/plain", "KiCad S-expression (kicad_pcb)"));
        }
        if trimmed.starts_with("(kicad_sch") {
            return Some(mm("kicad_sch", "KiCad Schematic", FormatCategory::ElectronicsPcb, "text/plain", "KiCad S-expression (kicad_sch)"));
        }
        if trimmed.starts_with("{") || trimmed.starts_with("[") {
            if ext == Some("vectorcraft") || (trimmed.contains("\"format\"") && (trimmed.contains("\"vectorcraft\"") || trimmed.contains("\"drawcraft\""))) {
                return Some(mm("vectorcraft", "ArtCraft VectorCraft Document", FormatCategory::VectorDesign, "application/json", "VectorCraft JSON structure"));
            }
            if ext == Some("figcraft") || (trimmed.contains("\"format\"") && trimmed.contains("\"figcraft\"")) {
                return Some(mm("figcraft", "ArtCraft FigCraft Design", FormatCategory::VectorDesign, "application/json", "FigCraft JSON structure"));
            }
            if trimmed.contains("\"nbformat\"") && trimmed.contains("\"cells\"") {
                return Some(mm("ipynb", "Jupyter Notebook", FormatCategory::SourceCode, "application/x-ipynb+json", "JSON notebook structure (nbformat/cells)"));
            }
            if trimmed.contains("\"FeatureCollection\"") && trimmed.contains("\"coordinates\"") {
                return Some(mm("geojson", "GeoJSON Geographic Data", FormatCategory::SpreadsheetDbSci, "application/geo+json", "GeoJSON FeatureCollection"));
            }
            if ext.is_none() || matches!(ext, Some("json" | "jsonc" | "json5" | "har" | "webmanifest")) {
                return Some(mm("json", "JSON Data", FormatCategory::WebData, "application/json", "JSON structural root"));
            }
        }
        if bytes.starts_with(b"d8:announce") || bytes.starts_with(b"d4:info") {
            return Some(mm("torrent", "BitTorrent Metadata File", FormatCategory::NetworkingSecurity, "application/x-bittorrent", "Bencode dictionary signature"));
        }
        if ext == Some("pdb") && (head.contains("ATOM  ") || head.contains("HEADER    ")) {
            return Some(mm("pdb_chem", "Protein Data Bank Molecular Structure", FormatCategory::SpreadsheetDbSci, "chemical/x-pdb", "PDB ATOM/HEADER column records"));
        }
        if ext == Some("obj") && head.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("v ") || t.starts_with("vt ") || t.starts_with("vn ") || t.starts_with("f ")
        }) {
            return Some(mm("obj", "Wavefront 3D Geometry (.obj)", FormatCategory::Model3D, "model/obj", "Wavefront vertex/face directives"));
        }
    }

    // COFF relocatable object check (before `infer`, which returns extension "obj" for COFF)
    if bytes.len() >= 20 {
        let machine = u16::from_le_bytes([bytes[0], bytes[1]]);
        if matches!(machine, 0x8664 | 0x014C | 0xAA64 | 0x01C4) && !looks_like_text(bytes) {
            return Some(mm("coff", "COFF Relocatable Object (.obj)", FormatCategory::ExecutableLib, "application/x-object", format!("COFF machine ID 0x{machine:04X}")));
        }
    }

    // Fallback to `infer` crate for any additional magic signatures it knows
    if let Some(kind) = infer::get(bytes) {
        let ext_guess = if kind.mime_type().contains("coff") { "coff" } else { kind.extension() };
        let cat = if ext_guess == "coff" {
            FormatCategory::ExecutableLib
        } else {
            FORMAT_REGISTRY
                .iter()
                .find(|f| f.ext == ext_guess)
                .map(|f| f.category)
                .unwrap_or(FormatCategory::GenericMisc)
        };
        return Some(mm(
            ext_guess,
            kind.mime_type(),
            cat,
            kind.mime_type(),
            format!("Magic signature match ({})", kind.mime_type()),
        ));
    }

    None
}

fn inspect_zip_container(bytes: &[u8], ext: Option<&str>) -> MagicMatch {
    let mut names = HashSet::new();
    if let Ok(mut archive) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) {
        for i in 0..archive.len().min(96) {
            if let Ok(f) = archive.by_index(i) {
                if let Ok(n) = f.name() {
                    names.insert(n.into_owned());
                }
            }
        }
    }

    if names.contains("word/document.xml") {
        return mm("docx", "Microsoft Word OOXML (.docx)", FormatCategory::Document, "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "ZIP container with word/document.xml");
    }
    if names.contains("xl/workbook.xml") {
        return mm("xlsx", "Microsoft Excel OOXML (.xlsx)", FormatCategory::SpreadsheetDbSci, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", "ZIP container with xl/workbook.xml");
    }
    if names.contains("ppt/presentation.xml") {
        return mm("pptx", "Microsoft PowerPoint OOXML (.pptx)", FormatCategory::OfficePresentation, "application/vnd.openxmlformats-officedocument.presentationml.presentation", "ZIP container with ppt/presentation.xml");
    }
    if names.contains("AndroidManifest.xml") {
        return mm("apk", "Android Application Package (APK)", FormatCategory::ExecutableLib, "application/vnd.android.package-archive", "ZIP container with AndroidManifest.xml");
    }
    if names.iter().any(|n| n.starts_with("Payload/") && n.contains(".app/")) {
        return mm("ipa", "iOS App Archive (IPA)", FormatCategory::ExecutableLib, "application/octet-stream", "ZIP container with Payload/*.app");
    }
    if names.contains("Document.xml") && (names.contains("GuiDocument.xml") || ext == Some("fcstd")) {
        return mm("fcstd", "FreeCAD Document Archive", FormatCategory::CadBim, "application/zip", "ZIP container with FreeCAD Document.xml");
    }
    if names.contains("3D/3dmodel.model") || names.iter().any(|n| n.ends_with(".model")) {
        return mm("3mf", "3D Manufacturing Format (3MF)", FormatCategory::CadBim, "model/3mf", "ZIP container with 3D/3dmodel.model");
    }
    if names.contains("META-INF/container.xml") {
        return mm("epub", "EPUB E-Book Container", FormatCategory::Document, "application/epub+zip", "ZIP container with META-INF/container.xml");
    }
    if names.contains("META-INF/MANIFEST.MF") && matches!(ext, Some("jar" | "war" | "ear")) {
        return mm("jar", "Java Archive (JAR)", FormatCategory::SourceCode, "application/java-archive", "ZIP container with META-INF/MANIFEST.MF");
    }
    if names.contains("content.xml") && names.contains("mimetype") {
        let id = match ext {
            Some("ods") => "ods",
            Some("odp") => "odp",
            _ => "odt",
        };
        return mm(id, "OpenDocument Archive (ODF)", FormatCategory::Document, "application/vnd.oasis.opendocument.text", "ZIP ODF container with content.xml");
    }
    if names.contains("canvas.fig") {
        return mm("fig", "Figma Packaged Design (.fig)", FormatCategory::VectorDesign, "application/zip", "ZIP container with canvas.fig");
    }
    if names.contains("stack.xml") || ext == Some("ora") {
        return mm("ora", "OpenRaster Layered Image (.ora)", FormatCategory::RasterImage, "image/openraster", "ZIP OpenRaster container with stack.xml");
    }
    if names.contains("maindoc.xml") || ext == Some("kra") {
        return mm("kra", "Krita Layered Document (.kra)", FormatCategory::RasterImage, "application/x-krita", "ZIP Krita container with maindoc.xml");
    }
    if names.contains("manifest.json") && (ext == Some("pcraft") || ext == Some("photocraft") || names.iter().any(|n| n.starts_with("tiles/") || n.starts_with("composite/"))) {
        return mm("pcraft", "ArtCraft PhotoCraft Project", FormatCategory::RasterImage, "application/zip", "ZIP container with PhotoCraft manifest.json");
    }
    if names.contains("document.json") && (ext == Some("figcraft") || names.iter().any(|n| n.starts_with("images/") || n == "thumbnail.png")) {
        return mm("figcraft", "ArtCraft FigCraft Design", FormatCategory::VectorDesign, "application/zip", "ZIP container with FigCraft document.json");
    }
    if names.iter().any(|n| n.starts_with("Index/") && n.ends_with(".iwa")) || ext == Some("key") {
        return mm("keynote", "Apple Keynote / iWork Package", FormatCategory::OfficePresentation, "application/zip", "ZIP iWork IWA container");
    }
    if ext == Some("cbz") {
        return mm("cbz", "Comic Book ZIP Archive", FormatCategory::Document, "application/vnd.comicbook+zip", "ZIP comic book archive");
    }

    mm("zip", "ZIP Compressed Archive", FormatCategory::ArchivePackage, "application/zip", "PK\\x03\\x04 ZIP signature")
}

fn mm(
    id: &'static str,
    name: &'static str,
    category: FormatCategory,
    mime: &'static str,
    basis: impl Into<String>,
) -> MagicMatch {
    MagicMatch {
        id,
        name,
        category,
        mime,
        basis: basis.into(),
        confidence: DetectionConfidence::SignatureVerified,
        warnings: Vec::new(),
    }
}

pub fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let slice = &bytes[..bytes.len().min(8192)];
    if slice.starts_with(b"\xFF\xFE") || slice.starts_with(b"\xFE\xFF") {
        return true;
    }
    let nul_count = slice.iter().filter(|&&b| b == 0).count();
    if nul_count > 0 {
        return false;
    }
    if simdutf8::basic::from_utf8(slice).is_ok() {
        let ctrl = slice
            .iter()
            .filter(|&&b| b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0C))
            .count();
        return ctrl * 20 < slice.len().max(1);
    }
    let printable = slice
        .iter()
        .filter(|&&b| (0x20..=0x7E).contains(&b) || matches!(b, b'\n' | b'\r' | b'\t') || b >= 0x80)
        .count();
    printable * 100 >= slice.len() * 88
}
