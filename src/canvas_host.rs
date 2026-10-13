//! Private Canvas bindings. The prelude consumes and deletes the bootstrap hooks.
//!
//! `__canvas_2d` addresses canvas elements by node. `__offscreen_canvas` is
//! installed in Window and worker Realms: it serves the Realm-neutral canvas
//! operations (gradient/metric brands, colors, patterns) and OffscreenCanvas
//! objects by id (HTML #the-offscreencanvas-interface). Both share one
//! implementation of the 2D drawing operations.
use super::{
    Ctx, HostState, Value, host_arg_node, host_arg_string, host_dom, host_layout_environment,
};
use crate::canvas::{Canvas, Sizing};
use crate::canvas_offscreen::{Mode, Offscreen};
use crate::dom::Dom;
use resvg::tiny_skia as sk;
use std::cell::RefCell;
use std::rc::Rc;
use vello_cpu::kurbo::Affine;

/// One binding call: the operation, its numeric arguments and its payload.
struct Request {
    op: String,
    n: Vec<f64>,
    payload: Value,
    text: String,
}

impl Request {
    fn read(ctx: &mut Ctx, args: &[Value]) -> Result<Self, Value> {
        let op = host_arg_string(ctx, args, 1);
        let mut n = Vec::new();
        if let Some(values) = args.get(2) {
            let length = ctx.member_get(values, "length")?.as_num_opt().unwrap_or(0.) as usize;
            if length > 65536 {
                return Err(ctx.make_error("RangeError", "Canvas argument list too large"));
            }
            for index in 0..length {
                let v = ctx.member_get(values, &index.to_string())?;
                n.push(ctx.coerce_number(&v)?);
            }
        }
        let payload = args.get(3).cloned().unwrap_or(Value::Undefined);
        let text = payload.as_text().unwrap_or_default().into_owned();
        Ok(Self {
            op,
            n,
            payload,
            text,
        })
    }
}

/// Operations that address no canvas.
fn realm_op(ctx: &mut Ctx, req: &Request) -> Result<Option<Value>, Value> {
    let op = req.op.as_str();
    if op == "gradientSlots" || op == "textMetricsSlots" {
        // Shared, privately rooted WeakMap: cross-Realm CanvasGradient brands
        // work without exposing data or keeping otherwise dead gradients alive.
        let state = ctx.host_mut::<HostState>().unwrap();
        let slots = if op == "gradientSlots" {
            &mut state.canvas_gradient_slots
        } else {
            &mut state.canvas_text_metrics_slots
        };
        return Ok(Some(slots.get_or_insert(req.payload.clone()).clone()));
    }
    // HTML #dom-canvaspattern-settransform and pattern release: patterns are
    // not tied to one canvas.
    if op == "patternTransform" || op == "freePattern" {
        let state = ctx.host_mut::<HostState>().unwrap();
        let n = &req.n;
        let id = n.first().copied().unwrap_or(0.) as u32;
        if op == "freePattern" {
            state.canvas_patterns.remove(&id);
        } else if n.len() >= 7
            && let Some(pattern) = state.canvas_patterns.get(&id)
        {
            pattern.set_matrix(Affine::new([n[1], n[2], n[3], n[4], n[5], n[6]]));
        }
        return Ok(Some(Value::Undefined));
    }
    if op == "color" {
        return Ok(Some(
            Canvas::color(&req.text).map_or(Value::Null, |(color, _)| {
                ctx.make_array(color.into_iter().map(|n| Value::Num(n as f64)).collect())
            }),
        ));
    }
    Ok(None)
}

/// The font style source object of a 2D context (HTML
/// #canvastextdrawingstyles): a canvas element, or an OffscreenCanvas with
/// its inherited language and direction and its Realm's Document, if any.
struct Environment<'a> {
    dom: &'a Dom,
    element: Option<usize>,
    document: Option<usize>,
    inherited: (Option<String>, bool),
}

impl<'a> Environment<'a> {
    fn element(dom: &'a Dom, id: usize) -> Self {
        Self {
            dom,
            element: Some(id),
            document: Some(id),
            inherited: (None, false),
        }
    }

    fn offscreen(dom: &'a Dom, offscreen: &Offscreen) -> Self {
        Self {
            dom,
            element: None,
            document: offscreen.document,
            inherited: (offscreen.language.clone(), offscreen.rtl),
        }
    }

    /// HTML #text-preparation-algorithm "inherit" sources and the unit
    /// environment of letter and word spacing.
    fn text(&self) -> crate::canvas_text::TextEnvironment {
        let (rtl, language) = match self.element {
            Some(id) => (
                self.dom.html_directionality_rtl(id),
                self.dom.inherited_lang(id).map(str::to_owned),
            ),
            None => (self.inherited.1, self.inherited.0.clone()),
        };
        crate::canvas_text::TextEnvironment {
            rtl,
            language,
            root_font_size: self.dom.root_font_px(),
            viewport: self.dom.viewport_px(),
        }
    }

    /// The computed font-size and font-weight relative font values resolve
    /// against: the element's, when it is being rendered, else the normal
    /// weight 10px sans-serif default (HTML #dom-context-2d-font).
    fn font_units(&self) -> (crate::layout2::Units, f32) {
        let dom = self.dom;
        let rendered = self.element.is_some_and(|id| {
            let mut rendered = dom.is_connected(id);
            let mut ancestor = Some(id);
            while rendered && let Some(node) = ancestor {
                if dom.computed_value_resolved(node, "display").as_deref() == Some("none") {
                    rendered = false;
                }
                ancestor = dom.parent_flat(node);
            }
            rendered
        });
        match self.element {
            Some(id) if rendered => (
                crate::layout2::Units::of(dom, id),
                dom.computed_value_resolved(id, "font-weight")
                    .as_deref()
                    .and_then(crate::layout2::css_font_weight)
                    .unwrap_or(400.),
            ),
            _ => (
                crate::layout2::Units {
                    fs: 10.,
                    root: dom.root_font_px(),
                    ch: crate::text::shape_canvas(
                        "0",
                        &crate::text::TextStyle {
                            size: 10.,
                            ..crate::text::TextStyle::default()
                        },
                    )
                    .advance,
                },
                400.,
            ),
        }
    }

    /// CSS Color 4 #currentcolor-color: the canvas element's used color
    /// (inherited currentcolor included); black without an element.
    fn current_color(&self) -> String {
        let mut current = self.element;
        while let Some(node) = current {
            if let Some(value) = self.dom.computed_value_resolved(node, "color")
                && !value.trim().eq_ignore_ascii_case("currentcolor")
            {
                return value;
            }
            current = self.dom.parent_flat(node);
        }
        "black".to_owned()
    }

