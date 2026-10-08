//! Private, byte-oriented WebGL host boundary. JS bindings perform Web IDL
//! conversions and brand checks before any driver state is borrowed.
use super::{Ctx, HostState, Value, host_arg_node, host_arg_string, host_dom};
use crate::webgl::{Attributes, Context, Reply};

#[path = "webgl_host/fast.rs"]
mod fast;
pub(super) use fast::Cache as FastCache;

/// WebGL contexts of OffscreenCanvases are keyed above every DOM node id;
/// their canvas is the OffscreenCanvas's native bitmap, not a DOM entry.
const OFFSCREEN_KEYS: usize = 1 << 40;

pub(super) fn offscreen_key(id: u32) -> usize {
    OFFSCREEN_KEYS + id as usize
}

fn offscreen_id(key: usize) -> Option<u32> {
    key.checked_sub(OFFSCREEN_KEYS)
        .and_then(|id| u32::try_from(id).ok())
}

/// Whether a binding's canvas argument names an OffscreenCanvas context.
pub(super) fn is_offscreen_argument(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_num_opt)
        .is_some_and(|key| key.is_finite() && key >= OFFSCREEN_KEYS as f64)
}

/// Run `f` with the canvas a WebGL context draws into: the element's DOM
/// canvas, or the OffscreenCanvas's bitmap.
fn with_canvas<R>(
    state: &HostState,
    key: usize,
    f: impl FnOnce(&mut crate::canvas::Canvas) -> R,
) -> Option<R> {
    if let Some(id) = offscreen_id(key) {
        let offscreen = state.offscreen_canvases.get(&id)?.clone();
        let mut offscreen = offscreen.borrow_mut();
        if offscreen.mode != crate::canvas_offscreen::Mode::WebGl {
            return None;
        }
        f(offscreen.canvas.as_mut()?).into()
    } else {
        let dom = state.dom.borrow();
        let mut canvases = dom.canvases.borrow_mut();
        canvases.get_mut(&key).map(f)
    }
}

pub(super) fn publish(ctx: &mut Ctx, present: bool, only: Option<usize>) {
    let Some(state) = ctx.host_mut::<HostState>() else {
        return;
    };
    let mut allocated = state
        .webgl
        .values()
        .map(Context::allocated_bytes)
        .sum::<usize>();
    let keys: Vec<usize> = state.webgl.keys().copied().collect();
    for id in keys {
        if only.is_some_and(|only| id != only) {
            continue;
        }
        // Only an OffscreenCanvas with a placeholder presents its frames
        // (and then clears an unpreserved drawing buffer); others are read
        // back when consumed.
        if present
            && let Some(offscreen) = offscreen_id(id)
            && state
                .offscreen_canvases
                .get(&offscreen)
                .is_none_or(|canvas| canvas.borrow().placeholder.is_none())
        {
            continue;
        }
        let Some(mut context) = state.webgl.remove(&id) else {
            continue;
        };
        allocated -= context.allocated_bytes();
        context.budget = crate::webgl::PAGE_BUDGET.saturating_sub(allocated);
        let published = with_canvas(state, id, |canvas| {
            if context.generation != canvas.generation {
                if context.resize(canvas.width, canvas.height).is_err() {
                    context.lose();
                }
                context.generation = canvas.generation;
            }
            if (!present || context.dirty)
                && let Some((pixels, opaque)) = context.snapshot(present)
                && canvas.width > 0
                && canvas.height > 0
            {
                canvas.publish_webgl(context.width, context.height, pixels, opaque, present);
                return true;
            }
            false
        });
        // A presented OffscreenCanvas frame is pushed to its placeholder.
        if published == Some(true)
            && present
            && let Some(offscreen) = offscreen_id(id)
            && let Some(offscreen) = state.offscreen_canvases.get(&offscreen)
        {
            offscreen.borrow_mut().dirty = true;
        }
        allocated += context.allocated_bytes();
        state.webgl.insert(id, context);
    }
}

/// Read an OffscreenCanvas's WebGL drawing buffer back into its bitmap
/// before the bitmap is consumed.
pub(super) fn publish_offscreen(ctx: &mut Ctx, id: u32) {
    let key = offscreen_key(id);
    if ctx
        .host_mut::<HostState>()
        .is_some_and(|state| state.webgl.contains_key(&key))
    {
        publish(ctx, false, Some(key));
    }
}

