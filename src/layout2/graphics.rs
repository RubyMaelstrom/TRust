//! Renderer-neutral graphical paint extraction from canonical fragments.
//!
//! CSS 2.2 Appendix E remains the ordering authority. This module turns that
//! traversal into a stateful TRust display list; it does not contain Vello,
//! framebuffer, DPI, winit, Ratatui, or terminal-cell types.

use std::collections::{HashMap, HashSet};
use std::f32::consts::{FRAC_PI_2, PI, SQRT_2};
use std::str::FromStr as _;

use url::Url;

use crate::core::{CssPoint, CssSize};
use crate::dom::{Dom, NodeId, PseudoEl};
use crate::render::{
    Affine2d, BlendMode, CompositingLayer, CornerRadii, CssAnimationPoint, CssAnimationScope,
    CssPaintAnimation, CssRect, CssTransformFrame, DecorationStyle, DisplayCommand,
    GradientInterpolation, GradientStop, HitRegion, ImageFit, ImageHandle, ImageRequest,
    ImageSampling, LineJoin, MarqueeBehavior, MarqueeDirection, MarqueeScope, PagePaint,
    PaintBrush, PaintColor, PaintLine, PaintShape, PathElement, ScrollContainer, StickyConstraint,
    StrokeStyle, TextDecorationPaint, TextShadowPaint, TopLayerEntry,
};

use super::ImageSizes;
use super::NO_NODE;
use super::Units;
use super::flow::{Clip, Frag, FragKind, TopFrag};
use super::overflow::{ScrollAreas, overflow_axes, viewport_overflow_disabled};
use super::style::{Outline, OutlineStyle, outline_of};
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
    fn of(fragment: &Frag) -> Option<Self> {
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
            Self::Element(NO_NODE) => None,
            Self::Element(node) => dom.computed_value_resolved(node, property),
            Self::Pseudo(node, pseudo) => dom.pseudo_layout_value(node, pseudo, property),
        }
    }
}

struct Builder<'a> {
    dom: &'a Dom,
    /// CSS Backgrounds 3 #body-background: canvas propagation is constant
    /// during this immutable paint transaction. Recompute for each paint so
    /// root/body style changes cannot leave a stale source behind.
    document_root: Option<NodeId>,
    canvas_background_source: Option<NodeId>,
    base: &'a Url,
    images: &'a ImageSizes,
    fixed: &'a [Frag],
    viewport_w: f32,
    viewport_h: f32,
    /// Canvas background layers with `background-attachment: fixed`, pinned
    /// to the viewport below the scrolling document.
    fixed_under: Vec<DisplayCommand>,
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
    has_media_controls: bool,
    /// Overflow clips round the padding edge, independently of whether the
    /// box exposes a user scrolling mechanism (hidden/clip do not).
    rounded_overflow_clips: HashMap<NodeId, PaintShape>,
    /// Keep rectangular overflow clips attached to their generating boxes.
    /// A flattened fragment clip loses which side of a scroll transform each
    /// edge belongs on, especially at a child document's viewport boundary.
    overflow_clips: HashMap<NodeId, Clip>,
    scroll_tree: std::borrow::Cow<'a, super::spatial::ScrollTree>,
    patch_boundaries: Vec<super::GraphicalPatchBoundary>,
    boundaries: Vec<super::GraphicalBoundary>,
    /// Absolute overflow clips already active in the display list. A clip
    /// inherited from outside a transformed stacking context must be pushed
    /// before that transform and retained for the context's descendants.
    hard_clips: Vec<CssRect>,
    transformed_clips: Vec<CssRect>,
    /// Scrollports already emitted around the current paint subtree. Scroll
    /// clipping is in the scroll container's coordinate system, so a
    /// descendant stacking-context transform must be nested inside these
    /// clips rather than wrapping them. Keeping the active prefix lets the
    /// ordinary fragment painter add only newly-entered scrollports.
    scroll_nodes: Vec<(NodeId, bool)>,
    scroll_indices: HashMap<NodeId, usize>,
}

impl<'a> Builder<'a> {
    fn viewport(&self) -> Vp {
        Vp {
            w: self.viewport_w,
            h: self.viewport_h,
        }
    }

