//! The UI's handle on the PDF engine.
//!
//! Both targets present the same async API; only the transport differs.
//!
//! * **web** — the engine lives in a Web Worker so rasterising never blocks the
//!   UI. Requests cross as `postMessage`; structured ones carry JSON, bulky
//!   ones carry raw buffers.
//! * **desktop** — the engine lives on a background thread. The editor is
//!   *constructed inside that thread*, so it never has to be `Send`; only the
//!   request and reply values cross.

use futures_channel::oneshot;
use pdf_editor_shared::{Command, DocInfo, FormField, ImageSpec, Response};

/// What a request produced: either a JSON-encoded [`Response`] or raw bytes.
enum Reply {
    Json(String),
    Bytes(Vec<u8>),
}

type Answer = Result<Reply, String>;

fn decode(reply: Reply) -> Result<Response, String> {
    match reply {
        Reply::Json(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
        Reply::Bytes(_) => Err("expected a structured reply, got bytes".into()),
    }
}

fn bytes(reply: Reply) -> Result<Vec<u8>, String> {
    match reply {
        Reply::Bytes(b) => Ok(b),
        Reply::Json(s) => Err(format!("expected bytes, got {s}")),
    }
}

fn info(response: Response) -> Result<DocInfo, String> {
    match response {
        Response::Info(i) => Ok(i),
        Response::Fields(_) => Err("expected document info".into()),
    }
}

// ---------------------------------------------------------------------------
// Shared surface
// ---------------------------------------------------------------------------

impl Engine {
    pub async fn open(&self, data: Vec<u8>) -> Result<DocInfo, String> {
        info(decode(self.request(Request::Open(data)).await?)?)
    }

    pub async fn blank(&self, width: f64, height: f64) -> Result<DocInfo, String> {
        info(decode(self.request(Request::Blank(width, height)).await?)?)
    }

    /// Run a command that returns document info.
    pub async fn exec(&self, cmd: Command) -> Result<DocInfo, String> {
        info(decode(self.request(Request::Exec(cmd)).await?)?)
    }

    pub async fn fields(&self) -> Result<Vec<FormField>, String> {
        match decode(self.request(Request::Exec(Command::FormFields)).await?)? {
            Response::Fields(f) => Ok(f),
            Response::Info(_) => Err("expected form fields".into()),
        }
    }

    /// Rasterise a page to PNG bytes.
    pub async fn render(&self, index: usize, scale: f32) -> Result<Vec<u8>, String> {
        bytes(self.request(Request::Render(index, scale)).await?)
    }

    pub async fn add_image(&self, spec: ImageSpec) -> Result<DocInfo, String> {
        info(decode(self.request(Request::AddImage(Box::new(spec))).await?)?)
    }

    pub async fn merge(&self, data: Vec<u8>, index: Option<usize>) -> Result<DocInfo, String> {
        info(decode(self.request(Request::Merge(data, index)).await?)?)
    }

    pub async fn save(&self) -> Result<Vec<u8>, String> {
        bytes(self.request(Request::Save).await?)
    }

    pub async fn extract(&self, pages: Vec<usize>) -> Result<Vec<u8>, String> {
        bytes(self.request(Request::Extract(pages)).await?)
    }
}

/// One unit of work for the engine.
enum Request {
    Open(Vec<u8>),
    Blank(f64, f64),
    Exec(Command),
    Render(usize, f32),
    AddImage(Box<ImageSpec>),
    Merge(Vec<u8>, Option<usize>),
    Save,
    Extract(Vec<usize>),
}

// ---------------------------------------------------------------------------
// Desktop: a thread that owns the editor
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::*;
    use pdf_editor_core::PdfEditor;
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Mutex};

    /// A queued unit of work plus the channel its answer goes back on.
    type Job = (Request, oneshot::Sender<Answer>);

    #[derive(Clone)]
    pub struct Engine {
        tx: Arc<Mutex<Sender<Job>>>,
    }

    impl Engine {
        pub fn new() -> Self {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("pdf-engine".into())
                .spawn(move || {
                    // Built here, so the editor never crosses a thread boundary
                    // and does not need to be Send.
                    let mut editor: Option<PdfEditor> = None;
                    while let Ok((req, reply)) = rx.recv() {
                        let _ = reply.send(handle(&mut editor, req));
                    }
                })
                .expect("spawn engine thread");
            Self { tx: Arc::new(Mutex::new(tx)) }
        }

        pub(super) async fn request(&self, req: Request) -> Answer {
            let (tx, rx) = oneshot::channel();
            self.tx
                .lock()
                .map_err(|_| "engine thread panicked".to_string())?
                .send((req, tx))
                .map_err(|_| "engine thread has stopped".to_string())?;
            rx.await.map_err(|_| "engine dropped the request".to_string())?
        }
    }

    fn handle(slot: &mut Option<PdfEditor>, req: Request) -> Answer {
        let to_json = |r: &Response| serde_json::to_string(r).map_err(|e| e.to_string());

        match req {
            Request::Open(data) => {
                let mut ed = PdfEditor::new(&data).map_err(|e| e.to_string())?;
                let info = ed.info().map_err(|e| e.to_string())?;
                *slot = Some(ed);
                Ok(Reply::Json(to_json(&Response::Info(info))?))
            }
            Request::Blank(w, h) => {
                let mut ed = PdfEditor::blank(w, h).map_err(|e| e.to_string())?;
                let info = ed.info().map_err(|e| e.to_string())?;
                *slot = Some(ed);
                Ok(Reply::Json(to_json(&Response::Info(info))?))
            }
            other => {
                let ed = slot.as_mut().ok_or("no document is open")?;
                match other {
                    Request::Exec(cmd) => {
                        let r = ed.apply(cmd).map_err(|e| e.to_string())?;
                        Ok(Reply::Json(to_json(&r)?))
                    }
                    Request::Render(i, scale) => Ok(Reply::Bytes(
                        ed.render_page_png(i, scale).map_err(|e| e.to_string())?,
                    )),
                    Request::AddImage(spec) => {
                        ed.add_image(&spec).map_err(|e| e.to_string())?;
                        let info = ed.info().map_err(|e| e.to_string())?;
                        Ok(Reply::Json(to_json(&Response::Info(info))?))
                    }
                    Request::Merge(data, at) => {
                        ed.merge(&data, at).map_err(|e| e.to_string())?;
                        let info = ed.info().map_err(|e| e.to_string())?;
                        Ok(Reply::Json(to_json(&Response::Info(info))?))
                    }
                    Request::Save => Ok(Reply::Bytes(ed.save().map_err(|e| e.to_string())?)),
                    Request::Extract(pages) => Ok(Reply::Bytes(
                        ed.extract_pages(&pages).map_err(|e| e.to_string())?,
                    )),
                    Request::Open(_) | Request::Blank(..) => unreachable!("handled above"),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Web: a module Worker hosting the wasm engine
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::*;
    use js_sys::{Object, Reflect, Uint8Array};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{MessageEvent, Worker, WorkerOptions, WorkerType};

    type Pending = Rc<RefCell<HashMap<u32, oneshot::Sender<Answer>>>>;

    struct Inner {
        worker: Worker,
        /// Shared with the message handler: a reply is matched to its request
        /// by id, so this must be the *same* map the handler drains.
        pending: Pending,
        next_id: RefCell<u32>,
        /// Kept alive for as long as the worker is.
        _onmessage: Closure<dyn FnMut(MessageEvent)>,
    }

    #[derive(Clone)]
    pub struct Engine {
        inner: Rc<Inner>,
    }

    impl Engine {
        pub fn new() -> Self {
            let opts = WorkerOptions::new();
            opts.set_type(WorkerType::Module);
            let worker = Worker::new_with_options("./worker.js", &opts)
                .expect("the engine worker failed to start");

            let pending: Pending = Rc::new(RefCell::new(HashMap::new()));
            let routed = pending.clone();
            let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
                let data = e.data();
                let id = Reflect::get(&data, &"id".into())
                    .ok()
                    .and_then(|v| v.as_f64())
                    .unwrap_or(-1.0) as u32;
                let Some(tx) = routed.borrow_mut().remove(&id) else { return };

                let ok = Reflect::get(&data, &"ok".into())
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let result = Reflect::get(&data, &"result".into()).unwrap_or(JsValue::NULL);

                let answer = if ok {
                    if let Some(s) = result.as_string() {
                        Ok(Reply::Json(s))
                    } else {
                        Ok(Reply::Bytes(Uint8Array::new(&result).to_vec()))
                    }
                } else {
                    Err(result.as_string().unwrap_or_else(|| "engine error".into()))
                };
                let _ = tx.send(answer);
            });
            worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

            Self {
                inner: Rc::new(Inner {
                    worker,
                    pending,
                    next_id: RefCell::new(1),
                    _onmessage: onmessage,
                }),
            }
        }

        pub(super) async fn request(&self, req: Request) -> Answer {
            let id = {
                let mut n = self.inner.next_id.borrow_mut();
                let id = *n;
                *n += 1;
                id
            };
            let (tx, rx) = oneshot::channel();
            self.inner.pending.borrow_mut().insert(id, tx);

            let msg = Object::new();
            let set = |k: &str, v: &JsValue| {
                let _ = Reflect::set(&msg, &k.into(), v);
            };
            set("id", &JsValue::from_f64(id as f64));

            match req {
                Request::Open(data) => {
                    set("op", &"open".into());
                    set("data", &Uint8Array::from(&data[..]).into());
                }
                Request::Blank(w, h) => {
                    set("op", &"blank".into());
                    set("width", &JsValue::from_f64(w));
                    set("height", &JsValue::from_f64(h));
                }
                Request::Exec(cmd) => {
                    set("op", &"exec".into());
                    let json = serde_json::to_string(&cmd).map_err(|e| e.to_string())?;
                    set("command", &JsValue::from_str(&json));
                }
                Request::Render(i, scale) => {
                    set("op", &"render".into());
                    set("index", &JsValue::from_f64(i as f64));
                    set("scale", &JsValue::from_f64(scale as f64));
                }
                Request::AddImage(spec) => {
                    set("op", &"addImage".into());
                    set("data", &Uint8Array::from(&spec.rgba[..]).into());
                    set("page", &JsValue::from_f64(spec.page as f64));
                    set("imgWidth", &JsValue::from_f64(spec.img_width as f64));
                    set("imgHeight", &JsValue::from_f64(spec.img_height as f64));
                    set("x", &JsValue::from_f64(spec.x));
                    set("y", &JsValue::from_f64(spec.y));
                    set("width", &JsValue::from_f64(spec.width));
                    set("height", &JsValue::from_f64(spec.height));
                }
                Request::Merge(data, at) => {
                    set("op", &"merge".into());
                    set("data", &Uint8Array::from(&data[..]).into());
                    match at {
                        Some(i) => set("index", &JsValue::from_f64(i as f64)),
                        None => set("index", &JsValue::NULL),
                    }
                }
                Request::Save => set("op", &"save".into()),
                Request::Extract(pages) => {
                    set("op", &"extract".into());
                    let arr = js_sys::Array::new();
                    for p in pages {
                        arr.push(&JsValue::from_f64(p as f64));
                    }
                    set("pages", &arr.into());
                }
            }

            self.inner
                .worker
                .post_message(&msg)
                .map_err(|_| "could not reach the engine worker".to_string())?;
            rx.await.map_err(|_| "the engine worker stopped".to_string())?
        }
    }
}

pub use imp::Engine;

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
