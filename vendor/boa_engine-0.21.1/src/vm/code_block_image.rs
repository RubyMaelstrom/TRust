//! Heap- and thread-independent images of compiled code (the K1/K2 keystone).
//!
//! A compiled [`CodeBlock`] is normally pinned to the thread and GC heap that
//! built it: nested functions are `Gc<CodeBlock>` handles into the thread-local
//! `boa_gc` heap, constant strings and binding/IC names are `JsString`
//! (non-atomic refcount), bigints and scopes are `Rc`. None of it is `Send`, so
//! a `CodeBlock` cannot be cached across pages, handed from a compile worker to
//! a page thread, or otherwise reused outside its origin thread.
//!
//! [`CodeBlockImage`] is the *detached* form: a flat tree of owned, `Send`
//! plain Rust data with no `Gc`/`Rc`/`JsString`. [`CodeBlock::to_image`]
//! dehydrates a live block into an image; [`CodeBlock::from_image`] rehydrates
//! an image back into a live `Gc<CodeBlock>` — and the latter runs on *any*
//! thread, allocating into that thread's heap.
//!
//! - **K1 (heap-independent artifact):** the recursive `Gc<CodeBlock>` function
//!   tree becomes a `Box<CodeBlockImage>` tree; the inline cache (a runtime
//!   `WeakShape`/slot cache) is carried as its property names only and rebuilt
//!   empty, since its contents are meaningful only against a live heap.
//! - **K2 (portable identifiers):** every identifier — constant strings,
//!   binding names, scope names, IC names, the function name — is carried as
//!   owned UTF-16 (`Vec<u16>`), never a `Context`-local interner index. A
//!   compiled `CodeBlock` already names identifiers by `JsString`, not by
//!   interner `Sym` (the interner is a parse/compile-time structure that does
//!   not survive into the artifact), so K2 is satisfied simply by carrying the
//!   strings; no interner remap is needed at this layer.
//!
//! The bytecode itself is already a flat `Box<[u8]>` whose operands index into
//! the (now imaged) constant pool, so it round-trips by a plain copy.
//!
//! This primitive underlies the in-memory compiled-code cache, compile-on-
//! arrival, and parallel compile (see the engine performance plan): all three
//! reduce to "compile somewhere, run on the page thread" = dehydrate → (move
//! across threads) → rehydrate.

use std::cell::Cell;
use std::path::PathBuf;

use boa_ast::scope::{BindingLocator, BindingLocatorImage, Scope, ScopeImage};
use boa_ast::{LinearPosition, LinearSpan, Position, SourceText as AstSourceText};
use boa_gc::Gc;

use crate::bigint::RawBigInt;
use crate::builtins::function::ThisMode;
use crate::spanned_source_text::{SourceText, SpannedSourceText};
use crate::{JsBigInt, JsString};

use super::code_block::{CodeBlock, CodeBlockFlags, Constant, Handler};
use super::inline_cache::InlineCache;
use super::opcode::ByteCode;
use super::source_info::{Entry, SourceInfo, SourceMap, SourcePath};

/// A detached, owned, `Send` image of a [`CodeBlock`]. See the module docs.
///
/// The fields are private: to consumers (e.g. a cross-page code cache) this is
/// an opaque, movable token, produced by [`CodeBlock::to_image`] and consumed
/// by [`CodeBlock::from_image`]. `PartialEq` is derived so a dehydrate →
/// rehydrate → dehydrate round-trip can be asserted lossless.
#[derive(Clone, Debug, PartialEq)]
pub struct CodeBlockImage {
    flags: u16,
    length: u32,
    parameter_length: u32,
    register_count: u32,
    this_mode: ThisMode,
    mapped_arguments_binding_indices: Vec<Option<u32>>,
    bytecode: Box<[u8]>,
    constants: Vec<ConstantImage>,
    bindings: Vec<BindingLocatorImage>,
    handlers: Vec<Handler>,
    /// Inline-cache property names only; rehydrated to fresh empty caches.
    ic_names: Vec<Vec<u16>>,
    source_info: SourceInfoImage,
}

/// Owned image of a single constant-pool entry.
#[derive(Clone, Debug, PartialEq)]
enum ConstantImage {
    String(Vec<u16>),
    /// A nested function: the recursive `Gc<CodeBlock>` becomes an owned subtree.
    Function(Box<CodeBlockImage>),
    BigInt(RawBigInt),
    Scope(ScopeImage),
}

/// Owned image of a [`CodeBlock`]'s [`SourceInfo`].
#[derive(Clone, Debug, PartialEq)]
struct SourceInfoImage {
    function_name: Vec<u16>,
    map_entries: Vec<EntryImage>,
    path: SourcePathImage,
    /// The function's own source slice (`SpannedSourceText::to_code_points`),
    /// consumed only by `Function.prototype.toString`. `None` for code without
    /// a span (e.g. a top-level script, whose spanned text is empty).
    spanned_source: Option<Box<[u16]>>,
}

/// Owned image of one bytecode → source-position map entry.
#[derive(Clone, Debug, PartialEq)]
struct EntryImage {
    pc: u32,
    /// `(line, column)`, both 1-based and non-zero (see [`Position`]).
    position: Option<(u32, u32)>,
}

/// Owned image of a [`SourcePath`] (`Rc<Path>` → `PathBuf`).
#[derive(Clone, Debug, PartialEq)]
enum SourcePathImage {
    None,
    Eval,
    Json,
    Path(PathBuf),
}

