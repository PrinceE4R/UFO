use std::collections::BTreeMap;

use crate::model::{
    BinarySection, DetectedFormat, ExtractedString, HexPage, HexRow, MetadataEntry, ViewerPayload,
};

/// Builds fixed-width 16-byte hex + ASCII rows starting at `base_offset`.
pub fn build_hex_rows(bytes: &[u8], base_offset: u64, max_bytes: usize) -> Vec<HexRow> {
    let slice = &bytes[..bytes.len().min(max_bytes)];
    slice
        .chunks(16)
        .enumerate()
        .map(|(idx, chunk)| {
            let ascii = chunk
                .iter()
                .map(|&b| if (0x20..=0x7E).contains(&b) { b as char } else { '.' })
                .collect();
            HexRow {
                offset: base_offset + (idx * 16) as u64,
                bytes: chunk.to_vec(),
                ascii,
            }
        })
        .collect()
}

pub fn slice_hex_page(bytes: &[u8], offset: u64, length: usize) -> HexPage {
    let start = (offset as usize).min(bytes.len());
    let len = length.clamp(256, 32 * 1024);
    let end = (start + len).min(bytes.len());
    HexPage {
        offset: start as u64,
        total_bytes: bytes.len() as u64,
        rows: build_hex_rows(&bytes[start..end], start as u64, len),
    }
}

/// Scans printable ASCII and UTF-16LE strings inside binary buffers.
pub fn extract_strings(bytes: &[u8], max_strings: usize) -> Vec<ExtractedString> {
    let scan_len = bytes.len().min(512 * 1024);
    let data = &bytes[..scan_len];
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < data.len() && out.len() < max_strings {
        if (0x20..=0x7E).contains(&data[i]) {
            let start = i;
            while i < data.len() && (0x20..=0x7E).contains(&data[i]) && i - start < 240 {
                i += 1;
            }
            if i - start >= 5 {
                let s = String::from_utf8_lossy(&data[start..i]).to_string();
                out.push(ExtractedString {
                    offset: start as u64,
                    encoding: "ASCII".into(),
                    value: s,
                });
                continue;
            }
        }
        // Check simple UTF-16LE ASCII sequence
        if i + 7 < data.len() && (0x20..=0x7E).contains(&data[i]) && data[i + 1] == 0 {
            let start = i;
            let mut chars = Vec::new();
            while i + 1 < data.len()
                && (0x20..=0x7E).contains(&data[i])
                && data[i + 1] == 0
                && chars.len() < 160
            {
                chars.push(data[i] as char);
                i += 2;
            }
            if chars.len() >= 5 {
                out.push(ExtractedString {
                    offset: start as u64,
                    encoding: "UTF-16LE".into(),
                    value: chars.into_iter().collect(),
                });
                continue;
            }
        }
        i += 1;
    }
    out
}

pub fn decode_binary(
    bytes: &[u8],
    detected: &DetectedFormat,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let strings = extract_strings(bytes, 250);
    match detected.id.as_str() {
        "exe" | "dll" | "sys" | "efi" | "com" => parse_pe(bytes, strings, metadata),
        "elf" | "so" | "o" | "ko" | "out" => parse_elf(bytes, strings, metadata),
        "macho" | "dylib" => parse_macho(bytes, strings, metadata),
        "wasm" => parse_wasm(bytes, strings, metadata),
        "dex" => parse_dex(bytes, strings, metadata),
        "pcap" | "pcapng" => parse_pcap(bytes, strings, metadata),
        "torrent" => parse_torrent(bytes, strings, metadata),
        "lnk" => parse_lnk(bytes, strings, metadata),
        "uf2" => parse_uf2(bytes, strings, metadata),
        "der" | "crt" | "cer" | "cert" => parse_der_asn1(bytes, strings, metadata),
        _ => ViewerPayload::HexOnly {
            note: format!(
                "{} — showing hexadecimal view, extracted strings, and entropy telemetry.",
                detected.name
            ),
            strings,
        },
    }
}

fn le_u16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn le_u32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn le_u64(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8)
        .map(|s| u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}
