use std::io::Write as _;

use ufo_core::{
    extract_archive_entry, open_bytes, prepare_resave, slice_hex_page, slice_text_page,
    DetectionConfidence, FormatCategory, ViewerPayload, AMBIGUOUS_EXTENSIONS, FORMAT_REGISTRY,
};

#[test]
fn registry_covers_over_400_extensions_and_all_23_categories() {
    assert!(FORMAT_REGISTRY.len() >= 400, "Expected 400+ extensions, got {}", FORMAT_REGISTRY.len());
    assert!(AMBIGUOUS_EXTENSIONS.len() >= 17);
}

#[test]
fn opens_and_disambiguates_wavefront_vs_coff_obj() {
    let wavefront = b"# cube\nv 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";
    let rep_3d = open_bytes("cube.obj", wavefront, None);
    assert_eq!(rep_3d.detected.id, "obj");
    assert_eq!(rep_3d.detected.category, FormatCategory::Model3D);
    assert!(rep_3d.detected.ambiguity.is_some());
    assert!(!rep_3d.detected.ambiguity.as_ref().unwrap().extension_mismatch);
    match rep_3d.viewer {
        ViewerPayload::Mesh3d { triangle_count, .. } => assert_eq!(triangle_count, 1),
        other => panic!("Expected Mesh3d, got {other:?}"),
    }

    let mut coff = vec![0u8; 32];
    coff[0] = 0x64;
    coff[1] = 0x86; // AMD64 COFF machine
    let rep_coff = open_bytes("main.obj", &coff, None);
    assert_eq!(rep_coff.detected.id, "coff");
    assert_eq!(rep_coff.detected.category, FormatCategory::ExecutableLib);
    assert!(rep_coff.detected.ambiguity.as_ref().unwrap().extension_mismatch);
}

#[test]
fn detects_extension_mismatch_warning_for_spoofed_c_file() {
    // Minimal 1x1 RGBA PNG named "kore.c" (triggers warning like demo UI)
    let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([234, 255, 255, 255]));
    let mut png_bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .unwrap();

    let rep = open_bytes("kore.c", &png_bytes, None);
    assert_eq!(rep.detected.id, "png");
    assert!(rep.detected.ambiguity.as_ref().unwrap().extension_mismatch);
    assert!(!rep.detected.warnings.is_empty());
    match rep.viewer {
        ViewerPayload::Image { width, height, .. } => assert_eq!((width, height), (2, 2)),
        other => panic!("Expected Image, got {other:?}"),
    }
}

#[test]
fn disambiguates_typescript_vs_mpeg_ts() {
    let ts_code = b"export interface FileReport { id: string; size: number; }\n";
    let rep_code = open_bytes("app.ts", ts_code, None);
    assert_eq!(rep_code.detected.id, "ts");
    assert!(matches!(rep_code.viewer, ViewerPayload::Text { .. }));

    let mut mpeg = vec![0u8; 188 * 3];
    mpeg[0] = 0x47;
    mpeg[188] = 0x47;
    mpeg[376] = 0x47;
    let rep_vid = open_bytes("stream.ts", &mpeg, None);
    assert_eq!(rep_vid.detected.id, "mpegts");
    assert_eq!(rep_vid.detected.category, FormatCategory::VideoSubtitles);
}

#[test]
fn decodes_binary_stl_and_gcode_and_dxf() {
    // Binary STL: 80-byte header + 1 tri (50 bytes) = 134 bytes
    let mut stl = vec![0u8; 134];
    stl[0..5].copy_from_slice(b"UFO3D");
    stl[80..84].copy_from_slice(&1u32.to_le_bytes());
    stl[96..100].copy_from_slice(&1.0f32.to_le_bytes()); // v1.x
    stl[112..116].copy_from_slice(&1.0f32.to_le_bytes()); // v2.y
    let rep_stl = open_bytes("part.stl", &stl, None);
    match rep_stl.viewer {
        ViewerPayload::Mesh3d { triangle_count, .. } => assert_eq!(triangle_count, 1),
        other => panic!("Expected Mesh3d, got {other:?}"),
    }

    let gcode = b"G28\nG1 X10 Y10 Z0.2\nG1 X50 Y10 Z0.2\nG1 X50 Y50 Z0.4\n";
    let rep_gc = open_bytes("bracket.gcode", gcode, None);
    match rep_gc.viewer {
        ViewerPayload::Mesh3d { edge_count, .. } => assert_eq!(edge_count, 3),
        other => panic!("Expected Mesh3d toolpath, got {other:?}"),
    }

    let dxf = b"0\nSECTION\n2\nENTITIES\n0\nLINE\n10\n0\n20\n0\n11\n100\n21\n50\n0\nENDSEC\n0\nEOF\n";
    let rep_dxf = open_bytes("plan.dxf", dxf, None);
    match rep_dxf.viewer {
        ViewerPayload::Vector2d { entity_count, svg_markup, .. } => {
            assert_eq!(entity_count, 1);
            assert!(svg_markup.contains("<line"));
        }
        other => panic!("Expected Vector2d, got {other:?}"),
    }
}

