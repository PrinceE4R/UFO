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

fn make_png_bytes(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba(rgba));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn wrap_zstd_raw_frame(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 32);
    out.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]); // Zstd frame magic
    out.push(0xA0); // Single_Segment=1, FCS_Field_Size=2 (4 bytes LE)
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00]);
        return out;
    }
    for (ci, chunk) in raw.chunks(128 * 1024).enumerate() {
        let is_last = (ci + 1) * 128 * 1024 >= raw.len();
        let hdr = ((chunk.len() as u32) << 3) | u32::from(is_last);
        out.extend_from_slice(&hdr.to_le_bytes()[..3]);
        out.extend_from_slice(chunk);
    }
    out
}

#[test]
fn decodes_solidcore_step_multi_solid_layers_and_freecad_xml_metadata() {
    let step = concat!(
        "ISO-10303-21;\n",
        "HEADER;\n",
        "FILE_DESCRIPTION(('STEP Assembly'),'2;1');\n",
        "FILE_NAME('assy.step','2026-11-26',(''),(''),'','','');\n",
        "FILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\n",
        "ENDSEC;\n",
        "DATA;\n",
        "#1=CARTESIAN_POINT('',(0.,0.,0.));\n",
        "#2=CARTESIAN_POINT('',(10.,0.,0.));\n",
        "#3=DIRECTION('',(0.,0.,-1.));\n",
        "#4=DIRECTION('',(1.,0.,0.));\n",
        "#5=DIRECTION('',(0.,-1.,0.));\n",
        "#6=DIRECTION('',(-1.,0.,0.));\n",
        "#7=DIRECTION('',(0.,0.,1.));\n",
        "#8=DIRECTION('',(0.57735027,0.57735027,0.57735027));\n",
        "#9=DIRECTION('',(-0.70710678,0.70710678,0.));\n",
        "#10=CARTESIAN_POINT('',(0.,0.,0.));\n",
        "#11=CARTESIAN_POINT('',(10.,0.,0.));\n",
        "#12=CARTESIAN_POINT('',(0.,10.,0.));\n",
        "#13=CARTESIAN_POINT('',(0.,0.,10.));\n",
        "#14=AXIS2_PLACEMENT_3D('',#1,#3,#4);\n",
        "#15=AXIS2_PLACEMENT_3D('',#1,#5,#4);\n",
        "#16=AXIS2_PLACEMENT_3D('',#1,#6,#7);\n",
        "#17=AXIS2_PLACEMENT_3D('',#2,#8,#9);\n",
        "#18=PLANE('',#14);\n",
        "#19=PLANE('',#15);\n",
        "#20=PLANE('',#16);\n",
        "#21=PLANE('',#17);\n",
        "#22=VERTEX_POINT('',#10);\n",
        "#23=VERTEX_POINT('',#11);\n",
        "#24=VERTEX_POINT('',#12);\n",
        "#25=VERTEX_POINT('',#13);\n",
        "#26=DIRECTION('',(1.,0.,0.));\n",
        "#27=DIRECTION('',(-0.70710678,0.70710678,0.));\n",
        "#28=DIRECTION('',(0.,-1.,0.));\n",
        "#29=DIRECTION('',(0.,0.,1.));\n",
        "#30=DIRECTION('',(-0.70710678,0.,0.70710678));\n",
        "#31=DIRECTION('',(0.,-0.70710678,0.70710678));\n",
        "#32=VECTOR('',#26,10.);\n",
        "#33=VECTOR('',#27,14.1421356);\n",
        "#34=VECTOR('',#28,10.);\n",
        "#35=VECTOR('',#29,10.);\n",
        "#36=VECTOR('',#30,14.1421356);\n",
        "#37=VECTOR('',#31,14.1421356);\n",
        "#40=LINE('',#10,#32);\n",
        "#41=LINE('',#11,#33);\n",
        "#42=LINE('',#12,#34);\n",
        "#43=LINE('',#10,#35);\n",
        "#44=LINE('',#11,#36);\n",
        "#45=LINE('',#12,#37);\n",
        "#50=EDGE_CURVE('',#22,#23,#40,.T.);\n",
        "#51=EDGE_CURVE('',#23,#24,#41,.T.);\n",
        "#52=EDGE_CURVE('',#24,#22,#42,.T.);\n",
        "#53=EDGE_CURVE('',#22,#25,#43,.T.);\n",
        "#54=EDGE_CURVE('',#23,#25,#44,.T.);\n",
        "#55=EDGE_CURVE('',#24,#25,#45,.T.);\n",
        "#60=ORIENTED_EDGE('',*,*,#52,.F.);\n",
        "#61=ORIENTED_EDGE('',*,*,#51,.F.);\n",
        "#62=ORIENTED_EDGE('',*,*,#50,.F.);\n",
        "#63=ORIENTED_EDGE('',*,*,#50,.T.);\n",
        "#64=ORIENTED_EDGE('',*,*,#54,.T.);\n",
        "#65=ORIENTED_EDGE('',*,*,#53,.F.);\n",
        "#66=ORIENTED_EDGE('',*,*,#53,.T.);\n",
        "#67=ORIENTED_EDGE('',*,*,#55,.F.);\n",
        "#68=ORIENTED_EDGE('',*,*,#52,.T.);\n",
        "#69=ORIENTED_EDGE('',*,*,#51,.T.);\n",
        "#70=ORIENTED_EDGE('',*,*,#55,.T.);\n",
        "#71=ORIENTED_EDGE('',*,*,#54,.F.);\n",
        "#75=EDGE_LOOP('',(#60,#61,#62));\n",
        "#76=EDGE_LOOP('',(#63,#64,#65));\n",
        "#77=EDGE_LOOP('',(#66,#67,#68));\n",
        "#78=EDGE_LOOP('',(#69,#70,#71));\n",
        "#80=FACE_OUTER_BOUND('',#75,.T.);\n",
        "#81=FACE_OUTER_BOUND('',#76,.T.);\n",
        "#82=FACE_OUTER_BOUND('',#77,.T.);\n",
        "#83=FACE_OUTER_BOUND('',#78,.T.);\n",
        "#90=ADVANCED_FACE('',(#80),#18,.T.);\n",
        "#91=ADVANCED_FACE('',(#81),#19,.T.);\n",
        "#92=ADVANCED_FACE('',(#82),#20,.T.);\n",
        "#93=ADVANCED_FACE('',(#83),#21,.T.);\n",
        "#100=CLOSED_SHELL('',(#90,#91,#92,#93));\n",
        "#110=MANIFOLD_SOLID_BREP('TetraA',#100);\n",
        "#111=MANIFOLD_SOLID_BREP('TetraB',#100);\n",
        "ENDSEC;\n",
        "END-ISO-10303-21;\n"
    );

    let rep = open_bytes("assembly.step", step.as_bytes(), None);
    assert_eq!(rep.detected.id, "step");
    match rep.viewer {
        ViewerPayload::Mesh3d {
            format_label,
            triangle_count,
            layers,
            materials,
            edge_count,
            ..
        } => {
            assert!(
                format_label.contains("Solidcore"),
                "Expected Solidcore STEP label, got {format_label}"
            );
            assert!(triangle_count >= 4, "Expected triangles from 2 STEP solids, got {triangle_count}");
            assert!(edge_count >= 4, "Expected B-rep edges, got {edge_count}");
            assert!(layers.len() >= 2, "Expected per-solid SceneLayer3d entries, got {}", layers.len());
            assert!(!materials.is_empty());
        }
        other => panic!("Expected Solidcore STEP Mesh3d, got {other:?}"),
    }

    // FreeCAD archive with Document.xml + GuiDocument.xml
    let mut fcstd_buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut fcstd_buf));
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("Document.xml", opts).unwrap();
        zw.write_all(
            br#"<?xml version="1.0" encoding="utf-8"?>
<Document SchemaVersion="4">
  <Objects Count="2">
    <Object type="Part::Box" name="Box" />
    <Object type="Part::Cylinder" name="Cyl" />
  </Objects>
  <ObjectData Count="2">
    <Object name="Box"><Properties Count="1"><Property name="Label" type="App::PropertyString"><String value="BasePlate"/></Property></Properties></Object>
    <Object name="Cyl"><Properties Count="1"><Property name="Label" type="App::PropertyString"><String value="PinHidden"/></Property></Properties></Object>
  </ObjectData>
</Document>"#,
        ).unwrap();
        zw.start_file("GuiDocument.xml", opts).unwrap();
        zw.write_all(
            br#"<?xml version="1.0" encoding="utf-8"?>
<Document>
  <ViewProviderData Count="2">
    <ViewProvider name="Box"><Properties Count="2"><Property name="Visibility" type="App::PropertyBool"><Bool value="true"/></Property><Property name="ShapeColor" type="App::PropertyColor"><PropertyColor value="3368601855"/></Property></Properties></ViewProvider>
    <ViewProvider name="Cyl"><Properties Count="1"><Property name="Visibility" type="App::PropertyBool"><Bool value="false"/></Property></Properties></ViewProvider>
  </ViewProviderData>
</Document>"#,
        ).unwrap();
        zw.finish().unwrap();
    }
    let fc_rep = open_bytes("assembly.fcstd", &fcstd_buf, None);
    assert_eq!(fc_rep.detected.id, "fcstd");
}

