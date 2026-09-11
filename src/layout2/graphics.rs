//! Renderer-neutral graphical paint extraction from canonical fragments.
//!
//! CSS 2.2 Appendix E remains the ordering authority. This module turns that
//! traversal into a stateful TRust display list; it does not contain Vello,
//! framebuffer, DPI, winit, Ratatui, or terminal-cell types.

use std::collections::{HashMap, HashSet};
use std::f32::consts::{FRAC_PI_2, PI};
use std::str::FromStr as _;

use url::Url;

use crate::core::{CssPoint, CssSize};
use crate::dom::{Dom, NodeId, PseudoEl};
use crate::render::{
    Affine2d, BlendMode, CompositingLayer, CornerRadii, CssAnimationPoint, CssAnimationScope,
    CssPaintAnimation, CssRect, DecorationStyle, DisplayCommand, GradientInterpolation,
    GradientStop, HitRegion, ImageFit, ImageHandle, ImageRequest, ImageSampling, LineCap,
    MarqueeBehavior, MarqueeDirection, MarqueeScope, PagePaint, PaintBrush, PaintColor, PaintLine,
    PaintShape, PathElement, ScrollContainer, StickyConstraint, StrokeStyle, TextDecorationPaint,
    TextShadowPaint, TopLayerEntry,
};

use super::ImageSizes;
use super::NO_NODE;
use super::Units;
use super::flow::{Clip, Frag, FragKind, TopFrag};
use super::overflow::{ScrollAreas, viewport_overflow_disabled, viewport_overflow_source};
use super::style::{Outline, OutlineStyle, Pos, outline_of};
use super::value::{Len, Vp};

/// Computed-style source used by graphical box decoration. Generated boxes
/// retain the originating element plus pseudo identity because they have no
/// addressable DOM node of their own (CSS Pseudo 4 §4.1).
#[derive(Clone, Copy)]
enum PaintStyle {
    Element(NodeId),
    Pseudo(NodeId, PseudoEl),
}

impl PaintStyle {
    fn of(fragment: &Frag<'_>) -> Option<Self> {
        fragment
            .paint
            .pseudo
            .map(|(node, pseudo)| Self::Pseudo(node, pseudo))
            .or_else(|| (fragment.node != NO_NODE).then_some(Self::Element(fragment.node)))
    }

    fn node(self) -> NodeId {
        match self {
            Self::Element(node) | Self::Pseudo(node, _) => node,
        }
    }

    fn value(self, dom: &Dom, property: &str) -> Option<String> {
        match self {
            Self::Element(node) => dom.computed_value_resolved(node, property),
            Self::Pseudo(node, pseudo) => dom.pseudo_layout_value(node, pseudo, property),
        }
    }
}

#[derive(Clone, Copy)]
struct ClipAncestry {
    position: Pos,
    absolute_cb: bool,
    fixed_cb: bool,
    top_layer: bool,
}

struct Builder<'a, 't> {
    dom: &'a Dom,
    base: &'a Url,
    images: &'a ImageSizes,
    fixed: &'a [Frag<'t>],
    viewport_w: f32,
    viewport_h: f32,
    fixed_depth: usize,
    /// Finite extent used when a CSS clip is unbounded on one axis. CSS
    /// Overflow L3 §3 defines that axis as unclipped; display-list paths,
    /// unlike CSS clip edges, cannot contain ±∞, so the extent must cover the
    /// paintable document and leave the final viewport clip to the compositor.
    clip_extent: CssRect,
    commands: Vec<DisplayCommand>,
    lines: Vec<PaintLine>,
    image_requests: Vec<ImageRequest>,
    canvas_images: Vec<(NodeId, crate::render::CanvasImage)>,
    image_handles: HashSet<ImageHandle>,
    scroll_containers: Vec<ScrollContainer>,
    scrolling: ScrollAreas,
    sticky_constraints: Vec<StickyConstraint>,
    marquee_scopes: HashMap<NodeId, MarqueeScope>,
    /// CSS 2.2 `clip:rect()` regions keyed by the positioned element that
    /// establishes them. Unlike overflow clips these are not represented in
    /// the fragment geometry, so the paint adapter carries them down the flat
    /// ancestor chain for every descendant command.
    legacy_clips: HashMap<NodeId, CssRect>,
    /// Blockified replaced elements have a principal border box separate
    /// from their anonymous content line. Keep its used geometry for radii.
    replaced_border_boxes: HashMap<NodeId, CssRect>,
    /// Overflow clips round the padding edge, independently of whether the
    /// box exposes a user scrolling mechanism (hidden/clip do not).
    rounded_overflow_clips: HashMap<NodeId, PaintShape>,
    clip_ancestry: HashMap<NodeId, ClipAncestry>,
    patch_boundaries: Vec<super::GraphicalPatchBoundary>,
    boundaries: Vec<super::GraphicalBoundary>,
    /// Absolute overflow clips already active in the display list. A clip
    /// inherited from outside a transformed stacking context must be pushed
    /// before that transform and retained for the context's descendants.
    hard_clips: Vec<CssRect>,
    /// Scrollports already emitted around the current paint subtree. Scroll
    /// clipping is in the scroll container's coordinate system, so a
    /// descendant stacking-context transform must be nested inside these
    /// clips rather than wrapping them. Keeping the active prefix lets the
    /// ordinary fragment painter add only newly-entered scrollports.
    scroll_nodes: Vec<(NodeId, bool)>,
}

impl<'a, 't> Builder<'a, 't> {
    // The paint adapter's inputs are distinct borrowed engine products. A
    // parameter object would only move these references without simplifying
    // ownership or call sites.
    #[allow(clippy::too_many_arguments)]
    fn new(
        dom: &'a Dom,
        base: &'a Url,
        images: &'a ImageSizes,
        root: &Frag<'t>,
        fixed: &'a [Frag<'t>],
        top_layer: &[TopFrag<'t>],
        flow_bottom: f32,
        viewport_w: f32,
        viewport_h: f32,
    ) -> Self {
        let mut this = Self {
            dom,
            base,
            images,
            fixed,
            viewport_w,
            viewport_h,
            fixed_depth: 0,
            clip_extent: paint_extent(root, fixed, top_layer, flow_bottom),
            commands: Vec::new(),
            lines: Vec::new(),
            image_requests: Vec::new(),
            canvas_images: Vec::new(),
            image_handles: HashSet::new(),
            scroll_containers: Vec::new(),
            scrolling: ScrollAreas::new(dom, root),
            sticky_constraints: Vec::new(),
            marquee_scopes: HashMap::new(),
            legacy_clips: HashMap::new(),
            replaced_border_boxes: HashMap::new(),
            rounded_overflow_clips: HashMap::new(),
            clip_ancestry: HashMap::new(),
            patch_boundaries: Vec::new(),
            boundaries: Vec::new(),
            hard_clips: Vec::new(),
            scroll_nodes: Vec::new(),
        };
        this.collect_legacy_clips(root);
        this.collect_scroll_containers(root, false);
        this.collect_patch_boundaries(root);
        this.collect_sticky(root);
        this.collect_marquees(root);
        // Fixed-positioned fragments are retained outside the normal-flow
        // fragment tree for compositing. CSS Position 3 changes their
        // containing block and viewport attachment, but descendants still
        // establish ordinary CSS Overflow scroll containers and interaction
        // boundaries.
        for fixed in fixed {
            this.scrolling.extend(dom, fixed);
            this.collect_legacy_clips(fixed);
            this.collect_scroll_containers(fixed, true);
            this.collect_patch_boundaries(fixed);
            this.collect_sticky(fixed);
            this.collect_marquees(fixed);
        }
        for top in top_layer {
            this.scrolling.extend(dom, &top.fragment);
            this.collect_legacy_clips(&top.fragment);
            this.collect_scroll_containers(&top.fragment, top.fixed);
            this.collect_patch_boundaries(&top.fragment);
            this.collect_sticky(&top.fragment);
            this.collect_marquees(&top.fragment);
        }
        for index in 0..this.scroll_containers.len() {
            let node = this.scroll_containers[index].node;
            this.scroll_containers[index].ancestors = this.scroll_ancestor_nodes(node);
        }
        this
    }

    fn scroll_ancestor_nodes(&self, node: NodeId) -> Vec<NodeId> {
        let mut result = Vec::new();
        let mut current = Some(node);
        let mut waiting = None;
        while let Some(id) = current {
            let context = self.clip_ancestry.get(&id);
            if context.is_some_and(|c| match waiting {
                Some(Pos::Absolute) => c.absolute_cb,
                Some(Pos::Fixed) => c.fixed_cb,
                _ => false,
            }) {
                waiting = None;
            }
            if waiting.is_none() {
                if id != node && self.scroll_containers.iter().any(|c| c.node == id) {
                    result.push(id);
                }
                if let Some(context) = context {
                    if context.top_layer {
                        break;
                    }
                    if matches!(context.position, Pos::Absolute | Pos::Fixed) {
                        waiting = Some(context.position);
                    }
                }
            }
            current = self.dom.parent_flat(id);
        }
        result
    }

    /// Emit one descendant paint command inside its nearest marquee's fixed
    /// clip and sampled translation. The marquee element's own box paint uses
    /// the ordinary command path, so borders/backgrounds never move.
    fn push_marquee_content(&mut self, node: NodeId, command: DisplayCommand) {
        self.push_clipped_marquee_content(node, command, None);
    }

    fn push_clipped_marquee_content(
        &mut self,
        node: NodeId,
        command: DisplayCommand,
        clip: Option<PaintShape>,
    ) {
        let scope = self.marquee_scope(node, true);
        if let Some(scope) = scope.clone() {
            self.push_marquee_scope(scope);
        }
        let clipped = clip.is_some();
        if let Some(shape) = clip {
            self.commands.push(DisplayCommand::PushClip(shape));
        }
        self.commands.push(command);
        if clipped {
            self.commands.push(DisplayCommand::PopClip);
        }
        if scope.is_some() {
            self.pop_marquee_scope();
        }
    }

    fn marquee_scope(&self, node: NodeId, include_node: bool) -> Option<MarqueeScope> {
        let mut current = if node == NO_NODE {
            None
        } else if include_node {
            Some(node)
        } else {
            self.dom.parent_flat(node)
        };
        while let Some(id) = current {
            if let Some(scope) = self.marquee_scopes.get(&id) {
                return Some(scope.clone());
            }
            current = self.dom.parent_flat(id);
        }
        None
    }

    fn push_marquee_scope(&mut self, scope: MarqueeScope) {
        self.commands
            .push(DisplayCommand::PushClip(PaintShape::Rect(scope.viewport)));
        self.commands.push(DisplayCommand::BeginMarquee(scope));
    }

    fn pop_marquee_scope(&mut self) {
        self.commands.push(DisplayCommand::EndMarquee);
        self.commands.push(DisplayCommand::PopClip);
    }

    fn image(&mut self, source: String) -> ImageHandle {
        let handle = ImageHandle::for_source(&source);
        if self.image_handles.insert(handle) {
            self.image_requests.push(ImageRequest { handle, source });
        }
        handle
    }

    fn replaced_image(&mut self, node: NodeId, source: Option<&String>) -> Option<ImageHandle> {
        if node != NO_NODE && self.dom.canvas_size(node).is_some() {
            let canvas = self.dom.canvas_image(node)?;
            let handle = canvas.handle;
            if self.image_handles.insert(handle) {
                self.canvas_images.push((node, canvas));
            }
            Some(handle)
        } else {
            source.map(|source| self.image(resolve_image_source(self.base, source)))
        }
    }

    fn effective_clip(&self, node: NodeId, hard: Option<Clip>) -> Option<CssRect> {
        self.clip_chain(node, hard)
    }

    fn ancestor_clip(&self, node: NodeId, hard: Option<Clip>) -> Option<CssRect> {
        self.clip_chain(node, hard)
    }

    fn clip_chain(&self, node: NodeId, hard: Option<Clip>) -> Option<CssRect> {
        let mut clip = hard.map(|clip| self.clip_rect(clip));
        let mut current = (node != NO_NODE).then_some(node);
        while let Some(id) = current {
            if let Some(rect) = self.legacy_clips.get(&id) {
                clip = intersect_css_rects(clip, *rect);
            }
            current = self.dom.parent_flat(id);
        }
        clip
    }

    fn collect_legacy_clips(&mut self, fragment: &Frag<'_>) {
        if fragment.node != NO_NODE
            && matches!(fragment.kind, FragKind::Block | FragKind::TableCell(_))
        {
            let node = fragment.node;
            if fragment.paint.cb_abs || fragment.paint.cb_fixed || self.dom.is_popover_showing(node)
            {
                self.clip_ancestry.insert(
                    node,
                    ClipAncestry {
                        position: Pos::of(self.dom, node),
                        absolute_cb: fragment.paint.cb_abs,
                        fixed_cb: fragment.paint.cb_fixed,
                        top_layer: self.dom.is_popover_showing(node),
                    },
                );
            }
            if let Some(shape) = rounded_overflow_clip(self.dom, fragment) {
                self.rounded_overflow_clips.insert(node, shape);
            }
        }
        if fragment.node != NO_NODE
            && matches!(fragment.kind, FragKind::Block)
            && matches!(
                self.dom.tag_name(fragment.node),
                Some("img" | "svg" | "canvas" | "video")
            )
        {
            self.replaced_border_boxes.insert(
                fragment.node,
                CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
            );
        }
        if fragment.node != NO_NODE
            && matches!(fragment.kind, FragKind::Block | FragKind::TableCell(_))
            && let Some(rect) = legacy_clip_rect(
                self.dom,
                fragment.node,
                CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
            )
        {
            self.legacy_clips.insert(fragment.node, rect);
        }
        for child in &fragment.children {
            self.collect_legacy_clips(child);
        }
    }

    fn clip_rect(&self, clip: Clip) -> CssRect {
        let extent_right = self.clip_extent.x + self.clip_extent.width;
        let extent_bottom = self.clip_extent.y + self.clip_extent.height;
        let x0 = if clip.x0.is_finite() {
            clip.x0
        } else {
            self.clip_extent.x
        };
        let y0 = if clip.y0.is_finite() {
            clip.y0
        } else {
            self.clip_extent.y
        };
        let x1 = if clip.x1.is_finite() {
            clip.x1
        } else {
            extent_right
        };
        let y1 = if clip.y1.is_finite() {
            clip.y1
        } else {
            extent_bottom
        };
        CssRect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
    }

    fn push_hard_clip(&mut self, clip: CssRect) -> bool {
        if self.hard_clips.last() == Some(&clip) {
            return false;
        }
        self.commands
            .push(DisplayCommand::PushClip(PaintShape::Rect(clip)));
        self.hard_clips.push(clip);
        true
    }

    fn pop_hard_clip(&mut self) {
        self.commands.push(DisplayCommand::PopClip);
        self.hard_clips.pop();
    }

    fn push_scroll_ancestors(&mut self, node: NodeId) -> usize {
        self.push_scroll_chain(node, false)
    }

    fn push_scroll_content_chain(&mut self, node: NodeId) -> usize {
        self.push_scroll_chain(node, true)
    }

    fn push_scroll_chain(&mut self, node: NodeId, include_node: bool) -> usize {
        if node == NO_NODE {
            return 0;
        }
        let mut chain = Vec::new();
        let mut waiting_for_cb = if include_node {
            None
        } else {
            self.clip_ancestry
                .get(&node)
                .and_then(|context| match context.position {
                    Pos::Absolute | Pos::Fixed => Some(context.position),
                    _ => None,
                })
        };
        if self
            .clip_ancestry
            .get(&node)
            .is_some_and(|context| context.top_layer)
            && !include_node
        {
            return 0;
        }
        // CSS Overflow 3 §2.3 clips the contents of a scroll container to
        // its scrollport.  A shadow tree is attached to the light tree through
        // its host (DOM §4.2.2), so paint ancestry must cross that boundary:
        // otherwise a custom element's host box is clipped but the image/text
        // painted by its shadow tree escapes the same scrollport.
        let mut current = if include_node {
            Some(node)
        } else {
            self.dom.parent_flat(node)
        };
        while let Some(id) = current {
            let context = self.clip_ancestry.get(&id);
            if context.is_some_and(|context| match waiting_for_cb {
                Some(Pos::Absolute) => context.absolute_cb,
                Some(Pos::Fixed) => context.fixed_cb,
                _ => false,
            }) {
                waiting_for_cb = None;
            }
            if waiting_for_cb.is_none() && !matches!(self.dom.tag_name(id), Some("html" | "body")) {
                let container = self
                    .scroll_containers
                    .iter()
                    .find(|container| container.node == id);
                let shape =
                    self.rounded_overflow_clips.get(&id).cloned().or_else(|| {
                        container.map(|container| PaintShape::Rect(container.viewport))
                    });
                if let Some(shape) = shape {
                    chain.push((id, shape, container.is_some()));
                }
                // CSS Overflow 3 §2 and Position 3 §2.1: clipping follows
                // the containing-block chain. A positioned descendant skips
                // intervening boxes that do not establish its containing block.
                if let Some(context) = context {
                    if context.top_layer {
                        break;
                    }
                    if matches!(context.position, Pos::Absolute | Pos::Fixed) {
                        waiting_for_cb = Some(context.position);
                    }
                }
            }
            current = self.dom.parent_flat(id);
        }
        chain.reverse();
        let common = self
            .scroll_nodes
            .iter()
            .zip(&chain)
            .take_while(|(active, requested)| active.0 == requested.0)
            .count();
        // Paint traversal is properly nested: a caller can only request the
        // active ancestor chain or extend it. If a future paint path violates
        // that invariant, retain the complete requested chain instead of
        // dropping an outer clip; the normal pop below restores the active
        // prefix after the nested scope.
        let common = if common == self.scroll_nodes.len() {
            common
        } else {
            0
        };
        for (node, shape, scroll) in &chain[common..] {
            self.commands.push(DisplayCommand::PushClip(shape.clone()));
            if *scroll {
                self.commands.push(DisplayCommand::BeginScroll(*node));
            }
        }
        self.scroll_nodes.extend(
            chain[common..]
                .iter()
                .map(|(node, _, scroll)| (*node, *scroll)),
        );
        chain.len() - common
    }

    fn pop_scroll_ancestors(&mut self, count: usize) {
        for _ in 0..count {
            if self.scroll_nodes.pop().is_some_and(|(_, scroll)| scroll) {
                self.commands.push(DisplayCommand::EndScroll);
            }
            self.commands.push(DisplayCommand::PopClip);
        }
    }

    fn collect_scroll_containers(&mut self, fragment: &Frag<'_>, fixed: bool) {
        let nested_viewport = fragment.node != NO_NODE
            && matches!(self.dom.tag_name(fragment.node), Some("iframe" | "frame"));
        if fragment.node != NO_NODE
            && Some(fragment.node) != viewport_overflow_source(self.dom)
            && self.dom.document_element() != Some(fragment.node)
            && (self.dom.is_scroll_container(fragment.node)
                || self.dom.is_hscroll_container(fragment.node)
                || nested_viewport)
        {
            let viewport = if nested_viewport {
                fragment.content_box()
            } else {
                padding_box(fragment)
            };
            let extent = self.scrolling.nodes.get(&fragment.node);
            let content = CssSize::new(
                extent
                    .map_or(viewport.width, |r| r.width as f32)
                    .max(viewport.width),
                extent
                    .map_or(viewport.height, |r| r.height as f32)
                    .max(viewport.height),
            );
            self.scroll_containers.push(ScrollContainer {
                node: fragment.node,
                actor: if self.dom.render_live() {
                    Some(fragment.node)
                } else {
                    self.dom
                        .attr(fragment.node, "data-trust-node")
                        .and_then(|value| value.parse().ok())
                },
                viewport,
                content,
                offset: CssPoint::new(
                    self.dom.scroll_metric(fragment.node, 1).unwrap_or(0.0) as f32,
                    self.dom.scroll_metric(fragment.node, 0).unwrap_or(0.0) as f32,
                ),
                // An iframe is itself a nested viewport. HTML's default
                // scrolling behavior supplies scroll mechanisms only on axes
                // whose child document overflows that viewport; authored CSS
                // scroll containers retain their explicit axis eligibility.
                horizontal: self.dom.is_hscroll_container(fragment.node)
                    || (nested_viewport && content.width > viewport.width),
                vertical: self.dom.is_scroll_container(fragment.node)
                    || (nested_viewport && content.height > viewport.height),
                ancestors: Vec::new(),
                fixed,
                contain_overscroll: ["overscroll-behavior-x", "overscroll-behavior-y"].map(
                    |property| {
                        self.dom
                            .computed_value_resolved(fragment.node, property)
                            .is_some_and(|v| matches!(v.trim(), "contain" | "none"))
                    },
                ),
            });
        }
        for child in &fragment.children {
            self.collect_scroll_containers(child, fixed);
        }
    }

