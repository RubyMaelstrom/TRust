//! Color schemes: CSS Color Adjust 1 #color-scheme-prop and
//! #color-scheme-resolution, HTML #meta-color-scheme, and the values that
//! depend on an element's color scheme: CSS Color 4 #css-system-colors and
//! CSS Color 5 #light-dark, which compute to concrete colors.

use std::cell::RefCell;

use rustc_hash::FxHashMap;

use super::{ComputeView, NodeId, StyleBackend};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ColorScheme {
    Light,
    Dark,
}

/// The user's preferred color scheme, which `prefers-color-scheme` reports.
pub(crate) const PREFERRED: ColorScheme = ColorScheme::Dark;

impl ColorScheme {
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

/// A color scheme support: the supported schemes this UA knows, in order.
/// (The `only` flag never changes the result for an ordinary preference.)
type Support = Vec<ColorScheme>;

/// Parse `normal | [ light | dark | <custom-ident> ]+ && only?`: `None` when
/// invalid, `Some(None)` for `normal` (no support declared).
fn parse_support(value: &str) -> Option<Option<Support>> {
    let words: Vec<String> = value
        .split_ascii_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    if let [word] = words.as_slice()
        && word == "normal"
    {
        return Some(None);
    }
    let mut schemes = Vec::new();
    let mut only = 0;
    let mut idents = 0;
    for (index, word) in words.iter().enumerate() {
        match word.as_str() {
            "only" => {
                // `only` comes first or last, once.
                if index != 0 && index != words.len() - 1 {
                    return None;
                }
                only += 1;
            }
            "light" => schemes.push(ColorScheme::Light),
            "dark" => schemes.push(ColorScheme::Dark),
            "normal" | "initial" | "inherit" | "unset" | "revert" | "revert-layer" | "default" => {
                return None;
            }
            other => {
                let mut chars = other.chars();
                let start = chars.next().is_some_and(|c| {
                    c.is_ascii_alphabetic() || c == '_' || c == '-' || !c.is_ascii()
                });
                if !start
                    || !other
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || !c.is_ascii())
                {
                    return None;
                }
            }
        }
        if word != "only" {
            idents += 1;
        }
    }
    (idents > 0 && only <= 1).then_some(Some(schemes))
}

/// #color-scheme-resolution's used color scheme, for an ordinary preference.
fn used(support: Option<&Support>, preference: ColorScheme) -> ColorScheme {
    match support.filter(|schemes| !schemes.is_empty()) {
        None => ColorScheme::Light,
        Some(schemes) if schemes.contains(&preference) => preference,
        Some(schemes) => schemes[0],
    }
}

/// The page's supported color schemes, memoized per document for one
/// style epoch (a `<meta>` mutation changes the sheet set's epoch).
#[derive(Default)]
pub(super) struct PageSupport(RefCell<FxHashMap<NodeId, (u64, Option<Support>)>>);

impl PageSupport {
    /// Retained heap bytes, or `None` while borrowed.
    pub(super) fn retained_bytes(&self) -> Option<usize> {
        let map = self.0.try_borrow().ok()?;
        let entries = map.capacity() * std::mem::size_of::<(NodeId, (u64, Option<Support>))>();
        let lists: usize = map
            .values()
            .map(|(_, support)| support.as_ref().map_or(0, Vec::capacity))
            .sum();
        Some(entries + lists * std::mem::size_of::<ColorScheme>())
    }
}

