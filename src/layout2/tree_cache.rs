//! Persistent CSS formatting subtrees (CSS Display 3 #box-tree).
//!
//! A hit shares immutable children, not a deep copy or an old DOM. Parent
//! reconstruction still performs anonymous-box generation and itemization.
//! The builder's input/output list state and table nesting are explicit: a
//! sibling's counter mutation may change this subtree without restyling it.

use super::tree::{Atom, AtomKind, BoxNode, Built, Content, Inline};
use crate::dom::NodeId;
use rustc_hash::FxHashMap;
use std::mem::size_of;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 8192;

struct Entry {
    before: Vec<(i64, i64)>,
    after: Vec<(i64, i64)>,
    depth: usize,
    built: Built,
    bytes: usize,
    used: u64,
}

/// Account shared ownership conservatively: a subtree reachable from two
/// entries is counted twice. Thus eviction cannot hide a retained old tree.
/// This is a bounded requested-storage estimate, not allocator/RSS accounting.
#[derive(Default)]
pub(crate) struct BoxTreeCache {
    entries: FxHashMap<NodeId, Entry>,
    bytes: usize,
    clock: u64,
    pub(super) hits: usize,
    pub(super) builds: usize,
}

impl BoxTreeCache {
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    pub(crate) fn invalidate(&mut self, node: NodeId) {
        if let Some(entry) = self.entries.remove(&node) {
            self.bytes -= entry.bytes;
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.bytes + self.entries.capacity() * size_of::<(NodeId, Entry)>()
    }

    pub(super) fn get(
        &mut self,
        node: NodeId,
        lists: &[(i64, i64)],
        depth: usize,
    ) -> Option<(Built, Vec<(i64, i64)>)> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(&node)?;
        if entry.depth != depth || entry.before != lists {
            return None;
        }
        entry.used = self.clock;
        self.hits += 1;
        Some((entry.built.clone(), entry.after.clone()))
    }

    pub(super) fn insert(
        &mut self,
        node: NodeId,
        before: Vec<(i64, i64)>,
        depth: usize,
        after: &[(i64, i64)],
        built: &Built,
    ) {
        self.builds += 1;
        self.invalidate(node);
        let after = after.to_vec();
        let bytes =
            built_bytes(built) + (before.capacity() + after.capacity()) * size_of::<(i64, i64)>();
        if bytes > MAX_BYTES / 4 {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        self.entries.insert(
            node,
            Entry {
                before,
                after,
                depth,
                built: built.clone(),
                bytes,
                used: self.clock,
            },
        );
        self.bytes += bytes;
        while self.retained_bytes() > MAX_BYTES || self.entries.len() > MAX_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.invalidate(oldest);
        }
    }
}

fn atom_bytes(atom: &Atom) -> usize {
    match &atom.kind {
        AtomKind::Img { url, alt, .. } => url.as_ref().map_or(0, String::capacity) + alt.capacity(),
        AtomKind::GeneratedImage { url } => url.capacity(),
        AtomKind::Control { .. } | AtomKind::Media { .. } => 0,
    }
}

fn inline_bytes(inline: &Inline) -> usize {
    match inline {
        Inline::Text(t) => t.capacity(),
        Inline::Box { style, kids, .. } => {
            size_of::<super::style::BoxStyle>()
                + 4 * size_of::<usize>()
                + super::memo::box_style_bytes(style)
                + kids.len() * size_of::<Inline>()
                + kids.iter().map(inline_bytes).sum::<usize>()
        }
        Inline::Atom(atom) => atom_bytes(atom),
        Inline::OutOfFlow(b) | Inline::Float(b) | Inline::AtomBox(b) => box_bytes(b),
        Inline::Br => 0,
    }
}

fn boxes_bytes(boxes: &[super::tree::SharedBox], capacity: usize) -> usize {
    capacity * size_of::<super::tree::SharedBox>()
        + boxes.iter().map(|b| box_bytes(b)).sum::<usize>()
}

