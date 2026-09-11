// UI for the offline PDF editor. All PDF work happens in worker.js (Rust/WASM);
// this file only renders pixels it gets back and translates gestures into ops.

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));

const THUMB_WIDTH = 150; // CSS px
const MAX_RENDER_PIXELS = 4096; // per side, keeps hayro's u16 dims and memory sane
const ZOOM_STEPS = [0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2, 2.5, 3, 4];

// ---------------------------------------------------------------- worker RPC

const worker = new Worker(new URL('./worker.js', import.meta.url), { type: 'module' });
const pending = new Map();
let nextId = 1;

function call(op, args = {}, transfer = []) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    worker.postMessage({ id, op, args }, transfer);
  });
}

worker.onmessage = (e) => {
  const { id, ok, result, error } = e.data;
  const p = pending.get(id);
  if (!p) return;
  pending.delete(id);
  ok ? p.resolve(result) : p.reject(new Error(error));
};

worker.onerror = (e) => setStatus(`Worker error: ${e.message}`, true);

// ---------------------------------------------------------------- state

const state = {
  loaded: false,
  fileName: 'document.pdf',
  count: 0,
  sizes: [], // [[w, h]] in points, post-rotation
  zoom: 1,
  tool: 'select',
  selected: new Set(),
  generation: 0, // bumped whenever page pixels may have changed
  pendingImage: null, // { rgba, w, h } waiting to be placed
};

const els = {
  pages: $('#pages'),
  thumbs: $('#thumbs'),
  viewer: $('#viewer'),
  empty: $('#empty'),
  status: $('#status'),
  zoomLabel: $('#zoom-label'),
  textEditor: $('#text-editor'),
  textInput: $('#text-input'),
};

function setStatus(msg, isError = false) {
  els.status.textContent = msg;
  els.status.classList.toggle('error', isError);
  if (isError) console.error(msg);
}

// ---------------------------------------------------------------- render queue
// The worker is single-threaded, so we hand it one render at a time and prefer
// whatever is currently visible. Each job carries the generation it was queued
// under; results from an older generation are discarded.

const renderQueue = [];
let rendering = false;

function enqueueRender(job) {
  // job: { kind: 'page' | 'thumb', index, canvas, scale, generation }
  const dupe = renderQueue.findIndex((j) => j.kind === job.kind && j.index === job.index);
  if (dupe >= 0) renderQueue.splice(dupe, 1);
  renderQueue.push(job);
  pumpRenders();
}

async function pumpRenders() {
  if (rendering) return;
  rendering = true;
  try {
    while (renderQueue.length) {
      // Visible pages first, then thumbs.
      renderQueue.sort((a, b) => (a.kind === b.kind ? 0 : a.kind === 'page' ? -1 : 1));
      const job = renderQueue.shift();
      if (job.generation !== state.generation || !job.canvas.isConnected) continue;
      try {
        const { width, height, buffer } = await call('render', { index: job.index, scale: job.scale });
        if (job.generation !== state.generation || !job.canvas.isConnected) continue;
        const img = new ImageData(new Uint8ClampedArray(buffer), width, height);
        job.canvas.width = width;
        job.canvas.height = height;
        job.canvas.getContext('2d').putImageData(img, 0, 0);
        job.canvas.dataset.generation = String(job.generation);
        job.canvas.closest('.page')?.classList.remove('pending');
      } catch (err) {
        setStatus(`Render failed for page ${job.index + 1}: ${err.message}`, true);
      }
    }
  } finally {
    rendering = false;
  }
}

// ---------------------------------------------------------------- document sync

function applyInfo(info) {
  state.count = info.count;
  state.sizes = info.sizes;
  state.generation++;
  renderQueue.length = 0;
  $('#btn-undo').disabled = !info.canUndo;
  $('#btn-redo').disabled = !info.canRedo;
  for (const i of [...state.selected]) if (i >= state.count) state.selected.delete(i);
  buildPages();
  buildThumbs();
  updateSelectionUI();
  setStatus(`${state.count} page${state.count === 1 ? '' : 's'} · ${state.fileName}`);
}

