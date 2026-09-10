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
use url::Url;

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
}

impl EmbeddedDocument {
    pub fn new(html: &str, base: Url, viewport: CssSize, resources: ImageStore) -> Self {
        let mut dom = Dom::parse_document(html);
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
        };
        let requests = this.image_requests().to_vec();
        let mut changed = false;
        for request in requests {
            if let Some(image) = this.resources.get(request.handle) {
                this.sizes
                    .insert(request.source, (image.width, image.height));
                changed = true;
            } else if let Some(bytes) = crate::img::decode_data_url(&request.source) {
                if let Ok(image) = crate::img::decode_graphical(&bytes) {
                    this.sizes
                        .insert(request.source, (image.width, image.height));
                    this.resources.insert(request.handle, image);
                    changed = true;
                }
            }
        }
        if changed {
            this.relayout();
        }
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
        self.layout = layout2::lay_out_graphical(
            &self.dom,
            &self.base,
            Viewport::new(self.viewport.width, self.viewport.height),
            &self.forms,
            &self.controls,
            &self.sizes,
        );
    }
    pub fn set_attribute(&mut self, node: NodeId, name: &str, value: &str) {
        self.dom.set_attr(node, name, value);
        (self.forms, self.controls) = crate::http::extract_forms_arena(&self.dom, &self.base, None);
        self.relayout();
    }
    pub fn supply_image(&mut self, source: &str, image: ImageResource) -> bool {
        let requests: Vec<_> = self
            .image_requests()
            .iter()
            .filter(|r| r.source == source)
            .cloned()
            .collect();
        if requests.is_empty() {
            return false;
        }
        self.sizes
            .insert(source.to_owned(), (image.width, image.height));
        for request in requests {
            self.resources.insert(request.handle, image.clone());
        }
        self.relayout();
        true
    }
    pub fn hover(&mut self, node: Option<NodeId>) -> bool {
        if self.dom.set_hover_chain(node) {
            self.relayout();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{PhysicalSize, ScaleFactor};

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
