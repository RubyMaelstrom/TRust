//! The live, scriptable document: an arena DOM built straight from
//! html5ever, mutated by page JavaScript (through js.rs), then either
//! laid out into rows directly (layout.rs) or serialized back to HTML
//! for the app to re-parse and lay out.
//!
//! Deliberately NOT rcdom: a mutable DOM can't live with rcdom's
//! Node::drop force-clearing children (see CLAUDE.md), and an arena of
//! indices gives JS a flat, GC-free handle type — wrappers hold a
//! `NodeId`, the whole arena drops with the page.

use std::borrow::Cow;
use std::cell::{Ref, RefCell};

use html5ever::interface::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, ParseOpts, QualName, ns};

pub type NodeId = usize;

pub enum NodeData {
    Document,
    /// A document fragment: template contents, fragment-parse roots.
    Fragment,
    Doctype,
    Comment(String),
    Text(String),
    Element {
        name: QualName,
        attrs: Vec<Attribute>,
        /// `<template>` parses its children into a separate fragment.
        template_contents: Option<NodeId>,
    },
}

pub struct Node {
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub last_child: Option<NodeId>,
    pub prev_sibling: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub data: NodeData,
}

/// Elements that close themselves in HTML serialization.
const VOID_ELEMENTS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

thread_local! {
    /// Diagnostic only (`TRUST_NET_TRACE`): `trace_ms()` of the most recent
    /// DOM mutation, for sizing the DOM-stable→load-finish tail.
    static LAST_MUTATION_MS: std::cell::Cell<u128> = const { std::cell::Cell::new(0) };
}

/// The `trace_ms()` of the last DOM mutation on this thread (diagnostic).
pub fn last_mutation_ms() -> u128 {
    LAST_MUTATION_MS.with(|c| c.get())
}

pub struct Dom {
    nodes: Vec<Node>,
    /// host element → shadow root fragment (attachShadow).
    shadow_roots: std::collections::HashMap<NodeId, NodeId>,
    /// and the reverse: shadow root fragment → host element.
    shadow_hosts: std::collections::HashMap<NodeId, NodeId>,
    /// Set by every tree/attribute mutation; the living page takes it
    /// to decide whether a dispatch warrants re-extraction at all.
    dirty: bool,
    /// Monotonic mutation counter (bumped with `dirty`); keys the
    /// cached visibility cascade so it rebuilds only after changes.
    epoch: u64,
    /// adoptedStyleSheets text per scope (DOCUMENT or a shadow root
    /// fragment), pushed by the prelude on adoption/replaceSync.
    adopted_styles: std::collections::HashMap<NodeId, String>,
    /// Fetched `<link rel=stylesheet>` text, keyed by the link element.
    external_sheets: std::collections::HashMap<NodeId, String>,
    /// Lazily built visibility cascade, valid for one epoch.
    style_cache: RefCell<Option<(u64, std::rc::Rc<StyleIndex>)>>,
    /// Memoized inherited `computed_value` results for the current epoch,
    /// keyed (node, property index). Inheritance walks ancestors, so the
    /// layout's per-element reads would re-walk without this; cleared when
    /// the epoch advances.
    computed_cache: RefCell<ComputedCache>,
    /// Memoized selector-match results for the current epoch: for an element,
    /// the indices (into its tree scope's rule vec) of every author rule whose
    /// selector matches it. Selector matching is the cascade's dominant cost on
    /// CSS-heavy pages, and the layout/serializer read 30+ properties per
    /// element — without this each read re-matched every rule (O(elements ×
    /// rules × props)). With it, each element is matched ONCE per epoch (via the
    /// rightmost-key buckets), then every property/pseudo read reuses the list.
    matched_cache: RefCell<MatchedCache>,
    /// The CSS-pixel viewport (`cols*cell_px`, `rows*cell_px`) used to
    /// evaluate `@media` queries when the cascade is built; `(0, 0)` = unknown
    /// (width/height queries then conservatively don't match, as if skipped).
    /// Set by `execute_js` from `PageEnv`.
    viewport_px: (u32, u32),
}

/// Per-epoch memo for `computed_value`: the epoch the entries are valid for,
/// and inherited results keyed `(node, property index)`.
type ComputedCache = (
    u64,
    std::collections::HashMap<(NodeId, usize), Option<String>>,
);

/// Per-epoch memo for `matched_rules`: the epoch the entries are valid for, and
/// the matching author-rule indices (into the element's tree-scope rule vec)
/// per element, shared via `Rc` so every property read clones cheaply.
type MatchedCache = (
    u64,
    std::collections::HashMap<NodeId, std::rc::Rc<Vec<u32>>>,
);

/// The document node is always index 0.
pub const DOCUMENT: NodeId = 0;

impl Default for Dom {
    fn default() -> Self {
        Self::new()
    }
}

impl Dom {
    pub fn new() -> Self {
        let mut dom = Dom {
            nodes: Vec::new(),
            shadow_roots: std::collections::HashMap::new(),
            shadow_hosts: std::collections::HashMap::new(),
            dirty: false,
            epoch: 0,
            adopted_styles: std::collections::HashMap::new(),
            external_sheets: std::collections::HashMap::new(),
            style_cache: RefCell::new(None),
            computed_cache: RefCell::new((u64::MAX, std::collections::HashMap::new())),
            matched_cache: RefCell::new((u64::MAX, std::collections::HashMap::new())),
            viewport_px: (0, 0),
        };
        dom.new_node(NodeData::Document);
        dom
    }

    /// True when anything mutated since the last call; resets the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// The monotonic mutation counter. Anything memoized against the DOM's
    /// current shape (the geometry box map in `js.rs`, like the cascade
    /// caches here) keys on this and rebuilds when it advances.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Every mutation comes through here: the dirty bit for the living
    /// page, the epoch for the cached visibility cascade.
    fn touch(&mut self) {
        self.dirty = true;
        self.epoch = self.epoch.wrapping_add(1);
        // Diagnostic: record WHEN the DOM last changed, so we can size the
        // gap between DOM-stability and load-finish (the telemetry/idle
        // tail). Gated on the trace flag.
        if std::env::var_os("TRUST_NET_TRACE").is_some() {
            LAST_MUTATION_MS.with(|c| c.set(crate::http::trace_ms()));
        }
    }

    /// Set the CSS-pixel viewport (`cols*cell_px`, `rows*cell_px`) that
    /// `@media` queries evaluate against. Invalidates the cascade cache when
    /// it changes so breakpoint-gated rules re-resolve.
    pub fn set_viewport_px(&mut self, width: u32, height: u32) {
        if self.viewport_px != (width, height) {
            self.viewport_px = (width, height);
            self.touch();
        }
    }

    /// Parse a full HTML document into a fresh arena.
    pub fn parse_document(html: &str) -> Self {
        let sink = Sink {
            dom: RefCell::new(Dom::new()),
        };
        html5ever::parse_document(sink, ParseOpts::default()).one(StrTendril::from(html))
    }

    fn new_node(&mut self, data: NodeData) -> NodeId {
        self.nodes.push(Node {
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            data,
        });
        self.nodes.len() - 1
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn is_valid(&self, id: NodeId) -> bool {
        id < self.nodes.len()
    }

    pub fn create_element(&mut self, tag: &str) -> NodeId {
        let tag = tag.to_ascii_lowercase();
        // Script-created templates need their content fragment exactly
        // like parser-created ones (Lit renders through them).
        let template_contents = (tag == "template").then(|| self.new_node(NodeData::Fragment));
        let name = QualName::new(None, ns!(html), tag.into());
        self.new_node(NodeData::Element {
            name,
            attrs: Vec::new(),
            template_contents,
        })
    }

    pub fn create_text(&mut self, text: &str) -> NodeId {
        self.new_node(NodeData::Text(text.to_string()))
    }

    pub fn create_fragment(&mut self) -> NodeId {
        self.new_node(NodeData::Fragment)
    }

    pub fn create_comment(&mut self, text: &str) -> NodeId {
        self.new_node(NodeData::Comment(text.to_string()))
    }

    /// Unlink a node from its parent and siblings (the node and its
    /// subtree stay in the arena; arenas only ever grow — page-lifetime
    /// memory is the deal).
    pub fn detach(&mut self, id: NodeId) {
        self.touch();
        let (parent, prev, next) = {
            let n = &self.nodes[id];
            (n.parent, n.prev_sibling, n.next_sibling)
        };
        if let Some(prev) = prev {
            self.nodes[prev].next_sibling = next;
        }
        if let Some(next) = next {
            self.nodes[next].prev_sibling = prev;
        }
        if let Some(parent) = parent {
            if self.nodes[parent].first_child == Some(id) {
                self.nodes[parent].first_child = next;
            }
            if self.nodes[parent].last_child == Some(id) {
                self.nodes[parent].last_child = prev;
            }
        }
        let n = &mut self.nodes[id];
        n.parent = None;
        n.prev_sibling = None;
        n.next_sibling = None;
    }

    pub fn append(&mut self, parent: NodeId, child: NodeId) {
        self.touch();
        self.detach(child);
        let old_last = self.nodes[parent].last_child;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].prev_sibling = old_last;
        if let Some(last) = old_last {
            self.nodes[last].next_sibling = Some(child);
        } else {
            self.nodes[parent].first_child = Some(child);
        }
        self.nodes[parent].last_child = Some(child);
    }

    /// Insert `child` under `parent` immediately before `reference`;
    /// append when `reference` is None (DOM insertBefore semantics).
    pub fn insert_before(&mut self, parent: NodeId, child: NodeId, reference: Option<NodeId>) {
        let Some(reference) = reference else {
            self.append(parent, child);
            return;
        };
        if self.nodes[reference].parent != Some(parent) {
            // A real DOM throws NotFoundError; tolerate with an append.
            self.append(parent, child);
            return;
        }
        self.touch();
        self.detach(child);
        let prev = self.nodes[reference].prev_sibling;
        self.nodes[child].parent = Some(parent);
        self.nodes[child].prev_sibling = prev;
        self.nodes[child].next_sibling = Some(reference);
        self.nodes[reference].prev_sibling = Some(child);
        match prev {
            Some(prev) => self.nodes[prev].next_sibling = Some(child),
            None => self.nodes[parent].first_child = Some(child),
        }
    }

    /// Append text, merging into a trailing text node like a parser would.
    pub fn append_text(&mut self, parent: NodeId, text: &str) {
        if let Some(last) = self.nodes[parent].last_child
            && let NodeData::Text(existing) = &mut self.nodes[last].data
        {
            existing.push_str(text);
            self.touch();
            return;
        }
        let t = self.create_text(text);
        self.append(parent, t);
    }

    pub fn children(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut next = self.nodes[id].first_child;
        while let Some(c) = next {
            out.push(c);
            next = self.nodes[c].next_sibling;
        }
        out
    }

    /// The subtree under `root` in document order, excluding `root`.
    pub fn descendants(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self.children(root);
        stack.reverse();
        while let Some(id) = stack.pop() {
            out.push(id);
            let mut kids = self.children(id);
            kids.reverse();
            stack.extend(kids);
        }
        out
    }

