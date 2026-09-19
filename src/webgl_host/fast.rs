//! Native entries for common WebGL calls. Web IDL #js-operations still controls
//! branding, arity, and left-to-right conversion. Only arguments whose conversion
//! cannot run author code take this path; the complete JS binding handles all
//! other values, lost contexts, foreign objects, and sequence overloads.
use super::{Ctx, HostState, Reply, Value, execute};
use std::rc::Rc;

#[derive(Clone, Copy)]
enum Method {
    Numbers(&'static str, &'static [u8]),
    BindBuffer,
    AttribPointer,
    BindVertexArray,
    Uniform {
        width: usize,
        integer: bool,
        matrix: bool,
        vector: bool,
    },
}

pub(super) fn wrap(ctx: &mut Ctx, fallback: Value) -> Result<Value, Value> {
    let name = ctx.member_get(&fallback, "name")?;
    let Value::Str(name) = name else {
        return Ok(fallback);
    };
    let method = match name.as_ref() {
        "bindBuffer" => Method::BindBuffer,
        "vertexAttribPointer" => Method::AttribPointer,
        "bindVertexArrayOES" => Method::BindVertexArray,
        "drawArrays" => Method::Numbers("drawArrays", b"uii"),
        "drawElements" => Method::Numbers("drawElements", b"uiul"),
        "enableVertexAttribArray" => Method::Numbers("enableVertexAttribArray", b"u"),
        "disableVertexAttribArray" => Method::Numbers("disableVertexAttribArray", b"u"),
        name => {
            let Some(suffix) = name.strip_prefix("uniform") else {
                return Ok(fallback);
            };
            let matrix = suffix.starts_with("Matrix");
            let suffix = suffix.strip_prefix("Matrix").unwrap_or(suffix);
            let Some(width) = suffix
                .as_bytes()
                .first()
                .and_then(|c| c.checked_sub(b'0'))
                .filter(|w| (1..=4).contains(w))
            else {
                return Ok(fallback);
            };
            let kind = &suffix[1..];
            if !matches!(kind, "f" | "fv" | "i" | "iv") {
                return Ok(fallback);
            }
            Method::Uniform {
                width: width as usize,
                integer: kind.starts_with('i'),
                matrix,
                vector: kind.ends_with('v'),
            }
        }
    };
    let arity = match method {
        Method::Numbers(_, sig) => sig.len(),
        Method::BindBuffer => 2,
        Method::AttribPointer => 6,
        Method::BindVertexArray => 1,
        Method::Uniform { matrix: true, .. } => 3,
        Method::Uniform { vector: true, .. } => 2,
        Method::Uniform { width, .. } => width + 1,
    };
    // JS stores native-function -> fallback in its private WeakMap. Holding only
    // a weak handle here lets a retired Window's methods and Realm be collected;
    // the function's reachable ephemeron retains its fallback while it is callable.
    let fallback = ctx.downgrade_object_value(&fallback).unwrap();
    Ok(ctx.new_native_fn(
        &name,
        arity,
        Rc::new(move |ctx, this, args| {
            if args.len() >= arity
                && let Some(result) = run(ctx, &this, args, method)?
            {
                return Ok(result);
            }
            let fallback = fallback.upgrade().ok_or_else(|| {
                ctx.make_error("TypeError", "WebGL binding is no longer available")
            })?;
            ctx.invoke(fallback, this, args)
        }),
    ))
}

fn number(value: &Value, kind: u8) -> Option<f64> {
    if kind == b'b' {
        return if let Value::Bool(value) = value {
            Some(f64::from(*value))
        } else {
            None
        };
    }
    let Value::Num(value) = value else {
        return None;
    };
    Some(match kind {
        b'f' => *value as f32 as f64,
        b'l' => {
            if value.is_finite() {
                value.trunc()
            } else {
                0.
            }
        }
        b'u' | b'i' => {
            // Web IDL normal integer conversion / ECMA-262 ToUint32.
            let integer = if value.is_finite() {
                value.trunc().rem_euclid(4_294_967_296.) as u32
            } else {
                0
            };
            if kind == b'i' {
                integer as i32 as f64
            } else {
                integer as f64
            }
        }
        _ => return None,
    })
}

