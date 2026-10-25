//! Geometry Interfaces 1 #DOMRect: private numeric platform-object state.
//!
//! One Agent-wide registry supplies Web IDL brands across Window Realms. A
//! rectangle owns no DOM/layout snapshot and the registry holds only weak JS
//! identities, so a geometry read cannot retain a page or obsolete fragments.
//! Weak-table maintenance is generational and amortized, not a full census on
//! every rectangle allocation. Native functions' JS captures are engine-traced.

use super::{
    Ctx, HostGcVisitor, HostRetainedMemoryVisitor, HostState, RetainedManagedAllocation, Value,
    ValueKind, hold_weakly,
};
use rustc_hash::FxHashMap;
use trust_js::embed::WeakHandle;

const COMPACT_FLOOR: usize = 256;
const ATTRIBUTES: [&str; 8] = [
    "x", "y", "width", "height", "top", "right", "bottom", "left",
];

#[derive(Clone, Copy)]
struct Rectangle {
    values: [f64; 4],
    mutable: bool,
    /// SVG 2 types.html#InterfaceSVGAnimatedRect: a rectangle that reflects
    /// a `viewBox` attribute. Its writes first run the platform's
    /// `svgRectWrite` steps (read-only check, attribute reserialization).
    observed: bool,
}

impl Rectangle {
    fn attribute(self, index: usize) -> f64 {
        let [x, y, width, height] = self.values;
        let edge = |a: f64, b: f64, maximum: bool| {
            // Geometry 1 #conventions: NaN propagates. Rust's min/max alone
            // intentionally ignore one NaN, unlike the specified operation.
            if a.is_nan() || b.is_nan() {
                f64::NAN
            } else if a == 0.0 && b == 0.0 {
                // Preserve the ECMAScript min/max ordering of signed zeros.
                if maximum {
                    if a.is_sign_positive() || b.is_sign_positive() {
                        0.0
                    } else {
                        -0.0
                    }
                } else if a.is_sign_negative() || b.is_sign_negative() {
                    -0.0
                } else {
                    0.0
                }
            } else if maximum {
                a.max(b)
            } else {
                a.min(b)
            }
        };
        match index {
            0..=3 => self.values[index],
            4 => edge(y, y + height, false),
            5 => edge(x, x + width, true),
            6 => edge(y, y + height, true),
            7 => edge(x, x + width, false),
            _ => unreachable!("private attribute index"),
        }
    }
}

struct Entry {
    owner: WeakHandle,
    rectangle: Rectangle,
}

/// A Realm's DOMRectReadOnly and DOMRect prototypes, reported as edges from its global.
struct RealmPrototypes {
    global: WeakHandle,
    values: [WeakHandle; 2],
}

/// Rectangles by object identity, and each Realm's prototypes by its global's. Both name their
/// objects weakly; a collection's sweep forgets the dead ones before their cells (and so their
/// identities) are freed, so an identity never names a dead object's entry.
#[derive(Default)]
pub(super) struct Registry {
    realms: FxHashMap<usize, RealmPrototypes>,
    entries: FxHashMap<usize, Entry>,
}

fn shrink<T>(entries: &mut FxHashMap<usize, T>) {
    if entries.capacity() > entries.len().saturating_mul(4).max(COMPACT_FLOOR) {
        entries.shrink_to(entries.len().saturating_mul(2).max(COMPACT_FLOOR));
    }
}

impl Registry {
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    fn insert(&mut self, key: usize, owner: WeakHandle, rectangle: Rectangle) {
        debug_assert!(!self.entries.contains_key(&key));
        self.entries.insert(key, Entry { owner, rectangle });
    }

    fn get(&self, key: usize) -> Option<Rectangle> {
        self.entries.get(&key).map(|entry| entry.rectangle)
    }

    fn get_mut(&mut self, key: usize) -> Option<&mut Rectangle> {
        self.entries.get_mut(&key).map(|entry| &mut entry.rectangle)
    }