fn be_u16(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_be_bytes([s[0], s[1]]))
}
fn be_u32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}
fn be_u64(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8)
        .map(|s| u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

fn parse_pe(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let Some(lfanew) = le_u32(bytes, 0x3C).map(|v| v as usize) else {
        return ViewerPayload::HexOnly {
            note: "DOS / MZ Executable".into(),
            strings,
        };
    };
    if bytes.get(lfanew..lfanew + 4) != Some(b"PE\0\0") {
        return ViewerPayload::HexOnly {
            note: "Legacy MS-DOS MZ Executable (no PE header)".into(),
            strings,
        };
    }

    let coff = lfanew + 4;
    let machine = le_u16(bytes, coff).unwrap_or(0);
    let num_sections = le_u16(bytes, coff + 2).unwrap_or(0) as usize;
    let opt_size = le_u16(bytes, coff + 16).unwrap_or(0) as usize;
    let chars = le_u16(bytes, coff + 18).unwrap_or(0);

    let arch = match machine {
        0x8664 => "x86-64 (AMD64)",
        0x014C => "x86 (i386)",
        0xAA64 => "ARM64 (AArch64)",
        0x01C4 => "ARMv7 Thumb-2",
        _ => "Unknown PE Architecture",
    };

    let opt = coff + 20;
    let magic = le_u16(bytes, opt).unwrap_or(0);
    let is_pe32_plus = magic == 0x20B;
    let entry_rva = le_u32(bytes, opt + 16).unwrap_or(0);
    let subsystem = if is_pe32_plus {
        le_u16(bytes, opt + 68).unwrap_or(0)
    } else {
        le_u16(bytes, opt + 68).unwrap_or(0)
    };
    let sub_name = match subsystem {
        1 => "Native Driver",
        2 => "Windows GUI",
        3 => "Windows Console",
        10 => "EFI Application",
        _ => "Other Subsystem",
    };

    metadata.push(MetadataEntry { section: "PE Header".into(), key: "Architecture".into(), value: arch.into() });
    metadata.push(MetadataEntry { section: "PE Header".into(), key: "Format".into(), value: if is_pe32_plus { "PE32+ (64-bit)" } else { "PE32 (32-bit)" }.into() });
    metadata.push(MetadataEntry { section: "PE Header".into(), key: "Subsystem".into(), value: sub_name.into() });
    metadata.push(MetadataEntry { section: "PE Header".into(), key: "Characteristics".into(), value: format!("0x{chars:04X}{}", if chars & 0x2000 != 0 { " (DLL)" } else { "" }) });

    let sec_start = opt + opt_size;
    let mut sections = Vec::new();
    for i in 0..num_sections.min(64) {
        let s = sec_start + i * 40;
        let Some(hdr) = bytes.get(s..s + 40) else { break };
        let name = String::from_utf8_lossy(&hdr[0..8])
            .trim_end_matches('\0')
            .to_string();
        let vsize = le_u32(hdr, 8).unwrap_or(0);
        let vaddr = le_u32(hdr, 12).unwrap_or(0);
        let raw_size = le_u32(hdr, 16).unwrap_or(0);
        let raw_ptr = le_u32(hdr, 20).unwrap_or(0);
        let sflags = le_u32(hdr, 36).unwrap_or(0);
        let mut perm = String::new();
        if sflags & 0x4000_0000 != 0 { perm.push('R'); }
        if sflags & 0x8000_0000 != 0 { perm.push('W'); }
        if sflags & 0x2000_0000 != 0 { perm.push('X'); }
        sections.push(BinarySection {
            name,
            virtual_addr: format!("0x{vaddr:08X}"),
            raw_offset: format!("0x{raw_ptr:08X} ({raw_size} B)"),
            size: vsize.max(raw_size) as u64,
            flags: perm,
        });
    }

    let dll_imports: Vec<String> = strings
        .iter()
        .filter(|s| {
            let lower = s.value.to_ascii_lowercase();
            lower.ends_with(".dll") && s.value.len() < 48 && !s.value.contains(' ')
        })
        .map(|s| s.value.clone())
        .collect();

    ViewerPayload::Binary {
        arch_or_format: format!("{} · {}", if is_pe32_plus { "PE32+" } else { "PE32" }, arch),
        endianness: "Little-Endian".into(),
        entry_point: Some(format!("0x{entry_rva:08X}")),
        sections,
        imports: dll_imports,
        exports: Vec::new(),
        strings,
    }
}

fn parse_elf(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    if bytes.len() < 52 {
        return ViewerPayload::HexOnly { note: "Truncated ELF binary".into(), strings };
    }
    let is_64 = bytes[4] == 2;
    let is_le = bytes[5] == 1;
    let r16 = |o| if is_le { le_u16(bytes, o) } else { be_u16(bytes, o) };
    let r32 = |o| if is_le { le_u32(bytes, o) } else { be_u32(bytes, o) };
    let r64 = |o| if is_le { le_u64(bytes, o) } else { be_u64(bytes, o) };

    let e_type = r16(16).unwrap_or(0);
    let e_machine = r16(18).unwrap_or(0);
    let entry = if is_64 { r64(24).unwrap_or(0) } else { r32(24).unwrap_or(0) as u64 };
    let shoff = if is_64 { r64(40).unwrap_or(0) as usize } else { r32(32).unwrap_or(0) as usize };
    let shentsize = r16(if is_64 { 58 } else { 46 }).unwrap_or(0) as usize;
    let shnum = r16(if is_64 { 60 } else { 48 }).unwrap_or(0) as usize;
    let shstrndx = r16(if is_64 { 62 } else { 50 }).unwrap_or(0) as usize;

    let kind_name = match e_type {
        1 => "ET_REL (Relocatable)",
        2 => "ET_EXEC (Executable)",
        3 => "ET_DYN (Shared / PIE)",
        4 => "ET_CORE (Core Dump)",
        _ => "ELF Object",
    };
    let arch = match e_machine {
        0x3E => "x86-64",
        0x03 => "x86 (32-bit)",
        0xB7 => "AArch64 (ARM64)",
        0x28 => "ARM",
        0xF3 => "RISC-V",
        _ => "Other ISA",
    };
    metadata.push(MetadataEntry { section: "ELF Header".into(), key: "Type".into(), value: kind_name.into() });
    metadata.push(MetadataEntry { section: "ELF Header".into(), key: "Machine".into(), value: arch.into() });

    let mut sections = Vec::new();
    let strtab_offset = if shoff > 0 && shstrndx < shnum && shentsize >= 40 {
        let idx = shoff + shstrndx * shentsize;
        if is_64 {
            r64(idx + 24).map(|v| v as usize).unwrap_or(0)
        } else {
            r32(idx + 16).map(|v| v as usize).unwrap_or(0)
        }
    } else {
        0
    };

    if shoff > 0 && shentsize >= 40 {
        for i in 0..shnum.min(96) {
            let off = shoff + i * shentsize;
            if off + shentsize > bytes.len() { break; }
            let name_idx = r32(off).unwrap_or(0) as usize;
            let name = if strtab_offset > 0 && strtab_offset + name_idx < bytes.len() {
                let tail = &bytes[strtab_offset + name_idx..];
                let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len().min(32));
                String::from_utf8_lossy(&tail[..end]).to_string()
            } else {
                format!("sec_{i}")
            };
            let (flags, addr, foff, sz) = if is_64 {
                (r64(off + 8).unwrap_or(0), r64(off + 16).unwrap_or(0), r64(off + 24).unwrap_or(0), r64(off + 32).unwrap_or(0))
            } else {
                (r32(off + 8).unwrap_or(0) as u64, r32(off + 12).unwrap_or(0) as u64, r32(off + 16).unwrap_or(0) as u64, r32(off + 20).unwrap_or(0) as u64)
            };
            let mut perm = String::new();
            if flags & 0x2 != 0 { perm.push('A'); }
            if flags & 0x1 != 0 { perm.push('W'); }
            if flags & 0x4 != 0 { perm.push('X'); }
            if !name.is_empty() || sz > 0 {
                sections.push(BinarySection {
                    name: if name.is_empty() { "(null)".into() } else { name },
                    virtual_addr: format!("0x{addr:X}"),
                    raw_offset: format!("0x{foff:X}"),
                    size: sz,
                    flags: perm,
                });
            }
        }
    }

    ViewerPayload::Binary {
        arch_or_format: format!("{} · {} · {}", if is_64 { "ELF64" } else { "ELF32" }, arch, kind_name),
        endianness: if is_le { "Little-Endian" } else { "Big-Endian" }.into(),
        entry_point: Some(format!("0x{entry:X}")),
        sections,
        imports: Vec::new(),
        exports: Vec::new(),
        strings,
    }
}

