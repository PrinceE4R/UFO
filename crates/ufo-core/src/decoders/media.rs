use std::fmt::Write as _;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

use crate::decoders::text::decode_to_utf8;
use crate::model::{MediaKind, MetadataEntry, SubtitleCue, ViewerPayload};

/// Decodes Audio, Video container boxes, MIDI sequences, and Subtitle streams.
pub fn decode_media(
    bytes: &[u8],
    format_id: &str,
    metadata: &mut Vec<MetadataEntry>,
) -> Option<ViewerPayload> {
    match format_id {
        "wav" | "wave" => decode_wav(bytes, metadata),
        "mp3" => decode_mp3(bytes, metadata),
        "flac" => decode_flac(bytes, metadata),
        "ogg" | "oga" | "opus" | "spx" | "ogv" => decode_ogg(bytes, format_id, metadata),
        "mp4" | "m4a" | "m4v" | "mov" | "f4v" | "alac" | "3gp" | "3g2" => {
            decode_mp4_mov(bytes, format_id, metadata)
        }
        "mkv" | "webm" | "mka" => decode_ebml(bytes, format_id, metadata),
        "avi" => decode_avi(bytes, metadata),
        "mid" | "midi" | "kar" => decode_midi(bytes, metadata),
        "srt" | "vtt" | "ass" | "ssa" | "lrc" => decode_subtitles(bytes, format_id, metadata),
        "aac" | "aiff" | "aif" | "au" | "snd" | "voc" | "wma" | "wv" | "ape" | "ac3" | "dts"
        | "caf" | "dsf" | "dff" | "mpegts" | "mpg" | "mpeg" | "flv" => {
            let is_video = matches!(format_id, "mpegts" | "mpg" | "mpeg" | "flv");
            Some(ViewerPayload::Media {
                media_kind: if is_video { MediaKind::Video } else { MediaKind::Audio },
                container: format_id.to_ascii_uppercase(),
                codec_summary: format!("{} Media Stream", format_id.to_ascii_uppercase()),
                duration_secs: None,
                sample_rate: None,
                channels: None,
                dimensions: None,
                use_native_player: matches!(format_id, "aac" | "mpg" | "mpeg" | "flv"),
                waveform_peaks: Vec::new(),
                svg_preview: None,
                subtitles: Vec::new(),
                cover_art_data_url: None,
            })
        }
        _ => None,
    }
}

fn decode_wav(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let (mut channels, mut sample_rate, mut bps, mut fmt_code) = (2u16, 44100u32, 16u16, 1u16);
    let mut data_slice: &[u8] = &[];
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let sz = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        pos += 8;
        let end = pos.saturating_add(sz).min(bytes.len());
        if id == b"fmt " && end - pos >= 16 {
            fmt_code = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]);
            channels = u16::from_le_bytes([bytes[pos + 2], bytes[pos + 3]]);
            sample_rate = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]);
            bps = u16::from_le_bytes([bytes[pos + 14], bytes[pos + 15]]);
        } else if id == b"data" {
            data_slice = &bytes[pos..end];
        }
        pos = end + (sz & 1);
    }

    let bytes_per_sample = (bps as usize / 8).max(1) * (channels as usize).max(1);
    let total_frames = data_slice.len() / bytes_per_sample;
    let duration = if sample_rate > 0 { Some(total_frames as f64 / sample_rate as f64) } else { None };
    let peaks = compute_pcm_peaks(data_slice, bps, channels);

    metadata.push(MetadataEntry { section: "WAV Audio".into(), key: "Sample Rate".into(), value: format!("{sample_rate} Hz") });
    metadata.push(MetadataEntry { section: "WAV Audio".into(), key: "Channels / Bit Depth".into(), value: format!("{channels} ch / {bps}-bit (fmt {fmt_code})") });

    Some(ViewerPayload::Media {
        media_kind: MediaKind::Audio,
        container: "RIFF WAVE".into(),
        codec_summary: format!("{} · {sample_rate} Hz · {channels} ch · {bps}-bit", if fmt_code == 3 { "IEEE Float" } else { "Linear PCM" }),
        duration_secs: duration,
        sample_rate: Some(sample_rate),
        channels: Some(channels),
        dimensions: None,
        use_native_player: true,
        waveform_peaks: peaks,
        svg_preview: None,
        subtitles: Vec::new(),
        cover_art_data_url: None,
    })
}

