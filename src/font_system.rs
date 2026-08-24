//! Process-wide installed-font discovery and browser font policy.
//!
//! CSS Fonts 4 §5 leaves the installed-font set to the user agent, while
//! requiring ordered matching, all localized family names, Unicode Default
//! Caseless Matching, configurable generic families, and character fallback.
//! On Linux and FreeBSD, Fontconfig is the platform convention for expressing
//! those user choices, but the Fontconfig library is not part of the CSS
//! contract. This module reads the relevant XML configuration as bounded input,
//! discovers font files with ordinary Rust filesystem APIs, and configures the
//! pure-Rust Fontique and fontdb consumers from one immutable catalog.
//!
//! The catalog is initialized once. Fontique metadata is cloned into each
//! thread-local Parley context while font bytes remain path-backed and lazy.
//! SVG receives the same path set and generic-family choices, preventing HTML
//! and SVG text from silently selecting different host fonts.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use fontdb::{Database, Family, ID, Query, Source};
use parley::FontContext;
use parley::fontique::{
    Blob, Collection, CollectionOptions, FallbackKey, FamilyId, FontInfoOverride, GenericFamily,
    Language, Script, ScriptExt as _, SourceCache,
};
use roxmltree::{Document, Node, ParsingOptions};
use unicode_casefold::UnicodeCaseFold as _;

const MAX_CONFIG_FILES: usize = 1_024;
const MAX_CONFIG_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CONFIG_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CONFIG_NODES: u32 = 262_144;
const MAX_CONFIG_RULES: usize = 65_536;
const MAX_FONT_DIRECTORIES: usize = 4_096;
const MAX_FONT_FILES: usize = 32_768;
const MAX_FONT_DIRECTORY_DEPTH: usize = 32;
const MAX_FONT_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FAMILY_EXPANSIONS: usize = 2_048;

const EMERGENCY_FONTS: &[EmbeddedFont] = &[
    EmbeddedFont(include_bytes!("../assets/fonts/dejavu/DejaVuSans.ttf")),
    EmbeddedFont(include_bytes!("../assets/fonts/dejavu/DejaVuSerif.ttf")),
    EmbeddedFont(include_bytes!("../assets/fonts/dejavu/DejaVuSansMono.ttf")),
];

#[derive(Clone, Copy)]
struct EmbeddedFont(&'static [u8]);

impl AsRef<[u8]> for EmbeddedFont {
    fn as_ref(&self) -> &[u8] {
        self.0
    }
}

static CATALOG: OnceLock<Catalog> = OnceLock::new();
static SVG_CATALOG: OnceLock<SvgCatalog> = OnceLock::new();
static PAGE_FONT_EPOCH: AtomicU64 = AtomicU64::new(0);

struct Catalog {
    alias_candidates: HashMap<String, Vec<String>>,
    paths: Vec<PathBuf>,
    embedded_fallback: bool,
    base_text_collection: Collection,
    text_collection: Mutex<Collection>,
    /// Default-caseless font name to the exact spelling stored by Fontique.
    installed_names: HashMap<String, String>,
    generic_names: HashMap<GenericFamily, Vec<String>>,
    family_expansions: Mutex<ExpansionCache>,
}

/// One already-fetched `@font-face` resource. The CSS descriptor's family
/// name intentionally overrides the font file's internal name (CSS Fonts 4
/// §4.1 permits authors to assign an arbitrary family to a face).
pub(crate) struct PageFont {
    pub family: String,
    pub bytes: Vec<u8>,
}

impl PageFont {
    /// Decode a fetched CSS font resource into the SFNT container expected by
    /// Fontique. WOFF 1.0 and WOFF2 are transport containers, not formats that
    /// OpenType consumers are required to parse directly.
    pub(crate) fn from_web_resource(family: String, bytes: Vec<u8>) -> Option<Self> {
        let bytes = match bytes.get(..4)? {
            b"wOFF" => wuff::decompress_woff1(&bytes).ok()?,
            b"wOF2" => wuff::decompress_woff2(&bytes).ok()?,
            // TrueType, CFF OpenType, Apple TrueType, and collections are
            // already SFNT containers. Reject legacy EOT and arbitrary data
            // so CSS Fonts can continue with the next `src` item.
            [0, 1, 0, 0] | b"OTTO" | b"true" | b"ttcf" => bytes,
            _ => return None,
        };
        Some(Self { family, bytes })
    }
}

struct SvgCatalog {
    db: Arc<Database>,
    fallback_families: Vec<String>,
}

#[derive(Default)]
struct ExpansionCache {
    values: HashMap<String, String>,
    order: VecDeque<String>,
}

#[derive(Clone, Debug, Default)]
struct FontConfig {
    directories: Vec<PathBuf>,
    aliases: Vec<AliasRule>,
    locale_preferences: Vec<LocalePreference>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AliasRule {
    family: String,
    prefer: Vec<String>,
    accept: Vec<String>,
    default: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LocalePreference {
    language: String,
    families: Vec<String>,
}

#[derive(Clone, Copy)]
enum ConfigPathKind {
    Directory,
    Include,
}

/// Constructs a Parley context from the process-wide immutable metadata.
///
/// Windows and Apple platforms retain Fontique's native system APIs. Linux and
/// FreeBSD use the pure-Rust catalog because the vendored Fontique backend is
/// deliberately compiled without its Fontconfig FFI.
pub(crate) fn font_context() -> FontContext {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let collection = catalog()
            .text_collection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        FontContext {
            collection,
            source_cache: SourceCache::default(),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        FontContext::new()
    }
}

/// Replace the downloadable-font set for the foreground document. CSS Fonts 4
/// §4.1 scopes downloaded faces to documents; TRust has exactly one live
/// foreground page, so rebuilding from the immutable installed-font catalog on
/// navigation both enforces that scope and bounds retained font bytes.
pub(crate) fn install_page_fonts(fonts: Vec<PageFont>) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let catalog = catalog();
        let mut collection = catalog.base_text_collection.clone();
        for font in fonts {
            if font.family.trim().is_empty() || font.bytes.is_empty() {
                continue;
            }
            collection.register_fonts(
                Blob::new(Arc::new(font.bytes)),
                Some(FontInfoOverride {
                    family_name: Some(font.family.trim()),
                    ..FontInfoOverride::default()
                }),
            );
        }
        *catalog
            .text_collection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = collection;
        PAGE_FONT_EPOCH.fetch_add(1, Ordering::Release);
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _ = fonts;
    }
}

pub(crate) fn page_font_epoch() -> u64 {
    PAGE_FONT_EPOCH.load(Ordering::Acquire)
}

/// Applies installed aliases to a CSS family list without changing its
/// computed value. This is only needed on platforms using our catalog; native
/// Fontique backends perform platform aliases internally.
pub(crate) fn css_family_source(family: &str) -> Cow<'_, str> {
    let with_fallback = add_default_generic(family);
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let trimmed = with_fallback.trim();
        if !is_quoted_css_family(trimmed)
            && let Some(generic) = GenericFamily::parse(&trimmed.to_ascii_lowercase())
        {
            let canonical = generic.to_string();
            return if trimmed == canonical {
                with_fallback
            } else {
                Cow::Owned(canonical)
            };
        }
        let catalog = catalog();
        let key = with_fallback.as_ref();
        if let Some(hit) = catalog
            .family_expansions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .get(key)
            .cloned()
        {
            return Cow::Owned(hit);
        }
        let expanded = expand_css_family_list(key, catalog);
        if expanded == key {
            return with_fallback;
        }
        let mut cache = catalog
            .family_expansions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owned_key = key.to_string();
        if !cache.values.contains_key(&owned_key) {
            while cache.values.len() >= MAX_FAMILY_EXPANSIONS {
                let Some(oldest) = cache.order.pop_front() else {
                    break;
                };
                cache.values.remove(&oldest);
            }
            cache.order.push_back(owned_key.clone());
            cache.values.insert(owned_key, expanded.clone());
        }
        Cow::Owned(expanded)
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        with_fallback
    }
}