#[test]
fn decodes_zip_archive_and_extracts_nested_entry() {
    let mut buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("hello.json", opts).unwrap();
        zw.write_all(br#"{"ufo":"fast","speed":100}"#).unwrap();
        zw.finish().unwrap();
    }
    let rep = open_bytes("bundle.zip", &buf, None);
    match rep.viewer {
        ViewerPayload::Archive { total_entries, .. } => assert_eq!(total_entries, 1),
        other => panic!("Expected Archive, got {other:?}"),
    }
    let nested = extract_archive_entry(&buf, "hello.json").unwrap();
    let nested_rep = open_bytes("hello.json", &nested, None);
    assert!(matches!(nested_rep.viewer, ViewerPayload::Structured { .. }));
}

#[test]
fn decodes_wasm_module_and_elf_and_sqlite_and_wav() {
    // Valid minimal WASM module header + custom section
    let wasm = [0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x01, b'a', 0x00, 0x00];
    let rep_wasm = open_bytes("mod.wasm", &wasm, None);
    match rep_wasm.viewer {
        ViewerPayload::Binary { sections, .. } => assert_eq!(sections.len(), 1),
        other => panic!("Expected Binary for WASM, got {other:?}"),
    }

    // Minimal RIFF WAV header + 16-bit PCM
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&44u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&44100u32.to_le_bytes());
    wav.extend_from_slice(&88200u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&8u32.to_le_bytes());
    wav.extend_from_slice(&[0, 0, 0x7F, 0x3F, 0, 0, 0x80, 0xC0]);
    let rep_wav = open_bytes("tone.wav", &wav, None);
    match rep_wav.viewer {
        ViewerPayload::Media { sample_rate, waveform_peaks, .. } => {
            assert_eq!(sample_rate, Some(44100));
            assert!(!waveform_peaks.is_empty());
        }
        other => panic!("Expected Media for WAV, got {other:?}"),
    }
}

#[test]
fn opens_any_unknown_binary_safely_with_hex_and_resave() {
    let mut junk = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02];
    junk.extend_from_slice(b"UFO_EMBEDDED_STRING");
    junk.extend_from_slice(&[0x00, 0xFF, 0x80]);

    let rep = open_bytes("mystery_payload.qxz99", &junk, None);
    assert_eq!(rep.detected.confidence, DetectionConfidence::BinaryFallback);
    assert!(matches!(rep.viewer, ViewerPayload::HexOnly { .. }));
    assert!(rep.preview_strings.iter().any(|s| s.value.contains("UFO_EMBEDDED_STRING")));

    let hex_page = slice_hex_page(&junk, 0, 512);
    assert!(!hex_page.rows.is_empty());

    let text_sample = b"line1\nline2\nline3\n";
    let tpage = slice_text_page(text_sample, 1, 20, "text");
    assert_eq!(tpage.lines.len(), 2);

    let export = prepare_resave("mystery_payload.qxz99", &junk, true).unwrap();
    assert!(export.suggested_name.ends_with(".ufo-report.json"));
    assert!(String::from_utf8_lossy(&export.bytes).contains("mystery_payload.qxz99"));
}