fn resource(
    ctx: &mut Ctx,
    slots: &Value,
    this: &Value,
    epoch: f64,
    value: &Value,
    kind: &str,
) -> Result<Option<u32>, Value> {
    if matches!(value, Value::Null | Value::Undefined) {
        return Ok(Some(0));
    }
    let record = ctx.weak_map_get(slots, value)?;
    if !matches!(record, Value::Obj(_)) {
        return Ok(None);
    }
    if !matches!(ctx.member_get(&record, "kind")?, Value::Str(ref s) if s.as_ref() == kind) {
        return Ok(None);
    }
    let owner = ctx.member_get(&record, "context")?;
    if !ctx.values_strict_equal(&owner, this)
        || ctx.member_get(&record, "epoch")?.as_num_opt() != Some(epoch)
    {
        return Ok(None);
    }
    Ok(ctx
        .member_get(&record, "id")?
        .as_num_opt()
        .map(|n| n as u32))
}

fn run(
    ctx: &mut Ctx,
    this: &Value,
    args: &[Value],
    method: Method,
) -> Result<Option<Value>, Value> {
    let slots = ctx
        .host_mut::<HostState>()
        .unwrap()
        .webgl_slots
        .clone()
        .unwrap();
    let record = ctx.weak_map_get(&slots, this)?;
    if matches!(method, Method::BindVertexArray) {
        return bind_vertex_array(ctx, &slots, &record, &args[0]);
    }
    if !matches!(record, Value::Obj(_)) {
        return Ok(None);
    }
    if !matches!(ctx.member_get(&record, "kind")?, Value::Str(ref s) if s.as_ref() == "Context")
        || !matches!(ctx.member_get(&record, "lost")?, Value::Bool(false))
    {
        return Ok(None);
    }
    let Some(id) = ctx.member_get(&record, "id")?.as_num_opt() else {
        return Ok(None);
    };
    let id = id as usize;
    let Some(epoch) = ctx.member_get(&record, "epoch")?.as_num_opt() else {
        return Ok(None);
    };
    match method {
        Method::Numbers(op, sig) => {
            let mut numbers = [0.; 6];
            for (index, kind) in sig.iter().enumerate() {
                let Some(value) = number(&args[index], *kind) else {
                    return Ok(None);
                };
                numbers[index] = value;
            }
            execute(ctx, id, op, &numbers[..sig.len()], None, "");
        }
        Method::BindBuffer => {
            let Some(target) = number(&args[0], b'u') else {
                return Ok(None);
            };
            let Some(buffer) = resource(ctx, &slots, this, epoch, &args[1], "Buffer")? else {
                return Ok(None);
            };
            if matches!(execute(ctx, id, "bindBuffer", &[target, buffer as f64], None, ""), Reply::Number(n) if n == buffer as f64)
            {
                let refs = ctx.member_get(
                    &record,
                    if target as u32 == 34963 {
                        "vertexRefs"
                    } else {
                        "refs"
                    },
                )?;
                ctx.map_set(
                    &refs,
                    Value::from_string(format!("Buffer:{}", target as u32)),
                    args[1].clone(),
                )?;
            }
        }
        Method::AttribPointer => {
            let mut numbers = [0.; 6];
            for (index, kind) in b"uiubil".iter().enumerate() {
                let Some(value) = number(&args[index], *kind) else {
                    return Ok(None);
                };
                numbers[index] = value;
            }
            let binding = ctx
                .host_mut::<HostState>()
                .unwrap()
                .webgl
                .get(&id)
                .map(|c| c.array_buffer_binding());
            let Some(binding) = binding else {
                return Ok(None);
            };
            let refs = ctx.member_get(&record, "refs")?;
            let buffer = if binding == 0 {
                Value::Null
            } else {
                let value = ctx.map_get(&refs, &Value::str("Buffer:34962"))?;
                if resource(ctx, &slots, this, epoch, &value, "Buffer")? != Some(binding) {
                    return Ok(None);
                }
                value
            };
            if let Reply::Number(_) = execute(ctx, id, "vertexAttribPointer", &numbers, None, "") {
                let refs = ctx.member_get(&record, "vertexRefs")?;
                ctx.map_set(
                    &refs,
                    Value::from_string(format!("attrib{}", numbers[0] as u32)),
                    buffer,
                )?;
            }
        }
        Method::Uniform {
            width,
            integer,
            matrix,
            vector,
        } => {
            let Some(location) = resource(ctx, &slots, this, epoch, &args[0], "UniformLocation")?
            else {
                return Ok(None);
            };
            let transpose = if matrix {
                let Value::Bool(v) = args[1] else {
                    return Ok(None);
                };
                v
            } else {
                false
            };
            let size = if matrix { width * width } else { width };
            let mut numbers = vec![
                location as f64,
                size as f64,
                f64::from(integer),
                f64::from(matrix),
            ];
            if vector {
                let data = &args[if matrix { 2 } else { 1 }];
                let Some(bytes) = ctx.fixed_typed_array_bytes(
                    data,
                    if integer {
                        "Int32Array"
                    } else {
                        "Float32Array"
                    },
                    true,
                ) else {
                    return Ok(None);
                };
                if bytes.len() / 4 > 1_048_572 {
                    return Err(ctx.make_error("RangeError", "WebGL argument budget exceeded"));
                }
                numbers.extend(bytes.as_chunks::<4>().0.iter().map(|word| {
                    if integer {
                        i32::from_ne_bytes(*word) as f64
                    } else {
                        f32::from_ne_bytes(*word) as f64
                    }
                }));
            } else {
                for argument in &args[1..=width] {
                    let Some(value) = number(argument, if integer { b'i' } else { b'f' }) else {
                        return Ok(None);
                    };
                    numbers.push(value);
                }
            }
            if location != 0 {
                if transpose {
                    execute(ctx, id, "error", &[1281.], None, "");
                } else {
                    execute(ctx, id, "uniform", &numbers, None, "");
                }
            }
        }
        Method::BindVertexArray => unreachable!(),
    }
    Ok(Some(Value::Undefined))
}