pub(crate) fn svg_fontdb() -> Arc<Database> {
    svg_catalog().db.clone()
}

/// The SVG resolver uses the catalog's caseless aliases and ordered generic
/// families. usvg's stock resolver delegates to fontdb's case-sensitive name
/// comparison and therefore does not satisfy CSS Fonts 4 §5.1 by itself.
pub(crate) fn svg_font_resolver() -> resvg::usvg::FontResolver<'static> {
    let materialized = Arc::new(Mutex::new(HashMap::new()));
    let select_materialized = materialized.clone();
    resvg::usvg::FontResolver {
        select_font: Box::new(move |font, db| {
            select_svg_font(font, db)
                .and_then(|id| materialize_svg_face(db, id, &select_materialized))
        }),
        select_fallback: Box::new(move |c, excluded, db| {
            select_svg_fallback(c, excluded, db, &materialized)
        }),
    }
}

fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(Catalog::build)
}

fn svg_catalog() -> &'static SvgCatalog {
    SVG_CATALOG.get_or_init(SvgCatalog::build)
}

impl Catalog {
    fn build() -> Self {
        let config = load_platform_config();
        let paths = discover_font_paths(&config.directories);
        let alias_candidates = index_aliases(&config);
        let mut collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        });
        collection.load_fonts_from_paths(&paths);

        let mut installed_names = collect_collection_names(&mut collection);
        let mut generic_names =
            resolve_generic_names(&alias_candidates, &installed_names, None, None);
        if mandatory_generics_missing(&generic_names) {
            let (proportional, monospace) = collection_fallback_names(&mut collection);
            generic_names = resolve_generic_names(
                &alias_candidates,
                &installed_names,
                proportional.as_deref(),
                monospace.as_deref(),
            );
        }
        let embedded_fallback = mandatory_generics_missing(&generic_names);
        if embedded_fallback {
            for font in EMERGENCY_FONTS {
                collection.register_fonts(Blob::new(Arc::new(*font)), None);
            }
            installed_names = collect_collection_names(&mut collection);
            generic_names = resolve_generic_names(&alias_candidates, &installed_names, None, None);
        }

        for generic in GenericFamily::all() {
            let ids = generic_names
                .get(generic)
                .into_iter()
                .flatten()
                .filter_map(|name| collection.family_id(name))
                .collect::<Vec<_>>();
            collection.set_generic_families(*generic, ids.into_iter());
        }
        configure_fallbacks(&mut collection, &config);

        Self {
            alias_candidates,
            paths,
            embedded_fallback,
            base_text_collection: collection.clone(),
            text_collection: Mutex::new(collection),
            installed_names,
            generic_names,
            family_expansions: Mutex::new(ExpansionCache::default()),
        }
    }
}

impl SvgCatalog {
    fn build() -> Self {
        // SVG text is optional and relatively uncommon. Keep its independent
        // fontdb metadata index off the HTML startup path while deriving it
        // from the same immutable path set and policy as Parley.
        let catalog = catalog();
        let mut db = Database::new();
        for path in &catalog.paths {
            let _ = db.load_font_file(path);
        }
        if catalog.embedded_fallback {
            for font in EMERGENCY_FONTS {
                db.load_font_source(Source::Binary(Arc::new(*font)));
            }
        }
        set_svg_generics(&mut db, &catalog.generic_names);
        let fallback_families = svg_fallback_families(&db, &catalog.generic_names);
        Self {
            db: Arc::new(db),
            fallback_families,
        }
    }
}

fn collect_collection_names(collection: &mut Collection) -> HashMap<String, String> {
    collection
        .family_names()
        .map(|name| (fold(name), name.to_string()))
        .collect()
}

fn collection_fallback_names(collection: &mut Collection) -> (Option<String>, Option<String>) {
    let mut names = collection
        .family_names()
        .map(str::to_string)
        .collect::<Vec<_>>();
    names.sort_by_cached_key(|name| fold(name));
    let mut proportional = None;
    let mut monospace = None;
    for name in names {
        let Some(id) = collection.family_id(&name) else {
            continue;
        };
        let Some(family) = collection.family(id) else {
            continue;
        };
        let Some(font) = family.default_font() else {
            continue;
        };
        if font.is_monospaced() {
            monospace.get_or_insert(name);
        } else {
            proportional.get_or_insert(name);
        }
        if proportional.is_some() && monospace.is_some() {
            break;
        }
    }
    (proportional, monospace)
}

#[cfg(test)]
fn collect_installed_names(db: &Database) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for face in db.faces() {
        for (name, _) in &face.families {
            names.entry(fold(name)).or_insert_with(|| name.clone());
        }
    }
    names
}

fn mandatory_generics_missing(generics: &HashMap<GenericFamily, Vec<String>>) -> bool {
    [
        GenericFamily::Serif,
        GenericFamily::SansSerif,
        GenericFamily::Monospace,
    ]
    .into_iter()
    .any(|generic| generics.get(&generic).is_none_or(Vec::is_empty))
}

