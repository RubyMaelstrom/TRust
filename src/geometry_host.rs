//! Geometry Interfaces 1 #DOMRect: private numeric platform-object state.
//!
//! One Agent-wide registry supplies Web IDL brands across Window Realms. A
//! rectangle owns no DOM/layout snapshot and the registry holds only weak JS
//! identities, so a geometry read cannot retain a page or obsolete fragments.
//! Weak-table maintenance is generational and amortized, not a full census on
//! every rectangle allocation. Native functions' JS captures are engine-traced.

use super::{
    Ctx, HostGcVisitor, HostRetainedMemoryVisitor, HostState, RetainedManagedAllocation, Value,
};
use lumen::embed::WeakValue;
use rustc_hash::FxHashMap;
use std::cell::Cell;

const COMPACT_FLOOR: usize = 256;
const ATTRIBUTES: [&str; 8] = [
    "x", "y", "width", "height", "top", "right", "bottom", "left",
];

#[derive(Clone, Copy)]
struct Rectangle {
    values: [f64; 4],
    mutable: bool,
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
    owner: WeakValue,
    rectangle: Rectangle,
}

struct RealmPrototypes {
    global: WeakValue,
    values: [Value; 2],
}

pub(super) struct Registry {
    realms: FxHashMap<usize, RealmPrototypes>,
    young_realms: Vec<usize>,
    young: FxHashMap<usize, Entry>,
    old: FxHashMap<usize, Entry>,
    next_compaction: usize,
    registrations: usize,
    next_old_compaction: usize,
    minor: Cell<bool>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            realms: Default::default(),
            young_realms: Vec::new(),
            young: Default::default(),
            old: Default::default(),
            next_compaction: COMPACT_FLOOR,
            registrations: 0,
            next_old_compaction: COMPACT_FLOOR,
            minor: Cell::new(false),
        }
    }
}

fn shrink<T>(entries: &mut FxHashMap<usize, T>) {
    if entries.capacity() > entries.len().saturating_mul(4).max(COMPACT_FLOOR) {
        entries.shrink_to(entries.len().saturating_mul(2).max(COMPACT_FLOOR));
    }
}