    pub fn tag_name(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { name, .. } => Some(&name.local),
            _ => None,
        }
    }

    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Element { attrs, .. } => attrs
                .iter()
                .find(|a| str::eq_ignore_ascii_case(&a.name.local, name))
                .map(|a| &*a.value),
            _ => None,
        }
    }

    pub fn set_attr(&mut self, id: NodeId, name: &str, value: &str) {
        if let NodeData::Element { attrs, .. } = &mut self.nodes[id].data {
            let name = name.to_ascii_lowercase();
            if let Some(a) = attrs.iter_mut().find(|a| *a.name.local == name) {
                // Idempotent writes are free: no dirty, no redraw.
                if *a.value == *value {
                    return;
                }
                a.value = StrTendril::from(value);
            } else {
                attrs.push(Attribute {
                    name: QualName::new(None, ns!(), name.into()),
                    value: StrTendril::from(value),
                });
            }
            self.touch();
        }
    }

    pub fn remove_attr(&mut self, id: NodeId, name: &str) {
        self.touch();
        if let NodeData::Element { attrs, .. } = &mut self.nodes[id].data {
            attrs.retain(|a| !str::eq_ignore_ascii_case(&a.name.local, name));
        }
    }

    pub fn attr_names(&self, id: NodeId) -> Vec<String> {
        match &self.nodes[id].data {
            NodeData::Element { attrs, .. } => {
                attrs.iter().map(|a| a.name.local.to_string()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Is this element hidden — by the `hidden` attribute, or by the
    /// cascaded `display`/`visibility`/`opacity` (inline style, `<style>`
    /// elements, shadow sheets, adoptedStyleSheets, fetched `<link>`
    /// sheets)? Winner per property is the lexicographic max of
    /// (!important, inline, specificity, source order) — inline beats
    /// sheets except under !important, the real rules for a single
    /// author origin. Hidden subtrees don't render. This reads the author
    /// cascade directly (`cascaded`), NOT inheritance: visibility is treated
    /// like display (a hidden subtree stays hidden; no visible-child-of-
    /// hidden-parent). For inherited/UA-defaulted values use `computed_value`;
    /// no @-rules yet.
    /// Whether `id` is the host of an editing region — it carries a truthy
    /// `contenteditable` attribute (`""`/`true`/`plaintext-only`). This is the
    /// editor ROOT (where the attribute sits); descendants merely inherit
    /// editability and are not themselves hosts. TRust treats such a host like a
    /// textarea: one editable widget whose subtree we don't flow.
    pub fn is_contenteditable_host(&self, id: NodeId) -> bool {
        match self.attr(id, "contenteditable") {
            Some(v) => {
                let v = v.trim().to_ascii_lowercase();
                v.is_empty() || v == "true" || v == "plaintext-only"
            }
            None => false,
        }
    }

    pub fn is_hidden(&self, id: NodeId) -> bool {
        if self.attr(id, "hidden").is_some() {
            return true;
        }
        // UA default `dialog:not([open]) { display:none }`: a closed dialog
        // is a modal that hasn't been shown — never render its content (its
        // text otherwise bleeds into the page). An author rule setting the
        // dialog's `display` wins, so only apply when the cascade is silent.
        if self.tag_name(id) == Some("dialog")
            && self.attr(id, "open").is_none()
            && self.cascaded(id, "display").is_none()
        {
            return true;
        }
        if self.cascaded(id, "display").as_deref() == Some("none")
            || matches!(
                self.cascaded(id, "visibility").as_deref(),
                Some("hidden" | "collapse")
            )
        {
            return true;
        }
        // Visually-hidden / "sr-only" accessibility text: the universal idiom
        // for screen-reader-only content is a 1px, clipped, absolutely
        // positioned box (Bootstrap `.visually-hidden`, Tailwind / HTML5BP
        // `.sr-only`, archive.org's `aria-describedby` targets, …). It carries
        // text meant to be invisible to sighted users — render nothing, as a
        // browser does, instead of leaking it into the page (it's also often
        // wider than its sibling content, distorting flex/grid sizing).
        // `position` is checked first so the hot path short-circuits for the
        // overwhelming majority of nodes that aren't absolutely positioned.
        if self.cascaded(id, "position").as_deref() == Some("absolute")
            && self.cascaded(id, "overflow").as_deref() == Some("hidden")
            && self
                .cascaded(id, "width")
                .as_deref()
                .is_some_and(css_len_at_most_1px)
        {
            return true;
        }
        // A box collapsed to ZERO on an axis, with `overflow:hidden`/`clip` on
        // that axis, clips ALL its content to nothing — the standard "keep it
        // in the DOM but show nothing" idiom (a preloaded hero copy, a closed
        // `max-height:0` drawer/accordion, a `height:0` mega-menu). A browser
        // paints none of it; we used to render it (Steam's
        // `.menu_takeover_background{height:0;overflow:hidden}` preload copy of
        // the banner drew a full-width 1-row sliver). EXCEPTION: a `height:0`
        // box whose PADDING reserves the height is the responsive-image
        // "intrinsic ratio" box (`padding-bottom:56.25%` → a 16:9 thumbnail
        // whose absolutely-positioned child fills the padding box, Humble
        // Bundle's tiles) — its content box is zero but the padding box isn't,
        // so it is NOT empty; spare it (`intrinsic_ratio_container_rows` sizes
        // the child off exactly this).
        let clips = |v: Option<String>| {
            v.as_deref().is_some_and(|s| {
                let mut toks = s.split_whitespace().peekable();
                toks.peek().is_some() && toks.all(|t| matches!(t, "hidden" | "clip"))
            })
        };
        let overflow = self.cascaded(id, "overflow");
        let zero = |prop| {
            self.cascaded(id, prop)
                .as_deref()
                .is_some_and(css_len_is_zero)
        };
        let oy = clips(self.cascaded(id, "overflow-y")) || clips(overflow.clone());
        let ox = clips(self.cascaded(id, "overflow-x")) || clips(overflow);
        let h_zero = zero("height") || zero("max-height");
        let w_zero = zero("width") || zero("max-width");
        if (oy && h_zero && !self.has_axis_padding(id, true))
            || (ox && w_zero && !self.has_axis_padding(id, false))
        {
            return true;
        }
        // `opacity:0` is invisible — treat it as hidden, like the W3C/Bootstrap
        // slideshow idiom (`.slides{opacity:0}`, the active slide revealed by
        // an `animation-fill-mode:forwards` fade-in). Gated so a page with no
        // opacity rules pays nothing on this hot path.
        let has_inline_opacity = || {
            self.attr(id, "style")
                .is_some_and(|s| s.contains("opacity"))
        };
        if (self.style_index().has_opacity || has_inline_opacity())
            && self.effective_opacity(id) < OPACITY_HIDDEN
            // ...but a slide in a deck is kept "in the background" (the layout
            // renders one at a time and a control reveals the next), so it
            // must survive serialization rather than being dropped here.
            && !self
                .parent_composed(id)
                .is_some_and(|p| self.is_slideshow_container(p))
        {
            return true;
        }
        false
    }

    /// Whether an element reserves height (`vertical`) or width via positive
    /// padding on that axis — the responsive-image "intrinsic ratio" idiom
    /// (`padding-bottom:56.25%` on a `height:0` box). A non-zero/`auto`/unknown
    /// value counts (we only treat a provably-zero box as empty), so this
    /// returns `true` to SPARE a box from the zero-axis hide above.
    fn has_axis_padding(&self, id: NodeId, vertical: bool) -> bool {
        let props: [&str; 2] = if vertical {
            ["padding-top", "padding-bottom"]
        } else {
            ["padding-left", "padding-right"]
        };
        props.iter().any(|p| {
            self.cascaded(id, p)
                .as_deref()
                .is_some_and(|v| !css_len_is_zero(v))
        })
    }

    /// Whether an element holds a slide deck: ≥2 SLIDE-MATERIAL children, all of
    /// them absolutely positioned (so they stack/overlap rather than sit in
    /// flow) — the structural signature of a JS slideshow's slides. The layout
    /// shows one slide and generates controls to page between them.
    ///
    /// "Slide material" excludes inline overlay CHROME — a `position:absolute`
    /// `<span>` badge/caption/ribbon anchored to a corner (a thumbnail's view
    /// count, a "NEW" tag, a price). Those sit in a corner ALONGSIDE the
    /// content, they don't stack over it, so they aren't slides. Without this a
    /// single image plus two corner badges (the erome.com album card: an `<a>`
    /// wrapping an `<img>` and two count `<span>`s, all three absolute) was
    /// misread as a 3-slide carousel, painting spurious dead prev/next arrows
    /// over every thumbnail. A `<span>`/`<a>` that WRAPS replaced media is still
    /// a slide (an anchor-wrapped image carousel), and a static slide-material
    /// child still disqualifies the deck (an ordinary card with one overlay).
    pub fn is_slideshow_container(&self, id: NodeId) -> bool {
        let mut count = 0usize;
        for c in self.children(id) {
            let Some(tag) = self.tag_name(c) else {
                continue; // text/comment node — only element children count
            };
            if is_inline_overlay_chrome(tag) && !self.contains_replaced_media(c) {
                continue; // a corner badge/caption, not a slide
            }
            if !matches!(
                self.computed_style(c, "position").as_deref(),
                Some("absolute" | "fixed")
            ) {
                return false; // a static slide → an ordinary card, not a deck
            }
            count += 1;
        }
        count >= 2
    }

    /// Whether `id` is, or contains, a replaced media element (`<img>`,
    /// `<picture>`, `<video>`, `<canvas>`, `<iframe>`, `<object>`, `<embed>`) —
    /// so an inline wrapper carrying one (an `<a>`/`<span>` slide around an
    /// image) counts as slide material. `<svg>` is deliberately NOT counted: a
    /// tiny inline icon glyph inside a badge is chrome, not media.
    fn contains_replaced_media(&self, id: NodeId) -> bool {
        std::iter::once(id).chain(self.descendants(id)).any(|n| {
            matches!(
                self.tag_name(n),
                Some("img" | "picture" | "video" | "canvas" | "iframe" | "object" | "embed")
            )
        })
    }

    /// The element's effective opacity for visibility: its cascaded `opacity`
    /// (default 1), or — when an `animation-fill-mode:forwards|both` animation
    /// names a keyframe set whose END opacity is known — that resting value.
    /// So `.slides{opacity:0}` hides, while `.slides.active{animation:fade-in
    /// forwards}` (ending `opacity:1`) shows, with no slideshow-specific code.
    fn effective_opacity(&self, id: NodeId) -> f32 {
        let base = self
            .cascaded(id, "opacity")
            .and_then(|v| v.trim().parse::<f32>().ok())
            .unwrap_or(1.0);
        // Only a near-invisible base is worth the animation lookup; a normally
        // opaque (or merely faded) element shows as-is.
        if base >= OPACITY_HIDDEN {
            return base;
        }
        let (name, fill) = self.animation_of(id);
        if let Some(name) = name
            && matches!(fill.as_deref(), Some("forwards" | "both"))
            && let Some(&end) = self.style_index().keyframes.get(&name)
        {
            return end;
        }
        base
    }

    /// The element's animation name and fill-mode, from the longhands
    /// (`animation-name`/`animation-fill-mode`) or the `animation` shorthand.
    fn animation_of(&self, id: NodeId) -> (Option<String>, Option<String>) {
        let mut name = self.cascaded(id, "animation-name");
        let mut fill = self.cascaded(id, "animation-fill-mode");
        if (name.is_none() || fill.is_none())
            && let Some(shorthand) = self.cascaded(id, "animation")
        {
            for tok in shorthand.split_whitespace() {
                match tok {
                    "forwards" | "backwards" | "both" => {
                        fill.get_or_insert_with(|| tok.to_string());
                    }
                    _ if is_anim_keyword_or_time(tok) => {}
                    _ => {
                        name.get_or_insert_with(|| tok.to_string());
                    }
                }
            }
        }
        (name.filter(|n| n != "none" && !n.is_empty()), fill)
    }

    /// The cascaded `display` value for an element (the mini-cascade
    /// winner), or `None` when no rule sets it. `hidden` attribute counts
    /// as `display:none`. Drives block/inline flow in the layout pass and
    /// is baked into the serialized HTML so the re-parsed layout arena
    /// sees the same computed display the engine did.
    pub fn computed_display(&self, id: NodeId) -> Option<String> {
        if self.attr(id, "hidden").is_some() {
            return Some("none".to_string());
        }
        self.cascaded(id, "display")
    }

    /// The EFFECTIVE `display` — the author's cascaded `display` if set, else
    /// the tag's UA-stylesheet default (so an un-styled `<table>` reports
    /// `"table"`, a `<tr>` `"table-row"`, a `<td>` `"table-cell"`). Unlike
    /// `computed_display` (cascade-only, `None` when no rule sets it) this is
    /// never `None` for a known element, so the layout can route the CSS table
    /// formatting context off a bare HTML `<table>` with no CSS at all.
    pub fn effective_display(&self, id: NodeId) -> Option<String> {
        if let Some(d) = self.computed_display(id) {
            return Some(d);
        }
        Some(ua_display(self.tag_name(id)?).to_string())
    }

    /// The cascaded value of any tracked property (the layout reads
    /// margin/padding/text-align through this), or `None` when unset.
    /// Author cascade only (no UA defaults, no inheritance) — the
    /// non-inherited box properties the layout reads directly, and the
    /// value the serializer bakes.
    pub fn computed_style(&self, id: NodeId, prop: &str) -> Option<String> {
        self.cascaded(id, prop)
    }

    /// The computed value of a property — the single inheritance authority.
    /// For an inherited property (per the registry) an element that doesn't
    /// set it resolves to the parent's computed value; otherwise this is the
    /// specified value (author cascade, else the UA default). Memoized per
    /// epoch because the layout reads it per element. getComputedStyle and
    /// the layout's inherited-text reads both go through here, so a property
    /// inherits everywhere by being marked `inherited` once.
    pub fn computed_value(&self, id: NodeId, name: &str) -> Option<String> {
        let Some(idx) = PROPS.iter().position(|p| p.name == name) else {
            // Untracked: no UA default, no inheritance — author cascade.
            return self.cascaded(id, name);
        };
        if !PROPS[idx].inherited {
            return self.specified(id, name);
        }
        if let Some(hit) = self.computed_cache_get(id, idx) {
            return hit;
        }
        let v = self.specified(id, name).or_else(|| {
            self.nodes[id]
                .parent
                .and_then(|p| self.computed_value(p, name))
        });
        self.computed_cache_put(id, idx, v.clone());
        v
    }

    /// `computed_value` with `var()` references substituted — what
    /// getComputedStyle exposes to JS. CSS variables resolve in computed
    /// style (`Supports.variable` sets `margin-right:var(--x)` and reads
    /// `marginRight` back as the substituted value). A no-op when the value
    /// has no `var(`.
    pub fn computed_value_resolved(&self, id: NodeId, name: &str) -> Option<String> {
        self.computed_value(id, name)
            .map(|v| self.resolve_vars(id, &v))
    }

    fn computed_cache_get(&self, id: NodeId, idx: usize) -> Option<Option<String>> {
        let cache = self.computed_cache.borrow();
        (cache.0 == self.epoch)
            .then(|| cache.1.get(&(id, idx)).cloned())
            .flatten()
    }

    fn computed_cache_put(&self, id: NodeId, idx: usize, v: Option<String>) {
        let mut cache = self.computed_cache.borrow_mut();
        if cache.0 != self.epoch {
            cache.0 = self.epoch;
            cache.1.clear();
        }
        cache.1.insert((id, idx), v);
    }

    /// The specified value: the author cascade, or the UA default for the
    /// element's tag. (Before inheritance — `computed_value` adds that.)
    fn specified(&self, id: NodeId, name: &str) -> Option<String> {
        self.cascaded(id, name)
            .or_else(|| self.ua_default(id, name))
    }

    /// The user-agent default stylesheet, for the inherited properties the
    /// layout used to apply as hardcoded tag behavior: `<b>/<strong>` bold,
    /// `<i>/<em>` italic, `<pre>` pre white-space, and the list marker style
    /// (`<ul>` disc/circle/square by nesting depth, `<ol>` decimal or its
    /// `type` attribute). Non-inherited tag defaults stay where they belong:
    /// block/inline display (the layout's tag tables), `<a>` linking, heading
    /// sizing, and `<u>/<s>` decoration (`text_decoration`, which accumulates
    /// rather than inherits).
    fn ua_default(&self, id: NodeId, name: &str) -> Option<String> {
        let tag = self.tag_name(id)?;
        let v = match name {
            "font-weight" if matches!(tag, "b" | "strong") => "bold",
            "font-style" if matches!(tag, "i" | "em") => "italic",
            "white-space" if tag == "pre" => "pre",
            "list-style-type" if tag == "ul" => self.ul_marker_default(id),
            "list-style-type" if tag == "ol" => match self.attr(id, "type") {
                Some("a") => "lower-alpha",
                Some("A") => "upper-alpha",
                Some("i") => "lower-roman",
                Some("I") => "upper-roman",
                _ => "decimal",
            },
            "display" => ua_display(tag),
            _ => return None,
        };
        Some(v.to_string())
    }

    /// The default bullet for a `<ul>` by nesting depth, matching browsers:
    /// disc at the top level, circle one deep, square thereafter. An inner
    /// list inherits this through `computed_value`, so authors can still
    /// override it anywhere.
    fn ul_marker_default(&self, id: NodeId) -> &'static str {
        let mut depth = 0u32;
        let mut cur = Some(id);
        while let Some(c) = cur {
            if self.tag_name(c) == Some("ul") {
                depth += 1;
            }
            cur = self.nodes[c].parent;
        }
        match depth {
            0 | 1 => "disc",
            2 => "circle",
            _ => "square",
        }
    }

    /// The accumulated `(underline, line-through)` for an element's text.
    /// `text-decoration` is not inherited but PROPAGATED — the lines paint
    /// across descendant boxes and accumulate — so this walks ancestors→self:
    /// each `<u>/<ins>` adds underline, each `<s>/<strike>/<del>` adds
    /// line-through, an author `text-decoration(-line)` adds its named lines,
    /// and `none` clears both from that point down. Replaces the layout's
    /// emphasis threading for the two decoration flags.
    pub fn text_decoration(&self, id: NodeId) -> (bool, bool) {
        let mut chain = vec![id];
        while let Some(&c) = chain.last() {
            match self.nodes[c].parent {
                Some(p) => chain.push(p),
                None => break,
            }
        }
        let (mut underline, mut strike) = (false, false);
        for &e in chain.iter().rev() {
            match self.tag_name(e) {
                Some("u" | "ins") => underline = true,
                Some("s" | "strike" | "del") => strike = true,
                _ => {}
            }
            if let Some(v) = self
                .cascaded(e, "text-decoration-line")
                .or_else(|| self.cascaded(e, "text-decoration"))
            {
                if v.split_whitespace().any(|t| t == "none") {
                    underline = false;
                    strike = false;
                } else {
                    underline |= v.contains("underline");
                    strike |= v.contains("line-through");
                }
            }
        }
        (underline, strike)
    }

    /// The mini-cascade winner for one property (`display` or
    /// `visibility` — the two the style index tracks). Inline styles beat
    /// tree rules by specificity/order, `!important` and source order
    /// resolved by `CascadeKey`.
    fn cascaded(&self, id: NodeId, prop: &str) -> Option<String> {
        let mut winner: Option<(CascadeKey, String)> = None;
        if let Some(style) = self.attr(id, "style") {
            for decl in style.split(';') {
                let Some((k, v, important)) = parse_decl(decl) else {
                    continue;
                };
                for (pk, pv) in expand_box_shorthand(&k, &v) {
                    if pk == prop {
                        consider(&mut winner, (important, true, (0, 0, 0), usize::MAX), &pv);
                    }
                }
            }
        }
        let index = self.style_index();
        if let Some(rules) = index.scopes.get(&self.tree_scope(id)) {
            for &ri in self.matched_rules(id).iter() {
                let r = &rules[ri as usize];
                // `div::before{…}` rules target a generated box, not the
                // element — skip them in the element-property cascade.
                if rule_pseudo(r).is_some() {
                    continue;
                }
                for (pk, (imp, v)) in &r.decls {
                    if pk == prop {
                        consider(&mut winner, (*imp, false, r.specificity, r.order), v);
                    }
                }
            }
        }
        winner.map(|(_, v)| v)
    }

    /// An element's computed value for a custom property (`--foo`): its own
    /// cascaded declaration, else inherited from the composed parent (custom
    /// properties inherit). `None` if undefined up the whole chain.
    fn custom_prop(&self, id: NodeId, name: &str) -> Option<String> {
        if let Some(v) = self.cascaded(id, name) {
            return Some(v);
        }
        self.parent_composed(id)
            .and_then(|p| self.custom_prop(p, name))
    }

    /// Substitute every `var(--name, fallback)` in a CSS value with the
    /// element's computed custom-property value (cascaded + inherited), falling
    /// back to the (recursively resolved) fallback, then to empty when neither
    /// exists. Balanced-paren aware so `var()` inside `calc()` and nested
    /// `var()` both resolve. A no-op when the value has no `var(`.
    fn resolve_vars(&self, id: NodeId, value: &str) -> String {
        if !value.contains("var(") {
            return value.to_owned();
        }
        let mut out = String::new();
        let mut rest = value;
        let mut guard = 0;
        while let Some(pos) = rest.find("var(") {
            guard += 1;
            if guard > 64 {
                out.push_str(rest);
                return out;
            }
            out.push_str(&rest[..pos]);
            let after = &rest[pos + 4..];
            // Find the `)` that closes this `var(`.
            let mut depth = 1usize;
            let mut end = None;
            for (i, c) in after.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(end) = end else {
                out.push_str(&rest[pos..]); // unbalanced: leave as-is
                return out;
            };
            let inner = &after[..end];
            let (name, fallback) = match inner.split_once(',') {
                Some((n, f)) => (n.trim(), Some(f.trim())),
                None => (inner.trim(), None),
            };
            let resolved = self
                .custom_prop(id, name)
                .or_else(|| fallback.map(str::to_owned))
                .map(|v| self.resolve_vars(id, &v))
                .unwrap_or_default();
            out.push_str(&resolved);
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// The resolved `content` text for an element's `::before`/`::after`
    /// box, or `None` when no rule sets it (or it resolves to `none`/an
    /// unsupported value like `counter()`). Reads only pseudo-element rules
    /// in the element's tree scope (inline styles can't target a pseudo).
    pub fn pseudo_content(&self, id: NodeId, which: PseudoEl) -> Option<String> {
        let index = self.style_index();
        let rules = index.scopes.get(&self.tree_scope(id))?;
        let mut winner: Option<(CascadeKey, String)> = None;
        for &ri in self.matched_rules(id).iter() {
            let r = &rules[ri as usize];
            if rule_pseudo(r) != Some(which) {
                continue;
            }
            for (pk, (imp, v)) in &r.decls {
                if pk == "content" {
                    consider(&mut winner, (*imp, false, r.specificity, r.order), v);
                }
            }
        }
        let raw = winner.map(|(_, v)| v)?;
        // A hidden pseudo-element generates no rendered content. The common
        // width-reservation idiom `[data-content]::before { content:
        // attr(data-content); visibility:hidden }` (Primer's UnderlineNav tabs,
        // many tab/button components) paints a hidden bold copy of the label
        // purely to reserve its selected width — rendering it doubles the label
        // ("CodeCode IssuesIssues"). Honor the pseudo's own visibility/display.
        if matches!(
            self.pseudo_style(id, which, "visibility").as_deref(),
            Some("hidden" | "collapse")
        ) || self.pseudo_style(id, which, "display").as_deref() == Some("none")
        {
            return None;
        }
        self.parse_content_value(id, &raw)
    }

    /// The cascade-winning value of `prop` on `id`'s `::before`/`::after`
    /// pseudo-element, or `None` if no matching rule sets it.
    pub fn pseudo_style(&self, id: NodeId, which: PseudoEl, prop: &str) -> Option<String> {
        let index = self.style_index();
        let rules = index.scopes.get(&self.tree_scope(id))?;
        let mut winner: Option<(CascadeKey, String)> = None;
        for &ri in self.matched_rules(id).iter() {
            let r = &rules[ri as usize];
            if rule_pseudo(r) != Some(which) {
                continue;
            }
            for (pk, (imp, v)) in &r.decls {
                if pk == prop {
                    consider(&mut winner, (*imp, false, r.specificity, r.order), v);
                }
            }
        }
        winner.map(|(_, v)| v)
    }

    /// Whether `id` carries the clearfix idiom — a `::before`/`::after`
    /// pseudo-element that `clear`s floats (`.clearfix`, Bootstrap's `.row`,
    /// `.group`, …). Such a block CONTAINS its descendant floats (the universal
    /// pre-flexbox containment pattern: `::after{content:"";clear:both}`), so
    /// the layout treats it as a block formatting context. Without it a float
    /// grid leaks past its row and the next section paints on top of it.
    pub fn has_clearing_pseudo(&self, id: NodeId) -> bool {
        // The baked marker (set by the serializer when the real CSS was still
        // in scope) — the layout re-parses without the `::after{clear}` rule.
        if self.attr(id, "data-trust-clearfix").is_some() {
            return true;
        }
        [PseudoEl::Before, PseudoEl::After].into_iter().any(|p| {
            self.pseudo_style(id, p, "clear")
                .is_some_and(|v| matches!(v.trim(), "both" | "left" | "right"))
        })
    }

    /// Resolve a `content` value to display text: a quoted string (with CSS
    /// `\HEX`/`\c` escapes), `attr(name)` → the element's attribute, or
    /// `none`/`normal`/unsupported (`counter()`, `url()`) → `None`.
    fn parse_content_value(&self, id: NodeId, raw: &str) -> Option<String> {
        let v = raw.trim();
        if v.is_empty() || v == "none" || v == "normal" {
            return None;
        }
        if let Some(s) = unquote_css(v) {
            return (!s.is_empty()).then_some(s);
        }
        if let Some(inner) = v.strip_prefix("attr(").and_then(|r| r.strip_suffix(')')) {
            return self.attr(id, inner.trim()).map(str::to_owned);
        }
        None
    }

    /// The root of a node's tree: DOCUMENT for the light DOM, the
    /// shadow fragment for shadow content. An element consults only its
    /// own scope's sheets (selector matching can't cross the boundary
    /// either — ancestor walks stop at fragment roots).
    fn tree_scope(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        while let Some(p) = self.nodes[cur].parent {
            cur = p;
        }
        cur
    }

    /// The visibility cascade for the current epoch, built on first use
    /// after any mutation, shared until the next one.
    fn style_index(&self) -> std::rc::Rc<StyleIndex> {
        let mut cache = self.style_cache.borrow_mut();
        if let Some((epoch, idx)) = cache.as_ref()
            && *epoch == self.epoch
        {
            return idx.clone();
        }
        let idx = std::rc::Rc::new(self.build_style_index());
        *cache = Some((self.epoch, idx.clone()));
        idx
    }

    fn build_style_index(&self) -> StyleIndex {
        let mut index = StyleIndex::default();
        let mut order = 0;
        for id in self.composed_descendants(DOCUMENT) {
            let css: Cow<str> = match self.tag_name(id) {
                Some("style") => Cow::Owned(self.text_content(id)),
                Some("link") => match self.external_sheets.get(&id) {
                    Some(css) => Cow::Borrowed(css.as_str()),
                    None => continue,
                },
                _ => continue,
            };
            let scope = self.tree_scope(id);
            parse_sheet(
                &css,
                &mut order,
                index.scopes.entry(scope).or_default(),
                &mut index.keyframes,
                self.viewport_px,
            );
        }
        // Adopted sheets cascade after their scope's tree sheets (their
        // order values are necessarily higher); cross-scope order is
        // moot — an element only reads its own scope. Sort for
        // determinism across HashMap iteration.
        let mut adopted: Vec<_> = self.adopted_styles.iter().collect();
        adopted.sort_by_key(|(scope, _)| **scope);
        for (scope, css) in adopted {
            parse_sheet(
                css,
                &mut order,
                index.scopes.entry(*scope).or_default(),
                &mut index.keyframes,
                self.viewport_px,
            );
        }
        index.has_opacity = index
            .scopes
            .values()
            .flatten()
            .any(|r| r.decls.iter().any(|(k, _)| k == "opacity"));
        // Build the rightmost-key buckets so `matched_rules` tests only
        // candidate rules per element instead of the whole scope.
        index.buckets = index
            .scopes
            .iter()
            .map(|(scope, rules)| (*scope, RuleBuckets::build(rules)))
            .collect();
        index
    }

    /// The author rules (by index into the element's tree-scope rule vec) whose
    /// selectors match `id`, in the cascade context (no `:scope` root).
    /// Memoized per epoch: matching is the cascade's hot cost and the layout /
    /// serializer read 30+ properties per element, so doing it once and reusing
    /// the list is what keeps a CSS-heavy page (GitHub: ~8k rules) from going
    /// O(elements × rules × props). Candidate rules come from the rightmost-key
    /// buckets; only those are full-matched.
    fn matched_rules(&self, id: NodeId) -> std::rc::Rc<Vec<u32>> {
        {
            let cache = self.matched_cache.borrow();
            if cache.0 == self.epoch
                && let Some(hit) = cache.1.get(&id)
            {
                return hit.clone();
            }
        }
        let index = self.style_index();
        let scope = self.tree_scope(id);
        let matched = match (index.scopes.get(&scope), index.buckets.get(&scope)) {
            (Some(rules), Some(b)) => {
                let mut out: Vec<u32> = Vec::new();
                let test = |dom: &Dom, ri: u32, out: &mut Vec<u32>| {
                    if dom.matches_complex(id, &rules[ri as usize].selector.0, None) {
                        out.push(ri);
                    }
                };
                for &ri in &b.universal {
                    test(self, ri, &mut out);
                }
                if let Some(idv) = self.attr(id, "id")
                    && let Some(v) = b.by_id.get(idv)
                {
                    for &ri in v {
                        test(self, ri, &mut out);
                    }
                }
                if let Some(classes) = self.attr(id, "class") {
                    for cls in classes.split_ascii_whitespace() {
                        if let Some(v) = b.by_class.get(cls) {
                            for &ri in v {
                                test(self, ri, &mut out);
                            }
                        }
                    }
                }
                if let Some(tag) = self.tag_name(id)
                    && let Some(v) = b.by_tag.get(tag)
                {
                    for &ri in v {
                        test(self, ri, &mut out);
                    }
                }
                // Cascade order is carried by each rule's `order` (the cascade
                // tiebreaker), so the matched list need not be ordered — but
                // sort for deterministic iteration, and dedup so a repeated
                // class token (`class="box box"`) can't list a rule twice.
                out.sort_unstable();
                out.dedup();
                std::rc::Rc::new(out)
            }
            _ => std::rc::Rc::new(Vec::new()),
        };
        let mut cache = self.matched_cache.borrow_mut();
        if cache.0 != self.epoch {
            cache.0 = self.epoch;
            cache.1.clear();
        }
        cache.1.insert(id, matched.clone());
        matched
    }

    /// adoptedStyleSheets text for a scope (DOCUMENT or a shadow root),
    /// pushed by the prelude on adoption and on replace/replaceSync.
    /// Idempotent pushes are free — no dirty, no rebuild.
    pub fn set_adopted_styles(&mut self, scope: NodeId, css: &str) {
        if self.adopted_styles.get(&scope).map(String::as_str) == Some(css)
            || (css.trim().is_empty() && !self.adopted_styles.contains_key(&scope))
        {
            return;
        }
        self.adopted_styles.insert(scope, css.to_string());
        self.touch();
    }

    fn is_stylesheet_link(&self, id: NodeId) -> bool {
        if self.tag_name(id) != Some("link") {
            return false;
        }
        let Some(rel) = self.attr(id, "rel") else {
            return false;
        };
        let mut words = rel.split_ascii_whitespace();
        // An applied stylesheet has `rel="stylesheet"`. `rel="alternate
        // stylesheet"` is an ALTERNATE — not applied unless the user selects it
        // (HTML §4.6.7) — and a `disabled` sheet is off; neither contributes to
        // the cascade, so don't fetch or attach them.
        let is_sheet = words.clone().any(|w| w.eq_ignore_ascii_case("stylesheet"));
        let is_alternate = words.any(|w| w.eq_ignore_ascii_case("alternate"));
        is_sheet && !is_alternate && self.attr(id, "disabled").is_none()
    }

    /// Raw hrefs of external stylesheets, document order, so the fetch
    /// pipeline can resolve and download them before scripts run.
    pub fn stylesheet_links(&self) -> Vec<String> {
        self.descendants(DOCUMENT)
            .into_iter()
            .filter(|&id| self.is_stylesheet_link(id))
            .filter_map(|id| self.attr(id, "href").map(str::to_string))
            .collect()
    }

    /// Attach fetched `<link rel=stylesheet>` bodies (keyed by the raw
    /// href attribute) to their link elements; the cascade reads them
    /// scope-aware like any `<style>`.
    pub fn attach_external_sheets(&mut self, sheets: &[(String, String)]) {
        for (href, css) in sheets {
            let hit = self.descendants(DOCUMENT).into_iter().find(|&id| {
                !self.external_sheets.contains_key(&id)
                    && self.is_stylesheet_link(id)
                    && self.attr(id, "href") == Some(href.as_str())
            });
            if let Some(id) = hit {
                self.external_sheets.insert(id, css.clone());
                self.touch();
            }
        }
    }

    /// Attach a shadow root (a fragment) to a host element; rendering
    /// flattens it in place of the host's light children, with `<slot>`
    /// projection. Idempotent per host, like the real API isn't — pages
    /// that double-attach get the same root back rather than a throw.
    pub fn attach_shadow(&mut self, host: NodeId) -> NodeId {
        if let Some(&root) = self.shadow_roots.get(&host) {
            return root;
        }
        let root = self.create_fragment();
        self.shadow_roots.insert(host, root);
        self.shadow_hosts.insert(root, host);
        self.touch();
        root
    }

    /// Parent in the COMPOSED tree: shadow roots hand off to their host
    /// (event paths and ancestor checks cross shadow boundaries).
    pub fn parent_composed(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id]
            .parent
            .or_else(|| self.shadow_hosts.get(&id).copied())
    }

    /// Document-order walk of the COMPOSED tree: light children plus
    /// every shadow tree (interactive content hides in there).
    pub fn composed_descendants(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self.children(root);
        stack.reverse();
        while let Some(id) = stack.pop() {
            out.push(id);
            let mut kids = self.children(id);
            if let Some(shadow) = self.shadow_root(id) {
                kids.extend(self.children(shadow));
            }
            kids.reverse();
            stack.extend(kids);
        }
        out
    }

    pub fn shadow_root(&self, host: NodeId) -> Option<NodeId> {
        self.shadow_roots.get(&host).copied()
    }

    /// Composed-tree element ids whose tag is `name`, in document (pre-)order,
    /// piercing shadow roots — the catch-up upgrade set `customElements.define`
    /// needs. Done in Rust as a pointer walk (no per-node child Vec, no JS
    /// wrapper) because the prelude formerly walked the whole tree per `define`
    /// in JS — a `__dom_children`/`wrap` storm that dominated big-page boot
    /// (GitHub: ~O(defines × 16.8k nodes)).
    pub fn elements_by_tag_composed(&self, root: NodeId, name: &str) -> Vec<NodeId> {
        let mut out = Vec::new();
        if !self.is_valid(root) {
            return out;
        }
        let mut stack: Vec<NodeId> = vec![root];
        while let Some(id) = stack.pop() {
            if self.tag_name(id) == Some(name) {
                out.push(id);
            }
            self.push_composed_children(id, &mut stack);
        }
        out
    }

    /// Composed-tree element ids (root included, shadow-piercing, document
    /// order) whose tag is a custom-element name — i.e. contains a hyphen (the
    /// HTML naming rule for autonomous custom elements). Backs `ceScan`'s
    /// insertion-time upgrade/connect pass: the prelude can then touch only the
    /// custom-element candidates instead of wrapping every node in the inserted
    /// subtree.
    pub fn custom_elements_composed(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        if !self.is_valid(root) {
            return out;
        }
        let mut stack: Vec<NodeId> = vec![root];
        while let Some(id) = stack.pop() {
            if self.tag_name(id).is_some_and(|t| t.contains('-')) {
                out.push(id);
            }
            self.push_composed_children(id, &mut stack);
        }
        out
    }

    /// Push `id`'s composed children (light children, then shadow-root
    /// children) onto `stack` in reverse, so a LIFO pop yields them in
    /// document order (pre-order: a parent is processed before its children).
    fn push_composed_children(&self, id: NodeId, stack: &mut Vec<NodeId>) {
        let start = stack.len();
        let mut c = self.nodes[id].first_child;
        while let Some(cid) = c {
            stack.push(cid);
            c = self.nodes[cid].next_sibling;
        }
        if let Some(shadow) = self.shadow_root(id) {
            let mut c = self.nodes[shadow].first_child;
            while let Some(cid) = c {
                stack.push(cid);
                c = self.nodes[cid].next_sibling;
            }
        }
        stack[start..].reverse();
    }

    /// Where innerHTML-ish operations land: a template's content
    /// fragment, everyone else themselves.
    pub fn content_target(&self, id: NodeId) -> NodeId {
        match &self.nodes[id].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => *c,
            _ => id,
        }
    }

    /// The `<body>` of an iframe's nested document, when the JS prelude has
    /// realized one (a same-origin scripted/`srcdoc` frame builds an
    /// `<html><head><body>` subtree under the `<iframe>`; see
    /// `FrameDocument`). The serializers flow this body inline so the frame's
    /// content lays out as a normal block instead of the RAWTEXT the HTML
    /// parser otherwise makes of `<iframe>` children. `None` for an empty or
    /// cross-origin (never-loaded) frame.
    pub fn frame_body(&self, id: NodeId) -> Option<NodeId> {
        let html = self
            .children(id)
            .into_iter()
            .find(|&c| self.tag_name(c) == Some("html"))?;
        self.children(html)
            .into_iter()
            .find(|&c| self.tag_name(c) == Some("body"))
    }

    /// The host's light children assigned to a slot (by name, or the
    /// default slot). Text nodes always belong to the default slot.
    fn slot_assigned(&self, host: NodeId, slot_name: Option<&str>) -> Vec<NodeId> {
        self.children(host)
            .into_iter()
            .filter(|&c| match (self.attr(c, "slot"), slot_name) {
                (Some(a), Some(n)) => a == n,
                (None, None) => true,
                _ => false,
            })
            .collect()
    }

    /// Concatenated descendant text (DOM textContent).
    /// A comment's data (Lit's binding markers live there).
    pub fn comment_text(&self, id: NodeId) -> Option<&str> {
        match &self.nodes[id].data {
            NodeData::Comment(t) => Some(t),
            _ => None,
        }
    }

    pub fn set_comment_text(&mut self, id: NodeId, text: &str) {
        if let NodeData::Comment(t) = &mut self.nodes[id].data
            && t != text
        {
            *t = text.to_string();
            self.touch();
        }
    }

    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        if let NodeData::Text(t) = &self.nodes[id].data {
            return t.clone();
        }
        for d in self.descendants(id) {
            if let NodeData::Text(t) = &self.nodes[d].data {
                out.push_str(t);
            }
        }
        out
    }

    /// The terminal glyph for an icon element/subtree — the dominant web icon
    /// idiom is a Font-Awesome-style `<svg class="...fa-NAME"><use href=
    /// "#...fa-NAME"></svg>` (also `icon-NAME`/`bi-NAME`). We don't rasterize
    /// SVG (an icon-sized raster is an unreadable smear in a terminal); instead
    /// we recognize the icon by NAME and render its Unicode glyph. Scans `id`
    /// and its descendants for the first recognizable name. `None` when nothing
    /// matches (a non-icon `<svg>` — a D3 chart, a logo — stays unrendered).
    pub fn icon_glyph(&self, id: NodeId) -> Option<&'static str> {
        for n in std::iter::once(id).chain(self.descendants(id)) {
            for attr in ["class", "href", "xlink:href"] {
                if let Some(v) = self.attr(n, attr) {
                    for tok in v.split(|c: char| c.is_whitespace()) {
                        if let Some(name) = icon_token_name(tok)
                            && let Some(g) = icon_glyph_for(name)
                        {
                            return Some(g);
                        }
                    }
                }
            }
        }
        None
    }

    pub fn set_text(&mut self, id: NodeId, text: &str) {
        match &mut self.nodes[id].data {
            // Idempotent writes are free: no dirty, no redraw.
            NodeData::Text(t) if *t == text => (),
            NodeData::Text(t) => {
                *t = text.to_string();
                self.touch();
            }
            _ => {
                // A single-text-child rewrite to the same value is the
                // hot no-op (counters, clocks): skip it cheaply.
                let kids = self.children(id);
                if let [only] = kids[..]
                    && let NodeData::Text(t) = &self.nodes[only].data
                    && *t == text
                {
                    return;
                }
                self.touch();
                for c in kids {
                    self.detach(c);
                }
                let t = self.create_text(text);
                self.append(id, t);
            }
        }
    }

    /// Deep-copy a subtree (or a single node when `deep` is false).
    /// Template content propagates per the HTML cloning steps: a cloned
    /// template always owns a fresh content fragment, populated when
    /// deep (webcomponents-loader probes exactly this).
    pub fn clone_subtree(&mut self, id: NodeId, deep: bool) -> NodeId {
        let data = match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => NodeData::Fragment,
            NodeData::Doctype => NodeData::Doctype,
            NodeData::Comment(t) => NodeData::Comment(t.clone()),
            NodeData::Text(t) => NodeData::Text(t.clone()),
            NodeData::Element { name, attrs, .. } => NodeData::Element {
                name: name.clone(),
                attrs: attrs.clone(),
                template_contents: None,
            },
        };
        let src_content = match &self.nodes[id].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => Some(*c),
            _ => None,
        };
        let copy = self.new_node(data);
        if let Some(sc) = src_content {
            let frag = self.new_node(NodeData::Fragment);
            if let NodeData::Element {
                template_contents, ..
            } = &mut self.nodes[copy].data
            {
                *template_contents = Some(frag);
            }
            if deep {
                for c in self.children(sc) {
                    let cc = self.clone_subtree(c, true);
                    self.append(frag, cc);
                }
            }
        }
        if deep {
            for c in self.children(id) {
                let cc = self.clone_subtree(c, true);
                self.append(copy, cc);
            }
        }
        copy
    }

    /// Parse an HTML snippet in the context of `parent`'s tag and return
    /// the new nodes (already transplanted into this arena, detached).
    pub fn parse_fragment_into(&mut self, context_tag: &str, html: &str) -> Vec<NodeId> {
        let sink = Sink {
            dom: RefCell::new(Dom::new()),
        };
        let context = QualName::new(None, ns!(html), context_tag.to_ascii_lowercase().into());
        let frag: Dom =
            html5ever::parse_fragment(sink, ParseOpts::default(), context, Vec::new(), false)
                .one(StrTendril::from(html));
        // The fragment's children land under <html> under the document.
        let html_el = frag
            .children(DOCUMENT)
            .into_iter()
            .find(|&c| frag.tag_name(c) == Some("html"))
            .unwrap_or(DOCUMENT);
        frag.children(html_el)
            .into_iter()
            .map(|c| self.transplant(&frag, c))
            .collect()
    }

    /// Deep-copy a subtree from another arena into this one. Template
    /// content rides along (html5ever parks template children there).
    fn transplant(&mut self, other: &Dom, id: NodeId) -> NodeId {
        let data = match &other.nodes[id].data {
            NodeData::Document | NodeData::Fragment => NodeData::Fragment,
            NodeData::Doctype => NodeData::Doctype,
            NodeData::Comment(t) => NodeData::Comment(t.clone()),
            NodeData::Text(t) => NodeData::Text(t.clone()),
            NodeData::Element { name, attrs, .. } => NodeData::Element {
                name: name.clone(),
                attrs: attrs.clone(),
                template_contents: None,
            },
        };
        let src_content = match &other.nodes[id].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => Some(*c),
            _ => None,
        };
        let copy = self.new_node(data);
        if let Some(sc) = src_content {
            let frag = self.new_node(NodeData::Fragment);
            if let NodeData::Element {
                template_contents, ..
            } = &mut self.nodes[copy].data
            {
                *template_contents = Some(frag);
            }
            for c in other.children(sc) {
                let cc = self.transplant(other, c);
                self.append(frag, cc);
            }
        }
        for c in other.children(id) {
            let cc = self.transplant(other, c);
            self.append(copy, cc);
        }
        copy
    }

    /// First element (document order) whose id attribute matches.
    pub fn get_by_id(&self, target: &str) -> Option<NodeId> {
        self.descendants(DOCUMENT)
            .into_iter()
            .find(|&d| self.attr(d, "id") == Some(target))
    }

    /// Serialize a subtree to HTML (for the app to re-parse and lay
    /// out). `<script>` has done its job by now and `<noscript>` means
    /// "JS didn't run" — when this serializer is called, it did — so both
    /// are dropped, as are doctypes and `<template>` (inert by
    /// definition). The cascaded `display` is baked onto each element so
    /// the re-parsed layout arena flows it the way the engine computed.
    pub fn serialize(&self, root: NodeId) -> String {
        let mut out = String::new();
        self.serialize_node(root, None, &mut out);
        out
    }

    pub fn inner_html(&self, id: NodeId) -> String {
        let mut out = String::new();
        for c in self.children(self.content_target(id)) {
            self.serialize_node(c, None, &mut out);
        }
        out
    }

    fn serialize_node(&self, id: NodeId, host: Option<NodeId>, out: &mut String) {
        match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => {
                for c in self.children(id) {
                    self.serialize_node(c, host, out);
                }
            }
            NodeData::Doctype => {}
            // Comments survive round-trips (Lit's markers) and the
            // layout pass ignores them.
            NodeData::Comment(t) => {
                out.push_str("<!--");
                out.push_str(&t.replace("--", "- -"));
                out.push_str("-->");
            }
            NodeData::Text(t) => out.push_str(&escape_text(t)),
            NodeData::Element { name, attrs, .. } => {
                let tag: &str = &name.local;
                if matches!(tag, "script" | "noscript" | "template" | "style") || self.is_hidden(id)
                {
                    return;
                }
                // An iframe/frame with realized same-origin content: flow the
                // nested document's body inline as a block, so the re-parse
                // lays it out normally instead of as the RAWTEXT the HTML
                // parser makes of <iframe> children. Empty/cross-origin frames
                // emit nothing (unchanged).
                if matches!(tag, "iframe" | "frame") {
                    if let Some(body) = self.frame_body(id) {
                        let kids = self.children(body);
                        if !kids.is_empty() {
                            out.push_str("<div data-trust-frame=\"\">");
                            for c in kids {
                                self.serialize_node(c, host, out);
                            }
                            out.push_str("</div>");
                        }
                    }
                    return;
                }
                // <slot> inside a shadow tree: project the host's light
                // children (or the slot's own fallback content).
                if tag == "slot"
                    && let Some(h) = host
                {
                    let assigned = self.slot_assigned(h, self.attr(id, "name"));
                    if assigned.is_empty() {
                        for c in self.children(id) {
                            self.serialize_node(c, host, out);
                        }
                    } else {
                        for c in assigned {
                            self.serialize_node(c, None, out);
                        }
                    }
                    return;
                }
                out.push('<');
                out.push_str(tag);
                self.write_attrs(id, attrs, &mut |_, _| None, out);
                out.push('>');
                if VOID_ELEMENTS.contains(&tag) {
                    return;
                }
                // A shadow root renders IN PLACE of the light children
                // (flattened — text extraction wants content, not
                // composition fidelity).
                if let Some(root) = self.shadow_root(id) {
                    for c in self.children(root) {
                        self.serialize_node(c, Some(id), out);
                    }
                } else {
                    for c in self.children(id) {
                        self.serialize_node(c, host, out);
                    }
                }
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }

    /// Serialize for a LIVING page: like `serialize`, but elements in
    /// `clickable` become followable — non-anchors are wrapped in
    /// `<a href="x-trust-js:<id>:">` markers (the form-marker trick),
    /// and live anchors get their href rewritten to
    /// `x-trust-js:<id>:<original-href>` so clicks route through the
    /// page actor (which navigates only if not defaultPrevented).
    pub fn serialize_live(
        &self,
        root: NodeId,
        clickable: &std::collections::HashSet<NodeId>,
    ) -> String {
        let mut out = String::new();
        self.serialize_live_node(root, None, clickable, false, &mut out);
        out
    }

    fn serialize_live_node(
        &self,
        id: NodeId,
        host: Option<NodeId>,
        clickable: &std::collections::HashSet<NodeId>,
        in_anchor: bool,
        out: &mut String,
    ) {
        let NodeData::Element { name, attrs, .. } = &self.nodes[id].data else {
            return self.serialize_node_with(
                id,
                &mut |c, o| self.serialize_live_node(c, host, clickable, in_anchor, o),
                out,
            );
        };
        let tag: &str = &name.local;
        if matches!(tag, "script" | "noscript" | "template" | "style") || self.is_hidden(id) {
            return;
        }
        // iframe/frame nested-document content flows inline as a block (see the
        // static serializer + `frame_body`): scripted/`srcdoc` frame content
        // renders, RAWTEXT re-parse is avoided, empty/cross-origin frames emit
        // nothing.
        if matches!(tag, "iframe" | "frame") {
            if let Some(body) = self.frame_body(id) {
                let kids = self.children(body);
                if !kids.is_empty() {
                    out.push_str("<div data-trust-frame=\"\">");
                    for c in kids {
                        self.serialize_live_node(c, host, clickable, in_anchor, out);
                    }
                    out.push_str("</div>");
                }
            }
            return;
        }
        if tag == "slot"
            && let Some(h) = host
        {
            let assigned = self.slot_assigned(h, self.attr(id, "name"));
            if assigned.is_empty() {
                for c in self.children(id) {
                    self.serialize_live_node(c, host, clickable, in_anchor, out);
                }
            } else {
                for c in assigned {
                    self.serialize_live_node(c, None, clickable, in_anchor, out);
                }
            }
            return;
        }
        let is_click = clickable.contains(&id);
        let is_anchor = tag == "a";
        // A non-anchor clickable becomes a followable `<a>` marker — BUT
        // never nest one inside an existing anchor. An `<a>` inside an `<a>`
        // is invalid HTML; when the app re-parses this serialized output for
        // layout, html5ever's adoption agency SPLITS the outer anchor into
        // empty fragments that still carry its `aria-label`, which then leaks
        // as duplicated link text (archive.org tiles: a `<button class=info>`
        // wrapped inside the tile's own `<a>` printed the title three times).
        // Inside an anchor the clickable simply inherits that anchor's link.
        // A contenteditable host is routed to the editable-field path (it gets a
        // `data-trust-node` below and the form walk binds it), so never wrap it
        // as a JsClick — that would make it "follow" instead of "edit" even
        // though rich editors also register click listeners on their root.
        let wrap = is_click && !is_anchor && !in_anchor && !self.is_contenteditable_host(id);
        // Whether this element opens an anchor context for its descendants:
        // a real `<a>`, the wrapper we just emitted, or an already-open one.
        let child_in_anchor = in_anchor || is_anchor || is_click;
        if wrap {
            out.push_str(&format!("<a href=\"x-trust-js:{id}:\">"));
            // An icon-only clickable would render as an empty (and so
            // unselectable) link: give it a visible handle. An icon control
            // (an `<svg>`/`<use>` Font-Awesome-style glyph — the dominant web
            // icon idiom) shows the icon's GLYPH; a named-but-glyphless one
            // shows its accessible name; an unnamed one (a CSS-drawn carousel
            // dot) gets a compact marker rather than the noisy "[button]".
            if self.text_content(id).trim().is_empty() {
                if let Some(glyph) = self.icon_glyph(id) {
                    out.push_str(glyph);
                } else if let Some(label) = self
                    .attr(id, "aria-label")
                    .or_else(|| self.attr(id, "title"))
                    .or_else(|| self.attr(id, "value"))
                {
                    out.push('[');
                    out.push_str(&escape_text(label));
                    out.push(']');
                } else {
                    out.push('·');
                }
            }
        }
        out.push('<');
        out.push_str(tag);
        self.write_attrs(
            id,
            attrs,
            &mut |name, value| {
                (is_click && is_anchor && name == "href")
                    .then(|| format!("x-trust-js:{id}:{value}"))
            },
            out,
        );
        // A live anchor that never had an href still needs the marker.
        if is_click && is_anchor && self.attr(id, "href").is_none() {
            out.push_str(&format!(" href=\"x-trust-js:{id}:\""));
        }
        // The app re-parses this serialized HTML into a fresh layout DOM,
        // so form controls need an explicit pointer back to the resident
        // page actor's original node ids.
        if matches!(tag, "form" | "input" | "button" | "select" | "textarea")
            || self.is_contenteditable_host(id)
        {
            out.push_str(&format!(" data-trust-node=\"{id}\""));
        }
        out.push('>');
        if !VOID_ELEMENTS.contains(&tag) {
            if let Some(root) = self.shadow_root(id) {
                for c in self.children(root) {
                    self.serialize_live_node(c, Some(id), clickable, child_in_anchor, out);
                }
            } else {
                for c in self.children(id) {
                    self.serialize_live_node(c, host, clickable, child_in_anchor, out);
                }
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        if wrap {
            out.push_str("</a>");
        }
    }

    /// Write an element's attributes, baking the cascaded `display` into
    /// its `style` so the re-parsed layout arena flows it the way the
    /// engine (which has the external sheets) computed. `rewrite` lets the
    /// live serializer substitute an attribute value (anchor href markers).
    fn write_attrs(
        &self,
        id: NodeId,
        attrs: &[Attribute],
        rewrite: &mut dyn FnMut(&str, &str) -> Option<String>,
        out: &mut String,
    ) {
        // Bake the cascaded box/layout properties (the engine has the
        // sheets; the re-parsed layout arena doesn't) into the element's
        // inline style. `display:none`/`visibility:hidden` are already
        // dropped, so they never need baking.
        let mut bake = String::new();
        for prop in PROPS.iter().filter(|p| p.baked).map(|p| p.name) {
            if let Some(v) = self.cascaded(id, prop) {
                if prop == "display" && v == "none" {
                    continue;
                }
                // Resolve `var(--x, …)` to the defined custom-property value
                // now, while the stylesheets (and so the `--x` definitions) are
                // still here — the re-parsed layout arena has neither.
                let v = self.resolve_vars(id, &v);
                // An undefined `var()` with no fallback resolves to nothing —
                // don't bake an empty declaration.
                if v.trim().is_empty() {
                    continue;
                }
                bake.push_str(prop);
                bake.push(':');
                bake.push_str(&escape_attr(&v));
                bake.push(';');
            }
        }
        let mut style_done = false;
        for a in attrs {
            let name: &str = &a.name.local;
            let replaced = rewrite(name, &a.value);
            let value = replaced.as_deref().unwrap_or(&a.value);
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            out.push_str(&escape_attr(value));
            if name == "style" && !bake.is_empty() {
                if !value.trim().is_empty() && !value.trim_end().ends_with(';') {
                    out.push(';');
                }
                out.push_str(&bake);
                style_done = true;
            } else if name == "style" {
                style_done = true;
            }
            out.push('"');
        }
        if !bake.is_empty() && !style_done {
            out.push_str(" style=\"");
            out.push_str(&bake);
            out.push('"');
        }
        // Bake generated content (the layout arena has no `<style>` to
        // re-cascade `::before`/`::after`); the layout reads these attrs.
        for (which, attr) in [
            (PseudoEl::Before, "data-trust-before"),
            (PseudoEl::After, "data-trust-after"),
        ] {
            if let Some(t) = self.pseudo_content(id, which) {
                out.push(' ');
                out.push_str(attr);
                out.push_str("=\"");
                out.push_str(&escape_attr(&t));
                out.push('"');
            }
        }
        // Bake the clearfix signal for the same reason: the layout re-parses
        // this HTML with no `<style>`, so a `::after{clear:both}` rule (which
        // can't live in an inline `style`) would otherwise be lost and a float
        // grid would leak past its row. (`has_clearing_pseudo` reads the rule
        // here, the attribute at layout time.)
        if self.has_clearing_pseudo(id) {
            out.push_str(" data-trust-clearfix=\"\"");
        }
    }

    /// Non-element serialization shared between the plain and live
    /// serializers: documents/fragments recurse via `kids`, text
    /// escapes, the rest vanish.
    fn serialize_node_with(
        &self,
        id: NodeId,
        kids: &mut dyn FnMut(NodeId, &mut String),
        out: &mut String,
    ) {
        match &self.nodes[id].data {
            NodeData::Document | NodeData::Fragment => {
                for c in self.children(id) {
                    kids(c, out);
                }
            }
            NodeData::Doctype => {}
            NodeData::Comment(t) => {
                out.push_str("<!--");
                out.push_str(&t.replace("--", "- -"));
                out.push_str("-->");
            }
            NodeData::Text(t) => out.push_str(&escape_text(t)),
            NodeData::Element { .. } => unreachable!("elements handled by callers"),
        }
    }

    /// All `<script>` elements in document order, as (src-attr, inline
    /// source, type-attr) — the execution schedule for js.rs.
    /// Every `<script>` in document order: `(src, inline text, type, node)`.
    /// The node id lets the runner expose `document.currentScript` while a
    /// classic script executes.
    pub fn scripts(&self) -> Vec<(Option<String>, String, Option<String>, NodeId)> {
        self.descendants(DOCUMENT)
            .into_iter()
            .filter(|&d| self.tag_name(d) == Some("script"))
            .map(|d| {
                (
                    self.attr(d, "src").map(str::to_string),
                    self.text_content(d),
                    self.attr(d, "type").map(str::to_string),
                    d,
                )
            })
            .collect()
    }

    /// querySelector(All): match descendants of `root` against a
    /// selector list, document order.
    pub fn query(&self, root: NodeId, selectors: &SelectorList, first_only: bool) -> Vec<NodeId> {
        let mut out = Vec::new();
        for d in self.descendants(root) {
            // `:scope` in the selector resolves to this query root.
            if self.matches_scoped(d, selectors, Some(root)) {
                out.push(d);
                if first_only {
                    break;
                }
            }
        }
        out
    }

    pub fn matches(&self, id: NodeId, selectors: &SelectorList) -> bool {
        self.matches_scoped(id, selectors, None)
    }

    fn matches_scoped(&self, id: NodeId, selectors: &SelectorList, scope: Option<NodeId>) -> bool {
        selectors
            .0
            .iter()
            .any(|c| self.matches_complex(id, &c.0, scope))
    }

    fn matches_complex(
        &self,
        id: NodeId,
        parts: &[(Combinator, Compound)],
        scope: Option<NodeId>,
    ) -> bool {
        let Some(((comb, compound), rest)) = parts.split_last() else {
            return false;
        };
        if !self.matches_compound(id, compound, scope) {
            return false;
        }
        if rest.is_empty() {
            return true;
        }
        match comb {
            Combinator::Child => self.nodes[id]
                .parent
                .is_some_and(|p| self.matches_complex(p, rest, scope)),
            Combinator::Descendant | Combinator::None => {
                let mut up = self.nodes[id].parent;
                while let Some(a) = up {
                    if self.matches_complex(a, rest, scope) {
                        return true;
                    }
                    up = self.nodes[a].parent;
                }
                false
            }
            Combinator::NextSibling => self
                .prev_element_sibling(id)
                .is_some_and(|s| self.matches_complex(s, rest, scope)),
            Combinator::SubsequentSibling => {
                let mut sib = self.prev_element_sibling(id);
                while let Some(s) = sib {
                    if self.matches_complex(s, rest, scope) {
                        return true;
                    }
                    sib = self.prev_element_sibling(s);
                }
                false
            }
        }
    }

    /// The nearest preceding sibling that is an element (skips text/comments).
    fn prev_element_sibling(&self, id: NodeId) -> Option<NodeId> {
        let mut p = self.nodes[id].prev_sibling;
        while let Some(s) = p {
            if self.tag_name(s).is_some() {
                return Some(s);
            }
            p = self.nodes[s].prev_sibling;
        }
        None
    }

    /// `:empty` — the element has no element children and no text children
    /// with non-whitespace content (comments don't count).
    fn is_element_empty(&self, id: NodeId) -> bool {
        let mut child = self.nodes[id].first_child;
        while let Some(c) = child {
            match &self.nodes[c].data {
                NodeData::Element { .. } => return false,
                NodeData::Text(t) if !t.chars().all(char::is_whitespace) => return false,
                _ => {}
            }
            child = self.nodes[c].next_sibling;
        }
        true
    }

    /// The element's 1-based position among its parent's element children
    /// (`of_type`: only same-tag siblings; `from_end`: counted from the
    /// last). `None` if it has no parent or isn't an element.
    fn nth_position(&self, id: NodeId, of_type: bool, from_end: bool) -> Option<i32> {
        let parent = self.nodes[id].parent?;
        let my_tag = self.tag_name(id)?;
        let mut sibs = Vec::new();
        let mut child = self.nodes[parent].first_child;
        while let Some(c) = child {
            if let Some(t) = self.tag_name(c)
                && (!of_type || t == my_tag)
            {
                sibs.push(c);
            }
            child = self.nodes[c].next_sibling;
        }
        let idx = sibs.iter().position(|&s| s == id)?;
        Some(if from_end {
            (sibs.len() - idx) as i32
        } else {
            (idx + 1) as i32
        })
    }

    fn matches_structural(&self, id: NodeId, st: &Structural) -> bool {
        match st {
            Structural::Empty => self.is_element_empty(id),
            Structural::Nth {
                nth,
                of_type,
                from_end,
            } => self
                .nth_position(id, *of_type, *from_end)
                .is_some_and(|pos| nth.matches(pos)),
        }
    }

    fn matches_compound(&self, id: NodeId, c: &Compound, scope: Option<NodeId>) -> bool {
        if c.never {
            return false;
        }
        // `:scope` matches only the query root (None in the cascade → never).
        if c.scope && scope != Some(id) {
            return false;
        }
        let Some(tag) = self.tag_name(id) else {
            return false;
        };
        // `:root` is the document root element (`<html>` in HTML).
        if c.root && tag != "html" {
            return false;
        }
        if let Some(want) = &c.tag
            && want != "*"
            && want != tag
        {
            return false;
        }
        if let Some(want) = &c.id
            && self.attr(id, "id") != Some(want.as_str())
        {
            return false;
        }
        if !c.classes.is_empty() {
            let classes = self.attr(id, "class").unwrap_or("");
            let have: Vec<&str> = classes.split_ascii_whitespace().collect();
            if !c.classes.iter().all(|w| have.contains(&w.as_str())) {
                return false;
            }
        }
        for sel in &c.attrs {
            match self.attr(id, &sel.name) {
                None => return false,
                Some(got) => {
                    if !sel.matches(got) {
                        return false;
                    }
                }
            }
        }
        if !c
            .structural
            .iter()
            .all(|st| self.matches_structural(id, st))
        {
            return false;
        }
        c.nots.iter().all(|n| !self.matches_compound(id, n, scope))
    }
}

fn escape_text(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
    )
}

fn escape_attr(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>', '"']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
    )
}

