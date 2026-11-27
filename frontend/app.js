import { mountMeshViewer } from "./viewer3d.js";

const Tauri = window.__TAURI__?.core?.invoke ? window.__TAURI__ : null;

// Non-blocking Web Worker RPC for browser WASM execution
let worker = null;
let workerReqId = 1;
const workerPending = new Map();

function callWorker(action, payload) {
  if (!worker) {
    worker = new Worker("./worker.js", { type: "module" });
    worker.onmessage = (e) => {
      const { id, ok, result, error } = e.data;
      const entry = workerPending.get(id);
      if (!entry) return;
      workerPending.delete(id);
      if (ok) entry.resolve(result);
      else entry.reject(new Error(error));
    };
  }
  return new Promise((resolve, reject) => {
    const id = workerReqId++;
    workerPending.set(id, { resolve, reject });
    worker.postMessage({ id, action, payload });
  });
}

const state = {
  tabs: [], // { id, sessionId, streamUrl, assetBaseUrl, fileName, report, bytes, mode, cleanup, blobUrl, hexOffset, loadedLines, mdRaw, htmlRaw, activeSheet }
  activeTabId: null,
  infoOpen: false,
  isFullscreen: false,
  theme: localStorage.getItem("ufo-theme") === "light" ? "light" : "dark",
  isMobile: window.innerWidth <= 768,
  supportsWindowChrome: Boolean(Tauri),
  formatsCatalog: [],
  localAssets: new Map(), // normalized relative path or basename -> object URL (browser multi-file / folder fallback)
  nextLocalId: 1,
};

function registerBrowserAsset(relPath, file) {
  if (!file || !file.name) return;
  const cleanRel = String(relPath || file.webkitRelativePath || file.name)
    .replace(/\\/g, "/")
    .replace(/^\.\//, "")
    .toLowerCase();
  const base = file.name.toLowerCase();
  if (state.localAssets.has(cleanRel)) return;
  const url = URL.createObjectURL(file);
  state.localAssets.set(cleanRel, url);
  if (!state.localAssets.has(base)) {
    state.localAssets.set(base, url);
  }
}

async function readDroppedEntries(dataTransfer) {
  const items = Array.from(dataTransfer?.items || []);
  const entries = items
    .map((it) => (typeof it.webkitGetAsEntry === "function" ? it.webkitGetAsEntry() : null))
    .filter(Boolean);
  if (entries.length === 0) {
    return Array.from(dataTransfer?.files || []).map((f) => ({
      file: f,
      relPath: f.webkitRelativePath || f.name,
    }));
  }
  const out = [];
  const walk = async (entry, prefix) => {
    if (entry.isFile) {
      const file = await new Promise((resolve) => entry.file(resolve, () => resolve(null)));
      if (file) {
        out.push({ file, relPath: prefix ? `${prefix}/${file.name}` : file.name });
      }
    } else if (entry.isDirectory) {
      const reader = entry.createReader();
      const nextPrefix = prefix ? `${prefix}/${entry.name}` : entry.name;
      while (true) {
        const batch = await new Promise((resolve) => reader.readEntries(resolve, () => resolve([])));
        if (!batch || batch.length === 0) break;
        for (const child of batch) await walk(child, nextPrefix);
      }
    }
  };
  for (const e of entries) await walk(e, "");
  return out;
}

const els = {
  shell: document.getElementById("ufo-shell"),
  topbar: document.getElementById("ufo-topbar"),
  rail: document.getElementById("ufo-rail"),
  tabs: document.getElementById("ufo-tabs"),
  empty: document.getElementById("ufo-empty"),
  stage: document.getElementById("ufo-stage"),
  stageFormatTag: document.getElementById("stage-format-tag"),
  stageBasis: document.getElementById("stage-basis"),
  stageModes: document.getElementById("stage-modes"),
  warningBanner: document.getElementById("ufo-warning-banner"),
  canvasHost: document.getElementById("ufo-canvas-host"),
  loading: document.getElementById("ufo-loading"),
  loadingLabel: document.getElementById("ufo-loading-label"),
  dragOverlay: document.getElementById("ufo-drag-overlay"),
  fileInput: document.getElementById("ufo-file-input"),
  infoDrawer: document.getElementById("ufo-info-drawer"),
  infoBody: document.getElementById("ufo-info-body"),
  modal: document.getElementById("ufo-modal"),
  modalTitle: document.getElementById("ufo-modal-title"),
  modalBody: document.getElementById("ufo-modal-body"),
  actOpen: document.getElementById("act-open"),
  actResave: document.getElementById("act-resave"),
  actInfo: document.getElementById("act-info"),
  actFullscreen: document.getElementById("act-fullscreen"),
  actTheme: document.getElementById("act-theme"),
  iconFullscreen: document.getElementById("icon-fullscreen"),
  iconTheme: document.getElementById("icon-theme"),
  labelTheme: document.getElementById("label-theme"),
  statFormats: document.getElementById("stat-formats"),
};

// ─── Initialization ───────────────────────────────────────────────────────────
async function bootstrap() {
  applyTheme(state.theme);

  if (Tauri) {
    try {
      const pinfo = await Tauri.core.invoke("get_platform_info");
      state.isMobile = pinfo.is_mobile;
      state.supportsWindowChrome = pinfo.supports_window_chrome;
      els.statFormats.textContent = `${pinfo.registered_formats}+`;
      if (!pinfo.supports_window_chrome) {
        document.querySelectorAll(".desktop-only").forEach((n) => n.classList.add("hidden"));
      }
      state.formatsCatalog = await Tauri.core.invoke("get_formats_catalog");
      const launchFiles = await Tauri.core.invoke("take_launch_files");
      for (const path of launchFiles) {
        await openNativePath(path);
      }
      setupTauriEvents();
    } catch (err) {
      console.error("Tauri init error:", err);
    }
  } else {
    document.querySelectorAll(".desktop-only").forEach((n) => n.classList.add("hidden"));
    try {
      state.formatsCatalog = await callWorker("catalog");
      els.statFormats.textContent = `${state.formatsCatalog.length}+`;
    } catch (err) {
      console.error("WASM Worker init failed:", err);
    }
  }

  bindEvents();
  setupFullscreenHoverChrome();
  renderAll();
}

function setupTauriEvents() {
  const evt = window.__TAURI__?.event;
  if (!evt) return;
  evt.listen("tauri://drag-enter", () => els.dragOverlay.classList.remove("hidden"));
  evt.listen("tauri://drag-leave", () => els.dragOverlay.classList.add("hidden"));
  evt.listen("tauri://drag-drop", async (event) => {
    els.dragOverlay.classList.add("hidden");
    const paths = event.payload?.paths || [];
    for (const p of paths) {
      await openNativePath(p);
    }
  });
  evt.listen("single-instance-files", async (event) => {
    const paths = Array.isArray(event.payload) ? event.payload : [];
    for (const p of paths) {
      if (typeof p === "string" && p.trim()) {
        await openNativePath(p);
      }
    }
  });
}

function bindEvents() {
  document.getElementById("btn-hero-open").addEventListener("click", triggerOpenPicker);
  document.getElementById("ufo-drop-zone").addEventListener("click", triggerOpenPicker);
  els.actOpen.addEventListener("click", triggerOpenPicker);
  els.actResave.addEventListener("click", openResaveModal);
  els.actInfo.addEventListener("click", () => toggleInfoDrawer());
  document.getElementById("btn-close-info").addEventListener("click", () => toggleInfoDrawer(false));
  els.actFullscreen.addEventListener("click", toggleFullscreen);
  els.actTheme.addEventListener("click", () => applyTheme(state.theme === "dark" ? "light" : "dark"));
  document.getElementById("btn-formats").addEventListener("click", openFormatsModal);
  document.getElementById("btn-close-modal").addEventListener("click", () => els.modal.classList.add("hidden"));
  els.modal.addEventListener("click", (e) => {
    if (e.target === els.modal) els.modal.classList.add("hidden");
  });

  if (Tauri) {
    document.getElementById("btn-win-min").addEventListener("click", () => Tauri.core.invoke("window_minimize"));
    document.getElementById("btn-win-max").addEventListener("click", () => Tauri.core.invoke("window_toggle_maximize"));
    document.getElementById("btn-win-close").addEventListener("click", () => Tauri.core.invoke("window_close"));
    els.topbar.addEventListener("mousedown", (e) => {
      if (e.target.hasAttribute("data-tauri-drag-region") && e.button === 0) {
        Tauri.core.invoke("window_start_dragging");
      }
    });
  } else {
    const closeBtn = document.getElementById("btn-win-close");
    closeBtn.textContent = "CLOSE TAB";
    closeBtn.addEventListener("click", () => {
      if (state.activeTabId) closeTab(state.activeTabId);
    });
  }

  els.stageModes.addEventListener("click", (e) => {
    const btn = e.target.closest(".ufo-mode-btn");
    if (!btn) return;
    const tab = getActiveTab();
    if (!tab) return;
    tab.mode = btn.dataset.mode;
    renderStage();
  });

  els.fileInput.addEventListener("change", async (e) => {
    const files = Array.from(e.target.files || []);
    for (const file of files) registerBrowserAsset(file.webkitRelativePath || file.name, file);
    for (const file of files) {
      await openBrowserFile(file);
    }
    els.fileInput.value = "";
  });

  const vp = document.getElementById("ufo-viewport");
  vp.addEventListener("dragover", (e) => {
    if (Tauri && state.supportsWindowChrome) return;
    e.preventDefault();
    els.dragOverlay.classList.remove("hidden");
  });
  vp.addEventListener("dragleave", (e) => {
    if (e.relatedTarget === null || !vp.contains(e.relatedTarget)) {
      els.dragOverlay.classList.add("hidden");
    }
  });
  vp.addEventListener("drop", async (e) => {
    if (Tauri && state.supportsWindowChrome) return;
    e.preventDefault();
    els.dragOverlay.classList.add("hidden");
    const dropped = await readDroppedEntries(e.dataTransfer);
    for (const item of dropped) registerBrowserAsset(item.relPath, item.file);
    const mdFiles = dropped.filter((item) => /\.(md|markdown|mdown|mkd)$/i.test(item.file.name));
    const toOpen = mdFiles.length > 0 && dropped.length > 1 ? mdFiles : dropped;
    for (const item of toOpen) {
      await openBrowserFile(item.file);
    }
  });

  window.addEventListener("paste", async (e) => {
    const items = Array.from(e.clipboardData?.items || []);
    for (const item of items) {
      if (item.kind === "file") {
        const f = item.getAsFile();
        if (f) await openBrowserFile(f);
      }
    }
  });

  window.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "o") {
      e.preventDefault();
      triggerOpenPicker();
    } else if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "w") {
      if (state.activeTabId) {
        e.preventDefault();
        closeTab(state.activeTabId);
      }
    } else if (e.key === "F11") {
      e.preventDefault();
      toggleFullscreen();
    } else if (e.key === "Escape" && state.isFullscreen) {
      toggleFullscreen(false);
    }
  });

  document.addEventListener("fullscreenchange", () => {
    if (!Tauri && !document.fullscreenElement && state.isFullscreen) {
      setFullscreenUi(false);
    }
  });
}

// ─── Theme Toggle (Item 0) ────────────────────────────────────────────────────
function applyTheme(next) {
  state.theme = next === "light" ? "light" : "dark";
  document.documentElement.setAttribute("data-theme", state.theme);
  localStorage.setItem("ufo-theme", state.theme);
  const isLight = state.theme === "light";
  els.iconTheme.className = isLight ? "bi bi-moon-stars" : "bi bi-sun";
  els.labelTheme.textContent = isLight ? "Dark" : "Light";
  const metaTheme = document.querySelector('meta[name="theme-color"]');
  if (metaTheme) metaTheme.content = isLight ? "#f4f7f7" : "#000000";
}

