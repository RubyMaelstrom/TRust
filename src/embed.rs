//! Script-free, caller-owned HTML surfaces for native applications.
//!
//! This is an embedding adapter to the canonical DOM/layout/display list, not
//! another HTML engine. It does not fetch, navigate, or construct a JS realm.
//! Callers own resource policy and activation. Hit testing follows CSSOM View
//! §5 elementFromPoint using the same transformed and clipped paint as display:
//! https://www.w3.org/TR/cssom-view-1/#dom-document-elementfrompoint
//! Viewport scrolling reuses CSS Overflow 3 §2.3's existing graphical product.

use crate::{
    accessibility::SemanticTree,
    core::{CssPoint, CssSize, ViewportMetrics},
    dom::{Dom, NodeId},
    layout2::{self, ControlMap, GraphicalLayout, ImageSizes, Viewport},
    render::{CssRect, ImageRequest, ImageResource, ImageStore, PageHit, Scene},
};
use std::{collections::HashMap, sync::Arc};
use url::Url;

/// An embedding application's resource policy, checked before recursive layout.
/// HTML's tree-construction introduction permits practical nesting constraints.
/// https://html.spec.whatwg.org/multipage/parsing.html#tree-construction
#[derive(Clone, Copy)]
pub struct DocumentLimits {
    pub bytes: usize,
    pub nodes: usize,
    pub depth: usize,
}

/// Read-only attributes needed by native decorations and interaction code.
pub trait EmbeddedAttributes {
    fn attribute(&self, node: NodeId, name: &str) -> Option<&str>;
}

/// Install an application-owned set of font resources before creating its
/// surfaces. This reuses TRust's CSS Fonts 4 §4.1 family override machinery.
/// The set is process-wide: embedding applications must keep it stable while
/// their surfaces are live and must never fill it from untrusted articles.
pub fn install_application_fonts(fonts: Vec<(String, Vec<u8>)>) {
    crate::font_system::install_page_fonts(
        fonts
            .into_iter()
            .map(|(family, bytes)| crate::font_system::PageFont { family, bytes })
            .collect(),
    );
}

pub struct EmbeddedDocument {
    pub dom: Dom,
    pub layout: GraphicalLayout,
    pub resources: ImageStore,
    base: Url,
    viewport: CssSize,
    sizes: ImageSizes,
    forms: Vec<crate::doc::Form>,
    controls: ControlMap,
    geometry_revision: u64,
}

impl EmbeddedDocument {
    pub fn new(html: &str, base: Url, viewport: CssSize, resources: ImageStore) -> Self {
        Self::from_dom(Dom::parse_document(html), base, viewport, resources)
    }