fn configure_fallbacks(collection: &mut Collection, config: &FontConfig) {
    let family_names = collection
        .family_names()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut named_families = family_names
        .into_iter()
        .filter_map(|name| collection.family_id(&name).map(|id| (name, id)))
        .collect::<Vec<_>>();
    named_families.sort_by_cached_key(|(name, _)| fold(name));
    named_families.dedup_by_key(|(_, id)| *id);

    let generic_order = [
        GenericFamily::SansSerif,
        GenericFamily::Serif,
        GenericFamily::Monospace,
        GenericFamily::SystemUi,
        GenericFamily::Emoji,
        GenericFamily::Math,
        GenericFamily::Cursive,
        GenericFamily::Fantasy,
    ]
    .into_iter()
    .flat_map(|generic| collection.generic_families(generic).collect::<Vec<_>>())
    .collect::<Vec<_>>();

    let mut ordered_ids = Vec::new();
    let mut seen = HashSet::new();
    for id in generic_order
        .into_iter()
        .chain(named_families.iter().map(|(_, id)| *id))
    {
        if seen.insert(id) {
            ordered_ids.push(id);
        }
    }

    let coverage = family_script_coverage(collection, &ordered_ids);
    let mut script_families = HashMap::new();
    for (script, _) in Script::all_samples() {
        let mut families = ordered_ids
            .iter()
            .filter_map(|id| {
                let score = coverage.get(id)?.get(script).copied()?;
                (score != 0).then_some((*id, score))
            })
            .collect::<Vec<_>>();
        // Preserve configured order among equally capable families, while a
        // face covering more of the representative cluster wins.
        families.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        let families = families.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        collection.set_fallbacks(*script, families.iter().copied());
        script_families.insert(*script, families);
    }

    for preference in &config.locale_preferences {
        let Ok(language) = Language::parse(&preference.language) else {
            continue;
        };
        let ids = preference
            .families
            .iter()
            .filter_map(|name| collection.family_id(name))
            .collect::<Vec<_>>();
        if ids.is_empty() {
            continue;
        }
        for (script, _) in Script::all_samples() {
            let key = FallbackKey::new(*script, Some(&language));
            if key.is_tracked() {
                let mut seen = HashSet::new();
                let families = ids
                    .iter()
                    .copied()
                    .chain(script_families.get(script).into_iter().flatten().copied())
                    .filter(|id| seen.insert(*id))
                    .collect::<Vec<_>>();
                collection.set_fallbacks(key, families.into_iter());
            }
        }
    }
}

fn family_script_coverage(
    collection: &mut Collection,
    family_ids: &[FamilyId],
) -> HashMap<FamilyId, HashMap<Script, u8>> {
    let mut result = HashMap::new();
    for id in family_ids {
        let Some(family) = collection.family(*id) else {
            continue;
        };
        let mut scores = HashMap::<Script, u8>::new();
        for font in family.fonts() {
            for (script, _) in Script::all_samples() {
                let score = font.script_coverage(*script);
                scores
                    .entry(*script)
                    .and_modify(|current| *current = (*current).max(score))
                    .or_insert(score);
            }
        }
        result.insert(*id, scores);
    }
    result
}

fn select_svg_font(font: &resvg::usvg::Font, db: &mut Arc<Database>) -> Option<ID> {
    let catalog = catalog();
    let mut names = Vec::<String>::new();
    for family in font.families() {
        match family {
            svgtypes::FontFamily::Serif => {
                append_generic_names(&mut names, catalog, GenericFamily::Serif)
            }
            svgtypes::FontFamily::SansSerif => {
                append_generic_names(&mut names, catalog, GenericFamily::SansSerif)
            }
            svgtypes::FontFamily::Cursive => {
                append_generic_names(&mut names, catalog, GenericFamily::Cursive)
            }
            svgtypes::FontFamily::Fantasy => {
                append_generic_names(&mut names, catalog, GenericFamily::Fantasy)
            }
            svgtypes::FontFamily::Monospace => {
                append_generic_names(&mut names, catalog, GenericFamily::Monospace)
            }
            svgtypes::FontFamily::Named(name) => append_resolved_name(&mut names, name, catalog, 0),
        }
    }
    append_generic_names(&mut names, catalog, GenericFamily::Serif);
    dedup_folded(&mut names);

    let families = names
        .iter()
        .map(|name| Family::Name(name.as_str()))
        .collect::<Vec<_>>();
    let style = match font.style() {
        resvg::usvg::FontStyle::Normal => fontdb::Style::Normal,
        resvg::usvg::FontStyle::Italic => fontdb::Style::Italic,
        resvg::usvg::FontStyle::Oblique => fontdb::Style::Oblique,
    };
    let stretch = match font.stretch() {
        resvg::usvg::FontStretch::UltraCondensed => fontdb::Stretch::UltraCondensed,
        resvg::usvg::FontStretch::ExtraCondensed => fontdb::Stretch::ExtraCondensed,
        resvg::usvg::FontStretch::Condensed => fontdb::Stretch::Condensed,
        resvg::usvg::FontStretch::SemiCondensed => fontdb::Stretch::SemiCondensed,
        resvg::usvg::FontStretch::Normal => fontdb::Stretch::Normal,
        resvg::usvg::FontStretch::SemiExpanded => fontdb::Stretch::SemiExpanded,
        resvg::usvg::FontStretch::Expanded => fontdb::Stretch::Expanded,
        resvg::usvg::FontStretch::ExtraExpanded => fontdb::Stretch::ExtraExpanded,
        resvg::usvg::FontStretch::UltraExpanded => fontdb::Stretch::UltraExpanded,
    };
    db.query(&Query {
        families: &families,
        weight: fontdb::Weight(font.weight()),
        stretch,
        style,
    })
}