// ─── Fullscreen Edge-Reveal Chrome with Generous Delay (Item 6) ───────────────
let hideChromeTimer = null;
let pointerInsideChrome = false;

function setupFullscreenHoverChrome() {
  const showChrome = () => {
    if (!state.isFullscreen) return;
    els.shell.classList.remove("chrome-hidden");
    scheduleHideChrome();
  };

  const scheduleHideChrome = () => {
    if (hideChromeTimer) clearTimeout(hideChromeTimer);
    if (!state.isFullscreen || pointerInsideChrome || state.infoOpen || !els.modal.classList.contains("hidden")) {
      return;
    }
    hideChromeTimer = setTimeout(() => {
      if (state.isFullscreen && !pointerInsideChrome && !state.infoOpen && els.modal.classList.contains("hidden")) {
        els.shell.classList.add("chrome-hidden");
      }
    }, 1800); // 1.8s generous hover grace delay so clicking Close/Min/Info is effortless
  };

  for (const bar of [els.topbar, els.rail, els.infoDrawer]) {
    bar.addEventListener("mouseenter", () => {
      pointerInsideChrome = true;
      if (hideChromeTimer) clearTimeout(hideChromeTimer);
      if (state.isFullscreen) els.shell.classList.remove("chrome-hidden");
    });
    bar.addEventListener("mouseleave", () => {
      pointerInsideChrome = false;
      scheduleHideChrome();
    });
  }

  window.addEventListener("mousemove", (e) => {
    if (!state.isFullscreen) return;
    const nearTop = e.clientY <= 40;
    const nearRight = e.clientX >= window.innerWidth - 48;
    const nearBottom = state.isMobile && e.clientY >= window.innerHeight - 48;
    if (nearTop || nearRight || nearBottom) {
      showChrome();
    }
  }, { passive: true });

  window.addEventListener("touchstart", (e) => {
    if (!state.isFullscreen) return;
    const t = e.touches?.[0];
    if (!t) return;
    if (t.clientY <= 52 || t.clientY >= window.innerHeight - 56 || t.clientX >= window.innerWidth - 48) {
      showChrome();
    }
  }, { passive: true });
}

function setFullscreenUi(active) {
  state.isFullscreen = active;
  els.shell.classList.toggle("is-fullscreen", active);
  els.iconFullscreen.className = active ? "bi bi-fullscreen-exit" : "bi bi-fullscreen";
  els.actFullscreen.classList.toggle("active", active);
  if (hideChromeTimer) clearTimeout(hideChromeTimer);
  if (active) {
    els.shell.classList.remove("chrome-hidden");
    hideChromeTimer = setTimeout(() => {
      if (state.isFullscreen && !pointerInsideChrome) {
        els.shell.classList.add("chrome-hidden");
      }
    }, 1800);
  } else {
    els.shell.classList.remove("chrome-hidden");
  }
}

async function toggleFullscreen(force) {
  const next = typeof force === "boolean" ? force : !state.isFullscreen;
  if (Tauri && state.supportsWindowChrome) {
    const currentlyFs = next !== state.isFullscreen ? await Tauri.core.invoke("window_toggle_fullscreen") : state.isFullscreen;
    setFullscreenUi(currentlyFs);
  } else {
    if (next && !document.fullscreenElement) {
      await document.documentElement.requestFullscreen?.().catch(() => {});
    } else if (!next && document.fullscreenElement) {
      await document.exitFullscreen?.().catch(() => {});
    }
    setFullscreenUi(next);
  }
}

// ─── File Opening (Non-blocking Async / Web Worker) ───────────────────────────
function setLoading(show, label = "OPENING FILE...") {
  els.loadingLabel.textContent = label;
  els.loading.classList.toggle("hidden", !show);
}

async function triggerOpenPicker() {
  if (Tauri && state.supportsWindowChrome) {
    const paths = await Tauri.core.invoke("pick_files_dialog");
    for (const p of paths) {
      await openNativePath(p);
    }
  } else {
    els.fileInput.click();
  }
}

async function openNativePath(path) {
  setLoading(true, `OPENING ${path.split(/[\\/]/).pop()}...`);
  try {
    const res = await Tauri.core.invoke("open_path", { path });
    addTabFromSession(res.session_id, res.stream_url, res.report, null, res.asset_base_url || null);
  } catch (err) {
    showToastError(`Unable to open ${path}: ${err}`);
  } finally {
    setLoading(false);
  }
}

async function openBrowserFile(file) {
  registerBrowserAsset(file.webkitRelativePath || file.name, file);
  setLoading(true, `DECODING ${file.name}...`);
  try {
    const MAX_WASM_INMEM = 256 * 1024 * 1024;
    let bytes;
    if (file.size <= MAX_WASM_INMEM) {
      bytes = new Uint8Array(await file.arrayBuffer());
    } else {
      bytes = new Uint8Array(await file.slice(0, 8 * 1024 * 1024).arrayBuffer());
    }
    await openRawBytes(file.name, bytes, file.size);
  } finally {
    setLoading(false);
  }
}

async function openRawBytes(fileName, bytes, totalSizeHint) {
  if (Tauri) {
    const res = await Tauri.core.invoke("open_bytes_ipc", {
      fileName,
      bytes: Array.from(bytes),
    });
    addTabFromSession(res.session_id, res.stream_url, res.report, bytes, res.asset_base_url || null);
    return;
  }
  const report = await callWorker("open", {
    fileName,
    bytes,
    totalSizeHint: totalSizeHint ? BigInt(totalSizeHint) : undefined,
  });
  addTabFromSession(null, null, report, bytes, null);
}

function addTabFromSession(sessionId, streamUrl, report, bytes, assetBaseUrl = null) {
  const id = `tab-${state.nextLocalId++}`;
  state.tabs.push({
    id,
    sessionId,
    streamUrl,
    assetBaseUrl,
    fileName: report.file_name,
    report,
    bytes,
    mode: "preview",
    cleanup: null,
    blobUrl: null,
    hexOffset: 0,
    loadedLines: null,
    mdRaw: false,
    htmlRaw: false,
    activeSheet: 0,
  });
  state.activeTabId = id;
  renderAll();
}

function teardownTabPlayback(tab) {
  if (!tab) return;
  if (tab.cleanup) {
    try {
      tab.cleanup();
    } catch (_) {}
    tab.cleanup = null;
  }
}

function closeTab(tabId) {
  const idx = state.tabs.findIndex((t) => t.id === tabId);
  if (idx === -1) return;
  const tab = state.tabs[idx];
  teardownTabPlayback(tab);
  if (tab.blobUrl) {
    URL.revokeObjectURL(tab.blobUrl);
    tab.blobUrl = null;
  }
  if (tab.sessionId && Tauri) {
    Tauri.core.invoke("close_session", { sessionId: tab.sessionId }).catch(() => {});
  }
  state.tabs.splice(idx, 1);
  if (state.activeTabId === tabId) {
    const next = state.tabs[idx] || state.tabs[idx - 1] || null;
    state.activeTabId = next ? next.id : null;
  }
  renderAll();
}

function getActiveTab() {
  return state.tabs.find((t) => t.id === state.activeTabId) || null;
}

// ─── Render Top Tabs, Rail State, Stage, & Info Drawer ────────────────────────
function renderAll() {
  // Always halt active media/3D loops from previously visible tabs before re-rendering
  for (const t of state.tabs) {
    if (t.id !== state.activeTabId) teardownTabPlayback(t);
  }
  renderTabs();
  const active = getActiveTab();
  els.actResave.disabled = !active;
  els.actInfo.disabled = !active;
  if (!active) {
    els.empty.classList.remove("hidden");
    els.stage.classList.add("hidden");
    els.infoDrawer.classList.add("hidden");
    els.actInfo.classList.remove("active");
    state.infoOpen = false;
  } else {
    els.empty.classList.add("hidden");
    els.stage.classList.remove("hidden");
    renderStage();
    if (state.infoOpen) renderInfoDrawer();
  }
}

function renderTabs() {
  els.tabs.innerHTML = "";
  for (const tab of state.tabs) {
    const isActive = tab.id === state.activeTabId;
    const det = tab.report.detected;
    const hasWarning =
      (det.warnings && det.warnings.length > 0) ||
      det.ambiguity?.extension_mismatch ||
      det.confidence === "BinaryFallback";

    const btn = document.createElement("div");
    btn.className = `ufo-tab ${isActive ? "active" : ""} ${hasWarning ? "has-warning" : ""}`;
    btn.setAttribute("role", "tab");
    btn.setAttribute("aria-selected", String(isActive));

    const label = document.createElement("span");
    label.textContent = tab.fileName;
    btn.appendChild(label);

    if (hasWarning) {
      const w = document.createElement("i");
      w.className = "bi bi-exclamation-triangle ufo-tab-warn";
      w.title = det.warnings?.[0] || "Format discrepancy or binary fallback";
      btn.appendChild(w);
    }

    const closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.className = "ufo-tab-close";
    closeBtn.title = "Close file";
    closeBtn.innerHTML = `<i class="bi bi-x-circle tab-close-outline"></i><i class="bi bi-x-circle-fill tab-close-fill"></i>`;
    closeBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      closeTab(tab.id);
    });
    btn.appendChild(closeBtn);

    btn.addEventListener("click", () => {
      state.activeTabId = tab.id;
      renderAll();
    });
    els.tabs.appendChild(btn);
  }
}

function renderStage() {
  const tab = getActiveTab();
  if (!tab) return;
  teardownTabPlayback(tab);

  const r = tab.report;
  const det = r.detected;
  els.stageFormatTag.textContent = `${det.id.toUpperCase()} · ${formatBytes(r.file_size)}`;
  els.stageBasis.textContent = `${det.name} — ${det.detection_basis} (${(r.parse_time_us / 1000).toFixed(2)} ms)`;

  for (const b of els.stageModes.querySelectorAll(".ufo-mode-btn")) {
    b.classList.toggle("active", b.dataset.mode === tab.mode);
  }

  if (det.warnings?.length || det.ambiguity) {
    const msgs = [];
    if (det.warnings?.length) msgs.push(...det.warnings);
    if (det.ambiguity) {
      msgs.push(
        `Ambiguous .${det.ambiguity.extension}: [${det.ambiguity.candidates.join(" vs ")}] → Resolved as ${det.ambiguity.resolved_meaning}`
      );
    }
    els.warningBanner.innerHTML = `<i class="bi bi-exclamation-triangle"></i> <span>${escapeHtml(msgs.join(" · "))}</span>`;
    els.warningBanner.classList.remove("hidden");
  } else {
    els.warningBanner.classList.add("hidden");
  }

  const host = els.canvasHost;
  host.innerHTML = "";

  if (tab.mode === "hex") {
    renderHexView(host, tab);
    return;
  }
  if (tab.mode === "strings") {
    renderStringsView(host, r.preview_strings);
    return;
  }

  const v = r.viewer;
  switch (v.kind) {
    case "Image":
      tab.cleanup = renderImageView(host, v);
      break;
    case "Svg":
      tab.cleanup = renderSvgView(host, tab, v);
      break;
    case "Vector2d":
      tab.cleanup = renderVector2dView(host, v);
      break;
    case "LayeredDesign":
      tab.cleanup = renderLayeredDesignView(host, v);
      break;
    case "Mesh3d":
      tab.cleanup = mountMeshViewer(host, v, () => state.theme === "light");
      break;
    case "Text":
      renderTextView(host, tab, v);
      break;
    case "Table":
      renderTableView(host, tab, v);
      break;
    case "Document":
      renderDocumentView(host, tab, v);
      break;
    case "Archive":
      renderArchiveView(host, tab, v);
      break;
    case "Media":
      renderMediaView(host, tab, v);
      break;
    case "Font":
      renderFontView(host, tab, v);
      break;
    case "Binary":
      renderBinaryView(host, v);
      break;
    case "Structured":
      renderStructuredView(host, v);
      break;
    case "HexOnly":
      renderHexView(host, tab, v.note);
      break;
    default:
      renderHexView(host, tab);
  }
}

