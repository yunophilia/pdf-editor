//! Offline, client-side PDF editor core.
//!
//! * `lopdf` owns the mutable document model (page tree, content streams, resources).
//! * `hayro` rasterises pages for display — it is re-created lazily from the
//!   serialised `lopdf` document whenever the document changes.
//!
//! All coordinates crossing the API boundary are in **view space**: origin at the
//! top-left of the rendered page, y pointing down, one unit = one PDF point at
//! scale 1.0 (i.e. rendered pixels / scale). This is the natural coordinate system
//! for a canvas overlay and already accounts for `/Rotate` and `/CropBox`.

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use pdf_editor_shared::{
    DocInfo, ImageSpec, InkSpec, PageSize, Raster, RectSpec, Rgb, StandardFont, TextSpec,
};
use std::fmt::Write as _;

pub use pdf_editor_shared as shared;

mod form;

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

    /// Map a point through this matrix.
    fn apply(self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
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

/// Everything that can go wrong in the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// The underlying PDF library rejected the document or an operation on it.
    Pdf(String),
    PageIndex,
    Encrypted,
    NoPages,
    Invalid(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Pdf(m) => write!(f, "{m}"),
            Error::PageIndex => write!(f, "page index out of range"),
            Error::Encrypted => write!(f, "password-protected PDFs are not supported"),
            Error::NoPages => write!(f, "the PDF has no pages"),
            Error::Invalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn pdf_err<E: std::fmt::Debug>(e: E) -> Error {
    Error::Pdf(format!("{e:?}"))
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct PdfEditor {
    doc: Document,
    /// Rasteriser view of the document; `None` when stale.
    rendered: Option<Pdf>,
    interp: InterpreterSettings,
    undo_stack: Vec<Document>,
    redo_stack: Vec<Document>,
}

const UNDO_LIMIT: usize = 30;

impl PdfEditor {
    /// Load a PDF from bytes.
    pub fn new(bytes: &[u8]) -> Result<PdfEditor> {
        let mut doc = Document::load_mem(bytes).map_err(pdf_err)?;
        if doc.is_encrypted() {
            doc.decrypt("").map_err(|_| Error::Encrypted)?;
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
    pub fn blank(width: f64, height: f64) -> Result<PdfEditor> {
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

    /// A snapshot of everything the UI needs after a mutation.
    pub fn info(&mut self) -> Result<DocInfo> {
        let count = self.page_count();
        let mut pages = Vec::with_capacity(count);
        for i in 0..count {
            pages.push(self.page_size(i)?);
        }
        Ok(DocInfo {
            pages,
            can_undo: self.can_undo(),
            can_redo: self.can_redo(),
            has_form: self.has_form(),
        })
    }

    /// Size of the page as displayed (after `/Rotate`), in points.
    pub fn page_size(&mut self, index: usize) -> Result<PageSize> {
        self.ensure_rendered()?;
        let pdf = self.rendered.as_ref().unwrap();
        let page = pdf
            .pages()
            .get(index)
            .ok_or(Error::PageIndex)?;
        let (width, height) = page.render_dimensions();
        Ok(PageSize { width, height })
    }

    // --------------------------------------------------------------- render

    /// Rasterise a page at `scale` (1.0 = 72 dpi).
    pub fn render_page(&mut self, index: usize, scale: f32) -> Result<Raster> {
        self.ensure_rendered()?;
        let pdf = self.rendered.as_ref().unwrap();
        let page = pdf
            .pages()
            .get(index)
            .ok_or(Error::PageIndex)?;
        let settings = RenderSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: WHITE,
            ..Default::default()
        };
        let cache = RenderCache::new();
        let pixmap = hayro::render(page, &cache, &self.interp, &settings);
        Ok(Raster {
            width: pixmap.width() as u32,
            height: pixmap.height() as u32,
            data: pixmap.data_as_u8_slice().to_vec(),
        })
    }

    // ------------------------------------------------------------ page ops

    pub fn rotate_page(&mut self, index: usize, delta_degrees: i64) -> Result<()> {
        let id = self.page_id(index)?;
        self.checkpoint();
        let page = self.doc.get_dictionary_mut(id).map_err(pdf_err)?;
        let current = page.get(b"Rotate").and_then(Object::as_i64).unwrap_or(0);
        let next = ((current + delta_degrees) % 360 + 360) % 360;
        page.set("Rotate", next);
        self.invalidate();
        Ok(())
    }

    pub fn delete_page(&mut self, index: usize) -> Result<()> {
        let mut ids = self.page_ids();
        if ids.len() <= 1 {
            return Err(Error::Invalid("cannot delete the only page"));
        }
        if index >= ids.len() {
            return Err(Error::PageIndex);
        }
        self.checkpoint();
        let removed = ids.remove(index);
        self.set_kids(&ids)?;
        self.doc.delete_object(removed);
        self.invalidate();
        Ok(())
    }

    /// Move the page at `from` so that it ends up at position `to`.
    pub fn move_page(&mut self, from: usize, to: usize) -> Result<()> {
        let mut ids = self.page_ids();
        if from >= ids.len() || to >= ids.len() {
            return Err(Error::PageIndex);
        }
        self.checkpoint();
        let id = ids.remove(from);
        ids.insert(to, id);
        self.set_kids(&ids)?;
        self.invalidate();
        Ok(())
    }

    /// Reorder all pages. `order[i]` is the current index of the page that should become page `i`.
    pub fn reorder_pages(&mut self, order: &[usize]) -> Result<()> {
        let ids = self.page_ids();
        if order.len() != ids.len() {
            return Err(Error::Invalid("order must contain every page exactly once"));
        }
        let mut seen = vec![false; ids.len()];
        let mut new_ids = Vec::with_capacity(ids.len());
        for &i in order {
            if i >= ids.len() || seen[i] {
                return Err(Error::Invalid("order must contain every page exactly once"));
            }
            seen[i] = true;
            new_ids.push(ids[i]);
        }
        self.checkpoint();
        self.set_kids(&new_ids)?;
        self.invalidate();
        Ok(())
    }

    pub fn duplicate_page(&mut self, index: usize) -> Result<()> {
        let mut ids = self.page_ids();
        let src = self.page_id(index)?;
        let dict = self.doc.get_dictionary(src).map_err(pdf_err)?.clone();
        self.checkpoint();
        let new_id = self.doc.add_object(dict);
        ids.insert(index + 1, new_id);
        self.set_kids(&ids)?;
        self.invalidate();
        Ok(())
    }

    /// Insert a blank page of `width`×`height` points at `index` (may equal `page_count`).
    pub fn insert_blank_page(&mut self, index: usize, width: f64, height: f64) -> Result<()> {
        let mut ids = self.page_ids();
        if index > ids.len() {
            return Err(Error::PageIndex);
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
    pub fn merge(&mut self, bytes: &[u8], index: Option<usize>) -> Result<()> {
        let mut other = Document::load_mem(bytes).map_err(pdf_err)?;
        if other.is_encrypted() {
            other.decrypt("").map_err(|_| Error::Encrypted)?;
        }
        normalize_page_tree(&mut other)?;
        other.renumber_objects_with(self.doc.max_id + 1);
        let incoming: Vec<ObjectId> = other.page_iter().collect();
        if incoming.is_empty() {
            return Err(Error::NoPages);
        }
        self.checkpoint();

        let root = self.pages_root()?;
        for (id, obj) in other.objects {
            self.doc.objects.insert(id, obj);
        }
        self.doc.max_id = self.doc.objects.keys().map(|k| k.0).max().unwrap_or(self.doc.max_id);
        for &pid in &incoming {
            let page = self.doc.get_dictionary_mut(pid).map_err(pdf_err)?;
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
    pub fn extract_pages(&mut self, indices: &[usize]) -> Result<Vec<u8>> {
        let ids = self.page_ids();
        let mut keep = Vec::with_capacity(indices.len());
        for &i in indices {
            keep.push(*ids.get(i).ok_or(Error::PageIndex)?);
        }
        if keep.is_empty() {
            return Err(Error::Invalid("no pages selected"));
        }
        let mut doc = self.doc.clone();
        for id in ids.iter().filter(|id| !keep.contains(id)) {
            doc.delete_object(*id);
        }
        set_kids_in(&mut doc, &keep)?;
        finish_and_save(doc)
    }

    /// Serialise the current document.
    pub fn save(&mut self) -> Result<Vec<u8>> {
        finish_and_save(self.doc.clone())
    }

    // ---------------------------------------------------------- annotate

    /// Draw text with its baseline-left corner at view-space `(x, y)`.
    pub fn add_text(&mut self, spec: &TextSpec) -> Result<()> {
        let id = self.page_id(spec.page)?;
        self.checkpoint();
        let font_name = self.ensure_font(id, spec.font)?;
        let (cm, height) = self.view_frame(spec.page)?;
        let Rgb(r, g, b) = spec.color;

        let mut ops = String::new();
        writeln!(ops, "q {} BT", cm.pdf_op()).unwrap();
        writeln!(
            ops,
            "/{font_name} {} Tf {} {} {} rg {} TL",
            n(spec.size),
            n(r),
            n(g),
            n(b),
            n(spec.size * 1.2)
        )
        .unwrap();
        writeln!(ops, "{} {} Td", n(spec.x), n(height - spec.y)).unwrap();
        for (i, line) in spec.text.split('\n').enumerate() {
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

    /// Draw a rectangle in view space.
    pub fn add_rect(&mut self, spec: &RectSpec) -> Result<()> {
        if spec.fill.is_none() && !(spec.stroke.is_some() && spec.stroke_width > 0.0) {
            return Ok(());
        }
        let id = self.page_id(spec.page)?;
        self.checkpoint();
        let gs = self.ensure_gstate(id, spec.opacity, spec.multiply)?;
        let (cm, height) = self.view_frame(spec.page)?;
        let stroke = spec.stroke.filter(|_| spec.stroke_width > 0.0);
        let op = match (spec.fill.is_some(), stroke.is_some()) {
            (true, true) => "B",
            (true, false) => "f",
            _ => "S",
        };

        let mut ops = String::new();
        writeln!(ops, "q {} /{gs} gs", cm.pdf_op()).unwrap();
        if let Some(Rgb(r, g, b)) = spec.fill {
            writeln!(ops, "{} {} {} rg", n(r), n(g), n(b)).unwrap();
        }
        if let Some(Rgb(r, g, b)) = stroke {
            writeln!(ops, "{} {} {} RG {} w", n(r), n(g), n(b), n(spec.stroke_width)).unwrap();
        }
        writeln!(
            ops,
            "{} {} {} {} re {op} Q",
            n(spec.x),
            n(height - spec.y - spec.height),
            n(spec.width),
            n(spec.height)
        )
        .unwrap();
        self.append_content(id, ops)?;
        self.invalidate();
        Ok(())
    }

    /// Draw a polyline through `points` (`[x0, y0, x1, y1, ...]`, view space).
    pub fn add_ink(&mut self, spec: &InkSpec) -> Result<()> {
        if spec.points.len() < 4 {
            return Ok(());
        }
        let id = self.page_id(spec.page)?;
        self.checkpoint();
        let gs = self.ensure_gstate(id, spec.opacity, false)?;
        let (cm, height) = self.view_frame(spec.page)?;
        let Rgb(r, g, b) = spec.color;

        let mut ops = String::new();
        writeln!(
            ops,
            "q {} /{gs} gs {} {} {} RG {} w 1 J 1 j",
            cm.pdf_op(),
            n(r),
            n(g),
            n(b),
            n(spec.width)
        )
        .unwrap();
        writeln!(ops, "{} {} m", n(spec.points[0]), n(height - spec.points[1])).unwrap();
        for p in spec.points[2..].chunks_exact(2) {
            writeln!(ops, "{} {} l", n(p[0]), n(height - p[1])).unwrap();
        }
        ops.push_str("S Q\n");
        self.append_content(id, ops)?;
        self.invalidate();
        Ok(())
    }

    /// Place an RGBA8 bitmap into the view-space box `(x, y, width, height)`.
    pub fn add_image(&mut self, spec: &ImageSpec) -> Result<()> {
        let expected = spec.img_width as usize * spec.img_height as usize * 4;
        if spec.rgba.len() != expected {
            return Err(Error::Invalid("rgba buffer size does not match dimensions"));
        }
        let id = self.page_id(spec.page)?;
        self.checkpoint();

        let mut rgb = Vec::with_capacity(expected / 4 * 3);
        let mut alpha = Vec::with_capacity(expected / 4);
        let mut opaque = true;
        for px in spec.rgba.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
            alpha.push(px[3]);
            opaque &= px[3] == 255;
        }

        let mut dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => spec.img_width as i64,
            "Height" => spec.img_height as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
        };
        if !opaque {
            let mut smask = Stream::new(
                dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => spec.img_width as i64,
                    "Height" => spec.img_height as i64,
                    "ColorSpace" => "DeviceGray",
                    "BitsPerComponent" => 8,
                },
                alpha,
            );
            smask.compress().map_err(pdf_err)?;
            let smask_id = self.doc.add_object(smask);
            dict.set("SMask", Object::Reference(smask_id));
        }
        let mut image = Stream::new(dict, rgb);
        image.compress().map_err(pdf_err)?;
        let image_id = self.doc.add_object(image);
        let name = format!("PEImg{}", image_id.0);
        self.doc.add_xobject(id, name.as_str(), image_id).map_err(pdf_err)?;

        let (cm, height) = self.view_frame(spec.page)?;
        // Image space is the unit square with y up, matching the y-up drawing frame.
        let ops = format!(
            "q {} {} 0 0 {} {} {} cm /{name} Do Q\n",
            cm.pdf_op(),
            n(spec.width),
            n(spec.height),
            n(spec.x),
            n(height - spec.y - spec.height)
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

    fn ensure_rendered(&mut self) -> Result<()> {
        if self.rendered.is_none() {
            let mut buf = Vec::new();
            if self.has_form() {
                // Preview-only fixup; see resolve_appearance_states.
                let mut preview = self.doc.clone();
                form::resolve_appearance_states(&mut preview);
                preview.save_to(&mut buf).map_err(pdf_err)?;
            } else {
                self.doc.save_to(&mut buf).map_err(pdf_err)?;
            }
            let pdf = Pdf::new(buf).map_err(|e| Error::Pdf(format!("render: {e:?}")))?;
            self.rendered = Some(pdf);
        }
        Ok(())
    }

    fn page_ids(&self) -> Vec<ObjectId> {
        self.doc.page_iter().collect()
    }

    fn page_id(&self, index: usize) -> Result<ObjectId> {
        self.page_ids()
            .get(index)
            .copied()
            .ok_or_else(|| Error::PageIndex)
    }

    fn pages_root(&self) -> Result<ObjectId> {
        pages_root_of(&self.doc)
    }

    fn set_kids(&mut self, ids: &[ObjectId]) -> Result<()> {
        set_kids_in(&mut self.doc, ids)
    }

    /// Drawing frame for a page: a matrix that maps an upright, y-up copy of view space
    /// into the page's user space, plus the view height `H` needed to convert y-down view
    /// coordinates (`y_up = H - y`). Drawing in this frame keeps text and images upright
    /// regardless of `/Rotate`.
    fn view_frame(&mut self, index: usize) -> Result<(Affine, f64)> {
        self.ensure_rendered()?;
        let pdf = self.rendered.as_ref().unwrap();
        let page = pdf
            .pages()
            .get(index)
            .ok_or(Error::PageIndex)?;
        let (_, height) = page.render_dimensions();
        let height = height as f64;
        // user space → view space (y-down)
        let to_view = Affine(page.initial_transform(true).as_coeffs());
        // y-up view → y-down view
        let flip = Affine([1.0, 0.0, 0.0, -1.0, 0.0, height]);
        Ok((to_view.inverse().mul(flip), height))
    }

    /// Wrap the original content in `q … Q` once, then append `ops` as a new stream.
    fn append_content(&mut self, page_id: ObjectId, ops: String) -> Result<()> {
        let page = self.doc.get_dictionary(page_id).map_err(pdf_err)?;
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
        stream.compress().map_err(pdf_err)?;
        let new_id = self.doc.add_object(stream);
        list.push(Object::Reference(new_id));

        let page = self.doc.get_dictionary_mut(page_id).map_err(pdf_err)?;
        page.set("Contents", list);
        page.set(WRAPPED_KEY, true);
        Ok(())
    }

    /// Register a standard Type1 font on the page; returns the resource name.
    fn ensure_font(&mut self, page_id: ObjectId, font: StandardFont) -> Result<String> {
        let base = font.base_name();
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
                    .map_err(pdf_err)?
                    .set(name.as_bytes(), Object::Reference(font_id));
            }
            _ => return Err(Error::Invalid("page /Resources /Font is not a dictionary")),
        }
        Ok(name)
    }

    /// Register an ExtGState with the given opacity / blend mode; returns the resource name.
    fn ensure_gstate(&mut self, page_id: ObjectId, opacity: f64, multiply: bool) -> Result<String> {
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
            .map_err(pdf_err)?;
        Ok(name)
    }

    fn resources_dict_mut(&mut self, page_id: ObjectId) -> Result<&mut Dictionary> {
        self.doc
            .get_or_create_resources(page_id)
            .and_then(Object::as_dict_mut)
            .map_err(pdf_err)
    }
}

fn pages_root_of(doc: &Document) -> Result<ObjectId> {
    doc.catalog()
        .and_then(|c| c.get(b"Pages"))
        .and_then(Object::as_reference)
        .map_err(|_| Error::Invalid("document has no /Pages root"))
}

fn set_kids_in(doc: &mut Document, ids: &[ObjectId]) -> Result<()> {
    let root = pages_root_of(doc)?;
    let kids: Vec<Object> = ids.iter().map(|id| Object::Reference(*id)).collect();
    let root_dict = doc.get_dictionary_mut(root).map_err(pdf_err)?;
    root_dict.set("Kids", kids);
    root_dict.set("Count", ids.len() as i64);
    Ok(())
}

/// Flatten the page tree to a single `/Pages` node with every page as a direct kid, copying
/// inheritable attributes down onto each page first so nothing is lost.
fn normalize_page_tree(doc: &mut Document) -> Result<()> {
    let root = pages_root_of(doc)?;
    let ids: Vec<ObjectId> = doc.page_iter().collect();
    if ids.is_empty() {
        return Err(Error::NoPages);
    }
    for &pid in &ids {
        for key in INHERITABLE {
            let page = doc.get_dictionary(pid).map_err(pdf_err)?;
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
                doc.get_dictionary_mut(pid).map_err(pdf_err)?.set(key, v);
            }
        }
        doc.get_dictionary_mut(pid)
            .map_err(pdf_err)?
            .set("Parent", Object::Reference(root));
    }
    set_kids_in(doc, &ids)?;
    // The root must not carry inheritable attributes any more: they'd override nothing
    // (pages now have their own) but could confuse consumers after later edits.
    let root_dict = doc.get_dictionary_mut(root).map_err(pdf_err)?;
    for key in INHERITABLE {
        root_dict.remove(key);
    }
    root_dict.remove(b"Parent");
    Ok(())
}

fn finish_and_save(mut doc: Document) -> Result<Vec<u8>> {
    let ids: Vec<ObjectId> = doc.page_iter().collect();
    for id in ids {
        if let Ok(page) = doc.get_dictionary_mut(id) {
            page.remove(WRAPPED_KEY);
        }
    }
    doc.prune_objects();
    doc.renumber_objects();
    let mut buf = Vec::new();
    doc.save_to(&mut buf).map_err(pdf_err)?;
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
    use pdf_editor_shared::{FieldKind, FieldValue};

    const SAMPLE: &[u8] = include_bytes!("../tests/sample.pdf");
    const FORM: &[u8] = include_bytes!("../tests/form.pdf");

    fn text(page: usize, x: f64, y: f64, t: &str, size: f64) -> TextSpec {
        TextSpec {
            page,
            x,
            y,
            text: t.into(),
            font: StandardFont::HelveticaBold,
            size,
            color: Rgb::BLACK,
        }
    }

    fn filled(page: usize, x: f64, y: f64, w: f64, h: f64, color: Rgb) -> RectSpec {
        RectSpec {
            page,
            x,
            y,
            width: w,
            height: h,
            fill: Some(color),
            stroke: None,
            stroke_width: 0.0,
            opacity: 1.0,
            multiply: false,
        }
    }

    fn pixel(ed: &mut PdfEditor, index: usize, x: u32, y: u32) -> [u8; 3] {
        let px = ed.render_page(index, 1.0).unwrap();
        let i = ((y * px.width + x) * 4) as usize;
        [px.data[i], px.data[i + 1], px.data[i + 2]]
    }

    /// Count dark pixels inside a user-space box on an upright page.
    fn ink_in_box(ed: &mut PdfEditor, page: usize, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
        let h = ed.page_size(page).unwrap().height as u32;
        let px = ed.render_page(page, 1.0).unwrap();
        // User-space y measures up from the bottom; view y measures down.
        let (top, bottom) = (h.saturating_sub(y1), h.saturating_sub(y0));
        (top..bottom.min(px.height))
            .flat_map(|y| (x0..x1.min(px.width)).map(move |x| (x, y)))
            .filter(|&(x, y)| px.data[((y * px.width + x) * 4) as usize] < 200)
            .count()
    }

    #[test]
    fn affine_inverse_roundtrip() {
        let t = Affine([0.0, 1.0, -1.0, 0.0, 100.0, 5.0]);
        let id = t.mul(t.inverse());
        for (a, b) in id.0.iter().zip([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]) {
            assert!((a - b).abs() < 1e-9);
        }
    }

    #[test]
    fn affine_apply_matches_multiplication() {
        let t = Affine([2.0, 0.0, 0.0, -1.0, 10.0, 50.0]);
        assert_eq!(t.apply(3.0, 4.0), (16.0, 46.0));
    }

    #[test]
    fn escape() {
        assert_eq!(pdf_escape("a(b)\\c"), "a\\(b\\)\\\\c");
        assert_eq!(pdf_escape("é"), "\\351");
    }

    #[test]
    fn annotations_land_in_view_space() {
        let mut ed = PdfEditor::new(SAMPLE).unwrap();
        assert_eq!(ed.page_count(), 3);
        assert_eq!(
            ed.page_size(1).unwrap(),
            PageSize { width: 842.0, height: 595.0 }
        );

        ed.add_rect(&filled(2, 10.0, 10.0, 50.0, 50.0, Rgb(0.0, 0.0, 1.0)))
            .unwrap();
        assert_eq!(pixel(&mut ed, 2, 35, 35), [0, 0, 255]);
        assert_eq!(pixel(&mut ed, 2, 100, 100), [255, 255, 255]);

        ed.add_ink(&InkSpec {
            page: 2,
            points: vec![200.0, 50.0, 300.0, 50.0],
            color: Rgb(0.0, 1.0, 0.0),
            width: 6.0,
            opacity: 1.0,
        })
        .unwrap();
        assert_eq!(pixel(&mut ed, 2, 250, 50), [0, 255, 0]);

        ed.add_image(&ImageSpec {
            page: 2,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128],
            img_width: 2,
            img_height: 2,
            x: 100.0,
            y: 200.0,
            width: 40.0,
            height: 40.0,
        })
        .unwrap();
        assert_eq!(pixel(&mut ed, 2, 110, 210), [255, 0, 0]);
        assert_eq!(pixel(&mut ed, 2, 130, 210), [0, 255, 0]);
        assert_eq!(pixel(&mut ed, 2, 110, 230), [0, 0, 255]);

        // View-space (0,0) is the displayed top-left even on a rotated page.
        ed.add_rect(&filled(1, 0.0, 0.0, 30.0, 30.0, Rgb::BLACK)).unwrap();
        assert_eq!(pixel(&mut ed, 1, 15, 15), [0, 0, 0]);
        assert_eq!(pixel(&mut ed, 1, 400, 300), [255, 255, 255]);

        // Glyphs sit above the baseline, not mirrored below it.
        ed.add_text(&text(0, 100.0, 100.0, "Hello", 40.0)).unwrap();
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
        assert_eq!(ed.page_size(2).unwrap(), PageSize { width: 300.0, height: 400.0 });
        assert!(ed.undo());
        assert_eq!(ed.page_size(2).unwrap(), PageSize { width: 400.0, height: 300.0 });
        assert!(ed.redo());

        ed.move_page(2, 0).unwrap();
        assert_eq!(ed.page_size(0).unwrap(), PageSize { width: 300.0, height: 400.0 });
        ed.duplicate_page(0).unwrap();
        assert_eq!(ed.page_count(), 4);
        ed.delete_page(1).unwrap();
        assert_eq!(ed.page_count(), 3);
        ed.insert_blank_page(1, 200.0, 100.0).unwrap();
        ed.merge(SAMPLE, None).unwrap();
        assert_eq!(ed.page_count(), 7);
        ed.reorder_pages(&[6, 5, 4, 3, 2, 1, 0]).unwrap();
        assert_eq!(ed.page_size(0).unwrap(), PageSize { width: 400.0, height: 300.0 });

        let extracted = ed.extract_pages(&[0, 6]).unwrap();
        let mut ex = PdfEditor::new(&extracted).unwrap();
        assert_eq!(ex.page_count(), 2);

        let saved = ed.save().unwrap();
        let mut back = PdfEditor::new(&saved).unwrap();
        assert_eq!(back.page_count(), 7);
        assert!(!saved.windows(WRAPPED_KEY.len()).any(|w| w == WRAPPED_KEY));
    }

    #[test]
    fn info_summarises_the_document() {
        let mut ed = PdfEditor::new(SAMPLE).unwrap();
        let info = ed.info().unwrap();
        assert_eq!(info.page_count(), 3);
        assert!(!info.can_undo && !info.can_redo && !info.has_form);

        ed.rotate_page(0, 90).unwrap();
        let info = ed.info().unwrap();
        assert!(info.can_undo && !info.can_redo);
    }

    // ----------------------------------------------------------------- forms

    #[test]
    fn reads_every_field_kind() {
        let mut ed = PdfEditor::new(FORM).unwrap();
        assert!(ed.has_form());
        let fields = ed.form_fields().unwrap();

        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["fullname", "notes", "subscribe", "plan", "plan", "country"]);

        let name = &fields[0];
        assert_eq!(name.value, FieldValue::Text("Ada Lovelace".into()));
        assert_eq!(name.tooltip.as_deref(), Some("Your full name"));
        assert!(matches!(name.kind, FieldKind::Text { multiline: false, .. }));
        assert!(!name.read_only);

        assert!(matches!(
            fields[1].kind,
            FieldKind::Text { multiline: true, max_len: Some(200) }
        ));
        assert!(matches!(fields[2].kind, FieldKind::Checkbox { .. }));
        assert_eq!(fields[2].value, FieldValue::Bool(false));

        // The radio group contributes one entry per widget, sharing a name but
        // with distinct on-values.
        match (&fields[3].kind, &fields[4].kind) {
            (FieldKind::Radio { on_value: a }, FieldKind::Radio { on_value: b }) => {
                assert_eq!((a.as_str(), b.as_str()), ("Basic", "Pro"));
            }
            other => panic!("expected two radios, got {other:?}"),
        }

        match &fields[5].kind {
            FieldKind::Choice { options, .. } => assert_eq!(options, &["Canada", "Japan", "Peru"]),
            other => panic!("expected a choice field, got {other:?}"),
        }
        assert_eq!(fields[5].value, FieldValue::Selected(vec!["Japan".into()]));
    }

    #[test]
    fn widget_rects_are_view_space() {
        let mut ed = PdfEditor::new(FORM).unwrap();
        let fields = ed.form_fields().unwrap();

        // Page 1 is upright: /Rect [72 700 372 724] on a 792pt-tall page sits
        // 68pt down from the top.
        let name = &fields[0];
        assert_eq!(name.rect.page, 0);
        assert!((name.rect.x - 72.0).abs() < 0.5, "x was {}", name.rect.x);
        assert!((name.rect.y - 68.0).abs() < 0.5, "y was {}", name.rect.y);
        assert!((name.rect.width - 300.0).abs() < 0.5);
        assert!((name.rect.height - 24.0).abs() < 0.5);

        // Page 2 is /Rotate 90, so the widget's box swaps axes and must still
        // land inside the displayed 300x400 page.
        let country = &fields[5];
        assert_eq!(country.rect.page, 1);
        assert_eq!(ed.page_size(1).unwrap(), PageSize { width: 300.0, height: 400.0 });
        assert!((country.rect.width - 24.0).abs() < 0.5, "w {}", country.rect.width);
        assert!((country.rect.height - 200.0).abs() < 0.5, "h {}", country.rect.height);
        assert!(country.rect.x >= 0.0 && country.rect.x + country.rect.width <= 300.5);
        assert!(country.rect.y >= 0.0 && country.rect.y + country.rect.height <= 400.5);
    }

    #[test]
    fn editing_a_text_field_shows_up_when_rendered() {
        let mut ed = PdfEditor::new(FORM).unwrap();
        let id = ed.form_fields().unwrap()[0].id;

        // The fixture ships no appearance stream for the text field, so the
        // box starts empty and our generated stream is what shows up.
        let before = ink_in_box(&mut ed, 0, 72, 700, 372, 724);
        ed.set_field_value(id, &FieldValue::Text("Grace Hopper".into()))
            .unwrap();
        let after = ink_in_box(&mut ed, 0, 72, 700, 372, 724);
        assert!(after > before + 100, "expected rendered text: {before} -> {after}");

        assert_eq!(
            ed.form_fields().unwrap()[0].value,
            FieldValue::Text("Grace Hopper".into())
        );

        // The value survives a save/load round-trip.
        let saved = ed.save().unwrap();
        let mut back = PdfEditor::new(&saved).unwrap();
        assert_eq!(
            back.form_fields().unwrap()[0].value,
            FieldValue::Text("Grace Hopper".into())
        );
    }

    #[test]
    fn checkbox_and_radio_toggle_exclusively() {
        let mut ed = PdfEditor::new(FORM).unwrap();
        let fields = ed.form_fields().unwrap();
        let check = fields[2].id;
        let (basic, pro) = (fields[3].id, fields[4].id);

        ed.set_field_value(check, &FieldValue::Bool(true)).unwrap();
        assert_eq!(ed.form_fields().unwrap()[2].value, FieldValue::Bool(true));

        ed.set_field_value(basic, &FieldValue::Bool(true)).unwrap();
        let f = ed.form_fields().unwrap();
        assert_eq!(f[3].value, FieldValue::Bool(true), "Basic should be on");
        assert_eq!(f[4].value, FieldValue::Bool(false), "Pro should be off");

        // Choosing the other option in the group turns the first one off.
        ed.set_field_value(pro, &FieldValue::Bool(true)).unwrap();
        let f = ed.form_fields().unwrap();
        assert_eq!(f[3].value, FieldValue::Bool(false), "Basic should now be off");
        assert_eq!(f[4].value, FieldValue::Bool(true), "Pro should now be on");
    }

    #[test]
    fn editing_a_field_is_undoable() {
        let mut ed = PdfEditor::new(FORM).unwrap();
        let id = ed.form_fields().unwrap()[0].id;
        ed.set_field_value(id, &FieldValue::Text("changed".into())).unwrap();
        assert_eq!(
            ed.form_fields().unwrap()[0].value,
            FieldValue::Text("changed".into())
        );
        assert!(ed.undo());
        assert_eq!(
            ed.form_fields().unwrap()[0].value,
            FieldValue::Text("Ada Lovelace".into())
        );
    }

    #[test]
    fn flattening_removes_the_form_but_keeps_the_marks() {
        let mut ed = PdfEditor::new(FORM).unwrap();
        let fields = ed.form_fields().unwrap();
        ed.set_field_value(fields[2].id, &FieldValue::Bool(true)).unwrap();
        // The checkbox's "on" appearance is a blue square at user-space y=560..576.
        assert_eq!(pixel(&mut ed, 0, 80, 792 - 568), [0, 0, 255]);

        ed.flatten_form().unwrap();
        assert!(!ed.has_form());
        assert!(ed.form_fields().unwrap().is_empty());
        // Still blue: the appearance was stamped into the page content.
        assert_eq!(pixel(&mut ed, 0, 80, 792 - 568), [0, 0, 255]);

        let saved = ed.save().unwrap();
        let mut back = PdfEditor::new(&saved).unwrap();
        assert!(!back.has_form());
        assert_eq!(pixel(&mut back, 0, 80, 792 - 568), [0, 0, 255]);
    }

    #[test]
    fn unknown_field_ids_are_rejected() {
        let mut ed = PdfEditor::new(FORM).unwrap();
        assert_eq!(
            ed.set_field_value(99999, &FieldValue::Text("x".into())),
            Err(Error::Invalid("no such form field"))
        );
    }

    #[test]
    fn documents_without_forms_report_none() {
        let mut ed = PdfEditor::new(SAMPLE).unwrap();
        assert!(!ed.has_form());
        assert!(ed.form_fields().unwrap().is_empty());
        ed.flatten_form().unwrap();
    }

    #[test]
    fn blank_roundtrip() {
        let mut ed = PdfEditor::blank(200.0, 100.0).unwrap();
        assert_eq!(ed.page_count(), 1);
        ed.insert_blank_page(1, 300.0, 300.0).unwrap();
        ed.add_text(&text(0, 10.0, 50.0, "hi", 12.0)).unwrap();
        ed.rotate_page(0, 90).unwrap();
        assert_eq!(ed.page_size(0).unwrap(), PageSize { width: 100.0, height: 200.0 });
        let bytes = ed.save().unwrap();
        let mut back = PdfEditor::new(&bytes).unwrap();
        assert_eq!(back.page_count(), 2);
        let px = back.render_page(0, 1.0).unwrap();
        assert_eq!((px.width, px.height), (100, 200));
    }
}
