use crate::{
    AsContext,
    AsContextMut,
    Error,
    Func,
    RefType,
    StoreContext,
    Tag,
    TrapCode,
    Val,
    core::RawRef,
    engine::{Cell, LiftFromCells as _, LowerToCells as _, required_cells_for_tys},
    handle::RawHandle,
    store::{AsStoreId, ExnEntity, Stored},
};
use alloc::{boxed::Box, vec::Vec};
use core::{any::Any, num::NonZero};

/// A nullable reference type.
#[derive(Debug, Default, Copy, Clone)]
pub enum Nullable<T> {
    /// The [`Ref`] is `null`.
    #[default]
    Null,
    /// The [`Ref`] is a non-`null` value.
    Val(T),
}

impl<T> Nullable<T> {
    /// Returns `true` is `self` is null.
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Returns `Some` if `self` is a non-`null` value.
    ///
    /// Otherwise returns `None`.
    pub fn val(&self) -> Option<&T> {
        match self {
            Self::Val(val) => Some(val),
            Self::Null => None,
        }
    }

    /// Converts from `&Ref<T>` to `Ref<&T>`.
    pub fn as_ref(&self) -> Nullable<&T> {
        match self {
            Self::Val(val) => Nullable::Val(val),
            Self::Null => Nullable::Null,
        }
    }
}

impl<T> From<T> for Nullable<T> {
    fn from(value: T) -> Self {
        Self::Val(value)
    }
}

impl<T> From<Nullable<T>> for Option<T> {
    fn from(nullable: Nullable<T>) -> Self {
        match nullable {
            Nullable::Val(value) => Some(value),
            Nullable::Null => None,
        }
    }
}

/// A Wasm reference.
#[derive(Debug, Copy, Clone)]
pub enum Ref {
    /// A Wasm `funcref`.
    Func(Nullable<Func>),
    /// A Wasm `externref`.
    Extern(Nullable<ExternRef>),
    /// TRust: a Wasm `exnref`.
    Exn(Nullable<ExnRef>),
}

impl From<Nullable<Func>> for Ref {
    fn from(value: Nullable<Func>) -> Self {
        Self::Func(value)
    }
}

impl From<Nullable<ExternRef>> for Ref {
    fn from(value: Nullable<ExternRef>) -> Self {
        Self::Extern(value)
    }
}

impl From<Nullable<ExnRef>> for Ref {
    fn from(value: Nullable<ExnRef>) -> Self {
        Self::Exn(value)
    }
}

impl Ref {
    /// Create a [`Ref`] from its raw parts.
    pub(crate) fn from_raw_parts(val: RawRef, ty: RefType, store: impl AsStoreId) -> Self {
        match ty {
            RefType::Func => Ref::Func(<Nullable<Func>>::from_raw_parts(val, store)),
            RefType::Extern => Ref::Extern(<Nullable<ExternRef>>::from_raw_parts(val, store)),
            RefType::Exn => Ref::Exn(<Nullable<ExnRef>>::from_raw_parts(val, store)),
        }
    }

    /// Creates new default value of given type.
    #[inline]
    pub fn default_for_ty(ty: RefType) -> Self {
        match ty {
            RefType::Func => Self::from(<Nullable<Func>>::Null),
            RefType::Extern => Self::from(<Nullable<ExternRef>>::Null),
            RefType::Exn => Self::from(<Nullable<ExnRef>>::Null),
        }
    }

    /// Returns `true` if `self` is a `null` rerefence.
    #[inline]
    pub fn is_null(&self) -> bool {
        match self {
            Self::Func(nullable) => nullable.is_null(),
            Self::Extern(nullable) => nullable.is_null(),
            Self::Exn(nullable) => nullable.is_null(),
        }
    }

    /// Returns `true` if `self` is a `null` rerefence.
    #[inline]
    pub fn is_non_null(&self) -> bool {
        !self.is_null()
    }

    /// Creates a new `null` reference of type `ty`.
    #[inline]
    pub fn null(ty: RefType) -> Self {
        match ty {
            RefType::Extern => Self::Extern(Nullable::Null),
            RefType::Func => Self::Func(Nullable::Null),
            RefType::Exn => Self::Exn(Nullable::Null),
        }
    }

    /// Returns the [`RefType`] of `self`.
    #[inline]
    pub fn ty(&self) -> RefType {
        match self {
            Self::Func(_) => RefType::Func,
            Self::Extern(_) => RefType::Extern,
            Self::Exn(_) => RefType::Exn,
        }
    }

    /// Returns `true` if `self` is a `funcref`.
    #[inline]
    pub fn is_func(&self) -> bool {
        matches!(self, Self::Func(_))
    }