fn compute_pcm_peaks(data: &[u8], bps: u16, channels: u16) -> Vec<f32> {
    if data.is_empty() || bps != 16 {
        return Vec::new();
    }
    let bins = 128usize;
    let frame_size = 2 * (channels as usize).max(1);
    let total_frames = data.len() / frame_size;
    if total_frames == 0 {
        return Vec::new();
    }
    let step = (total_frames / bins).max(1);
    let mut peaks = Vec::with_capacity(bins);
    for b in 0..bins {
        let start_f = b * step;
        if start_f >= total_frames { break; }
        let end_f = ((b + 1) * step).min(total_frames);
        let stride = ((end_f - start_f) / 64).max(1);
        let mut max_amp = 0.0f32;
        let mut f = start_f;
        while f < end_f {
            let off = f * frame_size;
            if off + 1 < data.len() {
                let s = i16::from_le_bytes([data[off], data[off + 1]]) as f32 / 32768.0;
                max_amp = max_amp.max(s.abs());
            }
            f += stride;
        }
        peaks.push(max_amp.min(1.0));
    }
    peaks
}

fn decode_mp3(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut audio_start = 0usize;
    if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
        let tag_sz = (((bytes[6] & 0x7F) as usize) << 21)
            | (((bytes[7] & 0x7F) as usize) << 14)
            | (((bytes[8] & 0x7F) as usize) << 7)
            | ((bytes[9] & 0x7F) as usize);
        audio_start = (10 + tag_sz).min(bytes.len());
        metadata.push(MetadataEntry { section: "MP3".into(), key: "ID3v2 Tag Size".into(), value: format!("{tag_sz} bytes") });
    }
    let mut sample_rate = None;
    let mut bitrate_kbps = None;
    if let Some(pos) = bytes[audio_start..bytes.len().min(audio_start + 8192)]
        .windows(2)
        .position(|w| w[0] == 0xFF && (w[1] & 0xE0) == 0xE0)
    {
        let hdr = &bytes[audio_start + pos..];
        if hdr.len() >= 4 {
            let br_idx = (hdr[2] >> 4) as usize;
            let sr_idx = ((hdr[2] >> 2) & 0x03) as usize;
            let br_table = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0];
            let sr_table = [44100, 48000, 32000, 0];
            if br_table[br_idx] > 0 { bitrate_kbps = Some(br_table[br_idx]); }
            if sr_table[sr_idx] > 0 { sample_rate = Some(sr_table[sr_idx]); }
        }
    }
    let duration = bitrate_kbps.map(|br| ((bytes.len() - audio_start) as f64 * 8.0) / (br as f64 * 1000.0));
    if let Some(sr) = sample_rate {
        metadata.push(MetadataEntry { section: "MP3".into(), key: "Sample Rate".into(), value: format!("{sr} Hz") });
    }
    if let Some(br) = bitrate_kbps {
        metadata.push(MetadataEntry { section: "MP3".into(), key: "Bitrate".into(), value: format!("{br} kbps") });
    }
    Some(ViewerPayload::Media {
        media_kind: MediaKind::Audio,
        container: "MPEG Audio Layer III".into(),
        codec_summary: format!("MP3 · {} · {}", bitrate_kbps.map(|b| format!("{b} kbps")).unwrap_or_else(|| "VBR".into()), sample_rate.map(|s| format!("{s} Hz")).unwrap_or_default()),
        duration_secs: duration,
        sample_rate,
        channels: Some(2),
        dimensions: None,
        use_native_player: true,
        waveform_peaks: Vec::new(),
        svg_preview: None,
        subtitles: Vec::new(),
        cover_art_data_url: None,
    })
}

