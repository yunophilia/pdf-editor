//! Opening and saving files, which is the other place the two targets differ.
//!
//! Desktop uses native dialogs; web uses a hidden file input for reading and an
//! object URL for writing. Image decoding is deliberately *not* platform
//! specific — the `image` crate compiles to both, so one code path turns any
//! supported file into RGBA.

/// A file the user chose.
pub struct Picked {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Decode an image to raw RGBA, capping the longest side so a phone photo does
/// not become a 50-megapixel XObject.
pub fn decode_image(bytes: &[u8], max_side: u32) -> Result<(Vec<u8>, u32, u32), String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    let img = if w.max(h) > max_side {
        let k = max_side as f32 / w.max(h) as f32;
        img.resize(
            ((w as f32 * k) as u32).max(1),
            ((h as f32 * k) as u32).max(1),
            image::imageops::FilterType::CatmullRom,
        )
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Ok((rgba.into_raw(), w, h))
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use super::Picked;

    /// A document named on the command line, so the app can be launched with a
    /// file (or used as a handler for "Open with").
    pub async fn startup_document() -> Result<Option<Picked>, String> {
        let Some(arg) = std::env::args().nth(1) else { return Ok(None) };
        let path = std::path::PathBuf::from(arg);
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Some(Picked {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "document.pdf".into()),
            bytes,
        }))
    }

    pub async fn pick(title: &str, extensions: &[&str]) -> Option<Picked> {
        let file = rfd::AsyncFileDialog::new()
            .set_title(title)
            .add_filter("Supported", extensions)
            .pick_file()
            .await?;
        Some(Picked {
            name: file.file_name(),
            bytes: file.read().await,
        })
    }

    pub async fn save(bytes: Vec<u8>, suggested: &str) -> Result<Option<String>, String> {
        let Some(handle) = rfd::AsyncFileDialog::new()
            .set_file_name(suggested)
            .add_filter("PDF", &["pdf"])
            .save_file()
            .await
        else {
            return Ok(None);
        };
        handle.write(&bytes).await.map_err(|e| e.to_string())?;
        Ok(Some(handle.file_name()))
    }
}

#[cfg(target_arch = "wasm32")]
mod imp {
    use super::Picked;
    use wasm_bindgen::{JsCast, JsValue};

    /// A document named by `?open=`, so a page can link straight to a PDF.
    ///
    /// Same-origin only: a value carrying a scheme or a protocol-relative
    /// prefix is refused rather than fetched.
    pub async fn startup_document() -> Result<Option<Picked>, String> {
        let window = web_sys::window().ok_or("no window")?;
        let search = window.location().search().map_err(|_| "no location")?;
        let Some(value) = search
            .trim_start_matches('?')
            .split('&')
            .find_map(|pair| pair.strip_prefix("open="))
        else {
            return Ok(None);
        };
        let path = js_sys::decode_uri_component(value)
            .ok()
            .and_then(|s| s.as_string())
            .ok_or("could not decode the ?open= value")?;
        if path.contains("://") || path.starts_with("//") {
            return Err(format!("refusing to fetch {path}: same-origin paths only"));
        }

        let response = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(&path))
            .await
            .map_err(|e| format!("fetching {path} failed: {e:?}"))?
            .dyn_into::<web_sys::Response>()
            .map_err(|_| "unexpected fetch result".to_string())?;
        if !response.ok() {
            return Err(format!("fetching {path} returned {}", response.status()));
        }
        let buffer = wasm_bindgen_futures::JsFuture::from(
            response.array_buffer().map_err(|_| "no response body")?,
        )
        .await
        .map_err(|_| "could not read the response".to_string())?;

        Ok(Some(Picked {
            name: path.rsplit('/').next().unwrap_or("document.pdf").to_string(),
            bytes: js_sys::Uint8Array::new(&buffer).to_vec(),
        }))
    }

    /// The browser's own picker, driven from Rust so the call site matches
    /// desktop. Resolves to `None` if the dialog is dismissed.
    pub async fn pick(_title: &str, extensions: &[&str]) -> Option<Picked> {
        use futures_channel::oneshot;
        use wasm_bindgen::prelude::Closure;

        let document = web_sys::window()?.document()?;
        let input = document
            .create_element("input")
            .ok()?
            .dyn_into::<web_sys::HtmlInputElement>()
            .ok()?;
        input.set_type("file");
        let accept: Vec<String> = extensions.iter().map(|e| format!(".{e}")).collect();
        input.set_accept(&accept.join(","));

        let (tx, rx) = oneshot::channel::<Option<web_sys::File>>();
        let mut tx = Some(tx);
        let cloned = input.clone();
        let onchange = Closure::<dyn FnMut()>::new(move || {
            let file = cloned.files().and_then(|l| l.get(0));
            if let Some(tx) = tx.take() {
                let _ = tx.send(file);
            }
        });
        input.set_onchange(Some(onchange.as_ref().unchecked_ref()));
        input.click();

        let file = rx.await.ok()??;
        drop(onchange);

        let buffer = wasm_bindgen_futures::JsFuture::from(file.array_buffer())
            .await
            .ok()?;
        let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
        Some(Picked {
            name: file.name(),
            bytes,
        })
    }

    /// Hand the bytes to the browser as a download.
    pub async fn save(bytes: Vec<u8>, suggested: &str) -> Result<Option<String>, String> {
        let to_err = |_: JsValue| "could not start the download".to_string();

        let parts = js_sys::Array::new();
        parts.push(&js_sys::Uint8Array::from(&bytes[..]).into());
        let options = web_sys::BlobPropertyBag::new();
        options.set_type("application/pdf");
        let blob =
            web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).map_err(to_err)?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(to_err)?;

        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or("no document")?;
        let anchor = document
            .create_element("a")
            .map_err(to_err)?
            .dyn_into::<web_sys::HtmlAnchorElement>()
            .map_err(|_| "could not create a link".to_string())?;
        anchor.set_href(&url);
        anchor.set_download(suggested);
        anchor.click();
        let _ = web_sys::Url::revoke_object_url(&url);
        Ok(Some(suggested.to_string()))
    }
}

pub use imp::{pick, save, startup_document};