    /// Returns `true` if `self` is a `externref`.
    #[inline]
    pub fn is_extern(&self) -> bool {
        matches!(self, Self::Extern(_))
    }

    /// Returns `Some` if `self` is a `funcref`.
    ///
    /// Otherwise returns `None`.
    #[inline]
    pub fn as_func(&self) -> Option<Nullable<&Func>> {
        if let Self::Func(nullable) = self {
            return Some(nullable.as_ref());
        }
        None
    }

    /// Returns `Some` if `self` is a `externref`.
    ///
    /// Otherwise returns `None`.
    #[inline]
    pub fn as_extern(&self) -> Option<Nullable<&ExternRef>> {
        if let Self::Extern(nullable) = self {
            return Some(nullable.as_ref());
        }
        None
    }

    /// TRust: returns `Some` if `self` is an `exnref`.
    ///
    /// Otherwise returns `None`.
    #[inline]
    pub fn as_exn(&self) -> Option<Nullable<&ExnRef>> {
        if let Self::Exn(nullable) = self {
            return Some(nullable.as_ref());
        }
        None
    }

    /// Get the underlying [`Func`] reference.
    ///
    /// # Note
    ///
    /// - Returns `None` if this `Ref` is a null `func` reference.
    /// - Returns `Some(_)` if this `Ref` is a non-null `func` reference.
    ///
    /// # Panics
    ///
    /// If `self` is another kind of reference.
    #[inline]
    pub fn unwrap_func(&self) -> Nullable<&Func> {
        self.as_func()
            .expect("`Ref::unwrap_func` on non-func reference")
    }
}

/// An externally defined object.
#[derive(Debug)]
pub struct ExternRefEntity {
    inner: Box<dyn 'static + Any + Send + Sync>,
}

impl ExternRefEntity {
    /// Creates a new instance of `ExternRef` wrapping the given value.
    pub fn new<T>(object: T) -> Self
    where
        T: 'static + Any + Send + Sync,
    {
        Self {
            inner: Box::new(object),
        }
    }

    /// Returns a shared reference to the external object.
    pub fn data(&self) -> &dyn Any {
        &*self.inner
    }
}

define_handle! {
    /// Represents an opaque reference to any data within WebAssembly.
    struct ExternRef(NonZero<u32>, Stored) => ExternRefEntity;
}

impl ExternRef {
    /// Creates a new instance of `ExternRef` wrapping the given value.
    pub fn new<T>(mut ctx: impl AsContextMut, object: T) -> Self
    where
        T: 'static + Any + Send + Sync,
    {
        ctx.as_context_mut()
            .store
            .inner
            .alloc_extern_object(ExternRefEntity::new(object))
    }

    /// Returns a shared reference to the underlying data for this [`ExternRef`].
    ///
    /// # Panics
    ///
    /// Panics if `ctx` does not own this [`ExternRef`].
    pub fn data<'a, T: 'a>(&self, ctx: impl Into<StoreContext<'a, T>>) -> &'a dyn Any {
        ctx.into().store.inner.resolve_externref(self).data()
    }
}

/// TRust: the marker entity of an [`ExnRef`].
///
/// # Note
///
/// The exception instances themselves live in the store's exception arena, which also
/// reclaims those that became unreachable (see `ExnStore`).
#[derive(Debug)]
pub struct ExnRefEntity;

define_handle! {
    /// TRust: a reference to an exception instance of a [`Store`](crate::Store) (`exnref`).
    ///
    /// WebAssembly Core 3.0, exec/runtime: an exception reference addresses an exception
    /// instance, made of a tag and its field values, which `throw_ref` raises again.
    ///
    /// # Note
    ///
    /// The exception that a Wasm result, a host function parameter or a global or table read
    /// hands out stays alive as long as the store. Exceptions that the host allocates with
    /// [`ExnRef::new`] or receives as an uncaught exception ([`Error::as_exception`]) are held
    /// instead, and the store may free them once the host released its holds and Wasm no
    /// longer references them.
    struct ExnRef(NonZero<u32>, Stored) => ExnRefEntity;
}

impl ExnRef {
    /// TRust: allocates a new exception of `tag` with the tag fields `fields` (`exn_alloc`,
    /// WebAssembly Core 3.0 `appendix/embedding.rst` `embed-exn-alloc`).
    ///
    /// The new exception is held once (see [`ExnRef::hold`]): it stays allocated until that
    /// hold is released, for example by [`ExnRef::release`] or by throwing it from a host
    /// function with [`Error::exception`].
    ///
    /// # Errors
    ///
    /// - If `tag` or a value of `fields` does not originate from `ctx`.
    /// - If the types of `fields` do not match the parameter types of `tag`.
    /// - If the store cannot allocate another exception.
    pub fn new(mut ctx: impl AsContextMut, tag: &Tag, fields: &[Val]) -> Result<Self, Error> {
        let store = &mut ctx.as_context_mut().store.inner;
        let ty = store
            .try_resolve_tag(tag)
            .map_err(|error| Error::new(alloc::format!("invalid exception tag: {error}")))?
            .ty()
            .clone();
        ty.func_type().match_params(fields)?;
        let len_cells = usize::from(required_cells_for_tys(ty.params())?);
        let mut cells: Vec<Cell> = (0..len_cells).map(|_| Cell::from(0_u64)).collect();
        fields
            .lower_to_cells(&*store, &mut &mut cells[..])
            .map_err(|error| Error::new(alloc::format!("invalid exception field: {error}")))?;
        // Host allocations collect as well: exceptions that the host released must not
        // accumulate while Wasm catches nothing by reference.
        store.collect_exns_if_due();
        let raw = store
            .exns_mut()
            .alloc(ExnEntity::new(*tag, cells))
            .ok_or(TrapCode::OutOfSystemMemory)?;
        store.exns().hold(RawRef::from(raw.get()));
        Ok(Self::from_raw_parts(raw, &*store))
    }

