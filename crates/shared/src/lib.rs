//! Types shared between the UI and the PDF engine.
//!
//! On web these cross a `postMessage` boundary (serialised with JSON); on
//! desktop they cross a channel between the UI thread and a worker thread.
//! Keeping them in their own crate means the web UI's wasm module does not have
//! to link `lopdf` and `hayro` just to describe a page.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Page dimensions as displayed, i.e. after `/Rotate` is applied, in points.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// A snapshot of the document, returned after every mutation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DocInfo {
    pub pages: Vec<PageSize>,
    pub can_undo: bool,
    pub can_redo: bool,
    /// True when the document declares an `/AcroForm` with at least one field.
    pub has_form: bool,
}

impl DocInfo {
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }
}

/// An RGBA8 raster of one page.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Form fields
// ---------------------------------------------------------------------------

/// What kind of control a form field presents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FieldKind {
    /// Single- or multi-line text entry.
    Text { multiline: bool, max_len: Option<usize> },
    /// On/off control. `on_value` is the PDF name used for the "on" state,
    /// which is field-specific (often `/Yes`, but not always).
    Checkbox { on_value: String },
    /// One of a set of mutually exclusive values sharing a parent field.
    Radio { on_value: String },
    /// Dropdown or list. `editable` marks a combo box that accepts free text.
    Choice { options: Vec<String>, editable: bool },
    /// Signature field. Read-only here: we surface it but do not sign.
    Signature,
    /// A push button — no value, included so the UI can skip it.
    Button,
}

/// Where a field's widget sits on a page, in view space: origin at the
/// displayed top-left, y increasing downwards, one unit = one point.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct WidgetRect {
    pub page: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One editable field, flattened from the `/AcroForm` tree into a widget the UI
/// can position and bind to. A field with several widgets (a radio group, or a
/// text field repeated across pages) yields one entry per widget, all sharing
/// `name`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    /// Stable identity for this widget, used to address edits.
    pub id: u32,
    /// Fully qualified field name (`parent.child`), as PDF defines it.
    pub name: String,
    pub kind: FieldKind,
    pub rect: WidgetRect,
    pub value: FieldValue,
    pub read_only: bool,
    pub required: bool,
    /// Tooltip / alternate description (`/TU`), when the document supplies one.
    pub tooltip: Option<String>,
}

/// The current value of a field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FieldValue {
    Text(String),
    /// For checkboxes and radios: whether *this* widget is the selected state.
    Bool(bool),
    /// For choice fields: the selected option(s).
    Selected(Vec<String>),
    Empty,
}

impl FieldValue {
    pub fn as_text(&self) -> &str {
        match self {
            FieldValue::Text(s) => s,
            FieldValue::Selected(v) => v.first().map(String::as_str).unwrap_or(""),
            _ => "",
        }
    }

    pub fn is_on(&self) -> bool {
        matches!(self, FieldValue::Bool(true))
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Straight RGB, each component in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rgb(pub f64, pub f64, pub f64);

impl Rgb {
    pub const BLACK: Rgb = Rgb(0.0, 0.0, 0.0);
}

/// One of the 14 standard PDF fonts, which need no embedding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum StandardFont {
    #[default]
    Helvetica,
    HelveticaBold,
    HelveticaOblique,
    TimesRoman,
    TimesBold,
    TimesItalic,
    Courier,
    CourierBold,
}

impl StandardFont {
    /// The `/BaseFont` name written into the PDF.
    pub fn base_name(self) -> &'static str {
        match self {
            StandardFont::Helvetica => "Helvetica",
            StandardFont::HelveticaBold => "Helvetica-Bold",
            StandardFont::HelveticaOblique => "Helvetica-Oblique",
            StandardFont::TimesRoman => "Times-Roman",
            StandardFont::TimesBold => "Times-Bold",
            StandardFont::TimesItalic => "Times-Italic",
            StandardFont::Courier => "Courier",
            StandardFont::CourierBold => "Courier-Bold",
        }
    }

    pub const ALL: [StandardFont; 8] = [
        StandardFont::Helvetica,
        StandardFont::HelveticaBold,
        StandardFont::HelveticaOblique,
        StandardFont::TimesRoman,
        StandardFont::TimesBold,
        StandardFont::TimesItalic,
        StandardFont::Courier,
        StandardFont::CourierBold,
    ];
}

/// Text to stamp onto a page. Coordinates are view space; `y` is the baseline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextSpec {
    pub page: usize,
    pub x: f64,
    pub y: f64,
    pub text: String,
    pub font: StandardFont,
    pub size: f64,
    pub color: Rgb,
}

/// A rectangle in view space. `fill`/`stroke` are independently optional.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RectSpec {
    pub page: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub fill: Option<Rgb>,
    pub stroke: Option<Rgb>,
    pub stroke_width: f64,
    pub opacity: f64,
    /// Multiply blending, so a highlight darkens the text beneath it.
    pub multiply: bool,
}

/// A freehand polyline in view space.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InkSpec {
    pub page: usize,
    /// Flattened `[x0, y0, x1, y1, ...]`.
    pub points: Vec<f64>,
    pub color: Rgb,
    pub width: f64,
    pub opacity: f64,
}

/// An RGBA8 bitmap placed into a view-space box.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageSpec {
    pub page: usize,
    pub rgba: Vec<u8>,
    pub img_width: u32,
    pub img_height: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

// ---------------------------------------------------------------------------
// UI <-> engine protocol
// ---------------------------------------------------------------------------

/// A request from the UI to the engine.
///
/// Commands carrying large binary payloads (loading a document, placing an
/// image) are deliberately absent: those go through dedicated entry points so
/// the bytes never pass through JSON on the web target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Info,
    Rotate { index: usize, delta: i64 },
    Delete { index: usize },
    Move { from: usize, to: usize },
    Reorder { order: Vec<usize> },
    Duplicate { index: usize },
    InsertBlank { index: usize, width: f64, height: f64 },
    Undo,
    Redo,
    AddText(TextSpec),
    AddRect(RectSpec),
    AddInk(InkSpec),
    FormFields,
    SetField { id: u32, value: FieldValue },
    FlattenForm,
    ResetForm,
}

/// The engine's reply.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Info(DocInfo),
    Fields(Vec<FormField>),
}
