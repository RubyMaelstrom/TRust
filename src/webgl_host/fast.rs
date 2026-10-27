//! Native entries for common WebGL calls. Web IDL #js-operations still controls
//! branding, arity, and left-to-right conversion. Only arguments whose conversion
//! cannot run author code take this path; the complete JS binding handles all
//! other values, lost contexts, foreign objects, and sequence overloads.
use super::super::{held, hold_weakly};
use super::{Ctx, HostState, Reply, Value, ValueKind, execute};
use rustc_hash::FxHashMap;
use trust_js::embed::WeakHandle;

/// Memoized brand checks of the fast path, by object address. Each entry names
/// its object weakly; the host's collection sweep forgets the entries of dead
/// objects before their addresses can be reused (`Cache::sweep`). Context records
/// change only through the host's `init` and `dispose` operations, which
/// forget them; resource records never change after creation and are checked
/// against their context's current epoch.
#[derive(Default)]
pub(in crate::trust_js_backend) struct Cache {
    contexts: FxHashMap<usize, CachedContext>,
    resources: FxHashMap<usize, CachedResource>,
}

struct CachedContext {
    object: WeakHandle,
    id: usize,
    epoch: f64,
}

struct CachedResource {
    object: WeakHandle,
    kind: &'static str,
    owner: usize,
    epoch: f64,
    id: u32,
}

/// Bounds for pages that create many contexts or resources; a cleared cache
/// only means the next calls take the JS record path again.
const MAX_CACHED_CONTEXTS: usize = 64;
const MAX_CACHED_RESOURCES: usize = 8192;

impl Cache {
    /// A context was lost or restored: its record's `lost` and `epoch` changed.
    pub(in crate::trust_js_backend) fn forget_contexts(&mut self) {
        self.contexts.clear();
    }

    /// Forget the entries of objects a collection found dead, after it cleared their handles.
    pub(in crate::trust_js_backend) fn sweep(&mut self) {
        self.contexts.retain(|_, entry| !entry.object.is_cleared());
        self.resources.retain(|_, entry| !entry.object.is_cleared());
    }

    /// The tables' own storage.
    pub(in crate::trust_js_backend) fn retained_bytes(&self) -> usize {
        self.contexts.capacity() * size_of::<(usize, CachedContext)>()
            + self.resources.capacity() * size_of::<(usize, CachedResource)>()
    }
}

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

pub(super) fn wrap<'s>(ctx: &mut Ctx<'s>, fallback: Value<'s>) -> Result<Value<'s>, Value<'s>> {
    let name = ctx.member_get(&fallback, "name")?;
    let ValueKind::Str(name) = name.kind() else {
        return Ok(fallback);
    };
    let name = name.as_engine_text().into_owned();
    let method = match name.as_str() {
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
    let fallback = hold_weakly(ctx, &fallback);
    Ok(ctx.new_native_fn(
        &name,
        arity,
        move |ctx, this, args| {
            if args.len() >= arity
                && let Some(result) = run(ctx, &this, args, method)?
            {
                return Ok(result);
            }
            let fallback = fallback.upgrade(ctx).ok().flatten().ok_or_else(|| {
                ctx.make_error("TypeError", "WebGL binding is no longer available")
            })?;
            ctx.invoke(fallback, this, args)
        },
        trust_js::embed::NativeCaptures::None,
    ))
}

