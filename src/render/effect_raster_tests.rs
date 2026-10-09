//! Raster conformance for the effects both backends compute with the CPU
//! reference renderer: Filter Effects 2 backdrop filters and Backdrop Roots,
//! Filter Effects 1 SVG filter graphs, and CSS Masking 1 clip geometry that
//! is not a single nonzero outline. Each check runs on Vello CPU and, when an
//! adapter exists, on Hybrid, which must agree.

use std::sync::Arc;

use super::filter_graph::{
    CompositeOp, FilterGraph, FilterInput, FilterPrimitive, FilterPrimitiveKind,
};
use super::vello_cpu::{OwnedRgbaFrame, VelloCpuRenderer};
use super::vello_hybrid::VelloHybridRenderer;
use super::*;
use crate::core::ScaleFactor;

const WHITE: PaintColor = PaintColor::Rgba(255, 255, 255, 255);
const BLACK: PaintColor = PaintColor::Rgba(0, 0, 0, 255);
const RED: PaintColor = PaintColor::Rgba(255, 0, 0, 255);
const BLUE: PaintColor = PaintColor::Rgba(0, 0, 255, 255);

fn fill(rect: CssRect, color: PaintColor) -> DisplayCommand {
    DisplayCommand::Fill {
        shape: PaintShape::Rect(rect),
        brush: PaintBrush::Solid(color),
    }
}

fn scene(width: u32, height: u32, primitives: Vec<DisplayCommand>) -> Scene {
    Scene {
        viewport: ViewportMetrics::from_physical(
            PhysicalSize::new(width, height),
            ScaleFactor::new(1.),
        ),
        primitives,
        controls: Vec::new(),
        content_viewport: CssRect::new(0., 0., width as f32, height as f32),
        image_store: Default::default(),
        canvas_images: Default::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::new(width as f32, height as f32),
    }
}

fn pixel(frame: &OwnedRgbaFrame, x: usize, y: usize) -> [u8; 4] {
    let at = (y * frame.size.width as usize + x) * 4;
    frame.pixels[at..at + 4].try_into().unwrap()
}

fn invert() -> CssFilter {
    let mut matrix = [0.0; 20];
    for channel in 0..3 {
        matrix[channel * 5 + channel] = -1.0;
        matrix[channel * 5 + 4] = 1.0;
    }
    matrix[18] = 1.0;
    CssFilter::ColorMatrix(matrix)
}

fn backdrop_layer(filters: Vec<CssFilter>, shape: PaintShape) -> DisplayCommand {
    DisplayCommand::PushLayer(CompositingLayer {
        backdrop: Some(Arc::new(BackdropFilter {
            filters: filters.into(),
            shape,
        })),
        backdrop_root: true,
        ..CompositingLayer::new(1.0, BlendMode::Normal, Arc::from([]))
    })
}