fn decode_flac(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 42 || &bytes[0..4] != b"fLaC" { return None; }
    let mut pos = 4usize;
    let mut sr = 44100u32;
    let mut ch = 2u16;
    let mut bps = 16u8;
    let mut total_samples = 0u64;
    let mut cover_art = None;

    while pos + 4 <= bytes.len() {
        let hdr = bytes[pos];
        let is_last = (hdr & 0x80) != 0;
        let btype = hdr & 0x7F;
        let len = ((bytes[pos + 1] as usize) << 16) | ((bytes[pos + 2] as usize) << 8) | (bytes[pos + 3] as usize);
        pos += 4;
        if pos + len > bytes.len() { break; }
        let block = &bytes[pos..pos + len];
        if btype == 0 && block.len() >= 18 {
            sr = ((block[10] as u32) << 12) | ((block[11] as u32) << 4) | ((block[12] as u32) >> 4);
            ch = (((block[12] >> 1) & 0x07) + 1) as u16;
            bps = (((block[12] & 0x01) << 4) | (block[13] >> 4)) + 1;
            total_samples = (((block[13] & 0x0F) as u64) << 32)
                | ((block[14] as u64) << 24)
                | ((block[15] as u64) << 16)
                | ((block[16] as u64) << 8)
                | (block[17] as u64);
        } else if btype == 6 && block.len() > 32 && cover_art.is_none() {
            let mime_len = u32::from_be_bytes([block[4], block[5], block[6], block[7]]) as usize;
            if 8 + mime_len + 24 <= block.len() {
                let mime = String::from_utf8_lossy(&block[8..8 + mime_len]).to_string();
                let desc_off = 8 + mime_len;
                let desc_len = u32::from_be_bytes([block[desc_off], block[desc_off + 1], block[desc_off + 2], block[desc_off + 3]]) as usize;
                let pic_hdr = desc_off + 4 + desc_len + 16;
                if pic_hdr + 4 <= block.len() {
                    let pic_len = u32::from_be_bytes([block[pic_hdr], block[pic_hdr + 1], block[pic_hdr + 2], block[pic_hdr + 3]]) as usize;
                    if pic_hdr + 4 + pic_len <= block.len() {
                        let pic_bytes = &block[pic_hdr + 4..pic_hdr + 4 + pic_len];
                        cover_art = Some(format!("data:{mime};base64,{}", B64.encode(pic_bytes)));
                    }
                }
            }
        }
        pos += len;
        if is_last { break; }
    }
    let duration = if sr > 0 && total_samples > 0 { Some(total_samples as f64 / sr as f64) } else { None };
    metadata.push(MetadataEntry { section: "FLAC".into(), key: "Audio".into(), value: format!("{sr} Hz · {ch} ch · {bps}-bit") });
    Some(ViewerPayload::Media {
        media_kind: MediaKind::Audio,
        container: "FLAC Lossless".into(),
        codec_summary: format!("FLAC · {sr} Hz · {ch} ch · {bps}-bit"),
        duration_secs: duration,
        sample_rate: Some(sr),
        channels: Some(ch),
        dimensions: None,
        use_native_player: true,
        waveform_peaks: Vec::new(),
        svg_preview: None,
        subtitles: Vec::new(),
        cover_art_data_url: cover_art,
    })
}

fn decode_ogg(bytes: &[u8], format_id: &str, metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let is_video = format_id == "ogv";
    let mut sr = None;
    let mut ch = None;
    if let Some(pos) = bytes[..bytes.len().min(256)].windows(8).position(|w| w == b"OpusHead") {
        if let Some(hdr) = bytes.get(pos..pos + 16) {
            ch = Some(hdr[9] as u16);
            sr = Some(u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]));
        }
    } else if let Some(pos) = bytes[..bytes.len().min(256)].windows(7).position(|w| w == b"\x01vorbis") {
        if let Some(hdr) = bytes.get(pos..pos + 16) {
            ch = Some(hdr[11] as u16);
            sr = Some(u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]));
        }
    }
    if let Some(s) = sr {
        metadata.push(MetadataEntry { section: "Ogg".into(), key: "Sample Rate".into(), value: format!("{s} Hz") });
    }
    Some(ViewerPayload::Media {
        media_kind: if is_video { MediaKind::Video } else { MediaKind::Audio },
        container: "Ogg Container".into(),
        codec_summary: format!("Ogg {}", format_id.to_ascii_uppercase()),
        duration_secs: None,
        sample_rate: sr,
        channels: ch,
        dimensions: None,
        use_native_player: true,
        waveform_peaks: Vec::new(),
        svg_preview: None,
        subtitles: Vec::new(),
        cover_art_data_url: None,
    })
}