impl Registry {
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.young.len() + self.old.len()
    }

    fn insert(&mut self, key: usize, owner: WeakValue, rectangle: Rectangle) {
        if self.young.len() >= self.next_compaction {
            self.young
                .retain(|_, entry| entry.owner.upgrade().is_some());
            shrink(&mut self.young);
            self.next_compaction = self.young.len().saturating_mul(2).max(COMPACT_FLOOR);
        }
        self.registrations += 1;
        if self.registrations >= self.next_old_compaction {
            self.old.retain(|_, entry| entry.owner.upgrade().is_some());
            shrink(&mut self.old);
            self.registrations = 0;
            self.next_old_compaction = self.old.len().max(COMPACT_FLOOR);
        }
        // Keeping the WeakValue also prevents recycling the Rc allocation's
        // identity before this entry is retired. No address-only stale brand.
        debug_assert!(!self.old.contains_key(&key) && !self.young.contains_key(&key));
        self.young.insert(key, Entry { owner, rectangle });
    }

    fn get(&self, key: usize) -> Option<Rectangle> {
        self.young
            .get(&key)
            .or_else(|| self.old.get(&key))
            .map(|entry| entry.rectangle)
    }

    fn get_mut(&mut self, key: usize) -> Option<&mut Rectangle> {
        self.young
            .get_mut(&key)
            .or_else(|| self.old.get_mut(&key))
            .map(|entry| &mut entry.rectangle)
    }

    pub(super) fn trace(&self, visitor: &mut dyn HostGcVisitor) {
        self.minor.set(visitor.is_minor());
        // Interface prototypes are intrinsic to their Realm even if author
        // code replaces the global constructor. They are edges FROM the global,
        // never roots: a retired unreachable Realm must still be collectible.
        let mut trace_realm = |realm: &RealmPrototypes| {
            let global = realm.global.upgrade();
            for prototype in &realm.values {
                visitor.internal(prototype);
                if let Some(global) = &global {
                    visitor.edge(global, prototype);
                }
            }
        };
        if self.minor.get() {
            // Prototype handles are immutable after bootstrap. Once promoted,
            // both they and their Realm are old; the engine owns barriers for
            // author writes to the prototype objects themselves. Do not walk
            // every stable old Realm on each nursery collection.
            for key in &self.young_realms {
                if let Some(realm) = self.realms.get(key) {
                    trace_realm(realm);
                }
            }
        } else {
            for realm in self.realms.values() {
                trace_realm(realm);
            }
        }
    }

    pub(super) fn sweep(&mut self, is_live: &dyn Fn(&Value) -> bool) {
        let keep_realm = |realm: &RealmPrototypes| {
            realm
                .global
                .upgrade()
                .is_some_and(|global| is_live(&global))
        };
        if self.minor.get() {
            for key in &self.young_realms {
                if self.realms.get(key).is_some_and(|realm| !keep_realm(realm)) {
                    self.realms.remove(key);
                }
            }
        } else {
            self.realms.retain(|_, realm| keep_realm(realm));
            shrink(&mut self.realms);
        }
        self.young_realms.clear();
        if self.young_realms.capacity() > COMPACT_FLOOR {
            self.young_realms.shrink_to(COMPACT_FLOOR);
        }
        let keep = |_: &usize, entry: &mut Entry| {
            entry.owner.upgrade().is_some_and(|value| is_live(&value))
        };
        if !self.minor.get() {
            self.old.retain(keep);
            self.registrations = 0;
        }
        self.young.retain(keep);
        self.old.extend(self.young.drain());
        shrink(&mut self.old);
        shrink(&mut self.young);
        self.next_compaction = COMPACT_FLOOR;
        self.next_old_compaction = self.old.len().max(COMPACT_FLOOR);
    }

    pub(super) fn scan_retained_memory(&self, visitor: &mut dyn HostRetainedMemoryVisitor) {
        if self.young_realms.capacity() != 0 {
            visitor.allocation(RetainedManagedAllocation::new(
                "trust.geometry.young-realms",
                self.young_realms.as_ptr() as usize,
                self.young_realms.capacity() * std::mem::size_of::<usize>(),
            ));
        }
        if self.realms.capacity() != 0 {
            visitor.allocation(RetainedManagedAllocation::new(
                "trust.geometry.realms",
                &self.realms as *const _ as usize,
                self.realms.capacity() * std::mem::size_of::<(usize, RealmPrototypes)>(),
            ));
            visitor.opaque_storage();
        }
        for realm in self.realms.values() {
            for value in &realm.values {
                visitor.value(value);
            }
        }
        for (name, entries) in [
            ("trust.geometry.young", &self.young),
            ("trust.geometry.old", &self.old),
        ] {
            if entries.capacity() != 0 {
                visitor.allocation(RetainedManagedAllocation::new(
                    name,
                    entries as *const _ as usize,
                    entries.capacity() * std::mem::size_of::<(usize, Entry)>(),
                ));
                // Hash-table/control-block allocation layouts are opaque;
                // these requested bytes are a lower bound, not JS roots.
                visitor.opaque_storage();
            }
        }
    }
}

fn record(ctx: &mut Ctx, value: &Value, mutable: bool) -> Result<(usize, Rectangle), Value> {
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

fn register(ctx: &mut Ctx, value: &Value, rectangle: Rectangle) -> Result<(), Value> {
    let Some(key) = ctx.object_addr(value) else {
        return Err(ctx.make_error("TypeError", "DOMRect must be an object"));
    };
    let owner = ctx
        .downgrade_object_value(value)
        .expect("object identity has a weak handle");
    ctx.host_mut::<HostState>()
        .expect("geometry host")
        .geometry
        .insert(key, owner, rectangle);
    Ok(())
}

fn construct(ctx: &mut Ctx, _: Value, args: &[Value], captures: &[Value]) -> Result<Value, Value> {
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
            .filter(|value| !matches!(value, Value::Undefined))
        {
            *number = ctx.coerce_number(value)?;
        }
    }
    let mutable = matches!(captures[0], Value::Bool(true));
    let mut prototype = ctx.member_get(&new_target, "prototype")?;
    if ctx.object_addr(&prototype).is_none() {
        let global = ctx.callable_realm_global(&new_target)?;
        let key = ctx.object_addr(&global).expect("Realm global is an object");
        prototype = ctx
            .host_mut::<HostState>()
            .expect("geometry host")
            .geometry
            .realms
            .get(&key)
            .expect("geometry is exposed in each Window/Worker Realm")
            .values[usize::from(mutable)]
        .clone();
    }
    let value = ctx.new_object_with_proto(&prototype);
    register(ctx, &value, Rectangle { values, mutable })?;
    Ok(value)
}