function setLoaded(loaded) {
  state.loaded = loaded;
  els.empty.hidden = loaded;
  for (const id of ['#btn-merge', '#btn-save']) $(id).disabled = !loaded;
}

async function mutate(op, args, transfer) {
  try {
    const info = await call(op, args, transfer);
    applyInfo(info);
  } catch (err) {
    setStatus(`${op} failed: ${err.message}`, true);
  }
}

// ---------------------------------------------------------------- pages (main view)

const pageObserver = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      const el = entry.target;
      el.dataset.visible = entry.isIntersecting ? '1' : '';
      if (entry.isIntersecting) requestPageRender(el);
    }
  },
  { root: els.viewer, rootMargin: '300px 0px' },
);

function pageScale() {
  return state.zoom * (window.devicePixelRatio || 1);
}

function requestPageRender(el) {
  const canvas = el.querySelector('canvas.render');
  if (canvas.dataset.generation === String(state.generation) && Number(canvas.dataset.scale) === pageScale()) return;
  const index = Number(el.dataset.index);
  const [w, h] = state.sizes[index];
  let scale = pageScale();
  const maxSide = Math.max(w, h) * scale;
  if (maxSide > MAX_RENDER_PIXELS) scale *= MAX_RENDER_PIXELS / maxSide;
  canvas.dataset.scale = String(pageScale());
  el.classList.add('pending');
  enqueueRender({ kind: 'page', index, canvas, scale, generation: state.generation });
}

function buildPages() {
  const existing = $$('#pages .page');
  // Reconcile element count.
  while (existing.length > state.count) {
    const el = existing.pop();
    pageObserver.unobserve(el);
    el.remove();
  }
  while (existing.length < state.count) {
    const el = document.createElement('div');
    el.className = 'page';
    el.innerHTML = '<canvas class="render"></canvas><canvas class="overlay"></canvas><span class="page-label"></span>';
    bindOverlay(el);
    els.pages.appendChild(el);
    pageObserver.observe(el);
    existing.push(el);
  }
  existing.forEach((el, i) => {
    el.dataset.index = String(i);
    el.querySelector('.page-label').textContent = `Page ${i + 1}`;
    sizePage(el, i);
    if (el.dataset.visible) requestPageRender(el);
  });
}

function sizePage(el, index) {
  const [w, h] = state.sizes[index];
  el.style.width = `${w * state.zoom}px`;
  el.style.height = `${h * state.zoom}px`;
  const overlay = el.querySelector('canvas.overlay');
  overlay.width = Math.round(w * state.zoom * (window.devicePixelRatio || 1));
  overlay.height = Math.round(h * state.zoom * (window.devicePixelRatio || 1));
}

function setZoom(z, anchorRatio) {
  const viewer = els.viewer;
  const ratio = anchorRatio ?? (viewer.scrollTop + viewer.clientHeight / 2) / Math.max(1, viewer.scrollHeight);
  state.zoom = Math.min(8, Math.max(0.1, z));
  els.zoomLabel.textContent = `${Math.round(state.zoom * 100)}%`;
  $$('#pages .page').forEach((el, i) => {
    sizePage(el, i);
    if (el.dataset.visible) requestPageRender(el);
  });
  viewer.scrollTop = ratio * viewer.scrollHeight - viewer.clientHeight / 2;
}

function zoomStep(dir) {
  const cur = state.zoom;
  const next = dir > 0 ? ZOOM_STEPS.find((z) => z > cur + 1e-6) : [...ZOOM_STEPS].reverse().find((z) => z < cur - 1e-6);
  if (next) setZoom(next);
}

function zoomFit() {
  if (!state.count) return;
  const maxW = Math.max(...state.sizes.map(([w]) => w));
  const avail = els.viewer.clientWidth - 48 - 16; // padding + scrollbar
  setZoom(avail / maxW, 0);
}

// ---------------------------------------------------------------- thumbnails

const thumbObserver = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      entry.target.dataset.visible = entry.isIntersecting ? '1' : '';
      if (entry.isIntersecting) requestThumbRender(entry.target);
    }
  },
  { root: els.thumbs, rootMargin: '200px 0px' },
);