    fn collect_patch_boundaries(&mut self, fragment: &Frag<'_>) {
        if fragment.node != NO_NODE
            && !self.dom.is_scroll_container(fragment.node)
            && !self.dom.is_hscroll_container(fragment.node)
            && self
                .dom
                .establishes_independent_formatting_context(fragment.node)
            && let Some(actor) = if self.dom.render_live() {
                Some(fragment.node)
            } else {
                self.dom
                    .attr(fragment.node, "data-trust-node")
                    .and_then(|value| value.parse().ok())
            }
        {
            self.patch_boundaries.push(super::GraphicalPatchBoundary {
                actor,
                node: fragment.node,
            });
        }
        for child in &fragment.children {
            self.collect_patch_boundaries(child);
        }
    }

    fn collect_sticky(&mut self, fragment: &Frag<'t>) {
        self.collect_sticky_in(fragment, &mut Vec::new());
    }

    fn collect_sticky_in<'f>(&mut self, fragment: &'f Frag<'t>, ancestors: &mut Vec<&'f Frag<'t>>) {
        if fragment.node != NO_NODE
            && matches!(
                self.dom
                    .computed_value_resolved(fragment.node, "position")
                    .as_deref(),
                Some("sticky" | "-webkit-sticky")
            )
        {
            let mut parent = self.dom.parent_flat(fragment.node);
            let mut container = None;
            while let Some(node) = parent {
                if self.dom.is_scroll_container(node) || self.dom.is_hscroll_container(node) {
                    container = Some(node);
                    break;
                }
                parent = self.dom.parent_flat(node);
            }
            let vp = Vp {
                w: self.viewport_w,
                h: self.viewport_h,
            };
            let scrollport = container
                .and_then(|node| self.scroll_containers.iter().find(|s| s.node == node))
                .map(|s| s.viewport)
                .unwrap_or(CssRect::new(0., 0., vp.w, vp.h));
            let units = Units::of(self.dom, fragment.node);
            // Static/relative/sticky boxes use the formatting-context content
            // box, not the nearest scrollport, as their containing block.
            let mut blocks = ancestors
                .iter()
                .rev()
                .copied()
                .filter(|f| matches!(f.kind, FragKind::Block | FragKind::TableCell(_)));
            let cb = if let Some(parent) = blocks.next() {
                let basis = blocks
                    .next()
                    .map(|f| f.content_size.map_or(f.w, |s| s[0]))
                    .unwrap_or(vp.w);
                let padding = padding_box_with_style(parent);
                let pad = PaintStyle::of(parent)
                    .map(|style| {
                        let units = Units::of(self.dom, style.node());
                        ["top", "right", "bottom", "left"].map(|side| {
                            style
                                .value(self.dom, &format!("padding-{side}"))
                                .as_deref()
                                .and_then(|s| Len::parse(s, units, vp))
                                .and_then(|n| n.resolve(Some(basis)))
                                .unwrap_or(0.)
                                .max(0.)
                        })
                    })
                    .unwrap_or([0.; 4]);
                let size = parent.content_size.unwrap_or([
                    (padding.width - pad[1] - pad[3]).max(0.),
                    (padding.height - pad[0] - pad[2]).max(0.),
                ]);
                CssRect::new(padding.x + pad[3], padding.y + pad[0], size[0], size[1])
            } else {
                CssRect::new(0., 0., vp.w, vp.h)
            };
            let margin = ["top", "right", "bottom", "left"].map(|side| {
                self.dom
                    .computed_value_resolved(fragment.node, &format!("margin-{side}"))
                    .as_deref()
                    .and_then(|s| Len::parse(s, units, vp))
                    .and_then(|n| n.resolve(Some(cb.width)))
                    .unwrap_or(0.)
            });
            let distance = [
                fragment.y - cb.y,
                cb.x + cb.width - fragment.x - fragment.w,
                cb.y + cb.height - fragment.y - fragment.h,
                fragment.x - cb.x,
            ];
            let position_margin: [f32; 4] =
                std::array::from_fn(|i| margin[i].min(distance[i] - margin[i]));
            let vertical = self
                .dom
                .computed_value_resolved(fragment.node, "writing-mode")
                .unwrap_or_default();
            let rtl = self
                .dom
                .computed_value_resolved(fragment.node, "direction")
                .as_deref()
                == Some("rtl");
            self.sticky_constraints.push(StickyConstraint {
                node: fragment.node,
                rect: CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
                container,
                movement: [
                    position_margin[0] - distance[0],
                    distance[1] - position_margin[1],
                    distance[2] - position_margin[2],
                    position_margin[3] - distance[3],
                ],
                reverse: [
                    if vertical.starts_with("vertical") {
                        vertical == "vertical-rl"
                    } else {
                        rtl
                    },
                    vertical.starts_with("vertical") && rtl,
                ],
                // CSS Position 3 §3.4: insets are lengths/percentages relative
                // to the scrollport, not a px-only decoration shorthand.
                insets: ["top", "right", "bottom", "left"].map(|side| {
                    self.dom
                        .computed_value_resolved(fragment.node, side)
                        .as_deref()
                        .and_then(|s| Len::parse(s, units, vp))
                        .and_then(|n| {
                            n.resolve(Some(if matches!(side, "top" | "bottom") {
                                scrollport.height
                            } else {
                                scrollport.width
                            }))
                        })
                }),
            });
        }
        ancestors.push(fragment);
        for child in &fragment.children {
            self.collect_sticky_in(child, ancestors);
        }
        ancestors.pop();
    }

    fn collect_marquees(&mut self, fragment: &Frag<'_>) {
        if fragment.node != NO_NODE && self.dom.tag_name(fragment.node) == Some("marquee") {
            let viewport = padding_box(fragment);
            let content = marquee_content_bounds(fragment).unwrap_or(viewport);
            let behavior = match self
                .dom
                .attr(fragment.node, "behavior")
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("slide") => MarqueeBehavior::Slide,
                Some("alternate") => MarqueeBehavior::Alternate,
                _ => MarqueeBehavior::Scroll,
            };
            let direction = match self
                .dom
                .attr(fragment.node, "direction")
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("right") => MarqueeDirection::Right,
                Some("up") => MarqueeDirection::Up,
                Some("down") => MarqueeDirection::Down,
                _ => MarqueeDirection::Left,
            };
            let mut delay_ms = self
                .dom
                .attr(fragment.node, "scrolldelay")
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(85);
            if self.dom.attr(fragment.node, "truespeed").is_none() && delay_ms < 60 {
                delay_ms = 60;
            }
            let scroll_distance = self
                .dom
                .attr(fragment.node, "scrollamount")
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(6) as f32;
            let loop_count = self
                .dom
                .attr(fragment.node, "loop")
                .and_then(|value| value.trim().parse::<i64>().ok())
                .filter(|count| *count >= 1)
                .and_then(|count| u32::try_from(count).ok());
            self.marquee_scopes.insert(
                fragment.node,
                MarqueeScope {
                    viewport,
                    content,
                    behavior,
                    direction,
                    scroll_interval_seconds: delay_ms as f32 / 1_000.0,
                    scroll_distance,
                    loop_count,
                    running: self
                        .dom
                        .attr(fragment.node, "data-trust-marquee-stopped")
                        .is_none(),
                    paused_at_seconds: self
                        .dom
                        .attr(fragment.node, "data-trust-marquee-stopped")
                        .and_then(|value| value.parse::<f32>().ok())
                        .filter(|value| value.is_finite() && *value >= 0.0),
                    paused_total_seconds: self
                        .dom
                        .attr(fragment.node, "data-trust-marquee-paused-total")
                        .and_then(|value| value.parse::<f32>().ok())
                        .filter(|value| value.is_finite() && *value >= 0.0)
                        .unwrap_or(0.0),
                },
            );
        }
        for child in &fragment.children {
            self.collect_marquees(child);
        }
    }
}

fn marquee_content_bounds(fragment: &Frag<'_>) -> Option<CssRect> {
    fn union(a: CssRect, b: CssRect) -> CssRect {
        let left = a.x.min(b.x);
        let top = a.y.min(b.y);
        let right = (a.x + a.width).max(b.x + b.width);
        let bottom = (a.y + a.height).max(b.y + b.height);
        CssRect::new(left, top, right - left, bottom - top)
    }
    fn collect(fragment: &Frag<'_>, bounds: &mut Option<CssRect>) {
        let rect = CssRect::new(
            fragment.x,
            fragment.y,
            fragment.w.max(0.0),
            fragment.h.max(0.0),
        );
        *bounds = Some(bounds.map_or(rect, |old| union(old, rect)));
        for child in &fragment.children {
            collect(child, bounds);
        }
    }
    let mut bounds = None;
    for child in &fragment.children {
        collect(child, &mut bounds);
    }
    bounds
}

#[allow(clippy::too_many_arguments)]
pub(super) fn paint<'t>(
    dom: &Dom,
    base: &Url,
    images: &ImageSizes,
    root: &Frag<'t>,
    fixed: &'_ [Frag<'t>],
    top_layer: &[TopFrag<'t>],
    flow_bottom: f32,
    viewport_w: f32,
    viewport_h: f32,
) -> (
    PagePaint,
    Vec<super::GraphicalPatchBoundary>,
    Vec<super::GraphicalBoundary>,
) {
    let mut builder = Builder::new(
        dom,
        base,
        images,
        root,
        fixed,
        top_layer,
        flow_bottom,
        viewport_w,
        viewport_h,
    );
    // CSS Backgrounds 3 §§2.11.1–2: the root background becomes the canvas
    // background. For HTML, when the root has its initial transparent/none
    // background, the first BODY child's computed background is propagated to
    // the canvas instead. Its image positioning area remains the root box,
    // while its painting area is the complete canvas, including the margins
    // around a centered body and any viewport space below the document.
    let root_background = if root.node != NO_NODE && dom.document_element() == Some(root.node) {
        let canvas = CssRect::new(
            0.0,
            0.0,
            viewport_w.max(root.x + root.w).max(1.0),
            viewport_h.max(flow_bottom).max(root.max_bottom()).max(1.0),
        );
        let style_node = canvas_background_node(dom, root.node);
        paint_background_images_for_style(
            root,
            PaintStyle::Element(style_node),
            PaintShape::Rect(canvas),
            &mut builder,
            Some(canvas),
            None,
        );
        background_color(dom, style_node).filter(|color| !color.is_transparent())
    } else {
        None
    };
    build_sc(root, &mut builder);
    // v1 graphical patch segments cover document-flow primitives. Fixed-layer
    // ranges live in separate vectors and need a layer discriminator before
    // they can participate; do not publish ambiguous offsets.
    let boundaries = std::mem::take(&mut builder.boundaries);
    let mut patch_boundaries = std::mem::take(&mut builder.patch_boundaries);
    patch_boundaries.sort_unstable_by_key(|boundary| (boundary.actor, boundary.node));
    patch_boundaries.dedup_by_key(|boundary| boundary.actor);
    let primitives = std::mem::take(&mut builder.commands);
    let mut fixed: Vec<_> = fixed.iter().collect();
    fixed.sort_by_key(|fragment| fragment.paint.z.unwrap_or(0));
    let mut fixed_under_primitives = Vec::new();
    let mut fixed_primitives = Vec::new();
    for fragment in fixed {
        let start = builder.commands.len();
        build_sc(fragment, &mut builder);
        let commands = builder.commands.split_off(start);
        if super::flow::fixed_backdrop(dom, fragment, viewport_w, viewport_h) {
            fixed_under_primitives.extend(commands);
        } else {
            fixed_primitives.extend(commands);
        }
    }
    // CSS Positioned Layout 4 §3: each top-layer entry paints as its own
    // stacking context after the document, in ordered-set order. Its fragment
    // was laid against the ICB and carries no DOM-ancestor clipping.
    let mut top_layer_entries = Vec::new();
    for top in top_layer {
        let start = builder.commands.len();
        build_sc(&top.fragment, &mut builder);
        top_layer_entries.push(TopLayerEntry {
            fixed: top.fixed,
            primitives: builder.commands.split_off(start),
        });
    }
    let (width, height) = builder.scrolling.document;
    let width = width.max(viewport_w).max(0.);
    let height = height.max(flow_bottom).max(viewport_h).max(0.);
    let locked = viewport_overflow_disabled(dom);
    let paint = PagePaint {
        width,
        height,
        user_scroll_size: Some(CssSize::new(
            if locked[0] { viewport_w } else { width },
            if locked[1] { viewport_h } else { height },
        )),
        background: root_background,
        lines: builder.lines,
        primitives,
        fixed_under_primitives,
        fixed_primitives,
        fixed_interleaved: true,
        top_layer: top_layer_entries,
        image_requests: builder.image_requests,
        canvas_images: builder.canvas_images,
        scroll_containers: builder.scroll_containers,
        sticky_constraints: builder.sticky_constraints,
    };
    (paint, patch_boundaries, boundaries)
}

/// CSS 2.2 Appendix E order for one real stacking context. Opacity and
/// transforms wrap the context atomically, as required by CSS Color and CSS
/// Transforms; children never observe a renderer-specific layer object.
fn build_sc(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) {
    let boundary_start = builder.commands.len();
    let boundary_line_start = builder.lines.len();
    let boundary = graphical_boundary(fragment, builder);
    // CSS Overflow 3 §2.3 and CSS Transforms 1 §3: a scrollport is the
    // viewport through which a transformed descendant is seen. Emit the
    // ancestor scrollport before this stacking context's transform, so the
    // scrollport remains fixed in its own coordinate system instead of being
    // scaled/translated along with the page layer.
    let scroll_depth = builder.push_scroll_ancestors(fragment.node);
    let sticky = builder
        .sticky_constraints
        .iter()
        .find(|constraint| constraint.node == fragment.node)
        .cloned();
    if let Some(constraint) = &sticky {
        builder
            .commands
            .push(DisplayCommand::BeginSticky(constraint.clone()));
    }
    let animation = paint_animation_scope(fragment, builder);
    if let Some(animation) = &animation {
        builder
            .commands
            .push(DisplayCommand::BeginCssAnimation(animation.clone()));
    }
    let transform = paint_transform(fragment, builder);
    // CSS Overflow 3 §3.1 requires hidden overflow to clip descendants,
    // while CSS Transforms 1 §2 paints a transformed element's layer in its
    // parent stacking context. `Frag::clip` is already in absolute parent
    // coordinates, so establish it before the child's transform; pushing it
    // afterwards would transform the ancestor clip a second time.
    let context_clip = transform
        .and_then(|_| builder.ancestor_clip(fragment.node, fragment.clip))
        .is_some_and(|clip| builder.push_hard_clip(clip));
    let transformed = if let Some(transform) = transform {
        builder
            .commands
            .push(DisplayCommand::PushTransform(transform));
        true
    } else {
        false
    };
    // CSS Masking 1 §5: clip the complete stacking context, including its
    // own background, scrolling contents, and hit regions. Keep zero-area
    // clips active; they are how closed drawers suppress all their paint.
    let shape_clip = PaintStyle::of(fragment)
        .and_then(|style| {
            style.value(builder.dom, "clip-path").and_then(|value| {
                super::clip_path::ClipPath::parse(
                    &value,
                    Units::of(builder.dom, style.node()),
                    super::value::Vp {
                        w: builder.viewport_w,
                        h: builder.viewport_h,
                    },
                )
            })
        })
        .and_then(|inset| inset.shape_for(fragment));
    if let Some(shape) = &shape_clip {
        builder
            .commands
            .push(DisplayCommand::PushClip(shape.clone()));
    }
    let layered = push_layer(fragment, builder);
    paint_fragment(fragment, builder);
    let mut negative = Vec::new();
    let mut zero = Vec::new();
    let mut positive = Vec::new();
    collect_positioned(
        fragment,
        builder.fixed,
        &mut negative,
        &mut zero,
        &mut positive,
    );
    negative.sort_by_key(|child| positioned_z(child, builder.fixed));
    positive.sort_by_key(|child| positioned_z(child, builder.fixed));
    for child in negative {
        build_positioned(child, builder, true);
    }
    inflow_backgrounds(fragment, builder);
    paint_floats(fragment, builder);
    inflow_content(fragment, builder);
    for (child, real_context) in zero {
        match child {
            PositionedChild::Fragment(child) if real_context => build_sc(child, builder),
            PositionedChild::Fragment(child) => build_pseudo(child, builder),
            PositionedChild::Fixed(index) => build_fixed(index, builder),
        }
    }
    for child in positive {
        build_positioned(child, builder, true);
    }
    if layered {
        builder.commands.push(DisplayCommand::PopLayer);
    }
    if shape_clip.is_some() {
        builder.commands.push(DisplayCommand::PopClip);
    }
    if transformed {
        builder.commands.push(DisplayCommand::PopTransform);
    }
    if context_clip {
        builder.pop_hard_clip();
    }
    if animation.is_some() {
        builder.commands.push(DisplayCommand::EndCssAnimation);
    }
    if sticky.is_some() {
        builder.commands.push(DisplayCommand::EndSticky);
    }
    builder.pop_scroll_ancestors(scroll_depth);
    if let Some((actor, node, rect)) = boundary {
        builder.boundaries.push(super::GraphicalBoundary {
            actor,
            node,
            rect,
            commands: boundary_start..builder.commands.len(),
            lines: boundary_line_start..builder.lines.len(),
        });
    }
}

/// Resolve the CSS Animations keyframe values supported by the graphical
/// display list into translation tracks. CSS Animations 1 §3 samples the
/// animated value without relaying out the document; the stable fragment
/// geometry therefore remains the underlying value and each point stores only
/// its delta from that value. Percentages on `top` use the fixed-position
/// containing block (CSS Positioned Layout 3 §3.5), while transform
/// percentages use the element's own border box (CSS Transforms 1 §9).
fn paint_animation_scope(
    fragment: &Frag<'_>,
    builder: &Builder<'_, '_>,
) -> Option<CssAnimationScope> {
    if fragment.node == NO_NODE || fragment.paint.pseudo.is_some() {
        return None;
    }
    let definitions = builder.dom.css_animation_definitions(fragment.node);
    if definitions.is_empty() {
        return None;
    }
    let units = Units::of(builder.dom, fragment.node);
    let viewport = Vp {
        w: builder.viewport_w,
        h: builder.viewport_h,
    };
    let underlying_top = builder
        .dom
        .computed_value_resolved(fragment.node, "top")
        .as_deref()
        .and_then(|value| Len::parse(value, units, viewport))
        .and_then(|value| value.resolve(Some(builder.viewport_h)))
        .unwrap_or(0.0);
    let mut animations = Vec::new();
    for definition in definitions {
        let mut position = definition
            .keyframes
            .iter()
            .filter_map(|frame| {
                let value = frame
                    .top
                    .as_deref()
                    .and_then(|value| Len::parse(value, units, viewport))
                    .and_then(|value| value.resolve(Some(builder.viewport_h)))?;
                Some(CssAnimationPoint {
                    offset: frame.offset,
                    value: CssPoint::new(0.0, value - underlying_top),
                })
            })
            .collect::<Vec<_>>();
        let mut transform = definition
            .keyframes
            .iter()
            .filter_map(|frame| {
                let value = animation_transform_translation(
                    frame.transform.as_deref()?,
                    fragment.w,
                    fragment.h,
                )?;
                Some(CssAnimationPoint {
                    offset: frame.offset,
                    value,
                })
            })
            .collect::<Vec<_>>();
        complete_animation_track(&mut position);
        complete_animation_track(&mut transform);
        if position.is_empty() && transform.is_empty() {
            continue;
        }
        animations.push(CssPaintAnimation {
            name: definition.name,
            duration_seconds: definition.duration_seconds,
            delay_seconds: definition.delay_seconds,
            iteration_count: definition.iteration_count,
            direction: definition.direction,
            fill_mode: definition.fill_mode,
            timing_function: definition.timing_function,
            running: definition.running,
            position,
            transform,
        });
    }
    (!animations.is_empty()).then_some(CssAnimationScope { animations })
}

