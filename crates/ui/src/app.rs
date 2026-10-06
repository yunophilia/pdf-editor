//! The editor UI.
//!
//! Pages are rendered to PNG by the engine and shown as `<img>`; live tool
//! feedback is an SVG overlay and form fields are real inputs positioned over
//! the page. Nothing here paints to a canvas, so the same component tree runs
//! unchanged on the web and in the desktop webview.

use crate::files::{self, decode_image};
use crate::Engine;
use base64::Engine as _;
use dioxus::prelude::*;
use pdf_editor_shared::{
    Command, DocInfo, FieldKind, FieldValue, FormField, ImageSpec, InkSpec, PageSize, RectSpec,
    Rgb, StandardFont, TextSpec,
};
use std::collections::BTreeSet;

const ZOOM_STEPS: [f64; 13] = [0.25, 0.35, 0.5, 0.67, 0.8, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0];
const THUMB_SCALE: f32 = 0.22;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Select,
    Text,
    Highlight,
    Rect,
    Draw,
    Image,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Text => "Text",
            Tool::Highlight => "Highlight",
            Tool::Rect => "Rect",
            Tool::Draw => "Draw",
            Tool::Image => "Image",
        }
    }

    const ALL: [Tool; 6] = [
        Tool::Select,
        Tool::Text,
        Tool::Highlight,
        Tool::Rect,
        Tool::Draw,
        Tool::Image,
    ];
}

/// Everything the components share. `Signal` is `Copy`, so this is too.
#[derive(Clone, Copy)]
pub struct State {
    pub doc: Signal<Option<DocInfo>>,
    pub fields: Signal<Vec<FormField>>,
    pub zoom: Signal<f64>,
    pub tool: Signal<Tool>,
    pub selected: Signal<BTreeSet<usize>>,
    pub status: Signal<String>,
    pub file_name: Signal<String>,
    pub color: Signal<String>,
    pub font_size: Signal<f64>,
    pub stroke_width: Signal<f64>,
    pub opacity: Signal<f64>,
    pub show_forms: Signal<bool>,
    /// Bumped whenever page pixels may have changed, to re-run renders.
    pub generation: Signal<u64>,
    /// An in-progress text insertion: page and view-space position.
    pub text_at: Signal<Option<(usize, f64, f64)>>,
    /// An image waiting to be placed.
    pub pending_image: Signal<Option<(Vec<u8>, u32, u32)>>,
}

impl State {
    /// Fold a fresh engine reply into the UI state.
    fn apply(&mut self, info: DocInfo) {
        let pages = info.pages.len();
        self.doc.set(Some(info));
        self.generation += 1;
        self.selected.write().retain(|i| *i < pages);
    }

    fn fail(&mut self, context: &str, e: String) {
        self.status.set(format!("{context}: {e}"));
    }

    fn pages(&self) -> Vec<PageSize> {
        self.doc.read().as_ref().map(|d| d.pages.clone()).unwrap_or_default()
    }
}

fn hex_to_rgb(hex: &str) -> Rgb {
    let v = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    Rgb(
        ((v >> 16) & 0xFF) as f64 / 255.0,
        ((v >> 8) & 0xFF) as f64 / 255.0,
        (v & 0xFF) as f64 / 255.0,
    )
}

