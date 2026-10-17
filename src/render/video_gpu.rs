//! Video frames on the GPU for the Hybrid renderer. A frame's Y'CbCr planes
//! are uploaded as they are (8-bit or 16-bit samples: 1.5 bytes a pixel for
//! 8-bit 4:2:0 rather than RGBA's 4), and a render pass converts them to
//! R'G'B' straight into the frame's slot of Vello's image atlas, where the
//! page's draws sample it like any image. The CPU neither converts, nor
//! premultiplies (video is opaque), nor copies the picture again.
//!
//! The conversion is H.273's (`VideoFrame::rgb_rows`: quantization §8.1,
//! KR/KB matrices §8.3) at full precision for every bit depth, the matrices
//! the CPU reference (`VideoFrame::to_rgba`) applies in 8-bit fixed point.
//! Subsampled chroma is interpolated bilinearly at its sample positions:
//! H.273 ChromaSampleLocType 0, the MPEG and web default (co-sited with
//! even luma columns, between luma rows for 4:2:0), where the CPU path
//! repeats each chroma sample. Frames larger than the atlas allows are
//! scaled down in the same pass.

use std::collections::HashMap;

use vello_gpu::AtlasWriter;
use wgpu::util::DeviceExt;

use crate::media::frame::{Layout, Plane, PlaneData, VideoFrame};

/// Plane textures of a surface not drawn for this many frames are released.
const IDLE_FRAMES: u64 = 120;

const SHADER: &str = r#"
struct Params {
    r: vec4<f32>,
    g: vec4<f32>,
    b: vec4<f32>,
    // Visible luma size, and the output's: frames above the atlas limit
    // are scaled down.
    luma_size: vec2<f32>,
    // Chroma sample index of a luma position: (luma - offset) * scale.
    chroma_scale: vec2<f32>,
    chroma_offset: vec2<f32>,
    // 0: three planes; 1: luma and interleaved chroma; 2: luma only.
    arrangement: u32,
    // Chroma's neutral value, for luma-only pictures.
    neutral: f32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var luma: texture_2d<u32>;
@group(0) @binding(2) var chroma_a: texture_2d<u32>;
@group(0) @binding(3) var chroma_b: texture_2d<u32>;

struct Varying {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// One triangle covering the viewport, which is the frame's atlas slot.
@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Varying {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: Varying;
    out.position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}

fn fetch(plane: texture_2d<u32>, position: vec2<i32>) -> vec2<f32> {
    let last = vec2<i32>(textureDimensions(plane)) - vec2<i32>(1, 1);
    return vec2<f32>(textureLoad(plane, clamp(position, vec2<i32>(0, 0), last), 0).xy);
}

// Bilinear interpolation between sample centres at integer positions; an
// integer position reads its sample exactly.
fn interpolate(plane: texture_2d<u32>, position: vec2<f32>) -> vec2<f32> {
    let base = floor(position);
    let weight = position - base;
    let at = vec2<i32>(base);
    let top = mix(fetch(plane, at), fetch(plane, at + vec2<i32>(1, 0)), weight.x);
    let bottom = mix(
        fetch(plane, at + vec2<i32>(0, 1)),
        fetch(plane, at + vec2<i32>(1, 1)),
        weight.x,
    );
    return mix(top, bottom, weight.y);
}

@fragment
fn fs_main(in: Varying) -> @location(0) vec4<f32> {
    let position = in.uv * params.luma_size - vec2<f32>(0.5, 0.5);
    let y = interpolate(luma, position).x;
    let at = (position - params.chroma_offset) * params.chroma_scale;
    var chroma = vec2<f32>(params.neutral, params.neutral);
    if params.arrangement == 0u {
        chroma = vec2<f32>(interpolate(chroma_a, at).x, interpolate(chroma_b, at).x);
    } else if params.arrangement == 1u {
        chroma = interpolate(chroma_a, at);
    }
    let ycc = vec4<f32>(y, chroma.x, chroma.y, 1.0);
    let rgb = vec3<f32>(dot(params.r, ycc), dot(params.g, ycc), dot(params.b, ycc));
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// How a frame's planes are laid out on the GPU: what decides whether a
/// surface's textures can be reused for its next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    /// Texture formats and sizes, luma first.
    planes: [Option<(wgpu::TextureFormat, u32, u32)>; 3],
}

/// The textures one video surface's frames are uploaded into.
struct SurfacePlanes {
    shape: Shape,
    textures: Vec<wgpu::Texture>,
    views: Vec<wgpu::TextureView>,
    last_used_frame: u64,
}

/// A frame uploaded and ready to convert into an atlas slot.
pub(super) struct PreparedFrame {
    bind_group: wgpu::BindGroup,
    width: u16,
    height: u16,
}

/// Converts video frames for one Hybrid device.
pub(super) struct VideoConverter {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    /// Stands in for the chroma planes a picture does not have.
    unused: wgpu::TextureView,
    surfaces: HashMap<u64, SurfacePlanes>,
    max_dimension: u32,
}

/// Sample count of each pixel of a plane: 2 for interleaved chroma.
fn components(format: wgpu::TextureFormat) -> u32 {
    match format {
        wgpu::TextureFormat::Rg8Uint | wgpu::TextureFormat::Rg16Uint => 2,
        _ => 1,
    }
}

fn bytes_per_sample(format: wgpu::TextureFormat) -> u32 {
    match format {
        wgpu::TextureFormat::R16Uint | wgpu::TextureFormat::Rg16Uint => 2,
        _ => 1,
    }
}

impl Shape {
    /// The planes of `frame`'s visible picture, or `None` for a frame the
    /// converter does not take (the CPU path presents it).
    fn of(frame: &VideoFrame) -> Option<Self> {
        if !frame.valid() {
            return None;
        }
        let wide = frame.bit_depth > 8;
        let (single, pair) = if wide {
            (wgpu::TextureFormat::R16Uint, wgpu::TextureFormat::Rg16Uint)
        } else {
            (wgpu::TextureFormat::R8Uint, wgpu::TextureFormat::Rg8Uint)
        };
        let (width, height) = (frame.width, frame.height);
        let mut planes = [Some((single, width, height)), None, None];
        match frame.layout {
            Layout::Monochrome => {}
            Layout::Planar {
                chroma_shift: (sx, sy),
            } if sx <= 1 && sy <= 1 => {
                let chroma = (single, width.div_ceil(1 << sx), height.div_ceil(1 << sy));
                planes[1] = Some(chroma);
                planes[2] = Some(chroma);
            }
            Layout::Planar { .. } => return None,
            Layout::SemiPlanar => {
                planes[1] = Some((pair, width.div_ceil(2), height.div_ceil(2)));
            }
        }
        // Every plane's samples must be of the depth the format holds.
        let matches = |plane: &Plane| match plane.data {
            PlaneData::U8(_) => !wide,
            PlaneData::U16(_) => wide,
        };
        frame
            .planes
            .iter()
            .take(3)
            .all(matches)
            .then_some(Self { planes })
    }
}

impl VideoConverter {
    pub(super) fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Video Conversion Shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let plane = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Uint,
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Video Conversion Bind Group Layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                plane(1),
                plane(2),
                plane(3),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Video Conversion Pipeline Layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Video Conversion Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                // Vello's image atlas.
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let unused = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("Video Unused Plane"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Uint,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            pipeline,
            layout,
            unused,
            surfaces: HashMap::new(),
            max_dimension: device.limits().max_texture_dimension_2d,
        }
    }

