// High-speed WebGL2 / WebGL1 Mesh, Solidcore CAD, Textured GLB/Blend, Skeletal Animation, and Splatting Viewport

function decodeB64F32(b64) {
  if (!b64) return new Float32Array(0);
  const bin = atob(b64);
  const len = bin.length;
  const bytes = new Uint8Array(len);
  for (let i = 0; i < len; i++) bytes[i] = bin.charCodeAt(i);
  return new Float32Array(bytes.buffer, 0, Math.floor(len / 4));
}

function escHtml(s) {
  return String(s ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function quatToMat4(q, p, s) {
  const [x, y, z, w] = q;
  const [sx, sy, sz] = s;
  const x2 = x + x, y2 = y + y, z2 = z + z;
  const xx = x * x2, xy = x * y2, xz = x * z2;
  const yy = y * y2, yz = y * z2, zz = z * z2;
  const wx = w * x2, wy = w * y2, wz = w * z2;
  return [
    (1 - (yy + zz)) * sx, (xy + wz) * sx, (xz - wy) * sx, 0,
    (xy - wz) * sy, (1 - (xx + zz)) * sy, (yz + wx) * sy, 0,
    (xz + wy) * sz, (yz - wx) * sz, (1 - (xx + yy)) * sz, 0,
    p[0], p[1], p[2], 1,
  ];
}

function mulMat4F64(a, b) {
  const o = new Array(16);
  for (let c = 0; c < 4; c++) {
    for (let r = 0; r < 4; r++) {
      o[c * 4 + r] =
        a[r] * b[c * 4] +
        a[4 + r] * b[c * 4 + 1] +
        a[8 + r] * b[c * 4 + 2] +
        a[12 + r] * b[c * 4 + 3];
    }
  }
  return o;
}

function sampleVec3(times, arr, t, def) {
  if (!arr || !arr.length) return def;
  if (arr.length === 1 || !times || times.length <= 1 || t <= times[0]) return arr[0];
  const last = Math.min(times.length, arr.length) - 1;
  if (t >= times[last]) return arr[last];
  let lo = 0, hi = last;
  while (lo + 1 < hi) {
    const mid = (lo + hi) >> 1;
    if (times[mid] <= t) lo = mid; else hi = mid;
  }
  const span = Math.max(times[hi] - times[lo], 1e-6);
  const a = Math.max(0, Math.min(1, (t - times[lo]) / span));
  return [
    arr[lo][0] + (arr[hi][0] - arr[lo][0]) * a,
    arr[lo][1] + (arr[hi][1] - arr[lo][1]) * a,
    arr[lo][2] + (arr[hi][2] - arr[lo][2]) * a,
  ];
}

function sampleQuat(times, arr, t, def) {
  if (!arr || !arr.length) return def;
  if (arr.length === 1 || !times || times.length <= 1 || t <= times[0]) return arr[0];
  const last = Math.min(times.length, arr.length) - 1;
  if (t >= times[last]) return arr[last];
  let lo = 0, hi = last;
  while (lo + 1 < hi) {
    const mid = (lo + hi) >> 1;
    if (times[mid] <= t) lo = mid; else hi = mid;
  }
  const span = Math.max(times[hi] - times[lo], 1e-6);
  const a = Math.max(0, Math.min(1, (t - times[lo]) / span));
  const q0 = arr[lo];
  let q1 = arr[hi];
  const dot = q0[0] * q1[0] + q0[1] * q1[1] + q0[2] * q1[2] + q0[3] * q1[3];
  if (dot < 0) q1 = [-q1[0], -q1[1], -q1[2], -q1[3]];
  const rx = q0[0] + (q1[0] - q0[0]) * a;
  const ry = q0[1] + (q1[1] - q0[1]) * a;
  const rz = q0[2] + (q1[2] - q0[2]) * a;
  const rw = q0[3] + (q1[3] - q0[3]) * a;
  const len = Math.hypot(rx, ry, rz, rw) || 1;
  return [rx / len, ry / len, rz / len, rw / len];
}

export function mountMeshViewer(container, meshData, getIsLight) {
  container.innerHTML = "";
  const layers = Array.isArray(meshData.layers) ? meshData.layers : [];
  const materials = Array.isArray(meshData.materials) ? meshData.materials : [];
  const skel = meshData.skeleton || null;
  const hasSidePanel = layers.length > 0 || materials.length > 0 || !!skel;

  const shell = document.createElement("div");
  shell.className = hasSidePanel ? "ufo-stage-split" : "ufo-media-stage";
  if (!hasSidePanel) shell.style.padding = "0";

  const mainPane = document.createElement("div");
  mainPane.className = "ufo-design-main";
  if (!hasSidePanel) {
    mainPane.style.width = "100%";
    mainPane.style.height = "100%";
  }

  const canvas = document.createElement("canvas");
  canvas.className = "ufo-3d-canvas";
  mainPane.appendChild(canvas);
  shell.appendChild(mainPane);

  const normPosRaw = decodeB64F32(meshData.positions_b64);
  const normNrm = decodeB64F32(meshData.normals_b64);
  const uvArr = decodeB64F32(meshData.uvs_b64);
  const colArr = decodeB64F32(meshData.colors_b64);
  const wireRaw = decodeB64F32(meshData.wireframe_b64);
  const ptPosRaw = decodeB64F32(meshData.point_positions_b64);
  const ptColRaw = decodeB64F32(meshData.point_colors_b64);

  const hasSolid = normPosRaw.length >= 9;
  const hasWire = wireRaw.length >= 6;
  const hasPoints = ptPosRaw.length >= 3;
  const hasTex = Array.isArray(meshData.textures) && meshData.textures.some(Boolean);
  const hasSkel = !!(skel && Array.isArray(skel.bones) && skel.bones.length > 0);
  const clips = hasSkel && Array.isArray(skel.clips) ? skel.clips : [];

  let showSolid = hasSolid;
  let showWire = !hasSolid && !hasPoints && !hasSkel ? hasWire : (hasWire && !hasTex);
  let showPoints = hasPoints;
  let showTextures = hasTex;
  let showBones = hasSkel;
  let showGrid = hasSkel && !hasSolid;
  let autoSpin = false;
  let ptSize = meshData.is_gaussian_splat ? 6.0 : (hasSkel && !hasSolid ? 6.5 : 3.5);

  let animClipIdx = 0;
  let animPlaying = clips.length > 0;
  let animTime = 0;
  let animSpeed = 1.0;

  // Map each batch index to owning layer index + visibility array
  const layerVisible = layers.map((l) => l.visible !== false);
  const batchToLayer = new Map();
  layers.forEach((l, li) => {
    for (const bi of (l.batch_indices || [])) batchToLayer.set(bi, li);
  });
  let hoveredLayerIdx = -1;

  // Center and normalize bounding box
  const bmin = meshData.bounds_min || [-1, -1, -1];
  const bmax = meshData.bounds_max || [1, 1, 1];
  const cx = (bmin[0] + bmax[0]) * 0.5;
  const cy = (bmin[1] + bmax[1]) * 0.5;
  const cz = (bmin[2] + bmax[2]) * 0.5;
  const dx = bmax[0] - bmin[0];
  const dy = bmax[1] - bmin[1];
  const dz = bmax[2] - bmin[2];
  const scale = 2.0 / Math.max(Math.hypot(dx, dy, dz), 1e-5);

  const normPos = new Float32Array(normPosRaw.length);
  for (let i = 0; i < normPosRaw.length; i += 3) {
    normPos[i] = (normPosRaw[i] - cx) * scale;
    normPos[i + 1] = (normPosRaw[i + 1] - cy) * scale;
    normPos[i + 2] = (normPosRaw[i + 2] - cz) * scale;
  }

  let wireData;
  if (wireRaw.length >= 6) {
    wireData = new Float32Array(wireRaw.length);
    for (let i = 0; i < wireRaw.length; i += 3) {
      wireData[i] = (wireRaw[i] - cx) * scale;
      wireData[i + 1] = (wireRaw[i + 1] - cy) * scale;
      wireData[i + 2] = (wireRaw[i + 2] - cz) * scale;
    }
  } else if (normPos.length >= 9) {
    const triLimit = Math.min(normPos.length / 9, 25000);
    wireData = new Float32Array(triLimit * 18);
    let ptr = 0;
    for (let t = 0; t < triLimit; t++) {
      const o = t * 9;
      wireData[ptr++] = normPos[o]; wireData[ptr++] = normPos[o + 1]; wireData[ptr++] = normPos[o + 2];
      wireData[ptr++] = normPos[o + 3]; wireData[ptr++] = normPos[o + 4]; wireData[ptr++] = normPos[o + 5];
      wireData[ptr++] = normPos[o + 3]; wireData[ptr++] = normPos[o + 4]; wireData[ptr++] = normPos[o + 5];
      wireData[ptr++] = normPos[o + 6]; wireData[ptr++] = normPos[o + 7]; wireData[ptr++] = normPos[o + 8];
      wireData[ptr++] = normPos[o + 6]; wireData[ptr++] = normPos[o + 7]; wireData[ptr++] = normPos[o + 8];
      wireData[ptr++] = normPos[o]; wireData[ptr++] = normPos[o + 1]; wireData[ptr++] = normPos[o + 2];
    }
  } else {
    wireData = new Float32Array(0);
  }

  const ptPos = new Float32Array(ptPosRaw.length);
  for (let i = 0; i < ptPosRaw.length; i += 3) {
    ptPos[i] = (ptPosRaw[i] - cx) * scale;
    ptPos[i + 1] = (ptPosRaw[i + 1] - cy) * scale;
    ptPos[i + 2] = (ptPosRaw[i + 2] - cz) * scale;
  }

  // Build isometric ground grid lines when inspecting skeletal animations
  const gridLines = [];
  if (hasSkel) {
    const gy = Math.max(-1.05, (bmin[1] - cy) * scale);
    const step = 0.25;
    for (let k = -4; k <= 4; k++) {
      const p = k * step;
      gridLines.push(-1, gy, p, 1, gy, p, p, gy, -1, p, gy, 1);
    }
  }
  const gridArr = new Float32Array(gridLines);

  // Build HUD controls
  const hud = document.createElement("div");
  hud.className = "ufo-float-toolbar";
  const stats = hasSkel && !hasSolid
    ? `${skel.bones.length} BONES · ${clips.length} CLIPS`
    : hasPoints
      ? `${meshData.point_count.toLocaleString()} ${meshData.is_gaussian_splat ? "SPLATS" : "PTS"}`
      : `${meshData.triangle_count.toLocaleString()} TRIS · ${meshData.edge_count.toLocaleString()} EDGES`;

  let clipSelectHtml = "";
  if (clips.length > 0) {
    const opts = clips.map((c, i) => `<option value="${i}">${escHtml(c.name)} (${c.duration_secs.toFixed(2)}s)</option>`).join("");
    clipSelectHtml = `
      <button type="button" class="ufo-tool-btn active" data-act="play" title="Play / Pause Loop"><i class="bi bi-pause-fill"></i> LOOP</button>
      <select class="ufo-tool-btn" data-act="clip" style="max-width:150px">${opts}</select>
      <button type="button" class="ufo-tool-btn" data-act="speed">1x</button>
      <input type="range" class="ufo-tool-btn" min="0" max="1" step="0.005" value="0" data-act="scrub" style="width:90px;padding:0;accent-color:currentColor" title="Animation Timeline">
      <span class="ufo-tool-btn" data-act="timecode" style="cursor:default">0.00s</span>
    `;
  }

  hud.innerHTML = `
    <span class="ufo-tool-btn" style="cursor:default">${stats}</span>
    ${hasSolid ? `<button type="button" class="ufo-tool-btn ${showSolid ? "active" : ""}" data-act="solid">SURFACE</button>` : ""}
    ${hasTex ? `<button type="button" class="ufo-tool-btn ${showTextures ? "active" : ""}" data-act="tex">TEX</button>` : ""}
    ${(hasWire || hasSolid) ? `<button type="button" class="ufo-tool-btn ${showWire ? "active" : ""}" data-act="wire">WIRE</button>` : ""}
    ${hasSkel ? `<button type="button" class="ufo-tool-btn ${showBones ? "active" : ""}" data-act="bones">BONES</button>` : ""}
    ${hasSkel ? `<button type="button" class="ufo-tool-btn ${showGrid ? "active" : ""}" data-act="grid">GRID</button>` : ""}
    ${hasPoints ? `<button type="button" class="ufo-tool-btn ${showPoints ? "active" : ""}" data-act="pts">POINTS</button>` : ""}
    ${(hasPoints || hasSkel) ? `<label class="ufo-tool-btn" style="display:inline-flex;align-items:center;gap:5px">SZ <input type="range" min="1" max="16" step="0.5" value="${ptSize}" data-act="ptsize" style="width:52px;accent-color:currentColor"></label>` : ""}
    ${clipSelectHtml}
    <button type="button" class="ufo-tool-btn" data-act="spin">SPIN</button>
    <button type="button" class="ufo-tool-btn" data-act="reset">RESET</button>
  `;
  mainPane.appendChild(hud);

  // Build Side Panel for 3D Layers, Materials/Textures, and Skeleton Hierarchy
  if (hasSidePanel) {
    const side = document.createElement("aside");
    side.className = "ufo-side-panel";

    if (layers.length > 0) {
      const sec = document.createElement("div");
      sec.className = "ufo-side-section";
      sec.innerHTML = `
        <div class="ufo-side-head">
          <span><i class="bi bi-layers"></i> OBJECT LAYERS (${layers.length})</span>
          <div class="ufo-side-actions">
            <button type="button" class="ufo-mini-btn" data-lall="show" title="Show All Layers"><i class="bi bi-eye"></i></button>
            <button type="button" class="ufo-mini-btn" data-lall="hide" title="Hide All Layers"><i class="bi bi-eye-slash"></i></button>
          </div>
        </div>
      `;
      const list = document.createElement("div");
      list.className = "ufo-layer-list";
      const renderLayerRows = () => {
        list.innerHTML = "";
        layers.forEach((l, idx) => {
          const vis = layerVisible[idx];
          const row = document.createElement("div");
          row.className = `ufo-layer-row${vis ? "" : " is-hidden"}`;
          row.innerHTML = `
            <button type="button" class="ufo-eye-btn${vis ? "" : " is-off"}" title="${vis ? "Hide Layer" : "Show Layer"}">
              <i class="bi ${vis ? "bi-eye" : "bi-eye-slash"}"></i>
            </button>
            <div class="ufo-layer-meta">
              <span class="ufo-layer-name">${escHtml(l.name)}</span>
              <span class="ufo-layer-sub">${escHtml(l.kind)} · ${l.triangle_count.toLocaleString()} tris</span>
            </div>
          `;
          row.querySelector(".ufo-eye-btn").addEventListener("click", (ev) => {
            ev.stopPropagation();
            layerVisible[idx] = !layerVisible[idx];
            renderLayerRows();
          });
          row.addEventListener("mouseenter", () => { hoveredLayerIdx = idx; });
          row.addEventListener("mouseleave", () => { hoveredLayerIdx = -1; });
          list.appendChild(row);
        });
      };
      sec.querySelector("[data-lall='show']").addEventListener("click", () => {
        for (let i = 0; i < layerVisible.length; i++) layerVisible[i] = true;
        renderLayerRows();
      });
      sec.querySelector("[data-lall='hide']").addEventListener("click", () => {
        for (let i = 0; i < layerVisible.length; i++) layerVisible[i] = false;
        renderLayerRows();
      });
      renderLayerRows();
      sec.appendChild(list);
      side.appendChild(sec);
    }

    if (materials.length > 0 || hasTex) {
      const mSec = document.createElement("div");
      mSec.className = "ufo-side-section";
      mSec.innerHTML = `<div class="ufo-side-head"><span><i class="bi bi-palette"></i> MATERIALS & TEXTURES</span></div>`;
      const mList = document.createElement("div");
      mList.className = "ufo-layer-list";
      materials.slice(0, 40).forEach((m) => {
        const c = m.base_color || [0.8, 0.85, 0.9, 1.0];
        const css = `rgba(${Math.round(c[0] * 255)}, ${Math.round(c[1] * 255)}, ${Math.round(c[2] * 255)}, ${c[3] ?? 1})`;
        const r = document.createElement("div");
        r.className = "ufo-mat-row";
        r.innerHTML = `
          <span class="ufo-swatch" style="background:${css}"></span>
          <div class="ufo-layer-meta">
            <span class="ufo-layer-name">${escHtml(m.name)}</span>
            <span class="ufo-layer-sub">M:${(m.metallic ?? 0).toFixed(2)} R:${(m.roughness ?? 0.5).toFixed(2)}${m.texture_name ? ` · ${escHtml(m.texture_name)}` : ""}</span>
          </div>
        `;
        mList.appendChild(r);
      });
      if (hasTex) {
        const texWrap = document.createElement("div");
        texWrap.style.cssText = "display:flex;flex-wrap:wrap;gap:6px;padding:6px;";
        meshData.textures.filter(Boolean).slice(0, 12).forEach((url, ti) => {
          const im = document.createElement("img");
          im.src = url;
          im.title = `Texture #${ti}`;
          im.style.cssText = "width:48px;height:48px;object-fit:cover;border:1px solid var(--border);border-radius:4px;";
          texWrap.appendChild(im);
        });
        mList.appendChild(texWrap);
      }
      mSec.appendChild(mList);
      side.appendChild(mSec);
    }

    if (hasSkel) {
      const sSec = document.createElement("div");
      sSec.className = "ufo-side-section";
      sSec.innerHTML = `
        <div class="ufo-side-head">
          <span><i class="bi bi-diagram-3"></i> SKELETON (${skel.bones.length} BONES)</span>
        </div>
        <div style="padding:6px 10px;font-family:var(--mono);font-size:10px;color:var(--ice-dim);border-bottom:1px solid var(--border)">
          ${escHtml(skel.species_hint || "Articulated Rig")}
        </div>
      `;
      const bList = document.createElement("div");
      bList.className = "ufo-layer-list";
      skel.bones.slice(0, 80).forEach((b) => {
        const row = document.createElement("div");
        row.className = "ufo-layer-row";
        row.innerHTML = `
          <i class="bi bi-dot" style="color:var(--ice)"></i>
          <div class="ufo-layer-meta">
            <span class="ufo-layer-name">${escHtml(b.name)}</span>
            <span class="ufo-layer-sub">${b.parent != null ? `Parent: ${escHtml(skel.bones[b.parent]?.name || b.parent)}` : "Root Joint"}</span>
          </div>
        `;
        bList.appendChild(row);
      });
      sSec.appendChild(bList);
      side.appendChild(sSec);
    }

    shell.appendChild(side);
  }

  const cubeHud = document.createElement("div");
  cubeHud.className = "ufo-viewcube-hud";
  cubeHud.innerHTML = `
    <div class="ufo-viewcube-roll-bar">
      <button type="button" class="ufo-viewcube-btn" data-vc-act="ccw" title="Rotate Anticlockwise (90° · Shift: 15°)"><i class="bi bi-arrow-counterclockwise"></i></button>
      <button type="button" class="ufo-viewcube-btn" data-vc-act="iso" title="Isometric Home View"><i class="bi bi-house"></i></button>
      <button type="button" class="ufo-viewcube-btn" data-vc-act="cw" title="Rotate Clockwise (90° · Shift: 15°)"><i class="bi bi-arrow-clockwise"></i></button>
    </div>
    <div class="ufo-viewcube-scene" title="Drag to orbit · Click face to snap">
      <div class="ufo-viewcube-cube">
        <button type="button" class="ufo-viewcube-face" data-vc-face="front" style="transform:translateZ(26px)">FRONT</button>
        <button type="button" class="ufo-viewcube-face" data-vc-face="back" style="transform:rotateY(180deg) translateZ(26px)">BACK</button>
        <button type="button" class="ufo-viewcube-face" data-vc-face="right" style="transform:rotateY(90deg) translateZ(26px)">RIGHT</button>
        <button type="button" class="ufo-viewcube-face" data-vc-face="left" style="transform:rotateY(-90deg) translateZ(26px)">LEFT</button>
        <button type="button" class="ufo-viewcube-face" data-vc-face="top" style="transform:rotateX(90deg) translateZ(26px)">TOP</button>
        <button type="button" class="ufo-viewcube-face" data-vc-face="bottom" style="transform:rotateX(-90deg) translateZ(26px)">BOTTOM</button>
      </div>
    </div>
  `;
  mainPane.appendChild(cubeHud);

  container.appendChild(shell);

  const cubeEl = cubeHud.querySelector(".ufo-viewcube-cube");
  const cubeScene = cubeHud.querySelector(".ufo-viewcube-scene");

  const gl = canvas.getContext("webgl2", { antialias: true, alpha: false }) || canvas.getContext("webgl", { antialias: true });
  let rotQ = stdViewQuat("iso");
  let animView = null; // { fromQ, toQ, startMs, durationMs }
  let dist = 3.1;
  let panX = 0;
  let panY = 0;
  let rafId = 0;
  let destroyed = false;
  let lastFrameMs = performance.now();

  const startViewAnim = (toQ, durationMs = 260) => {
    autoSpin = false;
    const spinBtn = hud.querySelector("[data-act='spin']");
    if (spinBtn) spinBtn.classList.remove("active");
    animView = {
      fromQ: rotQ.slice(),
      toQ: quatNorm(toQ),
      startMs: performance.now(),
      durationMs,
    };
  };

  let drawFn = () => {};
  const createdTextures = [];

  if (gl) {
    const vsSrc = `
      attribute vec3 aPos;
      attribute vec3 aNrm;
      attribute vec2 aUv;
      attribute vec3 aCol;
      uniform mat4 uMvp;
      uniform mat4 uModel;
      uniform float uPointSize;
      varying vec3 vNrm;
      varying vec2 vUv;
      varying vec3 vCol;
      void main() {
        vNrm = mat3(uModel) * aNrm;
        vUv = aUv;
        vCol = aCol;
        gl_Position = uMvp * vec4(aPos, 1.0);
        gl_PointSize = max(2.0, uPointSize / max(gl_Position.w * 0.45, 0.35));
      }
    `;
    const fsSrc = `
      precision mediump float;
      varying vec3 vNrm;
      varying vec2 vUv;
      varying vec3 vCol;
      uniform vec4 uBaseColor;
      uniform float uMode; // 0=wire, 1=lit mesh, 2=points, 3=gaussian splat
      uniform float uHasTex;
      uniform float uHasVCol;
      uniform sampler2D uTex;
      void main() {
        if (uMode > 1.5) {
          vec2 c = gl_PointCoord - vec2(0.5);
          float r2 = dot(c, c);
          if (r2 > 0.25) discard;
          float alpha = uMode > 2.5 ? exp(-r2 * 10.0) : 1.0;
          vec3 col = uHasVCol > 0.5 ? vCol : uBaseColor.rgb;
          gl_FragColor = vec4(col, alpha);
          return;
        }
        if (uMode < 0.5) {
          gl_FragColor = uBaseColor;
          return;
        }
        vec3 n = normalize(vNrm);
        vec3 l1 = normalize(vec3(0.55, 0.82, 0.72));
        vec3 l2 = normalize(vec3(-0.65, -0.35, 0.45));
        float lit = 0.24 + max(abs(dot(n, l1)) * 0.68, 0.0) + max(abs(dot(n, l2)) * 0.26, 0.0);
        vec4 albedo = uBaseColor;
        if (uHasVCol > 0.5) albedo.rgb *= vCol;
        if (uHasTex > 0.5) albedo *= texture2D(uTex, vUv);
        gl_FragColor = vec4(albedo.rgb * min(lit, 1.15), albedo.a);
      }
    `;
    const prog = createProgram(gl, vsSrc, fsSrc);
    const locPos = gl.getAttribLocation(prog, "aPos");
    const locNrm = gl.getAttribLocation(prog, "aNrm");
    const locUv = gl.getAttribLocation(prog, "aUv");
    const locCol = gl.getAttribLocation(prog, "aCol");
    const locMvp = gl.getUniformLocation(prog, "uMvp");
    const locModel = gl.getUniformLocation(prog, "uModel");
    const locPtSize = gl.getUniformLocation(prog, "uPointSize");
    const locBaseCol = gl.getUniformLocation(prog, "uBaseColor");
    const locMode = gl.getUniformLocation(prog, "uMode");
    const locHasTex = gl.getUniformLocation(prog, "uHasTex");
    const locHasVCol = gl.getUniformLocation(prog, "uHasVCol");
    const locTex = gl.getUniformLocation(prog, "uTex");

    const makeBuf = (data, usage = gl.STATIC_DRAW) => {
      const b = gl.createBuffer();
      gl.bindBuffer(gl.ARRAY_BUFFER, b);
      gl.bufferData(gl.ARRAY_BUFFER, data, usage);
      return b;
    };
    const posBuf = makeBuf(normPos);
    const nrmBuf = makeBuf(normNrm.length === normPos.length ? normNrm : new Float32Array(normPos.length));
    const uvBuf = uvArr.length > 0 ? makeBuf(uvArr) : null;
    const colBuf = colArr.length > 0 ? makeBuf(colArr) : null;
    const wireBuf = makeBuf(wireData);
    const ptPosBuf = makeBuf(ptPos);
    const ptColBuf = ptColRaw.length === ptPos.length ? makeBuf(ptColRaw) : null;
    const gridBuf = gridArr.length > 0 ? makeBuf(gridArr) : null;
    const boneBuf = hasSkel ? gl.createBuffer() : null;
    const jointBuf = hasSkel ? gl.createBuffer() : null;

    // Pre-index clip channels by bone index
    const clipChannelMaps = clips.map((c) => {
      const m = new Map();
      for (const ch of (c.channels || [])) m.set(ch.bone_index, ch);
      return m;
    });

    const glTextures = (meshData.textures || []).map((dataUrl) => {
      if (!dataUrl) return null;
      const tex = gl.createTexture();
      createdTextures.push(tex);
      gl.bindTexture(gl.TEXTURE_2D, tex);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 1, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array([220, 235, 235, 255]));
      const img = new Image();
      img.onload = () => {
        if (destroyed) return;
        gl.bindTexture(gl.TEXTURE_2D, tex);
        gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, img);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.REPEAT);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.REPEAT);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR_MIPMAP_LINEAR);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
        gl.generateMipmap(gl.TEXTURE_2D);
      };
      img.src = dataUrl;
      return tex;
    });

    const batches = (meshData.batches && meshData.batches.length > 0)
      ? meshData.batches
      : [{ start_vertex: 0, vertex_count: normPos.length / 3, base_color: [0.82, 0.92, 0.92, 1.0], texture_index: null }];

    const scrubEl = hud.querySelector("[data-act='scrub']");
    const timeEl = hud.querySelector("[data-act='timecode']");

    drawFn = () => {
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      const w = Math.max(canvas.clientWidth * dpr, 1);
      const h = Math.max(canvas.clientHeight * dpr, 1);
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
      }
      const isLight = getIsLight ? getIsLight() : false;
      gl.viewport(0, 0, w, h);
      if (isLight) gl.clearColor(0.957, 0.969, 0.969, 1);
      else gl.clearColor(0, 0, 0, 1);
      gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
      gl.enable(gl.DEPTH_TEST);
      gl.useProgram(prog);

      const model = quatToMat4ColMajor(rotQ);
      const view = mat4Translate(panX, panY, -dist);
      const proj = mat4Perspective(45 * Math.PI / 180, w / h, 0.05, 100.0);
      const mvp = mat4Mul(proj, mat4Mul(view, model));
      gl.uniformMatrix4fv(locMvp, false, mvp);
      gl.uniformMatrix4fv(locModel, false, model);
      gl.uniform1f(locPtSize, ptSize * dpr);

      if (showGrid && gridBuf && gridArr.length >= 6) {
        gl.bindBuffer(gl.ARRAY_BUFFER, gridBuf);
        gl.enableVertexAttribArray(locPos);
        gl.vertexAttribPointer(locPos, 3, gl.FLOAT, false, 0, 0);
        gl.disableVertexAttribArray(locNrm);
        if (locUv >= 0) gl.disableVertexAttribArray(locUv);
        if (locCol >= 0) gl.disableVertexAttribArray(locCol);
        gl.uniform1f(locMode, 0.0);
        gl.uniform1f(locHasTex, 0.0);
        gl.uniform1f(locHasVCol, 0.0);
        if (isLight) gl.uniform4f(locBaseCol, 0.1, 0.14, 0.15, 0.25);
        else gl.uniform4f(locBaseCol, 0.35, 0.45, 0.48, 0.32);
        gl.drawArrays(gl.LINES, 0, gridArr.length / 3);
      }

      if (showSolid && normPos.length >= 9) {
        gl.disable(gl.BLEND);
        gl.bindBuffer(gl.ARRAY_BUFFER, posBuf);
        gl.enableVertexAttribArray(locPos);
        gl.vertexAttribPointer(locPos, 3, gl.FLOAT, false, 0, 0);

        gl.bindBuffer(gl.ARRAY_BUFFER, nrmBuf);
        gl.enableVertexAttribArray(locNrm);
        gl.vertexAttribPointer(locNrm, 3, gl.FLOAT, false, 0, 0);

        if (uvBuf && locUv >= 0) {
          gl.bindBuffer(gl.ARRAY_BUFFER, uvBuf);
          gl.enableVertexAttribArray(locUv);
          gl.vertexAttribPointer(locUv, 2, gl.FLOAT, false, 0, 0);
        } else if (locUv >= 0) {
          gl.disableVertexAttribArray(locUv);
          gl.vertexAttrib2f(locUv, 0, 0);
        }

        if (colBuf && locCol >= 0) {
          gl.bindBuffer(gl.ARRAY_BUFFER, colBuf);
          gl.enableVertexAttribArray(locCol);
          gl.vertexAttribPointer(locCol, 3, gl.FLOAT, false, 0, 0);
          gl.uniform1f(locHasVCol, 1.0);
        } else {
          if (locCol >= 0) {
            gl.disableVertexAttribArray(locCol);
            gl.vertexAttrib3f(locCol, 1, 1, 1);
          }
          gl.uniform1f(locHasVCol, 0.0);
        }

        gl.uniform1f(locMode, 1.0);
        batches.forEach((b, bi) => {
          const ownerLayer = batchToLayer.get(bi);
          if (ownerLayer !== undefined && !layerVisible[ownerLayer]) return;
          const tex = (showTextures && b.texture_index != null) ? glTextures[b.texture_index] : null;
          if (tex) {
            gl.activeTexture(gl.TEXTURE0);
            gl.bindTexture(gl.TEXTURE_2D, tex);
            gl.uniform1i(locTex, 0);
            gl.uniform1f(locHasTex, 1.0);
          } else {
            gl.uniform1f(locHasTex, 0.0);
          }
          let bc = b.base_color || [0.82, 0.92, 0.92, 1.0];
          if (hoveredLayerIdx >= 0 && ownerLayer === hoveredLayerIdx) {
            bc = [Math.min(1, bc[0] + 0.25), Math.min(1, bc[1] + 0.3), Math.min(1, bc[2] + 0.3), bc[3]];
          }
          gl.uniform4f(locBaseCol, bc[0], bc[1], bc[2], bc[3]);
          gl.drawArrays(gl.TRIANGLES, b.start_vertex, b.vertex_count);
        });
      }

      if (showWire && wireData.length >= 6) {
        gl.disable(gl.BLEND);
        gl.bindBuffer(gl.ARRAY_BUFFER, wireBuf);
        gl.enableVertexAttribArray(locPos);
        gl.vertexAttribPointer(locPos, 3, gl.FLOAT, false, 0, 0);
        gl.disableVertexAttribArray(locNrm);
        if (locUv >= 0) gl.disableVertexAttribArray(locUv);
        if (locCol >= 0) gl.disableVertexAttribArray(locCol);
        gl.uniform1f(locMode, 0.0);
        gl.uniform1f(locHasTex, 0.0);
        gl.uniform1f(locHasVCol, 0.0);
        if (showSolid) {
          gl.uniform4f(locBaseCol, 0.05, 0.14, 0.16, 0.75);
        } else if (isLight) {
          gl.uniform4f(locBaseCol, 0.05, 0.08, 0.09, 0.92);
        } else {
          gl.uniform4f(locBaseCol, 0.92, 1.0, 1.0, 0.92);
        }
        gl.drawArrays(gl.LINES, 0, wireData.length / 3);
      }

      if (showPoints && ptPos.length >= 3) {
        if (meshData.is_gaussian_splat) {
          gl.enable(gl.BLEND);
          gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
        }
        gl.bindBuffer(gl.ARRAY_BUFFER, ptPosBuf);
        gl.enableVertexAttribArray(locPos);
        gl.vertexAttribPointer(locPos, 3, gl.FLOAT, false, 0, 0);
        gl.disableVertexAttribArray(locNrm);
        if (locUv >= 0) gl.disableVertexAttribArray(locUv);
        if (ptColBuf && locCol >= 0) {
          gl.bindBuffer(gl.ARRAY_BUFFER, ptColBuf);
          gl.enableVertexAttribArray(locCol);
          gl.vertexAttribPointer(locCol, 3, gl.FLOAT, false, 0, 0);
          gl.uniform1f(locHasVCol, 1.0);
        } else {
          if (locCol >= 0) gl.disableVertexAttribArray(locCol);
          gl.uniform1f(locHasVCol, 0.0);
          gl.uniform4f(locBaseCol, isLight ? 0.06 : 0.92, isLight ? 0.1 : 1.0, isLight ? 0.12 : 1.0, 1.0);
        }
        gl.uniform1f(locMode, meshData.is_gaussian_splat ? 3.0 : 2.0);
        gl.uniform1f(locHasTex, 0.0);
        gl.drawArrays(gl.POINTS, 0, ptPos.length / 3);
        gl.disable(gl.BLEND);
      }

      if (showBones && hasSkel && boneBuf && jointBuf) {
        const activeClip = clips[animClipIdx] || null;
        const chMap = clipChannelMaps[animClipIdx] || new Map();
        const dur = activeClip ? Math.max(activeClip.duration_secs, 0.1) : 1.0;
        const t = animTime % dur;
        if (scrubEl && dur > 0) scrubEl.value = (t / dur).toFixed(3);
        if (timeEl) timeEl.textContent = `${t.toFixed(2)}s`;

        const bones = skel.bones;
        const worlds = new Array(bones.length);
        const pts = new Array(bones.length);
        for (let i = 0; i < bones.length; i++) {
          const b = bones[i];
          const ch = chMap.get(i);
          const locP = ch ? sampleVec3(ch.times, ch.translations, t, b.rest_local_pos) : b.rest_local_pos;
          const locR = ch ? sampleQuat(ch.times, ch.rotations, t, b.rest_local_rot) : b.rest_local_rot;
          const locS = ch ? sampleVec3(ch.times, ch.scales, t, [1, 1, 1]) : [1, 1, 1];
          const lmat = quatToMat4(locR, locP, locS);
          const wmat = (b.parent != null && b.parent < i) ? mulMat4F64(worlds[b.parent], lmat) : lmat;
          worlds[i] = wmat;
          pts[i] = [
            (wmat[12] - cx) * scale,
            (wmat[13] - cy) * scale,
            (wmat[14] - cz) * scale,
          ];
        }

        const segs = [];
        const joints = [];
        for (let i = 0; i < bones.length; i++) {
          const p = pts[i];
          joints.push(p[0], p[1], p[2]);
          const pi = bones[i].parent;
          if (pi != null && pi >= 0 && pi < i) {
            const pp = pts[pi];
            segs.push(pp[0], pp[1], pp[2], p[0], p[1], p[2]);
          }
        }

        if (hasSolid) gl.disable(gl.DEPTH_TEST);
        gl.disableVertexAttribArray(locNrm);
        if (locUv >= 0) gl.disableVertexAttribArray(locUv);
        if (locCol >= 0) gl.disableVertexAttribArray(locCol);
        gl.uniform1f(locHasTex, 0.0);
        gl.uniform1f(locHasVCol, 0.0);

        if (segs.length >= 6) {
          gl.bindBuffer(gl.ARRAY_BUFFER, boneBuf);
          gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(segs), gl.DYNAMIC_DRAW);
          gl.enableVertexAttribArray(locPos);
          gl.vertexAttribPointer(locPos, 3, gl.FLOAT, false, 0, 0);
          gl.uniform1f(locMode, 0.0);
          if (isLight) gl.uniform4f(locBaseCol, 0.02, 0.35, 0.42, 1.0);
          else gl.uniform4f(locBaseCol, 0.68, 0.98, 1.0, 1.0);
          gl.drawArrays(gl.LINES, 0, segs.length / 3);
        }
        if (joints.length >= 3) {
          gl.bindBuffer(gl.ARRAY_BUFFER, jointBuf);
          gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(joints), gl.DYNAMIC_DRAW);
          gl.enableVertexAttribArray(locPos);
          gl.vertexAttribPointer(locPos, 3, gl.FLOAT, false, 0, 0);
          gl.uniform1f(locMode, 2.0);
          if (isLight) gl.uniform4f(locBaseCol, 0.05, 0.1, 0.14, 1.0);
          else gl.uniform4f(locBaseCol, 1.0, 0.85, 0.35, 1.0);
          gl.drawArrays(gl.POINTS, 0, joints.length / 3);
        }
        if (hasSolid) gl.enable(gl.DEPTH_TEST);
      }
    };
  }

  const pointers = new Map();
  let lastPinch = 0;

  const applyOrbitDelta = (dxp, dyp) => {
    animView = null;
    const sens = 0.0085;
    const dQy = quatFromAxis([0, 1, 0], dxp * sens);
    const dQx = quatFromAxis([1, 0, 0], dyp * sens);
    rotQ = quatNorm(quatMul(quatMul(dQx, dQy), rotQ));
  };

  canvas.addEventListener("pointerdown", (e) => {
    canvas.setPointerCapture(e.pointerId);
    canvas.style.cursor = "grabbing";
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
  });
  canvas.addEventListener("pointermove", (e) => {
    if (!pointers.has(e.pointerId)) return;
    const prev = pointers.get(e.pointerId);
    const dxp = e.clientX - prev.x;
    const dyp = e.clientY - prev.y;
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pointers.size === 1) {
      if (e.buttons === 2 || e.shiftKey) {
        panX += dxp * 0.004 * dist;
        panY -= dyp * 0.004 * dist;
      } else {
        applyOrbitDelta(dxp, dyp);
      }
    } else if (pointers.size === 2) {
      const pts = Array.from(pointers.values());
      const d = Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y);
      if (lastPinch > 0) {
        dist = Math.max(0.35, Math.min(25, dist * (lastPinch / Math.max(d, 1))));
      }
      lastPinch = d;
    }
  });
  const release = (e) => {
    pointers.delete(e.pointerId);
    if (pointers.size < 2) lastPinch = 0;
    if (pointers.size === 0) canvas.style.cursor = "grab";
  };
  canvas.addEventListener("pointerup", release);
  canvas.addEventListener("pointercancel", release);
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    dist = Math.max(0.35, Math.min(25, dist * (1 + e.deltaY * 0.001)));
  }, { passive: false });

  // Bottom-left CAD ViewCube drag-orbit + face snap + CW/CCW roll controls
  let vcDrag = null;
  let vcSuppressClick = false;
  cubeScene.addEventListener("pointerdown", (e) => {
    cubeScene.setPointerCapture(e.pointerId);
    cubeScene.classList.add("dragging");
    vcDrag = { id: e.pointerId, x: e.clientX, y: e.clientY, moved: 0 };
  });
  cubeScene.addEventListener("pointermove", (e) => {
    if (!vcDrag || vcDrag.id !== e.pointerId) return;
    const dxp = e.clientX - vcDrag.x;
    const dyp = e.clientY - vcDrag.y;
    vcDrag.x = e.clientX;
    vcDrag.y = e.clientY;
    vcDrag.moved += Math.hypot(dxp, dyp);
    if (vcDrag.moved > 3) {
      applyOrbitDelta(dxp * 1.4, dyp * 1.4);
    }
  });
  const vcRelease = (e) => {
    if (!vcDrag || vcDrag.id !== e.pointerId) return;
    if (vcDrag.moved > 5) {
      vcSuppressClick = true;
      setTimeout(() => { vcSuppressClick = false; }, 60);
    }
    vcDrag = null;
    cubeScene.classList.remove("dragging");
  };
  cubeScene.addEventListener("pointerup", vcRelease);
  cubeScene.addEventListener("pointercancel", vcRelease);

  cubeHud.addEventListener("click", (e) => {
    if (vcSuppressClick) return;
    const faceBtn = e.target.closest("[data-vc-face]");
    if (faceBtn) {
      startViewAnim(stdViewQuat(faceBtn.dataset.vcFace));
      return;
    }
    const actBtn = e.target.closest("[data-vc-act]");
    if (!actBtn) return;
    const act = actBtn.dataset.vcAct;
    if (act === "iso") {
      panX = 0;
      panY = 0;
      dist = 3.1;
      startViewAnim(stdViewQuat("iso"));
    } else if (act === "cw" || act === "ccw") {
      const step = e.shiftKey ? (Math.PI / 12) : (Math.PI / 2);
      const signed = act === "cw" ? -step : step;
      const base = animView ? animView.toQ : rotQ;
      const target = quatMul(quatFromAxis([0, 0, 1], signed), base);
      startViewAnim(target, 220);
    }
  });

  hud.addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    const act = btn.dataset.act;
    if (act === "solid") { showSolid = !showSolid; btn.classList.toggle("active", showSolid); }
    if (act === "tex") { showTextures = !showTextures; btn.classList.toggle("active", showTextures); }
    if (act === "wire") { showWire = !showWire; btn.classList.toggle("active", showWire); }
    if (act === "bones") { showBones = !showBones; btn.classList.toggle("active", showBones); }
    if (act === "grid") { showGrid = !showGrid; btn.classList.toggle("active", showGrid); }
    if (act === "pts") { showPoints = !showPoints; btn.classList.toggle("active", showPoints); }
    if (act === "spin") { autoSpin = !autoSpin; btn.classList.toggle("active", autoSpin); }
    if (act === "play") {
      animPlaying = !animPlaying;
      btn.classList.toggle("active", animPlaying);
      btn.innerHTML = animPlaying ? `<i class="bi bi-pause-fill"></i> LOOP` : `<i class="bi bi-play-fill"></i> PLAY`;
    }
    if (act === "speed") {
      animSpeed = animSpeed === 1.0 ? 0.5 : animSpeed === 0.5 ? 2.0 : animSpeed === 2.0 ? 0.25 : 1.0;
      btn.textContent = `${animSpeed}x`;
    }
    if (act === "reset") {
      dist = 3.1;
      panX = 0;
      panY = 0;
      startViewAnim(stdViewQuat("iso"));
    }
  });
  hud.addEventListener("change", (e) => {
    if (e.target.dataset.act === "clip") {
      animClipIdx = parseInt(e.target.value, 10) || 0;
      animTime = 0;
    }
  });
  hud.addEventListener("input", (e) => {
    if (e.target.dataset.act === "ptsize") {
      ptSize = parseFloat(e.target.value) || 4.0;
    } else if (e.target.dataset.act === "scrub") {
      const c = clips[animClipIdx];
      if (c) animTime = (parseFloat(e.target.value) || 0) * Math.max(c.duration_secs, 0.1);
    }
  });

  function frame(now) {
    if (destroyed) return;
    const dt = Math.min((now - lastFrameMs) / 1000, 0.1);
    lastFrameMs = now;
    if (animView) {
      const rawT = Math.min(1, Math.max(0, (now - animView.startMs) / Math.max(animView.durationMs, 1)));
      const eased = rawT * rawT * (3 - 2 * rawT);
      rotQ = quatSlerp(animView.fromQ, animView.toQ, eased);
      if (rawT >= 1) animView = null;
    } else if (autoSpin) {
      rotQ = quatNorm(quatMul(quatFromAxis([0, 1, 0], 0.0032), rotQ));
    }
    if (cubeEl) {
      cubeEl.style.transform = quatToCssMatrix3d(rotQ);
    }
    if (animPlaying && clips.length > 0) {
      const dur = Math.max(clips[animClipIdx]?.duration_secs || 1.0, 0.1);
      animTime = (animTime + dt * animSpeed) % dur;
    }
    drawFn();
    rafId = requestAnimationFrame(frame);
  }
  rafId = requestAnimationFrame(frame);

  return () => {
    destroyed = true;
    cancelAnimationFrame(rafId);
    if (gl) {
      for (const t of createdTextures) gl.deleteTexture(t);
    }
  };
}