    /// TRust: returns the tag of the exception (`exn_tag`).
    ///
    /// Returns `None` if `self` does not originate from `ctx` or the exception was freed.
    pub fn tag(&self, ctx: impl AsContext) -> Option<Tag> {
        let store = &ctx.as_context().store.inner;
        let raw = self.unwrap_raw(store)?;
        Some(store.exns().get(raw)?.tag())
    }

    /// TRust: returns the tag fields of the exception (`exn_read`).
    ///
    /// Returns `None` if `self` does not originate from `ctx` or the exception was freed.
    pub fn fields(&self, ctx: impl AsContext) -> Option<Vec<Val>> {
        let store = &ctx.as_context().store.inner;
        let raw = self.unwrap_raw(store)?;
        let entity = store.exns().get(raw)?;
        let ty = store.try_resolve_tag(&entity.tag()).ok()?.ty();
        let mut fields: Vec<Val> = ty
            .params()
            .iter()
            .copied()
            .map(Val::default_for_ty)
            .collect();
        (&mut fields[..])
            .lift_from_cells(store, &mut &entity.fields()[..])
            .ok()?;
        // The host may keep the exception references it reads.
        store.pin_exn_vals(&fields);
        Some(fields)
    }

    /// TRust: returns the tag field at `index` of the exception (one value of `exn_read`).
    ///
    /// Returns `None` if `self` does not originate from `ctx`, the exception was freed or
    /// `index` is out of bounds.
    pub fn field(&self, ctx: impl AsContext, index: usize) -> Option<Val> {
        let store = &ctx.as_context().store.inner;
        let raw = self.unwrap_raw(store)?;
        let entity = store.exns().get(raw)?;
        let params = store.try_resolve_tag(&entity.tag()).ok()?.ty().params();
        let ty = *params.get(index)?;
        let offset = usize::from(required_cells_for_tys(&params[..index]).ok()?);
        let mut field = Val::default_for_ty(ty);
        (&mut field)
            .lift_from_cells(store, &mut &entity.fields()[offset..])
            .ok()?;
        store.pin_exn_vals(core::slice::from_ref(&field));
        Some(field)
    }

    /// TRust: adds a releasable hold of the exception, which keeps it allocated until a
    /// matching [`ExnRef::release`].
    ///
    /// Returns `false` if `self` does not originate from `ctx` or the exception was freed.
    pub fn hold(&self, ctx: impl AsContext) -> bool {
        let store = &ctx.as_context().store.inner;
        self.unwrap_raw(store)
            .is_some_and(|raw| store.exns().hold(raw))
    }

    /// TRust: releases one hold of the exception (see [`ExnRef::hold`]).
    ///
    /// The store frees the exception once it is neither held nor reachable from Wasm.
    pub fn release(&self, ctx: impl AsContext) {
        let store = &ctx.as_context().store.inner;
        if let Some(raw) = self.unwrap_raw(store) {
            store.exns().release(raw);
        }
    }

    /// TRust: returns the exception address of `self`: a nonzero number that identifies the
    /// exception while it is allocated, for embedder caches keyed by exception address (for
    /// example the WebAssembly JavaScript Interface's Exception object cache).
    ///
    /// Returns `None` if `self` does not originate from `ctx`.
    pub fn addr(&self, ctx: impl AsContext) -> Option<u32> {
        self.unwrap_raw(&ctx.as_context().store.inner)
            .map(u32::from)
    }