impl CodeBlock {
    /// Dehydrate this code block (and, recursively, every nested function in its
    /// constant pool) into a detached, `Send` [`CodeBlockImage`].
    ///
    /// The image shares nothing with the live block or its heap; it can be
    /// stored, cloned, and moved to another thread, then turned back into a live
    /// block with [`CodeBlock::from_image`].
    #[must_use]
    pub fn to_image(&self) -> CodeBlockImage {
        CodeBlockImage {
            flags: self.flags.get().bits(),
            length: self.length,
            parameter_length: self.parameter_length,
            register_count: self.register_count,
            this_mode: self.this_mode.clone(),
            mapped_arguments_binding_indices: self
                .mapped_arguments_binding_indices
                .iter()
                .copied()
                .collect(),
            bytecode: Box::from(self.bytecode.bytes()),
            constants: self
                .constants
                .iter()
                .map(ConstantImage::dehydrate)
                .collect(),
            bindings: self.bindings.iter().map(BindingLocator::to_image).collect(),
            handlers: self.handlers.iter().copied().collect(),
            ic_names: self.ic.iter().map(|c| c.name.to_vec()).collect(),
            source_info: SourceInfoImage::dehydrate(&self.source_info),
        }
    }

    /// Rehydrate a [`CodeBlockImage`] into a live `Gc<CodeBlock>` on the current
    /// thread's heap. The inverse of [`CodeBlock::to_image`]; needs no
    /// [`Context`](crate::Context) and may run on any thread.
    #[must_use]
    pub fn from_image(image: &CodeBlockImage) -> Gc<Self> {
        Gc::new(image.rehydrate())
    }
}

impl CodeBlockImage {
    /// Build a live [`CodeBlock`] from this image (without the final `Gc`
    /// wrapper). See [`CodeBlock::from_image`].
    fn rehydrate(&self) -> CodeBlock {
        CodeBlock {
            flags: Cell::new(CodeBlockFlags::from_bits_retain(self.flags)),
            length: self.length,
            parameter_length: self.parameter_length,
            register_count: self.register_count,
            this_mode: self.this_mode.clone(),
            mapped_arguments_binding_indices: self
                .mapped_arguments_binding_indices
                .iter()
                .copied()
                .collect(),
            bytecode: ByteCode::from_bytes(self.bytecode.clone()),
            constants: self
                .constants
                .iter()
                .map(ConstantImage::rehydrate)
                .collect(),
            bindings: self
                .bindings
                .iter()
                .map(BindingLocator::from_image)
                .collect(),
            handlers: self.handlers.iter().copied().collect(),
            ic: self
                .ic_names
                .iter()
                .map(|n| InlineCache::new(JsString::from(n.as_slice())))
                .collect(),
            source_info: self.source_info.rehydrate(),
        }
    }
}

impl ConstantImage {
    fn dehydrate(constant: &Constant) -> Self {
        match constant {
            Constant::String(s) => Self::String(s.to_vec()),
            Constant::Function(code) => Self::Function(Box::new(code.to_image())),
            Constant::BigInt(b) => Self::BigInt(b.as_inner().clone()),
            Constant::Scope(scope) => Self::Scope(scope.to_image()),
        }
    }

    fn rehydrate(&self) -> Constant {
        match self {
            Self::String(s) => Constant::String(JsString::from(s.as_slice())),
            Self::Function(image) => Constant::Function(CodeBlock::from_image(image)),
            Self::BigInt(b) => Constant::BigInt(JsBigInt::from(b.clone())),
            Self::Scope(image) => Constant::Scope(Scope::from_image(image)),
        }
    }
}

impl SourceInfoImage {
    fn dehydrate(info: &SourceInfo) -> Self {
        let map = info.map();
        Self {
            function_name: info.function_name().to_vec(),
            map_entries: map
                .entries()
                .iter()
                .map(|e| EntryImage {
                    pc: e.pc(),
                    position: e.position().map(|p| (p.line_number(), p.column_number())),
                })
                .collect(),
            path: SourcePathImage::dehydrate(map.path()),
            spanned_source: info
                .text_spanned()
                .to_code_points()
                .map(|cp| cp.to_vec().into_boxed_slice()),
        }
    }

    fn rehydrate(&self) -> SourceInfo {
        let entries: Box<[Entry]> = self
            .map_entries
            .iter()
            .map(|e| Entry {
                pc: e.pc,
                position: e.position.map(|(line, column)| Position::new(line, column)),
            })
            .collect();
        let map = SourceMap::new(entries, self.path.rehydrate());
        // Rebuild the spanned source as a standalone slice spanning its whole
        // length: `Function.prototype.toString` reads only `to_code_points()`,
        // which then yields the identical code points.
        let spanned = match &self.spanned_source {
            Some(src) => {
                let span = LinearSpan::new(LinearPosition::new(0), LinearPosition::new(src.len()));
                SpannedSourceText::new(
                    SourceText::new(AstSourceText::from_code_points(src.to_vec())),
                    Some(span),
                )
            }
            None => SpannedSourceText::new_empty(),
        };
        SourceInfo::new(map, JsString::from(self.function_name.as_slice()), spanned)
    }
}

impl SourcePathImage {
    fn dehydrate(path: &SourcePath) -> Self {
        match path {
            SourcePath::None => Self::None,
            SourcePath::Eval => Self::Eval,
            SourcePath::Json => Self::Json,
            SourcePath::Path(p) => Self::Path(p.to_path_buf()),
        }
    }

    fn rehydrate(&self) -> SourcePath {
        match self {
            Self::None => SourcePath::None,
            Self::Eval => SourcePath::Eval,
            Self::Json => SourcePath::Json,
            Self::Path(p) => SourcePath::from(Some(p.clone())),
        }
    }
}