function quatFromAxis(axis, angle) {
  const half = angle * 0.5;
  const s = Math.sin(half);
  const len = Math.hypot(axis[0], axis[1], axis[2]) || 1;
  return [axis[0] / len * s, axis[1] / len * s, axis[2] / len * s, Math.cos(half)];
}

function quatMul(a, b) {
  const [ax, ay, az, aw] = a;
  const [bx, by, bz, bw] = b;
  return [
    aw * bx + ax * bw + ay * bz - az * by,
    aw * by - ax * bz + ay * bw + az * bx,
    aw * bz + ax * by - ay * bx + az * bw,
    aw * bw - ax * bx - ay * by - az * bz,
  ];
}

function quatNorm(q) {
  const l = Math.hypot(q[0], q[1], q[2], q[3]) || 1;
  return [q[0] / l, q[1] / l, q[2] / l, q[3] / l];
}

function quatSlerp(q0, q1, t) {
  let [x0, y0, z0, w0] = q0;
  let [x1, y1, z1, w1] = q1;
  let d = x0 * x1 + y0 * y1 + z0 * z1 + w0 * w1;
  if (d < 0) {
    d = -d;
    x1 = -x1; y1 = -y1; z1 = -z1; w1 = -w1;
  }
  if (d > 0.9995) {
    return quatNorm([
      x0 + (x1 - x0) * t,
      y0 + (y1 - y0) * t,
      z0 + (z1 - z0) * t,
      w0 + (w1 - w0) * t,
    ]);
  }
  const th = Math.acos(Math.min(1, Math.max(-1, d)));
  const s = Math.sin(th);
  const wA = Math.sin((1 - t) * th) / s;
  const wB = Math.sin(t * th) / s;
  return quatNorm([
    x0 * wA + x1 * wB,
    y0 * wA + y1 * wB,
    z0 * wA + z1 * wB,
    w0 * wA + w1 * wB,
  ]);
}

