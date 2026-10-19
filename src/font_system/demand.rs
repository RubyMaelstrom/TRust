//! CSS Fonts 4 #font-style-matching, #cluster-matching and
//! #unicode-range-desc; CSS Font Loading 3 #font-face-set-css (CSSWG
//! 81c27f686901). A font is requested by text that can use it, not by
//! enumerating a stylesheet. Retained HTML/canvas/SVG font environments
//! share this journal with their owning document's loader.

use super::display::{Phase, Timeline};
use super::*;
use parley::fontique::{Attributes, FontStyle, FontWeight, FontWidth, QueryFamily, QueryStatus};
use std::{ops::Range, time::Instant};
use unicode_segmentation::UnicodeSegmentation as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FontKey {
    pub scope: usize,
    pub member: u64,
    pub script: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct FontCandidate {
    pub key: FontKey,
    pub face: Arc<crate::http::CssFontFace>,
    /// Decoded data, with the face's effective character map applied.
    pub data: Option<Arc<[u8]>>,
    pub failed: bool,
    /// CSS sources also have a loader when JavaScript is disabled.
    pub css: bool,
}

/// One member of a Document's FontFaceSet. CSS members are matched back to
/// their tree-scoped rule; manual members are appended after those rules.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FontRegistration {
    pub key: FontKey,
    pub face: crate::http::CssFontFace,
    pub css: bool,
    pub failed: bool,
    pub data: Option<Arc<[u8]>>,
}

#[derive(Default)]
struct Journal {
    active: HashMap<FontKey, FontCandidate>,
    used: HashSet<FontKey>,
    pending: Vec<FontKey>,
    timelines: HashMap<FontKey, Timeline>,
    dirty: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct FontRequest {
    pub document: usize,
    pub url: url::Url,
}

struct WakeTimer {
    at: Instant,
    task: tokio::task::AbortHandle,
}
impl Drop for WakeTimer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
pub(crate) struct FontDemands {
    journal: Mutex<Journal>,
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    revision: AtomicU64,
    timer: Mutex<Option<WakeTimer>>,
}

impl FontDemands {
    #[cfg(test)]
    pub(crate) fn elapse(&self, duration: std::time::Duration) {
        let mut journal = self.journal.lock().unwrap();
        for timer in journal.timelines.values_mut() {
            timer.elapse(duration);
        }
        if Self::advance_locked(&mut journal, Instant::now()) {
            self.changed(&mut journal);
        }
    }

    pub(crate) fn set_active(&self, sources: Vec<FontCandidate>) {
        let now = Instant::now();
        let mut journal = self.journal.lock().unwrap();
        // The first layout can precede creation of CSS-connected FontFace
        // objects. Transfer its timer to the member identity without starting
        // over; reverse rule order also distinguishes identical declarations.
        let mut migrated = HashSet::new();
        for source in sources.iter().rev().filter(|s| s.key.script && s.css) {
            if journal.active.contains_key(&source.key) {
                continue;
            }
            let old = journal
                .active
                .values()
                .filter(|old| {
                    !old.key.script
                        && old.css
                        && old.key.scope == source.key.scope
                        && old.face == source.face
                        && !migrated.contains(&old.key)
                })
                .max_by_key(|old| old.key.member)
                .map(|old| old.key);
            if let Some(old) = old {
                migrated.insert(old);
                if let Some(timer) = journal.timelines.remove(&old) {
                    journal.timelines.insert(source.key, timer);
                }
                if journal.used.remove(&old) {
                    journal.used.insert(source.key);
                }
                for pending in &mut journal.pending {
                    if *pending == old {
                        *pending = source.key;
                    }
                }
            }
        }
        let active: HashMap<_, _> = sources
            .into_iter()
            .map(|source| (source.key, source))
            .collect();
        let reset: Vec<_> = journal
            .active
            .iter()
            .filter_map(|(key, old)| {
                active
                    .get(key)
                    .filter(|new| old.face.sources != new.face.sources)
                    .map(|_| *key)
            })
            .collect();
        for key in reset {
            journal.timelines.remove(&key);
            journal.used.remove(&key);
        }
        journal.used.retain(|key| active.contains_key(key));
        journal.pending.retain(|key| active.contains_key(key));
        journal.timelines.retain(|key, _| active.contains_key(key));
        journal.active = active;
        let changed = Self::advance_locked(&mut journal, now);
        if changed {
            self.changed(&mut journal);
        }
        drop(journal);
        if changed {
            self.notify();
        }
    }

