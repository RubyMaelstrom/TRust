//! Retained scroll/clip ancestry for one completed layout.
//!
//! CSS Overflow 3 #scrolling and CSS Position 3 #def-cb follow containing
//! blocks, not DOM parents. This tree is immutable; only scroll positions are
//! sampled from the DOM. Geometry, graphical clipping and hit testing share
//! its ancestry. Scrolling never invalidates layout, shaping or this index.
//! CSSWG local snapshot 81c27f686901 (2026-09-06).

use super::flow::{Frag, FragKind, TopFrag};
use super::overflow::{Overflow, viewport_overflow_source_for};
use super::style::Pos;
use super::{Dom, NO_NODE, NodeId};
use crate::core::CssPoint;
use crate::render::Affine2d;
use rustc_hash::FxHashMap;

type Chain = Option<usize>;

#[derive(Clone, Copy, Debug, Default)]
struct Space {
    chain: Chain,
    viewport_fixed: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct NodeSpaces {
    border: Space,
    content: Space,
}

#[derive(Clone, Copy)]
struct Context {
    normal: Space,
    absolute: Space,
    fixed: Space,
    transform: Affine2d,
    root: NodeId,
    viewport_source: Option<NodeId>,
}

#[derive(Clone, Debug)]
pub(super) struct ScrollLink {
    parent: Chain,
    pub node: NodeId,
    pub axes: [bool; 2],
    /// CSSOM View changes coordinate systems at a nested Document, whereas
    /// paint keeps the embedding scrollport and all its ancestors.
    viewport: bool,
    basis: [f32; 4],
}

#[derive(Clone, Debug, Default)]
pub(super) struct ScrollTree {
    links: Vec<ScrollLink>,
    nodes: FxHashMap<NodeId, NodeSpaces>,
}

impl ScrollTree {
    pub fn new(dom: &Dom, root: &Frag, fixed: &[Frag], top_layer: &[TopFrag]) -> Self {
        let mut tree = Self::default();
        let document_root = dom.document_element().unwrap_or(NO_NODE);
        let context = Context {
            normal: Space::default(),
            absolute: Space::default(),
            fixed: Space {
                viewport_fixed: true,
                ..Space::default()
            },
            transform: Affine2d::IDENTITY,
            root: document_root,
            viewport_source: viewport_overflow_source_for(dom, document_root),
        };
        tree.walk(dom, root, context);
        for fragment in fixed {
            tree.walk(
                dom,
                fragment,
                Context {
                    normal: context.fixed,
                    absolute: context.fixed,
                    ..context
                },
            );
        }
        for top in top_layer {
            let space = Space {
                viewport_fixed: top.fixed,
                ..Space::default()
            };
            tree.walk(
                dom,
                &top.fragment,
                Context {
                    normal: space,
                    absolute: space,
                    fixed: space,
                    ..context
                },
            );
        }
        tree
    }

    fn link(
        &mut self,
        space: Space,
        node: NodeId,
        axes: [bool; 2],
        viewport: bool,
        transform: Affine2d,
    ) -> Space {
        let id = self.links.len();
        self.links.push(ScrollLink {
            parent: space.chain,
            node,
            axes,
            viewport,
            basis: transform.0[..4].try_into().unwrap(),
        });
        Space {
            chain: Some(id),
            ..space
        }
    }

    fn walk(&mut self, dom: &Dom, fragment: &Frag, context: Context) {
        if matches!(fragment.kind, FragKind::Fixed(_) | FragKind::Oof(..)) {
            return;
        }
        let border = match fragment.paint.position {
            Pos::Absolute => context.absolute,
            Pos::Fixed => context.fixed,
            _ => context.normal,
        };
        let transform = context.transform.then(super::transform::matrix(fragment));
        let mut content = border;
        let is_box = matches!(fragment.kind, FragKind::Block | FragKind::TableCell(_));
        let viewport = fragment.paint.child_viewport;
        if is_box
            && fragment.node != NO_NODE
            && (viewport
                || (fragment.node != context.root
                    && Some(fragment.node) != context.viewport_source
                    && fragment.paint.overflow != [Overflow::Visible; 2]))
        {
            content = self.link(
                border,
                fragment.node,
                if viewport {
                    [true; 2]
                } else {
                    fragment.paint.overflow.map(Overflow::scrollable)
                },
                viewport,
                transform,
            );
        }
        if is_box && fragment.node != NO_NODE {
            self.nodes
                .insert(fragment.node, NodeSpaces { border, content });
        }
        if let FragKind::Line(line) = &fragment.kind {
            // Inline wrappers do not create scrollports of their own. Record
            // their placement at their generated pieces, crossing slots and
            // shadow roots through flat-tree ancestry, once per layout.
            for piece in &line.pieces {
                let mut node = (piece.item.node != NO_NODE).then_some(piece.item.node);
                while let Some(id) = node {
                    if self.nodes.contains_key(&id) {
                        break;
                    }
                    self.nodes.insert(
                        id,
                        NodeSpaces {
                            border,
                            content: border,
                        },
                    );
                    node = dom.parent_flat(id);
                }
            }
        }
        let mut child = Context {
            normal: content,
            absolute: if fragment.paint.cb_abs {
                content
            } else {
                context.absolute
            },
            fixed: if fragment.paint.cb_fixed {
                content
            } else {
                context.fixed
            },
            transform,
            ..context
        };
        if viewport {
            // A child viewport's fixed boxes keep its clip, but not the
            // scrolling translation of its document canvas.
            let fixed_clip = self.link(border, fragment.node, [false; 2], true, transform);
            child.fixed = Space {
                viewport_fixed: true,
                ..fixed_clip
            };
            child.normal.viewport_fixed = false;
            child.absolute = child.normal;
            child.transform = Affine2d::IDENTITY;
            child.root = fragment.children.first().map_or(NO_NODE, |f| f.node);
            child.viewport_source = viewport_overflow_source_for(dom, child.root);
        }
        for fragment in &fragment.children {
            self.walk(dom, fragment, child);
        }
    }

