/* tslint:disable */
/* eslint-disable */

/**
 * Generates a full UFO diagnostic JSON export for Re-Save.
 */
export function ufo_export_report_json(file_name: string, bytes: Uint8Array | number[]): Uint8Array;

/**
 * Extracts raw bytes for an entry inside a ZIP/TAR/TAR.GZ/TAR.ZST/ISO/AR archive.
 */
export function ufo_extract_archive_entry(bytes: Uint8Array | number[], entry_path: string): Uint8Array;

/**
 * Returns a paged 16-byte hex window at `offset` as JSON.
 */
export function ufo_hex_page(bytes: Uint8Array | number[], offset: bigint, length: number): string;

/**
 * Opens any file byte slice inside WebAssembly and returns a JSON-serialized OpenFileReport.
 */
export function ufo_open_bytes(file_name: string, bytes: Uint8Array | number[], total_size_hint?: bigint | null): string;

/**
 * Returns the complete 400+ supported format registry and 23 categories.
 */
export function ufo_supported_formats_json(): string;

/**
 * Returns a syntax-highlighted line window for large text/code/log files as JSON.
 */
export function ufo_text_page(bytes: Uint8Array | number[], start_line: number, line_count: number, lang: string): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly ufo_export_report_json: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly ufo_extract_archive_entry: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly ufo_hex_page: (a: number, b: number, c: number, d: bigint, e: number) => void;
    readonly ufo_open_bytes: (a: number, b: number, c: number, d: number, e: number, f: number, g: bigint) => void;
    readonly ufo_supported_formats_json: (a: number) => void;
    readonly ufo_text_page: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => void;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_add_to_stack_pointer: (a: number) => number;
    readonly __wbindgen_export: (a: number, b: number) => number;
    readonly __wbindgen_export2: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_export3: (a: number, b: number, c: number) => void;
    readonly __wbindgen_export4: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