/// CSS Animations 1 §3.3 synthesizes missing 0%/100% values from the
/// underlying style. The graphical subset represents that style as a zero
/// delta, so adding endpoints here also makes interpolation well-defined.
fn complete_animation_track(track: &mut Vec<CssAnimationPoint>) {
    if track.is_empty() {
        return;
    }
    track.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    if track.first().is_some_and(|point| point.offset > 0.0) {
        track.insert(
            0,
            CssAnimationPoint {
                offset: 0.0,
                value: CssPoint::default(),
            },
        );
    }
    if track.last().is_some_and(|point| point.offset < 1.0) {
        track.push(CssAnimationPoint {
            offset: 1.0,
            value: CssPoint::default(),
        });
    }
}

fn animation_transform_translation(value: &str, width: f32, height: f32) -> Option<CssPoint> {
    if value.trim().eq_ignore_ascii_case("none") {
        return Some(CssPoint::default());
    }
    let mut result = CssPoint::default();
    for (name, args) in transform_functions(value)? {
        match name.as_str() {
            "translate" => {
                result.x += transform_length(args.first()?, width)?;
                result.y += transform_length(args.get(1).map_or("0", String::as_str), height)?;
            }
            "translatex" => result.x += transform_length(args.first()?, width)?,
            "translatey" => result.y += transform_length(args.first()?, height)?,
            "translate3d" => {
                result.x += transform_length(args.first()?, width)?;
                result.y += transform_length(args.get(1)?, height)?;
            }
            "matrix" if args.len() == 6 => {
                result.x += args.get(4)?.parse::<f32>().ok()?;
                result.y += args.get(5)?.parse::<f32>().ok()?;
            }
            // A transform animation whose matrix component cannot yet be
            // represented by a translation is left to the static underlying
            // transform rather than being approximated incorrectly.
            _ => return None,
        }
    }
    Some(result)
}

enum PositionedChild<'f, 't> {
    Fragment(&'f Frag<'t>),
    Fixed(usize),
}

fn positioned_z(child: &PositionedChild<'_, '_>, fixed: &[Frag<'_>]) -> i32 {
    match child {
        PositionedChild::Fragment(fragment) => fragment.paint.z.unwrap_or(0),
        PositionedChild::Fixed(index) => fixed
            .get(*index)
            .and_then(|fragment| fragment.paint.z)
            .unwrap_or(0),
    }
}

fn build_positioned(
    child: PositionedChild<'_, '_>,
    builder: &mut Builder<'_, '_>,
    _real_context: bool,
) {
    match child {
        PositionedChild::Fragment(fragment) => build_sc(fragment, builder),
        PositionedChild::Fixed(index) => build_fixed(index, builder),
    }
}

fn build_fixed(index: usize, builder: &mut Builder<'_, '_>) {
    let Some(fragment) = builder.fixed.get(index) else {
        return;
    };
    // The full-viewport, auto-z backdrop remains in the dedicated underlay;
    // its marker is intentionally silent in the document stream.
    if super::flow::fixed_backdrop(
        builder.dom,
        fragment,
        builder.viewport_w,
        builder.viewport_h,
    ) {
        return;
    }
    let outer = builder.fixed_depth == 0;
    if outer {
        builder.commands.push(DisplayCommand::BeginFixed);
    }
    builder.fixed_depth += 1;
    build_sc(fragment, builder);
    builder.fixed_depth -= 1;
    if outer {
        builder.commands.push(DisplayCommand::EndFixed);
    }
}

/// A graphical patch range must be atomic in CSS 2.2 Appendix-E order and its
/// interior layout must not affect outside layout. A real stacking context is
/// atomic for paint; an independent formatting context provides the layout
/// boundary. Anything less stays on the full-layout fallback.
fn graphical_boundary(
    fragment: &Frag<'_>,
    builder: &Builder<'_, '_>,
) -> Option<(usize, NodeId, CssRect)> {
    if fragment.node == NO_NODE
        || !fragment.paint.sc
        || !builder
            .dom
            .establishes_independent_formatting_context(fragment.node)
        || builder.dom.is_scroll_container(fragment.node)
        || builder.dom.is_hscroll_container(fragment.node)
    {
        return None;
    }
    let actor = if builder.dom.render_live() {
        fragment.node
    } else {
        builder
            .dom
            .attr(fragment.node, "data-trust-node")?
            .parse()
            .ok()?
    };
    Some((
        actor,
        fragment.node,
        CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
    ))
}

fn build_pseudo(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) {
    paint_fragment(fragment, builder);
    inflow_backgrounds(fragment, builder);
    paint_floats(fragment, builder);
    inflow_content(fragment, builder);
}

fn inflow_backgrounds(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) {
    for child in &fragment.children {
        if child.paint.sc || child.paint.positioned || child.paint.float {
            continue;
        }
        if matches!(child.kind, FragKind::Block | FragKind::TableCell(_)) {
            paint_fragment(child, builder);
        }
        inflow_backgrounds(child, builder);
    }
}

fn inflow_content(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) {
    for child in &fragment.children {
        if child.paint.sc || child.paint.positioned || child.paint.float {
            continue;
        }
        if !matches!(child.kind, FragKind::Block | FragKind::TableCell(_)) {
            paint_fragment(child, builder);
        }
        inflow_content(child, builder);
    }
}

fn paint_floats(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) {
    for child in &fragment.children {
        if child.paint.sc || child.paint.positioned {
            continue;
        }
        if child.paint.float {
            paint_fragment(child, builder);
            inflow_backgrounds(child, builder);
            paint_floats(child, builder);
            inflow_content(child, builder);
        } else {
            paint_floats(child, builder);
        }
    }
}

fn collect_positioned<'a, 'tree>(
    fragment: &'a Frag<'tree>,
    fixed: &[Frag<'tree>],
    negative: &mut Vec<PositionedChild<'a, 'tree>>,
    zero: &mut Vec<(PositionedChild<'a, 'tree>, bool)>,
    positive: &mut Vec<PositionedChild<'a, 'tree>>,
) {
    for child in &fragment.children {
        if let FragKind::Fixed(index) = child.kind {
            let z = fixed
                .get(index)
                .and_then(|fragment| fragment.paint.z)
                .unwrap_or(0);
            match z.cmp(&0) {
                std::cmp::Ordering::Less => negative.push(PositionedChild::Fixed(index)),
                std::cmp::Ordering::Equal => zero.push((PositionedChild::Fixed(index), true)),
                std::cmp::Ordering::Greater => positive.push(PositionedChild::Fixed(index)),
            }
            continue;
        }
        if child.paint.sc {
            match child.paint.z.unwrap_or(0) {
                z if z < 0 => negative.push(PositionedChild::Fragment(child)),
                0 => zero.push((PositionedChild::Fragment(child), true)),
                _ => positive.push(PositionedChild::Fragment(child)),
            }
            continue;
        }
        if child.paint.positioned {
            zero.push((PositionedChild::Fragment(child), false));
            collect_positioned(child, fixed, negative, zero, positive);
            continue;
        }
        collect_positioned(child, fixed, negative, zero, positive);
    }
}

fn paint_fragment(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) {
    let style = PaintStyle::of(fragment);
    if style.is_some_and(|style| {
        matches!(
            style.value(builder.dom, "visibility").as_deref(),
            Some("hidden" | "collapse")
        )
    }) {
        return;
    }
    let style_node = style.map(PaintStyle::node).unwrap_or(fragment.node);
    let scroll_depth = builder.push_scroll_ancestors(style_node);
    // Anonymous lines acquire their scroll scope per piece below. Their
    // inherited clip is in content coordinates too: establishing it here,
    // before BeginScroll, would leave it at the card's unscrolled position.
    let anonymous_line = fragment.node == NO_NODE && matches!(fragment.kind, FragKind::Line(_));
    let fragment_clip = (!anonymous_line)
        .then(|| builder.ancestor_clip(style_node, fragment.clip))
        .flatten();
    let pushed_fragment_clip = fragment_clip.is_some_and(|clip| builder.push_hard_clip(clip));
    // A descendant block's background, border, outline, and hit region are
    // part of the marquee contents just as its text and replaced images are.
    // Wrap the complete fragment paint while excluding the marquee's own
    // principal box, whose viewport border/background stays stationary.
    let marquee_scope = matches!(fragment.kind, FragKind::Block | FragKind::TableCell(_))
        .then(|| builder.marquee_scope(fragment.node, false))
        .flatten();
    if let Some(scope) = marquee_scope.clone() {
        builder.push_marquee_scope(scope);
    }
    let rect = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    if let FragKind::TableCell(layers) = &fragment.kind {
        // CSS 2.2 §17.5.1: row-group, row, then cell. Paint only inside
        // occupied cell rectangles, leaving border-spacing transparent.
        let shape = PaintShape::Rect(rect);
        for (node, area) in layers.iter() {
            let style = PaintStyle::Element(*node);
            if matches!(
                style.value(builder.dom, "visibility").as_deref(),
                Some("hidden" | "collapse")
            ) {
                continue;
            }
            if let Some(color) = background_color_for_style(builder.dom, style)
                && !color.is_transparent()
            {
                let clip = style
                    .value(builder.dom, "background-clip")
                    .unwrap_or_default();
                let clips = split_top_level(&clip, ',');
                let images = style
                    .value(builder.dom, "background-image")
                    .unwrap_or_else(|| "none".into());
                let index = split_top_level(&images, ',').len().saturating_sub(1);
                let color_shape = background_layer_shape(
                    fragment,
                    builder.dom,
                    layer_value(&clips, index, "border-box"),
                    &shape,
                );
                fill_background(builder, color_shape, PaintBrush::Solid(color), &shape);
            }
            let origin = CssRect::new(fragment.x + area[0], fragment.y + area[1], area[2], area[3]);
            paint_background_images_for_style(
                fragment,
                style,
                shape.clone(),
                builder,
                Some(rect),
                Some(origin),
            );
        }
    }
    if let Some(style) = style.filter(|_| fragment.w > 0.0 && fragment.h > 0.0) {
        let radii = border_radii(builder.dom, style, rect);
        let shape = rounded_shape(rect, radii);
        paint_box_shadows(builder.dom, style, &shape, builder);
        let is_root = builder.dom.document_element() == Some(fragment.node);
        let is_canvas_body = builder
            .dom
            .document_element()
            .is_some_and(|root| canvas_background_node(builder.dom, root) == fragment.node)
            || nested_canvas_background_source(builder.dom, fragment.node) == Some(fragment.node);
        if !is_root && !is_canvas_body {
            if fragment.node != NO_NODE {
                paint_native_control_surface(fragment, radii, builder);
            }
            if let Some(color) = background_color_for_style(builder.dom, style)
                && !color.is_transparent()
            {
                let clips = style
                    .value(builder.dom, "background-clip")
                    .unwrap_or_default();
                let clips = split_top_level(&clips, ',');
                let images = style
                    .value(builder.dom, "background-image")
                    .unwrap_or_else(|| "none".into());
                let index = split_top_level(&images, ',').len().saturating_sub(1);
                let color_shape = background_layer_shape(
                    fragment,
                    builder.dom,
                    layer_value(&clips, index, "border-box"),
                    &shape,
                );
                fill_background(builder, color_shape, PaintBrush::Solid(color), &shape);
            }
            paint_background_images(fragment, shape.clone(), builder, None);
        }
        // Each iframe owns a child navigable with its own document canvas.
        // Paint that canvas below the child document, inside the iframe's
        // scrollport, rather than on the nested BODY's finite CSS box. CSS
        // Backgrounds 3 §2.11 makes the propagated root/body background cover
        // the entire canvas; a `height:100%` body can therefore remain only
        // one viewport tall while overflowing descendants extend the canvas.
        // Keeping this underlay in the frame's scroll scope preserves ordinary
        // `background-attachment: scroll` positioning as the viewport moves.
        if fragment.node != NO_NODE
            && matches!(
                builder.dom.tag_name(fragment.node),
                Some("iframe" | "frame")
            )
        {
            paint_nested_document_canvas(fragment, builder);
        }
        paint_borders(fragment, radii, builder);
        paint_number_spin_buttons(fragment, builder);
        // CSS Pseudo 4 §4.1 generates a real box even for content:"". Its hit
        // target is the originating element, including when positioned outside
        // that element's principal box (the stretched-link card pattern).
        // Resolve inherited eligibility on the pseudo itself: pointer-events:auto
        // can override pointer-events:none on the originating element.
        let node = style.node();
        let hit_testable = match style {
            PaintStyle::Element(node) => builder.dom.point_hit_testable(node),
            PaintStyle::Pseudo(..) => {
                let mut suppressed =
                    style.value(builder.dom, "visibility").as_deref() == Some("force-hidden");
                let mut ancestor = Some(node);
                while let Some(id) = ancestor {
                    suppressed |= builder.dom.attr(id, "inert").is_some()
                        || builder
                            .dom
                            .computed_value_resolved(id, "visibility")
                            .as_deref()
                            == Some("force-hidden");
                    ancestor = builder.dom.parent_composed(id);
                }
                !suppressed
                    && style.value(builder.dom, "pointer-events").as_deref() != Some("none")
                    && style.value(builder.dom, "interactivity").as_deref() != Some("inert")
            }
        };
        if hit_testable {
            let link = if matches!(style, PaintStyle::Pseudo(..)) {
                let mut current = Some(node);
                let mut link = None;
                while let Some(id) = current {
                    if builder.dom.tag_name(id) == Some("a")
                        && let Some(href) = builder.dom.attr(id, "href")
                    {
                        link = Some(if builder.dom.render_clickable(id) {
                            crate::doc::Link::JsClick {
                                node: id,
                                href: href.to_string(),
                            }
                        } else {
                            crate::http::resolve(builder.base, href)
                        });
                        break;
                    }
                    current = builder.dom.parent_composed(id);
                }
                link
            } else {
                None
            };
            builder.commands.push(DisplayCommand::HitRegion(HitRegion {
                rect,
                node,
                actor: interaction_actor(builder.dom, node),
                link,
                cursor: style.value(builder.dom, "cursor"),
            }));
        }
    }
    if let FragKind::Line(line) = &fragment.kind {
        builder.lines.push(PaintLine {
            rect: CssRect::new(fragment.x, fragment.y, line.width, line.height),
            baseline: fragment.y + line.baseline,
            ascent: line.ascent,
            descent: line.descent,
        });
        for piece in &line.pieces {
            let node = piece.item.node;
            let style_node = piece.item.style_node;
            // CSS Lists 3 #marker-pseudo: generated markers belong to the list
            // item even though they have no DOM identity of their own. Use
            // their style host for paint ancestry, retaining NO_NODE for hits.
            let paint_node = if node == NO_NODE { style_node } else { node };
            if if let Some((node, pseudo)) = piece.item.pseudo {
                matches!(
                    builder
                        .dom
                        .pseudo_layout_value(node, pseudo, "visibility")
                        .as_deref(),
                    Some("hidden" | "collapse")
                )
            } else if style_node == NO_NODE {
                piece.item.invisible
            } else {
                builder.dom.visibility_hidden(style_node)
            } {
                continue;
            }
            // Line boxes are anonymous (`NO_NODE`), but their pieces retain
            // the generating DOM node. Use that node for the scrollport chain
            // so inline text/replaced content inside a shadow tree receives
            // the same clip and scroll transform as element fragments.
            let piece_scroll_depth = if fragment.paint.outside_marker {
                builder.push_scroll_ancestors(paint_node)
            } else if anonymous_line {
                builder.push_scroll_content_chain(paint_node)
            } else {
                0
            };
            // CSS Overflow 3 #scrolling: move a descendant's own overflow
            // clip with the descendant, through the stationary scrollport.
            // Scope the whole piece, including its hit region, after the
            // scroll transform; a hover-created stacking context must not
            // change which pixels/links survive clipping.
            let piece_clip = anonymous_line
                .then(|| builder.ancestor_clip(paint_node, fragment.clip))
                .flatten()
                .is_some_and(|clip| builder.push_hard_clip(clip));
            let form_piece = matches!(piece.item.kind, super::ItemKind::Form);
            let piece_rect = form_piece
                .then(|| {
                    CssRect::new(
                        fragment.x + piece.x,
                        fragment.y + piece.y,
                        piece.box_width,
                        piece.box_height,
                    )
                })
                .filter(|_| style_node != NO_NODE);
            let control_rect = piece.paint_control_box.then_some(piece_rect).flatten();
            if let Some(rect) = control_rect {
                paint_atomic_control_box(
                    builder,
                    fragment,
                    style_node,
                    rect,
                    piece.item.link.clone(),
                );
            }
            let mut clip = builder.effective_clip(paint_node, fragment.clip);
            if piece_rect.is_some() {
                // A control's label is clipped to its content paint rectangle,
                // not merely to the outer border box. This is the same box
                // used to place the glyphs, so authored padding cannot become
                // a second nested control surface or an overflow escape hatch.
                let mut label_rect = CssRect::new(
                    fragment.x + piece.x + piece.paint_x,
                    fragment.y + piece.y + piece.paint_y,
                    piece.paint_width,
                    piece.paint_height,
                );
                if builder.dom.input_spin_buttons(style_node)
                    && let Some(rect) = piece_rect
                {
                    let right = rect.x + rect.width - rect.width.clamp(8.0, 18.0);
                    label_rect.width = label_rect.width.min((right - label_rect.x).max(0.0));
                }
                clip = intersect_css_rects(clip, label_rect);
            }
            if let Some(shaped) = &piece.shaped {
                let origin = CssPoint::new(
                    fragment.x + piece.x + piece.paint_x,
                    fragment.y + piece.y + piece.paint_y,
                );
                let color = match piece.item.pseudo {
                    Some((node, pseudo)) => text_color_for_style(
                        builder.dom,
                        PaintStyle::Pseudo(node, pseudo),
                        piece.item.link.is_some(),
                    ),
                    None => text_color(builder.dom, style_node, piece.item.link.is_some()),
                };
                let mut shaped = shaped.clone();
                if style_node != NO_NODE {
                    let (underline, strikethrough) = builder.dom.text_decoration(style_node);
                    shaped.underline = underline;
                    shaped.strikethrough = strikethrough;
                }
                let decoration = TextDecorationPaint {
                    color: decoration_color(builder.dom, style_node).unwrap_or(color),
                    style: decoration_style(builder.dom, style_node),
                };
                let shadows = text_shadows(builder.dom, style_node, color);
                builder.push_marquee_content(
                    node,
                    DisplayCommand::GlyphRun {
                        origin,
                        shaped: shaped.clone(),
                        color,
                        decoration,
                        shadows,
                        clip,
                        node,
                        link: piece.item.link.clone(),
                    },
                );
                if style_node == NO_NODE || builder.dom.point_hit_testable(style_node) {
                    builder.push_marquee_content(
                        node,
                        DisplayCommand::HitRegion(HitRegion {
                            rect: CssRect::new(
                                origin.x,
                                origin.y,
                                shaped.advance,
                                shaped.line_height,
                            ),
                            node,
                            actor: interaction_actor(builder.dom, node),
                            link: piece.item.link.clone(),
                            cursor: cursor_value(builder.dom, style_node),
                        }),
                    );
                }
            } else if piece.item.graphical_image.is_some()
                || piece.item.image.is_some()
                || (node != NO_NODE && builder.dom.canvas_size(node).is_some())
            {
                let handle = builder.replaced_image(
                    node,
                    piece
                        .item
                        .graphical_image
                        .as_ref()
                        .or(piece.item.image.as_ref()),
                );
                let rect = CssRect::new(
                    fragment.x + piece.x + piece.paint_x,
                    fragment.y + piece.y + piece.paint_y,
                    piece.paint_width,
                    piece.paint_height,
                );
                // CSS Backgrounds 3 §4.2/§4.3: replaced pixels clip to the
                // curved CONTENT edge, independent of overflow. Object-fit's
                // painted rectangle may be smaller than this content box.
                let content = CssRect::new(
                    fragment.x + piece.x,
                    fragment.y + piece.y,
                    piece.box_width,
                    piece.box_height,
                );
                let border = builder
                    .replaced_border_boxes
                    .get(&node)
                    .copied()
                    .unwrap_or(content);
                let radii = (style_node != NO_NODE)
                    .then(|| border_radii(builder.dom, PaintStyle::Element(style_node), border));
                let content_clip = radii
                    .filter(|r| r.corners.iter().any(|&(x, y)| x > 0. && y > 0.))
                    .map(|r| rounded_shape(content, inset_radii(r, border, content)));
                if let Some(handle) = handle {
                    builder.push_clipped_marquee_content(
                        node,
                        DisplayCommand::Image {
                            rect,
                            handle,
                            source_rect: None,
                            fit: if piece.item.crop {
                                ImageFit::Cover
                            } else {
                                ImageFit::Contain
                            },
                            sampling: if if style_node == NO_NODE {
                                piece.item.pixelated
                            } else {
                                matches!(
                                    builder
                                        .dom
                                        .computed_value_resolved(style_node, "image-rendering")
                                        .as_deref(),
                                    Some(
                                        "pixelated"
                                            | "crisp-edges"
                                            | "-moz-crisp-edges"
                                            | "-webkit-optimize-contrast"
                                    )
                                )
                            } {
                                ImageSampling::Nearest
                            } else {
                                ImageSampling::Smooth
                            },
                            clip,
                            node,
                            link: piece.item.link.clone(),
                        },
                        content_clip,
                    );
                }
                if style_node == NO_NODE || builder.dom.point_hit_testable(style_node) {
                    builder.push_marquee_content(
                        node,
                        DisplayCommand::HitRegion(HitRegion {
                            rect,
                            node,
                            actor: interaction_actor(builder.dom, node),
                            link: piece.item.link.clone(),
                            cursor: cursor_value(builder.dom, style_node),
                        }),
                    );
                }
            }
            if piece_clip {
                builder.pop_hard_clip();
            }
            builder.pop_scroll_ancestors(piece_scroll_depth);
        }
    }
    if style.is_some() && fragment.w > 0.0 && fragment.h > 0.0 {
        paint_outline(fragment, builder);
    }
    if marquee_scope.is_some() {
        builder.pop_marquee_scope();
    }
    if pushed_fragment_clip {
        builder.pop_hard_clip();
    }
    builder.pop_scroll_ancestors(scroll_depth);
}

/// CSS 2.2 §11.1.2 legacy clipping rectangle. The four offsets are from the
/// positioned element's border box; `auto` leaves that edge at the border
/// edge. Percentages are not part of the legacy grammar.
fn legacy_clip_rect(dom: &Dom, node: NodeId, border_box: CssRect) -> Option<CssRect> {
    if !matches!(
        dom.computed_value_resolved(node, "position")
            .as_deref()
            .map(str::trim),
        Some("absolute" | "fixed")
    ) {
        return None;
    }
    let value = dom.computed_value_resolved(node, "clip")?;
    let value = value.trim();
    let inner = value.get(5..value.len().checked_sub(1)?).filter(|_| {
        value
            .get(..5)
            .is_some_and(|head| head.eq_ignore_ascii_case("rect("))
    })?;
    let normalized = inner.replace(',', " ");
    let parts: Vec<&str> = normalized.split_whitespace().collect();
    let [top, right, bottom, left] = parts.as_slice() else {
        return None;
    };
    let units = Units::of(dom, node);
    let edge = |value: &str, auto: f32| {
        value
            .trim()
            .eq_ignore_ascii_case("auto")
            .then_some(auto)
            .or_else(|| crate::layout2::css_length_px(value, units))
    };
    let top = edge(top, 0.0)?;
    let right = edge(right, border_box.width)?;
    let bottom = edge(bottom, border_box.height)?;
    let left = edge(left, 0.0)?;
    Some(CssRect::new(
        border_box.x + left,
        border_box.y + top,
        (right - left).max(0.0),
        (bottom - top).max(0.0),
    ))
}

fn intersect_css_rects(existing: Option<CssRect>, rect: CssRect) -> Option<CssRect> {
    let Some(existing) = existing else {
        return Some(rect);
    };
    let x0 = existing.x.max(rect.x);
    let y0 = existing.y.max(rect.y);
    let x1 = (existing.x + existing.width).min(rect.x + rect.width);
    let y1 = (existing.y + existing.height).min(rect.y + rect.height);
    // An empty intersection remains an active zero-area clip. Returning None
    // would mean "unclipped" to every caller and leak exactly the content the
    // two disjoint clips are required to suppress.
    Some(CssRect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0)))
}