    /// HTML #dom-context-2d-filter: resolve the current filter's references
    /// for a drawing operation. A reference to a missing or non-filter
    /// element makes the whole chain ignored (Filter Effects 1
    /// #FilterProperty); an external document that is not loaded means "no
    /// filtering". Either way the operation draws unfiltered.
    fn filter_chain(
        &self,
        filter: &crate::canvas_filter::Filter,
        base: &url::Url,
    ) -> Option<Vec<crate::canvas_filter::Resolved>> {
        use crate::canvas_filter::{Resolved, Step};
        let mut chain = Vec::with_capacity(filter.steps.len());
        for step in &filter.steps {
            match step {
                Step::Function(function) => chain.push(Resolved::Function(function.clone())),
                Step::Reference(reference) => {
                    let (fragment, external) = if let Some(fragment) = reference.strip_prefix('#') {
                        (fragment.to_owned(), None)
                    } else {
                        let url = base.join(reference).ok()?;
                        let mut without = url.clone();
                        without.set_fragment(None);
                        let mut document = base.clone();
                        document.set_fragment(None);
                        let fragment = url.fragment()?.to_owned();
                        (fragment, (without != document).then_some(url))
                    };
                    let fragment = percent_decoded(&fragment)?;
                    let markup = match external {
                        None => self.dom.canvas_svg_filter(self.document?, &fragment)?,
                        // A filter in an external resource document, loaded
                        // as for CSS references (`dom::svg_external`); the
                        // drawing proceeds unfiltered until it has loaded.
                        Some(url) => {
                            let node = self.element.or(self.document)?;
                            let crate::dom::ExternalSvg::Loaded(document) =
                                self.dom.external_svg_document(node, &url, base)
                            else {
                                return None;
                            };
                            document
                                .dom
                                .canvas_svg_filter(document.dom.document(), &fragment)?
                        }
                    };
                    chain.push(Resolved::Svg {
                        markup,
                        id: fragment,
                    });
                }
            }
        }
        Some(chain)
    }
}

