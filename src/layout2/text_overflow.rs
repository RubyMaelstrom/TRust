//! CSS Overflow 3 #text-overflow (local CSSWG snapshot 81c27f686901).
//!
//! `text-overflow` applies to block containers and specifies rendering when
//! inline content overflows the end line box edge of its block container
//! element ("the block") that has `overflow` other than `visible`. Lines laid
//! in the block's anonymous block boxes belong to the same block container
//! element; a nested element's lines follow that element's own (non-
//! inherited) value. Ellipsing only affects rendering (#ellipsing-details):
//! the painted line content changes, while the laid content stays available
//! to CSSOM View and scrollable overflow.

use std::sync::Arc;

use super::flow::{Frag, FragKind};
use super::overflow::Overflow;
use super::style::{BoxStyle, InlineStyle};
use crate::layout2::NO_NODE;

/// Whether `style` ellipses its overflowing lines. The inline axis must
/// clip: `hidden` or `clip`. A user-scrollable inline axis keeps every
/// character reachable instead; the specification lets the UA treat
/// `ellipsis` as `clip` while the user scrolls (#ellipsis-interaction), and
/// TRust's scroll offsets are applied after paint, where a static ellipsis
/// would hide the content scrolled into view.
pub(super) fn applies(style: &BoxStyle) -> bool {
    let inline_axis = usize::from(style.vertical.is_some());
    style.text_overflow_ellipsis
        && matches!(
            style.overflow[inline_axis],
            Overflow::Hidden | Overflow::Clip
        )
}

/// Ellipse the block's own line boxes among `children` (including those of
/// its anonymous block boxes). `root` is the block's root inline style;
/// `rtl` puts the end edge of its line boxes at their left.
pub(super) fn apply(children: &mut [Frag], root: &InlineStyle, rtl: bool) {
    let mut hidden = Vec::new();
    for fragment in children.iter_mut() {
        if fragment.flow.hidden
            || fragment.flow.inline_box
            || fragment.paint.outside_marker
            || fragment.paint.float
        {
            continue;
        }
        let anonymous = fragment.node == NO_NODE
            && fragment.paint.pseudo.is_none()
            && !fragment.flow.independent;
        match &mut fragment.kind {
            FragKind::Line(line) => {
                if line.laid.is_none() && !overflows_end(line, rtl) {
                    continue;
                }
                let line = Arc::make_mut(line);
                hidden.extend(super::inline::text_overflow_ellipsis(line, root, rtl));
            }
            FragKind::Block if anonymous => apply(&mut fragment.children, root, rtl),
            _ => {}
        }
    }
    // Atomic inline boxes are laid beside their line; hide the elided ones
    // and restore any a previous pass hid.
    for fragment in children.iter_mut().filter(|f| f.flow.inline_box) {
        fragment.flow.elided = hidden.contains(&fragment.node);
    }
}

