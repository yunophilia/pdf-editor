# PDF Editor

An offline PDF editor that runs entirely in the browser. The core is Rust
compiled to WebAssembly; the UI is plain HTML/CSS/JS with no build step or
framework. Everything is static, so it deploys straight to GitHub Pages and
keeps working with no network once loaded (it installs as a PWA).

Nothing you open ever leaves your machine.

## Features

- Open, view and zoom PDFs (rendered by [hayro](https://github.com/LaurenzV/hayro), a pure-Rust rasteriser)
- Page management: reorder (drag thumbnails), rotate, duplicate, delete, insert blank pages
- Merge another PDF in, or extract selected pages to a new file
- Annotate: text (standard fonts), highlighter, rectangles, freehand drawing, images
- Undo / redo
- Save the result as a new PDF

Edits are written as real PDF content (via [lopdf](https://github.com/J-F-Liu/lopdf)),
so they show up in every viewer.

## Layout

```
src/lib.rs      Rust core: page tree editing, annotations, rendering (wasm-bindgen API)
www/            The static site
  index.html    UI shell
  app.js        UI logic, talks to the worker
  worker.js     Web Worker that hosts the WASM module
  sw.js         Service worker for offline use
  pkg/          wasm-pack output (generated, not committed)
serve.py        Dev server with correct MIME types
tests/          Fixtures for `cargo test`
```

## Building locally

Requirements: Rust (stable), the `wasm32-unknown-unknown` target, and `wasm-pack`.

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack
```

Build the WASM package and serve the site:

```bash
wasm-pack build --target web --release --out-dir www/pkg
python serve.py
```

Then open <http://127.0.0.1:8765>. (Any static server works, as long as it
serves `.js` as `text/javascript` and `.wasm` as `application/wasm` — Python's
default `http.server` on Windows does not, hence `serve.py`.)

Run the tests with `cargo test`.

## Deploying to GitHub Pages

1. Push this repository to GitHub with the default branch named `main`.
2. In the repository settings, under **Pages**, set **Source** to **GitHub Actions**.
3. Every push to `main` runs [`.github/workflows/deploy.yml`](.github/workflows/deploy.yml),
   which tests, builds the WASM, and publishes `www/` to
   `https://<user>.github.io/<repo>/`.

All asset paths are relative, so the site works from a sub-path without
configuration.

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| `Ctrl+O` / `Ctrl+S` | Open / Save |
| `Ctrl+Z` / `Ctrl+Y` | Undo / Redo |
| `Ctrl` + `+` / `-` / wheel | Zoom |
| `V` `T` `H` `R` `D` `I` | Select, Text, Highlight, Rect, Draw, Image tools |
| `Delete` | Delete selected pages |
| `Esc` | Back to Select tool |
| Drop a PDF | Open it (hold `Shift` to append to the current document) |

## Limitations

- Password-protected PDFs can't be opened.
- Text annotations use the 14 standard PDF fonts (Latin-1 characters only).
- Existing text and objects can't be edited or removed — annotations are added
  on top. Use Undo to take back an annotation you just added.
- Rendering runs on the CPU in a worker; very complex pages take a moment.