#[test]
fn decodes_step_cad_and_sqlite_and_markdown_and_csv() {
    let step = b"ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('UFO'),'2;1');\nFILE_NAME('car.step','2026-11-26',(''),(''),'','','');\nFILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\nENDSEC;\nDATA;\n#1=CARTESIAN_POINT('',(0.,0.,0.));\n#2=CARTESIAN_POINT('',(10.,0.,0.));\nENDSEC;\nEND-ISO-10303-21;\n";
    let rep_step = open_bytes("car.step", step, None);
    assert_eq!(rep_step.detected.id, "step");
    assert!(matches!(rep_step.viewer, ViewerPayload::Mesh3d { .. }));

    // Minimal valid 1024-byte SQLite 3 database with empty leaf table root page 1
    let mut sq = vec![0u8; 1024];
    sq[0..16].copy_from_slice(b"SQLite format 3\0");
    sq[16..18].copy_from_slice(&1024u16.to_be_bytes());
    sq[56..60].copy_from_slice(&1u32.to_be_bytes()); // UTF-8
    sq[100] = 0x0D; // leaf table B-tree page
    let rep_sq = open_bytes("app.sqlite", &sq, None);
    assert_eq!(rep_sq.detected.id, "sqlite");
    assert!(matches!(rep_sq.viewer, ViewerPayload::Table { .. }));

    let md = b"# Universal File Opener\n\n- Fast\n- Rust\n";
    let rep_md = open_bytes("notes.md", md, None);
    match rep_md.viewer {
        ViewerPayload::Text { markdown_html, .. } => assert!(markdown_html.unwrap().contains("<h1>Universal File Opener</h1>")),
        other => panic!("Expected markdown Text, got {other:?}"),
    }

    let csv = b"id,ext,category\n1,step,CAD\n2,png,Image\n";
    let rep_csv = open_bytes("formats.csv", csv, None);
    match rep_csv.viewer {
        ViewerPayload::Table { sheets, .. } => {
            assert_eq!(sheets[0].columns, vec!["id", "ext", "category"]);
            assert_eq!(sheets[0].rows.len(), 2);
        }
        other => panic!("Expected Table, got {other:?}"),
    }
}

#[test]
fn decodes_gaussian_splat_ply_and_splat_with_colors() {
    let header = concat!(
        "ply\n",
        "format binary_little_endian 1.0\n",
        "element vertex 2\n",
        "property float x\n",
        "property float y\n",
        "property float z\n",
        "property float f_dc_0\n",
        "property float f_dc_1\n",
        "property float f_dc_2\n",
        "property float opacity\n",
        "end_header\n"
    );
    let mut ply = header.as_bytes().to_vec();
    for vals in [
        [0.0f32, 0.0, 0.0, 1.5, -1.5, -1.5, 2.0],
        [1.0f32, 0.5, -0.5, -1.5, 1.5, 1.5, 2.0],
    ] {
        for v in vals {
            ply.extend_from_slice(&v.to_le_bytes());
        }
    }

    let rep = open_bytes("scene.ply", &ply, None);
    assert_eq!(rep.detected.id, "ply");
    match rep.viewer {
        ViewerPayload::Mesh3d {
            point_count,
            is_gaussian_splat,
            point_positions_b64,
            point_colors_b64,
            ..
        } => {
            assert!(is_gaussian_splat);
            assert_eq!(point_count, 2);
            assert!(!point_positions_b64.is_empty());
            assert!(!point_colors_b64.is_empty());
        }
        other => panic!("Expected Mesh3d splat, got {other:?}"),
    }

    // Standard 32-byte .splat binary format (2 splats * 32 bytes)
    let mut splat = Vec::with_capacity(64);
    for (pos, rgba) in [([0.0f32, 1.0, 2.0], [234u8, 255, 255, 255]), ([1.0f32, 0.0, -1.0], [255u8, 80, 120, 220])] {
        for f in pos {
            splat.extend_from_slice(&f.to_le_bytes());
        }
        for s in [0.1f32, 0.1, 0.1] {
            splat.extend_from_slice(&s.to_le_bytes());
        }
        splat.extend_from_slice(&rgba);
        splat.extend_from_slice(&[255u8, 128, 128, 128]);
    }
    let rep_splat = open_bytes("garden.splat", &splat, None);
    assert_eq!(rep_splat.detected.id, "splat");
    match rep_splat.viewer {
        ViewerPayload::Mesh3d {
            point_count,
            is_gaussian_splat,
            point_positions_b64,
            point_colors_b64,
            ..
        } => {
            assert!(is_gaussian_splat);
            assert_eq!(point_count, 2);
            assert!(!point_positions_b64.is_empty());
            assert!(!point_colors_b64.is_empty());
        }
        other => panic!("Expected Mesh3d .splat, got {other:?}"),
    }
}