// ---- Selector subset ------------------------------------------------

/// The workhorse selector grammar: `tag`, `*`, `#id`, `.class`,
/// `[attr]`, `[attr⊙=value]` (⊙ ∈ {ε, ~, |, ^, $, *}), `:not(compound)`,
/// the structural pseudo-classes (`:empty`, `:first-child`/`:last-child`/
/// `:only-child`, `:*-of-type`, `:nth-child(An+B)` and friends), compounds
/// thereof, and the descendant (space), child (`>`), next-sibling (`+`) and
/// subsequent-sibling (`~`) combinators, in comma lists. Interaction pseudos
/// (`:hover`…) and pseudo-elements parse but never match — valid CSS that
/// can't be true in our world.
pub struct SelectorList(Vec<Complex>);

struct Complex(Vec<(Combinator, Compound)>);

#[derive(PartialEq)]
enum Combinator {
    /// Leftmost compound: nothing to its left.
    None,
    Descendant,
    Child,
    /// `A + B`: B's immediately-preceding element sibling is A.
    NextSibling,
    /// `A ~ B`: some preceding element sibling of B is A.
    SubsequentSibling,
}

/// The `::before` / `::after` generated-content pseudo-elements (CSS2
/// `:before`/`:after` legacy spelling too). The only pseudo-elements we
/// act on; others parse but never match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PseudoEl {
    Before,
    After,
}

