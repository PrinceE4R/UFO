// Non-blocking Web Worker hosting ufo-wasm for instant UI responsiveness in browsers.
import init, {
  ufo_export_report_json,
  ufo_extract_archive_entry,
  ufo_hex_page,
  ufo_open_bytes,
  ufo_supported_formats_json,
  ufo_text_page,
} from "./pkg/ufo_wasm.js";

let ready = init();

self.onmessage = async (e) => {
  const { id, action, payload } = e.data;
  try {
    await ready;
    let result;
    switch (action) {
      case "catalog":
        result = JSON.parse(ufo_supported_formats_json());
        break;
      case "open": {
        const t0 = performance.now();
        const json = ufo_open_bytes(payload.fileName, payload.bytes, payload.totalSizeHint);
        const report = JSON.parse(json);
        report.parse_time_us = Math.round((performance.now() - t0) * 1000);
        result = report;
        break;
      }
      case "hex":
        result = JSON.parse(ufo_hex_page(payload.bytes, BigInt(payload.offset), payload.length));
        break;
      case "text":
        result = JSON.parse(ufo_text_page(payload.bytes, payload.startLine, payload.lineCount, payload.lang));
        break;
      case "extract":
        result = ufo_extract_archive_entry(payload.bytes, payload.entryPath);
        break;
      case "export":
        result = ufo_export_report_json(payload.fileName, payload.bytes);
        break;
      default:
        throw new Error(`Unknown worker action: ${action}`);
    }
    self.postMessage({ id, ok: true, result });
  } catch (err) {
    self.postMessage({ id, ok: false, error: String(err) });
  }
};