/// Direct `<input>` controls are atomic pieces inside an anonymous line box,
/// but CSS backgrounds, borders, shadows, outlines, and pointer hit testing
/// apply to their complete replaced-element border box just as they do to a
/// normal fragment (CSS UI 4 §7.2 / HTML Rendering §15.5). Reuse the
/// canonical fragment decorators over a temporary geometry-only fragment.
fn paint_atomic_control_box(
    builder: &mut Builder<'_, '_>,
    parent: &Frag<'_>,
    node: NodeId,
    rect: CssRect,
    link: Option<crate::doc::Link>,
) {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return;
    }
    let clip = builder.effective_clip(node, parent.clip);
    let pushed_clip = clip.is_some_and(|clip| builder.push_hard_clip(clip));
    let style = super::style::BoxStyle::of(
        builder.dom,
        node,
        super::value::Vp {
            w: builder.viewport_w,
            h: builder.viewport_h,
        },
    );
    let control = Frag {
        node,
        x: rect.x,
        y: rect.y,
        w: rect.width,
        h: rect.height,
        border: style.border,
        css_size: None,
        content_size: None,
        content_offset: [0.0; 2],
        paint: Default::default(),
        clip: parent.clip,
        kind: FragKind::Block,
        children: Vec::new(),
    };
    let radii = border_radii(builder.dom, PaintStyle::Element(node), rect);
    let shape = rounded_shape(rect, radii);
    paint_box_shadows(builder.dom, PaintStyle::Element(node), &shape, builder);
    paint_native_control_surface(&control, radii, builder);
    if let Some(color) = background_color(builder.dom, node)
        && !color.is_transparent()
    {
        builder.commands.push(DisplayCommand::Fill {
            shape: shape.clone(),
            brush: PaintBrush::Solid(color),
        });
    }
    paint_background_images(&control, shape, builder, None);
    paint_borders(&control, radii, builder);
    paint_number_spin_buttons(&control, builder);
    if builder.dom.point_hit_testable(node) {
        builder.commands.push(DisplayCommand::HitRegion(HitRegion {
            rect,
            node,
            actor: interaction_actor(builder.dom, node),
            link,
            cursor: cursor_value(builder.dom, node),
        }));
    }
    paint_outline_box(
        builder,
        PaintStyle::Element(node),
        rect,
        outline_of(builder.dom, node, Units::of(builder.dom, node)),
    );
    if pushed_clip {
        builder.pop_hard_clip();
    }
}

/// HTML Rendering §15.5 permits a user agent to supply a native appearance for
/// form controls. CSS UI's `appearance:auto` is the opt-in/default state; an
/// author-requested `appearance:none`, an explicit background, or an explicit
/// border leaves the authored paint in charge. The graphical frontend needs a
/// real surface for native text/button widgets because terminal brackets are
/// intentionally emitted only by the terminal adapter.
fn paint_native_control_surface(
    fragment: &Frag<'_>,
    radii: CornerRadii,
    builder: &mut Builder<'_, '_>,
) {
    let node = fragment.node;
    // WHATWG HTML Rendering §15.5.10 defines checkbox/radio inputs as one
    // inline-block containing a *single* native control. Their atomic glyph
    // is that complete appearance; painting the generic text-control surface
    // behind it creates a second square/rectangle around the widget.
    let is_checkable = builder.dom.tag_name(node) == Some("input")
        && builder
            .dom
            .attr(node, "type")
            .is_some_and(|kind| matches!(kind.to_ascii_lowercase().as_str(), "checkbox" | "radio"));
    let is_control = matches!(builder.dom.tag_name(node), Some("button" | "textarea"))
        || (builder.dom.tag_name(node) == Some("input")
            && !builder
                .dom
                .attr(node, "type")
                .is_some_and(|kind| kind.eq_ignore_ascii_case("hidden")));
    // CSS UI 4 #appearance-switching: only host-language widgets have a
    // native surface. Making an ordinary div editable does not turn its
    // CSS box into an input/textarea with a UA background and border.
    if !is_control
        || is_checkable
        || builder
            .dom
            .computed_value_resolved(node, "appearance")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("none"))
        || builder
            .dom
            .computed_value_resolved(node, "-webkit-appearance")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("none"))
    {
        return;
    }

    let background_declared = builder
        .dom
        .computed_value_resolved(node, "background-color")
        .is_some()
        || builder
            .dom
            .computed_value_resolved(node, "background-image")
            .is_some();
    let border_declared = ["top", "right", "bottom", "left"].into_iter().any(|side| {
        builder
            .dom
            .computed_value_resolved(node, &format!("border-{side}-style"))
            .is_some()
    });
    let foreground = text_color(builder.dom, node, false);
    let light_foreground = paint_color_is_light(foreground);
    let surface = if light_foreground {
        PaintColor::Rgba(31, 34, 38, 255)
    } else {
        PaintColor::Rgba(255, 255, 255, 255)
    };
    let edge = if light_foreground {
        PaintColor::Rgba(125, 130, 138, 255)
    } else {
        PaintColor::Rgba(118, 118, 118, 255)
    };
    let rect = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    if !background_declared {
        builder.commands.push(DisplayCommand::Fill {
            shape: rounded_shape(rect, radii),
            brush: PaintBrush::Solid(surface),
        });
    }
    if !border_declared {
        builder.commands.push(DisplayCommand::Stroke {
            shape: rounded_shape(
                CssRect::new(
                    rect.x + 0.5,
                    rect.y + 0.5,
                    (rect.width - 1.0).max(0.0),
                    (rect.height - 1.0).max(0.0),
                ),
                radii,
            ),
            brush: PaintBrush::Solid(edge),
            style: StrokeStyle::solid(1.0),
        });
    }
}

/// HTML Rendering §15.5.6 leaves the exact number-control UI to the user
/// agent, but explicitly calls a spinbox with up/down controls a reasonable
/// rendering for `type=number`. Keep the affordance in the graphical display
/// list (the terminal frontend uses its own character-cell adaptation), and
/// suppress it when CSS UI requests `appearance:none`.
fn paint_number_spin_buttons(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) {
    let node = fragment.node;
    if node == NO_NODE
        || !builder.dom.input_spin_buttons(node)
        || fragment.w < 8.0
        || fragment.h < 8.0
    {
        return;
    }
    let rect = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    let rail = fragment.w.clamp(8.0, 18.0);
    let center_x = rect.x + rect.width - rail * 0.5 - 1.0;
    let midpoint = rect.y + rect.height * 0.5;
    let half_width = (rail * 0.22).max(1.5);
    let inset = (rect.height * 0.16).max(1.5);
    let color = text_color(builder.dom, node, false);
    let up = PaintShape::Polygon {
        points: vec![
            CssPoint::new(center_x - half_width, midpoint - inset),
            CssPoint::new(center_x + half_width, midpoint - inset),
            CssPoint::new(center_x, rect.y + inset),
        ],
        evenodd: false,
    };
    let down = PaintShape::Polygon {
        points: vec![
            CssPoint::new(center_x - half_width, midpoint + inset),
            CssPoint::new(center_x + half_width, midpoint + inset),
            CssPoint::new(center_x, rect.y + rect.height - inset),
        ],
        evenodd: false,
    };
    for shape in [up, down] {
        builder.commands.push(DisplayCommand::Fill {
            shape,
            brush: PaintBrush::Solid(color),
        });
    }
}

fn paint_color_is_light(color: PaintColor) -> bool {
    let (r, g, b) = match color {
        PaintColor::Rgba(r, g, b, _) => (r, g, b),
        PaintColor::Foreground | PaintColor::Content | PaintColor::Window => (220, 220, 220),
        _ => (30, 30, 30),
    };
    (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) >= 128_000
}

/// Paint a CSS Basic User Interface 4 §3 outline. Unlike a border, the
/// outline is outside the border edge and does not affect layout. The
/// outline's exact stacking is intentionally UA-defined; emitting it at the
/// end of this fragment's paint keeps it visible over the fragment's own text
/// while preserving the surrounding Appendix E traversal.
fn paint_outline(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) {
    let Some(style) = PaintStyle::of(fragment) else {
        return;
    };
    paint_outline_box(
        builder,
        style,
        CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
        fragment.paint.outline,
    );
}

fn paint_outline_box(
    builder: &mut Builder<'_, '_>,
    source: PaintStyle,
    border_box: CssRect,
    outline: Outline,
) {
    if !outline.paints() || matches!(outline.style, OutlineStyle::Auto) {
        return;
    }
    let width = outline.width;
    let grow = outline.offset + width / 2.0;
    let rect = CssRect::new(
        border_box.x - grow,
        border_box.y - grow,
        border_box.width + grow * 2.0,
        border_box.height + grow * 2.0,
    );
    let base_radii = border_radii(builder.dom, source, border_box);
    let radii = CornerRadii {
        corners: base_radii.corners.map(|(x, y)| (x + grow, y + grow)),
    };
    let color = source
        .value(builder.dom, "outline-color")
        .and_then(|value| {
            if value.trim().eq_ignore_ascii_case("currentcolor") {
                Some(text_color_for_style(builder.dom, source, false))
            } else {
                PaintColor::parse_css(&value)
            }
        })
        .unwrap_or_else(|| text_color_for_style(builder.dom, source, false));
    builder.commands.push(DisplayCommand::Stroke {
        shape: rounded_shape(rect, radii),
        brush: PaintBrush::Solid(color),
        style: match outline.style {
            OutlineStyle::Dotted => {
                let mut style = StrokeStyle::solid(width);
                style.dash = vec![0.0, width * 2.0];
                style.cap = LineCap::Round;
                style
            }
            OutlineStyle::Dashed => {
                let mut style = StrokeStyle::solid(width);
                style.dash = vec![width * 3.0, width * 2.0];
                style
            }
            _ => StrokeStyle::solid(width),
        },
    });
}

/// The resident page actor serializes its own node identity into presentation
/// markup. Arena ids belong only to this parse/layout pass and must never be
/// confused with actor ids when dispatching Pointer Events or form updates.
fn interaction_actor(dom: &Dom, node: NodeId) -> Option<usize> {
    if node == NO_NODE {
        return None;
    }
    if dom.render_live() {
        // Layout and hit testing address the canonical arena directly. The
        // exact painted node is the Pointer Events target; dispatch/default
        // activation then follows the DOM event path inside the actor.
        return Some(node);
    }
    let mut current = Some(node);
    while let Some(node) = current {
        if let Some(actor) = dom
            .attr(node, "data-trust-hover")
            .and_then(|value| value.parse().ok())
        {
            return Some(actor);
        }
        if let Some(marker) = dom.attr(node, "data-trust-click")
            && let Some(actor) = marker
                .strip_prefix("x-trust-js:")
                .and_then(|rest| rest.split(':').next())
                .and_then(|value| value.parse().ok())
        {
            return Some(actor);
        }
        if matches!(
            dom.tag_name(node),
            Some("form" | "input" | "button" | "select" | "textarea")
        ) && let Some(actor) = dom
            .attr(node, "data-trust-node")
            .and_then(|value| value.parse().ok())
        {
            return Some(actor);
        }
        if let Some(href) = dom.attr(node, "href")
            && let Some(marker) = href.strip_prefix("x-trust-js:")
            && let Some(actor) = marker
                .split(':')
                .next()
                .and_then(|value| value.parse().ok())
        {
            return Some(actor);
        }
        current = dom.node(node).parent;
    }
    None
}

fn push_layer(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) -> bool {
    let Some(style) = PaintStyle::of(fragment) else {
        return false;
    };
    let opacity = fragment.paint.opacity.clamp(0.0, 1.0);
    let blend = style
        .value(builder.dom, "mix-blend-mode")
        .as_deref()
        .map(blend_mode)
        .unwrap_or_default();
    if opacity < 1.0 || blend != BlendMode::Normal {
        builder
            .commands
            .push(DisplayCommand::PushLayer(CompositingLayer {
                opacity,
                blend,
            }));
        true
    } else {
        false
    }
}

fn paint_transform(fragment: &Frag<'_>, builder: &Builder<'_, '_>) -> Option<Affine2d> {
    let style = PaintStyle::of(fragment)?;
    let (matrix, layout_translation) = element_transform(
        builder.dom,
        style,
        fragment.w,
        fragment.h,
        fragment.x,
        fragment.y,
    )?;
    // Phase 2 retained translated fragment coordinates for terminal output.
    // Undo that already-applied translation inside the graphical transform so
    // the desktop path sees CSS's complete matrix exactly once.
    let corrected = if fragment.paint.pseudo.is_some() {
        // Generated-box transforms are wholly paint-time; their anonymous
        // fragment geometry was not pre-translated by `BoxStyle::of`.
        matrix
    } else {
        matrix.then(Affine2d::translate(
            -layout_translation.x,
            -layout_translation.y,
        ))
    };
    (!corrected.is_identity()).then_some(corrected)
}

fn paint_background_images(
    fragment: &Frag<'_>,
    shape: PaintShape,
    builder: &mut Builder<'_, '_>,
    canvas: Option<CssRect>,
) {
    if let Some(style) = PaintStyle::of(fragment) {
        paint_background_images_for_style(fragment, style, shape, builder, canvas, None);
    }
}

/// Return the root and propagated background source of a realized iframe
/// document. The child document is retained below its iframe owner in TRust's
/// arena, but it remains a distinct document/canvas for CSS painting.
fn frame_canvas_background(dom: &Dom, frame: NodeId) -> Option<(NodeId, NodeId)> {
    if !matches!(dom.tag_name(frame), Some("iframe" | "frame")) {
        return None;
    }
    let root = dom
        .children(frame)
        .into_iter()
        .find(|&child| dom.tag_name(child) == Some("html"))?;
    Some((root, canvas_background_node(dom, root)))
}