fn check_backdrop_filters(mut render: impl FnMut(&Scene) -> OwnedRgbaFrame) {
    // Filter Effects 2 #backdrop-filter-operation: the backdrop behind the
    // border box is filtered and clipped to it (with its radii); the
    // element's own transparent content leaves the filtered backdrop seen.
    let frame = render(&scene(
        60,
        40,
        vec![
            fill(CssRect::new(0., 0., 60., 40.), WHITE),
            fill(CssRect::new(0., 0., 30., 40.), RED),
            backdrop_layer(
                vec![invert()],
                PaintShape::RoundedRect {
                    rect: CssRect::new(10., 10., 40., 20.),
                    radii: CornerRadii {
                        corners: [(8., 8.); 4],
                    },
                },
            ),
            DisplayCommand::PopLayer,
        ],
    ));
    assert_eq!(pixel(&frame, 20, 20), [0, 255, 255, 255], "inverted red");
    assert_eq!(pixel(&frame, 40, 20), [0, 0, 0, 255], "inverted white");
    assert_eq!(pixel(&frame, 5, 20), [255, 0, 0, 255], "outside unchanged");
    assert_eq!(pixel(&frame, 10, 10), [255, 0, 0, 255], "rounded corner");
    assert_eq!(pixel(&frame, 55, 20), [255, 255, 255, 255]);

    // blur() mirrors at the clipped border box edge: a uniform backdrop
    // stays uniform up to the edge instead of fading to transparency.
    let frame = render(&scene(
        40,
        40,
        vec![
            fill(CssRect::new(0., 0., 40., 40.), BLUE),
            backdrop_layer(
                vec![CssFilter::Blur(4.)],
                PaintShape::Rect(CssRect::new(10., 10., 20., 20.)),
            ),
            DisplayCommand::PopLayer,
        ],
    ));
    assert_eq!(pixel(&frame, 10, 10), [0, 0, 255, 255]);
    assert_eq!(pixel(&frame, 20, 20), [0, 0, 255, 255]);

    // #BackdropRoot: a Backdrop Root ancestor bounds the backdrop. Only the
    // white half painted inside the root group is inverted; beyond it the
    // root is transparent and the black page shows through unfiltered.
    let frame = render(&scene(
        40,
        20,
        vec![
            fill(CssRect::new(0., 0., 40., 20.), BLACK),
            DisplayCommand::PushLayer(CompositingLayer {
                backdrop_root: true,
                ..CompositingLayer::new(1.0, BlendMode::Normal, Arc::from([]))
            }),
            fill(CssRect::new(0., 0., 20., 20.), WHITE),
            backdrop_layer(
                vec![invert()],
                PaintShape::Rect(CssRect::new(0., 0., 40., 20.)),
            ),
            DisplayCommand::PopLayer,
            DisplayCommand::PopLayer,
        ],
    ));
    assert_eq!(pixel(&frame, 10, 10), [0, 0, 0, 255], "inverted white");
    assert_eq!(
        pixel(&frame, 30, 10),
        [0, 0, 0, 255],
        "page outside the root"
    );

    // The element's opacity applies to the filtered backdrop too.
    let frame = render(&scene(
        20,
        20,
        vec![
            fill(CssRect::new(0., 0., 20., 20.), WHITE),
            DisplayCommand::PushLayer(CompositingLayer {
                backdrop: Some(Arc::new(BackdropFilter {
                    filters: vec![invert()].into(),
                    shape: PaintShape::Rect(CssRect::new(0., 0., 20., 20.)),
                })),
                backdrop_root: true,
                ..CompositingLayer::new(0.5, BlendMode::Normal, Arc::from([]))
            }),
            DisplayCommand::PopLayer,
        ],
    ));
    let half = pixel(&frame, 10, 10);
    assert!(
        (120..=135).contains(&half[0]) && half[0] == half[2],
        "{half:?}"
    );
}

fn graph(region: CssRect, primitives: Vec<FilterPrimitive>) -> CssFilter {
    CssFilter::Graph(Arc::new(FilterGraph {
        region,
        primitives,
        primitive_space: Affine2d::IDENTITY,
        fill_paint: [0.0; 4],
        stroke_paint: [0.0; 4],
    }))
}

fn check_svg_filter_graphs(mut render: impl FnMut(&Scene) -> OwnedRgbaFrame) {
    // Filter Effects 1 #FilterPrimitiveTree: flood blue, keep it where the
    // SourceGraphic is (feComposite in), then move it by feOffset.
    let region = CssRect::new(0., 0., 40., 40.);
    let filter = graph(
        region,
        vec![
            FilterPrimitive {
                subregion: region,
                linear_rgb: true,
                kind: FilterPrimitiveKind::Flood {
                    color: [0.0, 0.0, 1.0, 1.0],
                },
            },
            FilterPrimitive {
                subregion: region,
                linear_rgb: true,
                kind: FilterPrimitiveKind::Composite {
                    input: FilterInput::Result(0),
                    input2: FilterInput::SourceGraphic,
                    operator: CompositeOp::In,
                },
            },
            FilterPrimitive {
                subregion: region,
                linear_rgb: true,
                kind: FilterPrimitiveKind::Offset {
                    input: FilterInput::Result(1),
                    dx: 10.,
                    dy: 0.,
                },
            },
        ],
    );
    let frame = render(&scene(
        60,
        40,
        vec![
            fill(CssRect::new(0., 0., 60., 40.), WHITE),
            DisplayCommand::PushLayer(CompositingLayer::new(
                1.0,
                BlendMode::Normal,
                Arc::from([filter]),
            )),
            fill(CssRect::new(10., 10., 10., 10.), RED),
            DisplayCommand::PopLayer,
        ],
    ));
    assert_eq!(
        pixel(&frame, 15, 15),
        [255, 255, 255, 255],
        "source replaced"
    );
    assert_eq!(pixel(&frame, 25, 15), [0, 0, 255, 255], "offset flood");
    // An element with an empty filter is not rendered.
    let frame = render(&scene(
        20,
        20,
        vec![
            fill(CssRect::new(0., 0., 20., 20.), WHITE),
            DisplayCommand::PushLayer(CompositingLayer::new(
                1.0,
                BlendMode::Normal,
                Arc::from([graph(CssRect::new(0., 0., 20., 20.), Vec::new())]),
            )),
            fill(CssRect::new(0., 0., 20., 20.), RED),
            DisplayCommand::PopLayer,
        ],
    ));
    assert_eq!(pixel(&frame, 10, 10), [255, 255, 255, 255]);
}

