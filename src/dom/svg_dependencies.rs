//! SVG2 resource dependency proofs and shared formatting-path invalidation.
//!
//! Metadata stores native identities, never host/JS roots. A mutation invalidates
//! every dependent cached result before an observable layout read can occur.

use super::{Dom, NodeData, NodeId};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_CONSUMERS: usize = 2048;
const MAX_PROOF_EDGES: usize = 32 * 1024;

/// A source-subtree proof plus name-binding and explicitly conservative scopes.
/// Scope IDs are native Document/tree identities, never global string IDs.
#[derive(Default)]
pub(super) struct Proof {
    pub sources: Vec<NodeId>,
    pub bindings: Vec<NodeId>,
    pub conservative: Vec<NodeId>,
}

impl Proof {
    fn normalize(&mut self) {
        for ids in [
            &mut self.sources,
            &mut self.bindings,
            &mut self.conservative,
        ] {
            ids.sort_unstable();
            ids.dedup();
        }
    }

    fn edges(&self) -> usize {
        self.sources.len() + self.bindings.len() + self.conservative.len()
    }
}

type Reverse = FxHashMap<NodeId, FxHashSet<NodeId>>;

#[derive(Default)]
pub(super) struct State {
    work: Work,
    consumers: FxHashMap<NodeId, Proof>,
    sources: Reverse,
    bindings: Reverse,
    conservative: Reverse,
    edges: usize,
    /// Metadata overflow changes the proof, not observable rendering: every
    /// mutation must clear all derived caches until complete tracking resumes.
    global_conservative: bool,
    #[cfg(test)]
    limits: Option<(usize, usize)>,
}

#[derive(Default)]
struct Work {
    pending: Vec<NodeId>,
    visited: FxHashSet<NodeId>,
    #[cfg(test)]
    last_visited: usize,
}

impl State {
    pub(super) fn retained_bytes(&self) -> usize {
        self.work.pending.capacity() * std::mem::size_of::<NodeId>()
            + self.work.visited.capacity() * std::mem::size_of::<NodeId>()
            + self.consumers.capacity() * std::mem::size_of::<(NodeId, Proof)>()
            + self
                .consumers
                .values()
                .map(|proof| {
                    (proof.sources.capacity()
                        + proof.bindings.capacity()
                        + proof.conservative.capacity())
                        * std::mem::size_of::<NodeId>()
                })
                .sum::<usize>()
            + [&self.sources, &self.bindings, &self.conservative]
                .into_iter()
                .map(|map| {
                    map.capacity() * std::mem::size_of::<(NodeId, FxHashSet<NodeId>)>()
                        + map
                            .values()
                            .map(|set| set.capacity() * std::mem::size_of::<NodeId>())
                            .sum::<usize>()
                })
                .sum::<usize>()
    }

    /// Called only after both derived caches have been cleared. A cache may
    /// outlive its per-node entry through an ancestor, so proof removal alone
    /// is never sufficient to certify a reusable result.
    pub(super) fn clear_proofs(&mut self) {
        self.consumers.clear();
        self.sources.clear();
        self.bindings.clear();
        self.conservative.clear();
        self.edges = 0;
        self.global_conservative = false;
    }

    pub(super) fn needs_global_invalidation(&self) -> bool {
        self.global_conservative
    }

    /// Whether `consumer`'s dependencies are tracked: by its own proof, or by
    /// the conservative fallback that invalidates everything.
    pub(super) fn tracks(&self, consumer: NodeId) -> bool {
        self.global_conservative || self.consumers.contains_key(&consumer)
    }

    pub(super) fn publish(&mut self, consumer: NodeId, mut proof: Proof) {
        if self.global_conservative {
            return;
        }
        proof.sources.push(consumer);
        proof.normalize();
        self.remove_consumer(consumer);
        let limits = (MAX_CONSUMERS, MAX_PROOF_EDGES);
        #[cfg(test)]
        let limits = self.limits.unwrap_or(limits);
        if self.consumers.len() >= limits.0 || proof.edges() > limits.1.saturating_sub(self.edges) {
            // All existing outputs are correct at publication time: no author
            // code executes inside layout. Before the next mutation returns,
            // this conservative proof forces both derived caches to be cleared.
            self.clear_proofs();
            self.global_conservative = true;
            return;
        }
        for (map, keys) in [
            (&mut self.sources, &proof.sources),
            (&mut self.bindings, &proof.bindings),
            (&mut self.conservative, &proof.conservative),
        ] {
            for &key in keys {
                map.entry(key).or_default().insert(consumer);
            }
        }
        self.edges += proof.edges();
        self.consumers.insert(consumer, proof);
    }

    /// A non-serializing read (for example generated clickable fallback) can
    /// depend on a negative resource lookup too. Preserve any stronger proof
    /// installed by the image builder; narrowing it here could leave that
    /// independently retained image without its CSS/source dependencies.
    pub(super) fn publish_candidate(&mut self, consumer: NodeId, mut proof: Proof) {
        if let Some(previous) = self.consumers.get(&consumer) {
            proof.sources.extend_from_slice(&previous.sources);
            proof.bindings.extend_from_slice(&previous.bindings);
            proof.conservative.extend_from_slice(&previous.conservative);
        }
        self.publish(consumer, proof);
    }

    fn remove_consumer(&mut self, consumer: NodeId) {
        let Some(proof) = self.consumers.remove(&consumer) else {
            return;
        };
        self.edges -= proof.edges();
        for (map, keys) in [
            (&mut self.sources, proof.sources),
            (&mut self.bindings, proof.bindings),
            (&mut self.conservative, proof.conservative),
        ] {
            for key in keys {
                if let Some(users) = map.get_mut(&key) {
                    users.remove(&consumer);
                    if users.is_empty() {
                        map.remove(&key);
                    } else if users.capacity() > users.len().saturating_mul(4).max(16) {
                        users.shrink_to(users.len().saturating_mul(2));
                    }
                }
            }
        }
    }

    pub(super) fn remove_node(&mut self, node: NodeId) -> Vec<NodeId> {
        let mut affected = FxHashSet::default();
        for map in [&self.sources, &self.bindings, &self.conservative] {
            if let Some(users) = map.get(&node) {
                affected.extend(users);
            }
        }
        affected.insert(node);
        for &consumer in &affected {
            self.remove_consumer(consumer);
        }
        affected.into_iter().collect()
    }