// ─── Shared Pan & Zoom Helper (Item 10: Images, SVGs, & 2D Vectors) ───────────
function attachPanZoom(wrap, targetEl) {
  let scale = 1, tx = 0, ty = 0;
  const apply = () => {
    targetEl.style.transform = `translate3d(${tx}px, ${ty}px, 0) scale(${scale})`;
  };

  const ptrs = new Map();
  let lastPinch = 0;

  wrap.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 && e.pointerType === "mouse") return;
    wrap.setPointerCapture(e.pointerId);
    wrap.classList.add("dragging");
    ptrs.set(e.pointerId, { x: e.clientX, y: e.clientY });
  });
  wrap.addEventListener("pointermove", (e) => {
    if (!ptrs.has(e.pointerId)) return;
    const prev = ptrs.get(e.pointerId);
    const dx = e.clientX - prev.x;
    const dy = e.clientY - prev.y;
    ptrs.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (ptrs.size === 1) {
      tx += dx;
      ty += dy;
      apply();
    } else if (ptrs.size === 2) {
      const [a, b] = Array.from(ptrs.values());
      const d = Math.hypot(a.x - b.x, a.y - b.y);
      if (lastPinch > 0) {
        scale = Math.max(0.1, Math.min(30, scale * (d / lastPinch)));
        apply();
      }
      lastPinch = d;
    }
  });
  const rel = (e) => {
    ptrs.delete(e.pointerId);
    if (ptrs.size < 2) lastPinch = 0;
    if (ptrs.size === 0) wrap.classList.remove("dragging");
  };
  wrap.addEventListener("pointerup", rel);
  wrap.addEventListener("pointercancel", rel);
  wrap.addEventListener("wheel", (e) => {
    e.preventDefault();
    scale = Math.max(0.1, Math.min(30, scale * (e.deltaY < 0 ? 1.14 : 0.88)));
    apply();
  }, { passive: false });

  return {
    zoomIn: () => { scale = Math.min(30, scale * 1.25); apply(); },
    zoomOut: () => { scale = Math.max(0.1, scale * 0.8); apply(); },
    reset: () => { scale = 1; tx = 0; ty = 0; apply(); },
  };
}

// ─── Individual Format Viewers ────────────────────────────────────────────────
function renderImageView(host, v) {
  const stage = document.createElement("div");
  stage.className = "ufo-media-stage";
  stage.style.padding = "0";

  const wrap = document.createElement("div");
  wrap.className = "ufo-img-wrap";
  const img = document.createElement("img");
  img.src = v.data_url;
  img.alt = "Image preview";
  img.draggable = false;
  wrap.appendChild(img);
  stage.appendChild(wrap);

  const pz = attachPanZoom(wrap, img);

  const hud = document.createElement("div");
  hud.className = "ufo-float-toolbar";
  hud.innerHTML = `
    <span class="ufo-tool-btn" style="cursor:default">${v.width} × ${v.height} · ${v.channels}</span>
    <button type="button" class="ufo-tool-btn" data-z="out"><i class="bi bi-zoom-out"></i></button>
    <button type="button" class="ufo-tool-btn" data-z="in"><i class="bi bi-zoom-in"></i></button>
    <button type="button" class="ufo-tool-btn" data-z="reset">FIT</button>
  `;
  hud.addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    if (b.dataset.z === "in") pz.zoomIn();
    if (b.dataset.z === "out") pz.zoomOut();
    if (b.dataset.z === "reset") pz.reset();
  });
  stage.appendChild(hud);
  host.appendChild(stage);
  return null;
}

function renderSvgView(host, tab, v) {
  const stage = document.createElement("div");
  stage.className = "ufo-media-stage";
  stage.style.padding = "0";

  const box = document.createElement("div");
  box.className = "ufo-img-wrap ufo-svg-box";

  let blobUrl = null;
  const img = document.createElement("img");
  img.draggable = false;
  img.style.width = "85%";
  img.style.height = "85%";

  if (v.use_stream_url && tab.streamUrl) {
    img.src = tab.streamUrl;
  } else {
    const blob = new Blob([v.svg_markup], { type: "image/svg+xml" });
    blobUrl = URL.createObjectURL(blob);
    img.src = blobUrl;
  }

  box.appendChild(img);
  stage.appendChild(box);
  const pz = attachPanZoom(box, img);

  // Format SVG frame dimension badge (Item 9)
  const dimsParts = [];
  if (v.width || v.height) dimsParts.push(`${v.width || "auto"} × ${v.height || "auto"}`);
  if (v.view_box) dimsParts.push(`viewBox [${v.view_box}]`);
  const dimLabel = dimsParts.length ? `SVG · ${dimsParts.join(" · ")}` : "SCALABLE VECTOR (SVG)";

  const hud = document.createElement("div");
  hud.className = "ufo-float-toolbar";
  hud.innerHTML = `
    <span class="ufo-tool-btn" style="cursor:default">${escapeHtml(dimLabel)}</span>
    <button type="button" class="ufo-tool-btn" data-z="in"><i class="bi bi-zoom-in"></i></button>
    <button type="button" class="ufo-tool-btn" data-z="out"><i class="bi bi-zoom-out"></i></button>
    <button type="button" class="ufo-tool-btn" data-z="reset">FIT</button>
    ${v.source_text ? `<button type="button" class="ufo-tool-btn" data-z="src">SVG SRC</button>` : ""}
  `;

  let showingSrc = false;
  hud.addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    if (b.dataset.z === "in") pz.zoomIn();
    if (b.dataset.z === "out") pz.zoomOut();
    if (b.dataset.z === "reset") pz.reset();
    if (b.dataset.z === "src") {
      showingSrc = !showingSrc;
      b.classList.toggle("active", showingSrc);
      box.innerHTML = "";
      if (showingSrc) {
        const pre = document.createElement("pre");
        pre.style.cssText = "padding:18px;width:100%;height:100%;overflow:auto;font-family:var(--mono);font-size:12px;margin:0;cursor:text;user-select:text;";
        pre.textContent = v.source_text;
        box.appendChild(pre);
      } else {
        box.appendChild(img);
      }
    }
  });
  stage.appendChild(hud);
  host.appendChild(stage);
  return () => {
    if (blobUrl) URL.revokeObjectURL(blobUrl);
  };
}

function renderVector2dView(host, v) {
  const stage = document.createElement("div");
  stage.className = "ufo-media-stage";
  stage.style.padding = "0";

  const box = document.createElement("div");
  box.className = "ufo-img-wrap ufo-svg-box";
  const blob = new Blob([v.svg_markup], { type: "image/svg+xml" });
  const url = URL.createObjectURL(blob);
  const img = document.createElement("img");
  img.src = url;
  img.draggable = false;
  img.style.width = "88%";
  img.style.height = "88%";
  box.appendChild(img);
  stage.appendChild(box);

  const pz = attachPanZoom(box, img);
  const dimText = `${v.format_label} · ${v.width.toFixed(1)} × ${v.height.toFixed(1)} (${v.entity_count} entities)`;
  const hud = document.createElement("div");
  hud.className = "ufo-float-toolbar";
  hud.innerHTML = `
    <span class="ufo-tool-btn" style="cursor:default">${escapeHtml(dimText)}</span>
    <button type="button" class="ufo-tool-btn" data-z="in"><i class="bi bi-zoom-in"></i></button>
    <button type="button" class="ufo-tool-btn" data-z="out"><i class="bi bi-zoom-out"></i></button>
    <button type="button" class="ufo-tool-btn" data-z="reset">FIT</button>
  `;
  hud.addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    if (b.dataset.z === "in") pz.zoomIn();
    if (b.dataset.z === "out") pz.zoomOut();
    if (b.dataset.z === "reset") pz.reset();
  });
  stage.appendChild(hud);
  host.appendChild(stage);
  return () => URL.revokeObjectURL(url);
}

const SAFE_BLEND_MODES = new Set([
  "normal", "multiply", "screen", "overlay", "darken", "lighten",
  "color-dodge", "color-burn", "hard-light", "soft-light",
  "difference", "exclusion", "hue", "saturation", "color", "luminosity",
]);