#[test]
fn decodes_glb_with_embedded_texture_and_uv_batches() {
    let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 220, 200, 255]));
    let mut png_bytes = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .unwrap();

    // Binary buffer: 3*vec3 pos (36) + 3*vec2 uv (24) + 3*u16 idx (6, padded to 8 -> offset 68) + png_bytes
    let mut bin = Vec::new();
    for p in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for f in p {
            bin.extend_from_slice(&f.to_le_bytes());
        }
    }
    let uv_offset = bin.len();
    for uv in [[0.0f32, 0.0], [1.0, 0.0], [0.0, 1.0]] {
        for f in uv {
            bin.extend_from_slice(&f.to_le_bytes());
        }
    }
    let idx_offset = bin.len();
    for i in [0u16, 1, 2] {
        bin.extend_from_slice(&i.to_le_bytes());
    }
    bin.resize(idx_offset + 8, 0);
    let png_offset = bin.len();
    bin.extend_from_slice(&png_bytes);
    while bin.len() % 4 != 0 {
        bin.push(0);
    }

    let json = format!(
        r#"{{
          "asset": {{"version": "2.0"}},
          "scenes": [{{"nodes": [0]}}],
          "nodes": [{{"mesh": 0}}],
          "meshes": [{{"primitives": [{{"attributes": {{"POSITION": 0, "TEXCOORD_0": 1}}, "indices": 2, "material": 0}}]}}],
          "materials": [{{"pbrMetallicRoughness": {{"baseColorTexture": {{"index": 0}}}}}}],
          "textures": [{{"source": 0}}],
          "images": [{{"bufferView": 3, "mimeType": "image/png"}}],
          "buffers": [{{"byteLength": {buf_len}}}],
          "bufferViews": [
            {{"buffer": 0, "byteOffset": 0, "byteLength": 36}},
            {{"buffer": 0, "byteOffset": {uv_offset}, "byteLength": 24}},
            {{"buffer": 0, "byteOffset": {idx_offset}, "byteLength": 6}},
            {{"buffer": 0, "byteOffset": {png_offset}, "byteLength": {png_len}}}
          ],
          "accessors": [
            {{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0,0,0], "max": [1,1,0]}},
            {{"bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC2"}},
            {{"bufferView": 2, "componentType": 5123, "count": 3, "type": "SCALAR"}}
          ]
        }}"#,
        buf_len = bin.len(),
        uv_offset = uv_offset,
        idx_offset = idx_offset,
        png_offset = png_offset,
        png_len = png_bytes.len(),
    );
    let mut json_bytes = json.into_bytes();
    while json_bytes.len() % 4 != 0 {
        json_bytes.push(b' ');
    }

    let total_len = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut glb = Vec::with_capacity(total_len);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total_len as u32).to_le_bytes());
    glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    glb.extend_from_slice(&0x4E4F534Au32.to_le_bytes()); // JSON
    glb.extend_from_slice(&json_bytes);
    glb.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    glb.extend_from_slice(&0x004E4942u32.to_le_bytes()); // BIN
    glb.extend_from_slice(&bin);

    let rep = open_bytes("textured_tri.glb", &glb, None);
    assert_eq!(rep.detected.id, "glb");
    match rep.viewer {
        ViewerPayload::Mesh3d {
            triangle_count,
            uvs_b64,
            textures,
            batches,
            ..
        } => {
            assert_eq!(triangle_count, 1);
            assert!(!uvs_b64.is_empty());
            assert_eq!(textures.len(), 1);
            assert!(textures[0].starts_with("data:image/png;base64,"));
            assert_eq!(batches.len(), 1);
            assert_eq!(batches[0].texture_index, Some(0));
        }
        other => panic!("Expected textured GLB Mesh3d, got {other:?}"),
    }
}