/// HTML #dom-offscreencanvas-transfertoimagebitmap step 4 for WebGL.
pub(super) fn clear_offscreen(ctx: &mut Ctx, id: u32) {
    if let Some(context) = ctx
        .host_mut::<HostState>()
        .and_then(|state| state.webgl.get_mut(&offscreen_key(id)))
    {
        context.clear_drawing_buffer();
    }
}

pub(super) fn call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let mut op = host_arg_string(ctx, args, 1);
    let payload = args.get(3).cloned().unwrap_or(Value::Undefined);
    if op == "fastMethod" {
        return fast::wrap(ctx, payload);
    }
    if op == "slotGetter" {
        return Ok(Value::Obj(ctx.make_native(
            "WebGL private slots",
            1,
            |ctx, _, args| {
                let slots = ctx
                    .host_mut::<HostState>()
                    .unwrap()
                    .webgl_slots
                    .clone()
                    .unwrap();
                ctx.weak_map_get(&slots, args.first().unwrap_or(&Value::Undefined))
            },
        )));
    }
    if op == "slots" || op == "canvasSlots" {
        let state = ctx.host_mut::<HostState>().unwrap();
        let slots = if op == "slots" {
            &mut state.webgl_slots
        } else {
            &mut state.canvas_context_slots
        };
        return Ok(slots.get_or_insert(payload).clone());
    }
    let mut n = Vec::new();
    if let Some(values) = args.get(2) {
        if let Some(values) = ctx.copy_numeric_array(values, 1_048_576) {
            n = values;
        } else {
            let count = ctx.member_get(values, "length")?.as_num_opt().unwrap_or(0.) as usize;
            if count > 1_048_576 {
                return Err(ctx.make_error("RangeError", "WebGL argument budget exceeded"));
            }
            for i in 0..count {
                let v = ctx.member_get(values, &i.to_string())?;
                n.push(ctx.coerce_number(&v)?);
            }
        }
    }
    if op == "uniformTyped" {
        // WebGL Float32List/Int32List + Web IDL #js-to-union: the JS binding
        // has checked the intrinsic TypedArray brand. Read its view, including
        // byteOffset/byteLength, without invoking its iterator or indexed getters.
        let bytes = ctx.buffer_source_bytes(&payload, true).ok_or_else(|| {
            ctx.make_error("TypeError", "Expected an attached WebGL numeric list")
        })?;
        if bytes.len() / 4 > 1_048_572 {
            return Err(ctx.make_error("RangeError", "WebGL argument budget exceeded"));
        }
        let id = n.first().copied().unwrap_or(0.);
        if id <= 0. {
            return Ok(Value::Undefined);
        }
        if n.get(4).is_some_and(|v| *v != 0.) {
            op = "error".into();
            n.clear();
            n.push(1281.);
        } else {
            let integer = n.get(2).is_some_and(|v| *v != 0.);
            n.truncate(4);
            n.extend(bytes.as_chunks::<4>().0.iter().map(|word| {
                if integer {
                    i32::from_ne_bytes(*word) as f64
                } else {
                    f32::from_ne_bytes(*word) as f64
                }
            }));
            op = "uniform".into();
        }
    }
    let text = payload.as_text().unwrap_or_default().into_owned();
    let bytes = if matches!(
        op.as_str(),
        "bufferData" | "bufferSubData" | "texImage2D" | "texSubImage2D" | "readPixels"
    ) {
        ctx.buffer_source_bytes(&payload, true)
    } else {
        None
    };
    if matches!(
        op.as_str(),
        "bufferData" | "bufferSubData" | "texImage2D" | "texSubImage2D" | "readPixels"
    ) && !matches!(payload, Value::Undefined | Value::Null)
        && bytes.is_none()
    {
        return Err(ctx.make_error("TypeError", "Expected an attached BufferSource"));
    }
    let dom = host_dom(ctx);
    let id = if is_offscreen_argument(args.first()) {
        args[0].as_num_opt().unwrap_or(0.) as usize
    } else {
        host_arg_node(&dom.borrow(), args, 0)
            .ok_or_else(|| ctx.make_error("TypeError", "Invalid WebGL canvas"))?
    };
    if op == "dispose" || op == "init" {
        ctx.host_mut::<HostState>()
            .unwrap()
            .webgl_fast
            .forget_contexts();
    }
    if op == "dispose" {
        let state = ctx.host_mut::<HostState>().unwrap();
        state.webgl.remove(&id);
        if offscreen_id(id).is_none() {
            with_canvas(state, id, |canvas| {
                canvas.publish_webgl(0, 0, vec![], true, true)
            });
        }
        return Ok(Value::Undefined);
    }
    if op == "init"
        && let Some(offscreen) = offscreen_id(id)
    {
        return init_offscreen(ctx, id, offscreen, &n, text);
    }
    if op == "init" {
        let (width, height) = dom
            .borrow()
            .canvas_size(id)
            .ok_or_else(|| ctx.make_error("TypeError", "Invalid WebGL canvas"))?;
        if let Some(canvas) = dom.borrow().canvases.borrow().get(&id) {
            if !canvas.webgl {
                return Ok(Value::Bool(false));
            }
            if ctx.host_mut::<HostState>().unwrap().webgl.contains_key(&id) {
                return Ok(Value::Bool(true));
            }
        }
        if ctx.host_mut::<HostState>().unwrap().webgl.len() >= 16 {
            return Ok(Value::Bool(false));
        }
        let attrs = attributes(&n);
        let Some(mut canvas) = crate::canvas::Canvas::new(width, height, attrs.alpha) else {
            return Ok(Value::Bool(false));
        };
        let used = ctx
            .host_mut::<HostState>()
            .unwrap()
            .webgl
            .values()
            .map(Context::allocated_bytes)
            .sum::<usize>();
        match Context::new(
            width,
            height,
            attrs,
            canvas.generation,
            crate::webgl::PAGE_BUDGET.saturating_sub(used),
        ) {
            Ok(context) => {
                canvas.webgl = true;
                canvas.document_origin = text;
                dom.borrow().canvases.borrow_mut().insert(id, canvas);
                ctx.host_mut::<HostState>()
                    .unwrap()
                    .webgl
                    .insert(id, context);
                dom.borrow_mut().canvas_changed(id);
                return Ok(Value::Bool(true));
            }
            Err(error) => {
                if std::env::var_os("TRUST_WEBGL_TRACE").is_some() {
                    eprintln!("WebGL context: {error}");
                }
                return Ok(Value::Bool(false));
            }
        }
    }
    let reply = execute(ctx, id, &op, &n, bytes.as_deref(), &text);
    value(ctx, reply)
}

