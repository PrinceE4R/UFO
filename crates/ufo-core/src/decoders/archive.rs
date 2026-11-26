use std::io::{Cursor, Read as _};

use flate2::read::GzDecoder;
use ruzstd::decoding::StreamingDecoder as ZstdDecoder;

use crate::model::{ArchiveEntry, MetadataEntry, ViewerPayload};

const MAX_ARCHIVE_ENTRIES: usize = 2500;

/// Inspects ZIP, APK, IPA, JAR, TAR, TAR.GZ, TAR.ZST, ISO-9660, AR/DEB, GZ, ZST, Brotli, 7z, RAR, CAB.
pub fn decode_archive(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") || matches!(
        format_id,
        "zip" | "apk" | "aab" | "ipa" | "jar" | "war" | "ear" | "whl" | "egg" | "nupkg" | "cbz" | "apks" | "xapk" | "aar" | "ipsw" | "addon" | "extension" | "sketch" | "idml" | "sl1" | "sl2"
    ) {
        return decode_zip_archive(bytes, format_id, metadata);
    }

    if bytes.starts_with(&[0x1F, 0x8B, 0x08]) || matches!(format_id, "gz" | "tgz" | "tar.gz" | "crate" | "unitypackage" | "prproj") {
        let gz = GzDecoder::new(bytes);
        let mut out = Vec::new();
        if gz.take(32 * 1024 * 1024).read_to_end(&mut out).is_ok() {
            if is_tar_stream(&out) {
                return decode_tar_stream(&out, "TAR.GZ Archive", metadata);
            }
            return Some(ViewerPayload::Archive {
                archive_type: "Gzip Compressed Stream".into(),
                total_entries: 1,
                total_uncompressed: out.len() as u64,
                entries: vec![ArchiveEntry {
                    path: "decompressed_payload".into(),
                    is_dir: false,
                    size: out.len() as u64,
                    compressed_size: bytes.len() as u64,
                    method: "Deflate/Gzip".into(),
                    crc32: None,
                    previewable: true,
                }],
            });
        }
    }

    if bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) || matches!(format_id, "zst" | "tar.zst") {
        if let Ok(zstd) = ZstdDecoder::new(Cursor::new(bytes)) {
            let mut out = Vec::new();
            if zstd.take(32 * 1024 * 1024).read_to_end(&mut out).is_ok() {
                if is_tar_stream(&out) {
                    return decode_tar_stream(&out, "TAR.ZST Archive", metadata);
                }
                return Some(ViewerPayload::Archive {
                    archive_type: "Zstandard Compressed Stream".into(),
                    total_entries: 1,
                    total_uncompressed: out.len() as u64,
                    entries: vec![ArchiveEntry {
                        path: "decompressed_payload".into(),
                        is_dir: false,
                        size: out.len() as u64,
                        compressed_size: bytes.len() as u64,
                        method: "Zstandard".into(),
                        crc32: None,
                        previewable: true,
                    }],
                });
            }
        }
    }

    if format_id == "br" {
        let br = brotli::Decompressor::new(bytes, 4096);
        let mut out = Vec::new();
        if br.take(32 * 1024 * 1024).read_to_end(&mut out).is_ok() {
            return Some(ViewerPayload::Archive {
                archive_type: "Brotli Compressed Stream".into(),
                total_entries: 1,
                total_uncompressed: out.len() as u64,
                entries: vec![ArchiveEntry {
                    path: "decompressed_payload".into(),
                    is_dir: false,
                    size: out.len() as u64,
                    compressed_size: bytes.len() as u64,
                    method: "Brotli".into(),
                    crc32: None,
                    previewable: true,
                }],
            });
        }
    }

    if is_tar_stream(bytes) || matches!(format_id, "tar" | "ova" | "cbt" | "gem") {
        return decode_tar_stream(bytes, "POSIX TAR Archive", metadata);
    }

    if bytes.len() > 0x8006 && &bytes[0x8001..0x8006] == b"CD001" {
        return decode_iso9660(bytes, metadata);
    }

    if bytes.starts_with(b"!<arch>\n") {
        return decode_ar_archive(bytes, format_id, metadata);
    }

    if bytes.starts_with(b"7z\xBC\xAF\x27\x1C") {
        metadata.push(MetadataEntry { section: "Archive".into(), key: "Container".into(), value: "7-Zip (LZMA/LZMA2)".into() });
        return Some(ViewerPayload::Archive {
            archive_type: "7-Zip Archive".into(),
            total_entries: 0,
            total_uncompressed: 0,
            entries: vec![ArchiveEntry {
                path: "(7z solid header verified — inspect hex/strings or extract via external 7z)".into(),
                is_dir: false,
                size: bytes.len() as u64,
                compressed_size: bytes.len() as u64,
                method: "7z".into(),
                crc32: None,
                previewable: false,
            }],
        });
    }

    if bytes.starts_with(b"Rar!\x1A\x07") {
        let ver = if bytes.get(6) == Some(&1) { "RAR5" } else { "RAR4" };
        metadata.push(MetadataEntry { section: "Archive".into(), key: "Version".into(), value: ver.into() });
        return Some(ViewerPayload::Archive {
            archive_type: format!("{ver} Archive"),
            total_entries: 0,
            total_uncompressed: 0,
            entries: vec![ArchiveEntry {
                path: format!("({ver} header verified)"),
                is_dir: false,
                size: bytes.len() as u64,
                compressed_size: bytes.len() as u64,
                method: ver.into(),
                crc32: None,
                previewable: false,
            }],
        });
    }

    None
}