fn decode_mp4_mov(bytes: &[u8], format_id: &str, metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let mut duration_secs = None;
    let mut dimensions = None;
    let mut codecs = Vec::new();
    scan_mp4_boxes(bytes, 0, bytes.len().min(16 * 1024 * 1024), &mut duration_secs, &mut dimensions, &mut codecs);

    let is_audio = matches!(format_id, "m4a" | "alac") || (dimensions.is_none() && !codecs.iter().any(|c| matches!(c.as_str(), "avc1" | "hvc1" | "hev1" | "av01" | "vp09")));
    if let Some((w, h)) = dimensions {
        metadata.push(MetadataEntry { section: "ISO-BMFF".into(), key: "Resolution".into(), value: format!("{w} × {h}") });
    }
    if !codecs.is_empty() {
        metadata.push(MetadataEntry { section: "ISO-BMFF".into(), key: "Stream Codecs".into(), value: codecs.join(", ") });
    }
    Some(ViewerPayload::Media {
        media_kind: if is_audio { MediaKind::Audio } else { MediaKind::Video },
        container: format!("ISO-BMFF (.{format_id})"),
        codec_summary: if codecs.is_empty() { format_id.to_ascii_uppercase() } else { codecs.join(" + ") },
        duration_secs,
        sample_rate: None,
        channels: None,
        dimensions,
        use_native_player: true,
        waveform_peaks: Vec::new(),
        svg_preview: None,
        subtitles: Vec::new(),
        cover_art_data_url: None,
    })
}

fn scan_mp4_boxes(
    bytes: &[u8],
    start: usize,
    end: usize,
    duration_secs: &mut Option<f64>,
    dimensions: &mut Option<(u32, u32)>,
    codecs: &mut Vec<String>,
) {
    let mut pos = start;
    while pos + 8 <= end {
        let sz32 = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        let (hdr_len, box_sz) = if sz32 == 1 && pos + 16 <= end {
            let sz64 = u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap_or([0; 8])) as usize;
            (16, sz64)
        } else if sz32 == 0 {
            (8, end - pos)
        } else {
            (8, sz32)
        };
        if box_sz < hdr_len || pos + box_sz > end { break; }
        let body = pos + hdr_len;
        let box_end = pos + box_sz;

        if kind == b"mvhd" && body + 20 <= box_end {
            let ver = bytes[body];
            if ver == 0 && body + 20 <= box_end {
                let ts = u32::from_be_bytes([bytes[body + 12], bytes[body + 13], bytes[body + 14], bytes[body + 15]]);
                let dur = u32::from_be_bytes([bytes[body + 16], bytes[body + 17], bytes[body + 18], bytes[body + 19]]);
                if ts > 0 { *duration_secs = Some(dur as f64 / ts as f64); }
            }
        } else if kind == b"tkhd" && body + 84 <= box_end {
            let w_fixed = u32::from_be_bytes([bytes[body + 76], bytes[body + 77], bytes[body + 78], bytes[body + 79]]) >> 16;
            let h_fixed = u32::from_be_bytes([bytes[body + 80], bytes[body + 81], bytes[body + 82], bytes[body + 83]]) >> 16;
            if w_fixed > 0 && h_fixed > 0 {
                *dimensions = Some((w_fixed, h_fixed));
            }
        } else if kind == b"stsd" && body + 16 <= box_end {
            let cname = String::from_utf8_lossy(&bytes[body + 12..body + 16]).trim().to_string();
            if !cname.is_empty() && !codecs.contains(&cname) {
                codecs.push(cname);
            }
        } else if matches!(kind, b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl") {
            scan_mp4_boxes(bytes, body, box_end, duration_secs, dimensions, codecs);
        }
        pos = box_end;
    }
}