    /// Upload `frame` of video surface `surface` (recording the copies in
    /// `encoder`, ahead of the conversion) for conversion to `width` x
    /// `height` atlas pixels. `None` when the converter does not take it.
    pub(super) fn prepare(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        surface: u64,
        frame: &VideoFrame,
        (width, height): (u16, u16),
        frame_id: u64,
    ) -> Option<PreparedFrame> {
        let shape = Shape::of(frame)?;
        if shape
            .planes
            .iter()
            .flatten()
            .any(|&(_, w, h)| w > self.max_dimension || h > self.max_dimension)
        {
            return None;
        }
        let planes = self
            .surfaces
            .entry(surface)
            .and_modify(|planes| {
                if planes.shape != shape {
                    *planes = SurfacePlanes::new(device, shape);
                }
            })
            .or_insert_with(|| SurfacePlanes::new(device, shape));
        planes.last_used_frame = frame_id;
        upload(device, encoder, frame, planes)?;
        let rows = frame.rgb_rows();
        let (chroma_scale, chroma_offset) = match frame.layout {
            Layout::Planar {
                chroma_shift: (sx, sy),
            } => (
                [1.0 / f32::from(1u8 << sx), 1.0 / f32::from(1u8 << sy)],
                [0.0, if sy == 1 { 0.5 } else { 0.0 }],
            ),
            Layout::SemiPlanar => ([0.5, 0.5], [0.0, 0.5]),
            Layout::Monochrome => ([1.0, 1.0], [0.0, 0.0]),
        };
        let layout = match frame.layout {
            Layout::Planar { .. } => 0u32,
            Layout::SemiPlanar => 1,
            Layout::Monochrome => 2,
        };
        let neutral = (f32::from(frame.bit_depth.clamp(8, 16)) - 1.0).exp2();
        let mut uniform = Vec::with_capacity(80);
        for value in rows.iter().flatten().chain(&[
            frame.width as f32,
            frame.height as f32,
            chroma_scale[0],
            chroma_scale[1],
            chroma_offset[0],
            chroma_offset[1],
        ]) {
            uniform.extend_from_slice(&value.to_le_bytes());
        }
        uniform.extend_from_slice(&layout.to_le_bytes());
        uniform.extend_from_slice(&neutral.to_le_bytes());
        let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Video Conversion Parameters"),
            contents: &uniform,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let view = |index: usize| planes.views.get(index).unwrap_or(&self.unused);
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Video Conversion Bind Group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view(0)),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(view(1)),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(view(2)),
                },
            ],
        });
        Some(PreparedFrame {
            bind_group,
            width,
            height,
        })
    }

    /// The atlas writer that converts `frame` into its slot.
    pub(super) fn writer<'a>(&'a self, frame: &'a PreparedFrame) -> ConvertInto<'a> {
        ConvertInto {
            pipeline: &self.pipeline,
            frame,
        }
    }

    /// Release the planes of surfaces no longer drawn.
    pub(super) fn release_idle(&mut self, frame_id: u64) {
        self.surfaces
            .retain(|_, planes| frame_id.saturating_sub(planes.last_used_frame) < IDLE_FRAMES);
    }
}

