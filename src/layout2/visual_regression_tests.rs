//! Reduced standards regressions from real-page desktop rendering failures.
use super::*;
use crate::core::{CssPoint, CssSize};
use crate::render::{ImageStore, headless};

fn by_id(dom: &Dom, id: &str) -> NodeId {
    dom.descendants(crate::dom::DOCUMENT)
        .find(|&node| dom.attr(node, "id") == Some(id))
        .unwrap()
}

#[test]
fn relative_image_tiles_preserve_sizing_offsets_and_overflow_crop() {
    // CSS 2.2 §9.4.3, §10.3.2/§10.6.2 and §11.1: size against the
    // containing block, then offset without shrinking to the overflow clip.
    let source = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='300' height='300'%3E%3Cpath fill='red' d='M0 0h300v100H0z'/%3E%3Cpath fill='lime' d='M0 100h300v100H0z'/%3E%3Cpath fill='blue' d='M0 200h300v100H0z'/%3E%3Cpath fill='yellow' d='M100 100h100v100H100z'/%3E%3C/svg%3E";
    let html = format!(
        "<style>body{{margin:0;background:white}}.tile{{width:100px;height:100px;overflow:hidden;position:relative}}img{{width:300%;height:300%;position:relative}}</style><div class=tile><img id=image src=\"{source}\" style='left:-100%;top:-100%'></div><div id=after>After</div>"
    );
    let dom = Dom::parse_document(&html);
    let page = lay_out_graphical(
        &dom,
        &Url::parse("https://example.test/").unwrap(),
        Viewport::new(160., 160.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    let image = by_id(&dom, "image");
    let rect = page
        .paint
        .primitives
        .iter()
        .find_map(|c| match c {
            crate::render::DisplayCommand::Image { node, rect, .. } if *node == image => {
                Some(*rect)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!((rect.width, rect.height), (300., 300.), "{rect:?}");
    assert_eq!((rect.x, rect.y), (-100., -100.), "{rect:?}");
    let frame = headless::render_paint(&page.paint, CssSize::new(160., 160.)).unwrap();
    for (x, y) in [(10, 10), (50, 50), (90, 90)] {
        let pixel = &frame.pixels[(y * 160 + x) * 4..][..4];
        assert!(
            pixel[0] > 240 && pixel[1] > 240 && pixel[2] < 10,
            "{x},{y}: {pixel:?}"
        );
    }
    let outside = &frame.pixels[(50 * 160 + 120) * 4..][..4];
    assert_eq!(outside, &[255, 255, 255, 255]);
}

#[test]
fn carousel_inline_paint_and_hits_survive_retained_scroll_and_hover() {
    // CSS Overflow 3 #scrolling and CSS Transforms 1 #transform-rendering:
    // scrolling moves contents through a stationary scrollport. A card's
    // own overflow clip moves with that card, with or without a hover layer.
    let card = r#"<div class=card><a href='/card'><img width=60 height=40
        src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='60' height='40'%3E%3Cpath fill='red' d='M0 0h60v40H0z'/%3E%3C/svg%3E">TEXT</a>
        <b></b></div>"#;
    let html = format!(
        "<style>body{{margin:0;background:white}}#carousel{{display:flex;position:relative;
         width:100px;height:100px;overflow-x:auto;overflow-y:hidden}}
         .card{{position:relative;flex:0 0 100px;height:100px;overflow:hidden;background:#eee}}
         .card:hover{{z-index:2}}a{{color:blue;font:16px monospace}}
         b{{position:absolute;top:80px;left:0;width:20px;height:20px;background:lime}}</style>
         <div id=carousel>{card}{card}{card}</div>"
    );
    let mut dom = Dom::parse_document(&html);
    let carousel = by_id(&dom, "carousel");
    let cards = dom.children(carousel);
    let base = Url::parse("https://example.test/").unwrap();
    let viewport = CssSize::new(140., 120.);
    let layout = |dom: &Dom| {
        lay_out_graphical(
            dom,
            &base,
            Viewport::new(140., 120.),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        )
    };
    let mut page = layout(&dom).paint;
    let mut hit_scene = headless::scene_for_dom(
        &dom,
        &base,
        viewport,
        &[],
        &HashMap::new(),
        &HashMap::new(),
        ImageStore::default(),
    );
    let first = headless::render_paint(&page, viewport).unwrap();
    let red = first
        .pixels
        .chunks_exact(4)
        .filter(|p| p[0] > 200 && p[1] < 20 && p[2] < 20)
        .count();
    let blue = first
        .pixels
        .chunks_exact(4)
        .filter(|p| p[2] > 100 && p[0] < 100 && p[1] < 100)
        .count();
    assert!(
        red > 1500 && blue > 30,
        "fixture must paint image and text: {red}, {blue}"
    );
    let passes = layout_pass_count();
    for offset in [100., 200., 100., 0.] {
        page.scroll_containers
            .iter_mut()
            .find(|c| c.node == carousel)
            .unwrap()
            .offset
            .x = offset;
        let frame = headless::render_paint(&page, viewport).unwrap();
        let difference = headless::compare_rgba(&first, &frame, 0).unwrap();
        assert_eq!(
            difference.fraction_over_tolerance, 0.,
            "retained scroll to {offset} must preserve all card pixels: {difference:?}"
        );
        hit_scene.primitives.clear();
        hit_scene.append_page(&page, CssPoint::default());
        let hit = hit_scene
            .page_hit_at(CssPoint::new(20., 20.))
            .expect("scrolled image link");
        assert_eq!(
            hit.node,
            dom.children(dom.children(cards[(offset / 100.) as usize])[0])[0]
        );
        assert!(hit.link.is_some());
        assert!(
            hit_scene.page_hit_at(CssPoint::new(120., 20.)).is_none(),
            "link outside the scrollport must remain clipped"
        );
    }
    assert_eq!(
        layout_pass_count(),
        passes,
        "scroll-only pixels and hits reuse layout"
    );
    // A fresh paint after entering/leaving :hover must match retained scrolling.
    dom.set_scroll_pos(carousel, 0., 100., true);
    for hovered in [Some(cards[1]), None] {
        dom.set_hover_chain(hovered);
        let frame = headless::render_paint(&layout(&dom).paint, viewport).unwrap();
        let difference = headless::compare_rgba(&first, &frame, 0).unwrap();
        assert_eq!(
            difference.fraction_over_tolerance, 0.,
            "hover={hovered:?} must not reveal or hide content: {difference:?}"
        );
    }
}

#[test]
fn carousel_inline_clips_follow_nested_scroll_coordinates() {
    for direction in ["row", "column"] {
        for clip_style in [
            "overflow:hidden",
            "overflow:hidden;border-radius:12px",
            "overflow-x:clip;overflow-y:visible",
        ] {
            let card = "<div class=card><a href='/item'>SCROLLED TEXT</a><div style='height:40px;background:red'></div></div>";
            let dom = Dom::parse_document(&format!(
                "<style>body{{margin:0;background:white}}#outer{{width:100px;height:100px;overflow:auto;transform:translate(10px,10px)}}
                 #carousel{{width:100px;height:100px;display:flex;flex-direction:{direction};overflow:auto}}
                 .card{{position:relative;flex:none;width:100px;height:100px;background:#eee;{clip_style}}}
                 a{{font:16px monospace;color:blue}}</style>
                 <div id=outer><div style='height:60px'></div><div id=carousel>{card}{card}{card}</div></div>"
            ));
            let outer = by_id(&dom, "outer");
            let carousel = by_id(&dom, "carousel");
            let mut page = lay_out_graphical(
                &dom,
                &Url::parse("https://example.test/").unwrap(),
                Viewport::new(140., 140.),
                &[],
                &HashMap::new(),
                &HashMap::new(),
            )
            .paint;
            page.scroll_containers
                .iter_mut()
                .find(|c| c.node == outer)
                .unwrap()
                .offset
                .y = 60.;
            let first = headless::render_paint(&page, CssSize::new(140., 140.)).unwrap();
            assert!(
                first
                    .pixels
                    .chunks_exact(4)
                    .filter(|p| p[2] > 100 && p[0] < 100 && p[1] < 100)
                    .count()
                    > 30
            );
            for offset in [100., 200., 0.] {
                let scroll = page
                    .scroll_containers
                    .iter_mut()
                    .find(|c| c.node == carousel)
                    .unwrap();
                if direction == "row" {
                    scroll.offset.x = offset;
                } else {
                    scroll.offset.y = offset;
                }
                let frame = headless::render_paint(&page, CssSize::new(140., 140.)).unwrap();
                let difference = headless::compare_rgba(&first, &frame, 0).unwrap();
                assert_eq!(
                    difference.fraction_over_tolerance, 0.,
                    "{direction}, {clip_style}, {offset}: {difference:?}"
                );
            }
        }
    }
}

#[test]
fn contenteditable_keeps_ordinary_css_paint() {
    let dom = Dom::parse_document(
        "<style>body{margin:0;background:lime}div{width:120px;height:60px}</style><div contenteditable=true></div>",
    );
    let scene = headless::scene_for_dom(
        &dom,
        &Url::parse("https://example.test/").unwrap(),
        CssSize::new(140., 80.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        ImageStore::default(),
    );
    let frame = crate::render::vello_cpu::VelloCpuRenderer::new()
        .render_rgba(&scene)
        .unwrap();
    for (x, y) in [(0, 30), (60, 30), (119, 30)] {
        let i = (y * 140 + x) * 4;
        assert_eq!(
            &frame.pixels[i..i + 4],
            &[0, 255, 0, 255],
            "no generated native surface at {x},{y}"
        );
    }
}

#[test]
fn generated_text_keeps_pseudo_color_across_adjacent_runs_and_block_boxes() {
    let dom = Dom::parse_document(
        "<style>body{color:blue}p:before{content:'before ';color:red}p:after{content:' after';color:green}div:before{content:'block';display:block;color:lime}</style><p>middle</p><div></div>",
    );
    let layout = lay_out_graphical(
        &dom,
        &Url::parse("https://example.test/").unwrap(),
        Viewport::new(400.0, 300.0),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    for (text, color) in [
        ("before", (255, 0, 0)),
        ("middle", (0, 0, 255)),
        ("after", (0, 128, 0)),
        ("block", (0, 255, 0)),
    ] {
        assert!(layout.paint.primitives.iter().any(|p| matches!(p,
            crate::render::DisplayCommand::GlyphRun { shaped, color:crate::render::PaintColor::Rgba(r,g,b,255), .. }
            if shaped.text.contains(text) && (*r,*g,*b)==color)), "{text} must retain its own paint style");
    }
}

#[test]
fn bitmap_color_emoji_glyphs_have_visible_pixels() {
    // CSS Fonts 4 #font-palette-prop: a supported color font's normal
    // palette paints its colors, not an empty outline. Noto Color Emoji's
    // CBDT glyphs contain PNG bitmaps; shaping alone cannot render them.
    let dom = Dom::parse_document(
        "<style>body{margin:0;background:white;font:48px 'Noto Color Emoji'}</style>👍",
    );
    let scene = headless::scene_for_dom(
        &dom,
        &Url::parse("https://example.test/").unwrap(),
        CssSize::new(120., 100.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        ImageStore::default(),
    );
    let pixels = crate::render::vello_cpu::VelloCpuRenderer::new()
        .render_rgba(&scene)
        .unwrap()
        .pixels;
    let colored = pixels
        .chunks_exact(4)
        .filter(|p| p[3] > 128 && p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]) > 50)
        .count();
    assert!(
        colored > 100,
        "the emoji must paint actual color pixels: {colored}"
    );
}

#[test]
#[ignore = "offline SVG diagnostic; set TRUST_CAPTURE_DOM and TRUST_CAPTURE_DIR"]
fn captured_vega_svg_resource_diagnostic() {
    let path = std::env::var("TRUST_CAPTURE_DOM").unwrap();
    let out = std::path::PathBuf::from(std::env::var("TRUST_CAPTURE_DIR").unwrap());
    let html = std::fs::read_to_string(path).unwrap();
    let mut dom = Dom::parse_document(&html);
    dom.set_viewport_px(948., 1024.);
    let base = Url::parse("https://openai.com/").unwrap();
    for (index, svg) in dom
        .descendants(crate::dom::DOCUMENT)
        .filter(|&n| dom.tag_name(n) == Some("svg") && dom.attr(n, "class") == Some("marks"))
        .enumerate()
    {
        let Some((source, _)) = dom.svg_image_data(svg, Some(&base)) else {
            eprintln!(
                "chart {index} not renderable, hidden={}",
                dom.is_hidden(svg)
            );
            continue;
        };
        let bytes = crate::img::decode_data_url(&source).unwrap();
        std::fs::write(out.join(format!("chart-resource-{index}.svg")), &bytes).unwrap();
        match crate::img::decode(&bytes) {
            Ok((image, _)) => {
                let rgba = image.to_rgba8();
                eprintln!(
                    "chart {index}: {}x{}, visible pixels={}",
                    rgba.width(),
                    rgba.height(),
                    rgba.pixels().filter(|p| p[3] > 0).count()
                );
                rgba.save(out.join(format!("chart-resource-{index}.png")))
                    .unwrap();
            }
            Err(error) => eprintln!("chart {index} decode: {error}"),
        }
    }
}

#[test]
fn replaced_image_pixels_clip_to_the_curved_content_edge() {
    // CSS Backgrounds 3 §4.2/§4.3: replaced content clips even when overflow
    // is visible; subtract the used border and padding from each corner.
    let base = Url::parse("https://example.test/").unwrap();
    for (decoration, corner, expected) in [
        ("", (2, 2), [0, 0, 255, 255]),
        (
            "border:10px solid lime;padding:10px;background:yellow",
            (25, 25),
            [255, 255, 0, 255],
        ),
    ] {
        let html = format!(
            "<style>body{{margin:0;background:blue}}img{{display:block;width:100px;height:100px;border-radius:50px;{decoration}}}</style><img id=picture src='pixel.png'>"
        );
        let dom = Dom::parse_document(&html);
        let source = base.join("pixel.png").unwrap().to_string();
        let store = ImageStore::default();
        store.insert(
            crate::render::ImageHandle::for_source(&source),
            crate::render::ImageResource {
                width: 1,
                height: 1,
                rgba: std::sync::Arc::from([255, 0, 0, 255]),
                has_alpha: false,
            },
        );
        let scene = headless::scene_for_dom(
            &dom,
            &base,
            CssSize::new(200., 200.),
            &[],
            &HashMap::new(),
            &HashMap::from([(source, (1, 1))]),
            store,
        );
        let frame = crate::render::vello_cpu::VelloCpuRenderer::new()
            .render_rgba(&scene)
            .unwrap();
        let sample = |x: usize, y: usize| &frame.pixels[(y * 200 + x) * 4..(y * 200 + x) * 4 + 4];
        assert_eq!(sample(corner.0, corner.1), expected, "{decoration}");
        assert_eq!(sample(60, 60), [255, 0, 0, 255], "image center survives");
    }
}

#[test]
fn rounded_overflow_clips_transformed_descendants_and_their_hits() {
    // CSS Overflow 3 §3.1.2 and Backgrounds 3 §4.3. The ancestor's
    // padding-edge curve stays in its coordinate system, outside the child's
    // transform. The hidden/clip cases must not gain wheel scroll regions.
    let base = Url::parse("https://example.test/").unwrap();
    for overflow in [
        "hidden",
        "clip",
        "auto",
        "scroll",
        "clip visible",
        "visible",
    ] {
        for transformed in [false, true] {
            let dom = Dom::parse_document(&format!(
                "<style>body{{margin:0;background:blue}}#round{{position:relative;width:100px;height:100px;border:10px solid lime;border-radius:50px;overflow:{overflow}}}a{{display:block;width:100px;height:100px;{}}}img{{display:block;width:100px;height:100px}}</style><div id=round><a href='/next'><img src='pixel.png'></a></div>",
                if transformed {
                    "position:relative;left:50px;top:50px;transform:translate(-50px,-50px)"
                } else {
                    ""
                }
            ));
            let source = base.join("pixel.png").unwrap().to_string();
            let store = ImageStore::default();
            store.insert(
                crate::render::ImageHandle::for_source(&source),
                crate::render::ImageResource {
                    width: 1,
                    height: 1,
                    rgba: std::sync::Arc::from([255, 0, 0, 255]),
                    has_alpha: false,
                },
            );
            let scene = headless::scene_for_dom(
                &dom,
                &base,
                CssSize::new(200., 200.),
                &[],
                &HashMap::new(),
                &HashMap::from([(source, (1, 1))]),
                store,
            );
            let rounded = !matches!(overflow, "visible" | "clip visible");
            assert_eq!(
                scene.page_hit_at(CssPoint::new(12., 12.)).is_none(),
                rounded,
                "corner hit: {overflow}, transformed={transformed}"
            );
            assert!(
                scene.page_hit_at(CssPoint::new(60., 60.)).is_some(),
                "center hit: {overflow}"
            );
            let frame = crate::render::vello_cpu::VelloCpuRenderer::new()
                .render_rgba(&scene)
                .unwrap();
            let sample =
                |x: usize, y: usize| &frame.pixels[(y * 200 + x) * 4..(y * 200 + x) * 4 + 4];
            assert_eq!(
                sample(12, 12),
                if rounded {
                    [0, 0, 255, 255]
                } else {
                    [255, 0, 0, 255]
                },
                "corner paint: {overflow}, transformed={transformed}"
            );
            assert_eq!(sample(60, 60), [255, 0, 0, 255], "center paint: {overflow}");
        }
    }
}

#[test]
fn rounded_overflow_respects_positioned_containing_blocks() {
    let base = Url::parse("https://example.test/").unwrap();
    for (position, container, clipped) in [
        ("absolute", "", false),
        ("absolute", "position:relative", true),
        ("fixed", "position:relative", false),
        ("fixed", "transform:translate(0px,0px)", true),
    ] {
        let dom = Dom::parse_document(&format!(
            "<body style='margin:0;position:relative;background:blue'><div style='width:100px;height:100px;overflow:hidden;border-radius:50px;{container}'><a href='/next' style='position:{position};left:0;top:0;width:100px;height:100px;background:red'></a></div></body>"
        ));
        let scene = headless::scene_for_dom(
            &dom,
            &base,
            CssSize::new(200., 200.),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            ImageStore::default(),
        );
        let frame = crate::render::vello_cpu::VelloCpuRenderer::new()
            .render_rgba(&scene)
            .unwrap();
        let sample = |x: usize, y: usize| &frame.pixels[(y * 200 + x) * 4..(y * 200 + x) * 4 + 4];
        assert_eq!(
            sample(2, 2),
            if clipped {
                [0, 0, 255, 255]
            } else {
                [255, 0, 0, 255]
            },
            "{position}, {container}"
        );
        assert_eq!(
            sample(50, 50),
            [255, 0, 0, 255],
            "center: {position}, {container}"
        );
    }
}

#[test]
fn graphical_infinite_and_huge_radii_keep_their_rounded_shape() {
    use crate::render::{DisplayCommand, PaintShape};
    for radius in [
        "calc(infinity * 1px)",
        "3e38px",
        "min(3e38px, calc(infinity * 1px))",
    ] {
        let dom = Dom::parse_document(&format!(
            "<style>body{{margin:0}}div{{width:120px;height:40px;background:red;border-radius:{radius}}}</style><div></div>"
        ));
        let layout = lay_out_graphical(
            &dom,
            &Url::parse("https://example.test/").unwrap(),
            Viewport::new(160., 80.),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        let radii = layout
            .paint
            .primitives
            .iter()
            .find_map(|command| match command {
                DisplayCommand::Fill {
                    shape: PaintShape::RoundedRect { rect, radii },
                    ..
                } if rect.width == 120. && rect.height == 40. => Some(radii),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{radius}: pill must remain round"));
        assert!(
            radii
                .corners
                .iter()
                .all(|&(x, y)| (x - 20.).abs() < 0.01 && (y - 20.).abs() < 0.01),
            "{radius}: {:?}",
            radii.corners
        );
    }
}

#[test]
fn graphical_radii_and_sticky_offsets_resolve_css_lengths() {
    use crate::render::{DisplayCommand, PaintShape};
    let mut dom = Dom::parse_document(
        r#"<style>
      html {font-size:20px} body {margin:0}
      #pill {width:140px;height:36px;border-radius:2.5rem;background:red}
      #ellipse {width:200px;height:100px;border-radius:calc(25% + 1em);font-size:10px;background:blue}
      #scroll {width:300px;height:200px;overflow:auto}
      #sticky {position:sticky;top:calc(2rem + 10%);left:10%;width:50px;height:20px}
      #space {height:700px}
    </style><div id="pill"></div><div id="ellipse"></div><div id="scroll"><div id="sticky"></div><div id="space"></div></div>"#,
    );
    dom.set_viewport_px(800., 600.);
    let layout = lay_out_graphical(
        &dom,
        &Url::parse("https://example.test/").unwrap(),
        Viewport::new(800., 600.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    let rounded: Vec<_> = layout
        .paint
        .primitives
        .iter()
        .filter_map(|command| match command {
            DisplayCommand::Fill {
                shape: PaintShape::RoundedRect { rect, radii },
                ..
            } => Some((*rect, radii)),
            _ => None,
        })
        .collect();
    let pill = rounded
        .iter()
        .find(|(rect, _)| rect.width == 140.)
        .expect("rem pill painted round");
    assert_eq!(pill.1.corners, [(18., 18.); 4]);
    let ellipse = rounded
        .iter()
        .find(|(rect, _)| rect.width == 200.)
        .expect("calculated ellipse");
    assert_eq!(ellipse.1.corners, [(60., 35.); 4]);
    let sticky = layout
        .paint
        .sticky_constraints
        .iter()
        .find(|s| s.node == by_id(&dom, "sticky"))
        .unwrap();
    assert_eq!(sticky.container, Some(by_id(&dom, "scroll")));
    assert_eq!(sticky.insets, [Some(60.), None, None, Some(30.)]);
}

#[test]
fn sticky_links_stop_at_their_containing_block() {
    use crate::render::page_element_hits_at;
    let mut dom = Dom::parse_document(
        r#"<style>
      body {margin:0} #before {height:100px}
      section {height:300px;padding:20px}
      #sticky {display:block;position:sticky;top:20px;height:60px;width:100px;margin:10px;background:red}
      footer {height:1000px}
    </style><div id="before"></div><section><a id="sticky" href="/chapter">Chapter</a></section><footer>Footer</footer>"#,
    );
    dom.set_viewport_px(400., 200.);
    let layout = lay_out_graphical(
        &dom,
        &Url::parse("https://example.test/").unwrap(),
        Viewport::new(400., 200.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    let node = by_id(&dom, "sticky");
    let constraint = layout
        .paint
        .sticky_constraints
        .iter()
        .find(|s| s.node == node)
        .unwrap();
    assert_eq!(
        constraint.movement[2], 220.,
        "bottom margin stays in the section content box"
    );
    let hit = |scroll, y| {
        page_element_hits_at(
            &layout.paint,
            CssSize::new(400., 200.),
            CssPoint::new(0., scroll),
            CssPoint::new(40., y),
        )
        .iter()
        .any(|h| h.node == node)
    };
    assert!(
        hit(150., 30.),
        "sticky link follows the viewport while inside the section"
    );
    assert!(
        !hit(500., 30.),
        "sticky link must not overlap or intercept the footer"
    );
}

#[test]
fn force_hidden_generated_links_do_not_gain_hit_regions() {
    let dom = Dom::parse_document(
        r#"<style>
      body {margin:0} .card {position:relative;width:200px;height:150px}
      a {visibility:force-hidden;display:block;width:200px;height:150px}
      a::after {content:'';position:absolute;inset:0;visibility:visible}
      </style><div class="card"><a id="link" href="/hidden">Hidden</a></div>"#,
    );
    let scene = headless::scene_for_dom(
        &dom,
        &Url::parse("https://example.test/").unwrap(),
        CssSize::new(300., 300.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        ImageStore::default(),
    );
    assert!(
        !scene
            .page_hit_at(CssPoint::new(80., 60.))
            .is_some_and(|hit| hit.link.is_some())
    );
}

#[test]
#[ignore = "offline captured-page diagnostic; set TRUST_CAPTURE_DIR"]
fn captured_page_computed_style_diagnostic() {
    let dir = std::path::PathBuf::from(std::env::var("TRUST_CAPTURE_DIR").unwrap());
    let html = std::fs::read_to_string(dir.join("reference-dom.html")).unwrap();
    let css: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("reference-raw-css.json")).unwrap())
            .unwrap();
    let mut dom = Dom::parse_document(&html);
    dom.set_viewport_px(960., 1024.);
    let sheets = css
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["href"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("https://openai.com")
                    .to_string(),
                s["text"].as_str().unwrap().to_string(),
            )
        })
        .collect::<Vec<_>>();
    dom.attach_external_sheets(&sheets);
    let base =
        Url::parse("https://openai.com/index/research-acceleration-view-inside-openai/").unwrap();
    let layout = lay_out_graphical(
        &dom,
        &base,
        Viewport::new(960., 1024.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    for node in dom.descendants(crate::dom::DOCUMENT) {
        if matches!(dom.tag_name(node), Some("html" | "footer"))
            || dom.attr(node, "id") == Some("QvdJZvfqq1TbrRIUBSg8y")
            || dom.attr(node, "data-analytics") == Some("top-nav-try-chatgpt-3-0")
            || dom
                .attr(node, "class")
                .is_some_and(|s| s.contains("text-nav-header") && s.contains("max-w-container"))
        {
            eprintln!(
                "NODE {node} {:?} {:?}",
                dom.tag_name(node),
                dom.attr(node, "class")
            );
            eprintln!("  box: {:?}", layout.boxes.get(&node));
            for prop in [
                "color",
                "background-color",
                "border-top-left-radius",
                "font-size",
                "font-family",
                "display",
                "flex-direction",
                "--color-primary-100",
                "--type-nav-header-size",
            ] {
                eprintln!("  {prop}: {:?}", dom.computed_value_resolved(node, prop));
            }
        }
        if dom.tag_name(node) == Some("footer") {
            for child in dom.descendants(node).filter(|&n| {
                dom.attr(n, "class")
                    .is_some_and(|s| s.contains("@md:flex-row"))
            }) {
                assert_eq!(
                    dom.computed_value_resolved(child, "flex-direction")
                        .as_deref(),
                    Some("row")
                );
                eprintln!("  footer columns: {:?}", layout.boxes.get(&child));
            }
        }
    }
}

#[test]
fn container_queries_use_content_boxes_and_recompute_after_resize() {
    let mut dom = Dom::parse_document(
        r#"<style>
        body {margin:0} .container {container-type:inline-size;box-sizing:border-box;padding:20px;border:5px solid;width:100%}
        @layer base { .items {display:flex;flex-direction:column} }
        @layer utilities { @container (min-width:700px) { .items {flex-direction:row} } }
        .items > div {width:100px;height:30px}
        </style><div class="container"><div class="items" id="items"><div id="a"></div><div id="b"></div></div></div>"#,
    );
    let base = Url::parse("https://example.test/").unwrap();
    for (width, expected) in [(800., "row"), (720., "column"), (800., "row")] {
        dom.set_viewport_px(width, 500.);
        let layout = lay_out_graphical(
            &dom,
            &base,
            Viewport::new(width, 500.),
            &[],
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(
            dom.computed_value_resolved(by_id(&dom, "items"), "flex-direction")
                .as_deref(),
            Some(expected)
        );
        let a = &layout.boxes[&by_id(&dom, "a")];
        let b = &layout.boxes[&by_id(&dom, "b")];
        assert_eq!(a.top == b.top, expected == "row", "{width}: {a:?} {b:?}");
    }
}

#[test]
fn container_queries_select_eligible_ancestors_and_keep_nested_conditions() {
    let mut dom = Dom::parse_document(
        r#"<style>
      body {margin:0} #outer {container:Page / inline-size;width:800px;font-size:20px}
      #inner {container:Card / inline-size;width:300px;font-size:10px}
      .target {height:20px;width:20px;color:blue}
      @container (width > 500px) {.target {color:red}}
      @container Page (width >= 40em) {
        @container Card (200px < width <= 30em) {.target {width:80px}}
      }
      @container page (width > 1px) {.target {height:99px}}
      @container not (unsupported:1) {.target {height:100px}}
      #inner { @container Page (min-width:50rem) {height:120px} }
      #inner::after {content:'';position:absolute;width:1px;height:1px}
      @container Card (width = 300px) {#inner::after {width:30px}}
      </style><div id="outer"><div id="inner"><div class="target" id="target"></div></div></div>"#,
    );
    dom.set_viewport_px(960., 500.);
    let base = Url::parse("https://example.test/").unwrap();
    let layout = lay_out_graphical(
        &dom,
        &base,
        Viewport::new(960., 500.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    let target = by_id(&dom, "target");
    assert_eq!(
        dom.computed_value_resolved(target, "color").as_deref(),
        Some("blue")
    );
    assert_eq!(layout.boxes[&target].width, 80.);
    assert_eq!(layout.boxes[&target].height, 20.);
    assert_eq!(layout.boxes[&by_id(&dom, "inner")].height, 120.);
    assert_eq!(
        dom.pseudo_layout_value(by_id(&dom, "inner"), crate::dom::PseudoEl::After, "width")
            .as_deref(),
        Some("30px")
    );
}

#[test]
fn size_containment_excludes_descendant_intrinsic_width() {
    let mut dom = Dom::parse_document(
        r#"<style>
      body {margin:0} #size {container-type:size;width:200px;padding:10px}
      #inline {container-type:inline-size;display:inline-block;padding:10px}
      .child {width:500px;height:80px}
      @container (min-height:1px) {.child {color:red}}
      </style><div id="size"><div class="child"></div></div><div id="inline"><div class="child"></div></div>"#,
    );
    dom.set_viewport_px(800., 500.);
    let base = Url::parse("https://example.test/").unwrap();
    let layout = lay_out_graphical(
        &dom,
        &base,
        Viewport::new(800., 500.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    assert_eq!(layout.boxes[&by_id(&dom, "size")].height, 20.);
    assert_eq!(layout.boxes[&by_id(&dom, "inline")].width, 20.);
}

#[test]
fn escaped_selector_quotes_do_not_swallow_following_theme_rules() {
    let dom = Dom::parse_document(
        r#"<style>
        @layer utilities {
          .after\:content-\[\'\'\]::after { content:'' }
          .after\:content-\[\"\"\]::after { content:"" }
          .brace\} {color:red}
        }
        :root {--ink:#123456;--font:13px}
        a {color:var(--ink);font-size:var(--font);background:#eee}
        </style><a id="target" class="after:content-['']" href="/">Link</a>"#,
    );
    let target = by_id(&dom, "target");
    assert_eq!(
        dom.computed_value_resolved(target, "color").as_deref(),
        Some("#123456")
    );
    assert_eq!(
        dom.computed_value_resolved(target, "font-size").as_deref(),
        Some("13px")
    );
}

#[test]
fn empty_generated_stretched_link_covers_sibling_media() {
    for (origin, pseudo, expected) in [
        ("auto", "auto", true),
        ("auto", "none", false),
        ("none", "auto", true),
    ] {
        let dom = Dom::parse_document(&format!(
            r#"<style>
            body {{margin:0}} .card {{position:relative;width:200px}}
            .media {{height:120px;background:green}}
            a {{display:block;height:30px;pointer-events:{origin}}}
            a::after {{position:absolute;inset:0;pointer-events:{pseudo}}}
            @layer utilities {{ .after\:content-\[\"\"\]::after,.after\:content-\[\'\'\]::after {{ --tw-content:"";content:var(--tw-content) }} }}
            </style><div class="card"><div class="media"></div><a id="link" class="after:content-['']" href="/article">Read</a></div>"#
        ));
        let base = Url::parse("https://example.test/").unwrap();
        let scene = headless::scene_for_dom(
            &dom,
            &base,
            CssSize::new(300., 300.),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            ImageStore::default(),
        );
        let hit = scene.page_hit_at(CssPoint::new(80., 60.));
        assert_eq!(
            hit.as_ref()
                .is_some_and(|h| h.node == by_id(&dom, "link") && h.link.is_some()),
            expected,
            "{origin}/{pseudo}: {hit:?}"
        );
    }
}

#[test]
fn pseudo_custom_properties_override_inherit_and_handle_cycles() {
    let dom = Dom::parse_document(
        r#"<style>
        #origin {--size:20px;--inherited:var(--size)}
        #origin::before {--size:30px;content:'x';width:var(--size);height:var(--inherited)}
        #origin::after {--a:var(--b);--b:var(--a);content:var(--a,'fallback');width:var(--a,40px)}
        </style><div id="origin"></div>"#,
    );
    let origin = by_id(&dom, "origin");
    use crate::dom::PseudoEl::{After, Before};
    assert_eq!(
        dom.pseudo_layout_value(origin, Before, "width").as_deref(),
        Some("30px")
    );
    assert_eq!(
        dom.pseudo_layout_value(origin, Before, "height").as_deref(),
        Some("20px")
    );
    assert_eq!(
        dom.pseudo_layout_value(origin, After, "width").as_deref(),
        Some("40px")
    );
    assert_eq!(
        dom.pseudo_content(origin, After).as_deref(),
        Some("fallback")
    );
}

#[test]
fn svg_presentation_dimensions_survive_flex_sizing() {
    // SVG 2 §6.6 and §8.12: explicit presentation dimensions are author
    // sizing inputs, not the initial auto/100% SVG viewport size.
    let dom = Dom::parse_document(
        r#"<body style="margin:0">
      <div style="display:flex;width:40px;height:40px;align-items:center;justify-content:center">
        <svg id="icon" width="16" height="16" viewBox="0 0 16 16"><path d="M0 0h16v16H0z"/></svg>
      </div></body>"#,
    );
    let base = Url::parse("https://example.test/").unwrap();
    let layout = lay_out_graphical(
        &dom,
        &base,
        Viewport::new(200., 200.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    let icon = by_id(&dom, "icon");
    let rect = layout.boxes.get(&icon).unwrap();
    assert_eq!((rect.width, rect.height), (16., 16.), "{rect:?}");
}

#[test]
fn clip_path_closed_drawer_clips_paint_and_hits_without_changing_geometry() {
    // CSS Masking 1 §5: clipping affects all descendant paint/hits, not layout.
    let dom = Dom::parse_document(
        r#"<style>.closed {clip-path:inset(0 0 100% 0)}</style>
      <body style="margin:0;background:blue"><div id="drawer" class="closed"
      style="position:fixed;inset:0;background:red"><a href="/" style="display:block;width:100px;height:100px">Hidden navigation</a></div></body>"#,
    );
    let base = Url::parse("https://example.test/").unwrap();
    let layout = lay_out_graphical(
        &dom,
        &base,
        Viewport::new(200., 200.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    let drawer = by_id(&dom, "drawer");
    let rect = layout.boxes.get(&drawer).unwrap();
    assert_eq!((rect.width, rect.height), (200., 200.));
    let scene = headless::scene_for_dom(
        &dom,
        &base,
        CssSize::new(200., 200.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        ImageStore::default(),
    );
    assert!(
        scene.page_hit_at(CssPoint::new(10., 10.)).is_none(),
        "closed drawer intercepted click"
    );
    let pixels = crate::render::vello_cpu::VelloCpuRenderer::new()
        .render_rgba(&scene)
        .unwrap();
    let offset = (150 * 200 + 150) * 4;
    assert_eq!(&pixels.pixels[offset..offset + 4], &[0, 0, 255, 255]);
}

#[test]
fn svg_css_size_is_used_for_the_standalone_raster_resource() {
    // SVG 2 §8.12: CSS sizing properties, not superseded XML attributes,
    // determine the concrete object size of this vector resource.
    let dom = Dom::parse_document(
        r#"<svg id="icon" width="9" viewBox="0 0 10 10" style="width:18px;height:18px"><path d="M0 0h10v10H0z"/></svg>"#,
    );
    let icon = by_id(&dom, "icon");
    let (source, _) = dom.svg_image_data(icon, None).unwrap();
    let bytes = crate::img::decode_data_url(&source).unwrap();
    let image = crate::img::decode_graphical(&bytes).unwrap();
    assert_eq!(
        (image.width, image.height),
        (18, 18),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
}

#[test]
fn svg_resource_size_handles_math_and_non_intrinsic_css_overrides() {
    for width in ["calc(10px + 8px)", "auto", "100%"] {
        let dom = Dom::parse_document(&format!(
            r#"<svg id="icon" width="9" viewBox="0 0 10 10" style="width:{width};height:18px"><path d="M0 0h10v10H0z"/></svg>"#
        ));
        let (source, _) = dom.svg_image_data(by_id(&dom, "icon"), None).unwrap();
        let bytes = crate::img::decode_data_url(&source).unwrap();
        let image = crate::img::decode_graphical(&bytes).unwrap();
        assert_eq!(
            (image.width, image.height),
            (18, 18),
            "{width}: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

#[test]
fn svg_presentation_dimensions_respect_css_overrides_and_namespace() {
    let dom = Dom::parse_document(
        r#"<style>@layer base { #css {width:20px;height:12px} }</style>
      <div width="35" id="html"></div><svg width="16" height="16" id="css"></svg>
      <svg width="16" id="auto" style="width:auto"></svg><svg width="-2" id="bad"></svg>
      <svg><use id="use" width="16"/></svg>"#,
    );
    assert_eq!(
        dom.computed_value_resolved(by_id(&dom, "css"), "width")
            .as_deref(),
        Some("20px")
    );
    assert_eq!(
        dom.computed_value_resolved(by_id(&dom, "auto"), "width")
            .as_deref(),
        Some("auto")
    );
    for id in ["html", "use", "bad"] {
        assert!(
            !dom.author_declares(by_id(&dom, id), "width"),
            "dimension leaked onto {id}"
        );
    }
}

#[test]
fn clip_path_inset_rounding_math_and_validation() {
    use super::clip_path::Inset;
    use crate::render::{CssRect, PaintShape};
    let parse = |s| {
        Inset::parse(
            s,
            Units {
                fs: 16.,
                root: 16.,
                ch: 8.,
            },
            Vp { w: 800., h: 600. },
        )
        .unwrap()
    };
    let reference = CssRect::new(10., 20., 200., 100.);
    assert_eq!(
        parse("inset(75% 0 50% 0)").shape(reference),
        Some(PaintShape::Rect(CssRect::new(10., 80., 200., 0.)))
    );
    assert_eq!(
        parse("inset(-10px calc(10% + 5px)) border-box").shape(reference),
        Some(PaintShape::Rect(CssRect::new(35., 10., 150., 120.)))
    );
    let Some(PaintShape::RoundedRect { rect, radii }) =
        parse("inset(0 round 50% / 25%)").shape(reference)
    else {
        panic!("rounded inset missing")
    };
    assert_eq!(rect, reference);
    assert_eq!(radii.corners, [(100., 25.); 4]);
    let Some(PaintShape::RoundedRect { radii, .. }) =
        parse("inset(10px round 10% / 20%)").shape(reference)
    else {
        panic!("rounded inset missing")
    };
    assert_eq!(
        radii.corners,
        [(20., 20.); 4],
        "radii use the reference box, not the smaller inset"
    );
    for invalid in [
        "inset()",
        "inset(1px 2px 3px 4px 5px)",
        "inset(3)",
        "inset(0 round -1px)",
        "inset(0) junk",
        "border-box inset(0) border-box",
        "inset(0 round 1px / 2px / 3px)",
        "circle(20px)",
        "url(#clip)",
    ] {
        assert!(
            !super::clip_path::supports(invalid),
            "unexpected support: {invalid}"
        );
    }
}

#[test]
fn clip_path_rounded_intersection_applies_to_background_and_nested_hits() {
    let dom = Dom::parse_document(
        r#"<body style="margin:0;background:blue">
      <div style="width:100px;height:100px;clip-path:inset(0 round 50%);background:red">
        <a href="/" style="position:relative;z-index:20;display:block;width:100px;height:100px;background:red;cursor:pointer"></a>
      </div></body>"#,
    );
    let base = Url::parse("https://example.test/").unwrap();
    let scene = headless::scene_for_dom(
        &dom,
        &base,
        CssSize::new(200., 200.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        ImageStore::default(),
    );
    assert!(scene.page_hit_at(CssPoint::new(2., 2.)).is_none());
    assert!(
        scene.page_hit_at(CssPoint::new(50., 50.)).is_some(),
        "{:#?}",
        scene.primitives
    );
    let frame = crate::render::vello_cpu::VelloCpuRenderer::new()
        .render_rgba(&scene)
        .unwrap();
    assert_eq!(
        &frame.pixels[(2 * 200 + 2) * 4..(2 * 200 + 2) * 4 + 4],
        &[0, 0, 255, 255]
    );
    assert_eq!(
        &frame.pixels[(50 * 200 + 50) * 4..(50 * 200 + 50) * 4 + 4],
        &[255, 0, 0, 255]
    );
}

#[test]
fn clip_path_survives_serialization_and_restores_when_opened() {
    let mut dom = Dom::parse_document(
        r#"<style>.closed {clip-path:inset(0 0 100% 0)}</style>
      <body style="margin:0"><div id="drawer" class="closed" style="width:100px;height:100px"><a href="/">Navigation</a></div></body>"#,
    );
    let drawer = by_id(&dom, "drawer");
    assert!(
        dom.serialize(crate::dom::DOCUMENT)
            .contains("clip-path:inset(0 0 100% 0)")
    );
    dom.set_attr(drawer, "class", "open");
    assert_eq!(
        dom.computed_value_resolved(drawer, "clip-path")
            .as_deref()
            .unwrap_or("none"),
        "none"
    );
    let base = Url::parse("https://example.test/").unwrap();
    let scene = headless::scene_for_dom(
        &dom,
        &base,
        CssSize::new(200., 200.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        ImageStore::default(),
    );
    assert!(scene.page_hit_at(CssPoint::new(5., 5.)).is_some());
}

#[test]
fn clip_path_closed_drawer_stays_hidden_in_terminal_and_scroll_buffers() {
    let dom = Dom::parse_document(
        r#"<body style="margin:0"><p>Visible article</p>
      <div style="position:fixed;inset:0;overflow:auto;clip-path:inset(0 0 100% 0)">
        <a href="/" style="display:block;height:1000px">Hidden navigation</a>
      </div></body>"#,
    );
    let base = Url::parse("https://example.test/").unwrap();
    let out = lay_out_document(
        &dom,
        &base,
        TerminalViewport::new(40, 24, 8., 16.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );
    let main: String = out
        .rows
        .iter()
        .flat_map(|row| &row.items)
        .map(|item| item.text.as_str())
        .collect();
    assert!(
        main.contains("Visible article"),
        "article disappeared: {main}"
    );
    assert!(!main.contains("Hidden"));
    assert!(
        out.fixed.is_empty(),
        "closed drawer left a pinned surface: {:?}",
        out.fixed
    );
    assert!(
        out.regions.is_empty(),
        "closed drawer left a visible scroll region"
    );
    assert!(
        out.carousels.is_empty(),
        "closed drawer left a visible carousel"
    );
}

#[test]
fn clip_path_clips_fixed_descendants_without_becoming_their_containing_block() {
    let dom = Dom::parse_document(
        r#"<body style="margin:0;background:blue">
      <div style="margin:50px;width:100px;height:100px;clip-path:inset(25px)">
        <div id="fixed" style="position:fixed;inset:0;background:red;cursor:pointer"></div>
      </div></body>"#,
    );
    let base = Url::parse("https://example.test/").unwrap();
    let layout = lay_out_graphical(
        &dom,
        &base,
        Viewport::new(200., 200.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    let fixed = layout.boxes.get(&by_id(&dom, "fixed")).unwrap();
    assert_eq!(
        (fixed.left, fixed.top, fixed.width, fixed.height),
        (0., 0., 200., 200.)
    );
    let scene = headless::scene_for_dom(
        &dom,
        &base,
        CssSize::new(200., 200.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        ImageStore::default(),
    );
    assert!(scene.page_hit_at(CssPoint::new(50., 50.)).is_none());
    assert!(scene.page_hit_at(CssPoint::new(100., 100.)).is_some());
}