fn attributes(n: &[f64]) -> Attributes {
    let b = |i: usize, default: bool| n.get(i).map_or(default, |x| *x != 0.);
    Attributes {
        alpha: b(0, true),
        depth: b(1, true),
        stencil: b(2, false),
        premultiplied: b(3, true),
        preserve: b(4, false),
        fail_caveat: b(5, false),
    }
}

/// WebGL 1.0 §2.1 context creation for an OffscreenCanvas (HTML
/// #dom-offscreencanvas-getcontext "webgl"): the drawing buffer takes the
/// OffscreenCanvas's dimensions; its frames become the canvas's bitmap.
fn init_offscreen(
    ctx: &mut Ctx,
    key: usize,
    id: u32,
    n: &[f64],
    text: String,
) -> Result<Value, Value> {
    let state = ctx.host_mut::<HostState>().unwrap();
    let Some(offscreen) = state.offscreen_canvases.get(&id).cloned() else {
        return Err(ctx.make_error("TypeError", "Invalid WebGL canvas"));
    };
    let mut offscreen = offscreen.borrow_mut();
    match offscreen.mode {
        crate::canvas_offscreen::Mode::WebGl if state.webgl.contains_key(&key) => {
            return Ok(Value::Bool(true));
        }
        crate::canvas_offscreen::Mode::None | crate::canvas_offscreen::Mode::WebGl => {}
        _ => return Ok(Value::Bool(false)),
    }
    if state.webgl.len() >= 16 {
        return Ok(Value::Bool(false));
    }
    let attrs = attributes(n);
    let (width, height) = offscreen.bitmap_size();
    let Some(mut canvas) = crate::canvas::Canvas::new(width, height, attrs.alpha) else {
        return Ok(Value::Bool(false));
    };
    let used = state
        .webgl
        .values()
        .map(Context::allocated_bytes)
        .sum::<usize>();
    match Context::new(
        width,
        height,
        attrs,
        canvas.generation,
        crate::webgl::PAGE_BUDGET.saturating_sub(used),
    ) {
        Ok(context) => {
            canvas.webgl = true;
            canvas.document_origin = text;
            offscreen.canvas = Some(canvas);
            offscreen.mode = crate::canvas_offscreen::Mode::WebGl;
            offscreen.dirty = true;
            state.webgl.insert(key, context);
            Ok(Value::Bool(true))
        }
        Err(error) => {
            if std::env::var_os("TRUST_WEBGL_TRACE").is_some() {
                eprintln!("WebGL context: {error}");
            }
            Ok(Value::Bool(false))
        }
    }
}

