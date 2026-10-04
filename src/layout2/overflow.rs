//! CSS Overflow 3 #scrollable-overflow-calculation and #overflow-propagation.
//! Keep CSSOM scrolling areas and the desktop's scroll ranges on the same
//! fragment calculation; clipped child content belongs to its own scrollport.
use super::flow::{Frag, FragKind};
use super::{Dom, NO_NODE, NodeId, PxRect};
use std::collections::HashMap;

/// Computed overflow eligibility, independent of whether a frontend offers a
/// scrollbar. In particular, hidden is programmatically scrollable; clip is
/// not. CSS Overflow 3 #overflow-control (CSSWG snapshot 81c27f686901).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Overflow {
    #[default]
    Visible,
    Clip,
    Hidden,
    Auto,
    Scroll,
}

impl Overflow {
    pub fn scrollable(self) -> bool {
        matches!(self, Self::Hidden | Self::Auto | Self::Scroll)
    }

    pub fn user_scrollable(self) -> bool {
        matches!(self, Self::Auto | Self::Scroll)
    }

    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "clip" => Self::Clip,
            "hidden" => Self::Hidden,
            "auto" | "overlay" => Self::Auto,
            "scroll" => Self::Scroll,
            _ => Self::Visible,
        }
    }

    pub fn axes(cv: impl Fn(&str) -> Option<String>) -> [Self; 2] {
        Self::axes_from(cv("overflow"), cv("overflow-x"), cv("overflow-y"))
    }

    /// `axes` for a reader whose `overflow` value is the CSSOM serialization
    /// of its two longhands, as `computed_value_resolved` produces it: each
    /// longhand is read once instead of again through the shorthand.
    pub fn axes_from_longhands(x: Option<String>, y: Option<String>) -> [Self; 2] {
        let shorthand = (x.is_some() || y.is_some()).then(|| {
            let (x, y) = (
                x.as_deref().unwrap_or("visible"),
                y.as_deref().unwrap_or("visible"),
            );
            if x == y {
                x.to_string()
            } else {
                format!("{x} {y}")
            }
        });
        Self::axes_from(shorthand, x, y)
    }

    fn axes_from(
        shorthand: Option<String>,
        longhand_x: Option<String>,
        longhand_y: Option<String>,
    ) -> [Self; 2] {
        let shorthand = shorthand.unwrap_or_default();
        let mut tokens = shorthand.split_whitespace();
        let x = tokens.next().unwrap_or("visible");
        let y = tokens.next().unwrap_or(x);
        let mut axes = [
            Self::parse(longhand_x.as_deref().unwrap_or(x)),
            Self::parse(longhand_y.as_deref().unwrap_or(y)),
        ];
        let scrollable = axes.map(Self::scrollable);
        for axis in 0..2 {
            // The consulted draft supports single-axis scroll containers:
            // visible becomes auto, whereas clip remains non-scrollable.
            if scrollable[1 - axis] && axes[axis] == Self::Visible {
                axes[axis] = Self::Auto;
            }
        }
        axes
    }
}

pub(super) fn overflow_axes(dom: &Dom, node: NodeId) -> [String; 2] {
    if node == NO_NODE {
        return ["visible".into(), "visible".into()];
    }
    dom.overflow_axes(node).map(|v| {
        match v {
            Overflow::Visible => "visible",
            Overflow::Clip => "clip",
            Overflow::Hidden => "hidden",
            Overflow::Auto => "auto",
            Overflow::Scroll => "scroll",
        }
        .into()
    })
}

pub(super) fn viewport_overflow_source(dom: &Dom) -> Option<NodeId> {
    viewport_overflow_source_for(dom, dom.document_element()?)
}

pub(super) fn viewport_overflow_source_for(dom: &Dom, root: NodeId) -> Option<NodeId> {
    if !dom.is_valid(root) {
        return None;
    }
    if dom.computed_value_resolved(root, "display").as_deref() == Some("none") {
        return None;
    }
    let contained = |node| {
        dom.computed_value_resolved(node, "contain")
            .is_some_and(|v| v != "none")
    };
    if dom.tag_name(root) == Some("html")
        && !contained(root)
        && overflow_axes(dom, root) == ["visible", "visible"]
        && let Some(body) = dom
            .children(root)
            .into_iter()
            .find(|&node| dom.tag_name(node) == Some("body"))
        && !contained(body)
        && dom.computed_value_resolved(body, "display").as_deref() != Some("none")
    {
        return Some(body);
    }
    Some(root)
}

