//! Reuse independent formatting contexts, not their parent's layout decision.
//!
//! Flexbox 1 #layout-algorithm / #flex-item-display and Grid 2
//! #grid-item-display: an item's contents can be reused at identical inputs,
//! but the parent must still determine constraints, placement and baselines.
//! DOM invalidation removes changed branches and their ancestors; unknown
//! dependencies clear this optional cache. No DOM mutation or CSSOM flush is
//! skipped. Unresolved out-of-flow references are never retained.

use super::flow::{Frag, FragKind, GridTrackMap, retain_for_paint};
use super::style::{BoxStyle, InlineStyle};
use super::tree::BoxNode;
use super::value::{Len, Node, Vp};
use super::{ControlMap, Dom, Form, ImageSizes, NodeId};
use rustc_hash::FxHashMap;
use std::mem::size_of;
use url::Url;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 2048;
const MAX_VARIANTS: usize = 8;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Constraint {
    Intrinsic(bool),
    Item {
        width: f32,
        basis: f32,
        height: Option<f32>,
        ratio: bool,
    },
}

pub(super) struct Request<'a> {
    pub node: &'a BoxNode,
    pub parent: &'a InlineStyle,
    pub constraint: Constraint,
}

struct Key {
    style: BoxStyle,
    parent: InlineStyle,
    marker: Option<String>,
    marker_image: Option<String>,
    marker_inside: bool,
    constraint: Constraint,
}

impl Key {
    fn new(request: &Request<'_>) -> Self {
        Self {
            style: request.node.style.clone(),
            parent: request.parent.clone(),
            marker: request.node.marker.clone(),
            marker_image: request.node.marker_image.clone(),
            marker_inside: request.node.marker_inside,
            constraint: request.constraint,
        }
    }

    fn matches(&self, request: &Request<'_>) -> bool {
        self.constraint == request.constraint
            && self.style == request.node.style
            && self.parent == *request.parent
            && self.marker == request.node.marker
            && self.marker_image == request.node.marker_image
            && self.marker_inside == request.node.marker_inside
    }

    fn bytes(&self) -> usize {
        box_style_bytes(&self.style)
            + self.parent.font_family.capacity()
            + self.parent.language.as_ref().map_or(0, String::capacity)
            + self
                .parent
                .link
                .as_ref()
                .map_or(0, |link| link.retained_memory().0)
            + self.marker.as_ref().map_or(0, String::capacity)
            + self.marker_image.as_ref().map_or(0, String::capacity)
    }
}

pub(super) fn box_style_bytes(s: &super::style::BoxStyle) -> usize {
    s.margin
        .iter()
        .chain(&s.padding)
        .chain(&s.inset)
        .chain([
            &s.width,
            &s.min_width,
            &s.max_width,
            &s.height,
            &s.min_height,
            &s.max_height,
        ])
        .map(len_bytes)
        .sum()
}

fn len_bytes(len: &Len) -> usize {
    fn node_bytes(node: &Node) -> usize {
        match node {
            Node::Lin { .. } => 0,
            Node::Min(ns) | Node::Max(ns) => {
                ns.capacity() * size_of::<Node>() + ns.iter().map(node_bytes).sum::<usize>()
            }
            Node::Clamp(a, b, c) => {
                3 * size_of::<Node>() + node_bytes(a) + node_bytes(b) + node_bytes(c)
            }
            Node::Sum(a, b, _) => 2 * size_of::<Node>() + node_bytes(a) + node_bytes(b),
            Node::Scale(a, _) => size_of::<Node>() + node_bytes(a),
        }
    }
    match len {
        Len::Val(node) => node_bytes(node),
        _ => 0,
    }
}

#[derive(Clone)]
pub(super) struct Item {
    pub fragment: Frag<'static>,
    pub anchors: Vec<(NodeId, f32)>,
    pub tracks: GridTrackMap,
}

enum Value {
    Intrinsic(f32),
    Item(Box<Item>),
}

struct Entry {
    key: Key,
    value: Value,
    bytes: usize,
    used: u64,
}