fn select_svg_fallback(
    c: char,
    excluded: &[ID],
    db: &mut Arc<Database>,
    materialized: &Mutex<HashMap<ID, ID>>,
) -> Option<ID> {
    let catalog = catalog();
    let preferred_generic = if is_probable_emoji(c) {
        Some(GenericFamily::Emoji)
    } else {
        None
    };

    let mut families = Vec::new();
    if let Some(generic) = preferred_generic {
        families.extend(
            catalog
                .generic_names
                .get(&generic)
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    families.extend(svg_catalog().fallback_families.iter().cloned());
    dedup_folded(&mut families);

    let base = excluded
        .first()
        .and_then(|id| db.face(*id))
        .map(|face| (face.style, face.stretch, face.weight, face.monospaced));
    for family in families {
        let mut ids = face_ids_for_name(db, &family);
        ids.sort_by_key(|id| {
            db.face(*id)
                .map(|face| svg_face_distance(face, base))
                .unwrap_or((u8::MAX, u8::MAX, u16::MAX, u8::MAX))
        });
        for id in ids {
            let check_id = materialized
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .copied()
                .unwrap_or(id);
            if excluded.contains(&check_id) || !face_has_char(db, check_id, c) {
                continue;
            }
            return materialize_svg_face(db, id, materialized);
        }
    }
    None
}

fn svg_face_distance(
    face: &fontdb::FaceInfo,
    base: Option<(fontdb::Style, fontdb::Stretch, fontdb::Weight, bool)>,
) -> (u8, u8, u16, u8) {
    let Some((style, stretch, weight, monospaced)) = base else {
        return (0, 0, 0, 0);
    };
    (
        style_distance(style, face.style),
        stretch_rank(stretch).abs_diff(stretch_rank(face.stretch)),
        weight.0.abs_diff(face.weight.0),
        u8::from(monospaced != face.monospaced),
    )
}

fn style_distance(requested: fontdb::Style, candidate: fontdb::Style) -> u8 {
    use fontdb::Style::{Italic, Normal, Oblique};
    match (requested, candidate) {
        (a, b) if a == b => 0,
        (Italic, Oblique) | (Oblique, Italic) | (Normal, Oblique) => 1,
        _ => 2,
    }
}

fn stretch_rank(stretch: fontdb::Stretch) -> u8 {
    use fontdb::Stretch::*;
    match stretch {
        UltraCondensed => 1,
        ExtraCondensed => 2,
        Condensed => 3,
        SemiCondensed => 4,
        Normal => 5,
        SemiExpanded => 6,
        Expanded => 7,
        ExtraExpanded => 8,
        UltraExpanded => 9,
    }
}

fn materialize_svg_face(
    db: &mut Arc<Database>,
    id: ID,
    materialized: &Mutex<HashMap<ID, ID>>,
) -> Option<ID> {
    if let Some(id) = materialized
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&id)
        .copied()
    {
        return Some(id);
    }
    let face = db.face(id)?;
    let (path, index) = match &face.source {
        Source::Binary(_) => return Some(id),
        Source::File(path) => (path.clone(), face.index),
        #[allow(unreachable_patterns)]
        _ => return Some(id),
    };
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_FONT_FILE_BYTES) {
        return None;
    }
    let data = fs::read(path).ok()?;
    let ids = Arc::make_mut(db).load_font_source(Source::Binary(Arc::new(data)));
    let loaded = *ids.get(index as usize)?;
    materialized
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, loaded);
    Some(loaded)
}

fn face_has_char(db: &Database, id: ID, c: char) -> bool {
    if let Some(face) = db.face(id)
        && let Source::File(path) = &face.source
        && fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_FONT_FILE_BYTES)
    {
        return false;
    }
    db.with_face_data(id, |font_data, face_index| {
        ttf_parser::Face::parse(font_data, face_index)
            .ok()
            .and_then(|face| face.glyph_index(c))
            .is_some()
    })
    .unwrap_or(false)
}

fn append_generic_names(names: &mut Vec<String>, catalog: &Catalog, generic: GenericFamily) {
    names.extend(
        catalog
            .generic_names
            .get(&generic)
            .into_iter()
            .flatten()
            .cloned(),
    );
}

fn face_ids_for_name(db: &Database, name: &str) -> Vec<ID> {
    db.faces()
        .filter(|face| face.families.iter().any(|(family, _)| family == name))
        .map(|face| face.id)
        .collect()
}

fn is_probable_emoji(c: char) -> bool {
    matches!(c as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0xFE0F)
}

fn resolve_generic_names(
    aliases: &HashMap<String, Vec<String>>,
    installed: &HashMap<String, String>,
    proportional_fallback: Option<&str>,
    monospace_fallback: Option<&str>,
) -> HashMap<GenericFamily, Vec<String>> {
    let mut result = HashMap::new();
    for generic in GenericFamily::all() {
        let mut names = Vec::new();
        expand_config_name(
            &generic.to_string(),
            aliases,
            installed,
            &mut names,
            &mut HashSet::new(),
            0,
        );
        append_builtin_generic_candidates(
            *generic,
            installed,
            proportional_fallback,
            monospace_fallback,
            &mut names,
        );
        dedup_folded(&mut names);
        result.insert(*generic, names);
    }

    inherit_generic(&mut result, GenericFamily::UiSerif, GenericFamily::Serif);
    inherit_generic(
        &mut result,
        GenericFamily::UiSansSerif,
        GenericFamily::SystemUi,
    );
    inherit_generic(
        &mut result,
        GenericFamily::UiMonospace,
        GenericFamily::Monospace,
    );
    inherit_generic(
        &mut result,
        GenericFamily::UiRounded,
        GenericFamily::SystemUi,
    );
    inherit_generic(&mut result, GenericFamily::FangSong, GenericFamily::Serif);
    result
}

fn inherit_generic(
    map: &mut HashMap<GenericFamily, Vec<String>>,
    target: GenericFamily,
    source: GenericFamily,
) {
    if map.get(&target).is_none_or(Vec::is_empty) {
        let inherited = map.get(&source).cloned().unwrap_or_default();
        map.insert(target, inherited);
    }
}

fn expand_config_name(
    name: &str,
    aliases: &HashMap<String, Vec<String>>,
    installed: &HashMap<String, String>,
    output: &mut Vec<String>,
    visiting: &mut HashSet<String>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let key = fold(name);
    if let Some(exact) = installed.get(&key) {
        output.push(exact.clone());
        return;
    }
    if !visiting.insert(key.clone()) {
        return;
    }
    for candidate in aliases.get(&key).into_iter().flatten() {
        expand_config_name(candidate, aliases, installed, output, visiting, depth + 1);
    }
    // Keep this node marked for the rest of the resolution pass. Fontconfig
    // alias graphs often converge and can contain cycles; traversing a shared
    // branch again cannot add a new installed family and may otherwise make a
    // small graph exponentially expensive.
}

fn index_aliases(config: &FontConfig) -> HashMap<String, Vec<String>> {
    let mut index = HashMap::<String, Vec<String>>::new();
    for alias in &config.aliases {
        index.entry(fold(&alias.family)).or_default().extend(
            alias
                .prefer
                .iter()
                .chain(&alias.accept)
                .chain(&alias.default)
                .cloned(),
        );
    }
    index
}

