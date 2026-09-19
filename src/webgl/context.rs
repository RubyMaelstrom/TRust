use super::driver::Driver;
use glow::{self as gl, HasContext};
use std::collections::{HashMap, VecDeque};

pub(super) const MAX_BYTES: usize = 256 * 1024 * 1024;
pub(super) const MAX_OBJECTS: usize = 16384;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Attributes {
    pub alpha: bool,
    pub depth: bool,
    pub stencil: bool,
    pub premultiplied: bool,
    pub preserve: bool,
    pub fail_caveat: bool,
}
impl Default for Attributes {
    fn default() -> Self {
        Self {
            alpha: true,
            depth: true,
            stencil: false,
            premultiplied: true,
            preserve: false,
            fail_caveat: false,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) enum Reply {
    #[default]
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    Array(Vec<Reply>),
    Bytes(Vec<u8>),
}
impl From<u32> for Reply {
    fn from(n: u32) -> Self {
        Self::Number(n as f64)
    }
}
impl From<i32> for Reply {
    fn from(n: i32) -> Self {
        Self::Number(n as f64)
    }
}
impl Reply {
    pub fn numbers(values: impl IntoIterator<Item = f64>) -> Self {
        Self::Array(values.into_iter().map(Self::Number).collect())
    }
}

pub(super) struct Buffer {
    pub handle: gl::Buffer,
    pub deleted: bool,
    pub target: u32,
    pub usage: u32,
    pub bytes: Vec<u8>,
}
pub(super) struct Shader {
    pub handle: gl::Shader,
    pub kind: u32,
    pub source: String,
    pub log: String,
    pub compiled: bool,
    pub deleted: bool,
}
pub(super) struct Program {
    pub handle: gl::Program,
    pub attached: Vec<u32>,
    pub linked: bool,
    pub deleted: bool,
    pub serial: u32,
    pub active: Vec<u32>,
    pub samplers: Vec<(gl::UniformLocation, u32)>,
}
pub(super) struct Uniform {
    pub location: gl::UniformLocation,
    pub program: u32,
    pub serial: u32,
    pub kind: u32,
}
#[derive(Clone, Default)]
pub(super) struct Attrib {
    pub buffer: u32,
    pub size: i32,
    pub kind: u32,
    pub stride: i32,
    pub offset: i32,
    pub enabled: bool,
    pub normalized: bool,
    pub divisor: u32,
}
pub(super) struct VertexArray {
    pub handle: Option<gl::VertexArray>,
    pub bound: bool,
    // The currently bound object's state lives in Context; switching swaps
    // these vectors rather than copying every attribute on every draw.
    pub attribs: Vec<Attrib>,
    pub element_buffer: u32,
}
pub(super) struct Texture {
    pub handle: gl::Texture,
    pub target: u32,
    pub images: HashMap<(u32, i32), TexImage>,
    pub deleted: bool,
    pub min_filter: u32,
    pub wrap_s: u32,
    pub wrap_t: u32,
}
#[derive(Clone, Copy)]
pub(super) struct TexImage {
    pub width: i32,
    pub height: i32,
    pub format: u32,
    pub kind: u32,
}
impl TexImage {
    // Account conservatively for driver formats padded to RGBA8.
    pub fn bytes(self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}
pub(super) struct Renderbuffer {
    pub deleted: bool,
    pub handle: gl::Renderbuffer,
    pub width: i32,
    pub height: i32,
    pub format: u32,
}
impl Renderbuffer {
    pub fn bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}
pub(super) struct Framebuffer {
    pub handle: gl::Framebuffer,
    pub attachments: HashMap<u32, Attachment>,
}
#[derive(Clone, Copy)]
pub(super) struct Attachment {
    pub id: u32,
    pub texture: bool,
    pub target: u32,
    pub level: i32,
}

pub(crate) struct Context {
    pub(super) driver: Driver,
    pub attrs: Attributes,
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    pub dirty: bool,
    pub lost: bool,
    pub(super) errors: VecDeque<u32>,
    pub(super) next: u32,
    pub(super) default_fb: gl::Framebuffer,
    color: gl::Renderbuffer,
    depth: Option<gl::Renderbuffer>,
    pub(super) buffers: HashMap<u32, Buffer>,
    pub(super) shaders: HashMap<u32, Shader>,
    pub(super) programs: HashMap<u32, Program>,
    pub(super) uniforms: HashMap<u32, Uniform>,
    pub(super) textures: HashMap<u32, Texture>,
    pub(super) renderbuffers: HashMap<u32, Renderbuffer>,
    pub(super) framebuffers: HashMap<u32, Framebuffer>,
    pub(super) attribs: Vec<Attrib>,
    pub(super) current_attribs: Vec<[f32; 4]>,
    pub(super) vertex_arrays: HashMap<u32, VertexArray>,
    pub(super) vertex_array: u32,
    pub(super) vertex_array_extension: bool,
    pub(super) retired_buffers: Vec<u32>,
    pub(super) array_buffer: u32,
    pub(super) element_buffer: u32,
    pub(super) program: u32,
    pub(super) framebuffer: u32,
    pub(super) renderbuffer: u32,
    pub(super) active_texture: usize,
    pub(super) texture_units: Vec<[u32; 2]>,
    pub(super) pack: i32,
    pub(super) unpack: i32,
    pub(super) flip: bool,
    pub(super) premultiply: bool,
    pub(super) colorspace: u32,
    pub(super) derivatives: bool,
    pub(super) uint_indices: bool,
    pub(super) instancing: bool,
    pub(super) resources: usize,
    pub(crate) budget: usize,
    pub(super) validation_serial: u64,
}

impl Context {
    pub fn new(
        width: u32,
        height: u32,
        attrs: Attributes,
        generation: u64,
        budget: usize,
    ) -> Result<Self, String> {
        let driver = Driver::new()?;
        // SAFETY: Driver owns the current context on this thread. All following
        // handles are created by it and never passed to another context.
        unsafe {
            let g = &driver.gl;
            if attrs.fail_caveat {
                let renderer = g.get_parameter_string(gl::RENDERER).to_ascii_lowercase();
                if ["llvmpipe", "softpipe", "software"]
                    .iter()
                    .any(|s| renderer.contains(s))
                {
                    return Err("Software renderer is a major performance caveat".into());
                }
            }
            let default_fb = g.create_framebuffer()?;
            let color = g.create_renderbuffer()?;
            let depth = if attrs.depth || attrs.stencil {
                Some(g.create_renderbuffer()?)
            } else {
                None
            };
            let attribs = vec![
                Attrib {
                    size: 4,
                    kind: gl::FLOAT,
                    ..Default::default()
                };
                g.get_parameter_i32(gl::MAX_VERTEX_ATTRIBS).clamp(8, 32) as usize
            ];
            let units = g
                .get_parameter_i32(gl::MAX_COMBINED_TEXTURE_IMAGE_UNITS)
                .clamp(8, 96) as usize;
            let mut out = Self {
                driver,
                attrs,
                width: 0,
                height: 0,
                generation,
                dirty: false,
                lost: false,
                errors: VecDeque::new(),
                next: 1,
                default_fb,
                color,
                depth,
                buffers: HashMap::new(),
                shaders: HashMap::new(),
                programs: HashMap::new(),
                uniforms: HashMap::new(),
                textures: HashMap::new(),
                renderbuffers: HashMap::new(),
                framebuffers: HashMap::new(),
                current_attribs: vec![[0., 0., 0., 1.]; attribs.len()],
                attribs,
                vertex_arrays: HashMap::from([(
                    0,
                    VertexArray {
                        handle: None,
                        bound: true,
                        attribs: vec![],
                        element_buffer: 0,
                    },
                )]),
                vertex_array: 0,
                vertex_array_extension: false,
                retired_buffers: vec![],
                array_buffer: 0,
                element_buffer: 0,
                program: 0,
                framebuffer: 0,
                renderbuffer: 0,
                active_texture: 0,
                texture_units: vec![[0; 2]; units],
                pack: 4,
                unpack: 4,
                flip: false,
                premultiply: false,
                colorspace: 0x9244,
                derivatives: false,
                uint_indices: false,
                instancing: false,
                resources: 0,
                budget,
                validation_serial: 0,
            };
            out.resize(width, height)?;
            out.driver
                .gl
                .viewport(0, 0, out.width as i32, out.height as i32);
            out.driver
                .gl
                .scissor(0, 0, out.width as i32, out.height as i32);
            Ok(out)
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        self.driver.make_current()?;
        unsafe {
            let g = &self.driver.gl;
            let max = g
                .get_parameter_i32(gl::MAX_RENDERBUFFER_SIZE)
                .clamp(1, 4096) as u32;
            if width > max || height > max {
                return Err("Drawing buffer exceeds the implementation allocation limit".into());
            }
            let old = self.width as usize
                * self.height as usize
                * (if self.depth.is_some() { 8 } else { 4 });
            let new = width.max(1) as usize
                * height.max(1) as usize
                * (if self.depth.is_some() { 8 } else { 4 });
            if self.resources - old + new > self.budget {
                return Err("Drawing buffer resource budget exceeded".into());
            }
            self.resources = self.resources - old + new;
            self.width = width.max(1);
            self.height = height.max(1);
            g.bind_framebuffer(gl::FRAMEBUFFER, Some(self.default_fb));
            g.bind_renderbuffer(gl::RENDERBUFFER, Some(self.color));
            g.renderbuffer_storage(
                gl::RENDERBUFFER,
                if self.attrs.alpha {
                    gl::RGBA8
                } else {
                    gl::RGB8
                },
                self.width as i32,
                self.height as i32,
            );
            g.framebuffer_renderbuffer(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::RENDERBUFFER,
                Some(self.color),
            );
            if let Some(depth) = self.depth {
                g.bind_renderbuffer(gl::RENDERBUFFER, Some(depth));
                let format = if self.attrs.stencil {
                    if self.attrs.depth {
                        gl::DEPTH24_STENCIL8
                    } else {
                        gl::STENCIL_INDEX8
                    }
                } else {
                    gl::DEPTH_COMPONENT16
                };
                g.renderbuffer_storage(
                    gl::RENDERBUFFER,
                    format,
                    self.width as i32,
                    self.height as i32,
                );
                if self.attrs.depth {
                    g.framebuffer_renderbuffer(
                        gl::FRAMEBUFFER,
                        gl::DEPTH_ATTACHMENT,
                        gl::RENDERBUFFER,
                        Some(depth),
                    );
                }
                if self.attrs.stencil {
                    g.framebuffer_renderbuffer(
                        gl::FRAMEBUFFER,
                        gl::STENCIL_ATTACHMENT,
                        gl::RENDERBUFFER,
                        Some(depth),
                    );
                }
            }
            let status = g.check_framebuffer_status(gl::FRAMEBUFFER);
            if status != gl::FRAMEBUFFER_COMPLETE || g.get_error() != gl::NO_ERROR {
                return Err(format!("Drawing buffer allocation failed ({status:#x})"));
            }
            self.clear_default();
            self.driver
                .gl
                .bind_framebuffer(gl::FRAMEBUFFER, Some(self.bound_framebuffer()));
            self.driver.gl.bind_renderbuffer(
                gl::RENDERBUFFER,
                self.renderbuffers.get(&self.renderbuffer).map(|r| r.handle),
            );
        }
        self.dirty = true;
        Ok(())
    }

    pub(super) fn clear_default(&self) {
        self.clear_storage(!self.attrs.alpha);
    }

    pub(super) fn clear_storage(&self, opaque: bool) {
        unsafe {
            let g = &self.driver.gl;
            let scissor = g.is_enabled(gl::SCISSOR_TEST);
            let mut color = [0.; 4];
            g.get_parameter_f32_slice(gl::COLOR_CLEAR_VALUE, &mut color);
            let depth = g.get_parameter_f32(gl::DEPTH_CLEAR_VALUE);
            let stencil = g.get_parameter_i32(gl::STENCIL_CLEAR_VALUE);
            let mask = g.get_parameter_bool_array::<4>(gl::COLOR_WRITEMASK);
            let depth_mask = g.get_parameter_bool(gl::DEPTH_WRITEMASK);
            let stencil_mask = g.get_parameter_i32(gl::STENCIL_WRITEMASK) as u32;
            let back_mask = g.get_parameter_i32(gl::STENCIL_BACK_WRITEMASK) as u32;
            g.disable(gl::SCISSOR_TEST);
            g.color_mask(true, true, true, true);
            g.depth_mask(true);
            g.stencil_mask(u32::MAX);
            g.clear_color(0., 0., 0., if opaque { 1. } else { 0. });
            g.clear_depth_f32(1.);
            g.clear_stencil(0);
            g.clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT);
            g.clear_color(color[0], color[1], color[2], color[3]);
            g.clear_depth_f32(depth);
            g.clear_stencil(stencil);
            g.color_mask(mask[0], mask[1], mask[2], mask[3]);
            g.depth_mask(depth_mask);
            g.stencil_mask_separate(gl::FRONT, stencil_mask);
            g.stencil_mask_separate(gl::BACK, back_mask);
            if scissor {
                g.enable(gl::SCISSOR_TEST);
            }
        }
    }
    pub(super) fn bound_framebuffer(&self) -> gl::Framebuffer {
        self.framebuffers
            .get(&self.framebuffer)
            .map_or(self.default_fb, |f| f.handle)
    }
    pub(super) fn id(&mut self) -> Option<u32> {
        if self.buffers.len()
            + self.shaders.len()
            + self.programs.len()
            + self.textures.len()
            + self.framebuffers.len()
            + self.renderbuffers.len()
            + self.uniforms.len()
            + self.vertex_arrays.len()
            > MAX_OBJECTS
        {
            self.error(gl::OUT_OF_MEMORY);
            return None;
        }
        let id = self.next;
        self.next = self.next.checked_add(1)?;
        Some(id)
    }
    pub(super) fn error(&mut self, error: u32) -> Reply {
        self.validation_serial = self.validation_serial.wrapping_add(1);
        if !self.errors.contains(&error) {
            self.errors.push_back(error);
        }
        Reply::Null
    }
    pub fn lose(&mut self) {
        if !self.lost {
            self.lost = true;
            self.errors.clear();
            self.errors.push_back(0x9242);
        }
    }
    pub fn allocated_bytes(&self) -> usize {
        self.resources
    }
    pub fn array_buffer_binding(&self) -> u32 {
        self.array_buffer
    }
    pub fn retained_bytes(&self) -> usize {
        self.resources
            + self
                .buffers
                .values()
                .map(|b| b.bytes.capacity())
                .sum::<usize>()
            + self
                .shaders
                .values()
                .map(|s| s.source.capacity() + s.log.capacity())
                .sum::<usize>()
    }

    /// Resolve only on canvas consumption or presentation, never after every
    /// draw. GPU rows are bottom-up; the DOM bitmap is top-down premultiplied RGBA.
    pub fn snapshot(&mut self, present: bool) -> Option<Vec<u8>> {
        if self.lost || self.driver.make_current().is_err() {
            return None;
        }
        let mut pixels = vec![0; self.width as usize * self.height as usize * 4];
        unsafe {
            let g = &self.driver.gl;
            g.bind_framebuffer(gl::FRAMEBUFFER, Some(self.default_fb));
            g.pixel_store_i32(gl::PACK_ALIGNMENT, 1);
            g.read_pixels(
                0,
                0,
                self.width as i32,
                self.height as i32,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                gl::PixelPackData::Slice(Some(&mut pixels)),
            );
            g.pixel_store_i32(gl::PACK_ALIGNMENT, self.pack);
            if present && !self.attrs.preserve {
                self.clear_default();
            }
            g.bind_framebuffer(gl::FRAMEBUFFER, Some(self.bound_framebuffer()));
        }
        let stride = self.width as usize * 4;
        for y in 0..self.height as usize / 2 {
            let opposite = (self.height as usize - 1 - y) * stride;
            let (a, b) = pixels.split_at_mut(opposite);
            a[y * stride..(y + 1) * stride].swap_with_slice(&mut b[..stride]);
        }
        for p in pixels.as_chunks_mut::<4>().0 {
            if !self.attrs.alpha {
                p[3] = 255;
            }
            if !self.attrs.premultiplied {
                for c in 0..3 {
                    p[c] = ((p[c] as u16 * p[3] as u16 + 127) / 255) as u8;
                }
            } else {
                for c in 0..3 {
                    p[c] = p[c].min(p[3]);
                }
            }
        }
        if present {
            self.dirty = false;
        }
        Some(pixels)
    }

    pub fn execute(&mut self, op: &str, n: &[f64], bytes: Option<&[u8]>, text: &str) -> Reply {
        if op == "getError" {
            return self.errors.pop_front().unwrap_or(gl::NO_ERROR).into();
        }
        if op == "isContextLost" {
            return Reply::Bool(self.lost);
        }
        if self.lost {
            return Reply::Null;
        }
        if self.driver.make_current().is_err() {
            self.lose();
            return Reply::Null;
        }
        let value = self.dispatch(op, n, bytes, text);
        if !self.retired_buffers.is_empty()
            && matches!(
                op,
                "deleteBuffer" | "bindBuffer" | "vertexAttribPointer" | "deleteVertexArrayOES"
            )
        {
            self.collect_buffers();
        }
        if matches!(
            op,
            "deleteShader" | "deleteProgram" | "detachShader" | "useProgram"
        ) {
            self.programs
                .retain(|id, p| !p.deleted || *id == self.program);
            self.shaders.retain(|id, s| {
                let keep = !s.deleted || self.programs.values().any(|p| p.attached.contains(id));
                if !keep {
                    self.resources -= s.source.len();
                }
                keep
            });
        }
        if matches!(
            op,
            "deleteTexture"
                | "deleteRenderbuffer"
                | "deleteFramebuffer"
                | "framebufferTexture2D"
                | "framebufferRenderbuffer"
        ) {
            self.textures.retain(|id, t| {
                let keep = !t.deleted
                    || self
                        .framebuffers
                        .values()
                        .any(|f| f.attachments.values().any(|a| a.texture && a.id == *id));
                if !keep {
                    self.resources -= t.images.values().map(|i| i.bytes()).sum::<usize>();
                }
                keep
            });
            self.renderbuffers.retain(|id, r| {
                let keep = !r.deleted
                    || self
                        .framebuffers
                        .values()
                        .any(|f| f.attachments.values().any(|a| !a.texture && a.id == *id));
                if !keep {
                    self.resources -= r.bytes();
                }
                keep
            });
        }
        // Preserve native and browser validation errors in the same set.
        unsafe {
            for _ in 0..8 {
                let e = self.driver.gl.get_error();
                if e == gl::NO_ERROR {
                    break;
                }
                if e == gl::CONTEXT_LOST {
                    self.lose();
                } else {
                    self.error(e);
                }
            }
        }
        value
    }
    fn dispatch(&mut self, op: &str, n: &[f64], bytes: Option<&[u8]>, text: &str) -> Reply {
        if let Some(result) = self.vertex_array_call(op, n.first().copied().unwrap_or(0.) as u32) {
            return result;
        }
        if let Some(result) = self.object_call(op, n, bytes, text) {
            return result;
        }
        if let Some(result) = self.texture_call(op, n, bytes) {
            return result;
        }
        let a = |i: usize| n.get(i).copied().unwrap_or(0.);
        let u = |i| a(i) as u32;
        let i = |j| a(j) as i32;
        let f = |j| a(j) as f32;
        unsafe {
            let g = &self.driver.gl;
            match op {
                "getParameter" => return self.parameter(u(0)),
                "clearColor" => g.clear_color(f(0), f(1), f(2), f(3)),
                "clearDepth" => g.clear_depth_f32(f(0)),
                "clearStencil" => g.clear_stencil(i(0)),
                "colorMask" => g.color_mask(a(0) != 0., a(1) != 0., a(2) != 0., a(3) != 0.),
                "depthMask" => g.depth_mask(a(0) != 0.),
                "depthFunc" => g.depth_func(u(0)),
                "depthRange" => {
                    if a(0) > a(1) {
                        return self.error(gl::INVALID_OPERATION);
                    }
                    g.depth_range_f32(f(0), f(1));
                }
                "viewport" => {
                    if i(2) < 0 || i(3) < 0 {
                        return self.error(gl::INVALID_VALUE);
                    }
                    g.viewport(i(0), i(1), i(2), i(3));
                }
                "scissor" => {
                    if i(2) < 0 || i(3) < 0 {
                        return self.error(gl::INVALID_VALUE);
                    }
                    g.scissor(i(0), i(1), i(2), i(3));
                }
                "enable" | "disable" | "isEnabled" => {
                    if !matches!(
                        u(0),
                        gl::BLEND
                            | gl::CULL_FACE
                            | gl::DEPTH_TEST
                            | gl::DITHER
                            | gl::POLYGON_OFFSET_FILL
                            | gl::SAMPLE_ALPHA_TO_COVERAGE
                            | gl::SAMPLE_COVERAGE
                            | gl::SCISSOR_TEST
                            | gl::STENCIL_TEST
                    ) {
                        return self.error(gl::INVALID_ENUM);
                    }
                    if op == "isEnabled" {
                        return Reply::Bool(g.is_enabled(u(0)));
                    }
                    if op == "enable" {
                        g.enable(u(0));
                    } else {
                        g.disable(u(0));
                    }
                }
                "blendColor" => g.blend_color(f(0), f(1), f(2), f(3)),
                "blendEquation" | "blendEquationSeparate" => {
                    let valid = |v| {
                        matches!(
                            v,
                            gl::FUNC_ADD | gl::FUNC_SUBTRACT | gl::FUNC_REVERSE_SUBTRACT
                        )
                    };
                    if !valid(u(0)) || (op == "blendEquationSeparate" && !valid(u(1))) {
                        return self.error(gl::INVALID_ENUM);
                    }
                    if op == "blendEquation" {
                        g.blend_equation(u(0));
                    } else {
                        g.blend_equation_separate(u(0), u(1));
                    }
                }
                "blendFunc" | "blendFuncSeparate" => {
                    let valid = |v, source| {
                        matches!(
                            v,
                            gl::ZERO
                                | gl::ONE
                                | gl::SRC_COLOR
                                | gl::ONE_MINUS_SRC_COLOR
                                | gl::DST_COLOR
                                | gl::ONE_MINUS_DST_COLOR
                                | gl::SRC_ALPHA
                                | gl::ONE_MINUS_SRC_ALPHA
                                | gl::DST_ALPHA
                                | gl::ONE_MINUS_DST_ALPHA
                                | gl::CONSTANT_COLOR
                                | gl::ONE_MINUS_CONSTANT_COLOR
                                | gl::CONSTANT_ALPHA
                                | gl::ONE_MINUS_CONSTANT_ALPHA
                        ) || (source && v == gl::SRC_ALPHA_SATURATE)
                    };
                    if !valid(u(0), true)
                        || !valid(u(1), false)
                        || (op == "blendFuncSeparate"
                            && (!valid(u(2), true) || !valid(u(3), false)))
                    {
                        return self.error(gl::INVALID_ENUM);
                    }
                    let pair = |x| matches!(x, gl::CONSTANT_COLOR | gl::ONE_MINUS_CONSTANT_COLOR);
                    let alpha = |x| matches!(x, gl::CONSTANT_ALPHA | gl::ONE_MINUS_CONSTANT_ALPHA);
                    if (pair(u(0)) && alpha(u(1))) || (alpha(u(0)) && pair(u(1))) {
                        return self.error(gl::INVALID_OPERATION);
                    }
                    if op == "blendFunc" {
                        g.blend_func(u(0), u(1));
                    } else {
                        g.blend_func_separate(u(0), u(1), u(2), u(3));
                    }
                }
                "cullFace" => g.cull_face(u(0)),
                "frontFace" => g.front_face(u(0)),
                "lineWidth" => {
                    if f(0).is_nan() {
                        return self.error(gl::INVALID_VALUE);
                    }
                    g.line_width(f(0));
                }
                "polygonOffset" => g.polygon_offset(f(0), f(1)),
                "sampleCoverage" => g.sample_coverage(f(0), a(1) != 0.),
                "stencilFunc" => g.stencil_func(u(0), i(1), u(2)),
                "stencilFuncSeparate" => g.stencil_func_separate(u(0), u(1), i(2), u(3)),
                "stencilMask" => g.stencil_mask(u(0)),
                "stencilMaskSeparate" => g.stencil_mask_separate(u(0), u(1)),
                "stencilOp" => g.stencil_op(u(0), u(1), u(2)),
                "stencilOpSeparate" => g.stencil_op_separate(u(0), u(1), u(2), u(3)),
                "hint" => {
                    if u(0) != gl::GENERATE_MIPMAP_HINT
                        && !(self.derivatives && u(0) == gl::FRAGMENT_SHADER_DERIVATIVE_HINT)
                    {
                        return self.error(gl::INVALID_ENUM);
                    }
                    g.hint(u(0), u(1));
                }
                "flush" => g.flush(),
                "finish" => g.finish(),
                "clear" => {
                    if u(0)
                        & !(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT)
                        != 0
                    {
                        return self.error(gl::INVALID_VALUE);
                    }
                    if !self.framebuffer_complete() {
                        return self.error(gl::INVALID_FRAMEBUFFER_OPERATION);
                    }
                    self.driver.gl.clear(u(0));
                    if self.framebuffer == 0 {
                        self.dirty = true;
                    }
                }
                "drawArrays" | "drawElements" => return self.draw(op, n),
                "error" => return self.error(u(0)),
                "getSupportedExtensions" => {
                    return Reply::Array(
                        self.extensions()
                            .into_iter()
                            .map(|s| Reply::Text(s.into()))
                            .collect(),
                    );
                }
                "extension" => {
                    if !self.extensions().contains(&text) {
                        return Reply::Bool(false);
                    }
                    match text {
                        "OES_standard_derivatives" => self.derivatives = true,
                        "OES_element_index_uint" => self.uint_indices = true,
                        "ANGLE_instanced_arrays" => self.instancing = true,
                        "OES_vertex_array_object" => self.vertex_array_extension = true,
                        _ => {}
                    }
                    return Reply::Bool(true);
                }
                "drawArraysInstancedANGLE" | "drawElementsInstancedANGLE" => {
                    if !self.instancing {
                        return self.error(gl::INVALID_OPERATION);
                    }
                    return self.draw(op, n);
                }
                "vertexAttribDivisorANGLE" => {
                    if !self.instancing {
                        return self.error(gl::INVALID_OPERATION);
                    }
                    let Some(attr) = self.attribs.get_mut(u(0) as usize) else {
                        return self.error(gl::INVALID_VALUE);
                    };
                    attr.divisor = u(1);
                    self.driver.gl.vertex_attrib_divisor(u(0), u(1));
                }
                _ => return self.error(gl::INVALID_OPERATION),
            }
        }
        Reply::Null
    }
    fn extensions(&self) -> Vec<&'static str> {
        let g = &self.driver.gl;
        let es3 = g.version().major >= 3;
        let mut names = vec!["WEBGL_debug_renderer_info", "WEBGL_lose_context"];
        if es3
            || g.supported_extensions()
                .contains("GL_OES_standard_derivatives")
        {
            names.push("OES_standard_derivatives");
        }
        if es3
            || g.supported_extensions()
                .contains("GL_OES_element_index_uint")
        {
            names.push("OES_element_index_uint");
        }
        // The standardized extension name does not imply the ANGLE library.
        if es3 {
            names.push("ANGLE_instanced_arrays");
            names.push("OES_vertex_array_object");
        }
        names
    }
    fn parameter(&mut self, p: u32) -> Reply {
        unsafe {
            let g = &self.driver.gl;
            match p {
                gl::VENDOR => Reply::Text("TRust".into()),
                gl::RENDERER => Reply::Text("WebGL".into()),
                gl::VERSION => Reply::Text("WebGL 1.0 (OpenGL ES 2.0)".into()),
                gl::SHADING_LANGUAGE_VERSION => {
                    Reply::Text("WebGL GLSL ES 1.0 (GLSL ES 1.00)".into())
                }
                0x9245 => Reply::Text(g.get_parameter_string(gl::VENDOR)),
                0x9246 => Reply::Text(g.get_parameter_string(gl::RENDERER)),
                gl::ARRAY_BUFFER_BINDING => self.array_buffer.into(),
                gl::ELEMENT_ARRAY_BUFFER_BINDING => self.element_buffer.into(),
                gl::VERTEX_ARRAY_BINDING if self.vertex_array_extension => self.vertex_array.into(),
                gl::CURRENT_PROGRAM => self.program.into(),
                gl::FRAMEBUFFER_BINDING => self.framebuffer.into(),
                gl::RENDERBUFFER_BINDING => self.renderbuffer.into(),
                gl::TEXTURE_BINDING_2D => self.texture_units[self.active_texture][0].into(),
                gl::TEXTURE_BINDING_CUBE_MAP => self.texture_units[self.active_texture][1].into(),
                0x9240 => Reply::Bool(self.flip),
                0x9241 => Reply::Bool(self.premultiply),
                0x9243 => self.colorspace.into(),
                gl::FRAGMENT_SHADER_DERIVATIVE_HINT if self.derivatives => {
                    g.get_parameter_i32(p).into()
                }
                gl::IMPLEMENTATION_COLOR_READ_FORMAT => gl::RGBA.into(),
                gl::IMPLEMENTATION_COLOR_READ_TYPE => gl::UNSIGNED_BYTE.into(),
                gl::COMPRESSED_TEXTURE_FORMATS => Reply::Array(vec![]),
                gl::MAX_VERTEX_ATTRIBS => (self.attribs.len() as u32).into(),
                gl::MAX_COMBINED_TEXTURE_IMAGE_UNITS => (self.texture_units.len() as u32).into(),
                gl::BLEND
                | gl::CULL_FACE
                | gl::DEPTH_TEST
                | gl::DEPTH_WRITEMASK
                | gl::DITHER
                | gl::POLYGON_OFFSET_FILL
                | gl::SAMPLE_ALPHA_TO_COVERAGE
                | gl::SAMPLE_COVERAGE
                | gl::SAMPLE_COVERAGE_INVERT
                | gl::SCISSOR_TEST
                | gl::STENCIL_TEST => Reply::Bool(g.get_parameter_bool(p)),
                gl::COLOR_WRITEMASK => Reply::Array(
                    g.get_parameter_bool_array::<4>(p)
                        .into_iter()
                        .map(Reply::Bool)
                        .collect(),
                ),
                gl::VIEWPORT | gl::SCISSOR_BOX | gl::MAX_VIEWPORT_DIMS => {
                    let mut v = vec![0; if p == gl::MAX_VIEWPORT_DIMS { 2 } else { 4 }];
                    g.get_parameter_i32_slice(p, &mut v);
                    Reply::numbers(v.into_iter().map(f64::from))
                }
                gl::ALIASED_LINE_WIDTH_RANGE
                | gl::ALIASED_POINT_SIZE_RANGE
                | gl::DEPTH_RANGE
                | gl::BLEND_COLOR
                | gl::COLOR_CLEAR_VALUE => {
                    let mut v = vec![
                        0.;
                        if matches!(p, gl::BLEND_COLOR | gl::COLOR_CLEAR_VALUE) {
                            4
                        } else {
                            2
                        }
                    ];
                    g.get_parameter_f32_slice(p, &mut v);
                    Reply::numbers(v.into_iter().map(f64::from))
                }
                gl::DEPTH_CLEAR_VALUE
                | gl::LINE_WIDTH
                | gl::POLYGON_OFFSET_FACTOR
                | gl::POLYGON_OFFSET_UNITS
                | gl::SAMPLE_COVERAGE_VALUE => Reply::Number(g.get_parameter_f32(p) as f64),
                gl::ACTIVE_TEXTURE
                | gl::ALPHA_BITS
                | gl::BLUE_BITS
                | gl::GREEN_BITS
                | gl::RED_BITS
                | gl::DEPTH_BITS
                | gl::STENCIL_BITS
                | gl::SUBPIXEL_BITS
                | gl::BLEND_DST_ALPHA
                | gl::BLEND_DST_RGB
                | gl::BLEND_EQUATION_ALPHA
                | gl::BLEND_EQUATION_RGB
                | gl::BLEND_SRC_ALPHA
                | gl::BLEND_SRC_RGB
                | gl::CULL_FACE_MODE
                | gl::DEPTH_FUNC
                | gl::FRONT_FACE
                | gl::GENERATE_MIPMAP_HINT
                | gl::MAX_CUBE_MAP_TEXTURE_SIZE
                | gl::MAX_FRAGMENT_UNIFORM_VECTORS
                | gl::MAX_RENDERBUFFER_SIZE
                | gl::MAX_TEXTURE_IMAGE_UNITS
                | gl::MAX_TEXTURE_SIZE
                | gl::MAX_VARYING_VECTORS
                | gl::MAX_VERTEX_TEXTURE_IMAGE_UNITS
                | gl::MAX_VERTEX_UNIFORM_VECTORS
                | gl::PACK_ALIGNMENT
                | gl::UNPACK_ALIGNMENT
                | gl::SAMPLE_BUFFERS
                | gl::SAMPLES
                | gl::STENCIL_BACK_FAIL
                | gl::STENCIL_BACK_FUNC
                | gl::STENCIL_BACK_PASS_DEPTH_FAIL
                | gl::STENCIL_BACK_PASS_DEPTH_PASS
                | gl::STENCIL_BACK_REF
                | gl::STENCIL_BACK_VALUE_MASK
                | gl::STENCIL_BACK_WRITEMASK
                | gl::STENCIL_CLEAR_VALUE
                | gl::STENCIL_FAIL
                | gl::STENCIL_FUNC
                | gl::STENCIL_PASS_DEPTH_FAIL
                | gl::STENCIL_PASS_DEPTH_PASS
                | gl::STENCIL_REF
                | gl::STENCIL_VALUE_MASK
                | gl::STENCIL_WRITEMASK => g.get_parameter_i32(p).into(),
                _ => self.error(gl::INVALID_ENUM),
            }
        }
    }
}
