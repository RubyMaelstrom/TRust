//! Private, byte-oriented WebGL host boundary. JS bindings perform Web IDL
//! conversions and brand checks before any driver state is borrowed.
use super::{Ctx, HostState, Value, host_arg_node, host_arg_string, host_dom};
use crate::webgl::{Attributes, Context, Reply};

pub(super) fn publish(ctx: &mut Ctx, present: bool, only: Option<usize>) {
    let Some(state) = ctx.host_mut::<HostState>() else {
        return;
    };
    let dom = state.dom.borrow();
    let mut canvases = dom.canvases.borrow_mut();
    let mut allocated = state
        .webgl
        .values()
        .map(Context::allocated_bytes)
        .sum::<usize>();
    for (id, context) in &mut state.webgl {
        if only.is_some_and(|only| *id != only) {
            continue;
        }
        allocated -= context.allocated_bytes();
        context.budget = crate::webgl::PAGE_BUDGET.saturating_sub(allocated);
        let Some(canvas) = canvases.get_mut(id) else {
            allocated += context.allocated_bytes();
            continue;
        };
        if context.generation != canvas.generation {
            if context.resize(canvas.width, canvas.height).is_err() {
                context.lose();
            }
            context.generation = canvas.generation;
        }
        allocated += context.allocated_bytes();
        if (!present || context.dirty)
            && let Some(pixels) = context.snapshot(present)
            && canvas.width > 0
            && canvas.height > 0
        {
            canvas.publish_webgl(context.width, context.height, pixels, present);
        }
    }
}

pub(super) fn call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let op = host_arg_string(ctx, args, 1);
    let payload = args.get(3).cloned().unwrap_or(Value::Undefined);
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
        let count = ctx.member_get(values, "length")?.as_num_opt().unwrap_or(0.) as usize;
        if count > 1_048_576 {
            return Err(ctx.make_error("RangeError", "WebGL argument budget exceeded"));
        }
        for i in 0..count {
            let v = ctx.member_get(values, &i.to_string())?;
            n.push(ctx.coerce_number(&v)?);
        }
    }
    let text = if let Value::Str(s) = &payload {
        s.to_string()
    } else {
        String::new()
    };
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
    let id = host_arg_node(&dom.borrow(), args, 0)
        .ok_or_else(|| ctx.make_error("TypeError", "Invalid WebGL canvas"))?;
    if op == "dispose" {
        ctx.host_mut::<HostState>().unwrap().webgl.remove(&id);
        if let Some(canvas) = dom.borrow().canvases.borrow_mut().get_mut(&id) {
            canvas.publish_webgl(0, 0, vec![], true);
        }
        return Ok(Value::Undefined);
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
        let b = |i: usize, default: bool| n.get(i).map_or(default, |x| *x != 0.);
        let attrs = Attributes {
            alpha: b(0, true),
            depth: b(1, true),
            stencil: b(2, false),
            premultiplied: b(3, true),
            preserve: b(4, false),
            fail_caveat: b(5, false),
        };
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
    let state = ctx.host_mut::<HostState>().unwrap();
    let other = state
        .webgl
        .iter()
        .filter(|(key, _)| **key != id)
        .map(|(_, c)| c.allocated_bytes())
        .sum::<usize>();
    let Some(context) = state.webgl.get_mut(&id) else {
        return Ok(Value::Null);
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
    let result = match op.as_str() {
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
        _ => context.execute(&op, &n, bytes.as_deref(), &text),
    };
    if context.dirty
        && matches!(
            op.as_str(),
            "clear"
                | "drawArrays"
                | "drawElements"
                | "drawArraysInstancedANGLE"
                | "drawElementsInstancedANGLE"
        )
    {
        dom.borrow_mut().canvas_changed(id);
    }
    value(ctx, result)
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
