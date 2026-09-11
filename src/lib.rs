//! Offline, client-side PDF editor core.
//!
//! * `lopdf` owns the mutable document model (page tree, content streams, resources).
//! * `hayro` rasterises pages for display — it is re-created lazily from the
//!   serialised `lopdf` document whenever the document changes.
//!
//! All coordinates crossing the JS boundary are in **view space**: origin at the
//! top-left of the rendered page, y pointing down, one unit = one PDF point at
//! scale 1.0 (i.e. rendered pixels / scale). This is the natural coordinate system
//! for a canvas overlay and already accounts for `/Rotate` and `/CropBox`.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use std::fmt::Write as _;
use wasm_bindgen::prelude::*;

/// Keys that may be inherited from ancestor `/Pages` nodes (PDF 32000-1 §7.7.3.4).
const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];
/// Marker set on a page dictionary once its original content has been wrapped in `q … Q`.
const WRAPPED_KEY: &[u8] = b"PdfEditorWrapped";

// ---------------------------------------------------------------------------
// Small affine helper (PDF matrix layout: x' = a·x + c·y + e, y' = b·x + d·y + f)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Affine([f64; 6]);

impl Affine {
    fn mul(self, o: Affine) -> Affine {
        // self ∘ o  (apply `o` first, then `self`)
        let [a1, b1, c1, d1, e1, f1] = self.0;
        let [a2, b2, c2, d2, e2, f2] = o.0;
        Affine([
            a1 * a2 + c1 * b2,
            b1 * a2 + d1 * b2,
            a1 * c2 + c1 * d2,
            b1 * c2 + d1 * d2,
            a1 * e2 + c1 * f2 + e1,
            b1 * e2 + d1 * f2 + f1,
        ])
    }

    fn inverse(self) -> Affine {
        let [a, b, c, d, e, f] = self.0;
        let det = a * d - b * c;
        let inv = 1.0 / det;
        Affine([
            d * inv,
            -b * inv,
            -c * inv,
            a * inv,
            (c * f - d * e) * inv,
            (b * e - a * f) * inv,
        ])
    }

    fn pdf_op(&self) -> String {
        let [a, b, c, d, e, f] = self.0;
        format!("{} {} {} {} {} {} cm", n(a), n(b), n(c), n(d), n(e), n(f))
    }
}