fn serialization_state(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
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

fn get(ctx: &mut Ctx, this: Value, _: &[Value], captures: &[Value]) -> Result<Value, Value> {
    let index = captures[0].as_num_opt().unwrap() as usize;
    let (_, rectangle) = record(ctx, &this, matches!(captures[1], Value::Bool(true)))?;
    Ok(Value::Num(rectangle.attribute(index)))
}

fn set(ctx: &mut Ctx, this: Value, args: &[Value], captures: &[Value]) -> Result<Value, Value> {
    // Web IDL #dfn-attribute-setter: check the interface brand before running
    // observable ToNumber. No native borrow survives that reentrant operation.
    let (key, _) = record(ctx, &this, true)?;
    let number = ctx.coerce_number(args.first().unwrap_or(&Value::Undefined))?;
    let index = captures[0].as_num_opt().unwrap() as usize;
    ctx.host_mut::<HostState>()
        .expect("geometry host")
        .geometry
        .get_mut(key)
        .unwrap()
        .values[index] = number;
    Ok(Value::Undefined)
}

fn snapshot(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let (_, rectangle) = record(ctx, args.first().unwrap_or(&Value::Undefined), false)?;
    Ok(ctx.make_array(
        (0..8)
            .map(|index| Value::Num(rectangle.attribute(index)))
            .collect(),
    ))
}

fn create(ctx: &mut Ctx, _: Value, args: &[Value], captures: &[Value]) -> Result<Value, Value> {
    // Internal layout numbers are already IDL doubles, not author values.
    let values = std::array::from_fn(|i| args.get(i).and_then(Value::as_num_opt).unwrap_or(0.0));
    let value = ctx.new_object_with_proto(&captures[0]);
    register(
        ctx,
        &value,
        Rectangle {
            values,
            mutable: matches!(captures[1], Value::Bool(true)),
        },
    )?;
    Ok(value)
}

pub(super) fn bind(ctx: &mut Ctx, _: Value, args: &[Value]) -> Result<Value, Value> {
    let readonly = args.first().cloned().unwrap_or(Value::Null);
    let mutable = args.get(1).cloned().unwrap_or(Value::Null);
    let global = ctx.global_this();
    let key = ctx.object_addr(&global).unwrap();
    let owner = ctx.downgrade_object_value(&global).unwrap();
    ctx.host_mut::<HostState>()
        .expect("geometry host")
        .geometry
        .young_realms
        .push(key);
    ctx.host_mut::<HostState>()
        .expect("geometry host")
        .geometry
        .realms
        .insert(
            key,
            RealmPrototypes {
                global: owner,
                values: [readonly.clone(), mutable.clone()],
            },
        );
    let readonly_ctor =
        ctx.new_native_fn_with_captures("DOMRectReadOnly", 0, construct, vec![Value::Bool(false)]);
    let mutable_ctor =
        ctx.new_native_fn_with_captures("DOMRect", 0, construct, vec![Value::Bool(true)]);
    ctx.set_constructor_prototype(&readonly_ctor, &readonly);
    ctx.set_constructor_prototype(&mutable_ctor, &mutable);
    for (index, name) in ATTRIBUTES.iter().enumerate() {
        let getter = |ctx: &Ctx, needs_mutable| {
            ctx.new_native_fn_with_captures(
                &format!("get {name}"),
                0,
                get,
                vec![Value::Num(index as f64), Value::Bool(needs_mutable)],
            )
        };
        ctx.define_accessor_value(&readonly, name, Some(getter(ctx, false)), None, true);
        if index < 4 {
            let setter = ctx.new_native_fn_with_captures(
                &format!("set {name}"),
                1,
                set,
                vec![Value::Num(index as f64)],
            );
            ctx.define_accessor_value(&mutable, name, Some(getter(ctx, true)), Some(setter), true);
        }
    }
    Ok(ctx.make_array(vec![
        readonly_ctor,
        mutable_ctor,
        Value::Obj(ctx.make_native("snapshotRect", 1, snapshot)),
        ctx.new_native_fn_with_captures(
            "createDOMRect",
            4,
            create,
            vec![mutable, Value::Bool(true)],
        ),
        Value::Obj(ctx.make_native("serializeRect", 1, serialization_state)),
        ctx.new_native_fn_with_captures(
            "createDOMRectReadOnly",
            4,
            create,
            vec![readonly, Value::Bool(false)],
        ),
    ]))
}