fn parse_macho(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let magic = be_u32(bytes, 0).unwrap_or(0);
    if magic == 0xCAFE_BABE {
        let nfat = be_u32(bytes, 4).unwrap_or(0);
        let mut slices = Vec::new();
        for i in 0..nfat.min(16) as usize {
            let off = 8 + i * 20;
            if let (Some(cpu), Some(offset), Some(size)) = (be_u32(bytes, off), be_u32(bytes, off + 8), be_u32(bytes, off + 12)) {
                let cpu_name = match cpu {
                    0x0100_000C => "ARM64",
                    0x0100_0007 => "x86_64",
                    _ => "Mach-O Slice",
                };
                slices.push(BinarySection {
                    name: cpu_name.into(),
                    virtual_addr: "-".into(),
                    raw_offset: format!("0x{offset:X}"),
                    size: size as u64,
                    flags: "FAT".into(),
                });
            }
        }
        metadata.push(MetadataEntry { section: "Mach-O".into(), key: "Universal Slices".into(), value: nfat.to_string() });
        return ViewerPayload::Binary {
            arch_or_format: format!("Apple Universal Mach-O ({nfat} architectures)"),
            endianness: "Big/Mixed".into(),
            entry_point: None,
            sections: slices,
            imports: Vec::new(),
            exports: Vec::new(),
            strings,
        };
    }

    let is_le = matches!(magic, 0xCEFA_EDFE | 0xCFFA_EDFE);
    let is_64 = matches!(magic, 0xFEED_FACF | 0xCFFA_EDFE);
    let cpu = if is_le { le_u32(bytes, 4) } else { be_u32(bytes, 4) }.unwrap_or(0);
    let arch = match cpu {
        0x0100_000C => "ARM64 (Apple Silicon)",
        0x0100_0007 => "x86-64",
        _ => "Mach-O",
    };
    ViewerPayload::Binary {
        arch_or_format: format!("Apple Mach-O {} · {arch}", if is_64 { "64-bit" } else { "32-bit" }),
        endianness: if is_le { "Little-Endian" } else { "Big-Endian" }.into(),
        entry_point: None,
        sections: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        strings,
    }
}

