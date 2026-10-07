//! Shared declaration operations for CSSOM and CSS Conditional Rules.
//!
//! CSSOM #the-cssstyledeclaration-interface, CSS Syntax 3 #consume-declaration,
//! and CSS Conditional 3 #support-definition. Local CSSWG snapshot:
//! 81c27f68690138345b2b3b6af8ccc42dad3dca1d (2026-09-06).
use super::*;
use serde_json::{Value, json};

pub(crate) type Declarations = Vec<(String, String, bool)>;

pub(crate) struct Sheet {
    pub text: String,
    pub media: String,
    pub disabled: bool,
}

impl Dom {
    /// CSSOM View #dom-element-clientwidth / #dom-element-clientheight.
    /// Each Document, including child navigables, has its own root and mode.
    pub(crate) fn cssom_client_viewport(&self, id: NodeId) -> bool {
        let Some(document) = self.owner_document(id) else {
            return false;
        };
        let root = self.style_scope_root_element(id);
        let quirks = self.document_mode(document) == QuirksMode::Quirks;
        if quirks {
            self.tag_name(id) == Some("body") && self.node(id).parent == root
        } else {
            root == Some(id)
        }
    }

    /// CSSOM View §7 reads computed positioning state, not an author-visible
    /// `getComputedStyle()` call. Keep this small native query allocation-light:
    /// offset walks can inspect the same ancestors thousands of times in a task.
    /// Bits 0/1 identify fixed/other non-static positioning; bit 2 identifies a
    /// fixed-position containing block (which also contains absolute positions).
    pub(crate) fn cssom_offset_style(&self, id: NodeId) -> u8 {
        let value = |property: &str| self.computed_value(id, property).unwrap_or_default();
        let position = value("position");
        let mut flags = if position.is_empty() || position.eq_ignore_ascii_case("static") {
            0
        } else if position.eq_ignore_ascii_case("fixed") {
            1
        } else {
            2
        };
        let non_none = |property| {
            let value = value(property);
            !value.is_empty() && !value.eq_ignore_ascii_case("none")
        };
        // Transforms 1 §3, Positioned Layout 3 §2.1, Containment 2 §3.2/§3.4,
        // and Will Change §2. Filter Effects 1 §5 excludes each Document root.
        let root = self.style_scope_root_element(id) == Some(id);
        let fixed_block = [
            "transform",
            "translate",
            "rotate",
            "scale",
            "perspective",
            "backdrop-filter",
        ]
        .into_iter()
        .any(non_none)
            || (!root && non_none("filter"))
            || value("contain").split_ascii_whitespace().any(|token| {
                ["layout", "paint", "strict", "content"]
                    .iter()
                    .any(|keyword| token.eq_ignore_ascii_case(keyword))
            })
            || value("will-change").split(',').any(|token| {
                [
                    "transform",
                    "translate",
                    "rotate",
                    "scale",
                    "perspective",
                    "backdrop-filter",
                    "contain",
                ]
                .iter()
                .any(|keyword| token.trim().eq_ignore_ascii_case(keyword))
                    || (!root && token.trim().eq_ignore_ascii_case("filter"))
            });
        if fixed_block {
            flags |= 4;
        }
        flags
    }

    pub(crate) fn set_cssom_inline(&mut self, id: NodeId, declarations: Declarations) {
        if !self.is_valid(id) || self.tag_name(id).is_none() {
            return;
        }
        let text = serialize(&declarations, false);
        // CSSOM #update-style-attribute-for: an unchanged declaration block
        // serializes to the attribute value it already has. Neither the
        // cascade's declarations nor the attribute change, so nothing is
        // invalidated (as for an idempotent setAttribute).
        if self.cssom_inline.get(&id) == Some(&declarations)
            && self.get_attribute(id, "style") == Some(text.as_str())
        {
            return;
        }
        self.set_attr(id, "style", &text);
        self.cssom_inline.insert(id, declarations);
        self.touch_attr(id, "style");
    }

    pub(super) fn reset_cssom_sheet(&mut self, id: NodeId) {
        self.cssom_sheets.remove(&id);
        let version = self.cssom_sheet_versions.entry(id).or_default();
        *version = version.wrapping_add(1);
    }
    pub(super) fn note_cssom_sheet_attribute(&mut self, id: NodeId, name: &str) {
        let media = self.attr(id, "media").unwrap_or("").to_string();
        let disabled = self.attr(id, "disabled").is_some();
        if let Some(sheet) = self.cssom_sheets.get_mut(&id) {
            if name.eq_ignore_ascii_case("media") {
                sheet.media = media;
            }
            if name.eq_ignore_ascii_case("disabled") {
                sheet.disabled = disabled;
            }
        }
        if self.tag_name(id) == Some("link") && matches!(name, "href" | "rel" | "type") {
            self.reset_cssom_sheet(id);
        }
    }
    pub(crate) fn cssom_sheet_source(&self, id: NodeId) -> Option<String> {
        if !self.is_connected(id) {
            return None;
        }
        let text = match self.tag_name(id)? {
            "style"
                if self
                    .attr(id, "type")
                    .is_none_or(|v| v.is_empty() || v.eq_ignore_ascii_case("text/css")) =>
            {
                self.text_content(id)
            }
            "link"
                if self.attr(id, "rel").is_some_and(|v| {
                    v.split_ascii_whitespace()
                        .any(|v| v.eq_ignore_ascii_case("stylesheet"))
                }) =>
            {
                self.external_sheets.get(&id)?.clone()
            }
            _ => return None,
        };
        Some(
            json!([
                self.cssom_sheet_versions.get(&id).copied().unwrap_or(0),
                text
            ])
            .to_string(),
        )
    }

    pub(crate) fn set_cssom_sheet(
        &mut self,
        id: NodeId,
        text: String,
        media: String,
        disabled: bool,
    ) {
        if !matches!(self.tag_name(id), Some("style" | "link")) {
            return;
        }
        self.touch_style_at(id);
        self.cssom_sheets.insert(
            id,
            Sheet {
                text,
                media,
                disabled,
            },
        );
    }
}

pub(super) fn valid_property_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '-' || first == '_' || !first.is_ascii())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || !c.is_ascii())
        && name != "-"
        && name != "--"
}

/// Reject bad strings, unmatched delimiters and top-level declaration
/// separators. Nested blocks, including semicolons in custom properties,
/// remain component values; no string is searched for syntax inside itself.
pub(crate) fn valid_value(value: &str) -> bool {
    let mut blocks = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if matches!(c, '\n' | '\r' | '\x0c') {
                return false;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => blocks.push(')'),
            '[' => blocks.push(']'),
            '{' => blocks.push('}'),
            ')' | ']' | '}' if blocks.pop() != Some(c) => return false,
            ';' | '!' if blocks.is_empty() => return false,
            _ => {}
        }
    }
    quote.is_none() && blocks.is_empty() && !escaped
}

