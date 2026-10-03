//! HTML rendering's legacy presentational hints.
//!
//! WHATWG HTML snapshot e5071a20c8569 (2026-09-06), #the-page,
//! #phrasing-content-3, #tables-2, #the-hr-element-2 and #the-marquee-element-2;
//! color parsing: #rules-for-parsing-a-legacy-colour-value. These are CSS
//! declarations in the presentational-hint origin, not inline author styles.

use super::{Dom, NodeId, StyleView};

/// Where a presentational hint's URL resolves (HTML #the-page: against the
/// node document's base URL).
pub(super) enum DocumentBase {
    Url(url::Url),
    /// No document URL: the reference stays relative, like author CSS.
    Unknown,
}

impl Dom {
    pub(crate) fn marquee_scrolls_vertically(&self, id: NodeId) -> bool {
        self.style_view().marquee_scrolls_vertically(id)
    }

    pub(super) fn html_presentational_hints(
        &self,
        id: NodeId,
        hint: impl FnMut(&'static str, String),
    ) {
        self.style_view()
            .html_presentational_hints(id, &|node| self.document_base(node), hint);
    }

    /// An attribute that maps to a dimension property ignoring zero, parsed
    /// by HTML's rules for parsing non-zero dimension values: `"450px;"`
    /// is 450 pixels and `"80%"` a percentage (as `450px`/`80%`).
    pub(crate) fn nonzero_dimension_attr(&self, id: NodeId, name: &str) -> Option<String> {
        self.attr(id, name).and_then(nonzero_dimension_value)
    }
}

impl StyleView<'_> {
    /// The table a td/th belongs to through its row (and row group): the
    /// `table > tr > td` and `table > tbody > tr > td` shapes the HTML
    /// rendering rules select.
    fn cell_table(&self, cell: NodeId) -> Option<NodeId> {
        let row = self
            .nodes
            .parent(cell)
            .filter(|&row| self.tag_name(row) == Some("tr"))?;
        let parent = self.nodes.parent(row)?;
        let table = match self.tag_name(parent) {
            Some("table") => parent,
            Some("thead" | "tbody" | "tfoot") => self.nodes.parent(parent)?,
            _ => return None,
        };
        (self.tag_name(table) == Some("table")
            && self.namespace_uri(table) == Some("http://www.w3.org/1999/xhtml"))
        .then_some(table)
    }

    /// Whether a marquee's `direction` attribute is in the up or down state
    /// (HTML #attr-marquee-direction; the default is left).
    fn marquee_scrolls_vertically(&self, id: NodeId) -> bool {
        self.attr(id, "direction").is_some_and(|direction| {
            direction.eq_ignore_ascii_case("up") || direction.eq_ignore_ascii_case("down")
        })
    }