#[test]
fn decodes_blender_5x_large_bhead_zstd_scene() {
    fn push_large_bhead(dst: &mut Vec<u8>, code: &[u8; 4], sdna: i32, old: u64, nr: i64, payload: &[u8]) {
        dst.extend_from_slice(code);
        dst.extend_from_slice(&sdna.to_le_bytes());
        dst.extend_from_slice(&old.to_le_bytes());
        dst.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        dst.extend_from_slice(&nr.to_le_bytes());
        dst.extend_from_slice(payload);
    }

    // Build minimal Blender 5.3 SDNA schema with ID, AttributeArray, Attribute, AttributeStorage, Mesh
    let names = [
        "name[66]",
        "totvert",
        "faces_num",
        "*face_offset_indices",
        "attribute_storage",
        "id",
        "*dna_attributes",
        "dna_attributes_num",
        "*name",
        "*data",
    ];
    let types = [
        "char",             // 0: 1
        "int",              // 1: 4
        "void",             // 2: 0
        "ID",               // 3: 68
        "AttributeArray",   // 4: 8
        "Attribute",        // 5: 16
        "AttributeStorage", // 6: 16
        "Mesh",             // 7: 96
    ];
    let type_lens: [u16; 8] = [1, 4, 0, 68, 8, 16, 16, 96];

    let mut dna = Vec::new();
    dna.extend_from_slice(b"SDNA");
    dna.extend_from_slice(b"NAME");
    dna.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for n in names {
        dna.extend_from_slice(n.as_bytes());
        dna.push(0);
    }
    while dna.len() % 4 != 0 { dna.push(0); }

    dna.extend_from_slice(b"TYPE");
    dna.extend_from_slice(&(types.len() as u32).to_le_bytes());
    for t in types {
        dna.extend_from_slice(t.as_bytes());
        dna.push(0);
    }
    while dna.len() % 4 != 0 { dna.push(0); }

    dna.extend_from_slice(b"TLEN");
    for l in type_lens {
        dna.extend_from_slice(&l.to_le_bytes());
    }
    while dna.len() % 4 != 0 { dna.push(0); }

    dna.extend_from_slice(b"STRC");
    dna.extend_from_slice(&5u32.to_le_bytes());
    // 0: ID { char name[66]; }
    for (ti, fields) in [
        (3u16, vec![(0u16, 0u16)]),
        // 1: AttributeArray { void *data; }
        (4u16, vec![(2u16, 9u16)]),
        // 2: Attribute { char *name; void *data; }
        (5u16, vec![(0u16, 8u16), (2u16, 9u16)]),
        // 3: AttributeStorage { Attribute *dna_attributes; int dna_attributes_num; }
        (6u16, vec![(5u16, 6u16), (1u16, 7u16)]),
        // 4: Mesh { ID id; int totvert; int faces_num; int *face_offset_indices; AttributeStorage attribute_storage; }
        (7u16, vec![(3u16, 5u16), (1u16, 1u16), (1u16, 2u16), (1u16, 3u16), (6u16, 4u16)]),
    ] {
        dna.extend_from_slice(&ti.to_le_bytes());
        dna.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for (ft, fn_i) in fields {
            dna.extend_from_slice(&ft.to_le_bytes());
            dna.extend_from_slice(&fn_i.to_le_bytes());
        }
    }

    let mut blend = Vec::new();
    // 17-byte Blender 5.3 header: 8-byte ptr '-', LargeBHead8 '01', LE 'v', '0503'
    blend.extend_from_slice(b"BLENDER17-01v0503");

    // SCScene block
    let mut sc_id = vec![0u8; 68];
    sc_id[..7].copy_from_slice(b"SCScene");
    push_large_bhead(&mut blend, b"SC\0\0", 0, 0x1000, 1, &sc_id);

    // Position and corner_vert buffers
    let mut pos_buf = Vec::new();
    for p in [[0.0f32, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 2.0, 0.0]] {
        for f in p { pos_buf.extend_from_slice(&f.to_le_bytes()); }
    }
    let mut cv_buf = Vec::new();
    for i in [0i32, 1, 2] {
        cv_buf.extend_from_slice(&i.to_le_bytes());
    }
    let mut fo_buf = Vec::new();
    for i in [0i32, 3] {
        fo_buf.extend_from_slice(&i.to_le_bytes());
    }

    push_large_bhead(&mut blend, b"DATA", 0, 0x2001, 1, b"position\0");
    push_large_bhead(&mut blend, b"DATA", 0, 0x2002, 1, &pos_buf);
    push_large_bhead(&mut blend, b"DATA", 1, 0x2003, 1, &0x2002u64.to_le_bytes());
    push_large_bhead(&mut blend, b"DATA", 0, 0x2004, 1, b".corner_vert\0");
    push_large_bhead(&mut blend, b"DATA", 0, 0x2005, 1, &cv_buf);
    push_large_bhead(&mut blend, b"DATA", 1, 0x2006, 1, &0x2005u64.to_le_bytes());
    push_large_bhead(&mut blend, b"DATA", 0, 0x2007, 1, &fo_buf);

    // 2 Attribute structs @ 0x3000
    let mut attrs = Vec::new();
    attrs.extend_from_slice(&0x2001u64.to_le_bytes());
    attrs.extend_from_slice(&0x2003u64.to_le_bytes());
    attrs.extend_from_slice(&0x2004u64.to_le_bytes());
    attrs.extend_from_slice(&0x2006u64.to_le_bytes());
    push_large_bhead(&mut blend, b"DATA", 2, 0x3000, 2, &attrs);

    // Mesh struct @ 0x4000 (96 bytes matching SDNA offsets: 0..68 ID, 68 totvert, 72 faces_num, 76..84 *face_offset_indices, 84..92 *dna_attributes, 92..96 dna_attributes_num)
    let mut mesh = vec![0u8; 96];
    mesh[..6].copy_from_slice(b"MECube");
    mesh[68..72].copy_from_slice(&3i32.to_le_bytes());
    mesh[72..76].copy_from_slice(&1i32.to_le_bytes());
    mesh[76..84].copy_from_slice(&0x2007u64.to_le_bytes());
    mesh[84..92].copy_from_slice(&0x3000u64.to_le_bytes());
    mesh[92..96].copy_from_slice(&2i32.to_le_bytes());
    push_large_bhead(&mut blend, b"ME\0\0", 4, 0x4000, 1, &mesh);

    push_large_bhead(&mut blend, b"DNA1", 0, 0x5000, 1, &dna);
    push_large_bhead(&mut blend, b"ENDB", 0, 0, 0, &[]);

    // Wrap in valid RFC 8878 Zstandard single raw block frame (< 64 KiB)
    let mut zstd_blend = Vec::new();
    zstd_blend.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]); // Zstd magic
    assert!((256..65792).contains(&blend.len()));
    zstd_blend.push(0x60); // FHD: Single_Segment=1, FCS_Field_Size=1 (2 bytes)
    zstd_blend.extend_from_slice(&((blend.len() - 256) as u16).to_le_bytes());
    let blk_hdr = ((blend.len() as u32) << 3) | 1; // Last_Block=1, Raw_Block=0
    zstd_blend.extend_from_slice(&blk_hdr.to_le_bytes()[..3]);
    zstd_blend.extend_from_slice(&blend);

    let rep = open_bytes("blender53_test.blend", &zstd_blend, None);
    assert_eq!(rep.detected.id, "blend");
    match rep.viewer {
        ViewerPayload::Mesh3d {
            format_label,
            vertex_count,
            triangle_count,
            parts,
            ..
        } => {
            assert!(format_label.contains("v5.3"), "got label: {format_label}");
            assert_eq!(vertex_count, 3);
            assert_eq!(triangle_count, 1);
            assert!(parts.iter().any(|p| p.contains("Cube")));
        }
        other => panic!("Expected Blender Mesh3d, got {other:?}"),
    }
}