/// Format a number for a content stream (trim noise, never scientific notation).
fn n(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e9 {
        format!("{}", v as i64)
    } else {
        let s = format!("{:.4}", v);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn js_err<E: std::fmt::Debug>(e: E) -> JsError {
    JsError::new(&format!("{e:?}"))
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A rasterised page: tightly packed RGBA8, `width * height * 4` bytes.
#[wasm_bindgen]
pub struct RenderedPage {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

#[wasm_bindgen]
impl RenderedPage {
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u32 {
        self.width
    }
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u32 {
        self.height
    }
    /// Moves the pixel buffer out to JS (the struct is consumed).
    pub fn take_data(self) -> Vec<u8> {
        self.data
    }
}

#[wasm_bindgen]
pub struct PdfEditor {
    doc: Document,
    /// Rasteriser view of the document; `None` when stale.
    rendered: Option<Pdf>,
    interp: InterpreterSettings,
    undo_stack: Vec<Document>,
    redo_stack: Vec<Document>,
}

const UNDO_LIMIT: usize = 30;

#[wasm_bindgen]
pub fn init_panic_hook() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
impl PdfEditor {
    /// Load a PDF from bytes.
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: &[u8]) -> Result<PdfEditor, JsError> {
        let mut doc = Document::load_mem(bytes).map_err(js_err)?;
        if doc.is_encrypted() {
            doc.decrypt("").map_err(|_| JsError::new("password-protected PDFs are not supported"))?;
        }
        normalize_page_tree(&mut doc)?;
        Ok(PdfEditor {
            doc,
            rendered: None,
            interp: InterpreterSettings::default(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        })
    }

    /// Create an empty document with one blank page.
    pub fn blank(width: f64, height: f64) -> Result<PdfEditor, JsError> {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let content_id = doc.add_object(Stream::new(Dictionary::new(), Vec::new()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), width.into(), height.into()],
            "Contents" => content_id,
            "Resources" => Dictionary::new(),
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        Ok(PdfEditor {
            doc,
            rendered: None,
            interp: InterpreterSettings::default(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        })
    }

    // ------------------------------------------------------------ undo/redo

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else { return false };
        let current = std::mem::replace(&mut self.doc, prev);
        self.redo_stack.push(current);
        self.invalidate();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else { return false };
        let current = std::mem::replace(&mut self.doc, next);
        self.undo_stack.push(current);
        self.invalidate();
        true
    }

    // ------------------------------------------------------------------ info

    pub fn page_count(&self) -> usize {
        self.page_ids().len()
    }

    /// `[width, height]` of the page as displayed (after `/Rotate`), in points.
    pub fn page_size(&mut self, index: usize) -> Result<Vec<f32>, JsError> {
        self.ensure_rendered()?;
        let pdf = self.rendered.as_ref().unwrap();
        let page = pdf
            .pages()
            .get(index)
            .ok_or_else(|| JsError::new("page index out of range"))?;
        let (w, h) = page.render_dimensions();
        Ok(vec![w, h])
    }

    // --------------------------------------------------------------- render

    /// Rasterise a page at `scale` (1.0 = 72 dpi).
    pub fn render_page(&mut self, index: usize, scale: f32) -> Result<RenderedPage, JsError> {
        self.ensure_rendered()?;
        let pdf = self.rendered.as_ref().unwrap();
        let page = pdf
            .pages()
            .get(index)
            .ok_or_else(|| JsError::new("page index out of range"))?;
        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
            ..Default::default()
        };
        let cache = RenderCache::new();
        let pixmap = hayro::render(page, &cache, &self.interp, &settings);
        Ok(RenderedPage {
            width: pixmap.width() as u32,
            height: pixmap.height() as u32,
            data: pixmap.data_as_u8_slice().to_vec(),
        })
    }

    // ------------------------------------------------------------ page ops

    pub fn rotate_page(&mut self, index: usize, delta_degrees: i32) -> Result<(), JsError> {
        let id = self.page_id(index)?;
        self.checkpoint();
        let page = self.doc.get_dictionary_mut(id).map_err(js_err)?;
        let current = page.get(b"Rotate").and_then(Object::as_i64).unwrap_or(0);
        let next = ((current + delta_degrees as i64) % 360 + 360) % 360;
        page.set("Rotate", next);
        self.invalidate();
        Ok(())
    }

    pub fn delete_page(&mut self, index: usize) -> Result<(), JsError> {
        let mut ids = self.page_ids();
        if ids.len() <= 1 {
            return Err(JsError::new("cannot delete the only page"));
        }
        if index >= ids.len() {
            return Err(JsError::new("page index out of range"));
        }
        self.checkpoint();
        let removed = ids.remove(index);
        self.set_kids(&ids)?;
        self.doc.delete_object(removed);
        self.invalidate();
        Ok(())
    }

    /// Move the page at `from` so that it ends up at position `to`.
    pub fn move_page(&mut self, from: usize, to: usize) -> Result<(), JsError> {
        let mut ids = self.page_ids();
        if from >= ids.len() || to >= ids.len() {
            return Err(JsError::new("page index out of range"));
        }
        self.checkpoint();
        let id = ids.remove(from);
        ids.insert(to, id);
        self.set_kids(&ids)?;
        self.invalidate();
        Ok(())
    }

    /// Reorder all pages. `order[i]` is the current index of the page that should become page `i`.
    pub fn reorder_pages(&mut self, order: &[u32]) -> Result<(), JsError> {
        let ids = self.page_ids();
        if order.len() != ids.len() {
            return Err(JsError::new("order must contain every page exactly once"));
        }
        let mut seen = vec![false; ids.len()];
        let mut new_ids = Vec::with_capacity(ids.len());
        for &i in order {
            let i = i as usize;
            if i >= ids.len() || seen[i] {
                return Err(JsError::new("order must contain every page exactly once"));
            }
            seen[i] = true;
            new_ids.push(ids[i]);
        }
        self.checkpoint();
        self.set_kids(&new_ids)?;
        self.invalidate();
        Ok(())
    }

    pub fn duplicate_page(&mut self, index: usize) -> Result<(), JsError> {
        let mut ids = self.page_ids();
        let src = self.page_id(index)?;
        let dict = self.doc.get_dictionary(src).map_err(js_err)?.clone();
        self.checkpoint();
        let new_id = self.doc.add_object(dict);
        ids.insert(index + 1, new_id);
        self.set_kids(&ids)?;
        self.invalidate();
        Ok(())
    }

    /// Insert a blank page of `width`×`height` points at `index` (may equal `page_count`).
    pub fn insert_blank_page(&mut self, index: usize, width: f64, height: f64) -> Result<(), JsError> {
        let mut ids = self.page_ids();
        if index > ids.len() {
            return Err(JsError::new("page index out of range"));
        }
        let root = self.pages_root()?;
        self.checkpoint();
        let content_id = self.doc.add_object(Stream::new(Dictionary::new(), Vec::new()));
        let page_id = self.doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => root,
            "MediaBox" => vec![0.into(), 0.into(), width.into(), height.into()],
            "Contents" => content_id,
            "Resources" => Dictionary::new(),
        });
        ids.insert(index, page_id);
        self.set_kids(&ids)?;
        self.invalidate();
        Ok(())
    }

    /// Append (or insert at `index`) every page of another PDF.
    pub fn merge(&mut self, bytes: &[u8], index: Option<usize>) -> Result<(), JsError> {
        let mut other = Document::load_mem(bytes).map_err(js_err)?;
        if other.is_encrypted() {
            other.decrypt("").map_err(|_| JsError::new("password-protected PDFs are not supported"))?;
        }
        normalize_page_tree(&mut other)?;
        other.renumber_objects_with(self.doc.max_id + 1);
        let incoming: Vec<ObjectId> = other.page_iter().collect();
        if incoming.is_empty() {
            return Err(JsError::new("the other PDF has no pages"));
        }
        self.checkpoint();

        let root = self.pages_root()?;
        for (id, obj) in other.objects {
            self.doc.objects.insert(id, obj);
        }
        self.doc.max_id = self.doc.objects.keys().map(|k| k.0).max().unwrap_or(self.doc.max_id);
        for &pid in &incoming {
            let page = self.doc.get_dictionary_mut(pid).map_err(js_err)?;
            page.set("Parent", Object::Reference(root));
        }

        let mut ids = self.page_ids();
        let at = index.unwrap_or(ids.len()).min(ids.len());
        ids.splice(at..at, incoming);
        self.set_kids(&ids)?;
        self.invalidate();
        Ok(())
    }

    /// Serialise a new PDF containing only the given pages (in the given order).
    pub fn extract_pages(&mut self, indices: &[u32]) -> Result<Vec<u8>, JsError> {
        let ids = self.page_ids();
        let mut keep = Vec::with_capacity(indices.len());
        for &i in indices {
            keep.push(*ids.get(i as usize).ok_or_else(|| JsError::new("page index out of range"))?);
        }
        if keep.is_empty() {
            return Err(JsError::new("no pages selected"));
        }
        let mut doc = self.doc.clone();
        for id in ids.iter().filter(|id| !keep.contains(id)) {
            doc.delete_object(*id);
        }
        set_kids_in(&mut doc, &keep)?;
        finish_and_save(doc)
    }

    /// Serialise the current document.
    pub fn save(&mut self) -> Result<Vec<u8>, JsError> {
        finish_and_save(self.doc.clone())
    }

    // ---------------------------------------------------------- annotate

    /// Draw text with its baseline-left corner at view-space `(x, y)`.
    /// `font` is one of `Helvetica`, `Helvetica-Bold`, `Helvetica-Oblique`,
    /// `Times-Roman`, `Times-Bold`, `Times-Italic`, `Courier`, `Courier-Bold`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_text(
        &mut self,
        index: usize,
        x: f64,
        y: f64,
        text: &str,
        font: &str,
        size: f64,
        r: f64,
        g: f64,
        b: f64,
    ) -> Result<(), JsError> {
        let id = self.page_id(index)?;
        self.checkpoint();
        let font_name = self.ensure_font(id, font)?;
        let (cm, height) = self.view_frame(index)?;

        let mut ops = String::new();
        writeln!(ops, "q {} BT", cm.pdf_op()).unwrap();
        writeln!(ops, "/{font_name} {} Tf {} {} {} rg {} TL", n(size), n(r), n(g), n(b), n(size * 1.2)).unwrap();
        writeln!(ops, "{} {} Td", n(x), n(height - y)).unwrap();
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                ops.push_str("T* ");
            }
            writeln!(ops, "({}) Tj", pdf_escape(line)).unwrap();
        }
        ops.push_str("ET Q\n");
        self.append_content(id, ops)?;
        self.invalidate();
        Ok(())
    }

    /// Draw a rectangle in view space. Pass a negative `fill_*` component to skip the fill,
    /// a non-positive `stroke_width` to skip the stroke. `multiply` uses the Multiply blend
    /// mode (for highlighter-style marks).
    #[allow(clippy::too_many_arguments)]
    pub fn add_rect(
        &mut self,
        index: usize,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        stroke_r: f64,
        stroke_g: f64,
        stroke_b: f64,
        stroke_width: f64,
        fill_r: f64,
        fill_g: f64,
        fill_b: f64,
        opacity: f64,
        multiply: bool,
    ) -> Result<(), JsError> {
        let id = self.page_id(index)?;
        let gs = self.ensure_gstate(id, opacity, multiply)?;
        let (cm, height) = self.view_frame(index)?;
        let fill = fill_r >= 0.0 && fill_g >= 0.0 && fill_b >= 0.0;
        let stroke = stroke_width > 0.0;
        if !fill && !stroke {
            return Ok(());
        }
        self.checkpoint();
        let op = match (fill, stroke) {
            (true, true) => "B",
            (true, false) => "f",
            _ => "S",
        };
        let mut ops = String::new();
        writeln!(ops, "q {} /{gs} gs", cm.pdf_op()).unwrap();
        if fill {
            writeln!(ops, "{} {} {} rg", n(fill_r), n(fill_g), n(fill_b)).unwrap();
        }
        if stroke {
            writeln!(ops, "{} {} {} RG {} w", n(stroke_r), n(stroke_g), n(stroke_b), n(stroke_width)).unwrap();
        }
        writeln!(ops, "{} {} {} {} re {op} Q", n(x), n(height - y - h), n(w), n(h)).unwrap();
        self.append_content(id, ops)?;
        self.invalidate();
        Ok(())
    }

    /// Draw a polyline through `points` (`[x0, y0, x1, y1, …]`, view space).
    #[allow(clippy::too_many_arguments)]
    pub fn add_ink(
        &mut self,
        index: usize,
        points: &[f64],
        r: f64,
        g: f64,
        b: f64,
        width: f64,
        opacity: f64,
    ) -> Result<(), JsError> {
        if points.len() < 4 {
            return Ok(());
        }
        let id = self.page_id(index)?;
        self.checkpoint();
        let gs = self.ensure_gstate(id, opacity, false)?;
        let (cm, height) = self.view_frame(index)?;
        let mut ops = String::new();
        writeln!(ops, "q {} /{gs} gs {} {} {} RG {} w 1 J 1 j", cm.pdf_op(), n(r), n(g), n(b), n(width)).unwrap();
        writeln!(ops, "{} {} m", n(points[0]), n(height - points[1])).unwrap();
        for p in points[2..].chunks_exact(2) {
            writeln!(ops, "{} {} l", n(p[0]), n(height - p[1])).unwrap();
        }
        ops.push_str("S Q\n");
        self.append_content(id, ops)?;
        self.invalidate();
        Ok(())
    }

    /// Place an RGBA8 bitmap (`img_w`×`img_h`) into the view-space box `(x, y, w, h)`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_image(
        &mut self,
        index: usize,
        rgba: &[u8],
        img_w: u32,
        img_h: u32,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> Result<(), JsError> {
        let expected = img_w as usize * img_h as usize * 4;
        if rgba.len() != expected {
            return Err(JsError::new("rgba buffer size does not match dimensions"));
        }
        let id = self.page_id(index)?;
        self.checkpoint();

        let mut rgb = Vec::with_capacity(img_w as usize * img_h as usize * 3);
        let mut alpha = Vec::with_capacity(img_w as usize * img_h as usize);
        let mut opaque = true;
        for px in rgba.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
            alpha.push(px[3]);
            opaque &= px[3] == 255;
        }

        let mut dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => img_w as i64,
            "Height" => img_h as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
        };
        if !opaque {
            let mut smask = Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => img_w as i64,
                    "Height" => img_h as i64,
                    "ColorSpace" => "DeviceGray",
                    "BitsPerComponent" => 8,
                },
                alpha,
            );
            smask.compress().map_err(js_err)?;
            let smask_id = self.doc.add_object(smask);
            dict.set("SMask", Object::Reference(smask_id));
        }
        let mut image = Stream::new(dict, rgb);
        image.compress().map_err(js_err)?;
        let image_id = self.doc.add_object(image);
        let name = format!("PEImg{}", image_id.0);
        self.doc.add_xobject(id, name.as_str(), image_id).map_err(js_err)?;

        let (cm, height) = self.view_frame(index)?;
        // Image space is the unit square with y up, matching the y-up drawing frame.
        let ops = format!(
            "q {} {} 0 0 {} {} {} cm /{name} Do Q\n",
            cm.pdf_op(),
            n(w),
            n(h),
            n(x),
            n(height - y - h)
        );
        self.append_content(id, ops)?;
        self.invalidate();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