    pub(super) fn trace(&self, visitor: &mut dyn HostGcVisitor) {
        // Interface prototypes are intrinsic to their Realm even if author
        // code replaces the global constructor. They are edges FROM the global,
        // never roots: a retired unreachable Realm must still be collectible.
        for realm in self.realms.values() {
            for prototype in &realm.values {
                visitor.edge(&realm.global, prototype);
            }
        }
    }

    /// Forget what a collection found dead, after it cleared the weak handles.
    pub(super) fn sweep(&mut self) {
        self.realms.retain(|_, realm| !realm.global.is_cleared());
        shrink(&mut self.realms);
        self.entries.retain(|_, entry| !entry.owner.is_cleared());
        shrink(&mut self.entries);
    }

    pub(super) fn scan_retained_memory(&self, visitor: &mut dyn HostRetainedMemoryVisitor) {
        if self.realms.capacity() != 0 {
            visitor.allocation(RetainedManagedAllocation::new(
                "trust.geometry.realms",
                &self.realms as *const _ as usize,
                self.realms.capacity() * std::mem::size_of::<(usize, RealmPrototypes)>(),
            ));
            visitor.opaque_storage();
        }
        if self.entries.capacity() != 0 {
            visitor.allocation(RetainedManagedAllocation::new(
                "trust.geometry.entries",
                &self.entries as *const _ as usize,
                self.entries.capacity() * std::mem::size_of::<(usize, Entry)>(),
            ));
            // Hash-table allocation layouts are opaque; these requested bytes
            // are a lower bound.
            visitor.opaque_storage();
        }
    }
}

fn record<'s>(
    ctx: &mut Ctx<'s>,
    value: &Value<'s>,
    mutable: bool,
) -> Result<(usize, Rectangle), Value<'s>> {
    let key = ctx.object_addr(value);
    let rectangle = key.and_then(|key| {
        ctx.host_mut::<HostState>()
            .expect("geometry host")
            .geometry
            .get(key)
    });
    match (key, rectangle) {
        (Some(key), Some(rectangle)) if !mutable || rectangle.mutable => Ok((key, rectangle)),
        _ => Err(ctx.make_error("TypeError", "Illegal DOMRect receiver")),
    }
}

fn register<'s>(
    ctx: &mut Ctx<'s>,
    value: &Value<'s>,
    rectangle: Rectangle,
) -> Result<(), Value<'s>> {
    let Some(key) = ctx.object_addr(value) else {
        return Err(ctx.make_error("TypeError", "DOMRect must be an object"));
    };
    let owner = hold_weakly(ctx, value);
    ctx.host_mut::<HostState>()
        .expect("geometry host")
        .geometry
        .insert(key, owner, rectangle);
    Ok(())
}

fn construct<'s>(
    ctx: &mut Ctx<'s>,
    _: Value<'s>,
    args: &[Value<'s>],
    captures: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    // Web IDL #interface-object: convert arguments before looking up the
    // NewTarget prototype. Native construction avoids JS class allocation and
    // superclass calls, both observably different from this algorithm.
    if !ctx.is_constructing() {
        return Err(ctx.make_error("TypeError", "DOMRect constructor requires new"));
    }
    let new_target = ctx.constructor_new_target();
    let mut values = [0.0; 4];
    for (index, number) in values.iter_mut().enumerate() {
        if let Some(value) = args
            .get(index)
            .filter(|value| !matches!(value.kind(), ValueKind::Undefined))
        {
            *number = ctx.coerce_number(value)?;
        }
    }
    let mutable = matches!(captures[0].kind(), ValueKind::Bool(true));
    let mut prototype = ctx.member_get(&new_target, "prototype")?;
    if ctx.object_addr(&prototype).is_none() {
        let global = ctx.callable_realm_global(&new_target)?;
        let key = ctx.object_addr(&global).expect("Realm global is an object");
        let handle = ctx
            .host_mut::<HostState>()
            .expect("geometry host")
            .geometry
            .realms
            .get(&key)
            .expect("geometry is exposed in each Window/Worker Realm")
            .values[usize::from(mutable)]
        .clone();
        prototype = handle
            .upgrade(ctx)
            .ok()
            .flatten()
            .expect("a Realm's prototypes live while its global does");
    }
    let value = ctx.new_object_with_proto(&prototype);
    register(
        ctx,
        &value,
        Rectangle {
            values,
            mutable,
            observed: false,
        },
    )?;
    Ok(value)
}