fn append_builtin_generic_candidates(
    generic: GenericFamily,
    installed: &HashMap<String, String>,
    proportional_fallback: Option<&str>,
    monospace_fallback: Option<&str>,
    output: &mut Vec<String>,
) {
    let candidates: &[&str] = match generic {
        GenericFamily::Serif | GenericFamily::UiSerif => &[
            "Noto Serif",
            "DejaVu Serif",
            "Liberation Serif",
            "Times New Roman",
        ],
        GenericFamily::SansSerif | GenericFamily::UiSansSerif => {
            &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Arial"]
        }
        GenericFamily::Monospace | GenericFamily::UiMonospace => &[
            "Noto Sans Mono",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Courier New",
        ],
        GenericFamily::SystemUi | GenericFamily::UiRounded => &[
            "Adwaita Sans",
            "Cantarell",
            "Noto Sans UI",
            "Segoe UI",
            "Noto Sans",
            "DejaVu Sans",
        ],
        GenericFamily::Emoji => &[
            "Noto Color Emoji",
            "Apple Color Emoji",
            "Segoe UI Emoji",
            "Noto Emoji",
        ],
        GenericFamily::Math => &[
            "STIX Two Math",
            "Cambria Math",
            "Latin Modern Math",
            "DejaVu Math TeX Gyre",
        ],
        GenericFamily::Cursive => &["Comic Sans MS", "Apple Chancery", "URW Chancery L"],
        GenericFamily::Fantasy => &["Impact", "Papyrus"],
        GenericFamily::FangSong => &["FangSong", "STFangsong"],
    };
    output.extend(
        candidates
            .iter()
            .filter_map(|name| installed.get(&fold(name)).cloned()),
    );

    if output.is_empty() {
        let fallback = match generic {
            GenericFamily::Monospace | GenericFamily::UiMonospace => monospace_fallback,
            _ => proportional_fallback,
        };
        if let Some(name) = fallback {
            output.push(name.to_string());
        }
    }
}

fn set_svg_generics(db: &mut Database, generics: &HashMap<GenericFamily, Vec<String>>) {
    let first = |generic| {
        generics
            .get(&generic)
            .and_then(|families| families.first())
            .cloned()
    };
    if let Some(name) = first(GenericFamily::Serif) {
        db.set_serif_family(name);
    }
    if let Some(name) = first(GenericFamily::SansSerif) {
        db.set_sans_serif_family(name);
    }
    if let Some(name) = first(GenericFamily::Monospace) {
        db.set_monospace_family(name);
    }
    if let Some(name) = first(GenericFamily::Cursive) {
        db.set_cursive_family(name);
    }
    if let Some(name) = first(GenericFamily::Fantasy) {
        db.set_fantasy_family(name);
    }
}

fn svg_fallback_families(
    db: &Database,
    generics: &HashMap<GenericFamily, Vec<String>>,
) -> Vec<String> {
    let mut families = Vec::new();
    for generic in [
        GenericFamily::SansSerif,
        GenericFamily::Serif,
        GenericFamily::Monospace,
        GenericFamily::SystemUi,
        GenericFamily::Emoji,
        GenericFamily::Math,
    ] {
        families.extend(generics.get(&generic).into_iter().flatten().cloned());
    }
    for face in db.faces() {
        if let Some((name, _)) = face.families.first() {
            families.push(name.clone());
        }
    }
    dedup_folded(&mut families);
    families
}

fn expand_css_family_list(input: &str, catalog: &Catalog) -> String {
    let parsed = split_css_families(input);
    let mut output = Vec::new();
    let mut changed = false;
    for raw in parsed {
        let Some(name) = decode_css_family(raw) else {
            output.push(raw.trim().to_string());
            continue;
        };
        let key = fold(&name);
        let quoted = is_quoted_css_family(raw);
        let generic = GenericFamily::parse(&name.to_ascii_lowercase());
        if !quoted && let Some(generic) = generic {
            let canonical = generic.to_string();
            changed |= raw.trim() != canonical;
            output.push(canonical);
            continue;
        }
        if catalog.installed_names.contains_key(&key) || (quoted && generic.is_some()) {
            if raw.contains('\\') {
                output.push(quote_css_family(&name));
                changed = true;
            } else {
                output.push(raw.trim().to_string());
            }
            continue;
        }
        let mut expanded = Vec::new();
        append_resolved_name(&mut expanded, &name, catalog, 0);
        if expanded.is_empty() {
            if raw.contains('\\') {
                output.push(quote_css_family(&name));
                changed = true;
            } else {
                output.push(raw.trim().to_string());
            }
        } else {
            changed = true;
            output.extend(expanded.into_iter().map(|name| quote_css_family(&name)));
        }
    }
    if changed {
        output.join(", ")
    } else {
        input.to_string()
    }
}

fn append_resolved_name(output: &mut Vec<String>, name: &str, catalog: &Catalog, depth: usize) {
    append_resolved_name_inner(output, name, catalog, &mut HashSet::new(), depth);
    dedup_folded(output);
}

fn append_resolved_name_inner(
    output: &mut Vec<String>,
    name: &str,
    catalog: &Catalog,
    visited: &mut HashSet<String>,
    depth: usize,
) {
    if depth > 32 {
        return;
    }
    let key = fold(name);
    if let Some(exact) = catalog.installed_names.get(&key) {
        output.push(exact.clone());
        return;
    }
    if !visited.insert(key.clone()) {
        return;
    }
    for candidate in catalog.alias_candidates.get(&key).into_iter().flatten() {
        if GenericFamily::parse(&candidate.to_ascii_lowercase()).is_some() {
            output.push(candidate.to_ascii_lowercase());
        } else {
            append_resolved_name_inner(output, candidate, catalog, visited, depth + 1);
        }
    }
}

fn add_default_generic(family: &str) -> Cow<'_, str> {
    if family.trim().is_empty() {
        Cow::Borrowed("sans-serif")
    } else if split_css_families(family).into_iter().any(|candidate| {
        !is_quoted_css_family(candidate)
            && decode_css_family(candidate)
                .and_then(|name| GenericFamily::parse(&name.to_ascii_lowercase()))
                .is_some()
    }) {
        Cow::Borrowed(family)
    } else {
        Cow::Owned(format!("{family}, sans-serif"))
    }
}

fn is_quoted_css_family(raw: &str) -> bool {
    matches!(raw.trim_start().chars().next(), Some('"' | '\''))
}

fn split_css_families(input: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(current) = quote {
            if ch == current {
                quote = None;
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch == ',' {
            result.push(&input[start..index]);
            start = index + ch.len_utf8();
        }
    }
    result.push(&input[start..]);
    result
}

fn decode_css_family(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let inner = match (value.as_bytes().first(), value.as_bytes().last()) {
        (Some(b'\''), Some(b'\'')) | (Some(b'"'), Some(b'"')) if value.len() >= 2 => {
            &value[1..value.len() - 1]
        }
        _ => value,
    };
    Some(decode_css_escapes(inner))
}

fn decode_css_escapes(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 {
            let Some((_, next)) = chars.peek().copied() else {
                break;
            };
            if next.is_ascii_hexdigit() {
                chars.next();
                hex.push(next);
            } else {
                break;
            }
        }
        if !hex.is_empty() {
            if chars
                .peek()
                .is_some_and(|(_, next)| next.is_ascii_whitespace())
            {
                chars.next();
            }
            let codepoint = u32::from_str_radix(&hex, 16).ok();
            output.push(codepoint.and_then(char::from_u32).unwrap_or('\u{FFFD}'));
        } else if let Some((_, next)) = chars.next()
            && !matches!(next, '\n' | '\r' | '\u{000C}')
        {
            output.push(next);
        }
    }
    output
}

