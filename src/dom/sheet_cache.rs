//! Reuse immutable sheet parses across style-index rebuilds and shadow roots.
//!
//! CSS Syntax 3 #parse-stylesheet, CSSOM #documentorshadowroot and Cascade 5
//! #cascade-order/#layer-ordering: parsing can be shared, but source order,
//! layer context, base URLs and media evaluation still belong to each use.
use super::*;
use std::hash::{Hash, Hasher};

// Past either bound, a miss evicts the sheets the last two index builds did
// not use. Sheets in use stay whatever their size: the live index already
// shares their rules, and a site's main sheet (YouTube's is 3.7 MB) is exactly
// what each rebuild must not parse again.
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 256;

#[derive(Default)]
pub(super) struct Cache {
    entries: Vec<Entry>,
    bytes: usize,
    build: u64,
    #[cfg(test)]
    pub misses: usize,
}

struct Entry {
    hash: u64,
    css: Box<str>,
    base: Option<url::Url>,
    media: MediaEnvironment,
    before: LayerRegistry,
    after: LayerRegistry,
    // Only parser outputs are populated, under DOCUMENT as a temporary scope.
    // The live index installs these into the actual consuming tree scope.
    parsed: StyleIndex,
    fonts: Vec<crate::http::CssFontFace>,
    orders: usize,
    /// The last build that used this entry.
    used: u64,
    bytes: usize,
}

impl Cache {
    pub fn retained_bytes(&self) -> usize {
        self.bytes + self.entries.capacity() * std::mem::size_of::<Entry>()
    }

    /// Starts assembling a new sheet list.
    pub fn begin_build(&mut self) {
        self.build += 1;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &mut self,
        css: &str,
        order: &mut usize,
        out: &mut Vec<StyleRule>,
        keyframes: &mut FxHashMap<String, KeyframesRule>,
        counters: &mut counter_styles::Styles,
        properties: &mut properties::Registry,
        fonts: &mut Vec<crate::http::CssFontFace>,
        base: Option<&url::Url>,
        media: MediaEnvironment,
        layers: &mut LayerRegistry,
    ) {
        let mut hasher = rustc_hash::FxHasher::default();
        css.hash(&mut hasher);
        let hash = hasher.finish();
        // The hash is only a filter. Compare every parser input, including
        // prior layer declarations and anonymous-layer numbering, on a hit.
        let hit = self.entries.iter().position(|entry| {
            entry.hash == hash
                && entry.media == media
                && entry.base.as_ref() == base
                && entry.before == *layers
                && entry.css.as_ref() == css
        });
        let entry = if let Some(hit) = hit {
            self.entries[hit].used = self.build;
            &self.entries[hit]
        } else {
            #[cfg(test)]
            {
                self.misses += 1;
            }
            let mut parsed = StyleIndex::default();
            let mut parsed_fonts = Vec::new();
            let mut after = layers.clone();
            let mut orders = 0;
            parse_sheet(
                css,
                &mut orders,
                parsed.scopes.entry(DOCUMENT).or_default(),
                &mut parsed.keyframes,
                parsed.counter_styles.entry(DOCUMENT).or_default(),
                parsed.properties.entry(DOCUMENT).or_default(),
                &mut parsed_fonts,
                base,
                media,
                &mut after,
                "",
            );
            let mut fresh = Entry {
                hash,
                css: css.into(),
                base: base.cloned(),
                media,
                before: layers.clone(),
                after,
                parsed,
                fonts: parsed_fonts,
                orders,
                used: self.build,
                bytes: 0,
            };
            fresh.bytes = fresh.retained_bytes();
            if self.bytes + fresh.bytes > MAX_BYTES || self.entries.len() >= MAX_ENTRIES {
                let build = self.build;
                self.entries.retain(|entry| entry.used + 1 >= build);
                self.bytes = self.entries.iter().map(|entry| entry.bytes).sum();
            }
            self.bytes += fresh.bytes;
            self.entries.push(fresh);
            self.entries.last().unwrap()
        };
        // Keep use-site source order independent of the cached parse. In
        // particular, inserting/reordering sheets cannot reuse old order keys.
        out.extend(entry.parsed.scopes[&DOCUMENT].iter().map(|rule| StyleRule {
            order: *order + rule.order,
            data: rule.data.clone(),
        }));
        *order += entry.orders;
        keyframes.extend(entry.parsed.keyframes.clone());
        for (name, value) in &entry.parsed.counter_styles[&DOCUMENT] {
            if counters.get(name).is_none_or(|old| value.0 >= old.0) {
                counters.insert(name.clone(), value.clone());
            }
        }
        properties.extend(entry.parsed.properties[&DOCUMENT].clone());
        fonts.extend(entry.fonts.iter().cloned());
        *layers = entry.after.clone();
    }
}