function renderLayeredDesignView(host, v) {
  const pages = Array.isArray(v.pages) ? v.pages : [];
  if (pages.length === 0) {
    const empty = document.createElement("div");
    empty.className = "ufo-media-stage";
    empty.innerHTML = `<div class="ufo-empty-note">No pages or layers found in ${escapeHtml(v.format_label || "design")}.</div>`;
    host.appendChild(empty);
    return () => {};
  }

  let activeMode = pages.length > 1 && typeof v.active_page === "number" && pages[v.active_page]
    ? v.active_page
    : (pages.length > 1 ? "all" : 0);

  const layerVis = pages.map((p) => (p.layers || []).map((l) => l.visible !== false));
  const hasDrawables = pages.map((p) =>
    (p.layers || []).some((l) => Boolean(l.image_data_url || l.svg_fragment))
  );
  const showComposite = pages.map((p, pi) => Boolean(p.composite_data_url) && !hasDrawables[pi]);

  const split = document.createElement("div");
  split.className = "ufo-stage-split";

  const main = document.createElement("div");
  main.className = "ufo-design-main ufo-img-wrap";
  main.style.display = "flex";
  main.style.alignItems = "center";
  main.style.justifyContent = "center";

  const canvas = document.createElement("div");
  canvas.className = "ufo-design-canvas";
  main.appendChild(canvas);

  const pz = attachPanZoom(main, canvas);

  const hud = document.createElement("div");
  hud.className = "ufo-float-toolbar";
  const statusPill = document.createElement("span");
  statusPill.className = "ufo-tool-btn";
  statusPill.style.cursor = "default";
  hud.appendChild(statusPill);
  const zoomBtns = [
    ["in", `<i class="bi bi-zoom-in"></i>`, "Zoom in"],
    ["out", `<i class="bi bi-zoom-out"></i>`, "Zoom out"],
    ["reset", "FIT", "Reset pan & zoom"],
  ];
  for (const [z, label, title] of zoomBtns) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "ufo-tool-btn";
    b.dataset.z = z;
    b.title = title;
    b.innerHTML = label;
    hud.appendChild(b);
  }
  hud.addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    if (b.dataset.z === "in") pz.zoomIn();
    if (b.dataset.z === "out") pz.zoomOut();
    if (b.dataset.z === "reset") pz.reset();
  });
  main.appendChild(hud);

  const side = document.createElement("aside");
  side.className = "ufo-side-panel";
  split.appendChild(main);
  split.appendChild(side);
  host.appendChild(split);

  const frameRefs = new Map();
  const hlRefs = new Map();
  const scaleRefs = new Map();
  const rowRefs = new Map();

  function setStatus(text) {
    if (text) {
      statusPill.textContent = text;
      return;
    }
    const modeLabel = activeMode === "all"
      ? `All Pages (${pages.length})`
      : `${pages[activeMode]?.name || "Page"} · ${Math.round(pages[activeMode]?.width || 0)}×${Math.round(pages[activeMode]?.height || 0)}`;
    statusPill.textContent = `${v.format_label || "Design"} · ${modeLabel}`;
  }

  function highlightLayer(pIdx, lIdx) {
    const page = pages[pIdx];
    const hl = hlRefs.get(pIdx);
    const dispScale = scaleRefs.get(pIdx) || 1;
    if (!page || !hl) return;
    if (lIdx === null || lIdx === undefined) {
      hl.style.display = "none";
      rowRefs.forEach((el) => el.style.borderColor = "");
      setStatus(null);
      return;
    }
    const layer = page.layers?.[lIdx];
    if (!layer) return;
    const x = (Number(layer.x) || 0) * dispScale;
    const y = (Number(layer.y) || 0) * dispScale;
    const w = Math.max(10, (Number(layer.width) || 0) * dispScale);
    const h = Math.max(10, (Number(layer.height) || 0) * dispScale);
    hl.style.left = `${x}px`;
    hl.style.top = `${y}px`;
    hl.style.width = `${w}px`;
    hl.style.height = `${h}px`;
    hl.style.display = "block";
    rowRefs.forEach((el, key) => {
      el.style.borderColor = key === `${pIdx}:${lIdx}` ? "var(--ice)" : "";
    });
    setStatus(`${layer.name} · ${layer.kind} (${Math.round(layer.x || 0)}, ${Math.round(layer.y || 0)} · ${Math.round(layer.width || 0)}×${Math.round(layer.height || 0)})`);
  }

  function renderCanvas() {
    canvas.innerHTML = "";
    frameRefs.clear();
    hlRefs.clear();
    scaleRefs.clear();

    const indices = activeMode === "all" ? pages.map((_, idx) => idx) : [activeMode];
    const maxDim = activeMode === "all" ? (pages.length > 4 ? 340 : 460) : 820;

    for (const pIdx of indices) {
      const page = pages[pIdx];
      if (!page) continue;
      const pw = Math.max(1, Number(page.width) || 800);
      const ph = Math.max(1, Number(page.height) || 600);
      let dispScale = Math.min(1, maxDim / Math.max(pw, ph));
      if (Math.max(pw, ph) < 260) {
        dispScale = Math.min(3.5, Math.max(1, 360 / Math.max(pw, ph)));
      }
      scaleRefs.set(pIdx, dispScale);

      const frame = document.createElement("div");
      frame.className = "ufo-artboard-frame";
      frame.style.width = `${Math.max(24, pw * dispScale)}px`;
      frame.style.height = `${Math.max(24, ph * dispScale)}px`;
      if (page.background) {
        frame.style.background = page.background;
      }

      const titleTag = document.createElement("div");
      titleTag.className = "ufo-artboard-title";
      titleTag.textContent = `${page.name} (${Math.round(pw)}×${Math.round(ph)})`;
      frame.appendChild(titleTag);

      const stage = document.createElement("div");
      stage.style.position = "absolute";
      stage.style.left = "0";
      stage.style.top = "0";
      stage.style.width = `${pw}px`;
      stage.style.height = `${ph}px`;
      stage.style.transformOrigin = "top left";
      stage.style.transform = `scale(${dispScale})`;

      const layers = page.layers || [];
      const topDownStack = /photoshop|openraster|krita/i.test(v.format_label || "");
      if (showComposite[pIdx] && page.composite_data_url) {
        const compImg = document.createElement("img");
        compImg.src = page.composite_data_url;
        compImg.alt = page.name;
        compImg.draggable = false;
        compImg.style.position = "absolute";
        compImg.style.left = "0";
        compImg.style.top = "0";
        compImg.style.width = `${pw}px`;
        compImg.style.height = `${ph}px`;
        compImg.style.objectFit = "contain";
        compImg.style.pointerEvents = "none";
        stage.appendChild(compImg);
      } else {
        let drawnAny = false;
        for (let li = 0; li < layers.length; li++) {
          if (!layerVis[pIdx][li]) continue;
          const layer = layers[li];
          const opacity = Math.max(0, Math.min(1, Number(layer.opacity ?? 1)));
          const blendKey = String(layer.blend_mode || "normal").toLowerCase().replace(/_/g, "-");
          const blendCss = SAFE_BLEND_MODES.has(blendKey) ? blendKey : "normal";
          const layerZ = String(topDownStack ? (layers.length - li) : (li + 1));

          if (layer.image_data_url) {
            drawnAny = true;
            const lImg = document.createElement("img");
            lImg.src = layer.image_data_url;
            lImg.alt = layer.name;
            lImg.draggable = false;
            lImg.style.position = "absolute";
            lImg.style.left = `${Number(layer.x) || 0}px`;
            lImg.style.top = `${Number(layer.y) || 0}px`;
            lImg.style.width = `${Math.max(1, Number(layer.width) || pw)}px`;
            lImg.style.height = `${Math.max(1, Number(layer.height) || ph)}px`;
            lImg.style.opacity = String(opacity);
            lImg.style.mixBlendMode = blendCss;
            lImg.style.zIndex = layerZ;
            lImg.style.pointerEvents = "none";
            stage.appendChild(lImg);
          }
          if (layer.svg_fragment) {
            drawnAny = true;
            const ns = "http://www.w3.org/2000/svg";
            const svg = document.createElementNS(ns, "svg");
            svg.setAttribute("viewBox", `0 0 ${pw} ${ph}`);
            svg.setAttribute("width", String(pw));
            svg.setAttribute("height", String(ph));
            svg.style.position = "absolute";
            svg.style.left = "0";
            svg.style.top = "0";
            svg.style.opacity = String(opacity);
            svg.style.mixBlendMode = blendCss;
            svg.style.zIndex = layerZ;
            svg.style.overflow = "visible";
            svg.style.pointerEvents = "none";
            svg.innerHTML = layer.svg_fragment;
            stage.appendChild(svg);
          }
          if (!layer.image_data_url && !layer.svg_fragment && Number(layer.width) > 0 && Number(layer.height) > 0) {
            const box = document.createElement("div");
            box.style.position = "absolute";
            box.style.left = `${Number(layer.x) || 0}px`;
            box.style.top = `${Number(layer.y) || 0}px`;
            box.style.width = `${Number(layer.width)}px`;
            box.style.height = `${Number(layer.height)}px`;
            box.style.border = "1px dashed rgba(234, 255, 255, 0.25)";
            box.style.pointerEvents = "none";
            stage.appendChild(box);
          }
        }
        if (!drawnAny && page.composite_data_url && layerVis[pIdx].some(Boolean)) {
          const compFallback = document.createElement("img");
          compFallback.src = page.composite_data_url;
          compFallback.alt = page.name;
          compFallback.draggable = false;
          compFallback.style.position = "absolute";
          compFallback.style.left = "0";
          compFallback.style.top = "0";
          compFallback.style.width = `${pw}px`;
          compFallback.style.height = `${ph}px`;
          compFallback.style.objectFit = "contain";
          compFallback.style.pointerEvents = "none";
          stage.appendChild(compFallback);
        }
      }

      frame.appendChild(stage);

      const hl = document.createElement("div");
      hl.className = "ufo-hover-highlight";
      frame.appendChild(hl);
      hlRefs.set(pIdx, hl);
      frameRefs.set(pIdx, frame);

      frame.addEventListener("pointermove", (e) => {
        const rect = frame.getBoundingClientRect();
        const scaleX = rect.width > 0 ? rect.width / pw : dispScale;
        const scaleY = rect.height > 0 ? rect.height / ph : dispScale;
        const px = (e.clientX - rect.left) / scaleX;
        const py = (e.clientY - rect.top) / scaleY;
        let hit = null;
        for (let li = layers.length - 1; li >= 0; li--) {
          if (!layerVis[pIdx][li]) continue;
          const l = layers[li];
          const lx = Number(l.x) || 0;
          const ly = Number(l.y) || 0;
          const lw = Number(l.width) || 0;
          const lh = Number(l.height) || 0;
          if (lw > 0 && lh > 0 && px >= lx && px <= lx + lw && py >= ly && py <= ly + lh) {
            hit = li;
            break;
          }
        }
        frame.classList.add("is-hovered");
        highlightLayer(pIdx, hit);
      });
      frame.addEventListener("pointerleave", () => {
        frame.classList.remove("is-hovered");
        highlightLayer(pIdx, null);
      });

      canvas.appendChild(frame);
    }
    setStatus(null);
  }

  function renderSidePanel() {
    side.innerHTML = "";
    rowRefs.clear();

    // 1. Pages Section
    const pageSec = document.createElement("div");
    pageSec.className = "ufo-side-section";
    const pageHead = document.createElement("div");
    pageHead.className = "ufo-side-head";
    pageHead.innerHTML = `<span><i class="bi bi-journals"></i> Pages (${pages.length})</span>`;
    pageSec.appendChild(pageHead);

    const pageList = document.createElement("div");
    pageList.className = "ufo-page-list";

    if (pages.length > 1) {
      const allBtn = document.createElement("button");
      allBtn.type = "button";
      allBtn.className = `ufo-page-btn ${activeMode === "all" ? "active" : ""}`;
      allBtn.innerHTML = `<span>ALL PAGES</span><span>${pages.length} artboards</span>`;
      allBtn.addEventListener("mouseenter", () => {
        frameRefs.forEach((f) => f.classList.add("is-hovered"));
      });
      allBtn.addEventListener("mouseleave", () => {
        frameRefs.forEach((f) => f.classList.remove("is-hovered"));
      });
      allBtn.addEventListener("click", () => {
        activeMode = "all";
        pz.reset();
        renderCanvas();
        renderSidePanel();
      });
      pageList.appendChild(allBtn);
    }

    pages.forEach((p, pi) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = `ufo-page-btn ${activeMode === pi ? "active" : ""}`;
      const prefix = pages.length === 1 ? "Entire Page · " : `${pi + 1}. `;
      btn.innerHTML = `<span>${escapeHtml(prefix + p.name)}</span><span>${Math.round(p.width)}×${Math.round(p.height)}</span>`;
      btn.addEventListener("mouseenter", () => {
        const f = frameRefs.get(pi);
        if (f) f.classList.add("is-hovered");
        setStatus(`Page: ${p.name} · ${(p.layers || []).length} layers`);
      });
      btn.addEventListener("mouseleave", () => {
        const f = frameRefs.get(pi);
        if (f) f.classList.remove("is-hovered");
        setStatus(null);
      });
      btn.addEventListener("click", () => {
        activeMode = pi;
        pz.reset();
        renderCanvas();
        renderSidePanel();
      });
      pageList.appendChild(btn);
    });
    pageSec.appendChild(pageList);
    side.appendChild(pageSec);

    // 2. Layers Section
    const layerIndices = activeMode === "all" ? pages.map((_, idx) => idx) : [activeMode];
    const totalLayers = layerIndices.reduce((acc, pi) => acc + (pages[pi]?.layers?.length || 0), 0);

    const layerSec = document.createElement("div");
    layerSec.className = "ufo-side-section";
    layerSec.style.borderBottom = "none";

    const layerHead = document.createElement("div");
    layerHead.className = "ufo-side-head";
    const headTitle = document.createElement("span");
    headTitle.innerHTML = `<i class="bi bi-layers"></i> Layers (${totalLayers})`;
    layerHead.appendChild(headTitle);

    const actions = document.createElement("div");
    actions.className = "ufo-side-actions";

    const btnAll = document.createElement("button");
    btnAll.type = "button";
    btnAll.className = "ufo-mini-btn";
    btnAll.textContent = "All";
    btnAll.title = "Show all layers";
    btnAll.addEventListener("click", () => {
      for (const pi of layerIndices) {
        layerVis[pi] = layerVis[pi].map(() => true);
        if (hasDrawables[pi]) showComposite[pi] = false;
      }
      renderCanvas();
      renderSidePanel();
    });
    actions.appendChild(btnAll);

    const btnNone = document.createElement("button");
    btnNone.type = "button";
    btnNone.className = "ufo-mini-btn";
    btnNone.textContent = "None";
    btnNone.title = "Hide all layers";
    btnNone.addEventListener("click", () => {
      for (const pi of layerIndices) {
        layerVis[pi] = layerVis[pi].map(() => false);
        showComposite[pi] = false;
      }
      renderCanvas();
      renderSidePanel();
    });
    actions.appendChild(btnNone);

    const anyComposite = layerIndices.some((pi) => Boolean(pages[pi]?.composite_data_url));
    if (anyComposite) {
      const compActive = layerIndices.every((pi) => !pages[pi]?.composite_data_url || showComposite[pi]);
      const btnComp = document.createElement("button");
      btnComp.type = "button";
      btnComp.className = `ufo-mini-btn ${compActive ? "active" : ""}`;
      btnComp.textContent = "Merged";
      btnComp.title = "Toggle flattened composite preview vs live visible layers";
      btnComp.addEventListener("click", () => {
        const next = !compActive;
        for (const pi of layerIndices) {
          if (pages[pi]?.composite_data_url) showComposite[pi] = next;
        }
        renderCanvas();
        renderSidePanel();
      });
      actions.appendChild(btnComp);
    }

    layerHead.appendChild(actions);
    layerSec.appendChild(layerHead);

    const layerList = document.createElement("div");
    layerList.className = "ufo-layer-list";

    for (const pi of layerIndices) {
      const page = pages[pi];
      const layers = page?.layers || [];
      if (activeMode === "all" && pages.length > 1) {
        const sub = document.createElement("div");
        sub.className = "ufo-layer-sub";
        sub.style.padding = "4px 6px 2px";
        sub.textContent = `— ${page.name} (${layers.length}) —`;
        layerList.appendChild(sub);
      }
      layers.forEach((layer, li) => {
        const isVis = layerVis[pi][li];
        const row = document.createElement("div");
        row.className = `ufo-layer-row ${isVis ? "" : "is-hidden"}`;
        row.style.paddingLeft = `${6 + Math.min(5, Number(layer.depth) || 0) * 12}px`;

        const eyeBtn = document.createElement("button");
        eyeBtn.type = "button";
        eyeBtn.className = `ufo-eye-btn ${isVis ? "" : "is-off"}`;
        eyeBtn.title = isVis ? "Hide layer" : "Show layer";
        eyeBtn.innerHTML = `<i class="bi ${isVis ? "bi-eye" : "bi-eye-slash"}"></i>`;
        eyeBtn.addEventListener("click", (e) => {
          e.stopPropagation();
          const next = !layerVis[pi][li];
          layerVis[pi][li] = next;
          const kind = String(layer.kind || "");
          if (kind === "Group" || kind === "Frame" || kind === "Stack") {
            const baseDepth = Number(layer.depth) || 0;
            for (let ci = li + 1; ci < layers.length; ci++) {
              if ((Number(layers[ci].depth) || 0) <= baseDepth) break;
              layerVis[pi][ci] = next;
            }
          }
          if (hasDrawables[pi]) {
            showComposite[pi] = false;
          }
          renderCanvas();
          renderSidePanel();
        });

        const meta = document.createElement("div");
        meta.className = "ufo-layer-meta";
        const nm = document.createElement("div");
        nm.className = "ufo-layer-name";
        nm.textContent = layer.name || `Layer ${li + 1}`;
        const sub = document.createElement("div");
        sub.className = "ufo-layer-sub";
        sub.textContent = `${layer.kind || "Layer"} · ${Math.round(layer.width || 0)}×${Math.round(layer.height || 0)}`;
        meta.appendChild(nm);
        meta.appendChild(sub);

        row.appendChild(eyeBtn);
        row.appendChild(meta);

        row.addEventListener("mouseenter", () => highlightLayer(pi, li));
        row.addEventListener("mouseleave", () => highlightLayer(pi, null));
        row.addEventListener("click", () => eyeBtn.click());

        rowRefs.set(`${pi}:${li}`, row);
        layerList.appendChild(row);
      });
    }

    if (v.note) {
      const noteEl = document.createElement("div");
      noteEl.className = "ufo-layer-sub";
      noteEl.style.padding = "8px 6px";
      noteEl.textContent = v.note;
      layerList.appendChild(noteEl);
    }

    layerSec.appendChild(layerList);
    side.appendChild(layerSec);
  }

  renderCanvas();
  renderSidePanel();
  return () => {
    frameRefs.clear();
    hlRefs.clear();
    scaleRefs.clear();
    rowRefs.clear();
  };
}

