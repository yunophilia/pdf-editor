//! AcroForm support: reading interactive form fields, editing their values,
//! regenerating their appearance streams, and flattening them into page content.
//!
//! The field tree in a PDF is a forest of dictionaries where a *terminal* field
//! (one with a field type `/FT`) owns one or more *widget* annotations that give
//! it a position on a page. Several attributes — `/FT`, `/Ff`, `/V`, `/DA` — are
//! inherited from ancestors, and a field with a single widget usually has the
//! widget merged into the field dictionary itself. This module flattens all of
//! that into one [`FormField`] per widget.

use crate::{pdf_err, Affine, Error, PdfEditor, Result};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream, StringFormat};
use pdf_editor_shared::{FieldKind, FieldValue, FormField, WidgetRect};
use std::collections::HashMap;
use std::fmt::Write as _;

/// Field flags (`/Ff`), numbered from bit 1 as the spec writes them.
mod flags {
    pub const READ_ONLY: i64 = 1 << 0;
    pub const REQUIRED: i64 = 1 << 1;
    pub const MULTILINE: i64 = 1 << 12;
    pub const RADIO: i64 = 1 << 15;
    pub const PUSHBUTTON: i64 = 1 << 16;
    pub const COMBO: i64 = 1 << 17;
    pub const EDIT: i64 = 1 << 18;
}

/// How deep the field tree may nest before we assume a cycle.
const MAX_DEPTH: usize = 32;

// ---------------------------------------------------------------------------
// Text encoding
// ---------------------------------------------------------------------------

/// Decode a PDF text string: UTF-16BE when it carries a BOM, else PDFDocEncoding
/// (which agrees with Latin-1 across the range we care about).
fn decode_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        bytes.iter().map(|&b| b as char).collect()
    }
}

/// Encode a PDF text string, staying in Latin-1 when possible so the result
/// reads cleanly in a text editor, and widening to UTF-16BE when it must.
fn encode_text(s: &str) -> Object {
    if s.chars().all(|c| (c as u32) < 0x100) {
        Object::String(s.chars().map(|c| c as u8).collect(), StringFormat::Literal)
    } else {
        let mut out = vec![0xFE, 0xFF];
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_be_bytes());
        }
        Object::String(out, StringFormat::Literal)
    }
}

/// Escape a string for a `( … )` literal inside a content stream.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x100 => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// A terminal field, as gathered from the tree
// ---------------------------------------------------------------------------

struct Terminal {
    /// Object id of the field dictionary itself.
    field_id: ObjectId,
    /// Widget annotations belonging to it. When the widget is merged into the
    /// field dictionary this is just `field_id` again.
    widgets: Vec<ObjectId>,
    name: String,
    ft: Vec<u8>,
    ff: i64,
    value: Option<Object>,
    opts: Vec<String>,
    max_len: Option<usize>,
    tooltip: Option<String>,
}

impl PdfEditor {
    // ----------------------------------------------------------------- read

    /// Whether the document declares an `/AcroForm` holding at least one field.
    pub fn has_form(&self) -> bool {
        self.acroform()
            .and_then(|d| d.get(b"Fields").ok().cloned())
            .and_then(|f| self.resolve(&f).and_then(|o| o.as_array().ok().cloned()))
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    }