struct Environment {
    base: Url,
    vp: Vp,
    forms: Vec<Form>,
    controls: ControlMap,
    images: ImageSizes,
    font_epoch: u64,
    image_metadata_epoch: u64,
    presentation_epoch: u64,
    paint_epoch: u64,
}

impl Environment {
    fn bytes(&self) -> usize {
        self.base.as_str().len()
            + self.forms.capacity() * size_of::<Form>()
            + self.controls.capacity() * size_of::<(NodeId, (usize, usize))>()
            + self.images.capacity() * size_of::<(String, (u32, u32))>()
            + self.images.keys().map(String::capacity).sum::<usize>()
            + self
                .forms
                .iter()
                .map(|form| {
                    form.action.as_str().len()
                        + form.fields.capacity() * size_of::<crate::doc::Field>()
                        + form
                            .fields
                            .iter()
                            .map(|field| {
                                field.name.capacity()
                                    + field.value.capacity()
                                    + field.default_value.capacity()
                                    + field.label.capacity()
                                    + field.number.as_ref().map_or(0, |number| {
                                        [
                                            &number.min,
                                            &number.max,
                                            &number.step,
                                            &number.value_base,
                                            &number.editing,
                                        ]
                                        .into_iter()
                                        .flatten()
                                        .map(String::capacity)
                                        .sum::<usize>()
                                    })
                                    + match &field.kind {
                                        crate::doc::FieldKind::Select(options) => {
                                            options.capacity() * size_of::<(String, String)>()
                                                + options
                                                    .iter()
                                                    .map(|(a, b)| a.capacity() + b.capacity())
                                                    .sum::<usize>()
                                        }
                                        _ => 0,
                                    }
                            })
                            .sum::<usize>()
                })
                .sum::<usize>()
    }
}

/// Limits cover requested owned payload, not allocator metadata or process-wide
/// shared fonts. Both bytes AND entry count are bounded; a changing constraint
/// cannot retain an unbounded history, even when a hidden page never paints.
#[derive(Default)]
pub(crate) struct LayoutCache {
    #[cfg(test)]
    pub(crate) cold: bool,
    environment: Option<Environment>,
    environment_bytes: usize,
    entries: FxHashMap<NodeId, Vec<Entry>>,
    entry_storage_bytes: usize,
    bytes: usize,
    count: usize,
    clock: u64,
    pub(super) item_hits: usize,
    pub(super) intrinsic_hits: usize,
}