fn parse_wasm(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let version = le_u32(bytes, 4).unwrap_or(1);
    metadata.push(MetadataEntry { section: "WASM".into(), key: "Binary Version".into(), value: version.to_string() });

    let mut sections = Vec::new();
    let mut exports = Vec::new();
    let mut pos = 8usize;
    while pos < bytes.len() {
        let sec_id = bytes[pos];
        pos += 1;
        let (sec_len, next) = read_leb128(bytes, pos);
        pos = next;
        let sec_len_u = sec_len as usize;
        let sec_name = match sec_id {
            0 => "Custom",
            1 => "Type",
            2 => "Import",
            3 => "Function",
            4 => "Table",
            5 => "Memory",
            6 => "Global",
            7 => "Export",
            8 => "Start",
            9 => "Element",
            10 => "Code",
            11 => "Data",
            12 => "DataCount",
            _ => "Unknown",
        };
        sections.push(BinarySection {
            name: format!("{sec_name} (id {sec_id})"),
            virtual_addr: format!("section #{sec_id}"),
            raw_offset: format!("0x{pos:X}"),
            size: sec_len,
            flags: "WASM".into(),
        });

        // Parse export names if Export section (id 7)
        if sec_id == 7 && pos + sec_len_u <= bytes.len() {
            let mut p = pos;
            let (cnt, p2) = read_leb128(bytes, p);
            p = p2;
            for _ in 0..cnt.min(128) {
                let (slen, p3) = read_leb128(bytes, p);
                p = p3;
                let sl = slen as usize;
                if p + sl + 1 > pos + sec_len_u { break; }
                let ename = String::from_utf8_lossy(&bytes[p..p + sl]).to_string();
                p += sl;
                let kind = match bytes.get(p).copied().unwrap_or(0) {
                    0 => "fn",
                    1 => "table",
                    2 => "mem",
                    3 => "global",
                    _ => "sym",
                };
                p += 1;
                let (_, p4) = read_leb128(bytes, p);
                p = p4;
                exports.push(format!("{kind} {ename}"));
            }
        }
        pos = pos.saturating_add(sec_len_u);
    }

    ViewerPayload::Binary {
        arch_or_format: format!("WebAssembly Module v{version}"),
        endianness: "Little-Endian (LEB128)".into(),
        entry_point: None,
        sections,
        imports: Vec::new(),
        exports,
        strings,
    }
}