fn check_clip_geometry(mut render: impl FnMut(&Scene) -> OwnedRgbaFrame) {
    use crate::core::CssPoint as P;
    let square = |x: f32, y: f32, size: f32, reversed: bool| {
        let mut points = [
            P::new(x, y),
            P::new(x + size, y),
            P::new(x + size, y + size),
            P::new(x, y + size),
        ];
        if reversed {
            points.reverse();
        }
        let mut elements = vec![PathElement::MoveTo(points[0])];
        elements.extend(points[1..].iter().map(|p| PathElement::LineTo(*p)));
        elements.push(PathElement::Close);
        elements
    };
    // CSS Masking 1 #ClipPathElement: overlapping children of opposite
    // winding are OR'ed, not cancelled; an even-odd child keeps its hole.
    let union = ClipGeometry::Union(vec![
        ClipGeometry::Path {
            elements: square(0., 0., 20., false),
            evenodd: false,
        },
        ClipGeometry::Path {
            elements: square(10., 0., 20., true),
            evenodd: false,
        },
        ClipGeometry::Path {
            elements: [square(40., 0., 20., false), square(45., 5., 10., false)].concat(),
            evenodd: true,
        },
    ]);
    let frame = render(&scene(
        70,
        20,
        vec![
            fill(CssRect::new(0., 0., 70., 20.), WHITE),
            DisplayCommand::PushClip(PaintShape::Geometry(Arc::new(union))),
            fill(CssRect::new(0., 0., 70., 20.), RED),
            DisplayCommand::PopClip,
        ],
    ));
    for (x, inside) in [
        (5, true),
        (15, true),
        (25, true),
        (35, false),
        (42, true),
        (50, false),
        (65, false),
    ] {
        let expected = if inside {
            [255, 0, 0, 255]
        } else {
            [255, 255, 255, 255]
        };
        assert_eq!(pixel(&frame, x, 10), expected, "x={x}");
    }
    // An intersection (a clipPath clipped by another).
    let frame = render(&scene(
        30,
        10,
        vec![
            fill(CssRect::new(0., 0., 30., 10.), WHITE),
            DisplayCommand::PushClip(PaintShape::Geometry(Arc::new(ClipGeometry::Intersection(
                vec![
                    ClipGeometry::Union(vec![
                        ClipGeometry::Path {
                            elements: square(0., 0., 10., false),
                            evenodd: false,
                        },
                        ClipGeometry::Path {
                            elements: square(20., 0., 10., false),
                            evenodd: false,
                        },
                    ]),
                    ClipGeometry::Path {
                        elements: square(5., 0., 20., false),
                        evenodd: false,
                    },
                ],
            )))),
            fill(CssRect::new(0., 0., 30., 10.), RED),
            DisplayCommand::PopClip,
        ],
    ));
    for (x, inside) in [(2, false), (7, true), (15, false), (22, true), (27, false)] {
        let expected = if inside {
            [255, 0, 0, 255]
        } else {
            [255, 255, 255, 255]
        };
        assert_eq!(pixel(&frame, x, 5), expected, "x={x}");
    }
    // An empty clipPath clips everything away.
    let frame = render(&scene(
        10,
        10,
        vec![
            fill(CssRect::new(0., 0., 10., 10.), WHITE),
            DisplayCommand::PushClip(PaintShape::Geometry(Arc::new(ClipGeometry::Union(
                Vec::new(),
            )))),
            fill(CssRect::new(0., 0., 10., 10.), RED),
            DisplayCommand::PopClip,
        ],
    ));
    assert_eq!(pixel(&frame, 5, 5), [255, 255, 255, 255]);
}