fn decode_ebml(bytes: &[u8], format_id: &str, metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(65536)]);
    let mut codecs = Vec::new();
    for tag in ["V_VP9", "V_VP8", "V_AV1", "V_MPEG4/ISO/AVC", "V_MPEGH/ISO/HEVC", "A_OPUS", "A_VORBIS", "A_AAC", "A_FLAC", "A_AC3"] {
        if head.contains(tag) {
            codecs.push(tag.to_string());
        }
    }
    metadata.push(MetadataEntry { section: "EBML".into(), key: "Codecs".into(), value: if codecs.is_empty() { "Matroska / WebM".into() } else { codecs.join(", ") } });
    Some(ViewerPayload::Media {
        media_kind: if format_id == "mka" { MediaKind::Audio } else { MediaKind::Video },
        container: if format_id == "webm" { "WebM (EBML)".into() } else { "Matroska (EBML)".into() },
        codec_summary: if codecs.is_empty() { format_id.to_ascii_uppercase() } else { codecs.join(" + ") },
        duration_secs: None,
        sample_rate: None,
        channels: None,
        dimensions: None,
        use_native_player: format_id == "webm" || format_id == "mkv",
        waveform_peaks: Vec::new(),
        svg_preview: None,
        subtitles: Vec::new(),
        cover_art_data_url: None,
    })
}

fn decode_avi(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (mut w, mut h) = (0u32, 0u32);
    if let Some(pos) = bytes[..bytes.len().min(4096)].windows(4).position(|win| win == b"avih") {
        if pos + 44 <= bytes.len() {
            w = u32::from_le_bytes([bytes[pos + 40], bytes[pos + 41], bytes[pos + 42], bytes[pos + 43]]);
            h = u32::from_le_bytes([bytes[pos + 44], bytes[pos + 45], bytes[pos + 46], bytes[pos + 47]]);
        }
    }
    if w > 0 && h > 0 {
        metadata.push(MetadataEntry { section: "AVI".into(), key: "Resolution".into(), value: format!("{w} × {h}") });
    }
    Some(ViewerPayload::Media {
        media_kind: MediaKind::Video,
        container: "RIFF AVI".into(),
        codec_summary: "AVI Video Container".into(),
        duration_secs: None,
        sample_rate: None,
        channels: None,
        dimensions: (w > 0 && h > 0).then_some((w, h)),
        use_native_player: false,
        waveform_peaks: Vec::new(),
        svg_preview: None,
        subtitles: Vec::new(),
        cover_art_data_url: None,
    })
}

fn decode_midi(bytes: &[u8], metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    if bytes.len() < 14 || &bytes[0..4] != b"MThd" { return None; }
    let format = u16::from_be_bytes([bytes[8], bytes[9]]);
    let tracks = u16::from_be_bytes([bytes[10], bytes[11]]);
    let division = u16::from_be_bytes([bytes[12], bytes[13]]);

    let mut pos = 14usize;
    let mut notes: Vec<(usize, u8)> = Vec::new();
    let mut total_ticks = 0usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_be_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        pos += 8;
        let end = (pos + len).min(bytes.len());
        if id == b"MTrk" {
            let mut p = pos;
            let mut tick = 0usize;
            let mut running = 0u8;
            while p < end {
                let (dt, np) = read_vlq(bytes, p, end);
                tick += dt as usize;
                p = np;
                if p >= end { break; }
                let mut status = bytes[p];
                if status < 0x80 {
                    status = running;
                } else {
                    p += 1;
                    if status < 0xF0 { running = status; }
                }
                match status & 0xF0 {
                    0x90 if p + 1 < end => {
                        let note = bytes[p];
                        let vel = bytes[p + 1];
                        p += 2;
                        if vel > 0 && notes.len() < 4000 { notes.push((tick, note)); }
                    }
                    0x80 | 0xA0 | 0xB0 | 0xE0 => p = (p + 2).min(end),
                    0xC0 | 0xD0 => p = (p + 1).min(end),
                    0xF0 => {
                        if status == 0xFF && p + 1 < end {
                            p += 1;
                            let (mlen, np2) = read_vlq(bytes, p, end);
                            p = (np2 + mlen as usize).min(end);
                        } else {
                            let (slen, np2) = read_vlq(bytes, p, end);
                            p = (np2 + slen as usize).min(end);
                        }
                    }
                    _ => p += 1,
                }
            }
            total_ticks = total_ticks.max(tick);
        }
        pos = end;
    }

    let max_t = (total_ticks as f32).max(100.0);
    let mut rects = String::new();
    for (t, pitch) in notes.iter().take(1500) {
        let x = (*t as f32 / max_t) * 980.0 + 10.0;
        let y = 260.0 - (*pitch as f32 * 2.0);
        let _ = write!(rects, "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"4\" height=\"3\" fill=\"#EAFFFF\"/>");
    }
    let svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1000 280\">{rects}</svg>");
    metadata.push(MetadataEntry { section: "MIDI".into(), key: "Tracks / Format".into(), value: format!("{tracks} tracks (Type {format}, PPQ {division})") });
    metadata.push(MetadataEntry { section: "MIDI".into(), key: "Note Events".into(), value: notes.len().to_string() });

    Some(ViewerPayload::Media {
        media_kind: MediaKind::Midi,
        container: "Standard MIDI File".into(),
        codec_summary: format!("MIDI Type {format} · {tracks} tracks · {} notes", notes.len()),
        duration_secs: None,
        sample_rate: None,
        channels: None,
        dimensions: None,
        use_native_player: false,
        waveform_peaks: Vec::new(),
        svg_preview: Some(svg),
        subtitles: Vec::new(),
        cover_art_data_url: None,
    })
}

