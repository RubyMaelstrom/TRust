//! Window-free HTML rendering for tests and developer snapshots.
//!
//! This deliberately uses the production DOM → layout2 → display list → Vello
//! CPU path. It is not a second painter and does not initialize winit.

use std::path::Path;

use url::Url;

use super::vello_cpu::{OwnedRgbaFrame, VelloCpuRenderer};
use super::{ImageStore, Scene};
use crate::core::{CssPoint, CssSize, PhysicalSize, ScaleFactor, ViewportMetrics};
use crate::layout2::{ControlMap, ImageSizes, Viewport};

/// Render a self-contained HTML fixture. `data:` images are decoded
/// synchronously between an intrinsic-discovery pass and the final layout.
pub fn render_html(html: &str, base: &Url, viewport: CssSize) -> Result<OwnedRgbaFrame, String> {
    let mut dom = crate::dom::Dom::parse_document(html);
    dom.rewrite_inline_svgs(Some(base));
    let (forms, controls) = crate::http::extract_forms_arena(&dom, base, None);
    let mut sizes = ImageSizes::new();
    let store = ImageStore::default();
    let discovery = crate::layout2::lay_out_graphical(
        &dom,
        base,
        Viewport::new(viewport.width, viewport.height),
        &forms,
        &controls,
        &sizes,
    );
    for request in &discovery.paint.image_requests {
        if let Some(bytes) = crate::img::decode_data_url(&request.source)
            && let Ok(image) = crate::img::decode_graphical(&bytes)
        {
            sizes.insert(request.source.clone(), (image.width, image.height));
            store.insert(request.handle, image);
        }
    }
    render_dom_with_resources(&dom, base, viewport, &forms, &controls, &sizes, store)
}

/// Render an already-parsed DOM with caller-provided immutable image
/// resources. This is useful for deterministic fixtures whose pixels are
/// generated in the test rather than fetched from a network.
pub fn render_dom_with_resources(
    dom: &crate::dom::Dom,
    base: &Url,
    viewport: CssSize,
    forms: &[crate::doc::Form],
    controls: &ControlMap,
    sizes: &ImageSizes,
    image_store: ImageStore,
) -> Result<OwnedRgbaFrame, String> {
    let scene = scene_for_dom(dom, base, viewport, forms, controls, sizes, image_store);
    VelloCpuRenderer::new().render_rgba(&scene)
}

