//! Cloudflare-Workers-parity `HTMLRewriter`, bound to lol-html 3.0.0.
//!
//! # Architecture
//!
//! This exposes the LOW-LEVEL streaming engine to JS; the fluent CF
//! `.on()/.onDocument()/.transform()` wrapper is layered in JS on top of it.
//!
//! Everything runs SYNCHRONOUSLY on the JS thread. JS calls
//! [`HtmlRewriterEngine::write`] / [`HtmlRewriterEngine::end`]; lol-html invokes
//! the registered element/text/comment/doctype handlers INLINE during that call,
//! and each handler is dispatched straight back into the user's JS `Function`
//! synchronously (we are already on the JS thread, so `Function::call` is valid).
//! Rewritten bytes are pushed to the JS `output` callback the moment lol-html
//! produces them — the engine never buffers the whole document.
//!
//! # The lifetime problem
//!
//! lol-html hands a handler `&mut Element<'_, '_, _>` whose lifetime is bound to
//! the handler call; napi class instances must be `'static`. We use the proven
//! lol-html-js-api pattern (see cloudflare/lol-html `js-api/`, mirrored by
//! remorses/htmlrewriter): wrap a raw `*mut R` in [`NativeRefWrap`] guarded by a
//! shared "poisoned" [`Cell`]. The handler stashes the pointer into a napi
//! wrapper object, calls JS, then drops an [`Anchor`] whose `Drop` poisons the
//! cell — so any token the JS handler stored and tried to use after returning
//! fails loudly instead of dereferencing freed memory. Because the whole dance
//! is synchronous + single-threaded, the pointer is valid for exactly the JS
//! call's duration.
//!
//! # Follow-up: async handlers
//!
//! FIRST CUT supports SYNCHRONOUS JS handlers only. If a handler returns a
//! Promise we do NOT await it (the token would be poisoned by the time the
//! Promise resolved); we proceed immediately. Awaiting requires suspending the
//! lol-html write loop across an async boundary (lol-html's wasm js-api solves
//! this with binaryen asyncify) — tracked as future work.

use std::cell::Cell;
use std::rc::Rc;

use lol_html::html_content::{
    Comment as NativeComment, ContentType, Doctype as NativeDoctype,
    DocumentEnd as NativeDocumentEnd, Element as NativeElement, EndTag as NativeEndTag,
    TextChunk as NativeTextChunk,
};
use lol_html::{
    DocumentContentHandlers as NativeDocumentContentHandlers,
    ElementContentHandlers as NativeElementContentHandlers, HtmlRewriter as NativeHtmlRewriter,
    OutputSink, Selector, Settings,
};
use napi::bindgen_prelude::*;
use napi::sys;
use napi::{Env, JsValue};
use napi_derive::napi;

/// Uniform type for every stored JS handler / sink callback: an opaque
/// `(Unknown) -> Unknown` reference. Using one concrete type keeps the closures
/// and the engine fields monomorphic (the wrapper instance is passed as
/// `Unknown`, the return value is ignored).
type JsRef = FunctionRef<Unknown<'static>, Unknown<'static>>;

/// Shared cell holding the current JS `napi_env`. The handler closures and the
/// output sink are built at registration time but invoked synchronously during
/// `write()`/`end()`; they read the live env from here. The pointer is captured
/// at construction and refreshed on every write/end (it is stable on the main
/// JS thread, but refreshing costs nothing and is robust).
type EnvCell = Rc<Cell<sys::napi_env>>;

// ---------------------------------------------------------------------------
// Lifetime erasure: NativeRefWrap + Anchor (poison-on-drop)
// ---------------------------------------------------------------------------

/// Runtime guard for a [`NativeRefWrap`]. Dropping it poisons the shared cell,
/// invalidating any wrapper token the JS handler kept past its own call.
struct Anchor {
    poisoned: Rc<Cell<bool>>,
}