    pub fn try_new(
        html: &str,
        base: Url,
        viewport: CssSize,
        resources: ImageStore,
        limits: DocumentLimits,
    ) -> Result<Self, &'static str> {
        if html.len() > limits.bytes {
            return Err("This document is too large to display");
        }
        let dom = Dom::parse_document(html);
        if dom.node_count() > limits.nodes {
            return Err("This document contains too many elements to display");
        }
        let mut depths = vec![0usize; dom.node_count()];
        for node in dom.descendants(crate::dom::DOCUMENT) {
            depths[node] = dom.node(node).parent.map_or(0, |p| depths[p] + 1);
            if depths[node] > limits.depth {
                return Err("This document is nested too deeply to display");
            }
        }
        Ok(Self::from_dom(dom, base, viewport, resources))
    }

    fn from_dom(mut dom: Dom, base: Url, viewport: CssSize, resources: ImageStore) -> Self {
        dom.rewrite_inline_svgs(Some(&base));
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let sizes = ImageSizes::new();
        let layout = layout2::lay_out_graphical(
            &dom,
            &base,
            Viewport::new(viewport.width, viewport.height),
            &forms,
            &controls,
            &sizes,
        );
        let mut this = Self {
            dom,
            layout,
            resources,
            base,
            viewport,
            sizes,
            forms,
            controls,
            geometry_revision: 1,
        };
        let requests = this.image_requests().to_vec();
        let mut changed = false;
        for request in requests {
            if let Some(image) = this.resources.get(request.handle) {
                this.sizes
                    .insert(request.source, (image.width, image.height));
                changed = true;
            } else if let Some(bytes) = crate::img::decode_data_url(&request.source)
                && let Ok(image) = crate::img::decode_graphical(&bytes)
            {
                this.sizes
                    .insert(request.source, (image.width, image.height));
                this.resources.insert(request.handle, image);
                changed = true;
            }
        }
        if changed {
            this.relayout();
        }
        this.dom.take_dirty();
        this.dom.take_dirty_targets();
        this
    }
    pub fn base(&self) -> &Url {
        &self.base
    }
    /// Resolve an attribute through the flattened ancestry used by layout.
    pub fn ancestor_attribute(&self, mut node: NodeId, name: &str) -> Option<&str> {
        loop {
            if let Some(value) = self.dom.attr(node, name) {
                return Some(value);
            }
            node = self.dom.parent_flat(node)?;
        }
    }
    pub fn viewport(&self) -> CssSize {
        self.viewport
    }
    pub fn image_requests(&self) -> &[ImageRequest] {
        &self.layout.paint.image_requests
    }
    pub fn resize(&mut self, viewport: CssSize) {
        if self.viewport != viewport {
            self.viewport = viewport;
            self.relayout();
        }
    }
    pub fn relayout(&mut self) {
        let offsets = self.scroll_offsets();
        self.layout = layout2::lay_out_graphical(
            &self.dom,
            &self.base,
            Viewport::new(self.viewport.width, self.viewport.height),
            &self.forms,
            &self.controls,
            &self.sizes,
        );
        self.geometry_revision += 1;
        self.restore_scroll_offsets(&offsets);
        self.dom.take_dirty();
        self.dom.take_dirty_targets();
    }
    fn scroll_offsets(&self) -> HashMap<NodeId, CssPoint> {
        self.layout
            .paint
            .scroll_containers
            .iter()
            .map(|c| (c.node, c.offset))
            .collect()
    }
    fn restore_scroll_offsets(&mut self, offsets: &HashMap<NodeId, CssPoint>) {
        for c in &mut self.layout.paint.scroll_containers {
            if let Some(offset) = offsets.get(&c.node) {
                c.offset = CssPoint::new(
                    offset
                        .x
                        .clamp(0.0, (c.content.width - c.viewport.width).max(0.0)),
                    offset
                        .y
                        .clamp(0.0, (c.content.height - c.viewport.height).max(0.0)),
                );
            }
        }
    }
    pub fn set_attribute(&mut self, node: NodeId, name: &str, value: &str) {
        self.dom.set_attr(node, name, value);
        (self.forms, self.controls) = crate::http::extract_forms_arena(&self.dom, &self.base, None);
        self.relayout();
    }
    pub fn supply_image(&mut self, source: &str, image: ImageResource) -> bool {
        self.supply_images(std::iter::once((source.to_owned(), image))) != 0
    }

    /// Apply one resource transaction. CSS Images 3 #default-sizing requires
    /// new natural dimensions to participate in layout; replacing pixels at
    /// unchanged dimensions needs no new geometry transaction.
    pub fn supply_images(
        &mut self,
        images: impl IntoIterator<Item = (String, ImageResource)>,
    ) -> usize {
        let mut requests: HashMap<&str, Vec<_>> = HashMap::new();
        for request in &self.layout.paint.image_requests {
            requests
                .entry(&request.source)
                .or_default()
                .push(request.handle);
        }
        let mut supplied = 0;
        let mut dimensions_changed = false;
        for (source, image) in images {
            let Some(handles) = requests.get(source.as_str()) else {
                continue;
            };
            let size = (image.width, image.height);
            dimensions_changed |= self.sizes.get(&source) != Some(&size);
            self.sizes.insert(source, size);
            for handle in handles {
                self.resources.insert(*handle, image.clone());
            }
            supplied += 1;
        }
        if dimensions_changed {
            self.relayout();
        }
        supplied
    }
    pub fn hover(&mut self, node: Option<NodeId>) -> bool {
        if self.dom.set_hover_chain(node) {
            let offsets = self.scroll_offsets();
            // Selectors 4 #the-hover-pseudo still uses the canonical flat-tree
            // chain and invalidation. Only the proven paint-only tier can reuse
            // geometry; :hover width/display/opacity changes take full layout.
            let paint_only = self.dom.take_dirty_targets().is_some_and(|targets| {
                !targets.is_empty()
                    && targets
                        .iter()
                        .all(|(_, kind)| matches!(kind, crate::dom::DirtyKind::Paint))
            });
            if !paint_only
                || !layout2::repaint_graphical(
                    &mut self.layout,
                    &self.dom,
                    &self.base,
                    &self.controls,
                    &self.sizes,
                )
            {
                self.relayout();
            }
            self.restore_scroll_offsets(&offsets);
            self.dom.take_dirty();
            true
        } else {
            false
        }
    }
    pub fn clamp_scroll(&self, point: CssPoint) -> CssPoint {
        CssPoint::new(
            point.x.clamp(
                0.0,
                (self.layout.paint.width - self.viewport.width).max(0.0),
            ),
            point.y.clamp(
                0.0,
                (self.layout.paint.height - self.viewport.height).max(0.0),
            ),
        )
    }
    pub fn scene(
        &self,
        metrics: ViewportMetrics,
        rect: CssRect,
        scroll: CssPoint,
        seconds: f32,
    ) -> Scene {
        let mut scene = Scene {
            viewport: metrics,
            primitives: vec![],
            controls: vec![],
            content_viewport: rect,
            image_store: self.resources.clone(),
            canvas_images: Default::default(),
            page_scroll_containers: vec![],
            page_size: CssSize::default(),
        };
        scene.append_page_at(&self.layout.paint, self.clamp_scroll(scroll), seconds);
        scene
    }
    pub fn hit(&self, point: CssPoint, scroll: CssPoint) -> Option<PageHit> {
        crate::render::page_element_hits_at(
            &self.layout.paint,
            self.viewport,
            self.clamp_scroll(scroll),
            point,
        )
        .into_iter()
        .next()
    }
    pub fn semantics(&self, focused: Option<NodeId>) -> SemanticTree {
        SemanticTree::for_document(
            &self.dom,
            &self.layout.boxes,
            &self.forms,
            &self.controls,
            focused,
        )
    }

    /// A transferable presentation, never a second mutable DOM or JS realm.
    /// The document and its Rc-based style/fragment caches stay on their owner
    /// thread. Native callers send mutations back using these stable node IDs.
    pub fn snapshot(&self, semantics: bool) -> EmbeddedSnapshot {
        let mut nodes = vec![SnapshotNode::default(); self.dom.node_count()];
        for id in self.dom.flat_descendants(crate::dom::DOCUMENT) {
            let attrs = match &self.dom.node(id).data {
                crate::dom::NodeData::Element { attrs, .. } => attrs
                    .iter()
                    .map(|a| (a.name.local.to_string(), a.value.to_string()))
                    .collect(),
                _ => Vec::new(),
            };
            nodes[id] = SnapshotNode {
                parent: self.dom.parent_flat(id),
                children: self.dom.flat_children(id),
                tag: self.dom.tag_name(id).map(str::to_owned),
                attrs,
                opacity: self
                    .dom
                    .computed_value(id, "opacity")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1.0),
            };
        }
        nodes[crate::dom::DOCUMENT].children = self.dom.flat_children(crate::dom::DOCUMENT);
        EmbeddedSnapshot {
            dom: SnapshotDom { nodes },
            layout: SnapshotLayout {
                paint: self.layout.paint.clone(),
                boxes: self.layout.boxes.clone(),
            },
            resources: self.resources.clone(),
            base: self.base.clone(),
            viewport: self.viewport,
            semantics: semantics.then(|| Arc::new(self.semantics(None))),
            geometry_revision: self.geometry_revision,
        }
    }
}