    /// The presentational hints of `id`. `base` resolves the node
    /// document's base URL for the `background` attribute.
    pub(super) fn html_presentational_hints(
        &self,
        id: NodeId,
        base: &dyn Fn(NodeId) -> DocumentBase,
        mut hint: impl FnMut(&'static str, String),
    ) {
        if self.namespace_uri(id) != Some("http://www.w3.org/1999/xhtml") {
            return;
        }
        let tag = self.tag_name(id).unwrap_or("");
        // HTML #dimRendering maps embed, iframe and object dimensions to CSS
        // hints, including percentages. Keep these in the cascade so both the
        // live box tree and presentation snapshots use them, and author
        // `auto` can win.
        if matches!(tag, "embed" | "iframe" | "object") {
            for property in ["width", "height"] {
                if let Some(value) = self.attr(id, property).and_then(dimension_value) {
                    hint(property, value);
                }
            }
        }
        // HTML Rendering #the-textarea-element-2: `wrap=off` (ASCII
        // case-insensitive) is a presentational hint for `white-space: pre`,
        // which sets both of its longhands (CSS Text 4 #white-space-property).
        if tag == "textarea"
            && self
                .attr(id, "wrap")
                .is_some_and(|wrap| wrap.eq_ignore_ascii_case("off"))
        {
            hint("white-space", "pre".into());
            hint("white-space-collapse", "preserve".into());
            hint("text-wrap-mode", "nowrap".into());
        }
        // HTML Rendering #the-page: the first of the body's own margin
        // attributes, else its container frame's, maps to a pixel length on
        // both sides of its axis; an unparsable value uses the 8px default.
        if tag == "body" {
            let frame = self
                .frame_owner(id)
                .filter(|&frame| matches!(self.tag_name(frame), Some("iframe" | "frame")));
            let margin = |own: [&str; 2], container: &str| {
                own.iter()
                    .find_map(|name| self.attr(id, name))
                    .or_else(|| frame.and_then(|frame| self.attr(frame, container)))
                    .map(|value| format!("{}px", non_negative_integer(value).unwrap_or(8)))
            };
            if let Some(value) = margin(["marginheight", "topmargin"], "marginheight") {
                hint("margin-top", value.clone());
                hint("margin-bottom", value);
            }
            if let Some(value) = margin(["marginwidth", "leftmargin"], "marginwidth") {
                hint("margin-left", value.clone());
                hint("margin-right", value);
            }
        }
        // HTML Rendering #tables-2: `cellspacing` and `border` map to pixel
        // lengths; a border that is not equivalent to zero is outset. The
        // cells of such a table get 1px inset borders, and every cell takes
        // the table's `cellpadding`. Like Gecko and Blink, cells also take a
        // `bordercolor` (Gecko's `table[bordercolor] td { border-color:
        // inherit }`).
        if tag == "table" {
            if let Some(spacing) = self.attr(id, "cellspacing").and_then(non_negative_integer) {
                hint("border-spacing", format!("{spacing}px"));
            }
            if let Some(border) = self.attr(id, "border") {
                let width = non_negative_integer(border).unwrap_or(1);
                for side in ["top", "right", "bottom", "left"] {
                    hint(border_property(side, "width"), format!("{width}px"));
                    if width != 0 {
                        hint(border_property(side, "style"), "outset".into());
                    }
                }
            }
        }
        if matches!(tag, "td" | "th")
            && let Some(table) = self.cell_table(id)
        {
            if self
                .attr(table, "border")
                .is_some_and(|border| non_negative_integer(border) != Some(0))
            {
                for side in ["top", "right", "bottom", "left"] {
                    hint(border_property(side, "width"), "1px".into());
                    hint(border_property(side, "style"), "inset".into());
                }
            }
            if let Some(color) = self.attr(table, "bordercolor").and_then(legacy_color) {
                for side in ["top", "right", "bottom", "left"] {
                    hint(border_property(side, "color"), color.clone());
                }
            }
            if let Some(padding) = self
                .attr(table, "cellpadding")
                .and_then(non_negative_integer)
            {
                for property in [
                    "padding-top",
                    "padding-right",
                    "padding-bottom",
                    "padding-left",
                ] {
                    hint(property, format!("{padding}px"));
                }
            }
        }
        // HTML Rendering #tables-2: `td[nowrap], th[nowrap] { white-space:
        // nowrap }`, except that in quirks mode a cell whose width attribute
        // is a length (not a percentage) is set back to normal.
        if matches!(tag, "td" | "th") && self.attr(id, "nowrap").is_some() {
            let sized = self
                .attr(id, "width")
                .and_then(nonzero_dimension_value)
                .is_some_and(|width| width.ends_with("px"));
            let normal = sized && self.in_quirks_mode(id);
            hint(
                "white-space",
                if normal { "normal" } else { "nowrap" }.into(),
            );
        }
        // HTML Rendering #tables-2: table, row-group and row heights map to
        // the dimension property 'height'; cell heights ignore zero.
        let height = match tag {
            "table" | "thead" | "tbody" | "tfoot" | "tr" => {
                self.attr(id, "height").and_then(dimension_value)
            }
            "td" | "th" => self.attr(id, "height").and_then(nonzero_dimension_value),
            _ => None,
        };
        if let Some(height) = height {
            hint("height", height);
        }
        // HTML Rendering #attributes-for-embedded-content-and-images: `align`
        // on embed, iframe, img, object and an Image Button input floats the
        // element or aligns it vertically; `middle`/`center` put its middle
        // on the parent's baseline (Blink's -webkit-baseline-middle).
        let embedded = matches!(tag, "embed" | "iframe" | "img" | "object")
            || (tag == "input" && self.input_type(id) == "image");
        if embedded && let Some(align) = self.attr(id, "align") {
            let (property, value) = match align.trim().to_ascii_lowercase().as_str() {
                "left" => ("float", "left"),
                "right" => ("float", "right"),
                "top" => ("vertical-align", "top"),
                "baseline" => ("vertical-align", "baseline"),
                "texttop" => ("vertical-align", "text-top"),
                "absmiddle" | "abscenter" => ("vertical-align", "middle"),
                "bottom" => ("vertical-align", "bottom"),
                "middle" | "center" => ("vertical-align", "-webkit-baseline-middle"),
                _ => ("", ""),
            };
            if !property.is_empty() {
                hint(property, value.into());
            }
        }
        // HTML Rendering #the-marquee-element-2: width/height map to the
        // dimension properties, hspace/vspace to the margins, and an up/down
        // marquee's natural height is 200px. Horizontal marquees scroll a
        // single line of content, so it does not wrap (as in Gecko and Blink).
        if tag == "marquee" {
            let vertical = self.marquee_scrolls_vertically(id);
            if let Some(width) = self.attr(id, "width").and_then(dimension_value) {
                hint("width", width);
            }
            match self.attr(id, "height").and_then(dimension_value) {
                Some(height) => hint("height", height),
                None if vertical => hint("height", "200px".into()),
                None => {}
            }
            if let Some(space) = self.attr(id, "hspace").and_then(dimension_value) {
                hint("margin-left", space.clone());
                hint("margin-right", space);
            }
            if let Some(space) = self.attr(id, "vspace").and_then(dimension_value) {
                hint("margin-top", space.clone());
                hint("margin-bottom", space);
            }
            if !vertical {
                hint("text-wrap-mode", "nowrap".into());
            }
        }
        if matches!(
            tag,
            "body" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th" | "marquee"
        ) && let Some(color) = self.attr(id, "bgcolor").and_then(legacy_color)
        {
            hint("background-color", color);
        }
        // HTML Rendering #the-page and #tables-2: a non-empty `background`
        // attribute is a background-image hint, its URL parsed relative to
        // the node document; a parse failure contributes no hint. Without
        // any document URL the reference stays relative, like author CSS.
        if matches!(
            tag,
            "body" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th"
        ) && let Some(source) = self.attr(id, "background").filter(|s| !s.is_empty())
            && let Some(resolved) = match base(id) {
                DocumentBase::Url(base) => base.join(source).ok().map(String::from),
                DocumentBase::Unknown => Some(source.trim().to_string()),
            }
        {
            let escaped = resolved.replace('\\', "\\\\").replace('"', "\\\"");
            hint("background-image", format!("url(\"{escaped}\")"));
        }
        let color_attribute = match tag {
            "body" => Some("text"),
            "font" | "hr" => Some("color"),
            _ => None,
        };
        if let Some(color) = color_attribute
            .and_then(|attribute| self.attr(id, attribute))
            .and_then(legacy_color)
        {
            hint("color", color);
        }
        // HTML Rendering #frames-and-framesets: a frameset is as large as the
        // viewport (or the cell of its parent frameset) and splits itself
        // into a grid by its cols and rows dimension lists, absolute, percent
        // and relative (`*`) entries becoming px, % and fr tracks. Frames
        // and nested framesets fill their rectangles.
        if tag == "frameset" {
            for (attribute, property) in [
                ("cols", "grid-template-columns"),
                ("rows", "grid-template-rows"),
            ] {
                let tracks = self.attr(id, attribute).map(frameset_tracks);
                hint(
                    property,
                    tracks
                        .filter(|tracks| !tracks.is_empty())
                        .unwrap_or_else(|| "1fr".into()),
                );
            }
        }
        if matches!(tag, "frameset" | "frame") {
            let nested = self
                .nodes
                .parent(id)
                .is_some_and(|parent| self.tag_name(parent) == Some("frameset"));
            hint("width", "100%".into());
            hint("height", if nested { "100%" } else { "100vh" }.into());
            hint("min-width", "0".into());
            hint("min-height", "0".into());
            if tag == "frame" {
                for side in ["top", "right", "bottom", "left"] {
                    hint(border_property(side, "width"), "0".into());
                }
            }
        }
        // HTML Rendering #flow-content-3 and #tables-2: `<center>` and the
        // `align` attribute of a div or table part align its text. (They
        // also align descendant blocks; layout's `legacy_descendant_align`.)
        let table_part = matches!(tag, "thead" | "tbody" | "tfoot" | "tr" | "td" | "th");
        if tag == "center" {
            hint("text-align", "center".into());
        } else if (tag == "div" || table_part)
            && let Some(align) = self.attr(id, "align")
        {
            let align = align.to_ascii_lowercase();
            let value = match align.as_str() {
                "center" | "middle" => Some("center"),
                // #tables-2's UA sheet: `td[align=absmiddle i]` and so on.
                "absmiddle" if table_part => Some("center"),
                "left" | "right" | "justify" => Some(align.as_str()),
                _ => None,
            };
            if let Some(value) = value {
                hint("text-align", value.to_string());
            }
        }
        // HTML Rendering #the-hr-element-2.
        if tag == "hr" {
            match self
                .attr(id, "align")
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("left") => {
                    hint("margin-left", "0".into());
                    hint("margin-right", "auto".into());
                }
                Some("right") => {
                    hint("margin-left", "auto".into());
                    hint("margin-right", "0".into());
                }
                Some("center") => {
                    hint("margin-left", "auto".into());
                    hint("margin-right", "auto".into());
                }
                _ => {}
            }
            let solid = self.attr(id, "color").is_some() || self.attr(id, "noshade").is_some();
            if solid {
                for side in ["top", "right", "bottom", "left"] {
                    hint(border_property(side, "style"), "solid".into());
                }
            }
            if let Some(size) = self.attr(id, "size").and_then(non_negative_integer) {
                if solid {
                    let width = format!("{}px", f64::from(size) / 2.0);
                    for side in ["top", "right", "bottom", "left"] {
                        hint(border_property(side, "width"), width.clone());
                    }
                } else if size == 1 {
                    hint("border-bottom-width", "0".into());
                } else if size > 1 {
                    hint("height", format!("{}px", size - 2));
                }
            }
            if let Some(width) = self.attr(id, "width").and_then(dimension_value) {
                hint("width", width);
            }
        }
        if tag == "font" {
            if let Some(size) = self.attr(id, "size").and_then(legacy_font_size) {
                hint("font-size", size.to_string());
            }
            if let Some(face) = self.attr(id, "face").filter(|face| !face.trim().is_empty()) {
                hint("font-family", face.to_string());
            }
        }
        if tag == "table"
            && let Some(color) = self.attr(id, "bordercolor").and_then(legacy_color)
        {
            for side in [
                "border-top-color",
                "border-right-color",
                "border-bottom-color",
                "border-left-color",
            ] {
                hint(side, color.clone());
            }
        }
        // Body link colors target hyperlinks throughout this document, not
        // just body descendants. Use the same :link state as the selector
        // engine (all hyperlinks until history-dependent styling is modeled).
        // A frame document or shadow tree must not pick up an outer body.
        if matches!(tag, "a" | "area") && self.attr(id, "href").is_some() {
            let scope = self.tree_scope(id);
            let html = if self.tag_name(scope) == Some("html") {
                Some(scope)
            } else {
                self.child_iter(scope)
                    .find(|&child| self.tag_name(child) == Some("html"))
            };
            let body = html.and_then(|html| {
                self.child_iter(html)
                    .find(|&child| self.tag_name(child) == Some("body"))
            });
            if let Some(color) = body
                .and_then(|body| self.attr(body, "link"))
                .and_then(legacy_color)
            {
                hint("color", color);
            }
        }
    }
}