fn read_leb128(bytes: &[u8], mut pos: usize) -> (u64, usize) {
    let mut res = 0u64;
    let mut shift = 0u32;
    while pos < bytes.len() && shift < 63 {
        let b = bytes[pos];
        pos += 1;
        res |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (res, pos)
}

fn parse_dex(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let ver = String::from_utf8_lossy(bytes.get(4..7).unwrap_or(b"035")).to_string();
    let str_ids = le_u32(bytes, 56).unwrap_or(0);
    let type_ids = le_u32(bytes, 64).unwrap_or(0);
    let method_ids = le_u32(bytes, 88).unwrap_or(0);
    let class_defs = le_u32(bytes, 96).unwrap_or(0);
    metadata.push(MetadataEntry { section: "Android DEX".into(), key: "DEX Version".into(), value: ver.clone() });
    metadata.push(MetadataEntry { section: "Android DEX".into(), key: "Classes".into(), value: class_defs.to_string() });
    metadata.push(MetadataEntry { section: "Android DEX".into(), key: "Methods".into(), value: method_ids.to_string() });

    let sections = vec![
        BinarySection { name: "string_ids".into(), virtual_addr: "-".into(), raw_offset: "0x70".into(), size: str_ids as u64 * 4, flags: format!("{str_ids} strings") },
        BinarySection { name: "type_ids".into(), virtual_addr: "-".into(), raw_offset: "-".into(), size: type_ids as u64 * 4, flags: format!("{type_ids} types") },
        BinarySection { name: "method_ids".into(), virtual_addr: "-".into(), raw_offset: "-".into(), size: method_ids as u64 * 8, flags: format!("{method_ids} methods") },
        BinarySection { name: "class_defs".into(), virtual_addr: "-".into(), raw_offset: "-".into(), size: class_defs as u64 * 32, flags: format!("{class_defs} classes") },
    ];
    ViewerPayload::Binary {
        arch_or_format: format!("Android Dalvik DEX v{ver}"),
        endianness: "Little-Endian".into(),
        entry_point: None,
        sections,
        imports: Vec::new(),
        exports: Vec::new(),
        strings,
    }
}

fn parse_pcap(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    if bytes.starts_with(&[0x0A, 0x0D, 0x0D, 0x0A]) {
        metadata.push(MetadataEntry { section: "Capture".into(), key: "Format".into(), value: "PCAPNG".into() });
        return ViewerPayload::Binary {
            arch_or_format: "PCAP Next-Gen Network Capture".into(),
            endianness: "Little-Endian".into(),
            entry_point: None,
            sections: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
            strings,
        };
    }
    let is_le = bytes.starts_with(&[0xD4, 0xC3, 0xB2, 0xA1]) || bytes.starts_with(&[0x4D, 0x3C, 0xB2, 0xA1]);
    let snaplen = if is_le { le_u32(bytes, 16) } else { be_u32(bytes, 16) }.unwrap_or(0);
    let link = if is_le { le_u32(bytes, 20) } else { be_u32(bytes, 20) }.unwrap_or(0);
    let mut pos = 24usize;
    let mut packets = 0usize;
    let mut proto_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    while pos + 16 <= bytes.len() && packets < 50_000 {
        let incl_len = if is_le { le_u32(bytes, pos + 8) } else { be_u32(bytes, pos + 8) }.unwrap_or(0) as usize;
        pos += 16;
        if pos + incl_len > bytes.len() { break; }
        let pkt = &bytes[pos..pos + incl_len];
        if link == 1 && pkt.len() >= 14 {
            let et = be_u16(pkt, 12).unwrap_or(0);
            match et {
                0x0800 => {
                    *proto_counts.entry("IPv4").or_default() += 1;
                    if pkt.len() >= 24 {
                        match pkt[23] {
                            6 => *proto_counts.entry("TCP").or_default() += 1,
                            17 => *proto_counts.entry("UDP").or_default() += 1,
                            1 => *proto_counts.entry("ICMP").or_default() += 1,
                            _ => {}
                        }
                    }
                }
                0x86DD => *proto_counts.entry("IPv6").or_default() += 1,
                0x0806 => *proto_counts.entry("ARP").or_default() += 1,
                _ => *proto_counts.entry("Other Eth").or_default() += 1,
            }
        }
        packets += 1;
        pos += incl_len;
    }
    metadata.push(MetadataEntry { section: "PCAP".into(), key: "Packets".into(), value: packets.to_string() });
    metadata.push(MetadataEntry { section: "PCAP".into(), key: "SnapLen".into(), value: snaplen.to_string() });
    let sections = proto_counts
        .into_iter()
        .map(|(k, v)| BinarySection {
            name: k.into(),
            virtual_addr: format!("{v} packets"),
            raw_offset: "-".into(),
            size: v as u64,
            flags: "PROTO".into(),
        })
        .collect();
    ViewerPayload::Binary {
        arch_or_format: format!("libpcap Network Capture ({packets} packets)"),
        endianness: if is_le { "Little-Endian" } else { "Big-Endian" }.into(),
        entry_point: None,
        sections,
        imports: Vec::new(),
        exports: Vec::new(),
        strings,
    }
}

fn parse_torrent(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(65536)]);
    if let Some(pos) = text.find("8:announce") {
        let rest = &text[pos + 10..];
        if let Some(colon) = rest.find(':') {
            if let Ok(len) = rest[..colon].parse::<usize>() {
                let url = rest.get(colon + 1..colon + 1 + len).unwrap_or("");
                metadata.push(MetadataEntry { section: "BitTorrent".into(), key: "Announce".into(), value: url.into() });
            }
        }
    }
    ViewerPayload::Binary {
        arch_or_format: "BitTorrent Bencode Metadata".into(),
        endianness: "Bencode ASCII/Binary".into(),
        entry_point: None,
        sections: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        strings,
    }
}

