// Runs the WASM editor off the main thread. One document per worker.
import init, { PdfEditor, init_panic_hook } from './pkg/pdf_editor.js';

let editor = null;
const ready = init().then(() => init_panic_hook());

function info() {
  const count = editor.page_count();
  const sizes = [];
  for (let i = 0; i < count; i++) {
    const [w, h] = editor.page_size(i);
    sizes.push([w, h]);
  }
  return { count, sizes, canUndo: editor.can_undo(), canRedo: editor.can_redo() };
}

const ops = {
  load({ bytes }) {
    editor?.free();
    editor = new PdfEditor(new Uint8Array(bytes));
    return info();
  },
  blank({ width, height }) {
    editor?.free();
    editor = PdfEditor.blank(width, height);
    return info();
  },
  info() {
    return info();
  },
  render({ index, scale }) {
    const page = editor.render_page(index, scale);
    const width = page.width;
    const height = page.height;
    const data = page.take_data();
    return { result: { width, height, buffer: data.buffer }, transfer: [data.buffer] };
  },
  rotate({ index, delta }) { editor.rotate_page(index, delta); return info(); },
  delete({ index }) { editor.delete_page(index); return info(); },
  move({ from, to }) { editor.move_page(from, to); return info(); },
  reorder({ order }) { editor.reorder_pages(Uint32Array.from(order)); return info(); },
  duplicate({ index }) { editor.duplicate_page(index); return info(); },
  insertBlank({ index, width, height }) { editor.insert_blank_page(index, width, height); return info(); },
  merge({ bytes, index }) { editor.merge(new Uint8Array(bytes), index ?? undefined); return info(); },
  extract({ indices }) {
    const out = editor.extract_pages(Uint32Array.from(indices));
    return { result: out.buffer, transfer: [out.buffer] };
  },
  save() {
    const out = editor.save();
    return { result: out.buffer, transfer: [out.buffer] };
  },
  undo() { editor.undo(); return info(); },
  redo() { editor.redo(); return info(); },
  addText({ index, x, y, text, font, size, color }) {
    editor.add_text(index, x, y, text, font, size, color[0], color[1], color[2]);
    return info();
  },
  addRect({ index, x, y, w, h, stroke, strokeWidth, fill, opacity, multiply }) {
    const s = stroke ?? [0, 0, 0];
    const f = fill ?? [-1, -1, -1];
    editor.add_rect(index, x, y, w, h, s[0], s[1], s[2], strokeWidth ?? 0, f[0], f[1], f[2], opacity ?? 1, !!multiply);
    return info();
  },
  addInk({ index, points, color, width, opacity }) {
    editor.add_ink(index, Float64Array.from(points), color[0], color[1], color[2], width, opacity ?? 1);
    return info();
  },
  addImage({ index, rgba, imgW, imgH, x, y, w, h }) {
    editor.add_image(index, new Uint8Array(rgba), imgW, imgH, x, y, w, h);
    return info();
  },
};

self.onmessage = async (e) => {
  const { id, op, args } = e.data;
  try {
    await ready;
    if (!ops[op]) throw new Error(`unknown op ${op}`);
    if (op !== 'load' && op !== 'blank' && !editor) throw new Error('no document loaded');
    const out = ops[op](args ?? {});
    if (out && typeof out === 'object' && 'transfer' in out) {
      self.postMessage({ id, ok: true, result: out.result }, out.transfer);
    } else {
      self.postMessage({ id, ok: true, result: out });
    }
  } catch (err) {
    self.postMessage({ id, ok: false, error: String(err?.message ?? err) });
  }
};
