//! CSS Overflow 3 #scrollable-overflow-calculation and #overflow-propagation.
//! Keep CSSOM scrolling areas and the desktop's scroll ranges on the same
//! fragment calculation; clipped child content belongs to its own scrollport.
use super::flow::{Frag, FragKind};
use super::{Dom, NO_NODE, NodeId, PxRect};
use std::collections::HashMap;

pub(super) fn overflow_axes(dom: &Dom, node: NodeId) -> [String; 2] {
    if node == NO_NODE {
        return ["visible".into(), "visible".into()];
    }
    let shorthand = dom
        .computed_value_resolved(node, "overflow")
        .unwrap_or_else(|| "visible".into());
    let mut tokens = shorthand.split_whitespace();
    let x = tokens.next().unwrap_or("visible");
    let y = tokens.next().unwrap_or(x);
    let mut axes = [
        dom.computed_value_resolved(node, "overflow-x")
            .unwrap_or_else(|| x.into()),
        dom.computed_value_resolved(node, "overflow-y")
            .unwrap_or_else(|| y.into()),
    ];
    // CSS Overflow 3 #overflow-properties: visible/clip compute to
    // auto/hidden when the other axis is a scrollable overflow value.
    let scrollable = axes
        .each_ref()
        .map(|a| !matches!(a.as_str(), "visible" | "clip"));
    for axis in 0..2 {
        if scrollable[1 - axis] {
            axes[axis] = match axes[axis].as_str() {
                "visible" => "auto".into(),
                "clip" => "hidden".into(),
                _ => axes[axis].clone(),
            };
        }
    }
    axes
}

pub(super) fn viewport_overflow_source(dom: &Dom) -> Option<NodeId> {
    let root = dom.document_element()?;
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

#[derive(Default)]
pub(super) struct ScrollAreas {
    pub nodes: HashMap<NodeId, PxRect>,
    pub document: (f32, f32),
}

#[derive(Clone, Copy)]
struct Edge(f32, f32);

impl Edge {
    fn union(&mut self, other: Self) {
        self.0 = self.0.max(other.0);
        self.1 = self.1.max(other.1);
    }
}

impl ScrollAreas {
    pub fn new(dom: &Dom, root: &Frag<'_>) -> Self {
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

    pub fn extend(&mut self, dom: &Dom, root: &Frag<'_>) {
        self.walk(dom, root, None, true, &mut Vec::new());
    }

    fn walk(
        &mut self,
        dom: &Dom,
        f: &Frag<'_>,
        viewport_source: Option<NodeId>,
        root: bool,
        escaping: &mut Vec<(bool, Edge)>,
    ) -> Edge {
        if matches!(f.kind, FragKind::Fixed(_) | FragKind::Oof(..)) {
            return Edge(0., 0.);
        }
        if f.flow.hidden || f.flow.float_clip_end.is_some() {
            // CSS Overflow 4 #line-clamp-containers: these boxes retain
            // CSSOM geometry but contribute only ink overflow. An abspos
            // descendant whose containing block escapes the hidden subtree
            // can still contribute to that external containing block.
            for child in &f.children {
                self.walk(dom, child, viewport_source, false, escaping);
            }
            return Edge(0., 0.);
        }
        let frame = f.node != NO_NODE && matches!(dom.tag_name(f.node), Some("iframe" | "frame"));
        let [top, right, bottom, left] = f.border;
        let content = f.content_box();
        let (x, y, end) = if frame {
            (
                content.x,
                content.y,
                Edge(content.x + content.width, content.y + content.height),
            )
        } else {
            (
                f.x + left,
                f.y + top,
                Edge(f.x + f.w - right, f.y + f.h - bottom),
            )
        };
        let mut area = end;
        let mut flow_end = Edge(content.x, content.y);
        let mut pending = Vec::new();
        for child in &f.children {
            let edge = self.walk(dom, child, viewport_source, false, &mut pending);
            area.union(edge);
            if edge.0 != 0. || edge.1 != 0. {
                flow_end.union(Edge(child.x + child.w, child.y + child.h));
            }
        }
        // Preserve end padding after in-flow content at the final scroll
        // position, without adding it to an escaping positioned descendant.
        area.union(Edge(
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
            let rect = PxRect {
                left: x as f64,
                top: y as f64,
                width: (area.0 - x).max(0.) as f64,
                height: (area.1 - y).max(0.) as f64,
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
            }
            if frame || paint_containment || axes[1] != "visible" {
                area.1 = end.1;
            }
        }
        // A child's border box also contributes independently of its clipped
        // scrollable overflow. The box's own scrolling area excludes borders.
        area.union(Edge(f.x + f.w, f.y + f.h));
        if !root && f.node != NO_NODE {
            match dom.computed_value_resolved(f.node, "position").as_deref() {
                Some("absolute") => {
                    escaping.push((false, area));
                    return Edge(0., 0.);
                }
                Some("fixed") => {
                    escaping.push((true, area));
                    return Edge(0., 0.);
                }
                _ => {}
            }
        }
        area
    }
}