#[derive(Default)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrSel>,
    /// `:not(...)` arguments: the compound matches only if none do.
    nots: Vec<Compound>,
    /// `:hover`, `:focus` and other pseudos we can't satisfy: parse fine,
    /// match never (fail-open — a never-matching hide rule hides nothing,
    /// and its comma-siblings stay alive).
    never: bool,
    /// Structural pseudo-classes (`:empty`, `:nth-child(…)`, `:first-child`,
    /// `:*-of-type`, …) the element must satisfy. All must hold (AND).
    structural: Vec<Structural>,
    /// `:scope`: matches the element a rooted query (`querySelectorAll`/
    /// jQuery `.find()`) was called on. jQuery rewrites context-rooted comma/
    /// complex selectors to `:scope X, :scope Y`, so without this they match
    /// nothing (it silently broke deselection-style code). Inert in the
    /// stylesheet cascade (no query root there).
    scope: bool,
    /// `:root`: matches the document root element (`<html>`). The conventional
    /// home of custom-property definitions (`:root { --foo: … }`), so matching
    /// it is what lets `var(--foo)` resolve to a root-defined value.
    root: bool,
    /// `::before`/`::after`: the rule targets a generated-content box on
    /// the matched element, NOT the element itself. The element-property
    /// cascade skips these; `pseudo_content` consults only these.
    pseudo: Option<PseudoEl>,
    /// Pseudo-class count, for specificity only.
    pseudos: u32,
}

struct AttrSel {
    name: String,
    op: AttrOp,
    value: Option<String>,
}

/// `An+B` (the `:nth-child` micro-grammar): position `p` (1-based) matches
/// when `p = a*k + b` for some integer `k ≥ 0`.
struct Nth {
    a: i32,
    b: i32,
}

impl Nth {
    fn matches(&self, pos: i32) -> bool {
        if self.a == 0 {
            pos == self.b
        } else {
            let diff = pos - self.b;
            diff % self.a == 0 && diff / self.a >= 0
        }
    }
}

/// A structural pseudo-class: a positional/childless test that depends on
/// the element's siblings, not its own attributes.
enum Structural {
    /// `:empty` — no element or non-empty text children.
    Empty,
    /// `:nth-child(An+B)` and its variants. `of_type` counts only same-tag
    /// siblings; `from_end` counts position from the last sibling.
    /// (`:first-child` = `nth(1)`, `:last-child` = `nth(1)` from end, etc.)
    Nth {
        nth: Nth,
        of_type: bool,
        from_end: bool,
    },
}

/// Parse the `An+B` argument of `:nth-child(...)` etc. — `odd`, `even`,
/// `2n+1`, `-n+3`, `n`, `3`, `+3`, with optional internal whitespace.
fn parse_nth(s: &str) -> Option<Nth> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let s = s.to_ascii_lowercase();
    match s.as_str() {
        "odd" => return Some(Nth { a: 2, b: 1 }),
        "even" => return Some(Nth { a: 2, b: 0 }),
        _ => {}
    }
    if let Some(npos) = s.find('n') {
        let a = match &s[..npos] {
            "" | "+" => 1,
            "-" => -1,
            x => x.parse().ok()?,
        };
        let b_str = &s[npos + 1..];
        let b = if b_str.is_empty() {
            0
        } else {
            b_str.strip_prefix('+').unwrap_or(b_str).parse().ok()?
        };
        Some(Nth { a, b })
    } else {
        Some(Nth {
            a: 0,
            b: s.parse().ok()?,
        })
    }
}

/// The simple structural pseudo-classes (no argument), expanded to their
/// `:nth`-equivalents. `:only-*` is the conjunction of first and last.
fn structural_simple(name: &str) -> Option<Vec<Structural>> {
    let first = |of_type| Structural::Nth {
        nth: Nth { a: 0, b: 1 },
        of_type,
        from_end: false,
    };
    let last = |of_type| Structural::Nth {
        nth: Nth { a: 0, b: 1 },
        of_type,
        from_end: true,
    };
    Some(match name {
        "first-child" => vec![first(false)],
        "last-child" => vec![last(false)],
        "only-child" => vec![first(false), last(false)],
        "first-of-type" => vec![first(true)],
        "last-of-type" => vec![last(true)],
        "only-of-type" => vec![first(true), last(true)],
        _ => return None,
    })
}

/// CSS attribute selector operators: `=`, `~=`, `|=`, `^=`, `$=`, `*=`.
enum AttrOp {
    Exact,
    Includes,
    Dash,
    Prefix,
    Suffix,
    Substring,
}

impl AttrSel {
    fn matches(&self, got: &str) -> bool {
        let Some(want) = &self.value else {
            return true; // bare [attr]: presence is enough
        };
        match self.op {
            AttrOp::Exact => got == want,
            AttrOp::Includes => got.split_ascii_whitespace().any(|w| w == want),
            AttrOp::Dash => {
                got == want
                    || got
                        .strip_prefix(want.as_str())
                        .is_some_and(|r| r.starts_with('-'))
            }
            AttrOp::Prefix => !want.is_empty() && got.starts_with(want.as_str()),
            AttrOp::Suffix => !want.is_empty() && got.ends_with(want.as_str()),
            AttrOp::Substring => !want.is_empty() && got.contains(want.as_str()),
        }
    }
}

impl Compound {
    fn is_empty(&self) -> bool {
        self.tag.is_none()
            && self.id.is_none()
            && self.classes.is_empty()
            && self.attrs.is_empty()
            && self.nots.is_empty()
            && !self.never
            && !self.scope
            && !self.root
            && self.structural.is_empty()
            && self.pseudo.is_none()
    }

    /// (ids, classes+attrs+pseudo-classes, tags) — `:not` contributes
    /// its argument's counts, not its own.
    fn spec(&self) -> (u32, u32, u32) {
        let mut s = (
            u32::from(self.id.is_some()),
            self.classes.len() as u32 + self.attrs.len() as u32 + self.pseudos,
            u32::from(matches!(&self.tag, Some(t) if t != "*")),
        );
        for n in &self.nots {
            let ns = n.spec();
            s = (s.0 + ns.0, s.1 + ns.1, s.2 + ns.2);
        }
        s
    }
}

impl Complex {
    fn specificity(&self) -> (u32, u32, u32) {
        let mut s = (0, 0, 0);
        for (_, c) in &self.0 {
            let cs = c.spec();
            s = (s.0 + cs.0, s.1 + cs.1, s.2 + cs.2);
        }
        s
    }
}