#[test]
fn backdrop_filters_svg_graphs_and_clip_geometry_cpu() {
    let mut renderer = VelloCpuRenderer::new();
    check_backdrop_filters(|scene| renderer.render_rgba(scene).unwrap());
    check_svg_filter_graphs(|scene| renderer.render_rgba(scene).unwrap());
    check_clip_geometry(|scene| renderer.render_rgba(scene).unwrap());
}

#[test]
fn backdrop_filters_svg_graphs_and_clip_geometry_hybrid() {
    let Ok(mut renderer) = futures::executor::block_on(VelloHybridRenderer::new_headless()) else {
        eprintln!("Effect raster regression not exercised: no Hybrid adapter");
        return;
    };
    check_backdrop_filters(|scene| renderer.render_rgba(scene).unwrap());
    check_svg_filter_graphs(|scene| renderer.render_rgba(scene).unwrap());
    check_clip_geometry(|scene| renderer.render_rgba(scene).unwrap());
}

#[test]
fn hit_testing_follows_clip_geometry() {
    // CSS Masking 1 #clipping-paths: pointer events are not dispatched in
    // clipped-out regions; the union is exact, holes included.
    let ring = ClipGeometry::Path {
        elements: vec![
            PathElement::MoveTo(crate::core::CssPoint::new(0., 0.)),
            PathElement::LineTo(crate::core::CssPoint::new(30., 0.)),
            PathElement::LineTo(crate::core::CssPoint::new(30., 30.)),
            PathElement::LineTo(crate::core::CssPoint::new(0., 30.)),
            PathElement::Close,
            PathElement::MoveTo(crate::core::CssPoint::new(10., 10.)),
            PathElement::LineTo(crate::core::CssPoint::new(20., 10.)),
            PathElement::LineTo(crate::core::CssPoint::new(20., 20.)),
            PathElement::LineTo(crate::core::CssPoint::new(10., 20.)),
            PathElement::Close,
        ],
        evenodd: true,
    };
    assert!(ring.contains(crate::core::CssPoint::new(5., 5.)));
    assert!(!ring.contains(crate::core::CssPoint::new(15., 15.)));
    assert!(!ring.contains(crate::core::CssPoint::new(35., 15.)));
    assert_eq!(ring.bounds(), Some(CssRect::new(0., 0., 30., 30.)));
}

#[test]
fn effects_that_read_beyond_their_commands_force_full_damage() {
    // A backdrop filter reads anything painted behind it, and a filter
    // spreads a changed command's pixels: a partial repaint of only the
    // changed command would leave stale filtered pixels.
    let backdrop = scene(
        40,
        20,
        vec![
            fill(CssRect::new(0., 0., 10., 10.), RED),
            backdrop_layer(
                vec![invert()],
                PaintShape::Rect(CssRect::new(20., 0., 20., 20.)),
            ),
            DisplayCommand::PopLayer,
        ],
    );
    let mut moved = backdrop.clone();
    moved.primitives[0] = fill(CssRect::new(0., 0., 10., 10.), BLUE);
    assert_eq!(scene_damage(&backdrop, &moved), SceneDamage::Full);
    let blurred = scene(
        40,
        20,
        vec![
            DisplayCommand::PushLayer(CompositingLayer::new(
                1.0,
                BlendMode::Normal,
                Arc::from([CssFilter::Blur(5.)]),
            )),
            fill(CssRect::new(0., 0., 10., 10.), RED),
            DisplayCommand::PopLayer,
        ],
    );
    let mut recolored = blurred.clone();
    recolored.primitives[1] = fill(CssRect::new(0., 0., 10., 10.), BLUE);
    assert_eq!(scene_damage(&blurred, &recolored), SceneDamage::Full);
    // Without filters the damage stays bounded.
    let plain = scene(40, 20, vec![fill(CssRect::new(0., 0., 10., 10.), RED)]);
    let mut changed = plain.clone();
    changed.primitives[0] = fill(CssRect::new(0., 0., 10., 10.), BLUE);
    assert!(matches!(
        scene_damage(&plain, &changed),
        SceneDamage::Partial(_)
    ));
}