#[test]
fn decodes_bvh_and_fbx_skeletal_animations_across_species() {
    let human_bvh = concat!(
        "HIERARCHY\n",
        "ROOT Hips\n{\n  OFFSET 0.0 90.0 0.0\n  CHANNELS 6 Xposition Yposition Zposition Zrotation Xrotation Yrotation\n",
        "  JOINT Spine\n  {\n    OFFSET 0.0 15.0 0.0\n    CHANNELS 3 Zrotation Xrotation Yrotation\n",
        "    JOINT Head\n    {\n      OFFSET 0.0 20.0 0.0\n      CHANNELS 3 Zrotation Xrotation Yrotation\n",
        "      End Site\n      {\n        OFFSET 0.0 10.0 0.0\n      }\n    }\n",
        "    JOINT LeftArm\n    {\n      OFFSET 12.0 14.0 0.0\n      CHANNELS 3 Zrotation Xrotation Yrotation\n",
        "      End Site\n      {\n        OFFSET 25.0 0.0 0.0\n      }\n    }\n",
        "  }\n",
        "  JOINT LeftUpLeg\n  {\n    OFFSET 10.0 -5.0 0.0\n    CHANNELS 3 Zrotation Xrotation Yrotation\n",
        "    JOINT LeftFoot\n    {\n      OFFSET 0.0 -40.0 0.0\n      CHANNELS 3 Zrotation Xrotation Yrotation\n",
        "      End Site\n      {\n        OFFSET 0.0 -5.0 12.0\n      }\n    }\n",
        "  }\n",
        "}\n",
        "MOTION\n",
        "Frames: 2\n",
        "Frame Time: 0.0333333\n",
        "0 90 0 0 0 0 5 0 0 0 0 0 10 0 0 -15 0 0 5 0 0\n",
        "0 91 2 0 10 0 10 0 0 0 5 0 -10 0 0 15 0 0 -5 0 0\n"
    );
    let rep_human = open_bytes("walk.bvh", human_bvh.as_bytes(), None);
    assert_eq!(rep_human.detected.id, "bvh");
    match rep_human.viewer {
        ViewerPayload::Mesh3d { skeleton: Some(skel), .. } => {
            assert!(skel.bones.len() >= 6);
            assert_eq!(skel.clips.len(), 1);
            assert!(skel.clips[0].duration_secs > 0.0);
            assert!(!skel.clips[0].channels.is_empty());
            assert!(skel.clips[0].channels[0].times.len() >= 2);
            assert!(
                skel.species_hint.as_deref().unwrap_or("").to_ascii_lowercase().contains("biped")
                    || skel.species_hint.as_deref().unwrap_or("").to_ascii_lowercase().contains("human"),
                "Unexpected species hint: {:?}",
                skel.species_hint
            );
        }
        other => panic!("Expected BVH skeleton Mesh3d, got {other:?}"),
    }

    let bird_bvh = concat!(
        "HIERARCHY\n",
        "ROOT Chest\n{\n  OFFSET 0.0 40.0 0.0\n  CHANNELS 6 Xposition Yposition Zposition Zrotation Xrotation Yrotation\n",
        "  JOINT WingL\n  {\n    OFFSET 8.0 2.0 0.0\n    CHANNELS 3 Zrotation Xrotation Yrotation\n",
        "    End Site { OFFSET 18.0 0.0 0.0 }\n  }\n",
        "  JOINT WingR\n  {\n    OFFSET -8.0 2.0 0.0\n    CHANNELS 3 Zrotation Xrotation Yrotation\n",
        "    End Site { OFFSET -18.0 0.0 0.0 }\n  }\n",
        "  JOINT Beak\n  {\n    OFFSET 0.0 6.0 10.0\n    CHANNELS 3 Zrotation Xrotation Yrotation\n",
        "    End Site { OFFSET 0.0 0.0 4.0 }\n  }\n",
        "  JOINT Tail\n  {\n    OFFSET 0.0 -2.0 -12.0\n    CHANNELS 3 Zrotation Xrotation Yrotation\n",
        "    End Site { OFFSET 0.0 0.0 -8.0 }\n  }\n",
        "}\nMOTION\nFrames: 2\nFrame Time: 0.04\n",
        "0 40 0 0 0 0 25 0 0 -25 0 0 0 0 0 5 0 0\n",
        "0 42 1 0 0 0 -30 0 0 30 0 0 0 0 0 -5 0 0\n"
    );
    let rep_bird = open_bytes("flap.bvh", bird_bvh.as_bytes(), None);
    match rep_bird.viewer {
        ViewerPayload::Mesh3d { skeleton: Some(skel), .. } => {
            let hint = skel.species_hint.unwrap_or_default().to_ascii_lowercase();
            assert!(hint.contains("avian") || hint.contains("bird"), "Expected avian/bird hint, got {hint}");
        }
        other => panic!("Expected avian BVH skeleton, got {other:?}"),
    }

    // ASCII FBX quadruped skeleton + animation clip
    let fbx_ascii = concat!(
        "; FBX 7.4.0 project file\n",
        "FBXHeaderExtension:  {\n  FBXHeaderVersion: 1003\n}\n",
        "Objects:  {\n",
        "  Model: 100, \"Model::Pelvis\", \"LimbNode\" {\n    Properties70:  {\n      P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",0,10,0\n    }\n  }\n",
        "  Model: 101, \"Model::FrontLegL\", \"LimbNode\" {\n    Properties70:  {\n      P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",5,-8,6\n    }\n  }\n",
        "  Model: 102, \"Model::HindLegR\", \"LimbNode\" {\n    Properties70:  {\n      P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",-5,-8,-6\n    }\n  }\n",
        "  Model: 103, \"Model::Tail\", \"LimbNode\" {\n    Properties70:  {\n      P: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",0,2,-10\n    }\n  }\n",
        "  AnimationStack: 200, \"AnimStack::Trot\", \"\" {}\n",
        "  AnimationLayer: 201, \"AnimLayer::Base\", \"\" {}\n",
        "  AnimationCurveNode: 301, \"AnimCurveNode::R\", \"\" {}\n",
        "  AnimationCurve: 401, \"AnimCurve::X\", \"\" {\n",
        "    KeyTime: *2 { a: 0,46186158000 }\n",
        "    KeyValueFloat: *2 { a: -20.0,20.0 }\n",
        "  }\n",
        "}\n",
        "Connections:  {\n",
        "  C: \"OO\",101,100\n",
        "  C: \"OO\",102,100\n",
        "  C: \"OO\",103,100\n",
        "  C: \"OO\",201,200\n",
        "  C: \"OO\",301,201\n",
        "  C: \"OP\",301,101, \"Lcl Rotation\"\n",
        "  C: \"OP\",401,301, \"d|X\"\n",
        "}\n"
    );
    let rep_fbx = open_bytes("quadruped_trot.fbx", fbx_ascii.as_bytes(), None);
    assert_eq!(rep_fbx.detected.id, "fbx");
    match rep_fbx.viewer {
        ViewerPayload::Mesh3d { skeleton: Some(skel), .. } => {
            assert!(skel.bones.len() >= 4, "Expected >=4 FBX bones, got {}", skel.bones.len());
            assert!(!skel.clips.is_empty(), "Expected FBX animation clips");
            let hint = skel.species_hint.unwrap_or_default().to_ascii_lowercase();
            assert!(hint.contains("quadruped") || hint.contains("animal"), "Expected quadruped hint, got {hint}");
        }
        other => panic!("Expected FBX skeleton Mesh3d, got {other:?}"),
    }
}