impl Drop for Anchor {
    fn drop(&mut self) {
        self.poisoned.set(true);
    }
}

/// A lifetime-erased `*mut R` valid only while its [`Anchor`] is alive.
///
/// `R` is the lol-html unit (`Element`, `TextChunk`, …) with its lifetimes
/// transmuted to `'static`. SAFETY rests entirely on the synchronous,
/// single-threaded handler dispatch + the poison cell: `get`/`get_mut` refuse
/// once poisoned, so a token outliving its handler call cannot deref freed data.
struct NativeRefWrap<R> {
    inner: *mut R,
    poisoned: Rc<Cell<bool>>,
}

impl<R> NativeRefWrap<R> {
    /// Wrap a borrowed lol-html unit, returning the erased pointer + its anchor.
    fn wrap<I>(inner: &mut I) -> (Self, Anchor) {
        let poisoned = Rc::new(Cell::new(false));
        // Erase the concrete (`I` = lol-html unit at its real lifetimes) into the
        // `'static` `R` the napi wrapper class is parameterized on. Sound only
        // because the wrap is invalidated (poisoned) when the anchor drops, before
        // the real borrow ends.
        let inner = (inner as *mut I).cast::<R>();
        (
            Self {
                inner,
                poisoned: Rc::clone(&poisoned),
            },
            Anchor { poisoned },
        )
    }

    fn check(&self) -> Result<()> {
        if self.poisoned.get() {
            Err(Error::new(
                Status::GenericFailure,
                "This content token is no longer valid. Content tokens are only valid during the \
                 execution of the relevant content handler.",
            ))
        } else {
            Ok(())
        }
    }

    fn get(&self) -> Result<&R> {
        self.check()?;
        // SAFETY: not poisoned ⇒ the original borrow is live and exclusive (we are
        // synchronously inside the handler call that produced it).
        Ok(unsafe { &*self.inner })
    }

    #[allow(clippy::mut_from_ref)]
    fn get_mut(&self) -> Result<&mut R> {
        self.check()?;
        // SAFETY: see `get`. lol-html units are accessed single-threaded and only
        // one wrapper points at a given unit at a time.
        Ok(unsafe { &mut *self.inner })
    }
}

/// `'static`-erased aliases for the lol-html units the wrappers point at.
type StaticElement = NativeElement<'static, 'static>;
type StaticEndTag = NativeEndTag<'static>;
type StaticTextChunk = NativeTextChunk<'static>;
type StaticComment = NativeComment<'static>;
type StaticDoctype = NativeDoctype<'static>;
type StaticDocumentEnd = NativeDocumentEnd<'static>;

// ---------------------------------------------------------------------------
// ContentType option bridge
// ---------------------------------------------------------------------------

/// The optional `{ html?: boolean }` trailing arg on mutation methods.
#[napi(object)]
pub struct ContentTypeOptions {
    pub html: Option<bool>,
}

fn content_type(opts: Option<ContentTypeOptions>) -> ContentType {
    match opts.and_then(|o| o.html) {
        Some(true) => ContentType::Html,
        _ => ContentType::Text,
    }
}

fn map_err<E: std::fmt::Display>(e: E) -> Error {
    Error::new(Status::GenericFailure, e.to_string())
}

// ---------------------------------------------------------------------------
// Wrapper classes (the JS-facing Element / TextChunk / Comment / …)
// ---------------------------------------------------------------------------

/// JS-facing wrapper for a matched element. Methods deref the stashed pointer
/// (erroring if the token is used past its handler call).
#[napi]
pub struct Element {
    wrap: NativeRefWrap<StaticElement>,
    /// Shared live-env source, forwarded to any `on_end_tag` handler.
    env_cell: EnvCell,
}

#[napi]
impl Element {
    #[napi(getter)]
    pub fn tag_name(&self) -> Result<String> {
        Ok(self.wrap.get()?.tag_name())
    }