fn quote_css_family(name: &str) -> String {
    let mut output = String::with_capacity(name.len() + 2);
    output.push('"');
    for ch in name.chars() {
        if matches!(ch, '"' | '\\') {
            output.push('\\');
        }
        output.push(ch);
    }
    output.push('"');
    output
}

fn dedup_folded(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(fold(value)));
}

fn fold(value: &str) -> String {
    value.case_fold().collect()
}

fn load_platform_config() -> FontConfig {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let mut loader = ConfigLoader::new();
        if let Some(root) = root_config_path() {
            loader.load(&root);
        }
        loader.config
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        FontConfig {
            directories: default_font_directories(),
            ..FontConfig::default()
        }
    }
}

struct ConfigLoader {
    config: FontConfig,
    seen: HashSet<PathBuf>,
    sysroot: Option<PathBuf>,
    total_bytes: u64,
}

impl ConfigLoader {
    fn new() -> Self {
        Self {
            config: FontConfig::default(),
            seen: HashSet::new(),
            sysroot: std::env::var_os("FONTCONFIG_SYSROOT")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            total_bytes: 0,
        }
    }

    fn load(&mut self, path: &Path) {
        if self.seen.len() >= MAX_CONFIG_FILES {
            return;
        }
        let rooted = self.apply_sysroot(path);
        let Ok(canonical) = fs::canonicalize(&rooted) else {
            return;
        };
        if !self.seen.insert(canonical.clone()) {
            return;
        }
        let Ok(metadata) = fs::metadata(&canonical) else {
            return;
        };
        if metadata.len() > MAX_CONFIG_FILE_BYTES
            || self.total_bytes.saturating_add(metadata.len()) > MAX_CONFIG_TOTAL_BYTES
        {
            return;
        }
        self.total_bytes += metadata.len();
        let Ok(xml) = fs::read_to_string(&canonical) else {
            return;
        };
        let Ok(document) = Document::parse_with_options(
            &xml,
            ParsingOptions {
                allow_dtd: true,
                nodes_limit: MAX_CONFIG_NODES,
                ..ParsingOptions::default()
            },
        ) else {
            return;
        };
        let root = document.root_element();
        if root.tag_name().name() != "fontconfig" {
            return;
        }
        for node in root.children().filter(Node::is_element) {
            match node.tag_name().name() {
                "dir" => {
                    if let Some(path) =
                        self.resolve_path(node, &canonical, ConfigPathKind::Directory)
                        && self.config.directories.len() < MAX_FONT_DIRECTORIES
                    {
                        self.config.directories.push(path);
                    }
                }
                "reset-dirs" => self.config.directories.clear(),
                "include" => {
                    if let Some(path) = self.resolve_path(node, &canonical, ConfigPathKind::Include)
                    {
                        self.load_include(&path);
                    }
                }
                "alias" => {
                    if self.config.aliases.len() < MAX_CONFIG_RULES
                        && let Some(alias) = parse_alias(node)
                    {
                        self.config.aliases.push(alias);
                    }
                }
                "match"
                    if self.config.aliases.len() < MAX_CONFIG_RULES
                        || self.config.locale_preferences.len() < MAX_CONFIG_RULES =>
                {
                    parse_pattern_match(
                        node,
                        &mut self.config.aliases,
                        &mut self.config.locale_preferences,
                    );
                    self.config.aliases.truncate(MAX_CONFIG_RULES);
                    self.config.locale_preferences.truncate(MAX_CONFIG_RULES);
                }
                _ => {}
            }
        }
    }

    fn load_include(&mut self, path: &Path) {
        let rooted = self.apply_sysroot(path);
        let Ok(metadata) = fs::metadata(&rooted) else {
            return;
        };
        if metadata.is_file() {
            self.load(&rooted);
            return;
        }
        if !metadata.is_dir() {
            return;
        }
        let Ok(entries) = fs::read_dir(rooted) else {
            return;
        };
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
                name.as_bytes().first().is_some_and(u8::is_ascii_digit) && name.ends_with(".conf")
            })
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            self.load(&path);
        }
    }

    fn resolve_path(
        &self,
        node: Node<'_, '_>,
        config_file: &Path,
        kind: ConfigPathKind,
    ) -> Option<PathBuf> {
        let value = node.text()?.trim();
        if value.is_empty() {
            return None;
        }
        let expanded = expand_tilde(Path::new(value));
        if expanded.is_absolute() {
            return Some(self.apply_sysroot(&expanded));
        }
        let prefix = node.attribute("prefix").unwrap_or("default");
        let path = match prefix {
            "xdg" => xdg_home(kind)?.join(expanded),
            "relative" => config_file.parent()?.join(expanded),
            "cwd" => std::env::current_dir().ok()?.join(expanded),
            _ => match kind {
                // Fontconfig's default for <dir> is cwd; includes are config
                // names and resolve relative to the including file/FONTCONFIG_PATH.
                ConfigPathKind::Directory => std::env::current_dir().ok()?.join(expanded),
                ConfigPathKind::Include => config_file.parent()?.join(expanded),
            },
        };
        Some(self.apply_sysroot(&path))
    }

    fn apply_sysroot(&self, path: &Path) -> PathBuf {
        let Some(sysroot) = &self.sysroot else {
            return path.to_path_buf();
        };
        if path.starts_with(sysroot) {
            return path.to_path_buf();
        }
        if let Ok(relative) = path.strip_prefix(Path::new("/")) {
            sysroot.join(relative)
        } else {
            sysroot.join(path)
        }
    }
}

fn root_config_path() -> Option<PathBuf> {
    let file = std::env::var_os("FONTCONFIG_FILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fonts.conf"));
    if file.is_absolute() {
        return Some(file);
    }
    let paths = std::env::var_os("FONTCONFIG_PATH")
        .filter(|value| !value.is_empty())
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_else(|| {
            vec![if cfg!(target_os = "freebsd") {
                PathBuf::from("/usr/local/etc/fonts")
            } else {
                PathBuf::from("/etc/fonts")
            }]
        });
    paths
        .iter()
        .map(|directory| directory.join(&file))
        .find(|path| path.is_file())
        .or_else(|| paths.first().map(|directory| directory.join(file)))
}

fn parse_alias(node: Node<'_, '_>) -> Option<AliasRule> {
    let family = child_text(node, "family")?;
    let group = |name| {
        node.children()
            .find(|child| child.is_element() && child.tag_name().name() == name)
            .map(family_children)
            .unwrap_or_default()
    };
    Some(AliasRule {
        family,
        prefer: group("prefer"),
        accept: group("accept"),
        default: group("default"),
    })
}