function quatToMat4ColMajor(q) {
  const [x, y, z, w] = q;
  const x2 = x + x, y2 = y + y, z2 = z + z;
  const xx = x * x2, xy = x * y2, xz = x * z2;
  const yy = y * y2, yz = y * z2, zz = z * z2;
  const wx = w * x2, wy = w * y2, wz = w * z2;
  return new Float32Array([
    1 - (yy + zz), xy + wz, xz - wy, 0,
    xy - wz, 1 - (xx + zz), yz + wx, 0,
    xz + wy, yz - wx, 1 - (xx + yy), 0,
    0, 0, 0, 1,
  ]);
}

function quatToCssMatrix3d(q) {
  // Conjugate by S = diag(1, -1, 1) to align WebGL (+Y up) with CSS 3D (+Y down).
  const m = quatToMat4ColMajor(q);
  const c00 = m[0],  c10 = -m[1], c20 = m[2];
  const c01 = -m[4], c11 = m[5],  c21 = -m[6];
  const c02 = m[8],  c12 = -m[9], c22 = m[10];
  return `matrix3d(${c00.toFixed(6)},${c10.toFixed(6)},${c20.toFixed(6)},0,${c01.toFixed(6)},${c11.toFixed(6)},${c21.toFixed(6)},0,${c02.toFixed(6)},${c12.toFixed(6)},${c22.toFixed(6)},0,0,0,0,1)`;
}