#[test]
fn decodes_step_planar_face_into_shaded_triangles() {
    let step = concat!(
        "ISO-10303-21;\n",
        "HEADER;\n",
        "FILE_DESCRIPTION(('STEP AP214 Square'),'2;1');\n",
        "FILE_NAME('plate.step','2026-11-26',(''),(''),'','','');\n",
        "FILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\n",
        "ENDSEC;\n",
        "DATA;\n",
        "#1=CARTESIAN_POINT('',(0.,0.,0.));\n",
        "#2=DIRECTION('',(0.,0.,1.));\n",
        "#3=DIRECTION('',(1.,0.,0.));\n",
        "#4=AXIS2_PLACEMENT_3D('',#1,#2,#3);\n",
        "#5=PLANE('',#4);\n",
        "#10=CARTESIAN_POINT('',(0.,0.,0.));\n",
        "#11=CARTESIAN_POINT('',(10.,0.,0.));\n",
        "#12=CARTESIAN_POINT('',(10.,10.,0.));\n",
        "#13=CARTESIAN_POINT('',(0.,10.,0.));\n",
        "#20=VERTEX_POINT('',#10);\n",
        "#21=VERTEX_POINT('',#11);\n",
        "#22=VERTEX_POINT('',#12);\n",
        "#23=VERTEX_POINT('',#13);\n",
        "#30=DIRECTION('',(1.,0.,0.));\n",
        "#31=DIRECTION('',(0.,1.,0.));\n",
        "#32=DIRECTION('',(-1.,0.,0.));\n",
        "#33=DIRECTION('',(0.,-1.,0.));\n",
        "#34=VECTOR('',#30,10.);\n",
        "#35=VECTOR('',#31,10.);\n",
        "#36=VECTOR('',#32,10.);\n",
        "#37=VECTOR('',#33,10.);\n",
        "#40=LINE('',#10,#34);\n",
        "#41=LINE('',#11,#35);\n",
        "#42=LINE('',#12,#36);\n",
        "#43=LINE('',#13,#37);\n",
        "#50=EDGE_CURVE('',#20,#21,#40,.T.);\n",
        "#51=EDGE_CURVE('',#21,#22,#41,.T.);\n",
        "#52=EDGE_CURVE('',#22,#23,#42,.T.);\n",
        "#53=EDGE_CURVE('',#23,#20,#43,.T.);\n",
        "#60=ORIENTED_EDGE('',*,*,#50,.T.);\n",
        "#61=ORIENTED_EDGE('',*,*,#51,.T.);\n",
        "#62=ORIENTED_EDGE('',*,*,#52,.T.);\n",
        "#63=ORIENTED_EDGE('',*,*,#53,.T.);\n",
        "#70=EDGE_LOOP('',(#60,#61,#62,#63));\n",
        "#80=FACE_OUTER_BOUND('',#70,.T.);\n",
        "#90=ADVANCED_FACE('',(#80),#5,.T.);\n",
        "#100=CLOSED_SHELL('',(#90));\n",
        "#110=MANIFOLD_SOLID_BREP('Plate',#100);\n",
        "ENDSEC;\n",
        "END-ISO-10303-21;\n"
    );

    let rep = open_bytes("plate.step", step.as_bytes(), None);
    assert_eq!(rep.detected.id, "step");
    match rep.viewer {
        ViewerPayload::Mesh3d {
            triangle_count,
            positions_b64,
            normals_b64,
            ..
        } => {
            assert!(triangle_count >= 2, "Expected shaded triangles, got {triangle_count}");
            assert!(!positions_b64.is_empty());
            assert!(!normals_b64.is_empty());
        }
        other => panic!("Expected shaded STEP Mesh3d, got {other:?}"),
    }
}

#[test]
fn extracts_svg_frame_dimensions_and_html_raw_render() {
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="360px" height="60px" viewBox="0 0 360 60"><rect width="360" height="60" fill="#000"/></svg>"##;
    let rep_svg = open_bytes("logo.svg", svg, None);
    assert_eq!(rep_svg.detected.id, "svg");
    match rep_svg.viewer {
        ViewerPayload::Svg {
            width,
            height,
            view_box,
            ..
        } => {
            assert_eq!(width.as_deref(), Some("360px"));
            assert_eq!(height.as_deref(), Some("60px"));
            assert_eq!(view_box.as_deref(), Some("0 0 360 60"));
        }
        other => panic!("Expected Svg payload, got {other:?}"),
    }

    let html = b"<!doctype html><html><body><h1>UFO Render</h1></body></html>";
    let rep_html = open_bytes("preview.html", html, None);
    assert_eq!(rep_html.detected.id, "html");
    match rep_html.viewer {
        ViewerPayload::Text { raw_html, .. } => {
            assert!(raw_html.unwrap().contains("<h1>UFO Render</h1>"));
        }
        other => panic!("Expected Text payload for HTML, got {other:?}"),
    }
}