function renderTextView(host, tab, v) {
  if (v.notebook_cells) {
    const wrap = document.createElement("div");
    wrap.className = "ufo-doc-view";
    for (const cell of v.notebook_cells) {
      const card = document.createElement("div");
      card.className = "ufo-doc-section";
      const exec = cell.execution_count != null ? ` [${cell.execution_count}]` : "";
      card.innerHTML = `<h3>${escapeHtml(cell.cell_type.toUpperCase())}${exec}</h3><pre style="font-family:var(--mono);font-size:12.5px;">${escapeHtml(cell.source)}</pre>`;
      if (cell.outputs?.length) {
        const out = document.createElement("pre");
        out.style.cssText = "border-top:1px solid var(--border);background:var(--bg);color:var(--tok-str);font-family:var(--mono);font-size:12px;";
        out.textContent = cell.outputs.join("\n");
        card.appendChild(out);
      }
      wrap.appendChild(card);
    }
    host.appendChild(wrap);
    return;
  }

  // HTML Rendered vs Source Code toggle (Item 8)
  if (v.raw_html) {
    const bar = document.createElement("div");
    bar.className = "ufo-sheet-tabs";
    bar.innerHTML = `
      <button type="button" class="ufo-tool-btn ${!tab.htmlRaw ? "active" : ""}" id="btn-html-ren">RENDERED HTML</button>
      <button type="button" class="ufo-tool-btn ${tab.htmlRaw ? "active" : ""}" id="btn-html-src">HTML SOURCE</button>
    `;
    host.appendChild(bar);
    bar.querySelector("#btn-html-ren").addEventListener("click", () => {
      tab.htmlRaw = false;
      renderStage();
    });
    bar.querySelector("#btn-html-src").addEventListener("click", () => {
      tab.htmlRaw = true;
      renderStage();
    });

    if (!tab.htmlRaw) {
      const frame = document.createElement("iframe");
      frame.setAttribute("sandbox", "allow-forms allow-modals allow-popups allow-scripts");
      frame.style.cssText = "width:100%;height:calc(100% - 42px);border:none;background:#ffffff;display:block;";
      frame.srcdoc = v.raw_html;
      host.appendChild(frame);
      return;
    }
  }

  if (v.markdown_html && !tab.mdRaw) {
    const bar = document.createElement("div");
    bar.className = "ufo-sheet-tabs";
    bar.innerHTML = `
      <button type="button" class="ufo-tool-btn active">RENDERED</button>
      <button type="button" class="ufo-tool-btn" id="btn-md-raw">RAW SOURCE</button>
    `;
    const body = document.createElement("div");
    body.className = "ufo-doc-view ufo-md-body";
    body.innerHTML = v.markdown_html;

    // Resolve local / relative same-folder images cleanly across Tauri asset URLs and browser file drops
    body.querySelectorAll("img.md-img").forEach((img) => {
      const rawSrc = img.getAttribute("data-ufo-src") || img.getAttribute("src") || "";
      const resolved = resolveMarkdownAssetUrl(tab, rawSrc);
      if (resolved) img.src = resolved;
      img.loading = "lazy";
      img.addEventListener("error", () => {
        if (img.dataset.missingHandled) return;
        img.dataset.missingHandled = "1";
        const badge = document.createElement("span");
        badge.className = "md-img-missing";
        const label = img.alt ? `${img.alt} (${rawSrc})` : rawSrc;
        badge.innerHTML = `<i class="bi bi-image"></i> <span>${escapeHtml(label)}</span>`;
        img.replaceWith(badge);
      });
    });

    // Smooth in-document heading anchor links and safe link handling
    body.addEventListener("click", (e) => {
      const link = e.target.closest("a.md-link");
      if (!link) return;
      const href = link.getAttribute("data-ufo-href") || link.getAttribute("href") || "";
      if (href.startsWith("#")) {
        e.preventDefault();
        const target = body.querySelector(`[id="${CSS.escape(href.slice(1))}"]`);
        if (target) target.scrollIntoView({ behavior: "smooth", block: "start" });
      }
    });

    host.appendChild(bar);
    host.appendChild(body);
    bar.querySelector("#btn-md-raw").addEventListener("click", () => {
      tab.mdRaw = true;
      renderStage();
    });
    return;
  }

  if (v.markdown_html && tab.mdRaw) {
    const bar = document.createElement("div");
    bar.className = "ufo-sheet-tabs";
    bar.innerHTML = `
      <button type="button" class="ufo-tool-btn" id="btn-md-ren">RENDERED</button>
      <button type="button" class="ufo-tool-btn active">RAW SOURCE</button>
    `;
    host.appendChild(bar);
    bar.querySelector("#btn-md-ren").addEventListener("click", () => {
      tab.mdRaw = false;
      renderStage();
    });
  }

  const lines = tab.loadedLines || v.lines;
  const wrap = document.createElement("div");
  wrap.className = "ufo-code-view";
  const frag = document.createDocumentFragment();
  for (const line of lines) {
    const row = document.createElement("div");
    row.className = "ufo-code-line";
    const ln = document.createElement("span");
    ln.className = "ufo-ln";
    ln.textContent = line.line_no;
    const lc = document.createElement("span");
    lc.className = "ufo-lc";
    for (const tok of line.tokens) {
      if (tok.kind === "plain") {
        lc.appendChild(document.createTextNode(tok.text));
      } else {
        const sp = document.createElement("span");
        sp.className = `tok-${tok.kind}`;
        sp.textContent = tok.text;
        lc.appendChild(sp);
      }
    }
    row.appendChild(ln);
    row.appendChild(lc);
    frag.appendChild(row);
  }
  wrap.appendChild(frag);

  if (lines.length < v.total_lines) {
    const more = document.createElement("div");
    more.style.cssText = "padding:16px;text-align:center;";
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "ufo-tool-btn";
    btn.textContent = `LOAD NEXT 500 LINES (${lines.length} / ${v.total_lines})`;
    btn.addEventListener("click", async () => {
      const next = await loadMoreLines(tab, lines.length, 500, v.language);
      if (next?.lines?.length) {
        tab.loadedLines = [...lines, ...next.lines];
        renderStage();
      }
    });
    more.appendChild(btn);
    wrap.appendChild(more);
  }

  host.appendChild(wrap);
}