fn number<'s>(value: &Value<'s>, kind: u8) -> Option<f64> {
    if kind == b'b' {
        return if let ValueKind::Bool(value) = value.kind() {
            Some(f64::from(value))
        } else {
            None
        };
    }
    let ValueKind::Num(value) = value.kind() else {
        return None;
    };
    Some(match kind {
        b'f' => value as f32 as f64,
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

fn resource<'s>(
    ctx: &mut Ctx<'s>,
    slots: &Value<'s>,
    this: &Value<'s>,
    epoch: f64,
    value: &Value<'s>,
    kind: &'static str,
) -> Result<Option<u32>, Value<'s>> {
    if matches!(value.kind(), ValueKind::Null | ValueKind::Undefined) {
        return Ok(Some(0));
    }
    let (Some(address), Some(owner)) = (ctx.object_addr(value), ctx.object_addr(this)) else {
        return Ok(None);
    };
    let cache = &ctx.host_mut::<HostState>().unwrap().webgl_fast;
    if let Some(cached) = cache.resources.get(&address)
        && !cached.object.is_cleared()
    {
        return Ok(
            (cached.kind == kind && cached.owner == owner && cached.epoch == epoch)
                .then_some(cached.id),
        );
    }
    let Some(id) = resource_record(ctx, slots, this, epoch, value, kind)? else {
        return Ok(None);
    };
    if value.as_obj().is_some() {
        let object = hold_weakly(ctx, value);
        let cache = &mut ctx.host_mut::<HostState>().unwrap().webgl_fast;
        if cache.resources.len() >= MAX_CACHED_RESOURCES {
            cache.resources.clear();
        }
        cache.resources.insert(
            address,
            CachedResource {
                object,
                kind,
                owner,
                epoch,
                id,
            },
        );
    }
    Ok(Some(id))
}

/// The id of a WebGL object or location record of `kind` owned by the
/// context `this` in its current `epoch`.
fn resource_record<'s>(
    ctx: &mut Ctx<'s>,
    slots: &Value<'s>,
    this: &Value<'s>,
    epoch: f64,
    value: &Value<'s>,
    kind: &str,
) -> Result<Option<u32>, Value<'s>> {
    let record = ctx.weak_map_get(slots, value)?;
    if !matches!(record.kind(), ValueKind::Obj(_)) {
        return Ok(None);
    }
    if !matches!(ctx.member_get(&record, "kind")?.kind(), ValueKind::Str(s) if s.as_engine_text() == kind)
    {
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

fn run<'s>(
    ctx: &mut Ctx<'s>,
    this: &Value<'s>,
    args: &[Value<'s>],
    method: Method,
) -> Result<Option<Value<'s>>, Value<'s>> {
    let slots = ctx
        .host_mut::<HostState>()
        .unwrap()
        .webgl_slots
        .clone()
        .unwrap();
    let slots = held(ctx, &slots);
    if matches!(method, Method::BindVertexArray) {
        let record = ctx.weak_map_get(&slots, this)?;
        return bind_vertex_array(ctx, &slots, &record, &args[0]);
    }
    let Some((id, epoch)) = context(ctx, &slots, this)? else {
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
                let record = ctx.weak_map_get(&slots, this)?;
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
                    ctx.string(format!("Buffer:{}", target as u32)),
                    args[1],
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
            let record = ctx.weak_map_get(&slots, this)?;
            let refs = ctx.member_get(&record, "refs")?;
            let buffer = if binding == 0 {
                Value::Null
            } else {
                let value = ctx.map_get(&refs, &ctx.str("Buffer:34962"))?;
                if resource(ctx, &slots, this, epoch, &value, "Buffer")? != Some(binding) {
                    return Ok(None);
                }
                value
            };
            if let Reply::Number(_) = execute(ctx, id, "vertexAttribPointer", &numbers, None, "") {
                let refs = ctx.member_get(&record, "vertexRefs")?;
                ctx.map_set(
                    &refs,
                    ctx.string(format!("attrib{}", numbers[0] as u32)),
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
                let ValueKind::Bool(v) = args[1].kind() else {
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

/// The id and epoch of the live WebGL context `this`, from its private record.
fn context<'s>(
    ctx: &mut Ctx<'s>,
    slots: &Value<'s>,
    this: &Value<'s>,
) -> Result<Option<(usize, f64)>, Value<'s>> {
    let Some(address) = ctx.object_addr(this) else {
        return Ok(None);
    };
    let cache = &ctx.host_mut::<HostState>().unwrap().webgl_fast;
    if let Some(cached) = cache.contexts.get(&address)
        && !cached.object.is_cleared()
    {
        return Ok(Some((cached.id, cached.epoch)));
    }
    let record = ctx.weak_map_get(slots, this)?;
    if !matches!(record.kind(), ValueKind::Obj(_)) {
        return Ok(None);
    }
    if !matches!(ctx.member_get(&record, "kind")?.kind(), ValueKind::Str(s) if s.as_engine_text() == "Context")
        || !matches!(
            ctx.member_get(&record, "lost")?.kind(),
            ValueKind::Bool(false)
        )
    {
        return Ok(None);
    }
    let Some(id) = ctx.member_get(&record, "id")?.as_num_opt() else {
        return Ok(None);
    };
    let Some(epoch) = ctx.member_get(&record, "epoch")?.as_num_opt() else {
        return Ok(None);
    };
    let id = id as usize;
    if this.as_obj().is_some() {
        let object = hold_weakly(ctx, this);
        let cache = &mut ctx.host_mut::<HostState>().unwrap().webgl_fast;
        if cache.contexts.len() >= MAX_CACHED_CONTEXTS {
            cache.contexts.clear();
        }
        cache
            .contexts
            .insert(address, CachedContext { object, id, epoch });
    }
    Ok(Some((id, epoch)))
}

fn bind_vertex_array<'s>(
    ctx: &mut Ctx<'s>,
    slots: &Value<'s>,
    extension: &Value<'s>,
    value: &Value<'s>,
) -> Result<Option<Value<'s>>, Value<'s>> {
    if !matches!(extension.kind(), ValueKind::Obj(_))
        || !matches!(ctx.member_get(extension, "kind")?.kind(), ValueKind::Str(s) if s.as_engine_text() == "VertexArrayExtension")
    {
        return Ok(None);
    }
    let owner = ctx.member_get(extension, "context")?;
    let record = ctx.weak_map_get(slots, &owner)?;
    if !matches!(
        ctx.member_get(&record, "lost")?.kind(),
        ValueKind::Bool(false)
    ) {
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
        ctx.map_set(&refs, ctx.str("vertexArray"), *value)?;
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