fn overflows_end(line: &super::flow::LineFrag, rtl: bool) -> bool {
    line.pieces.iter().chain(&line.atom_boxes).any(|piece| {
        if rtl {
            piece.x < line.band[0] - 0.01
        } else {
            piece.x + piece.box_width > line.band[1] + 0.01
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::dom::{Dom, NodeId};
    use crate::layout2::{Viewport, lay_out_graphical};
    use crate::render::DisplayCommand;
    use url::Url;

    fn layout(html: &str) -> (Dom, crate::layout2::GraphicalLayout) {
        let dom = Dom::parse_document(html);
        let layout = lay_out_graphical(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            Viewport::new(640.0, 480.0),
            &[],
            &Default::default(),
            &Default::default(),
        );
        (dom, layout)
    }

    fn by_id(dom: &Dom, id: &str) -> NodeId {
        dom.descendants(crate::dom::DOCUMENT)
            .find(|&node| dom.attr(node, "id") == Some(id))
            .unwrap()
    }

    /// Painted glyph runs as (text, x, width).
    fn runs(layout: &crate::layout2::GraphicalLayout) -> Vec<(String, f32, f32)> {
        layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::GlyphRun { shaped, origin, .. } => {
                    Some((shaped.text.clone(), origin.x, shaped.advance))
                }
                _ => None,
            })
            .collect()
    }

    const STYLE: &str = "<style>body{margin:0;font:16px/20px monospace}\
        .t{width:100px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}</style>";

    #[test]
    fn ellipsis_replaces_the_characters_at_the_end_edge() {
        let (_, layout) = layout(&format!(
            "{STYLE}<div class=t>abcdefghijklmnopqrstuvwxyz</div>"
        ));
        let runs = runs(&layout);
        let ellipsis = runs
            .iter()
            .find(|(text, ..)| text == "…")
            .expect("an ellipsis paints");
        let text = runs
            .iter()
            .find(|(text, ..)| text.starts_with("abc"))
            .expect("the remaining text paints");
        assert!(text.0.len() < 26, "characters are hidden: {}", text.0);
        // The ellipsis is placed immediately after the remaining content
        // and fits in the line box.
        assert!((ellipsis.1 - (text.1 + text.2)).abs() < 0.5, "{runs:?}");
        assert!(ellipsis.1 + ellipsis.2 <= 100.5, "{runs:?}");
        // Hidden characters are whole characters that would overlap the
        // ellipsis: one more would not fit.
        assert!(text.1 + text.2 + ellipsis.2 * 2.0 > 100.0, "{runs:?}");
    }

    #[test]
    fn ellipsing_does_not_change_layout_or_cssom_geometry() {
        let html = |overflow: &str| {
            format!(
                "{STYLE}<div class=t style='text-overflow:{overflow}'>\
                 <span id=s>abcdefghijklmnopqrstuvwxyz</span></div>"
            )
        };
        let (dom, clipped) = layout(&html("clip"));
        let (_, ellipsed) = layout(&html("ellipsis"));
        let span = by_id(&dom, "s");
        assert_eq!(clipped.boxes[&span].width, ellipsed.boxes[&span].width);
        assert!(ellipsed.boxes[&span].width > 200.0);
        assert!(runs(&clipped).iter().all(|(text, ..)| text != "…"));
        assert!(runs(&ellipsed).iter().any(|(text, ..)| text == "…"));
    }

    #[test]
    fn only_clipping_inline_overflow_ellipses() {
        for (style, expected) in [
            ("overflow:visible", false),
            ("overflow:clip", true),
            ("overflow-x:hidden", true),
            // A scroll container keeps its content reachable.
            ("overflow:auto", false),
            ("overflow:scroll", false),
            ("overflow:hidden;width:auto", false),
        ] {
            let (_, layout) = layout(&format!(
                "{STYLE}<div class=t style='{style}'>abcdefghijklmnopqrstuvwxyz</div>"
            ));
            assert_eq!(
                runs(&layout).iter().any(|(text, ..)| text == "…"),
                expected,
                "{style}"
            );
        }
    }

    #[test]
    fn fitting_lines_and_nested_blocks_do_not_ellipse() {
        let (_, layout) = layout(&format!(
            "{STYLE}<div class=t>short<p style='margin:0'>abcdefghijklmnopqrstuvwxyz</p></div>"
        ));
        // CSS Overflow 3 examples: the nested paragraph has its own
        // text-overflow (clip), so it does not ellipse.
        assert!(runs(&layout).iter().all(|(text, ..)| text != "…"));
    }

    #[test]
    fn anonymous_block_lines_ellipse_with_their_block() {
        let (_, layout) = layout(&format!(
            "{STYLE}<div class=t>abcdefghijklmnopqrstuvwxyz<p style='margin:0'>x</p>\
             ABCDEFGHIJKLMNOPQRSTUVWXYZ</div>"
        ));
        let ellipses = runs(&layout)
            .iter()
            .filter(|(text, ..)| text == "…")
            .count();
        assert_eq!(ellipses, 2);
    }

    #[test]
    fn atomic_inlines_at_the_end_edge_are_hidden() {
        let (dom, layout) = layout(&format!(
            "{STYLE}<div class=t>abcdefgh<span id=a style='display:inline-block;width:60px;\
             height:10px;background:red'></span></div>"
        ));
        let atom = by_id(&dom, "a");
        // Geometry is retained.
        assert_eq!(layout.boxes[&atom].width, 60.0);
        // Nothing of the elided box paints.
        let red = layout.paint.primitives.iter().any(|command| {
            matches!(
                command,
                DisplayCommand::FillRect {
                    color: crate::render::PaintColor::Rgba(255, 0, 0, _),
                    ..
                }
            )
        });
        assert!(!red, "the elided inline-block must not paint");
        let runs = runs(&layout);
        let text = runs.iter().find(|(text, ..)| text == "abcdefgh").unwrap();
        let ellipsis = runs.iter().find(|(text, ..)| text == "…").unwrap();
        assert!((ellipsis.1 - (text.1 + text.2)).abs() < 0.5, "{runs:?}");
    }

    #[test]
    fn the_first_character_is_clipped_rather_than_ellipsed() {
        let (_, layout) = layout(
            "<style>body{margin:0;font:16px/20px monospace}\
             .t{width:4px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}</style>\
             <div class=t>abc</div>",
        );
        let runs = runs(&layout);
        assert!(runs.iter().any(|(text, ..)| text == "a"), "{runs:?}");
        assert!(runs.iter().any(|(text, ..)| text == "…"), "{runs:?}");
    }

    #[test]
    fn the_ellipsis_is_styled_by_the_block_and_hits_the_elided_element() {
        let (dom, layout) = layout(&format!(
            "{STYLE}<div class=t id=d style='color:rgb(0,128,0)'>abc \
             <a id=l href='/x' style='color:rgb(0,0,255)'>defghijklmnopqrstuvwxyz</a></div>"
        ));
        let link = by_id(&dom, "l");
        let (color, node) = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                DisplayCommand::GlyphRun {
                    shaped,
                    color,
                    node,
                    ..
                } if shaped.text == "…" => Some((*color, *node)),
                _ => None,
            })
            .expect("ellipsis");
        assert_eq!(color, crate::render::PaintColor::Rgba(0, 128, 0, 255));
        assert_eq!(node, link);
    }

    #[test]
    fn flex_items_and_table_cells_ellipse() {
        for html in [
            "<div style='display:flex;width:100px'><span class=t style='width:auto;\
             min-width:0'>abcdefghijklmnopqrstuvwxyz</span><b>x</b></div>",
            "<table style='table-layout:fixed;width:100px;border-spacing:0'><tr>\
             <td class=t style='padding:0'>abcdefghijklmnopqrstuvwxyz</td></tr></table>",
        ] {
            let (_, layout) = layout(&format!("{STYLE}{html}"));
            let runs = runs(&layout);
            let ellipsis = runs.iter().find(|(text, ..)| text == "…");
            assert!(ellipsis.is_some(), "{html}: {runs:?}");
            assert!(ellipsis.unwrap().1 < 100.0, "{html}: {runs:?}");
        }
    }

    #[test]
    fn right_to_left_lines_overflow_and_ellipse_at_the_left() {
        // CSS Text 3 #text-align-property: an overflowing line is
        // start-aligned, so a right-to-left line overflows to the left, the
        // end edge, where the ellipsis replaces what no longer fits.
        let (_, layout) = layout(&format!(
            "{STYLE}<div class=t dir=rtl>abcdefghijklmnopqrstuvwxyz</div>"
        ));
        let runs = runs(&layout);
        let ellipsis = runs
            .iter()
            .find(|(text, ..)| text == "…")
            .expect("an ellipsis paints");
        let text = runs
            .iter()
            .find(|(text, ..)| text.ends_with("xyz"))
            .expect("the start of the line remains");
        assert!(text.0.len() < 26, "characters are hidden: {}", text.0);
        // Immediately before the remaining content, inside the line box,
        // with no room for one more character.
        assert!((ellipsis.1 + ellipsis.2 - text.1).abs() < 0.5, "{runs:?}");
        assert!(ellipsis.1 >= -0.5, "{runs:?}");
        assert!(text.1 - ellipsis.2 * 2.0 < 0.0, "{runs:?}");
        assert!((text.1 + text.2 - 100.0).abs() < 0.5, "{runs:?}");
        // Without text-overflow the overflowing line still starts at the
        // right edge.
        let (_, clipped) = super::tests::layout(&format!(
            "{STYLE}<div class=t dir=rtl style='text-overflow:clip'>abcdefghijklmnopqrstuvwxyz</div>"
        ));
        let runs = super::tests::runs(&clipped);
        let whole = runs
            .iter()
            .find(|(text, ..)| text.ends_with("xyz"))
            .unwrap();
        assert!((whole.1 + whole.2 - 100.0).abs() < 0.5, "{runs:?}");
        assert!(whole.1 < 0.0, "{runs:?}");
    }

    #[test]
    fn terminal_layout_paints_the_ellipsis_cell() {
        let dom = Dom::parse_document(
            "<style>.t{width:10ch;white-space:nowrap;overflow:hidden;\
             text-overflow:ellipsis}</style><div class=t>abcdefghijklmnop</div>",
        );
        let doc = crate::layout2::lay_out_document(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            crate::layout2::TerminalViewport::new(80, 24, 8.0, 16.0),
            &[],
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );
        let text: String = doc
            .rows
            .iter()
            .flat_map(|row| row.items.iter().map(|item| item.text.clone()))
            .collect();
        assert!(text.contains("abcdefghi…"), "{text:?}");
    }

    #[test]
    fn terminal_layout_ellipses_right_to_left_lines_at_the_left() {
        let dom = Dom::parse_document(
            "<style>.t{width:10ch;white-space:nowrap;overflow:hidden;\
             text-overflow:ellipsis}</style><div class=t dir=rtl>abcdefghijklmnop</div>",
        );
        let doc = crate::layout2::lay_out_document(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            crate::layout2::TerminalViewport::new(80, 24, 8.0, 16.0),
            &[],
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );
        let text: String = doc
            .rows
            .iter()
            .flat_map(|row| row.items.iter().map(|item| item.text.clone()))
            .collect();
        assert!(text.contains("…hijklmnop"), "{text:?}");
    }
}
