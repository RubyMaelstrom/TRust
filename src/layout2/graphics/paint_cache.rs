//! Retained leaf paint cache for the live page actor.
//!
//! Graphical paint (`paint_in_spaces`) turns a fragment tree into a display
//! list, one `paint_fragment` call per fragment, in CSS 2.2 Appendix E order.
//! Re-deriving every fragment on each frame costs time proportional to the
//! document, however little changed. This module memoizes the leaf: the
//! commands and side outputs one `paint_fragment(F)` call appended to the
//! builder. The cache is an optimization only; the full rebuild is always
//! correct, and a fragment is replayed only when its output provably equals
//! what a fresh build would produce.
//!
//! What a leaf's output is a function of (the reuse conditions below are the
//! complete list; anything else that paint reads must be added here or make
//! the chunk uncacheable via [`taint`]):
//!
//! 1. The fragment's own data, every field except its children
//!    ([`frag_shallow_eq`]; a line is compared by `Arc` identity, else by
//!    value, since layout allocates a line again whenever it lays its block
//!    again).
//! 2. The builder state on entry that `paint_fragment` consults: the active
//!    scroll-chain prefix, the innermost hard clip, the transformed clips
//!    ([`EntryState`]).
//! 3. Lookups into the builder's per-document tables, recorded while the
//!    chunk was built and re-evaluated on reuse ([`Read`]): the scroll/clip
//!    chain of each node it asked about, the replaced-element border boxes,
//!    the clip extent, and image sizes and SVG ratios.
//! 4. The Dom state of the nodes the fragment names. The paint journal
//!    (`dom::paint_journal`) lists the nodes whose state may have changed;
//!    they are expanded over their flat-tree descendants, so a change to an
//!    ancestor (inheritance, `inert`, `href`, text decoration) dirties every
//!    chunk below it. A chunk is clean when none of its nodes
//!    ([`chunk_nodes`]) is in that set.
//! 5. Document-wide inputs ([`GlobalKey`]) and the global stamp. When either
//!    changed, the whole cache is dropped.
//!
//! Never cached (the chunk is tainted): form controls, canvas, media,
//! iframes, SVG resource users, collapsed table borders, `background-clip:
//! text` (it reads descendant lines), and anything that moves commands into
//! the viewport-pinned underlay. When unsure, taint.
//!
//! Verification: `TRUST_PAINT_VERIFY=1` (always on under `cfg(test)`)
//! recomputes the whole paint with a fresh builder and compares the complete
//! outputs; a mismatch logs the first differing command with its chunk and
//! node, uses the full result and clears the cache (tests panic).
//! `TRUST_PAINT_CACHE=shadow` validates chunks as usual but always paints
//! them again, comparing the fresh output with the cached one.

use super::*;
use crate::dom::JournalDelta;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Chunks and commands the cache will hold; beyond either, new chunks are
/// painted but not kept.
const MAX_CHUNKS: usize = 120_000;
const MAX_COMMANDS: usize = 400_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Shadow,
    On,
}

#[cfg(test)]
thread_local! {
    static MODE_OVERRIDE: Cell<Option<Mode>> = const { Cell::new(None) };
}

/// `TRUST_PAINT_CACHE=off|shadow|on` (default on).
pub(crate) fn mode() -> Mode {
    #[cfg(test)]
    if let Some(mode) = MODE_OVERRIDE.with(Cell::get) {
        return mode;
    }
    static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *MODE.get_or_init(
        || match std::env::var("TRUST_PAINT_CACHE").ok().as_deref() {
            Some("off" | "0") => Mode::Off,
            Some("shadow") => Mode::Shadow,
            _ => Mode::On,
        },
    )
}

/// Whether every cached paint is also computed in full and compared.
pub(crate) fn verify_enabled() -> bool {
    if cfg!(test) {
        return true;
    }
    static VERIFY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VERIFY.get_or_init(|| std::env::var_os("TRUST_PAINT_VERIFY").is_some_and(|value| value != "0"))
}

#[cfg(test)]
pub(crate) struct ModeGuard(Option<Mode>);

#[cfg(test)]
impl Drop for ModeGuard {
    fn drop(&mut self) {
        MODE_OVERRIDE.with(|mode| mode.set(self.0));
    }
}

/// Force the cache mode on this thread until the guard drops.
#[cfg(test)]
pub(crate) fn force_mode(mode: Mode) -> ModeGuard {
    ModeGuard(MODE_OVERRIDE.with(|cell| cell.replace(Some(mode))))
}

thread_local! {
    /// Set while a leaf is painted by the features that make it uncacheable.
    static TAINT: Cell<u32> = const { Cell::new(0) };
}