function requestThumbRender(el) {
  const canvas = el.querySelector('canvas');
  if (canvas.dataset.generation === String(state.generation)) return;
  const index = Number(el.dataset.index);
  const [w] = state.sizes[index];
  const scale = (THUMB_WIDTH * (window.devicePixelRatio || 1)) / w;
  enqueueRender({ kind: 'thumb', index, canvas, scale, generation: state.generation });
}

function buildThumbs() {
  const existing = $$('#thumbs .thumb');
  while (existing.length > state.count) {
    const el = existing.pop();
    thumbObserver.unobserve(el);
    el.remove();
  }
  while (existing.length < state.count) {
    const el = document.createElement('div');
    el.className = 'thumb';
    el.draggable = true;
    el.innerHTML = '<canvas></canvas><span class="num"></span>';
    bindThumb(el);
    els.thumbs.appendChild(el);
    thumbObserver.observe(el);
    existing.push(el);
  }
  existing.forEach((el, i) => {
    el.dataset.index = String(i);
    el.querySelector('.num').textContent = String(i + 1);
    const [w, h] = state.sizes[i];
    el.querySelector('canvas').style.aspectRatio = `${w} / ${h}`;
    if (el.dataset.visible) requestThumbRender(el);
  });
}

let dragIndex = null;

function bindThumb(el) {
  el.addEventListener('click', (e) => {
    const i = Number(el.dataset.index);
    if (e.shiftKey && state.selected.size) {
      const anchor = Math.min(...state.selected);
      const [a, b] = [Math.min(anchor, i), Math.max(anchor, i)];
      for (let k = a; k <= b; k++) state.selected.add(k);
    } else if (e.ctrlKey || e.metaKey) {
      state.selected.has(i) ? state.selected.delete(i) : state.selected.add(i);
    } else {
      state.selected.clear();
      state.selected.add(i);
      $$('#pages .page')[i]?.scrollIntoView({ block: 'start', behavior: 'smooth' });
    }
    updateSelectionUI();
  });
  el.addEventListener('dragstart', (e) => {
    dragIndex = Number(el.dataset.index);
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', String(dragIndex));
  });
  el.addEventListener('dragover', (e) => {
    if (dragIndex === null) return;
    e.preventDefault();
    const before = e.offsetY < el.offsetHeight / 2;
    el.classList.toggle('drag-over-before', before);
    el.classList.toggle('drag-over-after', !before);
  });
  el.addEventListener('dragleave', () => el.classList.remove('drag-over-before', 'drag-over-after'));
  el.addEventListener('drop', async (e) => {
    e.preventDefault();
    el.classList.remove('drag-over-before', 'drag-over-after');
    if (dragIndex === null) return;
    const from = dragIndex;
    dragIndex = null;
    const target = Number(el.dataset.index);
    const before = e.offsetY < el.offsetHeight / 2;
    let to = before ? target : target + 1;
    if (from < to) to--;
    if (to === from) return;
    state.selected = new Set([to]);
    await mutate('move', { from, to });
  });
  el.addEventListener('dragend', () => {
    dragIndex = null;
    $$('.thumb').forEach((t) => t.classList.remove('drag-over-before', 'drag-over-after'));
  });
}

function updateSelectionUI() {
  $$('#thumbs .thumb').forEach((el, i) => el.classList.toggle('selected', state.selected.has(i)));
  const any = state.selected.size > 0 && state.loaded;
  for (const id of ['#pg-rotl', '#pg-rotr', '#pg-dup', '#pg-blank', '#pg-extract', '#pg-del']) $(id).disabled = !any;
  $('#pg-del').disabled = !any || state.selected.size >= state.count;
}

function selectedSorted() {
  return [...state.selected].sort((a, b) => a - b);
}

// ---------------------------------------------------------------- tools / overlay

function setTool(tool) {
  state.tool = tool;
  document.body.dataset.tool = tool;
  $$('#tools button').forEach((b) => b.classList.toggle('active', b.dataset.tool === tool));
  $$('#options label').forEach((l) => (l.hidden = !l.dataset.for.split(' ').includes(tool)));
  hideTextEditor();
  if (tool === 'image' && !state.pendingImage) $('#file-image').click();
}