/// Build the production renderer-neutral scene without rasterizing it. This
/// is the common input for backend differential tests and local benchmarks.
pub fn scene_for_dom(
    dom: &crate::dom::Dom,
    base: &Url,
    viewport: CssSize,
    forms: &[crate::doc::Form],
    controls: &ControlMap,
    sizes: &ImageSizes,
    image_store: ImageStore,
) -> Scene {
    let physical = PhysicalSize::new(
        viewport.width.ceil().max(1.0) as u32,
        viewport.height.ceil().max(1.0) as u32,
    );
    let metrics = ViewportMetrics::from_physical(physical, ScaleFactor::default());
    let layout = crate::layout2::lay_out_graphical(
        dom,
        base,
        Viewport::new(viewport.width, viewport.height),
        forms,
        controls,
        sizes,
    );
    let mut scene = Scene {
        viewport: metrics,
        primitives: Vec::new(),
        controls: Vec::new(),
        content_viewport: super::CssRect::new(0.0, 0.0, viewport.width, viewport.height),
        image_store,
        page_scroll_containers: Vec::new(),
        page_size: CssSize::default(),
    };
    scene.append_page(&layout.paint, CssPoint::default());
    scene
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PixelDifference {
    pub mean_absolute_channel_error: f64,
    pub maximum_channel_error: u8,
    pub fraction_over_tolerance: f64,
}

/// Compare straight-alpha RGBA frames. A channel tolerance keeps expected
/// edge-coverage and glyph-rasterization differences separate from semantic
/// paint failures (wrong color, clip, transform, or missing content).
pub fn compare_rgba(
    reference: &OwnedRgbaFrame,
    candidate: &OwnedRgbaFrame,
    tolerance: u8,
) -> Result<PixelDifference, String> {
    if reference.size != candidate.size || reference.pixels.len() != candidate.pixels.len() {
        return Err(format!(
            "frame dimensions differ: CPU={:?}/{} Hybrid={:?}/{}",
            reference.size,
            reference.pixels.len(),
            candidate.size,
            candidate.pixels.len()
        ));
    }
    if reference.pixels.is_empty() {
        return Ok(PixelDifference::default());
    }
    let mut total = 0_u64;
    let mut maximum = 0_u8;
    let mut over = 0_usize;
    for (&left, &right) in reference.pixels.iter().zip(&candidate.pixels) {
        let difference = left.abs_diff(right);
        total += u64::from(difference);
        maximum = maximum.max(difference);
        over += usize::from(difference > tolerance);
    }
    Ok(PixelDifference {
        mean_absolute_channel_error: total as f64 / reference.pixels.len() as f64,
        maximum_channel_error: maximum,
        fraction_over_tolerance: over as f64 / reference.pixels.len() as f64,
    })
}

/// Save a headless frame as PNG. Image encoding is tooling-only; the HTML
/// painter and its on-screen presentation remain Vello CPU based.
pub fn write_png(frame: &OwnedRgbaFrame, path: impl AsRef<Path>) -> Result<(), String> {
    image::save_buffer_with_format(
        path,
        &frame.pixels,
        frame.size.width,
        frame.size.height,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::{
        CssRect, DisplayCommand, ImageFit, ImageHandle, ImageResource, ImageSampling, PaintBrush,
        PaintColor, PaintShape,
    };

    const FIXTURE: &str = r#"
      <style>
        body { margin:0; color:#123456; background:#fafafa }
        .box { width:90px; height:45px; background:linear-gradient(to right, red, blue);
               border:4px dashed rebeccapurple; border-radius:12px; opacity:.8;
               transform:translateX(5px) rotate(2deg); overflow:hidden }
        .over { position:absolute; left:40px; top:20px; width:50px; height:50px;
                z-index:2; background:rgb(0 200 0 / 60%) }
        .fixed { position:fixed; right:0; top:0; background:#ff0 }
        .flex { display:flex } .grid { display:grid; grid-template-columns:1fr 1fr }
        table { border:1px solid black }
      </style>
      <div class=box>proportional text</div><div class=over></div>
      <div class=flex><b>flex</b><i>item</i></div>
      <div class=grid><span>grid</span><span>item</span></div>
      <table><tr><td>cell</td></tr></table><div class=fixed>fixed</div>
    "#;

    #[test]
    fn fixture_builds_semantic_display_list_and_headless_pixels() {
        let base = Url::parse("https://example.test/").unwrap();
        let dom = crate::dom::Dom::parse_document(FIXTURE);
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            Viewport::new(240.0, 180.0),
            &forms,
            &controls,
            &ImageSizes::new(),
        );
        assert!(layout.paint.primitives.iter().any(|command| matches!(
            command,
            DisplayCommand::Fill {
                brush: PaintBrush::LinearGradient { .. },
                shape: PaintShape::RoundedRect { .. }
            }
        )));
        assert!(layout.paint.primitives.iter().any(
            |command| matches!(command, DisplayCommand::PushLayer(layer) if layer.opacity == 0.8)
        ));
        assert!(
            layout
                .paint
                .primitives
                .iter()
                .any(|command| matches!(command, DisplayCommand::PushTransform(_)))
        );
        assert!(!layout.paint.fixed_primitives.is_empty());

        let frame = render_html(FIXTURE, &base, CssSize::new(240.0, 180.0)).unwrap();
        assert_eq!(frame.pixels.len(), 240 * 180 * 4);
        let distinct: std::collections::HashSet<_> =
            frame.pixels.chunks_exact(4).map(<[u8]>::to_vec).collect();
        assert!(
            distinct.len() > 8,
            "fixture should rasterize varied page paint"
        );
    }

    #[test]
    fn one_axis_overflow_clip_stays_finite_and_rasterizes() {
        // CSS Overflow L3 allows overflow-x:hidden with the y axis left
        // unbounded. The display-list boundary must materialize that axis as
        // a finite document extent: sending ±∞ to a backend clip path makes
        // an otherwise valid page disappear.
        let base = Url::parse("https://example.test/").unwrap();
        let html = r#"
            <style>html,body { margin:0; background:#101010 }</style>
            <div style="width:120px;height:64px;overflow-x:hidden;background:#24364a">
              <div style="height:180px;background:#5a2d72;color:white">visible</div>
            </div>
        "#;
        let dom = crate::dom::Dom::parse_document(html);
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            Viewport::new(240.0, 180.0),
            &forms,
            &controls,
            &ImageSizes::new(),
        );
        for command in layout
            .paint
            .primitives
            .iter()
            .chain(layout.paint.fixed_primitives.iter())
        {
            if let DisplayCommand::PushClip(PaintShape::Rect(rect)) = command {
                assert!(
                    rect.x.is_finite()
                        && rect.y.is_finite()
                        && rect.width.is_finite()
                        && rect.height.is_finite(),
                    "CSS clip reached the display list with non-finite geometry: {rect:?}"
                );
            }
        }
        let frame = render_html(html, &base, CssSize::new(240.0, 180.0)).unwrap();
        assert!(
            frame
                .pixels
                .chunks_exact(4)
                .any(|pixel| pixel == [90, 45, 114, 255]),
            "the visible child background was lost during rasterization"
        );
    }

    #[test]
    fn fixed_viewport_backdrop_paints_under_later_positioned_content() {
        // CSS Positioned Layout §2.2 + CSS 2.1 Appendix E §E.2: a fixed
        // backdrop remains viewport-pinned, but a later z-index:auto positioned
        // sibling paints above it in tree order. The old renderer appended all
        // fixed commands after the document and hid the red app surface.
        let base = Url::parse("https://example.test/").unwrap();
        let html = r#"
            <style>
              html, body { margin:0; width:240px; height:180px }
              .backdrop { position:fixed; inset:0; background:#112233 }
              #app { position:relative; width:240px; height:180px; background:#f00 }
            </style>
            <div class="backdrop"></div><div id="app"></div>
        "#;
        let dom = crate::dom::Dom::parse_document(html);
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            Viewport::new(240.0, 180.0),
            &forms,
            &controls,
            &ImageSizes::new(),
        );
        assert!(!layout.paint.fixed_under_primitives.is_empty());
        assert!(layout.paint.fixed_primitives.is_empty());
        let frame = render_html(html, &base, CssSize::new(240.0, 180.0)).unwrap();
        let center = ((90 * 240 + 120) * 4) as usize;
        assert_eq!(
            &frame.pixels[center..center + 4],
            &[255, 0, 0, 255],
            "later positioned content must paint above the fixed backdrop"
        );
    }

    #[test]
    fn caller_supplied_image_handle_rasterizes_without_blob_in_command() {
        let base = Url::parse("https://example.test/").unwrap();
        let html = "<img src='pixel.png' width=8 height=8>";
        let dom = crate::dom::Dom::parse_document(html);
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let source = base.join("pixel.png").unwrap().to_string();
        let handle = ImageHandle::for_source(&source);
        let store = ImageStore::default();
        store.insert(
            handle,
            super::super::ImageResource {
                width: 1,
                height: 1,
                rgba: std::sync::Arc::from([255, 0, 128, 255]),
                has_alpha: false,
            },
        );
        let sizes = ImageSizes::from([(source, (1, 1))]);
        let frame = render_dom_with_resources(
            &dom,
            &base,
            CssSize::new(20.0, 20.0),
            &forms,
            &controls,
            &sizes,
            store,
        )
        .unwrap();
        assert!(
            frame
                .pixels
                .chunks_exact(4)
                .any(|pixel| pixel == [255, 0, 128, 255])
        );
    }

    #[test]
    fn cpu_and_hybrid_preserve_fixture_paint_semantics() {
        let base = Url::parse("https://example.test/").unwrap();
        let dom = crate::dom::Dom::parse_document(FIXTURE);
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let store = ImageStore::default();
        let handle = ImageHandle::for_source("parity:test-image");
        store.insert(
            handle,
            super::super::ImageResource {
                width: 2,
                height: 2,
                rgba: std::sync::Arc::from([
                    255, 0, 0, 255, 0, 255, 0, 192, 0, 0, 255, 128, 255, 255, 0, 255,
                ]),
                has_alpha: true,
            },
        );
        let mut scene = scene_for_dom(
            &dom,
            &base,
            CssSize::new(240.0, 180.0),
            &forms,
            &controls,
            &ImageSizes::new(),
            store,
        );
        scene.primitives.push(DisplayCommand::Image {
            rect: CssRect::new(150.0, 100.0, 48.0, 48.0),
            handle,
            source_rect: None,
            fit: ImageFit::Fill,
            sampling: ImageSampling::Nearest,
            clip: Some(CssRect::new(150.0, 100.0, 42.0, 42.0)),
            node: 0,
            link: None,
        });

        let cpu = VelloCpuRenderer::new().render_rgba(&scene).unwrap();
        let Ok(mut hybrid) = futures::executor::block_on(
            crate::render::vello_hybrid::VelloHybridRenderer::new_headless(),
        ) else {
            // CI without a GPU-capable wgpu adapter still verifies the CPU
            // reference and the renderer-neutral command contract elsewhere.
            return;
        };
        let hybrid = hybrid.render_rgba(&scene).unwrap();
        let difference = compare_rgba(&cpu, &hybrid, 12).unwrap();
        eprintln!("CPU/Hybrid fixture difference: {difference:?}");
        assert!(
            difference.mean_absolute_channel_error < 8.0,
            "backend mean channel error was {difference:?}"
        );
        assert!(
            difference.fraction_over_tolerance < 0.08,
            "backend semantic difference was {difference:?}"
        );
    }

    #[test]
    fn nested_scroll_and_sticky_metadata_stays_in_css_pixels() {
        let base = Url::parse("https://example.test/").unwrap();
        let dom = crate::dom::Dom::parse_document(
            r#"<div id=s style="width:80px;height:40px;overflow:auto">
                 <div style="width:180px;height:120px">
                   <div id=k style="position:sticky;top:3px">sticky</div>
                 </div>
               </div>"#,
        );
        let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
        let layout = crate::layout2::lay_out_graphical(
            &dom,
            &base,
            Viewport::new(240.0, 180.0),
            &forms,
            &controls,
            &ImageSizes::new(),
        );
        let scroll = layout
            .paint
            .scroll_containers
            .iter()
            .find(|scroll| scroll.node == dom.get_by_id("s").unwrap())
            .unwrap();
        assert_eq!(scroll.viewport.width, 80.0);
        assert!(scroll.content.width >= 180.0);
        assert!(scroll.content.height >= 120.0);
        let sticky = layout
            .paint
            .sticky_constraints
            .iter()
            .find(|sticky| sticky.node == dom.get_by_id("k").unwrap())
            .unwrap();
        assert_eq!(sticky.container, Some(scroll.node));
        assert_eq!(sticky.insets[0], Some(3.0));
    }

    /// Repeatable end-to-end desktop pipeline benchmark.
    ///
    /// Run with:
    /// `TRUST_DESKTOP_BENCH=1 cargo test --release desktop_pipeline_bench -- --ignored --nocapture`
    ///
    /// Layout2 currently emits its stable `PagePaint` as part of layout, so
    /// those two stages are reported together. Scene composition, CPU raster,
    /// Hybrid raster+headless readback, and full CPU frame are measured
    /// independently. Actual swapchain presentation is measured with
    /// `TRUST_DESKTOP_TRACE=1 trust-desktop --renderer=... URL`.
    #[test]
    #[ignore]
    fn desktop_pipeline_bench() {
        if std::env::var_os("TRUST_DESKTOP_BENCH").is_none() {
            eprintln!("set TRUST_DESKTOP_BENCH=1 to run the desktop pipeline benchmark");
            return;
        }
        use std::time::{Duration, Instant};

        fn repeat(fragment: &str, count: usize) -> String {
            std::iter::repeat_n(fragment, count).collect()
        }

        let cases = [
            (
                "text-article",
                format!(
                    "<style>body{{max-width:760px;margin:auto;font:16px serif;line-height:1.5}}h2{{color:#245}} </style>{}",
                    repeat(
                        "<h2>Measured typography</h2><p>Unicode العربية 日本語 हिन्दी — proportional browser text with <b>bold</b>, <i>italic</i>, links and wrapping.</p>",
                        90
                    )
                ),
            ),
            (
                "flex-grid",
                format!(
                    "<style>.g{{display:grid;grid-template-columns:repeat(5,1fr);gap:7px}}.c{{display:flex;min-height:70px;padding:8px;border:2px solid #357;border-radius:9px;background:linear-gradient(135deg,#def,#9bc)}}</style><div class=g>{}</div>",
                    repeat(
                        "<div class=c><b>flex</b>&nbsp;grid item with wrapping</div>",
                        350
                    )
                ),
            ),
            (
                "overlapping-cards",
                format!(
                    "<style>.c{{position:relative;display:inline-block;width:125px;height:90px;margin:-8px 4px;padding:8px;background:#fff8;border-radius:12px;box-shadow:4px 5px 12px #0006;transform:rotate(.5deg)}}.c:nth-child(3n){{opacity:.72;transform:translate(8px) rotate(-2deg)}}</style>{}",
                    repeat("<div class=c>stacking context card</div>", 280)
                ),
            ),
            (
                "large-scroll",
                format!(
                    "<style>.row{{height:42px;border-bottom:1px solid #bbb}}.sticky{{position:sticky;top:0;background:#ffc}}</style><div class=sticky>sticky header</div>{}",
                    repeat("<div class=row>Long scrolling document row</div>", 1_200)
                ),
            ),
            (
                "dynamic-dom",
                format!(
                    "<style>.item{{display:flex;padding:3px}}.item:nth-child(odd){{background:#eef}}</style><div id=tick>tick 0</div>{}<script>/* mutation workload marker */</script>",
                    repeat("<div class=item>live DOM item</div>", 500)
                ),
            ),
        ];
        let iterations = std::env::var("TRUST_DESKTOP_BENCH_ITERATIONS")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(5)
            .max(1);
        let base = Url::parse("https://bench.invalid/").unwrap();
        let viewport = CssSize::new(960.0, 640.0);
        let mut hybrid = futures::executor::block_on(
            crate::render::vello_hybrid::VelloHybridRenderer::new_headless(),
        )
        .ok();

        eprintln!(
            "desktop benchmark: {iterations} iterations; Hybrid includes deterministic GPU readback"
        );
        for (name, html) in cases {
            let parse_started = Instant::now();
            let mut dom = crate::dom::Dom::parse_document(&html);
            let parse = parse_started.elapsed();
            let (forms, controls) = crate::http::extract_forms_arena(&dom, &base, None);
            let mut layout_total = Duration::ZERO;
            let mut compose_total = Duration::ZERO;
            let mut cpu_total = Duration::ZERO;
            let mut hybrid_total = Duration::ZERO;
            let mut full_total = Duration::ZERO;
            let mut cpu_renderer = VelloCpuRenderer::new();
            let mut full_cpu_renderer = VelloCpuRenderer::new();
            for iteration in 0..iterations {
                if name == "dynamic-dom"
                    && let Some(node) = dom.get_by_id("tick")
                {
                    dom.set_text(node, &format!("tick {iteration}"));
                }
                let layout_started = Instant::now();
                let layout = crate::layout2::lay_out_graphical(
                    &dom,
                    &base,
                    Viewport::new(viewport.width, viewport.height),
                    &forms,
                    &controls,
                    &ImageSizes::new(),
                );
                layout_total += layout_started.elapsed();

                let compose_started = Instant::now();
                let metrics = ViewportMetrics::from_physical(
                    PhysicalSize::new(960, 640),
                    ScaleFactor::default(),
                );
                let mut scene = Scene {
                    viewport: metrics,
                    primitives: Vec::new(),
                    controls: Vec::new(),
                    content_viewport: CssRect::new(0.0, 0.0, 960.0, 640.0),
                    image_store: ImageStore::default(),
                    page_scroll_containers: Vec::new(),
                    page_size: CssSize::default(),
                };
                scene.append_page(&layout.paint, CssPoint::default());
                compose_total += compose_started.elapsed();

                let cpu_started = Instant::now();
                cpu_renderer.render_rgba(&scene).unwrap();
                cpu_total += cpu_started.elapsed();
                if let Some(renderer) = &mut hybrid {
                    let hybrid_started = Instant::now();
                    renderer.render_rgba(&scene).unwrap();
                    hybrid_total += hybrid_started.elapsed();
                }

                let full_started = Instant::now();
                let fresh = crate::dom::Dom::parse_document(&html);
                let (fresh_forms, fresh_controls) =
                    crate::http::extract_forms_arena(&fresh, &base, None);
                let full_scene = scene_for_dom(
                    &fresh,
                    &base,
                    viewport,
                    &fresh_forms,
                    &fresh_controls,
                    &ImageSizes::new(),
                    ImageStore::default(),
                );
                full_cpu_renderer.render_rgba(&full_scene).unwrap();
                full_total += full_started.elapsed();
            }
            let per = |duration: Duration| duration.as_secs_f64() * 1_000.0 / f64::from(iterations);
            eprintln!(
                "{name:18} parse={:7.2}ms layout+paint={:8.2}ms compose={:7.2}ms cpu={:8.2}ms hybrid+readback={:8.2}ms full-cpu={:8.2}ms",
                parse.as_secs_f64() * 1_000.0,
                per(layout_total),
                per(compose_total),
                per(cpu_total),
                if hybrid.is_some() {
                    per(hybrid_total)
                } else {
                    f64::NAN
                },
                per(full_total),
            );
        }

        let metrics =
            ViewportMetrics::from_physical(PhysicalSize::new(960, 640), ScaleFactor::default());
        let store = ImageStore::default();
        let mut image_scene = Scene {
            viewport: metrics,
            primitives: vec![DisplayCommand::FillRect {
                rect: CssRect::new(0.0, 0.0, 960.0, 640.0),
                color: PaintColor::Rgba(245, 246, 248, 255),
            }],
            controls: Vec::new(),
            content_viewport: CssRect::new(0.0, 0.0, 960.0, 640.0),
            image_store: store.clone(),
            page_scroll_containers: Vec::new(),
            page_size: CssSize::new(960.0, 1_600.0),
        };
        for index in 0..200_u32 {
            let handle = ImageHandle::for_source(&format!("bench:image:{index}"));
            let r = (index.wrapping_mul(37) & 0xff) as u8;
            let g = (index.wrapping_mul(71) & 0xff) as u8;
            let b = (index.wrapping_mul(113) & 0xff) as u8;
            store.insert(
                handle,
                ImageResource {
                    width: 32,
                    height: 32,
                    rgba: std::sync::Arc::from([r, g, b, 255].repeat(32 * 32).into_boxed_slice()),
                    has_alpha: false,
                },
            );
            let col = index % 10;
            let row = index / 10;
            image_scene.primitives.push(DisplayCommand::Image {
                rect: CssRect::new(col as f32 * 94.0 + 8.0, row as f32 * 76.0 + 8.0, 86.0, 68.0),
                handle,
                source_rect: None,
                fit: ImageFit::Cover,
                sampling: ImageSampling::Smooth,
                clip: None,
                node: index as usize,
                link: None,
            });
        }
        let mut cpu = VelloCpuRenderer::new();
        let cpu_started = Instant::now();
        for _ in 0..iterations {
            cpu.render_rgba(&image_scene).unwrap();
        }
        let cpu_images = cpu_started.elapsed();
        let hybrid_images = hybrid.as_mut().map(|renderer| {
            let started = Instant::now();
            for _ in 0..iterations {
                renderer.render_rgba(&image_scene).unwrap();
            }
            started.elapsed()
        });
        eprintln!(
            "{:<18} cpu={:8.2}ms hybrid+readback={:8.2}ms (200 resources; retained after first frame)",
            "image-grid",
            cpu_images.as_secs_f64() * 1_000.0 / f64::from(iterations),
            hybrid_images.map_or(f64::NAN, |elapsed| elapsed.as_secs_f64() * 1_000.0
                / f64::from(iterations)),
        );

        let mut terminal = crate::terminal_view::TerminalView::new(34, 100);
        terminal.process(
            repeat(
                "\x1b[38;5;45mTRust MUD\x1b[0m  status line with ANSI colors\r\n",
                34,
            )
            .as_bytes(),
        );
        let paint = terminal.paint();
        let metrics =
            ViewportMetrics::from_physical(PhysicalSize::new(960, 646), ScaleFactor::default());
        let mut scene = Scene {
            viewport: metrics,
            primitives: Vec::new(),
            controls: Vec::new(),
            content_viewport: CssRect::new(0.0, 0.0, 960.0, 646.0),
            image_store: ImageStore::default(),
            page_scroll_containers: Vec::new(),
            page_size: CssSize::default(),
        };
        scene.append_page(&paint, CssPoint::default());
        let started = Instant::now();
        let mut cpu = VelloCpuRenderer::new();
        for _ in 0..iterations {
            cpu.render_rgba(&scene).unwrap();
        }
        eprintln!(
            "{:<18} raster={:8.2}ms (retained CPU backend)",
            "telnet-redraw",
            started.elapsed().as_secs_f64() * 1_000.0 / f64::from(iterations)
        );
    }
}