async function loadMoreLines(tab, startLine, count, lang) {
  if (Tauri && tab.sessionId) {
    return await Tauri.core.invoke("fetch_text_page", {
      sessionId: tab.sessionId,
      startLine,
      lineCount: count,
      lang,
    });
  }
  if (tab.bytes) {
    return await callWorker("text", { bytes: tab.bytes, startLine, lineCount: count, lang });
  }
  return null;
}

function renderTableView(host, tab, v) {
  const wrap = document.createElement("div");
  wrap.className = "ufo-table-wrap";

  const bar = document.createElement("div");
  bar.className = "ufo-sheet-tabs";
  v.sheets.forEach((s, i) => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = `ufo-tool-btn ${i === tab.activeSheet ? "active" : ""}`;
    b.textContent = `${s.name} (${s.total_rows})`;
    b.addEventListener("click", () => {
      tab.activeSheet = i;
      renderStage();
    });
    bar.appendChild(b);
  });

  const filter = document.createElement("input");
  filter.type = "search";
  filter.placeholder = "Filter rows...";
  filter.style.cssText = "margin-left:auto;background:var(--bg);color:var(--ice);border:1px solid var(--border);padding:3px 8px;font-family:var(--mono);font-size:11px;";
  bar.appendChild(filter);
  wrap.appendChild(bar);

  const scroll = document.createElement("div");
  scroll.className = "ufo-table-scroll";
  const sheet = v.sheets[tab.activeSheet] || v.sheets[0];

  const drawRows = (q) => {
    const ql = q.trim().toLowerCase();
    const tbl = document.createElement("table");
    tbl.className = "ufo-table";
    const thead = document.createElement("thead");
    const hr = document.createElement("tr");
    hr.innerHTML = `<th>#</th>${sheet.columns.map((c) => `<th>${escapeHtml(c)}</th>`).join("")}`;
    thead.appendChild(hr);
    tbl.appendChild(thead);

    const tbody = document.createElement("tbody");
    let shown = 0;
    for (let rIdx = 0; rIdx < sheet.rows.length && shown < 1000; rIdx++) {
      const row = sheet.rows[rIdx];
      if (ql && !row.some((cell) => cell.toLowerCase().includes(ql))) continue;
      const tr = document.createElement("tr");
      tr.innerHTML = `<td style="color:var(--muted)">${rIdx + 1}</td>${sheet.columns
        .map((_, cIdx) => `<td title="${escapeHtml(row[cIdx] ?? "")}">${escapeHtml(row[cIdx] ?? "")}</td>`)
        .join("")}`;
      tbody.appendChild(tr);
      shown++;
    }
    tbl.appendChild(tbody);
    scroll.innerHTML = "";
    scroll.appendChild(tbl);
  };

  drawRows("");
  filter.addEventListener("input", (e) => drawRows(e.target.value));
  wrap.appendChild(scroll);
  host.appendChild(wrap);
}

async function renderDocumentView(host, tab, v) {
  if (v.use_native_pdf_blob) {
    const bar = document.createElement("div");
    bar.className = "ufo-sheet-tabs";
    bar.innerHTML = `
      <button type="button" class="ufo-tool-btn active" id="pdf-btn-visual">VISUAL PDF</button>
      <button type="button" class="ufo-tool-btn" id="pdf-btn-text">EXTRACTED TEXT (${v.page_count} pages)</button>
    `;
    const body = document.createElement("div");
    body.style.cssText = "flex:1 1 auto;height:calc(100% - 42px);overflow:auto;";
    host.appendChild(bar);
    host.appendChild(body);

    const showVisual = async () => {
      body.innerHTML = "";
      const url = tab.streamUrl || (await getOrCreateBlobUrl(tab, "application/pdf"));
      if (url) {
        const frame = document.createElement("embed");
        frame.src = url;
        frame.type = "application/pdf";
        frame.style.cssText = "width:100%;height:100%;border:none;display:block;background:#111;";
        body.appendChild(frame);
      } else {
        showText();
      }
    };
    const showText = () => {
      body.innerHTML = `<div class="ufo-doc-view"><div class="ufo-doc-section"><h3>${escapeHtml(v.doc_type)}</h3><pre>${escapeHtml(v.extracted_text || "PDF contains rasterized/compressed streams — switch to Visual PDF or Hex.")}</pre></div></div>`;
    };

    bar.querySelector("#pdf-btn-visual").addEventListener("click", (e) => {
      e.target.classList.add("active");
      bar.querySelector("#pdf-btn-text").classList.remove("active");
      showVisual();
    });
    bar.querySelector("#pdf-btn-text").addEventListener("click", (e) => {
      e.target.classList.add("active");
      bar.querySelector("#pdf-btn-visual").classList.remove("active");
      showText();
    });
    showVisual();
    return;
  }

  const wrap = document.createElement("div");
  wrap.className = "ufo-doc-view";
  if (v.title) {
    const h = document.createElement("h2");
    h.textContent = v.title;
    wrap.appendChild(h);
  }
  for (const sec of v.sections) {
    const card = document.createElement("div");
    card.className = "ufo-doc-section";
    card.innerHTML = `<h3>${escapeHtml(sec.title)}</h3><pre>${escapeHtml(sec.body)}</pre>`;
    wrap.appendChild(card);
  }
  host.appendChild(wrap);
}

function renderArchiveView(host, tab, v) {
  const wrap = document.createElement("div");
  wrap.className = "ufo-table-wrap";

  const bar = document.createElement("div");
  bar.className = "ufo-sheet-tabs";
  bar.innerHTML = `<span class="ufo-tool-btn active">${escapeHtml(v.archive_type)} · ${v.total_entries} items (${formatBytes(v.total_uncompressed)})</span>`;
  const search = document.createElement("input");
  search.type = "search";
  search.placeholder = "Filter archive entries...";
  search.style.cssText = "margin-left:auto;background:var(--bg);color:var(--ice);border:1px solid var(--border);padding:3px 8px;font-family:var(--mono);font-size:11px;";
  bar.appendChild(search);
  wrap.appendChild(bar);

  const scroll = document.createElement("div");
  scroll.className = "ufo-table-scroll";

  const draw = (q) => {
    const ql = q.trim().toLowerCase();
    const tbl = document.createElement("table");
    tbl.className = "ufo-table";
    tbl.innerHTML = `<thead><tr><th>Path</th><th>Size</th><th>Packed</th><th>Method</th><th>Action</th></tr></thead>`;
    const tbody = document.createElement("tbody");
    let count = 0;
    for (const entry of v.entries) {
      if (ql && !entry.path.toLowerCase().includes(ql)) continue;
      if (count++ >= 1000) break;
      const tr = document.createElement("tr");
      const icon = entry.is_dir ? "bi-folder" : "bi-file-earmark";
      tr.innerHTML = `
        <td><i class="bi ${icon}"></i> ${escapeHtml(entry.path)}</td>
        <td>${entry.is_dir ? "-" : formatBytes(entry.size)}</td>
        <td>${entry.is_dir ? "-" : formatBytes(entry.compressed_size)}</td>
        <td>${escapeHtml(entry.method)}</td>
        <td>${entry.previewable ? `<button type="button" class="ufo-archive-entry-btn" data-entry="${escapeHtml(entry.path)}">OPEN</button>` : "-"}</td>
      `;
      tbody.appendChild(tr);
    }
    tbl.appendChild(tbody);
    scroll.innerHTML = "";
    scroll.appendChild(tbl);
  };

  draw("");
  search.addEventListener("input", (e) => draw(e.target.value));
  scroll.addEventListener("click", async (e) => {
    const btn = e.target.closest(".ufo-archive-entry-btn");
    if (!btn) return;
    const entryPath = btn.dataset.entry;
    try {
      if (Tauri && tab.sessionId) {
        const res = await Tauri.core.invoke("open_archive_entry_session", {
          sessionId: tab.sessionId,
          entryPath,
        });
        addTabFromSession(res.session_id, res.stream_url, res.report, null);
      } else if (tab.bytes) {
        const entryBytes = await callWorker("extract", { bytes: tab.bytes, entryPath });
        const name = entryPath.split("/").pop() || entryPath;
        await openRawBytes(name, entryBytes, entryBytes.length);
      }
    } catch (err) {
      showToastError(`Failed to extract ${entryPath}: ${err}`);
    }
  });

  wrap.appendChild(scroll);
  host.appendChild(wrap);
}

async function renderMediaView(host, tab, v) {
  if (v.media_kind === "Subtitle") {
    const wrap = document.createElement("div");
    wrap.className = "ufo-table-wrap";
    const tbl = document.createElement("table");
    tbl.className = "ufo-table";
    tbl.innerHTML = `<thead><tr><th>#</th><th>Start</th><th>End</th><th>Text</th></tr></thead>`;
    const tbody = document.createElement("tbody");
    for (const c of v.subtitles.slice(0, 1500)) {
      const tr = document.createElement("tr");
      tr.innerHTML = `<td>${c.index}</td><td>${escapeHtml(c.start)}</td><td>${escapeHtml(c.end)}</td><td style="white-space:normal">${escapeHtml(c.text)}</td>`;
      tbody.appendChild(tr);
    }
    tbl.appendChild(tbody);
    wrap.appendChild(tbl);
    host.appendChild(wrap);
    return;
  }

  if (v.media_kind === "Midi") {
    const stage = document.createElement("div");
    stage.className = "ufo-media-stage";
    stage.innerHTML = `
      <div class="ufo-audio-card" style="width:min(880px,95vw)">
        <div class="ufo-card-label">MIDI PIANO-ROLL SEQUENCE</div>
        <h3 style="margin:0 0 12px">${escapeHtml(v.codec_summary)}</h3>
        <div style="border:1px solid var(--border);background:#000;padding:10px;">${v.svg_preview || ""}</div>
      </div>
    `;
    host.appendChild(stage);
    return;
  }

  const stage = document.createElement("div");
  stage.className = "ufo-media-stage";

  // Stream video/audio straight from Tauri Range HTTP URI (zero-copy mmap!) or Blob URL
  const mediaUrl = v.use_native_player
    ? (tab.streamUrl || (await getOrCreateBlobUrl(tab, tab.report.detected.mime)))
    : null;

  // Guard against race if user already closed or switched tabs while awaiting
  if (tab.id !== state.activeTabId) return;

  if (v.media_kind === "Video") {
    let videoEl = null;
    if (mediaUrl) {
      videoEl = document.createElement("video");
      videoEl.src = mediaUrl;
      videoEl.controls = true;
      videoEl.playsInline = true;
      videoEl.style.cssText = "width:100%;height:100%;max-width:100%;max-height:100%;object-fit:contain;background:#000;";
      stage.style.padding = "0";
      stage.appendChild(videoEl);
    }
    const info = document.createElement("div");
    info.className = "ufo-float-toolbar";
    info.style.bottom = "58px";
    info.innerHTML = `<span class="ufo-tool-btn">${escapeHtml(v.container)} · ${escapeHtml(v.codec_summary)}${v.dimensions ? ` · ${v.dimensions[0]}×${v.dimensions[1]}` : ""}</span>`;
    stage.appendChild(info);
    host.appendChild(stage);
    tab.cleanup = () => stopMediaElement(videoEl);
    return;
  }

  // Audio card
  const card = document.createElement("div");
  card.className = "ufo-audio-card";
  const coverHtml = v.cover_art_data_url
    ? `<img src="${v.cover_art_data_url}" alt="Cover" style="width:96px;height:96px;object-fit:cover;border:1px solid var(--ice);margin-bottom:14px;"/>`
    : "";
  const waveHtml = v.waveform_peaks?.length
    ? `<div class="ufo-waveform">${v.waveform_peaks
        .map((p) => `<div class="ufo-wave-bar" style="height:${Math.max(8, Math.round(p * 100))}%"></div>`)
        .join("")}</div>`
    : "";
  const dur = v.duration_secs ? ` · ${v.duration_secs.toFixed(1)}s` : "";
  card.innerHTML = `
    ${coverHtml}
    <div class="ufo-card-label">${escapeHtml(v.container)}</div>
    <h3 style="margin:0 0 6px;font-size:18px">${escapeHtml(tab.fileName)}</h3>
    <div style="font-family:var(--mono);font-size:12px;color:var(--muted);margin-bottom:14px">${escapeHtml(v.codec_summary)}${dur}</div>
    ${waveHtml}
  `;
  let audioEl = null;
  if (mediaUrl) {
    audioEl = document.createElement("audio");
    audioEl.src = mediaUrl;
    audioEl.controls = true;
    audioEl.style.width = "100%";
    card.appendChild(audioEl);
  }
  stage.appendChild(card);
  host.appendChild(stage);
  tab.cleanup = () => stopMediaElement(audioEl);
}

