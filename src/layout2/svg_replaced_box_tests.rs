//! An inline outer `<svg>` in an HTML document is a CSS replaced element
//! (CSS 2 §10.3.2 #inline-replaced-width, SVG 2 coords.html#ViewportSpace):
//! the element itself, styled by its own cascade, paints its background,
//! border and outline around replaced content placed in its content box
//! (CSS 2 Appendix E #painting-order step 7.2.1), and that content clips to
//! the curved content edge (CSS Backgrounds 3 #corner-clipping). These run the
//! static (pre-script) document paths, which used to swap the element for a
//! synthetic `<img>` and so lost its class/id selectors.

use std::collections::HashMap;

use url::Url;

use crate::core::{CssPoint, CssSize};
use crate::dom::{DOCUMENT, Dom, NodeId};
use crate::render::headless::scene_for_html;
use crate::render::vello_cpu::{OwnedRgbaFrame, VelloCpuRenderer};
use crate::render::vello_hybrid::VelloHybridRenderer;
use crate::render::{DisplayCommand, PageHitIndex, Scene};

const SIZE: (f32, f32) = (200., 120.);

/// `.box`: border box 10..72 on both axes (margin 10, border 5, padding 6,
/// content 21..61), outline 7..10/72..75. An `img` rule must not reach it.
/// `#round`: a 60px circle at (100,10) whose square content fills it.
/// `#clip`: border box 100..150 × 80..110 with a content-box-clipped
/// background behind a half-width red rect.
const FIXTURE: &str = r#"<!doctype html><style>
body { margin: 0; background: white }
img { border: 9px solid rgb(255, 0, 0) }
svg.box { display: block; margin: 10px; width: 40px; height: 40px; padding: 6px;
  border: 5px solid rgb(0, 0, 255); background: rgb(0, 255, 0);
  outline: 3px solid rgb(255, 0, 255) }
#round { position: absolute; left: 100px; top: 10px; width: 60px; height: 60px;
  border-radius: 50% }
#clip { position: absolute; left: 100px; top: 80px; width: 40px; height: 20px;
  padding: 5px; background: rgb(0, 255, 0); background-clip: content-box }