fn execute(
    ctx: &mut Ctx,
    id: usize,
    op: &str,
    n: &[f64],
    bytes: Option<&[u8]>,
    text: &str,
) -> Reply {
    if offscreen_id(id).is_some() {
        return execute_offscreen(ctx, id, op, n, bytes, text);
    }
    let dom = host_dom(ctx);
    let state = ctx.host_mut::<HostState>().unwrap();
    // The page budget is shared; a lone context has it all.
    let other = if state.webgl.len() > 1 {
        state
            .webgl
            .iter()
            .filter(|(key, _)| **key != id)
            .map(|(_, c)| c.allocated_bytes())
            .sum::<usize>()
    } else {
        0
    };
    let Some(context) = state.webgl.get_mut(&id) else {
        return Reply::Null;
    };
    context.budget = crate::webgl::PAGE_BUDGET.saturating_sub(other);
    if let Some(canvas) = dom.borrow().canvases.borrow().get(&id)
        && canvas.generation != context.generation
    {
        if context.resize(canvas.width, canvas.height).is_err() {
            context.lose();
        }
        context.generation = canvas.generation;
    }
    let was_dirty = context.dirty;
    let result = match op {
        "width" => Reply::Number(context.width as f64),
        "height" => Reply::Number(context.height as f64),
        "attributes" => Reply::Array(
            [
                context.attrs.alpha,
                context.attrs.depth,
                context.attrs.stencil,
                false,
                context.attrs.premultiplied,
                context.attrs.preserve,
            ]
            .into_iter()
            .map(Reply::Bool)
            .collect(),
        ),
        _ => context.execute(op, n, bytes, text),
    };
    // Coalesce a canvas's draw operations until its next presentation. Creation
    // and attribute-driven resizing have already requested their own repaint.
    if !was_dirty && context.dirty {
        dom.borrow_mut().canvas_changed(id);
    }
    result
}

fn execute_offscreen(
    ctx: &mut Ctx,
    key: usize,
    op: &str,
    n: &[f64],
    bytes: Option<&[u8]>,
    text: &str,
) -> Reply {
    let state = ctx.host_mut::<HostState>().unwrap();
    let other = state
        .webgl
        .iter()
        .filter(|(other, _)| **other != key)
        .map(|(_, c)| c.allocated_bytes())
        .sum::<usize>();
    let Some(mut context) = state.webgl.remove(&key) else {
        return Reply::Null;
    };
    context.budget = crate::webgl::PAGE_BUDGET.saturating_sub(other);
    with_canvas(state, key, |canvas| {
        if canvas.generation != context.generation {
            if context.resize(canvas.width, canvas.height).is_err() {
                context.lose();
            }
            context.generation = canvas.generation;
        }
    });
    let was_dirty = context.dirty;
    let result = match op {
        "width" => Reply::Number(context.width as f64),
        "height" => Reply::Number(context.height as f64),
        "attributes" => Reply::Array(
            [
                context.attrs.alpha,
                context.attrs.depth,
                context.attrs.stencil,
                false,
                context.attrs.premultiplied,
                context.attrs.preserve,
            ]
            .into_iter()
            .map(Reply::Bool)
            .collect(),
        ),
        _ => context.execute(op, n, bytes, text),
    };
    let drew = !was_dirty && context.dirty;
    state.webgl.insert(key, context);
    if drew
        && let Some(id) = offscreen_id(key)
        && let Some(offscreen) = state.offscreen_canvases.get(&id)
    {
        offscreen.borrow_mut().dirty = true;
    }
    result
}

fn value(ctx: &mut Ctx, result: Reply) -> Result<Value, Value> {
    Ok(match result {
        Reply::Null => Value::Null,
        Reply::Number(n) => Value::Num(n),
        Reply::Bool(b) => Value::Bool(b),
        Reply::Text(s) => Value::from_string(s),
        Reply::Bytes(bytes) => ctx.make_uint8array(&bytes)?,
        Reply::Array(values) => {
            let values = values
                .into_iter()
                .map(|v| value(ctx, v))
                .collect::<Result<Vec<_>, _>>()?;
            ctx.make_array(values)
        }
    })
}