    // The paint adapter's inputs are distinct borrowed engine products. A
    // parameter object would only move these references without simplifying
    // ownership or call sites.
    #[allow(clippy::too_many_arguments)]
    fn new(
        dom: &'a Dom,
        base: &'a Url,
        images: &'a ImageSizes,
        root: &Frag,
        fixed: &'a [Frag],
        top_layer: &[TopFrag],
        flow_bottom: f32,
        viewport_w: f32,
        viewport_h: f32,
        scroll_tree: Option<&'a super::spatial::ScrollTree>,
    ) -> Self {
        let document_root = dom.document_element();
        let mut this = Self {
            dom,
            document_root,
            canvas_background_source: document_root.map(|root| canvas_background_node(dom, root)),
            base,
            images,
            fixed,
            viewport_w,
            viewport_h,
            fixed_under: Vec::new(),
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
            has_media_controls: false,
            rounded_overflow_clips: HashMap::new(),
            overflow_clips: HashMap::new(),
            scroll_tree: scroll_tree.map_or_else(
                || {
                    std::borrow::Cow::Owned(super::spatial::ScrollTree::new(
                        dom, root, fixed, top_layer,
                    ))
                },
                std::borrow::Cow::Borrowed,
            ),
            patch_boundaries: Vec::new(),
            boundaries: Vec::new(),
            hard_clips: Vec::new(),
            transformed_clips: Vec::new(),
            scroll_nodes: Vec::new(),
            scroll_indices: HashMap::new(),
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
        // Unbounded clip axes must also cover content retained outside an
        // iframe's initial viewport, ready to be scrolled into view.
        for container in &this.scroll_containers {
            this.clip_extent.width = this
                .clip_extent
                .width
                .max(container.viewport.x + container.content.width - this.clip_extent.x);
            this.clip_extent.height = this
                .clip_extent
                .height
                .max(container.viewport.y + container.content.height - this.clip_extent.y);
        }
        this
    }

    fn scroll_ancestor_nodes(&self, node: NodeId) -> Vec<NodeId> {
        self.scroll_tree
            .chain(node, false)
            .filter(|link| link.axes.iter().any(|axis| *axis))
            .map(|link| link.node)
            .collect()
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
        if source.is_none() && node != NO_NODE && self.dom.canvas_size(node).is_some() {
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
        // An ancestor clip already established outside a transformed subtree
        // must not also become a primitive-local clip inside that transform.
        // CSS Transforms 1 #transform-rendering keeps the ancestor's coordinate
        // system fixed; reapplying it clips sprite images before their offset.
        self.clip_chain(node, hard)
            .filter(|clip| !self.transformed_clips.contains(clip))
    }

    fn ancestor_clip(&self, node: NodeId, hard: Option<Clip>) -> Option<CssRect> {
        self.clip_chain(node, hard)
    }

    fn clip_chain(&self, node: NodeId, hard: Option<Clip>) -> Option<CssRect> {
        // CSS Overflow 3 #scrolling / HTML #the-page: a scrollport clips in
        // its own coordinate system, before translating the child canvas.
        // `Frag::clip` flattens those edges for terminal painting/geometry;
        // applying it again inside BeginScroll permanently hides content
        // beyond the initial iframe viewport. The scoped overflow chain below
        // retains each edge at its owner, including clips within the content.
        let mut clip = hard
            .filter(|_| !self.scroll_nodes.iter().any(|(_, scroll)| *scroll))
            .map(|clip| self.clip_rect(clip));
        let mut current = (node != NO_NODE).then_some(node);
        while let Some(id) = current {
            if let Some(rect) = self.legacy_clips.get(&id) {
                clip = intersect_css_rects(clip, *rect);
            }
            current = self.dom.parent_flat(id);
        }
        clip
    }

    fn collect_legacy_clips(&mut self, fragment: &Frag) {
        if fragment.node != NO_NODE
            && matches!(fragment.kind, FragKind::Block | FragKind::TableCell(_))
        {
            let node = fragment.node;
            if let Some(shape) = rounded_overflow_clip(self.dom, fragment) {
                self.rounded_overflow_clips.insert(node, shape);
            }
            if let Some(clip) = rectangular_overflow_clip(self.dom, fragment) {
                self.overflow_clips.insert(node, clip);
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
        // CSS Overflow 3 §2.3 clips the contents of a scroll container to
        // its scrollport.  A shadow tree is attached to the light tree through
        // its host (DOM §4.2.2), so paint ancestry must cross that boundary:
        // otherwise a custom element's host box is clipped but the image/text
        // painted by its shadow tree escapes the same scrollport.
        for link in self.scroll_tree.chain(node, include_node) {
            let id = link.node;
            let container = self
                .scroll_indices
                .get(&id)
                .map(|&index| &self.scroll_containers[index]);
            let shape = self
                .rounded_overflow_clips
                .get(&id)
                .cloned()
                .or_else(|| container.map(|container| PaintShape::Rect(container.viewport)))
                .or_else(|| {
                    self.overflow_clips
                        .get(&id)
                        .map(|clip| PaintShape::Rect(self.clip_rect(*clip)))
                });
            if let Some(shape) = shape {
                chain.push((
                    id,
                    shape,
                    container.is_some() && link.axes.iter().any(|axis| *axis),
                ));
            }
        }
        chain.reverse();
        let common = self
            .scroll_nodes
            .iter()
            .zip(&chain)
            .take_while(|(active, requested)| active.0 == requested.0 && active.1 == requested.2)
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

    fn collect_scroll_containers(&mut self, fragment: &Frag, fixed: bool) {
        let nested_viewport = fragment.node != NO_NODE
            && matches!(self.dom.tag_name(fragment.node), Some("iframe" | "frame"));
        if fragment.node != NO_NODE
            && self
                .scroll_tree
                .axes(fragment.node)
                .iter()
                .any(|axis| *axis)
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
            self.scroll_indices
                .entry(fragment.node)
                .or_insert(self.scroll_containers.len());
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
                reverse: super::overflow::scroll_reverse(self.dom, fragment.node),
                offset: CssPoint::new(
                    self.dom.scroll_metric(fragment.node, 1).unwrap_or(0.0) as f32,
                    self.dom.scroll_metric(fragment.node, 0).unwrap_or(0.0) as f32,
                ),
                // An iframe is itself a nested viewport. HTML's default
                // scrolling behavior supplies scroll mechanisms only on axes
                // whose child document overflows that viewport; authored CSS
                // scroll containers retain their explicit axis eligibility.
                horizontal: fragment.paint.overflow[0].user_scrollable()
                    || (nested_viewport && content.width > viewport.width),
                vertical: fragment.paint.overflow[1].user_scrollable()
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

    fn collect_patch_boundaries(&mut self, fragment: &Frag) {
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

    fn collect_sticky(&mut self, fragment: &Frag) {
        self.collect_sticky_in(fragment, &mut Vec::new());
    }

    fn collect_sticky_in<'f>(&mut self, fragment: &'f Frag, ancestors: &mut Vec<&'f Frag>) {
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

    fn collect_marquees(&mut self, fragment: &Frag) {
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

fn marquee_content_bounds(fragment: &Frag) -> Option<CssRect> {
    fn union(a: CssRect, b: CssRect) -> CssRect {
        let left = a.x.min(b.x);
        let top = a.y.min(b.y);
        let right = (a.x + a.width).max(b.x + b.width);
        let bottom = (a.y + a.height).max(b.y + b.height);
        CssRect::new(left, top, right - left, bottom - top)
    }
    fn collect(fragment: &Frag, bounds: &mut Option<CssRect>) {
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
pub(super) fn paint(
    dom: &Dom,
    base: &Url,
    images: &ImageSizes,
    root: &Frag,
    fixed: &[Frag],
    top_layer: &[TopFrag],
    flow_bottom: f32,
    viewport_w: f32,
    viewport_h: f32,
) -> (
    PagePaint,
    Vec<super::GraphicalPatchBoundary>,
    Vec<super::GraphicalBoundary>,
) {
    paint_in_spaces(
        dom,
        base,
        images,
        root,
        fixed,
        top_layer,
        flow_bottom,
        viewport_w,
        viewport_h,
        None,
    )
}

pub(super) fn paint_retained(
    dom: &Dom,
    base: &Url,
    images: &ImageSizes,
    fragments: &super::LayoutFragments,
) -> (
    PagePaint,
    Vec<super::GraphicalPatchBoundary>,
    Vec<super::GraphicalBoundary>,
) {
    paint_in_spaces(
        dom,
        base,
        images,
        &fragments.root,
        &fragments.fixed,
        &fragments.top_layer,
        fragments.flow_bottom,
        fragments.viewport.width,
        fragments.viewport.height,
        Some(fragments.scroll_tree(dom)),
    )
}

#[allow(clippy::too_many_arguments)]
fn paint_in_spaces(
    dom: &Dom,
    base: &Url,
    images: &ImageSizes,
    root: &Frag,
    fixed: &'_ [Frag],
    top_layer: &[TopFrag],
    flow_bottom: f32,
    viewport_w: f32,
    viewport_h: f32,
    scroll_tree: Option<&super::spatial::ScrollTree>,
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
        scroll_tree,
    );
    // CSS Backgrounds 3 §§2.11.1–2: the root background becomes the canvas
    // background. For HTML, when the root has its initial transparent/none
    // background, the first BODY child's computed background is propagated to
    // the canvas instead. Its image positioning area remains the root box,
    // while its painting area is the complete canvas, including the margins
    // around a centered body and any viewport space below the document.
    let root_background = if root.node != NO_NODE && builder.document_root == Some(root.node) {
        let canvas = CssRect::new(
            0.0,
            0.0,
            viewport_w.max(root.x + root.w).max(1.0),
            viewport_h.max(flow_bottom).max(root.max_bottom()).max(1.0),
        );
        let style_node = builder.canvas_background_source.unwrap_or(root.node);
        let style = PaintStyle::Element(style_node);
        let color = background_color(dom, style_node).filter(|color| !color.is_transparent());
        // CSS Color Adjust 1 #color-scheme-effect: the root's color scheme
        // sets the canvas surface.
        let surface = canvas_surface(dom, root.node);
        // CSS Compositing 1 #background-blend-mode: blended layers act as if
        // rendered into an isolated group over the background color, whose
        // initial backdrop is transparent black (#isolatedgroups). The canvas
        // surface (CSS Backgrounds 3 §2.11.1) is behind that group and must
        // not take part in the blend. Over an opaque background color the
        // group's result is the same as blending onto the page background.
        if background_blends(dom, style) && !color.is_some_and(is_opaque) {
            paint_isolated_canvas_background(root, style, canvas, color, &mut builder);
            surface
        } else {
            paint_background_images_for_style(
                root,
                style,
                PaintShape::Rect(canvas),
                &mut builder,
                Some(canvas),
                None,
            );
            color.or(surface)
        }
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
    let mut fixed_primitives = Vec::new();
    for fragment in fixed {
        let start = builder.commands.len();
        build_sc(fragment, &mut builder);
        let commands = builder.commands.split_off(start);
        fixed_primitives.extend(commands);
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
    let mut paint = PagePaint {
        width,
        height,
        user_scroll_size: Some(CssSize::new(
            if locked[0] { viewport_w } else { width },
            if locked[1] { viewport_h } else { height },
        )),
        background: root_background,
        lines: builder.lines,
        primitives,
        fixed_under_primitives: std::mem::take(&mut builder.fixed_under),
        fixed_primitives,
        fixed_interleaved: true,
        top_layer: top_layer_entries,
        browser_media: Vec::new(),
        image_requests: builder.image_requests,
        canvas_images: builder.canvas_images,
        scroll_containers: builder.scroll_containers,
        sticky_constraints: builder.sticky_constraints,
    };
    let media_fallbacks = dom.retained_media_controls();
    if builder.has_media_controls || !media_fallbacks.is_empty() {
        paint.collect_browser_media(|hit| {
            if let Some(target) = media_fallbacks.get(&hit.node) {
                return Some((hit.rect, crate::doc::Link::Media(target.clone())));
            }
            let Some(link @ crate::doc::Link::Media(_)) = &hit.link else {
                return None;
            };
            (matches!(dom.tag_name(hit.node), Some("video" | "audio"))
                && !dom.paint_suppressed(hit.node)
                && !dom.visibility_hidden(hit.node))
            .then(|| {
                if let crate::doc::Link::Media(target) = link {
                    dom.remember_media_target(hit.node, target);
                }
                (
                    builder
                        .replaced_border_boxes
                        .get(&hit.node)
                        .copied()
                        .unwrap_or(hit.rect),
                    link.clone(),
                )
            })
        });
    }
    (paint, patch_boundaries, boundaries)
}

/// CSS 2.2 Appendix E order for one real stacking context. Opacity and
/// transforms wrap the context atomically, as required by CSS Color and CSS
/// Transforms; children never observe a renderer-specific layer object.
fn build_sc(fragment: &Frag, builder: &mut Builder<'_>) {
    let boundary_start = builder.commands.len();
    let boundary_line_start = builder.lines.len();
    let boundary = graphical_boundary(fragment, builder);
    // CSS Overflow 3 §2.3 and CSS Transforms 1 §3: a scrollport is the
    // viewport through which a transformed descendant is seen. Emit the
    // ancestor scrollport before this stacking context's transform, so the
    // scrollport remains fixed in its own coordinate system instead of being
    // scaled/translated along with the page layer.
    let scroll_depth = if let Some((origin, _)) = fragment.paint.pseudo {
        // CSS Pseudo 4 #generated-content: a generated box is a child of its
        // originating element. Its host's overflow belongs outside the
        // pseudo-element's transform, just like a real child's ancestor clip.
        builder.push_scroll_content_chain(origin)
    } else {
        builder.push_scroll_ancestors(fragment.node)
    };
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
    let inherited_clip =
        transform.and_then(|_| builder.ancestor_clip(fragment.node, fragment.clip));
    let context_clip = inherited_clip.is_some_and(|clip| builder.push_hard_clip(clip));
    if let Some(clip) = inherited_clip {
        builder.transformed_clips.push(clip);
    }
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
    if inherited_clip.is_some() {
        builder.transformed_clips.pop();
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
fn paint_animation_scope(fragment: &Frag, builder: &Builder<'_>) -> Option<CssAnimationScope> {
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
    // Opacity keyframes scale a group relative to the static opacity layer
    // the element already paints with; missing 0%/100% keyframes take the
    // underlying computed value (CSS Animations 1 §3.3).
    let static_opacity = fragment.paint.opacity.clamp(0.0, 1.0);
    let relative = |opacity: f32| {
        if static_opacity > 0.0 {
            opacity / static_opacity
        } else {
            opacity
        }
    };
    let underlying_opacity = relative(
        builder
            .dom
            .computed_value_resolved(fragment.node, "opacity")
            .as_deref()
            .and_then(parse_opacity)
            .unwrap_or(1.0),
    );
    // CSS Animations 1 §3.3: missing transform keyframes take the static
    // transform, which the animation replaces while it applies.
    let (reference, transform_origin) = super::transform::animation_origin(
        |property| builder.dom.computed_value_resolved(fragment.node, property),
        units,
        viewport,
        fragment,
    );
    let underlying_transform = builder
        .dom
        .computed_value_resolved(fragment.node, "transform")
        .and_then(|value| super::transform::animation_steps(&value, units, viewport, reference))
        .unwrap_or_default();
    let mut animations = Vec::new();
    for definition in definitions {
        let mut opacity = definition
            .keyframes
            .iter()
            .filter_map(|frame| {
                Some(CssAnimationPoint {
                    offset: frame.offset,
                    value: CssPoint::new(relative(parse_opacity(frame.opacity.as_deref()?)?), 0.0),
                })
            })
            .collect::<Vec<_>>();
        complete_animation_track_with(&mut opacity, CssPoint::new(underlying_opacity, 0.0));
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
                Some(CssTransformFrame {
                    offset: frame.offset,
                    steps: super::transform::animation_steps(
                        frame.transform.as_deref()?,
                        units,
                        viewport,
                        reference,
                    )?,
                })
            })
            .collect::<Vec<_>>();
        complete_animation_track(&mut position);
        if !transform.is_empty() {
            transform.sort_by(|a, b| a.offset.total_cmp(&b.offset));
            if transform.first().is_some_and(|frame| frame.offset > 0.0) {
                transform.insert(
                    0,
                    CssTransformFrame {
                        offset: 0.0,
                        steps: underlying_transform.clone(),
                    },
                );
            }
            if transform.last().is_some_and(|frame| frame.offset < 1.0) {
                transform.push(CssTransformFrame {
                    offset: 1.0,
                    steps: underlying_transform.clone(),
                });
            }
        }
        if position.is_empty() && transform.is_empty() && opacity.is_empty() {
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
            transform_origin,
            static_transform: super::transform::matrix(fragment),
            opacity,
        });
    }
    (!animations.is_empty()).then_some(CssAnimationScope { animations })
}

/// CSS Animations 1 §3.3 synthesizes missing 0%/100% values from the
/// underlying style. The graphical subset represents that style as a zero
/// delta, so adding endpoints here also makes interpolation well-defined.
fn complete_animation_track(track: &mut Vec<CssAnimationPoint>) {
    complete_animation_track_with(track, CssPoint::default());
}

/// A CSS `<alpha-value>`: a number or percentage, clamped to [0, 1].
fn parse_opacity(value: &str) -> Option<f32> {
    let value = value.trim();
    let number = match value.strip_suffix('%') {
        Some(percent) => percent.trim().parse::<f32>().ok()? / 100.0,
        None => value.parse::<f32>().ok()?,
    };
    number.is_finite().then(|| number.clamp(0.0, 1.0))
}

fn complete_animation_track_with(track: &mut Vec<CssAnimationPoint>, underlying: CssPoint) {
    if track.is_empty() {
        return;
    }
    track.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    if track.first().is_some_and(|point| point.offset > 0.0) {
        track.insert(
            0,
            CssAnimationPoint {
                offset: 0.0,
                value: underlying,
            },
        );
    }
    if track.last().is_some_and(|point| point.offset < 1.0) {
        track.push(CssAnimationPoint {
            offset: 1.0,
            value: underlying,
        });
    }
}

enum PositionedChild<'f> {
    Fragment(&'f Frag),
    Fixed(usize),
}

fn positioned_z(child: &PositionedChild<'_>, fixed: &[Frag]) -> i32 {
    match child {
        PositionedChild::Fragment(fragment) => fragment.paint.z.unwrap_or(0),
        PositionedChild::Fixed(index) => fixed
            .get(*index)
            .and_then(|fragment| fragment.paint.z)
            .unwrap_or(0),
    }
}

fn build_positioned(child: PositionedChild<'_>, builder: &mut Builder<'_>, _real_context: bool) {
    match child {
        PositionedChild::Fragment(fragment) => build_sc(fragment, builder),
        PositionedChild::Fixed(index) => build_fixed(index, builder),
    }
}

fn build_fixed(index: usize, builder: &mut Builder<'_>) {
    let Some(fragment) = builder.fixed.get(index) else {
        return;
    };
    // CSS Positioned Layout 3 #stacking / CSS2 Appendix E.2: fixed boxes
    // participate at their stacking level and tree position even when they
    // cover the viewport. An auto-z box paints above in-flow backgrounds;
    // moving it into a viewport underlay would let the body hide its pixels.
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
fn graphical_boundary(fragment: &Frag, builder: &Builder<'_>) -> Option<(usize, NodeId, CssRect)> {
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

fn build_pseudo(fragment: &Frag, builder: &mut Builder<'_>) {
    paint_fragment(fragment, builder);
    inflow_backgrounds(fragment, builder);
    paint_floats(fragment, builder);
    inflow_content(fragment, builder);
}

fn inflow_backgrounds(fragment: &Frag, builder: &mut Builder<'_>) {
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

fn inflow_content(fragment: &Frag, builder: &mut Builder<'_>) {
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

fn paint_floats(fragment: &Frag, builder: &mut Builder<'_>) {
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

fn collect_positioned<'a>(
    fragment: &'a Frag,
    fixed: &[Frag],
    negative: &mut Vec<PositionedChild<'a>>,
    zero: &mut Vec<(PositionedChild<'a>, bool)>,
    positive: &mut Vec<PositionedChild<'a>>,
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

fn paint_fragment(fragment: &Frag, builder: &mut Builder<'_>) {
    if fragment.flow.hidden {
        return;
    }
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
                    builder.viewport(),
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
        paint_box_shadows(builder, style, rect, radii, fragment.border, false);
        let is_root = builder.document_root == Some(fragment.node);
        let is_canvas_body = builder.canvas_background_source == Some(fragment.node)
            || nested_canvas_background_source(builder.dom, fragment.node) == Some(fragment.node);
        if !is_root && !is_canvas_body {
            if fragment.node != NO_NODE {
                paint_native_control_surface(fragment, radii, builder);
            }
            let isolated = begin_background_isolation(builder, style);
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
                    builder.viewport(),
                );
                fill_background(builder, color_shape, PaintBrush::Solid(color), &shape);
            }
            paint_background_images(fragment, shape.clone(), builder, None);
            if isolated {
                builder.commands.push(DisplayCommand::PopLayer);
            }
        }
        paint_box_shadows(builder, style, rect, radii, fragment.border, true);
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
        paint_inline_box_decorations(builder, fragment, line, anonymous_line);
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
            let writing_transform = line.sideways.then_some(Affine2d([
                0.,
                1.,
                -1.,
                0.,
                fragment.x + fragment.y + fragment.w,
                fragment.y - fragment.x,
            ]));
            if let Some(transform) = writing_transform {
                builder
                    .commands
                    .push(DisplayCommand::PushTransform(transform));
            }
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
            if writing_transform.is_some() {
                clip = clip.map(|rect| {
                    CssRect::new(
                        fragment.x + rect.y - fragment.y,
                        fragment.y + fragment.w - (rect.x - fragment.x) - rect.width,
                        rect.height,
                        rect.width,
                    )
                });
            }
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
                // A multiline control's text scrolls within, and clips to,
                // its padding box.
                if let Some(text) = &piece.control_text {
                    let [top, right, bottom, left] = text.scrollport;
                    label_rect = CssRect::new(
                        label_rect.x - left,
                        label_rect.y - top,
                        label_rect.width + left + right,
                        label_rect.height + top + bottom,
                    );
                }
                clip = intersect_css_rects(clip, label_rect);
            }
            if let Some(label) = &piece.shaped {
                let content_origin = CssPoint::new(
                    fragment.x + piece.x + piece.paint_x,
                    fragment.y + piece.y + piece.paint_y,
                );
                // A multiline control (`<textarea>`) carries its value as
                // laid-out line runs whose wrapping, alignment and indent
                // are already resolved. Every other piece paints one run.
                let runs: Vec<(CssPoint, &crate::text::ShapedText)> = match &piece.control_text {
                    Some(text) => text
                        .lines
                        .iter()
                        .map(|line| {
                            (
                                CssPoint::new(content_origin.x + line.x, content_origin.y + line.y),
                                &line.shaped,
                            )
                        })
                        .collect(),
                    None => {
                        let mut origin = content_origin;
                        if form_piece && style_node != NO_NODE {
                            // CSS Text 3 #text-indent-property: the control's
                            // inner line inherits indentation, including
                            // negative lengths. Move its text, never its
                            // content clip or hit target.
                            let indent = builder
                                .dom
                                .computed_value_resolved(style_node, "text-indent")
                                .and_then(|v| {
                                    Len::parse(
                                        &v,
                                        Units::of(builder.dom, style_node),
                                        Vp {
                                            w: builder.viewport_w,
                                            h: builder.viewport_h,
                                        },
                                    )
                                })
                                .and_then(|v| v.resolve(Some(piece.paint_width)))
                                .unwrap_or(0.0);
                            origin.x += indent;
                            let spare = (piece.paint_width - indent - label.advance).max(0.0);
                            origin.x += match super::style::block_align(builder.dom, style_node) {
                                super::style::Align2::Center => spare / 2.0,
                                super::style::Align2::Right => spare,
                                _ => 0.0,
                            };
                        }
                        vec![(origin, label)]
                    }
                };
                let paint_style = piece
                    .item
                    .pseudo
                    .map_or(PaintStyle::Element(style_node), |(node, pseudo)| {
                        PaintStyle::Pseudo(node, pseudo)
                    });
                let current_color = match piece.item.pseudo {
                    Some((node, pseudo)) => {
                        text_color_for_style(builder.dom, PaintStyle::Pseudo(node, pseudo))
                    }
                    None => text_color(builder.dom, style_node, piece.item.link.is_some()),
                };
                let color = paint_style
                    .value(builder.dom, "-webkit-text-fill-color")
                    .as_deref()
                    .and_then(|value| resolve_color_for_style(builder.dom, paint_style, value))
                    .unwrap_or(current_color);
                let viewport = Vp {
                    w: builder.viewport_w,
                    h: builder.viewport_h,
                };
                let decoration = TextDecorationPaint {
                    color: decoration_color(builder.dom, style_node).unwrap_or(current_color),
                    style: decoration_style(builder.dom, style_node),
                    thickness: decoration_metric(
                        builder.dom,
                        style_node,
                        "text-decoration-thickness",
                        viewport,
                    ),
                    underline_offset: decoration_metric(
                        builder.dom,
                        style_node,
                        "text-underline-offset",
                        viewport,
                    ),
                };
                let shadows = text_shadows(builder.dom, style_node, current_color);
                for (origin, shaped) in runs {
                    let mut shaped = shaped.clone();
                    if style_node != NO_NODE {
                        let (underline, strikethrough) = builder.dom.text_decoration(style_node);
                        shaped.underline = underline;
                        shaped.strikethrough = strikethrough;
                    }
                    builder.push_marquee_content(
                        node,
                        DisplayCommand::GlyphRun {
                            origin,
                            shaped: shaped.clone(),
                            color,
                            decoration,
                            shadows: shadows.clone(),
                            clip,
                            node,
                            link: piece.item.link.clone(),
                        },
                    );
                    // WHATWG Compatibility #the-webkit-text-stroke-width: stroke
                    // glyph edges even when their foreground fill is transparent.
                    // Reuse the shaped outline (including font variation/skew)
                    // and the shared path command so both raster backends agree.
                    if let Some(width) = paint_style
                        .value(builder.dom, "-webkit-text-stroke-width")
                        .as_deref()
                        .and_then(|value| {
                            crate::dom::text_stroke_width_px(
                                value,
                                super::Units {
                                    fs: shaped.runs.first().map_or(16., |run| run.font_size),
                                    root: builder.dom.root_font_px(),
                                    ch: shaped.runs.first().map_or(8., |run| run.font_size * 0.5),
                                },
                                builder.dom.viewport_px(),
                            )
                        })
                        .filter(|width| *width > 0.)
                    {
                        let stroke_color = paint_style
                            .value(builder.dom, "-webkit-text-stroke-color")
                            .as_deref()
                            .and_then(|value| {
                                resolve_color_for_style(builder.dom, paint_style, value)
                            })
                            .unwrap_or(current_color);
                        let mut path = Vec::new();
                        crate::text::append_glyph_path(&mut path, &shaped, origin);
                        if let Some(clip) = clip {
                            builder.push_marquee_content(
                                node,
                                DisplayCommand::PushClip(PaintShape::Rect(clip)),
                            );
                        }
                        builder.push_marquee_content(
                            node,
                            DisplayCommand::Stroke {
                                shape: PaintShape::Path(path),
                                brush: PaintBrush::Solid(stroke_color),
                                style: StrokeStyle::solid(width),
                            },
                        );
                        if clip.is_some() {
                            builder.push_marquee_content(node, DisplayCommand::PopClip);
                        }
                    }
                    if style_node == NO_NODE || builder.dom.point_hit_testable(style_node) {
                        builder.has_media_controls |=
                            matches!(piece.item.link, Some(crate::doc::Link::Media(_)));
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
                let ratio = builder.dom.device_pixel_ratio();
                // CSS Backgrounds 3 §4.2/§4.3: replaced pixels clip to the
                // curved CONTENT edge, independent of overflow. Object-fit's
                // painted rectangle may be smaller than this content box.
                let content = snap_to_device_pixels(
                    CssRect::new(
                        fragment.x + piece.x,
                        fragment.y + piece.y,
                        piece.box_width,
                        piece.box_height,
                    ),
                    ratio,
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
                let natural_size_known = piece
                    .item
                    .graphical_image
                    .as_ref()
                    .or(piece.item.image.as_ref())
                    .is_some_and(|source| {
                        [source.clone(), resolve_image_source(builder.base, source)]
                            .iter()
                            .filter_map(|source| builder.images.get(source))
                            .any(|&(w, h)| w > 0 && h > 0 && (w, h) != (u32::MAX, u32::MAX))
                    });
                if let Some(handle) = handle {
                    builder.push_clipped_marquee_content(
                        node,
                        DisplayCommand::Image {
                            rect: snap_to_device_pixels(rect, ratio),
                            handle,
                            source_rect: None,
                            // Layout already resolved object-fit into the
                            // paint rectangle (css-images-3 §5.5: `fill`
                            // stretches). Only an image whose natural size
                            // layout did not know keeps its aspect ratio.
                            fit: if piece.item.crop {
                                ImageFit::Cover
                            } else if natural_size_known {
                                ImageFit::Fill
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
                    builder.has_media_controls |=
                        matches!(piece.item.link, Some(crate::doc::Link::Media(_)));
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
            if writing_transform.is_some() {
                builder.commands.push(DisplayCommand::PopTransform);
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
    builder: &mut Builder<'_>,
    parent: &Frag,
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
        flow: Default::default(),
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
    let style = PaintStyle::Element(node);
    paint_box_shadows(builder, style, rect, radii, control.border, false);
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
    paint_box_shadows(builder, style, rect, radii, control.border, true);
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
fn paint_native_control_surface(fragment: &Frag, radii: CornerRadii, builder: &mut Builder<'_>) {
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

    // CSS UI 4 #appearance-disabling-properties: an author-origin cascaded
    // value counts even when it is `initial` or `unset`, whose computed
    // value is the transparent, borderless initial value rather than the
    // UA's native surface.
    let background_declared = builder.dom.author_cascades(node, "background-color")
        || builder.dom.author_cascades(node, "background-image");
    // An authored border style replaces the native edge with the CSS border.
    // An authored color or width alone restyles the UA border, as in Gecko
    // and Blink: `border-color: transparent` hides a button's border, and a
    // colored field keeps a border in that color.
    let border_declared = ["top", "right", "bottom", "left"].into_iter().any(|side| {
        builder
            .dom
            .author_cascades(node, &format!("border-{side}-style"))
    });
    let edge_width = builder
        .dom
        .computed_value_resolved(node, "border-top-width")
        .and_then(|value| {
            LengthBasis::of(builder.dom, node, builder.viewport()).resolve(&value, 0.0)
        })
        .unwrap_or(1.0);
    // CSS Color Adjust 1 #color-scheme-effect: form controls take the
    // default colors of the element's color scheme.
    let scheme = builder.dom.color_scheme(node);
    let surface = scheme_color(
        scheme,
        if builder.dom.tag_name(node) == Some("button") {
            "buttonface"
        } else {
            "field"
        },
    );
    let edge = border_color(builder.dom, PaintStyle::Element(node), "top")
        .unwrap_or_else(|| scheme_color(scheme, "buttonborder"));
    let rect = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    if !background_declared {
        builder.commands.push(DisplayCommand::Fill {
            shape: rounded_shape(rect, radii),
            brush: PaintBrush::Solid(surface),
        });
    }
    if !border_declared && edge_width > 0.0 && !edge.is_transparent() {
        let half = edge_width / 2.0;
        builder.commands.push(DisplayCommand::Stroke {
            shape: rounded_shape(
                CssRect::new(
                    rect.x + half,
                    rect.y + half,
                    (rect.width - edge_width).max(0.0),
                    (rect.height - edge_width).max(0.0),
                ),
                radii,
            ),
            brush: PaintBrush::Solid(edge),
            style: StrokeStyle::solid(edge_width),
        });
    }
}

/// HTML Rendering §15.5.6 leaves the exact number-control UI to the user
/// agent, but explicitly calls a spinbox with up/down controls a reasonable
/// rendering for `type=number`. Keep the affordance in the graphical display
/// list (the terminal frontend uses its own character-cell adaptation), and
/// suppress it when CSS UI requests `appearance:none`.
fn paint_number_spin_buttons(fragment: &Frag, builder: &mut Builder<'_>) {
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

/// A system color of `scheme` (CSS Color 4 #css-system-colors).
fn scheme_color(scheme: crate::dom::color_scheme::ColorScheme, name: &str) -> PaintColor {
    crate::dom::color_scheme::system_color(name, scheme)
        .and_then(PaintColor::parse_css)
        .unwrap_or(PaintColor::Rgba(255, 255, 255, 255))
}

/// The canvas surface a root element's color scheme supplies when no
/// background reaches the canvas: none for light, the page's default.
fn canvas_surface(dom: &Dom, root: NodeId) -> Option<PaintColor> {
    let scheme = dom.color_scheme(root);
    (scheme == crate::dom::color_scheme::ColorScheme::Dark).then(|| scheme_color(scheme, "canvas"))
}

/// Paint a CSS Basic User Interface 4 §3 outline. Unlike a border, the
/// outline is outside the border edge and does not affect layout. The
/// outline's exact stacking is intentionally UA-defined; emitting it at the
/// end of this fragment's paint keeps it visible over the fragment's own text
/// while preserving the surrounding Appendix E traversal.
fn paint_outline(fragment: &Frag, builder: &mut Builder<'_>) {
    if fragment.flow.hidden {
        return;
    }
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
    builder: &mut Builder<'_>,
    source: PaintStyle,
    border_box: CssRect,
    outline: Outline,
) {
    if !outline.paints() || matches!(outline.style, OutlineStyle::Auto) {
        return;
    }
    let width = outline.width;
    let outset = outline.offset + width;
    let outer = CssRect::new(
        border_box.x - outset,
        border_box.y - outset,
        (border_box.width + outset * 2.0).max(0.0),
        (border_box.height + outset * 2.0).max(0.0),
    );
    // CSS UI 4 #outline-props: the outline follows the border-radius
    // curve, grown by the outset-adjusted border radius, so a square corner
    // stays square.
    let outer_radii = outset_radii(
        border_radii(builder.dom, source, border_box),
        border_box,
        outset,
    );
    let color = source
        .value(builder.dom, "outline-color")
        .and_then(|value| {
            if value.trim().eq_ignore_ascii_case("currentcolor") {
                Some(text_color_for_style(builder.dom, source))
            } else {
                PaintColor::parse_css(&value)
            }
        })
        .unwrap_or_else(|| text_color_for_style(builder.dom, source));
    // CSS UI 4 #outline-style: the border styles keep their meaning. Like
    // the box's border, the outline is a band around its edge, here of one
    // width, style and color on every side.
    let style = match outline.style {
        OutlineStyle::Dashed => "dashed",
        OutlineStyle::Dotted => "dotted",
        OutlineStyle::Double => "double",
        OutlineStyle::Groove => "groove",
        OutlineStyle::Ridge => "ridge",
        OutlineStyle::Inset => "inset",
        OutlineStyle::Outset => "outset",
        _ => "solid",
    };
    paint_border_edges(
        builder,
        BorderEdges {
            rect: outer,
            radii: outer_radii,
            widths: [width; 4],
            styles: [style; 4],
            colors: [color; 4],
        },
        false,
    );
}

/// CSS Backgrounds 3 #outset-adjusted-border-radius: the radii of `edge`
/// expanded by `outset`, which grow less where a corner is small for its
/// box, and stay zero (square) where they are zero.
fn outset_radii(radii: CornerRadii, edge: CssRect, outset: f32) -> CornerRadii {
    CornerRadii {
        corners: radii.corners.map(|(x, y)| {
            if outset <= 0. {
                return ((x + outset).max(0.), (y + outset).max(0.));
            }
            let ratio = |radius: f32, length: f32| {
                if length > 0. { radius / length } else { 0. }
            };
            let coverage = 2. * ratio(x, edge.width).min(ratio(y, edge.height));
            let adjust = |radius: f32| {
                if radius > outset || coverage > 1. {
                    return radius + outset;
                }
                let ratio = radius / outset;
                radius + outset * (1. - (1. - ratio).powi(3) * (1. - coverage.powi(3)))
            };
            (adjust(x), adjust(y))
        }),
    }
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

fn push_layer(fragment: &Frag, builder: &mut Builder<'_>) -> bool {
    let Some(style) = PaintStyle::of(fragment) else {
        return false;
    };
    let opacity = fragment.paint.opacity.clamp(0.0, 1.0);
    let blend = style
        .value(builder.dom, "mix-blend-mode")
        .as_deref()
        .map(blend_mode)
        .unwrap_or_default();
    // CSS Compositing 1 #isolation: `isolate` groups descendants so their
    // blend modes stop at this element.
    let isolate = style
        .value(builder.dom, "isolation")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("isolate"));
    if opacity < 1.0 || blend != BlendMode::Normal || isolate || !fragment.paint.filters.is_empty()
    {
        builder
            .commands
            .push(DisplayCommand::PushLayer(CompositingLayer {
                opacity,
                blend,
                filters: fragment.paint.filters.clone(),
            }));
        true
    } else {
        false
    }
}

fn paint_transform(fragment: &Frag, _builder: &Builder<'_>) -> Option<Affine2d> {
    let matrix = super::transform::matrix(fragment);
    (!matrix.is_identity()).then_some(matrix)
}

fn paint_background_images(
    fragment: &Frag,
    shape: PaintShape,
    builder: &mut Builder<'_>,
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
        .children(dom.frame_document(frame)?)
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
    let frame = dom.frame_owner(root)?;
    let (document_root, source) = frame_canvas_background(dom, frame)?;
    (document_root == root).then_some(source)
}

fn paint_nested_document_canvas(fragment: &Frag, builder: &mut Builder<'_>) {
    let Some((root, style_node)) = frame_canvas_background(builder.dom, fragment.node) else {
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
    // CSS Compositing 1 #background-blend-mode: blended layers blend only
    // with each other and the canvas background color, never with the
    // embedding document behind the frame.
    let isolated = begin_background_isolation(builder, PaintStyle::Element(style_node));
    builder
        .commands
        .push(DisplayCommand::BeginScroll(container.node));
    // CSS Color Adjust 1 #color-scheme-effect: an embedded document whose
    // root's color scheme differs from the embedding element's gets an
    // opaque Canvas instead of a transparent canvas.
    let differs = builder.dom.color_scheme(root) != builder.dom.color_scheme(fragment.node);
    if let Some(color) = background_color(builder.dom, style_node)
        .filter(|color| !color.is_transparent())
        .or_else(|| differs.then(|| scheme_color(builder.dom.color_scheme(root), "canvas")))
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
    if isolated {
        builder.commands.push(DisplayCommand::PopLayer);
    }
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
    fragment: &Frag,
    style: PaintStyle,
    shape: PaintShape,
    builder: &mut Builder<'_>,
    canvas: Option<CssRect>,
    positioning_override: Option<CssRect>,
) {
    let Some(value) = style.value(builder.dom, "background-image") else {
        return;
    };
    let images = split_top_level(&value, ',');
    // CSS Backgrounds 3 #background-image: `none` counts as a layer but
    // draws nothing. Preserve indices in mixed lists (and the separately
    // painted color's bottom-layer clip), while avoiding image geometry and
    // style resolution when no layer can draw an image.
    if images
        .iter()
        .all(|layer| layer.trim().is_empty() || layer.trim().eq_ignore_ascii_case("none"))
    {
        return;
    }
    let border_box = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    let padding_box = padding_box_with_style(fragment);
    let content_box =
        content_box_with_style(builder.dom, fragment, padding_box, builder.viewport());
    let lengths = LengthBasis::of(builder.dom, style.node(), builder.viewport());
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
    let attachment_value = style
        .value(builder.dom, "background-attachment")
        .unwrap_or_else(|| "scroll".into());
    let attachment_layers = split_top_level(&attachment_value, ',');
    let blend_value = style
        .value(builder.dom, "background-blend-mode")
        .unwrap_or_else(|| "normal".into());
    let blend_layers = split_top_level(&blend_value, ',');
    // CSS Backgrounds 3 #background-attachment: `fixed` is relative to the
    // viewport of the element's own document, which for a frame's document
    // is the frame's scrollport. (A serialized presentation arena has the
    // frame's `data-trust-frame` wrapper in its place.)
    let dom = builder.dom;
    let owner = dom.frame_owner(style.node()).or_else(|| {
        std::iter::successors(dom.node(style.node()).parent, |&node| dom.node(node).parent)
            .find(|&node| dom.attr(node, "data-trust-frame").is_some())
    });
    let frame = owner.and_then(|frame| {
        builder
            .scroll_containers
            .iter()
            .find(|container| container.node == frame)
            .map(|container| (frame, container.viewport))
    });
    let viewport = frame.map_or(
        CssRect::new(0.0, 0.0, builder.viewport_w, builder.viewport_h),
        |(_, viewport)| viewport,
    );
    // A fixed canvas layer's commands move to the viewport-pinned underlay
    // once that layer is done (every path below ends its iteration), after
    // closing the layer's blend group. A frame's canvas instead paints such
    // a layer outside the frame's scroll, re-entering it afterwards.
    let mut fixed_start: Option<usize> = None;
    let mut rescroll: Option<NodeId> = None;
    let mut blending = false;
    let pin_fixed = |builder: &mut Builder<'_>,
                     start: &mut Option<usize>,
                     rescroll: &mut Option<NodeId>,
                     blending: &mut bool| {
        if std::mem::take(blending) {
            builder.commands.push(DisplayCommand::PopLayer);
        }
        if let Some(start) = start.take() {
            let commands = builder.commands.split_off(start);
            builder.fixed_under.extend(commands);
        }
        if let Some(frame) = rescroll.take() {
            builder.commands.push(DisplayCommand::BeginScroll(frame));
        }
    };
    // CSS Backgrounds paints the first listed layer closest to the viewer, so
    // emit in reverse order after the background color.
    for (index, layer) in images.iter().enumerate().rev() {
        pin_fixed(builder, &mut fixed_start, &mut rescroll, &mut blending);
        let layer = layer.trim();
        if layer.eq_ignore_ascii_case("none") || layer.is_empty() {
            continue;
        }
        // CSS Backgrounds 3 #background-attachment: a fixed layer is
        // positioned against the viewport. On the canvas it also paints the
        // viewport and stays there while the document scrolls; other boxes
        // keep their painting area (their snapshot is exact at scroll 0).
        let fixed = layer_value(&attachment_layers, index, "scroll")
            .trim()
            .eq_ignore_ascii_case("fixed");
        let canvas = if fixed && canvas.is_some() {
            match frame {
                Some((frame, _)) => {
                    builder.commands.push(DisplayCommand::EndScroll);
                    rescroll = Some(frame);
                }
                None => fixed_start = Some(builder.commands.len()),
            }
            Some(viewport)
        } else {
            canvas
        };
        // CSS Compositing 1 #background-blend-mode: each layer blends with
        // the layers and color beneath it.
        let blend = blend_mode(layer_value(&blend_layers, index, "normal"));
        if blend != BlendMode::Normal {
            builder
                .commands
                .push(DisplayCommand::PushLayer(CompositingLayer {
                    opacity: 1.0,
                    blend,
                    filters: std::sync::Arc::from([]),
                }));
            blending = true;
        }
        let shape = if fixed && canvas.is_some() {
            PaintShape::Rect(viewport)
        } else {
            shape.clone()
        };
        let positioning_override = if fixed {
            Some(viewport)
        } else {
            positioning_override
        };
        let layer_shape = if canvas.is_some() {
            shape.clone()
        } else {
            background_layer_shape(
                fragment,
                builder.dom,
                layer_value(&clip_layers, index, "border-box"),
                &shape,
                builder.viewport(),
            )
        };
        {
            let origin = layer_value(&origin_layers, index, "padding-box");
            let positioning = positioning_override
                .unwrap_or_else(|| background_box(origin, border_box, padding_box, content_box));
            let size = layer_value(&size_layers, index, "auto auto");
            // A gradient is an image with no natural size or ratio: CSS
            // Backgrounds 3 #background-size resolves its `auto` axes, and
            // cover/contain, to the positioning area. It is then positioned,
            // repeated and clipped exactly like any other image (§§2.4-2.6).
            let gradient = is_gradient(layer);
            let (handle, (mut tile_w, mut tile_h)) = if gradient {
                (None, gradient_size(size, positioning, lengths))
            } else if let Some(url) = css_url(layer) {
                let base = builder.dom.style_resource_base(style.node(), builder.base);
                let source = resolve_image_source(&base, &url);
                let handle = builder.image(source.clone());
                // CSS Backgrounds 3 #background-size: auto/auto with a natural
                // ratio but neither natural dimension uses contain. SVG decoder
                // fallback pixels are not intrinsic width/height. This also keeps
                // the exact ratio for contain/cover and a single definite axis.
                let ratio_only = crate::img::svg_url_ratio_only(&source)
                    .or_else(|| crate::img::svg_ratio_only_get(&source))
                    .filter(|ratio| ratio.is_finite() && *ratio > 0.0);
                // CSS Backgrounds 3 #background-image and CSS Images 3
                // #invalid-image: a loading image, or one that failed to load,
                // still counts as a layer but draws nothing. Only decoded
                // resources have an entry (pending ones carry a sentinel).
                let Some(natural) = builder
                    .images
                    .get(&source)
                    .copied()
                    .filter(|(w, h)| *w > 0 && *h > 0 && *w != u32::MAX && *h != u32::MAX)
                    .map(|(w, h)| (w as f32, h as f32))
                else {
                    continue;
                };
                (
                    Some(handle),
                    background_size(size, natural, positioning, ratio_only, lengths),
                )
            } else {
                continue;
            };
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
            if !tile_w.is_finite() || !tile_h.is_finite() || tile_w <= 0.0 || tile_h <= 0.0 {
                continue;
            }
            let (mut start_x, mut start_y) =
                background_position(position, positioning, (tile_w, tile_h), lengths);
            let mut repeat = repeat;
            if matches!(repeat, BackgroundRepeat::Round) {
                let nx = (positioning.width / tile_w).round().max(1.0);
                let ny = (positioning.height / tile_h).round().max(1.0);
                tile_w = positioning.width / nx;
                tile_h = positioning.height / ny;
                (start_x, start_y) =
                    background_position(position, positioning, (tile_w, tile_h), lengths);
                repeat = BackgroundRepeat::Repeat;
            }
            let tile = match handle {
                Some(handle) => LayerTile::Image(handle),
                None => {
                    match parse_gradient(layer, CssRect::new(0.0, 0.0, tile_w, tile_h), lengths) {
                        Some(brush) => LayerTile::Gradient(brush),
                        None => continue,
                    }
                }
            };
            // Most gradients are one tile covering the whole painting area:
            // fill the clip shape directly, as a single command.
            let placed = CssRect::new(
                positioning.x + start_x,
                positioning.y + start_y,
                tile_w,
                tile_h,
            );
            if let LayerTile::Gradient(_) = tile
                && !matches!(repeat, BackgroundRepeat::Space)
                && placed.x <= clip.x
                && placed.y <= clip.y
                && placed.x + placed.width >= clip.x + clip.width
                && placed.y + placed.height >= clip.y + clip.height
                && let DisplayCommand::Fill { brush, .. } = tile.command(placed, style.node())
            {
                fill_background(builder, layer_shape, brush, &shape);
                continue;
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
                    &tile,
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
            // Repeated tiles meet on device pixels, as in Gecko and Blink:
            // two antialiased edges sharing a fractional pixel would let
            // the content beneath show through as a seam.
            let ratio = builder.dom.device_pixel_ratio();
            let snap = |value: f32, repeat: bool| {
                if repeat {
                    (value * ratio).round() / ratio
                } else {
                    value
                }
            };
            let mut y = y0;
            let mut count = 0usize;
            while y < y_end && count < 4096 {
                let mut x = x0;
                let mut x_count = 0usize;
                while x < x_end && x_count < 4096 {
                    let (left, top) = (snap(x, x_repeat), snap(y, y_repeat));
                    let rect = CssRect::new(
                        left,
                        top,
                        snap(x + tile_w, x_repeat) - left,
                        snap(y + tile_h, y_repeat) - top,
                    );
                    builder.commands.push(tile.command(rect, style.node()));
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
    pin_fixed(builder, &mut fixed_start, &mut rescroll, &mut blending);
}

/// Round a replaced element's content edges to device pixels, as Gecko and
/// Blink do. CSS leaves pixel snapping undefined (css-images-3
/// #the-image-rendering only governs scaling), but without it an image at
/// its natural size and a fractional offset, such as one centered in an
/// odd-width column, is resampled across pixel boundaries and blurs.
fn snap_to_device_pixels(rect: CssRect, ratio: f32) -> CssRect {
    if !(ratio.is_finite() && ratio > 0.0) {
        return rect;
    }
    let snap = |value: f32| (value * ratio).round() / ratio;
    let (left, top) = (snap(rect.x), snap(rect.y));
    CssRect::new(
        left,
        top,
        snap(rect.x + rect.width) - left,
        snap(rect.y + rect.height) - top,
    )
}

/// What one background layer paints into each of its tiles.
enum LayerTile {
    Image(ImageHandle),
    /// Laid out for a tile at the origin, and translated to each tile.
    Gradient(PaintBrush),
}

impl LayerTile {
    fn command(&self, rect: CssRect, node: NodeId) -> DisplayCommand {
        match self {
            Self::Image(handle) => DisplayCommand::Image {
                rect,
                handle: *handle,
                source_rect: None,
                // `rect` is the used background image size after CSS
                // Backgrounds §2.9. Rendering at intrinsic pixels here ignores
                // an authored `background-size` (a 2x source paints about twice
                // as large); fill the already aspect-correct tile exactly.
                fit: ImageFit::Fill,
                sampling: ImageSampling::Smooth,
                clip: None,
                node,
                link: None,
            },
            Self::Gradient(brush) => {
                let shift = |point: CssPoint| CssPoint::new(point.x + rect.x, point.y + rect.y);
                let mut brush = brush.clone();
                match &mut brush {
                    PaintBrush::LinearGradient { start, end, .. } => {
                        *start = shift(*start);
                        *end = shift(*end);
                    }
                    PaintBrush::RadialGradient { center, .. }
                    | PaintBrush::ConicGradient { center, .. } => *center = shift(*center),
                    PaintBrush::Solid(_) => {}
                }
                DisplayCommand::Fill {
                    shape: PaintShape::Rect(rect),
                    brush,
                }
            }
        }
    }
}

fn is_gradient(layer: &str) -> bool {
    let lower = layer.trim_start().to_ascii_lowercase();
    [
        "linear-gradient(",
        "radial-gradient(",
        "repeating-linear-gradient(",
        "repeating-radial-gradient(",
        "conic-gradient(",
        "repeating-conic-gradient(",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

/// CSS Backgrounds 3 #background-size for an image without natural
/// dimensions or ratio: an `auto` axis, and cover/contain, take the
/// positioning area's size.
fn gradient_size(value: &str, area: CssRect, lengths: LengthBasis) -> (f32, f32) {
    let tokens = split_ws(value);
    if tokens.first().is_some_and(|token| {
        token.eq_ignore_ascii_case("cover") || token.eq_ignore_ascii_case("contain")
    }) {
        return (area.width, area.height);
    }
    (
        tokens
            .first()
            .and_then(|token| background_length(token, area.width, lengths))
            .unwrap_or(area.width),
        tokens
            .get(1)
            .and_then(|token| background_length(token, area.height, lengths))
            .unwrap_or(area.height),
    )
}

// A glyph mask can extend beyond the background border; ordinary background
// box shapes are already bounded and need no extra stateful clip commands.
fn fill_background(
    builder: &mut Builder<'_>,
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
    fragment: &Frag,
    dom: &Dom,
    clip: &str,
    border: &PaintShape,
    viewport: Vp,
) -> PaintShape {
    if clip.trim() != "text" {
        let rect = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
        let padding = padding_box_with_style(fragment);
        let content = content_box_with_style(dom, fragment, padding, viewport);
        return background_clip_shape(background_box(clip, rect, padding, content), border, rect);
    }
    fn collect(f: &Frag, dom: &Dom, path: &mut Vec<crate::render::PathElement>) {
        if let FragKind::Line(line) = &f.kind {
            for piece in &line.pieces {
                let node = piece.item.style_node;
                if node != NO_NODE && dom.visibility_hidden(node) {
                    continue;
                }
                let Some(label) = &piece.shaped else {
                    continue;
                };
                let origin =
                    CssPoint::new(f.x + piece.x + piece.paint_x, f.y + piece.y + piece.paint_y);
                // A multiline control's value is its laid-out line runs.
                let runs: Vec<(CssPoint, &crate::text::ShapedText)> = match &piece.control_text {
                    Some(text) => text
                        .lines
                        .iter()
                        .map(|line| {
                            (
                                CssPoint::new(origin.x + line.x, origin.y + line.y),
                                &line.shaped,
                            )
                        })
                        .collect(),
                    None => vec![(origin, label)],
                };
                for (origin, shaped) in runs {
                    let mut shaped = shaped.clone();
                    if node != NO_NODE {
                        (shaped.underline, shaped.strikethrough) = dom.text_decoration(node);
                    }
                    crate::text::append_text_path(
                        path,
                        &shaped,
                        origin,
                        decoration_style(dom, node),
                    );
                }
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

fn padding_box_with_style(fragment: &Frag) -> CssRect {
    let [top, right, bottom, left] = fragment.border;
    CssRect::new(
        fragment.x + left,
        fragment.y + top,
        (fragment.w - left - right).max(0.0),
        (fragment.h - top - bottom).max(0.0),
    )
}

fn content_box_with_style(dom: &Dom, fragment: &Frag, padding: CssRect, viewport: Vp) -> CssRect {
    let width_basis = padding.width.max(0.0);
    let style = PaintStyle::of(fragment);
    let lengths = LengthBasis::of(dom, style.map_or(NO_NODE, PaintStyle::node), viewport);
    let pad = ["top", "right", "bottom", "left"].map(|side| {
        style
            .and_then(|style| style.value(dom, &format!("padding-{side}")))
            .as_deref()
            .and_then(|value| lengths.resolve(value, width_basis))
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

fn background_size(
    value: &str,
    natural: (f32, f32),
    area: CssRect,
    ratio_only: Option<f32>,
    lengths: LengthBasis,
) -> (f32, f32) {
    // Keep a nonzero ratio basis even for a zero-size positioning area: a
    // definite background width/height must still resolve its other axis.
    let natural = ratio_only.map_or(natural, |ratio| (ratio, 1.0));
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
        .and_then(|token| background_length(token, area.width, lengths));
    let height = tokens
        .get(1)
        .and_then(|token| background_length(token, area.height, lengths));
    match (width, height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, natural.1 * w / natural.0),
        (None, Some(h)) => (natural.0 * h / natural.1, h),
        _ if ratio_only.is_some() => {
            let scale = (area.width / natural.0).min(area.height / natural.1);
            (natural.0 * scale, natural.1 * scale)
        }
        _ => natural,
    }
}

fn background_length(value: &str, basis: f32, lengths: LengthBasis) -> Option<f32> {
    if value.eq_ignore_ascii_case("auto") {
        return None;
    }
    lengths.resolve(value, basis).map(|value| value.max(0.01))
}

fn background_position(
    value: &str,
    area: CssRect,
    image: (f32, f32),
    lengths: LengthBasis,
) -> (f32, f32) {
    let tokens = split_ws(value);
    if tokens.len() >= 3 {
        return edge_offset_position(&tokens, area, image, lengths).unwrap_or((0.0, 0.0));
    }
    let (x, y) = match tokens.as_slice() {
        [] => ("0%", "0%"),
        [one] if matches!(one.to_ascii_lowercase().as_str(), "top" | "bottom") => ("50%", *one),
        [one] => (*one, "50%"),
        // CSS Backgrounds 3 #background-position: two keywords can occur
        // in either order. A vertical first keyword (or horizontal second
        // keyword) assigns the axes before resolving percentage offsets.
        [x, y]
            if matches!(x.to_ascii_lowercase().as_str(), "top" | "bottom")
                || matches!(y.to_ascii_lowercase().as_str(), "left" | "right") =>
        {
            (*y, *x)
        }
        [x, y, ..] => (*x, *y),
    };
    (
        background_position_component(x, area.width, image.0, false, lengths),
        background_position_component(y, area.height, image.1, true, lengths),
    )
}

/// The three- and four-value `<bg-position>` forms (CSS Backgrounds 3
/// #background-position): each edge keyword may be followed by an offset
/// from that edge, and `center` takes whichever axis remains.
fn edge_offset_position(
    tokens: &[&str],
    area: CssRect,
    image: (f32, f32),
    lengths: LengthBasis,
) -> Option<(f32, f32)> {
    let keyword = |token: &str| {
        let lower = token.to_ascii_lowercase();
        matches!(
            lower.as_str(),
            "left" | "right" | "top" | "bottom" | "center"
        )
        .then_some(lower)
    };
    let (mut x, mut y, mut centers) = (None, None, 0);
    let mut index = 0;
    while index < tokens.len() {
        let edge = keyword(tokens[index])?;
        let offset = tokens
            .get(index + 1)
            .filter(|token| keyword(token).is_none())
            .copied();
        index += 1 + usize::from(offset.is_some());
        let axis = match edge.as_str() {
            "left" | "right" => &mut x,
            "top" | "bottom" => &mut y,
            _ if offset.is_none() => {
                centers += 1;
                continue;
            }
            _ => return None,
        };
        if axis.replace((edge, offset)).is_some() {
            return None;
        }
    }
    for _ in 0..centers {
        if x.is_none() {
            x = Some(("center".into(), None));
        } else if y.is_none() {
            y = Some(("center".into(), None));
        } else {
            return None;
        }
    }
    let resolve = |(edge, offset): (String, Option<&str>), area: f32, image: f32| {
        let offset = offset
            .and_then(|offset| lengths.resolve(offset, area - image))
            .unwrap_or(0.0);
        match edge.as_str() {
            "right" | "bottom" => area - image - offset,
            "center" => (area - image) / 2.0,
            _ => offset,
        }
    };
    Some((
        resolve(x.unwrap_or(("center".into(), None)), area.width, image.0),
        resolve(y.unwrap_or(("center".into(), None)), area.height, image.1),
    ))
}

fn background_position_component(
    value: &str,
    area: f32,
    image: f32,
    vertical: bool,
    lengths: LengthBasis,
) -> f32 {
    match value.trim().to_ascii_lowercase().as_str() {
        "left" if !vertical => 0.0,
        "top" if vertical => 0.0,
        "center" => (area - image) / 2.0,
        "right" if !vertical => area - image,
        "bottom" if vertical => area - image,
        // A percentage (also inside calc()) is of the area minus the image.
        other => lengths.resolve(other, area - image).unwrap_or(0.0),
    }
}

fn background_clip_shape(clip: CssRect, original: &PaintShape, border: CssRect) -> PaintShape {
    if clip == border {
        original.clone()
    } else if let PaintShape::RoundedRect { radii, .. } = original {
        // CSS Backgrounds 3 §3.11/§5.2: the padding and content edges retain
        // the outer corner curve after subtracting the intervening border
        // and padding widths. A rectangular clip paints square corners inside
        // an otherwise rounded button when background-clip is padding-box.
        rounded_shape(clip, inset_radii(*radii, border, clip))
    } else {
        PaintShape::Rect(clip)
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_spaced_background(
    builder: &mut Builder<'_>,
    clip: CssRect,
    node: NodeId,
    layer: &LayerTile,
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
            builder
                .commands
                .push(layer.command(CssRect::new(x, y, tile_w, tile_h), node));
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

/// The light and dark relief of a 3D border style. CSS Backgrounds 3
/// #border-style leaves the exact colors to the UA; like Gecko
/// (`NS_GetSpecial3DColors`), the light side keeps the border color and the
/// dark side is two thirds of it, except that black uses 70%/30% gray so
/// its relief stays visible.
fn border_shade(color: PaintColor, light: bool) -> PaintColor {
    let PaintColor::Rgba(r, g, b, a) = color else {
        return color;
    };
    if (r, g, b) == (0, 0, 0) {
        let gray = if light { 178 } else { 76 };
        return PaintColor::Rgba(gray, gray, gray, a);
    }
    if light {
        return color;
    }
    let dark = |v: u8| (f32::from(v) * (2.0 / 3.0)) as u8;
    PaintColor::Rgba(dark(r), dark(g), dark(b), a)
}

/// CSS 2.2 Appendix E step 7.2.1.4.1 and CSS Backgrounds 3
/// #box-decoration-break: each line fragment of a non-replaced inline box
/// paints its shadow, background and border beneath the line's content. The
/// fragment spans the box's pieces on this line horizontally and its content
/// area (font ascent to descent, CSS 2.2 §10.6.1) plus vertical padding and
/// border; with `slice`, the start/end edges appear only where the box
/// begins/ends.
fn paint_inline_box_decorations(
    builder: &mut Builder<'_>,
    fragment: &Frag,
    line: &super::flow::LineFrag,
    anonymous_line: bool,
) {
    struct Run {
        node: super::inline::InlineBoxKey,
        left: f32,
        right: f32,
        top: f32,
        bottom: f32,
        starts: bool,
        ends: bool,
    }
    if line.sideways {
        return;
    }
    let mut runs: Vec<Run> = Vec::new();
    for piece in &line.pieces {
        let Some(boxes) = &piece.boxes else {
            continue;
        };
        let start = fragment.x + piece.x;
        let end = start + piece.box_width;
        let (top, bottom) =
            piece
                .shaped
                .as_ref()
                .map_or((f32::INFINITY, f32::NEG_INFINITY), |text| {
                    let baseline = fragment.y + piece.y + text.baseline;
                    (baseline - text.ascent, baseline + text.descent)
                });
        for node in boxes
            .chain
            .iter()
            .filter(|entry| entry.decorated)
            .map(|entry| entry.key)
        {
            let index = runs
                .iter()
                .position(|run| run.node == node)
                .unwrap_or_else(|| {
                    runs.push(Run {
                        node,
                        left: start,
                        right: end,
                        top,
                        bottom,
                        starts: false,
                        ends: false,
                    });
                    runs.len() - 1
                });
            let run = &mut runs[index];
            run.left = run.left.min(start);
            run.right = run.right.max(end);
            run.top = run.top.min(top);
            run.bottom = run.bottom.max(bottom);
            if let Some(&(_, distance)) = boxes.opens.iter().find(|(open, _)| *open == node) {
                run.left = run.left.min(start - distance);
                run.starts = true;
            }
            if let Some(&(_, distance)) = boxes.closes.iter().find(|(close, _)| *close == node) {
                run.right = run.right.max(end + distance);
                run.ends = true;
            }
        }
    }
    for run in runs {
        let (node, pseudo) = run.node;
        let style = match pseudo {
            None => PaintStyle::Element(node),
            Some(which) => PaintStyle::Pseudo(node, which),
        };
        let hidden = match pseudo {
            None => builder.dom.visibility_hidden(node),
            Some(_) => style
                .value(builder.dom, "visibility")
                .is_some_and(|value| matches!(value.trim(), "hidden" | "collapse")),
        };
        if hidden {
            continue;
        }
        let (top, bottom) = if run.top.is_finite() {
            (run.top, run.bottom)
        } else {
            let baseline = fragment.y + line.baseline;
            (baseline - line.ascent, baseline + line.descent)
        };
        let box_style = match pseudo {
            None => super::style::BoxStyle::of(builder.dom, node, builder.viewport()),
            Some(which) => {
                super::style::BoxStyle::of_pseudo(builder.dom, node, which, builder.viewport())
            }
        };
        let basis = Some(fragment.w);
        let pad = |side: usize| box_style.padding[side].resolve(basis).unwrap_or(0.0);
        let [bt, br, bb, bl] = box_style.border;
        let border = [
            bt,
            if run.ends { br } else { 0.0 },
            bb,
            if run.starts { bl } else { 0.0 },
        ];
        let y = top - pad(super::style::TOP) - bt;
        let rect = CssRect::new(
            run.left,
            y,
            (run.right - run.left).max(0.0),
            bottom + pad(super::style::BOTTOM) + bb - y,
        );
        if rect.width <= 0.0 || rect.height <= 0.0 {
            continue;
        }
        let mut paint = fragment.paint.clone();
        paint.pseudo = pseudo.map(|which| (node, which));
        let decoration = Frag {
            flow: fragment.flow,
            node,
            x: rect.x,
            y: rect.y,
            w: rect.width,
            h: rect.height,
            border,
            css_size: None,
            content_size: None,
            content_offset: [border[3], border[0]],
            paint,
            clip: fragment.clip,
            kind: FragKind::Block,
            children: text_clip_line(builder.dom, style, fragment, line, run.node)
                .into_iter()
                .collect(),
        };
        let mut radii = border_radii(builder.dom, style, rect);
        if !run.starts {
            radii.corners[0] = (0.0, 0.0);
            radii.corners[3] = (0.0, 0.0);
        }
        if !run.ends {
            radii.corners[1] = (0.0, 0.0);
            radii.corners[2] = (0.0, 0.0);
        }
        let scroll_depth = if fragment.paint.outside_marker {
            builder.push_scroll_ancestors(node)
        } else if anonymous_line {
            builder.push_scroll_content_chain(node)
        } else {
            0
        };
        let clipped = anonymous_line
            .then(|| builder.ancestor_clip(node, fragment.clip))
            .flatten()
            .is_some_and(|clip| builder.push_hard_clip(clip));
        let shape = rounded_shape(rect, radii);
        let slice = (run.starts, run.ends);
        paint_sliced_box_shadows(builder, style, rect, radii, border, slice, false);
        let isolated = begin_background_isolation(builder, style);
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
                &decoration,
                builder.dom,
                layer_value(&clips, index, "border-box"),
                &shape,
                builder.viewport(),
            );
            fill_background(builder, color_shape, PaintBrush::Solid(color), &shape);
        }
        paint_background_images(&decoration, shape, builder, None);
        if isolated {
            builder.commands.push(DisplayCommand::PopLayer);
        }
        paint_sliced_box_shadows(builder, style, rect, radii, border, slice, true);
        paint_borders(&decoration, radii, builder);
        if clipped {
            builder.pop_hard_clip();
        }
        builder.pop_scroll_ancestors(scroll_depth);
    }
}

/// CSS Backgrounds 4 #valdef-background-clip-text: a text-clipped background
/// is masked by the text of the element and its in-flow descendants. For an
/// inline box that is its pieces on this line, which `background_layer_shape`
/// finds as a line child of the box's decoration fragment.
fn text_clip_line(
    dom: &Dom,
    style: PaintStyle,
    fragment: &Frag,
    line: &super::flow::LineFrag,
    node: super::inline::InlineBoxKey,
) -> Option<Frag> {
    let clips = style.value(dom, "background-clip")?;
    if !split_top_level(&clips, ',')
        .iter()
        .any(|clip| clip.trim().eq_ignore_ascii_case("text"))
    {
        return None;
    }
    let pieces = line
        .pieces
        .iter()
        .filter(|piece| {
            piece
                .boxes
                .as_ref()
                .is_some_and(|boxes| boxes.chain.iter().any(|entry| entry.key == node))
        })
        .cloned()
        .collect();
    Some(Frag {
        flow: fragment.flow,
        node: fragment.node,
        x: fragment.x,
        y: fragment.y,
        w: fragment.w,
        h: fragment.h,
        border: [0.0; 4],
        css_size: None,
        content_size: None,
        content_offset: [0.0; 2],
        paint: Default::default(),
        clip: fragment.clip,
        kind: FragKind::Line(std::sync::Arc::new(super::flow::LineFrag {
            sideways: line.sideways,
            atom_boxes: Vec::new(),
            band: line.band,
            alignment_offset: line.alignment_offset,
            justification: line.justification,
            pieces,
            contains_atomic_inline: false,
            width: line.width,
            height: line.height,
            baseline: line.baseline,
            ascent: line.ascent,
            descent: line.descent,
            forced: line.forced,
        })),
        children: Vec::new(),
    })
}

/// CSS Backgrounds and Borders §6: background first, then border. A uniform
/// solid border is one filled ring and a uniform dashed one one stroked
/// path; non-uniform sides retain each side's own color/style and CSS-pixel
/// width.
fn paint_borders(fragment: &Frag, radii: CornerRadii, builder: &mut Builder<'_>) {
    let Some(style) = PaintStyle::of(fragment) else {
        return;
    };
    if paint_border_image(fragment, style, builder) {
        return;
    }
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
            .unwrap_or_else(|| text_color_for_style(builder.dom, style))
    });
    let collapse = style.value(builder.dom, "border-collapse").as_deref() == Some("collapse");
    paint_border_edges(
        builder,
        BorderEdges {
            rect: CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
            radii,
            widths: fragment.border,
            styles: styles.each_ref().map(String::as_str),
            colors,
        },
        collapse,
    );
}

/// Paint the band of each of a box's border edges, or of its outline.
fn paint_border_edges(builder: &mut Builder<'_>, edges: BorderEdges<'_>, collapse: bool) {
    let BorderEdges {
        rect,
        radii,
        widths,
        styles,
        colors,
    } = edges;
    let [top, right, bottom, left] = widths;
    let owners = corner_owners(widths, &styles);
    let splits = corner_splits(rect, radii, widths, &owners);
    // The bands of ring that solid, double and 3D sides fill, each as the
    // fractions of the border width it spans and its color. #line-style
    // permits UA-chosen band thickness and shading, but double must have a
    // gap and the 3D styles must preserve their opposite relief.
    let fills: [Option<Vec<(f32, f32, PaintColor)>>; 4] = std::array::from_fn(|side| {
        let color = colors[side];
        if widths[side] <= 0. {
            return None;
        }
        let mut kind = styles[side];
        if collapse {
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
        Some(match kind {
            "solid" => vec![(0., 1., color)],
            "double" => vec![(0., 1. / 3., color), (2. / 3., 1., color)],
            "groove" | "ridge" => vec![
                (0., 0.5, border_shade(color, light)),
                (0.5, 1., border_shade(color, !light)),
            ],
            "inset" | "outset" => vec![(0., 1., border_shade(color, light))],
            _ => return None,
        })
    });
    // Adjoining sides that fill alike are filled together: clipping each
    // to its own part of the shared corner would antialias both clips on
    // the same line and let the background show through between them, a
    // seam Gecko and Blink do not draw.
    let breaks = (0..4)
        .filter(|&side| fills[side].is_some() && fills[side] != fills[(side + 3) % 4])
        .collect::<Vec<_>>();
    if breaks.is_empty()
        && let Some(bands) = &fills[0]
    {
        for &(start, end, color) in bands {
            builder.commands.push(DisplayCommand::Fill {
                shape: border_ring(rect, radii, widths, start, end),
                brush: PaintBrush::Solid(color),
            });
        }
    }
    for &first in &breaks {
        let Some(bands) = &fills[first] else {
            continue;
        };
        let count = (1..4)
            .take_while(|offset| fills[(first + offset) % 4] == fills[first])
            .count()
            + 1;
        builder
            .commands
            .push(DisplayCommand::PushClip(sides_clip(first, count, &splits)));
        for &(start, end, color) in bands {
            builder.commands.push(DisplayCommand::Fill {
                shape: border_ring(rect, radii, widths, start, end),
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
    if uniform && top > 0.0 && styles[0] == "dashed" {
        // CSS Backgrounds 3 #corner-shaping: the outer edge follows the
        // border radii and the padding edge those radii less the border
        // width, so a uniform dashed border is dashed along the ring's
        // center line.
        let inset = top / 2.0;
        let center = CssRect::new(
            rect.x + inset,
            rect.y + inset,
            (rect.width - top).max(0.0),
            (rect.height - top).max(0.0),
        );
        // A rounded corner whose radius is at most half the width leaves
        // the center line's corner square, and its mitered stroke would
        // cover the rounded outer corner, so keep the stroke within the
        // border area's ring. Larger radii already put the stroke's edges
        // on the curves, and a second antialiased edge would thin them.
        let clipped = radii
            .corners
            .iter()
            .any(|&(x, y)| x > 0. && y > 0. && x.min(y) <= inset);
        if clipped {
            builder.commands.push(DisplayCommand::PushClip(border_ring(
                rect, radii, widths, 0., 1.,
            )));
        }
        builder.commands.push(DisplayCommand::Stroke {
            shape: rounded_shape(center, inset_radii(radii, rect, center)),
            brush: PaintBrush::Solid(colors[0]),
            style: stroke_for_border(top, styles[0]),
        });
        if clipped {
            builder.commands.push(DisplayCommand::PopClip);
        }
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
        if width <= 0.0 || styles[index] != "dashed" {
            continue;
        }
        // CSS Backgrounds 3 #corner-transitions: adjoining sides meet
        // between the outer and inner corners, including transparent sides
        // and a zero-sized padding box (the usual CSS triangle).
        builder
            .commands
            .push(DisplayCommand::PushClip(sides_clip(index, 1, &splits)));
        if radii.corners.iter().all(|&(x, y)| x <= 0. || y <= 0.) {
            builder.commands.push(DisplayCommand::Stroke {
                shape: PaintShape::Path(vec![PathElement::MoveTo(start), PathElement::LineTo(end)]),
                brush: PaintBrush::Solid(colors[index]),
                style: stroke_for_border(width, styles[index]),
            });
        } else {
            // #corner-shaping: every style follows the curve, so dash the
            // ring's center line, kept within the ring where the adjoining
            // side is thinner.
            let half = widths.map(|width| width / 2.);
            let middle = CssRect::new(
                rect.x + half[3],
                rect.y + half[0],
                (rect.width - half[1] - half[3]).max(0.),
                (rect.height - half[0] - half[2]).max(0.),
            );
            builder.commands.push(DisplayCommand::PushClip(border_ring(
                rect, radii, widths, 0., 1.,
            )));
            builder.commands.push(DisplayCommand::Stroke {
                shape: PaintShape::Path(rounded_contour(
                    middle,
                    inset_radii(radii, rect, middle),
                    false,
                )),
                brush: PaintBrush::Solid(colors[index]),
                style: stroke_for_border(width, styles[index]),
            });
            builder.commands.push(DisplayCommand::PopClip);
        }
        builder.commands.push(DisplayCommand::PopClip);
    }
    paint_dotted_sides(builder, edges);
}

/// Paint one inline box fragment's outer or inner shadows.
/// CSS Backgrounds 3 #box-decoration-break `slice`: shadows belong to the
/// unbroken box, so a fragment casts none from the edges where it is cut.
fn paint_sliced_box_shadows(
    builder: &mut Builder<'_>,
    style: PaintStyle,
    rect: CssRect,
    radii: CornerRadii,
    border: [f32; 4],
    (starts, ends): (bool, bool),
    inset: bool,
) {
    if starts && ends {
        paint_box_shadows(builder, style, rect, radii, border, inset);
        return;
    }
    const REACH: f32 = 1.0e5;
    let left = if starts { rect.x } else { rect.x - REACH };
    let right = rect.x + rect.width + if ends { 0.0 } else { REACH };
    let unbroken = CssRect::new(left, rect.y, right - left, rect.height);
    let clip_left = if starts { left - REACH } else { rect.x };
    let clip_right = if ends {
        right + REACH
    } else {
        rect.x + rect.width
    };
    let pushed = builder.push_hard_clip(CssRect::new(
        clip_left,
        rect.y - REACH,
        clip_right - clip_left,
        rect.height + 2.0 * REACH,
    ));
    paint_box_shadows(builder, style, unbroken, radii, border, inset);
    if pushed {
        builder.pop_hard_clip();
    }
}

/// CSS Backgrounds 3 #shadow-layers: outer box-shadows are drawn immediately
/// below the element's background and inner shadows immediately above it,
/// below the borders, so callers paint each kind at its own step. Per
/// #shadow-shape an outer shadow is cast by the border box (`rect`) and an
/// inner shadow inside the padding box, whose corners follow the inner
/// border edge. The first shadow is on top, so emit the list back to front.
fn paint_box_shadows(
    builder: &mut Builder<'_>,
    style: PaintStyle,
    rect: CssRect,
    radii: CornerRadii,
    border: [f32; 4],
    inset: bool,
) {
    let Some(value) = style.value(builder.dom, "box-shadow") else {
        return;
    };
    let lengths = LengthBasis::of(builder.dom, style.node(), builder.viewport());
    let border_shape = rounded_shape(rect, radii);
    let shape = if inset {
        let [top, right, bottom, left] = border;
        let padding = CssRect::new(
            rect.x + left,
            rect.y + top,
            (rect.width - left - right).max(0.0),
            (rect.height - top - bottom).max(0.0),
        );
        background_clip_shape(padding, &border_shape, rect)
    } else {
        border_shape
    };
    for shadow in split_top_level(&value, ',').into_iter().rev() {
        if shadow.trim().eq_ignore_ascii_case("none") {
            continue;
        }
        let tokens = split_ws(shadow);
        if tokens.iter().any(|t| t.eq_ignore_ascii_case("inset")) != inset {
            continue;
        }
        // CSS Backgrounds 3 #shadow-color: an absent color is currentColor.
        let color = tokens
            .iter()
            .find_map(|token| resolve_color_for_style(builder.dom, style, token))
            .unwrap_or_else(|| text_color_for_style(builder.dom, style));
        let lengths: Vec<f32> = tokens
            .iter()
            .filter_map(|token| lengths.resolve(token, 0.0))
            .collect();
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

/// A border or outline stroke along the center of its band. Corners join
/// mitered, so that square corners stay square (CSS Backgrounds 3
/// #corner-shaping).
fn stroke_for_border(width: f32, style: &str) -> StrokeStyle {
    let mut stroke = StrokeStyle::solid(width);
    stroke.join = LineJoin::Miter;
    if style == "dashed" {
        stroke.dash = vec![width * 3.0, width * 2.0];
    }
    stroke
}

/// Which adjoining side paints a corner's transition region (CSS
/// Backgrounds 3 #corner-transitions). Corner `c` joins the incoming side
/// `(c + 3) % 4` to the outgoing side `c`, in top/right/bottom/left order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CornerOwner {
    /// The sides meet on the line from the outer to the inner corner.
    Split,
    Incoming,
    Outgoing,
}

fn visible_border(width: f32, style: &str) -> bool {
    width > 0. && !matches!(style, "none" | "hidden")
}

/// Like Gecko, a `dotted` side never paints part of a dot in a corner it
/// shares with another style: that side takes the whole corner, and the
/// thicker of two dotted sides draws their corner.
fn corner_owners<S: AsRef<str>>(widths: [f32; 4], styles: &[S; 4]) -> [CornerOwner; 4] {
    std::array::from_fn(|corner| {
        let (incoming, outgoing) = ((corner + 3) % 4, corner);
        let visible = |side: usize| visible_border(widths[side], styles[side].as_ref());
        let dotted = |side: usize| visible(side) && styles[side].as_ref() == "dotted";
        match (dotted(incoming), dotted(outgoing)) {
            (true, true) if (widths[incoming] - widths[outgoing]).abs() < 0.01 => {
                CornerOwner::Split
            }
            (true, true) if widths[incoming] > widths[outgoing] => CornerOwner::Incoming,
            (true, true) => CornerOwner::Outgoing,
            (true, false) if visible(outgoing) => CornerOwner::Outgoing,
            (false, true) if visible(incoming) => CornerOwner::Incoming,
            _ => CornerOwner::Split,
        }
    })
}

/// The boundary between the two sides at each corner of `rect`, from its
/// outer corner inward (CSS Backgrounds 3 #corner-transitions).
///
/// At a square corner the sides meet on the line to the inner corner. At a
/// rounded corner, as in Gecko, that line continues to the nearer of the
/// padding box's midlines, so that it crosses the whole curve: the crossing
/// stays within the corner's transition region and moves monotonically with
/// the ratio of the border widths, and a zero-width side leaves the whole
/// region to the other. A side that owns the corner instead takes the whole
/// region, bounded by the other side's edge.
fn corner_splits(
    rect: CssRect,
    radii: CornerRadii,
    widths: [f32; 4],
    owners: &[CornerOwner; 4],
) -> [Vec<CssPoint>; 4] {
    let [top, right, bottom, left] = widths;
    let inner = CssRect::new(
        rect.x + left,
        rect.y + top,
        (rect.width - left - right).max(0.),
        (rect.height - top - bottom).max(0.),
    );
    let middle = CssPoint::new(inner.x + inner.width / 2., inner.y + inner.height / 2.);
    let (x0, y0, x1, y1) = (rect.x, rect.y, rect.x + rect.width, rect.y + rect.height);
    // Outer corner, inner corner, and the side widths across x and y.
    let corners = [
        (
            CssPoint::new(x0, y0),
            CssPoint::new(inner.x, inner.y),
            left,
            top,
        ),
        (
            CssPoint::new(x1, y0),
            CssPoint::new(inner.x + inner.width, inner.y),
            right,
            top,
        ),
        (
            CssPoint::new(x1, y1),
            CssPoint::new(inner.x + inner.width, inner.y + inner.height),
            right,
            bottom,
        ),
        (
            CssPoint::new(x0, y1),
            CssPoint::new(inner.x, inner.y + inner.height),
            left,
            bottom,
        ),
    ];
    std::array::from_fn(|corner| {
        let (outer, square, width_x, width_y) = corners[corner];
        let (rx, ry) = radii.corners[corner];
        let rounded = rx > 0. && ry > 0.;
        let (sx, sy) = [(1., 1.), (-1., 1.), (-1., -1.), (1., -1.)][corner];
        // The far corner of the transition region: the inner curve's center.
        let region = if rounded {
            CssPoint::new(
                outer.x + sx * rx.max(width_x),
                outer.y + sy * ry.max(width_y),
            )
        } else {
            square
        };
        let even = corner.is_multiple_of(2);
        // Corners 0 and 2 have a vertical incoming and a horizontal outgoing
        // side; corners 1 and 3 the reverse.
        let along_vertical = CssPoint::new(outer.x, region.y);
        let along_horizontal = CssPoint::new(region.x, outer.y);
        match owners[corner] {
            CornerOwner::Split if rounded => vec![outer, toward_middle(outer, square, middle)],
            CornerOwner::Split => vec![outer, square],
            CornerOwner::Outgoing if even => vec![outer, along_vertical, region],
            CornerOwner::Outgoing => vec![outer, along_horizontal, region],
            CornerOwner::Incoming if even => vec![outer, along_horizontal, region],
            CornerOwner::Incoming => vec![outer, along_vertical, region],
        }
    })
}

/// Continue the line from `outer` through `inner` until it reaches the
/// nearer of the vertical and horizontal lines through `middle`.
fn toward_middle(outer: CssPoint, inner: CssPoint, middle: CssPoint) -> CssPoint {
    let (dx, dy) = (inner.x - outer.x, inner.y - outer.y);
    match (dx == 0., dy == 0.) {
        (true, true) => middle,
        (true, false) => CssPoint::new(inner.x, middle.y),
        (false, true) => CssPoint::new(middle.x, inner.y),
        (false, false) => {
            let scale = ((middle.x - outer.x) / dx).min((middle.y - outer.y) / dy);
            CssPoint::new(outer.x + dx * scale, outer.y + dy * scale)
        }
    }
}

/// The part of the border area that `count` consecutive sides from `first`
/// paint: their edges plus their share of the corners at either end, as
/// divided by `splits`.
fn sides_clip(first: usize, count: usize, splits: &[Vec<CssPoint>; 4]) -> PaintShape {
    let corner = |offset: usize| &splits[(first + offset) % 4];
    // Along the outer edge, through the corners between the sides…
    let mut points = (0..count)
        .map(|offset| corner(offset)[0])
        .collect::<Vec<_>>();
    // …in at the last corner and back along the inner edge…
    points.extend(corner(count).iter().copied());
    for offset in (1..count).rev() {
        points.extend(corner(offset).last().copied());
    }
    // …and out at the first corner.
    points.extend(corner(0).iter().rev().take(corner(0).len() - 1));
    PaintShape::Polygon {
        points,
        evenodd: false,
    }
}

/// A box's border edges in top/right/bottom/left order.
#[derive(Clone, Copy)]
struct BorderEdges<'a> {
    /// The border box.
    rect: CssRect,
    /// Outer border radii.
    radii: CornerRadii,
    widths: [f32; 4],
    styles: [&'a str; 4],
    colors: [PaintColor; 4],
}

/// How a dotted side's run of dots ends at one corner.
#[derive(Clone, Copy)]
enum DotEnd {
    /// The corner is shared with an equally thick dotted side: both runs end
    /// with a dot on the middle of the corner's center curve.
    Shared,
    /// The run ends `inset` along the side from the outer corner, with a dot
    /// there when `filled` and otherwise one gap from it.
    Edge { inset: f32, filled: bool },
}

/// CSS Backgrounds 3 #border-style: `dotted` is "a series of round dots".
/// The spacing is the UA's, and spacing "that makes the corners symmetrical"
/// is encouraged, so like Gecko each side divides its run into an even
/// number of dot-sized steps that alternate dot and gap, and two equally
/// thick dotted sides meet on one dot centered on their corner's curve. Dots
/// at most two device pixels across are squares on the pixel grid, as in
/// Gecko and Blink, since so small a circle only rasterizes as a blur.
fn paint_dotted_sides(builder: &mut Builder<'_>, edges: BorderEdges<'_>) {
    let BorderEdges {
        rect,
        radii,
        widths,
        styles,
        colors,
    } = edges;
    let dotted =
        |side: usize| visible_border(widths[side], styles[side]) && styles[side] == "dotted";
    if !(0..4).any(dotted) {
        return;
    }
    let owners = corner_owners(widths, &styles);
    let ratio = builder.dom.device_pixel_ratio().max(f32::EPSILON);
    // Outer corners and the directions into the box from each.
    let outer = [
        CssPoint::new(rect.x, rect.y),
        CssPoint::new(rect.x + rect.width, rect.y),
        CssPoint::new(rect.x + rect.width, rect.y + rect.height),
        CssPoint::new(rect.x, rect.y + rect.height),
    ];
    let inward = [(1., 1.), (-1., 1.), (-1., -1.), (1., -1.)];
    // A side runs clockwise from corner `side` to corner `side + 1`.
    let direction = [(1., 0.), (0., 1.), (-1., 0.), (0., -1.)];
    for side in (0..4).filter(|&side| dotted(side)) {
        let width = widths[side];
        let horizontal = side.is_multiple_of(2);
        let ends = [side, (side + 1) % 4].map(|corner| {
            let other = if corner == side {
                (side + 3) % 4
            } else {
                corner
            };
            let along = if horizontal {
                radii.corners[corner].0
            } else {
                radii.corners[corner].1
            };
            let ours = match owners[corner] {
                CornerOwner::Split => true,
                CornerOwner::Incoming => corner != side,
                CornerOwner::Outgoing => corner == side,
            };
            if owners[corner] == CornerOwner::Split && dotted(other) {
                DotEnd::Shared
            } else if ours || !visible_border(widths[other], styles[other]) {
                DotEnd::Edge {
                    inset: along.max(width / 2.),
                    filled: true,
                }
            } else {
                DotEnd::Edge {
                    inset: along.max(widths[other]) + width / 2.,
                    filled: false,
                }
            }
        });
        // The centerline of this side from its start anchor to its end.
        let (dx, dy) = direction[side];
        let mut run = Vec::new();
        for (position, corner) in [side, (side + 1) % 4].into_iter().enumerate() {
            let at_start = position == 0;
            let origin = outer[corner];
            let (sx, sy) = inward[corner];
            match ends[position] {
                DotEnd::Shared => {
                    // The quarter ellipse halfway through the border, from
                    // the vertical side (0) to the horizontal side (90°).
                    let (rx, ry) = radii.corners[corner];
                    let (ex, ey) = (rx.max(width / 2.), ry.max(width / 2.));
                    let (ax, ay) = (ex - width / 2., ey - width / 2.);
                    let center = CssPoint::new(origin.x + sx * ex, origin.y + sy * ey);
                    let point = |angle: f32| {
                        CssPoint::new(
                            center.x - sx * ax * angle.cos(),
                            center.y - sy * ay * angle.sin(),
                        )
                    };
                    let steps = if ax > 0. || ay > 0. { 8 } else { 0 };
                    // From the middle of the curve to this side's end of it,
                    // or back.
                    let far = if horizontal { FRAC_PI_2 } else { 0. };
                    let middle = FRAC_PI_2 / 2.;
                    let angles = (0..=steps).map(|step| {
                        let t = if steps == 0 {
                            0.
                        } else {
                            step as f32 / steps as f32
                        };
                        if at_start {
                            middle + (far - middle) * t
                        } else {
                            far + (middle - far) * t
                        }
                    });
                    run.extend(angles.map(point));
                }
                DotEnd::Edge { inset, .. } => {
                    let offset = if at_start { inset } else { -inset };
                    let (across_x, across_y) = if horizontal {
                        (0., sy * width / 2.)
                    } else {
                        (sx * width / 2., 0.)
                    };
                    run.push(CssPoint::new(
                        origin.x + dx * offset + across_x,
                        origin.y + dy * offset + across_y,
                    ));
                }
            }
        }
        let filled = ends.map(|end| match end {
            DotEnd::Shared => true,
            DotEnd::Edge { filled, .. } => filled,
        });
        let thin = width * ratio <= 2.0 + 1e-3;
        let mut centers = dot_centers(&run, width, filled[0], filled[1], thin);
        // A dot on a shared corner belongs to both sides. The side leaving
        // the corner draws it when their colors agree; otherwise each side
        // draws its half, divided along the line from the outer corner.
        let mut halves = Vec::new();
        for (position, corner) in [side, (side + 1) % 4].into_iter().enumerate() {
            if !matches!(ends[position], DotEnd::Shared) {
                continue;
            }
            let anchor = if position == 0 {
                run[0]
            } else {
                run[run.len() - 1]
            };
            let index = if position == 0 {
                0
            } else {
                centers.len().saturating_sub(1)
            };
            let Some(&center) = centers.get(index) else {
                continue;
            };
            if (center.x - anchor.x).abs() > 0.01 || (center.y - anchor.y).abs() > 0.01 {
                continue;
            }
            let other = if position == 0 {
                (side + 3) % 4
            } else {
                (side + 1) % 4
            };
            if colors[other] == colors[side] {
                if position == 1 {
                    centers.remove(index);
                }
                continue;
            }
            centers.remove(index);
            // Away from the corner along this side, and along the corner's
            // diagonal through the dot.
            let origin = outer[corner];
            let (diagonal_x, diagonal_y) = (anchor.x - origin.x, anchor.y - origin.y);
            let distance = diagonal_x.hypot(diagonal_y).max(f32::EPSILON);
            let reach = 4. * (distance + width);
            let sign = if position == 0 { 1. } else { -1. };
            halves.push((
                center,
                PaintShape::Polygon {
                    points: vec![
                        origin,
                        CssPoint::new(origin.x + dx * sign * reach, origin.y + dy * sign * reach),
                        CssPoint::new(
                            origin.x + diagonal_x / distance * reach,
                            origin.y + diagonal_y / distance * reach,
                        ),
                    ],
                    evenodd: false,
                },
            ));
        }
        let mut path = Vec::new();
        for center in centers {
            push_dot(&mut path, center, width, thin, ratio);
        }
        let brush = PaintBrush::Solid(colors[side]);
        if !path.is_empty() {
            builder.commands.push(DisplayCommand::Fill {
                shape: PaintShape::Path(path),
                brush: brush.clone(),
            });
        }
        for (center, clip) in halves {
            let mut path = Vec::new();
            push_dot(&mut path, center, width, thin, ratio);
            builder.commands.push(DisplayCommand::PushClip(clip));
            builder.commands.push(DisplayCommand::Fill {
                shape: PaintShape::Path(path),
                brush: brush.clone(),
            });
            builder.commands.push(DisplayCommand::PopClip);
        }
    }
}

/// Dot centers along the polyline `run`, which is divided into an even
/// number of steps of about `width` (odd when exactly one end is unfilled)
/// that alternate dot and gap. Thin steps are never shorter than `width`, so
/// that every dot and gap keeps a whole pixel on the grid.
fn dot_centers(
    run: &[CssPoint],
    width: f32,
    start_filled: bool,
    end_filled: bool,
    thin: bool,
) -> Vec<CssPoint> {
    if run.is_empty() || width <= 0. {
        return Vec::new();
    }
    let mut lengths = vec![0f32];
    for pair in run.windows(2) {
        let step = (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y);
        lengths.push(lengths[lengths.len() - 1] + step);
    }
    let total = lengths[lengths.len() - 1];
    let at = |distance: f32| {
        let index = lengths
            .windows(2)
            .position(|pair| distance <= pair[1])
            .unwrap_or(lengths.len().saturating_sub(2));
        let (Some(&from), Some(&to)) = (run.get(index), run.get(index + 1)) else {
            return run[0];
        };
        let span = lengths[index + 1] - lengths[index];
        let t = if span > 0. {
            ((distance - lengths[index]) / span).clamp(0., 1.)
        } else {
            0.
        };
        CssPoint::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t)
    };
    if total < width {
        // Too short for two dots: one in the middle, if neither end is a gap.
        return if start_filled && end_filled {
            vec![at(total / 2.)]
        } else {
            Vec::new()
        };
    }
    let steps = total / width;
    let mut steps = if thin { steps.floor() } else { steps.round() }.clamp(1., 65536.) as usize;
    if steps.is_multiple_of(2) != (start_filled == end_filled) {
        steps = if thin && steps > 1 {
            steps - 1
        } else {
            steps + 1
        };
    }
    let step = total / steps as f32;
    (0..=steps)
        .filter(|index| index.is_multiple_of(2) == start_filled)
        .map(|index| at(index as f32 * step))
        .collect()
}

/// One dot of `width` across: a circle, or a square on the device-pixel grid
/// when thin.
fn push_dot(path: &mut Vec<PathElement>, center: CssPoint, width: f32, thin: bool, ratio: f32) {
    let radius = width / 2.;
    let point = CssPoint::new;
    if thin {
        // The device pixels whose centers the dot covers, at least one.
        let snap = |middle: f32| {
            let start = ((middle - radius) * ratio - 0.5).ceil();
            let end = ((middle + radius) * ratio - 0.5).ceil().max(start + 1.);
            (start / ratio, end / ratio)
        };
        let ((x0, x1), (y0, y1)) = (snap(center.x), snap(center.y));
        path.extend([
            PathElement::MoveTo(point(x0, y0)),
            PathElement::LineTo(point(x1, y0)),
            PathElement::LineTo(point(x1, y1)),
            PathElement::LineTo(point(x0, y1)),
            PathElement::Close,
        ]);
        return;
    }
    let k = 0.5522848 * radius;
    let (x, y) = (center.x, center.y);
    path.extend([
        PathElement::MoveTo(point(x + radius, y)),
        PathElement::CurveTo(
            point(x + radius, y + k),
            point(x + k, y + radius),
            point(x, y + radius),
        ),
        PathElement::CurveTo(
            point(x - k, y + radius),
            point(x - radius, y + k),
            point(x - radius, y),
        ),
        PathElement::CurveTo(
            point(x - radius, y - k),
            point(x - k, y - radius),
            point(x, y - radius),
        ),
        PathElement::CurveTo(
            point(x + k, y - radius),
            point(x + radius, y - k),
            point(x + radius, y),
        ),
        PathElement::Close,
    ]);
}

fn rectangular_overflow_clip(dom: &Dom, fragment: &Frag) -> Option<Clip> {
    if matches!(dom.tag_name(fragment.node), Some("iframe" | "frame")) {
        return None;
    }
    let [x, y] =
        overflow_axes(dom, fragment.node).map(|value| matches!(value.as_str(), "hidden" | "clip"));
    if !x && !y {
        return None;
    }
    let padding = padding_box(fragment);
    Some(Clip {
        x0: if x { padding.x } else { f32::NEG_INFINITY },
        x1: if x {
            padding.x + padding.width
        } else {
            f32::INFINITY
        },
        y0: if y { padding.y } else { f32::NEG_INFINITY },
        y1: if y {
            padding.y + padding.height
        } else {
            f32::INFINITY
        },
    })
}

fn rounded_overflow_clip(dom: &Dom, fragment: &Frag) -> Option<PaintShape> {
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
        // CSS Values 4 #calc-ieee censors NaN to zero before range clamping.
        let clamp_radius = |radius: f32| {
            if radius.is_nan() {
                0.0
            } else {
                radius.clamp(0.0, f32::MAX)
            }
        };
        corners[index] = (clamp_radius(x), clamp_radius(y));
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

fn parse_gradient(value: &str, rect: CssRect, lengths: LengthBasis) -> Option<PaintBrush> {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("conic-gradient(") {
        return parse_conic_gradient(function_body(value)?, false, rect, lengths);
    } else if lower.starts_with("repeating-conic-gradient(") {
        return parse_conic_gradient(function_body(value)?, true, rect, lengths);
    }
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
    let mut ending_shape = None;
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
        ending_shape = Some(radial_ending_shape(&header, rect, lengths)?);
        parts.remove(0);
    }
    let (center, radius_x, radius_y) = match ending_shape {
        Some(shape) => shape,
        None if radial => radial_ending_shape("", rect, lengths)?,
        None => (CssPoint::default(), 0.0, 0.0),
    };
    // CSS Images 3 #color-stop-syntax: positions are fractions of the
    // gradient line (linear) or ray (radial).
    let dx = angle.sin();
    let dy = -angle.cos();
    let line = if radial {
        radius_x
    } else {
        rect.width * dx.abs() + rect.height * dy.abs()
    };
    let (stops, hints) = parse_stops(&parts, line, lengths)?;
    // CSS Color 4 #interpolation: without a <color-interpolation-method>,
    // stops written only in legacy sRGB syntax still interpolate in
    // gamma-encoded sRGB, for Web compatibility; any other stop selects Oklab.
    let interpolation =
        if !explicit_interpolation && stops.iter().all(|stop| legacy_srgb(&stop.color)) {
            GradientInterpolation {
                space: color::ColorSpaceTag::Srgb,
                ..interpolation
            }
        } else {
            interpolation
        };
    let mut stops = expand_color_hints(stops, &hints, interpolation);
    // CSS Images 3 #repeating-gradients: the stops repeat with the period
    // from the first to the last stop; a zero period paints their average.
    let (mut from, mut to) = (0.0, 1.0);
    if repeating {
        let (first, last) = (stops.first()?.offset, stops.last()?.offset);
        if last - first <= f32::EPSILON || !(last - first).is_finite() {
            return Some(PaintBrush::Solid(average_stop_color(&stops)));
        }
        for stop in &mut stops {
            stop.offset = (stop.offset - first) / (last - first);
        }
        (from, to) = (first, last);
    }
    if radial {
        Some(PaintBrush::RadialGradient {
            center,
            start_radius: radius_x * from,
            radius: radius_x * to,
            aspect: if radius_x > 0.0 {
                radius_y / radius_x
            } else {
                1.0
            },
            stops,
            interpolation,
            repeat: repeating,
        })
    } else {
        let center = CssPoint::new(rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
        let half = line / 2.0;
        let start = CssPoint::new(center.x - dx * half, center.y - dy * half);
        let along = |fraction: f32| {
            CssPoint::new(
                start.x + dx * line * fraction,
                start.y + dy * line * fraction,
            )
        };
        Some(PaintBrush::LinearGradient {
            start: along(from),
            end: along(to),
            stops,
            interpolation,
            repeat: repeating,
        })
    }
}

/// CSS Images 4 #conic-gradient-syntax:
/// `[ [ from <angle> ]? [ at <position> ]? ] || <color-interpolation-method>`,
/// then an `<angular-color-stop-list>`. Stop and hint positions are angles or
/// percentages of a full turn (#conic-color-stops); the center defaults to
/// the middle of the gradient box.
fn parse_conic_gradient(
    body: &str,
    repeating: bool,
    rect: CssRect,
    lengths: LengthBasis,
) -> Option<PaintBrush> {
    let mut parts = split_top_level(body, ',');
    if parts.len() < 2 {
        return None;
    }
    let (header, interpolation, explicit_interpolation) = gradient_interpolation(parts[0])?;
    let header = header.to_ascii_lowercase();
    let tokens = split_ws(&header);
    let (mut rotation, mut position) = (0.0, "center".to_string());
    if explicit_interpolation || matches!(tokens.first(), Some(&("from" | "at"))) {
        let mut rest = &tokens[..];
        if let ["from", turn, more @ ..] = rest {
            rotation = conic_turns(turn).filter(|_| !turn.ends_with('%'))? * 2.0 * PI;
            rest = more;
        }
        if let ["at", more @ ..] = rest {
            if more.is_empty() {
                return None;
            }
            position = more.join(" ");
            rest = &[];
        }
        if !rest.is_empty() {
            return None;
        }
        parts.remove(0);
    }
    let local = CssRect::new(0.0, 0.0, rect.width, rect.height);
    let (x, y) = background_position(&position, local, (0.0, 0.0), lengths);
    let center = CssPoint::new(rect.x + x, rect.y + y);
    let (stops, hints) = parse_stops_at(&parts, conic_turns)?;
    let interpolation =
        if !explicit_interpolation && stops.iter().all(|stop| legacy_srgb(&stop.color)) {
            GradientInterpolation {
                space: color::ColorSpaceTag::Srgb,
                ..interpolation
            }
        } else {
            interpolation
        };
    let mut stops = expand_color_hints(stops, &hints, interpolation);
    // Only 0deg to 360deg is painted. A repeating gradient repeats its first
    // to last stop (CSS Images 3 #repeating-gradients); otherwise stops
    // outside that turn still shape the colors inside it.
    let (first, last) = (stops.first()?.offset, stops.last()?.offset);
    let (from, to) = if repeating {
        if last - first <= f32::EPSILON || !(last - first).is_finite() {
            return Some(PaintBrush::Solid(average_stop_color(&stops)));
        }
        (first, last)
    } else {
        (first.min(0.0), last.max(1.0))
    };
    if !(to - from).is_finite() {
        return None;
    }
    for stop in &mut stops {
        stop.offset = (stop.offset - from) / (to - from);
    }
    Some(PaintBrush::ConicGradient {
        center,
        rotation,
        start_angle: from * 2.0 * PI,
        end_angle: to * 2.0 * PI,
        stops,
        interpolation,
        repeat: repeating,
    })
}

/// CSS Images 3 #radial-gradient-syntax: the center and horizontal and
/// vertical radii of `[<radial-shape> || <radial-size>]? [at <position>]?`
/// within `rect`, defaulting to an ellipse at the center reaching its
/// farthest corner (#radial-size).
fn radial_ending_shape(
    header: &str,
    rect: CssRect,
    lengths: LengthBasis,
) -> Option<(CssPoint, f32, f32)> {
    let lower = header.trim().to_ascii_lowercase();
    let tokens = split_ws(&lower);
    let at = tokens.iter().position(|token| *token == "at");
    let (shape_tokens, position) = match at {
        Some(index) => (&tokens[..index], Some(tokens[index + 1..].join(" "))),
        None => (&tokens[..], None),
    };
    if position.as_deref() == Some("") {
        return None;
    }
    let local = CssRect::new(0.0, 0.0, rect.width, rect.height);
    let (x, y) = background_position(
        position.as_deref().unwrap_or("center"),
        local,
        (0.0, 0.0),
        lengths,
    );
    let center = CssPoint::new(rect.x + x, rect.y + y);
    let mut shape = None;
    let mut extent = None;
    let mut sizes = Vec::new();
    for token in shape_tokens {
        match *token {
            "circle" | "ellipse" if shape.is_none() => shape = Some(*token),
            "closest-side" | "closest-corner" | "farthest-side" | "farthest-corner"
                if extent.is_none() && sizes.is_empty() =>
            {
                extent = Some(*token)
            }
            other if extent.is_none() && sizes.len() < 2 => sizes.push(other),
            _ => return None,
        }
    }
    let circle = match (shape, sizes.len()) {
        (Some("circle"), 0 | 1) | (None, 1) => true,
        (Some("ellipse") | None, 0 | 2) => false,
        _ => return None,
    };
    let (left, right) = (x, rect.width - x);
    let (top, bottom) = (y, rect.height - y);
    let closest = (left.abs().min(right.abs()), top.abs().min(bottom.abs()));
    let farthest = (left.abs().max(right.abs()), top.abs().max(bottom.abs()));
    let (radius_x, radius_y) = if let [size] = sizes.as_slice() {
        // A circle's single size is a length, never a percentage.
        if size.ends_with('%') {
            return None;
        }
        let radius = lengths.resolve(size, 0.0)?;
        (radius, radius)
    } else if let [width, height] = sizes.as_slice() {
        (
            lengths.resolve(width, rect.width)?,
            lengths.resolve(height, rect.height)?,
        )
    } else {
        let extent = extent.unwrap_or("farthest-corner");
        match (circle, extent) {
            (true, "closest-side") => (closest.0.min(closest.1), closest.0.min(closest.1)),
            (true, "farthest-side") => (farthest.0.max(farthest.1), farthest.0.max(farthest.1)),
            (true, "closest-corner") => (closest.0.hypot(closest.1), closest.0.hypot(closest.1)),
            (true, _) => (farthest.0.hypot(farthest.1), farthest.0.hypot(farthest.1)),
            (false, "closest-side") => closest,
            (false, "farthest-side") => farthest,
            (false, "closest-corner") => (closest.0 * SQRT_2, closest.1 * SQRT_2),
            (false, _) => (farthest.0 * SQRT_2, farthest.1 * SQRT_2),
        }
    };
    (radius_x >= 0.0 && radius_y >= 0.0).then_some((center, radius_x, radius_y))
}

/// The average color of a gradient's stops spaced evenly (CSS Images 3
/// #repeating-gradients' zero-length case).
fn average_stop_color(stops: &[GradientStop]) -> PaintColor {
    let colors: Vec<[f32; 4]> = stops
        .iter()
        .map(|stop| {
            let color = stop.color.to_alpha_color::<color::Srgb>().components;
            [
                color[0] * color[3],
                color[1] * color[3],
                color[2] * color[3],
                color[3],
            ]
        })
        .collect();
    let segments = colors.len().saturating_sub(1).max(1) as f32;
    let mut sum = [0.0f32; 4];
    for pair in colors.windows(2) {
        for channel in 0..4 {
            sum[channel] += (pair[0][channel] + pair[1][channel]) / 2.0;
        }
    }
    if colors.len() == 1 {
        sum = colors[0];
    }
    let alpha = sum[3] / segments;
    let unpremultiply = |channel: f32| {
        if alpha > 0.0 {
            (channel / segments / alpha * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8
        } else {
            0
        }
    };
    PaintColor::Rgba(
        unpremultiply(sum[0]),
        unpremultiply(sum[1]),
        unpremultiply(sum[2]),
        (alpha * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// Whether `color` was written as a hex, named, `rgb()`, `hsl()` or `hwb()`
/// color: the parser marks the sRGB ones as named (`color(srgb ...)` is not).
fn legacy_srgb(color: &color::DynamicColor) -> bool {
    use color::ColorSpaceTag as Space;
    match color.cs {
        Space::Srgb => color.flags.named(),
        Space::Hsl | Space::Hwb => true,
        _ => false,
    }
}

/// Color stops and their color hints: (index of the stop a hint precedes,
/// its position).
type StopsAndHints = (Vec<GradientStop>, Vec<(usize, f32)>);

/// CSS Images 3 #color-stop-syntax and #color-stop-fixup: parse color stops
/// (with zero, one or two `<length-percentage>` positions, lengths taken
/// along the `line`) and color hints, then default the first and last
/// positions, make positions non-decreasing and space unpositioned stops
/// evenly. Hints are returned as (index of the stop they precede, position).
fn parse_stops(parts: &[&str], line: f32, lengths: LengthBasis) -> Option<StopsAndHints> {
    parse_stops_at(parts, |token: &str| {
        let px = lengths.resolve(token, line)?;
        Some(if line > 0.0 { px / line } else { 0.0 })
    })
}

/// [`parse_stops`] with `position` resolving a stop or hint position to a
/// fraction of the gradient line.
fn parse_stops_at(parts: &[&str], position: impl Fn(&str) -> Option<f32>) -> Option<StopsAndHints> {
    let mut colors: Vec<(color::DynamicColor, Option<f32>)> = Vec::new();
    let mut hints: Vec<(usize, f32)> = Vec::new();
    for part in parts {
        let tokens = split_ws(part);
        match tokens.as_slice() {
            [hint] if color::parse_color(hint).is_err() => {
                // A hint sits between two color stops, never next to another.
                if colors.is_empty() || hints.last().is_some_and(|(at, _)| *at == colors.len()) {
                    return None;
                }
                hints.push((colors.len(), position(hint)?));
            }
            [stop, positions @ ..] if positions.len() <= 2 => {
                let stop = color::parse_color(stop).ok()?;
                if positions.is_empty() {
                    colors.push((stop, None));
                }
                for value in positions {
                    colors.push((stop, Some(position(value)?)));
                }
            }
            _ => return None,
        }
    }
    if colors.len() < 2 || hints.last().is_some_and(|(at, _)| *at == colors.len()) {
        return None;
    }
    let last = colors.len() - 1;
    colors[0].1.get_or_insert(0.0);
    colors[last].1.get_or_insert(1.0);
    let mut floor = f32::NEG_INFINITY;
    for (_, offset) in &mut colors {
        if let Some(offset) = offset {
            *offset = offset.max(floor);
            floor = *offset;
        }
    }
    let mut index = 0;
    while index < colors.len() {
        if colors[index].1.is_some() {
            index += 1;
            continue;
        }
        let start = index - 1;
        let end = (index..colors.len())
            .find(|&next| colors[next].1.is_some())
            .unwrap_or(last);
        let (from, to) = (colors[start].1.unwrap(), colors[end].1.unwrap());
        for (step, missing) in (index..end).enumerate() {
            colors[missing].1 = Some(from + (to - from) * (step + 1) as f32 / (end - start) as f32);
        }
        index = end;
    }
    let stops = colors
        .into_iter()
        .map(|(color, offset)| GradientStop {
            offset: offset.unwrap_or(0.0),
            color,
        })
        .collect::<Vec<_>>();
    let hints = hints
        .into_iter()
        .map(|(at, hint)| {
            let (from, to) = (stops[at - 1].offset, stops[at].offset);
            (at, hint.clamp(from, to))
        })
        .collect();
    Some((stops, hints))
}

/// CSS Images 3 #coloring-gradient-line: between two stops, a color hint at
/// fraction `H` of the way makes the mix at fraction `P` equal
/// `P^(log 0.5 / log H)`. Approximate that curve with intermediate stops
/// interpolated in the gradient's own color space. The renderer clamps
/// offsets to [0, 1] and pads outside them.
fn expand_color_hints(
    stops: Vec<GradientStop>,
    hints: &[(usize, f32)],
    interpolation: GradientInterpolation,
) -> Vec<GradientStop> {
    const STEPS: usize = 16;
    let mut out = Vec::with_capacity(stops.len() + hints.len() * STEPS);
    for (index, stop) in stops.iter().enumerate() {
        if let Some(&(_, hint)) = hints.iter().find(|(at, _)| *at == index) {
            let previous = &stops[index - 1];
            let span = stop.offset - previous.offset;
            let fraction = if span > 0.0 {
                (hint - previous.offset) / span
            } else {
                0.5
            };
            if (fraction - 0.5).abs() > f32::EPSILON {
                let mix =
                    previous
                        .color
                        .interpolate(stop.color, interpolation.space, interpolation.hue);
                for step in 1..STEPS {
                    let along = step as f32 / STEPS as f32;
                    let weight = if fraction <= 0.0 {
                        1.0
                    } else if fraction >= 1.0 {
                        0.0
                    } else {
                        along.powf(0.5f32.ln() / fraction.ln())
                    };
                    out.push(GradientStop {
                        offset: previous.offset + span * along,
                        color: mix.eval(weight),
                    });
                }
            }
        }
        out.push(*stop);
    }
    out
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

#[cfg(test)]
fn rotate(radians: f32) -> Affine2d {
    let (sin, cos) = radians.sin_cos();
    Affine2d([cos, sin, -sin, cos, 0.0, 0.0])
}

/// A conic gradient's `<angle-percentage>` as a fraction of a turn,
/// including math functions such as `calc(var(--p) * 1%)` (CSS Images 4
/// #conic-gradient-syntax). `from` takes only angles.
fn conic_turns(token: &str) -> Option<f32> {
    use crate::relative_color::{MathValue, math_value};
    if let Some(percent) = token.strip_suffix('%') {
        return percent
            .trim()
            .parse::<f32>()
            .ok()
            .map(|value| value / 100.0);
    }
    if let Some(radians) = angle(token) {
        return Some(radians / (2.0 * PI));
    }
    // Percentages are of a full turn, so a sum or comparison mixing them
    // with angles evaluates once they are written as degrees.
    let value = math_value(token).or_else(|| math_value(&percentages_as_degrees(token)))?;
    match value {
        MathValue::Percent(percent) => Some((percent / 100.0) as f32),
        MathValue::Degrees(degrees) => Some((degrees / 360.0) as f32),
        MathValue::Number(0.0) => Some(0.0),
        MathValue::Number(_) => None,
    }
}

/// `text` with each `<percentage>` token rewritten as the angle it is of a
/// full turn (`25%` becomes `90deg`).
fn percentages_as_degrees(text: &str) -> String {
    use cssparser::{Parser, ParserInput, ToCss, Token};
    fn rewrite<'i>(p: &mut Parser<'i, '_>, out: &mut String) {
        while let Ok(token) = p.next_including_whitespace_and_comments() {
            match token.clone() {
                Token::Percentage { unit_value, .. } => {
                    out.push_str(&format!("{}deg", unit_value * 360.0));
                }
                Token::Function(_) | Token::ParenthesisBlock => {
                    token.to_css(out).ok();
                    let _ = p.parse_nested_block(|p| {
                        rewrite(p, out);
                        Ok::<_, cssparser::ParseError<'_, ()>>(())
                    });
                    out.push(')');
                }
                other => {
                    other.to_css(out).ok();
                }
            }
        }
    }
    let mut input = ParserInput::new(text);
    let mut out = String::new();
    rewrite(&mut Parser::new(&mut input), &mut out);
    out
}

fn angle(value: &str) -> Option<f32> {
    let value = value.trim();
    if let Some(v) = value.strip_suffix("deg") {
        v.trim().parse::<f32>().ok().map(f32::to_radians)
    } else if let Some(v) = value.strip_suffix("grad") {
        v.trim().parse::<f32>().ok().map(|v| v / 400.0 * 2.0 * PI)
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
        return text_color_for_style(dom, PaintStyle::Element(node));
    }
    if link {
        PaintColor::Rgba(0, 70, 190, 255)
    } else {
        crate::render::CANVAS_TEXT
    }
}

fn text_color_for_style(dom: &Dom, style: PaintStyle) -> PaintColor {
    if let Some(color) = style
        .value(dom, "color")
        .as_deref()
        .and_then(|value| resolve_color_for_style(dom, style, value))
    {
        return color;
    }
    // HTML #phrasing-content-3 supplies hyperlink color in the UA cascade.
    // An activation target (summary, button, onclick host) is not a hyperlink
    // and must not acquire link styling from its frontend action descriptor.
    crate::render::CANVAS_TEXT
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
    let node = decoration_origin(dom, node)?;
    let value = dom.computed_value_resolved(node, "text-decoration-color");
    let value = value.as_deref().unwrap_or("currentcolor");
    if value.eq_ignore_ascii_case("currentcolor") {
        Some(text_color_for_style(dom, PaintStyle::Element(node)))
    } else {
        resolve_color(dom, node, value)
    }
}

fn decoration_style(dom: &Dom, node: NodeId) -> DecorationStyle {
    let Some(node) = decoration_origin(dom, node) else {
        return DecorationStyle::Solid;
    };
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

/// CSS Text Decoration 4 #text-decoration-width-property and
/// #underline-offset, read from the decorating box: a length, or a
/// percentage of its 1em. `auto` and `from-font` keep the automatic geometry.
fn decoration_metric(dom: &Dom, node: NodeId, property: &str, viewport: Vp) -> Option<f32> {
    let node = decoration_origin(dom, node)?;
    let value = dom.computed_value_resolved(node, property)?;
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") || value.eq_ignore_ascii_case("from-font") {
        return None;
    }
    Len::parse(value, Units::of(dom, node), viewport)?
        .resolve(Some(dom.font_px(node)))
        .filter(|value| value.is_finite())
}

fn decoration_origin(dom: &Dom, mut node: NodeId) -> Option<NodeId> {
    // CSS Text Decoration 3 #text-decoration-color-property: a propagated
    // decoration keeps its originating element's color, including currentcolor.
    // Descendant color/style declarations alone do not establish a new line.
    // Most text is undecorated; its memoized line flags avoid an ancestry
    // walk on every glyph run during painting.
    if node == NO_NODE || dom.text_decoration(node) == (false, false) {
        return None;
    }
    while node != NO_NODE {
        if dom
            .computed_value_resolved(node, "text-decoration-line")
            .is_some_and(|line| {
                line.split_whitespace()
                    .any(|part| matches!(part, "underline" | "line-through" | "overline"))
            })
        {
            return Some(node);
        }
        node = dom.parent_composed(node)?;
    }
    None
}

/// How a border-image region repeats along one axis
/// (CSS Backgrounds 3 #border-image-repeat).
#[derive(Clone, Copy, PartialEq)]
enum BorderImageRepeat {
    Stretch,
    Repeat,
    Round,
    Space,
}

/// The tile origins and used tile size along one axis of a border-image
/// region of `length` starting at `start`, for a scaled tile `tile`.
fn border_image_tiles(
    start: f32,
    length: f32,
    tile: f32,
    mode: BorderImageRepeat,
) -> (Vec<f32>, f32) {
    if length <= 0.0 {
        return (Vec::new(), 0.0);
    }
    if mode == BorderImageRepeat::Stretch || !tile.is_finite() || tile <= 0.0 {
        return (vec![start], length);
    }
    match mode {
        BorderImageRepeat::Round => {
            let count = (length / tile).round().max(1.0);
            let tile = length / count;
            (
                (0..count as usize)
                    .map(|i| start + i as f32 * tile)
                    .collect(),
                tile,
            )
        }
        BorderImageRepeat::Space => {
            let count = (length / tile).floor();
            if count < 1.0 {
                return (Vec::new(), tile);
            }
            let gap = (length - count * tile) / (count + 1.0);
            (
                (0..count as usize)
                    .map(|i| start + gap + i as f32 * (tile + gap))
                    .collect(),
                tile,
            )
        }
        _ => {
            // `repeat`: centered in the region, partial tiles at both ends.
            let first = start + (length - tile) / 2.0;
            let first = first - ((first - start) / tile).ceil() * tile;
            let mut origins = Vec::new();
            let mut position = first;
            while position < start + length && origins.len() < 4096 {
                origins.push(position);
                position += tile;
            }
            (origins, tile)
        }
    }
}

/// CSS Backgrounds 3 #border-images: slice the image into nine regions and
/// draw them over the border image area (the border box grown by
/// border-image-outset), corners scaled, edges and the optional `fill`
/// middle stretched or tiled per border-image-repeat. Returns whether the
/// image replaced the border styles; an image that is not available yet
/// leaves them in place.
fn paint_border_image(fragment: &Frag, style: PaintStyle, builder: &mut Builder<'_>) -> bool {
    let Some(source) = style
        .value(builder.dom, "border-image-source")
        .as_deref()
        .and_then(css_url)
    else {
        return false;
    };
    let base = builder.dom.style_resource_base(style.node(), builder.base);
    let source = resolve_image_source(&base, &source);
    let handle = builder.image(source.clone());
    let Some((iw, ih)) = builder
        .images
        .get(&source)
        .copied()
        .filter(|(w, h)| *w > 0 && *h > 0 && *w != u32::MAX)
        .map(|(w, h)| (w as f32, h as f32))
    else {
        return false;
    };
    let lengths = LengthBasis::of(builder.dom, style.node(), builder.viewport());
    let value = |name: &str, initial: &str| {
        style
            .value(builder.dom, name)
            .unwrap_or_else(|| initial.into())
    };
    // Box-shorthand expansion of 1-4 values into top, right, bottom, left.
    fn sides<T: Copy>(values: &[T]) -> Option<[T; 4]> {
        Some(match values {
            [a] => [*a, *a, *a, *a],
            [a, b] => [*a, *b, *a, *b],
            [a, b, c] => [*a, *b, *c, *b],
            [a, b, c, d] => [*a, *b, *c, *d],
            _ => return None,
        })
    }
    let slice_value = value("border-image-slice", "100%");
    let fill = slice_value
        .split_whitespace()
        .any(|token| token.eq_ignore_ascii_case("fill"));
    let slice_tokens: Vec<&str> = slice_value
        .split_whitespace()
        .filter(|token| !token.eq_ignore_ascii_case("fill"))
        .collect();
    let Some(slices) = sides(&slice_tokens) else {
        return false;
    };
    let image_size = [ih, iw, ih, iw];
    let slices: [f32; 4] = std::array::from_fn(|side| {
        let token = slices[side];
        let px = match token.strip_suffix('%') {
            Some(percent) => percent
                .parse::<f32>()
                .ok()
                .map(|p| p / 100.0 * image_size[side]),
            None => token.parse::<f32>().ok(),
        };
        px.unwrap_or(0.0).clamp(0.0, image_size[side])
    });
    let outset_value = value("border-image-outset", "0");
    let outset_tokens: Vec<&str> = outset_value.split_whitespace().collect();
    let outsets: [f32; 4] = match sides(&outset_tokens) {
        Some(tokens) => std::array::from_fn(|side| {
            let token = tokens[side];
            match token.parse::<f32>() {
                Ok(number) => number * fragment.border[side],
                Err(_) => lengths.resolve(token, 0.0).unwrap_or(0.0),
            }
            .max(0.0)
        }),
        None => [0.0; 4],
    };
    let area = CssRect::new(
        fragment.x - outsets[3],
        fragment.y - outsets[0],
        fragment.w + outsets[1] + outsets[3],
        fragment.h + outsets[0] + outsets[2],
    );
    let width_value = value("border-image-width", "1");
    let width_tokens: Vec<&str> = width_value.split_whitespace().collect();
    let Some(width_tokens) = sides(&width_tokens) else {
        return false;
    };
    let mut widths: [f32; 4] = std::array::from_fn(|side| {
        let token = width_tokens[side];
        let basis = if side % 2 == 0 {
            area.height
        } else {
            area.width
        };
        if token.eq_ignore_ascii_case("auto") {
            slices[side]
        } else if let Ok(number) = token.parse::<f32>() {
            number * fragment.border[side]
        } else {
            lengths.resolve(token, basis).unwrap_or(0.0)
        }
        .max(0.0)
    });
    // #border-image-width: overlapping opposite widths scale down together.
    let factor = [
        area.height / (widths[0] + widths[2]),
        area.width / (widths[1] + widths[3]),
    ]
    .into_iter()
    .filter(|f| f.is_finite())
    .fold(1.0f32, f32::min);
    if factor < 1.0 {
        for width in &mut widths {
            *width *= factor;
        }
    }
    let repeat_value = value("border-image-repeat", "stretch");
    let modes: Vec<BorderImageRepeat> = repeat_value
        .split_whitespace()
        .map(|token| match token.to_ascii_lowercase().as_str() {
            "repeat" => BorderImageRepeat::Repeat,
            "round" => BorderImageRepeat::Round,
            "space" => BorderImageRepeat::Space,
            _ => BorderImageRepeat::Stretch,
        })
        .collect();
    let horizontal = modes.first().copied().unwrap_or(BorderImageRepeat::Stretch);
    let vertical = modes.get(1).copied().unwrap_or(horizontal);
    let sampling = if matches!(
        builder
            .dom
            .computed_value_resolved(style.node(), "image-rendering")
            .as_deref(),
        Some("pixelated" | "crisp-edges" | "-moz-crisp-edges")
    ) {
        ImageSampling::Nearest
    } else {
        ImageSampling::Smooth
    };
    let [wt, wr, wb, wl] = widths;
    let [st, sr, sb, sl] = slices;
    let (x0, x1, x2) = (area.x, area.x + wl, area.x + area.width - wr);
    let (y0, y1, y2) = (area.y, area.y + wt, area.y + area.height - wb);
    let (u1, u2) = (sl, iw - sr);
    let (v1, v2) = (st, ih - sb);
    let node = style.node();
    let ratio = builder.dom.device_pixel_ratio().max(f32::EPSILON);
    let draw = |builder: &mut Builder<'_>,
                source: CssRect,
                region: CssRect,
                tile: (f32, f32),
                modes: (BorderImageRepeat, BorderImageRepeat)| {
        if source.width <= 0.0
            || source.height <= 0.0
            || region.width <= 0.0
            || region.height <= 0.0
        {
            return;
        }
        let (xs, tile_w) = border_image_tiles(region.x, region.width, tile.0, modes.0);
        let (ys, tile_h) = border_image_tiles(region.y, region.height, tile.1, modes.1);
        let clipped = modes.0 == BorderImageRepeat::Repeat || modes.1 == BorderImageRepeat::Repeat;
        if clipped {
            builder
                .commands
                .push(DisplayCommand::PushClip(PaintShape::Rect(region)));
        }
        // Adjoining `round` and `repeat` tiles meet on device pixels, as in
        // Gecko and Blink: two antialiased edges sharing a fractional pixel
        // would let the background show through as a seam, though the tiles
        // exactly fill the region. The region's own edges stay put.
        let edges = |origin: f32, size: f32, mode: BorderImageRepeat, from: f32, to: f32| {
            if !matches!(mode, BorderImageRepeat::Round | BorderImageRepeat::Repeat) {
                return (origin, size);
            }
            let snap = |value: f32| {
                if (value - from).abs() < 1e-3 || (value - to).abs() < 1e-3 {
                    value
                } else {
                    (value * ratio).round() / ratio
                }
            };
            let start = snap(origin);
            (start, snap(origin + size) - start)
        };
        let (right, bottom) = (region.x + region.width, region.y + region.height);
        for &y in &ys {
            let (top, height) = edges(y, tile_h, modes.1, region.y, bottom);
            for &x in &xs {
                let (left, width) = edges(x, tile_w, modes.0, region.x, right);
                builder.commands.push(DisplayCommand::Image {
                    rect: CssRect::new(left, top, width, height),
                    handle,
                    source_rect: Some(source),
                    fit: ImageFit::Fill,
                    sampling,
                    clip: None,
                    node,
                    link: None,
                });
            }
        }
        if clipped {
            builder.commands.push(DisplayCommand::PopClip);
        }
    };
    let stretch = (BorderImageRepeat::Stretch, BorderImageRepeat::Stretch);
    // Corners.
    draw(
        builder,
        CssRect::new(0.0, 0.0, sl, st),
        CssRect::new(x0, y0, wl, wt),
        (wl, wt),
        stretch,
    );
    draw(
        builder,
        CssRect::new(u2, 0.0, sr, st),
        CssRect::new(x2, y0, wr, wt),
        (wr, wt),
        stretch,
    );
    draw(
        builder,
        CssRect::new(u2, v2, sr, sb),
        CssRect::new(x2, y2, wr, wb),
        (wr, wb),
        stretch,
    );
    draw(
        builder,
        CssRect::new(0.0, v2, sl, sb),
        CssRect::new(x0, y2, wl, wb),
        (wl, wb),
        stretch,
    );
    // Edges: scaled to the edge thickness, then tiled along the edge.
    let (mid_w, mid_h) = (u2 - u1, v2 - v1);
    let scaled = |thickness: f32, slice: f32, length: f32| {
        if slice > 0.0 {
            length * thickness / slice
        } else {
            0.0
        }
    };
    draw(
        builder,
        CssRect::new(u1, 0.0, mid_w, st),
        CssRect::new(x1, y0, x2 - x1, wt),
        (scaled(wt, st, mid_w), wt),
        (horizontal, BorderImageRepeat::Stretch),
    );
    draw(
        builder,
        CssRect::new(u1, v2, mid_w, sb),
        CssRect::new(x1, y2, x2 - x1, wb),
        (scaled(wb, sb, mid_w), wb),
        (horizontal, BorderImageRepeat::Stretch),
    );
    draw(
        builder,
        CssRect::new(0.0, v1, sl, mid_h),
        CssRect::new(x0, y1, wl, y2 - y1),
        (wl, scaled(wl, sl, mid_h)),
        (BorderImageRepeat::Stretch, vertical),
    );
    draw(
        builder,
        CssRect::new(u2, v1, sr, mid_h),
        CssRect::new(x2, y1, wr, y2 - y1),
        (wr, scaled(wr, sr, mid_h)),
        (BorderImageRepeat::Stretch, vertical),
    );
    if fill {
        // The middle scales like the top (else bottom) and left (else right)
        // edges.
        let across = if st > 0.0 {
            wt / st
        } else if sb > 0.0 {
            wb / sb
        } else {
            1.0
        };
        let down = if sl > 0.0 {
            wl / sl
        } else if sr > 0.0 {
            wr / sr
        } else {
            1.0
        };
        draw(
            builder,
            CssRect::new(u1, v1, mid_w, mid_h),
            CssRect::new(x1, y1, x2 - x1, y2 - y1),
            (mid_w * across, mid_h * down),
            (horizontal, vertical),
        );
    }
    true
}

/// CSS Compositing 1 #isolation-blending: an element's background layers
/// blend only with each other and its background color, never with the
/// content behind the element, so blended backgrounds paint as an isolated
/// group. Returns whether the group was opened.
fn begin_background_isolation(builder: &mut Builder<'_>, style: PaintStyle) -> bool {
    let blended = background_blends(builder.dom, style);
    if blended {
        builder.commands.push(isolated_group());
    }
    blended
}

/// Whether any of the style's background layers uses a non-normal
/// `background-blend-mode`.
fn background_blends(dom: &Dom, style: PaintStyle) -> bool {
    style
        .value(dom, "background-blend-mode")
        .is_some_and(|modes| {
            split_top_level(&modes, ',')
                .iter()
                .any(|mode| blend_mode(mode) != BlendMode::Normal)
        })
}

/// Open a CSS Compositing 1 #isolatedgroups group: normal blending, full
/// opacity, and a transparent black initial backdrop.
fn isolated_group() -> DisplayCommand {
    DisplayCommand::PushLayer(CompositingLayer {
        opacity: 1.0,
        blend: BlendMode::Normal,
        filters: std::sync::Arc::from([]),
    })
}

fn is_opaque(color: PaintColor) -> bool {
    !matches!(color, PaintColor::Rgba(_, _, _, alpha) if alpha < 255)
}

/// Whether a background layer that draws an image is `fixed`.
fn has_fixed_background_layer(dom: &Dom, style: PaintStyle) -> bool {
    let Some(images) = style.value(dom, "background-image") else {
        return false;
    };
    let attachments = style
        .value(dom, "background-attachment")
        .unwrap_or_else(|| "scroll".into());
    let attachments = split_top_level(&attachments, ',');
    split_top_level(&images, ',')
        .iter()
        .enumerate()
        .any(|(index, layer)| {
            let layer = layer.trim();
            !layer.is_empty()
                && !layer.eq_ignore_ascii_case("none")
                && layer_value(&attachments, index, "scroll")
                    .trim()
                    .eq_ignore_ascii_case("fixed")
        })
}

/// Paint a blended root canvas background, which is not over an opaque
/// background color, as an isolated group (CSS Compositing 1
/// #background-blend-mode). The caller paints only the canvas surface below.
///
/// Fixed canvas layers live in the viewport-pinned underlay, which paints
/// below the scrolling document, so the group cannot span both. The color
/// and fixed layers then form the group in the underlay, and scrolling layers
/// keep blending with that group's result. This is exact wherever the
/// underlay is opaque; a second isolated group would instead never let the
/// scrolling layers blend with the fixed ones.
fn paint_isolated_canvas_background(
    root: &Frag,
    style: PaintStyle,
    canvas: CssRect,
    color: Option<PaintColor>,
    builder: &mut Builder<'_>,
) {
    let fixed = has_fixed_background_layer(builder.dom, style);
    let fixed_start = builder.fixed_under.len();
    if !fixed {
        builder.commands.push(isolated_group());
        if let Some(color) = color {
            builder.commands.push(DisplayCommand::Fill {
                shape: PaintShape::Rect(canvas),
                brush: PaintBrush::Solid(color),
            });
        }
    }
    paint_background_images_for_style(
        root,
        style,
        PaintShape::Rect(canvas),
        builder,
        Some(canvas),
        None,
    );
    if !fixed {
        builder.commands.push(DisplayCommand::PopLayer);
        return;
    }
    let mut group = vec![isolated_group()];
    if let Some(color) = color {
        group.push(DisplayCommand::Fill {
            shape: PaintShape::Rect(CssRect::new(
                0.0,
                0.0,
                builder.viewport_w,
                builder.viewport_h,
            )),
            brush: PaintBrush::Solid(color),
        });
    }
    group.extend(builder.fixed_under.drain(fixed_start..));
    group.push(DisplayCommand::PopLayer);
    builder.fixed_under.extend(group);
}

/// CSS Compositing 1 #ltblendmodegt.
fn blend_mode(value: &str) -> BlendMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "multiply" => BlendMode::Multiply,
        "screen" => BlendMode::Screen,
        "overlay" => BlendMode::Overlay,
        "darken" => BlendMode::Darken,
        "lighten" => BlendMode::Lighten,
        "color-dodge" => BlendMode::ColorDodge,
        "color-burn" => BlendMode::ColorBurn,
        "hard-light" => BlendMode::HardLight,
        "soft-light" => BlendMode::SoftLight,
        "difference" => BlendMode::Difference,
        "exclusion" => BlendMode::Exclusion,
        "hue" => BlendMode::Hue,
        "saturation" => BlendMode::Saturation,
        "color" => BlendMode::Color,
        "luminosity" => BlendMode::Luminosity,
        _ => BlendMode::Normal,
    }
}

/// Return a finite rectangle covering all paintable fragment borders. The
/// fragment clip is applied while finding the extent, so intentionally huge
/// overflow-hidden probes do not turn an unbounded display-list clip into a
/// huge raster path. The viewport compositor still supplies the final screen
/// clip; this extent only replaces CSS's conceptual unbounded axis.
fn paint_extent(root: &Frag, fixed: &[Frag], top_layer: &[TopFrag], flow_bottom: f32) -> CssRect {
    let mut bounds = (
        0.0_f32,
        0.0_f32,
        1.0_f32,
        flow_bottom.max(root.max_bottom()).max(1.0),
    );

    fn visit(fragment: &Frag, bounds: &mut (f32, f32, f32, f32)) {
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

fn padding_box(fragment: &Frag) -> CssRect {
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

/// What a paint-time CSS length resolves against: the element's font units
/// and the viewport (CSS Values 4 #relative-lengths). Computed values keep
/// their authored units, so `em`, `rem`, `vw` or `calc()` must not be read
/// as if every length were `px`.
#[derive(Clone, Copy)]
struct LengthBasis {
    units: Units,
    viewport: Vp,
}

impl LengthBasis {
    fn of(dom: &Dom, node: NodeId, viewport: Vp) -> Self {
        Self {
            units: if node == NO_NODE {
                Units::default()
            } else {
                Units::of(dom, node)
            },
            viewport,
        }
    }

    #[cfg(test)]
    fn fixed() -> Self {
        Self {
            units: Units::default(),
            viewport: Vp { w: 800.0, h: 600.0 },
        }
    }

    /// A `<length-percentage>`, its percentages taken of `basis`.
    fn resolve(self, value: &str, basis: f32) -> Option<f32> {
        Len::parse(value.trim(), self.units, self.viewport)?.resolve(Some(basis))
    }
}

impl PaintColor {
    pub fn parse_css(value: &str) -> Option<Self> {
        let value = value.trim();
        if let Some(resolved) = crate::relative_color::resolve(value) {
            return Self::parse_css(&resolved?);
        }
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

/// CSS Color 4 #color-function (every predefined space), #the-hwb-notation,
/// #specifying-lab-lch and #specifying-oklab-oklch, using the local 2026-09-06
/// CSSWG snapshot (81c27f686901). Display P3 uses D65; Lab uses D50, so the
/// conversion includes Bradford white-point adaptation. Keep source components
/// unclipped until actual-value conversion to our sRGB paint surface.
fn parse_perceptual_or_p3(value: &str) -> Option<PaintColor> {
    use color::ColorSpaceTag::{Hwb, Lab, Lch, Oklab, Oklch};
    let origin = color::parse_color(value).ok()?;
    let function = value.get(..value.find('(')?)?.trim();
    let supported = if function.eq_ignore_ascii_case("color") {
        true
    } else if function.eq_ignore_ascii_case("hwb") {
        origin.cs == Hwb
    } else {
        matches!(origin.cs, Lab | Lch | Oklab | Oklch)
    };
    if !supported
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

    #[test]
    fn floated_navigation_background_stays_behind_its_contents() {
        let (_, layout) = render_fixture(
            r#"<!doctype html><style>
            body{margin:0} nav{position:relative;min-height:76px;transform:translate3d(0,0,0);background:#123456}
            .container:before,.container:after{display:table;content:' '}
            .container:after{clear:both}.left{float:left;height:76px;width:300px}
            .right{float:right;height:76px;width:100px}
            </style><nav><div class=container><div class=left>Menu</div><div class=right>Play</div></div></nav>"#,
        );
        let rect = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                DisplayCommand::Fill {
                    shape: PaintShape::Rect(rect),
                    brush: PaintBrush::Solid(PaintColor::Rgba(18, 52, 86, 255)),
                } => Some(rect),
                _ => None,
            })
            .expect("navigation background");
        assert_eq!((rect.y, rect.height), (0., 76.));
    }

    #[test]
    fn flex_before_background_paints_below_positioned_items() {
        for display in ["flex", "grid"] {
            for (pseudo, order, expected) in [
                ("before", 0, [0, 255, 0]),
                ("after", 0, [0, 0, 0]),
                ("before", -1, [0, 0, 0]),
                ("after", 1, [0, 255, 0]),
            ] {
                let (_, layout) = render_fixture(&format!(
                    r#"<!doctype html><style>
                body{{margin:0;background:white}}.bar{{position:fixed;bottom:0;left:0;width:100%;z-index:20}}
                .items{{display:{display}}}.items::{pseudo}{{content:'';position:absolute;bottom:0;left:0;right:0;height:29px;background:black}}
                .item{{position:relative;width:100px;height:58px;background:lime;transform:translateY(0);order:{order}}}
                </style><div class=bar><div class=items><div class=item></div></div></div>"#
                ));
                let pixels =
                    crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                        .unwrap()
                        .pixels;
                assert_eq!(
                    &pixels[(590 * 800 + 50) * 4..(590 * 800 + 50) * 4 + 3],
                    expected,
                    "{display} ::{pseudo} order:{order}"
                );
            }
        }
    }

    /// The pixels of a fixture rendered at 800x600, as RGB triples.
    fn render_pixels(html: &str) -> impl Fn(usize, usize) -> [u8; 3] {
        let (_, layout) = render_fixture(html);
        let pixels = crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
            .unwrap()
            .pixels;
        move |x, y| {
            let at = (y * 800 + x) * 4;
            [pixels[at], pixels[at + 1], pixels[at + 2]]
        }
    }

    /// The runs of pixels matching `ink` along a row or column, as
    /// half-open ranges.
    fn ink_runs(points: impl Iterator<Item = (usize, bool)>) -> Vec<(usize, usize)> {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut open = None;
        for (position, inked) in points {
            match (inked, open) {
                (true, None) => open = Some(position),
                (false, Some(start)) => {
                    runs.push((start, position));
                    open = None;
                }
                _ => {}
            }
        }
        if let Some(start) = open {
            runs.push((start, usize::MAX));
        }
        runs
    }

    #[test]
    fn dotted_borders_paint_a_dot_every_two_widths() {
        // CSS Backgrounds 3 #border-style: dotted is "a series of round
        // dots". Each dot used to be a zero-length dash with round caps,
        // which the stroker drops, so only a few stray dots survived.
        let pixel = render_pixels(
            r#"<!doctype html><body style="margin:0;background:white">
            <div style="margin:10px;width:300px;height:20px;border-bottom:4px dotted #00f"></div>
            <div style="margin:10px;width:300px;height:20px;border-bottom:2px dotted #00f"></div>
            <div style="margin:10px;width:100px;height:40px;border:1px dotted #00f"></div>"#,
        );
        let blue = |x: usize, y: usize| {
            let [r, _, b] = pixel(x, y);
            b > 200 && r < 160
        };
        // 4px round dots from x = 12 to 308, 8px apart.
        let dots = ink_runs((0..330).map(|x| (x, blue(x, 32))));
        assert_eq!(dots.len(), 38, "{dots:?}");
        for (index, (start, end)) in dots.iter().enumerate() {
            let center = (start + end) as f32 / 2.;
            assert!((center - (12. + 8. * index as f32)).abs() <= 1., "{dots:?}");
        }
        // 2px dots are whole-pixel squares with whole-pixel gaps.
        let dots = ink_runs((0..330).map(|x| (x, blue(x, 65))));
        assert!((74..=76).contains(&dots.len()), "{dots:?}");
        assert_eq!((dots[0].0, dots[dots.len() - 1].1), (10, 310));
        for pair in dots.windows(2) {
            assert_eq!(pair[0].1 - pair[0].0, 2, "{dots:?}");
            assert!((2..=3).contains(&(pair[1].0 - pair[0].1)), "{dots:?}");
        }
        // A 1px box alternates pixels along every side, a dot in each corner.
        let top = ink_runs((0..130).map(|x| (x, blue(x, 76))));
        let left = ink_runs((70..130).map(|y| (y, blue(10, y))));
        assert_eq!((top[0].0, top[top.len() - 1].1), (10, 112), "{top:?}");
        assert_eq!((left[0].0, left[left.len() - 1].1), (76, 118), "{left:?}");
        for runs in [&top, &left] {
            for pair in runs.windows(2) {
                assert_eq!(pair[0].1 - pair[0].0, 1, "{runs:?}");
                assert!((1..=2).contains(&(pair[1].0 - pair[0].1)), "{runs:?}");
            }
        }
    }

    #[test]
    fn dotted_outlines_and_mixed_corners_keep_their_dots() {
        // CSS UI 4 #outline-style: outline styles mean what border styles
        // do. Where a dotted side meets another style, the other side takes
        // the whole corner, as in Gecko, instead of leaving half of it bare.
        let pixel = render_pixels(
            r#"<!doctype html><body style="margin:0;background:white">
            <div style="position:absolute;left:20px;top:20px;width:200px;height:40px;
                outline:3px dotted #00f"></div>
            <div style="position:absolute;left:20px;top:100px;width:200px;height:40px;
                border:6px solid #f00;border-left:6px dotted #00f"></div>"#,
        );
        let blue = |x: usize, y: usize| {
            let [r, _, b] = pixel(x, y);
            b > 200 && r < 160
        };
        // The outline's top edge spans x = 17..223 at y = 17..20.
        let dots = ink_runs((0..240).map(|x| (x, blue(x, 18))));
        assert!((33..=35).contains(&dots.len()), "{dots:?}");
        for pair in dots.windows(2) {
            assert!(pair[1].0 - pair[0].0 <= 7, "{dots:?}");
        }
        // The top-left corner square (20..26, 100..106) is all red.
        for (x, y) in [(21, 101), (24, 104), (21, 105), (25, 101)] {
            assert_eq!(pixel(x, y), [255, 0, 0], "({x}, {y})");
        }
        // The dotted left side starts one gap below the corner square.
        let dots = ink_runs((100..160).map(|y| (y, blue(23, y))));
        assert!(dots.len() >= 3, "{dots:?}");
        assert!((112..=114).contains(&dots[0].0), "{dots:?}");
    }

    #[test]
    fn unequal_sides_follow_their_rounded_corners() {
        // CSS Backgrounds 3 #corner-shaping: every style follows the curve,
        // and #corner-transitions: a zero-width side leaves the whole corner
        // to the other. The sides used to be clipped one border width deep
        // into each corner, dropping the curve when the sides differed.
        let pixel = render_pixels(
            r#"<!doctype html><body style="margin:0;background:white">
            <div style="position:absolute;left:20px;top:20px;width:300px;height:60px;
                border-radius:0 50px 0 0;border:4px solid #000;border-bottom:none"></div>
            <div style="position:absolute;left:20px;top:120px;width:140px;height:80px;
                border-radius:30px;border:8px solid #000;border-bottom:none"></div>
            <div style="position:absolute;left:420px;top:20px;width:140px;height:80px;
                border-radius:40px;border:8px dashed #00f"></div>"#,
        );
        // Around the 50px corner, in the middle of the 4px ring.
        for (x, y) in [(314, 38), (308, 34)] {
            assert!(pixel(x, y)[0] < 60, "({x}, {y}) {:?}", pixel(x, y));
        }
        // Where the left side thins into the missing bottom side.
        assert!(pixel(41, 206)[0] < 100, "{:?}", pixel(41, 206));
        // A dashed side keeps to its curve: nothing in the cut-off corner
        // (420..432, 20..32), and dashes along the arc.
        for (x, y) in [(421, 21), (425, 25), (424, 22)] {
            assert_eq!(pixel(x, y), [255, 255, 255], "({x}, {y})");
        }
        let arc = (0..=90)
            .filter(|degrees| {
                let angle = (*degrees as f32).to_radians();
                let [r, _, b] = pixel(
                    (460. - 36. * angle.cos()) as usize,
                    (60. - 36. * angle.sin()) as usize,
                );
                b > 200 && r < 100
            })
            .count();
        assert!(arc > 30, "{arc}");
    }

    #[test]
    fn square_corners_stay_square_and_round_ones_keep_their_radii() {
        // CSS Backgrounds 3 #corner-shaping: a zero radius is a square
        // corner, and the padding edge's radius is the outer radius less the
        // border width. Uniform borders and outlines were stroked with round
        // joins (notching square corners), outlines grew a zero radius, and
        // a rounded border's stroke used the outer radius on its center line.
        let pixel = render_pixels(
            r#"<!doctype html><body style="margin:0;background:white">
            <div style="position:absolute;left:40px;top:40px;width:100px;height:60px;
                background:red;border:16px solid #00f"></div>
            <div style="position:absolute;left:240px;top:40px;width:100px;height:60px;
                background:red;outline:16px solid #00f"></div>
            <div style="position:absolute;left:20px;top:200px;width:100px;height:60px;
                background:red;border:10px solid #00f;border-radius:30px"></div>
            <div style="position:absolute;left:240px;top:200px;width:100px;height:60px;
                background:red;border-radius:20px;outline:6px solid #00f;outline-offset:4px"></div>"#,
        );
        for (x, y) in [(40, 40), (41, 41), (171, 40), (171, 131), (40, 131)] {
            assert_eq!(pixel(x, y), [0, 0, 255], "border ({x}, {y})");
        }
        for (x, y) in [(224, 24), (226, 26), (355, 24), (355, 115), (224, 115)] {
            assert_eq!(pixel(x, y), [0, 0, 255], "outline ({x}, {y})");
        }
        // The 30px corner's outer curve and its 20px padding curve, both
        // centered on (50, 230).
        assert_eq!(pixel(29, 209), [0, 0, 255]);
        assert_eq!(pixel(37, 217), [255, 0, 0]);
        // The outline's outer radius is 20 + 10 and its inner one 24, both
        // centered on (260, 220).
        assert_eq!(pixel(233, 193), [255, 255, 255]);
        assert_eq!(pixel(240, 200), [0, 0, 255]);
        assert_eq!(pixel(244, 204), [255, 255, 255]);
        assert_eq!(pixel(232, 220), [0, 0, 255]);
        assert_eq!(pixel(260, 191), [0, 0, 255]);
    }

    #[test]
    fn uniform_dashed_borders_keep_radii_under_half_their_width() {
        // CSS Backgrounds 3 #corner-shaping: every style follows the curve
        // of the border. With a radius of at most half the width the dash
        // stroke's center line has a square corner, whose mitered join
        // painted the rounded-off outer corner.
        let pixel = render_pixels(
            r#"<!doctype html><body style="margin:0;background:white">
            <div style="position:absolute;left:40px;top:40px;width:80px;height:50px;
                border:12px dashed #00f;border-radius:4px"></div>
            <div style="position:absolute;left:240px;top:40px;width:80px;height:50px;
                border:12px dashed #00f;border-radius:6px"></div>"#,
        );
        for left in [40, 240] {
            let (right, bottom) = (left + 103, 113);
            for (x, y) in [(left, 40), (right, 40), (right, bottom), (left, bottom)] {
                assert_eq!(pixel(x, y), [255, 255, 255], "({x}, {y})");
            }
            // The dash over the top-left corner still fills the ring up to
            // its curve.
            for (x, y) in [(left + 3, 41), (left + 1, 44), (left + 11, 51)] {
                assert_eq!(pixel(x, y), [0, 0, 255], "({x}, {y})");
            }
        }
    }

    #[test]
    fn outlines_draw_double_and_3d_styles_like_borders() {
        // CSS UI 4 #outline-style: <outline-line-style> takes the border
        // styles "with the same meaning". These used to be one solid stroke.
        let pixel = render_pixels(
            r#"<!doctype html><body style="margin:0;background:white">
            <div style="position:absolute;left:40px;top:40px;width:100px;height:60px;
                outline:8px ridge #50579c"></div>
            <div style="position:absolute;left:200px;top:40px;width:100px;height:60px;
                outline:8px groove #50579c"></div>
            <div style="position:absolute;left:360px;top:40px;width:100px;height:60px;
                outline:9px double #50579c"></div>
            <div style="position:absolute;left:520px;top:40px;width:100px;height:60px;
                outline:8px inset #50579c"></div>"#,
        );
        let (light, dark) = ([80, 87, 156], [53, 58, 104]);
        // Ridge: light outside and dark inside on the top and left, the
        // reverse on the bottom and right; groove the opposite.
        assert_eq!((pixel(33, 70), pixel(38, 70)), (light, dark));
        assert_eq!((pixel(70, 33), pixel(70, 38)), (light, dark));
        assert_eq!((pixel(141, 70), pixel(146, 70)), (light, dark));
        assert_eq!((pixel(70, 101), pixel(70, 106)), (light, dark));
        assert_eq!((pixel(193, 70), pixel(198, 70)), (dark, light));
        assert_eq!((pixel(301, 70), pixel(306, 70)), (dark, light));
        // Double: two 3px lines with a 3px gap between.
        assert_eq!(pixel(352, 70), light);
        assert_eq!(pixel(355, 70), [255, 255, 255]);
        assert_eq!(pixel(358, 70), light);
        // Inset: dark top and left, light bottom and right.
        assert_eq!((pixel(515, 70), pixel(624, 70)), (dark, light));
        assert_eq!((pixel(570, 35), pixel(570, 104)), (dark, light));
    }

    #[test]
    fn alike_sides_meet_without_a_seam_at_their_corner() {
        // Each side was clipped to its own half of a corner, so two
        // antialiased clips met on the diagonal and let the white
        // background show through between sides of one color.
        let pixel = render_pixels(
            r#"<!doctype html><body style="margin:0;background:black">
            <div style="position:absolute;left:20px;top:20px;width:60px;height:30px;
                background:white;border:7px outset #86866d"></div>
            <div style="position:absolute;left:140px;top:20px;width:60px;height:30px;
                background:white;border:solid #86866d;border-width:7px 5px 9px 6px"></div>
            <div style="position:absolute;left:260px;top:20px;width:60px;height:30px;
                background:white;border:9px double #86866d"></div>"#,
        );
        for offset in 0..7 {
            // The outset's light top and left, then its dark bottom and right.
            assert_eq!(pixel(20 + offset, 20 + offset), [134, 134, 109], "{offset}");
            assert_eq!(pixel(92 - offset, 63 - offset), [89, 89, 72], "{offset}");
        }
        for (x, y) in [
            (140, 20),
            (143, 23),
            (145, 24),
            (210, 21),
            (209, 64),
            (142, 64),
        ] {
            assert_eq!(pixel(x, y), [134, 134, 109], "({x}, {y})");
        }
        for offset in [0, 1, 7, 8] {
            assert_eq!(
                pixel(260 + offset, 20 + offset),
                [134, 134, 109],
                "{offset}"
            );
        }
    }

    #[test]
    fn transparent_border_sides_form_a_triangle() {
        let (_, layout) = render_fixture(
            r#"<!doctype html><body style="margin:0;background:white">
            <div style="width:0;height:0;border:20px solid transparent;border-top-color:red"></div></body>"#,
        );
        let pixels = crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
            .unwrap()
            .pixels;
        let at = |x: usize, y: usize| &pixels[(y * 800 + x) * 4..(y * 800 + x) * 4 + 3];
        assert_eq!(at(20, 5), [255, 0, 0]);
        assert_eq!(at(2, 15), [255, 255, 255]);
        assert_eq!(at(20, 25), [255, 255, 255]);
    }

    #[test]
    fn background_clip_preserves_rounded_padding_and_content_edges() {
        // CSS Backgrounds 3 §3.11/§5.2: a clipped background follows the
        // inner corner after subtracting border and padding insets.
        let border = CssRect::new(0.0, 0.0, 120.0, 40.0);
        let outer = PaintShape::RoundedRect {
            rect: border,
            radii: CornerRadii {
                corners: [(20.0, 20.0); 4],
            },
        };
        for (clip, expected) in [
            (CssRect::new(1.0, 1.0, 118.0, 38.0), 19.0),
            (CssRect::new(6.0, 6.0, 108.0, 28.0), 14.0),
        ] {
            let PaintShape::RoundedRect { radii, .. } = background_clip_shape(clip, &outer, border)
            else {
                panic!("clipped rounded background became rectangular");
            };
            assert_eq!(radii.corners, [(expected, expected); 4]);
        }
    }

    fn render_fixture(html: &str) -> (Dom, crate::layout2::GraphicalLayout) {
        render_fixture_with_images(html, &Default::default())
    }

    fn render_fixture_with_images(
        html: &str,
        images: &crate::layout2::ImageSizes,
    ) -> (Dom, crate::layout2::GraphicalLayout) {
        let mut dom = Dom::parse_document(html);
        dom.set_render_clickables(Default::default(), true);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            crate::layout2::Viewport::new(800., 600.),
            &[],
            &Default::default(),
            images,
        );
        (dom, layout)
    }

    #[test]
    fn a_family_without_an_italic_face_is_slanted() {
        // CSS Fonts 4 #font-synthesis-style: with `font-synthesis-style:
        // auto`, italic text in a family that has only an upright face is
        // drawn obliquely. The stem of an "l" leans right toward its top.
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/dejavu/DejaVuSerif.ttf"
        ))
        .unwrap();
        let url: String = bytes.iter().map(|byte| format!("%{byte:02X}")).collect();
        let lean = |style: &str| {
            let (_, layout) = render_fixture(&format!(
                "<style>@font-face{{font-family:Web;src:url(data:font/ttf,{url})}} \
                 body{{margin:0;background:white}} p{{margin:0;font:100px/1 Web;{style}}}</style><p>l</p>"
            ));
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            let centroid = |rows: std::ops::Range<usize>| {
                let (mut sum, mut count) = (0.0, 0.0);
                for (i, p) in frame.pixels.as_chunks::<4>().0.iter().enumerate() {
                    if p[0] < 100 && rows.contains(&(i / 800)) {
                        sum += (i % 800) as f32;
                        count += 1.0;
                    }
                }
                assert!(count > 0.0, "{style}: no ink in {rows:?}");
                sum / count
            };
            centroid(20..40) - centroid(70..85)
        };
        assert!(lean("").abs() < 2.0, "upright: {}", lean(""));
        assert!(
            lean("font-style:italic") > 8.0,
            "italic: {}",
            lean("font-style:italic")
        );
    }

    #[test]
    fn an_authored_border_color_or_width_restyles_a_buttons_native_edge() {
        // As Gecko and Blink apply CSS UI 4 #appearance-switching: an
        // authored border color or width restyles the UA border, so a
        // transparent border color leaves an image button borderless.
        let ink = |style: &str| {
            let (_, layout) = render_fixture(&format!(
                "<style>body{{margin:0;background:white}}</style>\
                 <button style='width:60px;height:40px;background-color:transparent;{style}'></button>"
            ));
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            frame
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] < 200)
                .count()
        };
        assert!(ink("") > 50, "the native edge");
        assert_eq!(ink("border-color:transparent"), 0);
        assert_eq!(ink("border-width:0"), 0);
        assert!(ink("border-color:blue") > 50, "a recolored edge");
    }

    #[test]
    fn an_authored_initial_or_unset_background_devolves_a_native_button() {
        // CSS UI 4 #appearance-disabling-properties: any author-origin
        // cascaded value, `initial` and `unset` included, replaces the
        // native surface with the CSS background (here transparent). Only a
        // `revert` rolled back to the UA origin keeps the native surface.
        let center = |style: &str| {
            let (_, layout) = render_fixture(&format!(
                "<style>body{{margin:0;background:rgb(200,208,224)}}</style>\
                 <button style='width:60px;height:30px;border:none;{style}'></button>"
            ));
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            let at = (15 * 800 + 30) * 4;
            <[u8; 3]>::try_from(&frame.pixels[at..at + 3]).unwrap()
        };
        let page = [200, 208, 224];
        for style in [
            "background-color:unset",
            "background-color:initial",
            "background:unset",
            "background-color:transparent",
        ] {
            assert_eq!(center(style), page, "{style}");
        }
        assert_ne!(center(""), page, "the native surface");
        assert_ne!(center("background-color:revert"), page, "revert");
        // `border: unset` computes to no border, so no native edge either.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;background:white}</style>\
             <button style='width:60px;height:30px;background-color:unset;border:unset'></button>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        assert!(
            frame.pixels.as_chunks::<4>().0.iter().all(|p| p[0] > 200),
            "no native edge"
        );
    }

    #[test]
    fn outer_shadows_are_not_drawn_inside_the_box() {
        // CSS Backgrounds 3 #shadow-shape: an outer box-shadow is clipped
        // inside the border box, so a transparent box shows what is behind it.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;background:white} div{margin:20px;width:100px;height:50px;\
             border-radius:8px;box-shadow:6px 6px 4px 10px rgb(255,0,0)}</style><div></div>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let pixel = |x: usize, y: usize| &frame.pixels[(y * 800 + x) * 4..(y * 800 + x) * 4 + 3];
        assert_eq!(pixel(70, 45), [255, 255, 255], "inside the box");
        assert_eq!(pixel(125, 45)[..2], [255, 0], "the shadow outside it");
    }

    #[test]
    fn the_first_box_shadow_is_on_top() {
        // CSS Backgrounds 3 #shadow-layers: shadows are applied front to
        // back, the first on top, for outer and inner shadows alike.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;background:white} div{width:100px;height:100px;margin-bottom:20px}\
             </style><div style='box-shadow:5px 5px 0 red,10px 10px 0 blue'></div>\
             <div style='box-shadow:inset 0 10px 0 red,inset 0 20px 0 blue'></div>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let pixel = |x: usize, y: usize| frame.pixels[(y * 800 + x) * 4..][..3].to_vec();
        assert_eq!(pixel(102, 50), [255, 0, 0], "outer overlap");
        assert_eq!(pixel(108, 50), [0, 0, 255]);
        assert_eq!(pixel(50, 125), [255, 0, 0], "inner overlap");
        assert_eq!(pixel(50, 135), [0, 0, 255]);
    }

    #[test]
    fn box_shadows_default_to_the_current_color() {
        // CSS Backgrounds 3 #shadow-color: if the color is absent, it
        // defaults to currentColor, as does the currentcolor keyword.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;background:white} div{width:50px;height:50px;margin-bottom:20px}\
             </style><div style='color:red;box-shadow:10px 0 0'></div>\
             <div style='color:blue;box-shadow:inset 0 10px 0 currentcolor'></div>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let pixel = |x: usize, y: usize| frame.pixels[(y * 800 + x) * 4..][..3].to_vec();
        assert_eq!(pixel(55, 25), [255, 0, 0]);
        assert_eq!(pixel(25, 75), [0, 0, 255]);
    }

    #[test]
    fn a_wrapped_inline_box_casts_no_shadow_from_its_cut_edges() {
        // CSS Backgrounds 3 #box-decoration-break `slice`: the shadow is that
        // of the unbroken box, so the first line's fragment, which the box
        // continues past, casts none from its end edge.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;background:white} p{margin:0;width:200px;font:20px/30px monospace}\
             span{box-shadow:6px 0 0 rgb(255,0,0)}</style><p><span>aaaa bbbb cccc dddd eeee</span></p>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let red = |rows: std::ops::Range<usize>| {
            frame
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .enumerate()
                .filter(|(i, p)| rows.contains(&(i / 800)) && p[..3] == [255, 0, 0])
                .count()
        };
        assert_eq!(
            red(0..30),
            0,
            "the first fragment continues on the next line"
        );
        assert!(red(30..60) > 50, "the last fragment ends the box");
    }

    #[test]
    fn inner_shadows_paint_inside_the_padding_box_above_the_background() {
        // CSS Backgrounds 3 #shadow-shape: an inner shadow is cast as if
        // everything outside the padding edge were opaque, is drawn inside
        // the padding edge only, and a spread contracts its perimeter.
        // #shadow-layers draws it immediately above the background, below
        // the borders. Expected pixels were measured in Gecko and Blink.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;background:white} div{width:100px;height:100px;margin-bottom:10px}\
             .s{background:#c7b6ce;box-shadow:inset 0 10px 0 0 #b99077}</style>\
             <div class=s></div><div class=s style='border-radius:20px'></div>\
             <div style='box-shadow:inset 0 0 0 10px #000'></div>\
             <div style='width:80px;height:80px;border:10px solid #0f0;box-shadow:inset 0 0 0 10px #000'></div>\
             <div style='box-shadow:inset 0 10px 10px #000'></div>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let pixel = |x: usize, y: usize| frame.pixels[(y * 800 + x) * 4..][..3].to_vec();
        for top in [0, 110] {
            assert_eq!(
                pixel(50, top + 5),
                [185, 144, 119],
                "shadow over the background"
            );
            assert_eq!(pixel(50, top + 15), [199, 182, 206], "background below it");
        }
        assert_eq!(pixel(5, 270), [0, 0, 0], "spread inside the padding edge");
        assert_eq!(pixel(50, 225), [0, 0, 0]);
        assert_eq!(pixel(50, 270), [255, 255, 255]);
        assert_eq!(pixel(5, 380), [0, 255, 0], "the border stays on top");
        assert_eq!(pixel(15, 380), [0, 0, 0]);
        assert_eq!(pixel(25, 380), [255, 255, 255]);
        // Gecko and Blink: about 7, then 138 on the shifted edge, then white.
        let blurred = [441, 450, 470].map(|y| pixel(50, y)[0]);
        assert!(
            blurred[0] < 40 && (100..170).contains(&blurred[1]) && blurred[2] > 250,
            "a blurred shadow fades out below the top edge: {blurred:?}"
        );
    }

    #[test]
    fn generated_inline_boxes_paint_their_backgrounds_and_borders() {
        // CSS Pseudo 4 #treelike: ::before/::after and ::first-letter boxes
        // are styleable inline boxes. A badge's white text is unreadable
        // without its background.
        let count = |rule: &str, rgb: [u8; 3]| {
            let (_, layout) = render_fixture(&format!(
                "<style>body{{margin:0;background:white;font:20px sans-serif}} {rule}</style>\
                 <p class=a>Headline text</p>"
            ));
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            frame
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[..3] == rgb)
                .count()
        };
        let red = [255, 0, 0];
        assert!(
            count(
                ".a::before{content:'NEW';background:red;color:white;padding:0 4px}",
                red
            ) > 300
        );
        assert!(count(".a::after{content:'tag';border:3px solid red}", red) > 100);
        assert!(count(".a::first-letter{background:red}", red) > 100);
        assert_eq!(
            count(
                ".a::before{content:'NEW';background:red;visibility:hidden}",
                red
            ),
            0
        );
    }

    #[test]
    fn first_letter_styles_its_text_inline_or_as_a_drop_cap() {
        // CSS Pseudo 4 #first-letter-pseudo: ::first-letter wraps the first
        // letter (with adjacent punctuation, `O’`) of the first formatted
        // line, inside any inline element, and floats when it floats.
        let ink = |html: &str| {
            let (dom, layout) = render_fixture(html);
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            let mut red = (0, f32::MAX, 0.0f32, 0.0f32);
            let mut blue_left_below = f32::MAX;
            for (i, p) in frame.pixels.as_chunks::<4>().0.iter().enumerate() {
                let (x, y) = ((i % 800) as f32, (i / 800) as f32);
                if p[0] > 200 && p[1] < 80 && p[2] < 80 {
                    red = (red.0 + 1, red.1.min(y), red.2.max(y), red.3.max(x));
                }
                if p[2] > 200 && p[0] < 80 && p[1] < 80 && y > 40.0 {
                    blue_left_below = blue_left_below.min(x);
                }
            }
            let p = layout
                .boxes
                .get(&dom.get_by_id("p").unwrap())
                .unwrap()
                .height;
            (red, blue_left_below, p)
        };
        let page = |rule: &str, text: &str| {
            format!(
                "<style>body{{margin:0;background:white}} p{{margin:0;font:16px/20px sans-serif;color:blue;width:300px}} {rule}</style>\
                 <p id=p><i>{text}</i></p>"
            )
        };
        let text = "O’er the strange woods and over the sea, over spirits on the wing";
        let (plain, _, plain_height) = ink(&page("", text));
        assert_eq!(plain.0, 0);
        let (inline, _, inline_height) = ink(&page(
            "p::first-letter{color:red;font-size:48px;line-height:48px}",
            text,
        ));
        assert!(inline.0 > 200, "the initial is red: {inline:?}");
        assert!(inline.2 - inline.1 > 25.0, "and large: {inline:?}");
        assert!(
            inline_height > plain_height + 20.0,
            "its line grows: {inline_height}"
        );
        let (cap, blue_left, cap_height) = ink(&page(
            "p:first-letter{color:red;float:left;font-size:48px;line-height:48px}",
            text,
        ));
        assert!(cap.0 > 200, "{cap:?}");
        assert!(
            blue_left > cap.3,
            "lines beside the floated cap start after it: {blue_left} vs {cap:?}"
        );
        assert!(cap_height < inline_height, "a float does not grow the line");
        let (spaced, spaced_left, _) = ink(&page(
            "p::first-letter{color:red;float:left;font-size:48px;line-height:48px;margin-right:1em}",
            text,
        ));
        assert!(
            spaced_left - spaced.3 > 40.0,
            "its em margin uses the letter's own font size: {spaced_left} vs {spaced:?}"
        );
        let (nested, _, _) = ink(&page(
            ":first-letter{color:red;float:left;font-size:48px;line-height:48px}",
            text,
        ));
        assert_eq!(
            nested.3, cap.3,
            "the html, body and p pseudo-elements share one first letter"
        );
        let (none, _, _) = ink(&page(
            "p::first-letter{color:red}",
            "<img width=1 height=1>Text",
        ));
        assert_eq!(none.0, 0, "an image precedes any first letter");
        let (listed, _, _) = ink(
            "<style>body{margin:0;background:white} h1{margin:0;font:40px sans-serif;color:blue} \
             h1, p::first-letter{color:red}</style><h1>Title</h1><p id=p>x</p>",
        );
        assert!(listed.0 > 400, "the selector list stays valid: {listed:?}");
    }

    #[test]
    fn text_stroke_paints_transparent_glyphs_without_changing_layout() {
        let render = |style: &str| {
            render_fixture(&format!(
                "<body style='margin:0;background:white'><div id=x style='font:60px monospace;color:transparent;{style}'>Outline</div></body>"
            ))
        };
        let (plain_dom, plain) = render("");
        let (dom, stroked) = render("-webkit-text-stroke:2px red");
        let a = plain.boxes.get(&plain_dom.get_by_id("x").unwrap()).unwrap();
        let b = stroked.boxes.get(&dom.get_by_id("x").unwrap()).unwrap();
        assert_eq!((a.width, a.height), (b.width, b.height));
        let frame = crate::render::headless::render_paint(&stroked.paint, CssSize::new(800., 600.))
            .unwrap();
        let red = frame
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 200 && p[1] < 100 && p[2] < 100)
            .count();
        assert!(red > 200, "outline must produce visible red ink: {red}");
        let fill_frame = crate::render::headless::render_paint(
            &render("-webkit-text-fill-color:red").1.paint,
            CssSize::new(800., 600.),
        )
        .unwrap();
        let filled = fill_frame
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 200 && p[1] < 100 && p[2] < 100)
            .count();
        assert!(
            filled > red,
            "outlined letters must retain hollow interiors: filled={filled}, stroke={red}"
        );
    }

    #[test]
    fn vertical_text_sizes_columns_before_authored_rotation() {
        for mode in ["vertical-rl", "vertical-lr"] {
            let (dom, layout) = render_fixture(&format!(
                "<style>body{{margin:0}}#label{{position:absolute;left:40px;top:30px;font:20px/1 monospace;writing-mode:{mode};transform:rotate(180deg)}}#label span{{color:red}}</style><div id=label><span id=text>ENTRY / POINT</span></div>"
            ));
            let label = layout.boxes.get(&dom.get_by_id("label").unwrap()).unwrap();
            let text = layout.boxes.get(&dom.get_by_id("text").unwrap()).unwrap();
            assert!((label.width - 20.).abs() < 0.1, "{label:?}");
            assert!(label.height > 120. && label.height < 200., "{label:?}");
            assert!(
                text.width < 30. && text.height > 120.,
                "inline geometry must be vertical: {text:?}"
            );
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            let mut ink = Vec::new();
            for (index, pixel) in frame.pixels.as_chunks::<4>().0.iter().enumerate() {
                if pixel[0] > 160 && pixel[1] < 100 && pixel[2] < 100 {
                    ink.push(((index % 800) as u32, (index / 800) as u32));
                }
            }
            assert!(ink.len() > 100);
            assert!(
                ink.iter().all(|&(x, y)| (39..=61).contains(&x)
                    && y >= 29
                    && (y as f64) < 31. + label.height),
                "paint must stay inside the rotated vertical label"
            );
        }
    }

    #[test]
    fn vertical_text_wraps_into_columns_and_pseudo_inherits_font() {
        for mode in ["vertical-rl", "vertical-lr"] {
            let (dom, layout) = render_fixture(&format!(
                "<style>body{{margin:0}}#label{{font:20px/1 monospace;writing-mode:{mode};height:72px}}#host{{position:relative;margin-left:80px;font-size:20px}}#host::before{{content:'ENTRY / POINT';position:absolute;left:-30px;top:0;font:.5em/1 monospace;writing-mode:vertical-rl;transform:rotate(180deg)}}</style><div id=label><span id=first>ABCD</span><br><span id=second>EFGH</span></div><div id=host>Heading</div><div id=after>Following</div>"
            ));
            let get = |id| layout.boxes.get(&dom.get_by_id(id).unwrap()).unwrap();
            let first = get("first");
            let second = get("second");
            assert!((get("label").width - 40.).abs() < 0.1, "{:?}", get("label"));
            assert!((get("label").height - 72.).abs() < 0.1);
            assert_eq!(first.left > second.left, mode == "vertical-rl");
            assert!(get("after").top >= 90.);
            let generated = layout
                .paint
                .primitives
                .iter()
                .find_map(|command| match command {
                    DisplayCommand::GlyphRun { shaped, .. } if shaped.text.contains("ENTRY") => {
                        Some(shaped)
                    }
                    _ => None,
                })
                .unwrap();
            assert_eq!(generated.runs[0].font_size, 10.);
        }
    }

    #[test]
    fn canvas_background_source_is_recomputed_after_root_style_changes() {
        let mut dom = Dom::parse_document(
            r#"<html style="background:transparent"><body style="margin:8px;background:#123456"><div style="height:20px"></div></body></html>"#,
        );
        let root = dom.document_element().unwrap();
        let body_color = PaintColor::Rgba(18, 52, 86, 255);
        let root_color = PaintColor::Rgba(101, 67, 33, 255);
        for (style, expected_canvas, body_fills) in [
            ("background:transparent", body_color, 0),
            ("background:#654321", root_color, 1),
            ("background:transparent", body_color, 0),
        ] {
            dom.set_attr(root, "style", style);
            let layout = crate::layout2::lay_out_graphical(
                &dom,
                &Url::parse("https://example.test/").unwrap(),
                crate::layout2::Viewport::new(800., 600.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            assert_eq!(layout.paint.background, Some(expected_canvas));
            assert_eq!(
                layout
                    .paint
                    .primitives
                    .iter()
                    .filter(|command| matches!(
                        command,
                        DisplayCommand::Fill { brush: PaintBrush::Solid(color), .. }
                            if *color == body_color
                    ))
                    .count(),
                body_fills,
                "a propagated body background must not paint twice: {style}"
            );
        }
    }

    #[test]
    fn empty_background_layers_preserve_image_indices_and_color_clip() {
        // CSS Backgrounds 3 #background-image / #background-color: `none`
        // still occupies its index, including the bottom layer's color clip.
        // A gradient covering its content-box clip is one fill; over the
        // border box, its padding-box tile repeats into the border area.
        let border_tiles: Vec<_> = [-56., 4., 64.]
            .into_iter()
            .flat_map(|y| [-96., 4., 104.].map(|x| CssRect::new(x, y, 100., 60.)))
            .collect();
        for (images, gradient_rects_expected) in [
            ("none, none", Vec::new()),
            (
                "none, linear-gradient(red, blue)",
                vec![CssRect::new(14., 14., 80., 40.)],
            ),
            ("linear-gradient(red, blue), none", border_tiles),
        ] {
            let (_, layout) = render_fixture(&format!(
                r#"<body style="margin:0"><div style="width:80px;height:40px;padding:10px;border:4px solid black;background-color:#123456;background-image:{images};background-clip:border-box,content-box"></div>"#
            ));
            let color_rect = layout
                .paint
                .primitives
                .iter()
                .find_map(|command| match command {
                    DisplayCommand::Fill {
                        shape: PaintShape::Rect(rect),
                        brush: PaintBrush::Solid(PaintColor::Rgba(18, 52, 86, 255)),
                    } => Some(*rect),
                    _ => None,
                });
            assert_eq!(
                color_rect,
                Some(CssRect::new(14., 14., 80., 40.)),
                "{images}"
            );
            let gradient_rects: Vec<_> = layout
                .paint
                .primitives
                .iter()
                .filter_map(|command| match command {
                    DisplayCommand::Fill {
                        shape: PaintShape::Rect(rect),
                        brush: PaintBrush::LinearGradient { .. },
                    } => Some(*rect),
                    _ => None,
                })
                .collect();
            assert_eq!(gradient_rects, gradient_rects_expected, "{images}");
        }
    }

    #[test]
    fn background_keyword_axes_position_oversized_images_and_sprites() {
        // CSS Backgrounds 3 #background-position: keyword pairs can be
        // reordered, and percentages use the area minus the image size.
        for (width, height, image) in [
            (960., 620., (1920, 620)),
            (1920., 620., (1920, 620)),
            (10., 38., (8, 76)),
        ] {
            for (position, fraction) in [
                ("top center", (0.5, 0.)),
                ("center top", (0.5, 0.)),
                ("bottom right", (1., 1.)),
                ("right bottom", (1., 1.)),
                ("center left", (0., 0.5)),
                ("left center", (0., 0.5)),
                ("BOTTOM LEFT", (0., 1.)),
                ("center", (0.5, 0.5)),
                ("top", (0.5, 0.)),
                ("25% 75%", (0.25, 0.75)),
            ] {
                let dom = Dom::parse_document(&format!(
                    "<body style='margin:0'><div id=bg style='margin-left:13px;margin-top:17px;width:{width}px;height:{height}px;background:url(tile.png) {position} no-repeat'></div>"
                ));
                let layout = crate::layout2::lay_out_graphical(
                    &dom,
                    &Url::parse("https://example.test/").unwrap(),
                    crate::layout2::Viewport::new(width + 30., height + 30.),
                    &[],
                    &Default::default(),
                    &[("https://example.test/tile.png".into(), image)].into(),
                );
                let node = dom.get_by_id("bg").unwrap();
                let rect = layout
                    .paint
                    .primitives
                    .iter()
                    .find_map(|p| match p {
                        DisplayCommand::Image { node: n, rect, .. } if *n == node => Some(*rect),
                        _ => None,
                    })
                    .expect("background tile");
                assert_eq!(
                    rect,
                    CssRect::new(
                        13. + (width - image.0 as f32) * fraction.0,
                        17. + (height - image.1 as f32) * fraction.1,
                        image.0 as f32,
                        image.1 as f32,
                    ),
                    "{position}, area {width}x{height}"
                );
            }
        }
    }

    #[test]
    fn natural_size_images_paint_on_device_pixels() {
        // CSS leaves pixel snapping undefined (css-images-3
        // #the-image-rendering covers only scaling); Gecko and Blink round
        // replaced content to device pixels. One-pixel stripes at a half-pixel
        // offset, set directly or centered in an odd-width flex column, stay
        // crisp at x=11 and x=5 instead of being resampled to gray.
        let stripes = image::RgbaImage::from_fn(8, 8, |x, _| {
            image::Rgba(if x % 2 == 0 {
                [0, 0, 0, 255]
            } else {
                [255, 255, 255, 255]
            })
        });
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(stripes)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let url: String = png.iter().map(|byte| format!("%{byte:02X}")).collect();
        let frame = crate::render::headless::render_html(
            &format!(
                "<!doctype html><style>body{{margin:0;background:#fff}} img{{display:block}}</style>\
                 <div style='margin:10.5px 0 0 10.5px'><img src='data:image/png,{url}'></div>\
                 <div style='display:flex;flex-direction:column;align-items:center;width:17px;\
                 margin-top:10px'><img src='data:image/png,{url}'></div>"
            ),
            &Url::parse("https://example.test/").unwrap(),
            CssSize::new(100., 100.),
        )
        .unwrap();
        let row = |y: usize, x: std::ops::Range<usize>| {
            x.map(|x| frame.pixels[(y * 100 + x) * 4])
                .collect::<Vec<_>>()
        };
        let crisp = [255, 0, 255, 0, 255, 0, 255, 0, 255, 255];
        assert_eq!(row(14, 10..20), crisp, "offset by 10.5px");
        assert_eq!(row(10, 10..20), [255; 10], "rows also snap");
        assert_eq!(row(32, 4..14), crisp, "centered in a 17px column");
    }

    #[test]
    fn zero_font_size_keeps_line_height_around_middle_aligned_inline_blocks() {
        let (dom, layout) = render_fixture(
            "<!doctype html><style>body{margin:0} #bar{font-size:0;line-height:34px;background:#a47618} #dot{display:inline-block;width:18px;height:18px;vertical-align:middle;background:#56390a}</style><div id=bar><span id=dot></span></div><div id=after>Next</div>",
        );
        let get = |id| layout.boxes.get(&dom.get_by_id(id).unwrap()).unwrap();
        assert_eq!(get("bar").height, 34.);
        assert_eq!(get("dot").top, 8.);
        assert_eq!(get("dot").height, 18.);
        assert_eq!(get("after").top, 34.);
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let pixel = |x: usize, y: usize| &frame.pixels[(y * 800 + x) * 4..(y * 800 + x) * 4 + 3];
        for y in [0, 7, 26, 33] {
            assert_eq!(pixel(9, y), [164, 118, 24]);
        }
        assert_eq!(pixel(9, 16), [86, 57, 10]);
    }

    #[test]
    fn ratio_only_svg_background_auto_uses_the_positioning_area() {
        // CSS Backgrounds 3 #background-size: with neither natural dimension,
        // auto/auto uses contain. A decoder's fallback raster is not a natural
        // dimension. One definite axis still resolves the other by the ratio.
        let external = "https://example.test/ratio-only-background.svg";
        crate::img::record_svg_intrinsic_metadata(
            external,
            br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 80 40"/>"#,
        );
        assert_eq!(
            background_size(
                "20px auto",
                (300., 150.),
                CssRect::default(),
                Some(2.),
                LengthBasis::fixed()
            ),
            (20., 10.)
        );
        assert_eq!(
            background_size(
                "auto",
                (18., 9.),
                CssRect::new(0., 0., 80., 60.),
                None,
                LengthBasis::fixed()
            ),
            (18., 9.)
        );
        for source in [
            external,
            "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20viewBox='0%200%2080%2040'/%3E",
        ] {
            for (size, expected) in [
                ("auto", (80., 40.)),
                ("contain", (80., 40.)),
                ("cover", (120., 60.)),
                ("20px auto", (20., 10.)),
                ("auto 30px", (60., 30.)),
                ("50% auto", (40., 20.)),
                ("16px 36px", (16., 36.)),
            ] {
                // The decoder's fallback raster is loaded but not natural.
                let decoded = [(source.to_string(), (300, 150))].into_iter().collect();
                let (dom, layout) = render_fixture_with_images(
                    &format!(
                        r#"<div id=tile style="width:80px;height:60px;background-image:url(&quot;{source}&quot;);background-repeat:no-repeat;background-size:{size}"></div>"#
                    ),
                    &decoded,
                );
                let node = dom.get_by_id("tile").unwrap();
                let rect = layout
                    .paint
                    .primitives
                    .iter()
                    .find_map(|command| match command {
                        DisplayCommand::Image { node: n, rect, .. } if *n == node => Some(*rect),
                        _ => None,
                    })
                    .expect("background image tile");
                assert_eq!((rect.width, rect.height), expected, "{source}: {size}");
            }
        }
    }

    #[test]
    fn border_images_slice_into_regions_and_tile_their_edges() {
        // CSS Backgrounds 3 #border-images: a 30px image sliced at 10 draws
        // four corners and four edges into a 10px border (no middle without
        // `fill`); `round` fits whole 10px tiles along the 100px edges. An
        // image that is not available leaves the border styles in place.
        let html = r#"<body style="margin:0"><div id=b style="width:100px;height:40px;border:10px solid red;border-image:url(https://example.test/b.png) 10 round"></div></body>"#;
        let images = [("https://example.test/b.png".to_string(), (30, 30))]
            .into_iter()
            .collect();
        let (_, layout) = render_fixture_with_images(html, &images);
        let crops: Vec<(CssRect, CssRect)> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::Image {
                    rect,
                    source_rect: Some(source),
                    fit: ImageFit::Fill,
                    ..
                } => Some((*rect, *source)),
                _ => None,
            })
            .collect();
        let corners = crops
            .iter()
            .filter(|(rect, _)| rect.width == 10.0 && rect.height == 10.0);
        // Corners and 10px round tiles share the 10x10 size: 4 + 10 + 10 + 4 + 4.
        assert_eq!(corners.count(), 32, "{crops:?}");
        assert!(
            crops
                .iter()
                .all(|(_, source)| source.width == 10.0 && source.height == 10.0)
        );
        let red_border = |commands: &[DisplayCommand]| {
            commands.iter().any(|command| {
                matches!(
                    command,
                    DisplayCommand::Fill { brush: PaintBrush::Solid(color), .. }
                        | DisplayCommand::Stroke { brush: PaintBrush::Solid(color), .. }
                        if *color == PaintColor::Rgba(255, 0, 0, 255)
                )
            })
        };
        assert!(!red_border(&layout.paint.primitives));
        let (_, pending) = render_fixture_with_images(html, &Default::default());
        assert!(red_border(&pending.paint.primitives));
    }

    #[test]
    fn round_border_image_tiles_meet_on_whole_pixels() {
        // CSS Backgrounds 3 #border-image-repeat: `round` rescales the tiles
        // to fill the edge exactly, here three 33.33px tiles along a 100px
        // edge. Each tile was drawn at its fractional position, so two
        // antialiased edges shared a pixel and the page showed through.
        let html = r#"<body style="margin:0"><div style="margin:20px;width:100px;height:30px;
            border:15px solid transparent;border-image:url(https://example.test/c.png) 15 round">
            </div></body>"#;
        let images = [("https://example.test/c.png".to_string(), (69, 69))]
            .into_iter()
            .collect();
        let (_, layout) = render_fixture_with_images(html, &images);
        let mut tiles: Vec<CssRect> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::Image { rect, .. }
                    if rect.y == 20.0 && rect.x >= 35.0 && rect.x < 135.0 =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .collect();
        tiles.sort_by(|a, b| a.x.total_cmp(&b.x));
        assert_eq!(tiles.len(), 3, "{tiles:?}");
        let mut edge = 35.0;
        for tile in &tiles {
            assert_eq!(tile.x, edge, "{tiles:?}");
            edge = tile.x + tile.width;
            assert_eq!(edge, edge.round(), "{tiles:?}");
        }
        assert_eq!(edge, 135.0);
    }

    #[test]
    fn stretched_images_paint_with_fill_unless_their_size_is_unknown() {
        // css-images-3 §5.5: the initial object-fit, fill, stretches the
        // image over a box whose ratio differs; layout already resolved it.
        // An image still loading keeps its ratio inside the provisional box.
        let html = r#"<body style="margin:0"><img src="https://example.test/r.png" width=200 height=10></body>"#;
        for (known, expected) in [(true, ImageFit::Fill), (false, ImageFit::Contain)] {
            let images = if known {
                [("https://example.test/r.png".to_string(), (40, 20))]
                    .into_iter()
                    .collect()
            } else {
                Default::default()
            };
            let (_, layout) = render_fixture_with_images(html, &images);
            let fit = layout
                .paint
                .primitives
                .iter()
                .find_map(|command| match command {
                    DisplayCommand::Image { fit, .. } => Some(*fit),
                    _ => None,
                });
            assert_eq!(fit, Some(expected), "known={known}");
        }
    }

    #[test]
    fn inline_boxes_paint_sliced_backgrounds_and_borders_per_line() {
        // CSS 2.2 Appendix E / CSS Backgrounds 3 #box-decoration-break: a
        // wrapped span paints one background per line over its content area
        // (not the 24px line height); its start border appears only on the
        // first fragment. An undecorated span paints nothing of its own.
        let (dom, layout) = render_fixture(
            r#"<body style="margin:0;font:16px/24px sans-serif;width:200px"><p style="margin:0">a <span id=s style="background:#ff0000;padding:0 5px;border-left:3px solid #0000ff">long text that wraps across lines</span> b <span>plain</span></p>"#,
        );
        let span = dom.get_by_id("s").unwrap();
        let fills = |color: PaintColor| {
            layout
                .paint
                .primitives
                .iter()
                .filter_map(|command| match command {
                    DisplayCommand::Fill {
                        shape: PaintShape::Rect(rect),
                        brush: PaintBrush::Solid(fill),
                    } if *fill == color => Some(*rect),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let red = fills(PaintColor::Rgba(255, 0, 0, 255));
        assert_eq!(red.len(), 2, "{red:?}");
        assert!(
            red.iter()
                .all(|rect| rect.height > 14.0 && rect.height < 24.0),
            "{red:?}"
        );
        assert!(red[0].x > 10.0, "{red:?}");
        assert!(
            red[1].x.abs() < 0.01,
            "continuation starts at the line edge: {red:?}"
        );
        assert!(red[1].y > red[0].y + 20.0, "{red:?}");
        let blue = layout
            .paint
            .primitives
            .iter()
            .filter(|command| {
                matches!(command, DisplayCommand::Fill { brush: PaintBrush::Solid(color), .. }
                    if *color == PaintColor::Rgba(0, 0, 255, 255))
            })
            .count();
        assert_eq!(blue, 1, "start border on the first fragment only");
        assert!(layout.boxes.contains_key(&span));
    }

    #[test]
    fn gradient_color_stops_follow_the_fixup_rules() {
        // CSS Images 3 #color-stop-syntax / #color-stop-fixup: lengths are
        // fractions of the 100px gradient line, positions never decrease,
        // unpositioned stops are spaced evenly, a stop may carry two
        // positions, and a color hint adds a curved transition.
        let offsets = |value: &str| {
            let Some(PaintBrush::LinearGradient { stops, .. }) = parse_gradient(
                value,
                CssRect::new(0.0, 0.0, 100.0, 40.0),
                LengthBasis::fixed(),
            ) else {
                panic!("{value}")
            };
            stops
                .iter()
                .map(|stop| (stop.offset * 100.0).round())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            offsets("linear-gradient(90deg,red 1px,blue 1px)"),
            [1.0, 1.0]
        );
        assert_eq!(
            offsets("linear-gradient(90deg,red 40%,blue 10%)"),
            [40.0, 40.0]
        );
        assert_eq!(
            offsets("linear-gradient(90deg,red,lime,blue 50px,white,black)"),
            [0.0, 25.0, 50.0, 75.0, 100.0]
        );
        assert_eq!(
            offsets("linear-gradient(90deg,red 20px 40px,blue)"),
            [20.0, 40.0, 100.0]
        );
        let hinted = offsets("linear-gradient(90deg,black,20%,white)");
        assert_eq!(hinted.len(), 17);
        assert_eq!((hinted[0], hinted[16]), (0.0, 100.0));
        // A midpoint hint is the ordinary linear transition.
        assert_eq!(
            offsets("linear-gradient(90deg,black,50%,white)"),
            [0.0, 100.0]
        );
        for invalid in [
            "linear-gradient(red,10%)",
            "linear-gradient(10%,red,blue)",
            "linear-gradient(red,10%,20%,blue)",
            "linear-gradient(red 1px 2px 3px,blue)",
        ] {
            assert!(
                parse_gradient(
                    invalid,
                    CssRect::new(0.0, 0.0, 10.0, 10.0),
                    LengthBasis::fixed()
                )
                .is_none(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn unloaded_background_images_draw_nothing_but_are_requested() {
        // CSS Backgrounds 3 #background-image: an image that is still loading
        // or failed to download counts as a layer but draws nothing; the
        // background color below it still paints.
        let html = r#"<div id=tile style="width:80px;height:60px;background:#151515 url(tile.png)"></div>"#;
        let source = "https://example.test/tile.png";
        for (sizes, painted) in [
            (Default::default(), false),
            (
                [(source.to_string(), (u32::MAX, u32::MAX))]
                    .into_iter()
                    .collect(),
                false,
            ),
            ([(source.to_string(), (20, 10))].into_iter().collect(), true),
        ] {
            let (dom, layout) = render_fixture_with_images(html, &sizes);
            let node = dom.get_by_id("tile").unwrap();
            let image = layout.paint.primitives.iter().any(
                |command| matches!(command, DisplayCommand::Image { node: n, .. } if *n == node),
            );
            assert_eq!(image, painted, "{sizes:?}");
            assert!(layout.paint.primitives.iter().any(|command| matches!(
                command,
                DisplayCommand::Fill { brush: PaintBrush::Solid(color), .. }
                    if *color == PaintColor::Rgba(0x15, 0x15, 0x15, 255)
            )));
            assert_eq!(
                layout
                    .paint
                    .image_requests
                    .iter()
                    .map(|request| request.source.as_str())
                    .collect::<Vec<_>>(),
                [source]
            );
        }
    }

    #[test]
    fn frame_style_urls_resolve_against_the_frame_document() {
        // CSS Values 4 #relative-urls and #style-resource-base-url: a url()
        // from a frame's <style> sheet or style attribute resolves against
        // the frame Document's base URL, not the embedding page's.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><iframe id=f style="width:300px;height:200px;border:0"></iframe>"#,
        );
        let frame = dom.get_by_id("f").unwrap();
        dom.install_frame_document(
            frame,
            r#"<style>#s { background-image: url(b.png) } #g::before { content: url(gen.png) }</style>
               <body style="margin:0">
               <div id=s style="width:20px;height:20px"></div>
               <div style="width:20px;height:20px;background-image:url('r.png')"></div>
               <div style="width:20px;height:20px;border:4px solid;border-image:url(e.png) 1"></div>
               <ul style="list-style-image:url(m.png)"><li>item</li></ul>
               <div id=g></div></body>"#,
            "https://frame.test/dir/page.html",
        )
        .unwrap();
        let page = Url::parse("https://page.test/").unwrap();
        // The static snapshot that re-renders a script-free page for a new
        // environment flattens the frame and keeps only baked declarations.
        let snapshot = Dom::parse_document(&dom.serialize(crate::dom::DOCUMENT));
        for dom in [&dom, &snapshot] {
            let layout = crate::layout2::lay_out_graphical(
                dom,
                &page,
                crate::layout2::Viewport::new(400., 300.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            let painted: Vec<_> = layout
                .paint
                .image_requests
                .iter()
                .map(|request| request.source.as_str())
                .collect();
            let collected = crate::http::collect_image_urls(
                dom,
                &page,
                crate::layout2::Viewport::new(400., 300.),
                1.0,
            )
            .all;
            // Paint requests images as it draws them (a generated image only
            // once it has natural dimensions); discovery fetches CSS images
            // other than border images ahead of paint.
            for image in ["b.png", "r.png", "e.png", "m.png"] {
                let expected = format!("https://frame.test/dir/{image}");
                assert!(
                    painted.contains(&expected.as_str()),
                    "{expected} in {painted:?}"
                );
            }
            for image in ["b.png", "r.png", "m.png", "gen.png"] {
                let expected = format!("https://frame.test/dir/{image}");
                assert!(collected.contains(&expected), "{expected} in {collected:?}");
            }
            assert!(
                !painted
                    .iter()
                    .any(|source| source.starts_with("https://page.test/")),
                "{painted:?}"
            );
        }
    }

    #[test]
    fn iframe_scrolling_reveals_offscreen_text_and_links() {
        // HTML #the-page and CSS Overflow 3 #scrolling: the child canvas
        // moves through the iframe's stationary content-box viewport.
        for overflow in ["", "overflow:auto", "overflow:scroll"] {
            let mut dom = Dom::parse_document(&format!(
                r#"<body style="margin:0;background:white"><iframe id=f
                    style="position:absolute;left:20px;top:20px;width:160px;height:80px;
                    border:3px solid blue;padding:7px;{overflow}"></iframe>"#
            ));
            let frame = dom.get_by_id("f").unwrap();
            dom.install_frame_document(
                frame,
                r#"<body style="margin:0;background:white;font:24px/32px monospace">
                    <div style="height:160px">start</div>
                    <div style="height:40px;background:lime"><a id=tail href="/end"
                        style="color:black">END OF POST</a></div></body>"#,
                "https://frame.test/",
            )
            .unwrap();
            let tail = dom.get_by_id("tail").unwrap();
            let mut layout = crate::layout2::lay_out_graphical(
                &dom,
                &Url::parse("https://page.test/").unwrap(),
                crate::layout2::Viewport::new(320., 240.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            let viewport = CssSize::new(320., 240.);
            let port = layout
                .paint
                .scroll_containers
                .iter()
                .find(|s| s.node == frame)
                .unwrap()
                .viewport;
            assert_eq!(port, CssRect::new(30., 30., 160., 80.));
            let offset = 120.;
            for scrolled in [false, true] {
                layout
                    .paint
                    .scroll_containers
                    .iter_mut()
                    .find(|s| s.node == frame)
                    .unwrap()
                    .offset
                    .y = if scrolled { offset } else { 0. };
                let pixels = crate::render::headless::render_paint(&layout.paint, viewport)
                    .unwrap()
                    .pixels;
                let at = |x: usize, y: usize| &pixels[(y * 320 + x) * 4..(y * 320 + x) * 4 + 3];
                let has_text = (72..101).any(|y| {
                    (31..185).any(|x| {
                        let rgb = at(x, y);
                        rgb.iter().all(|channel| *channel < 100)
                    })
                });
                assert_eq!(
                    has_text, scrolled,
                    "{overflow}: lower text, scrolled={scrolled}"
                );
                assert_eq!(
                    at(188, 105),
                    if scrolled {
                        [0, 255, 0]
                    } else {
                        [255, 255, 255]
                    }
                );
                assert_eq!(
                    at(195, 100),
                    [255, 255, 255],
                    "content must stay inside the frame padding"
                );
                assert_eq!(
                    at(100, 125),
                    [255, 255, 255],
                    "content must stay above the frame bottom"
                );
                let hits = crate::render::page_element_hits_at(
                    &layout.paint,
                    viewport,
                    CssPoint::default(),
                    CssPoint::new(40., 80.),
                );
                assert_eq!(
                    hits.iter().any(|hit| hit.node == tail),
                    scrolled,
                    "the newly visible link must be clickable"
                );
            }
        }
    }

    #[test]
    fn repeated_background_tiles_meet_without_seams() {
        // Tiles 10.5px tall: their shared edges fall mid-pixel unless
        // snapped, and the black background showed through each seam.
        let dom = Dom::parse_document(
            "<body style='margin:0'><div style='width:40px;height:60px;background:black \
             linear-gradient(lime,lime) 0 0/10.5px 10.5px'></div>",
        );
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &Url::parse("https://page.test/").unwrap(),
            crate::layout2::Viewport::new(60., 60.),
            &[],
            &Default::default(),
            &Default::default(),
        );
        let pixels = crate::render::headless::render_paint(&layout.paint, CssSize::new(60., 60.))
            .unwrap()
            .pixels;
        for y in 0..60 {
            for x in 0..40 {
                let pixel = &pixels[(y * 60 + x) * 4..][..3];
                assert_eq!(pixel, [0, 255, 0], "({x}, {y})");
            }
        }
    }

    #[test]
    fn the_color_scheme_sets_the_canvas_frames_and_control_surfaces() {
        // CSS Color Adjust 1 #color-scheme-effect: a dark root paints a dark
        // Canvas; an iframe whose document's scheme differs gets an opaque
        // canvas; controls take Field. Light text alone changes nothing.
        let pixels = |html: &str, frame: Option<&str>| {
            let mut dom = Dom::parse_document(html);
            if let Some(markup) = frame {
                let id = dom.get_by_id("f").unwrap();
                dom.install_frame_document(id, markup, "https://frame.test/")
                    .unwrap();
            }
            let layout = crate::layout2::lay_out_graphical(
                &dom,
                &Url::parse("https://page.test/").unwrap(),
                crate::layout2::Viewport::new(200., 120.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            crate::render::headless::render_paint(&layout.paint, CssSize::new(200., 120.))
                .unwrap()
                .pixels
        };
        let at = |pixels: &[u8], x: usize, y: usize| pixels[(y * 200 + x) * 4..][..3].to_vec();
        let dark = pixels(
            "<meta name=color-scheme content=dark><body style=margin:0><textarea              style='width:80px;height:40px;margin:50px 0 0 50px'></textarea>",
            None,
        );
        assert_eq!(at(&dark, 10, 10), [28, 27, 34], "dark Canvas");
        assert_eq!(at(&dark, 90, 70), [43, 42, 51], "dark Field");
        let light_text = pixels(
            "<body style='margin:0;color:#eee'><textarea              style='width:80px;height:40px;margin:50px 0 0 50px'></textarea>",
            None,
        );
        assert_eq!(at(&light_text, 10, 10), [255, 255, 255]);
        assert_eq!(at(&light_text, 90, 70), [255, 255, 255], "light Field");
        let framed = pixels(
            "<body style='margin:0;background:#fc0'><iframe id=f              style='border:0;width:100px;height:100px'></iframe>",
            Some("<meta name=color-scheme content=dark><body>"),
        );
        assert_eq!(at(&framed, 50, 50), [28, 27, 34], "opaque dark canvas");
        assert_eq!(at(&framed, 150, 50), [255, 204, 0]);
        let same = pixels(
            "<body style='margin:0;background:#fc0'><iframe id=f              style='border:0;width:100px;height:100px'></iframe>",
            Some("<body>"),
        );
        assert_eq!(at(&same, 50, 50), [255, 204, 0], "transparent canvas");
    }

    #[test]
    fn a_fixed_frame_canvas_background_uses_the_frame_viewport() {
        // CSS Backgrounds 3 #background-attachment: `fixed` is relative to
        // the viewport of the element's document, here the iframe's, and the
        // frame's canvas paints only within it.
        let mut dom = Dom::parse_document(
            r#"<body style="margin:0"><iframe id=f style="position:absolute;left:30px;top:0;
                width:100px;height:100px;border:0"></iframe></body>"#,
        );
        let frame = dom.get_by_id("f").unwrap();
        dom.install_frame_document(
            frame,
            r#"<body style="margin:0;background:black linear-gradient(to right,red 50%,lime 50%)
                0 0/20px 20px fixed"><div style="height:300px"></div></body>"#,
            "https://frame.test/",
        )
        .unwrap();
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &Url::parse("https://page.test/").unwrap(),
            crate::layout2::Viewport::new(200., 120.),
            &[],
            &Default::default(),
            &Default::default(),
        );
        let pixels = crate::render::headless::render_paint(&layout.paint, CssSize::new(200., 120.))
            .unwrap()
            .pixels;
        let at = |x: usize, y: usize| &pixels[(y * 200 + x) * 4..(y * 200 + x) * 4 + 3];
        assert_eq!(at(35, 50), [255, 0, 0], "tiles start at the frame's edge");
        assert_eq!(at(45, 50), [0, 255, 0]);
        assert_eq!(at(170, 50), [255, 255, 255], "nothing outside the frame");
        assert_eq!(at(10, 50), [255, 255, 255]);
    }

    #[test]
    fn blended_canvas_backgrounds_are_isolated_from_the_canvas_surface() {
        // CSS Compositing 1 #background-blend-mode and #isolatedgroups: the
        // canvas background's layers blend inside an isolated group whose
        // initial backdrop is transparent black, not with the white canvas
        // surface (CSS Backgrounds 3 §2.11.1) or an embedding document.
        // Overlay of 50% black over rgb(51,102,153) gives rgb(25,51,102) in
        // Gecko and Blink; blending with white would leave white.
        const LAYERS: &str = "background-image:linear-gradient(rgba(0,0,0,.5),rgba(0,0,0,.5)),\
            linear-gradient(rgb(51,102,153),rgb(51,102,153));background-blend-mode:overlay";
        let pixels = |html: &str, frame: Option<&str>| {
            let mut dom = Dom::parse_document(html);
            if let Some(markup) = frame {
                let id = dom.get_by_id("f").unwrap();
                dom.install_frame_document(id, markup, "https://frame.test/")
                    .unwrap();
            }
            let layout = crate::layout2::lay_out_graphical(
                &dom,
                &Url::parse("https://page.test/").unwrap(),
                crate::layout2::Viewport::new(200., 120.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            crate::render::headless::render_paint(&layout.paint, CssSize::new(200., 120.))
                .unwrap()
                .pixels
        };
        let at = |pixels: &[u8], x: usize, y: usize| pixels[(y * 200 + x) * 4..][..3].to_vec();
        let close = |actual: Vec<u8>, expected: [u8; 3], what: &str| {
            assert!(
                actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 1),
                "{what}: {actual:?}"
            );
        };
        for (html, what) in [
            (
                format!("<!doctype html><body style='height:20px;{LAYERS}'>"),
                "propagated body",
            ),
            (
                format!("<!doctype html><html style='{LAYERS}'><body style='height:20px'>"),
                "root",
            ),
            (
                format!(
                    "<!doctype html><body style='height:20px;{LAYERS};background-attachment:fixed'>"
                ),
                "fixed layers",
            ),
        ] {
            let page = pixels(&html, None);
            close(at(&page, 50, 10), [25, 51, 102], what);
            close(at(&page, 50, 100), [25, 51, 102], what);
        }
        // A translucent color is the group's bottom layer and is painted once.
        let translucent = pixels(
            "<!doctype html><body style='background:rgba(0,0,255,.5) \
             linear-gradient(red,red);background-blend-mode:multiply'>",
            None,
        );
        close(at(&translucent, 50, 50), [128, 0, 0], "translucent color");
        // A fixed bottom layer over a translucent color: the scrolling top
        // layer still blends with it (Chromium gives rgb(12,141,38)).
        let mixed = pixels(
            &format!(
                "<!doctype html><body style='height:20px;background-color:rgba(0,255,0,.5);\
                 {LAYERS};background-attachment:scroll,fixed'>"
            ),
            None,
        );
        close(
            at(&mixed, 50, 50),
            [13, 140, 38],
            "fixed and scrolling layers",
        );
        let framed = pixels(
            "<body style='margin:0;background:white'><iframe id=f \
             style='border:0;width:100px;height:100px'></iframe>",
            Some(&format!("<body style='{LAYERS}'>")),
        );
        close(at(&framed, 50, 50), [25, 51, 102], "iframe canvas");
        close(at(&framed, 150, 50), [255, 255, 255], "outside the frame");
    }

    #[test]
    fn iframe_scrolling_preserves_ancestor_and_descendant_clips() {
        // CSS Overflow 3 #scrolling and CSS2 #overflow: the outer clip stays
        // stationary; a clipped block within the child canvas moves with it.
        for overflow in ["hidden", "clip", "clip visible"] {
            let mut dom = Dom::parse_document(
                r#"<body style="margin:0;background:white"><div style="position:absolute;
                    left:20px;top:20px;width:140px;height:80px;overflow:hidden">
                    <iframe id=f style="width:160px;height:80px;border:0"></iframe>
                    </div></body>"#,
            );
            let frame = dom.get_by_id("f").unwrap();
            dom.install_frame_document(
                frame,
                &format!(
                    r#"<body style="margin:0;background:white">
                    <div style="height:160px"></div>
                    <div style="width:100px;height:20px;overflow:{overflow}">
                        <div style="width:200px;height:60px;background:lime;
                            transform:translateY(0)"></div></div>
                    <div style="height:100px"></div></body>"#
                ),
                "https://frame.test/",
            )
            .unwrap();
            let mut layout = crate::layout2::lay_out_graphical(
                &dom,
                &Url::parse("https://page.test/").unwrap(),
                crate::layout2::Viewport::new(320., 240.),
                &[],
                &Default::default(),
                &Default::default(),
            );
            layout
                .paint
                .scroll_containers
                .iter_mut()
                .find(|s| s.node == frame)
                .unwrap()
                .offset
                .y = 140.;
            let pixels =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(320., 240.))
                    .unwrap()
                    .pixels;
            let at = |x: usize, y: usize| &pixels[(y * 320 + x) * 4..(y * 320 + x) * 4 + 3];
            assert_eq!(
                at(40, 45),
                [0, 255, 0],
                "{overflow}: scrolled block must paint"
            );
            assert_eq!(
                at(130, 45),
                [255, 255, 255],
                "{overflow}: descendant width clip"
            );
            assert_eq!(
                at(40, 70),
                if overflow == "clip visible" {
                    [0, 255, 0]
                } else {
                    [255, 255, 255]
                },
                "{overflow}: descendant height clip"
            );
            assert_eq!(
                at(165, 45),
                [255, 255, 255],
                "{overflow}: outer clip must remain stationary"
            );
        }
    }

    #[test]
    fn fixed_canvas_backgrounds_cover_the_viewport_and_stay_pinned() {
        // CSS Backgrounds 3 #background-attachment: a fixed layer is
        // positioned against the viewport, not the 2000px-tall root box, and
        // the canvas paints it in the viewport-pinned underlay.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;background:linear-gradient(red 50%,blue 50%) fixed}\
             div{height:2000px}</style><div></div>",
        );
        assert!(!layout.paint.fixed_under_primitives.is_empty());
        assert!(!layout.paint.primitives.iter().any(|command| matches!(
            command,
            DisplayCommand::Fill {
                brush: PaintBrush::LinearGradient { .. },
                ..
            }
        )));
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let at = |y: usize| {
            let i = (y * 800 + 400) * 4;
            [frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]]
        };
        assert_eq!(at(290), [255, 0, 0]);
        assert_eq!(at(310), [0, 0, 255]);
    }

    #[test]
    fn background_and_shadow_lengths_resolve_relative_units_and_edge_offsets() {
        // Computed values keep authored units: `em`, `vw` and calc() lengths
        // and the four-value <bg-position> must resolve at paint time.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;font-size:10px}div{width:200px;height:100px;\
             background:#ddd linear-gradient(red,red) no-repeat}\
             #a{background-size:5em 3em}\
             #b{background-size:2em 2.5vw;background-position:right 1em bottom 2em}\
             #c{background-size:0 0;box-shadow:1em 1em 0 blue}</style>\
             <div id=a></div><div id=b></div><div id=c></div>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let at = |x: usize, y: usize| {
            let i = (y * 800 + x) * 4;
            [frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]]
        };
        let (red, grey, blue) = ([255, 0, 0], [221, 221, 221], [0, 0, 255]);
        assert_eq!(at(45, 25), red, "5em x 3em tile");
        assert_eq!(at(55, 25), grey);
        assert_eq!(at(45, 35), grey);
        // A 20px tile (2em; 2.5vw of the 800px viewport), 10px from the
        // right and 20px from the bottom of the second box (y = 100..200).
        assert_eq!(at(180, 170), red, "edge-offset tile");
        assert_eq!(at(165, 170), grey);
        assert_eq!(at(180, 182), grey);
        assert_eq!(at(205, 295), blue, "1em shadow offset");
    }

    #[test]
    fn gradient_backgrounds_are_sized_positioned_and_tiled_like_images() {
        // CSS Backgrounds 3 §§2.4-2.6 and #background-size: a gradient has no
        // natural size, so `auto` is the positioning (padding) box; the tile is
        // then positioned, repeated into the border area, and clipped.
        let (_, layout) = render_fixture(
            "<style>body{margin:0}div{width:100px;height:40px}\
             #stripes{background:linear-gradient(red 50%,blue 50%) 0 0/100% 10px}\
             #border{border:10px solid transparent;background:linear-gradient(90deg,red 50%,blue 50%)}\
             #single{background:linear-gradient(red,red) 50% 50%/20px 20px no-repeat,lime}</style>\
             <div id=stripes></div><div id=border></div><div id=single></div>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let at = |x: usize, y: usize| {
            let i = (y * 800 + x) * 4;
            [frame.pixels[i], frame.pixels[i + 1], frame.pixels[i + 2]]
        };
        for (y, expected) in [
            (2, [255, 0, 0]),
            (7, [0, 0, 255]),
            (12, [255, 0, 0]),
            (37, [0, 0, 255]),
        ] {
            assert_eq!(at(50, y), expected, "stripes at y={y}");
        }
        // The border-box div starts at y=40; its padding box spans x=10..110.
        assert_eq!(
            at(15, 60),
            [255, 0, 0],
            "first tile starts at the padding edge"
        );
        assert_eq!(at(105, 60), [0, 0, 255]);
        assert_eq!(
            at(5, 60),
            [0, 0, 255],
            "the tile repeats into the left border"
        );
        assert_eq!(at(115, 60), [255, 0, 0], "and into the right border");
        // A positioned no-repeat tile over the background color.
        assert_eq!(at(50, 120), [255, 0, 0]);
        assert_eq!(at(10, 120), [0, 255, 0]);
    }

    #[test]
    fn radial_gradients_take_their_shape_size_and_position() {
        // CSS Images 3 #radial-gradient-syntax, in a 150x100 box.
        let rect = CssRect::new(0., 0., 150., 100.);
        let radial = |value: &str| match parse_gradient(value, rect, LengthBasis::fixed()) {
            Some(PaintBrush::RadialGradient {
                center,
                start_radius,
                radius,
                aspect,
                ..
            }) => (center.x, center.y, start_radius, radius, aspect),
            other => panic!("{value}: {other:?}"),
        };
        type Radial = (f32, f32, f32, f32, f32);
        let close = |(a, b): (Radial, Radial)| {
            let (a, b) = ([a.0, a.1, a.2, a.3, a.4], [b.0, b.1, b.2, b.3, b.4]);
            assert!(
                a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1.0e-3),
                "{a:?} != {b:?}"
            );
        };
        let sqrt2 = std::f32::consts::SQRT_2;
        // Default: an ellipse at the center reaching the farthest corner.
        close((
            radial("radial-gradient(red,blue)"),
            (75., 50., 0., 75. * sqrt2, 50. / 75.),
        ));
        close((
            radial("radial-gradient(circle at top left,red,blue)"),
            (0., 0., 0., 150f32.hypot(100.), 1.),
        ));
        close((
            radial("radial-gradient(ellipse closest-side,red,blue)"),
            (75., 50., 0., 75., 50. / 75.),
        ));
        close((
            radial("radial-gradient(circle closest-side at 30% 40%,red,blue)"),
            (45., 40., 0., 40., 1.),
        ));
        close((
            radial("radial-gradient(40px 20px at 75px 50px,red,blue)"),
            (75., 50., 0., 40., 0.5),
        ));
        assert!(
            parse_gradient(
                "radial-gradient(circle 10%,red,blue)",
                rect,
                LengthBasis::fixed()
            )
            .is_none()
        );
        // CSS Images 3 #repeating-gradients: the stops' span is the period.
        close((
            radial("repeating-radial-gradient(circle at 0 0,blue 5px,white 10px,blue 15px)"),
            (0., 0., 5., 15., 1.),
        ));
        let Some(PaintBrush::LinearGradient {
            start, end, repeat, ..
        }) = parse_gradient(
            "repeating-linear-gradient(0deg,black 0 2px,yellow 2px 4px)",
            rect,
            LengthBasis::fixed(),
        )
        else {
            panic!()
        };
        assert!(repeat);
        assert_eq!((start.y - end.y, start.x, end.x), (4., 75., 75.));
        // A zero period paints the stops' average color.
        assert_eq!(
            parse_gradient(
                "repeating-linear-gradient(red 5px,blue 5px)",
                rect,
                LengthBasis::fixed()
            ),
            Some(PaintBrush::Solid(PaintColor::Rgba(128, 0, 128, 255)))
        );
    }

    #[test]
    fn gradient_interpolation_syntax_preserves_color_space_hue_and_alpha() {
        use color::{ColorSpaceTag as Space, HueDirection as Hue};
        let rect = CssRect::new(0., 0., 100., 100.);
        for header in ["to bottom in oklab", "in oklab to bottom", "in oklab"] {
            let brush = parse_gradient(&format!("linear-gradient({header},rgba(22,22,22,.9) 0%,rgba(22,22,22,.5) 40%,transparent 97%)"),rect,LengthBasis::fixed()).unwrap();
            let PaintBrush::LinearGradient {
                interpolation,
                start,
                end,
                stops,
                ..
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
            LengthBasis::fixed(),
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
                parse_gradient(
                    &format!("linear-gradient({invalid},red,blue)"),
                    rect,
                    LengthBasis::fixed()
                )
                .is_none(),
                "{invalid}"
            );
        }
        // CSS Color 4 #interpolation: legacy sRGB stops default to sRGB.
        for (method, stops, expected) in [
            ("in srgb", "black,white", 128u8),
            ("in srgb-linear", "black,white", 188),
            ("in oklab", "black,white", 99),
            ("", "black,white", 128),
            ("", "#000,rgb(255 255 255)", 128),
            ("", "hsl(0 0% 0%),hwb(0 100% 0%)", 128),
            ("", "black,color(srgb 1 1 1)", 99),
            ("", "oklab(0 0 0),oklab(1 0 0)", 99),
        ] {
            let (_, layout) = render_fixture(&format!(
                "<style>body{{margin:0}}div{{width:100px;height:20px;background-image:linear-gradient(to right {method},{stops})}}</style><div></div>"
            ));
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            let actual = frame.pixels[(10 * 800 + 50) * 4];
            assert!(
                actual.abs_diff(expected) <= 3,
                "{method} {stops}: {actual} != {expected}"
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
    fn conic_gradient_positions_take_math_functions() {
        // CSS Images 4 #conic-gradient-syntax: stop positions and `from` are
        // <angle-percentage>/<angle> values, math functions included; a
        // percentage there is of a full turn. Expected pixels from Blink.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;background:white} div{width:100px;height:100px}</style>\
             <div style='--p:30;background:conic-gradient(red calc(var(--p)*1%), #ddd 0)'></div>\
             <div style='background:conic-gradient(from calc(45deg + 45deg), red 25%, blue 0)'></div>\
             <div style='background:conic-gradient(red min(25%, 90deg), blue 0)'></div>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let pixel = |x: usize, y: usize| frame.pixels[(y * 800 + x) * 4..][..3].to_vec();
        for (x, y, expected) in [
            (70, 30, [255, 0, 0]),
            (30, 70, [221, 221, 221]),
            (70, 130, [0, 0, 255]),
            (80, 180, [255, 0, 0]),
            (70, 230, [255, 0, 0]),
            (30, 270, [0, 0, 255]),
        ] {
            assert_eq!(pixel(x, y), expected, "({x}, {y})");
        }
    }

    #[test]
    fn conic_gradients_sweep_clockwise_from_the_top() {
        // CSS Images 4 #conic-gradients: stops run clockwise around the
        // center from 0deg, which points up, after turning by `from`; `at`
        // moves the center, and a repeating gradient tiles its first to last
        // stop around the turn. Expected pixels were measured in Blink.
        let (_, layout) = render_fixture(
            "<style>body{margin:0;background:white} div{width:100px;height:100px}</style>\
             <div style='background:conic-gradient(red 25%, lime 0 50%, blue 0 75%, yellow 0)'></div>\
             <div style='background:conic-gradient(from 90deg, red 25%, lime 0)'></div>\
             <div style='background:conic-gradient(at 25% 25%, red 25%, lime 0)'></div>\
             <div style='background:repeating-conic-gradient(from 45deg, #421d2c 0% 25%, \
             #2d1b1b 0% 50%) 0 0/50px 50px'></div>\
             <div style='background:conic-gradient(red, blue)'></div>",
        );
        let frame =
            crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.)).unwrap();
        let pixel = |x: usize, y: usize| frame.pixels[(y * 800 + x) * 4..][..3].to_vec();
        let (red, lime, blue) = ([255, 0, 0], [0, 255, 0], [0, 0, 255]);
        for (x, y, expected) in [
            (75, 25, red),
            (75, 75, lime),
            (25, 75, blue),
            (25, 25, [255, 255, 0]),
            (75, 175, red),
            (75, 125, lime),
            (25, 175, lime),
            (75, 210, red),
            (75, 275, lime),
            (10, 210, lime),
            (45, 325, [66, 29, 44]),
            (5, 325, [66, 29, 44]),
            (95, 325, [66, 29, 44]),
            (25, 305, [45, 27, 27]),
            (25, 345, [45, 27, 27]),
            (75, 305, [45, 27, 27]),
        ] {
            assert_eq!(pixel(x, y), expected, "({x}, {y})");
        }
        // Halfway round, straight down, sRGB interpolation is mid purple.
        let down = pixel(50, 495);
        assert!(
            down.iter()
                .zip([128, 0, 128])
                .all(|(a, e)| a.abs_diff(e) <= 3),
            "{down:?}"
        );
    }

    #[test]
    fn text_clipped_inline_backgrounds_paint_their_glyphs() {
        // CSS Backgrounds 4 #valdef-background-clip-text: the background is
        // clipped to the text of the element and its in-flow descendants,
        // inline boxes included. Gecko and Blink fill an inline logo's
        // glyphs with its background; following text is not in the mask.
        for background in ["background:red", "background:linear-gradient(red,red)"] {
            let (_, layout) = render_fixture(&format!(
                "<style>body{{margin:0;background:white}} p{{margin:0;font:40px/50px monospace}}\
                 #t{{{background};background-clip:text;color:transparent}} i{{color:transparent}}\
                 </style><p><span id=t>HH<b>HH</b></span><i>HHHH</i></p>"
            ));
            let frame =
                crate::render::headless::render_paint(&layout.paint, CssSize::new(800., 600.))
                    .unwrap();
            let red = |columns: std::ops::Range<usize>| {
                (0..50)
                    .flat_map(|y| columns.clone().map(move |x| (x, y)))
                    .filter(|&(x, y)| {
                        let pixel = &frame.pixels[(y * 800 + x) * 4..][..3];
                        pixel[0] > 200 && pixel[1] < 60 && pixel[2] < 60
                    })
                    .count()
            };
            let own = red(0..48);
            assert!(own > 100, "{background}: the box's own glyphs ({own})");
            assert!(
                own < 48 * 50 / 2,
                "{background}: glyphs, not a rectangle ({own})"
            );
            assert!(red(48..96) > 100, "{background}: a descendant's glyphs");
            assert_eq!(red(100..200), 0, "{background}: text after the box");
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
        // Fit-content around the 13.333px UA button text, not the 800px band.
        let width = layout.boxes[&dom.get_by_id("auto").unwrap()].width;
        assert!(
            width > 60. && width < 180.,
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