/// Split on `sep` outside parens, brackets, and quotes — `:not(.a, .b)`
/// and `[title="x,y"]` must survive list splitting.
fn split_top_level(input: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    let mut quote: Option<char> = None;
    for (i, c) in input.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(' | '[') => depth += 1,
            (None, ')' | ']') => depth -= 1,
            (None, c) if c == sep && depth == 0 => {
                out.push(&input[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&input[start..]);
    out
}

impl SelectorList {
    pub fn parse(input: &str) -> Option<SelectorList> {
        let mut list = Vec::new();
        for part in split_top_level(input, ',') {
            list.push(parse_complex(part.trim())?);
        }
        if list.is_empty() {
            None
        } else {
            Some(SelectorList(list))
        }
    }
}

fn parse_complex(input: &str) -> Option<Complex> {
    let mut parts: Vec<(Combinator, Compound)> = Vec::new();
    let mut chars = input.chars().peekable();
    let mut pending = Combinator::None;
    loop {
        // Inter-compound whitespace / combinators.
        let mut saw_space = false;
        while let Some(&c) = chars.peek() {
            if c.is_ascii_whitespace() {
                saw_space = true;
                chars.next();
            } else if c == '>' {
                pending = Combinator::Child;
                chars.next();
            } else if c == '+' {
                pending = Combinator::NextSibling;
                chars.next();
            } else if c == '~' {
                pending = Combinator::SubsequentSibling;
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek().is_none() {
            break;
        }
        if pending == Combinator::None && saw_space && !parts.is_empty() {
            pending = Combinator::Descendant;
        }

        let compound = parse_compound(&mut chars)?;
        if compound.is_empty() {
            return None;
        }
        parts.push((std::mem::replace(&mut pending, Combinator::None), compound));
    }
    if parts.is_empty() {
        None
    } else {
        Some(Complex(parts))
    }
}

fn parse_compound(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<Compound> {
    let mut compound = Compound::default();
    while let Some(&c) = chars.peek() {
        match c {
            '#' => {
                chars.next();
                compound.id = Some(take_name(chars)?);
            }
            '.' => {
                chars.next();
                compound.classes.push(take_name(chars)?);
            }
            '[' => {
                chars.next();
                let inner: String = chars.by_ref().take_while(|&c| c != ']').collect();
                let (name, op, value) = match inner.split_once('=') {
                    Some((n, v)) => {
                        let (n, op) = match n.chars().last() {
                            Some('~') => (&n[..n.len() - 1], AttrOp::Includes),
                            Some('|') => (&n[..n.len() - 1], AttrOp::Dash),
                            Some('^') => (&n[..n.len() - 1], AttrOp::Prefix),
                            Some('$') => (&n[..n.len() - 1], AttrOp::Suffix),
                            Some('*') => (&n[..n.len() - 1], AttrOp::Substring),
                            _ => (n, AttrOp::Exact),
                        };
                        (n, op, Some(v.trim().trim_matches(['"', '\'']).to_string()))
                    }
                    None => (inner.as_str(), AttrOp::Exact, None),
                };
                if name.trim().is_empty() {
                    return None;
                }
                compound.attrs.push(AttrSel {
                    name: name.trim().to_ascii_lowercase(),
                    op,
                    value,
                });
            }
            ':' => {
                chars.next();
                // `::foo` (double colon) marks a pseudo-element; `:before`
                // and `:after` have a legacy single-colon spelling too.
                if chars.peek() == Some(&':') {
                    chars.next();
                }
                let name = take_name(chars)?.to_ascii_lowercase();
                let mut arg = None;
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1u32;
                    let mut inner = String::new();
                    for c in chars.by_ref() {
                        match c {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        inner.push(c);
                    }
                    if depth != 0 {
                        return None;
                    }
                    arg = Some(inner);
                }
                if name == "not" {
                    // Step-1 :not takes compounds (no combinators) —
                    // anything fancier fails the parse (rule ignored,
                    // fail-open). Specificity comes from the argument.
                    for part in split_top_level(&arg?, ',') {
                        let part = part.trim();
                        if part.is_empty() || part.contains(char::is_whitespace) {
                            return None;
                        }
                        let mut inner_chars = part.chars().peekable();
                        let inner = parse_compound(&mut inner_chars)?;
                        if inner.is_empty() || inner_chars.peek().is_some() {
                            return None;
                        }
                        compound.nots.push(inner);
                    }
                } else if name == "before" || name == "after" {
                    // Generated-content pseudo-element: the compound still
                    // matches the element (tag/class parts), but the rule
                    // targets the element's ::before/::after box.
                    compound.pseudo = Some(if name == "before" {
                        PseudoEl::Before
                    } else {
                        PseudoEl::After
                    });
                    compound.pseudos += 1;
                } else if name == "scope" {
                    // Matches the query root (set by `query`); inert in the
                    // cascade. See `Compound::scope`.
                    compound.scope = true;
                    compound.pseudos += 1;
                } else if name == "root" {
                    compound.root = true;
                    compound.pseudos += 1;
                } else if name == "empty" {
                    compound.structural.push(Structural::Empty);
                    compound.pseudos += 1;
                } else if let Some(simple) = structural_simple(&name) {
                    compound.structural.extend(simple);
                    compound.pseudos += 1;
                } else if let Some((of_type, from_end)) = match name.as_str() {
                    "nth-child" => Some((false, false)),
                    "nth-last-child" => Some((false, true)),
                    "nth-of-type" => Some((true, false)),
                    "nth-last-of-type" => Some((true, true)),
                    _ => None,
                } {
                    // A malformed/absent An+B fails the parse (rule ignored,
                    // fail-open) rather than silently mismatching.
                    let nth = parse_nth(&arg?)?;
                    compound.structural.push(Structural::Nth {
                        nth,
                        of_type,
                        from_end,
                    });
                    compound.pseudos += 1;
                } else {
                    // Valid CSS we can never satisfy (no pointer, no focus):
                    // parse, count for specificity, never match.
                    compound.never = true;
                    compound.pseudos += 1;
                }
            }
            c if c.is_ascii_whitespace() || c == '>' || c == '+' || c == '~' => break,
            _ => {
                let tag = take_name(chars)?;
                compound.tag = Some(tag.to_ascii_lowercase());
            }
        }
    }
    Some(compound)
}

/// An identifier, `*`, or tag token.
fn take_name(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    let mut out = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_alphanumeric() || matches!(c, '-' | '_' | '*') {
            out.push(c);
            chars.next();
        } else {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

// ---- CSS visibility cascade (step 1) ---------------------------------
// A real mini-cascade for exactly two properties, `display` and
// `visibility`, so stylesheet-class hiding (.hidden{display:none}) and
// class-toggle re-showing (.menu.open{display:block}) work. Everything
// unparseable is IGNORED — fail-open always means "visible", never
// "hidden". `:hover`/`:focus` never match; @-blocks are skipped whole.

/// One CSS property the engine understands — the single source of truth
/// for the whole property surface. `is_tracked` (what the cascade stores)
/// and the serializer's bake list both derive from this table, so adding a
/// property is one entry here, not edits in three places. Kept deliberately
/// small: the box-layout primitives plus the visibility/animation set;
/// everything else is ignored (not stored, fail-open).
struct PropDef {
    name: &'static str,
    /// Inherited (CSS sense): when an element doesn't set this property,
    /// `computed_value` resolves it to the parent's computed value.
    /// `text-decoration` is deliberately NOT here — it is not inherited but
    /// *propagated* by painting (and accumulates), handled by
    /// `text_decoration` instead.
    inherited: bool,
    /// Baked into the element's inline `style` on serialization, so the
    /// re-parsed layout arena (which has no `<style>`) flows the property
    /// the way the engine computed it. `false` for properties consumed only
    /// inside the engine and never re-read from serialized HTML: `visibility`
    /// (hidden nodes are dropped outright), `opacity`/`animation*` (folded
    /// into `is_hidden`'s slideshow logic), and `content` (baked separately
    /// as `data-trust-before`/`data-trust-after` attributes).
    baked: bool,
}

const fn prop(name: &'static str, inherited: bool, baked: bool) -> PropDef {
    PropDef {
        name,
        inherited,
        baked,
    }
}

#[rustfmt::skip]
const PROPS: &[PropDef] = &[
    //    name                    inherited  baked
    prop("display",              false,     true),
    prop("visibility",           true,      false),
    prop("opacity",              false,     false),
    prop("animation-name",       false,     false),
    prop("animation-fill-mode",  false,     false),
    prop("animation",            false,     false),
    prop("margin-top",           false,     true),
    prop("margin-bottom",        false,     true),
    prop("margin-left",          false,     true),
    prop("margin-right",         false,     true),
    prop("padding-top",          false,     true),
    prop("padding-bottom",       false,     true),
    prop("padding-left",         false,     true),
    prop("text-align",           true,      true),
    prop("font-weight",          true,      true),
    prop("font-style",           true,      true),
    prop("white-space",          true,      true),
    prop("text-transform",       true,      true),
    prop("letter-spacing",       true,      true),
    prop("list-style-type",      true,      true),
    prop("list-style-position",  true,      true),
    prop("text-indent",          true,      true),
    prop("text-decoration",      false,     true),
    prop("text-decoration-line", false,     true),
    prop("content",              false,     false),
    prop("width",                false,     true),
    prop("max-width",            false,     true),
    prop("min-width",            false,     true),
    prop("height",               false,     true),
    prop("aspect-ratio",         false,     true),
    prop("object-fit",           false,     true),
    prop("flex-wrap",            false,     true),
    prop("flex-flow",            false,     true),
    prop("flex-direction",       false,     true),
    prop("float",                false,     true),
    prop("clear",                false,     true),
    prop("overflow",             false,     true),
    prop("cursor",               false,     false),
    prop("position",             false,     true),
    prop("z-index",              false,     true),
    prop("top",                  false,     true),
    prop("right",                false,     true),
    prop("bottom",               false,     true),
    prop("left",                 false,     true),
    prop("flex-grow",            false,     true),
    prop("flex-shrink",          false,     true),
    prop("flex-basis",           false,     true),
    prop("flex",                 false,     true),
    prop("gap",                  false,     true),
    prop("column-gap",           false,     true),
    prop("row-gap",              false,     true),
    prop("grid-template-columns", false,    true),
    prop("grid-template-rows",   false,     true),
    prop("grid-auto-flow",       false,     true),
    prop("grid-auto-columns",    false,     true),
    prop("grid-auto-rows",       false,     true),
    prop("grid-column",          false,     true),
    prop("grid-row",             false,     true),
    prop("justify-content",      false,     true),
    prop("align-items",          false,     true),
    prop("order",                false,     true),
    prop("border-top-width",     false,     true),
    prop("border-right-width",   false,     true),
    prop("border-bottom-width",  false,     true),
    prop("border-left-width",    false,     true),
    prop("border-top-style",     false,     true),
    prop("border-right-style",   false,     true),
    prop("border-bottom-style",  false,     true),
    prop("border-left-style",    false,     true),
];

fn is_tracked(name: &str) -> bool {
    // Custom properties (`--foo`) are always stored so `var()` references can
    // resolve to their defined (cascaded, inherited) value at bake time, not
    // just the fallback. They inherit and are case-folded like everything else.
    name.starts_with("--") || PROPS.iter().any(|p| p.name == name)
}

/// The HTML user-agent stylesheet's default `display` for a tag — what a
/// browser's `getComputedStyle(el).display` reports for an element with no
/// author `display`. jQuery's `.show()` reads an element's default display
/// (by computing the display of a throwaway element of the same tag) so it
/// can restore it; when that read comes back empty it falls back to a
/// temp-`<iframe>` probe (`iframe.contentWindow.document`) the prelude can't
/// satisfy, which threw and tore down jQuery's whole `.show()`/render path on
/// humblebundle.com. Reporting the UA display keeps jQuery off the iframe
/// path. This feeds `getComputedStyle` only; the layout owns the real display
/// via `computed_display` (author cascade + the layout's own tag tables).
fn ua_display(tag: &str) -> &'static str {
    match tag {
        "address" | "article" | "aside" | "blockquote" | "body" | "details" | "dialog" | "div"
        | "dl" | "dd" | "dt" | "fieldset" | "figcaption" | "figure" | "footer" | "form" | "h1"
        | "h2" | "h3" | "h4" | "h5" | "h6" | "header" | "hgroup" | "hr" | "html" | "main"
        | "nav" | "ol" | "p" | "pre" | "section" | "summary" | "ul" => "block",
        "li" => "list-item",
        "table" => "table",
        "thead" => "table-header-group",
        "tbody" => "table-row-group",
        "tfoot" => "table-footer-group",
        "tr" => "table-row",
        "td" | "th" => "table-cell",
        "caption" => "table-caption",
        "colgroup" => "table-column-group",
        "col" => "table-column",
        "button" | "input" | "select" | "textarea" | "meter" | "progress" => "inline-block",
        "head" | "title" | "meta" | "link" | "style" | "script" | "base" | "noscript"
        | "template" | "source" | "track" | "datalist" => "none",
        _ => "inline",
    }
}

/// Whether a CSS length is ≤ 1px — the box size of the "sr-only" visually
/// hidden clip idiom. Only unitless `0`/`1` and `px` lengths qualify; `em`,
/// `%`, `auto`, etc. are not the pattern and return `false`.
fn css_len_at_most_1px(v: &str) -> bool {
    let v = v.trim();
    let n = v.strip_suffix("px").unwrap_or(v).trim();
    n.parse::<f32>().is_ok_and(|x| x <= 1.0)
}

/// Whether a CSS length/percentage is exactly zero (`0`, `0px`, `0%`, `0em`,
/// …) — its leading numeric part parses to 0. `auto`/empty/`calc(…)`/
/// non-numeric → false (we can't prove those zero, so we never hide on them).
fn css_len_is_zero(v: &str) -> bool {
    let num: String = v
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+'))
        .collect();
    !num.is_empty() && num.parse::<f32>().map(|n| n == 0.0).unwrap_or(false)
}

/// Inline-by-default phrasing elements that, used as a `position:absolute`
/// overlay, are CHROME on a card — a corner badge (a view/photo count), a
/// caption, a price tag, a "NEW" ribbon — not a slide of a deck. They sit in a
/// corner alongside the content rather than stacking over it, so they must not
/// count toward a slideshow's slides (see `is_slideshow_container`). A real
/// deck's slides are block containers (`<div>`/`<li>`/`<figure>`) or media.
fn is_inline_overlay_chrome(tag: &str) -> bool {
    matches!(
        tag,
        "span"
            | "i"
            | "b"
            | "em"
            | "strong"
            | "small"
            | "label"
            | "sub"
            | "sup"
            | "abbr"
            | "cite"
            | "code"
            | "mark"
            | "s"
            | "u"
            | "q"
            | "samp"
            | "kbd"
            | "var"
            | "time"
            | "big"
            | "tt"
            | "ins"
            | "del"
            | "data"
            | "output"
            | "bdi"
            | "bdo"
    )
}

/// Below this effective opacity an element is treated as invisible (hidden).
/// Keeps merely-faded content (e.g. `opacity:0.5`) visible.
const OPACITY_HIDDEN: f32 = 0.05;

/// Expand a `margin`/`padding`/`border*`/`list-style` shorthand into the
/// longhands we track; pass anything else through unchanged.
fn expand_box_shorthand(prop: &str, value: &str) -> Vec<(String, String)> {
    if prop == "margin" || prop == "padding" {
        let Some([t, r, b, l]) = four_sides(value) else {
            return Vec::new();
        };
        return vec![
            (format!("{prop}-top"), t.to_string()),
            (format!("{prop}-right"), r.to_string()),
            (format!("{prop}-bottom"), b.to_string()),
            (format!("{prop}-left"), l.to_string()),
        ];
    }
    // `inset`: 1–4 values, top/right/bottom/left (the offset shorthand a
    // full-viewport modal often uses, `inset:0`).
    if prop == "inset" {
        let Some([t, r, b, l]) = four_sides(value) else {
            return Vec::new();
        };
        return vec![
            ("top".to_string(), t.to_string()),
            ("right".to_string(), r.to_string()),
            ("bottom".to_string(), b.to_string()),
            ("left".to_string(), l.to_string()),
        ];
    }
    // `border-width`/`border-style`: 1–4 values, top/right/bottom/left.
    if let Some(kind) = prop
        .strip_prefix("border-")
        .filter(|k| *k == "width" || *k == "style")
    {
        let Some(sides) = four_sides(value) else {
            return Vec::new();
        };
        return ["top", "right", "bottom", "left"]
            .iter()
            .zip(sides)
            .map(|(side, v)| (format!("border-{side}-{kind}"), v.to_string()))
            .collect();
    }
    // `border` / `border-{side}`: a `width || style || color` shorthand. We
    // keep the width and style (color is ignored), per side.
    if prop == "border" {
        let (w, s) = parse_border_shorthand(value);
        return border_longhands(&["top", "right", "bottom", "left"], w, s);
    }
    if let Some(side) = prop
        .strip_prefix("border-")
        .filter(|s| matches!(*s, "top" | "right" | "bottom" | "left"))
    {
        let (w, s) = parse_border_shorthand(value);
        return border_longhands(&[side], w, s);
    }
    // `grid-gap`/`grid-row-gap`/`grid-column-gap`: the deprecated aliases of
    // `gap`/`row-gap`/`column-gap` (still emitted by older toolchains and
    // GitHub's Primer). Normalize to the modern names the layout reads.
    if let Some(rest) = prop.strip_prefix("grid-")
        && matches!(rest, "gap" | "row-gap" | "column-gap")
    {
        return vec![(rest.to_string(), value.to_string())];
    }
    // `grid-template: <rows> / <columns>` (the area form is ignored — we don't
    // place by name). Split on the top-level `/` into the two track lists.
    if prop == "grid-template" {
        if let Some((rows, cols)) = split_top_level_slash(value) {
            return vec![
                ("grid-template-rows".to_string(), rows.trim().to_string()),
                ("grid-template-columns".to_string(), cols.trim().to_string()),
            ];
        }
        return Vec::new();
    }
    // `list-style: <type> || <position> || <image>` — we track the type and
    // position keywords (a bare `none` counts as the type, per the shorthand
    // grammar; the image and any URL are ignored).
    if prop == "list-style" {
        let mut out = Vec::new();
        if let Some(t) = list_style_shorthand_type(value) {
            out.push(("list-style-type".to_string(), t.to_string()));
        }
        if let Some(p) = value
            .split_whitespace()
            .find(|t| matches!(*t, "inside" | "outside"))
        {
            out.push(("list-style-position".to_string(), p.to_string()));
        }
        return out;
    }
    vec![(prop.to_string(), value.to_string())]
}

/// Split a value on the first `/` at paren-depth 0 (so a `minmax(a, b)` or
/// `repeat(2, 1fr)` track keeps its inner contents). `None` if there is no
/// top-level slash. Used for the `grid-template: rows / columns` shorthand.
fn split_top_level_slash(value: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    for (i, b) in value.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'/' if depth == 0 => return Some((&value[..i], &value[i + 1..])),
            _ => {}
        }
    }
    None
}

/// The top/right/bottom/left values of a CSS 1–4-value box shorthand.
fn four_sides(value: &str) -> Option<[&str; 4]> {
    let p: Vec<&str> = value.split_whitespace().collect();
    match p.as_slice() {
        [a] => Some([a, a, a, a]),
        [a, b] => Some([a, b, a, b]),
        [a, b, c] => Some([a, b, c, b]),
        [a, b, c, d] => Some([a, b, c, d]),
        _ => None,
    }
}

/// The `(width, style)` of a `border`/`border-<side>` shorthand (color
/// dropped). Order-independent: the style keyword and a width token (`thin`/
/// `medium`/`thick` or a length) are picked out; anything else is the color.
fn parse_border_shorthand(value: &str) -> (Option<&str>, Option<&str>) {
    const STYLES: &[&str] = &[
        "none", "hidden", "solid", "dashed", "dotted", "double", "groove", "ridge", "inset",
        "outset",
    ];
    let mut width = None;
    let mut style = None;
    for tok in value.split_whitespace() {
        if STYLES.contains(&tok) {
            style = Some(tok);
        } else if tok == "thin"
            || tok == "medium"
            || tok == "thick"
            || tok.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        {
            width = Some(tok);
        }
    }
    (width, style)
}

fn border_longhands(sides: &[&str], w: Option<&str>, s: Option<&str>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for side in sides {
        if let Some(w) = w {
            out.push((format!("border-{side}-width"), w.to_string()));
        }
        if let Some(s) = s {
            out.push((format!("border-{side}-style"), s.to_string()));
        }
    }
    out
}

/// The `list-style-type` keyword inside a `list-style` shorthand, if present.
fn list_style_shorthand_type(value: &str) -> Option<&str> {
    const TYPES: &[&str] = &[
        "none",
        "disc",
        "circle",
        "square",
        "decimal",
        "decimal-leading-zero",
        "lower-alpha",
        "upper-alpha",
        "lower-latin",
        "upper-latin",
        "lower-roman",
        "upper-roman",
    ];
    value.split_whitespace().find(|t| TYPES.contains(t))
}

/// One parsed rule, holding its tracked declarations (`(prop, (important,
/// value))`). Rules mentioning no tracked property are never stored.
struct StyleRule {
    selector: Complex,
    specificity: (u32, u32, u32),
    /// Source position across every sheet of the scope.
    order: usize,
    decls: Vec<(String, (bool, String))>,
}

/// (!important, inline, specificity, source order): the cascade key;
/// lexicographic max wins.
type CascadeKey = (bool, bool, (u32, u32, u32), usize);

fn consider(slot: &mut Option<(CascadeKey, String)>, key: CascadeKey, value: &str) {
    if slot.as_ref().is_none_or(|(k, _)| key >= *k) {
        *slot = Some((key, value.to_string()));
    }
}

/// Rules bucketed by tree scope: DOCUMENT for the light DOM, the shadow
/// fragment for each shadow tree. Shadow sheets never leak out;
/// document sheets never reach in.
#[derive(Default)]
struct StyleIndex {
    scopes: std::collections::HashMap<NodeId, Vec<StyleRule>>,
    /// Per-scope rule index, keyed by each rule's rightmost-compound key
    /// (id/class/tag/universal) — the standard browser "rule hash" so an
    /// element only tests rules that could possibly match it (see
    /// `matched_rules`). Parallel to `scopes`; values index into it.
    buckets: std::collections::HashMap<NodeId, RuleBuckets>,
    /// `@keyframes <name>` → the animation's END opacity (the `to`/`100%`
    /// keyframe), for honoring an `animation-fill-mode:forwards` reveal/hide.
    /// Only opacity is extracted (the one keyframe property visibility needs).
    keyframes: std::collections::HashMap<String, f32>,
    /// Whether any rule sets `opacity` at all — lets `is_hidden` skip the
    /// opacity cascade entirely on the overwhelming majority of pages.
    has_opacity: bool,
}

/// Rules of one scope, bucketed by the rightmost compound's most-selective
/// simple key. An element gathers candidates from the buckets matching its own
/// id/classes/tag plus `universal` (rules whose subject has no id/class/tag,
/// e.g. `*`, `[attr]`, pseudo-only), then full-matches only those. Each rule
/// lands in exactly one bucket, so the candidate sets are disjoint.
#[derive(Default)]
struct RuleBuckets {
    by_id: std::collections::HashMap<String, Vec<u32>>,
    by_class: std::collections::HashMap<String, Vec<u32>>,
    by_tag: std::collections::HashMap<String, Vec<u32>>,
    universal: Vec<u32>,
}

impl RuleBuckets {
    fn build(rules: &[StyleRule]) -> Self {
        let mut b = RuleBuckets::default();
        for (i, r) in rules.iter().enumerate() {
            let i = i as u32;
            // The subject (rightmost) compound decides the bucket; the most
            // selective key present wins (id > first class > tag).
            match r.selector.0.last().map(|(_, c)| c) {
                Some(c) if c.id.is_some() => {
                    b.by_id.entry(c.id.clone().unwrap()).or_default().push(i);
                }
                Some(c) if !c.classes.is_empty() => {
                    b.by_class.entry(c.classes[0].clone()).or_default().push(i);
                }
                Some(c) if c.tag.as_deref().is_some_and(|t| t != "*") => {
                    b.by_tag.entry(c.tag.clone().unwrap()).or_default().push(i);
                }
                _ => b.universal.push(i),
            }
        }
        b
    }
}

/// Parse one `prop: value [!important]` declaration. The value is
/// lowercased (keyword props), EXCEPT `content`, whose text is
/// case-significant (`content:"Read more"`).
fn parse_decl(decl: &str) -> Option<(String, String, bool)> {
    let (k, v) = decl.split_once(':')?;
    let k = k.trim().to_ascii_lowercase();
    let v = v.trim();
    let (v, important) = match v.rsplit_once('!') {
        Some((head, bang)) if bang.trim().eq_ignore_ascii_case("important") => (head, true),
        _ => (v, false),
    };
    let v = v.trim();
    let value = if k == "content" {
        v.to_string()
    } else {
        v.to_ascii_lowercase()
    };
    Some((k, value, important))
}

/// The pseudo-element a rule's subject (last compound) targets, if any.
fn rule_pseudo(rule: &StyleRule) -> Option<PseudoEl> {
    rule.selector.0.last().and_then(|(_, c)| c.pseudo)
}

/// Strip the surrounding quotes from a CSS string and decode its escapes
/// (`\HEX ` codepoints and `\c` literals). `None` if `v` isn't quoted.
fn unquote_css(v: &str) -> Option<String> {
    let quote = v.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    if v.chars().count() < 2 || !v.ends_with(quote) {
        return None;
    }
    let inner: String = {
        let mut it = v.chars();
        it.next();
        it.next_back();
        it.collect()
    };
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
            hex.push(chars.next().unwrap());
        }
        if !hex.is_empty() {
            // CSS allows one trailing whitespace to delimit the escape.
            if chars.peek() == Some(&' ') {
                chars.next();
            }
            if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(ch);
            }
        } else if let Some(lit) = chars.next() {
            out.push(lit);
        }
    }
    Some(out)
}

fn strip_css_comments(css: &str) -> Cow<'_, str> {
    if !css.contains("/*") {
        return Cow::Borrowed(css);
    }
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        out.push(' ');
        match rest[i + 2..].find("*/") {
            Some(j) => rest = &rest[i + 2 + j + 2..],
            None => return Cow::Owned(out),
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Collect a sheet's tracked rules into `out`. `@keyframes` end-opacity is
/// harvested; `@media` is evaluated against `viewport` (the CSS-pixel
/// viewport) and its body spliced in when it matches (dropped otherwise);
/// other @-blocks are skipped whole. Rules whose selectors don't parse are
/// skipped (fail-open).
fn parse_sheet(
    css: &str,
    order: &mut usize,
    out: &mut Vec<StyleRule>,
    keyframes: &mut std::collections::HashMap<String, f32>,
    viewport: (u32, u32),
) {
    let css = strip_css_comments(css);
    let mut rest = css.as_ref();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return;
        }
        if let Some(after) = rest.strip_prefix('@') {
            // `@keyframes <name> { ... }` (and the -webkit- prefix): we read
            // only the END opacity, to honor an animation that reveals/hides
            // an element via `animation-fill-mode:forwards` (slideshow fades).
            let lower = after.trim_start().to_ascii_lowercase();
            if let Some(rest_name) = lower
                .strip_prefix("keyframes")
                .or_else(|| lower.strip_prefix("-webkit-keyframes"))
                && let Some(brace_off) = after.find('{')
            {
                let name = after[after.len() - rest_name.len()..brace_off]
                    .trim()
                    .to_string();
                let (block, tail) = take_block(&after[brace_off..]);
                if let Some(end) = keyframes_end_opacity(block) {
                    keyframes.insert(name, end);
                }
                rest = tail;
                continue;
            }
            // `@media <query> { ... }`: evaluate the query against the
            // viewport and splice the matching block's rules into the cascade
            // (recurse, so nested @media and normal rules both work); drop the
            // body when it doesn't match. The viewport is what `execute_js`
            // reports (`cols*cell_px`).
            if let Some(rest_q) = lower.strip_prefix("media")
                && rest_q
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-')
                && let Some(brace_off) = after.find('{')
            {
                let query = &after[after.len() - rest_q.len()..brace_off];
                let (block, tail) = take_block(&after[brace_off..]);
                if media_query_matches(query, viewport) {
                    parse_sheet(block, order, out, keyframes, viewport);
                }
                rest = tail;
                continue;
            }
            // Other @-rules (@charset/@import end at ';'; block at-rules at
            // their balanced '}') are skipped whole.
            rest = match (after.find(';'), after.find('{')) {
                (Some(s), Some(b)) if s < b => &after[s + 1..],
                (_, Some(b)) => take_block(&after[b..]).1,
                (Some(s), None) => &after[s + 1..],
                (None, None) => return,
            };
            continue;
        }
        let Some(brace) = rest.find('{') else { return };
        let selector_text = rest[..brace].trim();
        let (block, after) = take_block(&rest[brace..]);
        rest = after;
        parse_style_rule(selector_text, block, order, out, viewport);
    }
}

/// Process one style-rule body into `out`: emit its own declarations for the
/// already-concrete selector list `resolved`, then recurse into any nested
/// rules (CSS Nesting), expanding each nested selector's `&` against
/// `resolved`. A nested `@media` applies its body to `resolved` when it
/// matches the viewport. `resolved` never carries an unexpanded `&` — the
/// top-level caller passes the raw selector and `expand_nesting` resolves the
/// `&` before each recursion.
///
/// Without this, a nested rule's declarations would leak onto the parent: the
/// width-reservation/underline idiom `.tab { &::after { width:100% } }` (Steam's
/// `.supernav`, Primer, many design systems) would make `.tab` itself
/// `width:100%`, breaking horizontal nav layouts.
fn parse_style_rule(
    resolved: &str,
    block: &str,
    order: &mut usize,
    out: &mut Vec<StyleRule>,
    viewport: (u32, u32),
) {
    let (decl_text, nested) = split_block(block);
    let decls = collect_decls(&decl_text);
    if !decls.is_empty()
        && let Some(SelectorList(complexes)) = SelectorList::parse(resolved.trim())
    {
        for selector in complexes {
            out.push(StyleRule {
                specificity: selector.specificity(),
                selector,
                order: *order,
                decls: decls.clone(),
            });
            *order += 1;
        }
    }
    for (nsel, nblock) in nested {
        // A nested grouping at-rule (CSS Nesting allows `@media`/`@supports`
        // inside a style rule). Evaluate `@media`; on a match apply its body to
        // the SAME parent selector. Other nested at-rules are skipped whole —
        // never leak their declarations onto the parent.
        if let Some(at) = nsel.strip_prefix('@') {
            let at = at.trim_start();
            let lower = at.to_ascii_lowercase();
            if let Some(rest_q) = lower.strip_prefix("media")
                && rest_q
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-')
                && media_query_matches(&at[5..], viewport)
            {
                parse_style_rule(resolved, nblock, order, out, viewport);
            }
            continue;
        }
        let child = expand_nesting(nsel, resolved);
        parse_style_rule(&child, nblock, order, out, viewport);
    }
}

/// Parse a declaration block's text into tracked `(prop, (important, value))`
/// pairs (later wins; never demote `!important`); shorthands are expanded.
fn collect_decls(decl_text: &str) -> Vec<(String, (bool, String))> {
    let mut decls: Vec<(String, (bool, String))> = Vec::new();
    for decl in decl_text.split(';') {
        let Some((k, v, important)) = parse_decl(decl) else {
            continue;
        };
        for (pk, pv) in expand_box_shorthand(&k, &v) {
            if !is_tracked(&pk) {
                continue;
            }
            if let Some(slot) = decls.iter_mut().find(|(n, _)| *n == pk) {
                if important >= slot.1.0 {
                    slot.1 = (important, pv);
                }
            } else {
                decls.push((pk, (important, pv)));
            }
        }
    }
    decls
}

/// Split a rule body into its declaration text and its nested rules
/// `(prelude, body)` (CSS Nesting). A top-level `{` begins a nested rule whose
/// prelude is the text back to the previous `;`/`}`; the remaining segments are
/// declarations. String/paren/bracket aware so a `;` or `{` inside `url(...)`,
/// `[attr=…]`, or a quoted value (`content:"{"`) doesn't split. The common
/// nesting-free block borrows its text unchanged.
fn split_block(block: &str) -> (Cow<'_, str>, Vec<(&str, &str)>) {
    if !block.contains('{') {
        return (Cow::Borrowed(block), Vec::new());
    }
    let bytes = block.as_bytes();
    let mut decls = String::new();
    let mut nested: Vec<(&str, &str)> = Vec::new();
    let mut seg_start = 0usize;
    let mut in_str: Option<u8> = None;
    let (mut paren, mut bracket) = (0i32, 0i32);
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => in_str = Some(c),
            b'(' => paren += 1,
            b')' => paren = (paren - 1).max(0),
            b'[' => bracket += 1,
            b']' => bracket = (bracket - 1).max(0),
            b';' if paren == 0 && bracket == 0 => {
                let seg = &block[seg_start..i];
                if !seg.trim().is_empty() {
                    decls.push_str(seg);
                    decls.push(';');
                }
                seg_start = i + 1;
            }
            b'{' if paren == 0 && bracket == 0 => {
                let prelude = block[seg_start..i].trim();
                let (inner, tail) = take_block(&block[i..]);
                if !prelude.is_empty() {
                    nested.push((prelude, inner));
                }
                i += block[i..].len() - tail.len();
                seg_start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let seg = &block[seg_start..];
    if !seg.trim().is_empty() {
        decls.push_str(seg);
        decls.push(';');
    }
    (Cow::Owned(decls), nested)
}

/// Expand a nested selector against its parent (CSS Nesting `&`). Returns a
/// concrete comma-joined selector list. Each `&` is replaced by the parent; a
/// nested selector with no `&` is a descendant (`parent nested`). When the
/// parent is itself a list, the product over (parent × nested) parts is taken
/// — equivalent to substituting `:is(parent)` for matching, without needing
/// `:is`.
fn expand_nesting(nested: &str, parent: &str) -> String {
    let parents = split_top_level(parent, ',');
    let mut out: Vec<String> = Vec::new();
    for n in split_top_level(nested, ',') {
        let n = n.trim();
        if n.is_empty() {
            continue;
        }
        for p in &parents {
            let p = p.trim();
            if n.contains('&') {
                out.push(n.replace('&', p));
            } else {
                out.push(format!("{p} {n}"));
            }
        }
    }
    out.join(", ")
}

/// Does a CSS `@media` query list match the viewport (CSS px; `0` = unknown)?
/// A comma list is OR. Within one query, conditions join with `and`; a
/// recognized media type (`screen`/`all`) and the width/height/orientation
/// features are evaluated, `not`/`only` honored. Anything unrecognized — or a
/// width/height test with an unknown viewport — makes that query NOT match,
/// which drops its rules exactly as skipping the whole `@media` block used to.
fn media_query_matches(query: &str, vp: (u32, u32)) -> bool {
    query
        .split(',')
        .any(|q| media_query_one(&q.trim().to_ascii_lowercase(), vp))
}

/// One comma-separated media query (already lowercased). A leading
/// `not`/`only` is a prefix on the whole query (not an `and`-joined part);
/// the rest is a media type and/or `and`-joined `(feature: value)` conditions.
fn media_query_one(q: &str, vp: (u32, u32)) -> bool {
    let mut q = q.trim();
    let mut negate = false;
    if let Some(rest) = q.strip_prefix("not ") {
        negate = true;
        q = rest.trim();
    } else if let Some(rest) = q.strip_prefix("only ") {
        q = rest.trim();
    }
    if q.is_empty() {
        return !negate; // bare `@media { }` / `@media only` applies to all
    }
    let mut matches = true;
    for part in q.split(" and ") {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(inner) = part.strip_prefix('(') {
            if !media_feature_matches(inner.trim_end_matches(')'), vp) {
                matches = false;
            }
        } else {
            // A bare token: only `screen`/`all` are our medium; any other type
            // (print/speech/tv/…) or unknown word can't match.
            match part {
                "screen" | "all" => {}
                _ => matches = false,
            }
        }
    }
    matches ^ negate
}

/// A single `feature: value` media condition against the viewport.
fn media_feature_matches(inner: &str, vp: (u32, u32)) -> bool {
    let (vw, vh) = vp;
    let Some((name, value)) = inner.split_once(':') else {
        return false; // boolean feature (`(color)`) — unrecognized, no match
    };
    let value = value.trim();
    match name.trim() {
        "min-width" => vw != 0 && media_px(value).is_some_and(|n| vw >= n),
        "max-width" => vw != 0 && media_px(value).is_some_and(|n| vw <= n),
        "width" => vw != 0 && media_px(value).is_some_and(|n| vw == n),
        "min-height" => vh != 0 && media_px(value).is_some_and(|n| vh >= n),
        "max-height" => vh != 0 && media_px(value).is_some_and(|n| vh <= n),
        "height" => vh != 0 && media_px(value).is_some_and(|n| vh == n),
        "orientation" if vw != 0 && vh != 0 => match value {
            "portrait" => vh >= vw,
            "landscape" => vw > vh,
            _ => false,
        },
        _ => false,
    }
}

/// A media-feature length as CSS pixels: `px`/unitless as-is, `em`/`rem` at
/// 16px. Other units (or unparseable) → `None` (the condition won't match).
/// The icon NAME inside a token, if it carries a Font-Awesome / icon-set
/// prefix: `fa-NAME` / `fas-fa-NAME` (FA), `bi-NAME` (Bootstrap Icons),
/// `icon-NAME`. Returns the longest trailing icon name (`svg-fas-fa-ellipsis`
/// → `ellipsis`, `#fas-fa-ellipsis` → `ellipsis`). A bare `fa`/`svg-fa` (no
/// dash-name) is not a name.
fn icon_token_name(tok: &str) -> Option<&str> {
    let tok = tok.trim_start_matches('#');
    for sep in ["fa-", "bi-", "icon-"] {
        if let Some(pos) = tok.rfind(sep) {
            let name = &tok[pos + sep.len()..];
            // A real icon name is non-empty, alphanumeric/dash (drop a trailing
            // state class accidentally glued on by the rfind on the wrong sep).
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return Some(name);
            }
        }
    }
    None
}

/// The Unicode glyph for a recognized icon name (Font-Awesome vocabulary, the
/// de-facto web icon naming). Covers the common UI/nav set; an unknown name
/// returns `None` (the caller falls back to the accessible name, then a marker).
fn icon_glyph_for(name: &str) -> Option<&'static str> {
    Some(match name {
        "ellipsis" | "ellipsis-h" => "⋯",
        "ellipsis-v" | "ellipsis-vertical" => "⋮",
        "bars" | "list" | "list-ul" => "☰",
        "bell" | "bell-o" => "🔔",
        "bookmark" | "bookmark-o" => "🔖",
        "rss" | "rss-square" | "feed" => "📡",
        "cog" | "cogs" | "gear" | "gears" | "sliders" => "⚙",
        "user" | "user-circle" | "circle-user" | "user-o" => "👤",
        "users" | "user-group" | "people-group" => "👥",
        "heart" | "heart-o" => "♥",
        "comment" | "comments" | "comment-dots" | "message" | "comment-o" => "💬",
        "search" | "magnifying-glass" => "🔍",
        "upload" | "cloud-upload" | "cloud-arrow-up" | "arrow-up-from-bracket" => "⬆",
        "download" | "cloud-download" | "cloud-arrow-down" | "arrow-down-to-bracket" => "⬇",
        "share" | "share-alt" | "share-nodes" | "arrow-up-from-square" => "↗",
        "link" | "chain" => "🔗",
        "camera" | "camera-retro" => "📷",
        "image" | "images" | "photo" | "picture-o" => "🖼",
        "eye" => "👁",
        "eye-slash" => "🙈",
        "video" | "video-camera" | "film" | "clapperboard" => "🎬",
        "play" | "circle-play" | "play-circle" => "▶",
        "pause" => "⏸",
        "times" | "xmark" | "close" | "x" | "remove" => "✕",
        "check" | "check-circle" | "circle-check" => "✓",
        "plus" | "add" => "＋",
        "minus" => "−",
        "star" | "star-o" => "★",
        "home" | "house" => "⌂",
        "envelope" | "envelope-o" | "mail" | "inbox" => "✉",
        "gear-complex" => "⚙",
        "trash" | "trash-o" | "trash-can" | "trash-alt" => "🗑",
        "edit" | "pen" | "pencil" | "pen-to-square" | "pencil-alt" => "✎",
        "lock" => "🔒",
        "unlock" | "lock-open" => "🔓",
        "flag" | "flag-o" => "⚑",
        "thumbs-up" | "thumbs-o-up" => "👍",
        "thumbs-down" | "thumbs-o-down" => "👎",
        "retweet" | "repeat" => "🔁",
        "gift" => "🎁",
        "fire" => "🔥",
        "bolt" | "flash" => "⚡",
        "globe" | "earth" => "🌐",
        "gear-six" => "⚙",
        "chevron-down" | "angle-down" | "caret-down" | "sort-down" => "▾",
        "chevron-up" | "angle-up" | "caret-up" | "sort-up" => "▴",
        "chevron-left" | "angle-left" | "caret-left" => "◂",
        "chevron-right" | "angle-right" | "caret-right" => "▸",
        "arrow-up" => "↑",
        "arrow-down" => "↓",
        "arrow-left" => "←",
        "arrow-right" => "→",
        "external-link" | "arrow-up-right-from-square" | "up-right-from-square" => "↗",
        _ => return None,
    })
}

fn media_px(value: &str) -> Option<u32> {
    let v = value.trim();
    let split = v
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(v.len());
    let n: f32 = v[..split].parse().ok()?;
    let px = match v[split..].trim() {
        "px" | "" => n,
        "em" | "rem" => n * 16.0,
        _ => return None,
    };
    Some(px.round().max(0.0) as u32)
}

/// The opacity at an `@keyframes` animation's END — the value at the highest
/// keyframe offset (`to`/`100%`). `None` if no keyframe sets opacity. Only
/// the END matters: with `animation-fill-mode:forwards` that's the resting
/// state, so a fade-in resolves to its `to{opacity:1}` (visible) and a
/// fade-out to `to{opacity:0}` (hidden).
fn keyframes_end_opacity(block: &str) -> Option<f32> {
    let mut best: Option<(f32, f32)> = None; // (offset, opacity)
    let mut rest = block;
    while let Some(brace) = rest.find('{') {
        let sel = &rest[..brace];
        let (decls, tail) = take_block(&rest[brace..]);
        rest = tail;
        let offset = sel
            .split(',')
            .filter_map(keyframe_offset)
            .fold(f32::MIN, f32::max);
        if offset == f32::MIN {
            continue;
        }
        for decl in decls.split(';') {
            if let Some((k, v, _)) = parse_decl(decl)
                && k == "opacity"
                && let Ok(o) = v.trim().parse::<f32>()
                && best.is_none_or(|(bo, _)| offset >= bo)
            {
                best = Some((offset, o));
            }
        }
    }
    best.map(|(_, o)| o)
}

/// A keyframe selector offset as a 0..1 fraction (`from`=0, `to`=1, `N%`).
fn keyframe_offset(sel: &str) -> Option<f32> {
    match sel.trim() {
        "from" => Some(0.0),
        "to" => Some(1.0),
        s => s
            .strip_suffix('%')
            .and_then(|p| p.trim().parse::<f32>().ok())
            .map(|p| p / 100.0),
    }
}

/// Whether an `animation` shorthand token is a non-name part (a time, a
/// timing function, an iteration count, a direction/fill/play keyword) — so
/// the remaining token can be taken as the `animation-name`.
fn is_anim_keyword_or_time(tok: &str) -> bool {
    const KW: &[&str] = &[
        "none",
        "normal",
        "reverse",
        "alternate",
        "alternate-reverse",
        "infinite",
        "running",
        "paused",
        "linear",
        "ease",
        "ease-in",
        "ease-out",
        "ease-in-out",
        "step-start",
        "step-end",
    ];
    if KW.contains(&tok) {
        return true;
    }
    let num = tok
        .strip_suffix("ms")
        .or_else(|| tok.strip_suffix('s'))
        .unwrap_or(tok);
    num.parse::<f32>().is_ok()
        || tok.parse::<f32>().is_ok()
        || tok.starts_with("cubic-bezier")
        || tok.starts_with("steps")
}

/// `input` starts at '{'; return (inner text, after-the-matching-'}').
fn take_block(input: &str) -> (&str, &str) {
    let mut depth = 0i32;
    for (i, c) in input.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return (&input[1..i], &input[i + 1..]);
                }
            }
            _ => {}
        }
    }
    // Unbalanced sheet: everything after the brace is the block.
    (&input[1.min(input.len())..], "")
}