    pub fn chain(&self, node: NodeId, content: bool) -> impl Iterator<Item = &ScrollLink> {
        let mut next = self.nodes.get(&node).and_then(|spaces| {
            if content {
                spaces.content.chain
            } else {
                spaces.border.chain
            }
        });
        std::iter::from_fn(move || {
            let link = self.links.get(next?)?;
            next = link.parent;
            Some(link)
        })
    }

    pub fn viewport_fixed(&self, node: NodeId) -> bool {
        self.nodes
            .get(&node)
            .is_some_and(|spaces| spaces.border.viewport_fixed)
    }

    pub fn axes(&self, node: NodeId) -> [bool; 2] {
        self.chain(node, true)
            .next()
            .filter(|link| link.node == node)
            .map_or([false; 2], |link| link.axes)
    }

    pub fn offset(&self, dom: &Dom, node: NodeId) -> CssPoint {
        let mut delta = CssPoint::default();
        for link in self.chain(node, false) {
            if link.viewport {
                break;
            }
            let x = if link.axes[0] {
                dom.scroll_metric(link.node, 1).unwrap_or(0.) as f32
            } else {
                0.
            };
            let y = if link.axes[1] {
                dom.scroll_metric(link.node, 0).unwrap_or(0.) as f32
            } else {
                0.
            };
            let [a, b, c, d] = link.basis;
            // Translations are vectors, not points. No matrix inversion is
            // needed, including singular transforms and negative scales.
            delta.x -= a * x + c * y;
            delta.y -= b * x + d * y;
        }
        delta
    }

    pub fn retained_bytes(&self) -> usize {
        self.links.capacity() * std::mem::size_of::<ScrollLink>()
            + self.nodes.capacity() * std::mem::size_of::<(NodeId, NodeSpaces)>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::CssSize;

    #[test]
    fn retained_scroll_spaces_agree_with_paint_and_hit_testing() {
        for overflow in ["auto", "hidden"] {
            for transform in [
                "translate(20px,30px) scale(2)",
                "translate(200px,30px) rotate(90deg)",
            ] {
                let mut dom = Dom::parse_document(&format!(
                    r#"<!doctype html><style>html,body{{margin:0}}
                    #sc{{position:relative;width:100px;height:80px;overflow:{overflow};
                    transform:{transform};transform-origin:0 0}}
                    #target{{position:absolute;left:70px;top:60px;width:20px;height:10px;background:red}}
                    </style><div id=sc><div id=target></div><div style='width:400px;height:500px'></div></div>"#
                ));
                let base = url::Url::parse("https://scroll.test/").unwrap();
                let viewport = super::super::Viewport::new(400., 300.);
                let images = Default::default();
                let measured = super::super::measure_retained_layout(
                    &dom,
                    &base,
                    viewport,
                    &[],
                    &Default::default(),
                    &images,
                );
                let fragments = measured.fragments.unwrap();
                let target = dom.get_by_id("target").unwrap();
                let sc = dom.get_by_id("sc").unwrap();
                let tree = fragments.scroll_tree(&dom) as *const ScrollTree;
                let passes = super::super::layout_pass_count();
                for (x, y) in [(35., 50.), (0., 0.), (10.5, 5.25)] {
                    dom.set_scroll_pos(sc, y, x, false);
                    let (delta, fixed) = fragments.scroll_placement(&dom, target);
                    assert!(!fixed);
                    let r = fragments.single_border_box(target).unwrap();
                    let x = r.left as f32 + delta.x;
                    let y = r.top as f32 + delta.y;
                    let center = CssPoint::new(x + r.width as f32 / 2., y + r.height as f32 / 2.);
                    let paint =
                        super::super::paint_retained_hit_test(&dom, &base, &images, &fragments);
                    assert!(
                        crate::render::page_element_hits_at(
                            &paint,
                            CssSize::new(400., 300.),
                            CssPoint::default(),
                            center
                        )
                        .iter()
                        .any(|hit| hit.node == target),
                        "hit {overflow} {transform} {center:?}"
                    );
                    let raster =
                        crate::render::headless::render_paint(&paint, CssSize::new(400., 300.))
                            .unwrap();
                    let at = (center.y as usize * 400 + center.x as usize) * 4;
                    assert_eq!(
                        &raster.pixels[at..at + 3],
                        &[255, 0, 0],
                        "paint {overflow} {transform} {center:?}"
                    );
                    assert_eq!(fragments.scroll_tree(&dom) as *const ScrollTree, tree);
                    assert_eq!(super::super::layout_pass_count(), passes);
                }
            }
        }
    }

    #[test]
    fn typed_overflow_preserves_single_axis_clip_and_programmatic_hidden() {
        let axes = |x: &str, y: &str| {
            Overflow::axes(|property| match property {
                "overflow-x" => Some(x.into()),
                "overflow-y" => Some(y.into()),
                _ => None,
            })
        };
        assert_eq!(axes("clip", "scroll"), [Overflow::Clip, Overflow::Scroll]);
        assert_eq!(
            axes("visible", "hidden"),
            [Overflow::Auto, Overflow::Hidden]
        );
        assert_eq!(
            axes("hidden", "visible"),
            [Overflow::Hidden, Overflow::Auto]
        );
        assert!(Overflow::Hidden.scrollable());
        assert!(!Overflow::Hidden.user_scrollable());
        assert!(!Overflow::Clip.scrollable());
    }
}