fn ascii_whitespace(ch: char) -> bool {
    matches!(ch, '\t' | '\n' | '\u{000c}' | '\r' | ' ')
}

/// HTML #rules-for-parsing-dimension-values (snapshot e5071a20c8569).
/// Leading ASCII whitespace and trailing garbage are allowed; a sign or a
/// leading dot fails, and only an immediately following '%' is a percentage.
fn dimension_value(input: &str) -> Option<String> {
    let bytes = input.trim_start_matches(ascii_whitespace).as_bytes();
    if !bytes.first()?.is_ascii_digit() {
        return None;
    }
    let mut position = 0;
    let mut value = 0.0_f64;
    while let Some(digit) = bytes.get(position).filter(|digit| digit.is_ascii_digit()) {
        value = (value * 10.0 + f64::from(digit - b'0')).min(f64::from(f32::MAX));
        position += 1;
    }
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        let mut divisor = 1.0;
        while let Some(digit) = bytes.get(position).filter(|digit| digit.is_ascii_digit()) {
            divisor *= 10.0;
            value += f64::from(digit - b'0') / divisor;
            position += 1;
        }
    }
    let unit = if bytes.get(position) == Some(&b'%') {
        "%"
    } else {
        "px"
    };
    Some(format!("{value}{unit}"))
}

/// HTML #rules-for-parsing-a-list-of-dimensions, as grid tracks: a number
/// with `%` is a percentage, with `*` a relative share (an empty or zero
/// share counts as one), and otherwise an absolute length in CSS pixels.
fn frameset_tracks(value: &str) -> String {
    let mut tracks = Vec::new();
    for entry in value.trim_end_matches(',').split(',') {
        let entry = entry.trim();
        let digits = entry
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(entry.len());
        let number = entry[..digits].parse::<f64>().unwrap_or(0.0);
        let rest = entry[digits..].trim_start();
        tracks.push(if rest.starts_with('%') {
            format!("{number}%")
        } else if rest.starts_with('*') {
            format!("{}fr", if number == 0.0 { 1.0 } else { number })
        } else {
            format!("{number}px")
        });
    }
    tracks.join(" ")
}

/// HTML #rules-for-parsing-non-negative-integers.
fn non_negative_integer(input: &str) -> Option<u32> {
    let digits = input.trim_start_matches(ascii_whitespace);
    let digits = digits.strip_prefix('+').unwrap_or(digits);
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    (end > 0).then(|| {
        digits[..end].bytes().fold(0_u32, |value, digit| {
            value
                .saturating_mul(10)
                .saturating_add(u32::from(digit - b'0'))
        })
    })
}

