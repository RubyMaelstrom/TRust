//! Video frames the Hybrid renderer converts on the GPU (`video_gpu`) show
//! the pixels of the CPU reference conversion (`VideoFrame::to_rgba`), for
//! every plane layout, bit depth, range and matrix, and interpolate
//! subsampled chroma at its sample positions.

use super::vello_cpu::{OwnedRgbaFrame, VelloCpuRenderer};
use super::vello_hybrid::VelloHybridRenderer;
use super::*;
use crate::core::ScaleFactor;
use crate::media::frame::{Layout, Plane, VideoFrame};
use crate::media::player::VideoSurface;
use crate::media::track::ColorInfo;

const WIDTH: u32 = 37;
const HEIGHT: u32 = 22;

fn video_scene(handle: ImageHandle) -> Scene {
    let mut scene = Scene {
        viewport: ViewportMetrics::from_physical(
            PhysicalSize::new(WIDTH, HEIGHT),
            ScaleFactor::new(1.0),
        ),
        primitives: Vec::new(),
        controls: Vec::new(),
        content_viewport: CssRect::new(0., 0., WIDTH as f32, HEIGHT as f32),
        image_store: Default::default(),
        canvas_images: Default::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::new(WIDTH as f32, HEIGHT as f32),
    };
    scene.primitives.push(DisplayCommand::Image {
        rect: CssRect::new(0., 0., WIDTH as f32, HEIGHT as f32),
        handle,
        source_rect: None,
        fit: ImageFit::Fill,
        sampling: ImageSampling::Nearest,
        clip: None,
        node: 0,
        link: None,
    });
    scene
}

/// A picture whose luma varies and whose chroma is `chroma(column, row)`
/// in chroma samples, at `depth` bits (16: P010's left-aligned 10 bits).
fn picture(
    layout: Layout,
    depth: u8,
    color: ColorInfo,
    chroma: impl Fn(usize, usize) -> (u32, u32),
) -> VideoFrame {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    // Sample values are given at 8 bits and scaled to the depth.
    let scale = |value: u32| match depth {
        8 => value,
        16 => value << 8,
        _ => value << (depth - 8),
    };
    let luma: Vec<u32> = (0..w * h)
        .map(|i| scale(16 + ((i % w) * 5 + (i / w) * 9) as u32 % 220))
        .collect();
    let plane = |samples: Vec<u32>, stride: usize, width: usize, height: usize| {
        if depth == 8 {
            Plane::u8(
                samples.into_iter().map(|s| s as u8).collect(),
                stride,
                width,
                height,
            )
        } else {
            Plane::u16(
                samples.into_iter().map(|s| s as u16).collect(),
                stride,
                width,
                height,
            )
        }
    };
    let mut planes = vec![plane(luma, w, w, h)];
    match layout {
        Layout::Monochrome => {}
        Layout::Planar {
            chroma_shift: (sx, sy),
        } => {
            let (cw, ch) = (w.div_ceil(1 << sx), h.div_ceil(1 << sy));
            let (mut u, mut v) = (Vec::new(), Vec::new());
            for row in 0..ch {
                for column in 0..cw {
                    let (cb, cr) = chroma(column, row);
                    u.push(scale(cb));
                    v.push(scale(cr));
                }
            }
            planes.push(plane(u, cw, cw, ch));
            planes.push(plane(v, cw, cw, ch));
        }
        Layout::SemiPlanar => {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let mut uv = Vec::new();
            for row in 0..ch {
                for column in 0..cw {
                    let (cb, cr) = chroma(column, row);
                    uv.extend([scale(cb), scale(cr)]);
                }
            }
            planes.push(plane(uv, 2 * cw, cw, ch));
        }
    }
    VideoFrame {
        pts: 0,
        duration: 0,
        width: WIDTH,
        height: HEIGHT,
        display_width: WIDTH,
        display_height: HEIGHT,
        bit_depth: depth,
        layout,
        color,
        planes,
    }
}

fn color(matrix: u8, full_range: bool) -> ColorInfo {
    ColorInfo {
        primaries: 1,
        transfer: 1,
        matrix,
        full_range,
    }
}

fn pixel(frame: &OwnedRgbaFrame, x: usize, y: usize) -> [u8; 4] {
    let i = (y * frame.size.width as usize + x) * 4;
    frame.pixels[i..i + 4].try_into().unwrap()
}