impl Entry {
    fn retained_bytes(&self) -> usize {
        let layers = |registry: &LayerRegistry| {
            registry.paths.capacity() * std::mem::size_of::<(String, Vec<u32>)>()
                + registry
                    .paths
                    .iter()
                    .map(|(name, path)| name.capacity() + path.capacity() * 4)
                    .sum::<usize>()
                + registry.counters.capacity() * std::mem::size_of::<(String, u32)>()
                + registry
                    .counters
                    .keys()
                    .map(String::capacity)
                    .sum::<usize>()
        };
        self.css.len()
            + self.base.as_ref().map_or(0, |base| base.as_str().len())
            + self.parsed.retained_memory().0
            + layers(&self.before)
            + layers(&self.after)
            + self.fonts.capacity() * std::mem::size_of::<crate::http::CssFontFace>()
            + self
                .fonts
                .iter()
                .map(|face| {
                    face.family.capacity()
                        + face.sources.capacity() * std::mem::size_of::<String>()
                        + face.sources.iter().map(String::capacity).sum::<usize>()
                })
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_shadow_sheets_reuse_parses_and_keep_live_cascade_order() {
        let mut dom = Dom::parse_document("<body id=body><div id=outside></div></body>");
        let body = dom.get_by_id("body").unwrap();
        let css = "@layer low, high; @layer low { div {width:10px} } @layer high {div {width:20px}} @media (min-width:1000px) {div {height:30px}}";
        let mut nodes = Vec::new();
        for _ in 0..24 {
            let host = dom.create_element("x-box");
            dom.append(body, host);
            let root = dom.attach_shadow(host);
            let node = dom.create_element("div");
            dom.append(root, node);
            dom.set_adopted_styles(root, css);
            assert_eq!(dom.computed_value(node, "width").as_deref(), Some("20px"));
            nodes.push((root, node));
        }
        assert_eq!(
            dom.parsed_sheets.borrow().misses,
            1,
            "parse once, even across rebuilds"
        );
        assert!(
            dom.computed_value(dom.get_by_id("outside").unwrap(), "width")
                .is_none()
        );
        let (root, node) = nodes[0];
        let sheet = dom.create_element("style");
        dom.set_text(sheet, "@layer high, low;");
        dom.append(root, sheet);
        assert_eq!(
            dom.computed_value(node, "width").as_deref(),
            Some("10px"),
            "prior layer order changes the parse context"
        );
        assert_eq!(
            dom.computed_value(nodes[1].1, "width").as_deref(),
            Some("20px")
        );
        dom.detach(sheet);
        assert_eq!(dom.computed_value(node, "width").as_deref(), Some("20px"));
        dom.set_adopted_styles(root, "div {width:45px}");
        assert_eq!(dom.computed_value(node, "width").as_deref(), Some("45px"));
        dom.set_adopted_styles(root, css);
        dom.set_viewport_px(1200., 600.);
        assert_eq!(dom.computed_value(node, "height").as_deref(), Some("30px"));
        dom.set_viewport_px(800., 600.);
        assert!(dom.computed_value(node, "height").is_none());
        // A later ordinary tree sheet still precedes the adopted sheet.
        dom.set_text(sheet, "div{width:70px}");
        dom.append(root, sheet);
        dom.set_adopted_styles(root, "div{width:80px}");
        assert_eq!(dom.computed_value(node, "width").as_deref(), Some("80px"));
        dom.set_adopted_styles(root, "");
        assert_eq!(dom.computed_value(node, "width").as_deref(), Some("70px"));
    }

    #[test]
    fn sheet_cache_preserves_metadata_bases_density_and_anonymous_layers() {
        let mut cache = Cache::default();
        let css = "@layer {div{width:10px!important}} @property --image {syntax:\"<url>\";inherits:false;initial-value:url(a.png)} @keyframes fade {to{opacity:1}} @font-face {font-family:Test;src:url(font.woff2)} @counter-style badge{system:cyclic;symbols:x} @media (min-resolution:2dppx){div{height:25px}}";
        for (base, density) in [
            ("https://one.test/", 1.),
            ("https://two.test/", 1.),
            ("https://one.test/", 2.),
            ("https://one.test/", 1.),
        ] {
            let base = url::Url::parse(base).unwrap();
            let mut index = StyleIndex::default();
            let mut fonts = Vec::new();
            let mut layers = LayerRegistry::default();
            let mut order = 17;
            for _ in 0..2 {
                cache.append(
                    css,
                    &mut order,
                    index.scopes.entry(DOCUMENT).or_default(),
                    &mut index.keyframes,
                    index.counter_styles.entry(DOCUMENT).or_default(),
                    index.properties.entry(DOCUMENT).or_default(),
                    &mut fonts,
                    Some(&base),
                    MediaEnvironment {
                        viewport: (800., 600.),
                        density,
                        quirks: false,
                    },
                    &mut layers,
                );
            }
            let rules = &index.scopes[&DOCUMENT];
            assert_eq!(rules[0].order, 17);
            assert_eq!(rules.last().unwrap().order + 1, order);
            let anonymous = rules
                .iter()
                .filter(|r| r.decls.iter().any(|(name, _)| name == "width"))
                .collect::<Vec<_>>();
            assert!(anonymous[0].layer_important > anonymous[1].layer_important);
            assert_eq!(
                index.properties[&DOCUMENT]["--image"].base.as_ref(),
                Some(&base)
            );
            assert!(index.keyframes.contains_key("fade"));
            assert!(index.counter_styles[&DOCUMENT].contains_key("badge"));
            assert_eq!(fonts.len(), 2);
            assert_eq!(
                rules
                    .iter()
                    .any(|r| r.decls.iter().any(|(name, _)| name == "height")),
                density == 2.
            );
        }
        assert_eq!(cache.misses, 6);
    }

    #[test]
    fn sheets_in_use_survive_eviction_and_stale_ones_do_not() {
        fn build(cache: &mut Cache, sheets: impl Iterator<Item = String>) {
            cache.begin_build();
            let mut index = StyleIndex::default();
            let mut layers = LayerRegistry::default();
            for css in sheets {
                cache.append(
                    &css,
                    &mut 0,
                    index.scopes.entry(DOCUMENT).or_default(),
                    &mut index.keyframes,
                    index.counter_styles.entry(DOCUMENT).or_default(),
                    index.properties.entry(DOCUMENT).or_default(),
                    &mut Vec::new(),
                    None,
                    MediaEnvironment {
                        viewport: (800., 600.),
                        density: 1.,
                        quirks: false,
                    },
                    &mut layers,
                );
            }
        }
        // More live sheets than MAX_ENTRIES, plus one whose text changes on
        // every rebuild (a style element that keeps receiving rules).
        let live = || (0..MAX_ENTRIES + 40).map(|i| format!(".c{i}{{width:{i}px}}"));
        let mut cache = Cache::default();
        for round in 0..6 {
            build(
                &mut cache,
                live().chain(std::iter::once(format!(".grow{{width:{round}px}}"))),
            );
            assert_eq!(
                cache.misses,
                MAX_ENTRIES + 40 + round + 1,
                "only the changed sheet is parsed again"
            );
            assert!(cache.entries.len() <= MAX_ENTRIES + 40 + 2);
        }
    }
}
