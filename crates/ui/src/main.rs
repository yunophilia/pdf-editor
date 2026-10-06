//! Desktop entry point. On wasm the app starts from the library's
//! `#[wasm_bindgen(start)]` instead, so this binary is empty there.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    use dioxus::desktop::{Config, LogicalSize, WindowBuilder};

    let window = WindowBuilder::new()
        .with_title("PDF Editor")
        .with_inner_size(LogicalSize::new(1280.0, 860.0));

    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(window))
        .launch(pdf_editor_ui::App);
}

#[cfg(target_arch = "wasm32")]
fn main() {}