/// Extracts raw bytes of an entry inside ZIP / TAR / TAR.GZ / TAR.ZST / ISO-9660 / AR / GZ / ZST archives.
pub fn extract_archive_entry(bytes: &[u8], entry_path: &str) -> Result<Vec<u8>, String> {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
        let file = arch.by_name(entry_path).map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity((file.size() as usize).min(64 * 1024 * 1024));
        file.take(64 * 1024 * 1024)
            .read_to_end(&mut out)
            .map_err(|e| e.to_string())?;
        return Ok(out);
    }
    if bytes.starts_with(&[0x1F, 0x8B, 0x08]) {
        let gz = GzDecoder::new(bytes);
        let mut out = Vec::new();
        gz.take(64 * 1024 * 1024).read_to_end(&mut out).map_err(|e| e.to_string())?;
        if entry_path == "decompressed_payload" {
            return Ok(out);
        }
        return extract_tar_entry(&out, entry_path);
    }
    if bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        let zstd = ZstdDecoder::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        zstd.take(64 * 1024 * 1024).read_to_end(&mut out).map_err(|e| e.to_string())?;
        if entry_path == "decompressed_payload" {
            return Ok(out);
        }
        return extract_tar_entry(&out, entry_path);
    }
    if is_tar_stream(bytes) {
        return extract_tar_entry(bytes, entry_path);
    }
    if bytes.starts_with(b"!<arch>\n") {
        return extract_ar_entry(bytes, entry_path);
    }
    if bytes.len() > 0x8006 && &bytes[0x8001..0x8006] == b"CD001" {
        return extract_iso_entry(bytes, entry_path);
    }
    Err("Entry extraction not supported for this container".into())
}

fn decode_zip_archive(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let mut arch = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let total_entries = arch.len();
    let mut total_uncompressed = 0u64;
    let mut entries = Vec::with_capacity(total_entries.min(MAX_ARCHIVE_ENTRIES));
    for i in 0..total_entries {
        if let Ok(f) = arch.by_index(i) {
            total_uncompressed = total_uncompressed.saturating_add(f.size());
            if entries.len() < MAX_ARCHIVE_ENTRIES {
                entries.push(ArchiveEntry {
                    path: f.name().ok().map(|n| n.into_owned()).unwrap_or_else(|| format!("entry_{i}")),
                    is_dir: f.is_dir(),
                    size: f.size(),
                    compressed_size: f.compressed_size(),
                    method: format!("{:?}", f.compression()),
                    crc32: Some(f.crc32()),
                    previewable: !f.is_dir() && f.size() <= 32 * 1024 * 1024,
                });
            }
        }
    }
    metadata.push(MetadataEntry { section: "Archive".into(), key: "Entries".into(), value: total_entries.to_string() });
    metadata.push(MetadataEntry { section: "Archive".into(), key: "Uncompressed Size".into(), value: format!("{total_uncompressed} bytes") });
    Some(ViewerPayload::Archive {
        archive_type: format!("{} Container", format_id.to_ascii_uppercase()),
        total_entries,
        total_uncompressed,
        entries,
    })
}

fn is_tar_stream(bytes: &[u8]) -> bool {
    bytes.len() >= 512 && &bytes[257..262] == b"ustar"
}

fn decode_tar_stream(
    bytes: &[u8],
    label: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let mut entries = Vec::new();
    let mut total_uncompressed = 0u64;
    let mut pos = 0usize;
    while pos + 512 <= bytes.len() {
        let hdr = &bytes[pos..pos + 512];
        if hdr.iter().all(|&b| b == 0) {
            break;
        }
        let name = read_tar_str(&hdr[0..100]);
        let prefix = read_tar_str(&hdr[345..500]);
        let full_path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let size = parse_octal(&hdr[124..136]);
        let typeflag = hdr[156];
        let is_dir = typeflag == b'5' || full_path.ends_with('/');
        total_uncompressed = total_uncompressed.saturating_add(size);
        if !full_path.is_empty() && entries.len() < MAX_ARCHIVE_ENTRIES {
            entries.push(ArchiveEntry {
                path: full_path,
                is_dir,
                size,
                compressed_size: size,
                method: "TAR".into(),
                crc32: None,
                previewable: !is_dir && size <= 32 * 1024 * 1024,
            });
        }
        let blocks = size.div_ceil(512) as usize;
        pos = pos.saturating_add(512 + blocks * 512);
    }
    metadata.push(MetadataEntry { section: "TAR".into(), key: "Entries".into(), value: entries.len().to_string() });
    Some(ViewerPayload::Archive {
        archive_type: label.into(),
        total_entries: entries.len(),
        total_uncompressed,
        entries,
    })
}

