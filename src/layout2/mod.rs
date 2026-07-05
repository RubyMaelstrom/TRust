//! layout2 — the NEW layout engine (LAYOUT_OVERHAUL_PLAN.md).
//!
//! The standard architecture, replacing layout.rs's single-pass text flow:
//!
//! ```text
//! styled DOM (dom.rs cascade, KEPT)
//!   → 1. BOX TREE  (tree.rs)   display/formatting context decided once,
//!                              anonymous boxes per CSS 2.1 §9.2; out-of-flow
//!                              boxes routed to static-position marks
//!   → 2. FRAGMENTS (flow.rs)   used geometry in f32 CSS px: widths top-down
//!                              (§10.3.3), real §8.3.1 margin collapsing,
//!                              heights bottom-up; inline.rs builds line
//!                              boxes (CSS Text collapsing/wrapping/align);
//!                              the positioned post-pass lays abspos/fixed
//!                              boxes against their containing blocks'
//!                              FINAL geometry (§10.1/§10.3.7/§10.6.4)
//!   → 3. PAINT     (paint.rs)  CSS 2.1 Appendix E painting order + a cell-
//!                              granularity compositor (overlaps allowed,
//!                              later paint wins); the ONE px→cell
//!                              quantization (edges snap) → the existing
//!                              `Doc.rows`/`Item` contract + the pinned
//!                              `FixedItem` layer, consumers unchanged
//! ```
//!
//! P0 = the skeleton: block flow, inline text, images, form-control atoms,
//! lists, real UA-stylesheet margins. Flex (P2), grid (P3), positioned/
//! stacking/paint-order/transform-translate (P4), overflow (P5), and tables
//! (P6, the CSS 2.1 §17 model in `table.rs`) are real.
//! P5 splits by CSS Overflow L3 §2: `hidden`/`clip` are a pure CLIP to the
//! padding box (P5a — sr-only boxes clip to nothing, definite-height panels
//! clip their overflow); `auto`/`scroll` are SCROLL CONTAINERS whose overflow
//! rides the scroll axis into a windowed buffer (a vertical Region, P5b, with
//! the principal-scroller rule per §3.1) or an inline strip (a horizontal
//! Carousel, P5c). Floats still degrade to honest block stacking — a staged
//! phase, never policy. Behind `set layout2 on` /
//! `TRUST_LAYOUT2=1` until parity (the plan's A/B gate); incremental-layout
//! boundaries are intentionally not emitted yet, so live pages take the
//! always-correct full-relayout path.

mod flex;
mod flow;
mod grid;
mod inline;
mod intrinsic;
mod paint;
mod replaced;
mod style;
mod table;
mod tree;
mod value;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use crate::doc::Form;
use crate::dom::Dom;
use crate::layout::{
    Carousel, ControlMap, FixedItem, ImageSizes, Region, Row, cell_px_h, cell_px_w,
};

use flow::Flow;
use value::Vp;

/// Session-global engine switch (`set layout2 on|off`), seeded once from
/// `TRUST_LAYOUT2` so test harnesses (`net_diag`, `layout_dump`) can A/B
/// without a UI. Same pattern as `layout::BORDERS_ENABLED`.
static ENABLED: AtomicBool = AtomicBool::new(false);

fn env_seed() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if std::env::var_os("TRUST_LAYOUT2").is_some_and(|v| v != "0") {
            ENABLED.store(true, Ordering::Relaxed);
        }
    });
}