fn serialization_state<'s>(
    ctx: &mut Ctx<'s>,
    _: Value<'s>,
    args: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    // HTML #structuredserializeinternal / Geometry 1 #structured-serialization:
    // inspect the primary interface and numeric slots, never author properties.
    let value = args.first().unwrap_or(&Value::Undefined);
    // A proxy has no platform brand, and HTML serialization rejects it without
    // calling any author trap. Undefined is private codec's rejection sentinel.
    if ctx.is_proxy_value(value) {
        return Ok(Value::Undefined);
    }
    let rectangle = ctx.object_addr(value).and_then(|key| {
        ctx.host_mut::<HostState>()
            .expect("geometry host")
            .geometry
            .get(key)
    });
    let Some(rectangle) = rectangle else {
        return Ok(Value::Null);
    };
    Ok(ctx.make_array(vec![
        Value::Bool(rectangle.mutable),
        Value::Num(rectangle.values[0]),
        Value::Num(rectangle.values[1]),
        Value::Num(rectangle.values[2]),
        Value::Num(rectangle.values[3]),
    ]))
}

fn get<'s>(
    ctx: &mut Ctx<'s>,
    this: Value<'s>,
    _: &[Value<'s>],
    captures: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    let index = captures[0].as_num_opt().unwrap() as usize;
    let (_, rectangle) = record(
        ctx,
        &this,
        matches!(captures[1].kind(), ValueKind::Bool(true)),
    )?;
    Ok(Value::Num(rectangle.attribute(index)))
}

fn set<'s>(
    ctx: &mut Ctx<'s>,
    this: Value<'s>,
    args: &[Value<'s>],
    captures: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    // Web IDL #dfn-attribute-setter: check the interface brand before running
    // observable ToNumber. No native borrow survives that reentrant operation.
    let (key, rectangle) = record(ctx, &this, true)?;
    let number = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))?;
    let index = captures[0].as_num_opt().unwrap() as usize;
    if rectangle.observed {
        // An SVGRect reflecting `viewBox` (SVG 2 #InterfaceSVGAnimatedRect)
        // throws for a read-only animVal and reserializes the attribute.
        super::host_call_trust(
            ctx,
            "svgRectWrite",
            &[this, Value::Num(index as f64), Value::Num(number)],
        )?;
    }
    if let Some(rectangle) = ctx
        .host_mut::<HostState>()
        .expect("geometry host")
        .geometry
        .get_mut(key)
    {
        rectangle.values[index] = number;
    }
    Ok(Value::Undefined)
}

/// `updateRect(rect, x, y, width, height, observed)`: the platform updates
/// a reflecting SVGRect from its attribute without running its write steps,
/// and marks it observed.
fn update<'s>(ctx: &mut Ctx<'s>, _: Value<'s>, args: &[Value<'s>]) -> Result<Value<'s>, Value<'s>> {
    let (key, _) = record(ctx, args.first().unwrap_or(&Value::Undefined), false)?;
    let values =
        std::array::from_fn(|i| args.get(i + 1).and_then(Value::as_num_opt).unwrap_or(0.0));
    let observed = matches!(
        args.get(5).map(|value| value.kind()),
        Some(ValueKind::Bool(true))
    );
    if let Some(rectangle) = ctx
        .host_mut::<HostState>()
        .expect("geometry host")
        .geometry
        .get_mut(key)
    {
        rectangle.values = values;
        rectangle.observed = observed;
    }
    Ok(Value::Undefined)
}

fn snapshot<'s>(
    ctx: &mut Ctx<'s>,
    _: Value<'s>,
    args: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    let (_, rectangle) = record(ctx, args.first().unwrap_or(&Value::Undefined), false)?;
    Ok(ctx.make_array(
        (0..8)
            .map(|index| Value::Num(rectangle.attribute(index)))
            .collect(),
    ))
}

