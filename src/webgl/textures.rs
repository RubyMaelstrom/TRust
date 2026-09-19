use super::context::*;
use glow::{self as gl, HasContext};
use std::collections::HashMap;

fn texture_target(target: u32) -> Option<(u32, usize)> {
    match target {
        gl::TEXTURE_2D => Some((gl::TEXTURE_2D, 0)),
        gl::TEXTURE_CUBE_MAP
        | gl::TEXTURE_CUBE_MAP_POSITIVE_X..=gl::TEXTURE_CUBE_MAP_NEGATIVE_Z => {
            Some((gl::TEXTURE_CUBE_MAP, 1))
        }
        _ => None,
    }
}
fn pixel_size(format: u32, kind: u32) -> Option<usize> {
    match (format, kind) {
        (gl::ALPHA | gl::LUMINANCE, gl::UNSIGNED_BYTE) => Some(1),
        (gl::LUMINANCE_ALPHA, gl::UNSIGNED_BYTE) => Some(2),
        (gl::RGB, gl::UNSIGNED_BYTE) => Some(3),
        (gl::RGBA, gl::UNSIGNED_BYTE) => Some(4),
        (gl::RGB, gl::UNSIGNED_SHORT_5_6_5)
        | (gl::RGBA, gl::UNSIGNED_SHORT_4_4_4_4 | gl::UNSIGNED_SHORT_5_5_5_1) => Some(2),
        _ => None,
    }
}
fn pixel_length(w: i32, h: i32, size: usize, alignment: i32) -> Option<(usize, usize)> {
    if w < 0 || h < 0 {
        return None;
    }
    let row = (w as usize).checked_mul(size)?;
    let stride = row.div_ceil(alignment as usize) * alignment as usize;
    let length = if h == 0 {
        0
    } else {
        stride.checked_mul(h as usize - 1)?.checked_add(row)?
    };
    (length <= MAX_BYTES).then_some((length, stride))
}

impl Texture {
    fn targets(&self) -> std::ops::RangeInclusive<u32> {
        if self.target == gl::TEXTURE_2D {
            gl::TEXTURE_2D..=gl::TEXTURE_2D
        } else {
            gl::TEXTURE_CUBE_MAP_POSITIVE_X..=gl::TEXTURE_CUBE_MAP_NEGATIVE_Z
        }
    }
    fn base(&self) -> Option<TexImage> {
        let base = *self.images.get(&(*self.targets().start(), 0))?;
        if base.width == 0
            || base.height == 0
            || self.target == gl::TEXTURE_CUBE_MAP && base.width != base.height
        {
            return None;
        }
        self.targets()
            .all(|t| {
                self.images.get(&(t, 0)).is_some_and(|i| {
                    i.width == base.width
                        && i.height == base.height
                        && i.format == base.format
                        && i.kind == base.kind
                })
            })
            .then_some(base)
    }
    pub(super) fn complete(&self) -> bool {
        let Some(base) = self.base() else {
            return false;
        };
        let mip = !matches!(self.min_filter, gl::NEAREST | gl::LINEAR);
        let pot = (base.width as u32).is_power_of_two() && (base.height as u32).is_power_of_two();
        if !pot && (mip || self.wrap_s != gl::CLAMP_TO_EDGE || self.wrap_t != gl::CLAMP_TO_EDGE) {
            return false;
        }
        if mip {
            for level in 1..=(base.width.max(base.height) as u32).ilog2() as i32 {
                if !self.targets().all(|t| {
                    self.images.get(&(t, level)).is_some_and(|i| {
                        i.width == (base.width >> level).max(1)
                            && i.height == (base.height >> level).max(1)
                            && i.format == base.format
                            && i.kind == base.kind
                    })
                }) {
                    return false;
                }
            }
        }
        true
    }
}