impl<B: StyleBackend + ?Sized> ComputeView<'_, B> {
    /// The element color scheme of `id`.
    pub(crate) fn color_scheme(&self, id: NodeId) -> ColorScheme {
        let nodes = self.style_view().nodes;
        let document = nodes.owner_document_of(id).unwrap_or(super::DOCUMENT);
        // An embedded document's preference is its embedding element's scheme.
        let preference = match nodes.try_parent(document).flatten() {
            Some(frame) if matches!(self.tag_name(frame), Some("iframe" | "frame")) => {
                self.color_scheme(frame)
            }
            _ => PREFERRED,
        };
        let own = self
            .computed_value_resolved(id, "color-scheme")
            .as_deref()
            .and_then(parse_support)
            .flatten();
        match own {
            Some(support) => used(Some(&support), preference),
            None => used(self.page_color_schemes(document).as_ref(), preference),
        }
    }

    /// HTML #meta-color-scheme: the first valid `<meta name=color-scheme>`
    /// content in the document's tree order.
    fn page_color_schemes(&self, document: NodeId) -> Option<Support> {
        let epoch = self.style_epoch();
        if let Some((at, support)) = self.page_support().0.borrow().get(&document)
            && *at == epoch
        {
            return support.clone();
        }
        let nodes = self.style_view().nodes;
        let support = self
            .descendants(document)
            .filter(|&id| nodes.owner_document(id) == document && self.is_color_scheme_meta(id))
            .find_map(|id| self.attr(id, "content").and_then(parse_support))
            .flatten();
        self.page_support()
            .0
            .borrow_mut()
            .insert(document, (epoch, support.clone()));
        support
    }

    pub(super) fn is_color_scheme_meta(&self, id: NodeId) -> bool {
        self.style_view().nodes.is_element(id)
            && self.tag_name(id) == Some("meta")
            && self
                .attr(id, "name")
                .is_some_and(|name| name.eq_ignore_ascii_case("color-scheme"))
            && self.attr(id, "content").is_some()
    }
}

/// CSS Color 4 #css-system-colors, with the deprecated colors mapped as
/// #deprecated-system-colors lists. The light palette is TRust's own; the
/// dark one follows Gecko's generic dark colors.
pub(crate) fn system_color(name: &str, scheme: ColorScheme) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    let name = match name.as_str() {
        "activecaption" | "appworkspace" | "background" | "inactivecaption" | "infobackground"
        | "menu" | "scrollbar" | "window" => "canvas",
        "captiontext" | "infotext" | "menutext" | "windowtext" => "canvastext",
        "buttonhighlight" | "buttonshadow" | "threedface" => "buttonface",
        "activeborder" | "inactiveborder" | "threeddarkshadow" | "threedhighlight"
        | "threedlightshadow" | "threedshadow" | "windowframe" => "buttonborder",
        "inactivecaptiontext" => "graytext",
        other => other,
    };
    let light = scheme == ColorScheme::Light;
    Some(match name {
        "canvas" | "field" | "buttonface" if light => "#ffffff",
        "canvas" => "#1c1b22",
        "field" | "buttonface" => "#2b2a33",
        "canvastext" | "fieldtext" | "buttontext" if light => "#000000",
        "canvastext" | "fieldtext" | "buttontext" => "#fbfbfe",
        "buttonborder" if light => "#767676",
        "graytext" if light => "#6d6d6d",
        "buttonborder" | "graytext" => "#75757a",
        "linktext" if light => "#0000ee",
        "linktext" => "#00cadb",
        "visitedtext" if light => "#551a8b",
        "visitedtext" => "#ffadff",
        "activetext" if light => "#ff0000",
        "activetext" => "#ff6666",
        "highlight" | "selecteditem" if light => "#0078d7",
        "highlight" => "rgba(0, 221, 255, 0.306)",
        "selecteditem" => "#00ddff",
        "highlighttext" | "selecteditemtext" if light => "#000000",
        "highlighttext" => "#fbfbfe",
        "selecteditemtext" => "#2b2a33",
        "accentcolor" => "#0078d7",
        "accentcolortext" | "marktext" => "#000000",
        "mark" => "#ffff00",
        _ => return None,
    })
}

/// Properties whose values can hold a `<color>` (or a `light-dark()` image).
pub(super) fn holds_colors(property: &str) -> bool {
    property.ends_with("color")
        || property.ends_with("-image")
        || property.ends_with("-shadow")
        || matches!(
            property,
            "fill" | "stroke" | "filter" | "background" | "border-image-source"
        )
}