    pub(super) fn compact(&mut self, live_nodes: usize) {
        // Empty scratch storage is not a root. Bound retained peak capacity by
        // the current live arena, including after native collection.
        let bound = live_nodes.saturating_mul(4).max(64);
        if self.work.pending.capacity() > bound {
            self.work.pending.shrink_to(live_nodes.max(16));
        }
        if self.work.visited.capacity() > bound {
            self.work.visited.shrink_to(live_nodes.max(16));
        }
        if self.consumers.capacity() > self.consumers.len().saturating_mul(4).max(64) {
            self.consumers
                .shrink_to(self.consumers.len().saturating_mul(2));
        }
        for map in [
            &mut self.sources,
            &mut self.bindings,
            &mut self.conservative,
        ] {
            if map.capacity() > map.len().saturating_mul(4).max(64) {
                map.shrink_to(map.len().saturating_mul(2));
            }
        }
    }
}

impl Dom {
    /// SVG2 #UseShadowTree requires referenced-input changes to reach instances.
    /// Track the serializer's inputs: the outer SVG and every element its
    /// `use` closure imports (`Dom::svg_use_closure`). Resources imported
    /// through `url()` or non-`use` `href` references are watched by the
    /// conservative Document scope their references install below.
    /// Unproven CSS/projection inputs keep explicit conservative Document
    /// watches, not an invented precise proof.
    pub(super) fn svg_input_dependencies(
        &self,
        root: NodeId,
        imported: &[NodeId],
        track_styles: bool,
    ) -> Proof {
        let mut proof = Proof::default();
        let mut visited = FxHashSet::default();
        for source in std::iter::once(root).chain(imported.iter().copied()) {
            proof.sources.push(source);
            for node in std::iter::once(source).chain(self.descendants(source)) {
                if !visited.insert(node) {
                    continue;
                }
                let native = &self.nodes[node];
                // The existing layout serializer can project shadow content or
                // a child Document through a native presentation edge. Source
                // ancestry alone cannot prove those inputs, so retain their
                // true Document identity conservatively. This does not alter
                // the serializer or merge DOM/stylesheet lookup scopes.
                if native.owner_document != self.nodes[source].owner_document
                    || self.shadow_roots.contains_key(&node)
                {
                    proof.conservative.push(native.owner_document);
                }
                let NodeData::Element { name, attrs, .. } = &native.data else {
                    continue;
                };
                // We serialize all supported presentation winners and raw
                // attributes. CSS functions can contain escaped URLs or var()
                // substitutions; tokenization (not substring matching) avoids
                // falsely certifying those inputs as independent of resources.
                let uncertain_css = track_styles
                    && (attrs.iter().any(|attribute| {
                        matches!(
                            attribute.name.local.as_ref(),
                            "style"
                                | "fill"
                                | "stroke"
                                | "clip-path"
                                | "filter"
                                | "mask"
                                | "marker"
                                | "marker-start"
                                | "marker-mid"
                                | "marker-end"
                        ) && css_may_reference_resource(&attribute.value)
                    }) || {
                        let maps = self.cascaded_maps(node);
                        let uncertain = |value: &String| {
                            super::pending_shorthand(value).is_some()
                                || css_may_reference_resource(value)
                        };
                        maps.before
                            .values()
                            .chain(maps.after.values())
                            .any(uncertain)
                            || (maps.elem.values().any(uncertain) && {
                                // The element's own winners reach the resource
                                // only as the declarations the serializer
                                // bakes, after css-values-5 #substitution.
                                // Judge those resolved values: a `var()` that
                                // substitutes a color names no resource, and a
                                // later custom-property change restyles this
                                // node, which reaches the consumer through its
                                // source ancestry like any other style edit
                                // inside the subtree.
                                let mut emitted = self.baked_element_style(node, false);
                                emitted.push_str(&self.svg_resource_style(node));
                                resolved_css_may_reference_resource(&emitted)
                            })
                    });
                if uncertain_css {
                    proof.conservative.push(native.owner_document);
                }
                let Some(href) = self
                    .attr(node, "href")
                    .or_else(|| self.attr(node, "xlink:href"))
                else {
                    continue;
                };
                if &*name.local != "use" {
                    // Paint-server/template/text-path URL semantics may be
                    // interpreted inside the isolated SVG renderer. Until its
                    // complete provenance is exposed, watch the source scope.
                    proof.conservative.push(native.owner_document);
                    continue;
                }
                let Some(fragment) = href.trim().strip_prefix('#') else {
                    proof.conservative.push(native.owner_document);
                    continue;
                };
                if fragment.is_empty() {
                    continue;
                }
                // The `use` binds the first element with the ID in its own
                // tree (DOM getElementById): any insertion, removal, reorder
                // or ID change in that scope can rebind it. Watch misses as
                // well as hits.
                proof.bindings.push(self.tree_scope(node));
            }
        }
        proof
    }

    /// A metadata overflow leaves a conservative proof in place. Revoke every
    /// derived layout/box before completing the next mutation; serialized DOM
    /// getters and resident geometry also observe the mutation/presentation
    /// revision advanced by their caller. Raster data URLs are immutable keys.
    pub(super) fn invalidate_untracked_svg_resources(&self) {
        if self.svg_dependencies.borrow().needs_global_invalidation() {
            self.layout_cache.borrow_mut().clear();
            self.box_tree_cache.borrow_mut().clear();
            self.svg_dependencies.borrow_mut().clear_proofs();
        }
    }

