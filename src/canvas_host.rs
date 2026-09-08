//! Private Canvas binding. The prelude consumes and deletes the bootstrap hook.
use super::{
    Ctx, HostState, Value, host_arg_node, host_arg_string, host_dom, host_layout_environment,
};
use crate::canvas::Canvas;
use resvg::tiny_skia as sk;
use vello_cpu::kurbo::Affine;

pub(super) fn call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
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
    let text = if let Value::Str(s) = &payload {
        s.to_string()
    } else {
        String::new()
    };
    let bytes = if op == "put" {
        ctx.typed_array_bytes(&payload)
    } else {
        None
    };
    let dom = host_dom(ctx);
    let mut dom = dom.borrow_mut();
    let id = host_arg_node(&dom, args, 0)
        .ok_or_else(|| ctx.make_error("TypeError", "Invalid canvas"))?;
    let (width, height) = dom
        .canvas_size(id)
        .ok_or_else(|| ctx.make_error("TypeError", "Invalid canvas"))?;
    if op == "size" {
        return Ok(ctx.make_array(vec![Value::Num(width as f64), Value::Num(height as f64)]));
    }
    if op == "clean" {
        return Ok(Value::Bool(
            dom.canvases
                .borrow()
                .get(&id)
                .is_none_or(|canvas| canvas.origin_clean),
        ));
    }
    if op == "init" {
        let mut canvases = dom.canvases.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(entry) = canvases.entry(id) {
            let Some(mut canvas) =
                Canvas::new(width, height, n.first().copied().unwrap_or(1.) != 0.)
            else {
                return Ok(Value::Bool(false));
            };
            canvas.document_origin = text;
            entry.insert(canvas);
        }
        return Ok(Value::Bool(true));
    }
    if op == "url" {
        let mut canvases = dom.canvases.borrow_mut();
        let url = if let Some(canvas) = canvases.get_mut(&id) {
            canvas.data_url()
        } else {
            Canvas::new(width, height, true)
                .map_or_else(|| "data:,".to_string(), |mut canvas| canvas.data_url())
        };
        return Ok(Value::from_string(url));
    }
    let source = if op == "draw" {
        let origin = dom
            .canvases
            .borrow()
            .get(&id)
            .map_or_else(String::new, |canvas| canvas.document_origin.clone());
        n.first()
            .and_then(|source| source_bitmap(ctx, &dom, *source as usize, &origin))
    } else {
        None
    };
    let mut canvases = dom.canvases.borrow_mut();
    let canvas = canvases
        .get_mut(&id)
        .ok_or_else(|| ctx.make_error("TypeError", "Canvas context not initialized"))?;
    let mut changed = false;
    let result = match op.as_str() {
        "generation" => Value::Num(canvas.generation as f64),
        "lost" => Value::Bool(canvas.lost()),
        "reset" => {
            let _ = canvas.resize(width, height);
            changed = true;
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
            let count = match op.as_str() {
                "closePath" => 0,
                "moveTo" | "lineTo" => 2,
                "rect" | "quadraticCurveTo" => 4,
                "arcTo" => 5,
                "bezierCurveTo" => 6,
                "roundRect" => 12,
                _ => 8,
            };
            if n.len() >= count {
                canvas.path_command(&op, &n);
            }
            Value::Undefined
        }
        "fill" | "stroke" => {
            canvas.paint_path(op == "stroke", text == "evenodd");
            changed = true;
            Value::Undefined
        }
        "fillPath" | "strokePath" | "clipPath" => {
            canvas.external_path(&text, &op, n.first() == Some(&1.));
            changed = op != "clipPath";
            Value::Undefined
        }
        "clip" => {
            canvas.clip(text == "evenodd");
            Value::Undefined
        }
        "fillRect" | "strokeRect" | "clearRect" => {
            if n.len() >= 4 {
                canvas.rectangle(&op, &n);
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
            if n.len() >= 10
                && let Some(bytes) = bytes
            {
                canvas.put(
                    &bytes,
                    n[0] as u32,
                    n[1] as u32,
                    &n[2..],
                    n[8] != 0.,
                    n[9] as u8,
                );
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
                if !ctx.typed_array_set_bytes(&payload, &bytes) {
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
                    _ => return Ok(Value::Undefined),
                };
                canvas.draw(&source, &rect);
                if rect[2] != 0. && rect[3] != 0. {
                    canvas.origin_clean &= clean;
                }
                changed = true;
            }
            Value::Undefined
        }
        "setStyle" => {
            if let Some((color, serialized)) = Canvas::color(&text) {
                if n.first() == Some(&1.) {
                    canvas.state.stroke_color = color;
                    canvas.state.stroke_text = serialized;
                } else {
                    canvas.state.fill = color;
                    canvas.state.fill_text = serialized;
                }
            }
            Value::Undefined
        }
        "getStyle" => Value::from_string(if n.first() == Some(&1.) {
            canvas.state.stroke_text.clone()
        } else {
            canvas.state.fill_text.clone()
        }),
        "setComposite" => {
            if crate::canvas::blend(&text).is_some() {
                canvas.state.composite = text;
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
            canvas.state.dash = n;
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
        _ => return Err(ctx.make_error("TypeError", format!("Unknown canvas operation: {op}"))),
    };
    drop(canvases);
    if changed {
        dom.canvas_changed(id);
    }
    Ok(result)
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
        let state = ctx.host_mut::<HostState>()?;
        let bytes = state.blobs.lock().ok()?.get(&source)?.0.clone();
        let origin = url::Url::parse(&source).ok()?.origin();
        (
            bytes,
            destination_origin != "null" && origin.ascii_serialization() == destination_origin,
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
    let mut bitmap = sk::Pixmap::new(image.width, image.height)?;
    for (out, pixel) in bitmap
        .data_mut()
        .chunks_exact_mut(4)
        .zip(image.rgba.chunks_exact(4))
    {
        for channel in 0..3 {
            out[channel] = ((pixel[channel] as u32 * pixel[3] as u32 + 127) / 255) as u8;
        }
        out[3] = pixel[3];
    }
    Some((bitmap, clean))
}