// ---- CSSOM: stylesheet text → a rule tree exposed to page JS ---------
//
// `parse_sheet` above is a CASCADE builder: it drops untracked properties,
// flattens `@media` against the viewport, and keeps only the data layout
// needs. CSSOM is a different view — page JS reads `<style>.sheet.cssRules`
// for raw fidelity (`selectorText`, every declaration, at-rule structure),
// e.g. feature-detection libraries and css3test's `Supports.atrule`. So
// this is a separate, lossless-ish parser whose output (compact JSON) the
// js.rs prelude wraps as CSSStyleRule/CSSMediaRule/etc. Unknown at-rules
// are DROPPED — a real browser omits unrecognized at-rules from cssRules,
// which is exactly what at-rule feature detection relies on.

/// Whether the selector engine can parse `sel` (backs `CSS.supports(
/// "selector(…)")`). Honest: only selectors we can actually evaluate.
pub fn selector_parses(sel: &str) -> bool {
    let sel = sel.trim();
    !sel.is_empty() && SelectorList::parse(sel).is_some()
}

/// Parse a stylesheet into the CSSOM rule tree as compact JSON.
pub fn parse_cssom_json(css: &str) -> String {
    let css = strip_css_comments(css);
    cssom_rules_json(css.as_ref())
}

/// One JSON array of rules from a chunk of stylesheet text (recurses for
/// grouping at-rules like `@media`).
fn cssom_rules_json(css: &str) -> String {
    let mut out = String::from("[");
    let mut rest = css;
    let mut first = true;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(after) = rest.strip_prefix('@') {
            let (json, tail) = at_rule_json(after);
            rest = tail;
            if let Some(j) = json {
                push_item(&mut out, &mut first, &j);
            }
            continue;
        }
        let Some(brace) = rest.find('{') else { break };
        let sel = rest[..brace].trim().to_string();
        let (block, tail) = take_block(&rest[brace..]);
        rest = tail;
        // Keep every braced rule with a non-empty prelude: CSSOM is a text
        // view, so `selectorText` is preserved even for selectors the
        // engine can't evaluate (the cascade drops those separately).
        if sel.is_empty() {
            continue;
        }
        let item = format!(
            "{{\"t\":\"style\",\"sel\":{},\"d\":{}}}",
            json_string(&sel),
            decls_json(block)
        );
        push_item(&mut out, &mut first, &item);
    }
    out.push(']');
    out
}

/// An at-rule body (text after the `@`). Returns its JSON (None = unknown,
/// dropped) and the tail after its `;` or closing `}`.
fn at_rule_json(after: &str) -> (Option<String>, &str) {
    let name_end = after
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(after.len());
    let raw_name = after[..name_end].to_ascii_lowercase();
    let name = raw_name
        .trim_start_matches("-webkit-")
        .trim_start_matches("-moz-")
        .trim_start_matches("-o-")
        .trim_start_matches("-ms-");
    let semi = after.find(';');
    let brace = after.find('{');
    let statement = match (semi, brace) {
        (Some(s), Some(b)) => s < b,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => true,
    };
    if statement {
        let end = semi.map(|s| s + 1).unwrap_or(after.len());
        let prelude = after[name_end..semi.unwrap_or(after.len())].trim();
        return (statement_at_rule_json(name, prelude), &after[end..]);
    }
    let b = brace.unwrap();
    let prelude = after[name_end..b].trim().to_string();
    let (body, tail) = take_block(&after[b..]);
    (block_at_rule_json(name, &prelude, body), tail)
}

fn block_at_rule_json(name: &str, prelude: &str, body: &str) -> Option<String> {
    let grouping = |t: &str| {
        Some(format!(
            "{{\"t\":\"{}\",\"q\":{},\"r\":{}}}",
            t,
            json_string(prelude),
            cssom_rules_json(body)
        ))
    };
    match name {
        "media" => grouping("media"),
        "supports" => grouping("supports"),
        "container" => grouping("container"),
        "scope" => grouping("scope"),
        "layer" => grouping("layer"),
        "document" => grouping("document"),
        "keyframes" => Some(format!(
            "{{\"t\":\"keyframes\",\"name\":{},\"r\":{}}}",
            json_string(prelude),
            keyframes_rules_json(body)
        )),
        "font-face" => Some(format!(
            "{{\"t\":\"font-face\",\"d\":{}}}",
            decls_json(body)
        )),
        "page" => Some(format!(
            "{{\"t\":\"page\",\"sel\":{},\"d\":{}}}",
            json_string(prelude),
            decls_json(body)
        )),
        "counter-style" => Some(format!(
            "{{\"t\":\"counter-style\",\"name\":{},\"d\":{}}}",
            json_string(prelude),
            decls_json(body)
        )),
        "property" => Some(format!(
            "{{\"t\":\"property\",\"name\":{},\"d\":{}}}",
            json_string(prelude),
            decls_json(body)
        )),
        "font-feature-values" => Some(format!(
            "{{\"t\":\"font-feature-values\",\"name\":{},\"d\":[]}}",
            json_string(prelude)
        )),
        _ => None,
    }
}

fn statement_at_rule_json(name: &str, prelude: &str) -> Option<String> {
    match name {
        // @charset never appears in cssRules in real browsers — drop it.
        "import" => Some(format!(
            "{{\"t\":\"import\",\"q\":{}}}",
            json_string(prelude)
        )),
        "namespace" => Some(format!(
            "{{\"t\":\"namespace\",\"q\":{}}}",
            json_string(prelude)
        )),
        "layer" => Some(format!(
            "{{\"t\":\"layer\",\"q\":{},\"r\":[]}}",
            json_string(prelude)
        )),
        _ => None,
    }
}

/// `@keyframes` body: a list of keyframe rules whose "selector" is the
/// keyText (`0%`/`from`/`to`).
fn keyframes_rules_json(body: &str) -> String {
    let mut out = String::from("[");
    let mut rest = body;
    let mut first = true;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let Some(brace) = rest.find('{') else { break };
        let key = rest[..brace].trim().to_string();
        let (block, tail) = take_block(&rest[brace..]);
        rest = tail;
        let item = format!(
            "{{\"t\":\"keyframe\",\"key\":{},\"d\":{}}}",
            json_string(&key),
            decls_json(block)
        );
        push_item(&mut out, &mut first, &item);
    }
    out.push(']');
    out
}

/// A declaration block → JSON array of `[name, value]` pairs (raw, NOT
/// filtered by `is_tracked` — CSSOM reports what was written). Naive
/// `;`-split, matching `parse_sheet`.
fn decls_json(block: &str) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for decl in block.split(';') {
        let Some((k, v, _important)) = parse_decl(decl) else {
            continue;
        };
        let item = format!("[{},{}]", json_string(&k), json_string(&v));
        push_item(&mut out, &mut first, &item);
    }
    out.push(']');
    out
}

fn push_item(out: &mut String, first: &mut bool, item: &str) {
    if !*first {
        out.push(',');
    }
    *first = false;
    out.push_str(item);
}