    #[napi(setter)]
    pub fn set_tag_name(&mut self, name: String) -> Result<()> {
        self.wrap.get_mut()?.set_tag_name(&name).map_err(map_err)
    }

    /// The original-case tag name (lol-html `tag_name_preserve_case`).
    #[napi(getter)]
    pub fn tag_name_preserve_case(&self) -> Result<String> {
        Ok(self.wrap.get()?.tag_name_preserve_case())
    }

    #[napi(getter)]
    pub fn namespace_uri(&self) -> Result<String> {
        Ok(self.wrap.get()?.namespace_uri().to_string())
    }

    /// All attributes as `[name, value]` pairs (matches the CF `attributes`
    /// iterable shape, surfaced to JS as an array of 2-tuples).
    #[napi(getter)]
    pub fn attributes(&self) -> Result<Vec<Vec<String>>> {
        Ok(self
            .wrap
            .get()?
            .attributes()
            .iter()
            .map(|a| vec![a.name(), a.value()])
            .collect())
    }

    #[napi]
    pub fn get_attribute(&self, name: String) -> Result<Option<String>> {
        Ok(self.wrap.get()?.get_attribute(&name))
    }

    #[napi]
    pub fn has_attribute(&self, name: String) -> Result<bool> {
        Ok(self.wrap.get()?.has_attribute(&name))
    }

    #[napi]
    pub fn set_attribute(&mut self, name: String, value: String) -> Result<()> {
        self.wrap
            .get_mut()?
            .set_attribute(&name, &value)
            .map_err(map_err)
    }

    #[napi]
    pub fn remove_attribute(&mut self, name: String) -> Result<()> {
        self.wrap.get_mut()?.remove_attribute(&name);
        Ok(())
    }

