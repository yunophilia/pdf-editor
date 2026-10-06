//! Cross-platform PDF editor UI. The same component tree runs on the web
//! (wasm + a Web Worker) and on the desktop (native engine thread + webview).

pub mod app;
pub mod engine;
pub mod files;

pub use app::App;
pub use engine::Engine;

/// Web entry point. The desktop build starts from `main.rs` instead.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    dioxus::launch(App);
}