/// CSS Overflow 3 #overflow-propagation, including the HTML body fallback.
pub(super) fn viewport_overflow_disabled(dom: &Dom) -> [bool; 2] {
    viewport_overflow_source(dom).map_or([false; 2], |node| {
        overflow_axes(dom, node).map(|v| matches!(v.as_str(), "hidden" | "clip"))
    })
}

/// CSSOM View #scrolling-area and CSS Overflow 3 #scrolling: the origin is
/// block-start/inline-start, or main-start/cross-start for a flex container.
pub(crate) fn scroll_reverse(dom: &Dom, node: NodeId) -> [bool; 2] {
    let rtl = dom.computed_value_resolved(node, "direction").as_deref() == Some("rtl");
    let writing = dom
        .computed_value_resolved(node, "writing-mode")
        .unwrap_or_default();
    let vertical = writing.starts_with("vertical-") || writing.starts_with("sideways-");
    let mut reverse = if vertical {
        [writing.ends_with("-rl"), rtl ^ (writing == "sideways-lr")]
    } else {
        [rtl, false]
    };
    if matches!(
        dom.effective_display(node).as_deref(),
        Some("flex" | "inline-flex")
    ) {
        let direction = dom
            .computed_value_resolved(node, "flex-direction")
            .unwrap_or_default();
        let main = usize::from(vertical ^ direction.starts_with("column"));
        if direction.ends_with("-reverse") {
            reverse[main] = !reverse[main];
        }
        if dom.computed_value_resolved(node, "flex-wrap").as_deref() == Some("wrap-reverse") {
            reverse[1 - main] = !reverse[1 - main];
        }
    }
    reverse
}

#[derive(Default)]
pub(super) struct ScrollAreas {
    pub nodes: HashMap<NodeId, PxRect>,
    pub document: (f32, f32),
}

#[derive(Clone, Copy)]
struct Edge(f32, f32, f32, f32);

impl Edge {
    fn point(x: f32, y: f32) -> Self {
        Self(x, y, x, y)
    }
    fn rect(x: f32, y: f32, right: f32, bottom: f32) -> Self {
        Self(right, bottom, x, y)
    }
    fn union(&mut self, other: Self) {
        self.0 = self.0.max(other.0);
        self.1 = self.1.max(other.1);
        self.2 = self.2.min(other.2);
        self.3 = self.3.min(other.3);
    }
}

impl ScrollAreas {
    pub fn new(dom: &Dom, root: &Frag) -> Self {
        let mut result = Self::default();
        let mut escaping = Vec::new();
        let edge = result.walk(
            dom,
            root,
            viewport_overflow_source(dom),
            true,
            &mut escaping,
        );
        result.document = (edge.0, edge.1);
        result
    }

    pub fn extend(&mut self, dom: &Dom, root: &Frag) {
        self.walk(dom, root, None, true, &mut Vec::new());
    }