fn read_vlq(b: &[u8], mut pos: usize, end: usize) -> (u32, usize) {
    let mut val = 0u32;
    for _ in 0..4 {
        if pos >= end { break; }
        let byte = b[pos];
        pos += 1;
        val = (val << 7) | (byte & 0x7F) as u32;
        if byte & 0x80 == 0 { break; }
    }
    (val, pos)
}

fn decode_subtitles(bytes: &[u8], format_id: &str, metadata: &mut Vec<MetadataEntry>) -> Option<ViewerPayload> {
    let (text, _, _) = decode_to_utf8(bytes);
    let mut cues = Vec::new();
    if matches!(format_id, "srt" | "vtt") {
        let blocks = text.replace("\r\n", "\n");
        for (idx, block) in blocks.split("\n\n").enumerate() {
            let lines: Vec<&str> = block.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            if let Some(ti) = lines.iter().position(|l| l.contains("-->")) {
                let (s, e) = lines[ti].split_once("-->").unwrap_or(("", ""));
                let body = lines[ti + 1..].join(" ");
                cues.push(SubtitleCue {
                    index: idx + 1,
                    start: s.trim().into(),
                    end: e.trim().into(),
                    text: body,
                });
            }
        }
    } else if format_id == "lrc" {
        for (idx, line) in text.lines().enumerate() {
            if let Some(close) = line.find(']') {
                if line.starts_with('[') {
                    cues.push(SubtitleCue {
                        index: idx + 1,
                        start: line[1..close].to_string(),
                        end: String::new(),
                        text: line[close + 1..].trim().to_string(),
                    });
                }
            }
        }
    } else {
        for (idx, line) in text.lines().enumerate() {
            if let Some(rest) = line.strip_prefix("Dialogue:") {
                let parts: Vec<&str> = rest.splitn(10, ',').collect();
                if parts.len() >= 10 {
                    cues.push(SubtitleCue {
                        index: idx + 1,
                        start: parts[1].trim().into(),
                        end: parts[2].trim().into(),
                        text: parts[9].trim().into(),
                    });
                }
            }
        }
    }
    metadata.push(MetadataEntry { section: "Subtitles".into(), key: "Cues".into(), value: cues.len().to_string() });
    Some(ViewerPayload::Media {
        media_kind: MediaKind::Subtitle,
        container: format_id.to_ascii_uppercase(),
        codec_summary: format!("{} Timed Cues ({})", format_id.to_ascii_uppercase(), cues.len()),
        duration_secs: None,
        sample_rate: None,
        channels: None,
        dimensions: None,
        use_native_player: false,
        waveform_peaks: Vec::new(),
        svg_preview: None,
        subtitles: cues,
        cover_art_data_url: None,
    })
}
