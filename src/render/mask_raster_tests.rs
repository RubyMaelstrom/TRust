//! CSS Masking 1 #MaskValues and #the-mask-composite as raster groups: a
//! masked element's group is multiplied by a destination-in group of its
//! mask layers, which combine with each other through Porter-Duff operators.

use super::vello_cpu::{OwnedRgbaFrame, VelloCpuRenderer};
use super::vello_hybrid::VelloHybridRenderer;
use super::*;
use crate::core::ScaleFactor;

const RED: PaintColor = PaintColor::Rgba(255, 0, 0, 255);
const WHITE: PaintColor = PaintColor::Rgba(255, 255, 255, 255);

fn fill(rect: CssRect, color: PaintColor) -> DisplayCommand {
    DisplayCommand::Fill {
        shape: PaintShape::Rect(rect),
        brush: PaintBrush::Solid(color),
    }
}

fn layer(compose: CompositeOperator, clip: Option<CssRect>) -> DisplayCommand {
    DisplayCommand::PushLayer(CompositingLayer {
        compose,
        clip,
        ..CompositingLayer::new(1.0, BlendMode::Normal, Arc::from([]))
    })
}

/// A white page with a red 40x20 element over it whose group is bounded by
/// `bounds` and masked by `mask` (commands painted inside the mask group).
fn masked_scene(bounds: Option<CssRect>, mask: Vec<DisplayCommand>) -> Scene {
    let mut primitives = vec![
        fill(CssRect::new(0., 0., 40., 20.), WHITE),
        layer(CompositeOperator::SourceOver, bounds),
        fill(CssRect::new(0., 0., 40., 20.), RED),
        layer(CompositeOperator::DestinationIn, None),
    ];
    primitives.extend(mask);
    primitives.extend([DisplayCommand::PopLayer, DisplayCommand::PopLayer]);
    Scene {
        viewport: ViewportMetrics::from_physical(PhysicalSize::new(40, 20), ScaleFactor::new(1.)),
        primitives,
        controls: Vec::new(),
        content_viewport: CssRect::new(0., 0., 40., 20.),
        image_store: Default::default(),
        canvas_images: Default::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::new(40., 20.),
    }
}

fn pixel(frame: &OwnedRgbaFrame, x: usize, y: usize) -> [u8; 4] {
    let at = (y * frame.size.width as usize + x) * 4;
    frame.pixels[at..at + 4].try_into().unwrap()
}

fn check_alpha_mask(mut render: impl FnMut(&Scene) -> OwnedRgbaFrame) {
    // An opaque mask region keeps the content, a half-transparent one halves
    // it, and an uncovered region (inside or outside the paint bound)
    // removes it: CSS Masking 1 #MaskValues treats it as transparent black.
    let frame = render(&masked_scene(
        Some(CssRect::new(0., 0., 30., 20.)),
        vec![
            fill(
                CssRect::new(0., 0., 10., 20.),
                PaintColor::Rgba(0, 0, 0, 255),
            ),
            fill(
                CssRect::new(10., 0., 10., 20.),
                PaintColor::Rgba(0, 0, 255, 128),
            ),
        ],
    ));
    assert_eq!(pixel(&frame, 5, 10), [255, 0, 0, 255]);
    let half = pixel(&frame, 15, 10);
    assert_eq!(half[0], 255, "{half:?}");
    assert!((120..=135).contains(&half[1]), "{half:?}");
    assert_eq!(half[1], half[2], "{half:?}");
    assert_eq!(pixel(&frame, 25, 10), [255, 255, 255, 255]);
    assert_eq!(pixel(&frame, 35, 10), [255, 255, 255, 255]);
}

fn check_mask_composite(mut render: impl FnMut(&Scene) -> OwnedRgbaFrame) {
    // CSS Masking 1 #the-mask-composite: the upper layer (x 10..30) is the
    // source and the lower one (x 0..20) the destination.
    let black = PaintColor::Rgba(0, 0, 0, 255);
    for (operator, expected) in [
        (CompositeOperator::SourceOver, [true, true, true, false]),
        (CompositeOperator::SourceOut, [false, false, true, false]),
        (CompositeOperator::SourceIn, [false, true, false, false]),
        (CompositeOperator::Xor, [true, false, true, false]),
    ] {
        let frame = render(&masked_scene(
            None,
            vec![
                fill(CssRect::new(0., 0., 20., 20.), black),
                layer(operator, None),
                fill(CssRect::new(10., 0., 20., 20.), black),
                DisplayCommand::PopLayer,
            ],
        ));
        for (x, shown) in [5, 15, 25, 35].into_iter().zip(expected) {
            let color = if shown {
                [255, 0, 0, 255]
            } else {
                [255, 255, 255, 255]
            };
            assert_eq!(pixel(&frame, x, 10), color, "{operator:?} at x={x}");
        }
    }
}

fn check_luminance_mask(mut render: impl FnMut(&Scene) -> OwnedRgbaFrame) {
    // CSS Masking 1 #MaskValues: a luminance mask layer is its tiles' alpha
    // multiplied (destination-in) by their luminanceToAlpha.
    let tiles = [
        fill(CssRect::new(0., 0., 20., 20.), WHITE),
        fill(
            CssRect::new(20., 0., 10., 20.),
            PaintColor::Rgba(0, 0, 0, 255),
        ),
        fill(
            CssRect::new(30., 0., 10., 20.),
            PaintColor::Rgba(255, 255, 255, 128),
        ),
    ];
    let luminance = CssFilter::ColorMatrix([
        0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.2125, 0.7154,
        0.0721, 0.0, 0.0,
    ]);
    let mut mask = vec![layer(CompositeOperator::SourceOver, None)];
    mask.extend(tiles.clone());
    mask.push(DisplayCommand::PushLayer(CompositingLayer {
        compose: CompositeOperator::DestinationIn,
        ..CompositingLayer::new(1.0, BlendMode::Normal, Arc::from([luminance]))
    }));
    mask.extend(tiles);
    mask.extend([DisplayCommand::PopLayer, DisplayCommand::PopLayer]);
    let frame = render(&masked_scene(None, mask));
    assert_eq!(pixel(&frame, 10, 10), [255, 0, 0, 255]);
    assert_eq!(pixel(&frame, 25, 10), [255, 255, 255, 255]);
    let half = pixel(&frame, 35, 10);
    assert!(half[0] == 255 && (120..=135).contains(&half[1]), "{half:?}");
}

#[test]
fn destination_in_groups_mask_their_backdrop_cpu() {
    let mut renderer = VelloCpuRenderer::new();
    check_alpha_mask(|scene| renderer.render_rgba(scene).unwrap());
    check_mask_composite(|scene| renderer.render_rgba(scene).unwrap());
    check_luminance_mask(|scene| renderer.render_rgba(scene).unwrap());
}

#[test]
fn destination_in_groups_mask_their_backdrop_hybrid() {
    let Ok(mut renderer) = futures::executor::block_on(VelloHybridRenderer::new_headless()) else {
        eprintln!("Mask compositing regression not exercised: no Hybrid adapter");
        return;
    };
    check_alpha_mask(|scene| renderer.render_rgba(scene).unwrap());
    check_mask_composite(|scene| renderer.render_rgba(scene).unwrap());
    check_luminance_mask(|scene| renderer.render_rgba(scene).unwrap());
}