fn border_property(side: &str, part: &str) -> &'static str {
    match (side, part) {
        ("top", "width") => "border-top-width",
        ("right", "width") => "border-right-width",
        ("bottom", "width") => "border-bottom-width",
        ("left", "width") => "border-left-width",
        ("top", "style") => "border-top-style",
        ("right", "style") => "border-right-style",
        ("bottom", "style") => "border-bottom-style",
        ("left", "style") => "border-left-style",
        ("top", _) => "border-top-color",
        ("right", _) => "border-right-color",
        ("bottom", _) => "border-bottom-color",
        _ => "border-left-color",
    }
}

/// HTML #rules-for-parsing-nonzero-dimension-values: as dimension values,
/// except that zero is an error.
fn nonzero_dimension_value(input: &str) -> Option<String> {
    dimension_value(input).filter(|value| {
        value
            .trim_end_matches(['%', 'p', 'x'])
            .parse::<f64>()
            .is_ok_and(|number| number != 0.0)
    })
}

/// HTML #rules-for-parsing-a-legacy-colour-value. In particular, this is not
/// the CSS color grammar: arbitrary old attribute strings undergo the legacy
/// component algorithm, while an empty string and `transparent` fail.
fn legacy_color(input: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    let input = input.trim_matches(ascii_whitespace);
    if input.eq_ignore_ascii_case("transparent") {
        return None;
    }
    // CSS Color 4 #named-colors. Only an ASCII color name reaches the CSS
    // parser here; functions, escapes, system colors and alpha hex syntax
    // must take HTML's separate legacy algorithm below.
    if input.len() <= 20
        && input.bytes().all(|byte| byte.is_ascii_alphabetic())
        && let Some(crate::render::PaintColor::Rgba(r, g, b, 255)) =
            crate::render::PaintColor::parse_css(&input.to_ascii_lowercase())
    {
        return Some(format!("#{r:02x}{g:02x}{b:02x}"));
    }
    let short = input.as_bytes();
    if short.len() == 4 && short[0] == b'#' && short[1..].iter().all(u8::is_ascii_hexdigit) {
        let channels: [u8; 3] = std::array::from_fn(|i| hex_digit(short[i + 1]) * 17);
        return Some(format!(
            "#{:02x}{:02x}{:02x}",
            channels[0], channels[1], channels[2]
        ));
    }
    // Replace astral code points before the 128-code-point truncation. The
    // intermediate is bounded even for an arbitrarily large attribute.
    let mut digits = Vec::with_capacity(130);
    for ch in input.chars() {
        let count = if ch as u32 > 0xffff { 2 } else { 1 };
        let digit = if ch.is_ascii_hexdigit() {
            ch as u8
        } else {
            b'0'
        };
        for _ in 0..count {
            if digits.len() == 128 {
                break;
            }
            // The initial # is removed only after truncation.
            digits.push(if digits.is_empty() && ch == '#' {
                b'#'
            } else {
                digit
            });
        }
        if digits.len() == 128 {
            break;
        }
    }
    if digits.first() == Some(&b'#') {
        digits.remove(0);
    }
    while digits.is_empty() || !digits.len().is_multiple_of(3) {
        digits.push(b'0');
    }
    let length = digits.len() / 3;
    let mut start = length.saturating_sub(8);
    while length - start > 2 && (0..3).all(|channel| digits[channel * length + start] == b'0') {
        start += 1;
    }
    let end = (start + 2).min(length);
    let component = |channel| {
        digits[channel * length + start..channel * length + end]
            .iter()
            .fold(0u8, |value, digit| value * 16 + hex_digit(*digit))
    };
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        component(0),
        component(1),
        component(2)
    ))
}

fn hex_digit(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => unreachable!("legacy color normalization only retains hex digits"),
    }
}

