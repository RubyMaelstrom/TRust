use super::{context::*, shader};
use glow::{self as gl, HasContext};

impl Context {
    pub(super) fn object_call(
        &mut self,
        op: &str,
        n: &[f64],
        bytes: Option<&[u8]>,
        text: &str,
    ) -> Option<Reply> {
        let a = |i: usize| n.get(i).copied().unwrap_or(0.);
        let u = |i| a(i) as u32;
        let i = |j| a(j) as i32;
        let result = unsafe {
            match op {
                "createBuffer" => {
                    let Some(id) = self.id() else {
                        return Some(Reply::Null);
                    };
                    match self.driver.gl.create_buffer() {
                        Ok(handle) => {
                            self.buffers.insert(
                                id,
                                Buffer {
                                    handle,
                                    target: 0,
                                    usage: gl::STATIC_DRAW,
                                    bytes: vec![],
                                },
                            );
                            id.into()
                        }
                        Err(_) => self.error(gl::OUT_OF_MEMORY),
                    }
                }
                "bindBuffer" => {
                    if !matches!(u(0), gl::ARRAY_BUFFER | gl::ELEMENT_ARRAY_BUFFER) {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    let handle = if u(1) == 0 {
                        None
                    } else {
                        let Some(b) = self.buffers.get_mut(&u(1)) else {
                            return Some(self.error(gl::INVALID_OPERATION));
                        };
                        if b.target != 0 && b.target != u(0) {
                            return Some(self.error(gl::INVALID_OPERATION));
                        }
                        b.target = u(0);
                        Some(b.handle)
                    };
                    self.driver.gl.bind_buffer(u(0), handle);
                    if u(0) == gl::ARRAY_BUFFER {
                        self.array_buffer = u(1);
                    } else {
                        self.element_buffer = u(1);
                    }
                    Reply::Null
                }
                "bufferData" | "bufferSubData" => {
                    if !matches!(u(0), gl::ARRAY_BUFFER | gl::ELEMENT_ARRAY_BUFFER) {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    let id = if u(0) == gl::ARRAY_BUFFER {
                        self.array_buffer
                    } else {
                        self.element_buffer
                    };
                    let Some(b) = self.buffers.get_mut(&id) else {
                        return Some(self.error(gl::INVALID_OPERATION));
                    };
                    if op == "bufferData" {
                        if !matches!(u(2), gl::STATIC_DRAW | gl::DYNAMIC_DRAW | gl::STREAM_DRAW) {
                            return Some(self.error(gl::INVALID_ENUM));
                        }
                        let size = bytes.map_or(a(1) as usize, <[u8]>::len);
                        if a(1) < 0. {
                            return Some(self.error(gl::INVALID_VALUE));
                        }
                        if size > MAX_BYTES || self.resources - b.bytes.len() + size > self.budget {
                            return Some(self.error(gl::OUT_OF_MEMORY));
                        }
                        let data = bytes.map_or_else(|| vec![0; size], <[u8]>::to_vec);
                        self.driver.gl.buffer_data_u8_slice(u(0), &data, u(2));
                        let error = self.driver.gl.get_error();
                        if error != gl::NO_ERROR {
                            return Some(self.error(error));
                        }
                        self.resources = self.resources - b.bytes.len() + size;
                        b.bytes = data;
                        b.usage = u(2);
                    } else {
                        let Some(bytes) = bytes else {
                            return Some(self.error(gl::INVALID_VALUE));
                        };
                        let offset = a(1) as usize;
                        if a(1) < 0.
                            || offset
                                .checked_add(bytes.len())
                                .is_none_or(|end| end > b.bytes.len())
                        {
                            return Some(self.error(gl::INVALID_VALUE));
                        }
                        self.driver
                            .gl
                            .buffer_sub_data_u8_slice(u(0), offset as i32, bytes);
                        b.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
                    }
                    Reply::Null
                }
                "getBufferParameter" => {
                    let id = match u(0) {
                        gl::ARRAY_BUFFER => self.array_buffer,
                        gl::ELEMENT_ARRAY_BUFFER => self.element_buffer,
                        _ => return Some(self.error(gl::INVALID_ENUM)),
                    };
                    if let Some(b) = self.buffers.get(&id) {
                        match u(1) {
                            gl::BUFFER_SIZE => (b.bytes.len() as u32).into(),
                            gl::BUFFER_USAGE => b.usage.into(),
                            _ => self.error(gl::INVALID_ENUM),
                        }
                    } else {
                        self.error(gl::INVALID_OPERATION)
                    }
                }
                "deleteBuffer" => {
                    if let Some(b) = self.buffers.remove(&u(0)) {
                        self.driver.gl.delete_buffer(b.handle);
                        self.resources -= b.bytes.len();
                        if self.array_buffer == u(0) {
                            self.array_buffer = 0;
                        }
                        if self.element_buffer == u(0) {
                            self.element_buffer = 0;
                        }
                        for a in &mut self.attribs {
                            if a.buffer == u(0) {
                                a.buffer = 0;
                            }
                        }
                    }
                    Reply::Null
                }
                "isBuffer" => Reply::Bool(self.buffers.get(&u(0)).is_some_and(|b| b.target != 0)),
                "createShader" => {
                    if !matches!(u(0), gl::VERTEX_SHADER | gl::FRAGMENT_SHADER) {
                        return Some(self.error(gl::INVALID_ENUM));
                    }
                    let Some(id) = self.id() else {
                        return Some(Reply::Null);
                    };
                    match self.driver.gl.create_shader(u(0)) {
                        Ok(handle) => {
                            self.shaders.insert(
                                id,
                                Shader {
                                    handle,
                                    kind: u(0),
                                    source: String::new(),
                                    log: String::new(),
                                    compiled: false,
                                    deleted: false,
                                },
                            );
                            id.into()
                        }
                        Err(_) => self.error(gl::OUT_OF_MEMORY),
                    }
                }
                "shaderSource" => {
                    if let Some(s) = self.shaders.get_mut(&u(0)) {
                        if text.len() > 1024 * 1024
                            || self.resources - s.source.len() + text.len() > self.budget
                        {
                            return Some(self.error(gl::OUT_OF_MEMORY));
                        }
                        self.resources = self.resources - s.source.len() + text.len();
                        s.source = text.into();
                        Reply::Null
                    } else {
                        self.error(gl::INVALID_VALUE)
                    }
                }
                "compileShader" => {
                    let Some(s) = self.shaders.get_mut(&u(0)) else {
                        return Some(self.error(gl::INVALID_VALUE));
                    };
                    match shader::prepare(
                        &s.source,
                        s.kind == gl::VERTEX_SHADER,
                        self.derivatives,
                        self.driver
                            .gl
                            .get_shader_precision_format(gl::FRAGMENT_SHADER, gl::HIGH_FLOAT)
                            .is_some_and(|p| p.precision > 0),
                    ) {
                        Ok(prepared) => {
                            self.driver.gl.shader_source(s.handle, &prepared);
                            self.driver.gl.compile_shader(s.handle);
                            s.compiled = self.driver.gl.get_shader_compile_status(s.handle);
                            s.log = self.driver.gl.get_shader_info_log(s.handle);
                        }
                        Err(e) => {
                            s.compiled = false;
                            s.log = e;
                        }
                    }
                    Reply::Null
                }
                "getShaderSource" | "getShaderInfoLog" | "getShaderParameter" => {
                    let Some(s) = self.shaders.get(&u(0)) else {
                        return Some(self.error(gl::INVALID_VALUE));
                    };
                    match op {
                        "getShaderSource" => Reply::Text(s.source.clone()),
                        "getShaderInfoLog" => Reply::Text(s.log.clone()),
                        _ => match u(1) {
                            gl::COMPILE_STATUS => Reply::Bool(s.compiled),
                            gl::DELETE_STATUS => Reply::Bool(s.deleted),
                            gl::SHADER_TYPE => s.kind.into(),
                            _ => self.error(gl::INVALID_ENUM),
                        },
                    }
                }
                "deleteShader" => {
                    if let Some(s) = self.shaders.get_mut(&u(0))
                        && !s.deleted
                    {
                        self.driver.gl.delete_shader(s.handle);
                        s.deleted = true;
                    }
                    Reply::Null
                }
                "isShader" => Reply::Bool(self.shaders.contains_key(&u(0))),
                "getShaderPrecisionFormat" => {
                    if !matches!(u(0), gl::VERTEX_SHADER | gl::FRAGMENT_SHADER)
                        || !matches!(
                            u(1),
                            gl::LOW_FLOAT
                                | gl::MEDIUM_FLOAT
                                | gl::HIGH_FLOAT
                                | gl::LOW_INT
                                | gl::MEDIUM_INT
                                | gl::HIGH_INT
                        )
                    {
                        self.error(gl::INVALID_ENUM)
                    } else {
                        self.driver
                            .gl
                            .get_shader_precision_format(u(0), u(1))
                            .map_or(Reply::Null, |p| {
                                Reply::numbers([
                                    p.range_min as f64,
                                    p.range_max as f64,
                                    p.precision as f64,
                                ])
                            })
                    }
                }
                "createProgram" => {
                    let Some(id) = self.id() else {
                        return Some(Reply::Null);
                    };
                    match self.driver.gl.create_program() {
                        Ok(handle) => {
                            self.programs.insert(
                                id,
                                Program {
                                    handle,
                                    attached: vec![],
                                    linked: false,
                                    deleted: false,
                                    serial: 0,
                                    active: vec![],
                                    samplers: vec![],
                                },
                            );
                            id.into()
                        }
                        Err(_) => self.error(gl::OUT_OF_MEMORY),
                    }
                }
                "attachShader" | "detachShader" => {
                    let (Some(p), Some(s)) =
                        (self.programs.get_mut(&u(0)), self.shaders.get(&u(1)))
                    else {
                        return Some(self.error(gl::INVALID_VALUE));
                    };
                    if op == "attachShader" {
                        if p.attached.contains(&u(1)) {
                            return Some(self.error(gl::INVALID_OPERATION));
                        }
                        self.driver.gl.attach_shader(p.handle, s.handle);
                        let error = self.driver.gl.get_error();
                        if error != gl::NO_ERROR {
                            return Some(self.error(error));
                        }
                        p.attached.push(u(1));
                    } else {
                        if !p.attached.contains(&u(1)) {
                            return Some(self.error(gl::INVALID_OPERATION));
                        }
                        self.driver.gl.detach_shader(p.handle, s.handle);
                        p.attached.retain(|id| *id != u(1));
                    }
                    Reply::Null
                }
                "linkProgram" => {
                    let Some(p) = self.programs.get_mut(&u(0)) else {
                        return Some(self.error(gl::INVALID_VALUE));
                    };
                    p.serial = p.serial.wrapping_add(1);
                    p.active.clear();
                    p.samplers.clear();
                    p.linked = p
                        .attached
                        .iter()
                        .all(|s| self.shaders.get(s).is_some_and(|s| s.compiled));
                    if p.linked {
                        self.driver.gl.link_program(p.handle);
                        p.linked = self.driver.gl.get_program_link_status(p.handle);
                    }
                    if p.linked {
                        for index in 0..self.driver.gl.get_active_attributes(p.handle) {
                            if let Some(a) = self.driver.gl.get_active_attribute(p.handle, index)
                                && let Some(location) =
                                    self.driver.gl.get_attrib_location(p.handle, &a.name)
                            {
                                let count = match a.atype {
                                    gl::FLOAT_MAT2 => 2,
                                    gl::FLOAT_MAT3 => 3,
                                    gl::FLOAT_MAT4 => 4,
                                    _ => 1,
                                };
                                p.active.extend(location..location + count);
                            }
                        }
                    }
                    if p.linked {
                        for index in 0..self.driver.gl.get_active_uniforms(p.handle) {
                            if let Some(info) = self.driver.gl.get_active_uniform(p.handle, index)
                                && matches!(info.utype, gl::SAMPLER_2D | gl::SAMPLER_CUBE)
                            {
                                for i in 0..info.size {
                                    let name = if info.size == 1 {
                                        info.name.clone()
                                    } else {
                                        info.name.replacen("[0]", &format!("[{i}]"), 1)
                                    };
                                    if let Some(loc) =
                                        self.driver.gl.get_uniform_location(p.handle, &name)
                                    {
                                        p.samplers.push((loc, info.utype));
                                    }
                                }
                            }
                        }
                    }
                    Reply::Null
                }
                "useProgram" => {
                    let handle = if u(0) == 0 {
                        None
                    } else {
                        let Some(p) = self.programs.get(&u(0)) else {
                            return Some(self.error(gl::INVALID_VALUE));
                        };
                        if !p.linked || p.deleted {
                            return Some(self.error(gl::INVALID_OPERATION));
                        }
                        Some(p.handle)
                    };
                    self.driver.gl.use_program(handle);
                    self.program = u(0);
                    Reply::Null
                }
                "validateProgram" => {
                    if let Some(p) = self.programs.get(&u(0)) {
                        self.driver.gl.validate_program(p.handle);
                        Reply::Null
                    } else {
                        self.error(gl::INVALID_VALUE)
                    }
                }
                "deleteProgram" => {
                    if let Some(p) = self.programs.get_mut(&u(0))
                        && !p.deleted
                    {
                        self.driver.gl.delete_program(p.handle);
                        p.deleted = true;
                    }
                    Reply::Null
                }
                "isProgram" => Reply::Bool(self.programs.contains_key(&u(0))),
                "deleteUniformLocation" => {
                    self.uniforms.remove(&u(0));
                    Reply::Null
                }
                "getAttachedShaders" => self.programs.get(&u(0)).map_or(Reply::Null, |p| {
                    Reply::numbers(p.attached.iter().map(|x| *x as f64))
                }),
                "getProgramInfoLog" => self.programs.get(&u(0)).map_or(Reply::Null, |p| {
                    Reply::Text(
                        if p.attached
                            .iter()
                            .any(|s| self.shaders.get(s).is_some_and(|s| !s.compiled))
                        {
                            "Attached shader failed WebGL validation".into()
                        } else {
                            self.driver.gl.get_program_info_log(p.handle)
                        },
                    )
                }),
                "getProgramParameter" => {
                    let Some(p) = self.programs.get(&u(0)) else {
                        return Some(self.error(gl::INVALID_VALUE));
                    };
                    match u(1) {
                        gl::LINK_STATUS => Reply::Bool(p.linked),
                        gl::DELETE_STATUS => Reply::Bool(p.deleted),
                        gl::VALIDATE_STATUS => {
                            Reply::Bool(self.driver.gl.get_program_validate_status(p.handle))
                        }
                        gl::ATTACHED_SHADERS => (p.attached.len() as u32).into(),
                        gl::ACTIVE_ATTRIBUTES => {
                            self.driver.gl.get_active_attributes(p.handle).into()
                        }
                        gl::ACTIVE_UNIFORMS => self.driver.gl.get_active_uniforms(p.handle).into(),
                        _ => self.error(gl::INVALID_ENUM),
                    }
                }
                "bindAttribLocation" | "getAttribLocation" | "getUniformLocation" => {
                    if text.len() > 256 || !text.is_ascii() || text.contains('\0') {
                        return Some(self.error(gl::INVALID_VALUE));
                    }
                    if text.starts_with("webgl_") || text.starts_with("_webgl_") {
                        return Some(if op == "bindAttribLocation" {
                            self.error(gl::INVALID_OPERATION)
                        } else if op == "getAttribLocation" {
                            (-1).into()
                        } else {
                            Reply::Null
                        });
                    }
                    let Some(p) = self.programs.get(&u(0)) else {
                        return Some(self.error(gl::INVALID_VALUE));
                    };
                    if op == "bindAttribLocation" {
                        if u(1) as usize >= self.attribs.len() {
                            return Some(self.error(gl::INVALID_VALUE));
                        }
                        self.driver.gl.bind_attrib_location(p.handle, u(1), text);
                        Reply::Null
                    } else if !p.linked {
                        self.error(gl::INVALID_OPERATION)
                    } else if op == "getAttribLocation" {
                        self.driver
                            .gl
                            .get_attrib_location(p.handle, text)
                            .map_or((-1).into(), Reply::from)
                    } else if let Some(location) =
                        self.driver.gl.get_uniform_location(p.handle, text)
                    {
                        let mut kind = None;
                        for index in 0..self.driver.gl.get_active_uniforms(p.handle) {
                            if let Some(info) = self.driver.gl.get_active_uniform(p.handle, index) {
                                let exact = text == info.name
                                    || text == info.name.strip_suffix("[0]").unwrap_or(&info.name);
                                let element = info
                                    .name
                                    .rsplit_once("[0]")
                                    .and_then(|(prefix, suffix)| {
                                        text.strip_prefix(prefix)?
                                            .strip_suffix(suffix)?
                                            .strip_prefix('[')?
                                            .strip_suffix(']')?
                                            .parse::<i32>()
                                            .ok()
                                    })
                                    .is_some_and(|i| i >= 0 && i < info.size);
                                if exact || element {
                                    kind = Some(info.utype);
                                    break;
                                }
                            }
                        }
                        let Some(kind) = kind else {
                            return Some(self.error(gl::INVALID_OPERATION));
                        };
                        let uniform = Uniform {
                            location,
                            program: u(0),
                            serial: p.serial,
                            kind,
                        };
                        let Some(id) = self.id() else {
                            return Some(Reply::Null);
                        };
                        self.uniforms.insert(id, uniform);
                        id.into()
                    } else {
                        Reply::Null
                    }
                }
                "getActiveAttrib" | "getActiveUniform" => {
                    let Some(p) = self.programs.get(&u(0)) else {
                        return Some(self.error(gl::INVALID_VALUE));
                    };
                    if op == "getActiveAttrib" {
                        self.driver
                            .gl
                            .get_active_attribute(p.handle, u(1))
                            .map(|a| {
                                Reply::Array(vec![
                                    a.size.into(),
                                    a.atype.into(),
                                    Reply::Text(a.name),
                                ])
                            })
                    } else {
                        self.driver.gl.get_active_uniform(p.handle, u(1)).map(|a| {
                            Reply::Array(vec![a.size.into(), a.utype.into(), Reply::Text(a.name)])
                        })
                    }
                    .unwrap_or_else(|| self.error(gl::INVALID_VALUE))
                }
                "enableVertexAttribArray"
                | "disableVertexAttribArray"
                | "vertexAttribPointer"
                | "vertexAttrib"
                | "getVertexAttrib"
                | "getVertexAttribOffset" => {
                    let Some(attr) = self.attribs.get_mut(u(0) as usize) else {
                        return Some(self.error(gl::INVALID_VALUE));
                    };
                    match op {
                        "enableVertexAttribArray" => {
                            attr.enabled = true;
                            self.driver.gl.enable_vertex_attrib_array(u(0));
                            Reply::Null
                        }
                        "disableVertexAttribArray" => {
                            attr.enabled = false;
                            self.driver.gl.disable_vertex_attrib_array(u(0));
                            Reply::Null
                        }
                        "vertexAttrib" => {
                            attr.current = [a(1) as f32, a(2) as f32, a(3) as f32, a(4) as f32];
                            self.driver
                                .gl
                                .vertex_attrib_4_f32_slice(u(0), &attr.current);
                            Reply::Null
                        }
                        "vertexAttribPointer" => {
                            if !(1..=4).contains(&i(1))
                                || i(4) < 0
                                || i(4) > 255
                                || a(5) < 0.
                                || a(5) > i32::MAX as f64
                            {
                                return Some(self.error(gl::INVALID_VALUE));
                            }
                            let width = match u(2) {
                                gl::BYTE | gl::UNSIGNED_BYTE => 1,
                                gl::SHORT | gl::UNSIGNED_SHORT => 2,
                                gl::FLOAT => 4,
                                _ => return Some(self.error(gl::INVALID_ENUM)),
                            };
                            if i(4) % width != 0
                                || i(5) % width != 0
                                || (self.array_buffer == 0 && i(5) != 0)
                            {
                                return Some(self.error(gl::INVALID_OPERATION));
                            }
                            attr.buffer = self.array_buffer;
                            attr.size = i(1);
                            attr.kind = u(2);
                            attr.normalized = a(3) != 0.;
                            attr.stride = i(4);
                            attr.offset = i(5);
                            self.driver.gl.vertex_attrib_pointer_f32(
                                u(0),
                                i(1),
                                u(2),
                                a(3) != 0.,
                                i(4),
                                i(5),
                            );
                            Reply::Null
                        }
                        "getVertexAttribOffset" => {
                            if u(1) == gl::VERTEX_ATTRIB_ARRAY_POINTER {
                                attr.offset.into()
                            } else {
                                self.error(gl::INVALID_ENUM)
                            }
                        }
                        _ => match u(1) {
                            gl::VERTEX_ATTRIB_ARRAY_BUFFER_BINDING => attr.buffer.into(),
                            gl::VERTEX_ATTRIB_ARRAY_ENABLED => Reply::Bool(attr.enabled),
                            gl::VERTEX_ATTRIB_ARRAY_NORMALIZED => Reply::Bool(attr.normalized),
                            gl::VERTEX_ATTRIB_ARRAY_SIZE => attr.size.into(),
                            gl::VERTEX_ATTRIB_ARRAY_TYPE => attr.kind.into(),
                            gl::VERTEX_ATTRIB_ARRAY_STRIDE => attr.stride.into(),
                            gl::VERTEX_ATTRIB_ARRAY_DIVISOR if self.instancing => {
                                attr.divisor.into()
                            }
                            gl::CURRENT_VERTEX_ATTRIB => {
                                Reply::numbers(attr.current.map(f64::from))
                            }
                            _ => self.error(gl::INVALID_ENUM),
                        },
                    }
                }
                "uniform" => return Some(self.uniform(n)),
                "uniformType" => self
                    .uniforms
                    .get(&u(0))
                    .map_or(Reply::Null, |l| l.kind.into()),
                "getUniform" => {
                    let (Some(p), Some(loc)) = (self.programs.get(&u(0)), self.uniforms.get(&u(1)))
                    else {
                        return Some(self.error(gl::INVALID_OPERATION));
                    };
                    if loc.program != u(0) || loc.serial != p.serial {
                        return Some(self.error(gl::INVALID_OPERATION));
                    }
                    let (width, int) = uniform_shape(loc.kind);
                    if int {
                        let mut values = [0; 16];
                        self.driver
                            .gl
                            .get_uniform_i32(p.handle, &loc.location, &mut values);
                        Reply::numbers(values[..width].iter().copied().map(f64::from))
                    } else {
                        let mut values = [0.; 16];
                        self.driver
                            .gl
                            .get_uniform_f32(p.handle, &loc.location, &mut values);
                        Reply::numbers(values[..width].iter().copied().map(f64::from))
                    }
                }
                _ => return None,
            }
        };
        Some(result)
    }

    fn uniform(&mut self, n: &[f64]) -> Reply {
        let (Some(&id), Some(&width), Some(&integer), Some(&matrix)) =
            (n.first(), n.get(1), n.get(2), n.get(3))
        else {
            return self.error(gl::INVALID_VALUE);
        };
        if id == 0. {
            return Reply::Null;
        }
        let Some(loc) = self.uniforms.get(&(id as u32)) else {
            return self.error(gl::INVALID_OPERATION);
        };
        if self.program != loc.program
            || self
                .programs
                .get(&loc.program)
                .is_none_or(|p| p.serial != loc.serial || !p.linked)
        {
            return self.error(gl::INVALID_OPERATION);
        }
        let values = &n[4..];
        let width = width as usize;
        let shape = uniform_shape(loc.kind);
        if values.is_empty() || !values.len().is_multiple_of(width) {
            return self.error(gl::INVALID_VALUE);
        }
        if (matrix != 0.) != matches!(loc.kind, gl::FLOAT_MAT2 | gl::FLOAT_MAT3 | gl::FLOAT_MAT4) {
            return self.error(gl::INVALID_OPERATION);
        }
        if shape != (width, integer != 0.)
            && !matches!(
                (loc.kind, width, integer != 0.),
                (gl::BOOL, 1, false)
                    | (gl::BOOL_VEC2, 2, false)
                    | (gl::BOOL_VEC3, 3, false)
                    | (gl::BOOL_VEC4, 4, false)
            )
        {
            return self.error(gl::INVALID_OPERATION);
        }
        if matches!(loc.kind, gl::SAMPLER_2D | gl::SAMPLER_CUBE)
            && values
                .iter()
                .any(|x| *x < 0. || *x >= self.texture_units.len() as f64)
        {
            return self.error(gl::INVALID_VALUE);
        }
        unsafe {
            let g = &self.driver.gl;
            let l = Some(&loc.location);
            if integer != 0. {
                let v: Vec<i32> = values.iter().map(|x| *x as i32).collect();
                match width {
                    1 => g.uniform_1_i32_slice(l, &v),
                    2 => g.uniform_2_i32_slice(l, &v),
                    3 => g.uniform_3_i32_slice(l, &v),
                    4 => g.uniform_4_i32_slice(l, &v),
                    _ => return self.error(gl::INVALID_OPERATION),
                }
            } else {
                let v: Vec<f32> = values.iter().map(|x| *x as f32).collect();
                if matrix != 0. {
                    match width {
                        4 => g.uniform_matrix_2_f32_slice(l, false, &v),
                        9 => g.uniform_matrix_3_f32_slice(l, false, &v),
                        16 => g.uniform_matrix_4_f32_slice(l, false, &v),
                        _ => return self.error(gl::INVALID_OPERATION),
                    }
                } else {
                    match width {
                        1 => g.uniform_1_f32_slice(l, &v),
                        2 => g.uniform_2_f32_slice(l, &v),
                        3 => g.uniform_3_f32_slice(l, &v),
                        4 => g.uniform_4_f32_slice(l, &v),
                        _ => return self.error(gl::INVALID_OPERATION),
                    }
                }
            }
        }
        Reply::Null
    }
    pub(super) fn draw(&mut self, op: &str, n: &[f64]) -> Reply {
        let a = |i: usize| n.get(i).copied().unwrap_or(0.);
        let mode = a(0) as u32;
        let arrays = op.starts_with("drawArrays");
        let instanced = op.ends_with("InstancedANGLE");
        let instances = if instanced {
            a(if arrays { 3 } else { 4 }) as i32
        } else {
            1
        };
        if instances < 0 {
            return self.error(gl::INVALID_VALUE);
        }
        if mode > gl::TRIANGLE_FAN {
            return self.error(gl::INVALID_ENUM);
        }
        let Some(p) = self.programs.get(&self.program) else {
            return self.error(gl::INVALID_OPERATION);
        };
        if !p.linked {
            return self.error(gl::INVALID_OPERATION);
        }
        let count = if arrays { a(2) } else { a(1) };
        if count < 0. || a(1) < 0. {
            return self.error(gl::INVALID_VALUE);
        }
        if count == 0. || instances == 0 {
            return Reply::Null;
        }
        let max = if arrays {
            a(1) as usize + count as usize - 1
        } else {
            let kind = a(2) as u32;
            let width = match kind {
                gl::UNSIGNED_BYTE => 1,
                gl::UNSIGNED_SHORT => 2,
                gl::UNSIGNED_INT if self.uint_indices => 4,
                _ => return self.error(gl::INVALID_ENUM),
            };
            if a(3) < 0. {
                return self.error(gl::INVALID_VALUE);
            }
            let offset = a(3) as usize;
            if !offset.is_multiple_of(width) {
                return self.error(gl::INVALID_OPERATION);
            }
            let Some(b) = self.buffers.get(&self.element_buffer) else {
                return self.error(gl::INVALID_OPERATION);
            };
            let Some(end) = offset
                .checked_add(count as usize * width)
                .filter(|end| *end <= b.bytes.len())
            else {
                return self.error(gl::INVALID_OPERATION);
            };
            b.bytes[offset..end]
                .chunks_exact(width)
                .map(|v| match width {
                    1 => v[0] as usize,
                    2 => u16::from_ne_bytes([v[0], v[1]]) as usize,
                    _ => u32::from_ne_bytes([v[0], v[1], v[2], v[3]]) as usize,
                })
                .max()
                .unwrap_or(0)
        };
        if instanced
            && !p.active.iter().any(|i| {
                self.attribs
                    .get(*i as usize)
                    .is_some_and(|a| a.enabled && a.divisor == 0)
            })
        {
            return self.error(gl::INVALID_OPERATION);
        }
        if self.attribs.iter().any(|a| a.enabled && a.buffer == 0) {
            return self.error(gl::INVALID_OPERATION);
        }
        for index in &p.active {
            let Some(a) = self.attribs.get(*index as usize) else {
                return self.error(gl::INVALID_OPERATION);
            };
            if !a.enabled {
                continue;
            }
            let Some(b) = self.buffers.get(&a.buffer) else {
                return self.error(gl::INVALID_OPERATION);
            };
            let width = match a.kind {
                gl::BYTE | gl::UNSIGNED_BYTE => 1,
                gl::SHORT | gl::UNSIGNED_SHORT => 2,
                _ => 4,
            };
            let element = a.size as usize * width;
            let stride = if a.stride == 0 {
                element
            } else {
                a.stride as usize
            };
            let max = if a.divisor == 0 {
                max
            } else {
                (instances as usize - 1) / a.divisor as usize
            };
            if max
                .checked_mul(stride)
                .and_then(|x| x.checked_add(a.offset as usize + element))
                .is_none_or(|end| end > b.bytes.len())
            {
                return self.error(gl::INVALID_OPERATION);
            }
        }
        if !self.framebuffer_complete() {
            return self.error(gl::INVALID_FRAMEBUFFER_OPERATION);
        }
        let mut restore = Vec::new();
        unsafe {
            let g = &self.driver.gl;
            if g.is_enabled(gl::STENCIL_TEST) {
                let bits = g.get_parameter_i32(gl::STENCIL_BITS).clamp(0, 31);
                let mask = ((1u32 << bits) - 1) as i32;
                for (front, back) in [
                    (gl::STENCIL_WRITEMASK, gl::STENCIL_BACK_WRITEMASK),
                    (gl::STENCIL_VALUE_MASK, gl::STENCIL_BACK_VALUE_MASK),
                ] {
                    if g.get_parameter_i32(front) & mask != g.get_parameter_i32(back) & mask {
                        return self.error(gl::INVALID_OPERATION);
                    }
                }
                if g.get_parameter_i32(gl::STENCIL_REF).clamp(0, mask)
                    != g.get_parameter_i32(gl::STENCIL_BACK_REF).clamp(0, mask)
                {
                    return self.error(gl::INVALID_OPERATION);
                }
            }
            let mut units = std::collections::HashMap::new();
            for (loc, kind) in &p.samplers {
                let mut v = [0];
                g.get_uniform_i32(p.handle, loc, &mut v);
                let unit = v[0] as usize;
                if unit >= self.texture_units.len()
                    || units.insert(unit, *kind).is_some_and(|k| k != *kind)
                {
                    return self.error(gl::INVALID_OPERATION);
                }
                let slot = usize::from(*kind == gl::SAMPLER_CUBE);
                let id = self.texture_units[unit][slot];
                if id != 0
                    && self
                        .framebuffers
                        .get(&self.framebuffer)
                        .is_some_and(|f| f.attachments.values().any(|a| a.texture && a.id == id))
                {
                    return self.error(gl::INVALID_OPERATION);
                }
                if let Some(t) = self.textures.get(&id)
                    && !t.complete()
                {
                    restore.push((unit, t.target, t.handle));
                }
            }
            for (unit, target, _) in &restore {
                g.active_texture(gl::TEXTURE0 + *unit as u32);
                g.bind_texture(*target, None);
            }
            if instanced {
                if arrays {
                    g.draw_arrays_instanced(mode, a(1) as i32, count as i32, instances);
                } else {
                    g.draw_elements_instanced(
                        mode,
                        count as i32,
                        a(2) as u32,
                        a(3) as i32,
                        instances,
                    );
                }
            } else if arrays {
                g.draw_arrays(mode, a(1) as i32, count as i32);
            } else {
                g.draw_elements(mode, count as i32, a(2) as u32, a(3) as i32);
            }
            for (unit, target, handle) in restore {
                g.active_texture(gl::TEXTURE0 + unit as u32);
                g.bind_texture(target, Some(handle));
            }
            g.active_texture(gl::TEXTURE0 + self.active_texture as u32);
        }
        if self.framebuffer == 0 {
            self.dirty = true;
        }
        Reply::Null
    }
}
pub(super) fn uniform_shape(kind: u32) -> (usize, bool) {
    match kind {
        gl::FLOAT => (1, false),
        gl::FLOAT_VEC2 => (2, false),
        gl::FLOAT_VEC3 => (3, false),
        gl::FLOAT_VEC4 | gl::FLOAT_MAT2 => (4, false),
        gl::FLOAT_MAT3 => (9, false),
        gl::FLOAT_MAT4 => (16, false),
        gl::INT_VEC2 | gl::BOOL_VEC2 => (2, true),
        gl::INT_VEC3 | gl::BOOL_VEC3 => (3, true),
        gl::INT_VEC4 | gl::BOOL_VEC4 => (4, true),
        _ => (1, true),
    }
}
