//! End-to-end conformance for CSS effects that reference SVG resources or
//! read the backdrop: Filter Effects 1 `filter: <url>`, Filter Effects 2
//! `backdrop-filter`, CSS Masking 1 `clip-path` (`<url>`, `path()`,
//! `shape()`, box values) and `mask-image: <url>` to a `mask` element.
//! Documents are laid out and painted by the graphical adapter, then
//! rasterized by the CPU reference renderer.

use std::collections::HashMap;

use url::Url;

use crate::core::{CssPoint, CssSize};
use crate::dom::Dom;
use crate::render::vello_cpu::{OwnedRgbaFrame, VelloCpuRenderer};
use crate::render::{DisplayCommand, ImageStore, PaintShape, Scene};

const SIZE: (f32, f32) = (200., 100.);

/// Lay out and paint `html`, decoding the `data:` images it requests (the
/// generated mask documents) before painting it again.
fn render(html: &str) -> (Scene, OwnedRgbaFrame) {
    let dom = Dom::parse_document(html);
    let base = Url::parse("https://example.test/page.html").unwrap();
    let store = ImageStore::default();
    let mut sizes = HashMap::new();
    // Document SVG images carry the current navigation, which concurrently
    // running tests may advance: lay out until a pass needs no new image,
    // and paint that pass.
    let mut layout;
    let mut passes = 0;
    loop {
        layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            crate::layout2::Viewport::new(SIZE.0, SIZE.1),
            &[],
            &HashMap::new(),
            &sizes,
        );
        let mut decoded_any = false;
        for request in &layout.paint.image_requests {
            if sizes.contains_key(&request.source) {
                continue;
            }
            if let Some(Ok(image)) = crate::img::decode_graphical_svg_data_url(&request.source) {
                sizes.insert(request.source.clone(), (image.width, image.height));
                store.insert(request.handle, image);
                decoded_any = true;
            }
        }
        passes += 1;
        if !decoded_any || passes == 8 {
            break;
        }
    }
    let mut scene = Scene {
        viewport: crate::core::ViewportMetrics::from_physical(
            crate::core::PhysicalSize::new(SIZE.0 as u32, SIZE.1 as u32),
            crate::core::ScaleFactor::default(),
        ),
        primitives: Vec::new(),
        controls: Vec::new(),
        content_viewport: crate::render::CssRect::new(0., 0., SIZE.0, SIZE.1),
        image_store: store,
        canvas_images: Default::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::default(),
    };
    scene.append_page(&layout.paint, CssPoint::default());
    let frame = VelloCpuRenderer::new().render_rgba(&scene).unwrap();
    (scene, frame)
}

fn pixel(frame: &OwnedRgbaFrame, x: usize, y: usize) -> [u8; 4] {
    let at = (y * frame.size.width as usize + x) * 4;
    frame.pixels[at..at + 4].try_into().unwrap()
}

