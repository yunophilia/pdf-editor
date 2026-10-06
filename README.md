# PDF Editor

A PDF editor written in Rust that runs **entirely offline**, as a web app and as
a native desktop app from the same codebase. Nothing you open ever leaves your
machine — there is no server, no upload, no telemetry.

- **Web:** <https://yunophilia.github.io/pdf-editor/> — static files on GitHub
  Pages; installs as a PWA and keeps working with no network after first load.
- **Desktop:** a native binary for Windows, macOS and Linux.

## Features

- View, zoom and page through PDFs (rendered by [hayro](https://github.com/LaurenzV/hayro), a pure-Rust rasteriser)
- Page management: rotate, reorder, duplicate, delete, insert blank pages
- Merge another PDF in, or extract selected pages to a new file
- **Interactive form (AcroForm) editing** — text, multiline, checkbox, radio
  groups and dropdowns, edited in place over the page, with appearance streams
  regenerated so the values show up in every viewer. Forms can be flattened.
- Annotate: text, highlighter, rectangles, freehand ink, images
- Undo / redo, and save to a new PDF

Edits are written as real PDF content via [lopdf](https://github.com/J-F-Liu/lopdf),
so other viewers see them too.

## How it is put together

```
crates/shared   Types crossing the UI <-> engine boundary (serde). Deliberately
                dependency-light so the web UI's wasm need not link the engine.
crates/core     The engine: page tree, content streams, forms, rasterising.
                Plain Rust — no wasm-bindgen — so it builds natively too.
crates/worker   Thin wasm shim hosting the engine in a Web Worker (web only).
crates/ui       Dioxus app. One component tree for both targets.
web/            Static shell for the web build (HTML, service worker, manifest).
```

Two things, and only two, are platform-specific:

| | Web | Desktop |
| --- | --- | --- |
| Engine transport | Web Worker, `postMessage` | a thread that owns the editor |
| File dialogs | file input / object URL | `rfd` native dialogs |

Everything else is shared. Pages are rasterised to PNG by the engine and shown
as `<img>`, with an SVG overlay for live tool feedback and real inputs for form
fields, so the view layer needs no canvas interop and behaves identically in a
browser and in the desktop webview. Image decoding uses the `image` crate on
both targets.

The desktop build is not merely the web build in a window: the engine is
compiled natively there, so it has real threads and real file I/O, and none of
the wasm download cost.

## Building

Requirements: Rust (stable). For the web build also:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
```

### Desktop

```bash
cargo run --release -p pdf-editor-ui
```

Pass a path to open a document at launch: `cargo run -p pdf-editor-ui -- file.pdf`.

On Linux you will need the webview development packages
(`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`).

### Web

```bash
./build-web.sh && python serve.py
```

Then open <http://127.0.0.1:8765>. `?open=<same-origin path>` loads a PDF
straight away, e.g. `http://127.0.0.1:8765/?open=sample.pdf`.

Any static server works, provided it serves `.js` as `text/javascript` and
`.wasm` as `application/wasm` — Python's stock `http.server` does not on
Windows, which is why `serve.py` exists.

### Tests

```bash
cargo test --workspace
```

The engine tests check real output: pixels sampled from rendered pages confirm
that annotations land where they were placed (including on rotated pages),
that glyphs sit the right way up, and that form edits and flattening show up
when re-rendered.

## Deploying

Push to `main`; [`.github/workflows/deploy.yml`](.github/workflows/deploy.yml)
tests, builds both wasm bundles, publishes `web/` to Pages, and uploads desktop
binaries for all three platforms as build artifacts.

The service worker fetches the page itself network-first and everything else
cache-first, so a deploy takes effect on the next load rather than after an
extra reload, while the app still works fully offline.

Pages has to be enabled on the repository first — **Settings → Pages → Source:
GitHub Actions**. The workflow's own token is not permitted to create the Pages
site, so this one step cannot be automated.

## Limitations

- Password-protected PDFs cannot be opened.
- Text uses the 14 standard PDF fonts (Latin-1 only); there is no font embedding.
- Existing page content cannot be edited or removed — annotations are layered on
  top. Undo takes back anything you just added.
- Signature fields are shown but cannot be signed.
- Rendering is CPU-only and single-threaded on the web (wasm threads need
  `SharedArrayBuffer`, which needs COOP/COEP headers that GitHub Pages cannot
  set). The desktop build has no such limit.
