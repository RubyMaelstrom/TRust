//! Resolve gesture policy from retained paint at the time a contact starts.
//! Pointer Events 49c398264c8d #conforming-touch-behavior: policy follows the
//! target's ancestors, with each element's actual transformed coordinate axes.

use super::*;
use crate::touch::PanConstraint;

#[derive(Debug)]
pub struct TouchTarget {
    pub hit: PageHit,
    /// Already stopped at the inclusive nearest scroll container or the
    /// current Document root. Embedding elements cannot restrict this list.
    pub constraints: Vec<PanConstraint>,
    /// Linear coordinate transforms for the contact's ancestor scrollports.
    /// Scroll offsets remain live presentation state, not gesture policy.
    pub transforms: HashMap<usize, Affine2d>,
    /// Containing-block scroll ancestry, independent of overlapping boxes
    /// and of the DOM ancestry used for touch-action.
    pub scroll_chain: Vec<usize>,
}

impl Scene {
    pub fn touch_target_at(
        &self,
        point: CssPoint,
        parents: &HashMap<usize, usize>,
    ) -> Option<TouchTarget> {
        if !self.content_viewport.contains(point) {
            return None;
        }
        // Include ordinary elements; native activation hit testing deliberately
        // skips those, but touch-action is independent of clickability.
        let hit = self.page_element_hits_at(point).into_iter().next()?;
        let mut path = Vec::new();
        let mut ancestors = HashSet::new();
        let mut current = Some(hit.node);
        while let Some(node) = current {
            if !ancestors.insert(node) {
                break;
            }
            path.push(node);
            current = parents.get(&node).copied();
        }
        let mut scopes: HashMap<usize, Vec<(TouchScope, Affine2d)>> = HashMap::new();
        let mut transform = Affine2d::IDENTITY;
        let mut stack = Vec::new();
        for command in &self.primitives {
            match command {
                DisplayCommand::PushTransform(next) => {
                    stack.push(transform);
                    transform = transform.then(*next);
                }
                DisplayCommand::PopTransform => {
                    transform = stack.pop().unwrap_or(Affine2d::IDENTITY)
                }
                DisplayCommand::TouchScope(scope)
                    if ancestors.contains(&scope.node) || scope.scroll_container =>
                {
                    // Fragment translations do not change a policy's axes.
                    let [a, b, c, d, _, _] = transform.0;
                    let basis = Affine2d([a, b, c, d, 0.0, 0.0]);
                    let entries = scopes.entry(scope.node).or_default();
                    if !entries.iter().any(|(_, old)| *old == basis) {
                        entries.push((*scope, basis));
                    }
                }
                _ => {}
            }
        }
        // A scrolling box normally has one coordinate basis. If a future
        // fragmented/projected box has incompatible bases, do not guess
        // a transform for native scrolling of that box.
        let transforms = scopes
            .iter()
            .filter(|(_, entries)| entries.len() == 1)
            .map(|(&node, entries)| (node, entries[0].1))
            .collect();
        let scroll_target = path
            .iter()
            .find_map(|node| {
                scopes
                    .get(node)?
                    .iter()
                    .find_map(|(scope, _)| scope.scroll_space.then_some(scope.scroll_target))
            })
            .flatten();
        let scroll_chain = scroll_target
            .and_then(|node| {
                self.page_scroll_containers
                    .iter()
                    .find(|container| container.node == node)
            })
            .map_or_else(Vec::new, |container| {
                std::iter::once(container.node)
                    .chain(container.ancestors.iter().copied())
                    .collect()
            });
        let mut constraints = Vec::new();
        for node in path {
            let Some(entries) = scopes.get(&node) else {
                continue;
            };
            // Where fragment bases differ, permit only motion valid in all
            // of them. No screen-space bounding rectangle substitutes for an
            // ancestor's local coordinate system.
            for &(scope, basis) in entries {
                constraints.push(PanConstraint::new(scope.action, basis));
            }
            if entries
                .iter()
                .any(|(scope, _)| scope.scroll_container || scope.document_root)
            {
                break;
            }
        }
        Some(TouchTarget {
            hit,
            constraints,
            transforms,
            scroll_chain,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::touch::{PanGesture, PanUpdate};

    fn scene(dom: &crate::dom::Dom) -> Scene {
        let layout = crate::layout2::lay_out_graphical(
            dom,
            &url::Url::parse("https://touch.test/").unwrap(),
            crate::layout2::Viewport::new(400.0, 300.0),
            &[],
            &Default::default(),
            &Default::default(),
        );
        chromeless_scene(
            &layout.paint,
            CssSize::new(400.0, 300.0),
            CssPoint::default(),
        )
    }

    fn pan(dom: &crate::dom::Dom, point: CssPoint) -> PanGesture {
        let parents = (0..dom.node_count())
            .filter_map(|node| dom.parent_composed(node).map(|parent| (node, parent)))
            .collect();
        let target = scene(dom)
            .touch_target_at(point, &parents)
            .expect("touch hit");
        assert_eq!(target.hit.node, dom.get_by_id("target").unwrap());
        PanGesture::new(point, Affine2d::IDENTITY, [true; 2], target.constraints)
    }

    #[test]
    fn touch_action_ancestors_apply_even_without_their_own_visible_hit_region() {
        for style in ["pointer-events:none", "visibility:hidden"] {
            let dom = crate::dom::Dom::parse_document(&format!(
                "<!doctype html><style>body{{margin:0}}#outer{{touch-action:none;{style}}}\
                 #target{{pointer-events:auto;visibility:visible;width:80px;height:80px}}</style>\
                 <div id=outer><div id=target></div></div>"
            ));
            assert_eq!(
                pan(&dom, CssPoint::new(10.0, 20.0)).update(CssPoint::new(10.0, 5.0)),
                PanUpdate::Pointer,
                "{style}"
            );
        }
    }

    #[test]
    fn touch_action_stops_at_scroll_containers_and_excludes_non_replaced_inlines() {
        for html in [
            "<style>body{margin:0;touch-action:none}#scroller{height:80px;overflow:auto}\
             #target{height:300px}</style><div id=scroller><div id=target></div></div>",
            "<style>body{margin:0}#inline{touch-action:none}#target{display:inline-block;width:80px;height:80px}</style>\
             <span id=inline><span id=target></span></span>",
        ] {
            let dom = crate::dom::Dom::parse_document(html);
            assert!(
                matches!(
                    pan(&dom, CssPoint::new(10.0, 20.0)).update(CssPoint::new(10.0, 5.0)),
                    PanUpdate::Scroll { started: true, .. }
                ),
                "{html}"
            );
        }
    }

    #[test]
    fn touch_action_root_policy_and_rotated_descendant_use_painted_coordinates() {
        let blocked = crate::dom::Dom::parse_document(
            "<style>html{touch-action:none}body{margin:0}#target{width:80px;height:80px}</style><div id=target></div>",
        );
        assert_eq!(
            pan(&blocked, CssPoint::new(10.0, 20.0)).update(CssPoint::new(10.0, 5.0)),
            PanUpdate::Pointer
        );
        let rotated = crate::dom::Dom::parse_document(
            "<style>body{margin:0}#target{width:80px;height:80px;transform-origin:0 0;\
             transform:translateX(80px) rotate(90deg);touch-action:pan-x}</style><div id=target></div>",
        );
        assert!(matches!(
            pan(&rotated, CssPoint::new(10.0, 20.0)).update(CssPoint::new(10.0, 5.0)),
            PanUpdate::Scroll { started: true, .. }
        ));
        assert_eq!(
            pan(&rotated, CssPoint::new(20.0, 20.0)).update(CssPoint::new(5.0, 20.0)),
            PanUpdate::Pointer
        );
    }

    #[test]
    fn touch_scroll_chain_uses_containing_blocks_instead_of_overlapping_boxes() {
        for position in ["static", "absolute", "fixed"] {
            let dom = crate::dom::Dom::parse_document(&format!(
                "<style>body{{margin:0}}#sc{{overflow:auto;height:100px;width:100px}}\
                 #target{{position:{position};left:0;top:0;width:80px;height:80px}}\
                 </style><div id=sc><div id=target></div><div style='height:500px'></div></div>"
            ));
            let parents = (0..dom.node_count())
                .filter_map(|node| dom.parent_composed(node).map(|parent| (node, parent)))
                .collect();
            let target = scene(&dom)
                .touch_target_at(CssPoint::new(10.0, 20.0), &parents)
                .unwrap();
            assert_eq!(target.hit.node, dom.get_by_id("target").unwrap());
            assert_eq!(
                target.scroll_chain,
                if position == "static" {
                    vec![dom.get_by_id("sc").unwrap()]
                } else {
                    Vec::new()
                },
                "{position}"
            );
        }
    }

    #[test]
    fn touch_action_does_not_cross_an_embedded_document_boundary() {
        let mut dom = crate::dom::Dom::parse_document(
            "<style>body{margin:0;touch-action:none}iframe{border:0;width:100px;height:100px;touch-action:none}</style><iframe id=frame></iframe>",
        );
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(
            frame,
            "<style>body{margin:0}#target{width:80px;height:80px}</style><div id=target></div>",
            "https://touch.test/child",
        )
        .unwrap();
        assert!(matches!(
            pan(&dom, CssPoint::new(10.0, 20.0)).update(CssPoint::new(10.0, 5.0)),
            PanUpdate::Scroll { started: true, .. }
        ));
    }
}