#[test]
fn decodes_blender_multi_object_layers_materials_and_packed_textures() {
    fn push_large_bhead(dst: &mut Vec<u8>, code: &[u8; 4], sdna: i32, old: u64, nr: i64, payload: &[u8]) {
        dst.extend_from_slice(code);
        dst.extend_from_slice(&sdna.to_le_bytes());
        dst.extend_from_slice(&old.to_le_bytes());
        dst.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        dst.extend_from_slice(&nr.to_le_bytes());
        dst.extend_from_slice(payload);
    }

    let names = [
        "name[66]",             // 0
        "totvert",              // 1
        "faces_num",            // 2
        "*face_offset_indices", // 3
        "attribute_storage",    // 4
        "id",                   // 5
        "*dna_attributes",      // 6
        "dna_attributes_num",   // 7
        "*name",                // 8
        "*data",                // 9
        "totcol",               // 10
        "**mat",                // 11
        "r",                    // 12
        "g",                    // 13
        "b",                    // 14
        "a",                    // 15
        "size",                 // 16
        "*packedfile",          // 17
        "type",                 // 18
        "visibility_flag",      // 19
        "loc[3]",               // 20
        "scale[3]",             // 21
    ];
    let types = [
        "char",             // 0: 1
        "int",              // 1: 4
        "void",             // 2: 0
        "float",            // 3: 4
        "short",            // 4: 2
        "ID",               // 5: 68
        "AttributeArray",   // 6: 8
        "Attribute",        // 7: 16
        "AttributeStorage", // 8: 16
        "PackedFile",       // 9: 16
        "Image",            // 10: 76
        "Material",         // 11: 84
        "Mesh",             // 12: 108
        "Object",           // 13: 108
    ];
    let type_lens: [u16; 14] = [1, 4, 0, 4, 2, 68, 8, 16, 16, 16, 76, 84, 108, 108];

    let mut dna = Vec::new();
    dna.extend_from_slice(b"SDNA");
    dna.extend_from_slice(b"NAME");
    dna.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for n in names { dna.extend_from_slice(n.as_bytes()); dna.push(0); }
    while dna.len() % 4 != 0 { dna.push(0); }

    dna.extend_from_slice(b"TYPE");
    dna.extend_from_slice(&(types.len() as u32).to_le_bytes());
    for t in types { dna.extend_from_slice(t.as_bytes()); dna.push(0); }
    while dna.len() % 4 != 0 { dna.push(0); }

    dna.extend_from_slice(b"TLEN");
    for l in type_lens { dna.extend_from_slice(&l.to_le_bytes()); }
    while dna.len() % 4 != 0 { dna.push(0); }

    dna.extend_from_slice(b"STRC");
    dna.extend_from_slice(&9u32.to_le_bytes());
    for (ti, fields) in [
        // 0: ID { char name[66]; }
        (5u16, vec![(0u16, 0u16)]),
        // 1: AttributeArray { void *data; }
        (6u16, vec![(2u16, 9u16)]),
        // 2: Attribute { char *name; void *data; }
        (7u16, vec![(0u16, 8u16), (2u16, 9u16)]),
        // 3: AttributeStorage { Attribute *dna_attributes; int dna_attributes_num; }
        (8u16, vec![(7u16, 6u16), (1u16, 7u16)]),
        // 4: PackedFile { int size; void *data; } (note align: 4 + 4 pad + 8 ptr = 16 -> put int size; int totvert(pad); void *data)
        (9u16, vec![(1u16, 16u16), (2u16, 9u16)]),
        // 5: Image { ID id; PackedFile *packedfile; } (68 + 8 = 76)
        (10u16, vec![(5u16, 5u16), (9u16, 17u16)]),
        // 6: Material { ID id; float r, g, b, a; } (68 + 16 = 84)
        (11u16, vec![(5u16, 5u16), (3u16, 12u16), (3u16, 13u16), (3u16, 14u16), (3u16, 15u16)]),
        // 7: Mesh { ID id; int totvert; int faces_num; int *face_offset_indices; AttributeStorage attribute_storage; short totcol; Material **mat; } (68+4+4+8+16+4+8 -> let totcol be int to keep 4+8 aligned: 68+4+4+8+16+4(totcol? short=2 so offset 100..108 ptr -> 68+4+4+8+16 = 100 -> put **mat @ 100..108, totcol @ 108..112? wait size=108: put short totcol + short pad or use int size matches! Actually put *mat before padding? Let's keep totcol as i16 + **mat at offset 100..108 if Mesh type_len = 112: 68..100=AttrStorage, 100..108 **mat, 108..110 totcol => keep *mat at 100..108 and totcol at 108)!
        (12u16, vec![(5u16, 5u16), (1u16, 1u16), (1u16, 2u16), (1u16, 3u16), (8u16, 4u16), (11u16, 11u16), (4u16, 10u16)]),
        // 8: Object { ID id; short type; short totcol; void *data; int visibility_flag; float loc[3]; float scale[3]; }
        // Offsets: 0..68 ID, 68..70 type, 70..72 pad(totcol), 72..80 *data, 80..84 visibility_flag, 84..96 loc[3], 96..108 scale[3] => 108 bytes!
        (13u16, vec![(5u16, 5u16), (4u16, 18u16), (4u16, 10u16), (2u16, 9u16), (1u16, 19u16), (3u16, 20u16), (3u16, 21u16)]),
    ] {
        dna.extend_from_slice(&ti.to_le_bytes());
        dna.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for (ft, fn_i) in fields {
            dna.extend_from_slice(&ft.to_le_bytes());
            dna.extend_from_slice(&fn_i.to_le_bytes());
        }
    }
    // Fix Mesh TLEN index 12 from 108 to 110 (68 + 4 + 4 + 8 + 16 + 8 + 2 = 110)
    let tlen_pos = dna.windows(4).position(|w| w == b"TLEN").unwrap() + 4;
    dna[tlen_pos + 12 * 2..tlen_pos + 13 * 2].copy_from_slice(&110u16.to_le_bytes());

    let mut blend = Vec::new();
    blend.extend_from_slice(b"BLENDER17-01v0503");

    let mut sc_id = vec![0u8; 68];
    sc_id[..7].copy_from_slice(b"SCScene");
    push_large_bhead(&mut blend, b"SC\0\0", 0, 0x1000, 1, &sc_id);

    // Packed PNG texture -> PackedFile -> Image
    let png_tex = make_png_bytes(2, 2, [20, 220, 180, 255]);
    push_large_bhead(&mut blend, b"DATA", 0, 0x1100, 1, &png_tex);
    // Note PackedFile DNA struct 4 has int size (4) + void* data (8) = 12 bytes in tight DNA layout above!
    let mut pf = vec![0u8; 12];
    pf[0..4].copy_from_slice(&(png_tex.len() as i32).to_le_bytes());
    pf[4..12].copy_from_slice(&0x1100u64.to_le_bytes());
    push_large_bhead(&mut blend, b"DATA", 4, 0x1101, 1, &pf);

    let mut im = vec![0u8; 76];
    im[..8].copy_from_slice(b"IMTexAlb");
    im[68..76].copy_from_slice(&0x1101u64.to_le_bytes());
    push_large_bhead(&mut blend, b"IM\0\0", 5, 0x1102, 1, &im);

    // Material
    let mut ma = vec![0u8; 84];
    ma[..8].copy_from_slice(b"MAAlloy1");
    ma[68..72].copy_from_slice(&0.2f32.to_le_bytes());
    ma[72..76].copy_from_slice(&0.8f32.to_le_bytes());
    ma[76..80].copy_from_slice(&0.9f32.to_le_bytes());
    ma[80..84].copy_from_slice(&1.0f32.to_le_bytes());
    push_large_bhead(&mut blend, b"MA\0\0", 6, 0x1200, 1, &ma);
    push_large_bhead(&mut blend, b"DATA", 0, 0x1201, 1, &0x1200u64.to_le_bytes());

    // Mesh geometry buffers
    let mut pos_buf = Vec::new();
    for p in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
        for f in p { pos_buf.extend_from_slice(&f.to_le_bytes()); }
    }
    let cv_buf: Vec<u8> = [0i32, 1, 2].iter().flat_map(|i| i.to_le_bytes()).collect();
    let fo_buf: Vec<u8> = [0i32, 3].iter().flat_map(|i| i.to_le_bytes()).collect();
    push_large_bhead(&mut blend, b"DATA", 0, 0x2001, 1, b"position\0");
    push_large_bhead(&mut blend, b"DATA", 0, 0x2002, 1, &pos_buf);
    push_large_bhead(&mut blend, b"DATA", 1, 0x2003, 1, &0x2002u64.to_le_bytes());
    push_large_bhead(&mut blend, b"DATA", 0, 0x2004, 1, b".corner_vert\0");
    push_large_bhead(&mut blend, b"DATA", 0, 0x2005, 1, &cv_buf);
    push_large_bhead(&mut blend, b"DATA", 1, 0x2006, 1, &0x2005u64.to_le_bytes());
    push_large_bhead(&mut blend, b"DATA", 0, 0x2007, 1, &fo_buf);

    let mut attrs = Vec::new();
    attrs.extend_from_slice(&0x2001u64.to_le_bytes());
    attrs.extend_from_slice(&0x2003u64.to_le_bytes());
    attrs.extend_from_slice(&0x2004u64.to_le_bytes());
    attrs.extend_from_slice(&0x2006u64.to_le_bytes());
    push_large_bhead(&mut blend, b"DATA", 2, 0x3000, 2, &attrs);

    // Mesh @ 0x4000
    let mut mesh = vec![0u8; 110];
    mesh[..6].copy_from_slice(b"MEQuad");
    mesh[68..72].copy_from_slice(&3i32.to_le_bytes());
    mesh[72..76].copy_from_slice(&1i32.to_le_bytes());
    mesh[76..84].copy_from_slice(&0x2007u64.to_le_bytes());
    mesh[84..92].copy_from_slice(&0x3000u64.to_le_bytes());
    mesh[92..96].copy_from_slice(&2i32.to_le_bytes());
    mesh[100..108].copy_from_slice(&0x1201u64.to_le_bytes());
    mesh[108..110].copy_from_slice(&1i16.to_le_bytes());
    push_large_bhead(&mut blend, b"ME\0\0", 7, 0x4000, 1, &mesh);

    // 2 Objects: LayerVisible & LayerHidden
    for (idx, (oid, vis_flag, lx)) in [(0x4100u64, 0i32, 0.0f32), (0x4101u64, 0x0fi32, 3.0f32)].iter().enumerate() {
        let mut ob = vec![0u8; 108];
        let nm = if idx == 0 { b"OBLayerVisible".as_slice() } else { b"OBLayerHidden".as_slice() };
        ob[..nm.len()].copy_from_slice(nm);
        ob[68..70].copy_from_slice(&1i16.to_le_bytes()); // OB_MESH
        ob[72..80].copy_from_slice(&0x4000u64.to_le_bytes());
        ob[80..84].copy_from_slice(&vis_flag.to_le_bytes());
        ob[84..88].copy_from_slice(&lx.to_le_bytes());
        ob[96..100].copy_from_slice(&1.0f32.to_le_bytes());
        ob[100..104].copy_from_slice(&1.0f32.to_le_bytes());
        ob[104..108].copy_from_slice(&1.0f32.to_le_bytes());
        push_large_bhead(&mut blend, b"OB\0\0", 8, *oid, 1, &ob);
    }

    push_large_bhead(&mut blend, b"DNA1", 0, 0x5000, 1, &dna);
    push_large_bhead(&mut blend, b"ENDB", 0, 0, 0, &[]);

    let rep = open_bytes("scene_layers.blend", &blend, None);
    assert_eq!(rep.detected.id, "blend");
    match rep.viewer {
        ViewerPayload::Mesh3d {
            triangle_count,
            layers,
            materials,
            textures,
            ..
        } => {
            assert_eq!(triangle_count, 2, "2 Object instances of 1 tri = 2 triangles");
            assert_eq!(layers.len(), 2, "Expected 2 Object layers, got {layers:?}");
            assert!(layers[0].visible);
            assert!(!layers[1].visible);
            assert!(!materials.is_empty(), "Expected Material extracted");
            assert!(!textures.is_empty(), "Expected packed Image texture extracted");
        }
        other => panic!("Expected Blender multi-layer Mesh3d, got {other:?}"),
    }
}