#[test]
fn hybrid_video_frames_match_the_cpu_conversion() {
    let Ok(mut hybrid) = futures::executor::block_on(VelloHybridRenderer::new_headless()) else {
        eprintln!("skipped: no GPU adapter");
        return;
    };
    let surface = VideoSurface::new();
    let scene = video_scene(ImageHandle::for_video(surface.id()));
    let planar = |sx, sy| Layout::Planar {
        chroma_shift: (sx, sy),
    };
    // Chroma constant over the picture: interpolation cannot differ from
    // the CPU's repetition, so the two conversions agree to rounding.
    let flat = |_, _| (90, 200);
    let cases = [
        ("4:2:0", picture(planar(1, 1), 8, color(1, false), flat)),
        (
            "NV12",
            picture(Layout::SemiPlanar, 8, color(1, false), flat),
        ),
        ("4:2:2", picture(planar(1, 0), 8, color(6, false), flat)),
        ("4:4:4", picture(planar(0, 0), 8, color(9, false), flat)),
        (
            "gray",
            picture(Layout::Monochrome, 8, color(1, false), flat),
        ),
        ("full range", picture(planar(1, 1), 8, color(1, true), flat)),
        ("10-bit", picture(planar(1, 1), 10, color(9, false), flat)),
        (
            "P010",
            picture(Layout::SemiPlanar, 16, color(1, false), flat),
        ),
        (
            "GBR",
            picture(planar(0, 0), 8, color(0, true), |c, r| {
                ((c * 7) as u32, (r * 11) as u32)
            }),
        ),
    ];
    for (name, frame) in cases {
        // The pictures are presentable: neither path may fall back to the
        // placeholder both would otherwise agree on.
        assert!(frame.valid(), "{name}");
        let expected = frame.to_rgba(1).unwrap().0;
        surface.show_for_test(frame);
        let cpu = VelloCpuRenderer::new().render_rgba(&scene).unwrap();
        let gpu = hybrid.render_rgba(&scene).unwrap();
        for y in 0..HEIGHT as usize {
            for x in 0..WIDTH as usize {
                let (a, b) = (pixel(&cpu, x, y), pixel(&gpu, x, y));
                let at = (y * WIDTH as usize + x) * 4;
                assert_eq!(
                    a,
                    expected[at..at + 4],
                    "{name}: the CPU path shows the frame"
                );
                // The CPU path reduces 10-bit samples to 8 before its
                // matrix; the GPU keeps them, which may differ by a step.
                let tolerance = if name == "10-bit" || name == "P010" {
                    2
                } else {
                    1
                };
                assert!(
                    a.iter().zip(&b).all(|(a, b)| a.abs_diff(*b) <= tolerance),
                    "{name} at ({x}, {y}): CPU {a:?}, GPU {b:?}"
                );
            }
        }
    }
}

#[test]
fn hybrid_video_chroma_is_interpolated_at_its_sample_positions() {
    // H.273 ChromaSampleLocType 0: 4:2:0 chroma is co-sited with even luma
    // columns and lies midway between luma rows. With chroma changing only
    // across columns, even columns show their sample exactly (as the CPU's
    // repetition does), and odd columns the mean of their neighbours.
    let Ok(mut hybrid) = futures::executor::block_on(VelloHybridRenderer::new_headless()) else {
        eprintln!("skipped: no GPU adapter");
        return;
    };
    let surface = VideoSurface::new();
    let scene = video_scene(ImageHandle::for_video(surface.id()));
    let ramp = |column: usize, _| (128, 60 + 8 * column as u32);
    let frame = picture(
        Layout::Planar {
            chroma_shift: (1, 1),
        },
        8,
        color(1, true),
        ramp,
    );
    let rows = frame.rgb_rows();
    surface.show_for_test(frame.clone());
    let gpu = hybrid.render_rgba(&scene).unwrap();
    let luma = |x: usize, y: usize| match &frame.planes[0].data {
        crate::media::frame::PlaneData::U8(data) => f32::from(data[y * WIDTH as usize + x]),
        _ => unreachable!(),
    };
    let red = |y: f32, cr: f32| {
        let [a, b, c, d] = rows[0];
        ((a * y + b * 128.0 + c * cr + d).clamp(0.0, 1.0) * 255.0).round()
    };
    for y in 0..HEIGHT as usize {
        for x in 0..WIDTH as usize - 1 {
            let column = x / 2;
            let cr = if x % 2 == 0 {
                ramp(column, 0).1 as f32
            } else {
                (ramp(column, 0).1 + ramp(column + 1, 0).1) as f32 / 2.0
            };
            let expected = red(luma(x, y), cr);
            let actual = f32::from(pixel(&gpu, x, y)[0]);
            assert!(
                (expected - actual).abs() <= 1.0,
                "({x}, {y}): red {actual}, expected {expected}"
            );
        }
    }
}