fn extract_tar_entry(bytes: &[u8], target: &str) -> Result<Vec<u8>, String> {
    let mut pos = 0usize;
    while pos + 512 <= bytes.len() {
        let hdr = &bytes[pos..pos + 512];
        if hdr.iter().all(|&b| b == 0) {
            break;
        }
        let name = read_tar_str(&hdr[0..100]);
        let prefix = read_tar_str(&hdr[345..500]);
        let full_path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let size = parse_octal(&hdr[124..136]) as usize;
        pos += 512;
        if full_path == target && pos + size <= bytes.len() {
            return Ok(bytes[pos..pos + size].to_vec());
        }
        let blocks = size.div_ceil(512);
        pos = pos.saturating_add(blocks * 512);
    }
    Err(format!("TAR entry '{target}' not found"))
}

fn read_tar_str(slice: &[u8]) -> String {
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..end]).to_string()
}

fn parse_octal(slice: &[u8]) -> u64 {
    let s = read_tar_str(slice);
    u64::from_str_radix(s.trim(), 8).unwrap_or(0)
}

fn decode_ar_archive(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    let mut pos = 8usize;
    let mut entries = Vec::new();
    let mut total_uncompressed = 0u64;
    while pos + 60 <= bytes.len() {
        let hdr = &bytes[pos..pos + 60];
        let name = String::from_utf8_lossy(&hdr[0..16]).trim().trim_end_matches('/').to_string();
        let size: u64 = String::from_utf8_lossy(&hdr[48..58]).trim().parse().unwrap_or(0);
        pos += 60;
        total_uncompressed += size;
        entries.push(ArchiveEntry {
            path: name,
            is_dir: false,
            size,
            compressed_size: size,
            method: "AR".into(),
            crc32: None,
            previewable: size <= 32 * 1024 * 1024,
        });
        pos += (size as usize) + ((size as usize) & 1);
    }
    metadata.push(MetadataEntry { section: "AR Archive".into(), key: "Members".into(), value: entries.len().to_string() });
    Some(ViewerPayload::Archive {
        archive_type: if format_id == "deb" { "Debian Package (.deb)" } else { "Unix AR Archive" }.into(),
        total_entries: entries.len(),
        total_uncompressed,
        entries,
    })
}

fn extract_ar_entry(bytes: &[u8], target: &str) -> Result<Vec<u8>, String> {
    let mut pos = 8usize;
    while pos + 60 <= bytes.len() {
        let hdr = &bytes[pos..pos + 60];
        let name = String::from_utf8_lossy(&hdr[0..16]).trim().trim_end_matches('/').to_string();
        let size: usize = String::from_utf8_lossy(&hdr[48..58]).trim().parse().unwrap_or(0);
        pos += 60;
        if name == target && pos + size <= bytes.len() {
            return Ok(bytes[pos..pos + size].to_vec());
        }
        pos += size + (size & 1);
    }
    Err(format!("AR entry '{target}' not found"))
}

fn decode_iso9660(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let pvd = bytes.get(0x8000..0x8800)?;
    if &pvd[1..6] != b"CD001" {
        return None;
    }
    let vol_id = String::from_utf8_lossy(&pvd[40..72]).trim().to_string();
    let block_size = u16::from_le_bytes([pvd[128], pvd[129]]).max(2048) as usize;
    let root_lba = u32::from_le_bytes([pvd[158], pvd[159], pvd[160], pvd[161]]) as usize;
    let root_len = u32::from_le_bytes([pvd[166], pvd[167], pvd[168], pvd[169]]) as usize;

    metadata.push(MetadataEntry { section: "ISO-9660".into(), key: "Volume ID".into(), value: vol_id.clone() });
    metadata.push(MetadataEntry { section: "ISO-9660".into(), key: "Block Size".into(), value: format!("{block_size} bytes") });

    let mut entries = Vec::new();
    let mut total_uncompressed = 0u64;
    walk_iso_dir(bytes, block_size, root_lba, root_len, "", 0, &mut entries, &mut total_uncompressed);

    Some(ViewerPayload::Archive {
        archive_type: format!("ISO-9660 Disc Image ({vol_id})"),
        total_entries: entries.len(),
        total_uncompressed,
        entries,
    })
}