impl PdfEditor {
    fn invalidate(&mut self) {
        self.rendered = None;
    }

    /// Snapshot the document before a mutation so it can be undone.
    fn checkpoint(&mut self) {
        self.undo_stack.push(self.doc.clone());
        if self.undo_stack.len() > UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    fn ensure_rendered(&mut self) -> Result<(), JsError> {
        if self.rendered.is_none() {
            let mut buf = Vec::new();
            self.doc.save_to(&mut buf).map_err(js_err)?;
            let pdf = Pdf::new(buf).map_err(|e| JsError::new(&format!("render: {e:?}")))?;
            self.rendered = Some(pdf);
        }
        Ok(())
    }

    fn page_ids(&self) -> Vec<ObjectId> {
        self.doc.page_iter().collect()
    }

    fn page_id(&self, index: usize) -> Result<ObjectId, JsError> {
        self.page_ids()
            .get(index)
            .copied()
            .ok_or_else(|| JsError::new("page index out of range"))
    }

    fn pages_root(&self) -> Result<ObjectId, JsError> {
        pages_root_of(&self.doc)
    }

    fn set_kids(&mut self, ids: &[ObjectId]) -> Result<(), JsError> {
        set_kids_in(&mut self.doc, ids)
    }

    /// Drawing frame for a page: a matrix that maps an upright, y-up copy of view space
    /// into the page's user space, plus the view height `H` needed to convert y-down view
    /// coordinates (`y_up = H - y`). Drawing in this frame keeps text and images upright
    /// regardless of `/Rotate`.
    fn view_frame(&mut self, index: usize) -> Result<(Affine, f64), JsError> {
        self.ensure_rendered()?;
        let pdf = self.rendered.as_ref().unwrap();
        let page = pdf
            .pages()
            .get(index)
            .ok_or_else(|| JsError::new("page index out of range"))?;
        let (_, height) = page.render_dimensions();
        let height = height as f64;
        // user space → view space (y-down)
        let to_view = Affine(page.initial_transform(true).as_coeffs());
        // y-up view → y-down view
        let flip = Affine([1.0, 0.0, 0.0, -1.0, 0.0, height]);
        Ok((to_view.inverse().mul(flip), height))
    }

    /// Wrap the original content in `q … Q` once, then append `ops` as a new stream.
    fn append_content(&mut self, page_id: ObjectId, ops: String) -> Result<(), JsError> {
        let page = self.doc.get_dictionary(page_id).map_err(js_err)?;
        let wrapped = page.has(WRAPPED_KEY);
        let mut list: Vec<Object> = match page.get(b"Contents") {
            Ok(Object::Reference(id)) => vec![Object::Reference(*id)],
            Ok(Object::Array(arr)) => arr.clone(),
            _ => vec![],
        };
        if !wrapped {
            let q = self.doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
            let qq = self.doc.add_object(Stream::new(Dictionary::new(), b"\nQ\n".to_vec()));
            list.insert(0, Object::Reference(q));
            list.push(Object::Reference(qq));
        }
        let mut stream = Stream::new(Dictionary::new(), ops.into_bytes());
        stream.compress().map_err(js_err)?;
        let new_id = self.doc.add_object(stream);
        list.push(Object::Reference(new_id));

        let page = self.doc.get_dictionary_mut(page_id).map_err(js_err)?;
        page.set("Contents", list);
        page.set(WRAPPED_KEY, true);
        Ok(())
    }

    /// Register a standard Type1 font on the page; returns the resource name.
    fn ensure_font(&mut self, page_id: ObjectId, base: &str) -> Result<String, JsError> {
        const ALLOWED: [&str; 12] = [
            "Helvetica",
            "Helvetica-Bold",
            "Helvetica-Oblique",
            "Helvetica-BoldOblique",
            "Times-Roman",
            "Times-Bold",
            "Times-Italic",
            "Times-BoldItalic",
            "Courier",
            "Courier-Bold",
            "Courier-Oblique",
            "Courier-BoldOblique",
        ];
        let base = if ALLOWED.contains(&base) { base } else { "Helvetica" };
        let name = format!("PEF{}", base.replace('-', ""));

        let resources = self.resources_dict_mut(page_id)?;
        if !resources.has(b"Font") {
            resources.set("Font", Dictionary::new());
        }
        let already = match resources.get(b"Font") {
            Ok(Object::Dictionary(d)) => d.has(name.as_bytes()),
            _ => false,
        };
        if already {
            return Ok(name);
        }
        let font_id = self.doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => base,
            "Encoding" => "WinAnsiEncoding",
        });
        let resources = self.resources_dict_mut(page_id)?;
        match resources.get_mut(b"Font") {
            Ok(Object::Dictionary(d)) => d.set(name.as_bytes(), Object::Reference(font_id)),
            Ok(Object::Reference(rid)) => {
                let rid = *rid;
                self.doc
                    .get_dictionary_mut(rid)
                    .map_err(js_err)?
                    .set(name.as_bytes(), Object::Reference(font_id));
            }
            _ => return Err(JsError::new("page /Resources /Font is not a dictionary")),
        }
        Ok(name)
    }

    /// Register an ExtGState with the given opacity / blend mode; returns the resource name.
    fn ensure_gstate(&mut self, page_id: ObjectId, opacity: f64, multiply: bool) -> Result<String, JsError> {
        let opacity = opacity.clamp(0.0, 1.0);
        let pct = (opacity * 100.0).round() as u32;
        let name = format!("PEGS{pct}{}", if multiply { "M" } else { "" });
        let gs_id = self.doc.add_object(dictionary! {
            "Type" => "ExtGState",
            "CA" => opacity,
            "ca" => opacity,
            "BM" => if multiply { "Multiply" } else { "Normal" },
        });
        self.doc
            .add_graphics_state(page_id, name.as_str(), gs_id)
            .map_err(js_err)?;
        Ok(name)
    }

    fn resources_dict_mut(&mut self, page_id: ObjectId) -> Result<&mut Dictionary, JsError> {
        self.doc
            .get_or_create_resources(page_id)
            .and_then(Object::as_dict_mut)
            .map_err(js_err)
    }
}

