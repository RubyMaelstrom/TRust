//! One style/layout transaction, shared by CSSOM, observers and painting.
//!
//! CSS Conditional 5 §5.4 requires query changes to participate in the style
//! change event. The final, settled pass is the result, not a disposable
//! preflight followed by another layout. Callers may consume borrowed fragments
//! immediately or retain an immutable tree for later consumers of that result.

use super::*;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LayoutWork {
    pub passes: usize,
    pub query_updates: usize,
    pub tree: Duration,
    pub flow: Duration,
    pub item_hits: usize,
    pub intrinsic_hits: usize,
    pub tree_hits: usize,
    pub tree_builds: usize,
}

pub(super) struct FragmentLayout {
    pub root: flow::Frag,
    pub fixed: Vec<flow::Frag>,
    pub top_layer: Vec<flow::TopFrag>,
    pub flow_bottom: f32,
    pub anchors: Vec<(NodeId, f32)>,
    pub tracks: flow::GridTrackMap,
    pub work: LayoutWork,
}

/// Completed, owned CSS-pixel fragments. There is no mutable DOM or borrowed
/// box-tree data here. The actor and frontend can share this allocation; a
/// terminal adapter mutates only its private working copy when windowing it.
#[derive(Debug)]
pub(crate) struct LayoutFragments {
    pub(super) root: flow::Frag,
    pub(super) fixed: Vec<flow::Frag>,
    pub(super) top_layer: Vec<flow::TopFrag>,
    pub(super) flow_bottom: f32,
    pub(super) viewport: Viewport,
    pub(super) anchors: Vec<(NodeId, f32)>,
    single_boxes: OnceLock<measure::SingleBoxIndex>,
    layout_boxes: OnceLock<HashMap<NodeId, PxRect>>,
    client_metrics: OnceLock<rustc_hash::FxHashMap<NodeId, [f32; 4]>>,
    scroll_tree: OnceLock<spatial::ScrollTree>,
}

impl LayoutFragments {
    /// The canonical measurement transaction transfers its finished output
    /// directly to shared ownership. CSSOM and paint consume the same tree;
    /// retaining a result does not recursively copy it.
    pub(super) fn from_owned(
        root: flow::Frag,
        fixed: Vec<flow::Frag>,
        top_layer: Vec<flow::TopFrag>,
        flow_bottom: f32,
        viewport: Viewport,
        anchors: Vec<(NodeId, f32)>,
    ) -> Option<Arc<Self>> {
        (flow::positioned_complete(&root)
            && fixed.iter().all(flow::positioned_complete)
            && top_layer
                .iter()
                .all(|top| flow::positioned_complete(&top.fragment)))
        .then(|| {
            Arc::new(Self {
                root,
                fixed,
                top_layer,
                flow_bottom,
                viewport,
                anchors,
                single_boxes: OnceLock::new(),
                layout_boxes: OnceLock::new(),
                client_metrics: OnceLock::new(),
                scroll_tree: OnceLock::new(),
            })
        })
    }

    pub(crate) fn single_border_box(&self, node: NodeId) -> Option<PxRect> {
        self.single_boxes
            .get_or_init(|| measure::SingleBoxIndex::new(&self.root, &self.fixed, &self.top_layer))
            .get(node)
    }

    /// CSSOM View offset* ignores transforms on both the element and its
    /// ancestors. This projection shares the snapshot index; only fragmented
    /// and inline boxes require the complete composed projection.
    pub(crate) fn layout_border_box(&self, dom: &Dom, node: NodeId) -> Option<PxRect> {
        self.single_boxes
            .get_or_init(|| measure::SingleBoxIndex::new(&self.root, &self.fixed, &self.top_layer))
            .get_layout(node)
            .or_else(|| {
                self.layout_boxes
                    .get_or_init(|| {
                        measure::layout_boxes(dom, &self.root, &self.fixed, &self.top_layer)
                    })
                    .get(&node)
                    .copied()
            })
    }