    /// Find direct and indirect users of changed canonical source subtrees.
    /// `binding_change` includes insertion/removal/reorder and ID mutations:
    /// successful lookups as well as unresolved ones must be reconsidered.
    pub(super) fn svg_dirty_consumers(
        &self,
        changed: &[NodeId],
        binding_change: bool,
    ) -> Vec<NodeId> {
        self.invalidate_untracked_svg_resources();
        if self.svg_dependencies.borrow().consumers.is_empty() {
            return Vec::new();
        }
        let state = self.svg_dependencies.borrow();
        let mut pending = Vec::new();
        let mut consumers = FxHashSet::default();
        let mut ancestors = FxHashSet::default();
        let mut scopes = FxHashSet::default();
        let mut binding_ancestors = FxHashSet::default();
        for &node in changed {
            if let Some(users) = state.sources.get(&node) {
                pending.extend(users.iter().copied());
            }
            if let Some(native) = self.nodes.get(node) {
                if scopes.insert(native.owner_document)
                    && let Some(users) = state.conservative.get(&native.owner_document)
                {
                    pending.extend(users.iter().copied());
                }
                if binding_change {
                    // Preserve the actual legacy lookup domain: its native
                    // descendants() can cross iframe presentation edges. Do
                    // not change that behavior here or miss a binding change
                    // it can observe. A detached old Document has no such
                    // parent edge, so unrelated retired contexts stay separate.
                    let mut current = Some(node);
                    while let Some(id) = current {
                        if !binding_ancestors.insert(id) {
                            break;
                        }
                        if let Some(users) = state.bindings.get(&id) {
                            pending.extend(users.iter().copied());
                        }
                        current = self.nodes.get(id).and_then(|native| native.parent);
                    }
                }
            }
        }
        // A local edit also changes every referenced source subtree containing
        // it. Stop at Document/fragment boundaries rather than the presentation
        // iframe parent. Consumers can themselves occur in a referenced source.
        pending.extend(changed.iter().copied());
        while let Some(start) = pending.pop() {
            let mut current = Some(start);
            while let Some(node) = current {
                if !ancestors.insert(node) {
                    break;
                }
                if let Some(users) = state.sources.get(&node) {
                    for &consumer in users {
                        if consumers.insert(consumer) {
                            pending.push(consumer);
                        }
                    }
                }
                let Some(native) = self.nodes.get(node) else {
                    break;
                };
                if matches!(native.data, super::NodeData::Document) {
                    break;
                }
                current = native.parent;
            }
        }
        consumers.into_iter().collect()
    }

    /// CSS Shadow #flattening: invalidate the union of light/composed and
    /// formatting parent paths, once per node for this synchronous batch.
    /// A missing local cache entry is not a reason to stop: a parent can retain
    /// a cached box containing that node. Unrelated shadow roots do not make
    /// an ordinary edge require slot lookup.
    pub(super) fn invalidate_layout_paths(&self, roots: impl IntoIterator<Item = NodeId>) {
        let mut work = std::mem::take(&mut self.svg_dependencies.borrow_mut().work);
        debug_assert!(work.pending.is_empty() && work.visited.is_empty());
        work.pending.extend(roots);
        if work.pending.len() == 1 && self.shadow_roots.is_empty() {
            let mut next = work.pending.pop();
            #[cfg(test)]
            let mut visited = 0;
            while let Some(id) = next {
                let Some(node) = self.nodes.get(id) else {
                    break;
                };
                next = node.parent;
                self.layout_cache.borrow_mut().invalidate(id);
                self.box_tree_cache.borrow_mut().invalidate(id);
                #[cfg(test)]
                {
                    visited += 1;
                }
            }
            #[cfg(test)]
            {
                work.last_visited = visited;
            }
            self.svg_dependencies.borrow_mut().work = work;
            return;
        }
        while let Some(id) = work.pending.pop() {
            let Some(node) = self.nodes.get(id) else {
                continue;
            };
            if !work.visited.insert(id) {
                continue;
            }
            let parent = node.parent;
            let composed = parent.or_else(|| self.shadow_hosts.get(&id).copied());
            // DOM #find-a-slot can return a slot only when the actual light
            // parent hosts a shadow tree. Otherwise both paths commonly merge.
            let flat = parent.and_then(|parent| {
                if self.shadow_roots.contains_key(&parent) {
                    self.assigned_slot(id).or(Some(parent))
                } else {
                    self.shadow_hosts.get(&parent).copied().or(Some(parent))
                }
            });
            self.layout_cache.borrow_mut().invalidate(id);
            self.box_tree_cache.borrow_mut().invalidate(id);
            work.pending.extend(composed);
            if flat != composed {
                work.pending.extend(flat);
            }
        }
        #[cfg(test)]
        {
            work.last_visited = work.visited.len();
        }
        work.visited.clear();
        self.svg_dependencies.borrow_mut().work = work;
        self.svg_dependencies.borrow_mut().compact(self.nodes.len());
    }
}

fn css_may_reference_resource(value: &str) -> bool {
    let mut parser = cssparser::Parser::new(value);
    while let Ok(token) = parser.next_including_whitespace_and_comments() {
        if matches!(
            token,
            cssparser::Token::UnquotedUrl(_) | cssparser::Token::Function(_)
        ) {
            return true;
        }
    }
    false
}

/// Functional notations whose grammars accept no `<url>`, `<image>` or
/// string-as-URL argument: CSS Color 4 colors, CSS Values 4 math, CSS
/// Transforms 1/2, CSS Easing 1 and the Filter Effects 1 filter functions
/// (`drop-shadow()` takes only lengths and a color). Their arguments are still
/// tokenized, so a nested `url()` remains a resource reference.
const RESOURCE_FREE_FUNCTIONS: &[&str] = &[
    "rgb",
    "rgba",
    "hsl",
    "hsla",
    "hwb",
    "lab",
    "lch",
    "oklab",
    "oklch",
    "color",
    "color-mix",
    "calc",
    "min",
    "max",
    "clamp",
    "round",
    "mod",
    "rem",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "pow",
    "sqrt",
    "hypot",
    "log",
    "exp",
    "abs",
    "sign",
    "matrix",
    "matrix3d",
    "translate",
    "translatex",
    "translatey",
    "translatez",
    "translate3d",
    "scale",
    "scalex",
    "scaley",
    "scalez",
    "scale3d",
    "rotate",
    "rotatex",
    "rotatey",
    "rotatez",
    "rotate3d",
    "skew",
    "skewx",
    "skewy",
    "perspective",
    "cubic-bezier",
    "steps",
    "linear",
    "blur",
    "brightness",
    "contrast",
    "drop-shadow",
    "grayscale",
    "hue-rotate",
    "invert",
    "opacity",
    "saturate",
    "sepia",
];