    /// TRust: returns the reference of the allocated exception at the address `addr` (see
    /// [`ExnRef::addr`]), or `None` if no exception of `ctx` is allocated there.
    pub fn from_addr(ctx: impl AsContext, addr: u32) -> Option<Self> {
        let store = &ctx.as_context().store.inner;
        store.exns().get(RawRef::from(addr))?;
        Some(Self::from_raw_parts(NonZero::new(addr)?, store))
    }
}

#[test]
fn externref_sizeof() {
    // These assertions are important in order to convert `FuncRef`
    // from and to 64-bit `RawValue` instances.
    //
    // The following equation must be true:
    //     size_of(ExternRef) == size_of(ExternObject) == size_of(RawValue)
    use core::mem::size_of;
    assert_eq!(size_of::<ExternRef>(), size_of::<u64>());
    assert_eq!(size_of::<ExternRef>(), size_of::<ExternRef>());
}

#[test]
fn externref_null_to_zero() {
    use crate::Store;
    let store = <Store<()>>::default();
    let null = <Nullable<ExternRef>>::Null;
    assert_eq!(null.unwrap_raw(&store), Some(RawRef::from(0)));
    assert!(<Nullable<ExternRef>>::from_raw_parts(RawRef::from(0), &store).is_null());
}

#[test]
fn funcref_sizeof() {
    // These assertions are important in order to convert `FuncRef`
    // from and to 64-bit `RawValue` instances.
    //
    // The following equation must be true:
    //     size_of(Func) == size_of(RawValue) == size_of(FuncRef)
    use crate::Func;
    use core::mem::size_of;
    assert_eq!(size_of::<Func>(), size_of::<u64>());
    assert_eq!(size_of::<Func>(), size_of::<Nullable<Func>>());
}

#[test]
fn funcref_null_to_zero() {
    use crate::{Func, Store};
    let store = <Store<()>>::default();
    let null = <Nullable<Func>>::Null;
    assert_eq!(null.unwrap_raw(&store), Some(RawRef::from(0)));
    assert!(<Nullable<Func>>::from_raw_parts(RawRef::from(0), &store).is_null());
}

macro_rules! impl_conversions {
    ( $( $reftype:ty: $ty:ident ),* $(,)? ) => {
        $(
            impl $reftype {
                /// Unwraps the underlying [`RawRef`] if `self` originates from `store`.
                ///
                /// Otherwise returns `None`.
                pub(crate) fn unwrap_raw(&self, store: impl AsStoreId) -> Option<RawRef> {
                    use crate::Handle as _;
                    let value = store.unwrap(self.as_raw())?;
                    Some(RawRef::from(value.raw().get()))
                }

                #[doc = concat!("Create a [`", stringify!($reftype), "`] from its raw parts.")]
                pub(crate) fn from_raw_parts(value: NonZero<u32>, store: impl AsStoreId) -> Self {
                    let raw_handle = <RawHandle<$reftype>>::new(value);
                    <$reftype as $crate::Handle>::from_raw(store.wrap(raw_handle))
                }
            }

            impl Nullable<$reftype> {
                /// Unwraps the underlying [`RawRef`] if `self` originates from `store`.
                ///
                /// Otherwise returns `None`.
                pub(crate) fn unwrap_raw(&self, store: impl AsStoreId) -> Option<RawRef> {
                    match self {
                        Self::Null => Some(RawRef::from(0_u32)),
                        Self::Val(value) => <$reftype>::unwrap_raw(value, store),
                    }
                }

                #[doc = concat!("Create a [`Nullable<", stringify!($reftype), ">`] from its raw parts.")]
                pub(crate) fn from_raw_parts(val: RawRef, store: impl AsStoreId) -> Self {
                    match <NonZero<u32>>::new(u32::from(val)) {
                        Some(value) => Self::Val(<$reftype>::from_raw_parts(value, store)),
                        None => Self::Null,
                    }
                }
            }
        )*
    };
}
impl_conversions! {
    ExternRef: Extern,
    ExnRef: Exn,
    Func: Func,
}

impl Ref {
    /// Unwraps the underlying [`RawRef`] if `self` originates from `store`.
    ///
    /// Otherwise returns `None`.
    pub(crate) fn unwrap_raw(&self, store: impl AsStoreId) -> Option<RawRef> {
        match self {
            Self::Func(value) => value.unwrap_raw(store),
            Self::Extern(value) => value.unwrap_raw(store),
            Self::Exn(value) => value.unwrap_raw(store),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Store};

    #[test]
    fn it_works() {
        let engine = Engine::default();
        let mut store = <Store<()>>::new(&engine, ());
        let value = 42_i32;
        let obj = ExternRef::new::<i32>(&mut store, value);
        assert_eq!(obj.data(&store).downcast_ref::<i32>(), Some(&value),);
    }
}
