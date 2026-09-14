//! HTML rendering's legacy color and font presentational hints.
//!
//! WHATWG HTML snapshot e5071a20c8569 (2026-09-06), #the-page,
//! #phrasing-content-3, #tables-2, #the-hr-element-2 and #the-marquee-element-2;
//! color parsing: #rules-for-parsing-a-legacy-colour-value. These are CSS
//! declarations in the presentational-hint origin, not inline author styles.

use super::{Dom, NodeId};

impl Dom {
    pub(super) fn html_presentational_hints(
        &self,
        id: NodeId,
        mut hint: impl FnMut(&'static str, String),
    ) {
        if self.namespace_uri(id) != Some("http://www.w3.org/1999/xhtml") {
            return;
        }
        let tag = self.tag_name(id).unwrap_or("");
        if matches!(
            tag,
            "body" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th" | "marquee"
        ) && let Some(color) = self.attr(id, "bgcolor").and_then(legacy_color)
        {
            hint("background-color", color);
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
        assert_eq!(dom.font_px(dom.get_by_id("font").unwrap()), 48.);
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
        assert_eq!(dom.font_px(font), 12.);
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