const SYSTEM_COLORS: &[&str] = &[
    "accentcolor",
    "accentcolortext",
    "activeborder",
    "activecaption",
    "activetext",
    "appworkspace",
    "background",
    "buttonborder",
    "buttonface",
    "buttonhighlight",
    "buttonshadow",
    "buttontext",
    "canvas",
    "canvastext",
    "captiontext",
    "field",
    "fieldtext",
    "graytext",
    "highlight",
    "highlighttext",
    "inactiveborder",
    "inactivecaption",
    "inactivecaptiontext",
    "infobackground",
    "infotext",
    "linktext",
    "mark",
    "marktext",
    "menu",
    "menutext",
    "scrollbar",
    "selecteditem",
    "selecteditemtext",
    "threeddarkshadow",
    "threedface",
    "threedhighlight",
    "threedlightshadow",
    "threedshadow",
    "visitedtext",
    "window",
    "windowframe",
    "windowtext",
];

/// Replace the system colors and `light-dark()` functions in `value` with
/// their computed colors for `scheme`; `None` when there are none.
pub(crate) fn resolve(value: &str, scheme: ColorScheme) -> Option<String> {
    // Most values hold neither; find out without allocating.
    let mentions = value
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .any(|word| {
            word.eq_ignore_ascii_case("light-dark")
                || SYSTEM_COLORS
                    .iter()
                    .any(|name| word.eq_ignore_ascii_case(name))
        });
    if !mentions {
        return None;
    }
    let out = substitute(value, scheme);
    (out != value).then_some(out)
}

fn substitute(value: &str, scheme: ColorScheme) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\'' {
            let end = string_end(bytes, i);
            out.push_str(&value[i..end]);
            i = end;
        } else if b == b'#'
            || b.is_ascii_digit()
            || (b == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            // A hash or a number with its unit: never a color keyword.
            let end = i
                + 1
                + value[i + 1..]
                    .find(|c: char| {
                        !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '%'))
                    })
                    .unwrap_or(value.len() - i - 1);
            out.push_str(&value[i..end]);
            i = end;
        } else if b.is_ascii_alphabetic() || b == b'-' || b == b'_' {
            let end = i + value[i..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .unwrap_or(value.len() - i);
            let word = &value[i..end];
            if bytes.get(end) == Some(&b'(') {
                let close = block_end(bytes, end);
                if word.eq_ignore_ascii_case("url") {
                    out.push_str(&value[i..close]);
                } else if word.eq_ignore_ascii_case("light-dark") {
                    let inner = &value[end + 1..close.saturating_sub(1).max(end + 1)];
                    let args = top_level_commas(inner);
                    let chosen = if scheme == ColorScheme::Light {
                        args.first()
                    } else {
                        args.get(1)
                    };
                    out.push_str(&substitute(chosen.map_or("", |arg| arg.trim()), scheme));
                } else {
                    out.push_str(word);
                    out.push('(');
                    out.push_str(&substitute(
                        &value[end + 1..close.saturating_sub(1).max(end + 1)],
                        scheme,
                    ));
                    if bytes.get(close - 1) == Some(&b')') {
                        out.push(')');
                    }
                }
                i = close;
            } else {
                out.push_str(system_color(word, scheme).unwrap_or(word));
                i = end;
            }
        } else {
            let len = value[i..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&value[i..i + len]);
            i += len;
        }
    }
    out
}

