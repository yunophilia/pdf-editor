//! Web-only shim: hosts the engine inside a Web Worker.
//!
//! Structured requests travel as JSON (they are small); page rasters and file
//! bytes are passed as raw buffers so they never go through a JSON encoder.
//! The desktop build does not use this crate — it owns a [`PdfEditor`] on a
//! background thread instead.

use pdf_editor_core::PdfEditor;
use pdf_editor_shared::{Command, ImageSpec, Response};
use wasm_bindgen::prelude::*;

fn err<E: std::fmt::Display>(e: E) -> JsError {
    JsError::new(&e.to_string())
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// A rasterised page handed to JS as a PNG.
#[wasm_bindgen]
pub struct Png(Vec<u8>);

#[wasm_bindgen]
impl Png {
    /// Moves the buffer out so it can be transferred rather than copied.
    pub fn take(self) -> Vec<u8> {
        self.0
    }
}

#[wasm_bindgen]
pub struct Engine {
    inner: PdfEditor,
}

#[wasm_bindgen]
impl Engine {
    /// Open a document from its bytes.
    pub fn open(bytes: &[u8]) -> Result<Engine, JsError> {
        Ok(Engine {
            inner: PdfEditor::new(bytes).map_err(err)?,
        })
    }

    /// Start a new document with a single blank page.
    pub fn blank(width: f64, height: f64) -> Result<Engine, JsError> {
        Ok(Engine {
            inner: PdfEditor::blank(width, height).map_err(err)?,
        })
    }

    /// Run one structured command; returns a JSON-encoded [`Response`].
    pub fn exec(&mut self, command: &str) -> Result<String, JsError> {
        let cmd: Command = serde_json::from_str(command).map_err(err)?;
        let response = self.inner.apply(cmd).map_err(err)?;
        serde_json::to_string(&response).map_err(err)
    }

    pub fn render(&mut self, index: usize, scale: f32) -> Result<Png, JsError> {
        Ok(Png(self.inner.render_page_png(index, scale).map_err(err)?))
    }

    /// Place an image. The pixels are passed separately so they stay out of JSON.
    #[allow(clippy::too_many_arguments)]
    pub fn add_image(
        &mut self,
        page: usize,
        rgba: Vec<u8>,
        img_width: u32,
        img_height: u32,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Result<String, JsError> {
        self.inner
            .add_image(&ImageSpec {
                page,
                rgba,
                img_width,
                img_height,
                x,
                y,
                width,
                height,
            })
            .map_err(err)?;
        self.info()
    }

    pub fn merge(&mut self, bytes: &[u8], index: Option<usize>) -> Result<String, JsError> {
        self.inner.merge(bytes, index).map_err(err)?;
        self.info()
    }

    pub fn save(&mut self) -> Result<Vec<u8>, JsError> {
        self.inner.save().map_err(err)
    }

    pub fn extract(&mut self, indices: Vec<u32>) -> Result<Vec<u8>, JsError> {
        let idx: Vec<usize> = indices.iter().map(|&i| i as usize).collect();
        self.inner.extract_pages(&idx).map_err(err)
    }

    fn info(&mut self) -> Result<String, JsError> {
        let info = self.inner.info().map_err(err)?;
        serde_json::to_string(&Response::Info(info)).map_err(err)
    }
}
