// High-speed WebGL2 / WebGL1 Mesh, STEP CAD, Textured GLB, and Gaussian Splatting Viewport

function decodeB64F32(b64) {
  if (!b64) return new Float32Array(0);
  const bin = atob(b64);
  const len = bin.length;
  const bytes = new Uint8Array(len);
  for (let i = 0; i < len; i++) bytes[i] = bin.charCodeAt(i);
  return new Float32Array(bytes.buffer, 0, Math.floor(len / 4));
}

export function mountMeshViewer(container, meshData, getIsLight) {
  container.innerHTML = "";
  const wrap = document.createElement("div");
  wrap.className = "ufo-media-stage";
  wrap.style.padding = "0";

  const canvas = document.createElement("canvas");
  canvas.className = "ufo-3d-canvas";
  wrap.appendChild(canvas);

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

  let showSolid = hasSolid;
  let showWire = !hasSolid && !hasPoints ? hasWire : (hasWire && !hasTex);
  let showPoints = hasPoints;
  let showTextures = hasTex;
  let autoSpin = false;
  let ptSize = meshData.is_gaussian_splat ? 6.0 : 3.5;

  const hud = document.createElement("div");
  hud.className = "ufo-float-toolbar";
  const stats = hasPoints
    ? `${meshData.point_count.toLocaleString()} ${meshData.is_gaussian_splat ? "SPLATS" : "PTS"}`
    : `${meshData.triangle_count.toLocaleString()} TRIS · ${meshData.edge_count.toLocaleString()} EDGES`;

  hud.innerHTML = `
    <span class="ufo-tool-btn" style="cursor:default">${stats}</span>
    ${hasSolid ? `<button type="button" class="ufo-tool-btn ${showSolid ? "active" : ""}" data-act="solid">SURFACE</button>` : ""}
    ${hasTex ? `<button type="button" class="ufo-tool-btn ${showTextures ? "active" : ""}" data-act="tex">TEX</button>` : ""}
    ${(hasWire || hasSolid) ? `<button type="button" class="ufo-tool-btn ${showWire ? "active" : ""}" data-act="wire">WIRE</button>` : ""}
    ${hasPoints ? `<button type="button" class="ufo-tool-btn ${showPoints ? "active" : ""}" data-act="pts">POINTS</button>
      <label class="ufo-tool-btn" style="display:inline-flex;align-items:center;gap:5px">SZ <input type="range" min="1" max="16" step="0.5" value="${ptSize}" data-act="ptsize" style="width:54px;accent-color:currentColor"></label>` : ""}
    <button type="button" class="ufo-tool-btn" data-act="spin">SPIN</button>
    <button type="button" class="ufo-tool-btn" data-act="reset">RESET</button>
  `;
  wrap.appendChild(hud);
  container.appendChild(wrap);

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

  const gl = canvas.getContext("webgl2", { antialias: true, alpha: false }) || canvas.getContext("webgl", { antialias: true });
  let yaw = 0.65;
  let pitch = 0.42;
  let dist = 3.1;
  let panX = 0;
  let panY = 0;
  let rafId = 0;
  let destroyed = false;

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
        gl_PointSize = max(1.0, uPointSize / max(gl_Position.w * 0.45, 0.35));
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

    const makeBuf = (data) => {
      const b = gl.createBuffer();
      gl.bindBuffer(gl.ARRAY_BUFFER, b);
      gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
      return b;
    };
    const posBuf = makeBuf(normPos);
    const nrmBuf = makeBuf(normNrm.length === normPos.length ? normNrm : new Float32Array(normPos.length));
    const uvBuf = uvArr.length > 0 ? makeBuf(uvArr) : null;
    const colBuf = colArr.length > 0 ? makeBuf(colArr) : null;
    const wireBuf = makeBuf(wireData);
    const ptPosBuf = makeBuf(ptPos);
    const ptColBuf = ptColRaw.length === ptPos.length ? makeBuf(ptColRaw) : null;

    // Upload embedded GLB/glTF textures asynchronously
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

      const model = mat4RotateXY(pitch, yaw);
      const view = mat4Translate(panX, panY, -dist);
      const proj = mat4Perspective(45 * Math.PI / 180, w / h, 0.05, 100.0);
      const mvp = mat4Mul(proj, mat4Mul(view, model));
      gl.uniformMatrix4fv(locMvp, false, mvp);
      gl.uniformMatrix4fv(locModel, false, model);
      gl.uniform1f(locPtSize, ptSize * dpr);

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
        for (const b of batches) {
          const tex = (showTextures && b.texture_index != null) ? glTextures[b.texture_index] : null;
          if (tex) {
            gl.activeTexture(gl.TEXTURE0);
            gl.bindTexture(gl.TEXTURE_2D, tex);
            gl.uniform1i(locTex, 0);
            gl.uniform1f(locHasTex, 1.0);
          } else {
            gl.uniform1f(locHasTex, 0.0);
          }
          const bc = b.base_color || [0.82, 0.92, 0.92, 1.0];
          gl.uniform4f(locBaseCol, bc[0], bc[1], bc[2], bc[3]);
          gl.drawArrays(gl.TRIANGLES, b.start_vertex, b.vertex_count);
        }
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
    };
  }

  const pointers = new Map();
  let lastPinch = 0;

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
        yaw += dxp * 0.009;
        pitch = Math.max(-1.55, Math.min(1.55, pitch + dyp * 0.009));
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

  hud.addEventListener("click", (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    const act = btn.dataset.act;
    if (act === "solid") { showSolid = !showSolid; btn.classList.toggle("active", showSolid); }
    if (act === "tex") { showTextures = !showTextures; btn.classList.toggle("active", showTextures); }
    if (act === "wire") { showWire = !showWire; btn.classList.toggle("active", showWire); }
    if (act === "pts") { showPoints = !showPoints; btn.classList.toggle("active", showPoints); }
    if (act === "spin") { autoSpin = !autoSpin; btn.classList.toggle("active", autoSpin); }
    if (act === "reset") { yaw = 0.65; pitch = 0.42; dist = 3.1; panX = 0; panY = 0; }
  });
  hud.addEventListener("input", (e) => {
    if (e.target.dataset.act === "ptsize") {
      ptSize = parseFloat(e.target.value) || 4.0;
    }
  });

  function frame() {
    if (destroyed) return;
    if (autoSpin) yaw += 0.0032; // Smooth reduced studio turntable spin speed
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

function mat4RotateXY(rx, ry) {
  const cx = Math.cos(rx), sx = Math.sin(rx);
  const cy = Math.cos(ry), sy = Math.sin(ry);
  return new Float32Array([
    cy, sx * sy, -cx * sy, 0,
    0, cx, sx, 0,
    sy, -sx * cy, cx * cy, 0,
    0, 0, 0, 1,
  ]);
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