/// Mark the chunk being painted as uncacheable. `why` names a bit of
/// [`Taint`] for diagnostics.
#[inline]
pub(super) fn taint(why: Taint) {
    TAINT.with(|cell| cell.set(cell.get() | why as u32));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum Taint {
    Form = 1,
    Canvas = 1 << 1,
    Media = 1 << 2,
    Frame = 1 << 3,
    Svg = 1 << 4,
    Collapsed = 1 << 5,
    TextClip = 1 << 6,
    Pinned = 1 << 7,
    Unbalanced = 1 << 8,
}

/// A table lookup a chunk made while it was built. Reuse re-evaluates each
/// against the current builder and requires the same answer.
#[derive(Clone, Debug)]
pub(super) enum Read {
    /// The scroll/clip chain of `node` (`Builder::scroll_chain_links`).
    Chain {
        node: NodeId,
        include_node: bool,
        links: Vec<(NodeId, PaintShape, bool)>,
    },
    ReplacedBorder(NodeId, Option<CssRect>),
    /// The finite extent used for an unbounded clip edge.
    ClipExtent(CssRect),
    ImageSize(String, Option<(u32, u32)>),
    /// `f32` bits of an SVG's ratio-only intrinsic ratio.
    SvgRatio(String, Option<u32>),
}

/// The side outputs of the chunk being built, beyond its commands and lines.
#[derive(Default)]
pub(super) struct Recording {
    pub reads: Vec<Read>,
    /// Every image the chunk requested, in call order (first occurrence).
    pub images: Vec<(ImageHandle, String)>,
    pub replaced: Vec<NodeId>,
    /// The chunk painted a media element's hit region (`note_media_hit`).
    pub media_hits: bool,
}

impl Recording {
    pub(super) fn read_chain(
        &mut self,
        node: NodeId,
        include_node: bool,
        links: &[(NodeId, PaintShape, bool)],
    ) {
        let known = self.reads.iter().any(|read| {
            matches!(read, Read::Chain { node: n, include_node: i, .. }
                if *n == node && *i == include_node)
        });
        if !known {
            self.reads.push(Read::Chain {
                node,
                include_node,
                links: links.to_vec(),
            });
        }
    }

    pub(super) fn read(&mut self, read: Read) {
        // A leaf asks about the same node repeatedly (one scroll chain per
        // piece); keep one entry per distinct question.
        let same = |old: &Read| match (old, &read) {
            (
                Read::Chain {
                    node: a,
                    include_node: x,
                    ..
                },
                Read::Chain {
                    node: b,
                    include_node: y,
                    ..
                },
            ) => a == b && x == y,
            (Read::ReplacedBorder(a, _), Read::ReplacedBorder(b, _)) => a == b,
            (Read::ClipExtent(_), Read::ClipExtent(_)) => true,
            (Read::ImageSize(a, _), Read::ImageSize(b, _)) => a == b,
            (Read::SvgRatio(a, _), Read::SvgRatio(b, _)) => a == b,
            _ => false,
        };
        if !self.reads.iter().any(same) {
            self.reads.push(read);
        }
    }
}

/// The builder state `paint_fragment` consults on entry.
#[derive(Clone, Debug, PartialEq)]
struct EntryState {
    scroll_nodes: Vec<(NodeId, bool)>,
    hard: Option<CssRect>,
    transformed: Vec<CssRect>,
}

impl EntryState {
    fn of(builder: &Builder<'_>) -> Self {
        Self {
            scroll_nodes: builder.scroll_nodes.clone(),
            hard: builder.hard_clips.last().copied(),
            transformed: builder.transformed_clips.clone(),
        }
    }

    fn matches(&self, builder: &Builder<'_>) -> bool {
        self.scroll_nodes == builder.scroll_nodes
            && self.hard.as_ref() == builder.hard_clips.last()
            && self.transformed == builder.transformed_clips
    }
}

/// One memoized `paint_fragment` call.
struct Chunk {
    /// The fragment as painted, without its children.
    shallow: Frag,
    entry: EntryState,
    /// Every node the fragment names; see [`chunk_nodes`].
    nodes: Box<[NodeId]>,
    reads: Box<[Read]>,
    commands: Box<[DisplayCommand]>,
    lines: Box<[PaintLine]>,
    images: Box<[(ImageHandle, String)]>,
    replaced: Box<[NodeId]>,
    media_hits: bool,
    /// The last paint that validated or built this chunk.
    frame: u64,
}

/// Document-wide inputs of every leaf. A change drops the whole cache.
#[derive(Clone, PartialEq)]
struct GlobalKey {
    viewport: [u32; 2],
    dom_viewport: [u32; 2],
    ratio: u32,
    base: Url,
    document_root: Option<NodeId>,
    canvas_background_source: Option<NodeId>,
    selection_rules: bool,
    backdrop_filters: bool,
    inline_media: bool,
    /// `clip: rect()` regions and marquees decide clips and scopes by
    /// flat-tree ancestry; they are rare, so they key the cache whole.
    legacy_clips: Vec<(NodeId, CssRect)>,
    marquees: Vec<(NodeId, MarqueeScope)>,
    epochs: [u64; 5],
}

impl GlobalKey {
    fn of(builder: &Builder<'_>) -> Self {
        let mut legacy_clips: Vec<_> = builder
            .legacy_clips
            .iter()
            .map(|(&node, &rect)| (node, rect))
            .collect();
        legacy_clips.sort_unstable_by_key(|(node, _)| *node);
        let mut marquees: Vec<_> = builder
            .marquee_scopes
            .iter()
            .map(|(&node, scope)| (node, scope.clone()))
            .collect();
        marquees.sort_unstable_by_key(|(node, _)| *node);
        let dom_viewport = builder.dom.viewport_px();
        Self {
            viewport: [builder.viewport_w.to_bits(), builder.viewport_h.to_bits()],
            dom_viewport: [dom_viewport.0.to_bits(), dom_viewport.1.to_bits()],
            ratio: builder.dom.device_pixel_ratio().to_bits(),
            base: builder.base.clone(),
            document_root: builder.document_root,
            canvas_background_source: builder.canvas_background_source,
            selection_rules: builder.selection_rules,
            backdrop_filters: builder.backdrop_filters,
            inline_media: builder.dom.inline_media(),
            legacy_clips,
            marquees,
            epochs: builder.dom.paint_epochs(),
        }
    }
}

/// Why the cache was dropped at the start of a paint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum Reset {
    #[default]
    None,
    First,
    Journal,
    Budget,
    Key(&'static str),
}

pub(crate) struct PaintCache {
    key: Option<GlobalKey>,
    cursor: crate::dom::JournalCursor,
    frame: u64,
    map: FxHashMap<u64, Vec<Chunk>>,
    chunks: usize,
    commands: usize,
    /// Consecutive large paints that reused little (see [`Session::retire`]).
    cold: u32,
    /// Paints still to run uncached after a run of cold ones: recording a
    /// page that changes wholesale every frame only costs time.
    skip: u32,
}

impl PaintCache {
    fn new() -> Self {
        Self {
            key: None,
            cursor: Default::default(),
            frame: 0,
            map: FxHashMap::default(),
            chunks: 0,
            commands: 0,
            cold: 0,
            skip: 0,
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.chunks = 0;
        self.commands = 0;
        self.key = None;
    }

    /// Bytes the cache holds, approximately (memory accounting).
    pub(crate) fn retained_bytes(&self) -> usize {
        self.chunks * std::mem::size_of::<Chunk>()
            + self.commands * std::mem::size_of::<DisplayCommand>()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stats {
    pub leaves: u32,
    pub hits: u32,
    /// No chunk with this fragment's shape and entry state.
    pub new: u32,
    /// A chunk matched but a node it names is dirty.
    pub dirty: u32,
    /// A chunk matched but a recorded table lookup changed.
    pub reads: u32,
    pub tainted: u32,
    /// Leaves of each kind (line, other) that found no chunk.
    pub new_lines: u32,
    /// How many tainted leaves each [`Taint`] bit marked.
    pub taints: [u16; 9],
    pub over_budget: u32,
    /// Misses reported by `TRUST_DIAG_PAINT_MISSES`.
    pub traced: u32,
    pub replayed_commands: u32,
    pub painted_commands: u32,
}

/// One paint's use of the cache, owned by the builder for its duration.
pub(super) struct Session {
    cache: Box<PaintCache>,
    /// The flat-tree closure of the journal's roots since the last paint.
    dirty: FxHashSet<NodeId>,
    mode: Mode,
    frame: u64,
    pub reset: Reset,
    pub roots: usize,
    pub stats: Stats,
    /// Verification only: command ranges and the node that produced them.
    spans: Option<Vec<Span>>,
    /// Chunks validated or built by this paint (`frame` is current).
    touched: usize,
}

#[derive(Clone, Debug)]
struct Span {
    commands: (usize, usize),
    node: NodeId,
    /// Every node the fragment names (`chunk_nodes`).
    nodes: Box<[NodeId]>,
    replayed: bool,
}

/// Open the cache for one paint, or `None` when the paint is uncached.
pub(super) fn begin(builder: &Builder<'_>) -> Option<Box<Session>> {
    let mode = mode();
    let dom = builder.dom;
    if mode == Mode::Off || !dom.render_live() || dom.style_depends_on_layout() {
        return None;
    }
    // Style invalidations are deferred to the first read; the journal only
    // learns of their roots when they are applied.
    dom.flush_style_invalidations();
    let mut cache = dom
        .paint_cache_cell()
        .borrow_mut()
        .take()
        .unwrap_or_else(|| Box::new(PaintCache::new()));
    if cache.skip > 0 {
        cache.skip -= 1;
        cache.clear();
        *dom.paint_cache_cell().borrow_mut() = Some(cache);
        return None;
    }
    let journal = dom.paint_journal();
    let cursor = journal.cursor();
    let key = GlobalKey::of(builder);
    let mut reset = Reset::None;
    let mut dirty = FxHashSet::default();
    let mut roots = 0;
    if let Some(old) = &cache.key
        && *old != key
    {
        reset = Reset::Key(key_difference(old, &key));
    } else if cache.key.is_none() {
        reset = Reset::First;
    } else {
        match journal.since(cache.cursor) {
            JournalDelta::Clean => {
                #[cfg(test)]
                LAST_ROOTS.with(|last| *last.borrow_mut() = "clean".to_string());
            }
            JournalDelta::Global => reset = Reset::Journal,
            JournalDelta::Roots(list) => {
                roots = list.len();
                #[cfg(test)]
                LAST_ROOTS.with(|last| *last.borrow_mut() = format!("{list:?}"));
                let budget = (dom.node_count() / 4).max(256);
                match dom.paint_dirty_set(&list, budget) {
                    Some(set) => dirty = set,
                    None => reset = Reset::Budget,
                }
            }
        }
    }
    if reset != Reset::None {
        cache.clear();
    }
    cache.key = Some(key);
    // Anything noted while this paint runs is pending for the next one.
    cache.cursor = cursor;
    cache.frame = cache.frame.wrapping_add(1);
    let frame = cache.frame;
    Some(Box::new(Session {
        cache,
        dirty,
        mode,
        frame,
        reset,
        roots,
        stats: Stats::default(),
        spans: verify_enabled().then(Vec::new),
        touched: 0,
    }))
}

fn key_difference(old: &GlobalKey, new: &GlobalKey) -> &'static str {
    if old.viewport != new.viewport || old.dom_viewport != new.dom_viewport {
        "viewport"
    } else if old.ratio != new.ratio {
        "ratio"
    } else if old.base != new.base {
        "base"
    } else if old.legacy_clips != new.legacy_clips {
        "clip-rect"
    } else if old.marquees != new.marquees {
        "marquee"
    } else if old.epochs != new.epochs {
        "epochs"
    } else {
        "document"
    }
}

impl Session {
    /// Drop what this paint did not use and hand the cache back to its Dom.
    /// Every surviving chunk was validated or built by this paint, so the next
    /// paint need only consult the journal since this one began.
    fn retire(mut self, dom: &Dom) -> Stats {
        let frame = self.frame;
        let cache = &mut self.cache;
        if cache.chunks != self.touched {
            let mut chunks = 0;
            let mut commands = 0;
            cache.map.retain(|_, bucket| {
                bucket.retain(|chunk| chunk.frame == frame);
                for chunk in bucket.iter() {
                    chunks += 1;
                    commands += chunk.commands.len();
                }
                !bucket.is_empty()
            });
            cache.chunks = chunks;
            cache.commands = commands;
        }
        let stats = self.stats;
        // A page that rewrites most of itself every frame, or resets the
        // cache every frame, gains nothing from a cache that must be recorded
        // again: back off, doubling up to 15 uncached paints. The first paint
        // is expected to miss and does not count; a paint that reuses enough
        // ends the run.
        if self.reset != Reset::First && stats.leaves >= 1000 {
            if stats.hits * 4 < stats.leaves {
                self.cache.cold = (self.cache.cold + 1).min(4);
                self.cache.skip = (1 << self.cache.cold) - 1;
            } else {
                self.cache.cold = 0;
            }
        }
        *dom.paint_cache_cell().borrow_mut() = Some(self.cache);
        stats
    }

    /// Forget everything: a verified mismatch means some input is missing
    /// from the reuse conditions.
    pub(super) fn poison(&mut self) {
        self.cache.clear();
        self.touched = 0;
    }

    /// The fragment node of the command range containing command `index`.
    fn span_of(&self, index: usize) -> Option<(NodeId, Box<[NodeId]>, bool)> {
        self.spans
            .as_ref()?
            .iter()
            .find(|span| span.commands.0 <= index && index < span.commands.1)
            .map(|span| (span.node, span.nodes.clone(), span.replayed))
    }
}

#[cfg(test)]
thread_local! {
    /// The stats of the last cached paint on this thread (fuzz tests).
    static LAST_STATS: Cell<Option<Stats>> = const { Cell::new(None) };
    static LAST_ROOTS: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    static LAST_RESET: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// `TRUST_DIAG_PAINT_MISSES`: name the first misses of each paint.
fn trace_misses() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TRUST_DIAG_PAINT_MISSES").is_some())
}

fn diag_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TRUST_DIAG_FRAME").is_some())
}

/// Close a cached paint: report it (`TRUST_DIAG_FRAME`) and keep the cache.
pub(super) fn finish(session: Session, dom: &Dom, elapsed: std::time::Duration, mismatch: bool) {
    let (mode, reset, roots, dirty) = (
        session.mode,
        session.reset.clone(),
        session.roots,
        session.dirty.len(),
    );
    let (chunks, commands) = (session.cache.chunks, session.cache.commands);
    let stats = session.retire(dom);
    #[cfg(test)]
    {
        LAST_STATS.with(|last| last.set(Some(stats)));
        LAST_RESET.with(|last| *last.borrow_mut() = format!("{reset:?}"));
    }
    if diag_on() {
        let causes = dom.paint_journal().global_causes();
        eprintln!(
            "DIAGPAINTCACHE mode={mode:?} reset={reset:?} roots={roots} dirty_nodes={dirty} leaves={} hit={} new={} new_lines={} dirty={} reads={} tainted={} taints={} over_budget={} replayed_cmds={} painted_cmds={} chunks={chunks} cmds={commands} mismatch={mismatch} time={}us",
            stats.leaves,
            stats.hits,
            stats.new,
            stats.new_lines,
            stats.dirty,
            stats.reads,
            stats.tainted,
            stats
                .taints
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("/"),
            stats.over_budget,
            stats.replayed_commands,
            stats.painted_commands,
            elapsed.as_micros(),
        );
        if !causes.is_empty() {
            let causes: Vec<_> = causes
                .into_iter()
                .map(|(site, count)| format!("{site}x{count}"))
                .collect();
            eprintln!("DIAGPAINTGLOBAL {}", causes.join(" "));
        }
    }
}

/// Describe the first difference between a cached paint and the full one.
/// Panics under `cfg(test)`.
pub(super) fn report_mismatch(session: &Session, dom: &Dom, cached: &Painted, full: &Painted) {
    let (a, b) = (&cached.0, &full.0);
    let mut report = String::new();
    let mut compare = |name: &str, x: &[DisplayCommand], y: &[DisplayCommand]| -> bool {
        if x == y {
            return false;
        }
        let at = x
            .iter()
            .zip(y)
            .position(|(x, y)| x != y)
            .unwrap_or(x.len().min(y.len()));
        let chunk = if name == "primitives" {
            session
                .span_of(at)
                .map(|(node, nodes, replayed)| {
                    let named: Vec<_> = nodes
                        .iter()
                        .map(|&n| {
                            format!(
                                "{n}:{:?}#{:?}.{:?} color={:?}",
                                dom.tag_name(n),
                                dom.attr(n, "id"),
                                dom.attr(n, "class"),
                                dom.computed_value_resolved(n, "color")
                            )
                        })
                        .collect();
                    let ancestry: Vec<_> = nodes
                        .first()
                        .map(|&first| {
                            std::iter::successors(Some(first), |&n| dom.parent_flat(n))
                                .map(|n| {
                                    format!(
                                        "{n}={:?}#{:?}.{:?}",
                                        dom.tag_name(n),
                                        dom.attr(n, "id"),
                                        dom.attr(n, "class")
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let dirty: Vec<_> = nodes
                        .iter()
                        .map(|n| session.dirty.contains(n))
                        .collect();
                    #[cfg(test)]
                    let roots = LAST_ROOTS.with(|last| last.borrow().clone());
                    #[cfg(not(test))]
                    let roots = String::new();
                    format!(
                        " chunk node={node} nodes={named:?} ancestry={ancestry:?} dirty={dirty:?} replayed={replayed} journal={roots}"
                    )
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        report = format!(
            "{name}[{at}] ({} vs {} commands){chunk}\n{}",
            x.len(),
            y.len(),
            difference(&format!("{:?}", x.get(at)), &format!("{:?}", y.get(at))),
        );
        true
    };
    let _ = compare("primitives", &a.primitives, &b.primitives)
        || compare(
            "fixed_under",
            &a.fixed_under_primitives,
            &b.fixed_under_primitives,
        )
        || compare("fixed", &a.fixed_primitives, &b.fixed_primitives)
        || a.top_layer
            .iter()
            .zip(&b.top_layer)
            .any(|(x, y)| compare("top_layer", &x.primitives, &y.primitives));
    if report.is_empty() {
        report = if a.lines != b.lines {
            format!("lines ({} vs {})", a.lines.len(), b.lines.len())
        } else if a.image_requests != b.image_requests {
            format!(
                "image_requests\n{}",
                difference(
                    &format!("{:?}", a.image_requests),
                    &format!("{:?}", b.image_requests)
                )
            )
        } else if a.replaced_images != b.replaced_images {
            "replaced_images".to_string()
        } else if cached.2 != full.2 {
            format!(
                "boundaries\n{}",
                difference(&format!("{:?}", cached.2), &format!("{:?}", full.2))
            )
        } else {
            "other PagePaint fields".to_string()
        };
    }
    eprintln!("PAINTCACHE MISMATCH {report}");
    if cfg!(test) {
        panic!("retained paint differs from a full paint: {report}");
    }
}

/// The first place two debug renderings differ, with context.
fn difference(cached: &str, full: &str) -> String {
    let at = cached
        .bytes()
        .zip(full.bytes())
        .position(|(x, y)| x != y)
        .unwrap_or(cached.len().min(full.len()));
    let window = |text: &str| {
        let start = text.floor_char_boundary(at.saturating_sub(100));
        let end = text.floor_char_boundary((at + 100).min(text.len()));
        text[start..end].to_string()
    };
    format!(
        "  first difference at byte {at}\n  cached: ...{}...\n  full:   ...{}...",
        window(cached),
        window(full)
    )
}

/// Paint one fragment through the cache.
pub(super) fn paint_leaf(fragment: &Frag, builder: &mut Builder<'_>) {
    let Some(mut session) = builder.retained.take() else {
        return paint_fragment(fragment, builder);
    };
    session.stats.leaves += 1;
    let key = chunk_key(fragment);
    // A candidate shares the fragment's data and entry state.
    let found = session.cache.map.get(&key).and_then(|bucket| {
        bucket.iter().position(|chunk| {
            frag_shallow_eq(&chunk.shallow, fragment) && chunk.entry.matches(builder)
        })
    });
    let mut stale = false;
    if let Some(index) = found {
        let chunk = &session.cache.map[&key][index];
        if chunk.nodes.iter().any(|node| session.dirty.contains(node)) {
            session.stats.dirty += 1;
            stale = true;
        } else if !reads_valid(&chunk.reads, builder) {
            session.stats.reads += 1;
            stale = true;
        } else {
            session.stats.hits += 1;
            if session.mode == Mode::On {
                replay(&mut session, key, index, fragment, builder);
                builder.retained = Some(session);
                return;
            }
        }
    } else {
        session.stats.new += 1;
        if matches!(fragment.kind, FragKind::Line(_)) {
            session.stats.new_lines += 1;
        }
    }
    if (found.is_none() || stale) && trace_misses() && session.stats.traced < 40 {
        session.stats.traced += 1;
        eprintln!(
            "PAINTCACHE miss node={} kind={} at=({:.1},{:.1}) size=({:.1},{:.1}) why={}",
            fragment.node,
            kind_name(&fragment.kind),
            fragment.x,
            fragment.y,
            fragment.w,
            fragment.h,
            if found.is_none() { "new" } else { "stale" },
        );
    }
    // Paint it for real, recording what it appends.
    let hit = found.filter(|_| !stale);
    let before = Marks::of(builder);
    let entry = EntryState::of(builder);
    TAINT.with(|cell| cell.set(0));
    *builder.recording.borrow_mut() = Some(Recording::default());
    builder.retained = None;
    #[cfg(test)]
    audit::begin(builder.dom, &chunk_nodes(fragment));
    paint_fragment(fragment, builder);
    #[cfg(test)]
    {
        let undeclared = audit::end();
        let tainted_now = TAINT.with(Cell::get) != 0;
        assert!(
            undeclared.is_empty() || tainted_now,
            "paint of {} fragment at node {} read Dom state of nodes it does not name: {undeclared:?}",
            kind_name(&fragment.kind),
            fragment.node,
        );
    }
    let recording = builder.recording.borrow_mut().take().unwrap_or_default();
    let mut tainted = TAINT.with(|cell| cell.replace(0));
    let balanced = EntryState::of(builder) == entry;
    if !balanced {
        tainted |= Taint::Unbalanced as u32;
    }
    debug_assert!(balanced, "paint_fragment left builder scopes open");
    if builder.fixed_under.len() != before.fixed_under {
        tainted |= Taint::Pinned as u32;
    }
    if builder.canvas_images.len() != before.canvas_images {
        tainted |= Taint::Canvas as u32;
    }
    let appended = builder.commands.len() - before.commands;
    session.stats.painted_commands += appended as u32;
    if let Some(spans) = session.spans.as_mut() {
        spans.push(Span {
            commands: (before.commands, builder.commands.len()),
            node: fragment.node,
            nodes: chunk_nodes(fragment),
            replayed: false,
        });
    }
    if tainted != 0 {
        session.stats.tainted += 1;
        for (bit, count) in session.stats.taints.iter_mut().enumerate() {
            *count += u16::from(tainted & (1 << bit) != 0);
        }
        builder.retained = Some(session);
        return;
    }
    let chunk = Chunk {
        shallow: shallow_clone(fragment),
        entry,
        nodes: chunk_nodes(fragment),
        reads: recording.reads.into_boxed_slice(),
        commands: builder.commands[before.commands..].into(),
        lines: builder.lines[before.lines..].into(),
        images: recording.images.into_boxed_slice(),
        replaced: recording.replaced.into_boxed_slice(),
        media_hits: recording.media_hits,
        frame: session.frame,
    };
    if let Some(index) = hit {
        // Shadow mode: the validated chunk and the fresh one must agree.
        let old = &session.cache.map[&key][index];
        if !chunk_output_eq(old, &chunk) {
            eprintln!(
                "PAINTCACHE shadow mismatch node={} kind={} commands={}/{}",
                fragment.node,
                kind_name(&fragment.kind),
                old.commands.len(),
                chunk.commands.len()
            );
            debug_assert!(
                false,
                "shadow paint cache mismatch at node {}",
                fragment.node
            );
        }
    }
    let cache = &mut session.cache;
    if found.is_none() && (cache.chunks >= MAX_CHUNKS || cache.commands >= MAX_COMMANDS) {
        session.stats.over_budget += 1;
        builder.retained = Some(session);
        return;
    }
    let bucket = cache.map.entry(key).or_default();
    match found {
        Some(index) => {
            if bucket[index].frame != session.frame {
                session.touched += 1;
            }
            cache.commands = cache.commands - bucket[index].commands.len() + chunk.commands.len();
            bucket[index] = chunk;
        }
        None => {
            session.touched += 1;
            cache.chunks += 1;
            cache.commands += chunk.commands.len();
            bucket.push(chunk);
        }
    }
    builder.retained = Some(session);
}

/// Test-only audit of the reuse conditions' completeness: while a leaf is
/// painted for real, every node its Dom reads touch (`attr`, computed
/// values, ...) must be one it names (`chunk_nodes`) or a flat-tree or
/// composed ancestor of one, since only those are covered by the dirty set's
/// expansion. A read of any other node is a dependency the cache would miss.
/// An element's tag name never changes, so reading one is not a dependency
/// and is not audited.
#[cfg(test)]
pub(crate) mod audit {
    use super::*;
    use std::cell::RefCell;

    struct State {
        allowed: FxHashSet<NodeId>,
        violations: Vec<String>,
    }

    thread_local! {
        static ACTIVE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    pub(super) fn begin(dom: &Dom, roots: &[NodeId]) {
        // Walk first: the walk's own Dom reads must not be audited.
        let mut allowed = FxHashSet::default();
        for &root in roots {
            let mut pending = vec![root];
            while let Some(id) = pending.pop() {
                if dom.is_valid(id) && allowed.insert(id) {
                    pending.extend(dom.parent_flat(id));
                    pending.extend(dom.parent_composed(id));
                }
            }
        }
        ACTIVE.with(|active| {
            *active.borrow_mut() = Some(State {
                allowed,
                violations: Vec::new(),
            });
        });
    }

    pub(super) fn end() -> Vec<String> {
        ACTIVE.with(|active| {
            active
                .borrow_mut()
                .take()
                .map(|state| state.violations)
                .unwrap_or_default()
        })
    }

    pub(crate) fn read(node: NodeId, what: &'static str) {
        ACTIVE.with(|active| {
            let Ok(mut guard) = active.try_borrow_mut() else {
                return;
            };
            let Some(state) = guard.as_mut() else {
                return;
            };
            if node != NO_NODE && !state.allowed.contains(&node) && state.violations.len() < 16 {
                state.violations.push(format!("{what}({node})"));
            }
        });
    }
}

fn kind_name(kind: &FragKind) -> &'static str {
    match kind {
        FragKind::Block => "block",
        FragKind::TableCell(_) => "cell",
        FragKind::Line(_) => "line",
        FragKind::Oof(..) => "oof",
        FragKind::Fixed(_) => "fixed",
    }
}

/// Builder lengths before a leaf is painted.
struct Marks {
    commands: usize,
    lines: usize,
    fixed_under: usize,
    canvas_images: usize,
}

impl Marks {
    fn of(builder: &Builder<'_>) -> Self {
        Self {
            commands: builder.commands.len(),
            lines: builder.lines.len(),
            fixed_under: builder.fixed_under.len(),
            canvas_images: builder.canvas_images.len(),
        }
    }
}

/// Append a validated chunk's outputs, exactly as painting it again would.
fn replay(
    session: &mut Session,
    key: u64,
    index: usize,
    fragment: &Frag,
    builder: &mut Builder<'_>,
) {
    let frame = session.frame;
    let start = builder.commands.len();
    let chunk = &mut session.cache.map.get_mut(&key).unwrap()[index];
    if chunk.frame != frame {
        session.touched += 1;
    }
    chunk.frame = frame;
    // An equal line laid again: hold the new allocation, so the next
    // comparison is a pointer comparison and the old one can be freed.
    if let (FragKind::Line(old), FragKind::Line(new)) = (&mut chunk.shallow.kind, &fragment.kind)
        && !Arc::ptr_eq(old, new)
    {
        *old = new.clone();
    }
    builder.commands.extend_from_slice(&chunk.commands);
    builder.lines.extend_from_slice(&chunk.lines);
    // First occurrence order across the whole paint: a handle already
    // requested by an earlier leaf is not requested again.
    for (handle, source) in chunk.images.iter() {
        if builder.image_handles.insert(*handle) {
            builder.image_requests.push(ImageRequest {
                handle: *handle,
                source: source.clone(),
            });
        }
    }
    builder
        .replaced_images
        .extend(chunk.replaced.iter().copied());
    builder.has_media_hits |= chunk.media_hits;
    session.stats.replayed_commands += chunk.commands.len() as u32;
    if let Some(spans) = session.spans.as_mut() {
        spans.push(Span {
            commands: (start, builder.commands.len()),
            node: fragment.node,
            nodes: chunk_nodes(fragment),
            replayed: true,
        });
    }
}

fn chunk_output_eq(a: &Chunk, b: &Chunk) -> bool {
    a.commands == b.commands
        && a.lines == b.lines
        && a.images == b.images
        && a.replaced == b.replaced
        && a.media_hits == b.media_hits
}

/// Re-evaluate the lookups a chunk recorded.
fn reads_valid(reads: &[Read], builder: &Builder<'_>) -> bool {
    reads.iter().all(|read| match read {
        Read::Chain {
            node,
            include_node,
            links,
        } => builder.scroll_chain_links(*node, *include_node) == *links,
        Read::ReplacedBorder(node, rect) => {
            builder.replaced_border_boxes.get(node).copied() == *rect
        }
        Read::ClipExtent(extent) => builder.clip_extent == *extent,
        Read::ImageSize(source, size) => builder.images.get(source).copied() == *size,
        Read::SvgRatio(source, ratio) => svg_ratio(source).map(f32::to_bits) == *ratio,
    })
}

/// The intrinsic ratio of an SVG source that has a ratio but no size.
pub(super) fn svg_ratio(source: &str) -> Option<f32> {
    crate::img::svg_url_ratio_only(source)
        .or_else(|| crate::img::svg_ratio_only_get(source))
        .filter(|ratio| ratio.is_finite() && *ratio > 0.0)
}

fn chunk_key(fragment: &Frag) -> u64 {
    let mut hasher = FxHasher::default();
    fragment.node.hash(&mut hasher);
    fragment.x.to_bits().hash(&mut hasher);
    fragment.y.to_bits().hash(&mut hasher);
    fragment.w.to_bits().hash(&mut hasher);
    fragment.h.to_bits().hash(&mut hasher);
    match &fragment.kind {
        FragKind::Block => 0u8.hash(&mut hasher),
        FragKind::TableCell(layers) => {
            1u8.hash(&mut hasher);
            layers.len().hash(&mut hasher);
        }
        FragKind::Line(line) => {
            // Content, not identity: an equal line laid again must find its
            // chunk. Cheap fields only; `kind_eq` decides.
            2u8.hash(&mut hasher);
            line.pieces.len().hash(&mut hasher);
            line.width.to_bits().hash(&mut hasher);
            line.height.to_bits().hash(&mut hasher);
            line.baseline.to_bits().hash(&mut hasher);
            if let Some(piece) = line.pieces.first() {
                piece.x.to_bits().hash(&mut hasher);
                piece.item.text.len().hash(&mut hasher);
                piece.item.node.hash(&mut hasher);
            }
        }
        FragKind::Oof(..) => 3u8.hash(&mut hasher),
        FragKind::Fixed(index) => {
            4u8.hash(&mut hasher);
            index.hash(&mut hasher);
        }
    }
    if let Some((node, pseudo)) = fragment.paint.pseudo {
        node.hash(&mut hasher);
        pseudo.hash(&mut hasher);
    }
    hasher.finish()
}

fn shallow_clone(f: &Frag) -> Frag {
    Frag {
        flow: f.flow,
        node: f.node,
        x: f.x,
        y: f.y,
        w: f.w,
        h: f.h,
        border: f.border,
        css_size: f.css_size,
        content_size: f.content_size,
        content_offset: f.content_offset,
        paint: f.paint.clone(),
        clip: f.clip,
        kind: f.kind.clone(),
        children: Vec::new(),
    }
}

fn bits_eq<const N: usize>(a: &[f32; N], b: &[f32; N]) -> bool {
    a.iter().zip(b).all(|(a, b)| a.to_bits() == b.to_bits())
}

fn opt_bits_eq<const N: usize>(a: &Option<[f32; N]>, b: &Option<[f32; N]>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => bits_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn arc_eq<T: ?Sized + PartialEq>(a: &Arc<T>, b: &Arc<T>) -> bool {
    Arc::ptr_eq(a, b) || **a == **b
}

fn opt_arc_eq<T: ?Sized + PartialEq>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => arc_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn clip_eq(a: &Option<Clip>, b: &Option<Clip>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            a.x0.to_bits() == b.x0.to_bits()
                && a.y0.to_bits() == b.y0.to_bits()
                && a.x1.to_bits() == b.x1.to_bits()
                && a.y1.to_bits() == b.y1.to_bits()
        }
        (None, None) => true,
        _ => false,
    }
}

/// `FlowInfo` compared by bits: its `used` basis holds NaN for "unknown", so
/// `PartialEq` would never call an unchanged fragment equal. Exhaustive on
/// purpose, like [`frag_shallow_eq`].
fn flow_eq(a: &super::super::clamp::FlowInfo, b: &super::super::clamp::FlowInfo) -> bool {
    let super::super::clamp::FlowInfo {
        independent,
        inline_box,
        hidden,
        elided,
        clamp_container,
        float_clip_end,
        auto_height,
        margin_bottom,
        offset_y,
        inline_cb,
        used,
    } = a;
    *independent == b.independent
        && *inline_box == b.inline_box
        && *hidden == b.hidden
        && *elided == b.elided
        && *clamp_container == b.clamp_container
        && float_clip_end.map(f32::to_bits) == b.float_clip_end.map(f32::to_bits)
        && opt_bits_eq(auto_height, &b.auto_height)
        && margin_bottom.to_bits() == b.margin_bottom.to_bits()
        && offset_y.to_bits() == b.offset_y.to_bits()
        && *inline_cb == b.inline_cb
        && bits_eq(&used.cb, &b.used.cb)
        && bits_eq(&used.inset, &b.used.inset)
}

/// Equality of two fragments' own data (everything except `children`), with
/// floats compared by bits. The destructuring is exhaustive on purpose: a
/// new `Frag` field must be classified here before the crate builds.
pub(super) fn frag_shallow_eq(a: &Frag, b: &Frag) -> bool {
    let Frag {
        flow,
        node,
        x,
        y,
        w,
        h,
        border,
        css_size,
        content_size,
        content_offset,
        paint,
        clip,
        kind,
        children: _,
    } = a;
    *node == b.node
        && x.to_bits() == b.x.to_bits()
        && y.to_bits() == b.y.to_bits()
        && w.to_bits() == b.w.to_bits()
        && h.to_bits() == b.h.to_bits()
        && bits_eq(border, &b.border)
        && opt_bits_eq(css_size, &b.css_size)
        && opt_bits_eq(content_size, &b.content_size)
        && bits_eq(content_offset, &b.content_offset)
        && clip_eq(clip, &b.clip)
        && kind_eq(kind, &b.kind)
        && paint_flags_eq(paint, &b.paint)
        && flow_eq(flow, &b.flow)
}

fn kind_eq(a: &FragKind, b: &FragKind) -> bool {
    match (a, b) {
        (FragKind::Block, FragKind::Block) => true,
        (FragKind::TableCell(a), FragKind::TableCell(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|((na, ra), (nb, rb))| na == nb && bits_eq(ra, rb))
        }
        // Layout allocates a line again whenever it lays its block again,
        // equal or not; identical content paints identically.
        (FragKind::Line(a), FragKind::Line(b)) => Arc::ptr_eq(a, b) || **a == **b,
        (FragKind::Fixed(a), FragKind::Fixed(b)) => a == b,
        _ => false,
    }
}

fn paint_flags_eq(a: &super::super::flow::PaintFlags, b: &super::super::flow::PaintFlags) -> bool {
    let super::super::flow::PaintFlags {
        position,
        overflow,
        outside_marker,
        pseudo,
        positioned,
        sc,
        z,
        opacity,
        filters,
        backdrop,
        transform,
        child_viewport,
        bg,
        outline,
        cb_abs,
        cb_fixed,
        float,
        border_collapsed,
        collapsed_borders,
        column_rules,
    } = a;
    *position == b.position
        && *overflow == b.overflow
        && *outside_marker == b.outside_marker
        && *pseudo == b.pseudo
        && *positioned == b.positioned
        && *sc == b.sc
        && *z == b.z
        && opacity.to_bits() == b.opacity.to_bits()
        && arc_eq(filters, &b.filters)
        && *backdrop == b.backdrop
        && opt_arc_eq(transform, &b.transform)
        && *child_viewport == b.child_viewport
        && *bg == b.bg
        && *outline == b.outline
        && *cb_abs == b.cb_abs
        && *cb_fixed == b.cb_fixed
        && *float == b.float
        && *border_collapsed == b.border_collapsed
        && match (collapsed_borders, &b.collapsed_borders) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
        && opt_arc_eq(column_rules, &b.column_rules)
}

/// Every node a fragment's paint can name: the roots of all its Dom reads.
/// Their flat-tree ancestors are covered by the dirty set's expansion.
fn chunk_nodes(fragment: &Frag) -> Box<[NodeId]> {
    let mut nodes = Vec::new();
    let mut add = |node: NodeId| {
        if node != NO_NODE {
            nodes.push(node);
        }
    };
    add(fragment.node);
    if let Some((node, _)) = fragment.paint.pseudo {
        add(node);
    }
    match &fragment.kind {
        FragKind::TableCell(layers) => {
            for (node, _) in layers.iter() {
                add(*node);
            }
        }
        FragKind::Line(line) => {
            for piece in &line.pieces {
                add(piece.item.node);
                add(piece.item.style_node);
                if let Some((node, _)) = piece.item.pseudo {
                    add(node);
                }
                for span in &piece.sources {
                    add(span.node);
                }
                if let Some(boxes) = &piece.boxes {
                    for entry in boxes.chain.iter() {
                        add(entry.key.0);
                    }
                }
            }
        }
        _ => {}
    }
    nodes.sort_unstable();
    nodes.dedup();
    nodes.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    //! Differential fuzzing of the retained leaf cache against a full paint.
    //!
    //! Every cached paint under `cfg(test)` is also painted in full by a fresh
    //! builder and compared (`paint_in_spaces`); a difference panics with the
    //! first differing command. These tests drive a live Dom through random
    //! documents and mutations, repainting after each, so any input the reuse
    //! conditions miss shows up as a panic that names the seed and the
    //! operations so far.
    use super::LAST_RESET;
    use super::*;
    use crate::dom::Dom;
    use crate::layout2::{ImageSizes, Viewport, measure_retained_layout, paint_retained_layout};
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    /// xorshift64*, seeded; the only source of randomness.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() >> 11) as usize % n.max(1)
        }

        fn chance(&mut self, percent: usize) -> bool {
            self.below(100) < percent
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }
    }

    const CLASSES: &[&str] = &[
        "pos", "abs", "zneg", "zpos", "op", "tf", "tr", "fl", "ov", "sc", "bg", "bd", "sh", "rad",
        "col", "big", "fx", "st", "iso", "clp", "flt", "hid", "ul", "bi", "bef", "aft", "flex",
        "grid", "ib", "hv", "fo", "tc", "cr", "sel", "mq", "cc", "bcol", "el", "rtl", "mbg", "msk",
        "bf", "pn", "cur", "vert", "tclip", "out", "tsh", "fx2", "lh", "anim", "anim2", "trans",
        "colb",
    ];

    const SHEET: &str = "
        .pos{position:relative;top:2px;left:1px}
        .abs{position:absolute;top:4px;left:6px;width:60px;height:20px;background:#dde}
        .zneg{position:relative;z-index:-1;background:#fed}
        .zpos{position:relative;z-index:3;background:#def}
        .op{opacity:.6}
        .tf{transform:translate(3px,2px)}
        .tr{transform:rotate(3deg)}
        .fl{float:left;width:50px;height:20px;background:#efe}
        .ov{overflow:hidden;height:30px;width:120px}
        .sc{overflow:auto;height:30px;width:120px}
        .bg{background:#ccd}
        .bd{border:2px solid #333;padding:2px}
        .sh{box-shadow:2px 2px 3px #888}
        .rad{border-radius:6px;background:#cde}
        .col{color:#a33}
        .big{font-size:20px}
        .fx{position:fixed;bottom:0;right:0;background:#fff}
        .st{position:sticky;top:0}
        .iso{isolation:isolate}
        .clp{clip-path:inset(2px)}
        .flt{filter:blur(1px)}
        .hid{visibility:hidden}
        .ul{text-decoration:underline}
        .bi{background-image:linear-gradient(red,blue)}
        .bef::before{content:'x';color:red}
        .aft::after{content:'y'}
        .flex{display:flex;gap:4px}
        .grid{display:grid;grid-template-columns:1fr 1fr}
        .ib{display:inline-block;width:30px;height:20px;background:#eee}
        .hv:hover{background:#fcc;color:red}
        .fo:focus{outline:2px solid blue}
        .tc{text-align:center}
        .cr{clip:rect(0,0,0,0);position:absolute}
        .sel::selection{background:yellow}
        .mq{opacity:.9}
        .cc{column-count:2;column-rule:1px solid #c00;width:200px}
        .bcol{border-collapse:collapse}
        td{border:1px solid #888;padding:1px}
        .el{overflow:hidden;white-space:nowrap;text-overflow:ellipsis;width:60px}
        .rtl{direction:rtl;unicode-bidi:bidi-override}
        .mbg{background:linear-gradient(#fee,#eef),#ffd;background-size:10px 10px,auto}
        .msk{-webkit-mask-image:linear-gradient(black,transparent);mask-image:linear-gradient(black,transparent)}
        .bf{backdrop-filter:blur(2px)}
        .pn{pointer-events:none}
        .cur{cursor:pointer}
        .vert{writing-mode:vertical-rl;height:60px}
        .tclip{-webkit-background-clip:text;background-clip:text;color:transparent;background-image:linear-gradient(red,blue)}
        .out{outline:2px solid green;outline-offset:2px}
        .tsh{text-shadow:1px 1px 2px #999}
        .fx2{position:fixed;top:0;left:0;z-index:2;background:#ffc;width:40px;height:20px}
        .lh{line-height:2}
        @keyframes pulse{from{opacity:1;color:#000}to{opacity:.4;color:#c00}}
        @keyframes slide{from{background-color:#eef}to{background-color:#fee}}
        .anim{animation:pulse 2s linear infinite}
        .anim2{animation:slide 1s linear infinite alternate}
        .trans{transition:color 1s linear,background-color 1s linear}
        .colb{color:#00c;background-color:#cfc}
        @media (max-width:300px){ .bg{background:#fcc} .big{font-size:12px} }
    ";

    const WORDS: &[&str] = &[
        "alpha",
        "beta",
        "gamma delta",
        "epsilon zeta eta",
        "theta",
        "iota kappa lambda mu",
        "A longer run of words that will certainly wrap across several lines of the box",
    ];

    const SVG_IMAGES: &[&str] = &[
        "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='20' height='10'%3E%3Crect width='20' height='10' fill='red'/%3E%3C/svg%3E",
        "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='12' height='12'%3E%3Ccircle cx='6' cy='6' r='5'/%3E%3C/svg%3E",
    ];

    struct Fuzz {
        rng: Rng,
        dom: Dom,
        base: Url,
        viewport: Viewport,
        images: ImageSizes,
        log: Rc<RefCell<Vec<String>>>,
        next_id: usize,
        /// The document timeline, in seconds.
        clock: f64,
        frames: usize,
        hits: u32,
        leaves: u32,
    }

    /// Prints the seed and the operations so far when the test panics.
    struct Report {
        seed: u64,
        log: Rc<RefCell<Vec<String>>>,
    }

    impl Drop for Report {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("paint cache fuzz FAILED: seed={}", self.seed);
                for (i, line) in self.log.borrow().iter().enumerate() {
                    eprintln!("  {i:4}: {line}");
                }
            }
        }
    }

    impl Fuzz {
        fn new(seed: u64) -> Self {
            let mut rng = Rng::new(seed);
            let mut next_id = 0;
            let mut body = String::new();
            for _ in 0..3 + rng.below(4) {
                element(&mut rng, 0, &mut next_id, &mut body);
            }
            let html = format!("<style>{SHEET}</style><div id=root>{body}</div>");
            Self::from_html(rng, next_id, html)
        }

        fn from_html(rng: Rng, next_id: usize, html: String) -> Self {
            let mut dom = Dom::parse_document(&html);
            dom.set_render_clickables(HashSet::new(), true);
            Self {
                rng,
                dom,
                base: Url::parse("https://example.com/").unwrap(),
                viewport: Viewport::new(480., 320.),
                images: ImageSizes::new(),
                log: Rc::new(RefCell::new(vec![format!("document: {html}")])),
                next_id,
                clock: 0.0,
                frames: 0,
                hits: 0,
                leaves: 0,
            }
        }

        /// Lay out and paint the document as the live page does; the cached
        /// paint is verified against a full one inside.
        fn frame(&mut self) -> Stats {
            let (forms, controls) = crate::http::extract_forms_arena(&self.dom, &self.base, None);
            let measured = measure_retained_layout(
                &self.dom,
                &self.base,
                self.viewport,
                &forms,
                &controls,
                &self.images,
            );
            let Some(fragments) = measured.fragments else {
                return Stats::default();
            };
            let _ = paint_retained_layout(
                &self.dom,
                &self.base,
                &controls,
                &self.images,
                fragments,
                measured.boxes,
                measured.tracks,
                false,
            );
            self.frames += 1;
            let stats = super::LAST_STATS
                .with(|last| last.take())
                .unwrap_or_default();
            self.hits += stats.hits;
            self.leaves += stats.leaves;
            self.note(format!(
                "    paint: roots={} reset={} leaves={} hit={} new={} dirty={} reads={} tainted={}",
                super::LAST_ROOTS.with(|roots| std::mem::take(&mut *roots.borrow_mut())),
                super::LAST_RESET.with(|reset| reset.borrow().clone()),
                stats.leaves,
                stats.hits,
                stats.new,
                stats.dirty,
                stats.reads,
                stats.tainted,
            ));
            stats
        }

        fn elements(&self) -> Vec<NodeId> {
            self.dom
                .descendants(crate::dom::DOCUMENT)
                .filter(|&id| {
                    self.dom
                        .attr(id, "id")
                        .is_some_and(|name| name.starts_with('e'))
                })
                .collect()
        }

        fn leaf_nodes(&self) -> Vec<NodeId> {
            self.dom
                .descendants(crate::dom::DOCUMENT)
                .filter(|&id| {
                    self.dom.tag_name(id).is_none()
                        && !self.dom.text_content(id).is_empty()
                        && self
                            .dom
                            .parent_flat(id)
                            .is_some_and(|parent| self.dom.tag_name(parent) != Some("style"))
                })
                .collect()
        }

        fn note(&self, line: String) {
            self.log.borrow_mut().push(line);
        }

        fn step(&mut self) {
            let elements = self.elements();
            if elements.is_empty() {
                return;
            }
            let pick = self.rng.below(100);
            let target = *self.rng.pick(&elements);
            let name = self.dom.attr(target, "id").unwrap_or("?").to_string();
            match pick {
                0..=7 => {
                    let leaves = self.leaf_nodes();
                    if let Some(&text) = leaves.get(self.rng.below(leaves.len().max(1))) {
                        let words = (*self.rng.pick(WORDS)).to_string();
                        self.note(format!("set_text node {text} {words:?}"));
                        self.dom.set_text(text, &words);
                    }
                }
                8..=22 => {
                    let class = *self.rng.pick(CLASSES);
                    let old = self.dom.attr(target, "class").unwrap_or("").to_string();
                    let mut tokens: Vec<&str> = old.split_whitespace().collect();
                    if let Some(at) = tokens.iter().position(|token| *token == class) {
                        tokens.remove(at);
                    } else {
                        tokens.push(class);
                    }
                    let new = tokens.join(" ");
                    self.note(format!("class {name}: {old:?} -> {new:?}"));
                    self.dom.set_attr(target, "class", &new);
                }
                23..=30 => {
                    let styles = [
                        "color:#36c",
                        "margin-left:7px",
                        "background:#9c9",
                        "padding:3px",
                        "opacity:.5",
                        "font-weight:bold",
                        "z-index:2;position:relative",
                        "transform:scale(.9)",
                        "overflow:hidden;height:16px",
                        "display:none",
                        "border:1px dashed #333",
                        "text-decoration:line-through",
                    ];
                    if self.rng.chance(25) {
                        self.note(format!("remove style {name}"));
                        self.dom.remove_attr(target, "style");
                    } else {
                        let style = *self.rng.pick(&styles);
                        self.note(format!("style {name}: {style:?}"));
                        self.dom.set_attr(target, "style", style);
                    }
                }
                31..=38 => {
                    let mut html = String::new();
                    element(&mut self.rng, 2, &mut self.next_id, &mut html);
                    self.note(format!("insert into {name}: {html}"));
                    let children = self.dom.parse_fragment_in(Some(target), &html);
                    let reference = self.dom.child_iter(target).nth(self.rng.below(3));
                    for child in children {
                        self.dom.detach(child);
                        self.dom.insert_before(target, child, reference);
                    }
                }
                39..=43 => {
                    if name != "root" {
                        self.note(format!("remove {name}"));
                        self.dom.detach(target);
                    }
                }
                44..=47 => {
                    let destination = *self.rng.pick(&elements);
                    let inside =
                        std::iter::successors(Some(destination), |&n| self.dom.parent_flat(n))
                            .any(|ancestor| ancestor == target);
                    if name != "root" && !inside {
                        self.note(format!(
                            "move {name} into {}",
                            self.dom.attr(destination, "id").unwrap_or("?")
                        ));
                        self.dom.detach(target);
                        self.dom.insert_before(destination, target, None);
                    }
                }
                48..=52 => {
                    let chosen = self.rng.chance(80).then_some(target);
                    self.note(format!("hover {chosen:?}"));
                    self.dom.set_hover_chain(chosen);
                }
                53..=56 => {
                    let top = (self.rng.below(60)) as f64;
                    self.note(format!("scroll {name} to {top}"));
                    self.dom.set_scroll_pos(target, top, 0.0, false);
                }
                57..=60 => {
                    let source = *self.rng.pick(SVG_IMAGES);
                    let size = if source.contains("circle") {
                        (12, 12)
                    } else {
                        (20, 10)
                    };
                    self.note(format!("image arrives {} {size:?}", &source[..40]));
                    self.images.insert(source.to_string(), size);
                    self.dom.resource_changed();
                }
                61..=63 => {
                    let sizes = [(480., 320.), (260., 300.), (640., 360.), (480., 240.)];
                    let (w, h) = *self.rng.pick(&sizes);
                    self.note(format!("viewport {w}x{h}"));
                    self.viewport = Viewport::new(w, h);
                    self.dom.set_viewport_px(w, h);
                }
                64..=66 => {
                    self.note("insert style sheet".into());
                    let style = self.dom.create_element("style");
                    let rule = format!(".{}{{outline:1px solid #090}}", self.rng.pick(CLASSES));
                    self.dom.append_text(style, &rule);
                    let root = self.dom.document_element().unwrap_or(crate::dom::DOCUMENT);
                    self.dom.insert_before(root, style, None);
                }
                67..=69 => {
                    let mut set = HashSet::new();
                    for _ in 0..self.rng.below(4) {
                        set.insert(*self.rng.pick(&elements));
                    }
                    self.note(format!("clickables {set:?}"));
                    self.dom.set_render_clickables(set, true);
                }
                70..=71 => {
                    self.note(format!("canvas pixels {name}"));
                    self.dom.canvas_changed(target);
                }
                72..=78 => {
                    let attrs = [
                        ("inert", ""),
                        ("hidden", ""),
                        ("dir", "rtl"),
                        ("lang", "ar"),
                        ("title", "t"),
                        ("href", "/next"),
                        ("contenteditable", "true"),
                        ("tabindex", "0"),
                    ];
                    let (key, value) = *self.rng.pick(&attrs);
                    if self.dom.attr(target, key).is_some() {
                        self.note(format!("remove {key} from {name}"));
                        self.dom.remove_attr(target, key);
                    } else {
                        self.note(format!("set {key}={value:?} on {name}"));
                        self.dom.set_attr(target, key, value);
                    }
                }
                79..=80 => {
                    let ratio = if self.rng.chance(50) { 1.0 } else { 2.0 };
                    self.note(format!("device pixel ratio {ratio}"));
                    self.dom.set_device_pixel_ratio(ratio);
                }
                81..=83 => {
                    let text = *self.rng.pick(WORDS);
                    self.note(format!("append text to {name}: {text:?}"));
                    self.dom.append_text(target, text);
                }
                84..=85 => {
                    self.note(format!("shadow root on {name}"));
                    if self.dom.shadow_root(target).is_none()
                        && self.dom.tag_name(target) == Some("div")
                    {
                        let shadow = self.dom.attach_shadow(target);
                        let wrapper = self.dom.create_element("section");
                        self.dom.set_attr(wrapper, "class", "bd");
                        let slot = self.dom.create_element("slot");
                        self.dom.insert_before(wrapper, slot, None);
                        self.dom.insert_before(shadow, wrapper, None);
                    }
                }
                86..=87 => {
                    self.note(format!("popover toggle {name}"));
                    let open = !self.dom.is_popover_showing(target);
                    self.dom.set_popover_open(target, open);
                }
                88..=89 => {
                    self.note(format!("image changed {name}"));
                    self.dom.image_changed(target);
                }
                90..=91 => {
                    let rule = format!(
                        ".{}{{background:#{:x}}}",
                        self.rng.pick(CLASSES),
                        0xaaa + self.rng.below(0x400)
                    );
                    self.note(format!("edit sheet: {rule}"));
                    let style = self
                        .dom
                        .descendants(crate::dom::DOCUMENT)
                        .find(|&id| self.dom.tag_name(id) == Some("style"));
                    let text = style.and_then(|style| self.dom.child_iter(style).next());
                    if let Some(text) = text {
                        let sheet = format!("{SHEET} {rule}");
                        self.dom.set_text(text, &sheet);
                    }
                }
                94..=97 => {
                    self.clock += 0.1 + self.rng.below(15) as f64 / 10.0;
                    self.note(format!("animation time {:.1}s", self.clock));
                    self.dom.update_css_animations(self.clock);
                    self.dom.update_css_transitions(self.clock);
                }
                92..=93 => {
                    let focus = self.rng.chance(75).then_some(target);
                    self.note(format!("focus {focus:?}"));
                    self.dom.set_focused_area(crate::dom::DOCUMENT, focus);
                }
                _ => self.note("repaint".into()),
            }
            self.frame();
        }
    }

    /// A random element: tag, classes, content, children.
    fn element(rng: &mut Rng, depth: usize, next_id: &mut usize, out: &mut String) {
        let id = *next_id;
        *next_id += 1;
        let class: Vec<&str> = (0..rng.below(4)).map(|_| *rng.pick(CLASSES)).collect();
        let class = class.join(" ");
        let tag = *rng.pick(&[
            "div", "div", "div", "p", "span", "span", "a", "b", "section", "ul", "table", "img",
            "button", "canvas", "em",
        ]);
        let words = *rng.pick(WORDS);
        match tag {
            "img" => {
                let source = *rng.pick(SVG_IMAGES);
                out.push_str(&format!(
                    "<img id=e{id} class='{class}' src=\"{source}\" width=20 height=10>"
                ));
                return;
            }
            "canvas" => {
                out.push_str(&format!(
                    "<canvas id=e{id} class='{class}' width=20 height=10></canvas>"
                ));
                return;
            }
            "button" => {
                out.push_str(&format!(
                    "<button id=e{id} class='{class}'>{words}</button>"
                ));
                return;
            }
            "table" => {
                out.push_str(&format!(
                    "<table id=e{id} class='{class}'><tr><td id=e{}>{words}</td><td id=e{}>cell</td></tr></table>",
                    *next_id,
                    *next_id + 1
                ));
                *next_id += 2;
                return;
            }
            "ul" => {
                out.push_str(&format!(
                    "<ul id=e{id} class='{class}'><li id=e{}>{words}</li><li id=e{}>second</li></ul>",
                    *next_id,
                    *next_id + 1
                ));
                *next_id += 2;
                return;
            }
            _ => {}
        }
        let href = if tag == "a" { " href='/x'" } else { "" };
        out.push_str(&format!("<{tag} id=e{id} class='{class}'{href}>{words} "));
        if depth < 3 {
            for _ in 0..rng.below(4) {
                element(rng, depth + 1, next_id, out);
                if rng.chance(40) {
                    out.push_str(rng.pick(WORDS));
                }
            }
        }
        out.push_str(&format!("</{tag}>"));
    }

    fn run(seed: u64, steps: usize) -> (u32, u32) {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut fuzz = Fuzz::new(seed);
        let _report = Report {
            seed,
            log: fuzz.log.clone(),
        };
        fuzz.frame();
        for _ in 0..steps {
            fuzz.step();
        }
        (fuzz.hits, fuzz.leaves)
    }

    #[test]
    fn shallow_equality_sees_every_field() {
        use crate::layout2::overflow::Overflow;
        use crate::layout2::style::Pos;
        let base = Frag::empty();
        assert!(frag_shallow_eq(&base, &base.clone()));
        let mut variants: Vec<(&str, Frag)> = Vec::new();
        macro_rules! vary {
            ($($path:ident).+ = $value:expr) => {{
                let mut f = base.clone();
                f.$($path).+ = $value;
                variants.push((stringify!($($path).+), f));
            }};
        }
        vary!(node = 3);
        vary!(x = 1.0);
        vary!(y = 1.0);
        vary!(w = 1.0);
        vary!(h = 1.0);
        vary!(border = [0.0, 1.0, 0.0, 0.0]);
        vary!(css_size = Some([1.0, 1.0]));
        vary!(content_size = Some([1.0, 1.0]));
        vary!(content_offset = [0.0, 1.0]);
        vary!(
            clip = Some(Clip {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0
            })
        );
        vary!(kind = FragKind::Fixed(1));
        vary!(kind = FragKind::TableCell(vec![(1, [0.0; 4])].into()));
        vary!(paint.position = Pos::Absolute);
        vary!(paint.overflow = [Overflow::Hidden, Overflow::Visible]);
        vary!(paint.outside_marker = true);
        vary!(paint.pseudo = Some((1, crate::dom::PseudoEl::Before)));
        vary!(paint.positioned = true);
        vary!(paint.sc = true);
        vary!(paint.z = Some(1));
        vary!(paint.opacity = 0.5);
        vary!(paint.backdrop = true);
        vary!(paint.child_viewport = true);
        vary!(paint.bg = true);
        vary!(
            paint.outline = Outline {
                width: 1.0,
                offset: 0.0,
                style: OutlineStyle::Solid
            }
        );
        vary!(paint.cb_abs = true);
        vary!(paint.cb_fixed = true);
        vary!(paint.float = true);
        vary!(paint.border_collapsed = true);
        vary!(flow.independent = true);
        vary!(flow.inline_box = true);
        vary!(flow.hidden = true);
        vary!(flow.elided = true);
        vary!(flow.clamp_container = true);
        vary!(flow.float_clip_end = Some(1.0));
        vary!(flow.auto_height = Some([1.0, 2.0]));
        vary!(flow.margin_bottom = 1.0);
        vary!(flow.offset_y = 1.0);
        vary!(flow.inline_cb = true);
        vary!(flow.used.cb = [1.0, 1.0]);
        vary!(flow.used.inset = [1.0, 0.0, 0.0, 0.0]);
        for (field, variant) in &variants {
            assert!(!frag_shallow_eq(&base, variant), "{field} is not compared");
            assert!(!frag_shallow_eq(variant, &base), "{field} is not compared");
            assert!(frag_shallow_eq(variant, &variant.clone()), "{field}");
        }
        // Layout leaves NaN in `used` for unknown bases.
        let mut unknown = base.clone();
        unknown.flow.used.cb[1] = f32::NAN;
        assert!(frag_shallow_eq(&unknown, &unknown.clone()));
        // Children are not part of a fragment's own paint.
        let mut parent = base.clone();
        parent.children.push(Frag::empty());
        assert!(frag_shallow_eq(&base, &parent));
    }

    fn page(body: &str) -> Fuzz {
        let html = format!("<style>{SHEET}</style><div id=root>{body}</div>");
        Fuzz::from_html(Rng::new(0), 1000, html)
    }

    fn by_id(fuzz: &Fuzz, id: &str) -> NodeId {
        fuzz.dom.get_by_id(id).unwrap_or_else(|| panic!("no #{id}"))
    }

    #[test]
    fn an_unchanged_document_repaints_from_the_cache() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut fuzz = page(
            "<p id=a>first paragraph</p><p id=b class=bg>second <b>bold</b> paragraph</p>
             <div id=c class='bd rad'>box <em>em</em></div>",
        );
        let first = fuzz.frame();
        assert_eq!(first.hits, 0, "{first:?}");
        assert!(first.leaves > 4, "{first:?}");
        let second = fuzz.frame();
        assert_eq!(second.hits + second.tainted, second.leaves, "{second:?}");
        assert_eq!(second.new + second.dirty + second.reads, 0, "{second:?}");
        assert!(second.replayed_commands > 0, "{second:?}");
    }

    #[test]
    fn a_text_change_repaints_only_what_names_it() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut fuzz = page(
            "<p id=a>first paragraph</p><p id=b>second <b id=bold>bold</b> paragraph</p>
             <p id=c>third paragraph</p><p id=d>fourth paragraph</p>",
        );
        fuzz.frame();
        let bold = by_id(&fuzz, "bold");
        let text = fuzz.dom.child_iter(bold).next().unwrap();
        fuzz.dom.set_text(text, "changed");
        let stats = fuzz.frame();
        assert!(stats.hits >= stats.leaves - 3, "{stats:?}");
        assert!(stats.new + stats.dirty >= 1, "{stats:?}");
    }

    #[test]
    fn a_style_change_of_an_ancestor_repaints_its_descendants() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut fuzz = page("<div id=a><p id=b>inside <i>deep</i></p></div><p id=c>outside</p>");
        fuzz.frame();
        let a = by_id(&fuzz, "a");
        fuzz.dom.set_attr(a, "class", "col ul");
        let stats = fuzz.frame();
        // The inherited color and decoration reach the lines below #a only.
        assert!(stats.dirty + stats.new >= 2, "{stats:?}");
        assert!(stats.hits >= 1, "{stats:?}");
    }

    #[test]
    fn document_wide_changes_drop_the_cache() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut fuzz = page("<p>one</p><p>two</p>");
        fuzz.frame();
        fuzz.viewport = Viewport::new(300., 200.);
        fuzz.dom.set_viewport_px(300., 200.);
        let stats = fuzz.frame();
        assert_eq!(stats.hits, 0, "{stats:?}");
        let again = fuzz.frame();
        assert_eq!(again.hits + again.tainted, again.leaves, "{again:?}");
    }

    #[test]
    fn controls_canvases_and_generated_boxes_are_never_replayed() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut fuzz = page(
            "<p id=plain>plain</p><button id=button>press</button><canvas id=canvas width=20 height=10></canvas>
             <span class=bef id=generated>generated</span>",
        );
        fuzz.frame();
        let stats = fuzz.frame();
        assert!(stats.tainted >= 2, "{stats:?}");
        assert!(stats.hits >= 1, "{stats:?}");
    }

    #[test]
    fn a_page_that_moves_every_fragment_each_frame_backs_off() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut fuzz = page(&"<p>some text</p>".repeat(700));
        assert_eq!(fuzz.frame().hits, 0);
        let root = by_id(&fuzz, "root");
        let spacer = fuzz.dom.create_element("div");
        fuzz.dom.set_attr(spacer, "style", "height:3px");
        let first = fuzz.dom.child_iter(root).next();
        fuzz.dom.insert_before(root, spacer, first);
        // Everything below the spacer moved: a large paint that reuses little.
        let cold = fuzz.frame();
        let reset = LAST_RESET.with(|reset| reset.borrow().clone());
        assert_eq!(reset, "None", "{cold:?}");
        assert!(
            cold.leaves >= 1000 && cold.hits * 4 < cold.leaves,
            "{cold:?}"
        );
        // The next paint runs uncached rather than recording a page that
        // is about to move again.
        assert_eq!(fuzz.frame().leaves, 0);
        fuzz.dom.set_attr(spacer, "style", "height:5px");
        // Then the cache starts over: a first paint, and reuse after it.
        let first = fuzz.frame();
        assert!(first.leaves > 1000 && first.hits == 0, "{first:?}");
        let warm = fuzz.frame();
        assert_eq!(warm.hits + warm.tainted, warm.leaves, "{warm:?}");
    }

    #[test]
    fn a_page_that_resets_the_cache_every_frame_backs_off() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let mut fuzz = page(&"<p>some text</p>".repeat(700));
        fuzz.frame();
        let mut skipped = 0;
        for frame in 0..12 {
            let width = if frame % 2 == 0 { 300. } else { 480. };
            fuzz.viewport = Viewport::new(width, 320.);
            fuzz.dom.set_viewport_px(width, 320.);
            if fuzz.frame().leaves == 0 {
                skipped += 1;
            }
        }
        assert!(skipped >= 4, "{skipped} of 12 paints were uncached");
    }

    #[test]
    fn shadow_mode_validates_without_replaying() {
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let _mode = force_mode(Mode::Shadow);
        let mut fuzz = page("<p>one</p><p class=col>two</p>");
        fuzz.frame();
        let stats = fuzz.frame();
        assert!(stats.hits > 0, "{stats:?}");
        assert_eq!(stats.replayed_commands, 0, "{stats:?}");
    }

    #[test]
    fn fuzz_seeds() {
        let mut hits = 0;
        let mut leaves = 0;
        for seed in 1..=10 {
            let (h, l) = run(seed, 200);
            hits += h;
            leaves += l;
        }
        assert!(leaves > 0);
        assert!(hits > 0, "the cache never hit in {leaves} leaf paints");
    }

    /// A long local soak: `TRUST_PAINT_FUZZ_SEEDS=first..last` (default
    /// 1..200) and `TRUST_PAINT_FUZZ_STEPS` (default 300); run with
    /// `cargo test --lib paint_cache::tests::fuzz_soak -- --ignored`.
    #[test]
    #[ignore = "long-running soak; run explicitly"]
    fn fuzz_soak() {
        let seeds = std::env::var("TRUST_PAINT_FUZZ_SEEDS").unwrap_or_else(|_| "1..200".into());
        let (first, last) = seeds.split_once("..").expect("first..last");
        let steps = std::env::var("TRUST_PAINT_FUZZ_STEPS")
            .ok()
            .and_then(|steps| steps.parse().ok())
            .unwrap_or(300);
        let (mut hits, mut leaves) = (0u64, 0u64);
        for seed in first.parse::<u64>().unwrap()..last.parse::<u64>().unwrap() {
            let (h, l) = run(seed, steps);
            hits += u64::from(h);
            leaves += u64::from(l);
        }
        eprintln!("soak: {hits} hits in {leaves} leaf paints");
    }
}