function stdViewQuat(name) {
  const h = Math.PI * 0.5;
  switch (String(name || "").toLowerCase()) {
    case "front": return [0, 0, 0, 1];
    case "back": return quatFromAxis([0, 1, 0], Math.PI);
    case "right": return quatFromAxis([0, 1, 0], -h);
    case "left": return quatFromAxis([0, 1, 0], h);
    case "top": return quatFromAxis([1, 0, 0], h);
    case "bottom": return quatFromAxis([1, 0, 0], -h);
    case "iso":
    default:
      return quatNorm(quatMul(quatFromAxis([1, 0, 0], 0.42), quatFromAxis([0, 1, 0], -0.65)));
  }
}

function createProgram(gl, vsSrc, fsSrc) {
  const vs = gl.createShader(gl.VERTEX_SHADER);
  gl.shaderSource(vs, vsSrc);
  gl.compileShader(vs);
  const fs = gl.createShader(gl.FRAGMENT_SHADER);
  gl.shaderSource(fs, fsSrc);
  gl.compileShader(fs);
  const p = gl.createProgram();
  gl.attachShader(p, vs);
  gl.attachShader(p, fs);
  gl.linkProgram(p);
  return p;
}

function mat4Perspective(fov, aspect, near, far) {
  const f = 1.0 / Math.tan(fov / 2);
  const nf = 1 / (near - far);
  return new Float32Array([
    f / aspect, 0, 0, 0,
    0, f, 0, 0,
    0, 0, (far + near) * nf, -1,
    0, 0, 2 * far * near * nf, 0,
  ]);
}

function mat4Translate(x, y, z) {
  return new Float32Array([1,0,0,0, 0,1,0,0, 0,0,1,0, x,y,z,1]);
}

function mat4Mul(a, b) {
  const o = new Float32Array(16);
  for (let c = 0; c < 4; c++) {
    for (let r = 0; r < 4; r++) {
      o[c * 4 + r] =
        a[r] * b[c * 4] +
        a[4 + r] * b[c * 4 + 1] +
        a[8 + r] * b[c * 4 + 2] +
        a[12 + r] * b[c * 4 + 3];
    }
  }
  return o;
}