/// If `node` is the BODY supplying a nested document's canvas background,
/// return that source node. This suppresses the ordinary BODY-box paint after
/// the same values have been propagated to the iframe canvas.
fn nested_canvas_background_source(dom: &Dom, node: NodeId) -> Option<NodeId> {
    if node == NO_NODE || dom.tag_name(node) != Some("body") {
        return None;
    }
    let root = dom.parent_flat(node)?;
    if dom.tag_name(root) != Some("html") {
        return None;
    }
    let frame = dom.parent_flat(root)?;
    let (document_root, source) = frame_canvas_background(dom, frame)?;
    (document_root == root).then_some(source)
}

fn paint_nested_document_canvas(fragment: &Frag<'_>, builder: &mut Builder<'_, '_>) {
    let Some((_root, style_node)) = frame_canvas_background(builder.dom, fragment.node) else {
        return;
    };
    let Some(container) = builder
        .scroll_containers
        .iter()
        .find(|container| container.node == fragment.node)
        .cloned()
    else {
        return;
    };

    // The infinite CSS canvas only needs a finite retained representation out
    // to this viewport's scrolling-area edges. At every legal scroll offset,
    // that rectangle still completely covers the iframe scrollport.
    let canvas = CssRect::new(
        container.viewport.x,
        container.viewport.y,
        container.content.width.max(container.viewport.width),
        container.content.height.max(container.viewport.height),
    );
    builder
        .commands
        .push(DisplayCommand::PushClip(PaintShape::Rect(
            container.viewport,
        )));
    builder
        .commands
        .push(DisplayCommand::BeginScroll(container.node));
    if let Some(color) = background_color(builder.dom, style_node)
        && !color.is_transparent()
    {
        builder.commands.push(DisplayCommand::Fill {
            shape: PaintShape::Rect(canvas),
            brush: PaintBrush::Solid(color),
        });
    }
    paint_background_images_for_style(
        fragment,
        PaintStyle::Element(style_node),
        PaintShape::Rect(canvas),
        builder,
        Some(canvas),
        // The nested root's box starts at its viewport origin and extends over
        // the scrollable document. It is not the embedding iframe's border or
        // padding box, whose geometry `fragment` otherwise describes.
        Some(canvas),
    );
    builder.commands.push(DisplayCommand::EndScroll);
    builder.commands.push(DisplayCommand::PopClip);
}

/// Return the element whose computed background is propagated to the canvas.
///
/// CSS Backgrounds 3 §2.11.2 gives HTML a special case: if the root's
/// `background-image` is `none` and its `background-color` is transparent, the
/// first direct `body` child supplies the canvas background. The body's used
/// background values are then treated as if they were specified on the root;
/// callers therefore use the root fragment for geometry while reading style
/// from this returned node.
fn canvas_background_node(dom: &Dom, root: NodeId) -> NodeId {
    let root_image = dom
        .computed_value_resolved(root, "background-image")
        .is_some_and(|value| !value.trim().eq_ignore_ascii_case("none"));
    let root_color = background_color(dom, root);
    if root_image || root_color.is_some_and(|color| !color.is_transparent()) {
        return root;
    }
    dom.children(root)
        .into_iter()
        .find(|&child| {
            dom.tag_name(child) == Some("body")
                && dom.computed_value_resolved(child, "display").as_deref() != Some("none")
        })
        .unwrap_or(root)
}

fn paint_background_images_for_style(
    fragment: &Frag<'_>,
    style: PaintStyle,
    shape: PaintShape,
    builder: &mut Builder<'_, '_>,
    canvas: Option<CssRect>,
    positioning_override: Option<CssRect>,
) {
    let Some(value) = style.value(builder.dom, "background-image") else {
        return;
    };
    let border_box = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    let padding_box = padding_box_with_style(fragment);
    let content_box = content_box_with_style(builder.dom, fragment, padding_box);
    let clip_value = style
        .value(builder.dom, "background-clip")
        .unwrap_or_else(|| "border-box".into());
    let origin_value = style
        .value(builder.dom, "background-origin")
        .unwrap_or_else(|| "padding-box".into());
    let clip_layers = split_top_level(&clip_value, ',');
    let origin_layers = split_top_level(&origin_value, ',');
    let repeat_value = style
        .value(builder.dom, "background-repeat")
        .unwrap_or_else(|| "repeat".into());
    let position_value = style
        .value(builder.dom, "background-position")
        .unwrap_or_else(|| "0% 0%".into());
    let size_value = style
        .value(builder.dom, "background-size")
        .unwrap_or_else(|| "auto auto".into());
    let repeat_layers = split_top_level(&repeat_value, ',');
    let position_layers = split_top_level(&position_value, ',');
    let size_layers = split_top_level(&size_value, ',');
    let images = split_top_level(&value, ',');
    // CSS Backgrounds paints the first listed layer closest to the viewer, so
    // emit in reverse order after the background color.
    for (index, layer) in images.iter().enumerate().rev() {
        let layer = layer.trim();
        if layer.eq_ignore_ascii_case("none") || layer.is_empty() {
            continue;
        }
        let layer_shape = if canvas.is_some() {
            shape.clone()
        } else {
            background_layer_shape(
                fragment,
                builder.dom,
                layer_value(&clip_layers, index, "border-box"),
                &shape,
            )
        };
        if let Some(brush) = parse_gradient(layer, positioning_override.unwrap_or(border_box)) {
            fill_background(builder, layer_shape, brush, &shape);
        } else if let Some(url) = css_url(layer) {
            let source = resolve_image_source(builder.base, &url);
            let handle = builder.image(source.clone());
            let origin = layer_value(&origin_layers, index, "padding-box");
            let positioning = positioning_override
                .unwrap_or_else(|| background_box(origin, border_box, padding_box, content_box));
            let clip = canvas.unwrap_or_else(|| {
                background_box(
                    layer_value(&clip_layers, index, "border-box"),
                    border_box,
                    padding_box,
                    content_box,
                )
            });
            let repeat = parse_background_repeat(layer_value(&repeat_layers, index, "repeat"));
            let position = layer_value(&position_layers, index, "0% 0%");
            let size = layer_value(&size_layers, index, "auto auto");
            let natural = builder
                .images
                .get(&source)
                .copied()
                .filter(|(w, h)| *w > 0 && *h > 0 && *w != u32::MAX && *h != u32::MAX)
                .map(|(w, h)| (w as f32, h as f32))
                .unwrap_or((300.0, 150.0));
            let (mut tile_w, mut tile_h) = background_size(size, natural, positioning);
            if !tile_w.is_finite() || !tile_h.is_finite() || tile_w <= 0.0 || tile_h <= 0.0 {
                continue;
            }
            let (mut start_x, mut start_y) =
                background_position(position, positioning, (tile_w, tile_h));
            let mut repeat = repeat;
            if matches!(repeat, BackgroundRepeat::Round) {
                let nx = (positioning.width / tile_w).round().max(1.0);
                let ny = (positioning.height / tile_h).round().max(1.0);
                tile_w = positioning.width / nx;
                tile_h = positioning.height / ny;
                (start_x, start_y) = background_position(position, positioning, (tile_w, tile_h));
                repeat = BackgroundRepeat::Repeat;
            }
            if matches!(repeat, BackgroundRepeat::Space) {
                // `space` preserves the intrinsic tile size and distributes
                // the remaining space between complete tiles. If only one
                // tile fits on an axis, CSS falls back to no-repeat there.
                let nx = (positioning.width / tile_w).floor() as usize;
                let ny = (positioning.height / tile_h).floor() as usize;
                builder.commands.push(DisplayCommand::PushClip(layer_shape));
                paint_spaced_background(
                    builder,
                    clip,
                    style.node(),
                    handle,
                    positioning,
                    (tile_w, tile_h),
                    (start_x, start_y),
                    nx,
                    ny,
                );
                builder.commands.push(DisplayCommand::PopClip);
                continue;
            }
            builder.commands.push(DisplayCommand::PushClip(layer_shape));
            builder
                .commands
                .push(DisplayCommand::PushClip(background_clip_shape(
                    clip, &shape, border_box,
                )));
            // CSS Backgrounds 3 §§2.4 and 2.6: size and position the image
            // first, then repeat it as needed to cover the painting area.
            let x_repeat = matches!(repeat, BackgroundRepeat::Repeat | BackgroundRepeat::RepeatX);
            let y_repeat = matches!(repeat, BackgroundRepeat::Repeat | BackgroundRepeat::RepeatY);
            if !x_repeat {
                start_x += positioning.x;
            }
            if !y_repeat {
                start_y += positioning.y;
            }
            let x0 = if x_repeat {
                let absolute_start = positioning.x + start_x;
                positioning.x + (absolute_start - positioning.x).rem_euclid(tile_w) - tile_w
            } else {
                start_x
            };
            let y0 = if y_repeat {
                let absolute_start = positioning.y + start_y;
                positioning.y + (absolute_start - positioning.y).rem_euclid(tile_h) - tile_h
            } else {
                start_y
            };
            let x_end = clip.x + clip.width;
            let y_end = clip.y + clip.height;
            let mut y = y0;
            let mut count = 0usize;
            while y < y_end && count < 4096 {
                let mut x = x0;
                let mut x_count = 0usize;
                while x < x_end && x_count < 4096 {
                    let tile = CssRect::new(x, y, tile_w, tile_h);
                    builder.commands.push(DisplayCommand::Image {
                        rect: tile,
                        handle,
                        source_rect: None,
                        // `rect` is the used background image size after
                        // CSS Backgrounds §2.9. Rendering at intrinsic pixels
                        // here ignores an authored `background-size` (a 2x
                        // source paints about twice as large); fill the already
                        // aspect-correct tile rectangle exactly.
                        fit: ImageFit::Fill,
                        sampling: ImageSampling::Smooth,
                        clip: None,
                        node: style.node(),
                        link: None,
                    });
                    if !x_repeat {
                        break;
                    }
                    x += tile_w;
                    x_count += 1;
                }
                if !y_repeat {
                    break;
                }
                y += tile_h;
                count += 1;
            }
            builder.commands.push(DisplayCommand::PopClip);
            builder.commands.push(DisplayCommand::PopClip);
        }
    }
}

// A glyph mask can extend beyond the background border; ordinary background
// box shapes are already bounded and need no extra stateful clip commands.
fn fill_background(
    builder: &mut Builder<'_, '_>,
    shape: PaintShape,
    brush: PaintBrush,
    border: &PaintShape,
) {
    let text = matches!(shape, PaintShape::Path(_));
    if text {
        builder
            .commands
            .push(DisplayCommand::PushClip(border.clone()));
    }
    builder.commands.push(DisplayCommand::Fill { shape, brush });
    if text {
        builder.commands.push(DisplayCommand::PopClip);
    }
}

/// CSS Backgrounds 4 #background-clip (local snapshot 2026-09-06): text
/// clipping includes in-flow and floated descendants, independently of text
/// color. Positioned out-of-flow descendants do not contribute to the mask.
fn background_layer_shape(
    fragment: &Frag<'_>,
    dom: &Dom,
    clip: &str,
    border: &PaintShape,
) -> PaintShape {
    if clip.trim() != "text" {
        let rect = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
        let padding = padding_box_with_style(fragment);
        let content = content_box_with_style(dom, fragment, padding);
        return background_clip_shape(background_box(clip, rect, padding, content), border, rect);
    }
    fn collect(f: &Frag<'_>, dom: &Dom, path: &mut Vec<crate::render::PathElement>) {
        if let FragKind::Line(line) = &f.kind {
            for piece in &line.pieces {
                let node = piece.item.style_node;
                if node != NO_NODE && dom.visibility_hidden(node) {
                    continue;
                }
                let Some(shaped) = &piece.shaped else {
                    continue;
                };
                let mut shaped = shaped.clone();
                if node != NO_NODE {
                    (shaped.underline, shaped.strikethrough) = dom.text_decoration(node);
                }
                crate::text::append_text_path(
                    path,
                    &shaped,
                    CssPoint::new(f.x + piece.x + piece.paint_x, f.y + piece.y + piece.paint_y),
                    decoration_style(dom, node),
                );
            }
        }
        for child in &f.children {
            if matches!(child.kind, FragKind::Oof(..) | FragKind::Fixed(_))
                || (child.node != NO_NODE
                    && dom
                        .computed_value_resolved(child.node, "position")
                        .is_some_and(|v| matches!(v.as_str(), "absolute" | "fixed")))
            {
                continue;
            }
            collect(child, dom, path);
        }
    }
    let mut path = Vec::new();
    collect(fragment, dom, &mut path);
    PaintShape::Path(path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackgroundRepeat {
    Repeat,
    RepeatX,
    RepeatY,
    NoRepeat,
    Space,
    Round,
}

fn parse_background_repeat(value: &str) -> BackgroundRepeat {
    let tokens = split_ws(value);
    if tokens.len() > 1 {
        let first = tokens[0].to_ascii_lowercase();
        let second = tokens[1].to_ascii_lowercase();
        return match (first.as_str(), second.as_str()) {
            ("repeat", "no-repeat") => BackgroundRepeat::RepeatX,
            ("no-repeat", "repeat") => BackgroundRepeat::RepeatY,
            ("no-repeat", "no-repeat") => BackgroundRepeat::NoRepeat,
            ("space", "space") => BackgroundRepeat::Space,
            ("round", "round") => BackgroundRepeat::Round,
            _ => BackgroundRepeat::Repeat,
        };
    }
    match tokens
        .first()
        .map(|token| token.to_ascii_lowercase())
        .as_deref()
    {
        Some("no-repeat") => BackgroundRepeat::NoRepeat,
        Some("repeat-x") => BackgroundRepeat::RepeatX,
        Some("repeat-y") => BackgroundRepeat::RepeatY,
        Some("space") => BackgroundRepeat::Space,
        Some("round") => BackgroundRepeat::Round,
        _ => BackgroundRepeat::Repeat,
    }
}

fn layer_value<'a>(layers: &[&'a str], index: usize, default: &'a str) -> &'a str {
    if layers.is_empty() {
        default
    } else {
        layers[index % layers.len()].trim()
    }
}

fn background_box(value: &str, border: CssRect, padding: CssRect, content: CssRect) -> CssRect {
    match value.trim().to_ascii_lowercase().as_str() {
        "content-box" => content,
        "padding-box" => padding,
        _ => border,
    }
}

fn padding_box_with_style(fragment: &Frag<'_>) -> CssRect {
    let [top, right, bottom, left] = fragment.border;
    CssRect::new(
        fragment.x + left,
        fragment.y + top,
        (fragment.w - left - right).max(0.0),
        (fragment.h - top - bottom).max(0.0),
    )
}

fn content_box_with_style(dom: &Dom, fragment: &Frag<'_>, padding: CssRect) -> CssRect {
    let width_basis = padding.width.max(0.0);
    let style = PaintStyle::of(fragment);
    let pad = ["top", "right", "bottom", "left"].map(|side| {
        style
            .and_then(|style| style.value(dom, &format!("padding-{side}")))
            .as_deref()
            .and_then(|value| transform_length(value, width_basis))
            .unwrap_or(0.0)
            .max(0.0)
    });
    CssRect::new(
        padding.x + pad[3],
        padding.y + pad[0],
        (padding.width - pad[1] - pad[3]).max(0.0),
        (padding.height - pad[0] - pad[2]).max(0.0),
    )
}

fn background_size(value: &str, natural: (f32, f32), area: CssRect) -> (f32, f32) {
    let tokens = split_ws(value);
    if tokens
        .first()
        .is_some_and(|token| token.eq_ignore_ascii_case("cover"))
    {
        let scale = (area.width / natural.0).max(area.height / natural.1);
        return (natural.0 * scale, natural.1 * scale);
    }
    if tokens
        .first()
        .is_some_and(|token| token.eq_ignore_ascii_case("contain"))
    {
        let scale = (area.width / natural.0).min(area.height / natural.1);
        return (natural.0 * scale, natural.1 * scale);
    }
    let width = tokens
        .first()
        .and_then(|token| background_length(token, area.width));
    let height = tokens
        .get(1)
        .and_then(|token| background_length(token, area.height));
    match (width, height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, natural.1 * w / natural.0),
        (None, Some(h)) => (natural.0 * h / natural.1, h),
        _ => natural,
    }
}

fn background_length(value: &str, basis: f32) -> Option<f32> {
    if value.eq_ignore_ascii_case("auto") {
        return None;
    }
    transform_length(value, basis).map(|value| value.max(0.01))
}

fn background_position(value: &str, area: CssRect, image: (f32, f32)) -> (f32, f32) {
    let tokens = split_ws(value);
    let (x, y) = match tokens.as_slice() {
        [] => ("0%", "0%"),
        [one] if matches!(one.to_ascii_lowercase().as_str(), "top" | "bottom") => ("50%", *one),
        [one] => (*one, "50%"),
        [x, y, ..] => (*x, *y),
    };
    (
        background_position_component(x, area.width, image.0, false),
        background_position_component(y, area.height, image.1, true),
    )
}

fn background_position_component(value: &str, area: f32, image: f32, vertical: bool) -> f32 {
    match value.trim().to_ascii_lowercase().as_str() {
        "left" if !vertical => 0.0,
        "top" if vertical => 0.0,
        "center" => (area - image) / 2.0,
        "right" if !vertical => area - image,
        "bottom" if vertical => area - image,
        other => {
            if let Some(percent) = other.strip_suffix('%').and_then(|v| v.parse::<f32>().ok()) {
                (area - image) * percent / 100.0
            } else {
                px(other).unwrap_or(0.0)
            }
        }
    }
}