/// HTML #rules-for-parsing-a-legacy-font-size. Signed values are relative to
/// 3, not the parent font's size. Trailing non-digits are ignored.
fn legacy_font_size(input: &str) -> Option<&'static str> {
    let input = input.trim_start_matches(ascii_whitespace);
    let (sign, digits) = match input.as_bytes().first()? {
        b'+' => (1, &input[1..]),
        b'-' => (-1, &input[1..]),
        _ => (0, input),
    };
    if !digits.as_bytes().first()?.is_ascii_digit() {
        return None;
    }
    // Values above 7 clamp to an endpoint in every mode; saturating at 8
    // avoids overflow without changing even a very long number's result.
    let value = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0i32, |n, digit| (n * 10 + i32::from(digit - b'0')).min(8));
    let value = match sign {
        1 => 3 + value,
        -1 => 3 - value,
        _ => value,
    }
    .clamp(1, 7);
    Some(
        [
            "x-small",
            "small",
            "medium",
            "large",
            "x-large",
            "xx-large",
            "xxx-large",
        ][value as usize - 1],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iframe_dimension_hints_follow_html_parsing_and_cascade() {
        for (input, expected) in [
            ("100%", Some("100%")),
            (" \t75.5%ignored", Some("75.5%")),
            ("25.%", Some("25%")),
            ("0%", Some("0%")),
            ("100 %", Some("100px")),
            ("32px", Some("32px")),
            ("1e2", Some("1px")),
            ("+20", None),
            ("-20", None),
            (".5", None),
            ("\u{a0}20", None),
            ("", None),
        ] {
            assert_eq!(dimension_value(input).as_deref(), expected, "{input:?}");
        }
        let mut dom = Dom::parse_document(
            r#"<style>@layer sizing { #f { height:auto } }</style>
            <iframe id=f width="100%" height="80%"></iframe>"#,
        );
        let frame = dom.get_by_id("f").unwrap();
        assert_eq!(dom.computed_value(frame, "width").as_deref(), Some("100%"));
        assert_eq!(dom.computed_value(frame, "height").as_deref(), Some("auto"));
        dom.set_attr(frame, "width", "75%");
        assert_eq!(dom.computed_value(frame, "width").as_deref(), Some("75%"));
        dom.set_attr(frame, "style", "width:auto");
        assert_eq!(dom.computed_value(frame, "width").as_deref(), Some("auto"));
        let snapshot = dom.serialize(frame);
        assert!(snapshot.contains("width:300px"), "{snapshot}");
        assert!(snapshot.contains("height:150px"), "{snapshot}");
    }

    #[test]
    fn background_attribute_hints_resolve_against_the_document_base() {
        let mut dom = Dom::parse_document(
            r#"<base href="/art/"><body id=b background="stars.gif">
            <table id=t background=""><tr id=r background=" ">
            <td id=c background='tile "1".gif'>x</td></tr></table>
            <div id=d background=stars.gif></div>"#,
        );
        dom.set_doc_url(url::Url::parse("https://site.example/dir/page.html").ok());
        let image = |dom: &Dom, id: &str| {
            dom.computed_value(dom.get_by_id(id).unwrap(), "background-image")
                .unwrap_or_else(|| "none".into())
        };
        assert_eq!(
            image(&dom, "b"),
            r#"url("https://site.example/art/stars.gif")"#
        );
        // Only an empty value is ignored; whitespace parses as the base.
        assert_eq!(image(&dom, "t"), "none");
        assert_eq!(image(&dom, "r"), r#"url("https://site.example/art/")"#);
        assert_eq!(
            image(&dom, "c"),
            r#"url("https://site.example/art/tile%20%221%22.gif")"#
        );
        assert_eq!(image(&dom, "d"), "none");
        let cell = dom.get_by_id("c").unwrap();
        dom.set_attr(cell, "background", "moon.png");
        assert_eq!(
            image(&dom, "c"),
            r#"url("https://site.example/art/moon.png")"#
        );
        dom.set_attr(cell, "style", "background-image:none");
        assert_eq!(image(&dom, "c"), "none");
    }

    #[test]
    fn table_height_attributes_map_to_the_height_property() {
        let dom = Dom::parse_document(
            r#"<table id=t height="80%"><tbody id=g height=0><tr id=r height=" 40.5">
            <td id=c height=0>a</td><td id=d height="12x">b</td><th id=h height="0%">c</th></tr></tbody></table>
            <div id=v height=40></div>"#,
        );
        let height = |id: &str| {
            dom.computed_value(dom.get_by_id(id).unwrap(), "height")
                .unwrap_or_else(|| "auto".into())
        };
        assert_eq!(height("t"), "80%");
        assert_eq!(height("g"), "0px");
        assert_eq!(height("r"), "40.5px");
        // td/th use the nonzero dimension rules.
        assert_eq!(height("c"), "auto");
        assert_eq!(height("d"), "12px");
        assert_eq!(height("h"), "auto");
        assert_eq!(height("v"), "auto");
    }

    #[test]
    fn table_attributes_style_the_table_and_its_cells() {
        let mut dom = Dom::parse_document(
            r#"<table id=t border=" +3px" cellspacing=6 cellpadding=5 bordercolor=red>
            <tr><td id=c style="padding-left:0">a</td></tr></table>
            <table id=z border=0><tbody><tr><td id=zc>b</td></tr></tbody></table>
            <table id=d><tr><th id=dc>c</th></tr></table>
            <table id=n border=1><tr><td><table><tr><td id=nested>d</td></tr></table></td></tr></table>"#,
        );
        let value = |dom: &Dom, id: &str, property: &str| {
            dom.computed_value(dom.get_by_id(id).unwrap(), property)
                .unwrap_or_default()
        };
        assert_eq!(value(&dom, "t", "border-spacing"), "6px");
        assert_eq!(value(&dom, "t", "border-top-width"), "3px");
        assert_eq!(value(&dom, "t", "border-left-style"), "outset");
        assert_eq!(value(&dom, "c", "border-bottom-style"), "inset");
        assert_eq!(value(&dom, "c", "border-bottom-width"), "1px");
        assert_eq!(value(&dom, "c", "border-top-color"), "#ff0000");
        assert_eq!(value(&dom, "c", "padding-top"), "5px");
        assert_eq!(value(&dom, "c", "padding-left"), "0");
        // A zero border keeps zero widths and no styles, here or on cells.
        assert_eq!(value(&dom, "z", "border-top-width"), "0px");
        assert_eq!(value(&dom, "z", "border-top-style"), "");
        assert_eq!(value(&dom, "zc", "border-top-style"), "");
        // UA defaults (HTML Rendering #tables-2).
        assert_eq!(value(&dom, "d", "border-spacing"), "2px");
        assert_eq!(value(&dom, "d", "box-sizing"), "border-box");
        assert_eq!(value(&dom, "dc", "padding-right"), "1px");
        // Cells belong to their own table only.
        assert_eq!(value(&dom, "nested", "border-top-style"), "");
        // Changing the table's attribute restyles its cells.
        let table = dom.get_by_id("t").unwrap();
        dom.set_attr(table, "cellpadding", "9");
        assert_eq!(value(&dom, "c", "padding-top"), "9px");
    }

    #[test]
    fn body_margin_attributes_map_to_margins() {
        let margins = |html: &str| {
            let dom = Dom::parse_document(html);
            let root = dom.document_element().unwrap();
            let body = dom
                .child_iter(root)
                .find(|&child| dom.tag_name(child) == Some("body"))
                .unwrap();
            ["margin-top", "margin-right", "margin-bottom", "margin-left"]
                .map(|property| dom.computed_value(body, property).unwrap_or_default())
        };
        assert_eq!(
            margins("<body topmargin=0 leftmargin=3>"),
            ["0px", "3px", "0px", "3px"]
        );
        // marginheight/marginwidth come first; junk uses the 8px default.
        assert_eq!(
            margins("<body marginheight=5 topmargin=0 marginwidth=x leftmargin=2>"),
            ["5px", "8px", "5px", "8px"]
        );
        assert_eq!(margins("<body>"), ["", "", "", ""]);
        assert_eq!(
            margins("<body topmargin=0 style=margin-top:4px>"),
            ["4px", "", "0px", ""]
        );
        // A frame's marginheight/marginwidth are the fallbacks.
        let mut dom = Dom::parse_document("<iframe id=f marginwidth=0 marginheight=12></iframe>");
        let frame = dom.get_by_id("f").unwrap();
        dom.install_frame_document(frame, "<body leftmargin=4>x", "https://frame.test/")
            .unwrap();
        let body = dom.frame_body(frame).unwrap();
        let value = |property| dom.computed_value(body, property).unwrap_or_default();
        assert_eq!(value("margin-top"), "12px");
        assert_eq!(value("margin-left"), "4px");
    }

    #[test]
    fn legacy_font_sizes_use_browser_keyword_pixels() {
        // HTML Rendering maps <font size> 1-7 to x-small..xxx-large; CSS
        // Fonts 4 #absolute-size-mapping lets UAs tune the keyword sizes,
        // and Gecko and Blink agree on these for a 16px medium.
        let dom = Dom::parse_document(
            "<font id=f1 size=1>a</font><font id=f2 size=2>a</font><font id=f3 size=3>a</font>\
             <font id=f4 size=4>a</font><font id=f5 size=5>a</font><font id=f6 size=6>a</font>\
             <font id=f7 size=7>a</font><span id=xxs style=font-size:xx-small>a</span>",
        );
        let px = |id: &str| dom.font_px(dom.get_by_id(id).unwrap());
        assert_eq!(
            ["f1", "f2", "f3", "f4", "f5", "f6", "f7", "xxs"].map(px),
            [10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0, 9.0]
        );
    }

    #[test]
    fn hr_elements_follow_the_html_rendering_rules() {
        let dom = Dom::parse_document(
            r#"<hr id=plain><hr id=dashed style="border-style:dashed;width:300px">
            <hr id=noshade noshade size=6 width=50%><hr id=tall size=10 align=left>
            <hr id=thin size=1 color=blue>"#,
        );
        let value = |id: &str, property: &str| {
            dom.computed_value_resolved(dom.get_by_id(id).unwrap(), property)
                .unwrap_or_default()
        };
        assert_eq!(value("plain", "color"), "gray");
        assert_eq!(value("plain", "border-top-style"), "inset");
        assert_eq!(value("plain", "border-left-width"), "1px");
        assert_eq!(value("plain", "margin-left"), "auto");
        // An authored style keeps the 1px UA width (not the initial medium).
        assert_eq!(value("dashed", "border-bottom-width"), "1px");
        assert_eq!(value("noshade", "border-top-style"), "solid");
        assert_eq!(value("noshade", "border-top-width"), "3px");
        assert_eq!(value("noshade", "width"), "50%");
        assert_eq!(value("tall", "height"), "8px");
        assert_eq!(value("tall", "margin-left"), "0");
        assert_eq!(value("thin", "border-top-style"), "solid");
        assert_eq!(value("thin", "border-top-width"), "0.5px");
    }

    #[test]
    fn nowrap_cells_do_not_wrap_except_sized_ones_in_quirks_mode() {
        // HTML Rendering #tables-2, as Gecko applies it (Blink ignores the
        // quirk).
        for (doctype, sized) in [("", "normal"), ("<!doctype html>", "nowrap")] {
            let dom = Dom::parse_document(&format!(
                "{doctype}<table><tr><td id=a nowrap>a</td><td id=b nowrap width=150>b</td>\
                 <td id=c nowrap width=50%>c</td><td id=d>d</td></tr></table>"
            ));
            let value = |id: &str| {
                dom.computed_value_resolved(dom.get_by_id(id).unwrap(), "white-space")
                    .unwrap_or_default()
            };
            assert_eq!(value("a"), "nowrap", "{doctype}");
            assert_eq!(value("b"), sized, "{doctype}");
            assert_eq!(value("c"), "nowrap", "{doctype}");
            assert_ne!(value("d"), "nowrap", "{doctype}");
        }
    }

    #[test]
    fn align_attributes_and_center_are_text_align_hints() {
        // HTML Rendering #flow-content-3 and #tables-2.
        let mut dom = Dom::parse_document(
            r#"<div style="text-align:right"><center id=c><p id=p>x</p><p id=pa align=LEFT>x</p></center>
            <div id=d align=middle>x</div><h2 id=h align=justify>x</h2><div id=bad align=absmiddle>x</div></div>
            <table><caption id=cap>c</caption><tr id=tr align=right><th id=th1>h</th></tr>
            <tr><th id=th2>h</th><td id=td align=absmiddle>d</td><td id=styled align=center style="text-align:left">s</td></tr></table>"#,
        );
        let value = |id: &str| {
            dom.computed_value_resolved(dom.get_by_id(id).unwrap(), "text-align")
                .unwrap_or_default()
        };
        assert_eq!(value("c"), "center");
        assert_eq!(value("p"), "center", "inherited from <center>");
        assert_eq!(value("pa"), "left");
        assert_eq!(value("d"), "center");
        assert_eq!(value("h"), "justify");
        assert_eq!(value("bad"), "right", "absmiddle is only for table parts");
        assert_eq!(value("cap"), "center");
        assert_eq!(
            value("th1"),
            "right",
            "a th inherits a non-initial alignment"
        );
        assert_eq!(value("th2"), "center");
        assert_eq!(value("td"), "center");
        assert_eq!(value("styled"), "left", "author style beats the hint");
        let (p, td) = (dom.get_by_id("p").unwrap(), dom.get_by_id("td").unwrap());
        dom.set_attr(p, "align", "right");
        dom.set_attr(td, "align", "left");
        let value = |id: NodeId| dom.computed_value_resolved(id, "text-align");
        assert_eq!(value(p).as_deref(), Some("right"));
        assert_eq!(value(td).as_deref(), Some("left"));
    }

    #[test]
    fn form_controls_do_not_inherit_the_font_size() {
        // Gecko and Blink UA sheets: input/button/select 13.333px, textarea
        // `font: medium monospace` (13px); author CSS still wins.
        let dom = Dom::parse_document(
            r#"<div style="font-size:20px"><input id=i><button id=b>x</button><select id=s></select>
            <textarea id=t></textarea><input id=a style="font-size:1.5em"><button id=n style="font-size:inherit">y</button>
            <button><span id=c>z</span></button></div>"#,
        );
        let px = |id: &str| dom.font_px(dom.get_by_id(id).unwrap());
        for (id, expected) in [
            ("i", 13.333333),
            ("b", 13.333333),
            ("s", 13.333333),
            ("t", 13.0),
            ("a", 30.0),
            ("n", 20.0),
            ("c", 13.333333),
        ] {
            assert!((px(id) - expected).abs() < 0.001, "{id}: {}", px(id));
        }
    }

    #[test]
    fn monospace_elements_use_the_monospace_medium_size() {
        // HTML Rendering makes pre/code/kbd/samp/tt monospace; Gecko and
        // Blink size a keyword-derived font against a 13px monospace medium
        // only when the family list is exactly `monospace`.
        let dom = Dom::parse_document(
            r#"<pre id=pre>a</pre><code id=code>a</code><kbd id=kbd>a</kbd>
            <span id=pair style="font-family:monospace, serif">a</span>
            <div style="font-size:20px"><code id=fixed>a</code></div>
            <h1><code id=heading>a</code></h1><small><tt id=small>a</tt></small>
            <pre><span id=back style="font-family:sans-serif">a</span></pre>
            <code id=em style="font-size:1.5em">a</code>"#,
        );
        let px = |id: &str| dom.font_px(dom.get_by_id(id).unwrap());
        assert_eq!(
            dom.computed_value(dom.get_by_id("pre").unwrap(), "font-family")
                .as_deref(),
            Some("monospace")
        );
        for (id, expected) in [
            ("pre", 13.0),
            ("code", 13.0),
            ("kbd", 13.0),
            ("pair", 16.0),
            ("fixed", 20.0),
            ("heading", 26.0),
            ("small", 13.0 / 1.2),
            ("back", 16.0),
            ("em", 19.5),
        ] {
            assert!((px(id) - expected).abs() < 0.01, "{id}: {}", px(id));
        }
    }

    #[test]
    fn legacy_color_follows_html_code_point_and_component_rules() {
        for (input, expected) in [
            ("", None),
            (" \tTRANSPARENT\n", None),
            (" \t\n", Some("#000000")),
            (" ReBeccAPurple ", Some("#663399")),
            ("#AbC", Some("#aabbcc")),
            ("ff69b4", Some("#ff69b4")),
            ("#12", Some("#010200")),
            ("#1234", Some("#123400")),
            ("#12345678", Some("#124578")),
            ("chucknorris", Some("#c00000")),
            ("😀a", Some("#00000a")),
            ("#f00\u{00a0}", Some("#f00000")),
            ("000012000034000056", Some("#123456")),
            ("100000012200000034300000056", Some("#123456")),
        ] {
            assert_eq!(legacy_color(input).as_deref(), expected, "{input:?}");
        }
        let prefix = "😀".repeat(63) + "ab";
        assert_eq!(
            legacy_color(&(prefix.clone() + "cdef")),
            legacy_color(&prefix)
        );
        assert_eq!(
            legacy_color(&("0".repeat(128) + "ffffff")),
            Some("#000000".into())
        );
    }

    #[test]
    fn legacy_font_size_uses_absolute_keywords_and_saturates() {
        for (input, expected) in [
            ("", None),
            ("+", None),
            ("- 2", None),
            ("no", None),
            ("0", Some("x-small")),
            ("1", Some("x-small")),
            ("2", Some("small")),
            ("3", Some("medium")),
            ("4", Some("large")),
            ("5", Some("x-large")),
            ("6", Some("xx-large")),
            ("7", Some("xxx-large")),
            (" \n+2tail", Some("x-large")),
            ("-2", Some("x-small")),
            ("+0", Some("medium")),
            ("99999999999999999999999", Some("xxx-large")),
            ("-99999999999999999999999", Some("x-small")),
        ] {
            assert_eq!(legacy_font_size(input), expected, "{input:?}");
        }
    }

    #[test]
    fn legacy_html_hints_join_inheritance_and_author_cascade() {
        let dom = Dom::parse_document(
            r##"
            <!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 2.0//EN">
            <style>
              @layer first { #sheet { color: lime; font-size: 20px; } }
              #initial { color: initial } #inherit { color: inherit }
              #revert { color: revert } #layer { color: revert-layer }
              #important { color: revert-layer !important }
              #variable { color: var(--missing, revert-layer) }
            </style>
            <body id=body bgcolor="#2d2d2d" text="#ffffff" link="#ff69b4">
              <p id=plain>Text</p><a id=link href=/news><b id=child>Link</b></a>
              <h1><font id=font color="#4a90d9" size=7 face=monospace>Title</font></h1>
              <font id=sheet color=red size=7>Sheet overrides hints</font>
              <font id=initial color=red>Initial</font><font id=inherit color=red>Inherit</font>
              <font id=revert color=red>Revert</font><font id=layer color=red>Layer</font>
              <font id=important color=red>Important</font><font id=variable color=red>Variable</font>
              <div id=unrelated color=red bgcolor=red>Attributes without hints</div>
            </body>
        "##,
        );
        let value =
            |id, property| dom.computed_value_resolved(dom.get_by_id(id).unwrap(), property);
        assert_eq!(
            value("body", "background-color").as_deref(),
            Some("#2d2d2d")
        );
        for id in ["plain", "inherit", "revert", "unrelated"] {
            assert_eq!(value(id, "color").as_deref(), Some("#ffffff"), "{id}");
        }
        for id in ["link", "child"] {
            assert_eq!(value(id, "color").as_deref(), Some("#ff69b4"));
        }
        assert_eq!(value("font", "color").as_deref(), Some("#4a90d9"));
        assert_eq!(value("font", "font-size").as_deref(), Some("xxx-large"));
        // face=monospace sizes the keyword from the 13px monospace row.
        assert_eq!(dom.font_px(dom.get_by_id("font").unwrap()), 39.);
        assert_eq!(value("font", "font-family").as_deref(), Some("monospace"));
        assert_eq!(value("sheet", "color").as_deref(), Some("lime"));
        assert_eq!(value("sheet", "font-size").as_deref(), Some("20px"));
        assert_eq!(value("initial", "color"), None);
        for id in ["layer", "important", "variable"] {
            assert_eq!(value(id, "color").as_deref(), Some("#ff0000"), "{id}");
        }
        assert_eq!(value("unrelated", "background-color"), None);
    }

    #[test]
    fn legacy_html_hints_follow_attribute_changes_and_document_boundaries() {
        let mut dom = Dom::parse_document(
            r#"
            <body id=body text=white link=red vlink=green alink=blue>
              <a id=link href=/><span id=child>Link</span></a>
              <font id=font color=blue size=7>Title</font><div id=host></div><iframe id=frame></iframe>
            </body>
        "#,
        );
        let body = dom.get_by_id("body").unwrap();
        let child = dom.get_by_id("child").unwrap();
        let font = dom.get_by_id("font").unwrap();
        assert_eq!(
            dom.computed_value(child, "color").as_deref(),
            Some("#ff0000")
        );
        assert_eq!(dom.font_px(font), 48.);
        dom.set_attr(body, "link", "rebeccapurple");
        assert_eq!(
            dom.computed_value(child, "color").as_deref(),
            Some("#663399")
        );
        dom.remove_attr(body, "link");
        assert_eq!(
            dom.computed_value(child, "color").as_deref(),
            Some("#0000ee")
        );
        dom.set_attr(font, "size", "-2");
        dom.set_attr(font, "color", "transparent");
        // size -2 is x-small: 10px for a 16px medium, as in Gecko and Blink.
        assert_eq!(dom.font_px(font), 10.);
        assert_eq!(
            dom.computed_value(font, "color").as_deref(),
            Some("#ffffff")
        );
        dom.set_attr(body, "link", "red");
        let frame = dom.get_by_id("frame").unwrap();
        dom.install_frame_document(
            frame,
            "<body text=black link=blue><a id=inner href=/>Frame</a>",
            "https://frame.test/",
        )
        .unwrap();
        let inner = dom.get_by_id("inner").unwrap();
        assert_eq!(
            dom.computed_value(inner, "color").as_deref(),
            Some("#0000ff")
        );
        assert_eq!(
            dom.computed_value(child, "color").as_deref(),
            Some("#ff0000")
        );
        let shadow = dom.attach_shadow(dom.get_by_id("host").unwrap());
        let shadow_link = dom.create_element("a");
        dom.set_attr(shadow_link, "href", "/shadow");
        dom.append(shadow, shadow_link);
        assert_eq!(
            dom.computed_value(shadow_link, "color").as_deref(),
            Some("#0000ee")
        );
        // A hyperlink outside the body still uses its document's link hint.
        let html = dom.document_element().unwrap();
        let link = dom.create_element("a");
        dom.set_attr(link, "href", "/outside");
        dom.append(html, link);
        assert_eq!(
            dom.computed_value(link, "color").as_deref(),
            Some("#ff0000")
        );
        dom.set_attr(body, "link", "blue");
        assert_eq!(
            dom.computed_value(link, "color").as_deref(),
            Some("#0000ff")
        );
        dom.detach(body);
        assert_eq!(
            dom.computed_value(link, "color").as_deref(),
            Some("#0000ee")
        );
        dom.append(html, body);
        assert_eq!(
            dom.computed_value(link, "color").as_deref(),
            Some("#0000ff")
        );
    }

    #[test]
    fn embedded_content_align_attributes_float_and_align_vertically() {
        // HTML Rendering #attributes-for-embedded-content-and-images.
        let dom = Dom::parse_document(
            "<img id=l align=LEFT><iframe id=r align=right></iframe>\
             <input id=i type=image align=absmiddle><object id=t align=texttop></object>\
             <img id=m align=middle><embed id=c align=abscenter><img id=b align=bottom>\
             <div id=d align=left>d</div><input id=x type=text align=left>\
             <img id=s align=left style='float:none'>",
        );
        let value =
            |id: &str, property: &str| dom.computed_value(dom.get_by_id(id).unwrap(), property);
        for (id, property, expected) in [
            ("l", "float", Some("left")),
            ("r", "float", Some("right")),
            ("i", "vertical-align", Some("middle")),
            ("t", "vertical-align", Some("text-top")),
            ("m", "vertical-align", Some("-webkit-baseline-middle")),
            ("c", "vertical-align", Some("middle")),
            ("b", "vertical-align", Some("bottom")),
            ("d", "float", None),
            ("x", "float", None),
            ("s", "float", Some("none")),
        ] {
            assert_eq!(value(id, property).as_deref(), expected, "{id} {property}");
        }
    }

    #[test]
    fn marquee_dimension_space_and_direction_attributes_are_hints() {
        // HTML Rendering #the-marquee-element-2, measured against LibreWolf
        // 153: dimensions and spacing map to CSS, an up/down marquee is
        // 200px tall by default, and horizontal content stays on one line.
        let dom = Dom::parse_document(
            "<marquee id=a width=200 height=70% hspace=10 vspace=5>a</marquee>\
             <marquee id=b direction=UP>b</marquee><marquee id=c direction=down height=50>c</marquee>\
             <marquee id=d style='height:30px;white-space:normal' height=70>d</marquee>",
        );
        let value =
            |id: &str, property: &str| dom.computed_value(dom.get_by_id(id).unwrap(), property);
        for (id, property, expected) in [
            ("a", "width", Some("200px")),
            ("a", "height", Some("70%")),
            ("a", "margin-left", Some("10px")),
            ("a", "margin-right", Some("10px")),
            ("a", "margin-top", Some("5px")),
            ("a", "text-wrap-mode", Some("nowrap")),
            ("b", "height", Some("200px")),
            ("b", "text-wrap-mode", None),
            ("c", "height", Some("50px")),
            ("d", "height", Some("30px")),
            ("d", "text-wrap-mode", Some("wrap")),
        ] {
            assert_eq!(value(id, property).as_deref(), expected, "{id} {property}");
        }
        let resolved =
            |id: &str| dom.cssom_resolved_value(dom.get_by_id(id).unwrap(), "white-space");
        assert_eq!(resolved("a").as_deref(), Some("nowrap"));
        assert_eq!(resolved("b").as_deref(), Some("normal"));
    }

    #[test]
    fn legacy_table_colors_are_hints_and_remain_namespace_scoped() {
        let dom = Dom::parse_document(
            r#"
            <style>td { background: lime; } #css { background: initial; }</style>
            <body><table id=table bgcolor=red bordercolor=blue>
              <tbody id=group bgcolor=green><tr id=row bgcolor=yellow>
                <td id=cell bgcolor=red>Cell</td><td id=css bgcolor=red>CSS</td>
              </tr></tbody></table><marquee id=ticker bgcolor=purple>Text</marquee>
              <svg><text id=svg color=red>SVG paint attributes use SVG's rules</text></svg>
            </body>
        "#,
        );
        for (id, color) in [
            ("table", "#ff0000"),
            ("group", "#008000"),
            ("row", "#ffff00"),
            ("cell", "lime"),
            ("ticker", "#800080"),
        ] {
            assert_eq!(
                dom.computed_value(dom.get_by_id(id).unwrap(), "background-color")
                    .as_deref(),
                Some(color)
            );
        }
        assert_eq!(
            dom.computed_value(dom.get_by_id("table").unwrap(), "border-top-color")
                .as_deref(),
            Some("#0000ff")
        );
        assert_eq!(
            dom.computed_value(dom.get_by_id("css").unwrap(), "background-color"),
            None
        );
        assert_eq!(
            dom.computed_value(dom.get_by_id("svg").unwrap(), "color"),
            None
        );
    }
}