function hexToRgb(hex) {
  const v = parseInt(hex.slice(1), 16);
  return [((v >> 16) & 255) / 255, ((v >> 8) & 255) / 255, (v & 255) / 255];
}

function opts() {
  return {
    color: hexToRgb($('#opt-color').value),
    size: Number($('#opt-size').value) || 14,
    font: $('#opt-font').value,
    width: Number($('#opt-width').value) || 1,
    opacity: Number($('#opt-opacity').value) / 100,
    fill: $('#opt-fill').checked,
  };
}

/** Pointer position → view space (points, y-down) for a page element. */
function toView(el, e) {
  const r = el.getBoundingClientRect();
  return { x: (e.clientX - r.left) / state.zoom, y: (e.clientY - r.top) / state.zoom };
}

function bindOverlay(el) {
  const overlay = el.querySelector('canvas.overlay');
  let drag = null; // { start, points }

  const ctx = () => overlay.getContext('2d');
  const dpr = () => window.devicePixelRatio || 1;
  const clear = () => ctx().clearRect(0, 0, overlay.width, overlay.height);

  overlay.addEventListener('pointerdown', (e) => {
    if (e.button !== 0 || !state.loaded) return;
    const index = Number(el.dataset.index);
    const p = toView(el, e);
    state.selected = new Set([index]);
    updateSelectionUI();

    switch (state.tool) {
      case 'text':
        showTextEditor(el, index, p, e);
        return;
      case 'image':
        if (!state.pendingImage) {
          $('#file-image').click();
          return;
        }
        // fallthrough — image uses drag box like rect
      case 'highlight':
      case 'rect':
      case 'draw':
        drag = { start: p, points: [p.x, p.y] };
        overlay.setPointerCapture(e.pointerId);
        e.preventDefault();
        return;
      default:
        return;
    }
  });

  overlay.addEventListener('pointermove', (e) => {
    if (!drag) return;
    const p = toView(el, e);
    const o = opts();
    const c = ctx();
    const s = state.zoom * dpr();
    clear();
    c.lineCap = c.lineJoin = 'round';
    if (state.tool === 'draw') {
      drag.points.push(p.x, p.y);
      c.strokeStyle = $('#opt-color').value;
      c.globalAlpha = o.opacity;
      c.lineWidth = o.width * s;
      c.beginPath();
      c.moveTo(drag.points[0] * s, drag.points[1] * s);
      for (let i = 2; i < drag.points.length; i += 2) c.lineTo(drag.points[i] * s, drag.points[i + 1] * s);
      c.stroke();
    } else {
      const box = normBox(drag.start, p, state.tool === 'image' ? state.pendingImage : null);
      drag.box = box;
      if (state.tool === 'highlight') {
        c.globalAlpha = o.opacity * 0.6;
        c.fillStyle = $('#opt-color').value;
        c.fillRect(box.x * s, box.y * s, box.w * s, box.h * s);
      } else if (state.tool === 'rect') {
        c.globalAlpha = o.opacity;
        if (o.fill) {
          c.fillStyle = $('#opt-color').value;
          c.fillRect(box.x * s, box.y * s, box.w * s, box.h * s);
        }
        c.strokeStyle = $('#opt-color').value;
        c.lineWidth = o.width * s;
        c.strokeRect(box.x * s, box.y * s, box.w * s, box.h * s);
      } else if (state.tool === 'image') {
        c.globalAlpha = 0.8;
        c.drawImage(state.pendingImage.bitmap, box.x * s, box.y * s, box.w * s, box.h * s);
      }
    }
  });

  const finish = async (e) => {
    if (!drag) return;
    const d = drag;
    drag = null;
    clear();
    const index = Number(el.dataset.index);
    const o = opts();
    const p = toView(el, e);

    if (state.tool === 'draw') {
      if (d.points.length >= 4) {
        await mutate('addInk', { index, points: d.points, color: o.color, width: o.width, opacity: o.opacity });
      }
      return;
    }

    let box = d.box ?? normBox(d.start, p, state.tool === 'image' ? state.pendingImage : null);
    if (state.tool === 'image') {
      const img = state.pendingImage;
      if (box.w < 4 || box.h < 4) {
        // Plain click: place at a sensible default size, centred on the click.
        const [pw] = state.sizes[index];
        const w = Math.min(img.w, pw * 0.5);
        const h = (w * img.h) / img.w;
        box = { x: d.start.x - w / 2, y: d.start.y - h / 2, w, h };
      }
      const rgba = img.rgba.slice();
      await mutate('addImage', { index, rgba: rgba.buffer, imgW: img.w, imgH: img.h, ...box }, [rgba.buffer]);
      return;
    }

    if (box.w < 1 || box.h < 1) return;
    if (state.tool === 'highlight') {
      await mutate('addRect', { index, ...box, strokeWidth: 0, fill: o.color, opacity: o.opacity, multiply: true });
    } else if (state.tool === 'rect') {
      await mutate('addRect', {
        index,
        ...box,
        stroke: o.color,
        strokeWidth: o.width,
        fill: o.fill ? o.color : null,
        opacity: o.opacity,
        multiply: false,
      });
    }
  };
  overlay.addEventListener('pointerup', finish);
  overlay.addEventListener('pointercancel', () => {
    drag = null;
    clear();
  });
}