// Immediately stops audio/video decoder threads when closing tab or switching views (Item 11)
function stopMediaElement(el) {
  if (!el) return;
  try {
    el.pause();
    el.currentTime = 0;
    el.removeAttribute("src");
    el.srcObject = null;
    el.load();
  } catch (_) {}
}

async function renderFontView(host, tab, v) {
  const wrap = document.createElement("div");
  wrap.className = "ufo-font-specimen";
  const fontId = `ufo-font-${tab.id.replace(/[^a-z0-9]/gi, "")}`;
  const url = tab.streamUrl || (await getOrCreateBlobUrl(tab, tab.report.detected.mime));
  if (url) {
    const style = document.createElement("style");
    style.textContent = `@font-face { font-family: '${fontId}'; src: url('${url}'); }`;
    wrap.appendChild(style);
  }

  const head = document.createElement("div");
  head.innerHTML = `
    <span class="ufo-badge">${escapeHtml(v.subfamily.toUpperCase())} · ${v.glyph_count} GLYPHS · ${v.upem} UPEM</span>
    <h2 style="margin:0 0 14px">${escapeHtml(v.family)}</h2>
    <div style="margin-bottom:18px">${v.preview_glyphs_svg || ""}</div>
    <div style="font-family:var(--mono);font-size:11px;color:var(--muted);margin-bottom:10px">LIVE SPECIMEN WATERFALL</div>
    <div style="font-family:'${fontId}',var(--sans);border:1px solid var(--border);padding:18px;background:var(--surface)">
      <div style="font-size:40px;margin-bottom:10px">The quick brown fox jumps over 0123456789</div>
      <div style="font-size:22px;margin-bottom:10px">ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz</div>
      <div style="font-size:14px;color:var(--ice-dim)">!@#$%^&amp;*()_+-=[]{}|;':",./&lt;&gt;?</div>
    </div>
    <div style="margin-top:16px;display:flex;flex-wrap:wrap;gap:6px;">
      ${v.tables.map((t) => `<span class="ufo-tool-btn">${escapeHtml(t)}</span>`).join("")}
    </div>
  `;
  wrap.appendChild(head);
  host.appendChild(wrap);
}

function renderBinaryView(host, v) {
  const wrap = document.createElement("div");
  wrap.className = "ufo-doc-view";
  wrap.innerHTML = `
    <span class="ufo-badge">${escapeHtml(v.endianness.toUpperCase())}</span>
    <h2 style="margin:0 0 14px">${escapeHtml(v.arch_or_format)}</h2>
    ${v.entry_point ? `<div style="font-family:var(--mono);margin-bottom:16px">Entry Point: <strong>${escapeHtml(v.entry_point)}</strong></div>` : ""}
  `;

  if (v.sections.length) {
    const secCard = document.createElement("div");
    secCard.className = "ufo-doc-section";
    secCard.innerHTML = `<h3>SECTIONS (${v.sections.length})</h3>`;
    const tbl = document.createElement("table");
    tbl.className = "ufo-table";
    tbl.innerHTML = `<thead><tr><th>Name</th><th>Virtual Addr</th><th>Raw Offset</th><th>Size</th><th>Flags</th></tr></thead><tbody>${v.sections
      .map(
        (s) =>
          `<tr><td>${escapeHtml(s.name)}</td><td>${escapeHtml(s.virtual_addr)}</td><td>${escapeHtml(s.raw_offset)}</td><td>${formatBytes(s.size)}</td><td>${escapeHtml(s.flags)}</td></tr>`
      )
      .join("")}</tbody>`;
    secCard.appendChild(tbl);
    wrap.appendChild(secCard);
  }

  if (v.imports.length || v.exports.length) {
    const symCard = document.createElement("div");
    symCard.className = "ufo-doc-section";
    symCard.innerHTML = `<h3>IMPORTS / EXPORTS</h3><pre style="font-family:var(--mono);font-size:12px">${escapeHtml([...v.imports, ...v.exports].join("\n"))}</pre>`;
    wrap.appendChild(symCard);
  }
  host.appendChild(wrap);
}

function renderStructuredView(host, v) {
  const wrap = document.createElement("div");
  wrap.className = "ufo-doc-view";
  if (v.tree_nodes.length) {
    const card = document.createElement("div");
    card.className = "ufo-doc-section";
    card.innerHTML = `<h3>${escapeHtml(v.format_label)} — STRUCTURE TREE</h3>`;
    const list = document.createElement("div");
    list.style.cssText = "padding:10px 14px;font-family:var(--mono);font-size:12px;";
    list.innerHTML = v.tree_nodes
      .map(
        (n) =>
          `<div style="padding-left:${n.depth * 16}px;margin:3px 0"><span class="tok-kw">${escapeHtml(n.key)}:</span> <span class="tok-str">${escapeHtml(n.value_preview)}</span></div>`
      )
      .join("");
    card.appendChild(list);
    wrap.appendChild(card);
  }

  const raw = document.createElement("div");
  raw.className = "ufo-doc-section";
  raw.innerHTML = `<h3>FORMATTED SOURCE</h3><pre style="font-family:var(--mono);font-size:12px">${escapeHtml(v.formatted_text)}</pre>`;
  wrap.appendChild(raw);
  host.appendChild(wrap);
}

function renderHexView(host, tab, note) {
  const wrap = document.createElement("div");
  wrap.className = "ufo-table-wrap";

  const bar = document.createElement("div");
  bar.className = "ufo-sheet-tabs";
  const offHex = "0x" + tab.hexOffset.toString(16).toUpperCase().padStart(8, "0");
  bar.innerHTML = `
    ${note ? `<span class="ufo-tool-btn" style="cursor:default">${escapeHtml(note)}</span>` : ""}
    <span class="ufo-tool-btn active">OFFSET ${offHex}</span>
    <button type="button" class="ufo-tool-btn" id="hex-prev">PREV 4KB</button>
    <button type="button" class="ufo-tool-btn" id="hex-next">NEXT 4KB</button>
    <input type="text" id="hex-jump" placeholder="Jump hex e.g. 0x1000" style="margin-left:auto;background:var(--bg);color:var(--ice);border:1px solid var(--border);padding:3px 8px;font-family:var(--mono);font-size:11px;width:165px;">
  `;
  wrap.appendChild(bar);

  const table = document.createElement("div");
  table.className = "ufo-hex-table ufo-table-scroll";

  const drawHex = (rows) => {
    table.innerHTML = rows
      .map((row) => {
        const off = row.offset.toString(16).toUpperCase().padStart(8, "0");
        const bytesHex = row.bytes.map((b) => b.toString(16).toUpperCase().padStart(2, "0")).join(" ");
        return `<div class="ufo-hex-row"><span class="ufo-hex-off">${off}</span><span class="ufo-hex-bytes">${bytesHex}</span><span class="ufo-hex-ascii">${escapeHtml(row.ascii)}</span></div>`;
      })
      .join("");
  };

  drawHex(tab.report.hex_preview);

  const loadAt = async (offset) => {
    const clean = Math.max(0, Math.min(Math.max(tab.report.file_size - 16, 0), offset));
    let page = null;
    if (Tauri && tab.sessionId) {
      page = await Tauri.core.invoke("fetch_hex_page", { sessionId: tab.sessionId, offset: clean, length: 4096 });
    } else if (tab.bytes) {
      page = await callWorker("hex", { bytes: tab.bytes, offset: clean, length: 4096 });
    }
    if (page) {
      tab.hexOffset = page.offset;
      bar.querySelector(".ufo-tool-btn.active").textContent = "OFFSET 0x" + page.offset.toString(16).toUpperCase().padStart(8, "0");
      drawHex(page.rows);
    }
  };

  bar.querySelector("#hex-prev").addEventListener("click", () => loadAt(tab.hexOffset - 4096));
  bar.querySelector("#hex-next").addEventListener("click", () => loadAt(tab.hexOffset + 4096));
  bar.querySelector("#hex-jump").addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      const val = parseInt(e.target.value.trim(), 16);
      if (!Number.isNaN(val)) loadAt(val);
    }
  });

  wrap.appendChild(table);
  host.appendChild(wrap);
}

function renderStringsView(host, strings) {
  const wrap = document.createElement("div");
  wrap.className = "ufo-table-wrap";
  const scroll = document.createElement("div");
  scroll.className = "ufo-table-scroll";
  const tbl = document.createElement("table");
  tbl.className = "ufo-table";
  tbl.innerHTML = `<thead><tr><th>Offset</th><th>Encoding</th><th>String</th></tr></thead><tbody>${strings
    .map(
      (s) =>
        `<tr><td>0x${s.offset.toString(16).toUpperCase()}</td><td>${escapeHtml(s.encoding)}</td><td style="white-space:normal">${escapeHtml(s.value)}</td></tr>`
    )
    .join("")}</tbody>`;
  scroll.appendChild(tbl);
  wrap.appendChild(scroll);
  host.appendChild(wrap);
}

// ─── Information Drawer, Re-Save Modal, Formats Catalog ──────────────────────
function toggleInfoDrawer(force) {
  state.infoOpen = typeof force === "boolean" ? force : !state.infoOpen;
  els.infoDrawer.classList.toggle("hidden", !state.infoOpen);
  els.actInfo.classList.toggle("active", state.infoOpen);
  if (state.infoOpen) renderInfoDrawer();
}