</style>
<svg class=box viewBox="0 0 10 10"><rect width="10" height="10" fill="rgb(255,0,0)"/></svg>
<svg id=round viewBox="0 0 10 10" preserveAspectRatio="none"><rect width="10" height="10" fill="rgb(255,0,0)"/></svg>
<svg id=clip viewBox="0 0 20 10" preserveAspectRatio="none"><rect width="10" height="10" fill="rgb(255,0,0)"/></svg>"#;

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const MAGENTA: [u8; 4] = [255, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

fn base() -> Url {
    Url::parse("https://example.test/").unwrap()
}

fn scene() -> Scene {
    scene_for_html(FIXTURE, &base(), CssSize::new(SIZE.0, SIZE.1))
}

fn pixel(frame: &OwnedRgbaFrame, x: usize, y: usize) -> [u8; 4] {
    let at = (y * frame.size.width as usize + x) * 4;
    frame.pixels[at..at + 4].try_into().unwrap()
}

fn check_svg_box_pixels(frame: &OwnedRgbaFrame, renderer: &str) {
    let expect = |x: usize, y: usize, color: [u8; 4], what: &str| {
        let got = pixel(frame, x, y);
        assert!(
            got.iter().zip(color).all(|(a, b)| a.abs_diff(b) <= 2),
            "{renderer}: {what} at ({x},{y}) is {got:?}, expected {color:?}"
        );
    };
    expect(41, 41, RED, "replaced content in the content box");
    for (x, y) in [(18, 41), (64, 41), (41, 18), (41, 64)] {
        expect(x, y, GREEN, "background under the padding");
    }
    for (x, y) in [(12, 41), (69, 41), (41, 12), (41, 69)] {
        expect(x, y, BLUE, "the svg's own 5px border, not img's");
    }
    for (x, y) in [(8, 41), (73, 41), (41, 8), (41, 73)] {
        expect(x, y, MAGENTA, "outline outside the border edge");
    }
    expect(4, 41, WHITE, "nothing beyond the outline");
    expect(130, 40, RED, "rounded content center");
    for (x, y) in [(103, 13), (157, 13), (103, 67), (157, 67)] {
        expect(x, y, WHITE, "content clipped to the curved content edge");
    }
    expect(115, 95, RED, "drawn half of the clipped svg");
    expect(
        135,
        95,
        GREEN,
        "content-box background behind transparent content",
    );
    expect(
        102,
        95,
        WHITE,
        "background-clip: content-box leaves padding bare",
    );
    expect(
        135,
        82,
        WHITE,
        "background-clip: content-box leaves padding bare",
    );
}

#[test]
fn inline_svg_paints_its_box_decorations_with_vello_cpu() {
    let frame = VelloCpuRenderer::new().render_rgba(&scene()).unwrap();
    check_svg_box_pixels(&frame, "Vello CPU");
}

#[test]
fn inline_svg_paints_its_box_decorations_with_hybrid() {
    let Ok(mut renderer) = futures::executor::block_on(VelloHybridRenderer::new_headless()) else {
        eprintln!("Inline svg box regression not exercised: no Hybrid adapter");
        return;
    };
    let frame = renderer.render_rgba(&scene()).unwrap();
    check_svg_box_pixels(&frame, "Hybrid");
}

fn svg_by_class(dom: &Dom, class: &str) -> NodeId {
    dom.descendants(DOCUMENT)
        .find(|&node| dom.tag_name(node) == Some("svg") && dom.attr(node, "class") == Some(class))
        .unwrap()
}

#[test]
fn inline_svg_geometry_and_hit_testing_follow_its_painted_box() {
    // CSSOM View measures the svg's border box; the replaced content paints
    // in its content box; a pointer over its padding or content hits the svg
    // element itself (no synthesized stand-in node).
    let dom = Dom::parse_document(FIXTURE);
    let svg = svg_by_class(&dom, "box");
    let layout = crate::layout2::lay_out_graphical(
        &dom,
        &base(),
        crate::layout2::Viewport::new(SIZE.0, SIZE.1),
        &[],
        &HashMap::new(),
        &crate::layout2::ImageSizes::new(),
    );
    let border_box = layout.boxes[&svg];
    assert_eq!(
        (
            border_box.left,
            border_box.top,
            border_box.width,
            border_box.height
        ),
        (10., 10., 62., 62.)
    );
    let content = layout
        .paint
        .primitives
        .iter()
        .find_map(|command| match command {
            DisplayCommand::Image { rect, node, .. } if *node == svg => Some(*rect),
            _ => None,
        })
        .expect("the svg's replaced content paints");
    assert_eq!(
        (content.x, content.y, content.width, content.height),
        (21., 21., 40., 40.)
    );
    let mut index = PageHitIndex::new(&layout.paint);
    for point in [CssPoint::new(41., 41.), CssPoint::new(18., 41.)] {
        let hits = index.query(
            &layout.paint,
            CssSize::new(SIZE.0, SIZE.1),
            CssPoint::default(),
            point,
            0.,
        );
        assert_eq!(hits.first().map(|hit| hit.node), Some(svg), "{point:?}");
    }
}

#[test]
fn terminal_inline_svg_sits_in_its_content_box() {
    // The terminal's static renderer lays the same replaced box out in
    // 8×16 cells: border 8px + padding 16px = 3 columns, border 8px +
    // padding 8px = 1 row, and the 48×32px content box is 6×2 cells. A type
    // selector for `img` does not match the svg.
    let html = r#"<style>body{margin:0} img{border:16px solid red}
        svg.box{display:block;width:48px;height:32px;padding:8px 16px;
        border:8px solid blue;background:green}</style>
        <svg class=box viewBox="0 0 3 2"><rect width="3" height="2"/></svg>"#;
    let svg = svg_by_class(&Dom::parse_document(html), "box");
    // The first pass discovers the svg's image; the frontend decodes it and
    // lays the page out again.
    let pending = crate::http::parse(
        &base(),
        "text/html",
        html.as_bytes(),
        40,
        24,
        &crate::layout2::ImageSizes::new(),
    );
    let source = pending
        .image_urls
        .iter()
        .find(|source| source.starts_with("data:image/svg+xml"))
        .cloned()
        .expect("the inline svg's image is discovered");
    let decoded = crate::layout2::ImageSizes::from([(source.clone(), (48, 32))]);
    let doc = crate::http::parse(&base(), "text/html", html.as_bytes(), 40, 24, &decoded);
    let image = doc
        .rows
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            line.items
                .iter()
                .find(|item| item.image.as_deref() == Some(source.as_str()))
                .map(|item| (row, item.col, item.width, item.height, item.node))
        })
        .unwrap_or_else(|| panic!("no svg image item: {:#?}", doc.rows));
    assert_eq!(image, (1, 3, 6, 2, svg));
}