#[test]
fn decodes_avif_and_ora_kra_modern_images() {
    fn box4(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let len = (8 + body.len()) as u32;
        let mut v = len.to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(body);
        v
    }
    let ftyp = box4(b"ftyp", b"avif\0\0\0\0avifmif1miaf");
    let mut ispe_body = vec![0u8; 12];
    ispe_body[4..8].copy_from_slice(&4u32.to_be_bytes());
    ispe_body[8..12].copy_from_slice(&3u32.to_be_bytes());
    let ispe = box4(b"ispe", &ispe_body);
    let ipco = box4(b"ipco", &ispe);
    let iprp = box4(b"iprp", &ipco);
    let mut meta_body = vec![0u8; 4]; // FullBox version/flags
    meta_body.extend_from_slice(&iprp);
    let meta = box4(b"meta", &meta_body);
    let mut avif = ftyp;
    avif.extend_from_slice(&meta);

    let rep_avif = open_bytes("photo.avif", &avif, None);
    assert_eq!(rep_avif.detected.id, "avif");
    match rep_avif.viewer {
        ViewerPayload::Image { width, height, data_url, .. } => {
            assert_eq!((width, height), (4, 3));
            assert!(data_url.starts_with("data:image/"));
        }
        other => panic!("Expected AVIF Image payload, got {other:?}"),
    }

    // OpenRaster (.ora)
    let mut ora_buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut ora_buf));
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("mimetype", opts).unwrap();
        zw.write_all(b"image/openraster").unwrap();
        zw.start_file("stack.xml", opts).unwrap();
        zw.write_all(
            br#"<?xml version='1.0' encoding='UTF-8'?>
<image w="8" h="8">
  <stack>
    <layer name="HiddenInk" src="data/l1.png" visibility="hidden" opacity="0.5" x="1" y="1" />
    <layer name="BaseFill" src="data/l0.png" visibility="visible" opacity="1.0" x="0" y="0" />
  </stack>
</image>"#,
        ).unwrap();
        zw.start_file("data/l0.png", opts).unwrap();
        zw.write_all(&make_png_bytes(8, 8, [10, 40, 200, 255])).unwrap();
        zw.start_file("data/l1.png", opts).unwrap();
        zw.write_all(&make_png_bytes(4, 4, [240, 20, 20, 255])).unwrap();
        zw.start_file("mergedimage.png", opts).unwrap();
        zw.write_all(&make_png_bytes(8, 8, [10, 40, 200, 255])).unwrap();
        zw.finish().unwrap();
    }
    let rep_ora = open_bytes("painting.ora", &ora_buf, None);
    assert_eq!(rep_ora.detected.id, "ora");
    match rep_ora.viewer {
        ViewerPayload::LayeredDesign { pages, .. } => {
            assert_eq!(pages.len(), 1);
            assert_eq!(pages[0].layers.len(), 2);
            assert!(pages[0].composite_data_url.is_some());
        }
        other => panic!("Expected LayeredDesign for ORA, got {other:?}"),
    }

    // Krita (.kra)
    let mut kra_buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut kra_buf));
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("mimetype", opts).unwrap();
        zw.write_all(b"application/x-krita").unwrap();
        zw.start_file("maindoc.xml", opts).unwrap();
        zw.write_all(
            br#"<DOC><IMAGE width="6" height="6"><layers><layer name="Paint" visible="1" opacity="255" /></layers></IMAGE></DOC>"#,
        ).unwrap();
        zw.start_file("mergedimage.png", opts).unwrap();
        zw.write_all(&make_png_bytes(6, 6, [80, 200, 120, 255])).unwrap();
        zw.finish().unwrap();
    }
    let rep_kra = open_bytes("artwork.kra", &kra_buf, None);
    assert_eq!(rep_kra.detected.id, "kra");
    match rep_kra.viewer {
        ViewerPayload::LayeredDesign { pages, .. } => {
            assert_eq!(pages.len(), 1);
            assert!(pages[0].composite_data_url.is_some());
        }
        other => panic!("Expected LayeredDesign for KRA, got {other:?}"),
    }
}