fn box_bytes(b: &BoxNode) -> usize {
    size_of::<BoxNode>()
        + 2 * size_of::<usize>()
        + super::memo::box_style_bytes(&b.style)
        + b.marker.as_ref().map_or(0, String::capacity)
        + b.marker_image.as_ref().map_or(0, String::capacity)
        + boxes_bytes(&b.oof, b.oof.capacity())
        + match &b.content {
            Content::Blocks(bs) | Content::Flex(bs) | Content::Grid(bs) => {
                boxes_bytes(bs, bs.capacity())
            }
            Content::Inlines(is) => {
                is.capacity() * size_of::<Inline>() + is.iter().map(inline_bytes).sum::<usize>()
            }
            Content::Atomic(a) => atom_bytes(a),
            Content::Table(t) => {
                size_of::<super::tree::TableBox>()
                    + boxes_bytes(&t.top_captions, t.top_captions.capacity())
                    + boxes_bytes(&t.bottom_captions, t.bottom_captions.capacity())
                    + t.col_specs.capacity() * size_of::<Option<super::tree::ColSpec>>()
                    + t.cells.capacity() * size_of::<super::tree::TableCell>()
                    + t.cells.iter().map(|c| box_bytes(&c.b)).sum::<usize>()
            }
        }
}

fn built_bytes(b: &Built) -> usize {
    match b {
        Built::Block(b) => box_bytes(b),
        Built::Inline(i) => inline_bytes(i),
        Built::Hoist(bs) => {
            bs.capacity() * size_of::<Built>() + bs.iter().map(built_bytes).sum::<usize>()
        }
        Built::Skip => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::Dom;
    use crate::layout2::{
        ControlMap, ImageSizes, LayoutWork, TerminalViewport, Viewport, adapt_terminal,
        measure_retained_layout, paint_retained_layout,
    };
    use std::sync::Arc;
    use url::Url;

    fn measure(dom: &Dom) -> crate::layout2::RetainedMeasurement {
        measure_retained_layout(
            dom,
            &Url::parse("https://example.com/").unwrap(),
            Viewport::new(720., 480.),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
        )
    }

    #[test]
    fn measured_terminal_metadata_preserves_full_adapter_output() {
        let dom = Dom::parse_document(
            r#"<style>
            #hidden { display:none } #contents { display:contents }
            #scroll { overflow:auto; width:160px; height:40px }
            #fixed { position:fixed; top:120px }
            </style><div id=hidden><button>hidden</button><span>hidden</span></div>
            <main id=contents><a id=anchor href='/next'>A link with wrapped text</a>
            <div id=scroll><p>one</p><p>two</p><p>three</p></div>
            <div contenteditable=true>editable <b>text</b></div></main>
            <div id=fixed>fixed</div>"#,
        );
        let base = Url::parse("https://example.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let images = ImageSizes::new();
        let measured = measure_retained_layout(
            &dom,
            &base,
            Viewport::new(720., 480.),
            &forms,
            &controls,
            &images,
        );
        let optimized = paint_retained_layout(
            &dom,
            &base,
            &controls,
            &images,
            measured.fragments.unwrap(),
            measured.boxes,
            measured.tracks,
            true,
        );
        let mut full = optimized.clone();
        let native = paint_retained_layout(
            &dom,
            &base,
            &controls,
            &images,
            optimized.paint_cache.as_ref().unwrap().fragments.clone(),
            optimized.boxes.clone(),
            optimized.grid_tracks.clone(),
            false,
        );
        let mut expected_native = optimized.clone();
        expected_native.paint_cache.as_mut().unwrap().terminal = None;
        assert!(
            native.presentation_eq(&expected_native),
            "native-only painting must preserve all graphics, geometry, and hit-test metadata"
        );
        full.paint_cache.as_mut().unwrap().terminal = Some(
            crate::layout2::terminal::TerminalPaintModel::from_dom(&dom, &base, &controls),
        );
        let viewport = TerminalViewport::new(90, 30, 8., 16.);
        let optimized = adapt_terminal(&optimized, viewport, &Default::default());
        let full = adapt_terminal(&full, viewport, &Default::default());
        assert_eq!(optimized.rows, full.rows);
        assert_eq!(optimized.anchor_rows, full.anchor_rows);
        assert_eq!(optimized.fixed, full.fixed);
        assert_eq!(optimized.regions, full.regions);
    }

    fn equivalent_to_cold(dom: &mut Dom) -> LayoutWork {
        let base = Url::parse("https://example.com/").unwrap();
        let images = ImageSizes::new();
        let warm = measure(dom);
        let work = warm.work;
        let paint = paint_retained_layout(
            dom,
            &base,
            &Default::default(),
            &images,
            warm.fragments.unwrap(),
            warm.boxes.clone(),
            warm.tracks.clone(),
            true,
        );
        dom.force_cold_style_layout_for_test();
        let cold = measure(dom);
        assert_eq!(cold.work.tree_hits, 0);
        assert_eq!(cold.work.item_hits, 0);
        assert_eq!(warm.boxes, cold.boxes, "box geometry");
        assert_eq!(warm.tracks, cold.tracks, "grid tracks");
        assert_eq!(
            warm.scrolling_areas, cold.scrolling_areas,
            "scrolling extents"
        );
        let cold_paint = paint_retained_layout(
            dom,
            &base,
            &Default::default(),
            &images,
            cold.fragments.unwrap(),
            cold.boxes,
            cold.tracks,
            true,
        );
        assert!(
            paint.presentation_eq(&cold_paint),
            "paint / hit-test payload"
        );
        let terminal = TerminalViewport::new(90, 30, 8., 16.);
        assert_eq!(
            adapt_terminal(&paint, terminal, &Default::default()).rows,
            adapt_terminal(&cold_paint, terminal, &Default::default()).rows
        );
        dom.layout_cache.borrow_mut().cold = false;
        work
    }

    fn block(dom: &Dom, name: &str) -> Arc<BoxNode> {
        match &dom.box_tree_cache.borrow().entries[&dom.get_by_id(name).unwrap()].built {
            Built::Block(b) => b.clone(),
            _ => panic!("{name} must generate a block"),
        }
    }

    const HTML: &str = r#"<style>
      body {margin:0} main {display:grid;grid-template-columns:1fr 1fr;gap:9px}
      section {border:2px solid black;padding:5%;min-width:0}
      article {display:flex;gap:3px;flex-wrap:wrap}
      .wide {font-size:25px;padding:9px;color:blue}
    </style><main><section id=changing><article id=list><b id=first>first</b><i id=second>second</i></article><aside id=emptiness>empty state</aside></section>
    <section id=stable><article><a href=/next>linked text</a><span>another item</span></article></section></main>"#;

    #[test]
    fn transition_frames_reuse_independent_subtrees_and_match_cold_layout() {
        let mut dom = Dom::parse_document(HTML);
        let changing = dom.get_by_id("changing").unwrap();
        dom.set_attr(changing, "style", "height:40px;transition:height 1s linear");
        dom.update_css_transitions(0.);
        dom.set_attr(
            changing,
            "style",
            "height:140px;transition:height 1s linear",
        );
        dom.update_css_transitions(1.);
        for time in [1.25, 1.5, 2.] {
            measure(&dom);
            let stable = block(&dom, "stable");
            dom.update_css_transitions(time);
            measure(&dom);
            assert!(Arc::ptr_eq(&stable, &block(&dom, "stable")));
            equivalent_to_cold(&mut dom);
        }
    }

    #[test]
    fn immutable_subtrees_survive_local_attributes_and_child_list_edits() {
        let mut dom = Dom::parse_document(HTML);
        let list = dom.get_by_id("list").unwrap();
        for op in 0..6 {
            measure(&dom);
            let stable = block(&dom, "stable");
            let old_root = Arc::downgrade(&block(&dom, "changing"));
            match op {
                0 => dom.set_attr(list, "class", "wide"),
                1 => dom.remove_attr(list, "class"),
                2 => {
                    let new = dom.create_element("strong");
                    dom.append_text(new, "new item");
                    dom.append(list, new);
                }
                3 => dom.detach(dom.get_by_id("second").unwrap()),
                4 => dom.set_text(
                    dom.get_by_id("first").unwrap(),
                    "much longer first child wrapping now",
                ),
                5 => dom.set_attr(list, "style", "display:block;width:100px"),
                _ => unreachable!(),
            }
            assert!(
                old_root.upgrade().is_none(),
                "invalidated ancestor must not retain an old tree"
            );
            measure(&dom);
            assert!(
                Arc::ptr_eq(&stable, &block(&dom, "stable")),
                "must share, not deep-clone, the independent subtree"
            );
            let work = equivalent_to_cold(&mut dom);
            assert!(work.tree_hits > 0);
        }
    }

    #[test]
    fn structural_selectors_anonymous_boxes_and_reparenting_match_cold() {
        for selector in [
            "#list > :nth-child(2n) {padding:7px}",
            "#list > b + i {font-size:22px}",
            "#list:empty + aside {height:65px}",
            "main:has(#list > strong) #stable {font-size:27px}",
            "#list > :nth-child(2 of :not(.skip)) {border:9px solid blue}",
        ] {
            let html = format!("{HTML}<style>{selector}</style>");
            let mut dom = Dom::parse_document(&html);
            let list = dom.get_by_id("list").unwrap();
            let first = dom.get_by_id("first").unwrap();
            for op in 0..7 {
                measure(&dom);
                match op {
                    0 => {
                        let e = dom.create_element("strong");
                        dom.append_text(e, "inserted");
                        dom.insert_before(list, e, Some(first));
                    }
                    1 => dom.set_attr(list, "style", "display:block"),
                    2 => dom.set_attr(first, "style", "display:block"),
                    3 => dom.set_attr(list, "style", "display:contents"),
                    4 => dom.append(dom.get_by_id("stable").unwrap(), first),
                    5 => dom.set_attr(list, "style", "display:none"),
                    6 => dom.replace_all_children(list, Vec::new()),
                    _ => unreachable!(),
                }
                equivalent_to_cold(&mut dom);
            }
        }
    }

    #[test]
    fn reused_list_subtrees_transfer_counter_state_in_tree_order() {
        let mut dom = Dom::parse_document(
            r#"<style>li {display:list-item} main{display:flex} aside{width:100px}</style>
          <main><ol id=list start=3><li id=a>alpha</li><li id=b>beta<ol reversed><li>nested</li><li>nested two</li></ol></li><li id=c value=20>gamma</li><li id=d>delta</li></ol>
          <aside id=stable>independent</aside></main>"#,
        );
        let list = dom.get_by_id("list").unwrap();
        for (name, attr, value) in [
            ("a", "value", "8"),
            ("a", "style", "display:none"),
            ("a", "style", "display:list-item"),
            ("list", "reversed", ""),
            ("list", "start", "30"),
            ("c", "value", "-3"),
        ] {
            measure(&dom);
            dom.set_attr(dom.get_by_id(name).unwrap(), attr, value);
            equivalent_to_cold(&mut dom);
        }
        measure(&dom);
        let new = dom.create_element("li");
        dom.append_text(new, "last");
        dom.append(list, new);
        equivalent_to_cold(&mut dom);
    }

    #[test]
    fn referenced_svg_mutations_do_not_leave_shared_resources_stale() {
        let mut dom = Dom::parse_document(
            r##"<svg style="display:none"><symbol id="shape" viewBox="0 0 20 20"><path id="path" d="M0 0H20V20Z"/><text id="text">old</text></symbol></svg>
          <main style="display:flex"><section><svg width="40" height="40"><use href="#shape"/></svg></section><section>other</section></main>"##,
        );
        for op in 0..3 {
            measure(&dom);
            match op {
                0 => dom.set_attr(dom.get_by_id("path").unwrap(), "d", "M0 0H10V10Z"),
                1 => dom.set_text(dom.get_by_id("text").unwrap(), "replacement"),
                2 => dom.detach(dom.get_by_id("path").unwrap()),
                _ => unreachable!(),
            }
            equivalent_to_cold(&mut dom);
        }
    }

    #[test]
    fn tree_cache_bounds_and_replacement_account_for_shared_ownership() {
        let mut cache = BoxTreeCache::default();
        let value = Built::Inline(Inline::Text("x".repeat(16384)));
        for node in 0..5000 {
            cache.insert(node, vec![(node as i64, 1)], 0, &[], &value);
            assert!(cache.entries.len() <= MAX_ENTRIES);
            assert!(cache.retained_bytes() <= MAX_BYTES);
        }
        assert!(cache.entries.len() < 5000);
        assert_eq!(
            cache.bytes,
            cache.entries.values().map(|e| e.bytes).sum::<usize>()
        );
        for _ in 0..1000 {
            cache.insert(4999, vec![], 0, &[], &Built::Skip);
        }
        assert!(cache.retained_bytes() <= MAX_BYTES);
        cache.clear();
        assert_eq!(cache.bytes, 0);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn disabled_selectors_keep_local_edits_local_but_expire_first_legend_state() {
        let mut dom = Dom::parse_document(
            r#"<style>
              input:disabled + span {font-size:30px;color:red}
              input:enabled + span {font-size:12px;color:blue}
              a:any-link {padding:2px}
            </style><main><div id=changing>local</div><section id=stable>independent</section>
              <fieldset id=group disabled><legend id=first><input id=a><span>A</span></legend>
                <legend id=second><input id=b><span>B</span></legend></fieldset>
            </main>"#,
        );
        measure(&dom);
        let stable = block(&dom, "stable");
        let child = dom.create_element("span");
        dom.set_text(child, "new");
        dom.append(dom.get_by_id("changing").unwrap(), child);
        measure(&dom);
        assert!(Arc::ptr_eq(&stable, &block(&dom, "stable")));
        equivalent_to_cold(&mut dom);
        let first = dom.get_by_id("first").unwrap();
        let second = dom.get_by_id("second").unwrap();
        let group = dom.get_by_id("group").unwrap();
        for op in 0..3 {
            measure(&dom);
            match op {
                0 => dom.insert_before(group, second, Some(first)),
                1 => dom.detach(second),
                2 => dom.append(second, first),
                _ => unreachable!(),
            }
            equivalent_to_cold(&mut dom);
        }
    }

    #[test]
    fn picture_source_siblings_and_shadow_distribution_expire_affected_trees() {
        let mut dom = Dom::parse_document(
            r#"<picture id=picture><source id=source srcset='a.png 1x'><img id=image width=80 height=40 src=fallback.png></picture>"#,
        );
        let image_source = |dom: &Dom| {
            let cache = dom.box_tree_cache.borrow();
            let Built::Inline(Inline::Atom(Atom {
                kind: AtomKind::Img { url, .. },
                ..
            })) = &cache.entries[&dom.get_by_id("image").unwrap()].built
            else {
                panic!("image atom")
            };
            url.clone().unwrap()
        };
        measure(&dom);
        assert_eq!(image_source(&dom), "https://example.com/a.png");
        dom.set_attr(dom.get_by_id("source").unwrap(), "srcset", "b.png 1x");
        measure(&dom);
        assert_eq!(image_source(&dom), "https://example.com/b.png");
        equivalent_to_cold(&mut dom);
        measure(&dom);
        let earlier = dom.create_element("source");
        dom.set_attr(earlier, "srcset", "first.png 1x");
        dom.insert_before(
            dom.get_by_id("picture").unwrap(),
            earlier,
            dom.get_by_id("source"),
        );
        measure(&dom);
        assert_eq!(image_source(&dom), "https://example.com/first.png");
        equivalent_to_cold(&mut dom);

        let mut dom =
            Dom::parse_document("<div id=host><span id=a slot=one>A</span><b id=b>B</b></div>");
        let host = dom.get_by_id("host").unwrap();
        let shadow = dom.attach_shadow(host);
        let slot = dom.create_element("slot");
        dom.set_attr(slot, "name", "one");
        dom.append(shadow, slot);
        for op in 0..3 {
            measure(&dom);
            match op {
                0 => dom.set_attr(dom.get_by_id("b").unwrap(), "slot", "one"),
                1 => dom.remove_attr(dom.get_by_id("a").unwrap(), "slot"),
                2 => dom.set_attr(slot, "name", "other"),
                _ => unreachable!(),
            }
            equivalent_to_cold(&mut dom);
        }
    }
}
