//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! DeckCraft in an `<iframe>` and keep its files (a webmail's drive, a Nextcloud app). The same
//! messages as WordCraft's and GridCraft's bridges; each is a plain object with a `type`:
//!
//! - `deckcraft:ready` (to the parent): `{ version }`, the app is listening; send a file now.
//! - `deckcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a presentation picked with File › Open or dropped on the window.
//! - `deckcraft:save` (to the parent): `{ name, saveAs, bytes }` (a transferred `ArrayBuffer`) for
//!   every file DeckCraft would otherwise download. The parent stores it and reports failures
//!   itself. `saveAs` says whether the file becomes the presentation's file:
//!   - Save: `false`. A browser has no path to save to, so Save would download a `.deckcraft`
//!     file; in host mode the presentation is sent as `<title>.pptx` instead, the format the
//!     parent opened, and counts as saved.
//!   - Save As to PowerPoint (.pptx) under another name: `true`. The presentation is now that file.
//!   - Exports (PDF, pictures, GIF) and other formats: `false`. They are copies.
//! - `deckcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `deckcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the page
//! starts without a presentation (the parent sends one, or the user starts one from File › New),
//! and the parent guards leaving the page.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use deckcraft_ui_egui::{Services, SlideApp};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// The longest user name taken from `?author=`.
const MAX_AUTHOR_CHARS: usize = 200;

/// A file waiting for the next frame, when the app can be reached.
type Queued = Rc<RefCell<Vec<(String, Vec<u8>)>>>;

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    /// Files the parent sent, opened on the next frame.
    opened: Queued,
    /// Files the app saved or exported, handed to the parent on the next frame.
    saved: Queued,
    /// The unsaved-changes state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to), or when the parent is
    /// on another origin: messages only go to this origin, so the browser would drop every save
    /// without an error while the presentation showed as saved.
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        // An opaque origin (a `file://` page in some browsers) can't be a `postMessage` target.
        if origin == "null" {
            return None;
        }
        // Reading another origin's location throws, which is `None` too.
        if parent.location().origin().ok()? != origin {
            return None;
        }
        Some(Self { window, parent, origin, opened: Rc::default(), saved: Rc::default(), reported: Rc::default() })
    }

    /// Hand saved and exported files to the parent instead of downloading them, and hear the
    /// files it sends.
    pub fn connect(&self, services: &mut Services, ctx: egui::Context) {
        self.listen(ctx.clone());
        let saved = self.saved.clone();
        let download_ctx = ctx.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            saved.borrow_mut().push((file_name(name), bytes.to_vec()));
            download_ctx.request_repaint();
        }));
        let saved = self.saved.clone();
        services.write = Some(Box::new(move |name: &str, bytes: &[u8]| {
            saved.borrow_mut().push((file_name(name), bytes.to_vec()));
            ctx.request_repaint();
            Ok(())
        }));
    }

    /// `?author=`: the host's name for the user signs comments.
    pub fn set_author(app: &mut SlideApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.prefs.author = author;
        }
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "deckcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into())]);
    }

    /// The app couldn't start (no WebGPU or WebGL2, for example): let the parent say so.
    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "deckcraft:failed".into()), ("error", error.into())]);
    }

    /// After each frame: open what the parent sent, hand over what the app saved, and tell the
    /// parent when unsaved changes appear or go away.
    pub fn sync(&self, app: &mut SlideApp) {
        let opened = std::mem::take(&mut *self.opened.borrow_mut());
        for (name, bytes) in opened {
            app.open_or_insert_bytes(&name, &bytes);
        }

        let saved = std::mem::take(&mut *self.saved.borrow_mut());
        for (name, bytes) in saved {
            if let Err(e) = self.hand_over(app, &name, bytes) {
                log::error!("couldn't hand {name} to the page: {e}");
                app.set_status(format!("Couldn't save {name}: {e}"));
            }
        }

        let dirty = app.session.documents().iter().any(|d| d.is_dirty());
        if self.reported.get() != Some(dirty) && self.send(&[("type", "deckcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    /// Send one saved file to the parent, as described in the module docs.
    fn hand_over(&self, app: &mut SlideApp, name: &str, bytes: Vec<u8>) -> Result<(), String> {
        let ext = extension(name);
        if ext == "deckcraft" {
            // Save: the presentation as PowerPoint under its own name.
            let Some(doc) = app.session.active() else { return Err("no presentation is open".into()) };
            let pptx = deckcraft_engine::cmd::file::save_bytes(&doc.doc, "pptx").map_err(|e| e.to_string())?;
            let pptx_name = format!("{}.pptx", doc.title());
            self.post_save(&pptx_name, pptx, false)?;
            mark_saved(app, None);
        } else if ext == "pptx" {
            let current = app.session.active().map(|d| format!("{}.pptx", d.title()));
            let save_as = current.as_deref() != Some(name);
            self.post_save(name, bytes, save_as)?;
            mark_saved(app, save_as.then(|| stem(name)));
        } else {
            self.post_save(name, bytes, false)?;
        }
        Ok(())
    }

    fn post_save(&self, name: &str, bytes: Vec<u8>, save_as: bool) -> Result<(), String> {
        let buffer = js_sys::Uint8Array::from(bytes.as_slice()).buffer();
        let message = message(&[
            ("type", "deckcraft:save".into()),
            ("name", name.into()),
            ("saveAs", save_as.into()),
            ("bytes", buffer.clone().into()),
        ])
        .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
    }

    /// Hear `deckcraft:open` from the parent and queue the file for the next frame.
    fn listen(&self, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let opened = self.opened.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            let data = event.data();
            if string(&data, "type").as_deref() != Some("deckcraft:open") {
                return;
            }
            let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Presentation.pptx".to_string());
            let Some(bytes) = bytes(&data) else { return };
            opened.borrow_mut().push((name, bytes));
            ctx.request_repaint();
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

/// The active presentation is saved: it is no longer dirty, and after Save As it carries the
/// new file's name.
fn mark_saved(app: &mut SlideApp, title: Option<String>) {
    if let Some(doc) = app.session.active_mut() {
        doc.saved_doc = doc.doc.clone();
        doc.revision += 1;
        if let Some(title) = title {
            doc.title = title;
        }
    }
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

fn stem(name: &str) -> String {
    std::path::Path::new(name).file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| name.to_string())
}

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default()
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}
