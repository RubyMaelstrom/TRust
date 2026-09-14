//! CSS Overflow 4 #webkit-line-clamp / #line-clamp-containers.
//!
//! Count in-flow lines in one block formatting context. Retain invisible
//! fragments for CSSOM geometry, but exclude them from automatic block sizes,
//! painting, hit testing and scrollable overflow. Independent formatting
//! contexts do not spend their ancestor's line budget.

use super::flow::{Frag, FragKind};
use super::style::InlineStyle;
use crate::dom::Dom;
use crate::layout2::NO_NODE;
use url::Url;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct FlowInfo {
    pub independent: bool,
    pub inline_box: bool,
    pub hidden: bool,
    pub clamp_container: bool,
    /// Visible floats in the clamp BFC are clipped at this content edge,
    /// relative to their own border-box top, and contribute only ink overflow.
    pub float_clip_end: Option<f32>,
    /// Content-height limits for an automatically sized block.
    pub auto_height: Option<[f32; 2]>,
    pub margin_bottom: f32,
    /// The box's own relative/transform translation, excluded from flow size.
    pub offset_y: f32,
}

struct Budget {
    remaining: usize,
    clamped: bool,
}

/// Returns the shortened content bottom only when a clamp point exists.
pub(super) fn apply(
    children: &mut [Frag<'_>],
    max_lines: usize,
    top: f32,
    dom: &Dom,
    base: &Url,
    inl: &InlineStyle,
) -> Option<f32> {
    let mut budget = Budget {
        remaining: max_lines,
        clamped: false,
    };
    let bottom = walk(children, top, &mut budget);
    if !budget.clamped {
        return None;
    }
    ellipsis(children, dom, base, inl);
    Some(bottom)
}

fn walk(children: &mut [Frag<'_>], top: f32, budget: &mut Budget) -> f32 {
    let mut bottom = top;
    for fragment in children.iter_mut() {
        if matches!(fragment.kind, FragKind::Oof(..) | FragKind::Fixed(_))
            || fragment.paint.outside_marker
            || fragment.paint.float
            || fragment.flow.inline_box
        {
            continue;
        }
        if fragment.flow.hidden {
            continue;
        }
        if budget.remaining == 0 {
            budget.clamped = true;
            fragment.flow.hidden = true;
            continue;
        }
        match &fragment.kind {
            FragKind::Line(_) => budget.remaining -= 1,
            _ if !fragment.flow.independent => {
                let content_top = fragment.y + fragment.content_offset[1];
                let end = walk(&mut fragment.children, content_top, budget);
                if budget.clamped
                    && let Some([min, max]) = fragment.flow.auto_height
                {
                    let old = fragment.content_size.map_or(0.0, |size| size[1]);
                    let height = (end - content_top).max(0.0).clamp(min, max.max(min));
                    let difference = old - height;
                    fragment.h -= difference;
                    if let Some(size) = &mut fragment.content_size {
                        size[1] = height;
                    }
                    if let Some(size) = &mut fragment.css_size {
                        size[1] -= difference;
                    }
                }
            }
            _ => {}
        }
        bottom = bottom
            .max(fragment.y - fragment.flow.offset_y + fragment.h + fragment.flow.margin_bottom);
    }
    if budget.clamped {
        // Atomic inline boxes and floats are appended after their line
        // fragments by the IFC adapter. Their line position, not that storage
        // order, determines whether they follow the clamp point.
        for fragment in children {
            if (fragment.flow.inline_box || fragment.paint.float)
                && fragment.y - fragment.flow.offset_y >= bottom
            {
                fragment.flow.hidden = true;
            }
        }
    }
    bottom
}

fn ellipsis(children: &mut [Frag<'_>], dom: &Dom, base: &Url, inl: &InlineStyle) -> bool {
    for index in (0..children.len()).rev() {
        let fragment = &mut children[index];
        if fragment.flow.hidden
            || fragment.flow.independent
            || fragment.paint.float
            || fragment.flow.inline_box
            || fragment.paint.outside_marker
        {
            continue;
        }
        if let FragKind::Line(line) = &mut fragment.kind {
            let old_atoms = line.atom_boxes.clone();
            super::inline::block_ellipsis(line, dom, base, inl);
            fragment.w = line.width;
            let atoms = line.atom_boxes.clone();
            for fragment in children.iter_mut().filter(|f| f.flow.inline_box) {
                let Some(old) = old_atoms.iter().find(|p| p.item.node == fragment.node) else {
                    continue;
                };
                if let Some(new) = atoms.iter().find(|p| p.item.node == fragment.node) {
                    super::flow::Flow::offset_frag(fragment, new.x - old.x, new.y - old.y);
                } else {
                    fragment.flow.hidden = true;
                }
            }
            return true;
        }
        let context = if fragment.node == NO_NODE {
            inl.with_pseudo(dom, fragment.paint.pseudo)
        } else {
            InlineStyle::derive(dom, fragment.node, inl, base)
        };
        if ellipsis(&mut fragment.children, dom, base, &context) {
            return true;
        }
    }
    false
}

pub(super) fn clip_floats(children: &mut [Frag<'_>], bottom: f32) {
    for fragment in children {
        if fragment.paint.float {
            fragment.flow.float_clip_end = Some(bottom - fragment.y);
        } else if !fragment.flow.independent {
            clip_floats(&mut fragment.children, bottom);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout2::{Viewport, lay_out_graphical};
    use crate::render::DisplayCommand;

    fn render(content: &str, extra: &str) -> (Dom, crate::layout2::GraphicalLayout) {
        let html = format!(
            r#"<style>body{{margin:0;font:16px/20px monospace}}
            #clamp{{display:-webkit-box;-webkit-box-orient:vertical;-webkit-line-clamp:2;width:120px;{extra}}}
            p{{margin:0}}</style><div id=clamp>{content}</div><div id=after>After</div>"#
        );
        let dom = Dom::parse_document(&html);
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

    fn height(dom: &Dom, layout: &crate::layout2::GraphicalLayout, id: &str) -> f64 {
        let node = dom
            .descendants(crate::dom::DOCUMENT)
            .find(|&node| dom.attr(node, "id") == Some(id))
            .unwrap();
        layout.boxes[&node].height
    }

    fn text(layout: &crate::layout2::GraphicalLayout) -> String {
        layout
            .paint
            .primitives
            .iter()
            .filter_map(|command| match command {
                DisplayCommand::GlyphRun { shaped, .. } => Some(shaped.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn legacy_line_clamp_limits_auto_height_and_paints_ellipsis() {
        let (dom, layout) = render(
            "one two three four five six seven eight nine",
            "padding:5px",
        );
        assert_eq!(height(&dom, &layout, "clamp"), 50.0);
        let text = text(&layout);
        assert!(text.contains('…'), "{text}");
        assert!(!text.contains("nine"), "{text}");
        let node = dom
            .descendants(crate::dom::DOCUMENT)
            .find(|&n| dom.attr(n, "id") == Some("after"))
            .unwrap();
        assert_eq!(layout.boxes[&node].top, 50.0);
    }

    #[test]
    fn legacy_line_clamp_needs_more_content_and_the_legacy_display_combination() {
        for (content, extra, expected, ellipsis) in [
            ("one<br>two", "", 40.0, false),
            ("one<br>two<br>three", "", 40.0, true),
            ("one<br>two<br>three", "display:block", 60.0, false),
            (
                "one<br>two<br>three",
                "-webkit-box-orient:horizontal",
                60.0,
                false,
            ),
            (
                "one<br>two<br>three",
                "-webkit-line-clamp:none",
                60.0,
                false,
            ),
            (
                "one<br>two<br>three",
                "display:-webkit-inline-box",
                40.0,
                true,
            ),
            (
                "one<br>two<br>three",
                "-webkit-line-clamp:none;display:block",
                60.0,
                false,
            ),
            ("one<br>two<br>three", "height:100px", 100.0, true),
        ] {
            let (dom, layout) = render(content, extra);
            assert_eq!(
                height(&dom, &layout, "clamp"),
                expected,
                "{content}, {extra}"
            );
            assert_eq!(
                text(&layout).contains('…'),
                ellipsis,
                "{content}, {extra}: {}",
                text(&layout)
            );
        }
    }

    #[test]
    fn legacy_line_clamp_counts_nested_blocks_but_skips_independent_contexts() {
        let (dom, layout) = render(
            "<div style='display:flow-root'>independent<br>context</div><p id=inner>one<br><b>two</b><br>hidden</p><p>also hidden</p>",
            "",
        );
        assert_eq!(height(&dom, &layout, "clamp"), 80.0);
        assert_eq!(height(&dom, &layout, "inner"), 40.0);
        let text = text(&layout);
        assert!(text.contains("independent") && text.contains('…'), "{text}");
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn legacy_line_clamp_preserves_geometry_but_excludes_hidden_hits_and_scroll_overflow() {
        let (dom, layout) = render(
            "one<br>two<br><a id=hidden href='/hidden'>hidden</a>",
            "overflow:auto",
        );
        let id = |name| {
            dom.descendants(crate::dom::DOCUMENT)
                .find(|&n| dom.attr(n, "id") == Some(name))
                .unwrap()
        };
        assert!(layout.boxes.contains_key(&id("hidden")));
        let (_, _, scrolling) = crate::layout2::measure_boxes_css(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            Viewport::new(640.0, 480.0),
            &[],
            &Default::default(),
            &Default::default(),
        );
        assert_eq!(scrolling[&id("clamp")].height, 40.0);
        assert!(!layout.paint.primitives.iter().any(|p| matches!(p,
            DisplayCommand::HitRegion(hit) if hit.node == id("hidden"))));
    }

    #[test]
    fn legacy_line_clamp_ellipsis_uses_root_style_and_realigns_the_shortened_line() {
        for align in ["left", "center", "right", "justify"] {
            let (_, layout) = render(
                "one<br><b>two three four</b><br>hidden",
                &format!("text-align:{align}"),
            );
            let runs: Vec<_> = layout
                .paint
                .primitives
                .iter()
                .filter_map(|p| match p {
                    DisplayCommand::GlyphRun { origin, shaped, .. } if origin.y < 40.0 => {
                        Some((origin, shaped))
                    }
                    _ => None,
                })
                .collect();
            let (_, ellipsis) = runs.iter().find(|(_, s)| s.text == "…").unwrap();
            assert_eq!(ellipsis.line_height, 20.0);
            assert!(
                runs.iter().all(|(p, s)| p.x + s.advance <= 120.1),
                "{align}: {runs:?}"
            );
            assert!(!text(&layout).contains("hidden"));
        }
    }

    #[test]
    fn legacy_line_clamp_float_does_not_enlarge_the_container() {
        let (dom, layout) = render(
            "<div style='float:left;width:20px;height:150px;background:red'></div>one<br>two<br>hidden",
            "",
        );
        assert_eq!(height(&dom, &layout, "clamp"), 40.0);
        assert!(!text(&layout).contains("hidden"));
    }

    #[test]
    fn legacy_line_clamp_ellipsis_accounts_for_atomic_inline_boxes() {
        for (width, visible) in [(60, true), (115, false)] {
            let (_, layout) = render(
                &format!(
                    "one<br><span style='display:inline-block;width:{width}px;height:20px'>badge</span><br>hidden"
                ),
                "",
            );
            assert_eq!(
                text(&layout).contains("badge"),
                visible,
                "{}",
                text(&layout)
            );
            let x = layout
                .paint
                .primitives
                .iter()
                .find_map(|p| match p {
                    DisplayCommand::GlyphRun { origin, shaped, .. } if shaped.text == "…" => {
                        Some(origin.x)
                    }
                    _ => None,
                })
                .unwrap();
            assert!((x - if visible { width as f32 } else { 0.0 }).abs() < 0.1);
        }
    }

    #[test]
    fn legacy_line_clamp_hides_positioned_content_by_containing_block() {
        for (position, visible) in [("static", true), ("relative", false)] {
            let (_, layout) = render(
                &format!(
                    "<p>one<br>two</p><div style='position:{position}'><a style='position:absolute;top:0'>escaped</a></div>"
                ),
                "position:relative",
            );
            assert_eq!(
                text(&layout).contains("escaped"),
                visible,
                "{position}: {}",
                text(&layout)
            );
        }
    }

    #[test]
    fn legacy_line_clamp_cascade_and_live_snapshot_keep_the_same_behavior() {
        for invalid in ["0", "-1", "2.5", "two", "2 3"] {
            let (dom, _) = render(
                "one<br>two<br>hidden",
                &format!("-webkit-line-clamp:{invalid}"),
            );
            let snapshot = dom.serialize_live(crate::dom::DOCUMENT, &Default::default());
            let snapshot = Dom::parse_document(&snapshot);
            for document in [&dom, &snapshot] {
                let node = document
                    .descendants(crate::dom::DOCUMENT)
                    .find(|&n| document.attr(n, "id") == Some("clamp"))
                    .unwrap();
                assert_eq!(
                    document.cssom_resolved_value(node, "display").as_deref(),
                    Some("flow-root")
                );
                assert_eq!(document.legacy_line_clamp(node), Some(2));
                let base = Url::parse("https://example.test/").unwrap();
                let rendered = crate::http::render_arena(
                    document,
                    &base,
                    Viewport::new(640.0, 480.0),
                    1.0,
                    None,
                    &Default::default(),
                );
                assert_eq!(rendered.layout.boxes[&node].height, 40.0);
                let terminal = crate::layout2::adapt_terminal(
                    &rendered.layout,
                    crate::layout2::TerminalViewport::new(80, 30, 8.0, 16.0),
                    &Default::default(),
                );
                let text: String = terminal
                    .rows
                    .iter()
                    .flat_map(|row| row.items.iter())
                    .map(|item| item.text.as_str())
                    .collect();
                assert!(text.contains('…') && !text.contains("hidden"), "{text}");
            }
        }
    }

    #[test]
    fn percentage_image_leaves_room_for_minimum_text_column_in_grid_and_flex() {
        for display in ["flex", "grid"] {
            let html = r#"<style>
            * { box-sizing:border-box } body { margin:0 }
            #slide { display:MODE; grid-template-columns:1fr .25fr; width:908px }
            #photo { display:inline-block; width:100%; max-width:100%; margin-bottom:20px }
            img { width:100%; height:100%; max-width:100%; margin-top:-17px; vertical-align:middle }
            #info { display:flex; flex:1; flex-direction:column; min-width:250px; padding-left:22px }
            h2 { width:250px; font:42px/48px sans-serif; margin:10px 0;
                display:-webkit-box; -webkit-box-orient:vertical; -webkit-line-clamp:2 }
            </style><div id=slide><a id=photo><img id=image src=photo.jpg></a><div id=info>
            <h2>Little Pawprints Cat Cafe</h2><p>Little cafe with cute cats to keep you company.</p>
            </div></div>"#.replace("MODE",display);
            let dom = Dom::parse_document(&html);
            let base = Url::parse("https://example.test/").unwrap();
            let images = [("https://example.test/photo.jpg".into(), (1200, 720))]
                .into_iter()
                .collect();
            let layout = lay_out_graphical(
                &dom,
                &base,
                Viewport::new(958.0, 1024.0),
                &[],
                &Default::default(),
                &images,
            );
            let node = |id| {
                dom.descendants(crate::dom::DOCUMENT)
                    .find(|&n| dom.attr(n, "id") == Some(id))
                    .unwrap()
            };
            let photo = &layout.boxes[&node("photo")];
            let info = &layout.boxes[&node("info")];
            assert!((photo.width - 658.0).abs() < 0.1, "{photo:?}, {info:?}");
            assert!((info.width - 250.0).abs() < 0.1, "{photo:?}, {info:?}");
            // CSS 2 #line-height: the initial intrinsic image contributes
            // its margin box, and the negative margin also moves its ink.
            let image = &layout.boxes[&node("image")];
            assert!((photo.height - 377.8).abs() < 0.1, "{display}: {photo:?}");
            assert!((image.top + 17.0).abs() < 0.1, "{display}: {image:?}");
            // A grid area supplies a definite percentage basis. Flexbox
            // #algo-stretch only repeats layout when the cross size changes;
            // this auto-height flex line keeps its original intrinsic image.
            let image_height = if display == "grid" {
                photo.height
            } else {
                394.8
            };
            assert!(
                (image.height - image_height).abs() < 0.1,
                "{display}: {image:?}, {photo:?}"
            );
            let slide = &layout.boxes[&node("slide")];
            assert!((slide.height - 397.8).abs() < 0.1, "{display}: {slide:?}");
        }
    }
}