    pub(crate) fn set_waker(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.wake.lock().unwrap().get_or_insert(wake);
    }

    fn notify(&self) {
        let wake = self.wake.lock().unwrap().clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    fn changed(&self, journal: &mut Journal) {
        journal.dirty = true;
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    fn advance_locked(journal: &mut Journal, now: Instant) -> bool {
        let Journal {
            active, timelines, ..
        } = journal;
        let mut changed = false;
        for (key, timer) in timelines {
            if let Some(source) = active.get(key) {
                changed |= timer.update(
                    source.face.descriptors.display,
                    source.data.is_some(),
                    source.failed,
                    now,
                );
            }
        }
        changed
    }

    pub(crate) fn advance(&self, now: Instant) {
        let mut journal = self.journal.lock().unwrap();
        if Self::advance_locked(&mut journal, now) {
            self.changed(&mut journal);
        }
    }

    pub(crate) fn take_changed(&self) -> bool {
        std::mem::take(&mut self.journal.lock().unwrap().dirty)
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    pub(crate) fn failed(&self, key: FontKey) -> bool {
        self.journal
            .lock()
            .unwrap()
            .timelines
            .get(&key)
            .is_some_and(|t| t.phase == Phase::Failed)
    }

    /// At most one sleeping task per arena. A new earlier deadline replaces
    /// the old task; ready/failed faces have no timer and there is no polling.
    /// Dropping the journal cancels the task, whose callback owns no journal.
    pub(crate) fn arm_timer(&self, handle: &tokio::runtime::Handle) {
        let next = self
            .journal
            .lock()
            .unwrap()
            .timelines
            .values()
            .filter_map(Timeline::next)
            .min();
        let mut timer = self.timer.lock().unwrap();
        if timer.as_ref().map(|timer| timer.at) == next {
            return;
        }
        *timer = None;
        if let Some(at) = next {
            let wake = self.wake.lock().unwrap().clone();
            if let Some(wake) = wake {
                let task = handle.spawn(async move {
                    tokio::time::sleep_until(at.into()).await;
                    wake();
                });
                *timer = Some(WakeTimer {
                    at,
                    task: task.abort_handle(),
                });
            }
        }
    }

    fn request(&self, key: FontKey, canvas: bool) -> Phase {
        let mut journal = self.journal.lock().unwrap();
        let Some(source) = journal.active.get(&key) else {
            return Phase::Failed;
        };
        let (loaded, failed, display) = (
            source.data.is_some(),
            source.failed,
            source.face.descriptors.display,
        );
        let requested = journal.used.insert(key);
        if requested {
            journal.pending.push(key);
        }
        let now = Instant::now();
        let mut changed = false;
        if !canvas && !loaded && !failed && !journal.timelines.contains_key(&key) {
            journal.timelines.insert(key, Timeline::new(display, now));
            changed = true;
        }
        let phase = if canvas {
            if failed {
                Phase::Failed
            } else if loaded {
                Phase::Ready
            } else {
                Phase::Swap
            }
        } else if let Some(timer) = journal.timelines.get_mut(&key) {
            changed |= timer.update(display, loaded, failed, now);
            timer.phase
        } else if failed {
            Phase::Failed
        } else {
            Phase::Ready
        };
        if changed {
            self.changed(&mut journal);
        }
        drop(journal);
        if requested || changed {
            self.notify();
        }
        phase
    }

    pub(crate) fn was_used(&self, key: FontKey) -> bool {
        self.journal.lock().unwrap().used.contains(&key)
    }

    pub(crate) fn take_pending(&self) -> Vec<FontCandidate> {
        let mut journal = self.journal.lock().unwrap();
        std::mem::take(&mut journal.pending)
            .into_iter()
            .filter_map(|key| journal.active.get(&key).cloned())
            .collect()
    }

    pub(crate) fn used_sources(&self) -> Vec<FontCandidate> {
        let journal = self.journal.lock().unwrap();
        journal
            .active
            .values()
            .filter(|source| journal.used.contains(&source.key))
            .cloned()
            .collect()
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        let journal = self.journal.lock().unwrap();
        journal.active.capacity() * std::mem::size_of::<(FontKey, FontCandidate)>()
            + journal.used.capacity() * std::mem::size_of::<FontKey>()
            + journal.pending.capacity() * std::mem::size_of::<FontKey>()
            + journal.timelines.capacity() * std::mem::size_of::<(FontKey, Timeline)>()
            + journal
                .active
                .values()
                .map(|source| {
                    source.face.retained_bytes() + source.data.as_ref().map_or(0, |data| data.len())
                })
                .sum::<usize>()
    }

    pub(crate) fn forget_scopes(&self, dead: &rustc_hash::FxHashSet<usize>) {
        let mut journal = self.journal.lock().unwrap();
        journal.active.retain(|key, _| !dead.contains(&key.scope));
        journal.used.retain(|key| !dead.contains(&key.scope));
        journal.pending.retain(|key| !dead.contains(&key.scope));
        journal
            .timelines
            .retain(|key, _| !dead.contains(&key.scope));
    }
}

/// Immutable selection inputs; a journal only accepts keys that still
/// belong to its current document. An old raster/layout cannot start a load
/// for a removed rule. The installed-font context is created lazily.
pub(crate) struct FontSelection {
    pub sources: Vec<FontCandidate>,
    pub demands: Arc<FontDemands>,
    installed: OnceLock<Mutex<FontContext>>,
    revision: u64,
    canvas: bool,
}

impl FontSelection {
    #[cfg(test)]
    pub(crate) fn new(sources: Vec<FontCandidate>, demands: Arc<FontDemands>) -> Arc<Self> {
        Self::for_context(sources, demands, false)
    }

    pub(crate) fn for_context(
        sources: Vec<FontCandidate>,
        demands: Arc<FontDemands>,
        canvas: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            sources,
            revision: demands.revision(),
            demands,
            installed: OnceLock::new(),
            canvas,
        })
    }

    pub(crate) fn same_as(&self, other: &Self) -> bool {
        self.canvas == other.canvas
            && self.revision == other.revision
            && Arc::ptr_eq(&self.demands, &other.demands)
            && self.sources.len() == other.sources.len()
            && self.sources.iter().zip(&other.sources).all(|(a, b)| {
                a.key == b.key && a.failed == b.failed && a.face == b.face && a.data == b.data
            })
    }

    /// Reverse rule order applies within a composite face. A pending face
    /// stops family fallback: fallback metrics may paint temporarily, but
    /// must not cause the later downloadable families to be fetched early.
    pub(crate) fn request(
        &self,
        text: &str,
        family: &str,
        width: f32,
        style: FontStyle,
        weight: f32,
    ) -> Vec<Range<usize>> {
        if text.is_empty() || self.sources.is_empty() {
            return Vec::new();
        }
        let Ok(families) = svgtypes::parse_font_families(family) else {
            return Vec::new();
        };
        self.request_families(text, &families, width, style, weight)
    }

    pub(crate) fn request_svg(&self, font: &resvg::usvg::Font, text: &str) -> Vec<Range<usize>> {
        use resvg::usvg::{FontStretch as W, FontStyle as S};
        let width = match font.stretch() {
            W::UltraCondensed => 0.5,
            W::ExtraCondensed => 0.625,
            W::Condensed => 0.75,
            W::SemiCondensed => 0.875,
            W::Normal => 1.,
            W::SemiExpanded => 1.125,
            W::Expanded => 1.25,
            W::ExtraExpanded => 1.5,
            W::UltraExpanded => 2.,
        };
        let style = match font.style() {
            S::Normal => FontStyle::Normal,
            S::Italic => FontStyle::Italic,
            S::Oblique => FontStyle::Oblique(None),
        };
        self.request_families(
            text,
            font.families(),
            width,
            style,
            f32::from(font.weight()),
        )
    }

    fn request_families(
        &self,
        text: &str,
        families: &[svgtypes::FontFamily],
        width: f32,
        style: FontStyle,
        weight: f32,
    ) -> Vec<Range<usize>> {
        let attributes = Attributes {
            width: FontWidth::from_ratio(width),
            style,
            weight: FontWeight::new(weight),
        };
        // Match descriptors once per family/run, and test repeated clusters
        // once. Text length must not multiply scans of every declared face.
        let selected: Vec<_> = families
            .iter()
            .map(|family| {
                let named = match family {
                    svgtypes::FontFamily::Named(name) => Some(fold(name)),
                    _ => None,
                };
                let candidates: Vec<_> = named
                    .as_ref()
                    .map(|name| {
                        self.sources
                            .iter()
                            .filter(|source| fold(&source.face.family) == *name)
                            .collect()
                    })
                    .unwrap_or_default();
                if candidates.is_empty() {
                    return None;
                }
                let available: Vec<_> = candidates
                    .into_iter()
                    .filter(|source| {
                        !source.failed && (self.canvas || !self.demands.failed(source.key))
                    })
                    .collect();
                let descriptors: Vec<_> = available
                    .iter()
                    .map(|source| source.face.descriptors.matching())
                    .collect();
                Some(
                    parley::fontique::match_face_descriptors(
                        &descriptors,
                        attributes.width,
                        attributes.style,
                        attributes.weight,
                    )
                    .into_iter()
                    .rev()
                    .map(|index| available[index])
                    .collect::<Vec<_>>(),
                )
            })
            .collect();
        let mut seen = HashMap::new();
        let mut hidden: Vec<Range<usize>> = Vec::new();
        for (at, cluster) in text.grapheme_indices(true) {
            if cluster.chars().all(ignorable) {
                continue;
            }
            let blocked =
                *seen.entry(cluster).or_insert_with(|| {
                    for (family, sources) in families.iter().zip(&selected) {
                        if let Some(sources) = sources {
                            for source in sources {
                                if !cluster.chars().filter(|&c| !ignorable(c)).any(|c| {
                                    source.face.unicode_range.is_empty()
                                        || source.face.unicode_range.iter().any(|&(start, end)| {
                                            (start..=end).contains(&(c as u32))
                                        })
                                }) {
                                    continue;
                                }
                                let phase = self.demands.request(source.key, self.canvas);
                                if phase == Phase::Failed {
                                    continue;
                                }
                                if source
                                    .data
                                    .as_ref()
                                    .is_none_or(|data| supports(data, 0, cluster))
                                {
                                    return phase == Phase::Block;
                                }
                            }
                        } else if self.installed_supports(family, attributes, cluster) {
                            return false;
                        }
                    }
                    false
                });
            if blocked {
                if let Some(last) = hidden.last_mut().filter(|last| last.end == at) {
                    last.end += cluster.len();
                } else {
                    hidden.push(at..at + cluster.len());
                }
            }
        }
        hidden
    }

    fn installed_supports(
        &self,
        family: &svgtypes::FontFamily,
        attributes: Attributes,
        text: &str,
    ) -> bool {
        let context = self.installed.get_or_init(|| {
            Mutex::new(FontContext {
                collection: catalog().base_text_collection.clone(),
                source_cache: font_source_cache(),
            })
        });
        let mut context = context.lock().unwrap();
        let FontContext {
            collection,
            source_cache,
        } = &mut *context;
        let named = match family {
            svgtypes::FontFamily::Named(name) => {
                let quoted = format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""));
                Some(expand_css_family_list(&quoted, catalog()))
            }
            _ => None,
        };
        let names = named
            .as_ref()
            .and_then(|names| svgtypes::parse_font_families(names).ok());
        let mut query = collection.query(source_cache);
        query.set_attributes(attributes);
        if let Some(names) = &names {
            query.set_families(names.iter().filter_map(|name| match name {
                svgtypes::FontFamily::Named(name) => Some(QueryFamily::Named(name)),
                _ => None,
            }));
        } else {
            let generic = match family {
                svgtypes::FontFamily::Serif => GenericFamily::Serif,
                svgtypes::FontFamily::SansSerif => GenericFamily::SansSerif,
                svgtypes::FontFamily::Monospace => GenericFamily::Monospace,
                svgtypes::FontFamily::Cursive => GenericFamily::Cursive,
                svgtypes::FontFamily::Fantasy => GenericFamily::Fantasy,
                svgtypes::FontFamily::Named(_) => return false,
            };
            query.set_families([generic]);
        }
        let mut found = false;
        query.matches_with(|font| {
            found = supports(font.blob.as_ref(), font.index, text);
            if found {
                QueryStatus::Stop
            } else {
                QueryStatus::Continue
            }
        });
        found
    }
}

