//! TRust: exception tags.
//!
//! WebAssembly Core 3.0 (local snapshot of the spec repository 37d6b059): `syntax/types.rst`
//! (`syntax-tagtype`), `exec/runtime.rst` ("Tag Instances") and `exec/modules.rst`
//! (`alloc-tag`). A tag is a store object of a tag type: a function type without results whose
//! parameters are the field types of the exceptions it identifies. Each instantiation allocates
//! fresh tags for its module's tag definitions, an imported tag is the very tag of its exporter
//! (`valid/matching.rst` `match-tagtype`: the types must be equivalent), and an embedder
//! allocates tags with `tag_alloc` (`appendix/embedding.rst` `embed-tag-alloc`), for example for
//! `new WebAssembly.Tag` and the JavaScript exception tag of the WebAssembly JavaScript
//! Interface (#tags, #js-exceptions).
//!
//! Catch clauses match exceptions by tag address, never by the defining instance or index, so
//! one exception is caught by every instance that imports its tag.

use crate::{AsContext, AsContextMut, FuncType, ValType, store::Stored};

/// The type of a [`Tag`]: the field types of the exceptions it identifies.
///
/// # Note
///
/// WebAssembly Core 3.0 expresses a tag type as a function type whose results are empty
/// (`valid/types.rst` `valid-tagtype`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TagType {
    /// The function type `[params] -> []` of the tag.
    ty: FuncType,
}

impl TagType {
    /// Creates a new [`TagType`] for exceptions with fields of the types `params`.
    pub fn new<P>(params: P) -> Self
    where
        P: IntoIterator,
        <P as IntoIterator>::IntoIter: Iterator<Item = ValType> + ExactSizeIterator,
    {
        Self {
            ty: FuncType::new(params, [] as [ValType; 0]),
        }
    }

    /// Creates a [`TagType`] from the validated function type `ty` of a tag declaration.
    pub(crate) fn from_func_type(ty: FuncType) -> Self {
        debug_assert!(ty.results().is_empty(), "tag types have no results: {ty:?}");
        Self { ty }
    }

    /// Returns the field types of the exceptions of the tag.
    pub fn params(&self) -> &[ValType] {
        self.ty.params()
    }

    /// Returns the function type `[params] -> []` of the tag.
    pub fn func_type(&self) -> &FuncType {
        &self.ty
    }
}

/// The store entity of a [`Tag`]: its tag type.
#[derive(Debug)]
pub struct TagEntity {
    ty: TagType,
}

impl TagEntity {
    /// Creates a new [`TagEntity`] of type `ty`.
    pub fn new(ty: TagType) -> Self {
        Self { ty }
    }

    /// Returns the [`TagType`] of the tag.
    pub fn ty(&self) -> &TagType {
        &self.ty
    }
}

define_handle! {
    /// TRust: a Wasm exception tag of a [`Store`](crate::Store).
    ///
    /// Two tags are the same tag exactly if their handles are equal: tag identity is what
    /// catch clauses and the WebAssembly JavaScript Interface compare.
    #[derive(PartialEq, Eq, Hash)]
    struct Tag(u32, Stored) => TagEntity;
}

impl Tag {
    /// Allocates a new tag of type `ty` (`tag_alloc`).
    pub fn new(mut ctx: impl AsContextMut, ty: TagType) -> Self {
        ctx.as_context_mut()
            .store
            .inner
            .alloc_tag(TagEntity::new(ty))
    }

    /// Returns the [`TagType`] of the tag (`tag_type`).
    ///
    /// # Panics
    ///
    /// If `ctx` does not own this [`Tag`].
    pub fn ty(&self, ctx: impl AsContext) -> TagType {
        ctx.as_context().store.inner.resolve_tag(self).ty().clone()
    }
}