function normalizeRelativeAssetPath(rawSrc) {
  let clean = String(rawSrc || "").trim().split(/[?#]/)[0];
  try {
    clean = decodeURIComponent(clean);
  } catch (_) {}
  clean = clean.replace(/\\/g, "/").replace(/^\.\//, "").replace(/^\/+/, "");
  const parts = [];
  for (const seg of clean.split("/")) {
    if (!seg || seg === ".") continue;
    if (seg === "..") {
      if (parts.length > 0 && parts[parts.length - 1] !== "..") parts.pop();
      else parts.push("..");
    } else {
      parts.push(seg);
    }
  }
  return parts.join("/");
}

function resolveMarkdownAssetUrl(tab, rawSrc) {
  if (!rawSrc) return null;
  if (/^(https?:|data:|blob:|ufo:|http:\/\/ufo\.localhost|file:|\/\/)/i.test(rawSrc)) {
    return rawSrc;
  }
  const rel = normalizeRelativeAssetPath(rawSrc);
  if (!rel) return null;
  if (tab.assetBaseUrl && !rel.startsWith("..")) {
    const encoded = rel.split("/").map((s) => encodeURIComponent(s)).join("/");
    return `${tab.assetBaseUrl}${encoded}`;
  }
  const lowerRel = rel.toLowerCase();
  if (state.localAssets.has(lowerRel)) return state.localAssets.get(lowerRel);
  const baseName = lowerRel.split("/").pop();
  if (baseName && state.localAssets.has(baseName)) return state.localAssets.get(baseName);
  // Check open image tabs in memory as additional convenience fallback
  for (const t of state.tabs) {
    if (t.fileName && t.fileName.toLowerCase() === baseName && t.bytes) {
      const mime = t.report?.detected?.mime || "application/octet-stream";
      if (!t.blobUrl) t.blobUrl = URL.createObjectURL(new Blob([t.bytes], { type: mime }));
      return t.blobUrl;
    }
  }
  return null;
}

function renderInfoDrawer() {
  const tab = getActiveTab();
  if (!tab) return;
  const r = tab.report;
  const det = r.detected;
  const h = r.hashes;
  const ext = (tab.fileName.includes(".") ? tab.fileName.split(".").pop() : det.id || "FILE").toUpperCase();

  const groups = new Map();
  for (const m of r.metadata) {
    if (!groups.has(m.section)) groups.set(m.section, []);
    groups.get(m.section).push(m);
  }

  const maxBin = Math.max(...h.histogram_64, 1);
  const confidencePretty = String(det.confidence || "")
    .replace(/([a-z])([A-Z])/g, "$1 $2");
  const parseMs = (r.parse_time_us / 1000).toFixed(2);
  const magicShort = det.magic_hex ? det.magic_hex.split(" ").slice(0, 6).join(" ") : "NONE (TEXT)";

  let html = `
    <div class="ufo-info-hero">
      <div class="ufo-info-hero-top">
        <span class="ufo-info-ext-pill">${escapeHtml(ext)} · ${escapeHtml(det.name)}</span>
        <span class="ufo-info-conf-pill">${escapeHtml(confidencePretty)}</span>
      </div>
      <div class="ufo-info-filename" title="${escapeHtml(tab.fileName)}">${escapeHtml(tab.fileName)}</div>
      <div class="ufo-info-subline">
        <span>${formatBytes(r.file_size)} (${r.file_size.toLocaleString()} B)</span>
        <span>·</span>
        <span>${escapeHtml(det.mime)}</span>
        <span>·</span>
        <span>${parseMs} ms</span>
      </div>
    </div>

    <div class="ufo-info-metrics">
      <div class="ufo-info-metric" title="Shannon entropy in bits per byte (8.0 max)">
        <span class="ufo-info-metric-k">ENTROPY</span>
        <span class="ufo-info-metric-v">${Number(h.entropy_bits || 0).toFixed(2)} b/B</span>
      </div>
      <div class="ufo-info-metric" title="Printable ASCII ratio across sampled bytes">
        <span class="ufo-info-metric-k">PRINTABLE</span>
        <span class="ufo-info-metric-v">${(h.printable_ratio * 100).toFixed(1)}%</span>
      </div>
      <div class="ufo-info-metric" title="${escapeHtml(det.magic_hex || 'No binary magic signature')}">
        <span class="ufo-info-metric-k">MAGIC</span>
        <span class="ufo-info-metric-v">${escapeHtml(magicShort)}</span>
      </div>
    </div>

    <div class="ufo-kv-group">
      <div class="ufo-kv-group-title">
        <span>BYTE DISTRIBUTION (0x00 → 0xFF)</span>
        <span class="ufo-kv-count">64 BINS</span>
      </div>
      <div class="ufo-entropy-bars">
        ${h.histogram_64.map((v, idx) => {
          const lo = (idx * 4).toString(16).padStart(2, "0").toUpperCase();
          const hi = (idx * 4 + 3).toString(16).padStart(2, "0").toUpperCase();
          return `<div class="ufo-entropy-bar" title="0x${lo}–0x${hi}: ${v.toLocaleString()} bytes" style="height:${Math.max(4, Math.round((v / maxBin) * 100))}%"></div>`;
        }).join("")}
      </div>
    </div>

    <div class="ufo-kv-group">
      <div class="ufo-kv-group-title">
        <span>CRYPTOGRAPHIC DIGESTS</span>
        <span class="ufo-kv-count">CRC / SHA</span>
      </div>
      <div class="ufo-hash-row">
        <span class="ufo-kv-k">CRC-32</span>
        <span class="ufo-hash-val">${escapeHtml(h.crc32)}</span>
        <button type="button" class="ufo-copy-mini" data-copy="${escapeHtml(h.crc32)}" title="Copy CRC-32"><i class="bi bi-copy"></i></button>
      </div>
      <div class="ufo-hash-row">
        <span class="ufo-kv-k">SHA-1</span>
        <span class="ufo-hash-val">${escapeHtml(h.sha1)}</span>
        <button type="button" class="ufo-copy-mini" data-copy="${escapeHtml(h.sha1)}" title="Copy SHA-1"><i class="bi bi-copy"></i></button>
      </div>
      <div class="ufo-hash-row">
        <span class="ufo-kv-k">SHA-256</span>
        <span class="ufo-hash-val">${escapeHtml(h.sha256)}</span>
        <button type="button" class="ufo-copy-mini" data-copy="${escapeHtml(h.sha256)}" title="Copy SHA-256"><i class="bi bi-copy"></i></button>
      </div>
    </div>

    <div class="ufo-kv-group">
      <div class="ufo-kv-group-title">
        <span>DETECTION &amp; SIGNATURE</span>
        <span class="ufo-kv-count">CORE</span>
      </div>
      <div class="ufo-kv-row"><div class="ufo-kv-k">Basis</div><div class="ufo-kv-v">${escapeHtml(det.detection_basis)}</div></div>
      <div class="ufo-kv-row"><div class="ufo-kv-k">Magic Bytes</div><div class="ufo-kv-v">${escapeHtml(det.magic_hex || "(empty)")}</div></div>
      <div class="ufo-kv-row"><div class="ufo-kv-k">Parse Time</div><div class="ufo-kv-v">${parseMs} ms (${r.parse_time_us.toLocaleString()} µs)</div></div>
    </div>
  `;

  for (const [sec, items] of groups.entries()) {
    html += `
      <div class="ufo-kv-group">
        <div class="ufo-kv-group-title">
          <span>${escapeHtml(sec.toUpperCase())}</span>
          <span class="ufo-kv-count">${items.length}</span>
        </div>
    `;
    for (const item of items) {
      html += `<div class="ufo-kv-row"><div class="ufo-kv-k">${escapeHtml(item.key)}</div><div class="ufo-kv-v">${escapeHtml(item.value)}</div></div>`;
    }
    html += `</div>`;
  }

  els.infoBody.innerHTML = html;
  els.infoBody.querySelectorAll("[data-copy]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const val = btn.dataset.copy || "";
      navigator.clipboard?.writeText(val).catch(() => {});
      btn.innerHTML = `<i class="bi bi-check2"></i>`;
      setTimeout(() => {
        btn.innerHTML = `<i class="bi bi-copy"></i>`;
      }, 1200);
    });
  });
}

function openResaveModal() {
  const tab = getActiveTab();
  if (!tab) return;
  els.modalTitle.textContent = `RE-SAVE · ${tab.fileName}`;
  els.modalBody.innerHTML = `
    <p style="color:var(--muted);margin-top:0">Export a clean copy of the opened file or save its full magic-byte &amp; structural telemetry report.</p>
    <div style="display:grid;gap:12px">
      <button type="button" class="ufo-primary-btn" id="resave-copy"><i class="bi bi-download"></i> SAVE COPY (${escapeHtml(tab.fileName)})</button>
      <button type="button" class="ufo-tool-btn" style="padding:12px" id="resave-report"><i class="bi bi-filetype-json"></i> EXPORT TELEMETRY REPORT (.ufo-report.json)</button>
    </div>
  `;
  els.modal.classList.remove("hidden");

  const doSave = async (asReport) => {
    if (Tauri && tab.sessionId && state.supportsWindowChrome) {
      await Tauri.core.invoke("resave_session_dialog", {
        sessionId: tab.sessionId,
        asReportJson: asReport,
      });
    } else {
      let bytes = tab.bytes;
      let name = tab.fileName;
      let mime = "application/octet-stream";
      if (!bytes && Tauri && tab.sessionId) {
        const raw = await Tauri.core.invoke("read_raw_bytes", { sessionId: tab.sessionId });
        bytes = new Uint8Array(raw);
      }
      if (asReport) {
        bytes = bytes
          ? await callWorker("export", { fileName: tab.fileName, bytes })
          : new TextEncoder().encode(JSON.stringify(tab.report, null, 2));
        name = `${tab.fileName}.ufo-report.json`;
        mime = "application/json";
      }
      const blob = new Blob([bytes], { type: mime });
      const a = document.createElement("a");
      a.href = URL.createObjectURL(blob);
      a.download = name;
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 1500);
    }
    els.modal.classList.add("hidden");
  };

  els.modalBody.querySelector("#resave-copy").addEventListener("click", () => doSave(false));
  els.modalBody.querySelector("#resave-report").addEventListener("click", () => doSave(true));
}

function openFormatsModal() {
  els.modalTitle.textContent = `SUPPORTED FORMATS (${state.formatsCatalog.length}+ EXTENSIONS · 23 CATEGORIES)`;
  els.modalBody.innerHTML = `
    <input type="search" id="cat-search" class="ufo-search-input" placeholder="Search extension, format, or category (e.g. step, ply, blend, sqlite, mp4)..." />
    <div id="cat-list"></div>
  `;
  els.modal.classList.remove("hidden");
  const list = els.modalBody.querySelector("#cat-list");
  const render = (q) => {
    const ql = q.trim().toLowerCase();
    const filtered = state.formatsCatalog
      .filter((f) => !ql || f.ext.includes(ql) || f.name.toLowerCase().includes(ql) || f.category.toLowerCase().includes(ql))
      .slice(0, 120);
    list.innerHTML = `<table class="ufo-table"><thead><tr><th>Ext</th><th>Format</th><th>Category</th></tr></thead><tbody>${filtered
      .map((f) => `<tr><td class="tok-kw">.${escapeHtml(f.ext)}</td><td>${escapeHtml(f.name)}</td><td style="color:var(--muted)">${escapeHtml(f.category)}</td></tr>`)
      .join("")}</tbody></table>`;
  };
  render("");
  els.modalBody.querySelector("#cat-search").addEventListener("input", (e) => render(e.target.value));
}

// ─── Utilities ────────────────────────────────────────────────────────────────
async function getOrCreateBlobUrl(tab, mime) {
  if (tab.blobUrl) return tab.blobUrl;
  let bytes = tab.bytes;
  if (!bytes && Tauri && tab.sessionId) {
    const buf = await Tauri.core.invoke("read_raw_bytes", { sessionId: tab.sessionId });
    bytes = new Uint8Array(buf);
  }
  if (!bytes) return null;
  tab.blobUrl = URL.createObjectURL(new Blob([bytes], { type: mime || "application/octet-stream" }));
  return tab.blobUrl;
}

function showToastError(msg) {
  els.warningBanner.innerHTML = `<i class="bi bi-exclamation-triangle"></i> <span>${escapeHtml(msg)}</span>`;
  els.warningBanner.classList.remove("hidden");
  els.empty.classList.add("hidden");
  els.stage.classList.remove("hidden");
}

function formatBytes(n) {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n;
  let u = -1;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return `${v.toFixed(1)} ${units[u]}`;
}

function escapeHtml(str) {
  return String(str ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

bootstrap();