fn pages_root_of(doc: &Document) -> Result<ObjectId, JsError> {
    doc.catalog()
        .and_then(|c| c.get(b"Pages"))
        .and_then(Object::as_reference)
        .map_err(|_| JsError::new("document has no /Pages root"))
}

fn set_kids_in(doc: &mut Document, ids: &[ObjectId]) -> Result<(), JsError> {
    let root = pages_root_of(doc)?;
    let kids: Vec<Object> = ids.iter().map(|id| Object::Reference(*id)).collect();
    let root_dict = doc.get_dictionary_mut(root).map_err(js_err)?;
    root_dict.set("Kids", kids);
    root_dict.set("Count", ids.len() as i64);
    Ok(())
}

/// Flatten the page tree to a single `/Pages` node with every page as a direct kid, copying
/// inheritable attributes down onto each page first so nothing is lost.
fn normalize_page_tree(doc: &mut Document) -> Result<(), JsError> {
    let root = pages_root_of(doc)?;
    let ids: Vec<ObjectId> = doc.page_iter().collect();
    if ids.is_empty() {
        return Err(JsError::new("PDF has no pages"));
    }
    for &pid in &ids {
        for key in INHERITABLE {
            let page = doc.get_dictionary(pid).map_err(js_err)?;
            if page.has(key) {
                continue;
            }
            let mut parent = page.get(b"Parent").and_then(Object::as_reference).ok();
            let mut found = None;
            let mut hops = 0;
            while let Some(p) = parent {
                let Ok(d) = doc.get_dictionary(p) else { break };
                if let Ok(v) = d.get(key) {
                    found = Some(v.clone());
                    break;
                }
                parent = d.get(b"Parent").and_then(Object::as_reference).ok();
                hops += 1;
                if hops > 64 {
                    break;
                }
            }
            if let Some(v) = found {
                doc.get_dictionary_mut(pid).map_err(js_err)?.set(key, v);
            }
        }
        doc.get_dictionary_mut(pid)
            .map_err(js_err)?
            .set("Parent", Object::Reference(root));
    }
    set_kids_in(doc, &ids)?;
    // The root must not carry inheritable attributes any more: they'd override nothing
    // (pages now have their own) but could confuse consumers after later edits.
    let root_dict = doc.get_dictionary_mut(root).map_err(js_err)?;
    for key in INHERITABLE {
        root_dict.remove(key);
    }
    root_dict.remove(b"Parent");
    Ok(())
}

