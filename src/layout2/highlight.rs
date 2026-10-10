//! CSS Pseudo 4 #highlight-pseudos: how a selection's highlight paints over
//! an element's text.

use crate::dom::{Dom, NodeId};
use crate::render::{HighlightPaint, PaintColor};

/// The `::selection` paint of `element`'s text. Without author colors the
/// paired default highlight colors apply (#paired-defaults): `Highlight`
/// behind text that keeps its own color, the UA processing
/// #highlight-styling allows when the author origin gives no colors
/// (`HighlightPaint::Ua`, resolved against the text's color). With
/// either author color both are the author's, respected exactly: the
/// background's initial value is transparent, and `currentColor` (the root
/// highlight's inherited `color`) leaves the text its own color. Painting
/// asks only where the document styles `::selection` at all
/// (`Dom::has_selection_rules`); elsewhere every highlight is the UA's.
pub(crate) fn selection_highlight(dom: &Dom, element: NodeId) -> HighlightPaint {
    let color = dom.selection_declared(element, "color");
    let background = dom.selection_declared(element, "background-color");
    if color.is_none() && background.is_none() {
        return HighlightPaint::Ua;
    }
    let scheme = dom.color_scheme(element);
    let resolve = |value: &str| -> Option<PaintColor> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("currentcolor") {
            return None;
        }
        if value.eq_ignore_ascii_case("initial") {
            return crate::dom::color_scheme::system_color("canvastext", scheme)
                .and_then(PaintColor::parse_css);
        }
        super::graphics::resolve_element_color(dom, element, value).or_else(|| {
            crate::dom::color_scheme::system_color(value, scheme).and_then(PaintColor::parse_css)
        })
    };
    HighlightPaint::Author {
        color: color.as_deref().and_then(resolve),
        background: background
            .as_deref()
            .filter(|value| !value.trim().eq_ignore_ascii_case("initial"))
            .and_then(resolve)
            .unwrap_or(PaintColor::Rgba(0, 0, 0, 0)),
    }
}

#[cfg(test)]
mod tests {
    use crate::dom::{Dom, NodeId};
    use crate::layout2::{Viewport, lay_out_graphical};
    use crate::render::selection::LineStart;
    use crate::render::{DisplayCommand, HighlightPaint, RunSelection};
    use url::Url;

    fn layout(html: &str, width: f32) -> (Dom, crate::layout2::GraphicalLayout) {
        let dom = Dom::parse_document(html);
        let layout = lay_out_graphical(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            Viewport::new(width, 480.0),
            &[],
            &Default::default(),
            &Default::default(),
        );
        (dom, layout)
    }

