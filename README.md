# Universal File Opener (UFO)

A lightning-fast, cross-platform universal file opener built primarily in **Rust** (`ufo-core`, `ufo-wasm`, `ufo-cli`, `src-tauri`), compiling for **Windows, macOS, Linux, Android, iOS, and Web** with a unified responsive UI optimized simultaneously for desktop and mobile.

## Architecture

- **`crates/ufo-core`** (100% Pure Rust, zero C dependencies):
  - **420+ Registered Extensions** across all **23 categories** + **17+ Ambiguous Extension Disambiguation Rules**.
  - **Magic-Byte & Container Inspection**: Verifies actual headers/containers before trusting extensions (e.g., Wavefront `.obj` vs COFF `.obj`, TypeScript `.ts` vs MPEG-TS, spoofed extensions, ZIP container classification for DOCX/XLSX/PPTX/APK/IPA/EPUB/3MF/FCStd).
  - **Native Decoders**:
    - **Raster & Photography**: PNG, JPEG, WebP, GIF, BMP, TIFF, ICO/CUR, ICNS, TGA, PCX, Netpbm, QOI, OpenEXR HDR, Radiance HDR, PSD/PSB, XPM, SVG/SVGZ, and Camera RAW embedded previews (`CR2`, `CR3`, `NEF`, `ARW`, `DNG`, `ORF`, `RW2`, `RAF`, `PEF`, `SRW`, `3FR`).
    - **3D & CAD**: STL (binary/ASCII), OBJ, PLY, glTF/GLB, 3MF, ISO 10303-21 STEP (`cadmpeg-codec-step`), Rhino 3DM (`cadmpeg-codec-rhino`), FreeCAD FCStd (`cadmpeg-codec-freecad`), DXF, G-Code toolpaths, RS-274X Gerber & KiCad PCB/schematics, FBX node summaries.
    - **Documents & Office**: PDF (visual + pure-Rust stream text extractor), DOCX, XLSX/XLSM, PPTX, ODT/ODS/ODP, EPUB, FB2, XPS/OXPS, OLE2/CFB (`DOC`, `XLS`, `PPT`, `MSG`, `MSI`), RTF, EML/VCF/ICS.
    - **Databases & Scientific/AI**: Pure-Rust SQLite 3 B-tree table/row reader, SafeTensors, GGUF, NumPy `.npy`, GeoJSON/GPX/KML map vector SVGs, Parquet footer metadata, CSV/TSV auto-delimiter tables.
    - **Audio, Video & Subtitles**: WAV (with PCM waveform), MP3 (ID3 + frame telemetry), FLAC (with picture extraction), Ogg/Opus/Vorbis, MP4/MOV/M4A ISO-BMFF box inspection, MKV/WebM EBML inspection, MIDI piano-roll SVG, SRT/VTT/ASS/LRC cues.
    - **Fonts**: TTF, OTF, TTC, WOFF 1.0/2.0 (`ttf-parser`) glyph outlines & live specimen waterfall.
    - **Archives & Disk Images**: ZIP, JAR, APK, IPA, TAR, TAR.GZ, TAR.ZST, GZ, ZST, Brotli, ISO-9660, AR/DEB, 7z/RAR headers — with interactive nested file extraction.
    - **Executables & Universal Fallback**: PE32/PE32+, ELF32/ELF64, Mach-O/Fat, WebAssembly `.wasm`, Android `.dex`, PCAP/PCAPNG, UF2, `.lnk`, `.torrent`, ASN.1 DER, plus paged Hex + ASCII + Strings + Shannon Entropy for any unknown binary.
- **`crates/ufo-wasm`**: `wasm-bindgen` bindings exposing `ufo-core` to browsers at native-near speed.
- **`src-tauri`**: Tauri v2 desktop & mobile application using zero-copy `memmap2` file mapping on native paths.
- **`crates/ufo-cli`**: Fast parallel CLI inspector.
- **`frontend/`**: Responsive high-contrast `#000000` / `#0F1113` / `#EAFFFF` brutalist UI with bundled Bootstrap Icons and custom WebGL2 3D/CAD viewport.

## Compiling for All 6 Targets

### 1. Web (Static + WASM)
```powershell
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130
.\scripts\build-web.ps1
# Serve frontend/ with any static HTTP server
```

### 2. Windows (x86_64 / aarch64)
```powershell
cargo build -p ufo-app --release
# Or with Tauri bundler:
cargo tauri build
```

### 3. macOS (Universal / Apple Silicon / Intel)
```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo tauri build --target universal-apple-darwin
```

### 4. Linux (x86_64 / aarch64)
```bash
cargo tauri build
```

### 5. Android (APK / AAB)
```bash
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo tauri android init
cargo tauri android build
```

### 6. iOS (iPhone / iPad IPA — requires macOS + Xcode)
```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
cargo tauri ios init
cargo tauri ios build
```

### CLI & Tests
```powershell
cargo test --workspace
cargo run -p ufo-cli -- path/to/any/file
```