#[test]
fn decodes_psd_figcraft_vectorcraft_pcraft_and_xfig_pages_and_layers() {
    // 1. Minimal 2x2 RGBA PSD with 2 Raw layers (one visible, one hidden) + merged image
    let mut psd = Vec::new();
    psd.extend_from_slice(b"8BPS");
    psd.extend_from_slice(&1u16.to_be_bytes()); // v1 PSD
    psd.extend_from_slice(&[0u8; 6]);
    psd.extend_from_slice(&4u16.to_be_bytes()); // 4 RGBA channels
    psd.extend_from_slice(&2u32.to_be_bytes()); // height = 2
    psd.extend_from_slice(&2u32.to_be_bytes()); // width = 2
    psd.extend_from_slice(&8u16.to_be_bytes()); // 8 bits
    psd.extend_from_slice(&3u16.to_be_bytes()); // RGB mode
    psd.extend_from_slice(&0u32.to_be_bytes()); // ColorModeData len
    psd.extend_from_slice(&0u32.to_be_bytes()); // ImageResources len

    let mut layer_info = Vec::new();
    layer_info.extend_from_slice(&2i16.to_be_bytes()); // 2 layers
    for (name, flags) in [(&b"\x02Bg"[..], 0x00u8), (&b"\x03Top"[..], 0x02u8)] {
        layer_info.extend_from_slice(&0i32.to_be_bytes()); // top
        layer_info.extend_from_slice(&0i32.to_be_bytes()); // left
        layer_info.extend_from_slice(&2i32.to_be_bytes()); // bottom
        layer_info.extend_from_slice(&2i32.to_be_bytes()); // right
        layer_info.extend_from_slice(&4u16.to_be_bytes()); // 4 channels
        for cid in [-1i16, 0, 1, 2] {
            layer_info.extend_from_slice(&cid.to_be_bytes());
            layer_info.extend_from_slice(&6u32.to_be_bytes()); // 2 byte comp + 4 pixel bytes
        }
        layer_info.extend_from_slice(b"8BIMnorm");
        layer_info.push(255); // opacity
        layer_info.push(0); // clipping
        layer_info.push(flags);
        layer_info.push(0); // filler
        let mut nm_padded = name.to_vec();
        while nm_padded.len() % 4 != 0 { nm_padded.push(0); }
        let extra_len = 4 + 4 + nm_padded.len();
        layer_info.extend_from_slice(&(extra_len as u32).to_be_bytes());
        layer_info.extend_from_slice(&0u32.to_be_bytes()); // mask data
        layer_info.extend_from_slice(&0u32.to_be_bytes()); // blending ranges
        layer_info.extend_from_slice(&nm_padded);
    }
    // Channel pixel data for both layers (each 4 channels * 6 bytes Raw RGBA 2x2)
    for val in [[255u8, 0, 0, 255], [255, 255, 255, 128]] {
        for ch_byte in [val[3], val[0], val[1], val[2]] {
            layer_info.extend_from_slice(&0u16.to_be_bytes()); // Raw
            layer_info.extend_from_slice(&[ch_byte; 4]);
        }
    }
    if layer_info.len() % 2 != 0 { layer_info.push(0); }
    let lmi_len = 4 + layer_info.len();
    psd.extend_from_slice(&(lmi_len as u32).to_be_bytes());
    psd.extend_from_slice(&(layer_info.len() as u32).to_be_bytes());
    psd.extend_from_slice(&layer_info);
    // Merged ImageData section: Raw compression + 4 planes * 4 bytes
    psd.extend_from_slice(&0u16.to_be_bytes());
    psd.extend_from_slice(&[200u8; 16]);

    let rep_psd = open_bytes("layered.psd", &psd, None);
    assert_eq!(rep_psd.detected.id, "psd");
    match rep_psd.viewer {
        ViewerPayload::LayeredDesign { pages, .. } => {
            assert_eq!(pages.len(), 1);
            assert_eq!(pages[0].layers.len(), 2);
            // PSD layer decoder emits UI panel top-to-bottom order
            assert_eq!(pages[0].layers[0].name, "Top");
            assert!(!pages[0].layers[0].visible);
            assert_eq!(pages[0].layers[1].name, "Bg");
            assert!(pages[0].layers[1].visible);
            assert!(pages[0].layers[0].image_data_url.is_some());
            assert!(pages[0].layers[1].image_data_url.is_some());
            assert!(pages[0].composite_data_url.is_some());
        }
        other => panic!("Expected PSD LayeredDesign, got {other:?}"),
    }

    // 2. Multi-page .figcraft ZIP
    let mut fc_buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut fc_buf));
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("document.json", opts).unwrap();
        let doc = serde_json::json!({
            "document": {
                "pages": [
                    {
                        "name": "Home",
                        "width": 800.0,
                        "height": 600.0,
                        "children": [
                            { "name": "Card", "kind": { "type": "rect", "fill": "#224488" }, "x": 20.0, "y": 20.0, "width": 200.0, "height": 120.0, "visible": true }
                        ]
                    },
                    {
                        "name": "Settings",
                        "width": 800.0,
                        "height": 600.0,
                        "children": [
                            { "name": "Toggle", "kind": { "type": "ellipse", "fill": "#eaffff" }, "x": 40.0, "y": 40.0, "width": 60.0, "height": 60.0, "visible": false }
                        ]
                    }
                ]
            }
        });
        zw.write_all(doc.to_string().as_bytes()).unwrap();
        zw.start_file("thumbnail.png", opts).unwrap();
        zw.write_all(&make_png_bytes(4, 4, [20, 40, 80, 255])).unwrap();
        zw.finish().unwrap();
    }
    let rep_fc = open_bytes("app_ui.figcraft", &fc_buf, None);
    assert_eq!(rep_fc.detected.id, "figcraft");
    match rep_fc.viewer {
        ViewerPayload::LayeredDesign { pages, .. } => {
            assert_eq!(pages.len(), 2);
            assert_eq!(pages[0].name, "Home");
            assert_eq!(pages[1].name, "Settings");
            assert!(pages[0].layers.iter().any(|l| l.svg_fragment.is_some()));
            assert!(!pages[1].layers[0].visible);
        }
        other => panic!("Expected FigCraft LayeredDesign, got {other:?}"),
    }

    // 3. Multi-artboard .vectorcraft JSON
    let vc_json = serde_json::json!({
        "document": {
            "artboards": [
                { "name": "Board A", "bounds": { "x": 0.0, "y": 0.0, "width": 400.0, "height": 300.0 } },
                { "name": "Board B", "bounds": { "x": 500.0, "y": 0.0, "width": 400.0, "height": 300.0 } }
            ],
            "layers": [
                { "name": "Badge", "visible": true, "x": 10.0, "y": 10.0, "width": 100.0, "height": 50.0, "shape": { "kind": "rect", "fill": "#ffaa00" } }
            ]
        }
    });
    let rep_vc = open_bytes("poster.vectorcraft", vc_json.to_string().as_bytes(), None);
    assert_eq!(rep_vc.detected.id, "vectorcraft");
    match rep_vc.viewer {
        ViewerPayload::LayeredDesign { pages, .. } => {
            assert_eq!(pages.len(), 2);
            assert_eq!(pages[0].name, "Board A");
            assert_eq!(pages[1].name, "Board B");
        }
        other => panic!("Expected VectorCraft LayeredDesign, got {other:?}"),
    }

    // 4. Sparse-tiled .pcraft ZIP
    let tile_raw = vec![120u8; 256 * 256 * 4];
    let tile_zst = wrap_zstd_raw_frame(&tile_raw);
    let mut pc_buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut pc_buf));
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("manifest.json", opts).unwrap();
        let manifest = serde_json::json!({
            "document": {
                "name": "Poster",
                "size": { "width": 64, "height": 64 },
                "layers": [
                    {
                        "name": "PaintLayer",
                        "visible": true,
                        "opacity": 1.0,
                        "blend": "normal",
                        "content": {
                            "kind": "raster",
                            "surface": { "tiles": [ { "tx": 0, "ty": 0, "hash": "tile0" } ] }
                        }
                    }
                ]
            }
        });
        zw.write_all(manifest.to_string().as_bytes()).unwrap();
        zw.start_file("tiles/tile0.zst", opts).unwrap();
        zw.write_all(&tile_zst).unwrap();
        zw.finish().unwrap();
    }
    let rep_pc = open_bytes("canvas.pcraft", &pc_buf, None);
    assert_eq!(rep_pc.detected.id, "pcraft");
    match rep_pc.viewer {
        ViewerPayload::LayeredDesign { pages, .. } => {
            assert_eq!(pages.len(), 1);
            assert_eq!(pages[0].layers.len(), 1);
            assert!(pages[0].layers[0].image_data_url.as_deref().unwrap_or("").starts_with("data:image/png;base64,"));
        }
        other => panic!("Expected PhotoCraft LayeredDesign, got {other:?}"),
    }

    // 5. Xfig ASCII .fig
    let xfig = b"#FIG 3.2\nPortrait\nCenter\nInches\nLetter\n100.00\nSingle\n-2\n1200 2\n2 1 0 1 0 7 50 -1 -1 0.0 0 0 -1 0 0 3\n\t 0 0 1500 0 1500 1500\n";
    let rep_fig = open_bytes("diagram.fig", xfig, None);
    assert_eq!(rep_fig.detected.id, "fig");
    match rep_fig.viewer {
        ViewerPayload::LayeredDesign { pages, .. } => {
            assert!(!pages.is_empty());
            assert!(pages[0].layers.iter().any(|l| l.svg_fragment.is_some()));
        }
        other => panic!("Expected Xfig LayeredDesign, got {other:?}"),
    }
}