    /// Every editable widget in the document, in field-tree order.
    pub fn form_fields(&mut self) -> Result<Vec<FormField>> {
        if !self.has_form() {
            return Ok(Vec::new());
        }

        // Page transforms first: this borrows the rasteriser, so finish with it
        // before touching the document.
        let transforms = self.page_transforms()?;
        let widget_pages = self.widget_page_map();

        let terminals = self.collect_terminals()?;
        let mut out = Vec::new();

        for t in &terminals {
            if matches!(self.field_kind(t, None), FieldKind::Button) {
                continue;
            }
            for &w in &t.widgets {
                let kind = self.field_kind(t, Some(w));
                let Some(&page) = widget_pages.get(&w) else {
                    // A widget no page lists is not reachable by the user.
                    continue;
                };
                let Some(rect) = self.widget_rect(w, page, &transforms) else {
                    continue;
                };
                let value = self.widget_value(t, w, &kind);
                out.push(FormField {
                    id: w.0,
                    name: t.name.clone(),
                    kind: kind.clone(),
                    rect,
                    value,
                    read_only: t.ff & flags::READ_ONLY != 0,
                    required: t.ff & flags::REQUIRED != 0,
                    tooltip: t.tooltip.clone(),
                });
            }
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- write

    /// Set a field's value by widget id, regenerating its appearance so the
    /// change is visible both here and in other viewers.
    pub fn set_field_value(&mut self, widget_id: u32, value: &FieldValue) -> Result<()> {
        let terminals = self.collect_terminals()?;
        let t = terminals
            .iter()
            .find(|t| t.widgets.iter().any(|w| w.0 == widget_id))
            .ok_or(Error::Invalid("no such form field"))?;
        if t.ff & flags::READ_ONLY != 0 {
            return Err(Error::Invalid("field is read-only"));
        }

        let widget_of = t.widgets.iter().copied().find(|w| w.0 == widget_id);
        let kind = self.field_kind(t, widget_of);
        let field_id = t.field_id;
        let widgets = t.widgets.clone();
        let widget = widgets
            .iter()
            .copied()
            .find(|w| w.0 == widget_id)
            .ok_or(Error::Invalid("no such form field"))?;

        self.checkpoint();

        match &kind {
            FieldKind::Checkbox { on_value } | FieldKind::Radio { on_value } => {
                let on = value.is_on();
                let state: Vec<u8> = if on { on_value.as_bytes().to_vec() } else { b"Off".to_vec() };
                self.doc
                    .get_dictionary_mut(field_id)
                    .map_err(pdf_err)?
                    .set("V", Object::Name(state.clone()));
                // Every widget of a radio group shows Off except the chosen one.
                for w in widgets {
                    let want = if w == widget && on { state.clone() } else { b"Off".to_vec() };
                    if let Ok(d) = self.doc.get_dictionary_mut(w) {
                        d.set("AS", Object::Name(want));
                    }
                }
            }
            FieldKind::Text { .. } | FieldKind::Choice { .. } => {
                let text = value.as_text().to_string();
                self.doc
                    .get_dictionary_mut(field_id)
                    .map_err(pdf_err)?
                    .set("V", encode_text(&text));
                for w in widgets {
                    self.regenerate_appearance(w, &text)?;
                }
            }
            FieldKind::Signature | FieldKind::Button => {
                return Err(Error::Invalid("field type cannot be edited"));
            }
        }

        self.invalidate();
        Ok(())
    }

    /// Bake every field's current appearance into the page content and remove
    /// the form, leaving a flat document that cannot be edited further.
    pub fn flatten_form(&mut self) -> Result<()> {
        if !self.has_form() {
            return Ok(());
        }
        self.checkpoint();

        let widget_pages = self.widget_page_map();
        let page_ids = self.page_ids();

        for (&widget, &page_idx) in &widget_pages {
            let Some(&page_id) = page_ids.get(page_idx) else { continue };
            let Ok(dict) = self.doc.get_dictionary(widget) else { continue };
            let rect = match read_rect(dict) {
                Some(r) => r,
                None => continue,
            };
            // The appearance to stamp is the normal one for the current state.
            let Some(stream_id) = self.appearance_stream_id(widget) else { continue };
            let name = format!("PEFlat{}", stream_id.0);
            self.doc
                .add_xobject(page_id, name.as_str(), stream_id)
                .map_err(pdf_err)?;
            // The XObject's own /BBox and /Matrix place its content; we only
            // need to move it to the widget's position on the page.
            let ops = format!("q 1 0 0 1 {} {} cm /{name} Do Q\n", fmt(rect.0), fmt(rect.1));
            self.append_content(page_id, ops)?;
        }

        // Drop the annotations and the form dictionary itself.
        for (&widget, _) in &widget_pages {
            for &page_id in &page_ids {
                if let Ok(dict) = self.doc.get_dictionary_mut(page_id) {
                    if let Ok(Object::Array(annots)) = dict.get_mut(b"Annots") {
                        annots.retain(|a| a.as_reference().map(|r| r != widget).unwrap_or(true));
                    }
                }
            }
        }
        if let Ok(catalog) = self.doc.catalog_mut() {
            catalog.remove(b"AcroForm");
        }

        self.invalidate();
        Ok(())
    }

    // -------------------------------------------------------------- helpers

    fn acroform(&self) -> Option<&Dictionary> {
        let obj = self.doc.catalog().ok()?.get(b"AcroForm").ok()?;
        self.resolve(obj)?.as_dict().ok()
    }

    fn acroform_id(&self) -> Option<ObjectId> {
        self.doc.catalog().ok()?.get(b"AcroForm").ok()?.as_reference().ok()
    }

    /// Follow references (bounded, so a malformed cycle cannot hang us).
    fn resolve<'a>(&'a self, obj: &'a Object) -> Option<&'a Object> {
        let mut cur = obj;
        for _ in 0..MAX_DEPTH {
            match cur {
                Object::Reference(id) => cur = self.doc.get_object(*id).ok()?,
                other => return Some(other),
            }
        }
        None
    }

    fn get<'a>(&'a self, dict: &'a Dictionary, key: &[u8]) -> Option<&'a Object> {
        self.resolve(dict.get(key).ok()?)
    }

    /// Matrix mapping each page's user space to its view space (y down).
    fn page_transforms(&mut self) -> Result<Vec<Affine>> {
        self.ensure_rendered()?;
        let pdf = self.rendered.as_ref().unwrap();
        Ok(pdf
            .pages()
            .iter()
            .map(|p| Affine(p.initial_transform(true).as_coeffs()))
            .collect())
    }

    /// Which page each widget annotation appears on.
    fn widget_page_map(&self) -> HashMap<ObjectId, usize> {
        let mut map = HashMap::new();
        for (i, page_id) in self.page_ids().iter().enumerate() {
            let Ok(dict) = self.doc.get_dictionary(*page_id) else { continue };
            let Some(annots) = self.get(dict, b"Annots").and_then(|a| a.as_array().ok()) else {
                continue;
            };
            for a in annots {
                if let Ok(id) = a.as_reference() {
                    map.insert(id, i);
                }
            }
        }
        map
    }

    /// Walk the field forest, flattening it to terminal fields.
    fn collect_terminals(&self) -> Result<Vec<Terminal>> {
        let Some(form) = self.acroform() else { return Ok(Vec::new()) };
        let roots: Vec<ObjectId> = self
            .get(form, b"Fields")
            .and_then(|f| f.as_array().ok())
            .map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
            .unwrap_or_default();

        let mut out = Vec::new();
        let inherited = Inherited::default();
        for root in roots {
            self.walk_field(root, "", &inherited, 0, &mut out);
        }
        Ok(out)
    }

    fn walk_field(
        &self,
        id: ObjectId,
        prefix: &str,
        parent: &Inherited,
        depth: usize,
        out: &mut Vec<Terminal>,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let Ok(dict) = self.doc.get_dictionary(id) else { return };

        let partial = self
            .get(dict, b"T")
            .and_then(|o| o.as_str().ok())
            .map(decode_text)
            .unwrap_or_default();
        let name = match (prefix.is_empty(), partial.is_empty()) {
            (_, true) => prefix.to_string(),
            (true, false) => partial.clone(),
            (false, false) => format!("{prefix}.{partial}"),
        };

        let here = Inherited {
            ft: self
                .get(dict, b"FT")
                .and_then(|o| o.as_name().ok())
                .map(<[u8]>::to_vec)
                .or_else(|| parent.ft.clone()),
            ff: self.get(dict, b"Ff").and_then(|o| o.as_i64().ok()).or(parent.ff),
            value: self.get(dict, b"V").cloned().or_else(|| parent.value.clone()),
        };

        // Kids that are themselves fields (they carry /T) mean this is an
        // interior node; kids without /T are this field's widgets.
        let kids: Vec<ObjectId> = self
            .get(dict, b"Kids")
            .and_then(|o| o.as_array().ok())
            .map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
            .unwrap_or_default();

        let child_fields: Vec<ObjectId> = kids
            .iter()
            .copied()
            .filter(|k| {
                self.doc
                    .get_dictionary(*k)
                    .map(|d| d.has(b"T"))
                    .unwrap_or(false)
            })
            .collect();

        if !child_fields.is_empty() {
            for kid in child_fields {
                self.walk_field(kid, &name, &here, depth + 1, out);
            }
            return;
        }

        let Some(ft) = here.ft.clone() else { return };
        // No kid widgets means the widget is merged into this dictionary.
        let widgets = if kids.is_empty() { vec![id] } else { kids };

        out.push(Terminal {
            field_id: id,
            widgets,
            name,
            ft,
            ff: here.ff.unwrap_or(0),
            value: here.value.clone(),
            opts: self.read_options(dict),
            max_len: self
                .get(dict, b"MaxLen")
                .and_then(|o| o.as_i64().ok())
                .map(|v| v.max(0) as usize),
            tooltip: self.get(dict, b"TU").and_then(|o| o.as_str().ok()).map(decode_text),
        });
    }

    /// `/Opt` entries are either a string or `[export, display]`.
    fn read_options(&self, dict: &Dictionary) -> Vec<String> {
        self.get(dict, b"Opt")
            .and_then(|o| o.as_array().ok())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| match self.resolve(o)? {
                        Object::String(s, _) => Some(decode_text(s)),
                        Object::Array(pair) => pair.first().and_then(|f| match self.resolve(f)? {
                            Object::String(s, _) => Some(decode_text(s)),
                            _ => None,
                        }),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn field_kind(&self, t: &Terminal, widget: Option<ObjectId>) -> FieldKind {
        match t.ft.as_slice() {
            b"Tx" => FieldKind::Text {
                multiline: t.ff & flags::MULTILINE != 0,
                max_len: t.max_len,
            },
            b"Btn" if t.ff & flags::PUSHBUTTON != 0 => FieldKind::Button,
            b"Btn" => {
                // Each widget in a radio group has its own "on" name, so the
                // kind has to be resolved per widget, not per field.
                let on_value = widget
                    .or_else(|| t.widgets.first().copied())
                    .and_then(|w| self.on_state(w))
                    .unwrap_or_else(|| "Yes".to_string());
                if t.ff & flags::RADIO != 0 {
                    FieldKind::Radio { on_value }
                } else {
                    FieldKind::Checkbox { on_value }
                }
            }
            b"Ch" => FieldKind::Choice {
                options: t.opts.clone(),
                editable: t.ff & flags::COMBO != 0 && t.ff & flags::EDIT != 0,
            },
            b"Sig" => FieldKind::Signature,
            _ => FieldKind::Text { multiline: false, max_len: None },
        }
    }

    /// The name a checkbox or radio widget uses for its "on" appearance — the
    /// one key in `/AP /N` that isn't `/Off`.
    fn on_state(&self, widget: ObjectId) -> Option<String> {
        let dict = self.doc.get_dictionary(widget).ok()?;
        let ap = self.get(dict, b"AP")?.as_dict().ok()?;
        let normal = self.get(ap, b"N")?.as_dict().ok()?;
        normal
            .iter()
            .map(|(k, _)| k.as_slice())
            .find(|k| *k != b"Off")
            .map(|k| String::from_utf8_lossy(k).into_owned())
    }

    fn widget_value(&self, t: &Terminal, widget: ObjectId, kind: &FieldKind) -> FieldValue {
        match kind {
            FieldKind::Checkbox { on_value } | FieldKind::Radio { on_value } => {
                // A radio group's /V names the chosen kid; compare against the
                // widget's own appearance state so only that widget reads as on.
                let selected = match t.value.as_ref() {
                    Some(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
                    _ => "Off".to_string(),
                };
                let mine = self
                    .doc
                    .get_dictionary(widget)
                    .ok()
                    .and_then(|d| d.get(b"AS").ok())
                    .and_then(|o| o.as_name().ok())
                    .map(|n| String::from_utf8_lossy(n).into_owned());
                let on = selected == *on_value && mine.as_deref() != Some("Off");
                FieldValue::Bool(on)
            }
            FieldKind::Choice { .. } => match t.value.as_ref() {
                Some(Object::String(s, _)) => FieldValue::Selected(vec![decode_text(s)]),
                Some(Object::Array(items)) => FieldValue::Selected(
                    items
                        .iter()
                        .filter_map(|i| match i {
                            Object::String(s, _) => Some(decode_text(s)),
                            _ => None,
                        })
                        .collect(),
                ),
                _ => FieldValue::Empty,
            },
            _ => match t.value.as_ref() {
                Some(Object::String(s, _)) => FieldValue::Text(decode_text(s)),
                Some(Object::Name(n)) => FieldValue::Text(String::from_utf8_lossy(n).into_owned()),
                _ => FieldValue::Empty,
            },
        }
    }

    /// A widget's `/Rect`, mapped from user space into the page's view space.
    fn widget_rect(
        &self,
        widget: ObjectId,
        page: usize,
        transforms: &[Affine],
    ) -> Option<WidgetRect> {
        let dict = self.doc.get_dictionary(widget).ok()?;
        let (x0, y0, x1, y1) = read_rect_full(dict)?;
        let m = *transforms.get(page)?;
        // Transform all four corners: under /Rotate the axes swap.
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)].map(|(x, y)| m.apply(x, y));
        let xs = corners.map(|c| c.0);
        let ys = corners.map(|c| c.1);
        let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some(WidgetRect {
            page,
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        })
    }

    /// The object id of a widget's current normal appearance stream.
    fn appearance_stream_id(&self, widget: ObjectId) -> Option<ObjectId> {
        let dict = self.doc.get_dictionary(widget).ok()?;
        let ap = dict.get(b"AP").ok().and_then(|o| self.resolve(o))?.as_dict().ok()?;
        match ap.get(b"N").ok()? {
            Object::Reference(id) => Some(*id),
            // A state dictionary: pick the entry matching /AS.
            Object::Dictionary(states) => {
                let want = dict.get(b"AS").ok()?.as_name().ok()?;
                states.get(want).ok()?.as_reference().ok()
            }
            _ => None,
        }
    }

    /// Build a fresh `/AP /N` form XObject showing `text`, honouring the
    /// field's `/DA` (default appearance) string for font and colour.
    fn regenerate_appearance(&mut self, widget: ObjectId, text: &str) -> Result<()> {
        let dict = self.doc.get_dictionary(widget).map_err(pdf_err)?;
        let Some((x0, y0, x1, y1)) = read_rect_full(dict) else { return Ok(()) };
        let (w, h) = ((x1 - x0).abs(), (y1 - y0).abs());

        let da = self
            .field_da(widget)
            .unwrap_or_else(|| "/Helv 0 Tf 0 g".to_string());
        let (font_name, mut size) = parse_da(&da);
        if size <= 0.0 {
            // Auto-size: fill most of the box, with a sane ceiling.
            size = (h * 0.66).clamp(4.0, 24.0);
        }
        let pad = 2.0_f64;
        let baseline = (h - size) / 2.0 + size * 0.22;

        let mut ops = String::new();
        // /Tx BMC ... EMC marks this as generated field content, per the spec.
        writeln!(ops, "/Tx BMC q").unwrap();
        writeln!(ops, "{} {} {} {} re W n", fmt(pad), 0.0, fmt(w - pad * 2.0), fmt(h)).unwrap();
        writeln!(ops, "BT {da}").unwrap();
        writeln!(ops, "/{font_name} {} Tf", fmt(size)).unwrap();
        writeln!(ops, "{} {} Td", fmt(pad), fmt(baseline.max(pad))).unwrap();
        writeln!(ops, "({}) Tj", escape(text)).unwrap();
        ops.push_str("ET Q EMC\n");

        let resources = self
            .acroform()
            .and_then(|f| f.get(b"DR").ok().cloned())
            .unwrap_or(Object::Dictionary(Dictionary::new()));

        let mut stream = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "FormType" => 1,
                "BBox" => vec![0.into(), 0.into(), w.into(), h.into()],
                "Resources" => resources,
            },
            ops.into_bytes(),
        );
        stream.compress().map_err(pdf_err)?;
        let stream_id = self.doc.add_object(stream);

        let widget_dict = self.doc.get_dictionary_mut(widget).map_err(pdf_err)?;
        widget_dict.set(
            "AP",
            dictionary! { "N" => Object::Reference(stream_id) },
        );
        widget_dict.remove(b"AS");
        Ok(())
    }

    /// A widget's `/DA`, falling back to the form-wide default.
    fn field_da(&self, widget: ObjectId) -> Option<String> {
        let mut cur = Some(widget);
        for _ in 0..MAX_DEPTH {
            let id = cur?;
            let dict = self.doc.get_dictionary(id).ok()?;
            if let Some(da) = self.get(dict, b"DA").and_then(|o| o.as_str().ok()) {
                return Some(decode_text(da));
            }
            cur = dict.get(b"Parent").ok().and_then(|o| o.as_reference().ok());
        }
        self.acroform()
            .and_then(|f| self.get(f, b"DA"))
            .and_then(|o| o.as_str().ok())
            .map(decode_text)
    }

    /// Ask viewers to rebuild appearances themselves as well as using ours —
    /// harmless when our streams are good, a safety net when they are not.
    pub fn set_need_appearances(&mut self, on: bool) -> Result<()> {
        let Some(id) = self.acroform_id() else { return Ok(()) };
        self.doc
            .get_dictionary_mut(id)
            .map_err(pdf_err)?
            .set("NeedAppearances", Object::Boolean(on));
        Ok(())
    }
}

#[derive(Default)]
struct Inherited {
    ft: Option<Vec<u8>>,
    ff: Option<i64>,
    value: Option<Object>,
}

fn number(obj: &Object) -> Option<f64> {
    match obj {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

fn read_rect_full(dict: &Dictionary) -> Option<(f64, f64, f64, f64)> {
    let arr = dict.get(b"Rect").ok()?.as_array().ok()?;
    if arr.len() < 4 {
        return None;
    }
    let v: Vec<f64> = arr.iter().filter_map(number).collect();
    if v.len() < 4 {
        return None;
    }
    Some((
        v[0].min(v[2]),
        v[1].min(v[3]),
        v[0].max(v[2]),
        v[1].max(v[3]),
    ))
}

fn read_rect(dict: &Dictionary) -> Option<(f64, f64)> {
    read_rect_full(dict).map(|(x0, y0, _, _)| (x0, y0))
}

fn fmt(v: f64) -> String {
    crate::n(v)
}

/// Pull the font resource name and size out of a `/DA` string such as
/// `"/Helv 12 Tf 0 g"`. Returns `("Helv", 12.0)`, or a size of 0 for auto.
fn parse_da(da: &str) -> (String, f64) {
    let toks: Vec<&str> = da.split_whitespace().collect();
    let mut name = "Helv".to_string();
    let mut size = 0.0;
    for (i, t) in toks.iter().enumerate() {
        if *t == "Tf" && i >= 2 {
            if let Some(stripped) = toks[i - 2].strip_prefix('/') {
                name = stripped.to_string();
            }
            size = toks[i - 1].parse().unwrap_or(0.0);
        }
    }
    (name, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn da_parsing() {
        assert_eq!(parse_da("/Helv 12 Tf 0 g"), ("Helv".into(), 12.0));
        assert_eq!(parse_da("0 g /Arial 0 Tf"), ("Arial".into(), 0.0));
        assert_eq!(parse_da("nonsense"), ("Helv".into(), 0.0));
    }

    #[test]
    fn text_roundtrip() {
        assert_eq!(decode_text(b"hello"), "hello");
        assert_eq!(decode_text(&[0xFE, 0xFF, 0x00, 0x41]), "A");
        match encode_text("caf\u{e9}") {
            Object::String(b, _) => assert_eq!(b, vec![b'c', b'a', b'f', 0xE9]),
            _ => panic!("expected a string"),
        }
        match encode_text("\u{4e2d}") {
            Object::String(b, _) => assert_eq!(b, vec![0xFE, 0xFF, 0x4E, 0x2D]),
            _ => panic!("expected a string"),
        }
    }

    #[test]
    fn flag_bits_match_the_spec() {
        // Spot-check against PDF 32000-1 table 227/228.
        assert_eq!(flags::READ_ONLY, 1);
        assert_eq!(flags::REQUIRED, 2);
        assert_eq!(flags::MULTILINE, 4096);
        assert_eq!(flags::RADIO, 32768);
        assert_eq!(flags::PUSHBUTTON, 65536);
    }
}

/// Collapse per-state appearance dictionaries to the single stream the widget
/// is currently showing.
///
/// Our renderer only draws an annotation whose `/AP /N` is a stream; when `/N`
/// is a state dictionary — which is exactly how checkboxes and radio buttons
/// must be written — it skips the annotation entirely. So the *preview* copy of
/// the document gets its state dictionaries resolved against each widget's
/// `/AS`, while the document we save keeps them intact and stays interactive in
/// other viewers.
pub(crate) fn resolve_appearance_states(doc: &mut Document) {
    let mut fixups: Vec<(ObjectId, ObjectId)> = Vec::new();

    for (&id, obj) in doc.objects.iter() {
        let Ok(dict) = obj.as_dict() else { continue };
        if dict.get(b"Subtype").and_then(|o| o.as_name()).ok() != Some(b"Widget".as_slice()) {
            continue;
        }
        let Ok(ap) = dict.get(b"AP").and_then(|o| o.as_dict()) else { continue };
        let Ok(Object::Dictionary(states)) = ap.get(b"N") else { continue };

        // Prefer the state named by /AS; fall back to /Off so an unset widget
        // still draws its empty box.
        let want = dict
            .get(b"AS")
            .and_then(|o| o.as_name())
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|_| b"Off".to_vec());
        let chosen = states
            .get(&want)
            .or_else(|_| states.get(b"Off"))
            .ok()
            .and_then(|o| o.as_reference().ok());
        if let Some(stream_id) = chosen {
            fixups.push((id, stream_id));
        }
    }

    for (widget, stream_id) in fixups {
        if let Ok(dict) = doc.get_dictionary_mut(widget) {
            dict.set("AP", dictionary! { "N" => Object::Reference(stream_id) });
        }
    }
}