fn create<'s>(
    ctx: &mut Ctx<'s>,
    _: Value<'s>,
    args: &[Value<'s>],
    captures: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    // Internal layout numbers are already IDL doubles, not author values.
    let values = std::array::from_fn(|i| args.get(i).and_then(Value::as_num_opt).unwrap_or(0.0));
    let value = ctx.new_object_with_proto(&captures[0]);
    register(
        ctx,
        &value,
        Rectangle {
            values,
            mutable: matches!(captures[1].kind(), ValueKind::Bool(true)),
            observed: false,
        },
    )?;
    Ok(value)
}

pub(super) fn bind<'s>(
    ctx: &mut Ctx<'s>,
    _: Value<'s>,
    args: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    let readonly = args.first().cloned().unwrap_or(Value::Null);
    let mutable = args.get(1).cloned().unwrap_or(Value::Null);
    let global = ctx.global_this();
    let key = ctx.object_addr(&global).unwrap();
    let prototypes = RealmPrototypes {
        global: hold_weakly(ctx, &global),
        values: [hold_weakly(ctx, &readonly), hold_weakly(ctx, &mutable)],
    };
    ctx.host_mut::<HostState>()
        .expect("geometry host")
        .geometry
        .realms
        .insert(key, prototypes);
    let readonly_ctor =
        ctx.new_native_fn_with_captures("DOMRectReadOnly", 0, construct, &[Value::Bool(false)]);
    let mutable_ctor =
        ctx.new_native_fn_with_captures("DOMRect", 0, construct, &[Value::Bool(true)]);
    ctx.set_constructor_prototype(&readonly_ctor, &readonly);
    ctx.set_constructor_prototype(&mutable_ctor, &mutable);
    for (index, name) in ATTRIBUTES.iter().enumerate() {
        let getter = |ctx: &Ctx<'s>, needs_mutable| {
            ctx.new_native_fn_with_captures(
                &format!("get {name}"),
                0,
                get,
                &[Value::Num(index as f64), Value::Bool(needs_mutable)],
            )
        };
        ctx.define_accessor_value(&readonly, name, Some(getter(ctx, false)), None, true);
        if index < 4 {
            let setter = ctx.new_native_fn_with_captures(
                &format!("set {name}"),
                1,
                set,
                &[Value::Num(index as f64)],
            );
            ctx.define_accessor_value(&mutable, name, Some(getter(ctx, true)), Some(setter), true);
        }
    }
    Ok(ctx.make_array(vec![
        readonly_ctor,
        mutable_ctor,
        Value::Obj(ctx.make_native("snapshotRect", 1, snapshot)),
        ctx.new_native_fn_with_captures("createDOMRect", 4, create, &[mutable, Value::Bool(true)]),
        Value::Obj(ctx.make_native("serializeRect", 1, serialization_state)),
        ctx.new_native_fn_with_captures(
            "createDOMRectReadOnly",
            4,
            create,
            &[readonly, Value::Bool(false)],
        ),
        Value::Obj(ctx.make_native("updateRect", 6, update)),
    ]))
}

/// Geometry 1 #dommatrix-parse for a Window Realm's DOMMatrix(ReadOnly)
/// constructor and setMatrixValue(): `null` for failure, otherwise
/// `[is2D, m11, m12, …, m44]` in column-major order.
pub(super) fn parse_matrix<'s>(
    ctx: &mut Ctx<'s>,
    _: Value<'s>,
    args: &[Value<'s>],
) -> Result<Value<'s>, Value<'s>> {
    let text = super::host_arg_string(ctx, args, 0);
    let Some((matrix, two_dimensional)) = crate::dom::css_transform_list_matrix(&text) else {
        return Ok(Value::Null);
    };
    let mut values = Vec::with_capacity(17);
    values.push(Value::Bool(two_dimensional));
    values.extend(matrix.into_iter().map(Value::Num));
    Ok(ctx.make_array(values))
}