    fn walk(
        &mut self,
        dom: &Dom,
        f: &Frag,
        viewport_source: Option<NodeId>,
        root: bool,
        escaping: &mut Vec<(bool, Edge)>,
    ) -> Edge {
        if matches!(f.kind, FragKind::Fixed(_) | FragKind::Oof(..)) {
            return Edge::point(0., 0.);
        }
        if f.flow.hidden || f.flow.float_clip_end.is_some() {
            // CSS Overflow 4 #line-clamp-containers: these boxes retain
            // CSSOM geometry but contribute only ink overflow. An abspos
            // descendant whose containing block escapes the hidden subtree
            // can still contribute to that external containing block.
            for child in &f.children {
                self.walk(dom, child, viewport_source, false, escaping);
            }
            return Edge::point(0., 0.);
        }
        let frame = f.node != NO_NODE && matches!(dom.tag_name(f.node), Some("iframe" | "frame"));
        let [top, right, bottom, left] = f.border;
        let content = f.content_box();
        let (x, y, end) = if frame {
            (
                content.x,
                content.y,
                Edge::rect(
                    content.x,
                    content.y,
                    content.x + content.width,
                    content.y + content.height,
                ),
            )
        } else {
            (
                f.x + left,
                f.y + top,
                Edge::rect(f.x + left, f.y + top, f.x + f.w - right, f.y + f.h - bottom),
            )
        };
        let mut area = end;
        let mut flow_end = Edge::point(content.x, content.y);
        let mut pending = Vec::new();
        for child in &f.children {
            let edge = self.walk(dom, child, viewport_source, false, &mut pending);
            area.union(edge);
            if edge.0 != 0. || edge.1 != 0. {
                flow_end.union(Edge::point(child.x + child.w, child.y + child.h));
            }
        }
        // Preserve end padding after in-flow content at the final scroll
        // position, without adding it to an escaping positioned descendant.
        area.union(Edge::point(
            flow_end.0 + (end.0 - content.x - content.width).max(0.),
            flow_end.1 + (end.1 - content.y - content.height).max(0.),
        ));
        // CSSOM View #scrolling-area: positioned descendants whose containing
        // block is outside this box bypass it, including its overflow clip.
        // They join the area only at the appropriate containing block.
        for (fixed, edge) in pending {
            if root
                || frame
                || if fixed {
                    f.paint.cb_fixed
                } else {
                    f.paint.cb_abs
                }
            {
                area.union(edge);
            } else {
                escaping.push((fixed, edge));
            }
        }
        if f.node != NO_NODE {
            let reverse = scroll_reverse(dom, f.node);
            let (left, right) = if reverse[0] {
                (area.2.min(x), end.0)
            } else {
                (x, area.0)
            };
            let (top, bottom) = if reverse[1] {
                (area.3.min(y), end.1)
            } else {
                (y, area.1)
            };
            let rect = PxRect {
                left: left as f64,
                top: top as f64,
                width: (right - left).max(0.) as f64,
                height: (bottom - top).max(0.) as f64,
                css_width: None,
                css_height: None,
            };
            self.nodes
                .entry(f.node)
                .and_modify(|old| {
                    let end_x = (old.left + old.width).max(rect.left + rect.width);
                    let end_y = (old.top + old.height).max(rect.top + rect.height);
                    old.left = old.left.min(rect.left);
                    old.top = old.top.min(rect.top);
                    old.width = end_x - old.left;
                    old.height = end_y - old.top;
                })
                .or_insert(rect);
        }
        if !root && Some(f.node) != viewport_source {
            let axes = overflow_axes(dom, f.node);
            let paint_containment = f.node != NO_NODE
                && dom
                    .computed_value_resolved(f.node, "contain")
                    .is_some_and(|v| {
                        v.split_whitespace()
                            .any(|v| matches!(v, "paint" | "content" | "strict"))
                    });
            if frame || paint_containment || axes[0] != "visible" {
                area.0 = end.0;
                area.2 = end.2;
            }
            if frame || paint_containment || axes[1] != "visible" {
                area.1 = end.1;
                area.3 = end.3;
            }
        }
        // A child's border box also contributes independently of its clipped
        // scrollable overflow. The box's own scrolling area excludes borders.
        area.union(Edge::rect(f.x, f.y, f.x + f.w, f.y + f.h));
        // CSS Transforms 1 #transform-rendering: transforms extend, never
        // shrink, overflow. The box's own scrolling area above is local and
        // unscaled; its contribution to an ancestor includes both bounds.
        if f.paint.transform.is_some() {
            let r = super::transform::bounds(
                super::transform::matrix(f),
                crate::render::CssRect::new(area.2, area.3, area.0 - area.2, area.1 - area.3),
            );
            area.union(Edge::rect(r.x, r.y, r.x + r.width, r.y + r.height));
        }
        if !root && f.node != NO_NODE {
            match dom.computed_value_resolved(f.node, "position").as_deref() {
                // Its containing block is an inline box of the parent's
                // formatting context, so it joins the parent's area.
                Some("absolute") if f.flow.inline_cb => {}
                Some("absolute") => {
                    escaping.push((false, area));
                    return Edge::point(0., 0.);
                }
                Some("fixed") => {
                    escaping.push((true, area));
                    return Edge::point(0., 0.);
                }
                _ => {}
            }
        }
        area
    }
}