impl LayoutCache {
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.entry_storage_bytes = 0;
        self.bytes = 0;
        self.count = 0;
    }

    pub(crate) fn invalidate(&mut self, node: NodeId) {
        if let Some(entries) = self.entries.remove(&node) {
            self.entry_storage_bytes -= entries.capacity() * size_of::<Entry>();
            self.bytes -= entries.iter().map(|entry| entry.bytes).sum::<usize>();
            self.count -= entries.len();
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.bytes
            + self.entries.capacity() * size_of::<(NodeId, Vec<Entry>)>()
            + self.entry_storage_bytes
            + self.environment_bytes
    }

    pub(super) fn prepare(
        &mut self,
        dom: &Dom,
        base: &Url,
        vp: Vp,
        forms: &[Form],
        controls: &ControlMap,
        images: &ImageSizes,
    ) -> bool {
        self.item_hits = 0;
        self.intrinsic_hits = 0;
        #[cfg(test)]
        if self.cold {
            self.clear();
            dom.box_tree_cache.borrow_mut().clear();
            return false;
        }
        let font_epoch = crate::font_system::page_font_epoch();
        let image_metadata_epoch = crate::img::svg_intrinsic_epoch();
        let presentation_epoch = dom.layout_presentation_epoch();
        let paint_epoch = dom.layout_paint_epoch();
        let same = self.environment.as_ref().is_some_and(|e| {
            e.base == *base
                && e.vp == vp
                && e.forms == forms
                && e.controls == *controls
                && e.images == *images
                && e.font_epoch == font_epoch
                && e.image_metadata_epoch == image_metadata_epoch
                && e.presentation_epoch == presentation_epoch
                && e.paint_epoch == paint_epoch
        });
        if !same {
            self.clear();
            dom.box_tree_cache.borrow_mut().clear();
            self.environment = Some(Environment {
                base: base.clone(),
                vp,
                forms: forms.to_vec(),
                controls: controls.clone(),
                images: images.clone(),
                font_epoch,
                image_metadata_epoch,
                presentation_epoch,
                paint_epoch,
            });
            self.environment_bytes = self.environment.as_ref().map_or(0, Environment::bytes);
            if self.retained_bytes() > MAX_BYTES {
                self.environment = None;
                self.environment_bytes = 0;
                return false;
            }
        }
        true
    }

    fn find(&mut self, request: &Request<'_>) -> Option<&Value> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self
            .entries
            .get_mut(&request.node.node)?
            .iter_mut()
            .find(|entry| entry.key.matches(request))?;
        entry.used = self.clock;
        Some(&entry.value)
    }

    pub(super) fn intrinsic(&mut self, request: &Request<'_>) -> Option<f32> {
        let Value::Intrinsic(value) = self.find(request)? else {
            return None;
        };
        let value = *value;
        self.intrinsic_hits += 1;
        Some(value)
    }

    pub(super) fn item(&mut self, request: &Request<'_>) -> Option<Item> {
        let Value::Item(item) = self.find(request)? else {
            return None;
        };
        let item = (**item).clone();
        self.item_hits += 1;
        Some(item)
    }

    fn insert(&mut self, request: &Request<'_>, value: Value, payload: usize) {
        let key = Key::new(request);
        let bytes = key.bytes() + payload;
        // Avoid making a deep formatting-context tower quadratic in retained
        // memory. Large results simply use the ordinary layout path.
        if bytes > MAX_BYTES / 4 {
            return;
        }
        if self
            .entries
            .get(&request.node.node)
            .is_some_and(|entries| entries.len() >= MAX_VARIANTS)
        {
            self.invalidate(request.node.node);
        }
        while self.count >= MAX_ENTRIES
            || self.retained_bytes() + bytes + size_of::<Entry>() > MAX_BYTES
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entries)| {
                    entries.iter().map(|entry| entry.used).max().unwrap_or(0)
                })
                .map(|(&node, _)| node);
            let Some(node) = oldest else { return };
            self.invalidate(node);
        }
        self.clock = self.clock.wrapping_add(1);
        let entries = self.entries.entry(request.node.node).or_default();
        let old_capacity = entries.capacity();
        entries.push(Entry {
            key,
            value,
            bytes,
            used: self.clock,
        });
        self.entry_storage_bytes += (entries.capacity() - old_capacity) * size_of::<Entry>();
        self.bytes += bytes;
        self.count += 1;
        // Vec/hash-table growth can reserve more than the inserted payload.
        while self.retained_bytes() > MAX_BYTES {
            let Some(node) = self
                .entries
                .iter()
                .min_by_key(|(_, entries)| {
                    entries.iter().map(|entry| entry.used).max().unwrap_or(0)
                })
                .map(|(&node, _)| node)
            else {
                break;
            };
            self.invalidate(node);
        }
    }

    pub(super) fn store_intrinsic(&mut self, request: &Request<'_>, value: f32) {
        self.insert(request, Value::Intrinsic(value), 0);
    }

    pub(super) fn store_item(
        &mut self,
        request: &Request<'_>,
        fragment: &Frag<'_>,
        anchors: &[(NodeId, f32)],
        tracks: &GridTrackMap,
    ) {
        fn portable(fragment: &Frag<'_>) -> bool {
            !matches!(fragment.kind, FragKind::Oof(..) | FragKind::Fixed(_))
                && fragment.children.iter().all(portable)
        }
        if !portable(fragment) {
            return;
        }
        let Some(fragment) = retain_for_paint(fragment) else {
            return;
        };
        let mut retained_tracks = GridTrackMap::new();
        fn collect(frag: &Frag<'_>, tracks: &GridTrackMap, out: &mut GridTrackMap) {
            if let Some(track) = tracks.get(&frag.node) {
                out.insert(frag.node, track.clone());
            }
            for child in &frag.children {
                collect(child, tracks, out);
            }
        }
        collect(&fragment, tracks, &mut retained_tracks);
        let item = Item {
            fragment,
            anchors: anchors.to_vec(),
            tracks: retained_tracks,
        };
        let bytes = size_of::<Item>()
            + fragment_bytes(&item.fragment)
            + item.anchors.capacity() * size_of::<(NodeId, f32)>()
            + item.tracks.capacity() * size_of::<(NodeId, (Vec<f32>, Vec<f32>))>()
            + item
                .tracks
                .values()
                .map(|(cols, rows)| (cols.capacity() + rows.capacity()) * size_of::<f32>())
                .sum::<usize>();
        self.insert(request, Value::Item(Box::new(item)), bytes);
    }
}