#[allow(clippy::too_many_arguments)]
fn walk_iso_dir(
    bytes: &[u8],
    block_size: usize,
    lba: usize,
    len: usize,
    prefix: &str,
    depth: usize,
    out: &mut Vec<ArchiveEntry>,
    total_size: &mut u64,
) {
    if depth > 6 || out.len() >= MAX_ARCHIVE_ENTRIES {
        return;
    }
    let start = lba.saturating_mul(block_size);
    let end = start.saturating_add(len).min(bytes.len());
    let mut pos = start;
    while pos < end && out.len() < MAX_ARCHIVE_ENTRIES {
        let rec_len = bytes[pos] as usize;
        if rec_len == 0 {
            // Advance to next sector boundary
            let next_sector = ((pos / block_size) + 1) * block_size;
            if next_sector <= pos { break; }
            pos = next_sector;
            continue;
        }
        if pos + rec_len > end || rec_len < 34 {
            break;
        }
        let rec = &bytes[pos..pos + rec_len];
        let ext_lba = u32::from_le_bytes([rec[2], rec[3], rec[4], rec[5]]) as usize;
        let data_len = u32::from_le_bytes([rec[10], rec[11], rec[12], rec[13]]) as usize;
        let flags = rec[25];
        let name_len = rec[32] as usize;
        if 33 + name_len <= rec.len() {
            let raw_name = &rec[33..33 + name_len];
            if raw_name != [0] && raw_name != [1] {
                let mut name = String::from_utf8_lossy(raw_name).to_string();
                if let Some(semi) = name.find(';') {
                    name.truncate(semi);
                }
                let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
                let is_dir = (flags & 0x02) != 0;
                *total_size = total_size.saturating_add(data_len as u64);
                out.push(ArchiveEntry {
                    path: path.clone(),
                    is_dir,
                    size: data_len as u64,
                    compressed_size: data_len as u64,
                    method: "ISO-9660".into(),
                    crc32: None,
                    previewable: !is_dir && data_len <= 32 * 1024 * 1024,
                });
                if is_dir {
                    walk_iso_dir(bytes, block_size, ext_lba, data_len, &path, depth + 1, out, total_size);
                }
            }
        }
        pos += rec_len;
    }
}

fn extract_iso_entry(bytes: &[u8], target: &str) -> Result<Vec<u8>, String> {
    let pvd = bytes.get(0x8000..0x8800).ok_or("Missing PVD")?;
    let block_size = u16::from_le_bytes([pvd[128], pvd[129]]).max(2048) as usize;
    let root_lba = u32::from_le_bytes([pvd[158], pvd[159], pvd[160], pvd[161]]) as usize;
    let root_len = u32::from_le_bytes([pvd[166], pvd[167], pvd[168], pvd[169]]) as usize;

    fn search(
        bytes: &[u8],
        bs: usize,
        lba: usize,
        len: usize,
        prefix: &str,
        target: &str,
        depth: usize,
    ) -> Option<Vec<u8>> {
        if depth > 6 { return None; }
        let start = lba.saturating_mul(bs);
        let end = start.saturating_add(len).min(bytes.len());
        let mut pos = start;
        while pos < end {
            let rec_len = bytes[pos] as usize;
            if rec_len == 0 {
                pos = ((pos / bs) + 1) * bs;
                continue;
            }
            if pos + rec_len > end || rec_len < 34 { break; }
            let rec = &bytes[pos..pos + rec_len];
            let ext_lba = u32::from_le_bytes([rec[2], rec[3], rec[4], rec[5]]) as usize;
            let data_len = u32::from_le_bytes([rec[10], rec[11], rec[12], rec[13]]) as usize;
            let flags = rec[25];
            let name_len = rec[32] as usize;
            if 33 + name_len <= rec.len() {
                let raw_name = &rec[33..33 + name_len];
                if raw_name != [0] && raw_name != [1] {
                    let mut name = String::from_utf8_lossy(raw_name).to_string();
                    if let Some(semi) = name.find(';') { name.truncate(semi); }
                    let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
                    if (flags & 0x02) != 0 {
                        if let Some(found) = search(bytes, bs, ext_lba, data_len, &path, target, depth + 1) {
                            return Some(found);
                        }
                    } else if path == target {
                        let off = ext_lba.saturating_mul(bs);
                        let end_off = off.saturating_add(data_len).min(bytes.len());
                        return Some(bytes[off..end_off].to_vec());
                    }
                }
            }
            pos += rec_len;
        }
        None
    }

    search(bytes, block_size, root_lba, root_len, "", target, 0)
        .ok_or_else(|| format!("ISO entry '{target}' not found"))
}