fn background_clip_shape(clip: CssRect, original: &PaintShape, border: CssRect) -> PaintShape {
    if clip == border {
        original.clone()
    } else {
        PaintShape::Rect(clip)
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_spaced_background(
    builder: &mut Builder<'_, '_>,
    clip: CssRect,
    node: NodeId,
    handle: ImageHandle,
    area: CssRect,
    tile: (f32, f32),
    start: (f32, f32),
    nx: usize,
    ny: usize,
) {
    builder
        .commands
        .push(DisplayCommand::PushClip(PaintShape::Rect(clip)));
    let (tile_w, tile_h) = tile;
    let gap_x = if nx > 1 {
        (area.width - nx as f32 * tile_w) / (nx - 1) as f32
    } else {
        0.0
    };
    let gap_y = if ny > 1 {
        (area.height - ny as f32 * tile_h) / (ny - 1) as f32
    } else {
        0.0
    };
    let y_count = ny.clamp(1, 4096);
    let x_count = nx.clamp(1, 4096);
    for row in 0..y_count {
        for col in 0..x_count {
            let x = area.x + start.0 + col as f32 * (tile_w + gap_x);
            let y = area.y + start.1 + row as f32 * (tile_h + gap_y);
            builder.commands.push(DisplayCommand::Image {
                rect: CssRect::new(x, y, tile_w, tile_h),
                handle,
                source_rect: None,
                fit: ImageFit::Fill,
                sampling: ImageSampling::Smooth,
                clip: None,
                node,
                link: None,
            });
        }
    }
    builder.commands.push(DisplayCommand::PopClip);
}

/// A rounded contour and its reverse form a nonzero-winding border ring.
/// CSS Backgrounds 3 #corner-shaping subtracts each side's inset from the
/// corresponding outer radius, including unequal border widths.
fn rounded_contour(rect: CssRect, radii: CornerRadii, reverse: bool) -> Vec<PathElement> {
    let x = rect.x;
    let y = rect.y;
    let r = x + rect.width;
    let b = y + rect.height;
    let [(tlx, tly), (trx, try_), (brx, bry), (blx, bly)] = radii.corners;
    let k = 0.5522848;
    let point = CssPoint::new;
    let start = point(x + tlx, y);
    let path = vec![
        PathElement::MoveTo(start),
        PathElement::LineTo(point(r - trx, y)),
        PathElement::CurveTo(
            point(r - trx + k * trx, y),
            point(r, y + try_ - k * try_),
            point(r, y + try_),
        ),
        PathElement::LineTo(point(r, b - bry)),
        PathElement::CurveTo(
            point(r, b - bry + k * bry),
            point(r - brx + k * brx, b),
            point(r - brx, b),
        ),
        PathElement::LineTo(point(x + blx, b)),
        PathElement::CurveTo(
            point(x + blx - k * blx, b),
            point(x, b - bly + k * bly),
            point(x, b - bly),
        ),
        PathElement::LineTo(point(x, y + tly)),
        PathElement::CurveTo(
            point(x, y + tly - k * tly),
            point(x + tlx - k * tlx, y),
            start,
        ),
        PathElement::Close,
    ];
    if !reverse {
        return path;
    }
    let mut previous = start;
    let mut segments = Vec::new();
    for segment in path.into_iter().skip(1) {
        match segment {
            PathElement::LineTo(end) => {
                segments.push(PathElement::LineTo(previous));
                previous = end;
            }
            PathElement::CurveTo(a, b, end) => {
                segments.push(PathElement::CurveTo(b, a, previous));
                previous = end;
            }
            _ => {}
        }
    }
    let mut out = vec![PathElement::MoveTo(start)];
    out.extend(segments.into_iter().rev());
    out.push(PathElement::Close);
    out
}

fn border_ring(
    rect: CssRect,
    radii: CornerRadii,
    widths: [f32; 4],
    start: f32,
    end: f32,
) -> PaintShape {
    let inset = |fraction: f32| {
        CssRect::new(
            rect.x + widths[3] * fraction,
            rect.y + widths[0] * fraction,
            (rect.width - (widths[1] + widths[3]) * fraction).max(0.),
            (rect.height - (widths[0] + widths[2]) * fraction).max(0.),
        )
    };
    let outer = inset(start);
    let inner = inset(end);
    let mut path = rounded_contour(outer, inset_radii(radii, rect, outer), false);
    if inner.width > 0. && inner.height > 0. {
        path.extend(rounded_contour(
            inner,
            inset_radii(radii, rect, inner),
            true,
        ));
    }
    PaintShape::Path(path)
}

fn border_shade(color: PaintColor, light: bool) -> PaintColor {
    let PaintColor::Rgba(r, g, b, a) = color else {
        return color;
    };
    let shade = |v: u8| {
        if light {
            (u16::from(v) + (255 - u16::from(v)) / 3) as u8
        } else {
            (u16::from(v) * 2 / 3) as u8
        }
    };
    PaintColor::Rgba(shade(r), shade(g), shade(b), a)
}

/// CSS Backgrounds and Borders §6: background first, then border. Uniform
/// rounded borders use one true stroked rounded path; non-uniform sides retain
/// each side's own color/style and CSS-pixel width.
fn paint_borders(fragment: &Frag<'_>, radii: CornerRadii, builder: &mut Builder<'_, '_>) {
    let Some(style) = PaintStyle::of(fragment) else {
        return;
    };
    let [top, right, bottom, left] = fragment.border;
    if [top, right, bottom, left].iter().all(|v| *v <= 0.0) {
        return;
    }
    let styles = ["top", "right", "bottom", "left"].map(|side| {
        style
            .value(builder.dom, &format!("border-{side}-style"))
            .unwrap_or_else(|| "none".into())
    });
    let colors = ["top", "right", "bottom", "left"].map(|side| {
        border_color(builder.dom, style, side)
            .unwrap_or_else(|| text_color_for_style(builder.dom, style, false))
    });
    let rect = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    // #line-style permits UA-chosen band thickness and shading, but double
    // must have a gap and the 3D styles must preserve their opposite relief.
    let complex = |s: &str| matches!(s, "double" | "groove" | "ridge" | "inset" | "outset");
    let inner = CssRect::new(
        rect.x + left,
        rect.y + top,
        (rect.width - left - right).max(0.),
        (rect.height - top - bottom).max(0.),
    );
    let corners = |r: CssRect| {
        [
            CssPoint::new(r.x, r.y),
            CssPoint::new(r.x + r.width, r.y),
            CssPoint::new(r.x + r.width, r.y + r.height),
            CssPoint::new(r.x, r.y + r.height),
        ]
    };
    let outside = corners(rect);
    let inside = corners(inner);
    for side in 0..4 {
        if fragment.border[side] <= 0. || !complex(&styles[side]) {
            continue;
        }
        let mut kind = styles[side].as_str();
        if style.value(builder.dom, "border-collapse").as_deref() == Some("collapse") {
            kind = match kind {
                "inset" => "ridge",
                "outset" => "groove",
                other => other,
            };
        }
        let raised = side == 0 || side == 3;
        let light = match kind {
            "ridge" | "outset" => raised,
            _ => !raised,
        };
        let color = colors[side];
        let bands = match kind {
            "double" => vec![(0., 1. / 3., color), (2. / 3., 1., color)],
            "groove" | "ridge" => vec![
                (0., 0.5, border_shade(color, light)),
                (0.5, 1., border_shade(color, !light)),
            ],
            _ => vec![(0., 1., border_shade(color, light))],
        };
        let next = (side + 1) % 4;
        builder
            .commands
            .push(DisplayCommand::PushClip(PaintShape::Polygon {
                points: vec![outside[side], outside[next], inside[next], inside[side]],
                evenodd: false,
            }));
        for (start, end, color) in bands {
            builder.commands.push(DisplayCommand::Fill {
                shape: border_ring(rect, radii, fragment.border, start, end),
                brush: PaintBrush::Solid(color),
            });
        }
        builder.commands.push(DisplayCommand::PopClip);
    }
    let uniform = (top - right).abs() < 0.01
        && (top - bottom).abs() < 0.01
        && (top - left).abs() < 0.01
        && styles.iter().all(|s| s == &styles[0])
        && colors.iter().all(|c| *c == colors[0]);
    if uniform && top > 0.0 && styles[0] != "none" && styles[0] != "hidden" && !complex(&styles[0])
    {
        let inset = top / 2.0;
        let inner = CssRect::new(
            rect.x + inset,
            rect.y + inset,
            (rect.width - top).max(0.0),
            (rect.height - top).max(0.0),
        );
        builder.commands.push(DisplayCommand::Stroke {
            shape: rounded_shape(inner, radii),
            brush: PaintBrush::Solid(colors[0]),
            style: stroke_for_border(top, &styles[0]),
        });
        return;
    }
    let sides = [
        (
            top,
            CssPoint::new(rect.x, rect.y + top / 2.0),
            CssPoint::new(rect.x + rect.width, rect.y + top / 2.0),
        ),
        (
            right,
            CssPoint::new(rect.x + rect.width - right / 2.0, rect.y),
            CssPoint::new(rect.x + rect.width - right / 2.0, rect.y + rect.height),
        ),
        (
            bottom,
            CssPoint::new(rect.x, rect.y + rect.height - bottom / 2.0),
            CssPoint::new(rect.x + rect.width, rect.y + rect.height - bottom / 2.0),
        ),
        (
            left,
            CssPoint::new(rect.x + left / 2.0, rect.y),
            CssPoint::new(rect.x + left / 2.0, rect.y + rect.height),
        ),
    ];
    for (index, (width, start, end)) in sides.into_iter().enumerate() {
        if width <= 0.0
            || matches!(styles[index].as_str(), "none" | "hidden")
            || complex(&styles[index])
        {
            continue;
        }
        builder.commands.push(DisplayCommand::Stroke {
            shape: PaintShape::Path(vec![PathElement::MoveTo(start), PathElement::LineTo(end)]),
            brush: PaintBrush::Solid(colors[index]),
            style: stroke_for_border(width, &styles[index]),
        });
    }
}

fn paint_box_shadows(
    dom: &Dom,
    style: PaintStyle,
    shape: &PaintShape,
    builder: &mut Builder<'_, '_>,
) {
    let Some(value) = style.value(dom, "box-shadow") else {
        return;
    };
    for shadow in split_top_level(&value, ',') {
        if shadow.trim().eq_ignore_ascii_case("none") {
            continue;
        }
        let tokens = split_ws(shadow);
        let inset = tokens.iter().any(|t| t.eq_ignore_ascii_case("inset"));
        let color = tokens
            .iter()
            .find_map(|token| PaintColor::parse_css(token))
            .unwrap_or(PaintColor::Rgba(0, 0, 0, 85));
        let lengths: Vec<f32> = tokens.iter().filter_map(|t| px(t)).collect();
        if lengths.len() < 2 {
            continue;
        }
        builder.commands.push(DisplayCommand::Shadow {
            shape: shape.clone(),
            color,
            offset: CssPoint::new(lengths[0], lengths[1]),
            blur_radius: lengths.get(2).copied().unwrap_or(0.0).max(0.0),
            spread: lengths.get(3).copied().unwrap_or(0.0),
            inset,
        });
    }
}

/// CSS Text Decoration 4 §4: parse each comma-separated shadow independently;
/// its first two lengths are offsets, followed by optional non-negative blur
/// and spread distances. The first authored layer is frontmost, so retain
/// author order and let the renderer paint the vector back-to-front.
fn text_shadows(dom: &Dom, node: NodeId, current_color: PaintColor) -> Vec<TextShadowPaint> {
    if node == NO_NODE {
        return Vec::new();
    }
    let Some(value) = dom.computed_value_resolved(node, "text-shadow") else {
        return Vec::new();
    };
    if value.trim().eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    let units = Units::of(dom, node);
    // Resource-bound hostile declarations while keeping substantially more
    // layers than ordinary outline recipes (Burgeritchi uses 26).
    split_top_level(&value, ',')
        .into_iter()
        .take(128)
        .filter_map(|shadow| {
            let tokens = split_ws(shadow);
            let inset = tokens
                .iter()
                .any(|token| token.eq_ignore_ascii_case("inset"));
            let color = tokens
                .iter()
                .find_map(|token| resolve_color(dom, node, token))
                .unwrap_or(current_color);
            let lengths = tokens
                .iter()
                .filter_map(|token| super::css_length_px(token, units))
                .collect::<Vec<_>>();
            if lengths.len() < 2 || lengths.len() > 4 {
                return None;
            }
            let blur_radius = lengths.get(2).copied().unwrap_or(0.0);
            let spread = lengths.get(3).copied().unwrap_or(0.0);
            if blur_radius < 0.0 || spread < 0.0 {
                return None;
            }
            Some(TextShadowPaint {
                color,
                offset: CssPoint::new(lengths[0], lengths[1]),
                blur_radius,
                spread,
                inset,
            })
        })
        .collect()
}

fn stroke_for_border(width: f32, style: &str) -> StrokeStyle {
    let mut stroke = StrokeStyle::solid(width);
    match style {
        "dotted" => {
            stroke.dash = vec![0.0, width * 2.0];
            stroke.cap = LineCap::Round;
        }
        "dashed" => stroke.dash = vec![width * 3.0, width * 2.0],
        _ => {}
    }
    stroke
}

fn rounded_overflow_clip(dom: &Dom, fragment: &Frag<'_>) -> Option<PaintShape> {
    if matches!(dom.tag_name(fragment.node), Some("html" | "body")) {
        return None;
    }
    let style = PaintStyle::Element(fragment.node);
    let shorthand = style.value(dom, "overflow").unwrap_or_default();
    let mut parts = shorthand.split_whitespace();
    let sx = parts.next().unwrap_or("visible");
    let sy = parts.next().unwrap_or(sx);
    let x = style.value(dom, "overflow-x");
    let y = style.value(dom, "overflow-y");
    let mut x = x.as_deref().unwrap_or(sx).trim();
    let mut y = y.as_deref().unwrap_or(sy).trim();
    let scrollable = |value| matches!(value, "hidden" | "auto" | "scroll" | "overlay");
    // CSS Overflow 3 §3.1: visible computes to auto opposite a scrollable
    // axis. clip/visible, in contrast, explicitly has no rounded clip.
    if x == "visible" && scrollable(y) {
        x = "auto";
    }
    if y == "visible" && scrollable(x) {
        y = "auto";
    }
    if !(scrollable(x) && scrollable(y) || x == "clip" && y == "clip") {
        return None;
    }
    let border = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    let radii = border_radii(dom, style, border);
    if !radii.corners.iter().any(|&(x, y)| x > 0. && y > 0.) {
        return None;
    }
    // CSS Backgrounds 3 §4.2/§4.3: overflow rounds the padding edge,
    // unlike a replaced element's own content-edge clip.
    let padding = padding_box(fragment);
    Some(rounded_shape(padding, inset_radii(radii, border, padding)))
}

fn border_radii(dom: &Dom, style: PaintStyle, rect: CssRect) -> CornerRadii {
    let mut units = None;
    let (w, h) = dom.viewport_px();
    let names = [
        "border-top-left-radius",
        "border-top-right-radius",
        "border-bottom-right-radius",
        "border-bottom-left-radius",
    ];
    let mut corners = [(0.0, 0.0); 4];
    for (index, name) in names.into_iter().enumerate() {
        let Some(value) = style.value(dom, name) else {
            continue;
        };
        let parts = split_ws(&value);
        let Some(first) = parts.first() else {
            continue;
        };
        let units = *units.get_or_insert_with(|| Units::of(dom, style.node()));
        let radius = |text: &str, basis| Len::parse(text, units, Vp { w, h })?.resolve(Some(basis));
        let x = radius(first, rect.width).unwrap_or(0.0);
        // A single percentage is copied as a value, not as its used horizontal
        // length. The vertical percentage basis is the border-box height.
        let y = radius(parts.get(1).unwrap_or(first), rect.height).unwrap_or(0.0);
        corners[index] = (x.max(0.0).min(f32::MAX), y.max(0.0).min(f32::MAX));
    }
    // CSS Backgrounds §5.5: proportionally reduce overlapping radii. CSS
    // Values 4 permits calc(infinity * 1px), clamped to our finite f32 limit.
    // Sum and scale in f64: adding two valid large f32 radii can otherwise
    // overflow, produce a zero factor, and turn a round pill into a square.
    let sums = [
        (
            f64::from(corners[0].0) + f64::from(corners[1].0),
            rect.width,
        ),
        (
            f64::from(corners[3].0) + f64::from(corners[2].0),
            rect.width,
        ),
        (
            f64::from(corners[0].1) + f64::from(corners[3].1),
            rect.height,
        ),
        (
            f64::from(corners[1].1) + f64::from(corners[2].1),
            rect.height,
        ),
    ];
    let factor = sums
        .into_iter()
        .filter(|(sum, _)| *sum > 0.0)
        .map(|(sum, side)| f64::from(side) / sum)
        .fold(1.0f64, f64::min)
        .min(1.0);
    for corner in &mut corners {
        corner.0 = (f64::from(corner.0) * factor) as f32;
        corner.1 = (f64::from(corner.1) * factor) as f32;
    }
    CornerRadii { corners }
}

fn inset_radii(mut radii: CornerRadii, outer: CssRect, inner: CssRect) -> CornerRadii {
    let left = (inner.x - outer.x).max(0.);
    let top = (inner.y - outer.y).max(0.);
    let right = (outer.x + outer.width - inner.x - inner.width).max(0.);
    let bottom = (outer.y + outer.height - inner.y - inner.height).max(0.);
    for (corner, (x, y)) in
        radii
            .corners
            .iter_mut()
            .zip([(left, top), (right, top), (right, bottom), (left, bottom)])
    {
        corner.0 = (corner.0 - x).max(0.);
        corner.1 = (corner.1 - y).max(0.);
    }
    radii
}

fn rounded_shape(rect: CssRect, radii: CornerRadii) -> PaintShape {
    if radii.corners.iter().all(|&(x, y)| x == 0.0 && y == 0.0) {
        PaintShape::Rect(rect)
    } else {
        PaintShape::RoundedRect { rect, radii }
    }
}

/// CSS Images 4 #linear-gradients and CSS Color 4 #color-interpolation-method.
/// The method and direction can appear in either order, but neither component
/// may be interleaved. Preserve the selected space and polar hue path.
fn gradient_interpolation(value: &str) -> Option<(String, GradientInterpolation, bool)> {
    use color::{ColorSpaceTag as Space, HueDirection as Hue};
    let lower = value.to_ascii_lowercase();
    let tokens = split_ws(&lower);
    let Some(index) = tokens.iter().position(|token| *token == "in") else {
        return Some((value.trim().into(), GradientInterpolation::default(), false));
    };
    let space = match *tokens.get(index + 1)? {
        "srgb" => Space::Srgb,
        "srgb-linear" => Space::LinearSrgb,
        "display-p3" => Space::DisplayP3,
        "a98-rgb" => Space::A98Rgb,
        "prophoto-rgb" => Space::ProphotoRgb,
        "rec2020" => Space::Rec2020,
        "lab" => Space::Lab,
        "lch" => Space::Lch,
        "oklab" => Space::Oklab,
        "oklch" => Space::Oklch,
        "hsl" => Space::Hsl,
        "hwb" => Space::Hwb,
        "xyz" | "xyz-d65" => Space::XyzD65,
        "xyz-d50" => Space::XyzD50,
        _ => return None,
    };
    let mut end = index + 2;
    let mut hue = Hue::Shorter;
    if let Some(mode) = tokens.get(end).and_then(|token| match *token {
        "shorter" => Some(Hue::Shorter),
        "longer" => Some(Hue::Longer),
        "increasing" => Some(Hue::Increasing),
        "decreasing" => Some(Hue::Decreasing),
        _ => None,
    }) {
        if !matches!(space, Space::Lch | Space::Oklch | Space::Hsl | Space::Hwb)
            || tokens.get(end + 1) != Some(&"hue")
        {
            return None;
        }
        hue = mode;
        end += 2;
    }
    let direction = if index == 0 {
        tokens[end..].join(" ")
    } else if end == tokens.len() {
        tokens[..index].join(" ")
    } else {
        return None;
    };
    Some((direction, GradientInterpolation { space, hue }, true))
}

fn parse_gradient(value: &str, rect: CssRect) -> Option<PaintBrush> {
    let lower = value.to_ascii_lowercase();
    let (radial, repeating, body) = if lower.starts_with("linear-gradient(") {
        (false, false, function_body(value)?)
    } else if lower.starts_with("repeating-linear-gradient(") {
        (false, true, function_body(value)?)
    } else if lower.starts_with("radial-gradient(") {
        (true, false, function_body(value)?)
    } else if lower.starts_with("repeating-radial-gradient(") {
        (true, true, function_body(value)?)
    } else {
        return None;
    };
    let mut parts = split_top_level(body, ',');
    if parts.len() < 2 {
        return None;
    }
    let mut angle = PI;
    let (header, interpolation, explicit_interpolation) = gradient_interpolation(parts[0])?;
    if !radial {
        if let Some(parsed) = gradient_direction(&header) {
            angle = parsed;
            parts.remove(0);
        } else if explicit_interpolation {
            if !header.is_empty() {
                return None;
            }
            parts.remove(0);
        }
    } else if explicit_interpolation || color::parse_color(split_ws(parts[0]).first()?).is_err() {
        // Shape/size/position retain the existing default geometry path.
        parts.remove(0);
    }
    let mut stops = parse_stops(&parts)?;
    if repeating && stops.last().is_some_and(|stop| stop.offset > 0.0) {
        let end = stops.last().unwrap().offset;
        for stop in &mut stops {
            stop.offset /= end;
        }
    }
    if radial {
        Some(PaintBrush::RadialGradient {
            center: CssPoint::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0),
            radius: rect.width.hypot(rect.height) / 2.0,
            stops,
            interpolation,
        })
    } else {
        let center = CssPoint::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
        let dx = angle.sin();
        let dy = -angle.cos();
        let half = (rect.width * dx.abs() + rect.height * dy.abs()) / 2.0;
        Some(PaintBrush::LinearGradient {
            start: CssPoint::new(center.x - dx * half, center.y - dy * half),
            end: CssPoint::new(center.x + dx * half, center.y + dy * half),
            stops,
            interpolation,
        })
    }
}

fn parse_stops(parts: &[&str]) -> Option<Vec<GradientStop>> {
    let mut stops = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let tokens = split_ws(part);
        let color = color::parse_color(tokens.first()?).ok()?;
        let offset = tokens
            .get(1)
            .and_then(|v| v.strip_suffix('%'))
            .and_then(|v| v.parse::<f32>().ok())
            .map(|v| v / 100.0)
            .unwrap_or_else(|| index as f32 / (parts.len() - 1).max(1) as f32);
        stops.push(GradientStop {
            offset: offset.clamp(0.0, 1.0),
            color,
        });
    }
    Some(stops)
}

