use serde::{Deserialize, Serialize};

/// All 23 domain categories from File_Formats_that_it_must_open.txt plus Universal Binary fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FormatCategory {
    Document,
    RasterImage,
    VectorDesign,
    Model3D,
    CadBim,
    FabricationCnc,
    Audio,
    VideoSubtitles,
    SourceCode,
    ExecutableLib,
    WebData,
    SpreadsheetDbSci,
    FontTypography,
    OfficePresentation,
    OperatingSystem,
    LinuxFirmwareDisk,
    ArchivePackage,
    ElectronicsPcb,
    GameDevModding,
    NetworkingSecurity,
    MobileDev,
    BuildMetadata,
    GenericMisc,
    UnknownBinary,
}

impl FormatCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Document => "Documents, Text & E-Books",
            Self::RasterImage => "Raster Images & Photography",
            Self::VectorDesign => "Vector Graphics & Design",
            Self::Model3D => "3D Models, Meshes & Animation",
            Self::CadBim => "CAD, Engineering & BIM",
            Self::FabricationCnc => "3D Printing, CNC & Fabrication",
            Self::Audio => "Audio, Music & Sound",
            Self::VideoSubtitles => "Video, Film & Subtitles",
            Self::SourceCode => "Programming & Source Code",
            Self::ExecutableLib => "Compiled Code & Executables",
            Self::WebData => "Web Development & Config",
            Self::SpreadsheetDbSci => "Spreadsheets, Databases & Scientific",
            Self::FontTypography => "Fonts & Typography",
            Self::OfficePresentation => "Office, Presentations & Publishing",
            Self::OperatingSystem => "OS & Windows System Files",
            Self::LinuxFirmwareDisk => "Linux, Unix, Firmware & Disk Images",
            Self::ArchivePackage => "Archives, Compression & Packages",
            Self::ElectronicsPcb => "Electronics, PCB & Embedded",
            Self::GameDevModding => "Game Development & Modding",
            Self::NetworkingSecurity => "Networking, Security & Certificates",
            Self::MobileDev => "Android & Apple Development",
            Self::BuildMetadata => "Developer Tools & Build Metadata",
            Self::GenericMisc => "Generic & Miscellaneous",
            Self::UnknownBinary => "Binary / Unclassified Data",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectionConfidence {
    SignatureVerified,
    StructureParsed,
    AmbiguityResolved,
    ExtensionHeuristic,
    TextHeuristic,
    BinaryFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmbiguityNotice {
    pub extension: String,
    pub candidates: Vec<String>,
    pub resolved_meaning: String,
    pub extension_mismatch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedFormat {
    pub id: String,
    pub name: String,
    pub category: FormatCategory,
    pub mime: String,
    pub confidence: DetectionConfidence,
    pub detection_basis: String,
    pub magic_hex: String,
    pub ambiguity: Option<AmbiguityNotice>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataEntry {
    pub section: String,
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHashes {
    pub crc32: String,
    pub sha256: String,
    pub sha1: String,
    pub entropy_bits: f32,
    pub printable_ratio: f32,
    pub histogram_64: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HexRow {
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub ascii: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedString {
    pub offset: u64,
    pub encoding: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextToken {
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HighlightedLine {
    pub line_no: usize,
    pub tokens: Vec<TextToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotebookCell {
    pub cell_type: String,
    pub execution_count: Option<u64>,
    pub source: String,
    pub outputs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSheet {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub total_rows: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocSection {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub compressed_size: u64,
    pub method: String,
    pub crc32: Option<u32>,
    pub previewable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinarySection {
    pub name: String,
    pub virtual_addr: String,
    pub raw_offset: String,
    pub size: u64,
    pub flags: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeNode {
    pub key: String,
    pub value_preview: String,
    pub kind: String,
    pub depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Audio,
    Video,
    Midi,
    Subtitle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleCue {
    pub index: usize,
    pub start: String,
    pub end: String,
    pub text: String,
}

/// Draw batch for multi-material / textured 3D meshes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshBatch {
    pub start_vertex: u32,
    pub vertex_count: u32,
    pub base_color: [f32; 4],
    pub texture_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneLayer3d {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub visible: bool,
    pub parent_id: Option<String>,
    pub material_names: Vec<String>,
    pub texture_names: Vec<String>,
    pub vertex_count: usize,
    pub triangle_count: usize,
    pub batch_indices: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SceneMaterialInfo {
    pub name: String,
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    pub texture_name: Option<String>,
    pub texture_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkeletonBone {
    pub id: usize,
    pub name: String,
    pub parent: Option<usize>,
    pub rest_local_pos: [f32; 3],
    pub rest_local_rot: [f32; 4],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoneChannel {
    pub bone_index: usize,
    pub times: Vec<f32>,
    pub translations: Vec<[f32; 3]>,
    pub rotations: Vec<[f32; 4]>,
    pub scales: Vec<[f32; 3]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationClip {
    pub name: String,
    pub duration_secs: f32,
    pub channels: Vec<BoneChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkeletonAnimation {
    pub species_hint: Option<String>,
    pub bones: Vec<SkeletonBone>,
    pub clips: Vec<AnimationClip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesignLayer {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub depth: usize,
    pub visible: bool,
    pub opacity: f32,
    pub blend_mode: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub svg_fragment: Option<String>,
    pub image_data_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesignPage {
    pub id: String,
    pub name: String,
    pub width: f64,
    pub height: f64,
    pub background: Option<String>,
    pub composite_data_url: Option<String>,
    pub layers: Vec<DesignLayer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ViewerPayload {
    Image {
        width: u32,
        height: u32,
        channels: String,
        bit_depth: u8,
        data_url: String,
        frames: usize,
        note: Option<String>,
    },
    Svg {
        svg_markup: String,
        width: Option<String>,
        height: Option<String>,
        view_box: Option<String>,
        source_text: String,
        use_stream_url: bool,
    },
    Text {
        language: String,
        total_lines: usize,
        total_bytes: u64,
        encoding: String,
        has_bom: bool,
        line_ending: String,
        lines: Vec<HighlightedLine>,
        truncated: bool,
        markdown_html: Option<String>,
        raw_html: Option<String>,
        notebook_cells: Option<Vec<NotebookCell>>,
    },
    Table {
        format_label: String,
        sheets: Vec<TableSheet>,
        active_sheet: usize,
    },
    Document {
        doc_type: String,
        title: Option<String>,
        page_count: usize,
        use_native_pdf_blob: bool,
        sections: Vec<DocSection>,
        extracted_text: String,
    },
    Archive {
        archive_type: String,
        total_entries: usize,
        total_uncompressed: u64,
        entries: Vec<ArchiveEntry>,
    },
    Mesh3d {
        format_label: String,
        vertex_count: usize,
        triangle_count: usize,
        edge_count: usize,
        point_count: usize,
        is_gaussian_splat: bool,
        bounds_min: [f32; 3],
        bounds_max: [f32; 3],
        /// Compact little-endian f32 buffers encoded as base64 for multi-MB speed.
        positions_b64: String,
        normals_b64: String,
        uvs_b64: String,
        colors_b64: String,
        wireframe_b64: String,
        point_positions_b64: String,
        point_colors_b64: String,
        textures: Vec<String>,
        batches: Vec<MeshBatch>,
        parts: Vec<String>,
        cad_summary: Option<String>,
        #[serde(default)]
        layers: Vec<SceneLayer3d>,
        #[serde(default)]
        materials: Vec<SceneMaterialInfo>,
        #[serde(default)]
        skeleton: Option<SkeletonAnimation>,
    },
    LayeredDesign {
        format_label: String,
        pages: Vec<DesignPage>,
        active_page: usize,
        note: Option<String>,
    },
    Vector2d {
        format_label: String,
        width: f64,
        height: f64,
        layers: Vec<String>,
        svg_markup: String,
        entity_count: usize,
    },
    Media {
        media_kind: MediaKind,
        container: String,
        codec_summary: String,
        duration_secs: Option<f64>,
        sample_rate: Option<u32>,
        channels: Option<u16>,
        dimensions: Option<(u32, u32)>,
        use_native_player: bool,
        waveform_peaks: Vec<f32>,
        svg_preview: Option<String>,
        subtitles: Vec<SubtitleCue>,
        cover_art_data_url: Option<String>,
    },
    Font {
        family: String,
        subfamily: String,
        glyph_count: usize,
        upem: u16,
        ascender: i16,
        descender: i16,
        tables: Vec<String>,
        preview_glyphs_svg: String,
        supports_web_font: bool,
    },
    Binary {
        arch_or_format: String,
        endianness: String,
        entry_point: Option<String>,
        sections: Vec<BinarySection>,
        imports: Vec<String>,
        exports: Vec<String>,
        strings: Vec<ExtractedString>,
    },
    Structured {
        format_label: String,
        formatted_text: String,
        tree_nodes: Vec<TreeNode>,
    },
    HexOnly {
        note: String,
        strings: Vec<ExtractedString>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenFileReport {
    pub file_name: String,
    pub file_size: u64,
    pub extension: Option<String>,
    pub detected: DetectedFormat,
    pub hashes: FileHashes,
    pub metadata: Vec<MetadataEntry>,
    pub parse_time_us: u64,
    pub viewer: ViewerPayload,
    pub hex_preview: Vec<HexRow>,
    pub preview_strings: Vec<ExtractedString>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HexPage {
    pub offset: u64,
    pub total_bytes: u64,
    pub rows: Vec<HexRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextPage {
    pub start_line: usize,
    pub total_lines: usize,
    pub lines: Vec<HighlightedLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    pub suggested_name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}