    fn runs(layout: &crate::layout2::GraphicalLayout) -> Vec<(String, RunSelection)> {
        layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::GlyphRun {
                    shaped, selection, ..
                } => Some((shaped.text.clone(), selection.clone())),
                _ => None,
            })
            .collect()
    }

    fn text_child(dom: &Dom, id: &str, index: usize) -> NodeId {
        let element = dom.get_by_id(id).unwrap();
        dom.children(element)
            .into_iter()
            .filter(|&child| dom.tag_name(child).is_none())
            .nth(index)
            .unwrap()
    }

    /// (byte start, byte end, node, data start, data end) of each span.
    fn spans(selection: &RunSelection) -> Vec<(u32, u32, NodeId, u32, u32)> {
        selection
            .spans()
            .iter()
            .map(|s| (s.start, s.end, s.node, s.offset, s.offset_end))
            .collect()
    }

    #[test]
    fn runs_name_the_text_data_they_lay_out() {
        let (dom, layout) = layout("<p id=p>  Hello   <b id=b>bold</b>\n world</p>", 640.0);
        let (first, bold, last) = (
            text_child(&dom, "p", 0),
            text_child(&dom, "b", 0),
            text_child(&dom, "p", 1),
        );
        let runs = runs(&layout);
        let find = |text: &str| runs.iter().find(|(t, _)| t == text).unwrap().1.clone();
        assert_eq!(spans(&find("Hello")), vec![(0, 5, first, 2, 7)]);
        assert_eq!(spans(&find("bold")), vec![(0, 4, bold, 0, 4)]);
        assert_eq!(spans(&find("world")), vec![(0, 5, last, 2, 7)]);
    }

    #[test]
    fn merged_words_keep_one_span_each_around_collapsed_space() {
        let (dom, layout) = layout("<p id=p>one  two\tthree</p>", 640.0);
        let text = text_child(&dom, "p", 0);
        let runs = runs(&layout);
        let (laid, selection) = runs.iter().find(|(t, _)| t.starts_with("one")).unwrap();
        assert_eq!(laid, "one two three");
        assert_eq!(
            spans(selection),
            vec![(0, 3, text, 0, 3), (4, 7, text, 5, 8), (8, 13, text, 9, 14)]
        );
        assert_eq!(selection.line_start, LineStart::Block);
    }

    #[test]
    fn preserved_text_maps_across_tabs_and_lines() {
        let (dom, layout) = layout("<pre id=p>ab\tc\nde</pre>", 640.0);
        let text = text_child(&dom, "p", 0);
        let runs = runs(&layout);
        let span_of = |laid: &str| spans(&runs.iter().find(|(t, _)| t == laid).unwrap().1);
        assert_eq!(span_of("ab"), vec![(0, 2, text, 0, 2)]);
        assert_eq!(span_of("c"), vec![(0, 1, text, 3, 4)]);
        assert_eq!(span_of("de"), vec![(0, 2, text, 5, 7)]);
        let de = runs.iter().find(|(t, _)| t == "de").unwrap();
        assert_eq!(de.1.line_start, LineStart::Forced);
    }

    #[test]
    fn a_transform_keeps_the_data_range_and_first_letter_splits_it() {
        let (dom, layout) = layout(
            "<style>p::first-letter{color:red}</style>\
             <p id=p style='text-transform:uppercase'>straße x</p>",
            640.0,
        );
        let text = text_child(&dom, "p", 0);
        let runs = runs(&layout);
        let letter = runs.iter().find(|(t, _)| t == "S").unwrap();
        assert_eq!(spans(&letter.1), vec![(0, 1, text, 0, 1)]);
        let rest = runs.iter().find(|(t, _)| t.starts_with("TRASSE")).unwrap();
        assert_eq!(rest.1.spans()[0].node, text);
        assert_eq!(rest.1.spans()[0].offset, 1);
        assert_eq!(rest.1.spans()[0].offset_end, 6, "the data's 'traße'");
    }

    #[test]
    fn soft_wraps_and_forced_breaks_mark_their_lines() {
        let (_, layout) = layout(
            "<p style='font:16px/20px monospace;width:60px;margin:0'>aaaa bbbb<br>cccc</p>",
            640.0,
        );
        let starts: Vec<_> = runs(&layout)
            .into_iter()
            .map(|(text, selection)| (text, selection.line_start))
            .collect();
        assert_eq!(
            starts,
            vec![
                ("aaaa".into(), LineStart::Block),
                ("bbbb".into(), LineStart::Soft),
                ("cccc".into(), LineStart::Forced),
            ]
        );
    }

    #[test]
    fn unselectable_and_generated_text_carry_no_sources() {
        let (_, layout) = layout(
            "<style>#g::before{content:'gen '}</style>\
             <p id=g>own</p>\
             <div style='user-select:none'>none <span>auto under none</span>\
             <span style='user-select:text'>text</span></div>\
             <p style='-webkit-user-select:none'>prefixed</p>\
             <button>label</button>",
            640.0,
        );
        let runs = runs(&layout);
        let selectable = |text: &str| {
            runs.iter()
                .find(|(t, _)| t.trim() == text)
                .unwrap_or_else(|| panic!("{text}: {runs:?}"))
                .1
                .selectable()
        };
        assert!(!selectable("gen"));
        assert!(selectable("own"));
        assert!(!selectable("none"));
        assert!(!selectable("auto under none"));
        assert!(selectable("text"));
        assert!(!selectable("prefixed"));
        assert!(!selectable("label"));
    }

    #[test]
    fn user_select_used_values_follow_css_ui_4() {
        use crate::dom::UserSelect;
        let dom = Dom::parse_document(
            "<div id=all style='user-select:all'><span id=in>x</span></div>\
             <div id=none style='user-select:none'><div id=ce contenteditable>y</div></div>\
             <button id=button>b</button><textarea id=area></textarea>\
             <textarea id=ro readonly></textarea><p id=p>z</p>",
        );
        let used = |id: &str| dom.user_select(dom.get_by_id(id).unwrap());
        assert_eq!(used("in"), UserSelect::All);
        assert_eq!(used("none"), UserSelect::None);
        assert_eq!(used("ce"), UserSelect::Contain);
        assert_eq!(used("button"), UserSelect::None);
        assert_eq!(used("area"), UserSelect::Contain);
        assert_eq!(used("ro"), UserSelect::Text);
        assert_eq!(used("p"), UserSelect::Text);
    }

    #[test]
    fn get_computed_style_reports_highlight_inheritance() {
        // CSS Pseudo 4 #getComputedStyle and #highlight-cascade: the
        // highlight's colors as if it were active, inherited from the
        // parent element's highlight; the root's color is currentColor and
        // its background transparent.
        let dom = Dom::parse_document(
            "<style>p::selection{background-color:#ff3fa4} em::selection{color:navy}</style>\
             <p id=p style='color:rgb(1,2,3)'>a <em id=em>b</em></p><div id=d>c</div>",
        );
        let value = |id: &str, name: &str| {
            dom.cssom_pseudo_resolved_value(
                dom.get_by_id(id).unwrap(),
                crate::dom::PseudoEl::Selection,
                name,
            )
            .unwrap()
        };
        assert_eq!(value("p", "background-color"), "rgb(255, 63, 164)");
        assert_eq!(value("em", "background-color"), "rgb(255, 63, 164)");
        assert_eq!(value("em", "color"), "rgb(0, 0, 128)");
        assert_eq!(value("p", "color"), "rgb(1, 2, 3)");
        assert_eq!(value("d", "background-color"), "rgba(0, 0, 0, 0)");
    }

    #[test]
    fn the_ua_highlight_contrasts_with_the_text() {
        // #highlight-styling: without author colors the UA's Highlight is
        // painted under text that keeps its color, the light scheme's under
        // dark text and the dark scheme's under light text.
        use crate::render::selection::{UA_DARK_HIGHLIGHT, UA_LIGHT_HIGHLIGHT};
        let (_, layout) = layout(
            "<p>light</p><p style='color-scheme:dark;color:#fbfbfe'>dark</p>",
            640.0,
        );
        let runs = runs(&layout);
        let highlight = |text: &str| runs.iter().find(|(t, _)| t == text).unwrap().1.highlight;
        assert_eq!(highlight("light"), HighlightPaint::Ua);
        assert_eq!(
            HighlightPaint::Ua.background(crate::render::CANVAS_TEXT),
            UA_LIGHT_HIGHLIGHT
        );
        assert_eq!(
            HighlightPaint::Ua.background(crate::render::PaintColor::Rgba(251, 251, 254, 255)),
            UA_DARK_HIGHLIGHT
        );
    }
}