fn string_end(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// The index just past the `)` closing the block opened at `open`.
fn block_end(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                i = string_end(bytes, i);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

fn top_level_commas(inner: &str) -> Vec<&str> {
    let bytes = inner.as_bytes();
    let mut parts = Vec::new();
    let (mut start, mut depth, mut i) = (0, 0usize, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                i = string_end(bytes, i);
                continue;
            }
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&inner[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::Dom;

    #[test]
    fn elements_take_the_page_or_their_own_color_scheme() {
        // HTML #meta-color-scheme: the first valid meta in tree order.
        let mut dom = Dom::parse_document(
            r#"<meta name=color-scheme content="light, dark"><meta name=color-scheme content="light dark">
            <body><p id=p>x</p><div id=light style="color-scheme:light"><span id=s
            style="background:light-dark(red,blue);border-color:ButtonBorder">y</span></div></body>"#,
        );
        let id = |dom: &Dom, name: &str| dom.get_by_id(name).unwrap();
        let p = id(&dom, "p");
        assert_eq!(dom.color_scheme(p), ColorScheme::Dark);
        assert_eq!(
            dom.computed_value_resolved(p, "color").as_deref(),
            Some("#fbfbfe"),
            "CanvasText of the dark scheme"
        );
        let s = id(&dom, "s");
        assert_eq!(dom.color_scheme(s), ColorScheme::Light);
        assert_eq!(
            dom.computed_value_resolved(s, "background-color")
                .as_deref(),
            Some("red")
        );
        assert_eq!(
            dom.computed_value_resolved(s, "border-top-color")
                .as_deref(),
            Some("#767676")
        );
        // A later change to the meta re-runs the algorithm.
        let metas: Vec<_> = dom
            .descendants(crate::dom::DOCUMENT)
            .filter(|&n| dom.tag_name(n) == Some("meta"))
            .collect();
        dom.set_attr(metas[1], "content", "light");
        assert_eq!(dom.color_scheme(p), ColorScheme::Light);
        assert_eq!(dom.computed_value_resolved(p, "color"), None);
    }

    #[test]
    fn color_scheme_values_parse_and_resolve() {
        use ColorScheme::{Dark, Light};
        assert_eq!(parse_support("normal"), Some(None));
        assert_eq!(parse_support("light dark"), Some(Some(vec![Light, Dark])));
        assert_eq!(parse_support("only dark"), Some(Some(vec![Dark])));
        assert_eq!(parse_support("dark only"), Some(Some(vec![Dark])));
        assert_eq!(parse_support("sepia"), Some(Some(vec![])));
        assert_eq!(parse_support("light normal"), None);
        assert_eq!(parse_support("only"), None);
        assert_eq!(parse_support("light only dark"), None);
        assert_eq!(parse_support("light, dark"), None);
        // #color-scheme-resolution with an ordinary dark preference.
        assert_eq!(used(None, Dark), Light);
        assert_eq!(used(Some(&vec![]), Dark), Light, "no supported scheme");
        assert_eq!(used(Some(&vec![Light, Dark]), Dark), Dark);
        assert_eq!(used(Some(&vec![Light]), Dark), Light);
        assert_eq!(used(Some(&vec![Dark]), Light), Dark);
    }

    #[test]
    fn system_colors_and_light_dark_compute_for_the_scheme() {
        use ColorScheme::{Dark, Light};
        assert_eq!(resolve("Canvas", Light).as_deref(), Some("#ffffff"));
        assert_eq!(resolve("CanvasText", Dark).as_deref(), Some("#fbfbfe"));
        assert_eq!(resolve("Window", Dark).as_deref(), Some("#1c1b22"));
        assert_eq!(
            resolve("light-dark(white, rgb(1 2 3))", Dark).as_deref(),
            Some("rgb(1 2 3)")
        );
        assert_eq!(
            resolve("0 0 2px light-dark(Field, #123), inset 1px 1px red", Light).as_deref(),
            Some("0 0 2px #ffffff, inset 1px 1px red")
        );
        assert_eq!(
            resolve(
                "linear-gradient(to right, ButtonFace 10%, light-dark(red, blue))",
                Dark
            )
            .as_deref(),
            Some("linear-gradient(to right, #2b2a33 10%, blue)")
        );
        assert_eq!(
            resolve("url(canvas.png), light-dark(url(a.png), none)", Dark).as_deref(),
            Some("url(canvas.png), none")
        );
        assert_eq!(resolve("#canvas 10px red", Light), None);
        assert_eq!(resolve("'Canvas'", Light), None);
    }
}
