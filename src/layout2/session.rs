//! One style/layout transaction, shared by CSSOM, observers and painting.
//!
//! CSS Conditional 5 §5.4 requires query changes to participate in the style
//! change event. The final, settled pass is the result, not a disposable
//! preflight followed by another layout. Callers may consume borrowed fragments
//! immediately or retain an immutable tree for later consumers of that result.

use super::*;
use std::sync::Arc;
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

pub(super) struct FragmentLayout<'t> {
    pub root: flow::Frag<'t>,
    pub fixed: Vec<flow::Frag<'t>>,
    pub top_layer: Vec<flow::TopFrag<'t>>,
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
    pub(super) root: flow::Frag<'static>,
    pub(super) fixed: Vec<flow::Frag<'static>>,
    pub(super) top_layer: Vec<flow::TopFrag<'static>>,
    pub(super) flow_bottom: f32,
    pub(super) viewport: Viewport,
    pub(super) anchors: Vec<(NodeId, f32)>,
}

impl LayoutFragments {
    pub(super) fn retain(
        root: &flow::Frag<'_>,
        fixed: &[flow::Frag<'_>],
        top_layer: &[flow::TopFrag<'_>],
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
        }))
    }

    /// Requested-storage lower bound for the host memory inventory. Shaping
    /// and link internals are opaque here, and reported as such by the caller.
    pub(crate) fn retained_bytes(&self) -> usize {
        fn fragment(frag: &flow::Frag<'_>) -> usize {
            let mut bytes = frag.children.capacity() * std::mem::size_of::<flow::Frag<'_>>();
            match &frag.kind {
                flow::FragKind::Line(line) => {
                    bytes += line.pieces.capacity() * std::mem::size_of::<inline::Piece>();
                    for piece in &line.pieces {
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
                    bytes += std::mem::size_of_val(layers.as_ref())
                        + layers.capacity() * std::mem::size_of::<(NodeId, [f32; 4])>();
                }
                _ => {}
            }
            bytes + frag.children.iter().map(fragment).sum::<usize>()
        }
        std::mem::size_of::<Self>()
            + self.fixed.capacity() * std::mem::size_of::<flow::Frag<'_>>()
            + self.top_layer.capacity() * std::mem::size_of::<flow::TopFrag<'_>>()
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

/// Keep the transient box tree alive until the consumer has finished with its
/// fragments. Even an unresolved out-of-flow placeholder keeps the original
/// borrowed full-layout path; inability to retain must never omit content.
pub(super) fn with_layout<R>(
    dom: &Dom,
    base: &Url,
    viewport: Viewport,
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
    finish: impl FnOnce(Option<FragmentLayout<'_>>) -> R,
) -> R {
    let vp = Vp {
        w: viewport.width,
        h: viewport.height,
    };
    let queries = dom.has_container_queries();
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
        if queries && pass < 64 {
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
    frag: &flow::Frag<'_>,
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