pub fn enabled() -> bool {
    env_seed();
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(on: bool) {
    env_seed(); // consume the env seed so it can't clobber an explicit set
    ENABLED.store(on, Ordering::Relaxed);
}

/// What the engine hands back to `http::parse_seeded`. Carousels, regions,
/// scroll clips, and boundaries arrive with their phases — consumers treat
/// the empty collections as "none on this page", and the live-page patch
/// machinery falls back to full relayout (always correct).
pub struct Output {
    pub rows: Vec<Row>,
    pub anchor_rows: HashMap<String, usize>,
    /// The pinned `position:fixed` layer (P4), in stack-level order.
    pub fixed: Vec<FixedItem>,
    /// Vertical inner-scroll viewports (P5b): each windows its own buffer over
    /// a reserved band of blank doc rows.
    pub regions: Vec<Region>,
    /// Horizontal scroll strips (P5c): items stay inline in the doc rows,
    /// column-shifted/clipped to the band at render.
    pub carousels: Vec<Carousel>,
    /// `(live actor node, clientHeight rows, scrollport width cells)` per scroll
    /// region, for the app's per-element scroll-geometry push (CSSOM View).
    pub scroll_clips: Vec<(usize, u16, u16)>,
}

/// Lay an HTML document out at `viewport` = (cols, rows) — the terminal
/// content area. Signature mirrors `layout::lay_out_with_carousels`.
pub fn lay_out_document(
    dom: &Dom,
    base: &Url,
    viewport: (usize, usize),
    forms: &[Form],
    controls: &ControlMap,
    images: &ImageSizes,
) -> Output {
    let cols = viewport.0.max(10);
    let cell_w = f32::from(cell_px_w());
    let cell_h = f32::from(cell_px_h());
    let vp = Vp {
        w: cols as f32 * cell_w,
        h: viewport.1 as f32 * cell_h, // 0 when unknown — vh stays unresolved
    };
    let Some(root) = tree::build(dom, base, controls, forms, vp) else {
        return Output {
            rows: Vec::new(),
            anchor_rows: HashMap::new(),
            fixed: Vec::new(),
            regions: Vec::new(),
            carousels: Vec::new(),
            scroll_clips: Vec::new(),
        };
    };
    let flow = Flow {
        dom,
        base,
        forms,
        images,
        vp,
        cell_w,
        cell_h,
        imemo: Default::default(),
    };
    let (mut frag, flow_bottom, anchors, fixed) = flow.layout(&root);
    let mut out = paint::paint(
        dom,
        &mut frag,
        &fixed,
        flow_bottom,
        &anchors,
        (cols, viewport.1),
        cell_w,
        cell_h,
    );
    page_media_fallback(dom, base, images, cols, &mut out.rows);
    Output {
        rows: out.rows,
        anchor_rows: out.anchor_rows,
        fixed: out.fixed,
        regions: out.regions,
        carousels: out.carousels,
        scroll_clips: out.scroll_clips,
    }
}

/// The page-level media affordance: a page that declares itself a video page
/// (Open Graph `og:video` — `page_declares_video`) but mounts NO
/// `<video>`/`<audio>` element still gets a "play in mpv" representation,
/// because yt-dlp can resolve the page itself (the Twitch watch-page fix —
/// the player never mounts without MSE). The page's og:image preview IS the
/// link once decoded; the text affordance stands in until then.
fn page_media_fallback(
    dom: &Dom,
    base: &Url,
    images: &ImageSizes,
    cols: usize,
    rows: &mut Vec<Row>,
) {
    use crate::doc::Link;
    use crate::layout::{Emphasis, Item, ItemKind, NO_NODE, display_width};
    if dom
        .descendants(crate::dom::DOCUMENT)
        .any(|id| matches!(dom.tag_name(id), Some("video" | "audio")))
        || !crate::layout::page_declares_video(dom)
    {
        return;
    }
    let link = Some(Link::Media(base.clone()));
    let poster = crate::layout::page_preview_image(dom, base)
        .and_then(|p| images.get(&p).map(|&(w, h)| (p, w, h)))
        .filter(|&(_, w, h)| w > 0 && h > 0);
    let item = match poster {
        Some((url, iw, ih)) => {
            let w = (iw as usize).min(cols).max(1) as u16;
            let h = ((u32::from(ih) * u32::from(w)) / u32::from(iw)).max(1) as u16;
            Item {
                col: 0,
                width: w,
                height: h,
                text: String::new(),
                kind: ItemKind::Image,
                image: Some(url),
                emph: Emphasis::default(),
                node: NO_NODE,
                link,
                crop: false,
                pixelated: false,
                invisible: false,
            }
        }
        None => Item {
            col: 0,
            width: display_width("▶ Watch in mpv") as u16,
            height: 1,
            text: String::from("▶ Watch in mpv"),
            kind: ItemKind::Link,
            image: None,
            emph: Emphasis::default(),
            node: NO_NODE,
            link,
            crop: false,
            pixelated: false,
            invisible: false,
        },
    };
    let extra = usize::from(item.height.max(1)) - 1;
    rows.push(Row { items: vec![item] });
    for _ in 0..extra {
        rows.push(Row::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Item, ItemKind, display_width};

    fn lay(html: &str, cols: usize) -> Output {
        lay_images(html, cols, &HashMap::new())
    }

    fn lay_images(html: &str, cols: usize, images: &ImageSizes) -> Output {
        let dom = Dom::parse_document(html);
        let base = Url::parse("http://e.com/").unwrap();
        lay_out_document(&dom, &base, (cols, 24), &[], &HashMap::new(), images)
    }

    /// A row's text with items placed at their columns (gaps = spaces).
    fn row_text(row: &Row) -> String {
        let mut out = String::new();
        let mut col = 0usize;
        for it in &row.items {
            while col < it.col as usize {
                out.push(' ');
                col += 1;
            }
            out.push_str(&it.text);
            col = it.col as usize + display_width(&it.text);
        }
        out
    }

    fn find<'a>(out: &'a Output, text: &str) -> (usize, &'a Item) {
        for (r, row) in out.rows.iter().enumerate() {
            for it in &row.items {
                if it.text.contains(text) {
                    return (r, it);
                }
            }
        }
        panic!("item containing {text:?} not found");
    }

    /// No painted item anywhere contains `text` (clipped away entirely).
    fn absent(out: &Output, text: &str) -> bool {
        !out.rows
            .iter()
            .any(|r| r.items.iter().any(|i| i.text.contains(text)))
    }

    // ---- the P0 gate: plain articles render with a browser's structure ----
    // Test cells are the nominal 8×16 px, so 1em = 16px = 1 row = 2 cols and
    // the UA sheet's px values quantize predictably: body margin 8px = 1 col,
    // list gutter 40px = 5 cols.

    #[test]
    fn article_structure_matches_browser() {
        let out = lay(
            "<body><h1>Title</h1><p>One two three.</p><p>Second para.</p></body>",
            80,
        );
        // body's 8px top margin collapses with h1's 0.67em·32px = 21.44px
        // margin → h1's line at y=21.44px → row 1; its left content edge is
        // body's 8px margin → col 1.
        let (r1, h1) = find(&out, "Title");
        assert_eq!((r1, h1.col), (1, 1));
        assert_eq!(h1.kind, ItemKind::Heading(1));
        // h1 bottom 37.44 + collapsed max(21.44, 16) → p at 58.88px → row 4.
        let (r2, p1) = find(&out, "One two three.");
        assert_eq!((r2, p1.col), (4, 1));
        assert_eq!(p1.kind, ItemKind::Text);
        // p↕p: exactly one collapsed 1em margin → one blank row between.
        let (r3, _) = find(&out, "Second para.");
        assert_eq!(r3, r2 + 2, "adjacent paragraphs collapse to one 1em gap");
        assert!(out.rows[r2 + 1].items.is_empty());
    }

    #[test]
    fn paragraph_wraps_at_content_width() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">aaa bbb ccc ddd</p></body>"#,
            10,
        );
        assert_eq!(row_text(&out.rows[0]), "aaa bbb");
        assert_eq!(row_text(&out.rows[1]), "ccc ddd");
    }

    #[test]
    fn sibling_margins_collapse_to_max() {
        let out = lay(
            r#"<body style="margin:0"><div style="margin-bottom:32px">a</div><div style="margin-top:16px">b</div></body>"#,
            20,
        );
        let (ra, _) = find(&out, "a");
        let (rb, _) = find(&out, "b");
        assert_eq!(ra, 0);
        assert_eq!(rb, 3, "gap = max(32px, 16px) = 2 rows");
    }

    #[test]
    fn parent_child_top_margins_collapse_through() {
        let out = lay(
            r#"<body style="margin:0"><div style="margin-top:32px"><p style="margin-top:16px;margin-bottom:0">x</p></div></body>"#,
            20,
        );
        let (r, _) = find(&out, "x");
        assert_eq!(r, 2, "joint margin = max(32, 16) = 32px = 2 rows");
    }

    #[test]
    fn empty_block_self_collapses() {
        let out = lay(
            r#"<body style="margin:0"><div>a</div><div style="margin-top:16px;margin-bottom:16px"></div><div>b</div></body>"#,
            20,
        );
        let (ra, _) = find(&out, "a");
        let (rb, _) = find(&out, "b");
        assert_eq!(
            rb - ra,
            2,
            "empty div's margins collapse into one 1-row gap"
        );
    }

    #[test]
    fn width_auto_margins_center_and_padding_indents() {
        // 80 cols = 640px CB; content width 50% = 320px, border box 336px
        // with the 16px padding; §10.3.3: ml = (640−336)/2 = 152px; content
        // at 152+16 = 168px = col 21.
        let out = lay(
            r#"<body style="margin:0"><div style="width:50%;margin:0 auto;padding-left:16px">x</div></body>"#,
            80,
        );
        let (_, it) = find(&out, "x");
        assert_eq!(it.col, 21);
    }

    #[test]
    fn box_sizing_border_box_shrinks_content() {
        // width:320px border-box with 16px padding each side → content 288px
        // = 36 cells; text wraps there, not at 40.
        let out = lay(
            r#"<body style="margin:0"><div style="box-sizing:border-box;width:320px;padding:0 16px;margin:0">
               <p style="margin:0">aaaa</p></div></body>"#,
            80,
        );
        let (_, it) = find(&out, "aaaa");
        assert_eq!(it.col, 2, "16px padding-left = 2 cols");
    }

    #[test]
    fn nested_lists_indent_and_change_markers() {
        let out = lay(
            r#"<body style="margin:0"><ul style="margin:0"><li>one</li><li>two<ul><li>sub</li></ul></li></ul></body>"#,
            40,
        );
        let (r1, one) = find(&out, "one");
        assert_eq!((r1, one.col), (0, 5), "40px list gutter = 5 cols");
        let marker = &out.rows[0].items[0];
        assert_eq!(marker.text, "• ");
        assert_eq!(marker.col, 3, "marker right-aligned against content");
        let (r3, sub) = find(&out, "sub");
        assert_eq!(sub.col, 10, "nested list adds another 40px gutter");
        let sub_marker = &out.rows[r3].items[0];
        assert_eq!(sub_marker.text, "◦ ", "depth-2 UA marker is circle");
    }

    #[test]
    fn ordered_list_counts_with_start_and_value() {
        let out = lay(
            r#"<body style="margin:0"><ol start="3" style="margin:0"><li>a</li><li value="10">b</li><li>c</li></ol></body>"#,
            40,
        );
        let (ra, _) = find(&out, "a");
        assert_eq!(out.rows[ra].items[0].text, "3. ");
        let (rb, _) = find(&out, "b");
        assert_eq!(out.rows[rb].items[0].text, "10. ");
        let (rc, _) = find(&out, "c");
        assert_eq!(out.rows[rc].items[0].text, "11. ");
    }

    #[test]
    fn blockquote_indents_both_sides() {
        let out = lay(
            r#"<body style="margin:0"><blockquote style="margin-top:0">quoted text</blockquote></body>"#,
            80,
        );
        let (_, it) = find(&out, "quoted text");
        assert_eq!(it.col, 5, "40px UA margin-left = 5 cols");
        assert_eq!(it.kind, ItemKind::Quote);
    }

    #[test]
    fn pre_preserves_spaces_newlines_and_tabs() {
        let out = lay(
            "<body style=\"margin:0\"><pre style=\"margin:0\">a  b\n\tc</pre></body>",
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "a  b");
        assert_eq!(row_text(&out.rows[1]), "        c", "tab → 8-cell stop");
        let (_, it) = find(&out, "a  b");
        assert_eq!(it.kind, ItemKind::Pre);
    }

    #[test]
    fn br_forces_breaks_and_blank_lines() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">a<br>b<br><br>c</p></body>"#,
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "a");
        assert_eq!(row_text(&out.rows[1]), "b");
        assert!(out.rows[2].items.is_empty(), "<br><br> yields a blank line");
        assert_eq!(row_text(&out.rows[3]), "c");
    }

    #[test]
    fn links_and_emphasis_thread_into_items() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">go <a href="/x">here <b>now</b></a></p></body>"#,
            40,
        );
        let (_, here) = find(&out, "here");
        assert_eq!(here.kind, ItemKind::Link);
        assert!(matches!(&here.link, Some(crate::doc::Link::Http(u)) if u.path() == "/x"));
        let (_, now) = find(&out, "now");
        assert!(now.emph.bold);
        assert_eq!(now.kind, ItemKind::Link);
        assert!(now.link.is_some(), "emphasis inside a link keeps the link");
    }

    #[test]
    fn collapsing_spans_inline_boundaries() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">a <b> b</b></p></body>"#,
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "a b", "one collapsed space");
    }

    #[test]
    fn decoded_image_reserves_box_and_text_sits_on_baseline() {
        let mut images = HashMap::new();
        images.insert("http://e.com/i.png".to_string(), (10u16, 3u16));
        let out = lay_images(
            r#"<body style="margin:0"><p style="margin:0"><img src="i.png" alt="pic">after</p></body>"#,
            40,
            &images,
        );
        let img = out.rows[0]
            .items
            .iter()
            .find(|it| it.kind == ItemKind::Image)
            .expect("image item");
        assert_eq!((img.col, img.width, img.height), (0, 10, 3));
        assert_eq!(img.image.as_deref(), Some("http://e.com/i.png"));
        // Baseline alignment: the replaced box's baseline is its bottom edge,
        // so the adjacent text sits on the image's LAST row.
        let (r, after) = find(&out, "after");
        assert_eq!((r, after.col), (2, 10));
    }

    #[test]
    fn undecoded_image_falls_back_to_alt_text() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0"><img src="i.png" alt="a kitten"></p></body>"#,
            40,
        );
        let (_, it) = find(&out, "a kitten");
        assert_eq!(it.kind, ItemKind::Image);
        assert_eq!(it.image, None);
    }

    #[test]
    fn undecoded_image_with_attr_dims_reserves_blank_box() {
        let out = lay(
            r#"<body style="margin:0"><img src="i.png" width="80" height="64" alt="x"></body>"#,
            40,
        );
        let img = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|it| it.kind == ItemKind::Image)
            .expect("reserved box");
        assert_eq!((img.width, img.height), (10, 4));
        assert_eq!(img.image, None, "no pixels yet — renderer paints blank");
    }

    #[test]
    fn text_align_center_and_right() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;text-align:center">mid</p><p style="margin:0;text-align:right">end</p></body>"#,
            20,
        );
        let (_, mid) = find(&out, "mid");
        assert_eq!(mid.col, 8, "(20-3)/2 = 8");
        let (_, end) = find(&out, "end");
        assert_eq!(end.col, 17);
    }

    #[test]
    fn text_justify_expands_word_gaps() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;text-align:justify">aa bb cc dd ee ff gg hh ii jj kk zz</p></body>"#,
            10,
        );
        // Every non-final line fills the full 10 cells.
        let n = out.rows.iter().filter(|r| !r.items.is_empty()).count();
        for row in out.rows.iter().take(n.saturating_sub(1)) {
            if row.items.is_empty() {
                continue;
            }
            assert_eq!(
                display_width(row_text(row).trim_end()),
                10,
                "justified line fills capacity: {:?}",
                row_text(row)
            );
        }
    }

    #[test]
    fn visibility_hidden_lays_out_but_paints_blank() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;visibility:hidden">ghost</p><p style="margin:0">real</p></body>"#,
            40,
        );
        let (rg, ghost) = find(&out, "ghost");
        assert!(ghost.invisible);
        assert_eq!(rg, 0, "hidden box still occupies its row");
        let (rr, real) = find(&out, "real");
        assert!(!real.invisible);
        assert_eq!(rr, 1);
    }

    #[test]
    fn display_none_generates_nothing() {
        let out = lay(
            r#"<body style="margin:0"><p style="display:none">gone</p><p style="margin:0">kept</p></body>"#,
            40,
        );
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("gone")),
            "display:none subtree renders nothing"
        );
        let (r, _) = find(&out, "kept");
        assert_eq!(r, 0);
    }

    #[test]
    fn anchor_rows_map_ids_to_first_rows() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">top</p><h2 id="sec" style="margin:16px 0">Section</h2><a name="legacy"></a></body>"#,
            40,
        );
        let (r, _) = find(&out, "Section");
        assert_eq!(out.anchor_rows.get("sec"), Some(&r));
        assert!(out.anchor_rows.contains_key("legacy"));
    }

    #[test]
    fn overlong_word_overflows_and_clips_at_viewport() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">aaaaaaaaaaaaaaaaaaaa</p></body>"#,
            10,
        );
        // 20-cell word at 10-cell viewport: clipped at the right edge (what a
        // browser shows before you scroll right), never force-broken.
        assert_eq!(row_text(&out.rows[0]), "aaaaaaaaaa");
        assert_eq!(out.rows.len(), 1);
    }

    #[test]
    fn nowrap_does_not_wrap() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0;white-space:nowrap">aaa bbb ccc ddd eee</p></body>"#,
            10,
        );
        let non_empty = out.rows.iter().filter(|r| !r.items.is_empty()).count();
        assert_eq!(non_empty, 1);
    }

    #[test]
    fn cjk_wraps_between_ideographs() {
        // 10 cols (the engine's minimum content width) = 5 wide glyphs.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">日本語のテキスト</p></body>"#,
            10,
        );
        assert_eq!(row_text(&out.rows[0]), "日本語のテ");
        assert_eq!(row_text(&out.rows[1]), "キスト");
    }

    #[test]
    fn details_closed_shows_only_summary() {
        let out = lay(
            r#"<body style="margin:0"><details><summary>more</summary><p>secret</p></details></body>"#,
            40,
        );
        find(&out, "more");
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("secret")),
            "closed details hides non-summary children"
        );
    }

    #[test]
    fn definite_height_reserves_rows() {
        let out = lay(
            r#"<body style="margin:0"><div style="height:64px"></div><p style="margin:0">below</p></body>"#,
            40,
        );
        let (r, _) = find(&out, "below");
        assert_eq!(r, 4, "64px = 4 rows reserved by the empty box");
    }

    // ---- the P1 gate: replaced elements (image-heavy pages) ----

    fn lay_with_forms(html: &str, cols: usize, images: &ImageSizes) -> Output {
        let dom = Dom::parse_document(html);
        let base = Url::parse("http://e.com/").unwrap();
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        lay_out_document(&dom, &base, (cols, 24), &forms, &controls, images)
    }

    fn img_sizes(pairs: &[(&str, u16, u16)]) -> ImageSizes {
        pairs
            .iter()
            .map(|&(u, w, h)| (u.to_string(), (w, h)))
            .collect()
    }

    fn first_image(out: &Output) -> (usize, &Item) {
        for (r, row) in out.rows.iter().enumerate() {
            for it in &row.items {
                if it.kind == ItemKind::Image {
                    return (r, it);
                }
            }
        }
        panic!("no image item");
    }

    #[test]
    fn max_width_100pct_downscales_preserving_ratio() {
        // Decoded 100×20 cells = 800×320px, 40-col (320px) viewport,
        // `max-width:100%`: the §10.4 table scales to 320×128px = 40×8.
        let images = img_sizes(&[("http://e.com/big.png", 100, 20)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="big.png" style="max-width:100%" alt="x"></body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!((img.width, img.height), (40, 8));
        assert_eq!(img.image.as_deref(), Some("http://e.com/big.png"));
    }

    #[test]
    fn object_fit_cover_sets_crop() {
        let images = img_sizes(&[("http://e.com/i.png", 20, 5)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="i.png" style="width:80px;height:80px;object-fit:cover"></body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!((img.width, img.height), (10, 5));
        assert!(img.crop, "cover fills the box and crops overflow");
    }

    #[test]
    fn object_fit_contain_letterboxes_centered() {
        // Natural 10×3 cells (80×48px) in an 80×144px box: contain keeps the
        // natural rect, centered 48px (3 rows) below the box top; the BOX
        // (10 cells × 9 rows) is what the flow reserves.
        let images = img_sizes(&[("http://e.com/i.png", 10, 3)]);
        let out = lay_images(
            r#"<body style="margin:0"><img src="i.png" style="width:80px;height:144px;object-fit:contain"><p style="margin:0">after</p></body>"#,
            40,
            &images,
        );
        let (r, img) = first_image(&out);
        assert_eq!(r, 3, "paint rect centered: (9-3)/2 rows below box top");
        assert_eq!((img.width, img.height), (10, 3));
        assert!(!img.crop);
        let (after, _) = find(&out, "after");
        assert_eq!(after, 9, "the flow advanced by the full 9-row box");
    }

    #[test]
    fn pct_height_resolves_against_definite_cb() {
        // CB height 160px; img height:50% = 80px = 5 rows; natural ratio
        // 80:160 px (1:2) gives width 40px = 5 cells.
        let images = img_sizes(&[("http://e.com/i.png", 10, 10)]);
        let out = lay_images(
            r#"<body style="margin:0"><div style="height:160px"><img src="i.png" style="height:50%"></div></body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!((img.width, img.height), (5, 5));
    }

    #[test]
    fn undecoded_image_with_aspect_ratio_reserves_box() {
        let out = lay(
            r#"<body style="margin:0"><img src="i.png" style="width:160px;aspect-ratio:2/1" alt="x"></body>"#,
            40,
        );
        let (_, img) = first_image(&out);
        assert_eq!((img.width, img.height), (20, 5), "160×80px from the ratio");
        assert_eq!(img.image, None, "reserved, not yet decoded");
    }

    #[test]
    fn thumbnail_row_wraps_into_grid() {
        // The image-heavy gate: six 10×4 thumbnails at 40 cols pack four to
        // a line and wrap, exactly like a browser's inline image run.
        let images = img_sizes(&[("http://e.com/t.png", 10, 4)]);
        let html = r#"<body style="margin:0"><p style="margin:0"><img src="t.png"><img src="t.png"><img src="t.png"><img src="t.png"><img src="t.png"><img src="t.png"></p></body>"#;
        let out = lay_images(html, 40, &images);
        let row0: Vec<u16> = out.rows[0].items.iter().map(|i| i.col).collect();
        assert_eq!(row0, vec![0, 10, 20, 30]);
        let row4: Vec<u16> = out.rows[4].items.iter().map(|i| i.col).collect();
        assert_eq!(row4, vec![0, 10], "fifth and sixth wrap to the next strip");
    }

    #[test]
    fn text_input_pads_to_size_attr_and_default() {
        let out = lay_with_forms(
            r#"<body style="margin:0"><form action="/s"><input name="q" size="10"><input name="r"></form></body>"#,
            80,
            &HashMap::new(),
        );
        let q = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.kind == ItemKind::Form && i.text.contains('q'))
            .expect("q widget");
        assert_eq!(display_width(&q.text), 12, "size=10 + brackets");
        assert!(q.text.starts_with("[q") && q.text.ends_with(']'));
        let r = out
            .rows
            .iter()
            .flat_map(|rw| &rw.items)
            .find(|i| i.kind == ItemKind::Form && i.text.contains('r'))
            .expect("r widget");
        assert_eq!(display_width(&r.text), 22, "UA default size 20 + brackets");
    }

    #[test]
    fn css_width_sizes_text_input() {
        let out = lay_with_forms(
            r#"<body style="margin:0"><form action="/s"><input name="q" style="width:80px"></form></body>"#,
            80,
            &HashMap::new(),
        );
        let q = out
            .rows
            .iter()
            .flat_map(|r| &r.items)
            .find(|i| i.kind == ItemKind::Form)
            .expect("widget");
        assert_eq!(display_width(&q.text), 10, "80px = 10 cells");
    }

    #[test]
    fn video_direct_source_renders_quality_label() {
        let out = lay(
            r#"<body style="margin:0"><video><source src="clip.mp4" res="720" label="HD"></video></body>"#,
            60,
        );
        let (_, it) = find(&out, "▶ Video · 720p HD");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.as_str().ends_with("clip.mp4"))
        );
    }

    #[test]
    fn audio_with_src_renders_audio_label() {
        let out = lay(
            r#"<body style="margin:0"><audio src="a.mp3"></audio></body>"#,
            60,
        );
        let (_, it) = find(&out, "♪ Audio");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.as_str().ends_with("a.mp3"))
        );
    }

    #[test]
    fn sourceless_video_targets_enclosing_card_link() {
        let out = lay(
            r#"<body style="margin:0"><a href="/watch/1"><video></video></a></body>"#,
            60,
        );
        let (_, it) = find(&out, "▶ Watch in mpv");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.path() == "/watch/1"),
            "the card's anchor names the playable page"
        );
    }

    #[test]
    fn sourceless_video_on_non_video_page_is_dead_end() {
        let out = lay(r#"<body style="margin:0"><video></video></body>"#, 60);
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("mpv") || i.link.is_some()),
            "homepage-autoplay hero: no playable target, no link"
        );
    }

    #[test]
    fn og_video_page_gets_page_level_fallback() {
        let out = lay(
            r#"<html><head><meta property="og:video" content="https://cdn.e.com/v.m3u8"></head><body style="margin:0"><p style="margin:0">a watch page</p></body></html>"#,
            60,
        );
        let (_, it) = find(&out, "▶ Watch in mpv");
        assert!(
            matches!(&it.link, Some(crate::doc::Link::Media(u)) if u.as_str() == "http://e.com/"),
            "the page itself is the yt-dlp target"
        );
    }

    #[test]
    fn video_poster_thumbnail_is_the_link() {
        let images = img_sizes(&[("http://e.com/p.jpg", 8, 4)]);
        let out = lay_images(
            r#"<body style="margin:0"><video src="clip.mp4" poster="p.jpg"></video></body>"#,
            60,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!(img.image.as_deref(), Some("http://e.com/p.jpg"));
        assert!(
            matches!(&img.link, Some(crate::doc::Link::Media(u)) if u.as_str().ends_with("clip.mp4"))
        );
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains('▶')),
            "the drawn preview IS the affordance — no text line under it"
        );
    }

    #[test]
    fn suppressed_out_of_flow_video_renders_nothing() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">cap</p><video src="t.mp4" style="opacity:0;position:absolute"></video></body>"#,
            60,
        );
        assert_eq!(
            out.rows.iter().filter(|r| !r.items.is_empty()).count(),
            1,
            "the lingering opacity:0 abspos microtrailer adds no row"
        );
    }

    // ---- the P2 gate: flexbox (the old engine's minefield, §9 as written) ----

    #[test]
    fn flex_row_places_items_side_by_side() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="width:80px">aa<br>bb<br>cc</div>
                <div style="width:80px">dd</div>
               </div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (ra, a) = find(&out, "aa");
        assert_eq!((ra, a.col), (0, 0));
        let (rd, d) = find(&out, "dd");
        assert_eq!((rd, d.col), (0, 10), "second item beside the first");
        let (raf, _) = find(&out, "after");
        assert_eq!(raf, 3, "container height = tallest item (3 lines)");
    }

    #[test]
    fn flex_grow_distributes_by_factor() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="flex:1">a</div><div style="flex:3">b</div>
               </div></body>"#,
            80,
        );
        let (_, b) = find(&out, "b");
        assert_eq!(b.col, 20, "1:3 split of 640px → second item at 160px");
    }

    #[test]
    fn flex_grow_freezes_at_max_width_and_redistributes() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="flex:1;max-width:160px">a</div><div style="flex:1">b</div>
               </div></body>"#,
            80,
        );
        let (_, b) = find(&out, "b");
        assert_eq!(b.col, 20, "a frozen at 160px; b takes the remaining 480px");
    }

    #[test]
    fn flex_shrink_floors_at_min_content() {
        // The §4.5 automatic minimum: a shrinking item can't compress below
        // its longest word (the class of bug that collapsed Steam's QR pane
        // in the old engine).
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;width:160px">
                <div style="width:320px">verylongword</div><div style="width:320px">x</div>
               </div></body>"#,
            80,
        );
        let (r, w) = find(&out, "verylongword");
        assert_eq!((r, w.col), (0, 0), "min-content floor keeps the word whole");
        let (_, x) = find(&out, "x");
        assert_eq!(x.col, 12, "neighbor absorbs the deficit: 160−96 = 64px");
    }

    #[test]
    fn flex_basis_zero_non_growing_item_keeps_content_minimum() {
        // `flex: 0 1 0px`: base 0, but the hypothetical main size clamps to
        // the §4.5 content minimum — the item shows its content.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="flex:0 1 0px">QRCODE</div><div style="flex:1">rest</div>
               </div></body>"#,
            80,
        );
        let (_, q) = find(&out, "QRCODE");
        assert_eq!(q.col, 0);
        assert_eq!(display_width(&q.text), 6, "not collapsed to zero");
        let (_, rest) = find(&out, "rest");
        assert_eq!(rest.col, 6, "flexible neighbor starts right after");
    }

    #[test]
    fn justify_content_center_and_space_between() {
        let out = lay(
            r#"<body style="margin:0">
               <div style="display:flex;justify-content:center"><div style="width:160px">mid</div></div>
               <div style="display:flex;justify-content:space-between">
                 <div style="width:80px">l</div><div style="width:80px">r</div>
               </div></body>"#,
            80,
        );
        let (_, mid) = find(&out, "mid");
        assert_eq!(mid.col, 30, "(640−160)/2 = 240px");
        let (_, l) = find(&out, "l");
        let (_, r) = find(&out, "r");
        assert_eq!(l.col, 0);
        assert_eq!(r.col, 70, "pushed to the far edge");
    }

    #[test]
    fn align_items_center_offsets_shorter_item() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;align-items:center">
                <div style="width:80px">a<br>b<br>c</div><div style="width:80px">mid</div>
               </div></body>"#,
            80,
        );
        let (r, _) = find(&out, "mid");
        assert_eq!(r, 1, "one-line item centers against the 3-line one");
    }

    #[test]
    fn main_axis_auto_margin_pushes_to_the_end() {
        // The nav idiom: `margin-left:auto` absorbs the free space (§9.5 —
        // auto margins eat it BEFORE justify-content sees any).
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div>logo</div><div style="margin-left:auto">login</div>
               </div></body>"#,
            80,
        );
        let (_, login) = find(&out, "login");
        assert_eq!(login.col, 75, "80 − 5 cells");
    }

    #[test]
    fn order_reorders_items() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="order:2;width:80px">second</div><div style="order:1;width:80px">first</div>
               </div></body>"#,
            80,
        );
        let (_, f) = find(&out, "first");
        let (_, s) = find(&out, "second");
        assert_eq!(f.col, 0);
        assert_eq!(s.col, 10);
    }

    #[test]
    fn row_reverse_mirrors_main_axis() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-direction:row-reverse">
                <div style="width:80px">one</div><div style="width:80px">two</div>
               </div></body>"#,
            80,
        );
        let (_, one) = find(&out, "one");
        assert_eq!(one.col, 70, "first item lands at the right edge");
        let (_, two) = find(&out, "two");
        assert_eq!(two.col, 60);
    }

    #[test]
    fn flex_wrap_breaks_lines_and_honors_gaps() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-wrap:wrap;gap:16px">
                <div style="width:240px">a</div><div style="width:240px">b</div><div style="width:240px">c</div>
               </div></body>"#,
            80,
        );
        let (ra, a) = find(&out, "a");
        let (rb, b) = find(&out, "b");
        let (rc, c) = find(&out, "c");
        assert_eq!((ra, a.col), (0, 0));
        assert_eq!((rb, b.col), (0, 32), "240px + 16px gap = 32 cells");
        assert_eq!((rc, c.col), (2, 0), "wrapped; 16px row-gap = 1 blank row");
    }

    #[test]
    fn column_with_definite_height_grows_items() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-direction:column;height:320px">
                <div style="flex:1">top</div><div style="flex:1">bottom</div>
               </div></body>"#,
            80,
        );
        let (rt, _) = find(&out, "top");
        let (rb, _) = find(&out, "bottom");
        assert_eq!(rt, 0);
        assert_eq!(rb, 10, "two 160px halves of the 320px column");
    }

    #[test]
    fn column_align_items_center_centers_fixed_width_item() {
        // The Steam-login-card shape: a bounded card centered in a column.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-direction:column;align-items:center">
                <div style="width:160px">card</div>
               </div></body>"#,
            80,
        );
        let (_, card) = find(&out, "card");
        assert_eq!(card.col, 30, "(640−160)/2 = 240px = col 30");
    }

    #[test]
    fn column_stretch_fills_and_pct_child_resolves() {
        // Stretch (the default) gives the item the full cross size; a
        // percentage child resolves against the item's USED width — the
        // grown-flex-base percentage class of bug.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex">
                <div style="flex:1"><div style="width:50%">aaaa bbbb cccc</div></div>
                <div style="flex:1">right</div>
               </div></body>"#,
            80,
        );
        // Item = 320px; child 50% = 160px = 20 cells: the text wraps there.
        let (r1, t) = find(&out, "aaaa bbbb cccc");
        assert_eq!((r1, t.col), (0, 0));
        assert!(display_width(&t.text) <= 20, "wrapped at the child's 160px");
        let (_, right) = find(&out, "right");
        assert_eq!(right.col, 40, "sibling at the flexed 320px boundary");
    }

    #[test]
    fn overflow_hidden_zeroes_the_automatic_minimum() {
        // §4.5: a scroll container's automatic minimum is zero — the
        // standards answer to the `min-width:0` hack.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;width:160px">
                <div style="flex:1;overflow:hidden">unshrinkablelongword</div>
                <div style="flex:1">x</div>
               </div></body>"#,
            80,
        );
        let (_, x) = find(&out, "x");
        assert_eq!(x.col, 10, "equal halves — no min-content floor applies");
    }

    #[test]
    fn nested_flex_tower_lays_out() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;flex-direction:column">
                <div style="display:flex"><div style="width:80px">a1</div><div style="width:80px">a2</div></div>
                <div style="display:flex"><div style="width:80px">b1</div><div style="width:80px">b2</div></div>
               </div></body>"#,
            80,
        );
        let (r, a2) = find(&out, "a2");
        assert_eq!((r, a2.col), (0, 10));
        let (r2, b2) = find(&out, "b2");
        assert_eq!((r2, b2.col), (1, 10));
    }

    #[test]
    fn anonymous_text_becomes_an_item() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex"><div style="width:80px">a</div>loose<div style="width:80px">b</div></div></body>"#,
            80,
        );
        let (_, loose) = find(&out, "loose");
        assert_eq!(loose.col, 10, "text run wraps into an anonymous item");
        let (_, b) = find(&out, "b");
        assert_eq!(b.col, 15, "after the 5-cell anonymous item");
    }

    #[test]
    fn templateless_grid_stacks_one_column() {
        // A grid with no template is a single implicit column, one row per
        // item — exactly what a browser renders (the old engine's
        // shelf-pack fallback is gone; real templates now do the packing).
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid">
                <div style="width:240px">t1</div><div>t2</div><div>t3</div>
               </div></body>"#,
            80,
        );
        let (r1, t1) = find(&out, "t1");
        let (r2, _) = find(&out, "t2");
        let (r3, _) = find(&out, "t3");
        assert_eq!((r1, t1.col), (0, 0));
        assert_eq!(r2, 1);
        assert_eq!(r3, 2);
    }

    #[test]
    fn flexed_image_item_scales_through_its_ratio() {
        // A replaced flex item flexed wider keeps its aspect through the
        // §9.4 replaced hypothetical cross size.
        // Natural 20×5 cells = 160×80px (2:1).
        let mut images = HashMap::new();
        images.insert("http://e.com/i.png".to_string(), (20u16, 5u16));
        let out = lay_images(
            r#"<body style="margin:0"><div style="display:flex"><img src="i.png" style="flex:1"></div></body>"#,
            40,
            &images,
        );
        let (_, img) = first_image(&out);
        assert_eq!(
            (img.width, img.height),
            (40, 10),
            "320px wide → 160px tall (2:1)"
        );
    }

    // ---- the P3 gate: grid (real tracks + placement on real widths) ----

    #[test]
    fn github_layout_shape_lays_side_by_side() {
        // The GitHub `Layout` gate: an auto sidebar beside a flexible
        // minmax(0, calc(100% − 296px)) main, placed by line numbers. The
        // §11.8 stretch hands the auto track the leftover — the sidebar
        // comes out exactly 296px, the design's intent.
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:auto minmax(0, calc(100% - 296px))">
                <div style="grid-column:1">nav</div>
                <div style="grid-column:2">main content</div>
               </div></body>"#,
            80,
        );
        let (_, nav) = find(&out, "nav");
        assert_eq!(nav.col, 0);
        let (r, main) = find(&out, "main content");
        assert_eq!((r, main.col), (0, 37), "296px = col 37");
    }

    #[test]
    fn archive_org_minmax_tile_grid() {
        // repeat(auto-fill, minmax(16rem, 1fr)) at 640px: two 256px minimums
        // fit; fr grows each to 320px.
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:repeat(auto-fill, minmax(16rem, 1fr))">
                <div>tile one</div><div>tile two</div><div>tile three</div>
               </div></body>"#,
            80,
        );
        let (r1, t1) = find(&out, "tile one");
        let (r2, t2) = find(&out, "tile two");
        let (r3, t3) = find(&out, "tile three");
        assert_eq!((r1, t1.col), (0, 0));
        assert_eq!((r2, t2.col), (0, 40), "second 320px track");
        assert_eq!((r3, t3.col), (1, 0), "wraps to the second grid row");
    }

    #[test]
    fn danbooru_auto_fill_gap_grid() {
        // repeat(auto-fill, minmax(80px, 1fr)) with an 8px gap at 640px:
        // ⌊648/88⌋ = 7 columns; the six 8px gaps leave 592px for the fr
        // expansion (84.57px each).
        let html = r#"<body style="margin:0"><div style="display:grid;gap:8px;grid-template-columns:repeat(auto-fill, minmax(80px, 1fr))">
            <div>p1</div><div>p2</div><div>p3</div><div>p4</div><div>p5</div><div>p6</div><div>p7</div><div>p8</div>
           </div></body>"#;
        let out = lay(html, 80);
        let rows0: Vec<u16> = out.rows[0].items.iter().map(|i| i.col).collect();
        assert_eq!(rows0.len(), 7, "seven thumbnails on the first grid row");
        let (r8, p8) = find(&out, "p8");
        // The 8px row-gap lands the second grid row at y = 24px, which
        // edge-snaps to doc row 2 (a full blank gap row).
        assert_eq!((r8, p8.col), (2, 0), "eighth wraps");
    }

    #[test]
    fn fixed_fr_tracks_and_gaps_position_exactly() {
        // 96px 1fr 2fr with 16px gaps at 640px: 608px of track space,
        // 512px flexed 1:2 → columns at 0 / 112px / 298.67px.
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:96px 1fr 2fr;gap:16px">
                <div>a</div><div>b</div><div>c</div>
               </div></body>"#,
            80,
        );
        let (_, a) = find(&out, "a");
        let (_, b) = find(&out, "b");
        let (_, c) = find(&out, "c");
        assert_eq!((a.col, b.col, c.col), (0, 14, 37));
    }

    #[test]
    fn auto_fit_collapses_empty_tracks_for_fr() {
        // The responsive-card idiom: auto-fit + minmax(96px, 1fr) with two
        // items at 640px — four empty repetitions collapse and the two live
        // tracks split the full width.
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:repeat(auto-fit, minmax(96px, 1fr))">
                <div>left</div><div>right</div>
               </div></body>"#,
            80,
        );
        let (_, l) = find(&out, "left");
        let (_, r) = find(&out, "right");
        assert_eq!(l.col, 0);
        assert_eq!(r.col, 40, "two 320px halves, not six 96px slots");
    }

    #[test]
    fn negative_lines_and_row_placement() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:80px 80px 80px">
                <div style="grid-column:-2">endish</div>
                <div style="grid-column:1;grid-row:2">below</div>
               </div></body>"#,
            80,
        );
        let (r1, e) = find(&out, "endish");
        assert_eq!((r1, e.col), (0, 20), "line -2 of 4 = third track");
        let (r2, b) = find(&out, "below");
        assert_eq!((r2, b.col), (1, 0));
    }

    #[test]
    fn template_areas_place_named_items() {
        let out = lay(
            r#"<body style="margin:0"><div style='display:grid;grid-template-columns:160px 1fr;grid-template-areas:"head head" "nav main"'>
                <div style="grid-area:main">the main pane</div>
                <div style="grid-area:head">header</div>
                <div style="grid-area:nav">nav</div>
               </div></body>"#,
            80,
        );
        let (rh, h) = find(&out, "header");
        assert_eq!((rh, h.col), (0, 0));
        let (rn, n) = find(&out, "nav");
        assert_eq!((rn, n.col), (1, 0));
        let (rm, m) = find(&out, "the main pane");
        assert_eq!(
            (rm, m.col),
            (1, 20),
            "main starts after the 160px nav track"
        );
    }

    #[test]
    fn auto_flow_column_fills_down_then_across() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-rows:16px 16px;grid-auto-flow:column;grid-auto-columns:80px">
                <div>one</div><div>two</div><div>three</div><div>four</div>
               </div></body>"#,
            80,
        );
        let (r1, c1) = find(&out, "one");
        let (r2, c2) = find(&out, "two");
        let (r3, c3) = find(&out, "three");
        let (r4, c4) = find(&out, "four");
        assert_eq!((r1, c1.col), (0, 0));
        assert_eq!((r2, c2.col), (1, 0));
        assert_eq!((r3, c3.col), (0, 10), "third fills the next column");
        assert_eq!((r4, c4.col), (1, 10));
    }

    #[test]
    fn dense_packing_backfills_holes() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:80px 80px;grid-auto-flow:row dense">
                <div style="grid-column:2">pinned</div>
                <div style="grid-column:span 2">wide</div>
                <div>filler</div>
               </div></body>"#,
            80,
        );
        let (rf, f) = find(&out, "filler");
        assert_eq!(
            (rf, f.col),
            (0, 0),
            "dense fills the hole beside the pinned item"
        );
    }

    #[test]
    fn definite_row_tracks_reserve_height() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:80px;grid-template-rows:64px 32px">
                <div>a</div><div>b</div>
               </div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (ra, _) = find(&out, "a");
        let (rb, _) = find(&out, "b");
        let (raf, _) = find(&out, "after");
        assert_eq!(ra, 0);
        assert_eq!(rb, 4, "64px first row = 4 rows");
        assert_eq!(raf, 6, "container = 96px = 6 rows");
    }

    #[test]
    fn justify_and_align_self_position_within_areas() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:320px">
                <div style="width:80px;justify-self:center">mid</div>
                <div style="width:80px;justify-self:end">end</div>
               </div></body>"#,
            80,
        );
        let (_, mid) = find(&out, "mid");
        assert_eq!(mid.col, 15, "(320−80)/2 = 120px");
        let (_, end) = find(&out, "end");
        assert_eq!(end.col, 30, "320−80 = 240px");
    }

    #[test]
    fn fit_content_track_caps_at_argument() {
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:fit-content(160px) 80px">
                <div>a very long run of grid content here</div><div>side</div>
               </div></body>"#,
            80,
        );
        let (_, side) = find(&out, "side");
        assert_eq!(side.col, 20, "first track capped at 160px");
    }

    #[test]
    fn spanning_item_grows_intrinsic_columns() {
        // A span-2 item wider than both auto tracks' single-track content
        // forces the pair to accommodate it (§11.5 spanning distribution).
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:auto auto;justify-content:start">
                <div>ab</div><div>cd</div>
                <div style="grid-column:1 / span 2">wwwwwwwwwwwwwwwwwwww</div>
               </div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (_, w) = find(&out, "wwwwwwwwwwwwwwwwwwww");
        assert_eq!(display_width(&w.text), 20, "spanner fits unwrapped");
    }

    #[test]
    fn grid_items_stretch_to_row_height() {
        // Default align-items stretch: the shorter item's box fills the
        // row (visible via the following content's position).
        let out = lay(
            r#"<body style="margin:0"><div style="display:grid;grid-template-columns:160px 160px">
                <div>tall<br>tall<br>tall</div><div>short</div>
               </div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (rs, s) = find(&out, "short");
        assert_eq!((rs, s.col), (0, 20));
        let (raf, _) = find(&out, "after");
        assert_eq!(raf, 3, "row height = the tall item");
    }
    // ---- the P4 gate: positioned + stacking + paint order + transforms ----
    // (Stacked cards paint with the top card visible, arrows land where
    // written, fixed rails pin, modals cover — Appendix E + cell compositing.)

    #[test]
    fn relative_offset_shifts_box_without_affecting_flow() {
        let out = lay(
            r#"<body style="margin:0">
               <div style="position:relative;left:16px;top:32px">moved</div>
               <div>after</div></body>"#,
            80,
        );
        let (rm, m) = find(&out, "moved");
        assert_eq!(
            (rm, m.col),
            (2, 2),
            "offset by (16px, 32px) = (2 cols, 2 rows)"
        );
        let (ra, a) = find(&out, "after");
        assert_eq!(
            (ra, a.col),
            (1, 0),
            "§9.4.3: the following box is placed as if the offset never happened"
        );
    }

    #[test]
    fn relative_negative_top_paints_over_earlier_content() {
        // §9.4.3 allows overlap; the positioned box paints in Appendix E
        // step 8, over the in-flow text — later cells win at the overlap.
        let out = lay(
            r#"<body style="margin:0"><div>AAAA</div><div style="position:relative;top:-16px">BB</div></body>"#,
            80,
        );
        assert_eq!(row_text(&out.rows[0]), "BBAA");
    }

    #[test]
    fn abspos_insets_position_against_positioned_ancestor() {
        // §10.1: the containing block is the positioned ancestor's padding
        // box; §9.3.2 insets offset from its edges.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;height:64px">
                <div style="position:absolute;left:16px;top:32px">X</div>
               </div></body>"#,
            80,
        );
        let (r, x) = find(&out, "X");
        assert_eq!((r, x.col), (2, 2));
    }

    #[test]
    fn abspos_right_bottom_anchor_and_shrink_to_fit() {
        // right/bottom anchoring solves left/top through the §10.3.7/§10.6.4
        // constraint equations — which needs the real shrink-to-fit width
        // (3 cells for "END") to come out at col 37.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:320px;height:64px">
                <div style="position:absolute;right:0;bottom:0">END</div>
               </div></body>"#,
            80,
        );
        let (r, e) = find(&out, "END");
        assert_eq!((r, e.col), (3, 37), "320−24px = col 37; 64−16px = row 3");
    }

    #[test]
    fn abspos_all_auto_lands_at_static_position() {
        // §10.3.7/§10.6.4 rule sets with everything auto: the box sits where
        // it would have flowed; being positioned it paints OVER the sibling
        // that flows into the same place.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">one</p><div style="position:absolute">abs</div><p style="margin:0">two</p></body>"#,
            80,
        );
        let (r, a) = find(&out, "abs");
        assert_eq!((r, a.col), (1, 0), "static position: after the first <p>");
        assert_eq!(
            row_text(&out.rows[1]),
            "abs",
            "the covered sibling's cells belong to the later-painted abspos box"
        );
    }

    #[test]
    fn abspos_without_positioned_ancestor_uses_the_icb() {
        let out = lay(
            r#"<body style="margin:8px"><div style="position:absolute;left:0;top:0">corner</div><p>content</p></body>"#,
            80,
        );
        let (r, c) = find(&out, "corner");
        assert_eq!(
            (r, c.col),
            (0, 0),
            "§10.1: no positioned ancestor → the initial containing block"
        );
    }

    #[test]
    fn abspos_left_and_right_solve_the_width() {
        // §10.3.7 rule 5: width = cb − left − right; proven through the
        // right-aligned line landing at the solved content edge.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:320px;height:32px">
                <div style="position:absolute;left:16px;right:16px;text-align:right">end</div>
               </div></body>"#,
            80,
        );
        let (_, e) = find(&out, "end");
        assert_eq!(e.col, 35, "left 2 + (288px = 36 cells) − 3 = col 35");
    }

    #[test]
    fn z_index_orders_overlapping_siblings_not_tree_order() {
        // The z:5 box is FIRST in the document but paints LAST (§9.9).
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;height:16px">
                <div style="position:absolute;left:0;top:0;z-index:5">BB</div>
                <div style="position:absolute;left:0;top:0;z-index:2">AAAA</div>
               </div></body>"#,
            80,
        );
        assert_eq!(row_text(&out.rows[0]), "BBAA");
    }

    #[test]
    fn negative_z_paints_under_in_flow_content() {
        // Appendix E step 3 (negative-z stacking contexts) precedes the
        // in-flow content steps — the page text wins the contested cells.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative">
                <div style="position:absolute;left:0;top:0;z-index:-1">XXXXXX</div>text</div></body>"#,
            80,
        );
        assert_eq!(row_text(&out.rows[0]), "textXX");
    }

    #[test]
    fn modal_background_covers_the_page_inside_its_rect() {
        // A positioned box with a background is an OPAQUE FILL: the page
        // cells under its rect are erased, its own content paints on top.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">underneath content here</p>
               <div style="position:absolute;left:0;top:0;width:80px;height:16px;background:#000">MODAL</div></body>"#,
            80,
        );
        let (r, m) = find(&out, "MODAL");
        assert_eq!((r, m.col), (0, 0));
        let survivors: Vec<&Item> = out.rows[0]
            .items
            .iter()
            .filter(|it| it.text != "MODAL")
            .collect();
        assert_eq!(survivors.len(), 1, "one clipped remainder of the page text");
        assert_eq!(
            (survivors[0].col, survivors[0].text.as_str()),
            (10, " content here"),
            "the page text survives only past the modal's 80px (10-cell) rect"
        );
    }

    #[test]
    fn card_stack_paints_top_card_and_arrows_where_written() {
        // The Twitch-hero shape: stacked cards with backgrounds; only the
        // top card's content shows, the z:3 arrows land at the written
        // insets over it.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;width:320px;height:48px">
                <div style="position:absolute;inset:0;background:#111">bottom card text</div>
                <div style="position:absolute;inset:0;background:#222">TOP CARD</div>
                <div style="position:absolute;left:0;top:16px;z-index:3">&lsaquo;</div>
                <div style="position:absolute;right:0;top:16px;z-index:3">&rsaquo;</div>
               </div></body>"#,
            80,
        );
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("bottom")),
            "the lower card is fully covered by the top card's opaque fill"
        );
        let (rt, t) = find(&out, "TOP CARD");
        assert_eq!((rt, t.col), (0, 0));
        let (rl, l) = find(&out, "‹");
        assert_eq!((rl, l.col), (1, 0));
        let (rr, r) = find(&out, "›");
        assert_eq!((rr, r.col), (1, 39), "right:0 → 320−8px = col 39");
    }

    #[test]
    fn fixed_rails_pin_into_the_fixed_layer() {
        // The Mastodon shape: two fixed side rails leave the document flow
        // entirely and pin at their viewport positions.
        let out = lay(
            r#"<body style="margin:0">
               <div style="position:fixed;left:0;top:0;width:80px">LEFT RAIL</div>
               <div style="position:fixed;right:0;top:0;width:80px">RIGHT</div>
               <p style="margin:0">main content</p></body>"#,
            80,
        );
        let (r, m) = find(&out, "main content");
        assert_eq!((r, m.col), (0, 0), "fixed boxes take no flow space");
        assert_eq!(out.fixed.len(), 2);
        let left = &out.fixed[0];
        assert_eq!((left.col, left.row), (0, 0));
        assert!(left.rows[0].items.iter().any(|i| i.text == "LEFT RAIL"));
        let right = &out.fixed[1];
        assert_eq!(
            (right.col, right.row),
            (70, 0),
            "right:0 at 640px viewport → 560px = col 70"
        );
        assert!(right.rows[0].items.iter().any(|i| i.text == "RIGHT"));
    }

    #[test]
    fn fixed_bottom_anchors_to_the_viewport() {
        let out = lay(
            r#"<body style="margin:0"><div style="position:fixed;left:0;bottom:0">status bar</div></body>"#,
            80,
        );
        assert_eq!(out.fixed.len(), 1);
        assert_eq!(
            out.fixed[0].row, 23,
            "bottom:0 at a 24-row viewport → 384−16px = row 23"
        );
    }

    #[test]
    fn fixed_inside_transformed_ancestor_stays_in_the_document() {
        // css-transforms-1 §3: a transformed ancestor is the containing
        // block for fixed descendants — the box positions against IT and
        // scrolls with the page instead of pinning.
        let out = lay(
            r#"<body style="margin:0"><div style="transform:translateX(0px);height:32px">
                <div style="position:fixed;left:16px;top:16px">inner</div>
               </div></body>"#,
            80,
        );
        assert!(out.fixed.is_empty(), "not pinned");
        let (r, i) = find(&out, "inner");
        assert_eq!((r, i.col), (1, 2));
    }

    #[test]
    fn transform_translate_offsets_paint_not_flow() {
        let out = lay(
            r#"<body style="margin:0"><div style="transform:translate(16px, 32px)">moved</div><p style="margin:0">after</p></body>"#,
            80,
        );
        let (rm, m) = find(&out, "moved");
        assert_eq!((rm, m.col), (2, 2));
        let (ra, _) = find(&out, "after");
        assert_eq!(ra, 1, "surrounding flow is unaffected (transforms-1 §3)");
    }

    #[test]
    fn translate_property_percentage_of_own_border_box() {
        let out = lay(
            r#"<body style="margin:0"><div style="width:160px;translate:100%">x</div></body>"#,
            80,
        );
        let (_, x) = find(&out, "x");
        assert_eq!(x.col, 20, "100% of the box's own 160px = 20 cols");
    }

    #[test]
    fn sticky_rests_at_flow_position_and_hosts_abspos() {
        // css-position-3 §3.4: sticky offsets are scroll-driven — zero at
        // the initial position — and a sticky box is positioned, so it IS a
        // containing block for abspos descendants.
        let out = lay(
            r#"<body style="margin:0"><div style="position:sticky;top:0;height:32px">header
                <div style="position:absolute;right:0;top:16px">A</div>
               </div><p style="margin:0">body text</p></body>"#,
            80,
        );
        let (rh, h) = find(&out, "header");
        assert_eq!((rh, h.col), (0, 0), "no offset at rest");
        let (ra, a) = find(&out, "A");
        assert_eq!((ra, a.col), (1, 79), "right:0 of the sticky box's 640px");
        let (rb, _) = find(&out, "body text");
        assert_eq!(rb, 2);
    }

    #[test]
    fn opacity_zero_abspos_contributes_nothing() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">cap</p><div style="opacity:0;position:absolute;left:0;top:0">ghost</div></body>"#,
            80,
        );
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| i.text.contains("ghost")),
            "a paint-suppressed out-of-flow box emits no cells at all"
        );
        assert_eq!(out.rows.iter().filter(|r| !r.items.is_empty()).count(), 1);
    }

    #[test]
    fn visibility_hidden_abspos_keeps_ghost_geometry() {
        let out = lay(
            r#"<body style="margin:0"><div style="position:absolute;left:0;top:16px;visibility:hidden">ghost</div><p style="margin:0">real</p></body>"#,
            80,
        );
        let (rr, real) = find(&out, "real");
        assert_eq!((rr, real.col), (0, 0));
        let (rg, g) = find(&out, "ghost");
        assert_eq!((rg, g.col), (1, 0), "visibility keeps the box");
        assert!(g.invisible, "…but paints it blank");
    }

    #[test]
    fn inline_abspos_takes_the_pen_static_position() {
        // §10.3.7: the hypothetical box of an inline-level abspos element
        // sits at the pen; painted in step 8 it covers the following text.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">before<span style="position:absolute">tip</span>rest</p></body>"#,
            80,
        );
        let (r, t) = find(&out, "tip");
        assert_eq!((r, t.col), (0, 6));
        assert_eq!(row_text(&out.rows[0]), "beforetipt");
    }

    #[test]
    fn three_row_areas_via_stylesheet_keep_their_rows() {
        // The regression that hid behind two-row templates: a named area's
        // half-open end track must NOT gain an extra spanned row (the
        // footer landed on nav's row). Stylesheet-driven, like real pages.
        let out = lay(
            r#"<html><head><style>
              .page { display: grid; grid-template-columns: 160px 1fr;
                      grid-template-areas: "head head" "nav main" "foot foot"; }
              .h { grid-area: head; } .n { grid-area: nav; }
              .m { grid-area: main; } .f { grid-area: foot; }
            </style></head><body style="margin:0"><div class="page">
              <div class="h">HEADER</div><div class="n">NAV</div>
              <div class="m">MAINAREA</div><div class="f">FOOTER</div>
            </div></body></html>"#,
            80,
        );
        let (rh, h) = find(&out, "HEADER");
        assert_eq!((rh, h.col), (0, 0));
        let (rn, n) = find(&out, "NAV");
        assert_eq!((rn, n.col), (1, 0));
        let (rm, m) = find(&out, "MAINAREA");
        assert_eq!((rm, m.col), (1, 20));
        let (rf, f) = find(&out, "FOOTER");
        assert_eq!((rf, f.col), (2, 0), "foot spans exactly its own row");
    }

    // ---- P5a: overflow clipping (CSS Overflow L3 §2/§3) ----
    // A non-`visible` overflow value clips content to the padding box. Since
    // the engine computes real used heights, a definite-height overflow box
    // simply occupies its height and the compositor drops the overflowing
    // cells — no buffer/window (that is P5b scrolling).

    #[test]
    fn sr_only_box_clips_its_label_to_nothing() {
        // The visually-hidden idiom: a sub-cell box with overflow:hidden. Its
        // clip (the padding box) rounds to under a cell, so the overflowing
        // label paints NOTHING — a browser shows a ~1px speck; we faithfully
        // render nothing rather than a stray glyph. GEOMETRIC, not a heuristic.
        let out = lay(
            r#"<body style="margin:0"><div style="width:1px;height:1px;overflow:hidden">label</div><p style="margin:0">real</p></body>"#,
            20,
        );
        assert!(absent(&out, "label"), "sr-only label clips to nothing");
        assert_eq!(
            find(&out, "real").0,
            0,
            "the sub-cell box reserves no rows either"
        );
    }

    #[test]
    fn overflow_hidden_clips_content_below_the_box() {
        // A 2-row (32px) overflow:hidden box holding four 1-row lines: the two
        // lines past the box are clipped, and following content is NOT
        // overlapped by them (the whole reason clipping is load-bearing here).
        let out = lay(
            r#"<body style="margin:0"><div style="height:32px;overflow:hidden;margin:0"><p style="margin:0">L1</p><p style="margin:0">L2</p><p style="margin:0">L3</p><p style="margin:0">L4</p></div><p style="margin:0">after</p></body>"#,
            20,
        );
        assert_eq!(find(&out, "L1").0, 0);
        assert_eq!(find(&out, "L2").0, 1);
        assert!(absent(&out, "L3"), "third line is clipped below the box");
        assert!(absent(&out, "L4"), "fourth line is clipped below the box");
        assert_eq!(
            find(&out, "after").0,
            2,
            "following content follows the box, not the clipped overflow"
        );
    }

    #[test]
    fn overflow_visible_does_not_clip() {
        // The control: the same box with overflow:visible keeps all four lines
        // painting past its 2-row height (only `visible` doesn't clip).
        let out = lay(
            r#"<body style="margin:0"><div style="height:32px;overflow:visible;margin:0"><p style="margin:0">L1</p><p style="margin:0">L2</p><p style="margin:0">L3</p><p style="margin:0">L4</p></div></body>"#,
            20,
        );
        assert_eq!(find(&out, "L3").0, 2);
        assert_eq!(find(&out, "L4").0, 3);
    }

    #[test]
    fn overflow_hidden_truncates_a_wide_line() {
        // Horizontal clip: an 8-col (64px) overflow:hidden box with nowrap
        // content wider than it clips at the BOX's right edge, not the
        // viewport's (viewport is 40 cols).
        let out = lay(
            r#"<body style="margin:0"><div style="width:64px;overflow:hidden;white-space:nowrap;margin:0">abcdefghijklmnop</div></body>"#,
            40,
        );
        assert_eq!(row_text(&out.rows[0]), "abcdefgh");
    }

    #[test]
    fn absolute_child_escapes_an_in_flow_overflow_hidden_ancestor() {
        // The abspos box's containing block is the positioned <body>, NOT the
        // in-flow overflow:hidden div between them — so that div does NOT clip
        // it (CSS Overflow L3 §3: a positioned box is clipped by its CB's clip
        // chain, which the CB-aware resolve_oof walk threads). It paints far
        // below the 1-row clip box's bottom.
        let out = lay(
            r#"<body style="margin:0;position:relative;height:200px"><div style="height:16px;overflow:hidden;margin:0"><span style="position:absolute;left:0;top:48px">escapee</span></div></body>"#,
            20,
        );
        assert_eq!(
            find(&out, "escapee").0,
            3,
            "abspos escapes the in-flow clip and paints at its CB offset (top:48px = row 3)"
        );
    }

    #[test]
    fn absolute_child_is_clipped_by_its_containing_block() {
        // Here the overflow:hidden box IS the abspos containing block
        // (position:relative), so its clip DOES apply — the child positioned
        // past the box bottom is clipped away.
        let out = lay(
            r#"<body style="margin:0"><div style="position:relative;height:16px;overflow:hidden;margin:0"><span style="position:absolute;left:0;top:48px">gone</span></div><p style="margin:0">after</p></body>"#,
            20,
        );
        assert!(
            absent(&out, "gone"),
            "abspos clipped by its own CB's overflow"
        );
        assert_eq!(find(&out, "after").0, 1, "the clip box is 1 row tall");
    }

    // ---- P5b: vertical scroll regions (CSS Overflow L3 §2/§3, CSSOM View) ----
    // A definite-height overflow-y:auto|scroll box whose content overflows is a
    // scroll container: its content goes into a separate buffer (scrollHeight),
    // the doc reserves a blank band of its clientHeight, and the renderer
    // windows the buffer over the band.

    #[test]
    fn overflow_y_auto_with_overflow_becomes_a_scroll_region() {
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">TOP</p><div style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">R1</p><p style="margin:0">R2</p><p style="margin:0">R3</p><p style="margin:0">R4</p><p style="margin:0">R5</p><p style="margin:0">R6</p></div><p style="margin:0">BOTTOM</p></body>"#,
            20,
        );
        assert_eq!(out.regions.len(), 1, "one scroll region");
        let r = &out.regions[0];
        assert_eq!(r.height, 3, "clientHeight = 48px = 3 rows");
        assert_eq!(r.buffer.len(), 6, "scrollHeight = 6 content rows");
        assert_eq!(r.voffset, 0, "scroll origin is the top (CSSOM View)");
        assert_eq!((r.start_row, r.left, r.width), (1, 0, 20), "band geometry");
        assert!(
            r.buffer
                .iter()
                .any(|row| row.items.iter().any(|i| i.text.contains("R6"))),
            "the buffer holds the full scrollable content"
        );
        assert!(
            absent(&out, "R4"),
            "region content is buffered, not in main rows"
        );
        assert_eq!(find(&out, "TOP").0, 0);
        assert_eq!(
            find(&out, "BOTTOM").0,
            4,
            "content below follows the 3-row band, not the 6-row content"
        );
    }

    #[test]
    fn overflow_y_auto_that_fits_is_not_a_region() {
        let out = lay(
            r#"<body style="margin:0"><div style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">A</p><p style="margin:0">B</p></div><p style="margin:0">after</p></body>"#,
            20,
        );
        assert!(
            out.regions.is_empty(),
            "content fits (2 rows < 3): no region"
        );
        assert_eq!(find(&out, "A").0, 0);
        assert_eq!(find(&out, "B").0, 1);
        assert_eq!(
            find(&out, "after").0,
            3,
            "the definite-height box still reserves its 3 rows"
        );
    }

    #[test]
    fn the_principal_scroller_flows_into_the_document() {
        // Locked viewport (html overflow:hidden) + the sole overflow:auto box
        // carrying the flow ⇒ the PRINCIPAL scroller: it flows into the
        // document (page-scrolled), NOT an inner region (CSS Overflow L3 §3.1).
        let out = lay(
            r#"<html style="overflow:hidden"><body style="margin:0"><div style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">P1</p><p style="margin:0">P2</p><p style="margin:0">P3</p><p style="margin:0">P4</p><p style="margin:0">P5</p><p style="margin:0">P6</p></div></body></html>"#,
            20,
        );
        assert!(
            out.regions.is_empty(),
            "the principal scroller is not virtualized into a region"
        );
        assert_eq!(find(&out, "P1").0, 0);
        assert_eq!(
            find(&out, "P6").0,
            5,
            "its content flows into the document (all rows page-scrollable)"
        );
    }

    #[test]
    fn region_seeds_voffset_from_the_scroll_top_signal() {
        // The live serializer bakes data-trust-scroll-top (rows) + data-trust-
        // node; the region seeds voffset (clamped to scrollHeight−clientHeight)
        // and emits a scroll_clip for the app's clientHeight geometry push.
        let out = lay(
            r#"<body style="margin:0"><div data-trust-node="42" data-trust-scroll-top="2" style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">S1</p><p style="margin:0">S2</p><p style="margin:0">S3</p><p style="margin:0">S4</p><p style="margin:0">S5</p><p style="margin:0">S6</p></div></body>"#,
            20,
        );
        assert_eq!(out.regions.len(), 1);
        let r = &out.regions[0];
        assert_eq!(r.voffset, 2, "seeded from data-trust-scroll-top");
        assert!(r.voffset_from_page);
        assert_eq!(r.live_node, Some(42));
        assert_eq!(
            out.scroll_clips,
            vec![(42, 3, 20)],
            "(live node, clientHeight rows, scrollport width cells)"
        );
    }

    // ---- P5c: horizontal carousels (CSS Overflow L3 §2, CSS Scroll Snap) ----
    // An overflow-x:auto|scroll box whose content overflows to the right is a
    // horizontal scroll strip: cards stay inline in the doc rows at their strip
    // columns (the renderer windows them to the band), snap stops are the cards'
    // leading edges, and the UA emits a ‹ › control pair.

    #[test]
    fn overflow_x_auto_strip_becomes_a_carousel() {
        // A flex row of five 40px cards (200px) in an 80px scroll box: the strip
        // overflows, so it becomes a carousel windowed to the 10-col band. Cards
        // are laid at their REAL flex widths — never a guessed "N across" size.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;overflow-x:auto;width:80px;margin:0"><div style="flex:0 0 auto;width:40px">C1</div><div style="flex:0 0 auto;width:40px">C2</div><div style="flex:0 0 auto;width:40px">C3</div><div style="flex:0 0 auto;width:40px">C4</div><div style="flex:0 0 auto;width:40px">C5</div></div></body>"#,
            40,
        );
        assert_eq!(out.carousels.len(), 1, "one carousel");
        let c = &out.carousels[0];
        assert_eq!((c.left, c.right), (0, 10), "band = 80px = 10 cols");
        assert_eq!(c.width, 25, "strip = 200px = 25 cols");
        assert_eq!(c.offset, 0, "scroll origin is the strip start");
        // No scroll-snap declared ⇒ FREE scroll, no snap positions (CSS Scroll
        // Snap 1: snapping is opt-in; we don't impose card-snap).
        assert!(
            !c.snap && c.stops.is_empty(),
            "free scroll, no imposed snap"
        );
        // Every card stays in the doc rows at its strip column (windowed at
        // render), including ones past the band's right edge.
        assert_eq!(find(&out, "C1").1.col, 0);
        assert_eq!(
            find(&out, "C5").1.col,
            20,
            "5th card at strip col 20, past the 10-col band"
        );
    }

    #[test]
    fn carousel_snaps_only_when_the_page_declares_it() {
        // scroll-snap-type on the container + scroll-snap-align on the cards ⇒
        // snap to those positions (CSS Scroll Snap 1). Here align:start ⇒ the
        // stops are the card leading edges.
        let out = lay(
            r#"<body style="margin:0"><div style="display:flex;overflow-x:auto;width:80px;margin:0;scroll-snap-type:x mandatory"><div style="flex:0 0 auto;width:40px;scroll-snap-align:start">C1</div><div style="flex:0 0 auto;width:40px;scroll-snap-align:start">C2</div><div style="flex:0 0 auto;width:40px;scroll-snap-align:start">C3</div></div></body>"#,
            40,
        );
        assert_eq!(out.carousels.len(), 1);
        let c = &out.carousels[0];
        assert!(c.snap, "the page declared scroll-snap-type: x mandatory");
        assert_eq!(c.stops, vec![0, 5, 10], "card leading edges (align:start)");
    }

    #[test]
    fn carousel_injects_no_scroll_chrome() {
        // We synthesise NO prev/next controls: the page defines its own scroll
        // affordance (or relies on the UA behavioural scroll, like a scrollbar).
        // The only items on screen are the page's own — nothing we invented.
        let out = lay(
            r#"<body style="margin:0"><p style="margin:0">HDR</p><div style="display:flex;overflow-x:auto;width:80px;margin:0"><div style="flex:0 0 auto;width:40px">C1</div><div style="flex:0 0 auto;width:40px">C2</div><div style="flex:0 0 auto;width:40px">C3</div><div style="flex:0 0 auto;width:40px">C4</div><div style="flex:0 0 auto;width:40px">C5</div></div></body>"#,
            40,
        );
        assert_eq!(out.carousels.len(), 1);
        assert!(
            absent(&out, "‹") && absent(&out, "›"),
            "no synthesised chrome"
        );
        assert!(
            !out.rows
                .iter()
                .flat_map(|r| &r.items)
                .any(|i| matches!(i.link, Some(crate::doc::Link::CarouselScroll(_)))),
            "no synthesised carousel-scroll links"
        );
    }

    // ---- P5-fidelity: nested scrollers (a scroll container inside another) ----

    #[test]
    fn a_region_nested_in_a_region_is_extracted_buffer_relative() {
        // An outer vertical scroll region whose content includes ANOTHER
        // vertical scroll region: the inner one is extracted into the OUTER's
        // `regions` (buffer-relative), independently scrollable within it.
        let out = lay(
            r#"<body style="margin:0"><div style="height:96px;overflow-y:auto;margin:0"><p style="margin:0">OT</p><div style="height:32px;overflow-y:auto;margin:0"><p style="margin:0">IN1</p><p style="margin:0">IN2</p><p style="margin:0">IN3</p><p style="margin:0">IN4</p></div><p style="margin:0">OB1</p><p style="margin:0">OB2</p><p style="margin:0">OB3</p><p style="margin:0">OB4</p><p style="margin:0">OB5</p></div></body>"#,
            20,
        );
        assert_eq!(out.regions.len(), 1, "one top-level (outer) region");
        let outer = &out.regions[0];
        assert_eq!(outer.height, 6, "outer clientHeight = 96px = 6 rows");
        assert_eq!(outer.buffer.len(), 8, "outer scrollHeight = 8 content rows");
        assert_eq!(outer.regions.len(), 1, "one region nested inside the outer");
        let inner = &outer.regions[0];
        assert_eq!(
            inner.start_row, 1,
            "inner band is buffer-relative (after OT)"
        );
        assert_eq!(inner.height, 2, "inner clientHeight = 32px = 2 rows");
        assert_eq!(
            inner.buffer.len(),
            4,
            "inner scrollHeight = 4 rows (IN1..IN4)"
        );
        assert!(
            inner
                .buffer
                .iter()
                .any(|r| r.items.iter().any(|i| i.text.contains("IN4"))),
            "inner content lives in the inner region's own buffer"
        );
        // The inner content is NOT in the outer buffer (its band is blank there).
        assert!(
            !outer
                .buffer
                .iter()
                .any(|r| r.items.iter().any(|i| i.text.contains("IN1"))),
            "the inner region's band is blank in the outer buffer"
        );
    }

    #[test]
    fn a_carousel_nested_in_a_region_is_windowed_within_it() {
        // The streaming-home idiom: a vertical feed (region) of horizontal
        // shelves (carousels). The shelf is extracted into the region's
        // `carousels` (buffer-relative) and windowed within the region's window.
        let out = lay(
            r#"<body style="margin:0"><div style="height:48px;overflow-y:auto;margin:0"><p style="margin:0">FEED-TOP</p><div style="display:flex;overflow-x:auto;width:80px;margin:0"><div style="flex:0 0 auto;width:40px">S1</div><div style="flex:0 0 auto;width:40px">S2</div><div style="flex:0 0 auto;width:40px">S3</div><div style="flex:0 0 auto;width:40px">S4</div></div><p style="margin:0">F1</p><p style="margin:0">F2</p><p style="margin:0">F3</p></div></body>"#,
            40,
        );
        assert_eq!(out.regions.len(), 1);
        let feed = &out.regions[0];
        assert_eq!(feed.carousels.len(), 1, "the shelf is nested in the feed");
        let shelf = &feed.carousels[0];
        assert_eq!(
            shelf.start, 1,
            "shelf band is buffer-relative (after FEED-TOP)"
        );
        assert!(
            shelf.width > (shelf.right - shelf.left),
            "the shelf overflows"
        );
        // The shelf cards live in the feed's buffer at their strip columns.
        assert!(
            feed.buffer
                .iter()
                .any(|r| r.items.iter().any(|i| i.text.contains("S4"))),
            "shelf cards are in the feed buffer (windowed at render)"
        );
    }

    // ---- the P6 gate: tables (CSS 2.1 §17) ----
    // The cell of a test is the nominal 8×16 px, so 1 col = 8px and a
    // width:100% table in an N-col band is N·8 px wide.

    /// The 0-based (row, col) of the item containing `text`.
    fn cell_at(out: &Output, text: &str) -> (usize, usize) {
        let (r, it) = find(out, text);
        (r, it.col as usize)
    }

    #[test]
    fn table_cells_lay_side_by_side() {
        // The core of §17: cells of one row share the same grid rows in
        // distinct columns — not each `<td>` on its own line.
        let out = lay(
            "<body><table><tr><td>LeftCell</td><td>RightCell</td></tr></table></body>",
            60,
        );
        assert_eq!(
            cell_at(&out, "LeftCell").0,
            cell_at(&out, "RightCell").0,
            "both cells share a row"
        );
        assert!(
            cell_at(&out, "RightCell").1 > cell_at(&out, "LeftCell").1,
            "the second cell is to the right"
        );
    }

    #[test]
    fn table_rows_stack_and_columns_align() {
        let out = lay(
            "<body><table>\
             <tr><td>r1a</td><td>r1b</td></tr>\
             <tr><td>r2a</td><td>r2b</td></tr></table></body>",
            60,
        );
        assert!(
            cell_at(&out, "r2a").0 > cell_at(&out, "r1a").0,
            "rows stack"
        );
        assert_eq!(
            cell_at(&out, "r1a").1,
            cell_at(&out, "r2a").1,
            "col 0 aligns"
        );
        assert_eq!(
            cell_at(&out, "r1b").1,
            cell_at(&out, "r2b").1,
            "col 1 aligns"
        );
    }

    #[test]
    fn a_display_block_table_still_lays_as_a_table() {
        // Markdown CSS forces `display:block` onto a `<table>` (so a wide table
        // scrolls). The `<thead>`/`<tbody>` keep their table displays, so
        // §17.2.1 wraps them in an anonymous table and the cells STILL lay side
        // by side.
        let out = lay(
            "<body><table style=\"display:block\">\
             <thead><tr><th>Command</th><th>Effect</th></tr></thead>\
             <tbody><tr><td>website.com</td><td>opens it</td></tr></tbody></table></body>",
            60,
        );
        assert_eq!(
            cell_at(&out, "Command").0,
            cell_at(&out, "Effect").0,
            "header cells share a row"
        );
        assert!(cell_at(&out, "Effect").1 > cell_at(&out, "Command").1);
        assert_eq!(
            cell_at(&out, "Command").1,
            cell_at(&out, "website.com").1,
            "the header column aligns with the body column"
        );
        assert!(cell_at(&out, "website.com").0 > cell_at(&out, "Command").0);
    }

    #[test]
    fn a_colspan_cell_spans_its_columns() {
        let out = lay(
            "<body><table>\
             <tr><td colspan=\"2\">Header</td></tr>\
             <tr><td>colA</td><td>colB</td></tr></table></body>",
            60,
        );
        assert!(cell_at(&out, "Header").0 < cell_at(&out, "colA").0);
        assert_eq!(
            cell_at(&out, "colA").0,
            cell_at(&out, "colB").0,
            "the two spanned cells share a row"
        );
        assert!(cell_at(&out, "colB").1 > cell_at(&out, "colA").1);
    }

    #[test]
    fn a_rowspan_cell_spans_its_rows() {
        // A top-aligned cell spanning two rows sits beside both; the second
        // row's other cell is below the first row's, in the same column.
        let out = lay(
            "<body><table>\
             <tr><td rowspan=\"2\" style=\"vertical-align:top\">Side</td><td>Top</td></tr>\
             <tr><td>Bottom</td></tr></table></body>",
            60,
        );
        assert_eq!(
            cell_at(&out, "Side").0,
            cell_at(&out, "Top").0,
            "spans from row 0"
        );
        assert!(
            cell_at(&out, "Bottom").0 > cell_at(&out, "Top").0,
            "second row is below"
        );
        assert_eq!(
            cell_at(&out, "Top").1,
            cell_at(&out, "Bottom").1,
            "Top/Bottom share the second column"
        );
        assert!(
            cell_at(&out, "Top").1 > cell_at(&out, "Side").1,
            "Side is the first column"
        );
    }

    #[test]
    fn a_nested_table_lays_out_inside_its_cell() {
        // The slackware nested-table trick: an inner table inside a cell lays
        // out within its cell's column, not collapsed.
        let out = lay(
            "<body><table><tr>\
             <td><table><tr><td>InnerL</td><td>InnerR</td></tr></table></td>\
             <td>Outer</td></tr></table></body>",
            60,
        );
        assert_eq!(cell_at(&out, "InnerL").0, cell_at(&out, "Outer").0);
        assert!(cell_at(&out, "InnerR").1 > cell_at(&out, "InnerL").1);
        assert!(cell_at(&out, "Outer").1 > cell_at(&out, "InnerR").1);
    }

    #[test]
    fn col_elements_size_their_table_columns() {
        // §17.5.2: a `<col width="10%">` of a width:100% table in a 40-col
        // (320px) band is 32px = 4 cols, so the second column starts at col 4.
        let out = lay(
            r#"<body style="margin:0"><table width="100%"><colgroup><col width="10%"><col></colgroup>
                 <tr><td>a</td><td>bb</td></tr></table></body>"#,
            40,
        );
        assert_eq!(cell_at(&out, "bb").1, 4);
    }

    #[test]
    fn col_span_repeats_its_width() {
        // `<col span="2" width="25%">` covers two 25% (80px = 10-col) columns.
        let out = lay(
            r#"<body style="margin:0"><table width="100%"><colgroup><col span="2" width="25%"></colgroup>
                 <tr><td>a</td><td>b</td><td>c</td></tr></table></body>"#,
            40,
        );
        assert_eq!(cell_at(&out, "b").1, 10);
        assert_eq!(cell_at(&out, "c").1, 20);
    }

    #[test]
    fn declared_cell_width_holds_on_a_widthless_table() {
        // §17.5.2.2: a declared column width raises the column's max-content, so
        // an 80px (10-col) first column holds its width even when the TABLE
        // declares none.
        let out = lay(
            r#"<body style="margin:0"><table><tr><td width="80">a</td><td>b</td></tr></table></body>"#,
            40,
        );
        assert_eq!(cell_at(&out, "b").1, 10);
    }

    #[test]
    fn a_narrow_menu_sits_beside_a_wide_content_column() {
        // The slackware.com layout-table pattern: a width:10% menu cell beside
        // an auto-width content cell, both on the same rows.
        let words = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do";
        let out = lay(
            &format!(
                "<body style=\"margin:0\"><table width=\"100%\"><tr valign=\"top\">\
                 <td width=\"10%\">Menu</td><td>{words}</td></tr></table></body>"
            ),
            80,
        );
        assert_eq!(
            cell_at(&out, "Menu").0,
            cell_at(&out, "lorem").0,
            "the menu sits beside the content"
        );
        assert!(
            cell_at(&out, "lorem").1 >= 8,
            "the content column starts past the narrow 10% menu"
        );
    }

    #[test]
    fn bare_cells_default_to_middle_vertical_alignment() {
        // §17.5.4 / Appendix D: `td,th { vertical-align: inherit }` +
        // `tbody { vertical-align: middle }` — a bare cell centers in its band.
        let out = lay(
            "<body><table><tr><td>l1<br>l2<br>l3</td><td>X</td></tr></table></body>",
            40,
        );
        assert_eq!(
            cell_at(&out, "X").0,
            cell_at(&out, "l2").0,
            "the undeclared cell centers"
        );
    }

    #[test]
    fn css_vertical_align_beats_the_valign_attribute() {
        // The `valign` presentational hint is an author-level rule preceding
        // all others, so an author `vertical-align` wins.
        let out = lay(
            "<body><table><tr>\
             <td>l1<br>l2<br>l3</td>\
             <td valign=\"bottom\" style=\"vertical-align:top\">X</td></tr></table></body>",
            40,
        );
        assert_eq!(
            cell_at(&out, "X").0,
            cell_at(&out, "l1").0,
            "CSS top beats the bottom hint"
        );
    }

    #[test]
    fn a_caption_renders_above_the_grid_and_centered() {
        // §17.4: a `table-caption` is a block box above the grid; the UA sheet
        // centers it (`caption { text-align: center }`).
        let out = lay(
            r#"<body style="margin:0"><table style="width:200px"><caption>Cap</caption>
                 <tr><td>cell</td></tr></table></body>"#,
            40,
        );
        assert!(
            cell_at(&out, "Cap").0 < cell_at(&out, "cell").0,
            "the caption is above the grid"
        );
        assert!(
            cell_at(&out, "Cap").1 >= 8,
            "the caption centers in the 200px (25-col) table, not flush left"
        );
    }

    #[test]
    fn caption_side_bottom_renders_below_the_grid() {
        let out = lay(
            r#"<body><table><caption style="caption-side:bottom">Cap</caption>
                 <tr><td>cell</td></tr></table></body>"#,
            40,
        );
        assert!(
            cell_at(&out, "Cap").0 > cell_at(&out, "cell").0,
            "the bottom caption is below the grid"
        );
    }

    #[test]
    fn an_auto_table_shrinks_and_centers_with_align_center() {
        // §17.5.2: a width:auto table shrinks to its content; `align=center`
        // centers it in a wide band (§17.4).
        let out = lay(
            r#"<body style="margin:0"><table align="center"><tr><td>Hi</td></tr></table></body>"#,
            80,
        );
        assert!(
            cell_at(&out, "Hi").1 > 20,
            "the shrunk table centers in the 80-col band, not flush left"
        );
    }

    #[test]
    fn css_padding_suppresses_cellpadding() {
        // The presentational-hint priority: a cell with ANY CSS padding ignores
        // the `cellpadding` attribute, so its content is not inset.
        let out = lay(
            r#"<body style="margin:0"><table cellpadding="8"><tr><td style="padding-bottom:4px">x</td></tr></table></body>"#,
            40,
        );
        assert_eq!(
            cell_at(&out, "x").1,
            0,
            "CSS padding wins — no cellpadding inset"
        );
    }

    #[test]
    fn cellpadding_insets_content_and_widens_the_column() {
        // With no CSS padding, `cellpadding="8"` insets the content by 8px
        // (1 col) and the auto column reserves room for it (content stays
        // unclipped — the width fold in `cell_min_max`).
        let out = lay(
            r#"<body style="margin:0"><table cellpadding="8"><tr><td>xy</td></tr></table></body>"#,
            40,
        );
        assert_eq!(
            cell_at(&out, "xy").1,
            1,
            "content inset by the 8px cellpadding"
        );
        assert!(!absent(&out, "xy"), "the content is not squeezed away");
    }

    #[test]
    fn deeply_nested_tables_still_render_the_innermost_content() {
        // Past MAX_TABLE_DEPTH a table degrades to block-stacked content; the
        // descent terminates and the innermost cell content still renders.
        let mut html = String::from("DEEPEST");
        for i in 0..40 {
            html = format!("<table><tr><td>L{i} {html}</td><td>x</td></tr></table>");
        }
        let out = lay(&format!("<body>{html}</body>"), 80);
        assert!(
            !absent(&out, "DEEPEST"),
            "the innermost content renders past the depth lid"
        );
    }
}