fn fragment_bytes(fragment: &Frag<'_>) -> usize {
    let own = match &fragment.kind {
        FragKind::Line(line) => {
            line.pieces.capacity() * size_of::<super::inline::Piece>()
                + line
                    .pieces
                    .iter()
                    .map(super::inline::Piece::retained_bytes)
                    .sum::<usize>()
        }
        FragKind::TableCell(layers) => {
            size_of::<Vec<(NodeId, [f32; 4])>>()
                + layers.capacity() * size_of::<(NodeId, [f32; 4])>()
        }
        _ => 0,
    };
    own + fragment.children.capacity() * size_of::<Frag<'_>>()
        + fragment.children.iter().map(fragment_bytes).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout2::{
        TerminalViewport, Viewport, adapt_terminal, measure_retained_layout, paint_retained_layout,
    };

    const HTML: &str = r#"<style>
        body { margin:0; font-size:16px; --pad:3px }
        main { display:grid; grid-template-columns:auto minmax(0,1fr); width:580px; gap:7px }
        #clock { display:flex; align-items:baseline; max-width:180px }
        #island { display:grid; grid-template-columns:1fr 2fr; gap:5px; padding:var(--pad) }
        .card { border:2px solid red; padding:5%; overflow:auto; height:75px }
        #float { float:left; width:25px; height:37px }
        #atomic { display:inline-flex; border:1px solid black; gap:3px }
        #abs-parent { position:relative; padding:4px }
        #abs { position:absolute; right:0; bottom:0; width:13px; height:17px }
        #fixed { position:fixed; right:5px; bottom:7px; width:11px; height:19px }
        #ratio { width:70%; aspect-ratio:2; background:green }
        img { width:30px; height:auto }
    </style><main>
        <header id=clock><span id=tick>12:00</span></header>
        <section id=island><article class=card><span id=float></span>alpha beta gamma delta epsilon</article>
        <article class=card><a href=/next id=atomic><b>linked</b><span>item</span></a><img src=/image.png></article></section>
        <section id=abs-parent><span>absolute anchor</span><i id=abs></i></section>
        <div id=ratio></div>
    </main><div id=fixed></div><footer id=below>below</footer>"#;

    fn assert_cold(
        dom: &mut Dom,
        base: &Url,
        viewport: Viewport,
        forms: &[Form],
        controls: &ControlMap,
        images: &ImageSizes,
    ) -> (usize, usize) {
        let warm = measure_retained_layout(dom, base, viewport, forms, controls, images);
        let hits = (warm.work.item_hits, warm.work.intrinsic_hits);
        let painted = paint_retained_layout(
            dom,
            base,
            controls,
            images,
            warm.fragments.unwrap(),
            warm.boxes.clone(),
            warm.tracks.clone(),
        );
        dom.force_cold_style_layout_for_test();
        let cold = measure_retained_layout(dom, base, viewport, forms, controls, images);
        assert_eq!((cold.work.item_hits, cold.work.intrinsic_hits), (0, 0));
        assert_eq!(warm.boxes, cold.boxes, "reused border-box geometry");
        assert_eq!(warm.tracks, cold.tracks, "reused grid CSSOM track sizes");
        assert_eq!(
            warm.scrolling_areas, cold.scrolling_areas,
            "reused overflow bounds"
        );
        let cold = paint_retained_layout(
            dom,
            base,
            controls,
            images,
            cold.fragments.unwrap(),
            cold.boxes,
            cold.tracks,
        );
        assert!(
            painted.presentation_eq(&cold),
            "reused display list / hit-test payload"
        );
        let terminal = TerminalViewport::new(80, 30, 8., 16.);
        assert_eq!(
            adapt_terminal(&painted, terminal, &Default::default()).rows,
            adapt_terminal(&cold, terminal, &Default::default()).rows
        );
        dom.layout_cache.borrow_mut().cold = false;
        hits
    }

    #[test]
    fn text_updates_reuse_independent_items_but_reflow_parent_constraints() {
        let mut dom = Dom::parse_document(HTML);
        let base = Url::parse("https://example.com/").unwrap();
        let vp = Viewport::new(640., 480.);
        let controls = ControlMap::new();
        let images = ImageSizes::from([("https://example.com/image.png".to_owned(), (120, 80))]);
        let island = dom.get_by_id("island").unwrap();
        for text in [
            "12:01",
            "a much longer clock wrapping across several lines",
            "x",
            "",
            "12:02",
        ] {
            measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
            assert!(dom.layout_cache.borrow().entries.contains_key(&island));
            dom.set_text(dom.get_by_id("tick").unwrap(), text);
            assert!(
                dom.layout_cache.borrow().entries.contains_key(&island),
                "independent item must survive mutation"
            );
            let hits = assert_cold(&mut dom, &base, vp, &[], &controls, &images);
            assert!(hits.0 + hits.1 > 0, "test must exercise reuse");
        }
    }

    #[test]
    fn cache_inputs_cover_viewport_resources_forms_and_inherited_styles() {
        let mut dom = Dom::parse_document(HTML);
        let base = Url::parse("https://example.com/").unwrap();
        let controls = ControlMap::new();
        let mut images = ImageSizes::new();
        for (width, height) in [(640., 480.), (400., 300.), (900., 600.), (640., 480.)] {
            let vp = Viewport::new(width, height);
            dom.set_viewport_px(width, height);
            measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
            images.insert(
                "https://example.com/image.png".to_owned(),
                (30, height as u32),
            );
            assert_cold(&mut dom, &base, vp, &[], &controls, &images);
            measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
            dom.set_attr(
                dom.get_by_id("island").unwrap(),
                "style",
                "font-size:23px;--pad:9px;opacity:.6",
            );
            assert_cold(&mut dom, &base, vp, &[], &controls, &images);
        }
        let html = "<style>form {display:flex}</style><form><input name=x value=hello></form>";
        let mut dom = Dom::parse_document(html);
        let (mut forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let vp = Viewport::new(640., 480.);
        measure_retained_layout(&dom, &base, vp, &forms, &controls, &images);
        forms[0].fields[0].value = "changed without a DOM attribute write".to_owned();
        assert_cold(&mut dom, &base, vp, &forms, &controls, &images);
    }

    #[test]
    fn container_query_and_empty_selector_changes_match_cold_layout() {
        let html = r#"<style>
            main { display:flex; width:550px }
            #clock { flex:0 1 auto }
            #container { container-type:inline-size; flex:1; min-width:0 }
            #grid { display:grid; grid-template-columns:1fr 2fr; height:30px }
            @container (width > 300px) { #grid { font-size:30px; height:4em } }
            body:has(#clock:empty) #grid { padding:7px; color:red }
        </style><main><div id=clock>tick</div><section id=container><div id=grid><span>A</span><span>B</span></div></section></main>"#;
        let mut dom = Dom::parse_document(html);
        let base = Url::parse("https://example.com/").unwrap();
        let vp = Viewport::new(640., 480.);
        for text in [
            "",
            "a_long_unbreakable_clock_occupying_most_of_the_main",
            "x",
        ] {
            measure_retained_layout(&dom, &base, vp, &[], &ControlMap::new(), &ImageSizes::new());
            dom.set_text(dom.get_by_id("clock").unwrap(), text);
            assert_cold(
                &mut dom,
                &base,
                vp,
                &[],
                &ControlMap::new(),
                &ImageSizes::new(),
            );
        }
    }

    #[test]
    fn svg_metadata_font_and_activation_revisions_expire_layout() {
        let source = "https://metadata.example/layout-reuse.svg";
        let mut dom = Dom::parse_document(&format!(
            "<main style='display:flex'><section id=box><img src='{source}' style='width:40px;height:auto'></section></main>"
        ));
        let base = Url::parse("https://example.com/").unwrap();
        let vp = Viewport::new(640., 480.);
        let controls = ControlMap::new();
        let images = ImageSizes::from([(source.to_owned(), (300, 150))]);
        crate::img::record_svg_intrinsic_metadata(
            source,
            br#"<svg viewBox="0 0 300 150" xmlns="http://www.w3.org/2000/svg"/>"#,
        );
        measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
        crate::img::record_svg_intrinsic_metadata(
            source,
            br#"<svg viewBox="0 0 300 100" xmlns="http://www.w3.org/2000/svg"/>"#,
        );
        assert_cold(&mut dom, &base, vp, &[], &controls, &images);
        measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
        {
            let mut cache = dom.layout_cache.borrow_mut();
            assert!(!cache.entries.is_empty());
            cache.environment.as_mut().unwrap().font_epoch =
                crate::font_system::page_font_epoch().wrapping_sub(1);
            cache.prepare(
                &dom,
                &base,
                Vp { w: 640., h: 480. },
                &[],
                &controls,
                &images,
            );
            assert!(
                cache.entries.is_empty(),
                "font installation invalidates every retained shape"
            );
        }
        measure_retained_layout(&dom, &base, vp, &[], &controls, &images);
        dom.set_render_clickables([dom.get_by_id("box").unwrap()].into_iter().collect(), true);
        assert_cold(&mut dom, &base, vp, &[], &controls, &images);
        crate::img::record_svg_intrinsic_metadata(source, b"not svg");
    }

    #[test]
    fn layout_cache_is_bounded_across_constraint_churn() {
        fn check_inventory(cache: &LayoutCache) {
            assert_eq!(
                cache.entry_storage_bytes,
                cache
                    .entries
                    .values()
                    .map(|entries| entries.capacity() * size_of::<Entry>())
                    .sum::<usize>()
            );
            assert_eq!(
                cache.environment_bytes,
                cache.environment.as_ref().map_or(0, Environment::bytes)
            );
            assert_eq!(
                cache.count,
                cache.entries.values().map(Vec::len).sum::<usize>()
            );
            assert_eq!(
                cache.bytes,
                cache
                    .entries
                    .values()
                    .flatten()
                    .map(|entry| entry.bytes)
                    .sum::<usize>()
            );
        }
        let dom = Dom::parse_document(HTML);
        let base = Url::parse("https://example.com/").unwrap();
        let vp = Vp { w: 640., h: 480. };
        let mut root = super::super::tree::build(&dom, &base, &ControlMap::new(), &[], vp).unwrap();
        let parent = InlineStyle::root();
        let mut cache = LayoutCache::default();
        cache.prepare(&dom, &base, vp, &[], &ControlMap::new(), &ImageSizes::new());
        for width in 0..5000 {
            let request = Request {
                node: &root,
                parent: &parent,
                constraint: Constraint::Item {
                    width: width as f32,
                    basis: 640.,
                    height: None,
                    ratio: true,
                },
            };
            cache.store_intrinsic(&request, width as f32);
            assert!(cache.count <= MAX_VARIANTS);
            assert!(cache.retained_bytes() <= MAX_BYTES);
            check_inventory(&cache);
        }
        cache.invalidate(root.node);
        assert_eq!(cache.count, 0);
        assert_eq!(cache.bytes, 0);
        check_inventory(&cache);
        let mut large_parent = parent;
        large_parent.font_family = "x".repeat(20_000);
        for node in 0..5000 {
            root.node = node;
            let request = Request {
                node: &root,
                parent: &large_parent,
                constraint: Constraint::Intrinsic(true),
            };
            cache.store_intrinsic(&request, 10.);
            assert!(cache.count <= MAX_ENTRIES);
            assert!(cache.retained_bytes() <= MAX_BYTES);
            if node % 100 == 0 {
                check_inventory(&cache);
            }
        }
        assert!(
            cache.count < MAX_ENTRIES,
            "byte budget must evict before the entry limit"
        );
        cache.clear();
        assert_eq!(cache.count, 0);
        assert_eq!(cache.bytes, 0);
        check_inventory(&cache);
    }
}