function normBox(a, b, aspectImg) {
  let x = Math.min(a.x, b.x);
  let y = Math.min(a.y, b.y);
  let w = Math.abs(b.x - a.x);
  let h = Math.abs(b.y - a.y);
  if (aspectImg && w > 0) {
    h = (w * aspectImg.h) / aspectImg.w;
    if (b.y < a.y) y = a.y - h;
  }
  return { x, y, w, h };
}

// ---------------------------------------------------------------- text tool

let textTarget = null; // { index, x, y }

function showTextEditor(pageEl, index, p, e) {
  textTarget = { index, x: p.x, y: p.y };
  const ed = els.textEditor;
  const o = opts();
  ed.hidden = false;
  // Keep the popover on-screen even near the right/bottom edges.
  ed.style.left = `${Math.min(e.clientX, window.innerWidth - 280)}px`;
  ed.style.top = `${Math.min(e.clientY, window.innerHeight - 140)}px`;
  els.textInput.style.fontSize = `${Math.max(11, o.size * state.zoom)}px`;
  els.textInput.style.color = $('#opt-color').value;
  els.textInput.value = '';
  els.textInput.focus();
}

function hideTextEditor() {
  els.textEditor.hidden = true;
  textTarget = null;
}

async function commitText() {
  const text = els.textInput.value.replace(/\r/g, '').replace(/\s+$/, '');
  const t = textTarget;
  hideTextEditor();
  if (!t || !text) return;
  const o = opts();
  // The click is where the user expects the top-left of the text; the PDF baseline is
  // roughly 0.8em below the cap line for the standard fonts.
  await mutate('addText', { index: t.index, x: t.x, y: t.y + o.size * 0.8, text, font: o.font, size: o.size, color: o.color });
}

$('#text-commit').addEventListener('click', commitText);
$('#text-cancel').addEventListener('click', hideTextEditor);
els.textInput.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    commitText();
  } else if (e.key === 'Escape') {
    hideTextEditor();
  }
});

// ---------------------------------------------------------------- file I/O

async function openFile(file) {
  if (!file) return;
  setStatus(`Opening ${file.name}…`);
  try {
    const bytes = await file.arrayBuffer();
    const info = await call('load', { bytes }, [bytes]);
    state.fileName = file.name;
    state.selected.clear();
    setLoaded(true);
    applyInfo(info);
    zoomFit();
  } catch (err) {
    setStatus(`Could not open ${file.name}: ${err.message}`, true);
  }
}

async function newBlank() {
  try {
    const info = await call('blank', { width: 612, height: 792 });
    state.fileName = 'untitled.pdf';
    state.selected.clear();
    setLoaded(true);
    applyInfo(info);
    zoomFit();
  } catch (err) {
    setStatus(`Could not create document: ${err.message}`, true);
  }
}

async function mergeFile(file) {
  if (!file) return;
  setStatus(`Adding ${file.name}…`);
  const bytes = await file.arrayBuffer();
  const at = state.selected.size ? Math.max(...state.selected) + 1 : undefined;
  await mutate('merge', { bytes, index: at }, [bytes]);
}

