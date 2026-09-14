//! Raster regressions independent of the host's installed fonts. Reference
//! heights were measured with FreeType's FT_LOAD_TARGET_LIGHT at 96dpi.

use std::sync::{Arc, OnceLock};

use super::vello_cpu::{OwnedRgbaFrame, VelloCpuRenderer};
use super::vello_hybrid::VelloHybridRenderer;
use super::*;
use crate::core::ScaleFactor;
use crate::text::{FontFace, ShapedGlyph, ShapedRun, ShapedText};

const FONT: &[u8] = include_bytes!("fixtures/jetbrains-mono/Bold-ASCII.ttf");

fn font() -> &'static parley::FontData {
    static FONT_DATA: OnceLock<parley::FontData> = OnceLock::new();
    FONT_DATA.get_or_init(|| parley::FontData::new(parley::fontique::Blob::new(Arc::new(FONT)), 0))
}

fn glyph_id() -> u32 {
    u32::from(
        ttf_parser::Face::parse(FONT, 0)
            .unwrap()
            .glyph_index('F')
            .unwrap()
            .0,
    )
}

fn glyph_scene(scale: f64) -> Scene {
    let size = crate::theme::TERMINAL_FONT_SIZE_CSS_PX;
    let advance = size * 0.6;
    let shaped = ShapedText {
        text: "F".into(),
        advance,
        ascent: 16.0,
        descent: 4.0,
        baseline: 16.0,
        line_height: 20.0,
        runs: vec![ShapedRun {
            font: FontFace::from_test_data(font().clone()),
            font_size: size,
            color: None,
            normalized_coords: Vec::new(),
            glyphs: vec![ShapedGlyph {
                id: glyph_id(),
                x: 0.0,
                y: 16.0,
                advance,
            }],
            text_range: 0..1,
            rtl: false,
            synth_bold: false,
            synth_skew_degrees: None,
        }],
        ..Default::default()
    };
    Scene {
        viewport: ViewportMetrics::from_physical(
            PhysicalSize::new(128, 128),
            ScaleFactor::new(scale),
        ),
        primitives: vec![DisplayCommand::GlyphRun {
            origin: CssPoint::new(8.0, 8.0),
            shaped,
            color: PaintColor::Rgba(0, 0, 0, 255),
            decoration: TextDecorationPaint {
                color: PaintColor::Rgba(0, 0, 0, 255),
                style: DecorationStyle::Solid,
            },
            shadows: Vec::new(),
            clip: None,
            node: 1,
            link: None,
        }],
        controls: Vec::new(),
        content_viewport: CssRect::new(0.0, 0.0, 64.0, 64.0),
        image_store: Default::default(),
        canvas_images: Default::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::new(64.0, 64.0),
    }
}

fn ink_height(frame: &OwnedRgbaFrame) -> usize {
    let mut rows = frame
        .pixels
        .chunks_exact(frame.size.width as usize * 4)
        .enumerate()
        .filter(|(_, row)| row.chunks_exact(4).any(|pixel| pixel[3] > 32))
        .map(|(y, _)| y);
    let first = rows.next().expect("the glyph must contain visible ink");
    rows.last().unwrap_or(first) - first + 1
}

#[test]
fn light_hinting_matches_terminal_cap_height_at_device_scales() {
    let mut cpu = VelloCpuRenderer::new();
    for (scale, expected) in [(1.0, 10), (1.25, 13), (2.0, 22)] {
        let scene = glyph_scene(scale);
        for _ in 0..3 {
            let frame = cpu.render_rgba(&scene).unwrap();
            assert_eq!(ink_height(&frame), expected, "scale {scale}");
        }
    }
}

#[test]
fn light_hinting_cpu_and_hybrid_agree_at_device_scales() {
    let Ok(mut hybrid) = futures::executor::block_on(VelloHybridRenderer::new_headless()) else {
        eprintln!("Light-hinting GPU regression not exercised: no Hybrid adapter");
        return;
    };
    let mut cpu = VelloCpuRenderer::new();
    for (scale, expected) in [(1.0, 10), (1.25, 13), (2.0, 22)] {
        let scene = glyph_scene(scale);
        let reference = cpu.render_rgba(&scene).unwrap();
        for _ in 0..3 {
            let frame = hybrid.render_rgba(&scene).unwrap();
            assert_eq!(ink_height(&frame), expected, "scale {scale}");
            let diff = headless::compare_rgba(&reference, &frame, 8).unwrap();
            assert!(diff.fraction_over_tolerance < 0.001, "{diff:?}");
        }
    }
}

fn draw_with_mode(
    resources: &mut ::vello_cpu::Resources,
    mode: glifo::HintingMode,
    atlas: bool,
    transform: ::vello_cpu::kurbo::Affine,
    hint: bool,
) -> OwnedRgbaFrame {
    let mut context = ::vello_cpu::RenderContext::new(64, 64);
    let mut pixmap = ::vello_cpu::Pixmap::new(64, 64);
    context.set_transform(transform);
    context.set_paint(::vello_cpu::color::palette::css::BLACK);
    context
        .glyph_run(resources, font())
        .font_size(crate::theme::TERMINAL_FONT_SIZE_CSS_PX)
        .hinting_mode(mode)
        .hint(hint)
        .atlas_cache(atlas)
        .fill_glyphs(
            [::vello_cpu::Glyph {
                id: glyph_id(),
                x: 8.0,
                y: 24.0,
            }]
            .into_iter(),
        );
    context.render(&mut pixmap, resources);
    OwnedRgbaFrame {
        size: PhysicalSize::new(64, 64),
        pixels: pixmap.data_as_u8_slice().to_vec(),
    }
}

#[test]
fn light_hinting_caches_distinguish_native_and_light_outlines() {
    use glifo::HintingMode::{Light, Vertical};
    for atlas in [false, true] {
        let mut resources = ::vello_cpu::Resources::new();
        let mut reference = None;
        // Exercise hinter, outline, and bitmap-atlas reuse in both directions.
        for (mode, expected) in [(Vertical, 12), (Light, 10), (Vertical, 12), (Light, 10)] {
            let frame = draw_with_mode(
                &mut resources,
                mode,
                atlas,
                ::vello_cpu::kurbo::Affine::IDENTITY,
                true,
            );
            assert_eq!(ink_height(&frame), expected, "{mode:?}, atlas={atlas}");
            if mode == Light {
                if let Some(previous) = reference.replace(frame.clone()) {
                    assert_eq!(frame, previous, "cache reuse changed the glyph");
                }
            }
        }
    }
}

#[test]
fn light_hinting_stays_disabled_for_rotated_text() {
    let mut resources = ::vello_cpu::Resources::new();
    let rotation = ::vello_cpu::kurbo::Affine::rotate(0.2);
    let draw = |resources: &mut _, hint| {
        draw_with_mode(resources, glifo::HintingMode::Light, true, rotation, hint)
    };
    assert_eq!(draw(&mut resources, true), draw(&mut resources, false));
}