impl EmbeddedAttributes for EmbeddedDocument {
    fn attribute(&self, node: NodeId, name: &str) -> Option<&str> {
        self.dom.attr(node, name)
    }
}

#[derive(Clone, Debug, Default)]
pub struct SnapshotNode {
    pub parent: Option<NodeId>,
    children: Vec<NodeId>,
    tag: Option<String>,
    attrs: Vec<(String, String)>,
    pub opacity: f32,
}

#[derive(Clone, Debug)]
pub struct SnapshotDom {
    nodes: Vec<SnapshotNode>,
}

impl SnapshotDom {
    pub fn is_valid(&self, node: NodeId) -> bool {
        node < self.nodes.len()
    }
    pub fn node(&self, node: NodeId) -> &SnapshotNode {
        &self.nodes[node]
    }
    pub fn attr(&self, node: NodeId, name: &str) -> Option<&str> {
        self.nodes
            .get(node)?
            .attrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn tag_name(&self, node: NodeId) -> Option<&str> {
        self.nodes.get(node)?.tag.as_deref()
    }
    pub fn descendants(&self, root: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut stack = vec![root];
        std::iter::from_fn(move || {
            let node = stack.pop()?;
            stack.extend(self.nodes.get(node)?.children.iter().rev().copied());
            Some(node)
        })
    }
}

#[derive(Clone, Debug)]
pub struct SnapshotLayout {
    pub paint: crate::render::PagePaint,
    pub boxes: HashMap<NodeId, layout2::PxRect>,
}

#[derive(Clone, Debug)]
pub struct EmbeddedSnapshot {
    pub dom: SnapshotDom,
    pub layout: SnapshotLayout,
    pub resources: ImageStore,
    pub semantics: Option<Arc<SemanticTree>>,
    /// Changes only when layout geometry is recalculated. Selection offsets
    /// into paint text must not outlive the geometry that produced them.
    pub geometry_revision: u64,
    base: Url,
    viewport: CssSize,
}

impl EmbeddedAttributes for EmbeddedSnapshot {
    fn attribute(&self, node: NodeId, name: &str) -> Option<&str> {
        self.dom.attr(node, name)
    }
}

impl EmbeddedSnapshot {
    pub fn base(&self) -> &Url {
        &self.base
    }
    pub fn viewport(&self) -> CssSize {
        self.viewport
    }
    pub fn image_requests(&self) -> &[ImageRequest] {
        &self.layout.paint.image_requests
    }
    pub fn ancestor_attribute(&self, mut node: NodeId, name: &str) -> Option<&str> {
        loop {
            if let Some(value) = self.dom.attr(node, name) {
                return Some(value);
            }
            node = self.dom.nodes.get(node)?.parent?;
        }
    }
    pub fn clamp_scroll(&self, point: CssPoint) -> CssPoint {
        CssPoint::new(
            point.x.clamp(
                0.0,
                (self.layout.paint.width - self.viewport.width).max(0.0),
            ),
            point.y.clamp(
                0.0,
                (self.layout.paint.height - self.viewport.height).max(0.0),
            ),
        )
    }
    pub fn scene(
        &self,
        metrics: ViewportMetrics,
        rect: CssRect,
        scroll: CssPoint,
        seconds: f32,
    ) -> Scene {
        let mut scene = Scene {
            viewport: metrics,
            primitives: vec![],
            controls: vec![],
            content_viewport: rect,
            image_store: self.resources.clone(),
            canvas_images: Default::default(),
            page_scroll_containers: vec![],
            page_size: CssSize::default(),
        };
        scene.append_page_at(&self.layout.paint, self.clamp_scroll(scroll), seconds);
        scene
    }
    pub fn hit(&self, point: CssPoint, scroll: CssPoint) -> Option<PageHit> {
        crate::render::page_element_hits_at(
            &self.layout.paint,
            self.viewport,
            self.clamp_scroll(scroll),
            point,
        )
        .into_iter()
        .next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{PhysicalSize, ScaleFactor};

    fn document(html: &str) -> EmbeddedDocument {
        EmbeddedDocument::new(
            html,
            Url::parse("https://example.test/").unwrap(),
            CssSize::new(320.0, 180.0),
            ImageStore::default(),
        )
    }

    fn solid_image(width: u32, height: u32) -> ImageResource {
        ImageResource {
            width,
            height,
            rgba: vec![255; (width * height * 4) as usize].into(),
            has_alpha: false,
        }
    }

    #[test]
    fn embedded_hover_reuses_only_proven_geometry_and_preserves_nested_scroll() {
        let mut doc = document(
            "<style>body{margin:0}#color{display:block;width:50px;height:20px;background:red}#color:hover{background:blue}#size{width:50px;height:20px}#size:hover{width:150px}#scroll{width:60px;height:30px;overflow:auto}#wide{width:300px;height:100px}</style><a id='color' href='/story'>link</a><div id='size'></div><div id='scroll'><div id='wide'></div></div>",
        );
        let color = doc.dom.get_by_id("color").unwrap();
        let size = doc.dom.get_by_id("size").unwrap();
        let scroller = doc.dom.get_by_id("scroll").unwrap();
        doc.layout
            .paint
            .scroll_containers
            .iter_mut()
            .find(|c| c.node == scroller)
            .unwrap()
            .offset = CssPoint::new(100.0, 25.0);
        let revision = doc.geometry_revision;
        let boxes = doc.layout.boxes.clone();
        assert!(doc.hover(Some(color)));
        assert_eq!(
            doc.geometry_revision, revision,
            "paint hover avoids flow work"
        );
        assert_eq!(doc.layout.boxes, boxes);
        let metrics =
            ViewportMetrics::from_physical(PhysicalSize::new(320, 180), ScaleFactor::default());
        let scene = doc.scene(
            metrics,
            CssRect::new(0.0, 0.0, 320.0, 180.0),
            CssPoint::default(),
            0.0,
        );
        let frame = crate::render::vello_cpu::VelloCpuRenderer::new()
            .render_rgba(&scene)
            .unwrap();
        assert_eq!(
            &frame.pixels[(10 * 320 + 45) * 4..(10 * 320 + 45) * 4 + 3],
            &[0, 0, 255]
        );
        assert_eq!(
            doc.layout
                .paint
                .scroll_containers
                .iter()
                .find(|c| c.node == scroller)
                .unwrap()
                .offset,
            CssPoint::new(100.0, 25.0)
        );
        let snapshot = doc.snapshot(true);
        let hit = snapshot
            .hit(CssPoint::new(10.0, 10.0), CssPoint::default())
            .unwrap();
        assert_eq!(
            snapshot.ancestor_attribute(hit.node, "href"),
            Some("/story")
        );
        assert!(
            snapshot
                .semantics
                .unwrap()
                .nodes
                .iter()
                .any(|n| n.dom_node == Some(color))
        );
        assert!(doc.hover(Some(size)));
        assert!(
            doc.geometry_revision > revision,
            "geometry hover still lays out"
        );
        assert_eq!(doc.layout.boxes[&size].width, 150.0);
        assert_eq!(
            doc.layout
                .paint
                .scroll_containers
                .iter()
                .find(|c| c.node == scroller)
                .unwrap()
                .offset,
            CssPoint::new(100.0, 25.0)
        );
    }

    #[test]
    fn embedded_image_batch_updates_natural_sizes_once_and_reuses_unchanged_sizes() {
        let mut doc = document(
            "<img id='a' src='/a.png' width='10' height='10'><img id='b' src='/b.png' width='10' height='10'>",
        );
        let a = doc.dom.get_by_id("a").unwrap();
        let b = doc.dom.get_by_id("b").unwrap();
        // Switch the initially requested placeholders to natural sizing in
        // the same transaction that supplies their decoded resources.
        for node in [a, b] {
            doc.dom.remove_attr(node, "width");
            doc.dom.remove_attr(node, "height");
        }
        let revision = doc.geometry_revision;
        let batch = || {
            vec![
                ("https://example.test/a.png".into(), solid_image(40, 20)),
                ("https://example.test/b.png".into(), solid_image(70, 30)),
            ]
        };
        assert_eq!(doc.supply_images(batch()), 2);
        assert_eq!(doc.geometry_revision, revision + 1);
        assert_eq!(doc.layout.boxes[&a].width, 40.0);
        assert_eq!(doc.layout.boxes[&b].height, 30.0);
        assert!(
            doc.image_requests()
                .iter()
                .all(|r| doc.resources.contains(r.handle))
        );
        assert_eq!(doc.supply_images(batch()), 2);
        assert_eq!(
            doc.geometry_revision,
            revision + 1,
            "pixel replacements preserve geometry"
        );
        assert!(!doc.supply_image("https://example.test/unrequested.png", solid_image(3, 3)));
    }

    #[test]
    fn embedded_resource_limits_reject_before_recursive_layout() {
        let limits = DocumentLimits {
            bytes: 1024,
            nodes: 30,
            depth: 16,
        };
        let prepare = |html: &str| {
            EmbeddedDocument::try_new(
                html,
                Url::parse("https://example.test/").unwrap(),
                CssSize::new(320.0, 180.0),
                ImageStore::default(),
                limits,
            )
        };
        assert_eq!(
            prepare(&"x".repeat(1025)).err(),
            Some("This document is too large to display")
        );
        assert_eq!(
            prepare(&"<br>".repeat(40)).err(),
            Some("This document contains too many elements to display")
        );
        assert_eq!(
            prepare(&format!("{}x{}", "<div>".repeat(20), "</div>".repeat(20))).err(),
            Some("This document is nested too deeply to display")
        );
        assert!(prepare("<p>A readable story</p>").is_ok());
    }

    #[test]
    fn table_background_layers_cover_rowspans_but_not_spacing() {
        fn render(extra: &str, rows: &str) -> crate::render::vello_cpu::OwnedRgbaFrame {
            let html = format!(
                "<style>body{{margin:0;background:white}}table{{width:200px;table-layout:fixed;border-collapse:collapse;background:yellow}}td{{padding:0;height:40px}}tbody{{background:blue}}{extra}</style><table><tbody>{rows}</tbody></table>"
            );
            let doc = EmbeddedDocument::new(
                &html,
                Url::parse("https://example.test/").unwrap(),
                CssSize::new(220.0, 120.0),
                ImageStore::default(),
            );
            let metrics =
                ViewportMetrics::from_physical(PhysicalSize::new(220, 120), ScaleFactor::default());
            let scene = doc.scene(
                metrics,
                CssRect::new(0.0, 0.0, 220.0, 120.0),
                CssPoint::default(),
                0.0,
            );
            crate::render::vello_cpu::VelloCpuRenderer::new()
                .render_rgba(&scene)
                .unwrap()
        }
        let frame = render(
            "",
            "<tr style='background:red'><td rowspan=2></td><td style='background:lime'></td></tr><tr><td></td></tr>",
        );
        let pixel = |f: &crate::render::vello_cpu::OwnedRgbaFrame, x: usize, y: usize| -> [u8; 3] {
            f.pixels[(y * 220 + x) * 4..(y * 220 + x) * 4 + 3]
                .try_into()
                .unwrap()
        };
        assert_eq!(pixel(&frame, 5, 5), [255, 0, 0]);
        assert_eq!(pixel(&frame, 105, 5), [0, 255, 0]);
        assert_eq!(
            pixel(&frame, 5, 55),
            [255, 0, 0],
            "row background follows originating rowspan"
        );
        assert_eq!(
            pixel(&frame, 105, 55),
            [0, 0, 255],
            "row-group visible through transparent row/cell"
        );
        let frame = render(
            "table{border-collapse:separate;border-spacing:10px}",
            "<tr style='background:red'><td></td><td></td></tr>",
        );
        assert_eq!(
            pixel(&frame, 100, 15),
            [255, 255, 0],
            "spacing keeps table background"
        );
        assert_eq!(pixel(&frame, 15, 15), [255, 0, 0]);
        let frame = render(
            "",
            "<tr style='background:linear-gradient(90deg,red,blue)'><td></td><td></td></tr>",
        );
        let left = pixel(&frame, 95, 5);
        let right = pixel(&frame, 105, 5);
        assert!(
            left[0].abs_diff(right[0]) < 25,
            "gradient uses the row positioning area, not each cell"
        );
    }

    #[test]
    fn embedded_document_uses_retained_layout_for_scroll_and_selection() {
        let view = EmbeddedDocument::new(
            "<style>body{margin:0}p{height:200px}</style><a href='https://example.test'>A real link</a><p>first</p><p>second</p><script>throw new Error('must never run')</script>",
            Url::parse("https://example.test").unwrap(),
            CssSize::new(320.0, 180.0),
            ImageStore::default(),
        );
        assert!(view.layout.paint.height > 300.0);
        assert!(view.clamp_scroll(CssPoint::new(0.0, 10000.0)).y < view.layout.paint.height);
        let metrics =
            ViewportMetrics::from_physical(PhysicalSize::new(320, 180), ScaleFactor::default());
        let scene = view.scene(
            metrics,
            CssRect::new(0.0, 0.0, 320.0, 180.0),
            CssPoint::default(),
            0.0,
        );
        assert!(!scene.find_text("real link").is_empty());
        assert!(
            view.semantics(None)
                .nodes
                .iter()
                .any(|n| n.role == crate::accessibility::Role::Link)
        );
        assert!(
            crate::render::vello_cpu::VelloCpuRenderer::new()
                .render_rgba(&scene)
                .is_ok()
        );
    }
}