fn png_data_url(bytes: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn out_name(current: &str, suffix: &str) -> String {
    let stem = current.strip_suffix(".pdf").unwrap_or(current);
    format!("{stem}{suffix}.pdf")
}

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

#[component]
pub fn App() -> Element {
    use_context_provider(Engine::new);
    let state = State {
        doc: use_signal(|| None),
        fields: use_signal(Vec::new),
        zoom: use_signal(|| 1.0),
        tool: use_signal(|| Tool::Select),
        selected: use_signal(BTreeSet::new),
        status: use_signal(|| "Open a PDF to get started".to_string()),
        file_name: use_signal(|| "document.pdf".to_string()),
        color: use_signal(|| "#d1242f".to_string()),
        font_size: use_signal(|| 14.0),
        stroke_width: use_signal(|| 2.0),
        opacity: use_signal(|| 1.0),
        show_forms: use_signal(|| true),
        generation: use_signal(|| 0u64),
        text_at: use_signal(|| None),
        pending_image: use_signal(|| None),
    };
    use_context_provider(|| state);

    // A document handed to us at launch: a path on the command line (desktop)
    // or a same-origin ?open= parameter (web).
    let engine = use_context::<Engine>();
    use_future(move || {
        let engine = engine.clone();
        let mut state = state;
        async move {
            let picked = match files::startup_document().await {
                Ok(Some(p)) => p,
                Ok(None) => return,
                Err(e) => return state.fail("Could not open the document given at startup", e),
            };
            match engine.open(picked.bytes).await {
                Ok(info) => {
                    let pages = info.pages.len();
                    state.file_name.set(picked.name.clone());
                    state.apply(info);
                    refresh_fields(&engine, state).await;
                    state.status.set(format!("{} · {pages} pages", picked.name));
                }
                Err(e) => state.fail("Could not open that file", e),
            }
        }
    });

    rsx! {
        style { {include_str!("../assets/style.css")} }
        div { class: "app",
            Toolbar {}
            div { class: "body",
                Sidebar {}
                Viewer {}
            }
            footer { class: "status", "{state.status}" }
        }
    }
}

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

#[component]
fn Toolbar() -> Element {
    let engine = use_context::<Engine>();
    let mut state = use_context::<State>();
    let has_doc = state.doc.read().is_some();
    let (can_undo, can_redo, has_form) = state
        .doc
        .read()
        .as_ref()
        .map(|d| (d.can_undo, d.can_redo, d.has_form))
        .unwrap_or((false, false, false));
    let tool = (state.tool)();

    let open = {
        let engine = engine.clone();
        move |_| {
            let engine = engine.clone();
            spawn(async move {
                let Some(picked) = files::pick("Open a PDF", &["pdf"]).await else { return };
                state.status.set(format!("Opening {}…", picked.name));
                match engine.open(picked.bytes).await {
                    Ok(info) => {
                        let pages = info.pages.len();
                        state.file_name.set(picked.name.clone());
                        state.apply(info);
                        refresh_fields(&engine, state).await;
                        state.status.set(format!("{} · {pages} pages", picked.name));
                    }
                    Err(e) => state.fail("Could not open that file", e),
                }
            });
        }
    };

    let new_doc = {
        let engine = engine.clone();
        move |_| {
            let engine = engine.clone();
            spawn(async move {
                match engine.blank(612.0, 792.0).await {
                    Ok(info) => {
                        state.file_name.set("untitled.pdf".into());
                        state.apply(info);
                        state.fields.set(Vec::new());
                        state.status.set("New blank document".into());
                    }
                    Err(e) => state.fail("Could not create a document", e),
                }
            });
        }
    };

    let add_pdf = {
        let engine = engine.clone();
        move |_| {
            let engine = engine.clone();
            spawn(async move {
                let Some(picked) = files::pick("Add a PDF", &["pdf"]).await else { return };
                let at = state.selected.read().iter().next_back().map(|i| i + 1);
                match engine.merge(picked.bytes, at).await {
                    Ok(info) => {
                        state.apply(info);
                        refresh_fields(&engine, state).await;
                        state.status.set(format!("Added {}", picked.name));
                    }
                    Err(e) => state.fail("Could not add that file", e),
                }
            });
        }
    };

    let save = {
        let engine = engine.clone();
        move |_| {
            let engine = engine.clone();
            spawn(async move {
                state.status.set("Saving…".into());
                match engine.save().await {
                    Ok(bytes) => {
                        let size = bytes.len();
                        let name = out_name(&state.file_name.read(), "-edited");
                        match files::save(bytes, &name).await {
                            Ok(Some(n)) => {
                                state.status.set(format!("Saved {n} ({} KB)", size / 1024))
                            }
                            Ok(None) => state.status.set("Save cancelled".into()),
                            Err(e) => state.fail("Could not save", e),
                        }
                    }
                    Err(e) => state.fail("Could not save", e),
                }
            });
        }
    };

    let place_image = {
        move |_| {
            spawn(async move {
                let Some(picked) = files::pick("Choose an image", &["png", "jpg", "jpeg", "gif", "webp", "bmp"]).await
                else {
                    return;
                };
                match decode_image(&picked.bytes, 2000) {
                    Ok((rgba, w, h)) => {
                        state.pending_image.set(Some((rgba, w, h)));
                        state.tool.set(Tool::Image);
                        state
                            .status
                            .set(format!("{} ready ({w}x{h}) — drag on a page to place it", picked.name));
                    }
                    Err(e) => state.fail("Could not read that image", e),
                }
            });
        }
    };

    rsx! {
        header { class: "toolbar",
            div { class: "group",
                button { onclick: open, "Open" }
                button { onclick: new_doc, "New" }
                button { disabled: !has_doc, onclick: add_pdf, "Add PDF" }
                button { class: "primary", disabled: !has_doc, onclick: save, "Save" }
            }

            div { class: "group",
                button {
                    disabled: !can_undo,
                    onclick: command(engine.clone(), state, Command::Undo, "Undo"),
                    "Undo"
                }
                button {
                    disabled: !can_redo,
                    onclick: command(engine.clone(), state, Command::Redo, "Redo"),
                    "Redo"
                }
            }

            div { class: "group tools",
                for t in Tool::ALL {
                    button {
                        key: "{t:?}",
                        class: if tool == t { "active" } else { "" },
                        disabled: !has_doc,
                        onclick: move |_| {
                            state.tool.set(t);
                            state.text_at.set(None);
                        },
                        "{t.label()}"
                    }
                }
                if tool == Tool::Image {
                    button { onclick: place_image, "Choose image…" }
                }
            }

            div { class: "group options",
                if matches!(tool, Tool::Text | Tool::Highlight | Tool::Rect | Tool::Draw) {
                    label {
                        "Colour "
                        input {
                            r#type: "color",
                            value: "{state.color}",
                            oninput: move |e| state.color.set(e.value()),
                        }
                    }
                }
                if tool == Tool::Text {
                    label {
                        "Size "
                        input {
                            r#type: "number",
                            min: "4",
                            max: "200",
                            value: "{state.font_size}",
                            oninput: move |e| {
                                if let Ok(v) = e.value().parse() { state.font_size.set(v) }
                            },
                        }
                    }
                }
                if matches!(tool, Tool::Rect | Tool::Draw) {
                    label {
                        "Width "
                        input {
                            r#type: "range",
                            min: "0.5",
                            max: "20",
                            step: "0.5",
                            value: "{state.stroke_width}",
                            oninput: move |e| {
                                if let Ok(v) = e.value().parse() { state.stroke_width.set(v) }
                            },
                        }
                    }
                }
                if matches!(tool, Tool::Highlight | Tool::Rect | Tool::Draw) {
                    label {
                        "Opacity "
                        input {
                            r#type: "range",
                            min: "0.05",
                            max: "1",
                            step: "0.05",
                            value: "{state.opacity}",
                            oninput: move |e| {
                                if let Ok(v) = e.value().parse() { state.opacity.set(v) }
                            },
                        }
                    }
                }
            }

            if has_form {
                div { class: "group",
                    button {
                        class: if (state.show_forms)() { "active" } else { "" },
                        onclick: move |_| {
                            let on = (state.show_forms)();
                            state.show_forms.set(!on);
                        },
                        "Form fields"
                    }
                    button {
                        onclick: command(engine.clone(), state, Command::FlattenForm, "Flattened the form"),
                        "Flatten"
                    }
                }
            }

            div { class: "group zoom",
                button { onclick: move |_| step_zoom(state, -1), "−" }
                span { class: "zoom-label", "{((state.zoom)() * 100.0).round()}%" }
                button { onclick: move |_| step_zoom(state, 1), "+" }
            }
        }
    }
}

fn step_zoom(mut state: State, dir: i32) {
    let current = (state.zoom)();
    let next = if dir > 0 {
        ZOOM_STEPS.iter().find(|z| **z > current + 1e-6).copied()
    } else {
        ZOOM_STEPS.iter().rev().find(|z| **z < current - 1e-6).copied()
    };
    if let Some(z) = next {
        state.zoom.set(z);
    }
}

/// An onclick handler that runs one engine command and folds in the result.
fn command(
    engine: Engine,
    mut state: State,
    cmd: Command,
    note: &'static str,
) -> impl FnMut(Event<MouseData>) {
    move |_| {
        let engine = engine.clone();
        let cmd = cmd.clone();
        spawn(async move {
            match engine.exec(cmd).await {
                Ok(info) => {
                    state.apply(info);
                    refresh_fields(&engine, state).await;
                    state.status.set(note.to_string());
                }
                Err(e) => state.fail(note, e),
            }
        });
    }
}

/// Re-read form fields, which move or change with almost every edit.
async fn refresh_fields(engine: &Engine, mut state: State) {
    let has_form = state.doc.read().as_ref().map(|d| d.has_form).unwrap_or(false);
    if !has_form {
        state.fields.set(Vec::new());
        return;
    }
    match engine.fields().await {
        Ok(f) => state.fields.set(f),
        Err(e) => state.fail("Could not read the form", e),
    }
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

#[component]
fn Sidebar() -> Element {
    let engine = use_context::<Engine>();
    let mut state = use_context::<State>();
    let pages = state.pages();
    let any = !state.selected.read().is_empty();
    let only_page = pages.len() <= 1;

    let selected_vec = move || state.selected.read().iter().copied().collect::<Vec<_>>();

    let rotate = move |delta: i64| {
        let engine = engine.clone();
        move |_| {
            let engine = engine.clone();
            spawn(async move {
                for i in selected_vec() {
                    match engine.exec(Command::Rotate { index: i, delta }).await {
                        Ok(info) => state.apply(info),
                        Err(e) => return state.fail("Could not rotate", e),
                    }
                }
                refresh_fields(&engine, state).await;
            });
        }
    };

    let delete = {
        let engine = use_context::<Engine>();
        move |_| {
            let engine = engine.clone();
            spawn(async move {
                // Delete from the back so earlier indices stay valid.
                for i in selected_vec().into_iter().rev() {
                    match engine.exec(Command::Delete { index: i }).await {
                        Ok(info) => state.apply(info),
                        Err(e) => return state.fail("Could not delete", e),
                    }
                }
                state.selected.write().clear();
                refresh_fields(&engine, state).await;
            });
        }
    };

    let duplicate = {
        let engine = use_context::<Engine>();
        move |_| {
            let engine = engine.clone();
            spawn(async move {
                for i in selected_vec().into_iter().rev() {
                    match engine.exec(Command::Duplicate { index: i }).await {
                        Ok(info) => state.apply(info),
                        Err(e) => return state.fail("Could not duplicate", e),
                    }
                }
                refresh_fields(&engine, state).await;
            });
        }
    };

    let insert_blank = {
        let engine = use_context::<Engine>();
        move |_| {
            let engine = engine.clone();
            spawn(async move {
                let pages = state.pages();
                let at = state.selected.read().iter().next_back().map(|i| i + 1).unwrap_or(pages.len());
                let size = pages.get(at.saturating_sub(1)).copied().unwrap_or(PageSize {
                    width: 612.0,
                    height: 792.0,
                });
                match engine
                    .exec(Command::InsertBlank {
                        index: at,
                        width: size.width as f64,
                        height: size.height as f64,
                    })
                    .await
                {
                    Ok(info) => {
                        state.apply(info);
                        refresh_fields(&engine, state).await;
                    }
                    Err(e) => state.fail("Could not insert a page", e),
                }
            });
        }
    };

    let extract = {
        let engine = use_context::<Engine>();
        move |_| {
            let engine = engine.clone();
            spawn(async move {
                let pages = selected_vec();
                if pages.is_empty() {
                    return;
                }
                match engine.extract(pages.clone()).await {
                    Ok(bytes) => {
                        let label: Vec<String> =
                            pages.iter().map(|p| (p + 1).to_string()).collect();
                        let name =
                            out_name(&state.file_name.read(), &format!("-p{}", label.join("_")));
                        match files::save(bytes, &name).await {
                            Ok(Some(n)) => state.status.set(format!("Extracted to {n}")),
                            Ok(None) => state.status.set("Extract cancelled".into()),
                            Err(e) => state.fail("Could not write the file", e),
                        }
                    }
                    Err(e) => state.fail("Could not extract", e),
                }
            });
        }
    };

    let move_page = {
        let engine = use_context::<Engine>();
        move |dir: i64| {
            let engine = engine.clone();
            move |_| {
                let engine = engine.clone();
                spawn(async move {
                    let Some(&from) = state.selected.read().iter().next() else { return };
                    let count = state.pages().len() as i64;
                    let to = (from as i64 + dir).clamp(0, count - 1) as usize;
                    if to == from {
                        return;
                    }
                    match engine.exec(Command::Move { from, to }).await {
                        Ok(info) => {
                            state.apply(info);
                            state.selected.set(BTreeSet::from([to]));
                            refresh_fields(&engine, state).await;
                        }
                        Err(e) => state.fail("Could not move the page", e),
                    }
                });
            }
        }
    };

    rsx! {
        aside { class: "sidebar",
            div { class: "sidebar-actions",
                button { disabled: !any, title: "Rotate left", onclick: rotate(-90), "⟲" }
                button { disabled: !any, title: "Rotate right", onclick: rotate(90), "⟳" }
                button { disabled: !any, title: "Move up", onclick: move_page(-1), "↑" }
                button { disabled: !any, title: "Move down", onclick: move_page(1), "↓" }
                button { disabled: !any, title: "Duplicate", onclick: duplicate, "⧉" }
                button { disabled: !any, title: "Insert blank page", onclick: insert_blank, "＋" }
                button { disabled: !any, title: "Extract to a new PDF", onclick: extract, "⤓" }
                button {
                    class: "danger",
                    disabled: !any || only_page,
                    title: "Delete",
                    onclick: delete,
                    "✕"
                }
            }
            div { class: "thumbs",
                for (i, size) in pages.iter().enumerate() {
                    Thumb { key: "{i}", index: i, width: size.width, height: size.height }
                }
            }
        }
    }
}

#[component]
fn Thumb(index: usize, width: f32, height: f32) -> Element {
    let engine = use_context::<Engine>();
    let mut state = use_context::<State>();
    let mut seen = use_signal(|| index < 8);

    let png = use_resource(move || {
        let engine = engine.clone();
        let generation = state.generation;
        let g = generation();
        let visible = seen();
        async move {
            let _ = g;
            if !visible {
                return None;
            }
            engine.render(index, THUMB_SCALE).await.ok().map(|b| png_data_url(&b))
        }
    });

    let selected = state.selected.read().contains(&index);
    let src = png().flatten();

    rsx! {
        div {
            class: if selected { "thumb selected" } else { "thumb" },
            onvisible: move |e| {
                if e.data().is_intersecting().unwrap_or(true) {
                    seen.set(true);
                }
            },
            onclick: move |e| {
                let mut sel = state.selected.write();
                if e.modifiers().ctrl() || e.modifiers().meta() {
                    if !sel.insert(index) {
                        sel.remove(&index);
                    }
                } else {
                    sel.clear();
                    sel.insert(index);
                }
            },
            match src {
                Some(url) => rsx! { img { src: "{url}", alt: "Page {index + 1}" } },
                None => rsx! {
                    div {
                        class: "thumb-placeholder",
                        style: "aspect-ratio: {width} / {height}",
                    }
                },
            }
            span { class: "num", "{index + 1}" }
        }
    }
}

// ---------------------------------------------------------------------------
// Viewer
// ---------------------------------------------------------------------------

#[component]
fn Viewer() -> Element {
    let state = use_context::<State>();
    let pages = state.pages();

    if pages.is_empty() {
        return rsx! {
            section { class: "viewer",
                div { class: "empty",
                    h1 { "PDF Editor" }
                    p { "Open a PDF to get started. Everything runs locally — nothing is uploaded." }
                }
            }
        };
    }

    rsx! {
        section { class: "viewer",
            div { class: "pages",
                for (i, size) in pages.iter().enumerate() {
                    PageView { key: "{i}", index: i, width: size.width, height: size.height }
                }
            }
        }
    }
}

/// A drag in progress, in view-space points.
#[derive(Clone, Default)]
struct Drag {
    start: (f64, f64),
    current: (f64, f64),
    points: Vec<(f64, f64)>,
}

impl Drag {
    fn box_of(&self) -> (f64, f64, f64, f64) {
        let (x0, y0) = self.start;
        let (x1, y1) = self.current;
        (x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs())
    }
}

#[component]
fn PageView(index: usize, width: f32, height: f32) -> Element {
    let engine = use_context::<Engine>();
    let mut state = use_context::<State>();
    let mut seen = use_signal(|| index < 3);
    let mut drag = use_signal(|| None::<Drag>);

    let zoom = (state.zoom)();
    let tool = (state.tool)();

    let png = {
        let engine = engine.clone();
        use_resource(move || {
            let engine = engine.clone();
            let generation = state.generation;
            let zoom_sig = state.zoom;
            let g = generation();
            let z = zoom_sig();
            let visible = seen();
            async move {
                let _ = g;
                if !visible {
                    return None;
                }
                engine.render(index, z as f32).await.ok().map(|b| png_data_url(&b))
            }
        })
    };

    let w = width as f64 * zoom;
    let h = height as f64 * zoom;

    // Pointer position -> view space (points, y down).
    let to_view = move |e: &Event<PointerData>| {
        let p = e.data().element_coordinates();
        (p.x / zoom, p.y / zoom)
    };

    let on_down = move |e: Event<PointerData>| {
        state.selected.set(BTreeSet::from([index]));
        match tool {
            Tool::Text => {
                let (x, y) = to_view(&e);
                state.text_at.set(Some((index, x, y)));
            }
            Tool::Highlight | Tool::Rect | Tool::Draw | Tool::Image => {
                let p = to_view(&e);
                drag.set(Some(Drag {
                    start: p,
                    current: p,
                    points: vec![p],
                }));
            }
            Tool::Select => {}
        }
    };

    let on_move = move |e: Event<PointerData>| {
        if drag.read().is_none() {
            return;
        }
        let p = to_view(&e);
        let mut d = drag.write();
        if let Some(d) = d.as_mut() {
            d.current = p;
            if tool == Tool::Draw {
                d.points.push(p);
            }
        }
    };

    let on_up = {
        let engine = engine.clone();
        move |_: Event<PointerData>| {
            let Some(d) = drag.write().take() else { return };
            let engine = engine.clone();
            let colour = hex_to_rgb(&state.color.read());
            let opacity = (state.opacity)();
            let stroke_width = (state.stroke_width)();
            let pending = state.pending_image.read().clone();

            spawn(async move {
                let result = match tool {
                    Tool::Draw => {
                        if d.points.len() < 2 {
                            return;
                        }
                        let points = d.points.iter().flat_map(|(x, y)| [*x, *y]).collect();
                        engine
                            .exec(Command::AddInk(InkSpec {
                                page: index,
                                points,
                                color: colour,
                                width: stroke_width,
                                opacity,
                            }))
                            .await
                    }
                    Tool::Highlight | Tool::Rect => {
                        let (x, y, bw, bh) = d.box_of();
                        if bw < 1.0 || bh < 1.0 {
                            return;
                        }
                        let highlight = tool == Tool::Highlight;
                        engine
                            .exec(Command::AddRect(RectSpec {
                                page: index,
                                x,
                                y,
                                width: bw,
                                height: bh,
                                fill: highlight.then_some(colour),
                                stroke: (!highlight).then_some(colour),
                                stroke_width: if highlight { 0.0 } else { stroke_width },
                                opacity,
                                multiply: highlight,
                            }))
                            .await
                    }
                    Tool::Image => {
                        let Some((rgba, iw, ih)) = pending else {
                            state.status.set("Choose an image first".into());
                            return;
                        };
                        let (x, y, mut bw, mut bh) = d.box_of();
                        if bw < 4.0 || bh < 4.0 {
                            bw = (iw as f64).min(300.0);
                            bh = bw * ih as f64 / iw as f64;
                        } else {
                            bh = bw * ih as f64 / iw as f64;
                        }
                        engine
                            .add_image(ImageSpec {
                                page: index,
                                rgba,
                                img_width: iw,
                                img_height: ih,
                                x,
                                y,
                                width: bw,
                                height: bh,
                            })
                            .await
                    }
                    Tool::Select | Tool::Text => return,
                };
                match result {
                    Ok(info) => {
                        state.apply(info);
                        refresh_fields(&engine, state).await;
                    }
                    Err(e) => state.fail("Edit failed", e),
                }
            });
        }
    };

    let src = png().flatten();
    let preview = drag.read().clone();
    let colour = state.color.read().clone();
    let show_forms = (state.show_forms)();

    rsx! {
        div { class: "page-slot",
            div {
                class: "page",
                style: "width: {w}px; height: {h}px;",
                onvisible: move |e| {
                    if e.data().is_intersecting().unwrap_or(true) {
                        seen.set(true);
                    }
                },

                match src {
                    Some(url) => rsx! { img { class: "render", src: "{url}", alt: "Page {index + 1}" } },
                    None => rsx! { div { class: "render placeholder" } },
                }

                svg {
                    class: "overlay",
                    "data-tool": "{tool:?}",
                    onpointerdown: on_down,
                    onpointermove: on_move,
                    onpointerup: on_up,

                    if let Some(d) = preview {
                        match tool {
                            Tool::Draw => {
                                let pts: Vec<String> = d
                                    .points
                                    .iter()
                                    .map(|(x, y)| format!("{},{}", x * zoom, y * zoom))
                                    .collect();
                                rsx! {
                                    polyline {
                                        points: "{pts.join(\" \")}",
                                        fill: "none",
                                        stroke: "{colour}",
                                        "stroke-width": "{stroke_px(state)}",
                                        "stroke-linecap": "round",
                                    }
                                }
                            }
                            Tool::Highlight | Tool::Rect | Tool::Image => {
                                let (x, y, bw, bh) = d.box_of();
                                rsx! {
                                    rect {
                                        x: "{x * zoom}",
                                        y: "{y * zoom}",
                                        width: "{bw * zoom}",
                                        height: "{bh * zoom}",
                                        fill: if tool == Tool::Rect { "none" } else { "{colour}" },
                                        "fill-opacity": "0.4",
                                        stroke: "{colour}",
                                        "stroke-width": "{stroke_px(state)}",
                                    }
                                }
                            }
                            _ => rsx! {},
                        }
                    }
                }

                if show_forms {
                    FormLayer { page: index, zoom }
                }

                if let Some((page, x, y)) = (state.text_at)() {
                    if page == index {
                        TextEntry { page, x, y, zoom }
                    }
                }
            }
            span { class: "page-label", "Page {index + 1}" }
        }
    }
}

fn stroke_px(state: State) -> f64 {
    ((state.stroke_width)() * (state.zoom)()).max(1.0)
}

// ---------------------------------------------------------------------------
// Text insertion
// ---------------------------------------------------------------------------

#[component]
fn TextEntry(page: usize, x: f64, y: f64, zoom: f64) -> Element {
    let engine = use_context::<Engine>();
    let mut state = use_context::<State>();
    let mut draft = use_signal(String::new);
    let size = (state.font_size)();

    let commit = move |_| {
        let engine = engine.clone();
        let text = draft.read().clone();
        spawn(async move {
            state.text_at.set(None);
            if text.trim().is_empty() {
                return;
            }
            let spec = TextSpec {
                page,
                x,
                // The click marks the top-left of the text; the PDF baseline
                // sits roughly 0.8em below the cap line.
                y: y + size * 0.8,
                text,
                font: StandardFont::Helvetica,
                size,
                color: hex_to_rgb(&state.color.read()),
            };
            match engine.exec(Command::AddText(spec)).await {
                Ok(info) => state.apply(info),
                Err(e) => state.fail("Could not add text", e),
            }
        });
    };

    rsx! {
        div {
            class: "text-entry",
            style: "left: {x * zoom}px; top: {y * zoom}px;",
            input {
                autofocus: true,
                value: "{draft}",
                placeholder: "Type, then Enter",
                oninput: move |e| draft.set(e.value()),
                onkeydown: move |e| {
                    if e.key() == Key::Escape {
                        state.text_at.set(None);
                    }
                },
                onchange: commit,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Form fields
// ---------------------------------------------------------------------------

#[component]
fn FormLayer(page: usize, zoom: f64) -> Element {
    let state = use_context::<State>();
    let fields: Vec<FormField> = state
        .fields
        .read()
        .iter()
        .filter(|f| f.rect.page == page)
        .cloned()
        .collect();

    rsx! {
        div { class: "form-layer",
            for field in fields {
                FieldWidget { key: "{field.id}", field: field.clone(), zoom }
            }
        }
    }
}

#[component]
fn FieldWidget(field: FormField, zoom: f64) -> Element {
    let engine = use_context::<Engine>();
    let mut state = use_context::<State>();

    let r = field.rect;
    let style = format!(
        "left: {}px; top: {}px; width: {}px; height: {}px;",
        r.x * zoom,
        r.y * zoom,
        r.width * zoom,
        r.height * zoom
    );
    let title = field
        .tooltip
        .clone()
        .unwrap_or_else(|| field.name.clone());

    let id = field.id;
    let commit = move |value: FieldValue| {
        let engine = engine.clone();
        spawn(async move {
            match engine.exec(Command::SetField { id, value }).await {
                Ok(info) => {
                    state.apply(info);
                    refresh_fields(&engine, state).await;
                }
                Err(e) => state.fail("Could not update the field", e),
            }
        });
    };

    let read_only = field.read_only;
    let font_px = (r.height * zoom * 0.6).clamp(8.0, 20.0);

    let inner = match &field.kind {
        FieldKind::Text { multiline, .. } => {
            let value = field.value.as_text().to_string();
            if *multiline {
                let commit = commit;
                rsx! {
                    textarea {
                        style: "font-size: {font_px}px",
                        readonly: read_only,
                        initial_value: "{value}",
                        onchange: move |e| commit(FieldValue::Text(e.value())),
                    }
                }
            } else {
                let commit = commit;
                rsx! {
                    input {
                        r#type: "text",
                        style: "font-size: {font_px}px",
                        readonly: read_only,
                        initial_value: "{value}",
                        onchange: move |e| commit(FieldValue::Text(e.value())),
                    }
                }
            }
        }
        FieldKind::Checkbox { .. } | FieldKind::Radio { .. } => {
            let on = field.value.is_on();
            let radio = matches!(field.kind, FieldKind::Radio { .. });
            let commit = commit;
            rsx! {
                input {
                    r#type: if radio { "radio" } else { "checkbox" },
                    name: "{field.name}",
                    checked: on,
                    disabled: read_only,
                    onchange: move |e| commit(FieldValue::Bool(e.checked())),
                }
            }
        }
        FieldKind::Choice { options, .. } => {
            let current = field.value.as_text().to_string();
            let options = options.clone();
            let commit = commit;
            rsx! {
                select {
                    style: "font-size: {font_px}px",
                    disabled: read_only,
                    onchange: move |e| commit(FieldValue::Selected(vec![e.value()])),
                    option { value: "", selected: current.is_empty(), "—" }
                    for opt in options {
                        option {
                            key: "{opt}",
                            value: "{opt}",
                            selected: opt == current,
                            "{opt}"
                        }
                    }
                }
            }
        }
        FieldKind::Signature => rsx! {
            div { class: "field-signature", "Signature" }
        },
        FieldKind::Button => rsx! {},
    };

    rsx! {
        div {
            class: if field.required { "field required" } else { "field" },
            style: "{style}",
            title: "{title}",
            {inner}
        }
    }
}