fn gradient_direction(value: &str) -> Option<f32> {
    let value = value.trim().to_ascii_lowercase();
    if let Some(angle) = angle(&value) {
        return Some(angle);
    }
    Some(match value.as_str() {
        "to top" => 0.0,
        "to top right" | "to right top" => PI / 4.0,
        "to right" => FRAC_PI_2,
        "to bottom right" | "to right bottom" => PI * 3.0 / 4.0,
        "to bottom" => PI,
        "to bottom left" | "to left bottom" => PI * 5.0 / 4.0,
        "to left" => PI * 3.0 / 2.0,
        "to top left" | "to left top" => PI * 7.0 / 4.0,
        _ => return None,
    })
}

fn element_transform(
    dom: &Dom,
    style: PaintStyle,
    width: f32,
    height: f32,
    x: f32,
    y: f32,
) -> Option<(Affine2d, CssPoint)> {
    let transform = style
        .value(dom, "transform")
        .unwrap_or_else(|| "none".into());
    let translate = style
        .value(dom, "translate")
        .unwrap_or_else(|| "none".into());
    if transform.trim().eq_ignore_ascii_case("none")
        && translate.trim().eq_ignore_ascii_case("none")
    {
        return None;
    }
    let mut matrix = Affine2d::IDENTITY;
    let mut layout_translation = CssPoint::default();
    if !translate.trim().eq_ignore_ascii_case("none") {
        let parts = split_ws(&translate);
        let tx = transform_length(parts.first().copied().unwrap_or("0"), width)?;
        let ty = transform_length(parts.get(1).copied().unwrap_or("0"), height)?;
        matrix = matrix.then(Affine2d::translate(tx, ty));
        layout_translation.x += tx;
        layout_translation.y += ty;
    }
    if !transform.trim().eq_ignore_ascii_case("none") {
        for (name, args) in transform_functions(&transform)? {
            let next = match name.as_str() {
                "matrix" if args.len() == 6 => {
                    let values: Vec<f32> = args
                        .iter()
                        .map(|value| value.parse::<f32>().ok())
                        .collect::<Option<_>>()?;
                    layout_translation.x += values[4];
                    layout_translation.y += values[5];
                    Affine2d(values.try_into().ok()?)
                }
                "translate" => {
                    let tx = transform_length(args.first()?, width)?;
                    let ty = transform_length(args.get(1).map_or("0", String::as_str), height)?;
                    layout_translation.x += tx;
                    layout_translation.y += ty;
                    Affine2d::translate(tx, ty)
                }
                "translatex" => {
                    let tx = transform_length(args.first()?, width)?;
                    layout_translation.x += tx;
                    Affine2d::translate(tx, 0.0)
                }
                "translatey" => {
                    let ty = transform_length(args.first()?, height)?;
                    layout_translation.y += ty;
                    Affine2d::translate(0.0, ty)
                }
                "translate3d" => {
                    let tx = transform_length(args.first()?, width)?;
                    let ty = transform_length(args.get(1)?, height)?;
                    layout_translation.x += tx;
                    layout_translation.y += ty;
                    Affine2d::translate(tx, ty)
                }
                "scale" => {
                    let sx = args.first()?.parse::<f32>().ok()?;
                    let sy = args.get(1).map_or(Some(sx), |v| v.parse().ok())?;
                    Affine2d::scale(sx, sy)
                }
                "scalex" => Affine2d::scale(args.first()?.parse().ok()?, 1.0),
                "scaley" => Affine2d::scale(1.0, args.first()?.parse().ok()?),
                "scale3d" => {
                    Affine2d::scale(args.first()?.parse().ok()?, args.get(1)?.parse().ok()?)
                }
                "rotate" => rotate(angle(args.first()?)?),
                "rotatez" => rotate(angle(args.first()?)?),
                "skewx" => Affine2d([1.0, 0.0, angle(args.first()?)?.tan(), 1.0, 0.0, 0.0]),
                "skewy" => Affine2d([1.0, angle(args.first()?)?.tan(), 0.0, 1.0, 0.0, 0.0]),
                "skew" => Affine2d([
                    1.0,
                    args.get(1).and_then(|v| angle(v)).unwrap_or(0.0).tan(),
                    angle(args.first()?)?.tan(),
                    1.0,
                    0.0,
                    0.0,
                ]),
                // 3D transforms are retained in style but cannot be projected
                // by this 2D display-list phase without inventing semantics.
                _ => continue,
            };
            matrix = matrix.then(next);
        }
    }
    let origin = style
        .value(dom, "transform-origin")
        .unwrap_or_else(|| "50% 50%".into());
    let parts = split_ws(&origin);
    let ox =
        x - layout_translation.x + transform_origin(parts.first().copied().unwrap_or("50%"), width);
    let oy =
        y - layout_translation.y + transform_origin(parts.get(1).copied().unwrap_or("50%"), height);
    let around = Affine2d::translate(ox, oy)
        .then(matrix)
        .then(Affine2d::translate(-ox, -oy));
    Some((around, layout_translation))
}

fn transform_functions(value: &str) -> Option<Vec<(String, Vec<String>)>> {
    let mut result = Vec::new();
    let mut rest = value.trim();
    while !rest.is_empty() {
        let open = rest.find('(')?;
        let name = rest[..open].trim().to_ascii_lowercase();
        let mut depth = 0;
        let close = rest[open..].char_indices().find_map(|(i, ch)| {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(open + i);
                    }
                }
                _ => {}
            }
            None
        })?;
        let body = &rest[open + 1..close];
        let args: Vec<String> = if body.contains(',') {
            split_top_level(body, ',')
                .into_iter()
                .map(|v| v.trim().into())
                .collect()
        } else {
            split_ws(body).into_iter().map(String::from).collect()
        };
        result.push((name, args));
        rest = rest[close + 1..].trim_start();
    }
    Some(result)
}

fn rotate(radians: f32) -> Affine2d {
    let (sin, cos) = radians.sin_cos();
    Affine2d([cos, sin, -sin, cos, 0.0, 0.0])
}

fn angle(value: &str) -> Option<f32> {
    let value = value.trim();
    if let Some(v) = value.strip_suffix("deg") {
        v.trim().parse::<f32>().ok().map(f32::to_radians)
    } else if let Some(v) = value.strip_suffix("rad") {
        v.trim().parse().ok()
    } else if let Some(v) = value.strip_suffix("turn") {
        v.trim().parse::<f32>().ok().map(|v| v * 2.0 * PI)
    } else if value == "0" {
        Some(0.0)
    } else {
        None
    }
}

fn transform_length(value: &str, basis: f32) -> Option<f32> {
    let value = value.trim();
    if let Some(v) = value.strip_suffix('%') {
        return v.trim().parse::<f32>().ok().map(|v| v * basis / 100.0);
    }
    px(value)
}

fn transform_origin(value: &str, basis: f32) -> f32 {
    match value.trim().to_ascii_lowercase().as_str() {
        "left" | "top" => 0.0,
        "center" => basis / 2.0,
        "right" | "bottom" => basis,
        other => transform_length(other, basis).unwrap_or(basis / 2.0),
    }
}

fn resolve_image_source(base: &Url, source: &str) -> String {
    if source.starts_with("data:") || source.starts_with("blob:") {
        source.to_string()
    } else {
        base.join(source)
            .map_or_else(|_| source.to_string(), |url| url.to_string())
    }
}

fn background_color(dom: &Dom, node: NodeId) -> Option<PaintColor> {
    background_color_for_style(dom, PaintStyle::Element(node))
}

fn cursor_value(dom: &Dom, node: NodeId) -> Option<String> {
    (node != NO_NODE)
        .then(|| dom.computed_value_resolved(node, "cursor"))
        .flatten()
        .filter(|value| !value.trim().is_empty())
}

fn background_color_for_style(dom: &Dom, style: PaintStyle) -> Option<PaintColor> {
    style
        .value(dom, "background-color")
        .as_deref()
        .and_then(|value| resolve_color_for_style(dom, style, value))
}

fn text_color(dom: &Dom, node: NodeId, link: bool) -> PaintColor {
    if node != NO_NODE {
        return text_color_for_style(dom, PaintStyle::Element(node), link);
    }
    if link {
        PaintColor::Rgba(0, 70, 190, 255)
    } else {
        PaintColor::Rgba(20, 20, 20, 255)
    }
}

fn text_color_for_style(dom: &Dom, style: PaintStyle, link: bool) -> PaintColor {
    if let Some(color) = style
        .value(dom, "color")
        .as_deref()
        .and_then(|value| resolve_color_for_style(dom, style, value))
    {
        return color;
    }
    if link {
        PaintColor::Rgba(0, 70, 190, 255)
    } else {
        PaintColor::Rgba(20, 20, 20, 255)
    }
}

fn resolve_color(dom: &Dom, node: NodeId, value: &str) -> Option<PaintColor> {
    resolve_color_for_style(dom, PaintStyle::Element(node), value)
}

fn resolve_color_for_style(dom: &Dom, style: PaintStyle, value: &str) -> Option<PaintColor> {
    if value.trim().eq_ignore_ascii_case("currentcolor") {
        return style
            .value(dom, "color")
            .filter(|color| !color.trim().eq_ignore_ascii_case("currentcolor"))
            .as_deref()
            .and_then(PaintColor::parse_css);
    }
    PaintColor::parse_css(value)
}

fn border_color(dom: &Dom, style: PaintStyle, side: &str) -> Option<PaintColor> {
    style
        .value(dom, &format!("border-{side}-color"))
        .as_deref()
        .and_then(|value| resolve_color_for_style(dom, style, value))
}

fn decoration_color(dom: &Dom, node: NodeId) -> Option<PaintColor> {
    if node == NO_NODE {
        return None;
    }
    dom.computed_value_resolved(node, "text-decoration-color")
        .as_deref()
        .and_then(|value| resolve_color(dom, node, value))
}

fn decoration_style(dom: &Dom, node: NodeId) -> DecorationStyle {
    if node == NO_NODE {
        return DecorationStyle::Solid;
    }
    match dom
        .computed_value_resolved(node, "text-decoration-style")
        .as_deref()
        .map(str::trim)
    {
        Some("double") => DecorationStyle::Double,
        Some("dotted") => DecorationStyle::Dotted,
        Some("dashed") => DecorationStyle::Dashed,
        Some("wavy") => DecorationStyle::Wavy,
        _ => DecorationStyle::Solid,
    }
}

fn blend_mode(value: &str) -> BlendMode {
    match value.trim() {
        "multiply" => BlendMode::Multiply,
        "screen" => BlendMode::Screen,
        "overlay" => BlendMode::Overlay,
        "darken" => BlendMode::Darken,
        "lighten" => BlendMode::Lighten,
        "difference" => BlendMode::Difference,
        "exclusion" => BlendMode::Exclusion,
        _ => BlendMode::Normal,
    }
}