/// A JSON-encoded string literal (quotes + escapes).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- html5ever integration ------------------------------------------

struct Sink {
    dom: RefCell<Dom>,
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = Dom;
    type ElemName<'a> = Ref<'a, QualName>;

    fn finish(self) -> Dom {
        self.dom.into_inner()
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        DOCUMENT
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> Ref<'a, QualName> {
        Ref::map(self.dom.borrow(), |d| match &d.nodes[*target].data {
            NodeData::Element { name, .. } => name,
            _ => panic!("elem_name on a non-element"),
        })
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        let mut dom = self.dom.borrow_mut();
        let contents = flags.template.then(|| dom.new_node(NodeData::Fragment));
        dom.new_node(NodeData::Element {
            name,
            attrs,
            template_contents: contents,
        })
    }

    fn create_comment(&self, text: StrTendril) -> NodeId {
        self.dom.borrow_mut().create_comment(&text)
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        self.dom.borrow_mut().create_comment("")
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        let mut dom = self.dom.borrow_mut();
        match child {
            NodeOrText::AppendNode(n) => dom.append(*parent, n),
            NodeOrText::AppendText(t) => dom.append_text(*parent, &t),
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        if self.dom.borrow().nodes[*element].parent.is_some() {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
        let mut dom = self.dom.borrow_mut();
        let dt = dom.new_node(NodeData::Doctype);
        dom.append(DOCUMENT, dt);
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        match &self.dom.borrow().nodes[*target].data {
            NodeData::Element {
                template_contents: Some(c),
                ..
            } => *c,
            _ => panic!("get_template_contents on a non-template"),
        }
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let mut dom = self.dom.borrow_mut();
        let Some(parent) = dom.nodes[*sibling].parent else {
            return;
        };
        match new_node {
            NodeOrText::AppendNode(n) => dom.insert_before(parent, n, Some(*sibling)),
            NodeOrText::AppendText(t) => {
                // Merge into the preceding text node when there is one.
                if let Some(prev) = dom.nodes[*sibling].prev_sibling
                    && let NodeData::Text(existing) = &mut dom.nodes[prev].data
                {
                    existing.push_str(&t);
                    return;
                }
                let tn = dom.create_text(&t);
                dom.insert_before(parent, tn, Some(*sibling));
            }
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, new_attrs: Vec<Attribute>) {
        let mut dom = self.dom.borrow_mut();
        if let NodeData::Element { attrs, .. } = &mut dom.nodes[*target].data {
            for a in new_attrs {
                if !attrs.iter().any(|e| e.name == a.name) {
                    attrs.push(a);
                }
            }
        }
    }

    fn associate_with_form(
        &self,
        _target: &NodeId,
        _form: &NodeId,
        _nodes: (&NodeId, Option<&NodeId>),
    ) {
    }

    fn remove_from_parent(&self, target: &NodeId) {
        self.dom.borrow_mut().detach(*target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut dom = self.dom.borrow_mut();
        for c in dom.children(*node) {
            dom.append(*new_parent, c);
        }
    }

    fn mark_script_already_started(&self, _node: &NodeId) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cssom_json_preserves_rule_structure() {
        // Style rule keeps selectorText + every declaration; @media nests its
        // children; @font-face is a descriptor block; an unknown at-rule is
        // dropped (browsers omit unrecognized at-rules from cssRules).
        let json = parse_cssom_json(
            "a.x { color: red; margin: 0 } \
             @media (min-width: 1px) { p { display: block } } \
             @font-face { font-family: Z } \
             @bogusrule q { z: 1 }",
        );
        assert!(json.contains(r#""t":"style""#), "{json}");
        assert!(json.contains(r#""sel":"a.x""#), "{json}");
        assert!(json.contains(r#"["color","red"]"#), "{json}");
        assert!(json.contains(r#"["margin","0"]"#), "{json}");
        assert!(json.contains(r#""t":"media""#), "{json}");
        assert!(json.contains(r#""q":"(min-width: 1px)""#), "{json}");
        assert!(json.contains(r#""t":"font-face""#), "{json}");
        // The unknown at-rule contributes no rule.
        assert!(
            !json.contains("bogusrule"),
            "unknown at-rule dropped: {json}"
        );
        assert!(!json.contains(r#"["z","1"]"#), "{json}");
    }

    #[test]
    fn selector_parses_accepts_real_rejects_empty() {
        assert!(selector_parses("a > b.c"));
        assert!(selector_parses(":scope .tab"));
        assert!(!selector_parses(""));
        assert!(!selector_parses("   "));
    }

    #[test]
    fn parses_and_serializes_a_document() {
        let dom = Dom::parse_document(
            "<html><head><title>T</title></head><body><p id=a>hi <b>there</b></p></body></html>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("<p id=\"a\">hi <b>there</b></p>"), "{html}");
    }

    #[test]
    fn serializer_drops_script_noscript_template_style_and_comments() {
        let dom = Dom::parse_document(
            "<body><script>evil()</script><noscript>no js!</noscript>\
             <template><p>inert</p></template><style>p{color:red}</style>\
             <!-- c -->keep</body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("evil"), "{html}");
        assert!(!html.contains("no js"), "{html}");
        assert!(!html.contains("inert"), "{html}");
        assert!(!html.contains("color:red"), "{html}");
        assert!(html.contains("keep"), "{html}");
    }

    #[test]
    fn css_cascade_hides_and_reshows() {
        // Stylesheet-class hiding, and the part a one-way hide-list
        // would get wrong: a MORE SPECIFIC rule re-showing.
        let dom = Dom::parse_document(
            "<head><style>
                .hidden { display: none }
                .menu { display: none }
                .menu.open { display: block }
             </style></head>
             <body><p class=hidden>secret</p>
             <div class=menu>shut menu</div>
             <div class='menu open'>open menu</div></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("secret"), "{html}");
        assert!(!html.contains("shut menu"), "{html}");
        assert!(html.contains("open menu"), "{html}");
    }

    #[test]
    fn css_opacity_hides_and_animation_reveals_one_slide() {
        // The W3C/Bootstrap slideshow idiom: every slide is opacity:0, and
        // the active one is revealed by a fade-in whose end state (fill-mode
        // forwards) is opacity:1. Honoring opacity (and the animation's end
        // opacity) shows exactly the active slide — no slideshow-specific code.
        let dom = Dom::parse_document(
            "<head><style>
                @keyframes fade-in { from { opacity: 0 } to { opacity: 1 } }
                @keyframes fade-out { from { opacity: 1 } to { opacity: 0 } }
                .slide { opacity: 0 }
                .slide.active { animation-name: fade-in; animation-fill-mode: forwards }
                .slide.leaving { animation-name: fade-out; animation-fill-mode: forwards }
                .faded { opacity: 0.5 }
             </style></head>
             <body>
               <div class='slide active'>shown slide</div>
               <div class='slide'>hidden slide</div>
               <div class='slide leaving'>leaving slide</div>
               <div class='faded'>still visible</div>
             </body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("shown slide"), "active slide visible: {html}");
        assert!(
            !html.contains("hidden slide"),
            "opacity:0 slide hidden: {html}"
        );
        assert!(
            !html.contains("leaving slide"),
            "fade-out ends opacity:0 → hidden: {html}"
        );
        assert!(
            html.contains("still visible"),
            "merely-faded (0.5) stays visible: {html}"
        );
    }

    #[test]
    fn css_cascade_inline_and_important_precedence() {
        // Inline style beats sheet rules — except !important.
        let dom = Dom::parse_document(
            "<head><style>
                #a { display: none }
                #b { display: none !important }
             </style></head>
             <body><p id=a style='display:block'>inline wins</p>
             <p id=b style='display:block'>important wins</p></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("inline wins"), "{html}");
        assert!(!html.contains("important wins"), "{html}");
    }

    #[test]
    fn css_cascade_fails_open() {
        // :hover can't be true here; @media blocks are skipped whole; a
        // selector list with an unparseable member (`:nth-child()` — an
        // empty An+B) dies entirely (the spec's rule, and it fails toward
        // VISIBLE).
        let dom = Dom::parse_document(
            "<head><style>
                .x:hover { display: none }
                @media (max-width: 600px) { .x { display: none } }
                :nth-child(), .y { display: none }
                .z { display: none }
             </style></head>
             <body><p class=x>pointer</p><p class=y>comma survivor</p>\
             <p class=z>plain hide</p></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("pointer"), "{html}");
        assert!(html.contains("comma survivor"), "{html}");
        assert!(!html.contains("plain hide"), "{html}");
    }

    #[test]
    fn css_not_and_attr_operators_match() {
        let dom = Dom::parse_document(
            "<head><style>
                li:not(.keep) { display: none }
                [data-state^=clos] { visibility: hidden }
             </style></head>
             <body><ul><li class=keep>kept</li><li>dropped</li></ul>
             <div data-state=closed>shut</div><div data-state=open>still open</div></body>",
        );
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("kept"), "{html}");
        assert!(!html.contains("dropped"), "{html}");
        assert!(!html.contains("shut"), "{html}");
        assert!(html.contains("still open"), "{html}");
    }

    #[test]
    fn css_shadow_scope_is_isolated() {
        // Shadow sheets hide shadow content but never leak into the
        // document; document sheets never reach into shadow trees.
        let mut dom = Dom::parse_document(
            "<head><style>.doc-hidden{display:none}</style></head>
             <body><div id=host></div><p class=sec>light sec stays</p>
             <p class=doc-hidden>doc target</p></body>",
        );
        let host = dom.get_by_id("host").unwrap();
        let root = dom.attach_shadow(host);
        let style = dom.create_element("style");
        let css = dom.create_text(".sec { display: none }");
        dom.append(style, css);
        dom.append(root, style);
        let hidden_span = dom.create_element("span");
        dom.set_attr(hidden_span, "class", "sec");
        let t1 = dom.create_text("shadow secret");
        dom.append(hidden_span, t1);
        dom.append(root, hidden_span);
        let shown_span = dom.create_element("span");
        dom.set_attr(shown_span, "class", "doc-hidden");
        let t2 = dom.create_text("shadow shown");
        dom.append(shown_span, t2);
        dom.append(root, shown_span);
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("shadow secret"), "{html}");
        assert!(html.contains("light sec stays"), "{html}");
        assert!(!html.contains("doc target"), "{html}");
        assert!(html.contains("shadow shown"), "{html}");
    }

    #[test]
    fn css_cascade_follows_mutations() {
        // The cached index rebuilds on the mutation epoch: class
        // toggles genuinely show and re-hide.
        let mut dom = Dom::parse_document(
            "<head><style>.menu{display:none}.menu.open{display:block}</style></head>
             <body><div id=m class=menu>payload</div></body>",
        );
        assert!(!dom.serialize(DOCUMENT).contains("payload"));
        let m = dom.get_by_id("m").unwrap();
        dom.set_attr(m, "class", "menu open");
        assert!(dom.serialize(DOCUMENT).contains("payload"));
        dom.set_attr(m, "class", "menu");
        assert!(!dom.serialize(DOCUMENT).contains("payload"));
    }

    #[test]
    fn external_sheets_join_the_cascade() {
        let mut dom = Dom::parse_document(
            "<head><link rel=stylesheet href='/a.css'></head>
             <body><p class=x>linked hide</p></body>",
        );
        assert_eq!(dom.stylesheet_links(), vec![String::from("/a.css")]);
        dom.attach_external_sheets(&[(String::from("/a.css"), String::from(".x{display:none}"))]);
        assert!(!dom.serialize(DOCUMENT).contains("linked hide"));
    }

    #[test]
    fn alternate_and_disabled_stylesheets_are_skipped() {
        // Only applied stylesheets feed the cascade and the fetch list: an
        // `alternate` stylesheet (user-selectable, off by default) and a
        // `disabled` one don't apply (HTML §4.6.7), so we neither fetch nor
        // attach them — they must not crowd real sheets out of the fetch cap.
        let dom = Dom::parse_document(
            "<head>\
             <link rel=stylesheet href='/main.css'>\
             <link rel='alternate stylesheet' href='/theme-dark.css'>\
             <link rel=stylesheet href='/late.css' disabled>\
             </head><body></body>",
        );
        assert_eq!(dom.stylesheet_links(), vec![String::from("/main.css")]);
    }

    #[test]
    fn hidden_pseudo_element_generates_no_content() {
        // The width-reservation idiom: a hidden bold copy of a tab label via
        // `::before{content:attr(data-content);visibility:hidden}`. Its content
        // must NOT render (else the label doubles — GitHub's "CodeCode"). A
        // visible `::before` still renders.
        let dom = Dom::parse_document(
            "<head><style>\
             .tab::before{content:attr(data-content);visibility:hidden}\
             .tag::before{content:\"#\"}\
             </style></head>\
             <body><span class=tab data-content=Code>Code</span>\
             <span class=tag>topic</span></body>",
        );
        let tab = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&i| dom.attr(i, "class") == Some("tab"))
            .unwrap();
        let tag = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&i| dom.attr(i, "class") == Some("tag"))
            .unwrap();
        assert_eq!(
            dom.pseudo_content(tab, PseudoEl::Before),
            None,
            "hidden ::before renders nothing"
        );
        assert_eq!(
            dom.pseudo_content(tag, PseudoEl::Before).as_deref(),
            Some("#"),
            "visible ::before still renders"
        );
    }

    #[test]
    fn css_nesting_keeps_nested_declarations_off_the_parent() {
        // CSS Nesting (2023): `.supernav { &::after { display:block; width:100% } }`
        // is Steam's nav-underline idiom (Primer and many design systems too).
        // The `&::after` declarations must target the ::after box, NOT leak onto
        // `.supernav` itself — leaking `width:100%` onto a floated nav item makes
        // every item fill the line and stack vertically. Likewise a plain nested
        // rule resolves to a descendant selector.
        let dom = Dom::parse_document(
            "<head><style>\
             .supernav{float:left}\
             .supernav{ &::after{ content:\"\"; display:block; width:100% } }\
             .card{ color:x; & .title{ font-weight:bold } }\
             </style></head>\
             <body>\
             <a class=supernav>STORE</a>\
             <div class=card><span class=title>Hi</span></div></body>",
        );
        let by = |cls: &str| {
            dom.descendants(DOCUMENT)
                .into_iter()
                .find(|&i| dom.attr(i, "class") == Some(cls))
                .unwrap()
        };
        let nav = by("supernav");
        // The nested `&::after` decls did NOT contaminate the element itself.
        assert_eq!(
            dom.computed_value(nav, "width"),
            None,
            "nested ::after width:100% must not apply to .supernav"
        );
        assert_ne!(
            dom.computed_value(nav, "display").as_deref(),
            Some("block"),
            "nested ::after display:block must not apply to .supernav"
        );
        assert_eq!(
            dom.computed_value(nav, "float").as_deref(),
            Some("left"),
            "the parent's own float survives"
        );
        // The decls landed on the ::after box instead.
        assert_eq!(
            dom.pseudo_style(nav, PseudoEl::After, "width").as_deref(),
            Some("100%"),
            "nested ::after width:100% reaches the pseudo box"
        );
        // A bare nested rule (`& .title`) resolves to a descendant.
        let title = by("title");
        assert_eq!(
            dom.computed_value(title, "font-weight").as_deref(),
            Some("bold"),
            "`.card & .title` applies to the descendant"
        );
    }

    #[test]
    fn computed_value_inherits_only_inherited_properties() {
        // An inherited property flows to a descendant that doesn't set it; a
        // non-inherited one stays put. This is the single inheritance
        // authority the layout and getComputedStyle both read through.
        let dom = Dom::parse_document(
            "<head><style>#outer{text-align:center;margin-left:4px}</style></head>
             <body><div id=outer><p id=inner>x</p></div></body>",
        );
        let inner = dom.get_by_id("inner").unwrap();
        assert_eq!(
            dom.computed_value(inner, "text-align").as_deref(),
            Some("center"),
            "text-align inherits"
        );
        assert_eq!(
            dom.computed_value(inner, "margin-left"),
            None,
            "margin-left does not inherit"
        );
    }

    #[test]
    fn computed_value_applies_and_inherits_ua_defaults() {
        // `<b>` is bold via the UA default layer; a nested span inherits it;
        // an explicit normal weight wins over the inherited bold.
        let dom = Dom::parse_document(
            "<body><b id=b>bold <span id=s>still</span>\
             <span id=n style='font-weight:normal'>not</span></b></body>",
        );
        let b = dom.get_by_id("b").unwrap();
        let s = dom.get_by_id("s").unwrap();
        let n = dom.get_by_id("n").unwrap();
        assert_eq!(
            dom.computed_value(b, "font-weight").as_deref(),
            Some("bold"),
            "UA default"
        );
        assert_eq!(
            dom.computed_value(s, "font-weight").as_deref(),
            Some("bold"),
            "inherited from <b>"
        );
        assert_eq!(
            dom.computed_value(n, "font-weight").as_deref(),
            Some("normal"),
            "own value beats inherited UA default"
        );
    }

    #[test]
    fn text_decoration_accumulates_and_resets() {
        // Underline + line-through accumulate across nesting (each box adds
        // its line); `text-decoration:none` clears both from there down.
        let dom = Dom::parse_document(
            "<body><u id=u>under <s id=s>both</s>\
             <span id=clear style='text-decoration:none'>neither</span></u></body>",
        );
        let u = dom.get_by_id("u").unwrap();
        let s = dom.get_by_id("s").unwrap();
        let clear = dom.get_by_id("clear").unwrap();
        assert_eq!(dom.text_decoration(u), (true, false), "<u> underlines");
        assert_eq!(
            dom.text_decoration(s),
            (true, true),
            "<s> inside <u> adds strike, keeps underline"
        );
        assert_eq!(
            dom.text_decoration(clear),
            (false, false),
            "text-decoration:none clears both"
        );
    }

    #[test]
    fn computed_value_memo_follows_mutations() {
        // The memo is epoch-keyed: changing an ancestor's class re-resolves an
        // inherited value rather than serving a stale cache hit.
        let mut dom = Dom::parse_document(
            "<head><style>.up{text-transform:uppercase}</style></head>
             <body><div id=o><span id=i>x</span></div></body>",
        );
        let i = dom.get_by_id("i").unwrap();
        let o = dom.get_by_id("o").unwrap();
        assert_eq!(dom.computed_value(i, "text-transform"), None);
        dom.set_attr(o, "class", "up");
        assert_eq!(
            dom.computed_value(i, "text-transform").as_deref(),
            Some("uppercase"),
            "mutation invalidates the inherited-value memo"
        );
    }

    #[test]
    fn rule_hash_buckets_resolve_the_same_cascade_as_a_full_scan() {
        // The rule-hash (rightmost-key buckets + per-element match memo) must
        // pick exactly the rules a full scan would. Exercises every bucket and
        // the cases a naive bucketing would get wrong: a multi-class subject
        // where the element has only one of the classes (must NOT match), a
        // universal/attribute subject (always tested), an id subject, a tag
        // subject, and specificity ordering across buckets.
        let dom = Dom::parse_document(
            "<head><style>\
               div { letter-spacing: 1px }\
               .box { letter-spacing: 2px }\
               .box.active { letter-spacing: 3px }\
               [data-on] { text-indent: 9px }\
               #hero { letter-spacing: 5px }\
             </style></head>\
             <body>\
               <div id=hero class='box active' data-on>h</div>\
               <div id=plain class='box'>p</div>\
               <span id=s class='active'>s</span>\
             </body>",
        );
        let hero = dom.get_by_id("hero").unwrap();
        let plain = dom.get_by_id("plain").unwrap();
        let s = dom.get_by_id("s").unwrap();
        // hero matches div/.box/.box.active/[data-on]/#hero; #hero wins
        // letter-spacing on specificity, and the attribute rule still applies.
        assert_eq!(
            dom.computed_style(hero, "letter-spacing").as_deref(),
            Some("5px")
        );
        assert_eq!(
            dom.computed_style(hero, "text-indent").as_deref(),
            Some("9px")
        );
        // plain has .box but NOT .active, so `.box.active` must not win.
        assert_eq!(
            dom.computed_style(plain, "letter-spacing").as_deref(),
            Some("2px")
        );
        // <span> has .active but lacks .box, so `.box.active` must not match it
        // even though it shares the bucket key (`box`) is irrelevant — `active`
        // is the bucket key and the second class is verified.
        assert_eq!(dom.computed_style(s, "letter-spacing"), None);
    }

    #[test]
    fn matched_rules_memo_follows_mutations() {
        // The per-element match memo is epoch-keyed: toggling a class must
        // re-match (the element gains the `.active` rule), not serve a stale
        // matched-rule list.
        let mut dom = Dom::parse_document(
            "<head><style>.active{letter-spacing:3px}</style></head>\
             <body><div id=d>x</div></body>",
        );
        let d = dom.get_by_id("d").unwrap();
        assert_eq!(dom.computed_style(d, "letter-spacing"), None);
        dom.set_attr(d, "class", "active");
        assert_eq!(
            dom.computed_style(d, "letter-spacing").as_deref(),
            Some("3px"),
            "mutation invalidates the matched-rules memo"
        );
    }

    #[test]
    fn media_queries_evaluate_against_the_viewport() {
        let vp = (800, 600); // 800x600 CSS px
        assert!(media_query_matches("(min-width: 768px)", vp));
        assert!(!media_query_matches("(min-width: 1000px)", vp));
        assert!(media_query_matches("(max-width: 1000px)", vp));
        assert!(media_query_matches("screen and (min-width: 640px)", vp));
        assert!(!media_query_matches("print", vp), "wrong medium");
        assert!(
            media_query_matches("print, (min-width: 640px)", vp),
            "comma is OR"
        );
        assert!(media_query_matches("(orientation: landscape)", vp));
        assert!(!media_query_matches("(orientation: portrait)", vp));
        assert!(media_query_matches("(min-width: 40em)", vp), "40em = 640px");
        assert!(media_query_matches("not (min-width: 1000px)", vp), "not");
        // Unknown feature, or an unknown viewport, conservatively don't match
        // (so the rules are dropped, exactly as skipping @media used to).
        assert!(!media_query_matches("(hover: hover)", vp));
        assert!(!media_query_matches("(min-width: 768px)", (0, 0)));
    }

    #[test]
    fn mutation_appends_inserts_and_detaches() {
        let mut dom = Dom::parse_document("<body><div id=root></div></body>");
        let root = dom.get_by_id("root").unwrap();
        let a = dom.create_element("p");
        let at = dom.create_text("first");
        dom.append(a, at);
        dom.append(root, a);
        let b = dom.create_element("p");
        dom.insert_before(root, b, Some(a));
        assert_eq!(dom.children(root), vec![b, a]);
        dom.detach(b);
        assert_eq!(dom.children(root), vec![a]);
        assert_eq!(dom.text_content(root), "first");
        let html = dom.serialize(DOCUMENT);
        assert!(
            html.contains("<div id=\"root\"><p>first</p></div>"),
            "{html}"
        );
    }

    #[test]
    fn attributes_set_get_remove() {
        let mut dom = Dom::parse_document("<body><a id=x href='/y'>l</a></body>");
        let a = dom.get_by_id("x").unwrap();
        assert_eq!(dom.attr(a, "href"), Some("/y"));
        assert_eq!(dom.attr(a, "HREF"), Some("/y"));
        dom.set_attr(a, "class", "big");
        assert_eq!(dom.attr(a, "class"), Some("big"));
        dom.remove_attr(a, "href");
        assert_eq!(dom.attr(a, "href"), None);
    }

    #[test]
    fn text_escaping_round_trips() {
        let mut dom = Dom::parse_document("<body><p id=t></p></body>");
        let p = dom.get_by_id("t").unwrap();
        dom.set_text(p, "a < b & \"c\"");
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("a &lt; b &amp; \"c\""), "{html}");
        // And the parser reads it back to the same text.
        let dom2 = Dom::parse_document(&html);
        let p2 = dom2.get_by_id("t").unwrap();
        assert_eq!(dom2.text_content(p2), "a < b & \"c\"");
    }

    #[test]
    fn fragment_parse_transplants_nodes() {
        let mut dom = Dom::parse_document("<body><div id=host></div></body>");
        let host = dom.get_by_id("host").unwrap();
        let nodes = dom.parse_fragment_into("div", "<p class=x>one</p>two");
        for n in &nodes {
            dom.append(host, *n);
        }
        assert_eq!(dom.text_content(host), "onetwo");
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("<p class=\"x\">one</p>two"), "{html}");
    }

    #[test]
    fn selectors_match_the_workhorse_grammar() {
        let dom = Dom::parse_document(
            "<body><div class='a b'><p id=p1 class=x>1</p><span data-k='v'>2</span></div>\
             <div><p class=x>3</p></div></body>",
        );
        let q = |s: &str| {
            let sel = SelectorList::parse(s).unwrap();
            dom.query(DOCUMENT, &sel, false).len()
        };
        assert_eq!(q("p"), 2);
        assert_eq!(q(".x"), 2);
        assert_eq!(q("#p1"), 1);
        assert_eq!(q("div.a.b p.x"), 1);
        assert_eq!(q("div > p"), 2);
        assert_eq!(q("body > p"), 0);
        assert_eq!(q("[data-k]"), 1);
        assert_eq!(q("[data-k=v]"), 1);
        assert_eq!(q("[data-k=w]"), 0);
        assert_eq!(q("p, span"), 3);
        assert_eq!(q("*"), 8); // html, head, body, div, p, span, div, p
    }

    #[test]
    fn scope_pseudo_matches_the_query_root() {
        // jQuery rewrites a context-rooted comma `.find()` to
        // `:scope X, :scope Y`. `:scope` must resolve to the element the query
        // is rooted on, or the query returns nothing — the SL Marketplace
        // tab-deselection bug (`removeClass` over `:scope .tab-header,…`).
        let dom = Dom::parse_document(
            "<body><div id=box><span class=a>1</span><span class=b>2</span>\
             <span class=a>3</span></div><span class=a>outside</span></body>",
        );
        let box_id = dom.get_by_id("box").unwrap();
        let q = |root: NodeId, s: &str| {
            let sel = SelectorList::parse(s).unwrap();
            dom.query(root, &sel, false).len()
        };
        // Rooted at #box, `:scope .a` finds the two inside, not the outsider.
        assert_eq!(q(box_id, ":scope .a"), 2, ":scope roots at #box");
        // The exact jQuery shape: a comma list of :scope-prefixed selectors.
        assert_eq!(
            q(box_id, ":scope .a, :scope .b"),
            3,
            "comma :scope list ORs"
        );
        // Inert in the cascade / scopeless match (no query root → never).
        let b = dom.query(box_id, &SelectorList::parse(".b").unwrap(), true)[0];
        assert!(!dom.matches(b, &SelectorList::parse(":scope").unwrap()));
    }

    #[test]
    fn sibling_combinators_match() {
        let dom = Dom::parse_document(
            "<body><ul><li class=a>1</li><li class=b>2</li><li class=c>3</li></ul></body>",
        );
        let q = |s: &str| {
            dom.query(DOCUMENT, &SelectorList::parse(s).unwrap(), false)
                .len()
        };
        // `.a + li` = the li immediately after .a (just one).
        assert_eq!(q(".a + li"), 1, "next-sibling matches one");
        // `.a ~ li` = every following li sibling (two).
        assert_eq!(q(".a ~ li"), 2, "subsequent-sibling matches all following");
        // `.c + li` = nothing follows .c.
        assert_eq!(q(".c + li"), 0, "no sibling after last");
    }

    #[test]
    fn structural_pseudo_classes_match() {
        let dom = Dom::parse_document(
            "<body><ul id=list>\
             <li>1</li><li>2</li><li>3</li><li>4</li><li>5</li>\
             </ul><div id=empty></div><div id=ws>   </div><div id=full>x</div></body>",
        );
        let root = DOCUMENT;
        let q = |s: &str| {
            dom.query(root, &SelectorList::parse(s).unwrap(), false)
                .len()
        };
        assert_eq!(q("li:first-child"), 1);
        assert_eq!(q("li:last-child"), 1);
        assert_eq!(q("li:only-child"), 0, "5 li children: none is only-child");
        assert_eq!(q("li:nth-child(2)"), 1);
        assert_eq!(q("li:nth-child(odd)"), 3, "1,3,5");
        assert_eq!(q("li:nth-child(even)"), 2, "2,4");
        assert_eq!(q("li:nth-child(2n+1)"), 3, "same as odd");
        assert_eq!(q("li:nth-last-child(1)"), 1, "== last-child");
        // :empty — whitespace-only counts as empty (Selectors-4); text doesn't.
        assert_eq!(q("#empty:empty"), 1);
        assert_eq!(q("#ws:empty"), 1, "whitespace-only is empty");
        assert_eq!(q("#full:empty"), 0, "text content is not empty");
    }

    #[test]
    fn of_type_pseudo_classes_match() {
        let dom = Dom::parse_document(
            "<body id=b><h1>t</h1><p>a</p><p>b</p><span>s</span><p>c</p></body>",
        );
        let b = dom.get_by_id("b").unwrap();
        let q = |s: &str| dom.query(b, &SelectorList::parse(s).unwrap(), false).len();
        assert_eq!(q("p:first-of-type"), 1, "first p");
        assert_eq!(q("p:last-of-type"), 1, "last p");
        assert_eq!(q("h1:only-of-type"), 1, "the lone h1");
        assert_eq!(q("p:only-of-type"), 0, "three p's");
        assert_eq!(q("p:nth-of-type(2)"), 1, "second p");
    }

    #[test]
    fn scripts_are_collected_in_document_order() {
        let dom = Dom::parse_document(
            "<head><script src='/a.js'></script></head>\
             <body><script>inline()</script><script type='module'>mod()</script></body>",
        );
        let scripts = dom.scripts();
        assert_eq!(scripts.len(), 3);
        assert_eq!(scripts[0].0.as_deref(), Some("/a.js"));
        assert_eq!(scripts[1].1, "inline()");
        assert_eq!(scripts[2].2.as_deref(), Some("module"));
    }

    #[test]
    fn dirty_bit_tracks_mutations_and_skips_idempotent_writes() {
        let mut dom = Dom::parse_document("<body><p id=a>x</p></body>");
        assert!(dom.take_dirty()); // parsing itself mutates
        assert!(!dom.take_dirty()); // and the take resets
        let a = dom.get_by_id("a").unwrap();
        dom.set_attr(a, "class", "y");
        assert!(dom.take_dirty());
        // Idempotent writes are free: no dirty, no redraw downstream.
        dom.set_attr(a, "class", "y");
        assert!(!dom.take_dirty());
        dom.set_text(a, "x");
        assert!(!dom.take_dirty());
        dom.set_text(a, "z");
        assert!(dom.take_dirty());
        let _ = dom.text_content(a); // reads stay clean
        let _ = dom.serialize(DOCUMENT);
        assert!(!dom.take_dirty());
    }

    #[test]
    fn serialize_live_marks_buttons_and_live_anchors() {
        let dom = Dom::parse_document(
            "<body><button id=b>Push</button>\
             <button id=icon aria-label=menu></button>\
             <button id=opts><svg class=\"svg-fa svg-fas-fa-ellipsis\"><use href=\"#fas-fa-ellipsis\"></use></svg></button>\
             <span id=dot></span>\
             <a id=plain href='/normal'>plain</a>\
             <a id=hot href='/hot'>hot</a></body>",
        );
        let b = dom.get_by_id("b").unwrap();
        let icon = dom.get_by_id("icon").unwrap();
        let opts = dom.get_by_id("opts").unwrap();
        let dot = dom.get_by_id("dot").unwrap();
        let hot = dom.get_by_id("hot").unwrap();
        let clickable = std::collections::HashSet::from([b, icon, opts, dot, hot]);
        let html = dom.serialize_live(DOCUMENT, &clickable);
        // Buttons wrapped; icon-only ones get a readable label.
        assert!(
            html.contains(&format!(
                "<a href=\"x-trust-js:{b}:\"><button id=\"b\" data-trust-node=\"{b}\">Push</button></a>"
            )),
            "{html}"
        );
        assert!(html.contains("[menu]"), "{html}");
        // A wrapped icon-only button renders the icon GLYPH as its handle (the
        // dominant web icon idiom) — the comment's ⋯ menu — not "·"/"[button]".
        // (An icon-only ANCHOR `<a><svg></a>` is glyphed by the layout instead,
        // see `icon_only_label`, since anchors aren't wrapped.)
        assert!(html.contains('⋯'), "ellipsis icon glyph: {html}");
        // An unnamed icon-only clickable (a CSS dot) gets a compact marker,
        // not the noisy "[button]".
        assert!(html.contains('·') && !html.contains("[button]"), "{html}");
        // The live anchor's href is rewritten with the original kept;
        // the plain one is untouched (the zero-overhead path).
        assert!(
            html.contains(&format!("href=\"x-trust-js:{hot}:/hot\"")),
            "{html}"
        );
        assert!(html.contains("href=\"/normal\""), "{html}");
    }

    #[test]
    fn clickable_inside_an_anchor_is_not_wrapped_in_a_nested_anchor() {
        // archive.org tiles: an info <button> nested inside the tile's own
        // <a aria-label="…">. Wrapping the button in its own x-trust-js <a>
        // makes an <a>-in-<a>; when the app re-parses this serialized output
        // for layout, html5ever's adoption agency SPLITS the outer anchor
        // into empty fragments that still carry aria-label — leaking the
        // title as two extra link lines. The nested clickable must stay
        // UN-wrapped (it inherits the surrounding anchor's link).
        let dom = Dom::parse_document(
            "<body><a id=tile href='/details/x' aria-label='Tile Title'>\
               <button id=info aria-label='info'>i</button>\
               <h3>Tile Title</h3>\
             </a></body>",
        );
        let tile = dom.get_by_id("tile").unwrap();
        let info = dom.get_by_id("info").unwrap();
        let clickable = std::collections::HashSet::from([tile, info]);
        let html = dom.serialize_live(DOCUMENT, &clickable);
        // Exactly one anchor in the output: the tile. The nested button got
        // no wrapper marker.
        assert_eq!(html.matches("<a ").count(), 1, "one anchor only: {html}");
        assert!(
            !html.contains(&format!("x-trust-js:{info}:")),
            "info button not wrapped in a nested anchor: {html}"
        );
        // The tile anchor still routes through the actor (href rewritten).
        assert!(
            html.contains(&format!("x-trust-js:{tile}:/details/x")),
            "{html}"
        );
        // The decisive check: re-parsing the serialized output keeps the
        // anchor INTACT — no adoption-agency split — so its aria-label never
        // leaks as duplicate text.
        let reparsed = Dom::parse_document(&html);
        let anchors = reparsed
            .descendants(DOCUMENT)
            .into_iter()
            .filter(|&d| reparsed.tag_name(d) == Some("a"))
            .count();
        assert_eq!(anchors, 1, "anchor survives re-parse un-split: {html}");
    }

    #[test]
    fn shadow_trees_flatten_with_slot_projection() {
        let mut dom = Dom::parse_document(
            "<body><my-card><span slot=title>Hello</span>plain text</my-card></body>",
        );
        let host = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&d| dom.tag_name(d) == Some("my-card"))
            .unwrap();
        let root = dom.attach_shadow(host);
        let nodes = dom.parse_fragment_into(
            "div",
            "<h2><slot name=title>untitled</slot></h2><p>body: <slot></slot></p><slot name=missing>fallback</slot>",
        );
        for n in nodes {
            dom.append(root, n);
        }
        let html = dom.serialize(DOCUMENT);
        // Shadow content replaces light children; slots project.
        assert!(
            html.contains("<h2><span slot=\"title\">Hello</span></h2>"),
            "{html}"
        );
        assert!(html.contains("body: plain text"), "{html}");
        // Unassigned slot falls back to its own content.
        assert!(html.contains("fallback"), "{html}");
        // The light children don't ALSO render outside their slots.
        assert_eq!(html.matches("Hello").count(), 1, "{html}");
    }

    #[test]
    fn custom_properties_resolve_through_the_cascade() {
        // A custom property defined on an ancestor inherits to a descendant and
        // resolves in its `var()` reference to the DEFINED value (not just the
        // fallback) — the lever for sites whose cell sizing rides custom props
        // (archive.org's `--infinitescrollercellminwidth`). Resolved at bake,
        // while the stylesheets are still present.
        let dom = Dom::parse_document(
            "<body><div id=root style=\"--cell: 12rem\">\
             <p id=c style=\"min-width: var(--cell, 16rem)\">x</p></div></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        let html = dom.serialize(c);
        // The resolved value is baked; it's appended after the original so the
        // re-parsed inline cascade (later-wins) uses 12rem, not the fallback.
        assert!(
            html.contains("min-width:12rem"),
            "defined --cell wins: {html}"
        );

        // A class-defined custom property (in a dropped stylesheet) resolves too.
        let dom = Dom::parse_document(
            "<body><div class=scope><p id=c style=\"min-width: var(--cell, 16rem)\">x</p></div>\
             <style>.scope{--cell:10rem}</style></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            dom.serialize(c).contains("min-width:10rem"),
            "stylesheet-defined --cell resolves"
        );

        // Defined on `:root` — the conventional home for custom properties.
        let dom = Dom::parse_document(
            "<html><head><style>:root{--cell:8rem}</style></head>\
             <body><p id=c style=\"min-width: var(--cell, 16rem)\">x</p></body></html>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            dom.serialize(c).contains("min-width:8rem"),
            ":root-defined --cell resolves"
        );
    }

    #[test]
    fn custom_property_falls_back_when_undefined() {
        let dom = Dom::parse_document(
            "<body><p id=c style=\"min-width: var(--cell, 16rem)\">x</p></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            dom.serialize(c).contains("min-width:16rem"),
            "undefined --cell uses the fallback"
        );
    }

    #[test]
    fn serialize_bakes_computed_display_into_style() {
        let dom = Dom::parse_document(
            "<html><head><style>li{display:inline}</style></head>\
             <body><ul><li>x</li></ul></body></html>",
        );
        let li = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.tag_name(id) == Some("li"))
            .unwrap();
        assert_eq!(dom.computed_display(li).as_deref(), Some("inline"));
        // The serialized HTML carries the computed display so a re-parse
        // (the layout arena) flows it the same way.
        let html = dom.serialize(DOCUMENT);
        assert!(html.contains("display:inline"), "baked display: {html}");
        // Merges into an existing inline style rather than dropping it.
        let dom = Dom::parse_document(
            r#"<body><p style="color:red" class="x">y</p><style>.x{display:inline}</style></body>"#,
        );
        let p = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.tag_name(id) == Some("p"))
            .unwrap();
        let html = dom.serialize(p);
        assert!(html.contains("color:red"), "keeps original style: {html}");
        assert!(html.contains("display:inline"), "adds display: {html}");

        // Box properties (margin shorthand → longhands) bake too, so a
        // living page's CSS spacing reaches the re-parsed layout arena.
        let dom =
            Dom::parse_document("<body><p class=x>y</p><style>.x{margin:1em 0}</style></body>");
        let p = dom
            .descendants(DOCUMENT)
            .into_iter()
            .find(|&id| dom.tag_name(id) == Some("p"))
            .unwrap();
        assert_eq!(dom.computed_style(p, "margin-top").as_deref(), Some("1em"));
        let html = dom.serialize(p);
        assert!(html.contains("margin-top:1em"), "bakes margin: {html}");
    }

    #[test]
    fn closed_dialog_is_hidden_open_one_renders() {
        // UA default `dialog:not([open]){display:none}`: a closed dialog's
        // content must not render (modal text otherwise bleeds into the
        // page), an open one does, and an author `display` rule wins.
        let dom = Dom::parse_document(
            "<body><dialog id=a>shut</dialog><dialog id=b open>shown</dialog></body>",
        );
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        assert!(dom.is_hidden(a), "closed dialog hidden");
        assert!(!dom.is_hidden(b), "open dialog renders");
        // Serialization drops the hidden one, keeps the open one.
        let html = dom.serialize(DOCUMENT);
        assert!(!html.contains("shut"), "closed dialog dropped: {html}");
        assert!(html.contains("shown"), "open dialog kept: {html}");
        // An author rule setting the dialog's display overrides the UA
        // default — a closed dialog forced visible renders.
        let dom = Dom::parse_document(
            "<body><dialog id=c>forced</dialog><style>#c{display:block}</style></body>",
        );
        let c = dom.get_by_id("c").unwrap();
        assert!(
            !dom.is_hidden(c),
            "author display:block beats the UA default"
        );
    }

    #[test]
    fn visually_hidden_sr_only_is_dropped() {
        // The universal screen-reader-only idiom (1px clipped absolutely
        // positioned box) carries text invisible to sighted users — both the
        // class form (Bootstrap/Tailwind `.sr-only`) and the inline form
        // (archive.org's `aria-describedby` targets) must be hidden + dropped,
        // while a normal sibling renders.
        let dom = Dom::parse_document(
            "<body>\
             <span id=a class=sr>screen reader only</span>\
             <span id=b style=\"position:absolute;overflow:hidden;width:1px;height:1px\">inline hidden</span>\
             <span id=c>visible</span>\
             <style>.sr{position:absolute;overflow:hidden;width:1px;height:1px;clip:rect(0,0,0,0)}</style>\
             </body>",
        );
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        let c = dom.get_by_id("c").unwrap();
        assert!(dom.is_hidden(a), "class .sr-only hidden");
        assert!(dom.is_hidden(b), "inline sr-only hidden");
        assert!(!dom.is_hidden(c), "normal content visible");
        let html = dom.serialize(DOCUMENT);
        assert!(
            !html.contains("screen reader only"),
            "class sr dropped: {html}"
        );
        assert!(!html.contains("inline hidden"), "inline sr dropped: {html}");
        assert!(html.contains("visible"), "normal kept: {html}");
        // A wider absolutely-positioned overflow-hidden box is NOT sr-only.
        let dom2 = Dom::parse_document(
            "<body><div id=d style=\"position:absolute;overflow:hidden;width:20em\">real</div></body>",
        );
        let d = dom2.get_by_id("d").unwrap();
        assert!(!dom2.is_hidden(d), "a real clipped box is not sr-only");
    }

    #[test]
    fn zero_axis_overflow_hidden_box_is_hidden_but_padding_ratio_box_renders() {
        // A box collapsed to zero on an axis with `overflow:hidden` on that
        // axis clips ALL its content — Steam's `.menu_takeover_background`
        // preload copy of the banner (`height:0;overflow:hidden`) drew a
        // full-width 1-row sliver. Hide it (and its image child).
        let dom = Dom::parse_document(
            "<body>\
             <div id=a style=\"height:0;overflow:hidden\"><img src=banner.jpg></div>\
             <div id=b style=\"max-height:0;overflow:hidden\">collapsed drawer</div>\
             <div id=c style=\"width:0;overflow-x:hidden\">narrow</div>\
             <div id=d style=\"height:0;overflow:hidden;padding-bottom:56.25%\"><img id=di src=tile.jpg></div>\
             <div id=e style=\"height:0\">no clip, not hidden</div>\
             <div id=f style=\"height:0;overflow:auto\">scrollable, not hidden</div>\
             </body>",
        );
        let g = |i| dom.get_by_id(i).unwrap();
        assert!(dom.is_hidden(g("a")), "height:0 + overflow:hidden hidden");
        assert!(
            dom.is_hidden(g("b")),
            "max-height:0 + overflow:hidden hidden"
        );
        assert!(dom.is_hidden(g("c")), "width:0 + overflow-x:hidden hidden");
        // The responsive-image intrinsic-ratio box (padding reserves height)
        // is NOT empty — its absolutely-positioned child fills the padding box.
        assert!(
            !dom.is_hidden(g("d")),
            "padding-bottom ratio box renders (responsive image idiom)"
        );
        assert!(!dom.is_hidden(g("di")), "the ratio box's image renders");
        assert!(
            !dom.is_hidden(g("e")),
            "height:0 with visible overflow is not hidden"
        );
        assert!(
            !dom.is_hidden(g("f")),
            "height:0 with overflow:auto is not hidden"
        );
        let html = dom.serialize(DOCUMENT);
        assert!(
            !html.contains("banner.jpg"),
            "hidden banner dropped: {html}"
        );
        assert!(html.contains("tile.jpg"), "ratio-box image kept: {html}");
    }

    #[test]
    fn clone_subtree_is_deep_and_detached() {
        let mut dom = Dom::parse_document("<body><div id=d><p>x</p></div></body>");
        let d = dom.get_by_id("d").unwrap();
        let copy = dom.clone_subtree(d, true);
        assert!(dom.node(copy).parent.is_none());
        assert_eq!(dom.text_content(copy), "x");
    }
}
