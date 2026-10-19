//! CSS Position 3 #stickypos-insets and CSS Overflow 3 #scrollable-overflow-calculation.
//! Resolve constraints once from layout; paint and CSSOM sample identical
//! offsets from the current scroll position without rebuilding the box tree.
//! CSSWG snapshot 81c27f686901 (2026-09-06).

use super::flow::{Frag, FragKind};
use super::overflow::ScrollAreas;
use super::value::{Len, Vp};
use super::{Dom, NO_NODE, Units};
use crate::render::{CssRect, StickyConstraint};

#[derive(Clone, Debug)]
pub(super) struct Geometry {
    pub constraint: StickyConstraint,
    pub viewport: CssRect,
}

#[derive(Clone, Copy)]
struct Page {
    viewport: CssRect,
    root: super::NodeId,
    overflow_source: Option<super::NodeId>,
}

impl Page {
    fn new(dom: &Dom, root: super::NodeId, viewport: CssRect) -> Self {
        Self {
            viewport,
            root,
            overflow_source: super::overflow::viewport_overflow_source_for(dom, root),
        }
    }
}

pub(super) fn collect(
    dom: &Dom,
    root: &Frag,
    vp: Vp,
    scrolling: Option<&ScrollAreas>,
) -> Vec<Geometry> {
    let mut result = Vec::new();
    let local = std::cell::OnceCell::new();
    collect_in(
        dom,
        root,
        vp,
        Page::new(
            dom,
            dom.document_element().unwrap_or(NO_NODE),
            CssRect::new(0., 0., vp.w, vp.h),
        ),
        &mut Vec::new(),
        &mut result,
        &mut || scrolling.unwrap_or_else(|| local.get_or_init(|| ScrollAreas::new(dom, root))),
    );
    result
}