fn finish_and_save(mut doc: Document) -> Result<Vec<u8>, JsError> {
    let ids: Vec<ObjectId> = doc.page_iter().collect();
    for id in ids {
        if let Ok(page) = doc.get_dictionary_mut(id) {
            page.remove(WRAPPED_KEY);
        }
    }
    doc.prune_objects();
    doc.renumber_objects();
    let mut buf = Vec::new();
    doc.save_to(&mut buf).map_err(js_err)?;
    Ok(buf)
}

/// Encode text for a `( … )` string literal in WinAnsiEncoding.
fn pdf_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x80 => out.push(c),
            c if (c as u32) < 0x100 => {
                // Latin-1 range: emit as an octal escape so the byte value survives.
                write!(out, "\\{:03o}", c as u32).unwrap();
            }
            _ => out.push('?'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affine_inverse_roundtrip() {
        let t = Affine([0.0, 1.0, -1.0, 0.0, 100.0, 5.0]);
        let id = t.mul(t.inverse());
        for (a, b) in id.0.iter().zip([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]) {
            assert!((a - b).abs() < 1e-9);
        }
    }

    #[test]
    fn escape() {
        assert_eq!(pdf_escape("a(b)\\c"), "a\\(b\\)\\\\c");
        assert_eq!(pdf_escape("é"), "\\351");
    }

    const SAMPLE: &[u8] = include_bytes!("../tests/sample.pdf");

    fn pixel(ed: &mut PdfEditor, index: usize, x: u32, y: u32) -> [u8; 3] {
        let px = ed.render_page(index, 1.0).unwrap();
        let i = ((y * px.width + x) * 4) as usize;
        [px.data[i], px.data[i + 1], px.data[i + 2]]
    }

    #[test]
    fn annotations_land_in_view_space() {
        let mut ed = PdfEditor::new(SAMPLE).unwrap();
        assert_eq!(ed.page_count(), 3);
        // Page 2 is A4 with /Rotate 90 -> displayed landscape.
        assert_eq!(ed.page_size(1).unwrap(), vec![842.0, 595.0]);

        // Filled rect on the small page.
        ed.add_rect(2, 10.0, 10.0, 50.0, 50.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, false).unwrap();
        assert_eq!(pixel(&mut ed, 2, 35, 35), [0, 0, 255]);
        assert_eq!(pixel(&mut ed, 2, 100, 100), [255, 255, 255]);

        // Ink stroke.
        ed.add_ink(2, &[200.0, 50.0, 300.0, 50.0], 0.0, 1.0, 0.0, 6.0, 1.0).unwrap();
        assert_eq!(pixel(&mut ed, 2, 250, 50), [0, 255, 0]);

        // 2x2 image keeps its orientation (top-left red, top-right green, bottom-left blue).
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128];
        ed.add_image(2, &rgba, 2, 2, 100.0, 200.0, 40.0, 40.0).unwrap();
        assert_eq!(pixel(&mut ed, 2, 110, 210), [255, 0, 0]);
        assert_eq!(pixel(&mut ed, 2, 130, 210), [0, 255, 0]);
        assert_eq!(pixel(&mut ed, 2, 110, 230), [0, 0, 255]);

        // On the rotated page, view-space (0,0) is still the displayed top-left corner.
        ed.add_rect(1, 0.0, 0.0, 30.0, 30.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, false).unwrap();
        assert_eq!(pixel(&mut ed, 1, 15, 15), [0, 0, 0]);
        assert_eq!(pixel(&mut ed, 1, 400, 300), [255, 255, 255]);

        // Text: baseline at y=100, 40pt. Glyphs must sit *above* the baseline (upright),
        // not below it (which is what a mirrored frame produces).
        ed.add_text(0, 100.0, 100.0, "Hello", "Helvetica-Bold", 40.0, 0.0, 0.0, 0.0).unwrap();
        let px = ed.render_page(0, 1.0).unwrap();
        let dark = |y0: u32, y1: u32| {
            (y0..y1)
                .flat_map(|y| (100..220u32).map(move |x| (x, y)))
                .filter(|&(x, y)| px.data[((y * px.width + x) * 4) as usize] < 128)
                .count()
        };
        let above = dark(65, 100);
        let below = dark(101, 136);
        assert!(above > 200, "expected glyph pixels above the baseline, got {above}");
        assert!(below < above / 4, "glyphs appear mirrored: above={above} below={below}");
    }

    #[test]
    fn page_operations_and_roundtrip() {
        let mut ed = PdfEditor::new(SAMPLE).unwrap();
        ed.rotate_page(2, 90).unwrap();
        assert_eq!(ed.page_size(2).unwrap(), vec![300.0, 400.0]);
        assert!(ed.undo());
        assert_eq!(ed.page_size(2).unwrap(), vec![400.0, 300.0]);
        assert!(ed.redo());
        assert_eq!(ed.page_size(2).unwrap(), vec![300.0, 400.0]);

        ed.move_page(2, 0).unwrap();
        assert_eq!(ed.page_size(0).unwrap(), vec![300.0, 400.0]);
        ed.duplicate_page(0).unwrap();
        assert_eq!(ed.page_count(), 4);
        ed.delete_page(1).unwrap();
        assert_eq!(ed.page_count(), 3);
        ed.insert_blank_page(1, 200.0, 100.0).unwrap();
        assert_eq!(ed.page_size(1).unwrap(), vec![200.0, 100.0]);
        ed.merge(SAMPLE, None).unwrap();
        assert_eq!(ed.page_count(), 7);
        ed.reorder_pages(&[6, 5, 4, 3, 2, 1, 0]).unwrap();
        assert_eq!(ed.page_size(0).unwrap(), vec![400.0, 300.0]);

        let extracted = ed.extract_pages(&[0, 6]).unwrap();
        let mut ex = PdfEditor::new(&extracted).unwrap();
        assert_eq!(ex.page_count(), 2);
        assert_eq!(ex.page_size(1).unwrap(), vec![300.0, 400.0]);

        let saved = ed.save().unwrap();
        let mut back = PdfEditor::new(&saved).unwrap();
        assert_eq!(back.page_count(), 7);
        assert_eq!(back.page_size(6).unwrap(), vec![300.0, 400.0]);
        // Save must not leak the wrap marker.
        assert!(!saved.windows(WRAPPED_KEY.len()).any(|w| w == WRAPPED_KEY));
    }

    #[test]
    fn blank_roundtrip() {
        let mut ed = PdfEditor::blank(200.0, 100.0).unwrap();
        assert_eq!(ed.page_count(), 1);
        ed.insert_blank_page(1, 300.0, 300.0).unwrap();
        ed.add_text(0, 10.0, 50.0, "hi", "Helvetica", 12.0, 0.0, 0.0, 0.0).unwrap();
        ed.rotate_page(0, 90).unwrap();
        assert_eq!(ed.page_size(0).unwrap(), vec![100.0, 200.0]);
        let bytes = ed.save().unwrap();
        let mut back = PdfEditor::new(&bytes).unwrap();
        assert_eq!(back.page_count(), 2);
        let px = back.render_page(0, 1.0).unwrap();
        assert_eq!((px.width, px.height), (100, 200));
    }
}