fn parse_pattern_match(
    node: Node<'_, '_>,
    aliases: &mut Vec<AliasRule>,
    locales: &mut Vec<LocalePreference>,
) {
    if !matches!(node.attribute("target"), None | Some("pattern")) {
        return;
    }
    let tests = node
        .children()
        .filter(|child| child.is_element() && child.tag_name().name() == "test")
        .collect::<Vec<_>>();
    if tests.is_empty()
        || tests.iter().any(|test| {
            !comparison_is_equality(*test)
                || !matches!(test.attribute("name"), Some("family" | "lang"))
        })
    {
        return;
    }

    let mut tested_families = Vec::new();
    let mut tested_languages = Vec::new();
    let mut edited_families = Vec::new();
    for child in node.children().filter(Node::is_element) {
        match (child.tag_name().name(), child.attribute("name")) {
            ("test", Some("family")) if comparison_is_equality(child) => {
                tested_families.extend(expression_strings(child));
            }
            ("test", Some("lang")) if comparison_is_equality(child) => {
                tested_languages.extend(expression_strings(child));
            }
            ("edit", Some("family"))
                if !matches!(child.attribute("mode"), Some("delete" | "delete_all")) =>
            {
                edited_families.extend(expression_strings(child));
            }
            _ => {}
        }
    }
    if edited_families.is_empty() {
        return;
    }
    // A conditional family+language rewrite cannot be represented as either
    // an unconditional alias or a script fallback preference. Ignore it
    // rather than broadening the condition and changing unrelated text.
    match (tested_families.is_empty(), tested_languages.is_empty()) {
        (false, true) => {
            for family in tested_families {
                aliases.push(AliasRule {
                    family,
                    prefer: edited_families.clone(),
                    ..AliasRule::default()
                });
            }
        }
        (true, false) => {
            for language in tested_languages {
                locales.push(LocalePreference {
                    language,
                    families: edited_families.clone(),
                });
            }
        }
        _ => {}
    }
}

fn comparison_is_equality(node: Node<'_, '_>) -> bool {
    matches!(node.attribute("compare"), None | Some("eq"))
}

fn expression_strings(node: Node<'_, '_>) -> Vec<String> {
    node.descendants()
        .filter(|child| child.is_element() && child.tag_name().name() == "string")
        .filter_map(|child| child.text().map(str::trim))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn child_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)?
        .text()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn family_children(node: Node<'_, '_>) -> Vec<String> {
    node.children()
        .filter(|child| child.is_element() && child.tag_name().name() == "family")
        .filter_map(|child| child.text())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn xdg_home(kind: ConfigPathKind) -> Option<PathBuf> {
    let (variable, suffix) = match kind {
        ConfigPathKind::Directory => ("XDG_DATA_HOME", ".local/share"),
        ConfigPathKind::Include => ("XDG_CONFIG_HOME", ".config"),
    };
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(suffix)))
}

fn expand_tilde(path: &Path) -> PathBuf {
    if path == Path::new("~") {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf());
    }
    if let Ok(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(relative);
    }
    path.to_path_buf()
}

fn discover_font_paths(configured: &[PathBuf]) -> Vec<PathBuf> {
    let directories = if configured.is_empty() {
        default_font_directories()
    } else {
        configured.to_vec()
    };
    let mut paths = Vec::new();
    let mut seen_directories = HashSet::new();
    let mut seen_files = HashSet::new();
    for directory in directories {
        scan_font_path(
            &directory,
            0,
            &mut seen_directories,
            &mut seen_files,
            &mut paths,
        );
        if paths.len() >= MAX_FONT_FILES {
            break;
        }
    }
    paths.sort();
    paths
}

fn scan_font_path(
    path: &Path,
    depth: usize,
    seen_directories: &mut HashSet<PathBuf>,
    seen_files: &mut HashSet<PathBuf>,
    output: &mut Vec<PathBuf>,
) {
    if depth > MAX_FONT_DIRECTORY_DEPTH
        || seen_directories.len() >= MAX_FONT_DIRECTORIES
        || output.len() >= MAX_FONT_FILES
    {
        return;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_file() {
        if metadata.len() <= MAX_FONT_FILE_BYTES && is_supported_font(path) {
            let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if seen_files.insert(canonical.clone()) {
                output.push(canonical);
            }
        }
        return;
    }
    if !metadata.is_dir() {
        return;
    }
    let Ok(canonical) = fs::canonicalize(path) else {
        return;
    };
    if !seen_directories.insert(canonical.clone()) {
        return;
    }
    let Ok(entries) = fs::read_dir(canonical) else {
        return;
    };
    let mut children = entries
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    children.sort();
    for child in children {
        scan_font_path(&child, depth + 1, seen_directories, seen_files, output);
        if output.len() >= MAX_FONT_FILES {
            break;
        }
    }
}

fn is_supported_font(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "ttf" | "otf" | "ttc" | "otc"
            )
        })
}