// CSSOM #concept-declarations-specified-order stores longhands. The cascade
// expander also retains raw background shorthand metadata for legacy reads;
// that untracked entry is not a declaration to validate or expose in CSSOM.
fn longhands(property: &str, value: &str) -> Vec<(String, String)> {
    expand_box_shorthand(property, value)
        .into_iter()
        .filter(|(name, _)| name != property || is_tracked(name))
        // The cascade also splits background-position into its x and y
        // longhands; CSSOM keeps the shorthand's own position value.
        .filter(|(name, _)| {
            name == property
                || !matches!(
                    name.as_str(),
                    "background-position-x" | "background-position-y"
                )
        })
        .map(|(name, value)| {
            if !matches!(
                name.as_str(),
                "background-position" | "background-size" | "background-image"
            ) || wide_keyword(&value).is_some()
                || pending_shorthand(&value).is_some()
                || find_var_function(&value).is_some()
            {
                return (name, value);
            }
            // CSS Backgrounds 3 #bg-position-serialization and
            // #bg-size-serialization; CSSOM #serialize-a-url.
            let value = match name.as_str() {
                "background-position" => split_top_level_commas(&value)
                    .into_iter()
                    .map(background_position_value)
                    .collect::<Vec<_>>()
                    .join(", "),
                "background-size" => split_top_level_commas(&value)
                    .into_iter()
                    .map(|size| {
                        let size = size.trim();
                        if !matches!(size, "cover" | "contain")
                            && split_top_level_ws(size).len() == 1
                        {
                            format!("{size} auto")
                        } else {
                            size.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                "background-image" => split_top_level_commas(&value)
                    .into_iter()
                    .map(|image| {
                        let image = image.trim();
                        let mut parser = cssparser::Parser::new(image);
                        if let Ok(url) = parser.expect_url()
                            && parser.is_exhausted()
                        {
                            format!("url({})", properties::string_text(&url))
                        } else {
                            image.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => value,
            };
            (name, value)
        })
        .collect()
}

fn background_position_value(position: &str) -> String {
    let mut parts = split_top_level_ws(position);
    match parts.as_slice() {
        ["top" | "bottom"] => parts.insert(0, "center"),
        [_] => parts.push("center"),
        [first, second]
            if matches!(*first, "top" | "bottom") || matches!(*second, "left" | "right") =>
        {
            parts.swap(0, 1);
        }
        [first, ..] if parts.len() >= 3 && matches!(*first, "top" | "bottom" | "center") => {
            if let Some(horizontal) = parts
                .iter()
                .position(|p| matches!(*p, "left" | "right" | "center"))
            {
                parts.rotate_left(horizontal);
            }
        }
        _ => {}
    }
    parts.join(" ")
}

pub(super) fn property_names(property: &str) -> Vec<String> {
    if property.starts_with("--") && property != "--" {
        return vec![property.into()];
    }
    if !valid_property_name(property) {
        return vec![];
    }
    if property.starts_with("--") {
        return vec![property.into()];
    }
    // WHATWG Compatibility #css-simple-aliases: a legacy name alias is a
    // supported property that names its standard property's longhands.
    let property = legacy_name_alias(&property.to_ascii_lowercase()).unwrap_or(property);
    let names: Vec<_> = longhands(property, "initial")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    if names.is_empty() || names.iter().any(|name| !is_tracked(name)) {
        vec![]
    } else {
        names
    }
}

/// Parse with the same component and shorthand grammars used by the cascade.
/// Each tuple is (longhand name, value, important flag).
fn expanded(property: &str, value: &str) -> Vec<(String, String, bool)> {
    expanded_in(property, value, false)
}

/// `expanded` for a declaration block whose Document is in quirks mode.
fn expanded_in(property: &str, value: &str, quirks: bool) -> Vec<(String, String, bool)> {
    let value = strip_css_comments(value);
    // CSSOM #parse-a-css-value parses a list of component values, so the end
    // of the value closes open blocks and strings as it does in a sheet.
    let value = close_at_end_of_input(value.trim());
    let value = value.as_ref();
    if !valid_value(value)
        || property_names(property).is_empty()
        || (!property.starts_with("--") && value.is_empty())
    {
        return vec![];
    }
    let property = properties::identifier_text(property);
    let Some((name, value, false)) = parse_decl_in(&format!("{property}:{value}"), quirks) else {
        return vec![];
    };
    let expanded = longhands(&name, &value);
    if expanded
        .iter()
        .any(|(name, value)| !accepts_longhand(name, value))
    {
        return vec![];
    }
    expanded
        .into_iter()
        .map(|(name, value)| (name, value, false))
        .collect()
}

pub(super) fn supports(property: &str, value: &str) -> bool {
    !expanded(&property.to_ascii_lowercase(), value).is_empty()
}

/// CSS Anchor Positioning 1 §2: serialize `none | <dashed-ident>#` after CSS
/// Syntax has decoded identifier escapes. Names retain their case.
pub(super) fn anchor_name_value(value: &str) -> Option<String> {
    let mut parser = cssparser::Parser::new(value);
    let names: Vec<String> = parser
        .parse_comma_separated(|parser| {
            Ok::<_, cssparser::ParseError<()>>(parser.expect_ident_cloned()?.to_string())
        })
        .ok()?;
    if !parser.is_exhausted() {
        return None;
    }
    if names.len() == 1 && names[0].eq_ignore_ascii_case("none") {
        return Some(String::from("none"));
    }
    if names
        .iter()
        .any(|name| !name.starts_with("--") || name.len() <= 2)
    {
        return None;
    }
    Some(
        names
            .iter()
            .map(|name| properties::identifier_text(name))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// Parse the complete Conditional Rules grammar before evaluating it.
/// Invalid syntax is distinct from an unknown, false feature, so negation
/// cannot turn a malformed condition into a matching rule.
pub(super) fn condition(text: &str) -> Option<bool> {
    fn evaluate(text: &str, depth: usize) -> Option<bool> {
        if depth > 64 {
            return None;
        }
        let tokens = split_top_level_ws(text);
        if tokens.len() == 2 && tokens[0].eq_ignore_ascii_case("not") {
            return Some(!term(tokens[1], depth + 1)?);
        }
        if tokens.len() == 1 {
            return term(tokens[0], depth + 1);
        }
        if tokens.len() < 3 || tokens.len().is_multiple_of(2) {
            return None;
        }
        let and = tokens[1].eq_ignore_ascii_case("and");
        if !and && !tokens[1].eq_ignore_ascii_case("or") {
            return None;
        }
        let mut value = term(tokens[0], depth + 1)?;
        for pair in tokens[1..].chunks(2) {
            if !pair[0].eq_ignore_ascii_case(tokens[1]) {
                return None;
            }
            let next = term(pair[1], depth + 1)?;
            value = if and { value && next } else { value || next };
        }
        Some(value)
    }
    fn term(text: &str, depth: usize) -> Option<bool> {
        if depth > 64 || !valid_value(text) {
            return None;
        }
        if let Some(inner) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
            if let Some(value) = evaluate(inner, depth + 1) {
                return Some(value);
            }
            if let Some((prop, value, _)) = parse_decl(inner) {
                return Some(supports(&prop, &value));
            }
            return Some(false); // <general-enclosed>
        }
        let open = text.find('(')?;
        if !valid_property_name(&text[..open]) || !text.ends_with(')') {
            return None;
        }
        if text[..open].eq_ignore_ascii_case("selector") {
            let selector = &text[open + 1..text.len() - 1];
            // Conditional Rules 4 accepts one complex selector, not a list.
            return Some(split_top_level(selector, ',').len() == 1 && selector_parses(selector));
        }
        Some(false)
    }
    let text = strip_css_comments(text);
    evaluate(text.trim(), 0)
}

pub(super) fn accepts_longhand(property: &str, value: &str) -> bool {
    if property.starts_with("--")
        || value.starts_with(PENDING_BOX_SHORTHAND)
        || find_var_function(value).is_some()
        || wide_keyword(value).is_some()
    {
        return true;
    }
    if !is_tracked(property) || value.is_empty() {
        return false;
    }
    let property = logical_to_physical(property).unwrap_or(property);
    if property == "anchor-name" {
        return anchor_name_value(value).is_some();
    }
    let lower = value.to_ascii_lowercase();
    let value = lower.as_str();
    let one_of = |values: &str| values.split_ascii_whitespace().any(|v| v == value);
    match property {
        "display" => one_of(
            "none contents block inline inline-block flow-root list-item flex inline-flex grid inline-grid table inline-table table-caption table-cell table-row table-row-group table-header-group table-footer-group table-column table-column-group -webkit-box -webkit-inline-box",
        ),
        "position" => one_of("static relative absolute fixed sticky"),
        "visibility" => one_of("visible hidden collapse"),
        "box-sizing" => one_of("content-box border-box"),
        "float" => one_of("none left right"),
        "clear" => one_of("none left right both"),
        "direction" => one_of("ltr rtl"),
        "flex-direction" => one_of("row row-reverse column column-reverse"),
        "flex-wrap" => one_of("nowrap wrap wrap-reverse"),
        "overflow" => {
            split_top_level_ws(value).len() <= 2
                && split_top_level_ws(value)
                    .iter()
                    .all(|v| matches!(*v, "visible" | "hidden" | "clip" | "scroll" | "auto"))
        }
        "overflow-x" | "overflow-y" => one_of("visible hidden clip scroll auto"),
        "overscroll-behavior-x" | "overscroll-behavior-y" => one_of("auto contain none chain"),
        "overflow-wrap" => one_of("normal break-word anywhere"),
        "word-break" => one_of("normal break-all keep-all break-word"),
        "text-wrap-mode" => one_of("wrap nowrap"),
        "text-wrap-style" => one_of("auto balance stable pretty avoid-short-last-line"),
        "white-space" => crate::layout2::WhiteSpace::components(value).is_some(),
        "white-space-collapse" => {
            one_of("collapse preserve preserve-breaks preserve-spaces break-spaces")
        }
        "writing-mode" => one_of("horizontal-tb vertical-rl vertical-lr sideways-rl sideways-lr"),
        "text-orientation" => one_of("mixed upright sideways"),
        "text-transform" => one_of("none uppercase lowercase capitalize"),
        "text-align" => one_of("start end left right center justify match-parent"),
        "text-overflow" => one_of("clip ellipsis"),
        "-webkit-line-clamp" => value == "none" || value.parse::<usize>().is_ok_and(|n| n > 0),
        "-webkit-box-orient" => one_of("horizontal vertical inline-axis block-axis"),
        "object-fit" => one_of("fill contain cover none scale-down"),
        "image-rendering" => one_of("auto smooth high-quality crisp-edges pixelated"),
        "table-layout" => one_of("auto fixed"),
        "border-collapse" => one_of("collapse separate"),
        "caption-side" => one_of("top bottom"),
        "list-style-position" => one_of("inside outside"),
        "column-fill" => one_of("auto balance"),
        "column-span" => one_of("none all"),
        "container-type" => one_of("normal size inline-size"),
        "interactivity" => one_of("auto inert"),
        "isolation" => one_of("auto isolate"),
        "filter" => crate::layout2::filter::valid(value),
        "clip-path" => value == "none" || crate::layout2::clip_path::supports(value),
        "z-index" => value == "auto" || crate::dom::css_integer(value).is_some(),
        "order" => crate::dom::css_integer(value).is_some(),
        "column-count" => value == "auto" || value.parse::<u32>().is_ok_and(|n| n > 0),
        "flex-grow" | "flex-shrink" => value.parse::<f32>().is_ok_and(|n| n.is_finite() && n >= 0.),
        "opacity" | "fill-opacity" | "stroke-opacity" | "stop-opacity" => value
            .trim_end_matches('%')
            .parse::<f32>()
            .is_ok_and(f32::is_finite),
        "font-weight" => {
            one_of("normal bold bolder lighter")
                || value
                    .parse::<f32>()
                    .is_ok_and(|n| (1. ..=1000.).contains(&n))
        }
        "font-style" => one_of("normal italic oblique"),
        "text-decoration-style" => one_of("solid double dotted dashed wavy"),
        "-webkit-text-stroke-width" => text_stroke_width_px(
            value,
            crate::layout2::Units {
                fs: 16.,
                root: 16.,
                ch: 8.,
            },
            (100., 100.),
        )
        .is_some(),
        p if p.ends_with("-style") && (p.starts_with("border-") || p == "outline-style") => {
            one_of("none hidden solid double dotted dashed groove ridge inset outset")
                || p == "outline-style" && value == "auto"
        }
        p if is_color_property(p) || p == "caret-color" => {
            supports_color_value(value) || p == "caret-color" && value == "auto"
        }
        // The grammar's permitted keywords are distinct from its length arm.
        p if property_rejects_unitless_nonzero_length(p)
            && !matches!(
                p,
                "box-shadow"
                    | "text-shadow"
                    | "background-position"
                    | "background-size"
                    | "object-position"
                    | "transform-origin"
                    | "translate"
            ) =>
        {
            let allowed_keyword = match p {
                "width" | "height" | "min-width" | "min-height" | "flex-basis" => {
                    one_of("auto min-content max-content fit-content")
                        || p == "flex-basis" && value == "content"
                }
                "max-width" | "max-height" => one_of("none min-content max-content fit-content"),
                "top" | "right" | "bottom" | "left" | "margin-top" | "margin-right"
                | "margin-bottom" | "margin-left" | "column-width" => value == "auto",
                "font-size" => one_of(
                    "xx-small x-small small medium large x-large xx-large xxx-large smaller larger",
                ),
                "line-height" | "letter-spacing" | "word-spacing" | "row-gap" | "column-gap" => {
                    value == "normal"
                }
                "vertical-align" => {
                    one_of("baseline sub super text-top text-bottom middle top bottom")
                }
                p if p.ends_with("-width")
                    && (p.starts_with("border-") || p == "outline-width") =>
                {
                    one_of("thin medium thick")
                }
                _ => false,
            };
            if allowed_keyword {
                return true;
            }
            let tokens = split_top_level_ws(value);
            let max = if p.ends_with("-radius") { 2 } else { 1 };
            !tokens.is_empty()
                && tokens.len() <= max
                && tokens.iter().all(|token| {
                    use crate::layout2::value::{Len, Vp};
                    if token.parse::<f32>().is_ok_and(|n| n != 0.) {
                        return false;
                    }
                    match Len::parse(
                        token,
                        crate::layout2::Units {
                            fs: 16.,
                            root: 16.,
                            ch: 8.,
                        },
                        Vp { w: 100., h: 100. },
                    ) {
                        Some(Len::Val(_)) => true,
                        Some(Len::FitContentLimit(_)) => matches!(
                            p,
                            "width"
                                | "height"
                                | "min-width"
                                | "min-height"
                                | "max-width"
                                | "max-height"
                                | "flex-basis"
                        ),
                        _ => false,
                    }
                })
        }
        _ => true,
    }
}

#[cfg(test)]
fn parse(text: &str) -> Vec<(String, String, bool)> {
    parse_in(text, false)
}

/// `parse` for a declaration block whose Document is in quirks mode.
fn parse_in(text: &str, quirks: bool) -> Vec<(String, String, bool)> {
    let text = strip_css_comments(text);
    let mut result: Vec<(String, String, bool)> = vec![];
    for decl in split_top_level(&text, ';') {
        let Some((property, value, important)) = parse_decl_in(decl, quirks) else {
            continue;
        };
        for (name, value, _) in expanded_in(&property, &value, quirks) {
            if let Some(index) = result.iter().position(|(key, _, _)| *key == name) {
                if !important && result[index].2 {
                    continue;
                }
                result.remove(index);
            }
            result.push((name, value, important));
        }
    }
    result
}

fn get(property: &str, declarations: &[(String, String, bool)]) -> String {
    let names = property_names(property);
    let values: Option<Vec<_>> = names
        .iter()
        .map(|name| declarations.iter().find(|(key, _, _)| key == name))
        .collect();
    let Some(values) = values.filter(|v| !v.is_empty()) else {
        return String::new();
    };
    if values.iter().any(|v| v.2 != values[0].2) {
        return String::new();
    }
    if values.len() == 1 {
        return if pending_shorthand(&values[0].1).is_some() {
            String::new()
        } else {
            values[0].1.clone()
        };
    }
    if values.iter().all(|v| v.1 == values[0].1) && wide_keyword(&values[0].1).is_some() {
        return values[0].1.clone();
    }
    if values.iter().any(|v| wide_keyword(&v.1).is_some()) {
        return String::new();
    }
    if let Some((shorthand, raw)) = pending_shorthand(&values[0].1) {
        return if shorthand == property && values.iter().all(|v| v.1 == values[0].1) {
            raw.to_string()
        } else {
            String::new()
        };
    }
    if values.iter().any(|v| pending_shorthand(&v.1).is_some()) {
        return String::new();
    }
    let v: Vec<_> = values.iter().map(|v| v.1.as_str()).collect();
    match property {
        "transition" => {
            let lists: Vec<_> = v.iter().map(|v| split_top_level_commas(v)).collect();
            if lists.iter().any(|list| list.len() != lists[0].len()) {
                return String::new();
            }
            (0..lists[0].len())
                .map(|i| {
                    format!(
                        "{} {} {} {}",
                        lists[0][i].trim(),
                        lists[1][i].trim(),
                        lists[2][i].trim(),
                        lists[3][i].trim()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
        "background" => background_value(&v),
        "mask" => mask_value(&v),
        "white-space" => match v.as_slice() {
            ["collapse", "wrap"] => "normal".into(),
            ["collapse", "nowrap"] => "nowrap".into(),
            ["preserve", "nowrap"] => "pre".into(),
            ["preserve", "wrap"] => "pre-wrap".into(),
            ["preserve-breaks", "wrap"] => "pre-line".into(),
            [collapse, "wrap"] => (*collapse).into(),
            _ => v.join(" "),
        },
        "text-wrap" => match v.as_slice() {
            ["wrap", "auto"] => "wrap".into(),
            ["nowrap", "auto"] => "nowrap".into(),
            ["wrap", style] => (*style).into(),
            _ => v.join(" "),
        },
        "margin" | "padding" | "inset" | "border-width" | "border-style" | "border-color" => {
            compress_four(&v)
        }
        "border" if v.len() == 12 => {
            if v.chunks(3).all(|side| side == &v[..3]) {
                v[..3].join(" ")
            } else {
                String::new()
            }
        }
        "font" if v.len() == 5 => format!("{} {} {} / {} {}", v[0], v[1], v[2], v[3], v[4]),
        "grid-row" | "grid-column" | "grid-area" => v.join(" / "),
        "gap" | "place-items" | "place-self" | "place-content" | "overflow"
            if v.len() == 2 && v[0] == v[1] =>
        {
            v[0].into()
        }
        "border-radius" if v.len() == 4 => {
            let sides: Vec<_> = v.iter().map(|v| split_top_level_ws(v)).collect();
            let x: Vec<_> = sides.iter().map(|v| v[0]).collect();
            let y: Vec<_> = sides.iter().map(|v| *v.get(1).unwrap_or(&v[0])).collect();
            let x = compress_four(&x);
            let y = compress_four(&y);
            if x == y { x } else { format!("{x} / {y}") }
        }
        _ => v.join(" "),
    }
}

/// CSS Backgrounds 3 #background and CSSOM #serialize-a-css-value: reconstruct
/// each layer in grammar order, omitting defaults only when it preserves all
/// longhands. Unequal list lengths cannot round-trip through the shorthand.
fn background_value(values: &[&str]) -> String {
    let [
        color,
        image,
        repeat,
        position,
        size,
        origin,
        clip,
        attachment,
    ] = values
    else {
        return String::new();
    };
    let lists: Vec<_> = [image, repeat, position, size, origin, clip, attachment]
        .into_iter()
        .map(|v| {
            split_top_level_commas(v)
                .into_iter()
                .map(str::trim)
                .collect::<Vec<_>>()
        })
        .collect();
    let count = lists[0].len();
    if count == 0 || lists.iter().any(|list| list.len() != count) {
        return String::new();
    }
    (0..count)
        .map(|i| {
            let mut parts = Vec::new();
            if lists[0][i] != "none" {
                parts.push(lists[0][i].to_string());
            }
            let sized = !matches!(lists[3][i], "auto" | "auto auto");
            if lists[2][i] != "0% 0%" || sized {
                parts.push(lists[2][i].to_string());
                if sized {
                    parts.push(format!("/ {}", lists[3][i]));
                }
            }
            if lists[1][i] != "repeat" {
                parts.push(lists[1][i].to_string());
            }
            if lists[6][i] != "scroll" {
                parts.push(lists[6][i].to_string());
            }
            if lists[4][i] != "padding-box" || lists[5][i] != "border-box" {
                parts.push(lists[4][i].to_string());
                if lists[5][i] != lists[4][i] {
                    parts.push(lists[5][i].to_string());
                }
            }
            if i + 1 == count && *color != "transparent" {
                parts.push((*color).to_string());
            }
            if parts.is_empty() {
                "none".to_string()
            } else {
                parts.join(" ")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// CSS Masking 1 #the-mask: each `<mask-layer>` in grammar order, omitting
/// initial components. One box serializes when mask-origin and mask-clip
/// agree (or mask-clip is `no-clip` over the initial origin, which `no-clip`
/// alone restores). Unequal list lengths cannot round-trip.
fn mask_value(values: &[&str]) -> String {
    let lists: Vec<Vec<&str>> = values
        .iter()
        .map(|v| {
            split_top_level_commas(v)
                .into_iter()
                .map(str::trim)
                .collect()
        })
        .collect();
    let [image, position, size, repeat, origin, clip, composite, mode] = lists.as_slice() else {
        return String::new();
    };
    let count = image.len();
    if count == 0 || lists.iter().any(|list| list.len() != count) {
        return String::new();
    }
    (0..count)
        .map(|i| {
            let mut parts = Vec::new();
            if image[i] != "none" {
                parts.push(image[i].to_string());
            }
            let sized = !matches!(size[i], "auto" | "auto auto");
            if position[i] != "0% 0%" || sized {
                parts.push(position[i].to_string());
                if sized {
                    parts.push(format!("/ {}", size[i]));
                }
            }
            if repeat[i] != "repeat" {
                parts.push(repeat[i].to_string());
            }
            match (origin[i], clip[i]) {
                ("border-box", "border-box") => {}
                ("border-box", "no-clip") => parts.push("no-clip".into()),
                (origin, clip) if origin == clip => parts.push(origin.into()),
                (origin, clip) => {
                    parts.push(origin.into());
                    parts.push(clip.into());
                }
            }
            if composite[i] != "add" {
                parts.push(composite[i].to_string());
            }
            if mode[i] != "match-source" {
                parts.push(mode[i].to_string());
            }
            if parts.is_empty() {
                "none".to_string()
            } else {
                parts.join(" ")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// CSSOM #serialize-a-css-declaration-block. Internal transport preserves
/// pending-substitution values; public serialization never exposes them.
fn serialize(declarations: &Declarations, internal: bool) -> String {
    const SHORTHANDS: &[&str] = &[
        "background",
        "mask",
        "border",
        "border-width",
        "border-style",
        "border-color",
        "border-radius",
        "margin",
        "padding",
        "inset",
        "font",
        "flex",
        "flex-flow",
        "grid-area",
        "grid-row",
        "grid-column",
        "gap",
        "place-content",
        "place-items",
        "place-self",
        "white-space",
        "overflow",
        "transition",
    ];
    let mut done = FxHashSet::default();
    let mut result = Vec::new();
    // CSSOM #serialize-into-a-shorthand-form: all has the largest reset
    // surface. A short declaration block cannot contain every longhand, so
    // skip it without repeatedly building/comparing that surface.
    let all = (declarations.len() >= all_longhand_names().len()).then_some("all");
    for (name, value, important) in declarations {
        if done.contains(name) {
            continue;
        }
        let mut serialized = None;
        if !internal {
            let pending = pending_shorthand(value).map(|(name, _)| name);
            for shorthand in pending
                .into_iter()
                .chain(all)
                .chain(SHORTHANDS.iter().copied())
            {
                let names = property_names(shorthand);
                if !names.iter().any(|n| n == name) || names.iter().any(|n| done.contains(n)) {
                    continue;
                }
                let value = get(shorthand, declarations);
                if value.is_empty() {
                    continue;
                }
                // CSSOM #serialize-a-css-declaration-block: combining
                // longhands across an opposite logical mapping changes which
                // declaration wins when cssText is parsed again.
                let indices: Vec<_> = declarations
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (name, _, _))| names.contains(name).then_some(i))
                    .collect();
                if let (Some(first), Some(last)) = (indices.first(), indices.last())
                    && declarations[*first..=*last].iter().any(|(name, _, _)| {
                        logical_mapping_group(name).is_some_and(|(group, logical)| {
                            names.iter().any(|longhand| {
                                logical_mapping_group(longhand).is_some_and(|(other, mapping)| {
                                    group == other && logical != mapping
                                })
                            })
                        })
                    })
                {
                    continue;
                }
                done.extend(names);
                serialized = Some((shorthand.to_string(), value));
                break;
            }
        }
        let (name, value) = serialized.unwrap_or_else(|| {
            (
                name.clone(),
                if !internal && pending_shorthand(value).is_some() {
                    String::new()
                } else {
                    value.clone()
                },
            )
        });
        result.push(format!(
            "{}: {value}{};",
            properties::identifier_text(&name),
            if *important { " !important" } else { "" }
        ));
        done.insert(name);
    }
    result.join(" ")
}

fn compress_four(v: &[&str]) -> String {
    let mut len = v.len();
    if len == 4 && v[3] == v[1] {
        len = 3;
    }
    if len == 3 && v[2] == v[0] {
        len = 2;
    }
    if len == 2 && v[1] == v[0] {
        len = 1;
    }
    v[..len].join(" ")
}

/// CSSOM #serialize-a-css-component-value, `<number>`: base ten in the
/// shortest form, rounded to at most six decimals, without scientific
/// notation, preceded by `-` when negative. Non-finite values keep CSS
/// Values 4's `infinity`/`NaN` spellings.
pub(crate) fn css_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "infinity" } else { "-infinity" }.into();
    }
    let mut text = format!("{value:.6}");
    if text.contains('.') {
        let trimmed = text.trim_end_matches('0').trim_end_matches('.').len();
        text.truncate(trimmed);
    }
    if text == "-0" {
        text = "0".into();
    }
    text
}

/// A CSS `<length>` in px, its number serialized as by [`css_number`]. The
/// f32 is widened through its shortest decimal form, so a specified
/// `20.7px` does not read back with single-precision noise as `20.700001px`.
pub(crate) fn css_px(px: f32) -> String {
    let widened = px.to_string().parse::<f64>().unwrap_or(f64::from(px));
    format!("{}px", css_number(widened))
}

/// A complete `<number>` token: digits with an optional sign, fraction and
/// exponent. Rust's float parser would also accept `inf` and `NaN`, which
/// are not numbers in CSS.
fn number_token(text: &str) -> Option<f64> {
    let text = text.trim();
    let numeric = !text.is_empty()
        && text.bytes().any(|b| b.is_ascii_digit())
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'));
    numeric
        .then(|| text.parse::<f64>().ok())
        .flatten()
        .filter(|v| v.is_finite())
}

/// CSSOM #resolved-values for the properties whose computed value is a
/// `<number>` (or `<integer>`): the number serialized per CSSOM, so
/// `opacity: .5` reads back as `0.5` and `z-index: 03` as `3`. CSS Color 4
/// #transparency (and the SVG, Masking and Shapes `<opacity-value>`
/// properties that share its definition): a percentage computes to the
/// equivalent number, clamped to [0, 1] like any other value. `None` leaves
/// keywords and unresolved values to the caller.
pub(crate) fn resolved_number(name: &str, value: &str) -> Option<String> {
    let alpha = matches!(
        name,
        "opacity"
            | "fill-opacity"
            | "stroke-opacity"
            | "stop-opacity"
            | "flood-opacity"
            | "shape-image-threshold"
    );
    if alpha {
        let value = value.trim();
        let number = match value.strip_suffix('%') {
            Some(percent) => number_token(percent)? / 100.0,
            None => number_token(value)?,
        };
        return Some(css_number(number.clamp(0.0, 1.0)));
    }
    matches!(
        name,
        "flex-grow"
            | "flex-shrink"
            | "order"
            | "z-index"
            | "orphans"
            | "widows"
            | "column-count"
            | "-webkit-line-clamp"
            | "tab-size"
            | "stroke-miterlimit"
            | "font-size-adjust"
            | "-webkit-box-flex"
            | "-webkit-box-ordinal-group"
    )
    .then(|| number_token(value).map(css_number))
    .flatten()
}

/// CSSOM #dom-cssstyledeclaration-getpropertyvalue on a computed style
/// declaration block (CSSOM #dom-window-getcomputedstyle), which holds only
/// longhands: a shorthand serializes from its longhands' resolved values
/// (CSSOM #serialize-a-css-value), in the shortest form that represents
/// them exactly, or as the empty string when it cannot represent them.
/// `longhand` reads a longhand's resolved value. `None` for the properties
/// this does not treat as shorthands.
pub(crate) fn resolved_shorthand(
    name: &str,
    longhand: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    resolved_shorthand_of(name, &longhand)
}

fn resolved_shorthand_of(name: &str, longhand: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let sides = |pattern: &dyn Fn(&str) -> String| -> Option<Vec<String>> {
        ["top", "right", "bottom", "left"]
            .into_iter()
            .map(|side| longhand(&pattern(side)))
            .collect()
    };
    let four = |values: Option<Vec<String>>| {
        values.map_or_else(String::new, |values| {
            compress_four(&values.iter().map(String::as_str).collect::<Vec<_>>())
        })
    };
    let all =
        |names: &[&str]| -> Option<Vec<String>> { names.iter().map(|n| longhand(n)).collect() };
    let pair = |values: Option<Vec<String>>| match values {
        Some(values) if values[0] == values[1] => values[0].clone(),
        Some(values) => values.join(" "),
        None => String::new(),
    };
    Some(match name {
        "margin" | "padding" => four(sides(&|side| format!("{name}-{side}"))),
        "inset" => four(sides(&|side| side.to_string())),
        "border-width" | "border-style" | "border-color" => {
            let kind = &name["border-".len()..];
            four(sides(&|side| format!("border-{side}-{kind}")))
        }
        // CSS Backgrounds 3 #border-radius: horizontal radii, then the
        // vertical ones after a slash when they differ.
        "border-radius" => {
            let Some(corners) = all(&[
                "border-top-left-radius",
                "border-top-right-radius",
                "border-bottom-right-radius",
                "border-bottom-left-radius",
            ]) else {
                return Some(String::new());
            };
            let pairs: Vec<Vec<&str>> = corners.iter().map(|c| split_top_level_ws(c)).collect();
            if pairs.iter().any(|pair| pair.is_empty() || pair.len() > 2) {
                return Some(String::new());
            }
            let x: Vec<&str> = pairs.iter().map(|pair| pair[0]).collect();
            let y: Vec<&str> = pairs.iter().map(|pair| *pair.last().unwrap()).collect();
            let (x, y) = (compress_four(&x), compress_four(&y));
            if x == y { x } else { format!("{x} / {y}") }
        }
        // CSS Backgrounds 3 #border-shorthands: width, style and color, each
        // of which the computed value always states.
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let side = &name["border-".len()..];
            all(&[
                &format!("border-{side}-width"),
                &format!("border-{side}-style"),
                &format!("border-{side}-color"),
            ])
            .map_or_else(String::new, |parts| parts.join(" "))
        }
        // #propdef-border: all four sides alike, and border-image at its
        // initial value, which the shorthand can only reset.
        "border" => {
            let sides: Option<Vec<String>> = ["top", "right", "bottom", "left"]
                .into_iter()
                .map(|side| {
                    resolved_shorthand_of(&format!("border-{side}"), longhand)
                        .filter(|side| !side.is_empty())
                })
                .collect();
            let image_initial = BORDER_IMAGE_LONGHANDS.iter().all(|(name, initial)| {
                longhand(name).is_none_or(|value| {
                    value == *initial
                        || (*name == "border-image-slice" && value == "100%")
                        || (*name == "border-image-width" && value == "1")
                        || (*name == "border-image-outset" && value == "0")
                        || (*name == "border-image-repeat" && value == "stretch")
                })
            });
            match sides {
                Some(sides) if image_initial && sides.iter().all(|side| *side == sides[0]) => {
                    sides[0].clone()
                }
                _ => String::new(),
            }
        }
        // CSS Logical 1 #logical-shorthands: the start value, then the end
        // value when it differs.
        "margin-block"
        | "margin-inline"
        | "padding-block"
        | "padding-inline"
        | "inset-block"
        | "inset-inline"
        | "border-block-width"
        | "border-inline-width"
        | "border-block-style"
        | "border-inline-style"
        | "border-block-color"
        | "border-inline-color" => {
            let (prefix, suffix) = match name.split_once("-block") {
                Some((prefix, suffix)) => (format!("{prefix}-block"), suffix),
                None => {
                    let (prefix, suffix) = name.split_once("-inline").unwrap();
                    (format!("{prefix}-inline"), suffix)
                }
            };
            pair(all(&[
                &format!("{prefix}-start{suffix}"),
                &format!("{prefix}-end{suffix}"),
            ]))
        }
        // CSS Logical 1 #border-shorthands: like `border`, both sides alike.
        "border-block" | "border-inline" => {
            let sides: Option<Vec<String>> = ["start", "end"]
                .into_iter()
                .map(|side| {
                    resolved_shorthand_of(&format!("{name}-{side}"), longhand)
                        .filter(|side| !side.is_empty())
                })
                .collect();
            match sides {
                Some(sides) if sides[0] == sides[1] => sides[0].clone(),
                _ => String::new(),
            }
        }
        "border-block-start" | "border-block-end" | "border-inline-start" | "border-inline-end" => {
            all(&[
                &format!("{name}-width"),
                &format!("{name}-style"),
                &format!("{name}-color"),
            ])
            .map_or_else(String::new, |parts| parts.join(" "))
        }
        // CSS UI 4 #outline: `<'outline-color'> || <'outline-style'> ||
        // <'outline-width'>`, each stated like the border shorthands'.
        "outline" => all(&["outline-color", "outline-style", "outline-width"])
            .map_or_else(String::new, |parts| parts.join(" ")),
        // CSS Align 3 #gap-shorthand and #place-content / #place-self: the
        // first value, then the second when it differs.
        "gap" => pair(all(&["row-gap", "column-gap"])),
        "place-content" => pair(all(&["align-content", "justify-content"])),
        "place-self" => pair(all(&["align-self", "justify-self"])),
        // CSS Overscroll 1 #overscroll-behavior-properties.
        "overscroll-behavior" => pair(all(&["overscroll-behavior-x", "overscroll-behavior-y"])),
        // CSS Flexbox 1 #flex-property: the computed value states all three
        // components (`flex: none` reads back as `0 0 auto`).
        "flex" => all(&["flex-grow", "flex-shrink", "flex-basis"])
            .map_or_else(String::new, |parts| parts.join(" ")),
        "flex-flow" => {
            all(&["flex-direction", "flex-wrap"]).map_or_else(String::new, |parts| parts.join(" "))
        }
        "font" => resolved_font(longhand).unwrap_or_default(),
        "background" => resolved_background(longhand).unwrap_or_default(),
        _ => return None,
    })
}

/// CSS Fonts 4 #font-prop: `[style] [small-caps] [weight] [stretch] size
/// [/ line-height] family`, omitting the components at their initial value.
/// Values the shorthand cannot express make it unrepresentable.
fn resolved_font(longhand: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let style = longhand("font-style")?;
    let weight = longhand("font-weight")?;
    let size = longhand("font-size")?;
    let line_height = longhand("line-height")?;
    let family = longhand("font-family")?;
    let variant = longhand("font-variant").unwrap_or_else(|| "normal".into());
    let stretch = longhand("font-stretch").unwrap_or_else(|| "100%".into());
    let stretch = match stretch.as_str() {
        "normal" | "100%" => None,
        "50%" | "ultra-condensed" => Some("ultra-condensed"),
        "62.5%" | "extra-condensed" => Some("extra-condensed"),
        "75%" | "condensed" => Some("condensed"),
        "87.5%" | "semi-condensed" => Some("semi-condensed"),
        "112.5%" | "semi-expanded" => Some("semi-expanded"),
        "125%" | "expanded" => Some("expanded"),
        "150%" | "extra-expanded" => Some("extra-expanded"),
        "200%" | "ultra-expanded" => Some("ultra-expanded"),
        _ => return None,
    };
    let mut parts = Vec::new();
    if style != "normal" {
        parts.push(style);
    }
    match variant.as_str() {
        "normal" => {}
        "small-caps" => parts.push(variant),
        _ => return None,
    }
    if !matches!(weight.as_str(), "normal" | "400") {
        parts.push(weight);
    }
    if let Some(stretch) = stretch {
        parts.push(stretch.into());
    }
    parts.push(size);
    if line_height != "normal" {
        parts.push("/".into());
        parts.push(line_height);
    }
    parts.push(family);
    Some(parts.join(" "))
}

/// CSS Backgrounds 3 #background, from resolved longhands: the shortest
/// serialization of each layer (`background_value`), whose final layer omits
/// a fully transparent color.
fn resolved_background(longhand: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut values = [
        "background-color",
        "background-image",
        "background-repeat",
        "background-position",
        "background-size",
        "background-origin",
        "background-clip",
        "background-attachment",
    ]
    .into_iter()
    .map(longhand)
    .collect::<Option<Vec<_>>>()?;
    if crate::render::PaintColor::parse_css(&values[0]).is_some_and(|c| c.is_transparent()) {
        values[0] = "transparent".into();
    }
    let value = background_value(&values.iter().map(String::as_str).collect::<Vec<_>>());
    (!value.is_empty()).then_some(value)
}

pub(crate) fn operation(op: &str, text: &str, extra: &str) -> Value {
    match op {
        "string" => json!(properties::string_text(text)),
        "identifier" => json!(properties::identifier_text(text)),
        // CSS Values 4 #deprecated-quirky-length applies to the declaration
        // blocks of a quirks-mode Document's elements and sheets.
        "parse" => json!(parse_in(text, extra == "quirks")),
        "serialize" | "source" => json!(serialize(
            &serde_json::from_str::<Declarations>(text).unwrap_or_default(),
            op == "source"
        )),
        "counter-descriptor" => json!(counter_styles::descriptor_valid(text, extra)),
        "counter-style-valid" => json!(counter_styles::CounterStyle::parse(text).valid()),
        "counter-name" => json!(counter_styles::normalize_name(text)),
        "descriptors" => json!(
            split_top_level(text, ';')
                .into_iter()
                .filter_map(parse_decl)
                .collect::<Vec<_>>()
        ),
        "expand" => json!(expanded(text, extra)),
        "expand-quirks" => json!(expanded_in(text, extra, true)),
        "names" => json!(property_names(text)),
        "get" => json!(get(
            text,
            &serde_json::from_str::<Vec<(String, String, bool)>>(extra).unwrap_or_default()
        )),
        "supports" => json!(supports(text, extra)),
        "selector" => json!(selector_parses(&if extra == "nested" {
            expand_nesting(text, "*")
        } else {
            replace_nesting_tokens(text, ":scope").0
        })),
        "condition" => json!(supports_condition(text) || supports_condition(&format!("({text})"))),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_numbers_serialize_in_the_shortest_form() {
        // CSSOM #serialize-a-css-component-value: base ten, at most six
        // decimals, no exponent, no negative zero.
        for (value, text) in [
            (0.5, "0.5"),
            (100.0, "100"),
            (1.0 / 3.0, "0.333333"),
            (-0.0, "0"),
            (-2.25, "-2.25"),
            (1e21, "1000000000000000000000"),
            (0.0000004, "0"),
            (f64::INFINITY, "infinity"),
        ] {
            assert_eq!(css_number(value), text, "{value}");
        }
        for (name, value, text) in [
            ("opacity", ".5", Some("0.5")),
            ("opacity", "50%", Some("0.5")),
            ("opacity", "1.5", Some("1")),
            ("opacity", "-1", Some("0")),
            ("fill-opacity", "+.25", Some("0.25")),
            ("flex-grow", "1e2", Some("100")),
            ("z-index", "03", Some("3")),
            ("order", "-0", Some("0")),
            ("z-index", "auto", None),
            ("opacity", "inf", None),
            ("opacity", "calc(1 / 2)", None),
            ("width", "5", None),
        ] {
            assert_eq!(
                resolved_number(name, value).as_deref(),
                text,
                "{name}: {value}"
            );
        }
    }

    #[test]
    fn computed_style_serializes_numbers_radii_and_shorthands() {
        // CSSOM #resolved-values and #serialize-a-css-value: computed numbers
        // in their shortest form, corner radii without a repeated vertical
        // radius, and shorthands (absent from a computed declaration block)
        // serialized from their longhands, or "" when they cannot represent
        // them. Values verified against headless Chromium 140 except where
        // noted.
        let dom = Dom::parse_document(
            "<!doctype html><style>\
             #a { border-radius: 20px; border-top-left-radius: 7px; opacity: .5; \
                  flex: .5 1e2 0; z-index: 03; order: -0 }\
             #b { border-radius: 10px 20% / 5px; font: italic bold 12px/1.5 serif; \
                  margin: 1px 2px; padding: 3px; position: relative; inset: 1px 2px 3px 4px; \
                  border: 2px solid red; gap: 3px 4px; opacity: 150%; flex-flow: column wrap }\
             #c { border-width: 1px 2px; border-style: solid dashed; border-color: red blue; \
                  opacity: 50%; background: url(x.png) red no-repeat; font-size: 10px; \
                  line-height: 1.15; border-bottom-left-radius: calc(-1.5em + 10px) 2.5em }\
             #d { opacity: 0.333333333; border-image: url(y.png) 30 }\
             </style><div id=a></div><div id=b></div><div id=c></div><div id=d></div>\
             <div id=e></div>",
        );
        let value = |id: &str, property: &str| {
            dom.cssom_resolved_value(dom.get_by_id(id).unwrap(), property)
                .unwrap_or_default()
        };
        for (id, property, expected) in [
            ("a", "border-top-left-radius", "7px"),
            ("a", "border-top-right-radius", "20px"),
            ("a", "border-radius", "7px 20px 20px"),
            ("a", "opacity", "0.5"),
            ("a", "flex-grow", "0.5"),
            ("a", "flex", "0.5 100 0px"),
            ("a", "z-index", "3"),
            ("a", "order", "0"),
            ("b", "border-top-left-radius", "10px 5px"),
            ("b", "border-top-right-radius", "20% 5px"),
            ("b", "border-radius", "10px 20% / 5px"),
            ("b", "opacity", "1"),
            ("b", "font", "italic 700 12px / 18px serif"),
            ("b", "margin", "1px 2px"),
            ("b", "padding", "3px"),
            ("b", "padding-inline", "3px"),
            ("b", "margin-block", "1px"),
            ("b", "inset", "1px 2px 3px 4px"),
            ("b", "border", "2px solid rgb(255, 0, 0)"),
            ("b", "border-top", "2px solid rgb(255, 0, 0)"),
            ("b", "border-width", "2px"),
            ("b", "gap", "3px 4px"),
            ("b", "flex-flow", "column wrap"),
            ("c", "border-width", "1px 2px"),
            ("c", "border-style", "solid dashed"),
            ("c", "border-color", "rgb(255, 0, 0) rgb(0, 0, 255)"),
            ("c", "border-block", "1px solid rgb(255, 0, 0)"),
            ("c", "border-inline", "2px dashed rgb(0, 0, 255)"),
            ("c", "border", ""),
            ("c", "opacity", "0.5"),
            ("c", "line-height", "11.5px"),
            ("c", "font", "10px / 11.5px sans-serif"),
            ("c", "border-bottom-left-radius", "0px 25px"),
            // CSSOM #serialize-a-css-value prefers the shortest form; Blink
            // instead lists every background longhand.
            ("c", "background", "url(x.png) no-repeat rgb(255, 0, 0)"),
            ("d", "opacity", "0.333333"),
            // The border shorthand also resets border-image, so it cannot
            // represent a non-initial one (Blink ignores border-image here).
            ("d", "border", ""),
            ("e", "border", "0px none rgb(0, 0, 0)"),
            ("e", "border-radius", "0px"),
            ("e", "margin", "0px"),
            ("e", "inset", "auto"),
            ("e", "gap", "normal"),
            ("e", "flex", "0 1 auto"),
            ("e", "background", "none"),
            ("e", "outline", "rgb(0, 0, 0) none 3px"),
            ("e", "overflow", "visible"),
            ("e", "font", "16px sans-serif"),
        ] {
            assert_eq!(value(id, property), expected, "#{id} {property}");
        }
    }

    #[test]
    fn all_shorthand_cssom_accepts_resets_and_roundtrips_exceptions() {
        for keyword in ["initial", "inherit", "unset", "revert", "revert-layer"] {
            assert!(supports("all", keyword));
            let source = format!("direction:rtl;--tone:blue;all:{keyword}!important");
            let declarations = parse(&source);
            assert_eq!(get("all", &declarations), keyword);
            let serialized = serialize(&declarations, false);
            assert!(
                serialized.contains(&format!("all: {keyword} !important;")),
                "{serialized}"
            );
            assert_eq!(get("direction", &parse(&serialized)), "rtl");
            assert_eq!(get("--tone", &parse(&serialized)), "blue");
        }
        for invalid in ["red", "unset inherit", "initial, unset", "12px"] {
            assert!(!supports("all", invalid), "{invalid}");
        }
        assert!(supports("all", "var(--missing, unset)"));
        assert_eq!(get("all", &parse("all:unset;color:red")), "");
    }

    #[test]
    fn logical_declaration_parse_and_serialization_preserve_cascade_order() {
        let source = "direction:rtl;border-left-width:1px;border-inline-end-width:5px;\
            border-top-width:2px;border-right-width:3px;border-bottom-width:4px";
        let serialized = serialize(&parse(source), false);
        assert!(!serialized.contains("border-width:"), "{serialized}");
        let mut dom = Dom::parse_document("<div id=box></div>");
        let node = dom.get_by_id("box").unwrap();
        dom.set_cssom_inline(node, parse(&serialized));
        assert_eq!(
            dom.computed_value_resolved(node, "border-left-width")
                .as_deref(),
            Some("5px")
        );
        dom.set_cssom_inline(
            node,
            parse("direction:rtl;margin-inline-start:1px;margin-right:2px;margin-inline-start:3px"),
        );
        assert_eq!(
            dom.computed_value_resolved(node, "margin-right").as_deref(),
            Some("3px")
        );
    }

    #[test]
    fn unchanged_cssom_inline_writes_invalidate_nothing() {
        let mut dom =
            Dom::parse_document("<style>p{width:var(--w)}</style><div id=root><p id=p>x</p></div>");
        let root = dom.get_by_id("root").unwrap();
        let p = dom.get_by_id("p").unwrap();
        dom.set_cssom_inline(root, parse("--w:40px;color:red"));
        assert_eq!(
            dom.computed_value_resolved(p, "width").as_deref(),
            Some("40px")
        );
        let (epoch, styles) = (dom.epoch(), dom.style_value_epoch);
        dom.take_dirty();
        dom.set_cssom_inline(root, parse("--w:40px;color:red"));
        assert_eq!((dom.epoch(), dom.style_value_epoch), (epoch, styles));
        assert!(!dom.take_dirty());
        dom.set_cssom_inline(root, parse("--w:50px;color:red"));
        assert_ne!(dom.epoch(), epoch);
        assert_eq!(
            dom.computed_value_resolved(p, "width").as_deref(),
            Some("50px")
        );
        // A direct attribute write replaces the CSSOM declarations even when
        // its text matches, so the next CSSOM write applies again.
        let text = dom.get_attribute(root, "style").unwrap().to_string();
        dom.set_attr(root, "style", &text);
        let epoch = dom.epoch();
        dom.set_cssom_inline(root, parse("--w:50px;color:red"));
        assert_ne!(dom.epoch(), epoch);
        assert_eq!(
            dom.computed_value_resolved(p, "width").as_deref(),
            Some("50px")
        );
    }

    #[test]
    fn anchor_name_dashed_ident_list_is_supported_and_cascades() {
        // CSS Anchor Positioning 1 §2: anchor-name is non-inherited and uses
        // a case-sensitive comma-separated list of dashed identifiers.
        for value in ["none", "NoNe", "--a", "--Card, --menu"] {
            assert!(supports("anchor-name", value), "{value}");
        }
        assert_eq!(anchor_name_value(r"n\6f ne").as_deref(), Some("none"));
        assert_eq!(anchor_name_value(r"--\43 ard").as_deref(), Some("--Card"));
        for value in ["--", "card", "--a --b", "--a,", "none, --a", "--a 1"] {
            assert!(!supports("anchor-name", value), "{value}");
        }
        assert!(!supports("position-area", "top center"));
        assert!(!supports("top", "anchor(bottom)"));

        let dom = crate::dom::Dom::parse_document(
            "<style>.anchor{anchor-name:--Card, --menu}</style>\
             <div id=anchor class=anchor><span id=child></span></div>\
             <div id=invalid style='anchor-name:card'></div>",
        );
        let anchor = dom.get_by_id("anchor").unwrap();
        let child = dom.get_by_id("child").unwrap();
        let invalid = dom.get_by_id("invalid").unwrap();
        assert_eq!(
            dom.computed_value_resolved(anchor, "anchor-name")
                .as_deref(),
            Some("--Card, --menu")
        );
        assert_eq!(
            dom.cssom_resolved_value(child, "anchor-name").as_deref(),
            Some("none")
        );
        assert_eq!(
            dom.cssom_resolved_value(invalid, "anchor-name").as_deref(),
            Some("none")
        );
        assert_eq!(expanded("anchor-name", "NoNe")[0].1, "none");
    }

    #[test]
    fn set_property_closes_values_at_end_of_input() {
        // CSSOM #parse-a-css-value / CSS Syntax 3 #consume-function: EOF
        // closes open functions and strings, so these values are valid.
        assert_eq!(
            expanded("width", "calc(1px * pow(2, sqrt(100))"),
            [(
                "width".into(),
                "calc(1px * pow(2, sqrt(100)))".into(),
                false
            )]
        );
        assert_eq!(expanded("color", "rgb(1, 2, 3")[0].1, "rgb(1, 2, 3)");
        assert_eq!(expanded("font-family", "\"Open")[0].1, "\"Open\"");
        // A newline still makes a bad string, and a stray closer or a
        // top-level `;` still invalidates the value.
        assert!(expanded("font-family", "\"Open\nSans\"").is_empty());
        assert!(expanded("width", "calc(1px])").is_empty());
        assert!(expanded("width", "1px; color: red").is_empty());
    }

    #[test]
    fn transition_shorthand_preserves_lists_and_rejects_unrepresentable_values() {
        let declarations = parse(
            "transition:height 0.3s ease-in-out, margin-top 1s cubic-bezier(0, 0, 1, 1) -100ms",
        );
        let serialized = get("transition", &declarations);
        assert_eq!(expanded("transition", &serialized), declarations);
        assert!(!serialized.contains(",  "));
        let unequal = parse("transition:height 1s;transition-property:height,width");
        assert_eq!(get("transition", &unequal), "");
        let important = parse("transition:height 1s;transition-delay:2s!important");
        assert_eq!(get("transition", &important), "");
    }

    #[test]
    fn background_shorthand_cssom_preserves_layers_resets_and_pending_values() {
        for value in [
            "#222",
            "none",
            "url(icon.png) left top / 20px 30px no-repeat fixed content-box border-box #fff",
            "linear-gradient(red, blue) left / cover no-repeat, url(tile.png) repeat-x red",
        ] {
            assert!(supports("background", value), "{value}");
            let declarations = expanded("background", value);
            assert_eq!(declarations.len(), 8);
            let serialized = get("background", &declarations);
            assert!(!serialized.is_empty(), "{value}");
            assert_eq!(
                expanded("background", &serialized),
                declarations,
                "{serialized}"
            );
        }
        let declarations = parse("background-image:url(old.png);background:#222!important");
        assert_eq!(get("background-color", &declarations), "#222");
        assert_eq!(get("background-image", &declarations), "none");
        assert!(declarations.iter().all(|d| d.2));
        assert_eq!(
            get(
                "background",
                &parse("background:url(/icon.png) top left / 10rem no-repeat")
            ),
            "url(\"/icon.png\") left top / 10rem auto no-repeat"
        );
        // Upstream background-shorthand-serialization.html: comma-separated
        // layers must omit defaults and normalize spacing independently.
        assert_eq!(
            get(
                "background",
                &parse("background:url(/icon.png) no-repeat, url(/icon.png) no-repeat")
            ),
            "url(\"/icon.png\") no-repeat, url(\"/icon.png\") no-repeat"
        );
        assert_eq!(
            get(
                "background",
                &parse(
                    "background:url(/icon.png) top left no-repeat, url(/icon.png) center / 100% 100% no-repeat, url(/icon.png) white"
                )
            ),
            "url(\"/icon.png\") left top no-repeat, url(\"/icon.png\") center center / 100% 100% no-repeat, url(\"/icon.png\") white"
        );
        let mut pending = parse("background:var(--Background,#666)");
        assert_eq!(get("background", &pending), "var(--Background,#666)");
        assert_eq!(get("background-color", &pending), "");
        assert_eq!(
            serialize(&pending, false),
            "background: var(--Background,#666);"
        );
        pending
            .iter_mut()
            .find(|d| d.0 == "background-image")
            .unwrap()
            .1 = "none".into();
        assert_eq!(get("background", &pending), "");
        assert!(!serialize(&pending, false).contains('\0'));
        assert!(!supports("background", "not-a-color"));
        assert!(property_names("not-a-property").is_empty());
    }

    #[test]
    fn mask_shorthand_cssom_round_trips_layers() {
        // CSS Masking 1 #the-mask and WHATWG Compatibility
        // #css-simple-aliases (`-webkit-mask` is a legacy name alias).
        for value in [
            "none",
            "url(dots.png) center / cover no-repeat",
            "linear-gradient(#fff 0 0) content-box, linear-gradient(#fff 0 0) exclude",
            "url(m.svg) left 4px top / 10px 20px repeat-x padding-box no-clip luminance",
        ] {
            assert!(supports("mask", value), "{value}");
            assert!(supports("-webkit-mask", value), "{value}");
            let declarations = expanded("mask", value);
            assert_eq!(declarations.len(), 8);
            let serialized = get("mask", &declarations);
            assert!(!serialized.is_empty(), "{value}");
            assert_eq!(expanded("mask", &serialized), declarations, "{serialized}");
        }
        assert_eq!(
            get(
                "mask",
                &parse("-webkit-mask:url(a.png) no-repeat center / contain")
            ),
            "url(a.png) center / contain no-repeat"
        );
        for (property, value) in [
            ("mask", "url(a.png) url(b.png)"),
            ("mask", "no-clip no-clip"),
            ("mask", "border-box padding-box no-clip"),
            ("mask", "xor"),
            ("mask-composite", "source-over"),
            ("-webkit-mask-composite", "xor"),
            ("mask-mode", "auto"),
            ("mask-origin", "no-clip"),
            ("mask-image", "red"),
            ("mask-size", "cover 10px"),
            ("mask-position", "5 5"),
        ] {
            assert!(!supports(property, value), "{property}: {value}");
        }
        for (property, value) in [
            ("-webkit-mask-image", "url(a.png), none"),
            ("mask-clip", "no-clip, content-box"),
            ("mask-repeat", "space round"),
            ("-webkit-mask-size", "50% auto, contain"),
            ("mask-position", "right 10% bottom"),
        ] {
            assert!(supports(property, value), "{property}: {value}");
        }
    }
}
