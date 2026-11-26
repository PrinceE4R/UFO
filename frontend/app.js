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
  tabs: [], // { id, sessionId, streamUrl, fileName, report, bytes, mode, cleanup, blobUrl, hexOffset, loadedLines, mdRaw, htmlRaw, activeSheet }
  activeTabId: null,
  infoOpen: false,
  isFullscreen: false,
  theme: localStorage.getItem("ufo-theme") === "light" ? "light" : "dark",
  isMobile: window.innerWidth <= 768,
  supportsWindowChrome: Boolean(Tauri),
  formatsCatalog: [],
  nextLocalId: 1,
};

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
    const files = Array.from(e.dataTransfer?.files || []);
    for (const f of files) {
      await openBrowserFile(f);
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
    addTabFromSession(res.session_id, res.stream_url, res.report, null);
  } catch (err) {
    showToastError(`Unable to open ${path}: ${err}`);
  } finally {
    setLoading(false);
  }
}

async function openBrowserFile(file) {
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
    addTabFromSession(res.session_id, res.stream_url, res.report, bytes);
    return;
  }
  const report = await callWorker("open", {
    fileName,
    bytes,
    totalSizeHint: totalSizeHint ? BigInt(totalSizeHint) : undefined,
  });
  addTabFromSession(null, null, report, bytes);
}

function addTabFromSession(sessionId, streamUrl, report, bytes) {
  const id = `tab-${state.nextLocalId++}`;
  state.tabs.push({
    id,
    sessionId,
    streamUrl,
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
    closeBtn.innerHTML = `<i class="bi bi-x-circle"></i>`;
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

function renderInfoDrawer() {
  const tab = getActiveTab();
  if (!tab) return;
  const r = tab.report;
  const groups = new Map();
  for (const m of r.metadata) {
    if (!groups.has(m.section)) groups.set(m.section, []);
    groups.get(m.section).push(m);
  }

  let html = `
    <div class="ufo-kv-group">
      <div class="ufo-kv-group-title">SIGNATURE &amp; TIMING</div>
      <div class="ufo-kv-row"><div class="ufo-kv-k">Magic Bytes</div><div class="ufo-kv-v">${escapeHtml(r.detected.magic_hex || "(empty)")}</div></div>
      <div class="ufo-kv-row"><div class="ufo-kv-k">Confidence</div><div class="ufo-kv-v">${escapeHtml(r.detected.confidence)}</div></div>
      <div class="ufo-kv-row"><div class="ufo-kv-k">Parse Time</div><div class="ufo-kv-v">${(r.parse_time_us / 1000).toFixed(2)} ms (${r.parse_time_us} µs)</div></div>
      <div class="ufo-kv-row"><div class="ufo-kv-k">Printable Ratio</div><div class="ufo-kv-v">${(r.hashes.printable_ratio * 100).toFixed(1)}%</div></div>
    </div>
  `;

  const maxBin = Math.max(...r.hashes.histogram_64, 1);
  html += `
    <div class="ufo-kv-group">
      <div class="ufo-kv-group-title">BYTE ENTROPY DISTRIBUTION (0x00 → 0xFF)</div>
      <div class="ufo-entropy-bars">
        ${r.hashes.histogram_64.map((v) => `<div class="ufo-entropy-bar" style="height:${Math.max(4, Math.round((v / maxBin) * 100))}%"></div>`).join("")}
      </div>
    </div>
  `;

  for (const [sec, items] of groups.entries()) {
    html += `<div class="ufo-kv-group"><div class="ufo-kv-group-title">${escapeHtml(sec.toUpperCase())}</div>`;
    for (const item of items) {
      html += `<div class="ufo-kv-row"><div class="ufo-kv-k">${escapeHtml(item.key)}</div><div class="ufo-kv-v">${escapeHtml(item.value)}</div></div>`;
    }
    html += `</div>`;
  }
  els.infoBody.innerHTML = html;
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