function download(buffer, name) {
  const blob = new Blob([buffer], { type: 'application/pdf' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

function outName(suffix) {
  return state.fileName.replace(/\.pdf$/i, '') + suffix + '.pdf';
}

async function saveDoc() {
  try {
    setStatus('Saving…');
    const buf = await call('save');
    download(buf, outName('-edited'));
    setStatus(`Saved ${outName('-edited')} (${(buf.byteLength / 1024).toFixed(0)} KB)`);
  } catch (err) {
    setStatus(`Save failed: ${err.message}`, true);
  }
}

async function extractSelected() {
  const indices = selectedSorted();
  if (!indices.length) return;
  try {
    const buf = await call('extract', { indices });
    download(buf, outName(`-pages-${indices.map((i) => i + 1).join('_')}`));
    setStatus(`Extracted ${indices.length} page${indices.length === 1 ? '' : 's'}`);
  } catch (err) {
    setStatus(`Extract failed: ${err.message}`, true);
  }
}

async function loadImage(file) {
  if (!file) return;
  try {
    const bitmap = await createImageBitmap(file);
    const MAX = 2000;
    const k = Math.min(1, MAX / Math.max(bitmap.width, bitmap.height));
    const w = Math.max(1, Math.round(bitmap.width * k));
    const h = Math.max(1, Math.round(bitmap.height * k));
    const c = new OffscreenCanvas(w, h);
    const cx = c.getContext('2d');
    cx.drawImage(bitmap, 0, 0, w, h);
    const rgba = cx.getImageData(0, 0, w, h).data;
    state.pendingImage = { rgba, w, h, bitmap };
    setTool('image');
    setStatus(`Image ready (${w}×${h}) — click or drag on a page to place it`);
  } catch (err) {
    setStatus(`Could not read image: ${err.message}`, true);
  }
}

// ---------------------------------------------------------------- page actions

async function forSelected(op, argsFor, reverse = false) {
  let idx = selectedSorted();
  if (reverse) idx = idx.reverse();
  for (const i of idx) {
    try {
      const info = await call(op, argsFor(i));
      applyInfo(info);
    } catch (err) {
      setStatus(`${op} failed: ${err.message}`, true);
      return;
    }
  }
}

$('#pg-rotl').addEventListener('click', () => forSelected('rotate', (i) => ({ index: i, delta: -90 })));
$('#pg-rotr').addEventListener('click', () => forSelected('rotate', (i) => ({ index: i, delta: 90 })));
$('#pg-dup').addEventListener('click', async () => {
  // Duplicate from the end so earlier indices stay valid.
  const idx = selectedSorted().reverse();
  for (const i of idx) await mutate('duplicate', { index: i });
});
$('#pg-blank').addEventListener('click', async () => {
  const after = state.selected.size ? Math.max(...state.selected) + 1 : state.count;
  const [w, h] = state.sizes[Math.max(0, after - 1)] ?? [612, 792];
  await mutate('insertBlank', { index: after, width: w, height: h });
  state.selected = new Set([after]);
  updateSelectionUI();
});
$('#pg-del').addEventListener('click', async () => {
  const idx = selectedSorted().reverse();
  if (idx.length >= state.count) return;
  for (const i of idx) await mutate('delete', { index: i });
  state.selected.clear();
  updateSelectionUI();
});
$('#pg-extract').addEventListener('click', extractSelected);

// ---------------------------------------------------------------- toolbar wiring

$('#btn-open').addEventListener('click', () => $('#file-open').click());
$('#empty-open').addEventListener('click', () => $('#file-open').click());
$('#btn-new').addEventListener('click', newBlank);
$('#empty-new').addEventListener('click', newBlank);
$('#btn-merge').addEventListener('click', () => $('#file-merge').click());
$('#btn-save').addEventListener('click', saveDoc);
$('#btn-undo').addEventListener('click', () => mutate('undo'));
$('#btn-redo').addEventListener('click', () => mutate('redo'));
$('#zoom-in').addEventListener('click', () => zoomStep(1));
$('#zoom-out').addEventListener('click', () => zoomStep(-1));
$('#zoom-fit').addEventListener('click', zoomFit);
$$('#tools button').forEach((b) => b.addEventListener('click', () => setTool(b.dataset.tool)));

$('#file-open').addEventListener('change', (e) => {
  openFile(e.target.files[0]);
  e.target.value = '';
});
$('#file-merge').addEventListener('change', (e) => {
  mergeFile(e.target.files[0]);
  e.target.value = '';
});
$('#file-image').addEventListener('change', (e) => {
  if (e.target.files[0]) loadImage(e.target.files[0]);
  else if (!state.pendingImage) setTool('select');
  e.target.value = '';
});

document.addEventListener('keydown', (e) => {
  const inField = ['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement?.tagName);
  const mod = e.ctrlKey || e.metaKey;
  if (mod && e.key.toLowerCase() === 'o') { e.preventDefault(); $('#file-open').click(); return; }
  if (mod && e.key.toLowerCase() === 's') { e.preventDefault(); if (state.loaded) saveDoc(); return; }
  if (mod && (e.key === '=' || e.key === '+')) { e.preventDefault(); zoomStep(1); return; }
  if (mod && e.key === '-') { e.preventDefault(); zoomStep(-1); return; }
  if (inField) return;
  if (mod && e.key.toLowerCase() === 'z') { e.preventDefault(); e.shiftKey ? mutate('redo') : mutate('undo'); return; }
  if (mod && e.key.toLowerCase() === 'y') { e.preventDefault(); mutate('redo'); return; }
  if (mod) return;
  const tools = { v: 'select', t: 'text', h: 'highlight', r: 'rect', d: 'draw', i: 'image' };
  if (tools[e.key.toLowerCase()]) setTool(tools[e.key.toLowerCase()]);
  if (e.key === 'Escape') { state.pendingImage = null; setTool('select'); }
  if (e.key === 'Delete' && state.selected.size && state.selected.size < state.count) $('#pg-del').click();
});

// Drag & drop anywhere.
let dragDepth = 0;
window.addEventListener('dragenter', (e) => {
  if (!e.dataTransfer?.types.includes('Files')) return;
  dragDepth++;
  $('#drop-hint').hidden = false;
});
window.addEventListener('dragleave', () => {
  if (--dragDepth <= 0) { dragDepth = 0; $('#drop-hint').hidden = true; }
});
window.addEventListener('dragover', (e) => { if (e.dataTransfer?.types.includes('Files')) e.preventDefault(); });
window.addEventListener('drop', (e) => {
  if (!e.dataTransfer?.types.includes('Files')) return;
  e.preventDefault();
  dragDepth = 0;
  $('#drop-hint').hidden = true;
  const file = e.dataTransfer.files[0];
  if (!file) return;
  if (file.type.startsWith('image/')) loadImage(file);
  else if (state.loaded && e.shiftKey) mergeFile(file);
  else openFile(file);
});

// Ctrl+wheel zoom.
els.viewer.addEventListener('wheel', (e) => {
  if (!e.ctrlKey) return;
  e.preventDefault();
  zoomStep(e.deltaY < 0 ? 1 : -1);
}, { passive: false });

els.viewer.addEventListener('scroll', hideTextEditor, { passive: true });

window.addEventListener('resize', () => {
  $$('#pages .page').forEach((el) => el.dataset.visible && requestPageRender(el));
});

// ---------------------------------------------------------------- boot

setTool('select');
setLoaded(false);
updateSelectionUI();

// Offline support. Skipped on localhost so development always sees fresh files.
const isLocalDev = ['localhost', '127.0.0.1'].includes(location.hostname);
if ('serviceWorker' in navigator && location.protocol.startsWith('http') && !isLocalDev) {
  navigator.serviceWorker.register('sw.js').catch((err) => console.warn('SW registration failed', err));
} else if ('serviceWorker' in navigator && isLocalDev) {
  navigator.serviceWorker.getRegistrations().then((rs) => rs.forEach((r) => r.unregister()));
}

// Small programmatic surface (handy for testing and for embedding).
window.pdfEditor = { openFile, mergeFile, loadImage, call, mutate, setZoom, state };
