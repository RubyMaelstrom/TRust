//! Per-use SVG sizing must not invalidate images referenced by earlier draws.

use super::vello_cpu::VelloCpuRenderer;
use super::vello_hybrid::VelloHybridRenderer;
use super::*;
use crate::core::ScaleFactor;

fn repeated_svg_scene(scale: f64) -> Scene {
    let mut scene = Scene {
        viewport: ViewportMetrics::from_physical(
            PhysicalSize::new((160. * scale) as u32, (100. * scale) as u32),
            ScaleFactor::new(scale),
        ),
        primitives: Vec::new(),
        controls: Vec::new(),
        content_viewport: CssRect::new(0., 0., 160., 100.),
        image_store: Default::default(),
        canvas_images: Default::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::new(160., 100.),
    };
    let handle = ImageHandle(42);
    // No viewBox: percentages follow each CSS object viewport while the
    // four-unit stripe retains its CSS width. A single shared bitmap cannot
    // serve the different concrete sizes without changing these pixels.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="100%"><rect width="100%" height="100%" fill="red"/><rect width="4" height="100%" fill="blue"/></svg>"#;
    scene
        .image_store
        .insert(handle, crate::img::decode_graphical(svg).unwrap());
    for rect in [
        CssRect::new(8., 8., 24., 24.),
        CssRect::new(48., 8., 64., 48.),
        CssRect::new(120., 8., 24., 24.),
    ] {
        scene.primitives.push(DisplayCommand::Image {
            rect,
            handle,
            source_rect: None,
            fit: ImageFit::Fill,
            sampling: ImageSampling::Nearest,
            clip: None,
            node: 0,
            link: None,
        });
    }
    scene
}

fn check_repeated_svg(mut render: impl FnMut(&Scene) -> super::vello_cpu::OwnedRgbaFrame) {
    // A/B/A order catches both premature destruction and overwriting an
    // atlas slot already referenced by another draw. Repeated frames and
    // device-scale changes exercise retained resources as well as first use.
    for scale in [1., 2., 1.5, 1.] {
        let mut scene = repeated_svg_scene(scale);
        for offset in [0., 3., 0.] {
            scene.primitives.insert(
                0,
                DisplayCommand::PushTransform(Affine2d::translate(0., offset)),
            );
            scene.primitives.push(DisplayCommand::PopTransform);
            let frame = render(&scene);
            for x in [8., 48., 120.] {
                for (dx, expected) in [(2., [0, 0, 255, 255]), (8., [255, 0, 0, 255])] {
                    let px = ((x + dx) * scale) as usize;
                    let py = ((16. + f64::from(offset)) * scale) as usize;
                    let i = (py * frame.size.width as usize + px) * 4;
                    assert_eq!(
                        &frame.pixels[i..i + 4],
                        &expected,
                        "scale={scale}, offset={offset}, x={x}, dx={dx}"
                    );
                }
            }
            scene.primitives.remove(0);
            scene.primitives.pop();
        }
    }

    // Different fractional CSS viewports can share the same integer upload
    // dimensions. Compare shared-source rendering with independent resources
    // so an in-place atlas update cannot silently change an earlier draw.
    let mut scene = repeated_svg_scene(1.);
    for (i, command) in scene.primitives.iter_mut().enumerate() {
        if let DisplayCommand::Image { rect, .. } = command {
            rect.width = if i == 1 { 24.875 } else { 24.125 };
            rect.height = 24.125;
        }
    }
    let shared = render(&scene);
    assert_eq!(shared.pixels, render(&scene).pixels);
    let resource = scene.image(ImageHandle(42)).unwrap();
    for (i, command) in scene.primitives.iter_mut().enumerate() {
        if let DisplayCommand::Image { handle, .. } = command {
            *handle = ImageHandle(100 + i as u64);
            scene.image_store.insert(*handle, resource.clone());
        }
    }
    assert_eq!(
        shared.pixels,
        render(&scene).pixels,
        "sharing an SVG must not change the pixels at any of its concrete sizes"
    );
}

pub(super) fn check_svg_cache_budget(
    mut render: impl FnMut(&Scene) -> (super::vello_cpu::OwnedRgbaFrame, usize),
) {
    use super::vello_cpu::MAX_REGISTERED_IMAGES;
    let mut scene = repeated_svg_scene(1.);
    let first = scene.primitives[2].clone();
    for batch in 0..3 {
        scene.primitives = vec![first.clone()];
        for variant in 0..MAX_REGISTERED_IMAGES + 8 {
            let mut command = first.clone();
            if let DisplayCommand::Image { rect, .. } = &mut command {
                *rect = CssRect::new(8., 8., 16. + variant as f32 / 100. + batch as f32, 16.);
            }
            scene.primitives.push(command);
        }
        let (frame, registered) = render(&scene);
        assert_eq!(
            registered, MAX_REGISTERED_IMAGES,
            "variants share the image budget"
        );
        let i = (16 * frame.size.width as usize + 122) * 4;
        assert_eq!(
            &frame.pixels[i..i + 4],
            &[0, 0, 255, 255],
            "cache pressure must not evict an earlier draw in this frame"
        );
    }
    // Dropping all page references releases every size/revision variant.
    scene.primitives.clear();
    assert_eq!(render(&scene).1, 0);
}

#[test]
fn repeated_svg_sizes_survive_scrolling_cpu() {
    let mut renderer = VelloCpuRenderer::new();
    check_repeated_svg(|scene| renderer.render_rgba(scene).unwrap());
}

#[test]
fn repeated_svg_sizes_survive_scrolling_hybrid() {
    let Ok(mut renderer) = futures::executor::block_on(VelloHybridRenderer::new_headless()) else {
        eprintln!("Repeated SVG size regression not exercised: no Hybrid adapter");
        return;
    };
    check_repeated_svg(|scene| renderer.render_rgba(scene).unwrap());
}