fn ignorable(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200c}' | '\u{200d}' | '\u{200e}' | '\u{200f}' |
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}')
}

fn supports(bytes: &[u8], index: u32, text: &str) -> bool {
    let Ok(face) = ttf_parser::Face::parse(bytes, index) else {
        return false;
    };
    let covers = |text: &str| {
        let mut previous = None;
        for c in text.chars() {
            if matches!(c, '\u{fe00}'..='\u{fe0f}' | '\u{e0100}'..='\u{e01ef}') {
                if previous.is_none_or(|base| face.glyph_variation_index(base, c).is_none()) {
                    return false;
                }
            } else if !ignorable(c) {
                if face.glyph_index(c).is_none() {
                    return false;
                }
                previous = Some(c);
            }
        }
        true
    };
    covers(text) || covers(&icu_normalizer::ComposingNormalizer::new_nfc().normalize(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(member: u64, css: &str) -> FontCandidate {
        FontCandidate {
            key: FontKey {
                scope: 0,
                member,
                script: false,
            },
            face: Arc::new(crate::http::font_face_descriptors(css).unwrap()),
            data: None,
            failed: false,
            css: true,
        }
    }

    fn selection(sources: Vec<FontCandidate>, demands: &Arc<FontDemands>) -> Arc<FontSelection> {
        demands.set_active(sources.clone());
        FontSelection::new(sources, demands.clone())
    }

    fn pending(demands: &FontDemands) -> Vec<u64> {
        let mut members: Vec<_> = demands
            .take_pending()
            .into_iter()
            .map(|s| s.key.member)
            .collect();
        members.sort_unstable();
        members
    }

    #[test]
    fn font_demand_matches_descriptors_ranges_and_reverse_rule_order() {
        let demands = Arc::new(FontDemands::default());
        let fonts = selection(
            vec![
                candidate(1, "font-family:Web;src:url(old);unicode-range:U+0-FF"),
                candidate(2, "font-family:Web;src:url(latin);unicode-range:U+0-FF"),
                candidate(
                    3,
                    "font-family:Web;src:url(cyrillic);unicode-range:U+400-4FF",
                ),
                candidate(4, "font-family:Web;src:url(bold);font-weight:700"),
                candidate(5, "font-family:Unused;src:url(unused)"),
            ],
            &demands,
        );
        assert!(pending(&demands).is_empty());
        fonts.request("A A Ж", "Web, Unused", 1., FontStyle::Normal, 400.);
        assert_eq!(pending(&demands), [2, 3]);
        fonts.request("A", "Web", 1., FontStyle::Normal, 700.);
        assert_eq!(pending(&demands), [4]);
        fonts.request("A A Ж", "Web, Unused", 1., FontStyle::Normal, 400.);
        assert!(pending(&demands).is_empty());
    }

    #[test]
    fn font_demand_defers_fallback_until_preceding_face_has_no_glyph() {
        let demands = Arc::new(FontDemands::default());
        let first = candidate(1, "font-family:First;src:url(first)");
        let second = candidate(2, "font-family:Second;src:url(second)");
        selection(vec![first.clone(), second.clone()], &demands).request(
            "\u{10ffff}",
            "First, Second",
            1.,
            FontStyle::Normal,
            400.,
        );
        assert_eq!(pending(&demands), [1]);
        let mut first = first;
        first.data = Some(Arc::from(
            include_bytes!("../../assets/fonts/dejavu/DejaVuSans.ttf").as_slice(),
        ));
        selection(vec![first, second], &demands).request(
            "\u{10ffff}",
            "First, Second",
            1.,
            FontStyle::Normal,
            400.,
        );
        assert_eq!(pending(&demands), [2]);
    }

    #[test]
    fn font_demand_respects_installed_family_and_removed_rules() {
        let demands = Arc::new(FontDemands::default());
        let fonts = selection(vec![candidate(1, "font-family:Web;src:url(web)")], &demands);
        fonts.request("Hello", "serif, Web", 1., FontStyle::Normal, 400.);
        assert!(pending(&demands).is_empty());
        demands.set_active(Vec::new());
        fonts.request("Hello", "Web", 1., FontStyle::Normal, 400.);
        assert!(pending(&demands).is_empty());
    }
}