const RED: [u8; 4] = [255, 0, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

#[test]
fn svg_filter_references_run_their_primitive_tree() {
    // Filter Effects 1 #FilterProperty `<url>`: the referenced filter's
    // primitives replace the element's rendering; feFlood + feComposite in
    // recolors its silhouette.
    let (_, frame) = render(
        r##"<body style="margin:0;background:white">
        <svg width=0 height=0 style="position:absolute"><filter id=recolor x=0 y=0 width=1 height=1>
          <feFlood flood-color="blue"/><feComposite in2="SourceGraphic" operator="in"/>
        </filter><filter id=empty></filter><rect id=not-a-filter/></svg>
        <div style="position:absolute;left:10px;top:10px;width:40px;height:40px;background:red;filter:url(#recolor)"></div>
        <div style="position:absolute;left:60px;top:10px;width:40px;height:40px;background:red;filter:url(#missing) invert(1)"></div>
        <div style="position:absolute;left:110px;top:10px;width:40px;height:40px;background:red;filter:url(#not-a-filter)"></div>
        <div style="position:absolute;left:160px;top:10px;width:30px;height:40px;background:red;filter:url(#empty)"></div>"##,
    );
    assert_eq!(pixel(&frame, 30, 30), BLUE);
    // An invalid reference ignores the whole chain (no invert either).
    assert_eq!(pixel(&frame, 80, 30), RED);
    assert_eq!(pixel(&frame, 130, 30), RED);
    // A filter without primitives does not render the element.
    assert_eq!(pixel(&frame, 175, 30), WHITE);
}

#[test]
fn svg_filter_lengths_follow_primitive_units_and_turbulence_fills_its_subregion() {
    let (_, frame) = render(
        r##"<body style="margin:0;background:white">
        <svg width=0 height=0 style="position:absolute">
          <filter id=shift primitiveUnits="objectBoundingBox" x="0" y="0" width="2" height="1">
            <feOffset dx="0.5"/>
          </filter>
          <filter id=noise x=0 y=0 width=1 height=1 color-interpolation-filters="sRGB">
            <feTurbulence baseFrequency="0.05" numOctaves="2" type="fractalNoise" seed="3"/>
            <feColorMatrix values="0 0 0 0 0  0 0 0 0 0  0 0 0 0 0  0 0 0 0 1"/>
          </filter>
        </svg>
        <div style="position:absolute;left:0;top:0;width:40px;height:40px;background:red;filter:url(#shift)"></div>
        <div style="position:absolute;left:100px;top:0;width:80px;height:80px;filter:url(#noise)"></div>"##,
    );
    // dx 0.5 of the 40px bounding box moves the box 20px right.
    assert_eq!(pixel(&frame, 10, 20), WHITE);
    assert_eq!(pixel(&frame, 30, 20), RED);
    assert_eq!(pixel(&frame, 55, 20), RED);
    // Opaque black noise covers the element's box only.
    assert_eq!(pixel(&frame, 140, 40), [0, 0, 0, 255]);
    assert_eq!(pixel(&frame, 190, 40), WHITE);
}

#[test]
fn backdrop_filter_reads_content_behind_the_element() {
    // Filter Effects 2 #backdrop-filter-operation: the backdrop behind the
    // border box is filtered; the element's own background paints over it.
    let (scene, frame) = render(
        r#"<body style="margin:0;background:white">
        <div style="position:absolute;left:0;top:0;width:100px;height:100px;background:red"></div>
        <div id=glass style="position:absolute;left:50px;top:20px;width:100px;height:60px;backdrop-filter:invert(1)"></div>
        <div style="position:absolute;left:0;top:90px;width:200px;height:10px;backdrop-filter:url(#none)"></div>"#,
    );
    assert_eq!(pixel(&frame, 75, 50), [0, 255, 255, 255], "inverted red");
    assert_eq!(pixel(&frame, 125, 50), [0, 0, 0, 255], "inverted white");
    assert_eq!(pixel(&frame, 25, 50), RED);
    // An invalid reference ignores the backdrop filter.
    assert_eq!(pixel(&frame, 150, 95), WHITE);
    assert!(scene.primitives.iter().any(|command| matches!(
        command,
        DisplayCommand::PushLayer(layer) if layer.backdrop.is_some()
    )));
}

#[test]
fn backdrop_filter_stops_at_backdrop_roots_and_contains_fixed_boxes() {
    // #BackdropRoot: an ancestor with opacity < 1 bounds the backdrop. The
    // page's red is behind the root, so only the root's own (transparent)
    // content is inverted, and the element shows the page as it is.
    let (_, frame) = render(
        r#"<body style="margin:0;background:red">
        <div style="opacity:0.999;position:absolute;inset:0">
          <div style="position:absolute;left:0;top:0;width:200px;height:100px;backdrop-filter:invert(1)">
            <div style="position:fixed;left:10px;top:10px;width:10px;height:10px;background:blue"></div>
          </div>
        </div>"#,
    );
    let shown = pixel(&frame, 100, 50);
    assert!(shown[0] > 250 && shown[1] < 5, "{shown:?}");
    let fixed = pixel(&frame, 15, 15);
    assert!(fixed[0] < 5 && fixed[2] > 250, "{fixed:?}");
}

#[test]
fn clip_path_references_paths_shapes_and_boxes() {
    // CSS Masking 1 #ClipPathElement: children are OR'ed; objectBoundingBox
    // units scale with the border box. Invalid references clip nothing.
    let (scene, frame) = render(
        r##"<body style="margin:0;background:white">
        <svg width=0 height=0 style="position:absolute"><defs>
          <clipPath id=two clipPathUnits="objectBoundingBox">
            <rect width="0.25" height="1"/><rect x="0.75" width="0.25" height="1"/>
          </clipPath>
        </defs></svg>
        <div style="position:absolute;left:0;top:0;width:80px;height:40px;background:red;clip-path:url(#two)"></div>
        <div style="position:absolute;left:100px;top:0;width:40px;height:40px;background:red;clip-path:url(#nothing)"></div>
        <div style="position:absolute;left:150px;top:0;width:40px;height:40px;background:red;clip-path:path(evenodd,'M0 0H40V40H0Z M10 10H30V30H10Z')"></div>
        <div style="position:absolute;left:0;top:50px;width:80px;height:40px;background:red;clip-path:shape(from 0 0, line to 100% 0, line to 0 100%, close)"></div>
        <div style="position:absolute;left:100px;top:60px;width:40px;height:20px;margin:10px;background:red;border-radius:10px;clip-path:margin-box">
          <div style="width:60px;height:40px;margin:-10px;background:blue"></div></div>"##,
    );
    assert_eq!(pixel(&frame, 10, 20), RED);
    assert_eq!(pixel(&frame, 40, 20), WHITE);
    assert_eq!(pixel(&frame, 70, 20), RED);
    assert_eq!(pixel(&frame, 120, 20), RED, "invalid reference");
    assert_eq!(pixel(&frame, 155, 20), RED, "even-odd ring");
    assert_eq!(pixel(&frame, 170, 20), WHITE, "even-odd hole");
    assert_eq!(pixel(&frame, 10, 55), RED);
    assert_eq!(pixel(&frame, 70, 85), WHITE);
    // margin-box: the margin edge, not the border box, clips; descendants
    // overflowing into the margin stay visible.
    assert_eq!(pixel(&frame, 112, 72), BLUE);
    // Hit testing follows the clip geometry too.
    assert!(scene.page_hit_at(CssPoint::new(40., 20.)).is_none());
    assert!(
        scene
            .primitives
            .iter()
            .any(|command| matches!(command, DisplayCommand::PushClip(PaintShape::Geometry(_))))
    );
}

#[test]
fn mask_image_references_render_the_mask_element() {
    // CSS Masking 1 #the-mask-image `<mask-source>`: a luminance mask whose
    // white left half keeps the content; `mask-mode: alpha` overrides
    // mask-type so the opaque black half keeps it as well.
    let (_, frame) = render(
        r##"<body style="margin:0;background:white">
        <svg width=0 height=0 style="position:absolute"><mask id=half maskContentUnits="objectBoundingBox">
          <rect width="0.5" height="1" fill="white"/><rect x="0.5" width="0.5" height="1" fill="black"/>
        </mask></svg>
        <div style="position:absolute;left:0;top:0;width:80px;height:40px;background:red;mask-image:url(#half)"></div>
        <div style="position:absolute;left:100px;top:0;width:80px;height:40px;background:red;mask:url(#half) alpha"></div>
        <div style="position:absolute;left:0;top:50px;width:80px;height:40px;background:red;mask-image:url(#missing)"></div>"##,
    );
    assert_eq!(pixel(&frame, 20, 20), RED);
    assert_eq!(pixel(&frame, 60, 20), WHITE);
    assert_eq!(pixel(&frame, 120, 20), RED);
    assert_eq!(pixel(&frame, 160, 20), RED);
    // A reference to nothing is a transparent black mask layer.
    assert_eq!(pixel(&frame, 40, 70), WHITE);
}

#[test]
fn effects_agree_between_cpu_and_hybrid() {
    // Backend selection must not change output: Hybrid draws the effect
    // pixels the CPU reference renderer computes.
    let (scene, cpu) = render(
        r##"<body style="margin:0;background:linear-gradient(90deg,#f06,#48f)">
        <svg width=0 height=0 style="position:absolute">
          <filter id=f><feTurbulence baseFrequency=".05" result=t/><feDisplacementMap in=SourceGraphic in2=t scale=10/></filter>
          <clipPath id=c><circle cx=30 cy=30 r=28/><rect x=40 width=40 height=40/></clipPath>
        </svg>
        <div style="position:absolute;left:10px;top:10px;width:60px;height:60px;background:#ff0;filter:url(#f)"></div>
        <div style="position:absolute;left:80px;top:10px;width:80px;height:60px;background:#0f0;clip-path:url(#c)"></div>
        <div style="position:absolute;left:40px;top:40px;width:120px;height:50px;border-radius:12px;backdrop-filter:blur(4px) invert(1)"></div>"##,
    );
    let Ok(mut hybrid) = futures::executor::block_on(
        crate::render::vello_hybrid::VelloHybridRenderer::new_headless(),
    ) else {
        eprintln!("Effect backend agreement not exercised: no Hybrid adapter");
        return;
    };
    let gpu = hybrid.render_rgba(&scene).unwrap();
    let difference = crate::render::headless::compare_rgba(&cpu, &gpu, 16).unwrap();
    assert!(difference.fraction_over_tolerance < 0.01, "{difference:?}");
}

#[test]
fn terminal_clips_by_the_bounds_of_a_referenced_clip_path() {
    // The terminal adapter clips to clip-path bounds; an SVG `clipPath`
    // resolves to its geometry's bounds there too. An empty clipPath hides
    // the box's text entirely.
    let dom = Dom::parse_document(
        r#"<body style="margin:0"><p>Visible article</p>
      <svg width=0 height=0><clipPath id=none></clipPath></svg>
      <div style="clip-path:url(#none)">Hidden text</div></body>"#,
    );
    let base = Url::parse("https://example.test/").unwrap();
    let out = crate::layout2::lay_out_document(
        &dom,
        &base,
        crate::layout2::TerminalViewport::new(40, 24, 8., 16.),
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );
    let text: String = out
        .rows
        .iter()
        .flat_map(|row| &row.items)
        .map(|item| item.text.as_str())
        .collect();
    assert!(text.contains("Visible article"), "{text}");
    assert!(!text.contains("Hidden"), "{text}");
}
