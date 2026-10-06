// Hosts the Rust PDF engine off the main thread.
//
// Structured requests carry JSON; rasters and file bytes travel as buffers and
// are transferred rather than copied where possible.
import init, { Engine } from './worker-pkg/pdf_editor_worker.js';

const ready = init();
let engine = null;

function requireEngine() {
  if (!engine) throw new Error('no document is open');
  return engine;
}

const ops = {
  open: (m) => {
    engine?.free();
    engine = Engine.open(new Uint8Array(m.data));
    return engine.exec(JSON.stringify('Info'));
  },
  blank: (m) => {
    engine?.free();
    engine = Engine.blank(m.width, m.height);
    return engine.exec(JSON.stringify('Info'));
  },
  exec: (m) => requireEngine().exec(m.command),
  render: (m) => requireEngine().render(m.index, m.scale).take(),
  addImage: (m) =>
    requireEngine().add_image(
      m.page,
      new Uint8Array(m.data),
      m.imgWidth,
      m.imgHeight,
      m.x,
      m.y,
      m.width,
      m.height,
    ),
  merge: (m) => requireEngine().merge(new Uint8Array(m.data), m.index ?? undefined),
  save: () => requireEngine().save(),
  extract: (m) => requireEngine().extract(Uint32Array.from(m.pages)),
};

self.onmessage = async (event) => {
  const message = event.data;
  try {
    await ready;
    const handler = ops[message.op];
    if (!handler) throw new Error(`unknown op ${message.op}`);
    const result = handler(message);
    // Byte results are transferred so large rasters are not copied.
    const transfer = result instanceof Uint8Array ? [result.buffer] : [];
    self.postMessage({ id: message.id, ok: true, result }, transfer);
  } catch (err) {
    self.postMessage({ id: message.id, ok: false, result: String(err?.message ?? err) });
  }
};