impl Context {
    pub(super) fn texture_call(
        &mut self,
        op: &str,
        n: &[f64],
        bytes: Option<&[u8]>,
    ) -> Option<Reply> {
        let a = |i: usize| n.get(i).copied().unwrap_or(0.);
        let u = |i| a(i) as u32;
        let i = |j| a(j) as i32;
        let result = unsafe {
            match op {
                "createTexture" => {
                    let Some(id) = self.id() else {
                        return Some(Reply::Null);
                    };
                    match self.driver.gl.create_texture() {
                        Ok(handle) => {
                            self.textures.insert(
                                id,
                                Texture {
                                    handle,
                                    target: 0,
                                    images: HashMap::new(),
                                    deleted: false,
                                    min_filter: gl::NEAREST_MIPMAP_LINEAR,
                                    wrap_s: gl::REPEAT,
                                    wrap_t: gl::REPEAT,
                                },
                            );
                            id.into()
                        }
                        Err(_) => self.error(gl::OUT_OF_MEMORY),
                    }
                }
                "activeTexture" => {
                    if u(0) < gl::TEXTURE0 || u(0) - gl::TEXTURE0 >= self.texture_units.len() as u32
                    {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    self.active_texture = (u(0) - gl::TEXTURE0) as usize;
                    self.driver.gl.active_texture(u(0));
                    Reply::Null
                }
                "bindTexture" => {
                    if !matches!(u(0), gl::TEXTURE_2D | gl::TEXTURE_CUBE_MAP) {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    let (_, unit) = texture_target(u(0))?;
                    let handle = if u(1) == 0 {
                        None
                    } else {
                        let Some(t) = self.textures.get_mut(&u(1)) else {
                            return Some(self.error(gl::INVALID_OPERATION));
                        };
                        if t.deleted || t.target != 0 && t.target != u(0) {
                            return Some(self.error(gl::INVALID_OPERATION));
                        }
                        t.target = u(0);
                        Some(t.handle)
                    };
                    self.driver.gl.bind_texture(u(0), handle);
                    self.texture_units[self.active_texture][unit] = u(1);
                    Reply::Null
                }
                "deleteTexture" => {
                    if let Some(t) = self.textures.get_mut(&u(0)).filter(|t| !t.deleted) {
                        t.deleted = true;
                        self.driver.gl.delete_texture(t.handle);
                        for unit in &mut self.texture_units {
                            for id in unit {
                                if *id == u(0) {
                                    *id = 0;
                                }
                            }
                        }
                        if let Some(f) = self.framebuffers.get_mut(&self.framebuffer) {
                            f.attachments.retain(|_, a| !a.texture || a.id != u(0));
                        }
                    }
                    Reply::Null
                }
                "isTexture" => Reply::Bool(
                    self.textures
                        .get(&u(0))
                        .is_some_and(|t| !t.deleted && t.target != 0),
                ),
                "pixelStorei" => {
                    match u(0) {
                        gl::PACK_ALIGNMENT | gl::UNPACK_ALIGNMENT => {
                            if !matches!(i(1), 1 | 2 | 4 | 8) {
                                return Some(self.error(gl::INVALID_VALUE));
                            }
                            self.driver.gl.pixel_store_i32(u(0), i(1));
                            if u(0) == gl::PACK_ALIGNMENT {
                                self.pack = i(1);
                            } else {
                                self.unpack = i(1);
                            }
                        }
                        0x9240 => self.flip = a(1) != 0.,
                        0x9241 => self.premultiply = a(1) != 0.,
                        0x9243 => {
                            if !matches!(u(1), 0 | 0x9244) {
                                return Some(self.error(gl::INVALID_VALUE));
                            }
                            self.colorspace = u(1);
                        }
                        _ => return Some(self.error(gl::INVALID_ENUM)),
                    }
                    Reply::Null
                }
                "texParameteri" | "texParameterf" | "getTexParameter" | "generateMipmap" => {
                    if !matches!(u(0), gl::TEXTURE_2D | gl::TEXTURE_CUBE_MAP) {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    let (_, unit) = texture_target(u(0))?;
                    let id = self.texture_units[self.active_texture][unit];
                    if !self.textures.contains_key(&id) {
                        return Some(self.error(gl::INVALID_OPERATION));
                    }
                    if op == "generateMipmap" {
                        let t = self.textures.get(&id).unwrap();
                        let Some(base) = t.base() else {
                            return Some(self.error(gl::INVALID_OPERATION));
                        };
                        if !(base.width as u32).is_power_of_two()
                            || !(base.height as u32).is_power_of_two()
                        {
                            return Some(self.error(gl::INVALID_OPERATION));
                        }
                        let mut images = t.images.clone();
                        for target in t.targets() {
                            for level in 1..=(base.width.max(base.height) as u32).ilog2() as i32 {
                                images.insert(
                                    (target, level),
                                    TexImage {
                                        width: (base.width >> level).max(1),
                                        height: (base.height >> level).max(1),
                                        ..base
                                    },
                                );
                            }
                        }
                        let bytes = self.resources
                            - t.images.values().map(|i| i.bytes()).sum::<usize>()
                            + images.values().map(|i| i.bytes()).sum::<usize>();
                        if bytes > self.budget {
                            return Some(self.error(gl::OUT_OF_MEMORY));
                        }
                        self.driver.gl.generate_mipmap(u(0));
                        let e = self.driver.gl.get_error();
                        if e != gl::NO_ERROR {
                            return Some(self.error(e));
                        }
                        self.resources = bytes;
                        self.textures.get_mut(&id).unwrap().images = images;
                        Reply::Null
                    } else {
                        if !matches!(
                            u(1),
                            gl::TEXTURE_MIN_FILTER
                                | gl::TEXTURE_MAG_FILTER
                                | gl::TEXTURE_WRAP_S
                                | gl::TEXTURE_WRAP_T
                        ) {
                            return Some(self.error(gl::INVALID_ENUM));
                        }
                        if op == "getTexParameter" {
                            self.driver.gl.get_tex_parameter_i32(u(0), u(1)).into()
                        } else {
                            let valid = match u(1) {
                                gl::TEXTURE_MIN_FILTER => matches!(
                                    u(2),
                                    gl::NEAREST
                                        | gl::LINEAR
                                        | gl::NEAREST_MIPMAP_NEAREST
                                        | gl::NEAREST_MIPMAP_LINEAR
                                        | gl::LINEAR_MIPMAP_NEAREST
                                        | gl::LINEAR_MIPMAP_LINEAR
                                ),
                                gl::TEXTURE_MAG_FILTER => matches!(u(2), gl::NEAREST | gl::LINEAR),
                                _ => matches!(
                                    u(2),
                                    gl::CLAMP_TO_EDGE | gl::REPEAT | gl::MIRRORED_REPEAT
                                ),
                            };
                            if !valid {
                                return Some(self.error(gl::INVALID_ENUM));
                            }
                            self.driver.gl.tex_parameter_i32(u(0), u(1), i(2));
                            let texture = self.textures.get_mut(&id).unwrap();
                            match u(1) {
                                gl::TEXTURE_MIN_FILTER => texture.min_filter = u(2),
                                gl::TEXTURE_WRAP_S => texture.wrap_s = u(2),
                                gl::TEXTURE_WRAP_T => texture.wrap_t = u(2),
                                _ => {}
                            }
                            Reply::Null
                        }
                    }
                }
                "texImage2D" | "texSubImage2D" => return Some(self.texture_image(op, n, bytes)),
                "compressedTexImage2D" | "compressedTexSubImage2D" => self.error(gl::INVALID_ENUM),
                "createFramebuffer" => {
                    let Some(id) = self.id() else {
                        return Some(Reply::Null);
                    };
                    match self.driver.gl.create_framebuffer() {
                        Ok(handle) => {
                            self.framebuffers.insert(
                                id,
                                Framebuffer {
                                    handle,
                                    attachments: HashMap::new(),
                                },
                            );
                            id.into()
                        }
                        Err(_) => self.error(gl::OUT_OF_MEMORY),
                    }
                }
                "bindFramebuffer" => {
                    if u(0) != gl::FRAMEBUFFER {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    if u(1) != 0 && !self.framebuffers.contains_key(&u(1)) {
                        return Some(self.error(gl::INVALID_OPERATION));
                    }
                    self.framebuffer = u(1);
                    self.driver
                        .gl
                        .bind_framebuffer(gl::FRAMEBUFFER, Some(self.bound_framebuffer()));
                    Reply::Null
                }
                "deleteFramebuffer" => {
                    if let Some(f) = self.framebuffers.remove(&u(0)) {
                        if self.framebuffer == u(0) {
                            self.framebuffer = 0;
                            self.driver
                                .gl
                                .bind_framebuffer(gl::FRAMEBUFFER, Some(self.default_fb));
                        }
                        self.driver.gl.delete_framebuffer(f.handle);
                    }
                    Reply::Null
                }
                "isFramebuffer" => Reply::Bool(
                    self.framebuffers
                        .get(&u(0))
                        .is_some_and(|f| self.driver.gl.is_framebuffer(f.handle)),
                ),
                "checkFramebufferStatus" => {
                    if u(0) != gl::FRAMEBUFFER {
                        self.error(gl::INVALID_ENUM);
                        0u32.into()
                    } else {
                        self.framebuffer_status().into()
                    }
                }
                "createRenderbuffer" => {
                    let Some(id) = self.id() else {
                        return Some(Reply::Null);
                    };
                    match self.driver.gl.create_renderbuffer() {
                        Ok(handle) => {
                            self.renderbuffers.insert(
                                id,
                                Renderbuffer {
                                    deleted: false,
                                    handle,
                                    width: 0,
                                    height: 0,
                                    format: gl::RGBA4,
                                },
                            );
                            id.into()
                        }
                        Err(_) => self.error(gl::OUT_OF_MEMORY),
                    }
                }
                "bindRenderbuffer" => {
                    if u(0) != gl::RENDERBUFFER {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    let handle = if u(1) == 0 {
                        None
                    } else {
                        let Some(r) = self.renderbuffers.get(&u(1)).filter(|r| !r.deleted) else {
                            return Some(self.error(gl::INVALID_OPERATION));
                        };
                        Some(r.handle)
                    };
                    self.renderbuffer = u(1);
                    self.driver.gl.bind_renderbuffer(gl::RENDERBUFFER, handle);
                    Reply::Null
                }
                "deleteRenderbuffer" => {
                    if let Some(r) = self.renderbuffers.get_mut(&u(0)).filter(|r| !r.deleted) {
                        r.deleted = true;
                        self.driver.gl.delete_renderbuffer(r.handle);
                        if self.renderbuffer == u(0) {
                            self.renderbuffer = 0;
                        }
                        if let Some(f) = self.framebuffers.get_mut(&self.framebuffer) {
                            f.attachments.retain(|_, a| a.texture || a.id != u(0));
                        }
                    }
                    Reply::Null
                }
                "isRenderbuffer" => Reply::Bool(
                    self.renderbuffers
                        .get(&u(0))
                        .is_some_and(|r| !r.deleted && self.driver.gl.is_renderbuffer(r.handle)),
                ),
                "renderbufferStorage" => {
                    if u(0) != gl::RENDERBUFFER {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    let format = match u(1) {
                        gl::RGBA4
                        | gl::RGB5_A1
                        | gl::RGB565
                        | gl::DEPTH_COMPONENT16
                        | gl::STENCIL_INDEX8 => u(1),
                        gl::DEPTH_STENCIL => gl::DEPTH24_STENCIL8,
                        _ => return Some(self.error(gl::INVALID_ENUM)),
                    };
                    if i(2) < 0 || i(3) < 0 {
                        return Some(self.error(gl::INVALID_VALUE));
                    }
                    if (i(2) as usize)
                        .saturating_mul(i(3) as usize)
                        .saturating_mul(4)
                        > MAX_BYTES
                    {
                        return Some(self.error(gl::OUT_OF_MEMORY));
                    }
                    let Some(r) = self.renderbuffers.get_mut(&self.renderbuffer) else {
                        return Some(self.error(gl::INVALID_OPERATION));
                    };
                    let bytes = i(2) as usize * i(3) as usize * 4;
                    if self.resources - r.bytes() + bytes > self.budget {
                        return Some(self.error(gl::OUT_OF_MEMORY));
                    }
                    self.driver
                        .gl
                        .renderbuffer_storage(gl::RENDERBUFFER, format, i(2), i(3));
                    let error = self.driver.gl.get_error();
                    if error != gl::NO_ERROR {
                        return Some(self.error(error));
                    }
                    self.resources = self.resources - r.bytes() + bytes;
                    r.width = i(2);
                    r.height = i(3);
                    r.format = u(1);
                    let handle = r.handle;
                    // WebGL #RESOURCE_RESTRICTIONS requires zeroed color/stencil
                    // and depth=1 even when the caller supplies no initial data.
                    let f = self.driver.gl.create_framebuffer().ok()?;
                    self.driver.gl.bind_framebuffer(gl::FRAMEBUFFER, Some(f));
                    let attachment = match u(1) {
                        gl::DEPTH_COMPONENT16 => gl::DEPTH_ATTACHMENT,
                        gl::STENCIL_INDEX8 => gl::STENCIL_ATTACHMENT,
                        gl::DEPTH_STENCIL => gl::DEPTH_STENCIL_ATTACHMENT,
                        _ => gl::COLOR_ATTACHMENT0,
                    };
                    self.driver.gl.framebuffer_renderbuffer(
                        gl::FRAMEBUFFER,
                        attachment,
                        gl::RENDERBUFFER,
                        Some(handle),
                    );
                    if i(2) > 0 && i(3) > 0 {
                        self.clear_storage(false);
                    }
                    self.driver
                        .gl
                        .bind_framebuffer(gl::FRAMEBUFFER, Some(self.bound_framebuffer()));
                    self.driver.gl.delete_framebuffer(f);
                    Reply::Null
                }
                "getRenderbufferParameter" => {
                    if u(0) != gl::RENDERBUFFER {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    let Some(r) = self.renderbuffers.get(&self.renderbuffer) else {
                        return Some(self.error(gl::INVALID_OPERATION));
                    };
                    match u(1) {
                        gl::RENDERBUFFER_WIDTH => r.width.into(),
                        gl::RENDERBUFFER_HEIGHT => r.height.into(),
                        gl::RENDERBUFFER_INTERNAL_FORMAT => r.format.into(),
                        gl::RENDERBUFFER_RED_SIZE
                        | gl::RENDERBUFFER_GREEN_SIZE
                        | gl::RENDERBUFFER_BLUE_SIZE
                        | gl::RENDERBUFFER_ALPHA_SIZE
                        | gl::RENDERBUFFER_DEPTH_SIZE
                        | gl::RENDERBUFFER_STENCIL_SIZE => self
                            .driver
                            .gl
                            .get_renderbuffer_parameter_i32(gl::RENDERBUFFER, u(1))
                            .into(),
                        _ => self.error(gl::INVALID_ENUM),
                    }
                }
                "framebufferRenderbuffer" | "framebufferTexture2D" => {
                    if u(0) != gl::FRAMEBUFFER
                        || !matches!(
                            u(1),
                            gl::COLOR_ATTACHMENT0
                                | gl::DEPTH_ATTACHMENT
                                | gl::STENCIL_ATTACHMENT
                                | gl::DEPTH_STENCIL_ATTACHMENT
                        )
                    {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    if self.framebuffer == 0 {
                        return Some(self.error(gl::INVALID_OPERATION));
                    }
                    let attachment = Attachment {
                        id: u(3),
                        texture: op == "framebufferTexture2D",
                        target: u(2),
                        level: i(4),
                    };
                    if attachment.texture {
                        if u(2) != gl::TEXTURE_2D
                            && !(gl::TEXTURE_CUBE_MAP_POSITIVE_X..=gl::TEXTURE_CUBE_MAP_NEGATIVE_Z)
                                .contains(&u(2))
                        {
                            return Some(self.error(gl::INVALID_ENUM));
                        }
                        if i(4) != 0 {
                            return Some(self.error(gl::INVALID_VALUE));
                        }
                        let handle = if u(3) == 0 {
                            None
                        } else {
                            let Some(t) = self.textures.get(&u(3)).filter(|t| !t.deleted) else {
                                return Some(self.error(gl::INVALID_OPERATION));
                            };
                            if texture_target(u(2)).map(|x| x.0) != Some(t.target) {
                                return Some(self.error(gl::INVALID_OPERATION));
                            }
                            Some(t.handle)
                        };
                        self.driver.gl.framebuffer_texture_2d(
                            gl::FRAMEBUFFER,
                            u(1),
                            u(2),
                            handle,
                            i(4),
                        );
                    } else {
                        if u(2) != gl::RENDERBUFFER {
                            return Some(self.error(gl::INVALID_ENUM));
                        }
                        let handle =
                            if u(3) == 0 {
                                None
                            } else {
                                let Some(r) = self.renderbuffers.get(&u(3)).filter(|r| {
                                    !r.deleted && self.driver.gl.is_renderbuffer(r.handle)
                                }) else {
                                    return Some(self.error(gl::INVALID_OPERATION));
                                };
                                Some(r.handle)
                            };
                        self.driver.gl.framebuffer_renderbuffer(
                            gl::FRAMEBUFFER,
                            u(1),
                            gl::RENDERBUFFER,
                            handle,
                        );
                    }
                    let f = self.framebuffers.get_mut(&self.framebuffer).unwrap();
                    if u(3) == 0 {
                        f.attachments.remove(&u(1));
                    } else {
                        f.attachments.insert(u(1), attachment);
                    }
                    Reply::Null
                }
                "getFramebufferAttachmentParameter" => {
                    if u(0) != gl::FRAMEBUFFER
                        || !matches!(
                            u(1),
                            gl::COLOR_ATTACHMENT0
                                | gl::DEPTH_ATTACHMENT
                                | gl::STENCIL_ATTACHMENT
                                | gl::DEPTH_STENCIL_ATTACHMENT
                        )
                    {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    let Some(f) = self.framebuffers.get(&self.framebuffer) else {
                        return Some(self.error(gl::INVALID_OPERATION));
                    };
                    let att = f.attachments.get(&u(1));
                    if att.is_none() && u(2) != gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    match u(2) {
                        gl::FRAMEBUFFER_ATTACHMENT_OBJECT_TYPE => att
                            .map_or(gl::NONE, |a| {
                                if a.texture {
                                    gl::TEXTURE
                                } else {
                                    gl::RENDERBUFFER
                                }
                            })
                            .into(),
                        gl::FRAMEBUFFER_ATTACHMENT_OBJECT_NAME => att.map_or(0, |a| a.id).into(),
                        gl::FRAMEBUFFER_ATTACHMENT_TEXTURE_LEVEL => {
                            if let Some(a) = att.filter(|a| a.texture) {
                                a.level.into()
                            } else {
                                self.error(gl::INVALID_ENUM)
                            }
                        }
                        gl::FRAMEBUFFER_ATTACHMENT_TEXTURE_CUBE_MAP_FACE => {
                            if let Some(a) = att.filter(|a| a.texture) {
                                if a.target == gl::TEXTURE_2D {
                                    0u32.into()
                                } else {
                                    a.target.into()
                                }
                            } else {
                                self.error(gl::INVALID_ENUM)
                            }
                        }
                        _ => self.error(gl::INVALID_ENUM),
                    }
                }
                "readPixels" => return Some(self.read_pixels(n, bytes)),
                "copyTexImage2D" | "copyTexSubImage2D" => return Some(self.copy_texture(op, n)),
                _ => return None,
            }
        };
        Some(result)
    }

    fn texture_image(&mut self, op: &str, n: &[f64], bytes: Option<&[u8]>) -> Reply {
        let a = |i: usize| n.get(i).copied().unwrap_or(0.);
        let u = |i| a(i) as u32;
        let i = |j| a(j) as i32;
        let Some((target, unit)) = texture_target(u(0)) else {
            return self.error(gl::INVALID_ENUM);
        };
        if u(0) == gl::TEXTURE_CUBE_MAP {
            return self.error(gl::INVALID_ENUM);
        }
        let sub = op == "texSubImage2D";
        let (w, h, format, kind) = if sub {
            (i(4), i(5), u(6), u(7))
        } else {
            (i(3), i(4), u(6), u(7))
        };
        if !matches!(
            format,
            gl::ALPHA | gl::LUMINANCE | gl::LUMINANCE_ALPHA | gl::RGB | gl::RGBA
        ) || !matches!(
            kind,
            gl::UNSIGNED_BYTE
                | gl::UNSIGNED_SHORT_5_6_5
                | gl::UNSIGNED_SHORT_4_4_4_4
                | gl::UNSIGNED_SHORT_5_5_5_1
        ) {
            return self.error(gl::INVALID_ENUM);
        }
        let Some(size) = pixel_size(format, kind) else {
            return self.error(gl::INVALID_OPERATION);
        };
        if w < 0 || h < 0 || i(1) < 0 || i(1) > 15 || (!sub && i(5) != 0) {
            return self.error(gl::INVALID_VALUE);
        }
        if !sub && u(2) != format {
            return self.error(gl::INVALID_OPERATION);
        }
        let id = self.texture_units[self.active_texture][unit];
        let Some(t) = self.textures.get(&id) else {
            return self.error(gl::INVALID_OPERATION);
        };
        let max = unsafe {
            self.driver.gl.get_parameter_i32(if unit == 0 {
                gl::MAX_TEXTURE_SIZE
            } else {
                gl::MAX_CUBE_MAP_TEXTURE_SIZE
            })
        };
        if w > max.checked_shr(i(1) as u32).unwrap_or(0)
            || h > max.checked_shr(i(1) as u32).unwrap_or(0)
        {
            return self.error(gl::INVALID_VALUE);
        }
        if !sub
            && i(1) != 0
            && ((!w.is_positive() || !(w as u32).is_power_of_two())
                || (!h.is_positive() || !(h as u32).is_power_of_two()))
        {
            return self.error(gl::INVALID_VALUE);
        }
        let old_bytes = t.images.get(&(u(0), i(1))).map_or(0, |i| i.bytes());
        let new_bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        if !sub && new_bytes > self.budget.saturating_sub(self.resources - old_bytes) {
            return self.error(gl::OUT_OF_MEMORY);
        }
        if sub {
            let Some(img) = t.images.get(&(u(0), i(1))) else {
                return self.error(gl::INVALID_OPERATION);
            };
            if img.format != format || img.kind != kind {
                return self.error(gl::INVALID_OPERATION);
            }
            if i(2) < 0
                || i(3) < 0
                || i(2) as i64 + w as i64 > img.width as i64
                || i(3) as i64 + h as i64 > img.height as i64
            {
                return self.error(gl::INVALID_VALUE);
            }
        } else if target == gl::TEXTURE_CUBE_MAP && w != h {
            return self.error(gl::INVALID_VALUE);
        }
        // DOM uploads are canonical RGBA, identified separately from BufferSource.
        let dom = a(8) != 0.;
        let bitmap = a(8) == 2.;
        let alignment = if dom { 1 } else { self.unpack };
        let Some((length, stride)) = pixel_length(w, h, size, alignment) else {
            return self.error(gl::OUT_OF_MEMORY);
        };
        let mut data = if let Some(bytes) = bytes {
            if dom {
                if bytes.len() < (w as usize * h as usize * 4) {
                    return self.error(gl::INVALID_OPERATION);
                }
                let mut data = vec![0; length];
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        let source_y = if self.flip && !bitmap {
                            h as usize - 1 - y
                        } else {
                            y
                        };
                        let offset = (source_y * w as usize + x) * 4;
                        let mut p: [u8; 4] = bytes[offset..offset + 4].try_into().unwrap();
                        if self.premultiply && !bitmap {
                            for c in 0..3 {
                                p[c] = ((p[c] as u16 * p[3] as u16 + 127) / 255) as u8;
                            }
                        }
                        let offset = y * stride + x * size;
                        match (format, kind) {
                            (gl::RGBA, gl::UNSIGNED_BYTE) => {
                                data[offset..offset + 4].copy_from_slice(&p)
                            }
                            (gl::RGB, gl::UNSIGNED_BYTE) => {
                                data[offset..offset + 3].copy_from_slice(&p[..3])
                            }
                            (gl::ALPHA, _) => data[offset] = p[3],
                            (gl::LUMINANCE, _) => data[offset] = p[0],
                            (gl::LUMINANCE_ALPHA, _) => {
                                data[offset..offset + 2].copy_from_slice(&[p[0], p[3]])
                            }
                            _ => {
                                let value = match kind {
                                    gl::UNSIGNED_SHORT_5_6_5 => {
                                        ((p[0] as u16 >> 3) << 11)
                                            | ((p[1] as u16 >> 2) << 5)
                                            | (p[2] as u16 >> 3)
                                    }
                                    gl::UNSIGNED_SHORT_4_4_4_4 => {
                                        ((p[0] as u16 >> 4) << 12)
                                            | ((p[1] as u16 >> 4) << 8)
                                            | ((p[2] as u16 >> 4) << 4)
                                            | (p[3] as u16 >> 4)
                                    }
                                    _ => {
                                        ((p[0] as u16 >> 3) << 11)
                                            | ((p[1] as u16 >> 3) << 6)
                                            | ((p[2] as u16 >> 3) << 1)
                                            | (p[3] as u16 >> 7)
                                    }
                                };
                                data[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
                            }
                        }
                    }
                }
                data
            } else {
                if bytes.len() < length {
                    return self.error(gl::INVALID_OPERATION);
                }
                bytes[..length].to_vec()
            }
        } else {
            if sub {
                return self.error(gl::INVALID_VALUE);
            }
            vec![0; length]
        };
        // Byte-source uploads also obey flipY and premultiplication in WebGL 1.
        if !dom && self.flip && h > 1 {
            for y in 0..h as usize / 2 {
                let other = (h as usize - 1 - y) * stride;
                let (a, b) = data.split_at_mut(other);
                let row = w as usize * size;
                a[y * stride..y * stride + row].swap_with_slice(&mut b[..row]);
            }
        }
        if !dom && self.premultiply {
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let p = &mut data[y * stride + x * size..y * stride + (x + 1) * size];
                    match (format, kind) {
                        (gl::RGBA, gl::UNSIGNED_BYTE) => {
                            for c in 0..3 {
                                p[c] = ((p[c] as u16 * p[3] as u16 + 127) / 255) as u8;
                            }
                        }
                        (gl::LUMINANCE_ALPHA, gl::UNSIGNED_BYTE) => {
                            p[0] = ((p[0] as u16 * p[1] as u16 + 127) / 255) as u8
                        }
                        (gl::RGBA, gl::UNSIGNED_SHORT_4_4_4_4) => {
                            let v = u16::from_ne_bytes([p[0], p[1]]);
                            let alpha = v & 15;
                            let v = ((((v >> 12) * alpha + 7) / 15) << 12)
                                | (((((v >> 8) & 15) * alpha + 7) / 15) << 8)
                                | (((((v >> 4) & 15) * alpha + 7) / 15) << 4)
                                | alpha;
                            p.copy_from_slice(&v.to_ne_bytes());
                        }
                        (gl::RGBA, gl::UNSIGNED_SHORT_5_5_5_1)
                            if u16::from_ne_bytes([p[0], p[1]]) & 1 == 0 =>
                        {
                            p.fill(0);
                        }
                        _ => {}
                    }
                }
            }
        }
        unsafe {
            let g = &self.driver.gl;
            g.pixel_store_i32(gl::UNPACK_ALIGNMENT, alignment);
            if sub {
                g.tex_sub_image_2d(
                    u(0),
                    i(1),
                    i(2),
                    i(3),
                    w,
                    h,
                    format,
                    kind,
                    gl::PixelUnpackData::Slice(Some(&data)),
                );
            } else {
                g.tex_image_2d(
                    u(0),
                    i(1),
                    format as i32,
                    w,
                    h,
                    0,
                    format,
                    kind,
                    gl::PixelUnpackData::Slice(Some(&data)),
                );
            }
            g.pixel_store_i32(gl::UNPACK_ALIGNMENT, self.unpack);
            let error = g.get_error();
            if error != gl::NO_ERROR {
                return self.error(error);
            }
        }
        if !sub {
            self.resources = self.resources - old_bytes + new_bytes;
            self.textures.get_mut(&id).unwrap().images.insert(
                (u(0), i(1)),
                TexImage {
                    width: w,
                    height: h,
                    format,
                    kind,
                },
            );
        }
        Reply::Null
    }
    pub(super) fn framebuffer_status(&self) -> u32 {
        if let Some(f) = self.framebuffers.get(&self.framebuffer) {
            let a = &f.attachments;
            let mut dimensions = None;
            for (point, a) in &f.attachments {
                let (w, h, format) = if a.texture {
                    let Some(i) = self
                        .textures
                        .get(&a.id)
                        .and_then(|t| t.images.get(&(a.target, a.level)))
                    else {
                        return gl::FRAMEBUFFER_INCOMPLETE_ATTACHMENT;
                    };
                    if *point != gl::COLOR_ATTACHMENT0 || !matches!(i.format, gl::RGB | gl::RGBA) {
                        return gl::FRAMEBUFFER_INCOMPLETE_ATTACHMENT;
                    }
                    (i.width, i.height, i.format)
                } else {
                    let Some(r) = self.renderbuffers.get(&a.id) else {
                        return gl::FRAMEBUFFER_INCOMPLETE_ATTACHMENT;
                    };
                    let valid = match *point {
                        gl::DEPTH_ATTACHMENT => r.format == gl::DEPTH_COMPONENT16,
                        gl::STENCIL_ATTACHMENT => r.format == gl::STENCIL_INDEX8,
                        gl::DEPTH_STENCIL_ATTACHMENT => r.format == gl::DEPTH_STENCIL,
                        _ => matches!(r.format, gl::RGBA4 | gl::RGB5_A1 | gl::RGB565),
                    };
                    if !valid {
                        return gl::FRAMEBUFFER_UNSUPPORTED;
                    }
                    (r.width, r.height, r.format)
                };
                let _ = format;
                if w == 0 || h == 0 {
                    return gl::FRAMEBUFFER_INCOMPLETE_ATTACHMENT;
                }
                if dimensions.is_some_and(|size| size != (w, h)) {
                    return gl::FRAMEBUFFER_INCOMPLETE_DIMENSIONS;
                }
                dimensions = Some((w, h));
            }
            if (a.contains_key(&gl::DEPTH_STENCIL_ATTACHMENT)
                && (a.contains_key(&gl::DEPTH_ATTACHMENT)
                    || a.contains_key(&gl::STENCIL_ATTACHMENT)))
                || (a.contains_key(&gl::DEPTH_ATTACHMENT)
                    && a.contains_key(&gl::STENCIL_ATTACHMENT))
            {
                return gl::FRAMEBUFFER_UNSUPPORTED;
            }
        }
        unsafe { self.driver.gl.check_framebuffer_status(gl::FRAMEBUFFER) }
    }
    pub(super) fn framebuffer_complete(&self) -> bool {
        self.framebuffer_status() == gl::FRAMEBUFFER_COMPLETE
    }
    fn framebuffer_size(&self) -> Option<(i32, i32)> {
        if self.framebuffer == 0 {
            return Some((self.width as i32, self.height as i32));
        }
        let a = self
            .framebuffers
            .get(&self.framebuffer)?
            .attachments
            .get(&gl::COLOR_ATTACHMENT0)?;
        if a.texture {
            self.textures
                .get(&a.id)?
                .images
                .get(&(a.target, a.level))
                .map(|i| (i.width, i.height))
        } else {
            self.renderbuffers.get(&a.id).map(|r| (r.width, r.height))
        }
    }
    fn read_pixels(&mut self, n: &[f64], bytes: Option<&[u8]>) -> Reply {
        if n.len() < 6 {
            return self.error(gl::INVALID_VALUE);
        }
        let (x, y, w, h) = (n[0] as i32, n[1] as i32, n[2] as i32, n[3] as i32);
        if w < 0 || h < 0 {
            return self.error(gl::INVALID_VALUE);
        }
        if n[4] as u32 != gl::RGBA || n[5] as u32 != gl::UNSIGNED_BYTE {
            return self.error(gl::INVALID_OPERATION);
        }
        let Some(bytes) = bytes else {
            return self.error(gl::INVALID_VALUE);
        };
        let Some((length, stride)) = pixel_length(w, h, 4, self.pack) else {
            return self.error(gl::INVALID_OPERATION);
        };
        if bytes.len() < length {
            return self.error(gl::INVALID_OPERATION);
        }
        if !self.framebuffer_complete() {
            return self.error(gl::INVALID_FRAMEBUFFER_OPERATION);
        }
        let Some((fw, fh)) = self.framebuffer_size() else {
            return self.error(gl::INVALID_OPERATION);
        };
        let mut out = bytes.to_vec();
        let left = x.max(0);
        let bottom = y.max(0);
        let right = (x as i64 + w as i64).min(fw as i64);
        let top = (y as i64 + h as i64).min(fh as i64);
        if right > left as i64 && top > bottom as i64 {
            let rw = right as i32 - left;
            let rh = top as i32 - bottom;
            let mut clipped = vec![0; rw as usize * rh as usize * 4];
            unsafe {
                self.driver.gl.pixel_store_i32(gl::PACK_ALIGNMENT, 1);
                self.driver.gl.read_pixels(
                    left,
                    bottom,
                    rw,
                    rh,
                    gl::RGBA,
                    gl::UNSIGNED_BYTE,
                    gl::PixelPackData::Slice(Some(&mut clipped)),
                );
                self.driver
                    .gl
                    .pixel_store_i32(gl::PACK_ALIGNMENT, self.pack);
            }
            for row in 0..rh as usize {
                let offset = (bottom as i64 - y as i64 + row as i64) as usize * stride
                    + (left as i64 - x as i64) as usize * 4;
                out[offset..offset + rw as usize * 4]
                    .copy_from_slice(&clipped[row * rw as usize * 4..(row + 1) * rw as usize * 4]);
            }
        }
        Reply::Bytes(out)
    }
    fn copy_texture(&mut self, op: &str, n: &[f64]) -> Reply {
        let a = |i: usize| n.get(i).copied().unwrap_or(0.) as i32;
        let target = a(0) as u32;
        let level = a(1);
        let sub = op == "copyTexSubImage2D";
        let Some((_, unit)) = texture_target(target) else {
            return self.error(gl::INVALID_ENUM);
        };
        if target == gl::TEXTURE_CUBE_MAP {
            return self.error(gl::INVALID_ENUM);
        }
        let (x, y, w, h) = if sub {
            (a(4), a(5), a(6), a(7))
        } else {
            (a(3), a(4), a(5), a(6))
        };
        if w < 0 || h < 0 || level < 0 || (!sub && a(7) != 0) {
            return self.error(gl::INVALID_VALUE);
        }
        if !self.framebuffer_complete() {
            return self.error(gl::INVALID_FRAMEBUFFER_OPERATION);
        }
        let Some((fw, fh)) = self.framebuffer_size() else {
            return self.error(gl::INVALID_OPERATION);
        };
        let id = self.texture_units[self.active_texture][unit];
        let Some(texture) = self.textures.get(&id) else {
            return self.error(gl::INVALID_OPERATION);
        };
        if self.framebuffers.get(&self.framebuffer).is_some_and(|f| {
            f.attachments
                .values()
                .any(|a| a.texture && a.id == id && a.target == target && a.level == level)
        }) {
            return self.error(gl::INVALID_OPERATION);
        }
        let format = if sub {
            let Some(image) = texture.images.get(&(target, level)) else {
                return self.error(gl::INVALID_OPERATION);
            };
            image.format
        } else {
            a(2) as u32
        };
        if !matches!(
            format,
            gl::ALPHA | gl::LUMINANCE | gl::LUMINANCE_ALPHA | gl::RGB | gl::RGBA
        ) {
            return self.error(gl::INVALID_ENUM);
        }
        // GLES 2 §3.7.2: the source must contain every requested component.
        if matches!(format, gl::ALPHA | gl::LUMINANCE_ALPHA | gl::RGBA)
            && unsafe { self.driver.gl.get_parameter_i32(gl::ALPHA_BITS) } == 0
        {
            return self.error(gl::INVALID_OPERATION);
        }
        let (dx, dy) = if sub {
            let Some(image) = texture.images.get(&(target, level)) else {
                return self.error(gl::INVALID_OPERATION);
            };
            if a(2) < 0
                || a(3) < 0
                || a(2) as i64 + w as i64 > image.width as i64
                || a(3) as i64 + h as i64 > image.height as i64
            {
                return self.error(gl::INVALID_VALUE);
            }
            (a(2), a(3))
        } else {
            let before = self.validation_serial;
            self.texture_image(
                "texImage2D",
                &[
                    target as f64,
                    level as f64,
                    a(2) as f64,
                    w as f64,
                    h as f64,
                    0.,
                    a(2) as f64,
                    gl::UNSIGNED_BYTE as f64,
                    0.,
                ],
                None,
            );
            if self.validation_serial != before {
                return Reply::Null;
            }
            (0, 0)
        };
        // GLES leaves out-of-bounds reads undefined. WebGL preserves these
        // destination pixels (fresh texImage storage was already zeroed).
        let left = x.max(0);
        let bottom = y.max(0);
        let right = (x as i64 + w as i64).min(fw as i64);
        let top = (y as i64 + h as i64).min(fh as i64);
        if right > left as i64 && top > bottom as i64 {
            unsafe {
                self.driver.gl.copy_tex_sub_image_2d(
                    target,
                    level,
                    (dx as i64 + left as i64 - x as i64) as i32,
                    (dy as i64 + bottom as i64 - y as i64) as i32,
                    left,
                    bottom,
                    right as i32 - left,
                    top as i32 - bottom,
                );
            }
        }
        Reply::Null
    }
}