/// Return a finite rectangle covering all paintable fragment borders. The
/// fragment clip is applied while finding the extent, so intentionally huge
/// overflow-hidden probes do not turn an unbounded display-list clip into a
/// huge raster path. The viewport compositor still supplies the final screen
/// clip; this extent only replaces CSS's conceptual unbounded axis.
fn paint_extent(
    root: &Frag<'_>,
    fixed: &[Frag<'_>],
    top_layer: &[TopFrag<'_>],
    flow_bottom: f32,
) -> CssRect {
    let mut bounds = (
        0.0_f32,
        0.0_f32,
        1.0_f32,
        flow_bottom.max(root.max_bottom()).max(1.0),
    );

    fn visit(fragment: &Frag<'_>, bounds: &mut (f32, f32, f32, f32)) {
        let (mut x0, mut y0, mut x1, mut y1) = (
            fragment.x,
            fragment.y,
            fragment.x + fragment.w,
            fragment.y + fragment.h,
        );
        if let Some(clip) = fragment.clip {
            if clip.x0.is_finite() {
                x0 = x0.max(clip.x0);
            }
            if clip.y0.is_finite() {
                y0 = y0.max(clip.y0);
            }
            if clip.x1.is_finite() {
                x1 = x1.min(clip.x1);
            }
            if clip.y1.is_finite() {
                y1 = y1.min(clip.y1);
            }
        }
        if x0.is_finite()
            && y0.is_finite()
            && x1.is_finite()
            && y1.is_finite()
            && x1 > x0
            && y1 > y0
        {
            bounds.0 = bounds.0.min(x0);
            bounds.1 = bounds.1.min(y0);
            bounds.2 = bounds.2.max(x1);
            bounds.3 = bounds.3.max(y1);
        }
        for child in &fragment.children {
            visit(child, bounds);
        }
    }

    visit(root, &mut bounds);
    for fragment in fixed {
        visit(fragment, &mut bounds);
    }
    for top in top_layer {
        visit(&top.fragment, &mut bounds);
    }
    CssRect::new(
        bounds.0,
        bounds.1,
        (bounds.2 - bounds.0).max(1.0),
        (bounds.3 - bounds.1).max(1.0),
    )
}

fn padding_box(fragment: &Frag<'_>) -> CssRect {
    let [top, right, bottom, left] = fragment.border;
    CssRect::new(
        fragment.x + left,
        fragment.y + top,
        (fragment.w - left - right).max(0.0),
        (fragment.h - top - bottom).max(0.0),
    )
}

fn function_body(value: &str) -> Option<&str> {
    let open = value.find('(')?;
    let close = value.rfind(')')?;
    (close > open).then_some(&value[open + 1..close])
}

fn css_url(value: &str) -> Option<String> {
    let body = function_body(value)?;
    value[..value.find('(')?]
        .trim()
        .eq_ignore_ascii_case("url")
        .then(|| body.trim().trim_matches(['\'', '"']).to_string())
}

fn split_top_level(value: &str, separator: char) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, ch) in value.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ch if ch == separator && depth == 0 => {
                result.push(&value[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    result.push(&value[start..]);
    result
}

fn split_ws(value: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = None;
    for (index, ch) in value.char_indices() {
        let boundary = depth == 0 && ch.is_whitespace();
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if boundary {
            if let Some(begin) = start.take() {
                result.push(&value[begin..index]);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        result.push(&value[begin..]);
    }
    result
}

fn px(value: &str) -> Option<f32> {
    let value = value.trim();
    if value == "0" {
        return Some(0.0);
    }
    value.strip_suffix("px")?.trim().parse().ok()
}

impl PaintColor {
    pub fn parse_css(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("transparent") {
            return Some(Self::Rgba(0, 0, 0, 0));
        }
        // Added by CSS Color 4 after the CSS3/SVG named-color table used by
        // svgtypes.
        if value.eq_ignore_ascii_case("rebeccapurple") {
            return Some(Self::Rgba(102, 51, 153, 255));
        }
        if let Ok(color) = svgtypes::Color::from_str(value) {
            return Some(Self::Rgba(color.red, color.green, color.blue, color.alpha));
        }
        parse_modern_rgb(value)
            .or_else(|| parse_hsl(value))
            .or_else(|| parse_perceptual_or_p3(value))
    }

    pub fn is_transparent(self) -> bool {
        matches!(self, Self::Rgba(_, _, _, 0))
    }
}

/// CSS Color 4 #color-function, #specifying-lab-lch and #specifying-oklab-oklch,
/// using the local 2026-09-06
/// CSSWG snapshot (81c27f686901). Display P3 uses D65; Lab uses D50, so the
/// conversion includes Bradford white-point adaptation. Keep source components
/// unclipped until actual-value conversion to our sRGB paint surface.
fn parse_perceptual_or_p3(value: &str) -> Option<PaintColor> {
    use color::ColorSpaceTag::{DisplayP3, Lab, Lch, Oklab, Oklch};
    let origin = color::parse_color(value).ok()?;
    if !matches!(origin.cs, DisplayP3 | Lab | Lch | Oklab | Oklch)
        || !origin
            .components
            .iter()
            .all(|component| component.is_finite())
    {
        return None;
    }
    let alpha = origin.components[3];
    // Perceptual lightness endpoints display as black/white regardless of
    // chroma. Lab/LCH use [0,100], Oklab/OkLCh [0,1] (CSS Color 4 §9).
    let lightness_max = match origin.cs {
        Lab | Lch => Some(100.0),
        Oklab | Oklch => Some(1.0),
        _ => None,
    };
    let rgba = if lightness_max.is_some() && origin.components[0] <= 0.0 {
        [0.0, 0.0, 0.0, alpha]
    } else if lightness_max.is_some_and(|max| origin.components[0] >= max) {
        [1.0, 1.0, 1.0, alpha]
    } else {
        gamut_map_to_srgb(origin)?
    };
    let [r, g, b, a] = rgba.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
    Some(PaintColor::Rgba(r, g, b, a))
}

/// CSS Color 4 #pseudo-binsearch: binary search with local MINDE. A simple
/// RGB channel clamp would distort out-of-gamut colors; reduce OkLCh chroma
/// while keeping lightness/hue, allowing a just-noticeable clipping delta.
fn gamut_map_to_srgb(origin: color::DynamicColor) -> Option<[f32; 4]> {
    use color::{ColorSpaceTag, DynamicColor};
    let in_gamut = |rgb: DynamicColor| rgb.components[..3].iter().all(|v| (0.0..=1.0).contains(v));
    let delta = |one: DynamicColor, two: DynamicColor| {
        let one = one.convert(ColorSpaceTag::Oklab).components;
        let two = two.convert(ColorSpaceTag::Oklab).components;
        ((one[0] - two[0]).powi(2) + (one[1] - two[1]).powi(2) + (one[2] - two[2]).powi(2)).sqrt()
    };
    let rgb = origin.convert(ColorSpaceTag::Srgb);
    if in_gamut(rgb) {
        return Some(rgb.components);
    }
    // Clear missing-component flags after resolving `none` to zero; those
    // flags are for interpolation, not the actual-value gamut search.
    let mut current = DynamicColor::from_alpha_color(origin.to_alpha_color::<color::Oklch>());
    if !current.components.iter().all(|v| v.is_finite()) {
        return None;
    }
    let [lightness, chroma, _, alpha] = current.components;
    if lightness >= 1.0 {
        return Some([1.0, 1.0, 1.0, alpha]);
    }
    if lightness <= 0.0 {
        return Some([0.0, 0.0, 0.0, alpha]);
    }
    const JND: f32 = 0.02;
    const EPSILON: f32 = 0.0001;
    let mut clipped = rgb.clip();
    if delta(clipped, current) < JND {
        return Some(clipped.components);
    }
    let (mut min, mut max) = (0.0, chroma);
    let mut min_in_gamut = true;
    while max - min > EPSILON {
        let chroma = min + (max - min) / 2.0;
        // Extremely large finite coordinates can exhaust f32 precision.
        if chroma <= min || chroma >= max {
            break;
        }
        current.components[1] = chroma;
        let rgb = current.convert(ColorSpaceTag::Srgb);
        if min_in_gamut && in_gamut(rgb) {
            min = chroma;
            continue;
        }
        clipped = rgb.clip();
        let error = delta(clipped, current);
        if error < JND {
            if JND - error < EPSILON {
                return Some(clipped.components);
            }
            min_in_gamut = false;
            min = chroma;
        } else {
            max = chroma;
        }
    }
    Some(clipped.components)
}

fn parse_modern_rgb(value: &str) -> Option<PaintColor> {
    let body = function_body(value)?;
    let name = value[..value.find('(')?].trim().to_ascii_lowercase();
    if !matches!(name.as_str(), "rgb" | "rgba") || body.contains(',') {
        return None;
    }
    let (rgb, alpha) = body
        .split_once('/')
        .map_or((body, None), |(rgb, a)| (rgb, Some(a)));
    let components = split_ws(rgb);
    if components.len() != 3 {
        return None;
    }
    let channel = |value: &str| {
        value
            .strip_suffix('%')
            .and_then(|v| v.trim().parse::<f32>().ok())
            .map(|v| v * 2.55)
            .or_else(|| value.parse::<f32>().ok())
            .map(|v| v.clamp(0.0, 255.0).round() as u8)
    };
    Some(PaintColor::Rgba(
        channel(components[0])?,
        channel(components[1])?,
        channel(components[2])?,
        alpha.and_then(alpha_byte).unwrap_or(255),
    ))
}

fn parse_hsl(value: &str) -> Option<PaintColor> {
    let body = function_body(value)?;
    let name = value[..value.find('(')?].trim().to_ascii_lowercase();
    if !matches!(name.as_str(), "hsl" | "hsla") || body.contains(',') {
        return None;
    }
    let (hsl, alpha) = body
        .split_once('/')
        .map_or((body, None), |(hsl, a)| (hsl, Some(a)));
    let parts = split_ws(hsl);
    if parts.len() != 3 {
        return None;
    }
    let h = parts[0]
        .trim_end_matches("deg")
        .parse::<f32>()
        .ok()?
        .rem_euclid(360.0)
        / 360.0;
    let s = parts[1]
        .strip_suffix('%')?
        .parse::<f32>()
        .ok()?
        .clamp(0.0, 100.0)
        / 100.0;
    let l = parts[2]
        .strip_suffix('%')?
        .parse::<f32>()
        .ok()?
        .clamp(0.0, 100.0)
        / 100.0;
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue = |mut t: f32| {
        t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    Some(PaintColor::Rgba(
        (hue(h + 1.0 / 3.0) * 255.0).round() as u8,
        (hue(h) * 255.0).round() as u8,
        (hue(h - 1.0 / 3.0) * 255.0).round() as u8,
        alpha.and_then(alpha_byte).unwrap_or(255),
    ))
}

fn alpha_byte(value: &str) -> Option<u8> {
    value
        .trim()
        .strip_suffix('%')
        .and_then(|v| v.trim().parse::<f32>().ok())
        .map(|v| v / 100.0)
        .or_else(|| value.trim().parse::<f32>().ok())
        .map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_fixture(html: &str) -> (Dom, crate::layout2::GraphicalLayout) {
        let mut dom = Dom::parse_document(html);
        dom.set_render_clickables(Default::default(), true);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport::new(800., 600.),
            &[],
            &Default::default(),
            &Default::default(),
        );
        (dom, layout)
    }

    #[test]
    fn gradient_interpolation_syntax_preserves_color_space_hue_and_alpha() {
        use color::{ColorSpaceTag as Space, HueDirection as Hue};
        let rect = CssRect::new(0., 0., 100., 100.);
        for header in ["to bottom in oklab", "in oklab to bottom", "in oklab"] {
            let brush = parse_gradient(&format!("linear-gradient({header},rgba(22,22,22,.9) 0%,rgba(22,22,22,.5) 40%,transparent 97%)"),rect).unwrap();
            let PaintBrush::LinearGradient {
                interpolation,
                start,
                end,
                stops,
            } = brush
            else {
                panic!()
            };
            assert_eq!(interpolation.space, Space::Oklab);
            assert!(end.y > start.y);
            assert_eq!(stops.len(), 3);
            assert_eq!(
                stops[0].color,
                color::parse_color("rgba(22,22,22,.9)").unwrap()
            );
        }
        let PaintBrush::LinearGradient {
            interpolation,
            stops,
            ..
        } = parse_gradient(
            "linear-gradient(in oklch longer hue to right,oklch(.6 .4 20),oklch(.7 none 310 / .5))",
            rect,
        )
        .unwrap()
        else {
            panic!()
        };
        assert_eq!(interpolation.space, Space::Oklch);
        assert_eq!(interpolation.hue, Hue::Longer);
        assert_eq!(
            stops[1].color,
            color::parse_color("oklch(.7 none 310 / .5)").unwrap()
        );
        for invalid in [
            "in unknown",
            "in oklab longer hue",
            "to in oklab bottom",
            "in oklch hue",
            "in oklch longer",
        ] {
            assert!(
                parse_gradient(&format!("linear-gradient({invalid},red,blue)"), rect).is_none(),
                "{invalid}"
            );
        }
        for (method, expected) in [
            ("in srgb", 128u8),
            ("in srgb-linear", 188),
            ("in oklab", 99),
        ] {
            let (_, layout) = render_fixture(&format!(
                "<style>body{{margin:0}}div{{width:100px;height:20px;background-image:linear-gradient(to right {method},black,white)}}</style><div></div>"
            ));
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            let actual = frame.pixels[(10 * 800 + 50) * 4];
            assert!(
                actual.abs_diff(expected) <= 3,
                "{method}: {actual} != {expected}"
            );
        }
    }

    #[test]
    fn generated_list_markers_follow_their_scrollport() {
        let (dom, layout) = render_fixture(
            r#"<style>body{margin:0} #panel{height:40px;overflow:auto} li{height:30px}</style><ol id=panel><li>one</li><li>two</li><li>three</li></ol>"#,
        );
        let panel = dom.get_by_id("panel").unwrap();
        let mut scrolls = Vec::new();
        let mut markers = 0;
        for command in &layout.paint.primitives {
            match command {
                DisplayCommand::BeginScroll(node) => scrolls.push(*node),
                DisplayCommand::EndScroll => {
                    scrolls.pop();
                }
                DisplayCommand::GlyphRun { shaped, node, .. }
                    if *node == NO_NODE && matches!(shaped.text.trim(), "1." | "2." | "3.") =>
                {
                    markers += 1;
                    assert!(
                        scrolls.contains(&panel),
                        "marker must move and clip with its list item"
                    );
                }
                _ => {}
            }
        }
        assert_eq!(markers, 3);
    }

    #[test]
    fn text_clipped_background_paints_glyphs_not_a_rectangle() {
        for background in [
            "background:linear-gradient(red,blue)",
            "background-color:red",
        ] {
            let (_, layout) = render_fixture(&format!(
                r#"<style>body{{margin:0;background:white}} #text{{font:32px sans-serif;width:200px;height:80px;{background};background-clip:text;color:transparent}}</style><div id=text>Thinking <span>test</span></div>"#
            ));
            assert!(layout.paint.primitives.iter().any(|p| matches!(p,DisplayCommand::Fill{shape:PaintShape::Path(path),..} if !path.is_empty())));
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            let pixels = &frame.pixels;
            let at = |x: usize, y: usize| &pixels[(y * 800 + x) * 4..(y * 800 + x) * 4 + 3];
            assert_eq!(
                at(190, 70),
                [255, 255, 255],
                "background outside glyphs must stay white"
            );
            assert!(
                (0..40).any(|y| (0..190).any(|x| at(x, y) != [255, 255, 255])),
                "glyph background must remain visible with transparent text"
            );
        }
    }

    #[test]
    fn nested_scroll_overflow_does_not_inflate_the_page_or_parent_panel() {
        let (dom, layout) = render_fixture(
            r#"<style>body{margin:0} #outer{height:200px;overflow:auto} #inner{height:100px;overflow:auto;overscroll-behavior:contain} #tall{height:2000px} #after{height:400px}</style><div id=outer><div id=inner><div id=tall></div></div><div id=after></div></div>"#,
        );
        assert_eq!(layout.paint.height, 600.);
        let get = |id| {
            layout
                .paint
                .scroll_containers
                .iter()
                .find(|c| c.node == dom.get_by_id(id).unwrap())
                .unwrap()
        };
        assert_eq!(get("inner").content.height, 2000.);
        assert_eq!(get("outer").content.height, 500.);
        assert_eq!(get("inner").ancestors, vec![get("outer").node]);
        assert_eq!(get("inner").contain_overscroll, [true, true]);
        let (_, _, scrolling) = crate::layout2::measure_boxes_css(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport::new(800., 600.),
            &[],
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(scrolling[&get("inner").node].height, 2000.);
        assert_eq!(scrolling[&get("outer").node].height, 500.);
    }

    #[test]
    fn positioned_overflow_bypasses_intervening_scrollports() {
        for (position, inner_height, document_height) in
            [("static", 100., 1100.), ("relative", 1100., 600.)]
        {
            let (dom, layout) = render_fixture(&format!(
                r#"<style>
                body{{margin:0}} #panel{{height:100px;overflow:auto;position:{position}}}
                #abs{{position:absolute;top:1000px;height:100px;width:10px}}
                </style><div id=panel><div id=abs></div></div>"#
            ));
            let panel = dom.get_by_id("panel").unwrap();
            assert_eq!(
                layout
                    .paint
                    .scroll_containers
                    .iter()
                    .find(|c| c.node == panel)
                    .unwrap()
                    .content
                    .height,
                inner_height
            );
            assert_eq!(layout.paint.height, document_height);
        }
        let (dom, layout) = render_fixture(
            r#"<style>body{margin:0} #panel{height:100px;overflow:auto;border:10px solid;padding:5px} #child{height:200px}</style><div id=panel><div id=child></div></div>"#,
        );
        let panel = dom.get_by_id("panel").unwrap();
        let container = layout
            .paint
            .scroll_containers
            .iter()
            .find(|c| c.node == panel)
            .unwrap();
        assert_eq!(container.viewport.height, 110.);
        assert_eq!(container.content.height, 210.);
    }

    #[test]
    fn viewport_overflow_propagation_preserves_programmatic_area() {
        for (root, body, expected) in [
            ("overflow:hidden", "", [true, true]),
            ("", "overflow:hidden", [true, true]),
            ("overflow-x:hidden", "", [true, false]),
            ("overflow:auto", "overflow:hidden", [false, false]),
            ("contain:layout", "overflow:hidden", [false, false]),
        ] {
            let (dom, layout) = render_fixture(&format!(
                "<html style='{root}'><body style='margin:0;{body}'><div style='height:1200px;width:1600px'></div></body></html>"
            ));
            assert_eq!(viewport_overflow_disabled(&dom), expected, "{root}; {body}");
            let user = layout.paint.user_scroll_size.unwrap();
            if expected[0] {
                assert_eq!(user.width, 800.);
            }
            if expected[1] {
                assert_eq!(user.height, 600.);
                assert_eq!(layout.paint.height, 1200.);
            }
        }
    }

    #[test]
    fn css_color4_display_p3_and_lab_convert_to_srgb() {
        for (source, expected) in [
            ("color(display-p3 .067 .067 .063)", [17, 17, 16, 255]),
            ("COLOR(DISPLAY-P3 50% 50% 50% / 25%)", [128, 128, 128, 64]),
            ("color(display-p3 none 0 0 / none)", [0, 0, 0, 0]),
            ("lab(100% 0 0 / .15)", [255, 255, 255, 38]),
            ("lab(50% 0 0)", [119, 119, 119, 255]),
            // CSS Color 4 #ex-lab-samples, including D50 -> D65 adaptation.
            ("lab(29.2345% 39.3825 20.0664)", [125, 35, 41, 255]),
            ("lab(29.69% 44.888% -29.04%)", [128, 0, 128, 255]),
            ("lab(-10% 40 40 / 120%)", [0, 0, 0, 255]),
            ("lab(110% 40 40 / -1)", [255, 255, 255, 0]),
        ] {
            let Some(PaintColor::Rgba(r, g, b, a)) = PaintColor::parse_css(source) else {
                panic!("failed to parse {source}");
            };
            for (actual, expected) in [r, g, b, a].into_iter().zip(expected) {
                assert!(
                    actual.abs_diff(expected) <= 1,
                    "{source}: {:?}",
                    [r, g, b, a]
                );
            }
        }
        for invalid in [
            "color(display-p3 0, 0, 0)",
            "color(display-p3 0 0)",
            "color(display-p3 0 0 0 / bad)",
            "color(unknown-space 0 0 0)",
            "lab(0%, 0, 0)",
            "lab(50 0 0) trailing",
        ] {
            assert_eq!(PaintColor::parse_css(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn css_color4_oklab_oklch_and_lch_convert_to_srgb() {
        // CSS Color 4 §9 examples, neutral colors, endpoint clamping and
        // missing components. Alpha does not participate in gamut mapping.
        for (source, expected) in [
            ("oklch(50% 0 0)", [99, 99, 99, 255]),
            ("oklab(.5 0 0 / 50%)", [99, 99, 99, 128]),
            ("oklch(40.101% 0.12332 21.555)", [125, 35, 41, 255]),
            ("oklab(40.101% 0.1147 0.0453)", [125, 35, 41, 255]),
            ("lch(29.2345% 44.2 27)", [125, 35, 41, 255]),
            ("OKLCH(50% -1 180deg / 25%)", [99, 99, 99, 64]),
            ("oklch(50% none none)", [99, 99, 99, 255]),
            ("oklch(110% .4 40 / .5)", [255, 255, 255, 128]),
            ("oklab(-1 .3 .4)", [0, 0, 0, 255]),
            ("lch(100% 200 30 / none)", [255, 255, 255, 0]),
        ] {
            let Some(PaintColor::Rgba(r, g, b, a)) = PaintColor::parse_css(source) else {
                panic!("failed to parse {source}");
            };
            for (actual, expected) in [r, g, b, a].into_iter().zip(expected) {
                assert!(
                    actual.abs_diff(expected) <= 1,
                    "{source}: {:?}",
                    [r, g, b, a]
                );
            }
        }
        for invalid in [
            "oklch(50%, 0, 0)",
            "oklab(50% 0)",
            "lch(50% 0 bad)",
            "oklch(50% 0 0 / bad)",
            "oklab(.5 0 0) trailing",
        ] {
            assert_eq!(PaintColor::parse_css(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn block_button_auto_width_is_fit_content_and_explicit_width_is_preserved() {
        let mut dom = Dom::parse_document(
            r#"<style>
            body{margin:0} button{display:block;padding:2px;border:0}
            button > div{display:flex;width:100%;gap:8px;align-items:center}
            svg{display:block;width:16px;height:16px;vertical-align:middle}
            #wide{width:300px}
            </style><button id=auto><div><svg viewBox='0 0 16 16'></svg><span>Thinking...</span></div></button>
            <button id=wide>Explicit</button>"#,
        );
        dom.set_render_clickables(Default::default(), true);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport::new(800., 600.),
            &[],
            &Default::default(),
            &Default::default(),
        );
        let width = layout.boxes[&dom.get_by_id("auto").unwrap()].width;
        assert!(
            width > 80. && width < 180.,
            "automatic button width: {width}"
        );
        assert_eq!(layout.boxes[&dom.get_by_id("wide").unwrap()].width, 300.);
    }

    #[test]
    fn block_svg_pixels_stay_in_the_content_box_despite_vertical_align() {
        let mut dom = Dom::parse_document(
            r#"<style>
            body{margin:0} button{display:block;width:32px;height:32px;padding:6px;border:0;box-sizing:border-box}
            svg{display:block;width:20px;height:20px;vertical-align:middle}
            </style><button><svg id=icon viewBox='0 0 20 20'><path d='M0 0H20V20H0Z'/></svg></button>"#,
        );
        dom.set_render_clickables(Default::default(), true);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport::new(800., 600.),
            &[],
            &Default::default(),
            &Default::default(),
        );
        let node = dom.get_by_id("icon").unwrap();
        let image = layout
            .paint
            .primitives
            .iter()
            .find_map(|p| match p {
                DisplayCommand::Image { node: n, rect, .. } if *n == node => Some(*rect),
                _ => None,
            })
            .unwrap();
        assert_eq!(image, CssRect::new(6., 6., 20., 20.));
        assert_eq!(layout.boxes[&node].top, 6.);
    }

    #[test]
    fn inline_middle_aligns_image_center_above_the_baseline() {
        let dom = Dom::parse_document(
            r#"<style>body{margin:0;font:20px sans-serif} img{width:16px;height:16px;vertical-align:middle}</style>
            <div>Text<img id=icon src='data:image/svg+xml,%3Csvg%20viewBox=%220%200%2016%2016%22/%3E'></div>"#,
        );
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport::new(800., 600.),
            &[],
            &Default::default(),
            &Default::default(),
        );
        let node = dom.get_by_id("icon").unwrap();
        let rect = layout.boxes[&node];
        let baseline = layout.paint.lines[0].baseline;
        let half_x = crate::text::x_height(&crate::text::TextStyle {
            size: 20.,
            ..Default::default()
        }) / 2.;
        assert!(((rect.top + rect.height / 2.) as f32 - (baseline - half_x)).abs() < 0.01);
    }

    #[test]
    fn perceptual_palette_colors_reach_inherited_text_and_controls() {
        let dom = Dom::parse_document(
            r#"<html class=dark><style>
            @layer theme { :root { --light: oklch(94% 0 0); --surface: oklch(20% 0 0); } }
            @layer base { a,button { color: inherit } }
            @layer utilities { .dark .surface { color:var(--light); background-color:var(--surface) } }
            @supports (color:oklch(50% 0 0)) { #supported { color:oklab(.5 0 0) } }
            </style><body><div class=surface><p id=text>Readable</p><a id=link href=#>Link</a>
            <button id=button>Submit</button><span id=supported>Supported</span></div></body></html>"#,
        );
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &Url::parse("https://colors.example/").unwrap(),
            crate::layout2::Viewport::new(800., 600.),
            &[],
            &crate::layout2::ControlMap::new(),
            &ImageSizes::new(),
        );
        for id in ["text", "link", "button", "supported"] {
            let node = dom.get_by_id(id).unwrap();
            let expected = PaintColor::parse_css(if id == "supported" {
                "oklab(.5 0 0)"
            } else {
                "oklch(94% 0 0)"
            })
            .unwrap();
            assert!(layout.paint.primitives.iter().any(|command| matches!(command,
                DisplayCommand::GlyphRun { node: painted, color, .. } if *painted == node && *color == expected)), "missing authored color on {id}");
        }
    }

    #[test]
    fn css_color4_out_of_gamut_colors_reduce_chroma() {
        // A saturated P3 primary is outside sRGB. Local MINDE retains its
        // perceived lightness by adding green/blue, rather than clamping to
        // the darker sRGB red primary. Alpha is independent of the mapping.
        let Some(PaintColor::Rgba(r, g, b, a)) =
            PaintColor::parse_css("color(display-p3 1 0 0 / .5)")
        else {
            panic!("P3 red must be paintable");
        };
        assert_eq!((r, a), (255, 128));
        assert!((10..=20).contains(&g), "green: {g}");
        assert!((5..=15).contains(&b), "blue: {b}");
    }

    #[test]
    fn modern_css_colors_and_alpha_are_retained() {
        assert_eq!(
            PaintColor::parse_css("rgb(100% 0% 0% / 25%)"),
            Some(PaintColor::Rgba(255, 0, 0, 64))
        );
        assert_eq!(
            PaintColor::parse_css("rebeccapurple"),
            Some(PaintColor::Rgba(102, 51, 153, 255))
        );
    }

    #[test]
    fn css_transform_list_post_multiplies() {
        let rotate = rotate(FRAC_PI_2);
        let matrix = Affine2d::translate(10.0, 0.0).then(rotate);
        assert!((matrix.0[4] - 10.0).abs() < 0.001);
        assert!((matrix.0[1] - 1.0).abs() < 0.001);
    }
}
