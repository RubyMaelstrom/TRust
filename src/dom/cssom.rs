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
        let fixed_block = ["transform", "perspective", "backdrop-filter"]
            .into_iter()
            .any(non_none)
            || (!root && non_none("filter"))
            || value("contain").split_ascii_whitespace().any(|token| {
                ["layout", "paint", "strict", "content"]
                    .iter()
                    .any(|keyword| token.eq_ignore_ascii_case(keyword))
            })
            || value("will-change").split(',').any(|token| {
                ["transform", "perspective", "backdrop-filter", "contain"]
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
        if id >= self.nodes.len() || self.tag_name(id).is_none() {
            return;
        }
        let text = serialize(&declarations, false);
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
                        let mut input = cssparser::ParserInput::new(image);
                        let mut parser = cssparser::Parser::new(&mut input);
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
    let value = strip_css_comments(value);
    let value = value.trim();
    if !valid_value(value)
        || property_names(property).is_empty()
        || (!property.starts_with("--") && value.is_empty())
    {
        return vec![];
    }
    let property = properties::identifier_text(property);
    let Some((name, value, false)) = parse_decl(&format!("{property}:{value}")) else {
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
        "filter" => crate::layout2::filter::color_filters(value).is_some(),
        "clip-path" => value == "none" || crate::layout2::clip_path::supports(value),
        "z-index" => value == "auto" || value.parse::<i32>().is_ok(),
        "order" => value.parse::<i32>().is_ok(),
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

fn parse(text: &str) -> Vec<(String, String, bool)> {
    let text = strip_css_comments(text);
    let mut result: Vec<(String, String, bool)> = vec![];
    for decl in split_top_level(&text, ';') {
        let Some((property, value, important)) = parse_decl(decl) else {
            continue;
        };
        for (name, value, _) in expanded(&property, &value) {
            if let Some(existing) = result.iter_mut().find(|(key, _, _)| *key == name) {
                if important || !existing.2 {
                    *existing = (name, value, important);
                }
            } else {
                result.push((name, value, important));
            }
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
        "white-space" => match v.as_slice() {
            ["collapse", "wrap"] => "normal".into(),
            ["collapse", "nowrap"] => "nowrap".into(),
            ["preserve", "nowrap"] => "pre".into(),
            ["preserve", "wrap"] => "pre-wrap".into(),
            ["preserve-breaks", "wrap"] => "pre-line".into(),
            [collapse, "wrap"] => (*collapse).into(),
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

/// CSSOM #serialize-a-css-declaration-block. Internal transport preserves
/// pending-substitution values; public serialization never exposes them.
fn serialize(declarations: &Declarations, internal: bool) -> String {
    const SHORTHANDS: &[&str] = &[
        "background",
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
    for (name, value, important) in declarations {
        if done.contains(name) {
            continue;
        }
        let mut serialized = None;
        if !internal {
            let pending = pending_shorthand(value).map(|(name, _)| name);
            for shorthand in pending.into_iter().chain(SHORTHANDS.iter().copied()) {
                let names = property_names(shorthand);
                if !names.iter().any(|n| n == name) || names.iter().any(|n| done.contains(n)) {
                    continue;
                }
                let value = get(shorthand, declarations);
                if value.is_empty() {
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

pub(crate) fn operation(op: &str, text: &str, extra: &str) -> Value {
    match op {
        "string" => json!(properties::string_text(text)),
        "identifier" => json!(properties::identifier_text(text)),
        "parse" => json!(parse(text)),
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
}