fn bind_vertex_array(
    ctx: &mut Ctx,
    slots: &Value,
    extension: &Value,
    value: &Value,
) -> Result<Option<Value>, Value> {
    if !matches!(extension, Value::Obj(_))
        || !matches!(ctx.member_get(extension, "kind")?, Value::Str(ref s) if s.as_ref() == "VertexArrayExtension")
    {
        return Ok(None);
    }
    let owner = ctx.member_get(extension, "context")?;
    let record = ctx.weak_map_get(slots, &owner)?;
    if !matches!(ctx.member_get(&record, "lost")?, Value::Bool(false)) {
        return Ok(None);
    }
    let Some(epoch) = ctx.member_get(&record, "epoch")?.as_num_opt() else {
        return Ok(None);
    };
    if ctx.member_get(extension, "epoch")?.as_num_opt() != Some(epoch) {
        return Ok(None);
    }
    let Some(array) = resource(ctx, slots, &owner, epoch, value, "VertexArrayObjectOES")? else {
        return Ok(None);
    };
    let Some(id) = ctx.member_get(&record, "id")?.as_num_opt() else {
        return Ok(None);
    };
    if matches!(execute(ctx, id as usize, "bindVertexArrayOES", &[array as f64], None, ""), Reply::Number(n) if n == array as f64)
    {
        let refs = ctx.member_get(&record, "refs")?;
        ctx.map_set(&refs, Value::str("vertexArray"), value.clone())?;
        let vertex_refs = if array == 0 {
            ctx.member_get(&record, "defaultVertexRefs")?
        } else {
            let array = ctx.weak_map_get(slots, value)?;
            ctx.member_get(&array, "refs")?
        };
        ctx.member_set(&record, "vertexRefs", vertex_refs)?;
    }
    Ok(Some(Value::Undefined))
}