fn parse_lnk(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let flags = le_u32(bytes, 20).unwrap_or(0);
    let file_size = le_u32(bytes, 28).unwrap_or(0);
    metadata.push(MetadataEntry { section: "ShellLink".into(), key: "LinkFlags".into(), value: format!("0x{flags:08X}") });
    metadata.push(MetadataEntry { section: "ShellLink".into(), key: "Target Size".into(), value: format!("{file_size} bytes") });
    ViewerPayload::Binary {
        arch_or_format: "Windows Shell Link (.lnk)".into(),
        endianness: "Little-Endian".into(),
        entry_point: None,
        sections: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        strings,
    }
}

fn parse_uf2(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    let blocks = bytes.len() / 512;
    let first_addr = le_u32(bytes, 12).unwrap_or(0);
    let total_blocks = le_u32(bytes, 24).unwrap_or(blocks as u32);
    let family_id = le_u32(bytes, 28).unwrap_or(0);
    metadata.push(MetadataEntry { section: "UF2 Firmware".into(), key: "Blocks".into(), value: total_blocks.to_string() });
    metadata.push(MetadataEntry { section: "UF2 Firmware".into(), key: "Base Address".into(), value: format!("0x{first_addr:08X}") });
    metadata.push(MetadataEntry { section: "UF2 Firmware".into(), key: "Board Family ID".into(), value: format!("0x{family_id:08X}") });
    ViewerPayload::Binary {
        arch_or_format: format!("UF2 Embedded Firmware ({total_blocks} blocks)"),
        endianness: "Little-Endian".into(),
        entry_point: Some(format!("0x{first_addr:08X}")),
        sections: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        strings,
    }
}

fn parse_der_asn1(
    bytes: &[u8],
    strings: Vec<ExtractedString>,
    metadata: &mut Vec<MetadataEntry>,
) -> ViewerPayload {
    metadata.push(MetadataEntry { section: "ASN.1 DER".into(), key: "Root Tag".into(), value: format!("0x{:02X}", bytes.first().copied().unwrap_or(0)) });
    ViewerPayload::Binary {
        arch_or_format: "ASN.1 DER X.509 Certificate / Key".into(),
        endianness: "Big-Endian TLV".into(),
        entry_point: None,
        sections: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        strings,
    }
}