    #[napi]
    pub fn before(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.before(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn after(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.after(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn prepend(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap
            .get_mut()?
            .prepend(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn append(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.append(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn set_inner_content(
        &mut self,
        content: String,
        options: Option<ContentTypeOptions>,
    ) -> Result<()> {
        self.wrap
            .get_mut()?
            .set_inner_content(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn replace(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap
            .get_mut()?
            .replace(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn remove(&mut self) -> Result<()> {
        self.wrap.get_mut()?.remove();
        Ok(())
    }

    #[napi]
    pub fn remove_and_keep_content(&mut self) -> Result<()> {
        self.wrap.get_mut()?.remove_and_keep_content();
        Ok(())
    }

    #[napi(getter)]
    pub fn removed(&self) -> Result<bool> {
        Ok(self.wrap.get()?.removed())
    }

    /// Register an end-tag handler. The handler fires synchronously when the
    /// matching end tag is reached, receiving an [`EndTag`] token.
    ///
    /// The end-tag handler shares this `Element` token's poison cell as its env
    /// source: it fires later (at the end tag), still synchronously on the JS
    /// thread, so it reads the live env from the same shared cell captured when
    /// the engine started.
    #[napi]
    pub fn on_end_tag(
        &mut self,
        handler: FunctionRef<Unknown<'static>, Unknown<'static>>,
    ) -> Result<()> {
        let func = handler;
        let env_cell = Rc::clone(&self.env_cell);
        self.wrap
            .get_mut()?
            .on_end_tag(Box::new(move |end_tag: &mut NativeEndTag<'_>| {
                dispatch::<StaticEndTag, EndTag>(&func, &env_cell, end_tag, |wrap| EndTag { wrap })
            }))
            .map_err(map_err)?;
        Ok(())
    }
}

/// JS-facing wrapper for an end tag (from [`Element::on_end_tag`]).
#[napi]
pub struct EndTag {
    wrap: NativeRefWrap<StaticEndTag>,
}

#[napi]
impl EndTag {
    #[napi(getter)]
    pub fn name(&self) -> Result<String> {
        Ok(self.wrap.get()?.name())
    }

    #[napi(setter)]
    pub fn set_name(&mut self, name: String) -> Result<()> {
        self.wrap.get_mut()?.set_name_str(name);
        Ok(())
    }

    #[napi]
    pub fn before(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.before(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn after(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.after(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn replace(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap
            .get_mut()?
            .replace(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn remove(&mut self) -> Result<()> {
        self.wrap.get_mut()?.remove();
        Ok(())
    }

    #[napi(getter)]
    pub fn removed(&self) -> Result<bool> {
        Ok(self.wrap.get()?.removed())
    }
}

/// JS-facing wrapper for a text-node fragment.
#[napi]
pub struct TextChunk {
    wrap: NativeRefWrap<StaticTextChunk>,
}

#[napi]
impl TextChunk {
    #[napi(getter)]
    pub fn text(&self) -> Result<String> {
        Ok(self.wrap.get()?.as_str().to_string())
    }

    #[napi(getter)]
    pub fn last_in_text_node(&self) -> Result<bool> {
        Ok(self.wrap.get()?.last_in_text_node())
    }

    #[napi]
    pub fn before(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.before(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn after(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.after(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn replace(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap
            .get_mut()?
            .replace(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn remove(&mut self) -> Result<()> {
        self.wrap.get_mut()?.remove();
        Ok(())
    }

    #[napi(getter)]
    pub fn removed(&self) -> Result<bool> {
        Ok(self.wrap.get()?.removed())
    }
}

/// JS-facing wrapper for an HTML comment.
#[napi]
pub struct Comment {
    wrap: NativeRefWrap<StaticComment>,
}

#[napi]
impl Comment {
    #[napi(getter)]
    pub fn text(&self) -> Result<String> {
        Ok(self.wrap.get()?.text())
    }

    #[napi(setter)]
    pub fn set_text(&mut self, text: String) -> Result<()> {
        self.wrap.get_mut()?.set_text(&text).map_err(map_err)
    }

    #[napi]
    pub fn before(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.before(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn after(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.after(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn replace(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap
            .get_mut()?
            .replace(&content, content_type(options));
        Ok(())
    }

    #[napi]
    pub fn remove(&mut self) -> Result<()> {
        self.wrap.get_mut()?.remove();
        Ok(())
    }

    #[napi(getter)]
    pub fn removed(&self) -> Result<bool> {
        Ok(self.wrap.get()?.removed())
    }
}

/// JS-facing wrapper for the document-type declaration (read-only).
#[napi]
pub struct Doctype {
    wrap: NativeRefWrap<StaticDoctype>,
}

#[napi]
impl Doctype {
    #[napi(getter)]
    pub fn name(&self) -> Result<Option<String>> {
        Ok(self.wrap.get()?.name())
    }

    #[napi(getter)]
    pub fn public_id(&self) -> Result<Option<String>> {
        Ok(self.wrap.get()?.public_id())
    }

    #[napi(getter)]
    pub fn system_id(&self) -> Result<Option<String>> {
        Ok(self.wrap.get()?.system_id())
    }
}

/// JS-facing wrapper for the document-end token (the `end` document handler).
#[napi]
pub struct DocumentEnd {
    wrap: NativeRefWrap<StaticDocumentEnd>,
}

#[napi]
impl DocumentEnd {
    #[napi]
    pub fn append(&mut self, content: String, options: Option<ContentTypeOptions>) -> Result<()> {
        self.wrap.get_mut()?.append(&content, content_type(options));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Handler dispatch
// ---------------------------------------------------------------------------

/// Synchronously dispatch one lol-html unit into a stored JS handler.
///
/// Wraps `unit` in a poison-guarded pointer, builds the napi wrapper object,
/// reads the live [`Env`] from the shared cell (we are on the JS thread), calls
/// the handler with the wrapper as its sole argument, then drops the anchor to
/// poison the token.
///
/// FIRST CUT: a returned Promise is NOT awaited (see the module-level note).
fn dispatch<S, W>(
    func: &JsRef,
    env_cell: &EnvCell,
    unit: &mut impl Sized,
    build: impl FnOnce(NativeRefWrap<S>) -> W,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync + 'static>>
where
    W: JavaScriptClassExt + 'static,
{
    let env = Env::from_raw(env_cell.get());
    let (wrap, anchor) = NativeRefWrap::<S>::wrap(unit);
    let result = (|| -> Result<()> {
        let instance = build(wrap).into_instance(&env)?;
        // Re-read the wrapper instance as an opaque `Unknown` so it matches the
        // uniform `JsRef` arg type.
        let arg = unsafe { Unknown::from_napi_value(env.raw(), instance.value().value)? };
        let f = func.borrow_back(&env)?;
        f.call(arg)?;
        Ok(())
    })();
    // Invalidate the token regardless of the handler outcome.
    drop(anchor);
    result.map_err(|e| Box::new(HandlerError(e)) as Box<dyn std::error::Error + Send + Sync>)
}

/// Carries a napi [`Error`] from a JS handler back out through lol-html's
/// `RewritingError::ContentHandlerError` so `write`/`end` can re-raise it.
#[derive(Debug)]
struct HandlerError(Error);

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.reason)
    }
}

impl std::error::Error for HandlerError {}

// ---------------------------------------------------------------------------
// Output sink
// ---------------------------------------------------------------------------

/// Pushes each rewritten chunk straight to the JS `output` callback as a
/// `Buffer`, synchronously, the moment lol-html produces it.
struct JsOutputSink {
    func: FunctionRef<Buffer, Unknown<'static>>,
    env_cell: EnvCell,
}

impl OutputSink for JsOutputSink {
    fn handle_chunk(&mut self, chunk: &[u8]) {
        // Invoked synchronously on the JS thread during write()/end().
        let env = Env::from_raw(self.env_cell.get());
        if let Ok(f) = self.func.borrow_back(&env) {
            // The JS wrapper surfaces sink errors; a failed call here cannot abort
            // lol-html's write loop (OutputSink is infallible), so it is dropped.
            let _ = f.call(Buffer::from(chunk.to_vec()));
        }
    }
}

// ---------------------------------------------------------------------------
// Handler-bundle argument objects
// ---------------------------------------------------------------------------

/// Pull an optional handler `Function` off a JS object and reference it as the
/// uniform [`JsRef`].
fn take_handler(obj: &Object, key: &str) -> Result<Option<JsRef>> {
    match obj.get::<Function<Unknown<'static>, Unknown<'static>>>(key)? {
        Some(f) => Ok(Some(f.create_ref()?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// Low-level streaming HTMLRewriter engine.
///
/// Handlers are registered with [`on`](HtmlRewriterEngine::on) /
/// [`on_document`](HtmlRewriterEngine::on_document) BEFORE the first
/// [`write`](HtmlRewriterEngine::write); the underlying lol-html rewriter is
/// built lazily on first write (lol-html consumes its `Settings` by value), so
/// registration after the first write is rejected.
#[napi]
pub struct HtmlRewriterEngine {
    output: Option<FunctionRef<Buffer, Unknown<'static>>>,
    selectors: Vec<Selector>,
    element_handlers: Vec<NativeElementContentHandlers<'static>>,
    document_handlers: Vec<NativeDocumentContentHandlers<'static>>,
    inner: Option<NativeHtmlRewriter<'static, JsOutputSink>>,
    started: bool,
    env_cell: EnvCell,
    /// Re-entrancy guard. lol-html drives the JS handlers synchronously from
    /// inside `write`/`end`; a handler that calls `write`/`end` AGAIN on the SAME
    /// instance would alias `&mut self` (napi-rs 3 has no borrow guard) and re-enter
    /// lol-html on an already-borrowed rewriter — reachable aliasing UB from safe
    /// user JS. The flag is set for the duration of `write`/`end` and cleared by an
    /// RAII guard (so it also clears on unwind). Cross-INSTANCE re-entrancy (a
    /// handler of engine A driving engine B) keeps working — this is per-instance.
    active: Rc<Cell<bool>>,
}

/// RAII flag-clearer for the engine's re-entrancy guard: clears `active` on drop
/// so the flag is released whether `write`/`end` returns normally or unwinds.
struct ActiveGuard(Rc<Cell<bool>>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

#[napi]
impl HtmlRewriterEngine {
    /// `output` is called synchronously with each rewritten `Buffer` chunk.
    #[napi(constructor)]
    pub fn new(env: &Env, output: FunctionRef<Buffer, Unknown<'static>>) -> Result<Self> {
        Ok(Self {
            output: Some(output),
            selectors: Vec::new(),
            element_handlers: Vec::new(),
            document_handlers: Vec::new(),
            inner: None,
            started: false,
            env_cell: Rc::new(Cell::new(env.raw())),
            active: Rc::new(Cell::new(false)),
        })
    }

    /// Acquire the per-instance re-entrancy guard, or return a clean JS error if a
    /// `write`/`end` is already in progress on this instance (i.e. a handler tried
    /// to drive the same engine re-entrantly). Returns the RAII guard that releases
    /// the flag on drop.
    fn enter(&self) -> Result<ActiveGuard> {
        if self.active.get() {
            return Err(Error::new(
                Status::GenericFailure,
                "HTMLRewriter is already processing; cannot write() or transform() \
                 re-entrantly from within a handler on the same instance.",
            ));
        }
        self.active.set(true);
        Ok(ActiveGuard(Rc::clone(&self.active)))
    }

    fn ensure_not_started(&self) -> Result<()> {
        if self.started {
            Err(Error::new(
                Status::GenericFailure,
                "Handlers cannot be added after the first write().",
            ))
        } else {
            Ok(())
        }
    }

    /// Register element/comment/text handlers for a CSS selector.
    ///
    /// `handlers` is `{ element?, comments?, text? }` of JS Functions.
    #[napi]
    pub fn on(&mut self, selector: String, handlers: Object) -> Result<()> {
        self.ensure_not_started()?;
        let parsed = selector.parse::<Selector>().map_err(map_err)?;

        let element = take_handler(&handlers, "element")?;
        let comments = take_handler(&handlers, "comments")?;
        let text = take_handler(&handlers, "text")?;

        let mut h = NativeElementContentHandlers::default();
        if let Some(func) = element {
            let env_cell = Rc::clone(&self.env_cell);
            h = h.element(move |el: &mut NativeElement<'_, '_>| {
                let ec = Rc::clone(&env_cell);
                dispatch::<StaticElement, Element>(&func, &env_cell, el, move |wrap| Element {
                    wrap,
                    env_cell: ec,
                })
            });
        }
        if let Some(func) = comments {
            let env_cell = Rc::clone(&self.env_cell);
            h = h.comments(move |c: &mut NativeComment<'_>| {
                dispatch::<StaticComment, Comment>(&func, &env_cell, c, |wrap| Comment { wrap })
            });
        }
        if let Some(func) = text {
            let env_cell = Rc::clone(&self.env_cell);
            h = h.text(move |t: &mut NativeTextChunk<'_>| {
                dispatch::<StaticTextChunk, TextChunk>(&func, &env_cell, t, |wrap| TextChunk {
                    wrap,
                })
            });
        }

        self.selectors.push(parsed);
        self.element_handlers.push(h);
        Ok(())
    }

    /// Register document-level handlers.
    ///
    /// `handlers` is `{ doctype?, comments?, text?, end? }` of JS Functions.
    #[napi]
    pub fn on_document(&mut self, handlers: Object) -> Result<()> {
        self.ensure_not_started()?;

        let doctype = take_handler(&handlers, "doctype")?;
        let comments = take_handler(&handlers, "comments")?;
        let text = take_handler(&handlers, "text")?;
        let end = take_handler(&handlers, "end")?;

        let mut h = NativeDocumentContentHandlers::default();
        if let Some(func) = doctype {
            let env_cell = Rc::clone(&self.env_cell);
            h = h.doctype(move |d: &mut NativeDoctype<'_>| {
                dispatch::<StaticDoctype, Doctype>(&func, &env_cell, d, |wrap| Doctype { wrap })
            });
        }
        if let Some(func) = comments {
            let env_cell = Rc::clone(&self.env_cell);
            h = h.comments(move |c: &mut NativeComment<'_>| {
                dispatch::<StaticComment, Comment>(&func, &env_cell, c, |wrap| Comment { wrap })
            });
        }
        if let Some(func) = text {
            let env_cell = Rc::clone(&self.env_cell);
            h = h.text(move |t: &mut NativeTextChunk<'_>| {
                dispatch::<StaticTextChunk, TextChunk>(&func, &env_cell, t, |wrap| TextChunk {
                    wrap,
                })
            });
        }
        if let Some(func) = end {
            let env_cell = Rc::clone(&self.env_cell);
            h = h.end(move |e: &mut NativeDocumentEnd<'_>| {
                dispatch::<StaticDocumentEnd, DocumentEnd>(&func, &env_cell, e, |wrap| {
                    DocumentEnd { wrap }
                })
            });
        }

        self.document_handlers.push(h);
        Ok(())
    }

    /// Build the lol-html rewriter on first write (its `Settings` are consumed
    /// by value), then feed `chunk` through it.
    #[napi]
    pub fn write(&mut self, env: &Env, chunk: Buffer) -> Result<()> {
        let _guard = self.enter()?;
        self.env_cell.set(env.raw());
        self.ensure_started()?;
        // `inner` is Some after `ensure_started`.
        self.inner
            .as_mut()
            .unwrap()
            .write(chunk.as_ref())
            .map_err(rewriting_error)
    }

    /// Flush remaining output and finalize. The engine cannot be reused after.
    #[napi]
    pub fn end(&mut self, env: &Env) -> Result<()> {
        let _guard = self.enter()?;
        self.env_cell.set(env.raw());
        self.ensure_started()?;
        self.inner.take().unwrap().end().map_err(rewriting_error)
    }

    /// Lazily construct the lol-html rewriter from the registered handlers.
    fn ensure_started(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        let output = self
            .output
            .take()
            .ok_or_else(|| Error::new(Status::GenericFailure, "HTMLRewriter already finalized."))?;

        let mut settings = Settings::new();
        let selectors = std::mem::take(&mut self.selectors);
        let element_handlers = std::mem::take(&mut self.element_handlers);
        for (selector, handlers) in selectors.into_iter().zip(element_handlers) {
            settings = settings
                .append_element_content_handler((std::borrow::Cow::Owned(selector), handlers));
        }
        for handlers in std::mem::take(&mut self.document_handlers) {
            settings = settings.append_document_content_handler(handlers);
        }

        self.inner = Some(NativeHtmlRewriter::new(
            settings,
            JsOutputSink {
                func: output,
                env_cell: Rc::clone(&self.env_cell),
            },
        ));
        self.started = true;
        Ok(())
    }
}

/// Surface a lol-html error to JS, unwrapping a JS-handler error back to its
/// original napi [`Error`] where possible.
fn rewriting_error(err: lol_html::errors::RewritingError) -> Error {
    use lol_html::errors::RewritingError;
    match err {
        RewritingError::ContentHandlerError(boxed) => match boxed.downcast::<HandlerError>() {
            Ok(h) => h.0,
            Err(other) => Error::new(Status::GenericFailure, other.to_string()),
        },
        other => Error::new(Status::GenericFailure, other.to_string()),
    }
}