    pub(crate) fn client_metrics(&self, node: NodeId) -> Option<[f32; 4]> {
        self.client_metrics
            .get_or_init(|| measure::client_metrics(&self.root, &self.fixed, &self.top_layer))
            .get(&node)
            .copied()
    }

    pub(super) fn scroll_tree(&self, dom: &Dom) -> &spatial::ScrollTree {
        self.scroll_tree
            .get_or_init(|| spatial::ScrollTree::new(dom, &self.root, &self.fixed, &self.top_layer))
    }

    pub(crate) fn scroll_placement(
        &self,
        dom: &Dom,
        node: NodeId,
    ) -> (crate::core::CssPoint, bool) {
        let tree = self.scroll_tree(dom);
        (tree.offset(dom, node), tree.viewport_fixed(node))
    }

    pub(crate) fn scroll_axes(&self, dom: &Dom, node: NodeId) -> [bool; 2] {
        self.scroll_tree(dom).axes(node)
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn measure_boxes(
        &self,
        dom: &Dom,
    ) -> (
        HashMap<NodeId, PxRect>,
        HashMap<NodeId, PxRect>,
        HashMap<NodeId, crate::render::CssRect>,
    ) {
        measure::boxes(dom, &self.root, &self.fixed, &self.top_layer)
    }

    pub(super) fn retain(
        root: &flow::Frag,
        fixed: &[flow::Frag],
        top_layer: &[flow::TopFrag],
        flow_bottom: f32,
        viewport: Viewport,
        anchors: &[(NodeId, f32)],
    ) -> Option<Arc<Self>> {
        Some(Arc::new(Self {
            root: flow::retain_for_paint(root)?,
            fixed: fixed
                .iter()
                .map(flow::retain_for_paint)
                .collect::<Option<_>>()?,
            top_layer: top_layer
                .iter()
                .map(|top| {
                    Some(flow::TopFrag {
                        fragment: flow::retain_for_paint(&top.fragment)?,
                        fixed: top.fixed,
                        order: top.order,
                    })
                })
                .collect::<Option<_>>()?,
            flow_bottom,
            viewport,
            anchors: anchors.to_vec(),
            single_boxes: OnceLock::new(),
            layout_boxes: OnceLock::new(),
            client_metrics: OnceLock::new(),
            scroll_tree: OnceLock::new(),
        }))
    }

    /// Requested-storage lower bound for the host memory inventory. Shaping
    /// and link internals are opaque here, and reported as such by the caller.
    pub(crate) fn retained_bytes(&self) -> usize {
        fn fragment(frag: &flow::Frag) -> usize {
            let mut bytes = frag.children.capacity() * std::mem::size_of::<flow::Frag>();
            match &frag.kind {
                flow::FragKind::Line(line) => {
                    bytes +=
                        std::mem::size_of::<flow::LineFrag>() + 2 * std::mem::size_of::<usize>();
                    bytes += (line.pieces.capacity() + line.atom_boxes.capacity())
                        * std::mem::size_of::<inline::Piece>();
                    for piece in line.pieces.iter().chain(&line.atom_boxes) {
                        bytes += piece.item.text.capacity();
                        for value in [
                            &piece.item.terminal_text,
                            &piece.item.graphical_image,
                            &piece.item.image,
                        ] {
                            bytes += value.as_ref().map_or(0, String::capacity);
                        }
                    }
                }
                flow::FragKind::TableCell(layers) => {
                    bytes += std::mem::size_of_val(layers.as_ref());
                }
                _ => {}
            }
            if let Some(collapsed) = &frag.paint.collapsed_borders {
                bytes += collapsed.retained_bytes();
            }
            bytes + frag.children.iter().map(fragment).sum::<usize>()
        }
        std::mem::size_of::<Self>()
            + self
                .scroll_tree
                .get()
                .map_or(0, spatial::ScrollTree::retained_bytes)
            + self.client_metrics.get().map_or(0, |metrics| {
                metrics.capacity() * std::mem::size_of::<(NodeId, [f32; 4])>()
            })
            + self.layout_boxes.get().map_or(0, |boxes| {
                boxes.capacity() * std::mem::size_of::<(NodeId, PxRect)>()
            })
            + self
                .single_boxes
                .get()
                .map_or(0, measure::SingleBoxIndex::retained_bytes)
            + self.fixed.capacity() * std::mem::size_of::<flow::Frag>()
            + self.top_layer.capacity() * std::mem::size_of::<flow::TopFrag>()
            + self.anchors.capacity() * std::mem::size_of::<(NodeId, f32)>()
            + fragment(&self.root)
            + self.fixed.iter().map(fragment).sum::<usize>()
            + self
                .top_layer
                .iter()
                .map(|top| fragment(&top.fragment))
                .sum::<usize>()
    }
}

#[cfg(test)]
thread_local! {
    static PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn layout_pass_count() -> usize {
    PASSES.with(std::cell::Cell::get)
}

/// Settle style and layout once. Deferred positioned children retain their
/// immutable input boxes until the containing-block pass resolves them.
pub(super) fn with_layout<R>(
    dom: &Dom,
    base: &Url,
    viewport: Viewport,
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
    finish: impl FnOnce(Option<FragmentLayout>) -> R,
) -> R {
    let _profile = diagnostics::Session::start();
    let vp = Vp {
        w: viewport.width,
        h: viewport.height,
    };
    let reuse = dom
        .layout_cache
        .borrow_mut()
        .prepare(dom, base, vp, forms, controls, images);
    let mut work = LayoutWork::default();
    for pass in 0..=64 {
        let started = Instant::now();
        let Some(root) = tree::build_document(dom, base, controls, forms, vp, reuse) else {
            return finish(None);
        };
        work.tree += started.elapsed();
        {
            let mut cache = dom.box_tree_cache.borrow_mut();
            work.tree_hits += std::mem::take(&mut cache.hits);
            work.tree_builds += std::mem::take(&mut cache.builds);
        }
        let flow = Flow {
            dom,
            base,
            forms,
            images,
            vp,
            reuse,
            imemo: Default::default(),
            grid_tracks: Default::default(),
            subgrid_rows: Default::default(),
            marker_line: Default::default(),
        };
        let started = Instant::now();
        let (frag, flow_bottom, anchors, fixed, top_layer) = flow.layout(&root);
        work.flow += started.elapsed();
        {
            let mut cache = dom.layout_cache.borrow_mut();
            work.item_hits += std::mem::take(&mut cache.item_hits);
            work.intrinsic_hits += std::mem::take(&mut cache.intrinsic_hits);
        }
        work.passes += 1;
        #[cfg(test)]
        PASSES.with(|count| count.set(count.get() + 1));
        if dom.style_depends_on_layout() && pass < 64 {
            let mut sizes = rustc_hash::FxHashMap::default();
            collect_container_sizes(dom, &frag, &mut sizes);
            for frag in &fixed {
                collect_container_sizes(dom, frag, &mut sizes);
            }
            for top in &top_layer {
                collect_container_sizes(dom, &top.fragment, &mut sizes);
            }
            if dom.update_container_sizes(sizes) {
                work.query_updates += 1;
                continue;
            }
        } else if pass == 64 && std::env::var_os("TRUST_LAYOUT_TRACE").is_some() {
            // Preserve the previous bounded fallback: 64 query updates, then
            // one final layout with the last available query environment.
            eprintln!("layout: container queries did not settle within 64 passes");
        }
        return finish(Some(FragmentLayout {
            root: frag,
            fixed,
            top_layer,
            flow_bottom,
            anchors,
            tracks: flow.grid_tracks.into_inner(),
            work,
        }));
    }
    unreachable!("the final layout pass always returns")
}

fn collect_container_sizes(
    dom: &Dom,
    frag: &flow::Frag,
    sizes: &mut rustc_hash::FxHashMap<NodeId, [f32; 2]>,
) {
    if let Some(mut size) = frag.content_size {
        let kind = dom.size_container_kind(frag.node);
        if kind != 0 {
            if kind == 1 {
                size[1] = 0.0;
            }
            sizes.insert(frag.node, size);
        }
    }
    for child in &frag.children {
        collect_container_sizes(dom, child, sizes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_rectangle_index_is_lazy_linear_and_snapshot_owned() {
        let mut html = String::from("<style>body{margin:0}div{width:12px;height:1px}</style>");
        for n in 0..1024 {
            html.push_str(&format!("<div id='n{n}'></div>"));
        }
        let mut dom = Dom::parse_document(&html);
        let base = Url::parse("https://example.com/").unwrap();
        let viewport = Viewport::new(640., 480.);
        let controls = HashMap::new();
        let images = HashMap::new();
        let nodes: Vec<_> = (0..1024)
            .map(|n| dom.get_by_id(&format!("n{n}")).unwrap())
            .collect();
        let measured = measure_retained_layout_for_box(
            &dom,
            &base,
            viewport,
            &[],
            &controls,
            &images,
            Some(nodes[0]),
        );
        assert!(!measured.complete_geometry);
        let fragments = measured.fragments.unwrap();
        assert!(
            fragments.single_boxes.get().is_none(),
            "one rectangle must not eagerly allocate the index"
        );
        let bytes_before = fragments.retained_bytes();
        for _ in 0..3 {
            for (n, &node) in nodes.iter().enumerate() {
                let rect = fragments.single_border_box(node).unwrap();
                assert_eq!(
                    (rect.left, rect.top, rect.width, rect.height),
                    (0., n as f64, 12., 1.)
                );
            }
        }
        let index = fragments.single_boxes.get().unwrap();
        assert_eq!(
            index.visited, 1026,
            "three sweeps must visit each actual fragment only once"
        );
        assert_eq!(
            fragments.retained_bytes() - bytes_before,
            index.retained_bytes()
        );
        let full = fragments.measure_boxes(&dom).0;
        for &node in &nodes {
            assert_eq!(fragments.single_border_box(node), full.get(&node).copied());
        }

        // Old products are immutable even if a consumer still owns them after
        // a new style/layout transaction. The new result starts with no index.
        dom.set_attr(nodes[1], "style", "width:37px;height:5px");
        let changed = measure_retained_layout_for_box(
            &dom,
            &base,
            viewport,
            &[],
            &controls,
            &images,
            Some(nodes[1]),
        );
        assert_eq!(changed.boxes[&nodes[1]].width, 37.);
        let replacement = changed.fragments.unwrap();
        assert!(replacement.single_boxes.get().is_none());
        assert_eq!(replacement.single_border_box(nodes[2]).unwrap().top, 6.);
        assert_eq!(fragments.single_border_box(nodes[2]).unwrap().top, 2.);
        assert_eq!(fragments.single_border_box(nodes[1]).unwrap().width, 12.);
        assert!(!std::ptr::eq(
            index,
            replacement.single_boxes.get().unwrap()
        ));
        let lifetime = Arc::downgrade(&fragments);
        drop(fragments);
        assert!(
            lifetime.upgrade().is_none(),
            "index lifetime must not retain an obsolete layout"
        );
    }

    #[test]
    fn retained_layout_consumes_the_settled_container_query_pass() {
        let html = r#"<style>
            body { margin:0 }
            #container { container-type:inline-size; width:300px }
            #child { width:50px; height:20px }
            @container (width > 200px) { #child { width:120px } }
        </style><div id="container"><div id="child"></div></div>"#;
        let dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let viewport = Viewport::new(640., 480.);
        let controls = HashMap::new();
        let images = HashMap::new();
        let first = measure_retained_layout(&dom, &base, viewport, &[], &controls, &images);
        assert_eq!(
            first.work.passes, 2,
            "initial query environment plus settled result"
        );
        assert_eq!(first.work.query_updates, 1);
        assert_eq!(first.boxes[&dom.get_by_id("child").unwrap()].width, 120.);
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let warm = measure_retained_layout(&dom, &base, viewport, &[], &controls, &images);
        assert_eq!(
            warm.work.passes, 1,
            "stable queries must not add a disposable preflight"
        );
        assert_eq!(first.boxes, warm.boxes);
        let before_paint = layout_pass_count();
        let retained = paint_retained_layout(
            &dom,
            &base,
            &controls,
            &images,
            first.fragments.unwrap(),
            first.boxes,
            first.tracks,
            true,
        );
        assert_eq!(
            layout_pass_count(),
            before_paint,
            "painting must consume existing fragments"
        );
        let cold_dom = Dom::parse_document(html);
        let cold = lay_out_graphical(&cold_dom, &base, viewport, &[], &controls, &images);
        assert!(
            retained.presentation_eq(&cold),
            "retained paint must match a cold full transaction"
        );
    }

    #[test]
    fn retained_layout_nested_queries_and_mutations_match_cold_recomputation() {
        let html = r#"<style>
            body { margin:0 }
            #outer { container-type:inline-size; width:480px }
            #inner { container-type:inline-size; width:100px; height:65px; overflow:auto }
            #grid { display:grid; grid-template-columns:1fr 2fr; width:100%; height:120px }
            #fixed { position:fixed; right:3px; bottom:7px; width:11px; height:13px }
            @container (width > 300px) { #inner { width:350px } }
            @container (width > 200px) { #grid { height:210px } }
        </style><div id="outer"><div id="inner"><div id="grid"><span>alpha</span><span>beta</span></div></div></div><div id="fixed"></div>"#;
        let mut dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let viewport = Viewport::new(640., 480.);
        let controls = HashMap::new();
        let images = HashMap::new();
        let _inputs = crate::layout2::stable_global_layout_inputs();
        for width in [480, 220, 600, 220, 480] {
            let style = format!("width:{width}px");
            dom.set_attr(dom.get_by_id("outer").unwrap(), "style", &style);
            let measured = measure_retained_layout(&dom, &base, viewport, &[], &controls, &images);
            let inner = dom.get_by_id("inner").unwrap();
            assert_eq!(
                measured.boxes[&inner].width,
                if width > 300 { 350. } else { 100. }
            );
            let retained = paint_retained_layout(
                &dom,
                &base,
                &controls,
                &images,
                measured.fragments.unwrap(),
                measured.boxes.clone(),
                measured.tracks.clone(),
                true,
            );
            let mut cold_dom = Dom::parse_document(html);
            cold_dom.set_attr(cold_dom.get_by_id("outer").unwrap(), "style", &style);
            let cold = lay_out_graphical(&cold_dom, &base, viewport, &[], &controls, &images);
            assert!(
                retained.presentation_eq(&cold),
                "presentation changed after width {width}"
            );
            let (boxes, tracks, scrolling) =
                measure_boxes_css(&cold_dom, &base, viewport, &[], &controls, &images);
            assert_eq!(measured.boxes, boxes);
            assert_eq!(measured.tracks, tracks);
            assert_eq!(
                measured.scrolling_areas, scrolling,
                "overflow must come from the settled pass"
            );
            let terminal_viewport = TerminalViewport::new(80, 30, 8.0, 16.0);
            let retained_terminal = adapt_terminal(&retained, terminal_viewport, &HashMap::new());
            let cold_terminal = adapt_terminal(&cold, terminal_viewport, &HashMap::new());
            assert_eq!(
                retained_terminal.rows, cold_terminal.rows,
                "terminal must consume the same fragments"
            );
        }
    }
}