/// `css_may_reference_resource` for values already past `var()` substitution.
/// SVG 2 painting #SpecifyingPaint: a `<paint>` names a paint server only
/// through `<url>`; a color function cannot. Any other function, including an
/// unresolved substitution function, remains a possible reference.
fn resolved_css_may_reference_resource(value: &str) -> bool {
    fn block(parser: &mut cssparser::Parser<'_>) -> bool {
        loop {
            let resource_free = match parser.next_including_whitespace_and_comments() {
                Err(_) => return false,
                Ok(cssparser::Token::UnquotedUrl(_) | cssparser::Token::BadUrl(_)) => {
                    return true;
                }
                Ok(cssparser::Token::Function(name)) => RESOURCE_FREE_FUNCTIONS
                    .iter()
                    .any(|function| name.eq_ignore_ascii_case(function)),
                Ok(
                    cssparser::Token::ParenthesisBlock
                    | cssparser::Token::SquareBracketBlock
                    | cssparser::Token::CurlyBracketBlock,
                ) => true,
                Ok(_) => continue,
            };
            if !resource_free
                || parser
                    .parse_nested_block(|nested| Ok::<_, cssparser::ParseError<()>>(block(nested)))
                    .unwrap_or(true)
            {
                return true;
            }
        }
    }
    block(&mut cssparser::Parser::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout2::{
        ControlMap, ImageSizes, Viewport, measure_retained_layout, paint_retained_layout,
    };

    fn measure(dom: &Dom) -> crate::layout2::RetainedMeasurement {
        measure_retained_layout(
            dom,
            &url::Url::parse("https://example.com/").unwrap(),
            Viewport::new(720., 480.),
            &[],
            &ControlMap::new(),
            &ImageSizes::new(),
        )
    }

    fn warm_parent_cache(dom: &Dom) {
        let resources = || {
            (
                crate::font_system::page_font_epoch(),
                crate::img::svg_intrinsic_epoch(),
                super::super::svg_sprite_revision(),
                crate::img::document_svg_revision(),
            )
        };
        for _ in 0..16 {
            // Parallel tests can install fonts or SVG resources between the two
            // layouts. Those valid global invalidations deliberately clear the
            // formatting cache; require a stable pair before asserting reuse.
            let before = resources();
            measure(dom);
            let warm = measure(dom);
            if before != resources() {
                continue;
            }
            assert!(
                warm.work.tree_hits > 0,
                "fixture must exercise parent cache hits"
            );
            return;
        }
        panic!("resource revisions never settled for cache warmup");
    }

    fn warm_matches_cold(dom: &mut Dom) {
        // Inline SVG sources carry the page's font environment and document
        // image navigation; a parallel page-load test must not change those
        // between the warm paint and its cold reconstruction.
        let _inputs = crate::layout2::stable_global_layout_inputs();
        let base = url::Url::parse("https://example.com/").unwrap();
        let warm = measure(dom);
        let paint = paint_retained_layout(
            dom,
            &base,
            &ControlMap::new(),
            &ImageSizes::new(),
            warm.fragments.unwrap(),
            warm.boxes.clone(),
            warm.tracks.clone(),
            true,
        );
        dom.force_cold_style_layout_for_test();
        let cold = measure(dom);
        assert_eq!(cold.work.tree_hits, 0);
        assert_eq!(warm.boxes, cold.boxes);
        assert_eq!(warm.scrolling_areas, cold.scrolling_areas);
        let fresh = paint_retained_layout(
            dom,
            &base,
            &ControlMap::new(),
            &ImageSizes::new(),
            cold.fragments.unwrap(),
            cold.boxes,
            cold.tracks,
            true,
        );
        assert!(
            paint.presentation_eq(&fresh),
            "cached SVG paint differs from full reconstruction"
        );
        dom.layout_cache.borrow_mut().cold = false;
    }

    fn markup(dom: &Dom, name: &str) -> String {
        let root = dom.get_by_id(name).unwrap();
        dom.svg_render_markup(root, None).expect("renderable SVG")
    }

    fn raster(dom: &Dom, root: NodeId) -> crate::render::ImageResource {
        let (data, _) = dom.svg_image_data(root, None).unwrap();
        let bytes = crate::img::decode_data_url(&data).unwrap();
        crate::img::decode_graphical(&bytes).unwrap()
    }

    fn pixel(image: &crate::render::ImageResource, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * image.width + x) * 4) as usize;
        image.rgba[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn svg_dependency_imports_every_valid_target_transitively_and_detached_lookup_stays_in_its_tree()
     {
        // SVG 2 struct.html #UseElement / linking.html #processingURL-target:
        // each `use` binds the first element with its ID in the use's tree,
        // which must be an SVG element; every such target outside the outer
        // SVG is imported, including targets reached from imported content.
        let fixtures: [(&str, Option<&[&str]>); 6] = [
            (
                r##"<svg><g id="a"><path d="M0 0h7v8z"/></g><circle id="b" r="3"/></svg>
                <svg id="consumer"><use href="#a"/><use href="#b"/></svg>"##,
                Some(&["M0 0h7v8z", "<circle"]),
            ),
            (
                r##"<svg><g id="a"><use href="#b"/></g><path id="b" d="M0 0h7v8z"/></svg>
                <svg id="consumer"><use href="#a"/></svg>"##,
                Some(&[r#"<g id="a">"#, r#"<path id="b""#]),
            ),
            (
                r##"<svg><g id="b"><rect id="same" width="3"/></g><g id="a"><circle id="same" r="2"/></g></svg>
                <svg id="consumer"><use href="#a"/><use href="#b"/><use href="#same"/></svg>"##,
                Some(&[
                    r#"<rect id="trust-ref-0""#,
                    r##"href="#trust-ref-0""##,
                    r#"<circle id="same""#,
                ]),
            ),
            // The first element with the ID is not an SVG element: invalid.
            (
                r##"<div id="same"></div><svg><path id="same" d="M0 0h7v8z"/></svg>
                <svg id="consumer"><use href="#same"/></svg>"##,
                None,
            ),
            (r##"<svg id="consumer"><use href="#missing"/></svg>"##, None),
            (
                r##"<svg id="consumer"><rect width="4" height="3" fill="url(#missing)"/></svg>"##,
                Some(&["url(#missing)"]),
            ),
        ];
        for (fixture, expected) in fixtures {
            let mut dom = Dom::parse_document(fixture);
            let root = dom.get_by_id("consumer").unwrap();
            let rendered = dom.svg_render_markup(root, None);
            match expected {
                Some(needles) => {
                    let markup = rendered.expect(fixture);
                    for needle in needles {
                        assert!(markup.contains(needle), "{needle} in {markup}");
                    }
                }
                None => assert!(rendered.is_none(), "{fixture}"),
            }
            // A detached consumer looks up IDs in its own tree only.
            dom.detach(root);
            assert!(dom.local_svg_use_target(root).is_none(), "{fixture}");
            assert_eq!(
                dom.svg_render_markup(root, None).is_some(),
                fixture.contains("url(#missing)"),
                "detached: {fixture}"
            );
        }
    }

    #[test]
    fn svg_dependency_selected_inputs_render_and_rebuild_after_source_change() {
        let mut dom = Dom::parse_document(
            r##"<svg style="display:none"><g id="a"><rect id="red" width="10" height="10" fill="red"/></g>
            <g id="b"><rect id="blue" width="10" height="10" fill="blue"/></g></svg>
            <svg id="consumer" width="20" height="10" viewBox="0 0 20 10"><use href="#a"/><use href="#b" x="10"/></svg>"##,
        );
        let root = dom.get_by_id("consumer").unwrap();
        let image = raster(&dom, root);
        assert_eq!((image.width, image.height), (20, 10));
        assert_eq!(pixel(&image, 2, 2), [255, 0, 0, 255]);
        assert_eq!(pixel(&image, 12, 2), [0, 0, 255, 255]);
        let state = dom.svg_dependencies.borrow();
        for source in ["a", "b"] {
            assert!(
                state.consumers[&root]
                    .sources
                    .contains(&dom.get_by_id(source).unwrap())
            );
        }
        drop(state);
        dom.set_attr(dom.get_by_id("red").unwrap(), "fill", "lime");
        dom.set_attr(dom.get_by_id("blue").unwrap(), "fill", "yellow");
        let image = raster(&dom, root);
        assert_eq!(pixel(&image, 2, 2), [0, 255, 0, 255]);
        assert_eq!(pixel(&image, 12, 2), [255, 255, 0, 255]);
    }

    #[test]
    fn svg_renderer_invalid_native_first_target_cannot_bind_later_serialized_duplicate() {
        let dom = Dom::parse_document(
            r##"<div id="duplicate"></div>
            <svg id="consumer" width="20" height="10" viewBox="0 0 20 10">
            <defs><path id="duplicate" d="M10 0h10v10H10z" fill="blue"/></defs>
            <rect width="4" height="4" fill="red"/><use href="#duplicate"/></svg>"##,
        );
        let image = raster(&dom, dom.get_by_id("consumer").unwrap());
        assert_eq!(pixel(&image, 2, 2), [255, 0, 0, 255]);
        assert_eq!(
            pixel(&image, 12, 2)[3],
            0,
            "an invalid canonical target must not bind to a later SVG duplicate"
        );
    }

    #[test]
    fn svg_renderer_import_order_cannot_change_canonical_duplicate_target() {
        let dom = Dom::parse_document(
            r##"<svg style="display:none">
            <g id="b"><rect id="duplicate" width="10" height="10" fill="red"/></g>
            <g id="a"><rect id="duplicate" width="10" height="10" fill="blue"/></g></svg>
            <svg id="consumer" width="30" height="10" viewBox="0 0 30 10">
            <use href="#a"/><use href="#b" x="10"/><use href="#duplicate" x="20"/></svg>"##,
        );
        let image = raster(&dom, dom.get_by_id("consumer").unwrap());
        assert_eq!(pixel(&image, 2, 2), [0, 0, 255, 255]);
        assert_eq!(pixel(&image, 12, 2), [255, 0, 0, 255]);
        assert_eq!(
            pixel(&image, 22, 2),
            [255, 0, 0, 255],
            "native first-ID lookup is independent of use/import order"
        );
        // Reversing the reference order imports the duplicate before its
        // container; the container then carries it, still bound once.
        let dom = Dom::parse_document(
            r##"<svg style="display:none">
            <g id="b"><rect id="duplicate" width="10" height="10" fill="red"/></g>
            <g id="a"><rect id="duplicate" width="10" height="10" fill="blue"/></g></svg>
            <svg id="consumer" width="30" height="10" viewBox="0 0 30 10">
            <use href="#duplicate" x="20"/><use href="#b" x="10"/><use href="#a"/></svg>"##,
        );
        let image = raster(&dom, dom.get_by_id("consumer").unwrap());
        assert_eq!(pixel(&image, 2, 2), [0, 0, 255, 255]);
        assert_eq!(pixel(&image, 12, 2), [255, 0, 0, 255]);
        assert_eq!(pixel(&image, 22, 2), [255, 0, 0, 255]);
    }

    #[test]
    fn svg_dependency_lookup_binds_first_id_in_tree_and_href_precedence() {
        // linking.html #processingURL: `xlink:href` applies only without an
        // `href`; the `href` names a `<div>` first, an invalid `use` target.
        let mut dom = Dom::parse_document(
            r##"<div id="duplicate"></div>
            <svg><path id="duplicate" d="M0 0h7v8z"/><path id="valid" d="M0 0h1v1z"/></svg>
            <svg id="consumer"><use href="#duplicate" xlink:href="#valid"/></svg>"##,
        );
        let root = dom.get_by_id("consumer").unwrap();
        assert!(dom.local_svg_use_target(root).is_none());
        assert!(dom.svg_render_markup(root, None).is_none());
        let proof = dom.svg_input_dependencies(root, &[], true);
        assert!(proof.bindings.contains(&super::super::DOCUMENT));
        assert!(dom.svg_dependencies.borrow().consumers.contains_key(&root));
        // Removing the invalid first binding is a binding change that
        // reaches the consumer; the path then binds.
        let div = dom.get_by_id("duplicate").unwrap();
        assert!(dom.svg_dirty_consumers(&[div], true).contains(&root));
        dom.detach(div);
        let selected = dom.local_svg_use_target(root).unwrap();
        assert_eq!(dom.tag_name(selected), Some("path"));
        assert_eq!(dom.attr(selected, "id"), Some("duplicate"));
        let markup = dom.svg_render_markup(root, None).unwrap();
        assert!(markup.contains("M0 0h7v8z"), "{markup}");
        assert!(!markup.contains("M0 0h1v1z"), "{markup}");
    }

    #[test]
    fn svg_dependency_mutation_reaches_users_not_independent_resources() {
        let dom = Dom::parse_document(
            r##"<svg><g id="source"><path id="path" d="M0 0h7v8z"/></g></svg>
            <svg id="first"><use href="#source"/></svg>
            <svg id="second"><use href="#source"/></svg>
            <svg id="independent"><path d="M0 0h2v2z"/></svg>"##,
        );
        for name in ["first", "second", "independent"] {
            markup(&dom, name);
        }
        let path = dom.get_by_id("path").unwrap();
        let users = dom.svg_dirty_consumers(&[path], false);
        assert!(users.contains(&dom.get_by_id("first").unwrap()));
        assert!(users.contains(&dom.get_by_id("second").unwrap()));
        assert!(!users.contains(&dom.get_by_id("independent").unwrap()));
    }

    #[test]
    fn svg_dependency_unresolved_lookup_becomes_live_after_insertion() {
        let mut dom = Dom::parse_document(r##"<svg id="consumer"><use href="#late"/></svg>"##);
        let root = dom.get_by_id("consumer").unwrap();
        assert!(dom.svg_render_markup(root, None).is_none());
        assert!(dom.svg_dependencies.borrow().consumers.contains_key(&root));
        let source = dom.create_element_ns("http://www.w3.org/2000/svg", None, "path");
        dom.set_attr(source, "id", "late");
        dom.set_attr(source, "d", "M0 0h7v8z");
        dom.append(root, source);
        assert!(markup(&dom, "consumer").contains("M0 0h7v8z"));
    }

    #[test]
    fn svg_dependency_candidate_only_reads_publish_negative_proof_without_narrowing_image_proof() {
        let dom = Dom::parse_document(
            r##"<button id="button"><svg id="consumer"><use href="#late"/></svg></button>
            <svg id="painted"><rect fill="url(#paint)" width="5" height="5"/></svg>"##,
        );
        let root = dom.get_by_id("consumer").unwrap();
        assert!(!dom.subtree_paints_icon(dom.get_by_id("button").unwrap()));
        assert!(dom.svg_dependencies.borrow().consumers.contains_key(&root));
        let painted = dom.get_by_id("painted").unwrap();
        assert!(dom.svg_render_markup(painted, None).is_some());
        assert!(
            dom.svg_dependencies.borrow().consumers[&painted]
                .conservative
                .contains(&super::super::DOCUMENT)
        );
        assert!(dom.svg_will_render(painted));
        assert!(
            dom.svg_dependencies.borrow().consumers[&painted]
                .conservative
                .contains(&super::super::DOCUMENT)
        );
        let users = dom.svg_dirty_consumers(&[dom.get_by_id("button").unwrap()], true);
        assert!(users.contains(&root));
    }

    #[test]
    fn svg_dependency_overflow_retains_conservative_proof_until_caches_revoked() {
        let mut dom = Dom::parse_document(
            r#"<svg id="one"><path d="M0 0h1v1z"/></svg><svg id="two"><path d="M0 0h2v2z"/></svg>"#,
        );
        dom.svg_dependencies.get_mut().limits = Some((1, 8));
        markup(&dom, "one");
        markup(&dom, "two");
        assert!(dom.svg_dependencies.borrow().global_conservative);
        assert!(dom.svg_dependencies.borrow().consumers.is_empty());
        let one = dom.get_by_id("one").unwrap();
        dom.set_attr(one, "width", "31");
        assert!(!dom.svg_dependencies.borrow().global_conservative);
        markup(&dom, "one");
        markup(&dom, "two");
        assert!(dom.svg_dependencies.borrow().global_conservative);
        dom.set_attr(one, "width", "32");
        assert!(markup(&dom, "one").contains("32"));
    }

    #[test]
    fn svg_dependency_css_uncertainty_uses_tokens_not_unescaped_substrings() {
        assert!(css_may_reference_resource(r"u\72l(#paint)"));
        assert!(css_may_reference_resource("var(--paint)"));
        assert!(css_may_reference_resource("image-set(url(a.svg) 1x)"));
        assert!(!css_may_reference_resource("#123456"));
    }

    #[test]
    fn svg_dependency_substituted_values_name_resources_only_through_urls() {
        for certain in [
            "fill:#123456;",
            "fill:rgb(230, 230, 230);stroke:hsl(0 0% 50% / .5);",
            "fill:color-mix(in srgb, red 40%, oklch(0.6 0.1 30));",
            "transform:translate(calc(8px * -1)) rotate(-35deg);",
            "transition:fill .3s cubic-bezier(.4, 0, .2, 1);",
            "opacity:0.5;font-family:\"Icons\";",
        ] {
            assert!(!resolved_css_may_reference_resource(certain), "{certain}");
        }
        for uncertain in [
            "fill:url(#paint);",
            r"fill:u\72l(#paint) red;",
            "fill:url('#paint');",
            "fill:var(--unresolved);",
            "fill:rgb(url(#paint));",
            "filter:blur(2px) url(#filter);",
            "mask-image:image-set(url(a.svg) 1x);",
            "background-image:linear-gradient(red, blue);",
            "fill:attr(data-paint);",
            "fill:(url(#paint));",
        ] {
            assert!(
                resolved_css_may_reference_resource(uncertain),
                "{uncertain}"
            );
        }
    }

    #[test]
    fn svg_dependency_substituted_colors_do_not_watch_the_document() {
        let mut dom = Dom::parse_document(
            r##"<style>
            :root { --icon: #e6e6e6; --move: all .3s ease }
            svg path { fill: var(--icon); transition: var(--move) }
            .painted path { fill: var(--paint) }
            .painted { --paint: url(#gradient) }
            .changed { color: red }
            </style><main><section id="icons"><svg id="plain" width="10" height="10">
            <path d="M0 0h7v8z"/></svg></section>
            <section><svg id="painted" class="painted" width="10" height="10">
            <path d="M0 0h7v8z"/></svg></section>
            <svg width="0" height="0"><linearGradient id="gradient">
            <stop offset="0" stop-color="red"/></linearGradient></svg>
            <p id="other">unrelated</p></main>"##,
        );
        let plain = dom.get_by_id("plain").unwrap();
        let painted = dom.get_by_id("painted").unwrap();
        assert!(markup(&dom, "plain").contains("fill:#e6e6e6"));
        assert!(markup(&dom, "painted").contains("url(#gradient)"));
        {
            let state = dom.svg_dependencies.borrow();
            assert!(state.consumers[&plain].conservative.is_empty());
            // A substituted paint-server reference keeps its document watch.
            assert!(
                state.consumers[&painted]
                    .conservative
                    .contains(&super::super::DOCUMENT)
            );
        }
        let other = dom.get_by_id("other").unwrap();
        let users = dom.svg_dirty_consumers(&[other], false);
        assert!(!users.contains(&plain));
        assert!(users.contains(&painted));
        // An unrelated style edit keeps the substituted-color icon's formatting
        // path; the gradient user is still rebuilt.
        warm_parent_cache(&dom);
        let icons = dom.get_by_id("icons").unwrap();
        assert!(dom.layout_cache.borrow().retains(icons));
        dom.set_attr(other, "class", "changed");
        assert!(dom.layout_cache.borrow().retains(icons));
        warm_matches_cold(&mut dom);
        // The custom property itself is a style input of the icon's subtree.
        let html = dom.document_element().unwrap();
        dom.set_attr(html, "style", "--icon: #102030");
        assert!(markup(&dom, "plain").contains("fill:#102030"));
        warm_matches_cold(&mut dom);
        assert!(
            dom.svg_dependencies.borrow().consumers[&plain]
                .conservative
                .is_empty()
        );
        // Substituting a paint server installs the conservative watch again.
        dom.set_attr(html, "style", "--icon: url(#gradient)");
        assert!(markup(&dom, "plain").contains("url(#gradient)"));
        assert!(
            dom.svg_dependencies.borrow().consumers[&plain]
                .conservative
                .contains(&super::super::DOCUMENT)
        );
        warm_matches_cold(&mut dom);
    }

    #[test]
    fn svg_dependency_ancestor_batch_visits_union_once_and_ignores_other_shadow_trees() {
        let mut dom = Dom::parse_document(
            "<main id=parent><svg id=a></svg><svg id=b></svg></main><aside id=host></aside>",
        );
        let a = dom.get_by_id("a").unwrap();
        let b = dom.get_by_id("b").unwrap();
        let mut expected = FxHashSet::default();
        for root in [a, b] {
            let mut next = Some(root);
            while let Some(node) = next {
                expected.insert(node);
                next = dom.parent_composed(node);
            }
        }
        let host = dom.get_by_id("host").unwrap();
        dom.attach_shadow(host);
        dom.invalidate_layout_paths([a, b, a, b]);
        assert_eq!(
            dom.svg_dependencies.borrow().work.last_visited,
            expected.len()
        );
        assert!(dom.svg_dependencies.borrow().work.visited.is_empty());
    }

    #[test]
    fn svg_dependency_ancestor_batch_includes_both_slot_and_light_paths() {
        let mut dom = Dom::parse_document("<main id=host><svg id=svg></svg></main>");
        let host = dom.get_by_id("host").unwrap();
        let svg = dom.get_by_id("svg").unwrap();
        let shadow = dom.attach_shadow(host);
        let wrapper = dom.create_element("section");
        let slot = dom.create_element("slot");
        dom.append(wrapper, slot);
        dom.append(shadow, wrapper);
        let mut expected = FxHashSet::default();
        let mut pending = vec![svg];
        while let Some(id) = pending.pop() {
            if expected.insert(id) {
                pending.extend(dom.parent_composed(id));
                pending.extend(dom.parent_flat(id));
            }
        }
        assert!(expected.contains(&slot) && expected.contains(&wrapper));
        dom.invalidate_layout_paths([svg, svg]);
        assert_eq!(
            dom.svg_dependencies.borrow().work.last_visited,
            expected.len()
        );
    }

    #[test]
    fn svg_dependency_cached_parent_matches_cold_after_shared_resource_mutations() {
        let mut dom = Dom::parse_document(
            r##"<svg style="display:none"><g id="source"><path id="path" d="M0 0h7v8z"/></g></svg>
            <main><section><svg id="first" width="20" height="20"><use href="#source"/></svg></section>
            <section><svg id="second" width="20" height="20"><use href="#source"/></svg></section></main>"##,
        );
        let path = dom.get_by_id("path").unwrap();
        warm_parent_cache(&dom);
        for color in ["red", "blue"] {
            dom.set_attr(path, "fill", color);
            warm_matches_cold(&mut dom);
        }
        let source = dom.get_by_id("source").unwrap();
        dom.detach(source);
        warm_matches_cold(&mut dom);
        assert!(
            dom.local_svg_use_target(dom.get_by_id("first").unwrap())
                .is_none()
        );
        let first = dom.get_by_id("first").unwrap();
        dom.append(first, source);
        warm_matches_cold(&mut dom);
        assert!(dom.local_svg_use_target(first).is_some());
    }

    #[test]
    fn svg_dependency_budget_fallback_revokes_parent_boxes_across_two_rebuilds() {
        let mut dom = Dom::parse_document(
            r##"<main><section><svg id="first" width="20" height="20"><use href="#late"/></svg></section>
            <section><svg id="second" width="20" height="20"><rect id="rect" width="4" height="5"/></svg></section></main>"##,
        );
        dom.svg_dependencies.get_mut().limits = Some((1, 8));
        warm_parent_cache(&dom);
        assert!(dom.svg_dependencies.borrow().global_conservative);
        let rect = dom.get_by_id("rect").unwrap();
        dom.set_attr(rect, "id", "late");
        warm_matches_cold(&mut dom);
        assert!(dom.svg_dependencies.borrow().global_conservative);
        dom.set_attr(rect, "fill", "blue");
        warm_matches_cold(&mut dom);
        assert!(dom.svg_dependencies.borrow().global_conservative);
        dom.set_attr(rect, "id", "gone");
        warm_matches_cold(&mut dom);
    }

    #[test]
    fn svg_dependency_duplicate_id_reorder_and_adoption_replace_the_binding() {
        let mut dom = Dom::parse_document(
            r##"<svg id="sources"><path id="same" d="M0 0h7v8z"/><circle id="same" r="3"/></svg>
            <svg id="consumer"><use href="#same"/></svg>"##,
        );
        let consumer = dom.get_by_id("consumer").unwrap();
        let sources = dom.get_by_id("sources").unwrap();
        let first = dom.get_by_id("same").unwrap();
        let second = dom
            .child_iter(sources)
            .find(|&id| dom.tag_name(id) == Some("circle"))
            .unwrap();
        assert_eq!(dom.local_svg_use_target(consumer), Some(first));
        markup(&dom, "consumer");
        dom.insert_before(sources, second, Some(first));
        assert_eq!(dom.local_svg_use_target(consumer), Some(second));
        let document = dom.parse_document_into("<html><body></body></html>");
        dom.adopt_node(document, second).unwrap();
        assert_eq!(dom.local_svg_use_target(consumer), Some(first));
        dom.adopt_node(document, first).unwrap();
        assert!(dom.local_svg_use_target(consumer).is_none());
    }

    #[test]
    fn svg_dependency_resolution_and_conservative_scopes_do_not_cross_documents_or_shadows() {
        let mut dom = Dom::parse_document(
            r##"<svg><path id="shared" d="M0 0h7v8z"/></svg><svg id="main"><use href="#shared"/></svg>
            <div id="host"></div><iframe id="frame"></iframe>"##,
        );
        let outer = dom.get_by_id("shared").unwrap();
        let host = dom.get_by_id("host").unwrap();
        let shadow = dom.attach_shadow(host);
        let shadow_svg = dom.create_element_ns("http://www.w3.org/2000/svg", None, "svg");
        let shadow_path = dom.create_element_ns("http://www.w3.org/2000/svg", None, "path");
        dom.set_attr(shadow_path, "id", "shared");
        dom.set_attr(shadow_path, "d", "M0 0h2v2z");
        let shadow_use = dom.create_element_ns("http://www.w3.org/2000/svg", None, "use");
        dom.set_attr(shadow_use, "href", "#shared");
        dom.append(shadow_svg, shadow_path);
        dom.append(shadow_svg, shadow_use);
        dom.append(shadow, shadow_svg);
        assert_eq!(dom.local_svg_use_target(shadow_svg), Some(shadow_path));
        assert_eq!(
            dom.local_svg_use_target(dom.get_by_id("main").unwrap()),
            Some(outer)
        );
        let document = dom.parse_document_into(
            r##"<svg id="child"><use href="#shared"/><rect fill="url(#paint)"/></svg>"##,
        );
        let child = dom
            .descendants(document)
            .find(|&id| dom.attr(id, "id") == Some("child"))
            .unwrap();
        // A child starts its own lookup at its Document, as in the C6 helper.
        let frame = dom.get_by_id("frame").unwrap();
        dom.append(frame, document);
        assert!(dom.local_svg_use_target(child).is_none());
        dom.svg_render_markup(child, None);
        let users = dom.svg_dirty_consumers(&[outer], false);
        assert!(!users.contains(&child));
        let users = dom.svg_dirty_consumers(&[child], false);
        assert!(users.contains(&child));
    }

    #[test]
    fn svg_dependency_lookup_stays_in_its_document_while_binding_watches_cover_frames() {
        let mut dom = Dom::parse_document(
            r##"<svg id="consumer"><use href="#late"/></svg><iframe id="frame"></iframe>"##,
        );
        let consumer = dom.get_by_id("consumer").unwrap();
        let frame = dom.get_by_id("frame").unwrap();
        let document = dom.parse_document_into(r##"<svg><path id="other" d="M0 0h3v3z"/></svg>"##);
        let path = dom
            .descendants(document)
            .find(|&id| dom.attr(id, "id") == Some("other"))
            .unwrap();
        dom.append(frame, document);
        assert!(dom.svg_render_markup(consumer, None).is_none());
        // The binding watch conservatively covers the presentation edge into
        // the frame's document.
        assert!(dom.svg_dirty_consumers(&[path], true).contains(&consumer));
        // DOM getElementById searches the consumer's own document only: an
        // element of the frame's document never binds.
        dom.set_attr(path, "id", "late");
        assert!(dom.local_svg_use_target(consumer).is_none());
        assert!(dom.svg_render_markup(consumer, None).is_none());
        dom.detach(document);
        assert!(dom.svg_render_markup(consumer, None).is_none());
        assert!(!dom.svg_dirty_consumers(&[path], true).contains(&consumer));
    }

    #[test]
    fn svg_dependency_projected_shadow_content_keeps_conservative_input_proof() {
        let mut dom = Dom::parse_document(
            r##"<svg><g id="source"><foreignObject><div id="host"></div></foreignObject></g></svg>
            <svg id="consumer"><use href="#source"/></svg>"##,
        );
        let host = dom.get_by_id("host").unwrap();
        let shadow = dom.attach_shadow(host);
        let text = dom.create_element("span");
        dom.append(shadow, text);
        let consumer = dom.get_by_id("consumer").unwrap();
        assert!(dom.svg_render_markup(consumer, None).is_some());
        assert!(
            dom.svg_dependencies.borrow().consumers[&consumer]
                .conservative
                .contains(&super::super::DOCUMENT)
        );
        assert!(dom.svg_dirty_consumers(&[text], false).contains(&consumer));
    }

    #[test]
    fn svg_dependency_metadata_is_non_rooting_and_bounded_after_retirement() {
        let mut dom = Dom::parse_document("<main></main>");
        for _ in 0..100 {
            let svg = dom.create_element_ns("http://www.w3.org/2000/svg", None, "svg");
            let path = dom.create_element_ns("http://www.w3.org/2000/svg", None, "path");
            dom.set_attr(path, "d", "M0 0h1v1z");
            dom.append(svg, path);
            assert!(dom.svg_render_markup(svg, None).is_some());
            dom.sweep_gc_nodes(&|node| node != svg && node != path);
            assert!(!dom.is_valid(svg));
            let state = dom.svg_dependencies.borrow();
            assert!(state.consumers.is_empty());
            assert!(state.sources.is_empty());
            assert_eq!(state.edges, 0);
            assert!(state.consumers.capacity() <= 64);
        }
    }

    #[test]
    fn svg_dependency_external_arrival_invalidates_negative_parent_layout() {
        let mut dom = Dom::parse_document(
            r#"<main><section><svg id="icon" width="20" height="20"><use href="svg-dependency-generation.svg#fresh"/></svg></section></main>"#,
        );
        let url = "https://example.com/svg-dependency-generation.svg";
        warm_parent_cache(&dom);
        let before = super::super::svg_sprite_revision();
        super::super::prime_sprite_sheet(
            url,
            r#"<svg><symbol id="fresh"><rect width="4" height="5"/></symbol></svg>"#,
        );
        assert!(super::super::sprite_sheet_cached(url));
        assert!(super::super::svg_sprite_revision() > before);
        assert_eq!(
            measure(&dom).work.tree_hits,
            0,
            "parent output must not retain the unresolved result"
        );
        warm_matches_cold(&mut dom);
        let symbol = super::super::sprite_symbol_markup(url, "fresh").unwrap();
        super::super::prime_sprite_sheet(
            url,
            r#"<svg><symbol id="fresh"><circle r="99"/></symbol></svg>"#,
        );
        assert_eq!(
            super::super::sprite_symbol_markup(url, "fresh").unwrap(),
            symbol,
            "immutable external records must not be replaced by concurrent/repeated arrivals"
        );
    }
}