/// HTML #find-a-potential-indicated-element: the percent-decoded fragment.
fn percent_decoded(fragment: &str) -> Option<String> {
    let bytes = fragment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(Some(high)), Some(Some(low))) = (
                bytes.get(i + 1).map(|b| hex(*b)),
                bytes.get(i + 2).map(|b| hex(*b)),
            )
        {
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Decode a private image record `[width, height, pixels, clean, _,
/// premultiplied]` (a loaded image or an ImageBitmap) into a bitmap.
fn record_bitmap(ctx: &mut Ctx, record: &Value) -> Result<Option<(sk::Pixmap, bool)>, Value> {
    let width = ctx.member_get(record, "0")?.as_num_opt().unwrap_or(0.) as u32;
    let height = ctx.member_get(record, "1")?.as_num_opt().unwrap_or(0.) as u32;
    let pixels = ctx.member_get(record, "2")?;
    let clean = matches!(ctx.member_get(record, "3")?, Value::Bool(true));
    let premultiplied = matches!(ctx.member_get(record, "5")?, Value::Bool(true));
    Ok(ctx
        .typed_array_bytes(&pixels)
        .and_then(|bytes| {
            if premultiplied {
                sk::Pixmap::from_vec(bytes, sk::IntSize::from_wh(width, height)?)
            } else {
                rgba_bitmap(width, height, &bytes)
            }
        })
        .map(|bitmap| (bitmap, clean)))
}

/// The image argument of drawImage()/createPattern(): a record, an
/// OffscreenCanvas (its id), or (element binding) an element's node.
fn image_source(
    ctx: &mut Ctx,
    req: &Request,
    origin: &str,
) -> Result<Option<(sk::Pixmap, bool)>, Value> {
    match &req.payload {
        Value::Undefined => {
            let dom = host_dom(ctx);
            let dom = dom.borrow();
            Ok(req
                .n
                .first()
                .and_then(|source| source_bitmap(ctx, &dom, *source as usize, origin)))
        }
        Value::Num(id) => {
            super::webgl_host::publish_offscreen(ctx, *id as u32);
            Ok(offscreen(ctx, *id as u32).and_then(|canvas| canvas.borrow().snapshot()))
        }
        record => record_bitmap(ctx, record),
    }
}

fn offscreen(ctx: &mut Ctx, id: u32) -> Option<Rc<RefCell<Offscreen>>> {
    ctx.host_mut::<HostState>()?
        .offscreen_canvases
        .get(&id)
        .cloned()
}

/// A premultiplied bitmap as a private ImageBitmap record.
fn bitmap_record(ctx: &mut Ctx, bitmap: &sk::Pixmap, clean: bool) -> Result<Value, Value> {
    let pixels = ctx.make_uint8array(bitmap.data())?;
    // Do not let inherited setters/thenables expose opaque source pixels.
    let record = ctx.new_object_with_proto(&Value::Null);
    for (i, value) in [
        Value::Num(f64::from(bitmap.width())),
        Value::Num(f64::from(bitmap.height())),
        pixels,
        Value::Bool(clean),
        Value::Num(1.),
        Value::Bool(true),
    ]
    .into_iter()
    .enumerate()
    {
        ctx.member_set(&record, &i.to_string(), value)?;
    }
    Ok(record)
}

/// HTML #a-serialisation-of-the-bitmap-as-a-file, as `[type, bytes]`.
fn encoded(ctx: &mut Ctx, encoded: Option<(&'static str, Vec<u8>)>) -> Result<Value, Value> {
    match encoded {
        Some((mime, bytes)) => {
            let bytes = ctx.make_uint8array(&bytes)?;
            Ok(ctx.make_array(vec![Value::from_string(mime.to_string()), bytes]))
        }
        None => Ok(Value::Null),
    }
}

/// HTML #set-an-imagebitmaprenderingcontext's-output-bitmap with an
/// ImageBitmap's data (`record`), or without one (transparent black at the
/// canvas's dimensions).
fn set_output_bitmap(
    ctx: &mut Ctx,
    canvas: &mut Canvas,
    record: &Value,
    width: u32,
    height: u32,
) -> Result<(), Value> {
    if matches!(record, Value::Null | Value::Undefined) {
        canvas.sizing = Sizing::Blank;
        canvas.adopt(width, height, None, true);
        return Ok(());
    }
    // The origin-clean flag is part of the bitmap data referenced.
    if let Some((bitmap, clean)) = record_bitmap(ctx, record)? {
        canvas.sizing = Sizing::Valid;
        canvas.adopt(bitmap.width(), bitmap.height(), Some(bitmap), clean);
    }
    Ok(())
}

pub(super) fn call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let req = Request::read(ctx, args)?;
    if let Some(result) = realm_op(ctx, &req)? {
        return Ok(result);
    }
    let op = req.op.as_str();
    let n = &req.n;
    let text = &req.text;
    let payload = &req.payload;
    let image_op = op == "draw" || op == "createPattern";
    let dom = host_dom(ctx);
    let id = host_arg_node(&dom.borrow(), args, 0)
        .ok_or_else(|| ctx.make_error("TypeError", "Invalid canvas"))?;
    let readback = if matches!(op, "url" | "snapshot") {
        Some(id)
    } else if image_op && matches!(payload, Value::Undefined) {
        n.first().map(|n| *n as usize)
    } else {
        None
    };
    if let Some(source) = readback {
        super::webgl_host::publish(ctx, false, Some(source));
    }
    // HTML #update-the-rendering, context lost steps: the 2D contexts of the
    // document `id` whose backing storage was lost since the last check.
    if op == "lostContexts" {
        let dom = dom.borrow();
        let lost: Vec<Value> = dom
            .canvases
            .borrow_mut()
            .iter_mut()
            .filter(|(node, canvas)| {
                !canvas.webgl
                    && canvas.sizing == Sizing::Attributes
                    && canvas.lost()
                    && !canvas.lost_notified
                    && dom.owner_document(**node) == Some(id)
            })
            .map(|(&node, canvas)| {
                canvas.lost_notified = true;
                Value::Num(node as f64)
            })
            .collect();
        return Ok(if lost.is_empty() {
            Value::Null
        } else {
            ctx.make_array(lost)
        });
    }
    // HTML #dom-offscreencanvas: a Window's OffscreenCanvas inherits the
    // document element's language and directionality.
    if op == "inheritance" {
        let dom = dom.borrow();
        let Some(root) = dom.document_element() else {
            return Ok(Value::Null);
        };
        let language = dom
            .inherited_lang(root)
            .map_or(Value::Null, |lang| Value::from_string(lang.to_owned()));
        let rtl = dom.html_directionality_rtl(root);
        return Ok(ctx.make_array(vec![language, Value::Bool(rtl)]));
    }
    let (width, height) = dom
        .borrow()
        .canvas_size(id)
        .ok_or_else(|| ctx.make_error("TypeError", "Invalid canvas"))?;
    match op {
        // The width and height IDL attributes reflect the content attributes.
        "attributeSize" => {
            let (width, height) = attribute_size(&dom.borrow(), id);
            return Ok(ctx.make_array(vec![Value::Num(width as f64), Value::Num(height as f64)]));
        }
        // The context lost steps: attempt to create a backing storage.
        "restoreStorage" => {
            let dom = dom.borrow();
            let mut canvases = dom.canvases.borrow_mut();
            let restored = canvases.get_mut(&id).is_some_and(|canvas| {
                let _ = canvas.resize(width, height);
                !canvas.lost()
            });
            drop(canvases);
            drop(dom);
            if restored {
                host_dom(ctx).borrow_mut().canvas_changed(id);
            }
            return Ok(Value::Bool(restored));
        }
        "size" => {
            return Ok(ctx.make_array(vec![Value::Num(width as f64), Value::Num(height as f64)]));
        }
        "clean" => {
            return Ok(Value::Bool(
                dom.borrow()
                    .canvases
                    .borrow()
                    .get(&id)
                    .is_none_or(|canvas| canvas.origin_clean),
            ));
        }
        "snapshot" => {
            let dom = dom.borrow();
            let canvases = dom.canvases.borrow();
            let blank;
            let canvas = if let Some(canvas) = canvases.get(&id) {
                canvas
            } else {
                let Some(canvas) = Canvas::new(width, height, true) else {
                    return Ok(Value::Null);
                };
                blank = canvas;
                &blank
            };
            let Some(bytes) = canvas.get(0, 0, width, height) else {
                return Ok(Value::Null);
            };
            let pixels = ctx.make_uint8array(&bytes)?;
            let record = ctx.new_object_with_proto(&Value::Null);
            for (i, value) in [
                Value::Num(width as f64),
                Value::Num(height as f64),
                pixels,
                Value::Bool(canvas.origin_clean),
                Value::Num(1.),
                Value::Bool(false),
            ]
            .into_iter()
            .enumerate()
            {
                ctx.member_set(&record, &i.to_string(), value)?;
            }
            return Ok(record);
        }
        "init" => {
            let dom = dom.borrow();
            let mut canvases = dom.canvases.borrow_mut();
            if canvases
                .get(&id)
                .is_some_and(|canvas| canvas.webgl || canvas.sizing != Sizing::Attributes)
            {
                return Ok(Value::Bool(false));
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = canvases.entry(id) {
                let mut canvas = Canvas::new(0, 0, n.first().copied().unwrap_or(1.) != 0.)
                    .expect("an empty bitmap");
                // A bitmap that cannot be allocated leaves the context lost
                // (HTML #concept-canvas-context-lost), not absent.
                let _ = canvas.resize(width, height);
                canvas.document_origin = text.clone();
                entry.insert(canvas);
            }
            return Ok(Value::Bool(true));
        }
        // HTML #the-imagebitmaprenderingcontext-interface creation: a blank
        // output bitmap at the attributes' dimensions.
        "bitmapInit" => {
            let alpha = n.first().copied().unwrap_or(1.) != 0.;
            let dom_ref = dom.borrow();
            let mut canvases = dom_ref.canvases.borrow_mut();
            if canvases.contains_key(&id) {
                return Ok(Value::Bool(false));
            }
            let Some(mut canvas) = Canvas::new(0, 0, alpha) else {
                return Ok(Value::Bool(false));
            };
            canvas.document_origin = text.clone();
            canvas.sizing = Sizing::Blank;
            canvas.adopt(width, height, None, true);
            canvases.insert(id, canvas);
            drop(canvases);
            drop(dom_ref);
            dom.borrow_mut().canvas_changed(id);
            return Ok(Value::Bool(true));
        }
        "bitmapTransfer" => {
            let dom_ref = dom.borrow();
            let mut canvases = dom_ref.canvases.borrow_mut();
            let Some(canvas) = canvases.get_mut(&id) else {
                return Ok(Value::Undefined);
            };
            // A blank output bitmap takes the content attributes' dimensions.
            let (attr_width, attr_height) = attribute_size(&dom_ref, id);
            set_output_bitmap(ctx, canvas, payload, attr_width, attr_height)?;
            drop(canvases);
            drop(dom_ref);
            dom.borrow_mut().canvas_changed(id);
            return Ok(Value::Undefined);
        }
        // HTML #dom-canvas-transfercontroltooffscreen steps 2-6: a new
        // OffscreenCanvas with the element's dimensions, language and
        // directionality, whose placeholder is this element.
        "placeholder" => {
            let wake = ctx
                .host_mut::<HostState>()
                .and_then(|state| state.task_events.clone());
            let (language, rtl) = {
                let dom = dom.borrow();
                (
                    dom.inherited_lang(id).map(str::to_owned),
                    dom.html_directionality_rtl(id),
                )
            };
            let link = std::sync::Arc::new(crate::canvas_offscreen::Placeholder::new(
                id,
                Box::new(move || {
                    if let Some(events) = &wake {
                        let _ = events.send(super::TrustJsHostTask::CanvasFrame { node: id });
                    }
                }),
            ));
            {
                let dom = dom.borrow();
                let mut canvases = dom.canvases.borrow_mut();
                if canvases.contains_key(&id) {
                    return Ok(Value::Null);
                }
                let Some(mut canvas) = Canvas::new(0, 0, true) else {
                    return Ok(Value::Null);
                };
                canvas.document_origin = text.clone();
                canvas.sizing = Sizing::Placeholder;
                canvas.adopt(width, height, None, true);
                canvas.placeholder = Some(link.clone());
                canvases.insert(id, canvas);
            }
            let mut offscreen = Offscreen::new(u64::from(width), u64::from(height));
            offscreen.language = language;
            offscreen.rtl = rtl;
            offscreen.placeholder = Some(std::sync::Arc::downgrade(&link));
            offscreen.document = Some(id);
            offscreen.origin = text.clone();
            return Ok(Value::Num(f64::from(insert_offscreen(ctx, offscreen))));
        }
        // HTML #dom-canvas-todataurl / #dom-canvas-toblob: serialize the bitmap
        // as `text` (PNG, JPEG or WebP; anything else is PNG) with quality n[0].
        "encode" => {
            let quality = n.first().copied().filter(|q| q.is_finite());
            let bytes = {
                let dom = dom.borrow();
                let mut canvases = dom.canvases.borrow_mut();
                if let Some(canvas) = canvases.get_mut(&id) {
                    canvas.serialize(text, quality)
                } else {
                    Canvas::new(width, height, true)
                        .and_then(|mut canvas| canvas.serialize(text, quality))
                }
            };
            return encoded(ctx, bytes);
        }
        "url" => {
            let dom = dom.borrow();
            let mut canvases = dom.canvases.borrow_mut();
            let url = if let Some(canvas) = canvases.get_mut(&id) {
                canvas.data_url()
            } else {
                Canvas::new(width, height, true)
                    .map_or_else(|| "data:,".to_string(), |mut canvas| canvas.data_url())
            };
            return Ok(Value::from_string(url));
        }
        _ => {}
    }
    let source = if image_op {
        let origin = dom
            .borrow()
            .canvases
            .borrow()
            .get(&id)
            .map_or_else(String::new, |canvas| canvas.document_origin.clone());
        image_source(ctx, &req, &origin)?
    } else {
        None
    };
    let base = host_layout_environment(ctx).0;
    let (result, changed) = {
        let dom = dom.borrow();
        let environment = Environment::element(&dom, id);
        let mut canvases = dom.canvases.borrow_mut();
        let canvas = canvases
            .get_mut(&id)
            .ok_or_else(|| ctx.make_error("TypeError", "Canvas context not initialized"))?;
        if op == "focusRing" {
            let path = canvas.intended_path((!text.is_empty()).then_some(text.as_str()));
            let painted = path.map(|path| canvas.focus_ring(&path)).is_some();
            (Value::Undefined, painted)
        } else {
            context_op(ctx, &req, canvas, &environment, &base, source)?
        }
    };
    if changed {
        dom.borrow_mut().canvas_changed(id);
    }
    Ok(result)
}

/// HTML #attr-canvas-width / #attr-canvas-height: the rules for parsing
/// non-negative integers, with the defaults 300 and 150.
fn attribute_size(dom: &Dom, id: usize) -> (u32, u32) {
    let parse = |name, default| {
        dom.attr(id, name)
            .map(|v| v.trim_start_matches([' ', '\t', '\n', '\r', '\u{c}']))
            .map(|v| v.strip_prefix('+').unwrap_or(v))
            .and_then(|v| {
                v.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<u32>()
                    .ok()
            })
            .unwrap_or(default)
    };
    (parse("width", 300), parse("height", 150))
}

fn insert_offscreen(ctx: &mut Ctx, offscreen: Offscreen) -> u32 {
    let state = ctx.host_mut::<HostState>().unwrap();
    loop {
        state.next_offscreen_canvas = state.next_offscreen_canvas.wrapping_add(1).max(1);
        if !state
            .offscreen_canvases
            .contains_key(&state.next_offscreen_canvas)
        {
            break;
        }
    }
    let id = state.next_offscreen_canvas;
    state
        .offscreen_canvases
        .insert(id, Rc::new(RefCell::new(offscreen)));
    id
}

/// `__offscreen_canvas(id, op, numbers, payload)`.
pub(super) fn offscreen_call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let req = Request::read(ctx, args)?;
    if let Some(result) = realm_op(ctx, &req)? {
        return Ok(result);
    }
    let op = req.op.as_str();
    let n = &req.n;
    let id = args
        .first()
        .and_then(Value::as_num_opt)
        .filter(|id| id.is_finite() && *id >= 0. && id.fract() == 0.)
        .map_or(0, |id| id as u32);
    match op {
        // HTML #dom-offscreencanvas: [width, height, rtl, document node or -1],
        // the inherited language as the payload, and the Realm's origin.
        "create" => {
            let mut offscreen = Offscreen::new(
                n.first().copied().unwrap_or(0.) as u64,
                n.get(1).copied().unwrap_or(0.) as u64,
            );
            offscreen.rtl = n.get(2).is_some_and(|rtl| *rtl != 0.);
            offscreen.document = n
                .get(3)
                .copied()
                .filter(|node| *node >= 0.)
                .map(|node| node as usize);
            offscreen.language = req.payload.as_text().map(|lang| lang.into_owned());
            offscreen.origin = args
                .get(4)
                .and_then(Value::as_text)
                .map(|origin| origin.into_owned())
                .unwrap_or_default();
            return Ok(Value::Num(f64::from(insert_offscreen(ctx, offscreen))));
        }
        "free" => {
            if let Some(state) = ctx.host_mut::<HostState>() {
                state.offscreen_canvases.remove(&id);
                state.webgl.remove(&super::webgl_host::offscreen_key(id));
            }
            return Ok(Value::Undefined);
        }
        // HTML OffscreenCanvas transfer-receiving steps: claim a data holder.
        "receive" => {
            let Some(holder) = crate::canvas_offscreen::receive(&req.text) else {
                return Ok(Value::Null);
            };
            let mut offscreen = Offscreen::new(holder.width, holder.height);
            offscreen.language = holder.language;
            offscreen.rtl = holder.rtl;
            offscreen.placeholder = holder.placeholder;
            // The receiving Window's Document, whose SVG filters it names.
            offscreen.document = n
                .first()
                .copied()
                .filter(|node| *node >= 0.)
                .map(|node| node as usize);
            offscreen.origin = args
                .get(4)
                .and_then(Value::as_text)
                .map(|origin| origin.into_owned())
                .unwrap_or_default();
            // A transferred bitmap starts transparent; its placeholder shows
            // it at the next rendering update.
            offscreen.dirty = true;
            let width = offscreen.width as f64;
            let height = offscreen.height as f64;
            let new_id = insert_offscreen(ctx, offscreen);
            return Ok(ctx.make_array(vec![
                Value::Num(f64::from(new_id)),
                Value::Num(width),
                Value::Num(height),
            ]));
        }
        // The context lost steps' candidates; each Realm marks its own.
        "lostContexts" => {
            let lost: Vec<Value> =
                ctx.host_mut::<HostState>()
                    .map(|state| {
                        state
                            .offscreen_canvases
                            .iter()
                            .filter_map(|(&id, canvas)| {
                                let canvas = canvas.borrow();
                                let lost = canvas.mode == Mode::TwoD
                                    && canvas.canvas.as_ref().is_some_and(|canvas| {
                                        canvas.lost() && !canvas.lost_notified
                                    });
                                lost.then_some(Value::Num(f64::from(id)))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            return Ok(if lost.is_empty() {
                Value::Null
            } else {
                ctx.make_array(lost)
            });
        }
        _ => {}
    }
    let Some(target) = offscreen(ctx, id) else {
        return Err(ctx.make_error("TypeError", "Invalid OffscreenCanvas"));
    };
    match op {
        "size" => {
            let target = target.borrow();
            let (width, height) = if target.mode == Mode::Detached {
                (0, 0)
            } else {
                (target.width, target.height)
            };
            return Ok(ctx.make_array(vec![Value::Num(width as f64), Value::Num(height as f64)]));
        }
        // HTML OffscreenCanvas width/height setters.
        "setSize" => {
            let mut target = target.borrow_mut();
            if target.mode != Mode::Detached {
                let width = n.first().copied().unwrap_or(0.) as u64;
                let height = n.get(1).copied().unwrap_or(0.) as u64;
                target.resize(width, height);
            }
            drop(target);
            request_rendering(ctx, id);
            return Ok(Value::Undefined);
        }
        "clean" => {
            let target = target.borrow();
            return Ok(Value::Bool(
                target
                    .canvas
                    .as_ref()
                    .is_none_or(|canvas| canvas.origin_clean),
            ));
        }
        // The offscreen 2D context creation algorithm, step 5-6: a bitmap
        // of the canvas's dimensions shared with the context.
        "init2d" | "initBitmap" => {
            let alpha = n.first().copied().unwrap_or(1.) != 0.;
            let mut target = target.borrow_mut();
            if target.mode != Mode::None {
                return Ok(Value::Bool(false));
            }
            let (width, height) = target.bitmap_size();
            let mut canvas = Canvas::new(0, 0, alpha).expect("empty canvas");
            canvas.document_origin = target.origin.clone();
            if op == "init2d" {
                // An allocation failure leaves the context lost, not absent.
                let _ = canvas.resize(width, height);
                target.mode = Mode::TwoD;
            } else {
                canvas.sizing = Sizing::Blank;
                canvas.adopt(width, height, None, true);
                target.mode = Mode::BitmapRenderer;
            }
            target.canvas = Some(canvas);
            target.dirty = true;
            return Ok(Value::Bool(true));
        }
        "bitmapTransfer" => {
            {
                let mut guard = target.borrow_mut();
                let target = &mut *guard;
                let (width, height) = target.bitmap_size();
                if let Some(canvas) = target.canvas.as_mut() {
                    set_output_bitmap(ctx, canvas, &req.payload, width, height)?;
                    // The OffscreenCanvas's bitmap is the context's output bitmap.
                    target.width = u64::from(canvas.width);
                    target.height = u64::from(canvas.height);
                }
                target.dirty = true;
            }
            request_rendering(ctx, id);
            return Ok(Value::Undefined);
        }
        // HTML #dom-offscreencanvas-transfertoimagebitmap steps 3-4.
        "transferToImageBitmap" => {
            super::webgl_host::publish_offscreen(ctx, id);
            let taken = {
                let mut target = target.borrow_mut();
                let (width, height) = target.bitmap_size();
                let webgl = target.mode == Mode::WebGl;
                let taken = match target.canvas.as_mut() {
                    Some(canvas) if webgl => canvas
                        .snapshot()
                        .map(|bitmap| (bitmap, canvas.origin_clean))
                        .inspect(|_| {
                            canvas.adopt(width, height, None, true);
                        }),
                    Some(canvas) => canvas.take_bitmap(),
                    None => None,
                };
                if taken.is_some() {
                    target.dirty = true;
                }
                taken
            };
            if taken.is_some() {
                super::webgl_host::clear_offscreen(ctx, id);
                request_rendering(ctx, id);
            }
            return match taken {
                Some((bitmap, clean)) => bitmap_record(ctx, &bitmap, clean),
                None => Ok(Value::Null),
            };
        }
        // An ImageBitmap/texture source snapshot of the bitmap.
        "snapshot" => {
            super::webgl_host::publish_offscreen(ctx, id);
            let snapshot = target.borrow().snapshot();
            return match snapshot {
                Some((bitmap, clean)) => bitmap_record(ctx, &bitmap, clean),
                None => Ok(Value::Null),
            };
        }
        // HTML #dom-offscreencanvas-converttoblob step 5 and 8.1.
        "encode" => {
            super::webgl_host::publish_offscreen(ctx, id);
            let quality = n.first().copied().filter(|q| q.is_finite());
            let bytes = {
                let target = target.borrow();
                target.snapshot().and_then(|(bitmap, clean)| {
                    let mut copy = Canvas::new(0, 0, true)?;
                    copy.adopt(bitmap.width(), bitmap.height(), Some(bitmap), clean);
                    copy.serialize(&req.text, quality)
                })
            };
            return encoded(ctx, bytes);
        }
        // HTML OffscreenCanvas transfer steps 2-9.
        "transfer" => {
            let holder = {
                let mut target = target.borrow_mut();
                if target.mode != Mode::None {
                    return Ok(Value::Null);
                }
                target.mode = Mode::Detached;
                target.canvas = None;
                crate::canvas_offscreen::DataHolder {
                    width: target.width,
                    height: target.height,
                    language: target.language.take(),
                    rtl: target.rtl,
                    placeholder: target.placeholder.take(),
                }
            };
            if let Some(state) = ctx.host_mut::<HostState>() {
                state.offscreen_canvases.remove(&id);
            }
            return Ok(
                crate::canvas_offscreen::send(holder).map_or(Value::Null, Value::from_string)
            );
        }
        "markLost" => {
            if let Some(canvas) = target.borrow_mut().canvas.as_mut() {
                canvas.lost_notified = true;
            }
            return Ok(Value::Undefined);
        }
        "restoreStorage" => {
            // Context lost steps: attempt to create a backing storage.
            let mut target = target.borrow_mut();
            let (width, height) = target.bitmap_size();
            let restored = target.canvas.as_mut().is_some_and(|canvas| {
                let _ = canvas.resize(width, height);
                !canvas.lost()
            });
            if restored {
                target.dirty = true;
            }
            return Ok(Value::Bool(restored));
        }
        _ => {}
    }
    let origin = target.borrow().origin.clone();
    let source = if op == "draw" || op == "createPattern" {
        image_source(ctx, &req, &origin)?
    } else {
        None
    };
    let base = host_layout_environment(ctx).0;
    let dom = host_dom(ctx);
    let (result, changed) = {
        let dom = dom.borrow();
        let mut guard = target.borrow_mut();
        let target = &mut *guard;
        let environment = Environment::offscreen(&dom, target);
        let Some(canvas) = target.canvas.as_mut().filter(|_| target.mode == Mode::TwoD) else {
            return Err(ctx.make_error("TypeError", "OffscreenCanvas context not initialized"));
        };
        let result = context_op(ctx, &req, canvas, &environment, &base, source)?;
        if result.1 {
            target.dirty = true;
        }
        result
    };
    if changed {
        request_rendering(ctx, id);
    }
    Ok(result)
}

/// A changed OffscreenCanvas whose placeholder is in this agent's DOM asks
/// for that document's next rendering update, where it is pushed.
fn request_rendering(ctx: &mut Ctx, id: u32) {
    let Some(target) = offscreen(ctx, id) else {
        return;
    };
    let node = target
        .borrow()
        .placeholder
        .as_ref()
        .and_then(std::sync::Weak::upgrade)
        .map(|link| link.node);
    if let Some(node) = node
        && ctx
            .host_mut::<HostState>()
            .is_some_and(|state| state.worker_self.is_none())
    {
        let dom = host_dom(ctx);
        let mut dom = dom.borrow_mut();
        let owned = dom
            .canvases
            .get_mut()
            .get(&node)
            .and_then(|canvas| canvas.placeholder.as_ref())
            .is_some();
        if owned {
            dom.replaced_pixels_changed(node);
        }
    }
}

/// HTML "update the rendering" for the OffscreenCanvases of this agent:
/// push each changed bitmap to its placeholder canvas element, then (in a
/// Window agent) present every committed frame to its placeholder.
pub(super) fn push_placeholders(ctx: &mut Ctx) {
    let Some(state) = ctx.host_mut::<HostState>() else {
        return;
    };
    for canvas in state.offscreen_canvases.values() {
        let mut canvas = canvas.borrow_mut();
        if canvas.placeholder.is_some() && canvas.dirty {
            canvas.push();
        }
    }
    if state.worker_self.is_none() {
        let dom = state.dom.clone();
        present_frames(&mut dom.borrow_mut(), None);
    }
}

/// Install committed frames as placeholder bitmaps: every placeholder's, or
/// only `node`'s.
pub(super) fn present_frames(dom: &mut Dom, node: Option<usize>) {
    let mut presented = Vec::new();
    for (&id, canvas) in dom.canvases.get_mut().iter_mut() {
        if node.is_some_and(|node| node != id) {
            continue;
        }
        if let Some(frame) = canvas.placeholder.as_ref().and_then(|link| link.take()) {
            crate::canvas_offscreen::present(canvas, frame);
            presented.push(id);
        }
    }
    for id in presented {
        dom.canvas_changed(id);
    }
}

/// Whether any OffscreenCanvas of this agent has a change to push.
pub(super) fn placeholders_dirty(ctx: &mut Ctx) -> bool {
    ctx.host_mut::<HostState>().is_some_and(|state| {
        state.offscreen_canvases.values().any(|canvas| {
            let canvas = canvas.borrow();
            canvas.dirty && canvas.placeholder.is_some()
        })
    })
}

/// The shared 2D drawing operations (HTML #2dcontext). Returns the result
/// and whether the bitmap changed.
fn context_op(
    ctx: &mut Ctx,
    req: &Request,
    canvas: &mut Canvas,
    environment: &Environment<'_>,
    base: &url::Url,
    source: Option<(sk::Pixmap, bool)>,
) -> Result<(Value, bool), Value> {
    let op = req.op.as_str();
    let n = &req.n;
    let text = &req.text;
    let payload = &req.payload;
    // HTML #drawing-model step 4: the current filter, with its references
    // resolved for this operation.
    if matches!(
        op,
        "fill"
            | "stroke"
            | "fillPath"
            | "strokePath"
            | "fillRect"
            | "strokeRect"
            | "fillText"
            | "strokeText"
            | "draw"
    ) {
        canvas.filter_chain = canvas
            .state
            .filter
            .clone()
            .and_then(|filter| environment.filter_chain(&filter, base));
    }
    let mut changed = false;
    let result = match op {
        "getTextStyle" => Value::from_string(
            canvas
                .state
                .text
                .attribute(n.first().copied().unwrap_or(0.) as u8),
        ),
        "setTextStyle" => {
            let index = n.first().copied().unwrap_or(0.) as u8;
            if index == crate::canvas_text::FONT {
                let (units, weight) = environment.font_units();
                canvas.state.text.set_font(text, units, weight);
            } else {
                canvas.state.text.set_attribute(index, text);
            }
            Value::Undefined
        }
        "measureText" | "fillText" | "strokeText" => {
            let prepared = canvas.state.text.prepare(text, &environment.text());
            if op == "measureText" {
                ctx.make_array(prepared.metrics().into_iter().map(Value::Num).collect())
            } else {
                canvas.draw_text(&prepared, n, op == "strokeText");
                changed = true;
                Value::Undefined
            }
        }
        // HTML #dom-context-2d-filter.
        "getFilter" => Value::from_string(
            canvas
                .state
                .filter
                .as_ref()
                .map_or_else(|| "none".to_owned(), |filter| filter.text.clone()),
        ),
        "setFilter" => {
            if text == "none" {
                canvas.state.filter = None;
            } else {
                let (units, _) = environment.font_units();
                let viewport = environment.dom.viewport_px();
                let length = |value: &str| {
                    crate::layout2::value::Len::parse(
                        value,
                        units,
                        crate::layout2::value::Vp {
                            w: viewport.0,
                            h: viewport.1,
                        },
                    )?
                    .resolve(None)
                };
                let color = environment.current_color();
                let current = crate::render::PaintColor::parse_css(&color)
                    .unwrap_or(crate::render::PaintColor::Rgba(0, 0, 0, 255));
                if let Some(filter) = crate::canvas_filter::parse(text, &length, current) {
                    canvas.state.filter = Some(std::sync::Arc::new(filter));
                }
            }
            Value::Undefined
        }
        "generation" => Value::Num(canvas.generation as f64),
        // HTML #concept-canvas-context-lost: set by the context lost steps.
        "lost" => Value::Bool(canvas.lost_notified),
        "reset" => {
            let _ = canvas.resize(canvas.width, canvas.height);
            changed = true;
            Value::Undefined
        }
        // The context lost steps' "reset the rendering context to its
        // default state": drawing state only, the storage is gone.
        "resetState" => {
            canvas.state = crate::canvas::State::default();
            canvas.restore_all();
            canvas.begin();
            canvas.generation = canvas.generation.wrapping_add(1);
            Value::Undefined
        }
        "save" => {
            canvas.save();
            Value::Undefined
        }
        "restore" => {
            canvas.restore();
            Value::Undefined
        }
        "beginPath" => {
            canvas.begin();
            Value::Undefined
        }
        "moveTo" | "lineTo" | "closePath" | "quadraticCurveTo" | "bezierCurveTo" | "rect"
        | "ellipse" | "arcTo" | "roundRect" => {
            let count = match op {
                "closePath" => 0,
                "moveTo" | "lineTo" => 2,
                "rect" | "quadraticCurveTo" => 4,
                "arcTo" => 5,
                "bezierCurveTo" => 6,
                "roundRect" => 12,
                _ => 8,
            };
            if n.len() >= count {
                canvas.path_command(op, n);
            }
            Value::Undefined
        }
        "fill" | "stroke" => {
            canvas.paint_path(op == "stroke", text == "evenodd");
            changed = true;
            Value::Undefined
        }
        "fillPath" | "strokePath" | "clipPath" => {
            canvas.external_path(text, op, n.first() == Some(&1.));
            changed = op != "clipPath";
            Value::Undefined
        }
        "clip" => {
            canvas.clip(text == "evenodd");
            Value::Undefined
        }
        "fillRect" | "strokeRect" | "clearRect" => {
            if n.len() >= 4 {
                canvas.rectangle(op, n);
                changed = true;
            }
            Value::Undefined
        }
        "transform" | "setTransform" => {
            if n.len() >= 6 && n[..6].iter().all(|n| n.is_finite()) {
                let matrix = Affine::new([n[0], n[1], n[2], n[3], n[4], n[5]]);
                if op == "transform" {
                    canvas.state.transform *= matrix;
                } else {
                    canvas.state.transform = matrix;
                }
            }
            Value::Undefined
        }
        "getTransform" => ctx.make_array(
            canvas
                .state
                .transform
                .as_coeffs()
                .into_iter()
                .map(Value::Num)
                .collect(),
        ),
        "put" => {
            // The pixels are read in place: ImageData is a whole-canvas-sized buffer in the
            // emulator/video case, and a copy per frame would double the memory traffic.
            if n.len() >= 10
                && ctx
                    .with_typed_array_bytes(payload, |bytes| {
                        canvas.put(
                            bytes,
                            n[0] as u32,
                            n[1] as u32,
                            &n[2..],
                            n[8] != 0.,
                            n[9] as u8,
                        );
                    })
                    .is_some()
            {
                changed = true;
            }
            Value::Undefined
        }
        "get" => {
            if n.len() >= 6 {
                let bytes = canvas
                    .get_converted(
                        n[0] as i64,
                        n[1] as i64,
                        n[2] as u32,
                        n[3] as u32,
                        n[4] != 0.,
                        n[5] as u8,
                    )
                    .ok_or_else(|| ctx.make_error("RangeError", "Canvas pixel buffer too large"))?;
                if !ctx.typed_array_set_bytes(payload, &bytes) {
                    return Err(ctx.make_error("TypeError", "Invalid pixel destination"));
                }
            }
            Value::Undefined
        }
        "draw" => {
            if let Some((source, clean)) = source {
                let dimensions = [source.width() as f64, source.height() as f64];
                let rect = match n.len() {
                    3 => vec![
                        0.,
                        0.,
                        dimensions[0],
                        dimensions[1],
                        n[1],
                        n[2],
                        dimensions[0],
                        dimensions[1],
                    ],
                    5 => vec![0., 0., dimensions[0], dimensions[1], n[1], n[2], n[3], n[4]],
                    9 => n[1..].to_vec(),
                    _ => {
                        canvas.filter_chain = None;
                        return Ok((Value::Undefined, false));
                    }
                };
                canvas.draw(&source, &rect);
                if rect[2] != 0. && rect[3] != 0. {
                    canvas.origin_clean &= clean;
                }
                changed = true;
            }
            Value::Undefined
        }
        "setShadowColor" => {
            // CSS Color 4 #parse-color: resolve currentcolor against the
            // font style source object, including inherited currentcolor.
            let value = if text.trim().eq_ignore_ascii_case("currentcolor") {
                environment.current_color()
            } else {
                text.clone()
            };
            if let Some((color, serialized)) = Canvas::color(&value) {
                canvas.state.shadow.color = color;
                canvas.state.shadow.serialized = serialized;
            }
            Value::Undefined
        }
        "getShadowColor" => Value::from_string(canvas.state.shadow.serialized.clone()),
        "setShadowBlur" => {
            if let Some(&value) = n.first()
                && value.is_finite()
                && value >= 0.
            {
                canvas.state.shadow.blur = value;
            }
            Value::Undefined
        }
        "getShadowBlur" => Value::Num(canvas.state.shadow.blur),
        "setShadowOffsetX" | "setShadowOffsetY" => {
            if let Some(&value) = n.first()
                && value.is_finite()
            {
                canvas.state.shadow.offset[usize::from(op == "setShadowOffsetY")] = value;
            }
            Value::Undefined
        }
        "getShadowOffsetX" | "getShadowOffsetY" => {
            Value::Num(canvas.state.shadow.offset[usize::from(op == "getShadowOffsetY")])
        }
        "setStyle" => {
            if let Some((color, serialized)) = Canvas::color(text) {
                canvas.state.gradients[usize::from(n.first() == Some(&1.))] = None;
                canvas.state.patterns[usize::from(n.first() == Some(&1.))] = None;
                if n.first() == Some(&1.) {
                    canvas.state.stroke_color = color;
                    canvas.state.stroke_text = serialized;
                } else {
                    canvas.state.fill = color;
                    canvas.state.fill_text = serialized;
                }
                Value::Bool(true)
            } else {
                Value::Bool(false)
            }
        }
        "setGradient" => {
            if let Some((&index, values)) = n.split_first()
                && let Some(gradient) = crate::canvas::Gradient::from_numbers(values)
            {
                canvas.state.gradients[usize::from(index == 1.)] =
                    Some(std::sync::Arc::new(gradient));
                canvas.state.patterns[usize::from(index == 1.)] = None;
            }
            Value::Undefined
        }
        // HTML #dom-context-2d-createpattern: snapshot the usable image.
        "createPattern" => match source {
            Some((image, clean)) => {
                let repetition = ["repeat", "repeat-x", "repeat-y", "no-repeat"]
                    .get(n.get(1).copied().unwrap_or(0.) as usize)
                    .copied()
                    .unwrap_or("repeat");
                let Some(pattern) = crate::canvas::Pattern::new(image, repetition, clean) else {
                    return Ok((Value::Null, false));
                };
                let state = ctx.host_mut::<HostState>().unwrap();
                state.next_canvas_pattern = state.next_canvas_pattern.wrapping_add(1).max(1);
                let pattern_id = state.next_canvas_pattern;
                state
                    .canvas_patterns
                    .insert(pattern_id, std::sync::Arc::new(pattern));
                ctx.make_array(vec![Value::Num(f64::from(pattern_id)), Value::Bool(clean)])
            }
            None => Value::Null,
        },
        // #dom-context-2d-fillstyle setter steps for a CanvasPattern.
        "setPattern" => {
            let index = usize::from(n.first() == Some(&1.));
            let pattern_id = n.get(1).copied().unwrap_or(0.) as u32;
            let pattern = ctx
                .host_mut::<HostState>()
                .and_then(|state| state.canvas_patterns.get(&pattern_id).cloned());
            if let Some(pattern) = pattern {
                if !pattern.origin_clean {
                    canvas.origin_clean = false;
                }
                canvas.state.gradients[index] = None;
                canvas.state.patterns[index] = Some(pattern);
            }
            Value::Undefined
        }
        "isPointInPath" | "isPointInStroke" => {
            let path = canvas.intended_path((!text.is_empty()).then_some(text.as_str()));
            let x = n.first().copied().unwrap_or(f64::NAN);
            let y = n.get(1).copied().unwrap_or(f64::NAN);
            Value::Bool(path.is_some_and(|path| {
                if op == "isPointInPath" {
                    canvas.point_in_path(&path, x, y, n.get(2) == Some(&1.))
                } else {
                    canvas.point_in_stroke(&path, x, y)
                }
            }))
        }
        "setSmoothingQuality" => {
            if let Some(quality) = ["low", "medium", "high"]
                .iter()
                .position(|value| value == text)
            {
                canvas.state.smoothing_quality = quality as u8;
            }
            Value::Undefined
        }
        "getSmoothingQuality" => Value::from_string(
            ["low", "medium", "high"][usize::from(canvas.state.smoothing_quality.min(2))]
                .to_string(),
        ),
        "getStyle" => Value::from_string(if n.first() == Some(&1.) {
            canvas.state.stroke_text.clone()
        } else {
            canvas.state.fill_text.clone()
        }),
        "setComposite" => {
            if crate::canvas::blend(text).is_some() {
                canvas.state.composite = text.clone();
            }
            Value::Undefined
        }
        "getComposite" => Value::from_string(canvas.state.composite.clone()),
        "setAlpha" => {
            if let Some(&alpha) = n.first()
                && (0. ..=1.).contains(&alpha)
            {
                canvas.state.alpha = alpha as f32;
            }
            Value::Undefined
        }
        "getAlpha" => Value::Num(canvas.state.alpha as f64),
        "setWidth" => {
            if let Some(&value) = n.first()
                && value > 0.
                && value.is_finite()
            {
                canvas.state.stroke.width = value as f32;
            }
            Value::Undefined
        }
        "getWidth" => Value::Num(canvas.state.stroke.width as f64),
        "setMiter" => {
            if let Some(&value) = n.first()
                && value > 0.
                && value.is_finite()
            {
                canvas.state.stroke.miter_limit = value as f32;
            }
            Value::Undefined
        }
        "getMiter" => Value::Num(canvas.state.stroke.miter_limit as f64),
        "setCap" => {
            canvas.state.stroke.line_cap = match text.as_str() {
                "butt" => sk::LineCap::Butt,
                "round" => sk::LineCap::Round,
                "square" => sk::LineCap::Square,
                _ => canvas.state.stroke.line_cap,
            };
            Value::Undefined
        }
        "getCap" => Value::from_string(
            match canvas.state.stroke.line_cap {
                sk::LineCap::Butt => "butt",
                sk::LineCap::Round => "round",
                sk::LineCap::Square => "square",
            }
            .into(),
        ),
        "setJoin" => {
            canvas.state.stroke.line_join = match text.as_str() {
                "miter" => sk::LineJoin::Miter,
                "round" => sk::LineJoin::Round,
                "bevel" => sk::LineJoin::Bevel,
                _ => canvas.state.stroke.line_join,
            };
            Value::Undefined
        }
        "getJoin" => Value::from_string(
            match canvas.state.stroke.line_join {
                sk::LineJoin::Round => "round",
                sk::LineJoin::Bevel => "bevel",
                _ => "miter",
            }
            .into(),
        ),
        "setSmoothing" => {
            canvas.state.smoothing = n.first() != Some(&0.);
            Value::Undefined
        }
        "getSmoothing" => Value::Bool(canvas.state.smoothing),
        "setDash" => {
            canvas.state.dash = n.clone();
            canvas.update_dash();
            Value::Undefined
        }
        "getDash" => ctx.make_array(canvas.state.dash.iter().copied().map(Value::Num).collect()),
        "setDashOffset" => {
            if let Some(&value) = n.first()
                && value.is_finite()
            {
                canvas.state.dash_offset = value;
                canvas.update_dash();
            }
            Value::Undefined
        }
        "getDashOffset" => Value::Num(canvas.state.dash_offset),
        _ => {
            canvas.filter_chain = None;
            return Err(ctx.make_error("TypeError", format!("Unknown canvas operation: {op}")));
        }
    };
    canvas.filter_chain = None;
    Ok((result, changed))
}

/// Snapshot canvas sources before drawing (including overlapping self-copies).
/// Image sources use already-available resource bytes; never block JS on I/O.
fn source_bitmap(
    ctx: &mut Ctx,
    dom: &crate::dom::Dom,
    id: usize,
    destination_origin: &str,
) -> Option<(sk::Pixmap, bool)> {
    if !dom.is_valid(id) {
        return None;
    }
    if let Some((width, height)) = dom.canvas_size(id) {
        if let Some(canvas) = dom.canvases.borrow().get(&id) {
            return canvas
                .snapshot()
                .map(|bitmap| (bitmap, canvas.origin_clean));
        }
        return Canvas::new(width, height, true)?
            .snapshot()
            .map(|bitmap| (bitmap, true));
    }
    if dom.tag_name(id) != Some("img") {
        return None;
    }
    let (base, viewport, density) = host_layout_environment(ctx);
    let source = crate::responsive_image::select(dom, id, &base, viewport, density)?.source;
    let (bytes, clean) = if source.starts_with("data:") {
        (crate::img::decode_data_url(&source)?, true)
    } else if source.starts_with("blob:") {
        // Fetch #scheme-fetch "blob" for the image's environment (File API
        // #blob-url-obtain-object); the image is clean when the Blob's
        // creator origin (URL #concept-url-origin) is the canvas's.
        let realm = ctx.host_job_context();
        let state = ctx.host_mut::<HostState>()?;
        let key = super::blob_storage_key(&super::realm_origin(state, realm));
        let entry = state.blobs.obtain(&source, Some(&key))?;
        (
            entry.read().ok()?,
            destination_origin != "null" && entry.origin == destination_origin,
        )
    } else {
        let url = url::Url::parse(&source).ok()?;
        let fetch = ctx
            .host_mut::<HostState>()?
            .network
            .as_ref()?
            .cache
            .peek(&url)?;
        let response = fetch.peek()?.as_ref().ok()?;
        if !(200..300).contains(&response.status) {
            return None;
        }
        // The existing image pipeline issues no-CORS requests. Never make
        // those cross-origin response pixels readable merely because decoded.
        let clean = destination_origin != "null"
            && !response.url_list.is_empty()
            && response
                .url_list
                .iter()
                .all(|url| url.origin().ascii_serialization() == destination_origin);
        (response.body.clone(), clean)
    };
    let image = crate::img::decode_graphical(&bytes).ok()?;
    Some((rgba_bitmap(image.width, image.height, &image.rgba)?, clean))
}

fn rgba_bitmap(width: u32, height: u32, rgba: &[u8]) -> Option<sk::Pixmap> {
    if (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?
        != rgba.len()
    {
        return None;
    }
    let mut bitmap = sk::Pixmap::new(width, height)?;
    for (out, pixel) in bitmap
        .data_mut()
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(rgba.as_chunks::<4>().0)
    {
        for channel in 0..3 {
            out[channel] = ((pixel[channel] as u32 * pixel[3] as u32 + 127) / 255) as u8;
        }
        out[3] = pixel[3];
    }
    Some(bitmap)
}