fn collect_in<'f, 's>(
    dom: &Dom,
    fragment: &'f Frag,
    vp: Vp,
    page: Page,
    ancestors: &mut Vec<&'f Frag>,
    result: &mut Vec<Geometry>,
    scrolling: &mut impl FnMut() -> &'s ScrollAreas,
) {
    if fragment.node != NO_NODE && fragment.paint.position == super::Pos::Sticky {
        // The scrollport constrains sticking, while the formatting context
        // constrains the position box. An intervening content wrapper must
        // remain a limit even when its own overflow is visible.
        let scrolls = [0, 1].map(|axis| {
            ancestors.iter().rev().copied().find(|ancestor| {
                ancestor.node != NO_NODE
                    && ancestor.paint.overflow[axis].scrollable()
                    && ancestor.node != page.root
                    && Some(ancestor.node) != page.overflow_source
            })
        });
        let [x_view, y_view] = scrolls.map(|scroll| scroll.map_or(page.viewport, padding_box));
        let viewport = CssRect::new(x_view.x, y_view.y, x_view.width, y_view.height);
        let parent = ancestors
            .iter()
            .rev()
            .copied()
            .find(|f| matches!(f.kind, FragKind::Block | FragKind::TableCell(_)));
        let mut cb = parent.map_or(page.viewport, Frag::content_box);
        let percentage_basis = cb.width;
        if let Some(parent) = parent
            && scrolls
                .iter()
                .any(|scroll| scroll.is_some_and(|scroll| std::ptr::eq(scroll, parent)))
            && let Some(area) = scrolling().nodes.get(&parent.node)
        {
            // A scroll container's contents are not bounded by its client
            // height. Use its scrollable content edges for a sticky child;
            // using the scrollport here makes headers unstick after only
            // one screen. Insets still resolve against `viewport` above.
            let padding = padding_box(parent);
            let left = area.left as f32 + (cb.x - padding.x).max(0.);
            let top = area.top as f32 + (cb.y - padding.y).max(0.);
            let right = (area.left + area.width) as f32
                - (padding.x + padding.width - cb.x - cb.width).max(0.);
            let bottom = (area.top + area.height) as f32
                - (padding.y + padding.height - cb.y - cb.height).max(0.);
            if scrolls[0].is_some_and(|scroll| std::ptr::eq(scroll, parent)) {
                let end = (cb.x + cb.width).max(right);
                cb.x = cb.x.min(left);
                cb.width = end - cb.x;
            }
            if scrolls[1].is_some_and(|scroll| std::ptr::eq(scroll, parent)) {
                let end = (cb.y + cb.height).max(bottom);
                cb.y = cb.y.min(top);
                cb.height = end - cb.y;
            }
        }
        let units = Units::of(dom, fragment.node);
        let margin = ["top", "right", "bottom", "left"].map(|side| {
            dom.computed_value_resolved(fragment.node, &format!("margin-{side}"))
                .as_deref()
                .and_then(|s| Len::parse(s, units, vp))
                .and_then(|n| n.resolve(Some(percentage_basis)))
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
        let writing_node = parent
            .filter(|parent| parent.node != NO_NODE)
            .map_or(fragment.node, |parent| parent.node);
        let writing = dom
            .computed_value_resolved(writing_node, "writing-mode")
            .unwrap_or_default();
        let vertical = writing.starts_with("vertical") || writing.starts_with("sideways");
        let rtl = dom
            .computed_value_resolved(writing_node, "direction")
            .as_deref()
            == Some("rtl");
        result.push(Geometry {
            constraint: StickyConstraint {
                node: fragment.node,
                rect: CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
                container: scrolls[1].map(|f| f.node),
                horizontal_container: scrolls[0].map(|f| f.node),
                movement: [
                    position_margin[0] - distance[0],
                    distance[1] - position_margin[1],
                    distance[2] - position_margin[2],
                    position_margin[3] - distance[3],
                ],
                reverse: [
                    if vertical {
                        writing.ends_with("-rl")
                    } else {
                        rtl
                    },
                    vertical && (rtl ^ (writing == "sideways-lr")),
                ],
                insets: ["top", "right", "bottom", "left"].map(|side| {
                    dom.computed_value_resolved(fragment.node, side)
                        .as_deref()
                        .and_then(|s| Len::parse(s, units, vp))
                        .and_then(|n| {
                            n.resolve(Some(if matches!(side, "top" | "bottom") {
                                viewport.height
                            } else {
                                viewport.width
                            }))
                        })
                }),
            },
            viewport,
        });
    }
    // A child Document's sticky boxes use its own viewport, never its
    // embedding Document's scrollport or containing block.
    if fragment.paint.child_viewport {
        let viewport = fragment.content_box();
        for child in &fragment.children {
            collect_in(
                dom,
                child,
                Vp {
                    w: viewport.width,
                    h: viewport.height,
                },
                Page::new(dom, child.node, viewport),
                &mut Vec::new(),
                result,
                scrolling,
            );
        }
    } else {
        ancestors.push(fragment);
        for child in &fragment.children {
            collect_in(dom, child, vp, page, ancestors, result, scrolling);
        }
        ancestors.pop();
    }
}

fn padding_box(fragment: &Frag) -> CssRect {
    let [top, right, bottom, left] = fragment.border;
    CssRect::new(
        fragment.x + left,
        fragment.y + top,
        (fragment.w - left - right).max(0.),
        (fragment.h - top - bottom).max(0.),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CssPoint, CssSize};

    fn fixture(html: &str) -> (Dom, std::sync::Arc<super::super::LayoutFragments>) {
        let mut dom = Dom::parse_document(html);
        dom.set_viewport_px(400., 300.);
        let measured = super::super::measure_retained_layout(
            &dom,
            &url::Url::parse("https://sticky.test/").unwrap(),
            super::super::Viewport::new(400., 300.),
            &[],
            &Default::default(),
            &Default::default(),
        );
        (dom, measured.fragments.unwrap())
    }

    fn assert_paint_at(
        dom: &Dom,
        fragments: &super::super::LayoutFragments,
        node: usize,
        point: CssPoint,
    ) {
        let paint = super::super::paint_retained_hit_test(
            dom,
            &url::Url::parse("https://sticky.test/").unwrap(),
            &Default::default(),
            fragments,
        );
        assert!(
            crate::render::page_element_hits_at(
                &paint,
                CssSize::new(400., 300.),
                CssPoint::default(),
                point
            )
            .iter()
            .any(|hit| hit.node == node),
            "{point:?}"
        );
        let mut index = crate::render::PageHitIndex::new(&paint);
        assert!(
            index
                .query(
                    &paint,
                    CssSize::new(400., 300.),
                    CssPoint::default(),
                    point,
                    0.
                )
                .iter()
                .any(|hit| hit.node == node)
        );
    }

    #[test]
    fn servo_sticky_scrollable_content_bounds_and_cssom_sample_without_layout() {
        // Reduced WPT position-sticky-get-bounding-client-rect.html from
        // Servo #43766: a direct child follows the whole scrollable content.
        let (mut dom, fragments) = fixture(
            r#"<style>body{margin:0}
            #sc{width:200px;height:200px;overflow:scroll}
            #s{position:sticky;top:50px;left:20px;width:100px;height:100px;background:red}
            #spacer{width:2000px;height:2000px}</style><div id=sc><div id=s></div><div id=spacer></div></div>"#,
        );
        let sc = dom.get_by_id("sc").unwrap();
        let s = dom.get_by_id("s").unwrap();
        let original = fragments.single_border_box(s).unwrap();
        let tree = fragments.scroll_tree(&dom) as *const super::super::spatial::ScrollTree;
        let passes = super::super::layout_pass_count();
        for (x, y) in [(0., 0.), (75., 100.), (500., 1500.)] {
            dom.set_scroll_pos(sc, y, x, false);
            let (delta, fixed) = fragments.scroll_placement(&dom, s);
            assert!(!fixed);
            assert_eq!(
                (
                    original.left + f64::from(delta.x),
                    original.top + f64::from(delta.y)
                ),
                (20., 50.)
            );
            assert_paint_at(&dom, &fragments, s, CssPoint::new(70., 100.));
            assert_eq!(fragments.scroll_tree(&dom) as *const _, tree);
        }
        assert_eq!(super::super::layout_pass_count(), passes);
    }

    #[test]
    fn servo_sticky_intermediate_containing_block_still_limits_movement() {
        let (mut dom, fragments) = fixture(
            r#"<style>body{margin:0}
            #sc{height:200px;overflow:auto}#wrapper{height:300px}
            #s{position:sticky;top:10px;width:100px;height:100px}</style>
            <div id=sc><div id=wrapper><div id=s></div></div><div style='height:2000px'></div></div>"#,
        );
        let s = dom.get_by_id("s").unwrap();
        let sc = dom.get_by_id("sc").unwrap();
        dom.set_scroll_pos(sc, 600., 0., false);
        let (delta, _) = fragments.scroll_placement(&dom, s);
        assert_eq!(
            delta.y, -400.,
            "the wrapper ends at300 even though the scroller continues"
        );
    }

    #[test]
    fn servo_sticky_body_is_a_real_scrollport_when_overflow_does_not_propagate() {
        let (mut dom, fragments) = fixture(
            r#"<style>html{overflow:hidden}body{margin:0;overflow:auto;width:100px;height:100px}
            #s{position:sticky;top:10px;width:30px;height:20px}</style><body id=body><div id=s></div><div style='height:1000px'></div>"#,
        );
        let body = dom.get_by_id("body").unwrap();
        let s = dom.get_by_id("s").unwrap();
        dom.set_scroll_pos(body, 500., 0., false);
        assert_eq!(fragments.scroll_placement(&dom, s).0.y, 10.);
        assert_paint_at(&dom, &fragments, s, CssPoint::new(15., 15.));
    }

    #[test]
    fn servo_sticky_axes_choose_their_own_scrollport() {
        let (mut dom, fragments) = fixture(
            r#"<style>body{margin:0}
            #outer{width:100px;height:150px;overflow-x:scroll;overflow-y:clip}
            #inner{width:300px;height:100px;overflow-x:clip;overflow-y:scroll}
            #s{position:sticky;top:10%;left:10%;width:50px;height:20px}
            </style><div id=outer><div id=inner><div id=s></div><div style='width:500px;height:1000px'></div></div></div>"#,
        );
        let s = dom.get_by_id("s").unwrap();
        dom.set_scroll_pos(dom.get_by_id("outer").unwrap(), 0., 100., false);
        dom.set_scroll_pos(dom.get_by_id("inner").unwrap(), 500., 0., false);
        let (delta, _) = fragments.scroll_placement(&dom, s);
        assert_eq!(delta, CssPoint::new(10., 10.));
        assert_paint_at(&dom, &fragments, s, CssPoint::new(20., 20.));
    }

    #[test]
    fn servo_sticky_nested_offsets_are_resolved_after_the_parent_moves() {
        let (mut dom, fragments) = fixture(
            r#"<style>body{margin:0}
            #sc{width:200px;height:200px;overflow:scroll}
            #outer{position:sticky;top:10px;left:20px;width:100px;height:60px}
            #inner{position:sticky;top:25px;left:40px;width:50px;height:20px}
            </style><div id=sc><div id=outer><div id=inner></div></div><div style='height:1000px;width:1000px'></div></div>"#,
        );
        dom.set_scroll_pos(dom.get_by_id("sc").unwrap(), 60., 50., false);
        let outer = dom.get_by_id("outer").unwrap();
        let inner = dom.get_by_id("inner").unwrap();
        assert_eq!(
            fragments.scroll_placement(&dom, outer).0,
            CssPoint::new(20., 10.)
        );
        assert_eq!(
            fragments.scroll_placement(&dom, inner).0,
            CssPoint::new(40., 25.)
        );
        assert_paint_at(&dom, &fragments, inner, CssPoint::new(45., 30.));
    }

    #[test]
    fn servo_sticky_transformed_scrollport_and_fixed_descendants_keep_their_spaces() {
        let (mut dom, fragments) = fixture(
            r#"<style>body{margin:0}
            #sc{width:100px;height:100px;overflow:auto;transform:translate(10px,20px) scale(2);transform-origin:0 0}
            #s{position:sticky;top:10px;left:5px;width:50px;height:30px}
            #a{position:absolute;top:0;left:0;width:5px;height:5px}
            </style><div id=sc><div id=s><div id=a></div></div><div style='height:1000px;width:1000px'></div></div>"#,
        );
        let s = dom.get_by_id("s").unwrap();
        let a = dom.get_by_id("a").unwrap();
        dom.set_scroll_pos(dom.get_by_id("sc").unwrap(), 500., 300., false);
        let original = fragments.single_border_box(s).unwrap();
        let (delta, _) = fragments.scroll_placement(&dom, s);
        assert_eq!(
            (
                original.left + f64::from(delta.x),
                original.top + f64::from(delta.y)
            ),
            (20., 40.)
        );
        assert_eq!(fragments.scroll_placement(&dom, a).0, delta);
        assert_paint_at(&dom, &fragments, s, CssPoint::new(30., 50.));

        let (dom, fragments) = fixture(
            r#"<style>body{margin:0;height:1500px}
            #s{position:sticky;top:10px;width:100px;height:30px}
            #fixed{position:fixed;top:3px;left:4px;width:5px;height:5px}</style><div id=s><div id=fixed></div></div>"#,
        );
        let viewport_scroll = CssPoint::new(0., 500.);
        let s = dom.get_by_id("s").unwrap();
        let fixed = dom.get_by_id("fixed").unwrap();
        assert_eq!(
            fragments.scroll_placement_at(&dom, s, viewport_scroll).0.y,
            510.
        );
        assert_eq!(
            fragments.scroll_placement_at(&dom, fixed, viewport_scroll),
            (CssPoint::default(), true)
        );
        assert_eq!(
            fragments
                .sticky_layout_placement(&dom, s, viewport_scroll)
                .y,
            510.
        );
    }
}