fn default_font_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = std::env::var_os("SYSTEMROOT") {
            directories.push(PathBuf::from(root).join("Fonts"));
        } else {
            directories.push(PathBuf::from(r"C:\Windows\Fonts"));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            let profile = PathBuf::from(profile);
            directories.push(profile.join(r"AppData\Local\Microsoft\Windows\Fonts"));
            directories.push(profile.join(r"AppData\Roaming\Microsoft\Windows\Fonts"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        directories.extend([
            PathBuf::from("/System/Library/Fonts"),
            PathBuf::from("/Library/Fonts"),
            PathBuf::from("/Network/Library/Fonts"),
        ]);
        if let Some(home) = std::env::var_os("HOME") {
            directories.push(PathBuf::from(home).join("Library/Fonts"));
        }
    }
    #[cfg(target_os = "freebsd")]
    directories.extend([
        PathBuf::from("/usr/local/share/fonts"),
        PathBuf::from("/usr/share/fonts"),
    ]);
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "freebsd"))))]
    directories.extend([
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ]);
    #[cfg(target_os = "redox")]
    directories.push(PathBuf::from("/ui/fonts"));
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        directories.push(home.join(".local/share/fonts"));
        directories.push(home.join(".fonts"));
    }
    directories
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_font_resource_rejects_unsupported_container_and_keeps_sfnt() {
        assert!(PageFont::from_web_resource("Legacy".into(), b"LP\0\0EOT".to_vec()).is_none());
        let sfnt = [b"OTTO".as_slice(), b"payload"].concat();
        let font = PageFont::from_web_resource("Site".into(), sfnt.clone()).unwrap();
        assert_eq!(font.family, "Site");
        assert_eq!(font.bytes, sfnt);
    }

    #[test]
    fn css_family_split_preserves_commas_inside_strings() {
        assert_eq!(
            split_css_families(r#""A, B", C\,D, sans-serif"#),
            vec![r#""A, B""#, r#" C\,D"#, " sans-serif"]
        );
    }

    #[test]
    fn css_escapes_decode_without_normalizing_names() {
        assert_eq!(
            decode_css_family(r#""Stra\DF e""#).as_deref(),
            Some("Straße")
        );
        assert_eq!(fold("Straße"), fold("STRASSE"));
    }

    #[test]
    fn quoted_generic_keyword_remains_a_named_family() {
        assert_eq!(add_default_generic(r#""serif""#), r#""serif", sans-serif"#);
    }

    #[test]
    fn aliases_and_pattern_substitutions_are_collected_in_document_order() {
        let xml = r#"
            <fontconfig>
              <alias><family>sans-serif</family><prefer>
                <family>First Sans</family><family>Second Sans</family>
              </prefer></alias>
              <match target="pattern">
                <test name="family"><string>sans</string></test>
                <edit name="family" mode="assign"><string>sans-serif</string></edit>
              </match>
              <match>
                <test name="lang"><string>ja</string></test>
                <edit name="family" mode="prepend"><string>Japanese UI</string></edit>
              </match>
            </fontconfig>
        "#;
        let doc = Document::parse(xml).unwrap();
        let mut aliases = Vec::new();
        let mut locales = Vec::new();
        for node in doc.root_element().children().filter(Node::is_element) {
            match node.tag_name().name() {
                "alias" => aliases.push(parse_alias(node).unwrap()),
                "match" => parse_pattern_match(node, &mut aliases, &mut locales),
                _ => {}
            }
        }
        assert_eq!(aliases[0].prefer, ["First Sans", "Second Sans"]);
        assert_eq!(aliases[1].family, "sans");
        assert_eq!(aliases[1].prefer, ["sans-serif"]);
        assert_eq!(locales[0].language, "ja");
        assert_eq!(locales[0].families, ["Japanese UI"]);
    }

    #[test]
    fn conditional_pattern_rules_are_never_broadened() {
        let xml = r#"
            <fontconfig><match>
              <test name="family"><string>sans-serif</string></test>
              <test name="lang"><string>ja</string></test>
              <edit name="family" mode="prepend"><string>Japanese Sans</string></edit>
            </match></fontconfig>
        "#;
        let doc = Document::parse(xml).unwrap();
        let mut aliases = Vec::new();
        let mut locales = Vec::new();
        parse_pattern_match(
            doc.root_element().first_element_child().unwrap(),
            &mut aliases,
            &mut locales,
        );
        assert!(aliases.is_empty());
        assert!(locales.is_empty());
    }

    #[test]
    fn alias_graph_resolution_is_cycle_safe_and_deduplicated() {
        let config = FontConfig {
            aliases: vec![
                AliasRule {
                    family: "sans-serif".into(),
                    prefer: vec!["branch-a".into(), "branch-b".into()],
                    ..AliasRule::default()
                },
                AliasRule {
                    family: "branch-a".into(),
                    prefer: vec!["installed".into(), "branch-b".into()],
                    ..AliasRule::default()
                },
                AliasRule {
                    family: "branch-b".into(),
                    prefer: vec!["branch-a".into(), "installed".into()],
                    ..AliasRule::default()
                },
            ],
            ..FontConfig::default()
        };
        let installed = HashMap::from([(fold("Installed"), "Installed".to_string())]);
        let aliases = index_aliases(&config);
        let mut output = Vec::new();
        expand_config_name(
            "sans-serif",
            &aliases,
            &installed,
            &mut output,
            &mut HashSet::new(),
            0,
        );
        dedup_folded(&mut output);
        assert_eq!(output, ["Installed"]);
    }

    #[test]
    fn embedded_fonts_satisfy_all_mandatory_css_generics() {
        let mut db = Database::new();
        let mut collection = Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        });
        for font in EMERGENCY_FONTS {
            db.load_font_source(Source::Binary(Arc::new(*font)));
            collection.register_fonts(Blob::new(Arc::new(*font)), None);
        }
        let names = collect_installed_names(&db);
        for name in names.values() {
            assert!(collection.family_id(name).is_some());
        }
        let ids = collection
            .family_names()
            .map(str::to_string)
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|name| collection.family_id(&name))
            .collect::<HashSet<_>>();
        let latin = Script::from_bytes(*b"Latn");
        let mut fontique_monospace = false;
        let mut fontique_proportional = false;
        let mut fontique_latin = false;
        for id in ids {
            let family = collection.family(id).unwrap();
            for font in family.fonts() {
                fontique_monospace |= font.is_monospaced();
                fontique_proportional |= !font.is_monospaced();
                fontique_latin |= font.script_coverage(latin) != 0;
            }
        }
        let generics = resolve_generic_names(&HashMap::new(), &names, None, None);
        assert!(!mandatory_generics_missing(&generics));
        assert!(fontique_monospace && fontique_proportional && fontique_latin);
        assert!(db.faces().any(|face| face.monospaced));
        assert!(db.faces().any(|face| !face.monospaced));
    }

    #[test]
    fn discovery_is_deterministic_and_deduplicates_overlapping_roots() {
        let roots = default_font_directories();
        let once = discover_font_paths(&roots);
        let mut repeated = roots.clone();
        repeated.extend(roots);
        let twice = discover_font_paths(&repeated);
        assert_eq!(once, twice);
        assert!(once.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn process_catalog_has_mandatory_css_generics() {
        let catalog = catalog();
        for generic in [
            GenericFamily::Serif,
            GenericFamily::SansSerif,
            GenericFamily::Monospace,
        ] {
            assert!(
                catalog
                    .generic_names
                    .get(&generic)
                    .is_some_and(|names| !names.is_empty()),
                "{generic} must resolve when installed fonts exist"
            );
        }
    }

    #[test]
    fn configured_text_collection_shapes_common_scripts() {
        let mut context = font_context();
        for family in [
            GenericFamily::Serif,
            GenericFamily::SansSerif,
            GenericFamily::Monospace,
        ] {
            assert!(context.collection.generic_families(family).next().is_some());
        }
        for name in catalog().installed_names.values() {
            assert!(
                context.collection.family_id(name).is_some(),
                "localized/collection family name {name:?} must be shared by HTML and SVG"
            );
        }
    }

    #[test]
    fn svg_and_text_catalogs_use_the_same_file_set() {
        let catalog = catalog();
        let svg_paths = svg_catalog()
            .db
            .faces()
            .filter_map(|face| match &face.source {
                Source::File(path) => Some(path.clone()),
                Source::Binary(_) => None,
            })
            .collect::<HashSet<_>>();
        assert_eq!(
            svg_paths,
            catalog.paths.iter().cloned().collect::<HashSet<_>>()
        );
    }
}