impl SurfacePlanes {
    fn new(device: &wgpu::Device, shape: Shape) -> Self {
        let textures: Vec<wgpu::Texture> = shape
            .planes
            .iter()
            .flatten()
            .map(|&(format, width, height)| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("Video Plane"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            })
            .collect();
        let views = textures
            .iter()
            .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect();
        Self {
            shape,
            textures,
            views,
            last_used_frame: 0,
        }
    }
}

/// Copy the visible part of each plane into one staging buffer (rows padded
/// to wgpu's 256-byte copy pitch) and record its copies into the textures.
fn upload(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    frame: &VideoFrame,
    planes: &SurfacePlanes,
) -> Option<()> {
    let layouts: Vec<(usize, u32, u32, u32)> = {
        let mut offset = 0usize;
        planes
            .shape
            .planes
            .iter()
            .flatten()
            .map(|&(format, width, height)| {
                let row = width * components(format) * bytes_per_sample(format);
                let pitch = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
                let start = offset;
                offset += pitch as usize * height as usize;
                (start, row, pitch, height)
            })
            .collect()
    };
    let total = layouts.last().map_or(0, |&(start, _, pitch, height)| {
        start + pitch as usize * height as usize
    });
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Video Plane Upload"),
        size: total as u64,
        usage: wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: true,
    });
    {
        // Mapped memory may be write-combining: rows are written once, in
        // order, and never read.
        let mut mapped = staging.slice(..).get_mapped_range_mut().ok()?;
        let mut wide_row = Vec::new();
        for (plane, &(start, row, pitch, rows)) in frame.planes.iter().zip(&layouts) {
            let row = row as usize;
            for line in 0..rows as usize {
                let destination = start + line * pitch as usize;
                let first = line * plane.stride;
                match &plane.data {
                    PlaneData::U8(data) => mapped
                        .slice(destination..destination + row)
                        .copy_from_slice(&data[first..first + row]),
                    PlaneData::U16(data) => {
                        wide_row.clear();
                        wide_row.extend(
                            data[first..first + row / 2]
                                .iter()
                                .flat_map(|sample| sample.to_le_bytes()),
                        );
                        mapped
                            .slice(destination..destination + row)
                            .copy_from_slice(&wide_row);
                    }
                }
            }
        }
    }
    staging.unmap();
    for (texture, &(start, _, pitch, rows)) in planes.textures.iter().zip(&layouts) {
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: start as u64,
                    bytes_per_row: Some(pitch),
                    rows_per_image: Some(rows),
                },
            },
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            texture.size(),
        );
    }
    Some(())
}

/// Vello's atlas writer for a prepared frame: a render pass converting its
/// planes into the slot, recorded in the encoder in order with Vello's own
/// atlas uploads, clears and copies.
pub(super) struct ConvertInto<'a> {
    pipeline: &'a wgpu::RenderPipeline,
    frame: &'a PreparedFrame,
}

impl AtlasWriter for ConvertInto<'_> {
    fn width(&self) -> u16 {
        self.frame.width
    }

    fn height(&self) -> u16 {
        self.frame.height
    }

    fn write_to_atlas(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        atlas_texture: &wgpu::Texture,
        offset: [u16; 2],
        width: u16,
        height: u16,
    ) {
        let view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Video Conversion"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                // Only this slot is drawn; the rest of the atlas stays.
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            occlusion_query_set: None,
            timestamp_writes: None,
            multiview_mask: None,
        });
        let (x, y) = (f32::from(offset[0]), f32::from(offset[1]));
        pass.set_viewport(x, y, f32::from(width), f32::from(height), 0.0, 1.0);
        pass.set_scissor_rect(
            u32::from(offset[0]),
            u32::from(offset[1]),
            u32::from(width),
            u32::from(height),
        );
        pass.set_pipeline(self.pipeline);
        pass.set_bind_group(0, &self.frame.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
