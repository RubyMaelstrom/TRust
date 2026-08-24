//! Renderer-neutral graphical paint extraction from canonical fragments.
//!
//! CSS 2.2 Appendix E remains the ordering authority. This module turns that
//! traversal into a stateful TRust display list; it does not contain Vello,
//! framebuffer, DPI, winit, Ratatui, or terminal-cell types.

use std::collections::HashSet;
use std::f32::consts::{FRAC_PI_2, PI};
use std::str::FromStr as _;

use url::Url;

use crate::core::{CssPoint, CssSize};
use crate::dom::{Dom, NodeId};
use crate::render::{
    Affine2d, BlendMode, CompositingLayer, CornerRadii, CssRect, DecorationStyle, DisplayCommand,
    GradientStop, HitRegion, ImageFit, ImageHandle, ImageRequest, ImageSampling, LineCap,
    PagePaint, PaintBrush, PaintColor, PaintLine, PaintShape, PathElement, ScrollContainer,
    StickyConstraint, StrokeStyle, TextDecorationPaint, TopLayerEntry,
};

use super::ImageSizes;
use super::NO_NODE;
use super::Units;
use super::flow::{Clip, Frag, FragKind, TopFrag};
use super::style::{Outline, OutlineStyle, outline_of};

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
    image_handles: HashSet<ImageHandle>,
    scroll_containers: Vec<ScrollContainer>,
    sticky_constraints: Vec<StickyConstraint>,
    patch_boundaries: Vec<super::GraphicalPatchBoundary>,
    boundaries: Vec<super::GraphicalBoundary>,
    /// Absolute overflow clips already active in the display list. A clip
    /// inherited from outside a transformed stacking context must be pushed
    /// before that transform and retained for the context's descendants.
    hard_clips: Vec<CssRect>,
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
            image_handles: HashSet::new(),
            scroll_containers: Vec::new(),
            sticky_constraints: Vec::new(),
            patch_boundaries: Vec::new(),
            boundaries: Vec::new(),
            hard_clips: Vec::new(),
        };
        this.collect_scroll_containers(root);
        this.collect_patch_boundaries(root);
        this.collect_sticky(root);
        for top in top_layer {
            this.collect_scroll_containers(&top.fragment);
            this.collect_sticky(&top.fragment);
        }
        this
    }

    fn image(&mut self, source: String) -> ImageHandle {
        let handle = ImageHandle::for_source(&source);
        if self.image_handles.insert(handle) {
            self.image_requests.push(ImageRequest { handle, source });
        }
        handle
    }

    fn effective_clip(&self, node: NodeId, hard: Option<Clip>) -> Option<CssRect> {
        let _ = node;
        hard.map(|clip| self.clip_rect(clip))
    }

    fn ancestor_clip(&self, node: NodeId, hard: Option<Clip>) -> Option<CssRect> {
        let _ = node;
        hard.map(|clip| self.clip_rect(clip))
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
        if node == NO_NODE {
            return 0;
        }
        let mut chain = Vec::new();
        // CSS Overflow 3 §2.3 clips the contents of a scroll container to
        // its scrollport.  A shadow tree is attached to the light tree through
        // its host (DOM §4.2.2), so paint ancestry must cross that boundary:
        // otherwise a custom element's host box is clipped but the image/text
        // painted by its shadow tree escapes the same scrollport.
        let mut current = self.dom.parent_flat(node);
        while let Some(id) = current {
            if !matches!(self.dom.tag_name(id), Some("html" | "body"))
                && let Some(container) = self
                    .scroll_containers
                    .iter()
                    .find(|container| container.node == id)
                    .cloned()
            {
                chain.push(container);
            }
            current = self.dom.parent_flat(id);
        }
        chain.reverse();
        for container in &chain {
            self.commands
                .push(DisplayCommand::PushClip(PaintShape::Rect(
                    container.viewport,
                )));
            self.commands
                .push(DisplayCommand::PushTransform(Affine2d::translate(
                    -container.offset.x,
                    -container.offset.y,
                )));
        }
        chain.len()
    }

    fn pop_scroll_ancestors(&mut self, count: usize) {
        for _ in 0..count {
            self.commands.push(DisplayCommand::PopTransform);
            self.commands.push(DisplayCommand::PopClip);
        }
    }

    fn collect_scroll_containers(&mut self, fragment: &Frag<'_>) {
        if fragment.node != NO_NODE
            && !matches!(self.dom.tag_name(fragment.node), Some("html" | "body"))
            && (self.dom.is_scroll_container(fragment.node)
                || self.dom.is_hscroll_container(fragment.node))
        {
            let viewport = padding_box(fragment);
            let (right, bottom) = subtree_extent(fragment);
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
                content: CssSize::new(
                    (right - viewport.x).max(viewport.width),
                    (bottom - viewport.y).max(viewport.height),
                ),
                offset: CssPoint::new(
                    self.dom.scroll_metric(fragment.node, 1).unwrap_or(0.0) as f32,
                    self.dom.scroll_metric(fragment.node, 0).unwrap_or(0.0) as f32,
                ),
                horizontal: self.dom.is_hscroll_container(fragment.node),
                vertical: self.dom.is_scroll_container(fragment.node),
            });
        }
        for child in &fragment.children {
            self.collect_scroll_containers(child);
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

    fn collect_sticky(&mut self, fragment: &Frag<'_>) {
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
            self.sticky_constraints.push(StickyConstraint {
                node: fragment.node,
                rect: CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
                container,
                insets: ["top", "right", "bottom", "left"].map(|side| {
                    self.dom
                        .computed_value_resolved(fragment.node, side)
                        .as_deref()
                        .and_then(px)
                }),
            });
        }
        for child in &fragment.children {
            self.collect_sticky(child);
        }
    }
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
        paint_background_images_for_node(
            root,
            style_node,
            PaintShape::Rect(canvas),
            &mut builder,
            Some(canvas),
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
    let paint = PagePaint {
        width: (root.x + root.w).max(0.0),
        height: root.max_bottom().max(flow_bottom).max(0.0),
        background: root_background,
        lines: builder.lines,
        primitives,
        fixed_under_primitives,
        fixed_primitives,
        fixed_interleaved: true,
        top_layer: top_layer_entries,
        image_requests: builder.image_requests,
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
    if transformed {
        builder.commands.push(DisplayCommand::PopTransform);
    }
    if context_clip {
        builder.pop_hard_clip();
    }
    if sticky.is_some() {
        builder.commands.push(DisplayCommand::EndSticky);
    }
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
        if matches!(child.kind, FragKind::Block) {
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
        if !matches!(child.kind, FragKind::Block) {
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
    if fragment.node != NO_NODE && builder.dom.visibility_hidden(fragment.node) {
        return;
    }
    let scroll_depth = builder.push_scroll_ancestors(fragment.node);
    let fragment_clip = builder.ancestor_clip(fragment.node, fragment.clip);
    let pushed_fragment_clip = fragment_clip.is_some_and(|clip| builder.push_hard_clip(clip));
    let rect = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    if fragment.node != NO_NODE && fragment.w > 0.0 && fragment.h > 0.0 {
        let radii = border_radii(builder.dom, fragment.node, rect);
        let shape = rounded_shape(rect, radii);
        paint_box_shadows(builder.dom, fragment.node, &shape, builder);
        let is_root = builder.dom.document_element() == Some(fragment.node);
        let is_canvas_body = builder
            .dom
            .document_element()
            .is_some_and(|root| canvas_background_node(builder.dom, root) == fragment.node);
        if !is_root && !is_canvas_body {
            paint_native_control_surface(fragment, radii, builder);
            if let Some(color) = background_color(builder.dom, fragment.node)
                && !color.is_transparent()
            {
                builder.commands.push(DisplayCommand::Fill {
                    shape: shape.clone(),
                    brush: PaintBrush::Solid(color),
                });
            }
            paint_background_images(fragment, shape.clone(), builder, None);
        }
        paint_borders(fragment, radii, builder);
        if builder.dom.point_hit_testable(fragment.node) {
            builder.commands.push(DisplayCommand::HitRegion(HitRegion {
                rect,
                node: fragment.node,
                actor: interaction_actor(builder.dom, fragment.node),
                link: None,
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
            if if style_node == NO_NODE {
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
            let piece_scroll_depth = if fragment.node == NO_NODE {
                builder.push_scroll_ancestors(node)
            } else {
                0
            };
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
            let mut clip = builder.effective_clip(node, fragment.clip);
            if piece_rect.is_some() {
                // A control's label is clipped to its content paint rectangle,
                // not merely to the outer border box. This is the same box
                // used to place the glyphs, so authored padding cannot become
                // a second nested control surface or an overflow escape hatch.
                let label_rect = CssRect::new(
                    fragment.x + piece.x + piece.paint_x,
                    fragment.y + piece.y + piece.paint_y,
                    piece.paint_width,
                    piece.paint_height,
                );
                clip = intersect_css_rects(clip, label_rect);
            }
            if let Some(shaped) = &piece.shaped {
                let origin = CssPoint::new(
                    fragment.x + piece.x + piece.paint_x,
                    fragment.y + piece.y + piece.paint_y,
                );
                let color = text_color(builder.dom, style_node, piece.item.link.is_some());
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
                builder.commands.push(DisplayCommand::GlyphRun {
                    origin,
                    shaped: shaped.clone(),
                    color,
                    decoration,
                    clip,
                    node,
                    link: piece.item.link.clone(),
                });
                if style_node == NO_NODE || builder.dom.point_hit_testable(style_node) {
                    builder.commands.push(DisplayCommand::HitRegion(HitRegion {
                        rect: CssRect::new(origin.x, origin.y, shaped.advance, shaped.line_height),
                        node,
                        actor: interaction_actor(builder.dom, node),
                        link: piece.item.link.clone(),
                    }));
                }
            } else if let Some(source) = piece
                .item
                .graphical_image
                .as_ref()
                .or(piece.item.image.as_ref())
            {
                let source = resolve_image_source(builder.base, source);
                let handle = builder.image(source);
                let rect = CssRect::new(
                    fragment.x + piece.x + piece.paint_x,
                    fragment.y + piece.y + piece.paint_y,
                    piece.paint_width,
                    piece.paint_height,
                );
                builder.commands.push(DisplayCommand::Image {
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
                });
                if style_node == NO_NODE || builder.dom.point_hit_testable(style_node) {
                    builder.commands.push(DisplayCommand::HitRegion(HitRegion {
                        rect,
                        node,
                        actor: interaction_actor(builder.dom, node),
                        link: piece.item.link.clone(),
                    }));
                }
            }
            builder.pop_scroll_ancestors(piece_scroll_depth);
        }
    }
    if fragment.node != NO_NODE && fragment.w > 0.0 && fragment.h > 0.0 {
        paint_outline(fragment, builder);
    }
    if pushed_fragment_clip {
        builder.pop_hard_clip();
    }
    builder.pop_scroll_ancestors(scroll_depth);
}

fn intersect_css_rects(existing: Option<CssRect>, rect: CssRect) -> Option<CssRect> {
    let Some(existing) = existing else {
        return Some(rect);
    };
    let x0 = existing.x.max(rect.x);
    let y0 = existing.y.max(rect.y);
    let x1 = (existing.x + existing.width).min(rect.x + rect.width);
    let y1 = (existing.y + existing.height).min(rect.y + rect.height);
    (x1 > x0 && y1 > y0).then(|| CssRect::new(x0, y0, x1 - x0, y1 - y0))
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
        paint: Default::default(),
        clip: parent.clip,
        kind: FragKind::Block,
        children: Vec::new(),
    };
    let radii = border_radii(builder.dom, node, rect);
    let shape = rounded_shape(rect, radii);
    paint_box_shadows(builder.dom, node, &shape, builder);
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
    if builder.dom.point_hit_testable(node) {
        builder.commands.push(DisplayCommand::HitRegion(HitRegion {
            rect,
            node,
            actor: interaction_actor(builder.dom, node),
            link,
        }));
    }
    paint_outline_box(
        builder,
        node,
        rect,
        outline_of(builder.dom, node, Units::of(builder.dom, node)),
    );
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
    let is_control = matches!(builder.dom.tag_name(node), Some("button" | "textarea"))
        || (builder.dom.tag_name(node) == Some("input")
            && !builder
                .dom
                .attr(node, "type")
                .is_some_and(|kind| kind.eq_ignore_ascii_case("hidden")))
        || builder.dom.is_contenteditable_host(node);
    if !is_control
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
    paint_outline_box(
        builder,
        fragment.node,
        CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
        fragment.paint.outline,
    );
}

fn paint_outline_box(
    builder: &mut Builder<'_, '_>,
    node: NodeId,
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
    let base_radii = border_radii(builder.dom, node, border_box);
    let radii = CornerRadii {
        corners: base_radii.corners.map(|(x, y)| (x + grow, y + grow)),
    };
    let color = builder
        .dom
        .computed_value_resolved(node, "outline-color")
        .and_then(|value| {
            if value.trim().eq_ignore_ascii_case("currentcolor") {
                Some(text_color(builder.dom, node, false))
            } else {
                PaintColor::parse_css(&value)
            }
        })
        .unwrap_or_else(|| text_color(builder.dom, node, false));
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
    if fragment.node == NO_NODE {
        return false;
    }
    let opacity = fragment.paint.opacity.clamp(0.0, 1.0);
    let blend = builder
        .dom
        .computed_value_resolved(fragment.node, "mix-blend-mode")
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
    if fragment.node == NO_NODE {
        return None;
    }
    let (matrix, layout_translation) = element_transform(
        builder.dom,
        fragment.node,
        fragment.w,
        fragment.h,
        fragment.x,
        fragment.y,
    )?;
    // Phase 2 retained translated fragment coordinates for terminal output.
    // Undo that already-applied translation inside the graphical transform so
    // the desktop path sees CSS's complete matrix exactly once.
    let corrected = matrix.then(Affine2d::translate(
        -layout_translation.x,
        -layout_translation.y,
    ));
    (!corrected.is_identity()).then_some(corrected)
}

fn paint_background_images(
    fragment: &Frag<'_>,
    shape: PaintShape,
    builder: &mut Builder<'_, '_>,
    canvas: Option<CssRect>,
) {
    paint_background_images_for_node(fragment, fragment.node, shape, builder, canvas);
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

fn paint_background_images_for_node(
    fragment: &Frag<'_>,
    style_node: NodeId,
    shape: PaintShape,
    builder: &mut Builder<'_, '_>,
    canvas: Option<CssRect>,
) {
    let Some(value) = builder
        .dom
        .computed_value_resolved(style_node, "background-image")
    else {
        return;
    };
    let border_box = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    let padding_box = padding_box_with_style(fragment);
    let content_box = content_box_with_style(builder.dom, fragment, padding_box);
    let clip_value = builder
        .dom
        .computed_value_resolved(style_node, "background-clip")
        .unwrap_or_else(|| "border-box".into());
    let origin_value = builder
        .dom
        .computed_value_resolved(style_node, "background-origin")
        .unwrap_or_else(|| "padding-box".into());
    let clip_layers = split_top_level(&clip_value, ',');
    let origin_layers = split_top_level(&origin_value, ',');
    let repeat_value = builder
        .dom
        .computed_value_resolved(style_node, "background-repeat")
        .unwrap_or_else(|| "repeat".into());
    let position_value = builder
        .dom
        .computed_value_resolved(style_node, "background-position")
        .unwrap_or_else(|| "0% 0%".into());
    let size_value = builder
        .dom
        .computed_value_resolved(style_node, "background-size")
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
        if let Some(brush) = parse_gradient(layer, border_box) {
            builder.commands.push(DisplayCommand::Fill {
                shape: shape.clone(),
                brush,
            });
        } else if let Some(url) = css_url(layer) {
            let source = resolve_image_source(builder.base, &url);
            let handle = builder.image(source.clone());
            let origin = layer_value(&origin_layers, index, "padding-box");
            let positioning = background_box(origin, border_box, padding_box, content_box);
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
                paint_spaced_background(
                    builder,
                    clip,
                    style_node,
                    handle,
                    positioning,
                    (tile_w, tile_h),
                    (start_x, start_y),
                    nx,
                    ny,
                );
                continue;
            }
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
                        node: style_node,
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
        }
    }
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
    let pad = ["top", "right", "bottom", "left"].map(|side| {
        dom.computed_value_resolved(fragment.node, &format!("padding-{side}"))
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

/// CSS Backgrounds and Borders §6: background first, then border. Uniform
/// rounded borders use one true stroked rounded path; non-uniform sides retain
/// each side's own color/style and CSS-pixel width.
fn paint_borders(fragment: &Frag<'_>, radii: CornerRadii, builder: &mut Builder<'_, '_>) {
    let node = fragment.node;
    let [top, right, bottom, left] = fragment.border;
    if [top, right, bottom, left].iter().all(|v| *v <= 0.0) {
        return;
    }
    let styles = ["top", "right", "bottom", "left"].map(|side| {
        builder
            .dom
            .computed_value_resolved(node, &format!("border-{side}-style"))
            .unwrap_or_else(|| "none".into())
    });
    let colors = ["top", "right", "bottom", "left"].map(|side| {
        border_color(builder.dom, node, side)
            .unwrap_or_else(|| text_color(builder.dom, node, false))
    });
    let rect = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
    let uniform = (top - right).abs() < 0.01
        && (top - bottom).abs() < 0.01
        && (top - left).abs() < 0.01
        && styles.iter().all(|s| s == &styles[0])
        && colors.iter().all(|c| *c == colors[0]);
    if uniform && top > 0.0 && styles[0] != "none" && styles[0] != "hidden" {
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
        if width <= 0.0 || matches!(styles[index].as_str(), "none" | "hidden") {
            continue;
        }
        builder.commands.push(DisplayCommand::Stroke {
            shape: PaintShape::Path(vec![PathElement::MoveTo(start), PathElement::LineTo(end)]),
            brush: PaintBrush::Solid(colors[index]),
            style: stroke_for_border(width, &styles[index]),
        });
    }
}

fn paint_box_shadows(dom: &Dom, node: NodeId, shape: &PaintShape, builder: &mut Builder<'_, '_>) {
    let Some(value) = dom.computed_value_resolved(node, "box-shadow") else {
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

fn stroke_for_border(width: f32, style: &str) -> StrokeStyle {
    let mut stroke = StrokeStyle::solid(width);
    match style {
        "dotted" => {
            stroke.dash = vec![0.0, width * 2.0];
            stroke.cap = LineCap::Round;
        }
        "dashed" => stroke.dash = vec![width * 3.0, width * 2.0],
        // Double/groove/ridge/inset/outset remain isolated approximations: the
        // retained style is real, but this phase emits a solid stroke until a
        // multi-band border painter lands.
        _ => {}
    }
    stroke
}

fn border_radii(dom: &Dom, node: NodeId, rect: CssRect) -> CornerRadii {
    let names = [
        "border-top-left-radius",
        "border-top-right-radius",
        "border-bottom-right-radius",
        "border-bottom-left-radius",
    ];
    let mut corners = [(0.0, 0.0); 4];
    for (index, name) in names.into_iter().enumerate() {
        let Some(value) = dom.computed_value_resolved(node, name) else {
            continue;
        };
        let parts = split_ws(&value);
        let x = radius(parts[0], rect.width).unwrap_or(0.0);
        let y = parts
            .get(1)
            .and_then(|v| radius(v, rect.height))
            .unwrap_or(x);
        corners[index] = (x.max(0.0), y.max(0.0));
    }
    // CSS Backgrounds §5.5: proportionally reduce overlapping radii.
    let sums = [
        (corners[0].0 + corners[1].0, rect.width),
        (corners[3].0 + corners[2].0, rect.width),
        (corners[0].1 + corners[3].1, rect.height),
        (corners[1].1 + corners[2].1, rect.height),
    ];
    let factor = sums
        .into_iter()
        .filter(|(sum, _)| *sum > 0.0)
        .map(|(sum, side)| side / sum)
        .fold(1.0f32, f32::min)
        .min(1.0);
    for corner in &mut corners {
        corner.0 *= factor;
        corner.1 *= factor;
    }
    CornerRadii { corners }
}

fn rounded_shape(rect: CssRect, radii: CornerRadii) -> PaintShape {
    if radii.corners.iter().all(|&(x, y)| x == 0.0 && y == 0.0) {
        PaintShape::Rect(rect)
    } else {
        PaintShape::RoundedRect { rect, radii }
    }
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
    if !radial {
        if let Some(parsed) = gradient_direction(parts[0]) {
            angle = parsed;
            parts.remove(0);
        }
    } else if PaintColor::parse_css(split_ws(parts[0]).first()?).is_none() {
        // Shape/size/position syntax is retained by the cascade; this first
        // implementation uses the standards default center/farthest-corner.
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
        })
    }
}

fn parse_stops(parts: &[&str]) -> Option<Vec<GradientStop>> {
    let mut stops = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let tokens = split_ws(part);
        let color = PaintColor::parse_css(tokens.first()?)?;
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
    if let Some(deg) = value.strip_suffix("deg") {
        return deg.trim().parse::<f32>().ok().map(f32::to_radians);
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
    node: NodeId,
    width: f32,
    height: f32,
    x: f32,
    y: f32,
) -> Option<(Affine2d, CssPoint)> {
    let transform = dom
        .computed_value_resolved(node, "transform")
        .unwrap_or_else(|| "none".into());
    let translate = dom
        .computed_value_resolved(node, "translate")
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
    let origin = dom
        .computed_value_resolved(node, "transform-origin")
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
    dom.computed_value_resolved(node, "background-color")
        .as_deref()
        .and_then(|value| resolve_color(dom, node, value))
}

fn text_color(dom: &Dom, node: NodeId, link: bool) -> PaintColor {
    if node != NO_NODE
        && let Some(color) = dom
            .computed_value_resolved(node, "color")
            .as_deref()
            .and_then(|value| resolve_color(dom, node, value))
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
    if value.trim().eq_ignore_ascii_case("currentcolor") {
        return dom
            .computed_value_resolved(node, "color")
            .filter(|color| !color.trim().eq_ignore_ascii_case("currentcolor"))
            .as_deref()
            .and_then(PaintColor::parse_css);
    }
    PaintColor::parse_css(value)
}

fn border_color(dom: &Dom, node: NodeId, side: &str) -> Option<PaintColor> {
    dom.computed_value_resolved(node, &format!("border-{side}-color"))
        .as_deref()
        .and_then(|value| resolve_color(dom, node, value))
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

fn subtree_extent(fragment: &Frag<'_>) -> (f32, f32) {
    fragment.children.iter().map(subtree_extent).fold(
        (fragment.x + fragment.w, fragment.y + fragment.h),
        |(right, bottom), (child_right, child_bottom)| {
            (right.max(child_right), bottom.max(child_bottom))
        },
    )
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

fn radius(value: &str, basis: f32) -> Option<f32> {
    value
        .trim()
        .strip_suffix('%')
        .and_then(|v| v.trim().parse::<f32>().ok())
        .map(|v| v * basis / 100.0)
        .or_else(|| px(value))
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
        parse_modern_rgb(value).or_else(|| parse_hsl(value))
    }

    pub fn is_transparent(self) -> bool {
        matches!(self, Self::Rgba(_, _, _, 0))
    }
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
