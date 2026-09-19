//! OES_vertex_array_object, WebGL snapshot 3b7a7538, including the deleted-buffer
//! resolution; GLES OES_vertex_array_object revision 16, §§2.X and 6.1.X.
//! Attribute divisors belong to the VAO (ANGLE_instanced_arrays interaction),
//! while ARRAY_BUFFER_BINDING and CURRENT_VERTEX_ATTRIB belong to the context.
use super::context::*;
use glow::{self as gl, HasContext};

impl Context {
    pub(super) fn vertex_array_call(&mut self, op: &str, id: u32) -> Option<Reply> {
        if !matches!(
            op,
            "createVertexArrayOES"
                | "deleteVertexArrayOES"
                | "bindVertexArrayOES"
                | "isVertexArrayOES"
        ) {
            return None;
        }
        if !self.vertex_array_extension {
            return Some(self.error(gl::INVALID_OPERATION));
        }
        Some(match op {
            "createVertexArrayOES" => {
                let Some(id) = self.id() else {
                    return Some(Reply::Null);
                };
                // Advertised on GLES 3 drivers, where VAOs are core operations.
                match unsafe { self.driver.gl.create_vertex_array() } {
                    Ok(handle) => {
                        self.vertex_arrays.insert(
                            id,
                            VertexArray {
                                handle: Some(handle),
                                bound: false,
                                element_buffer: 0,
                                attribs: vec![
                                    Attrib {
                                        size: 4,
                                        kind: gl::FLOAT,
                                        ..Default::default()
                                    };
                                    self.attribs.len()
                                ],
                            },
                        );
                        id.into()
                    }
                    Err(_) => self.error(gl::OUT_OF_MEMORY),
                }
            }
            "bindVertexArrayOES" => {
                if !self.vertex_arrays.contains_key(&id) {
                    self.error(gl::INVALID_OPERATION)
                } else {
                    self.bind_vertex_array(id);
                    id.into()
                }
            }
            "deleteVertexArrayOES" => {
                if id != 0 {
                    if self.vertex_array == id {
                        self.bind_vertex_array(0);
                    }
                    if let Some(array) = self.vertex_arrays.remove(&id)
                        && let Some(handle) = array.handle
                    {
                        unsafe {
                            self.driver.gl.delete_vertex_array(handle);
                        }
                    }
                }
                Reply::Null
            }
            _ => Reply::Bool(id != 0 && self.vertex_arrays.get(&id).is_some_and(|v| v.bound)),
        })
    }

    fn bind_vertex_array(&mut self, id: u32) {
        if self.vertex_array == id {
            return;
        }
        let old = self.vertex_arrays.get_mut(&self.vertex_array).unwrap();
        std::mem::swap(&mut old.attribs, &mut self.attribs);
        std::mem::swap(&mut old.element_buffer, &mut self.element_buffer);
        let new = self.vertex_arrays.get_mut(&id).unwrap();
        std::mem::swap(&mut new.attribs, &mut self.attribs);
        std::mem::swap(&mut new.element_buffer, &mut self.element_buffer);
        new.bound = true;
        unsafe {
            self.driver.gl.bind_vertex_array(new.handle);
        }
        self.vertex_array = id;
    }

    pub(super) fn delete_buffer(&mut self, id: u32) {
        let Some(buffer) = self.buffers.get_mut(&id).filter(|b| !b.deleted) else {
            return;
        };
        buffer.deleted = true;
        self.retired_buffers.push(id);
        // The WebGL extension explicitly keeps references in non-current VAOs.
        // Defer glDeleteBuffer until those references have gone away.
        unsafe {
            let g = &self.driver.gl;
            if self.array_buffer == id {
                self.array_buffer = 0;
                g.bind_buffer(gl::ARRAY_BUFFER, None);
            }
            if self.element_buffer == id {
                self.element_buffer = 0;
                g.bind_buffer(gl::ELEMENT_ARRAY_BUFFER, None);
            }
            if self.attribs.iter().any(|a| a.buffer == id) {
                g.bind_buffer(gl::ARRAY_BUFFER, None);
                for (index, attr) in self.attribs.iter_mut().enumerate() {
                    if attr.buffer == id {
                        attr.buffer = 0;
                        // A null pointer detaches driver storage. Preserve the
                        // other queryable WebGL attribute state in our shadow.
                        g.vertex_attrib_pointer_f32(
                            index as u32,
                            attr.size,
                            attr.kind,
                            attr.normalized,
                            attr.stride,
                            0,
                        );
                    }
                }
                g.bind_buffer(
                    gl::ARRAY_BUFFER,
                    self.buffers.get(&self.array_buffer).map(|b| b.handle),
                );
            }
        }
    }

    pub(super) fn collect_buffers(&mut self) {
        self.retired_buffers.retain(|id| {
            let referenced = self.array_buffer == *id
                || self.element_buffer == *id
                || self.attribs.iter().any(|a| a.buffer == *id)
                || self
                    .vertex_arrays
                    .values()
                    .any(|v| v.element_buffer == *id || v.attribs.iter().any(|a| a.buffer == *id));
            if !referenced && let Some(buffer) = self.buffers.remove(id) {
                unsafe {
                    self.driver.gl.delete_buffer(buffer.handle);
                }
                self.resources -= buffer.bytes.len();
            }
            referenced
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an installed EGL/GLES 3 driver"]
    fn webgl_vertex_array_releases_retired_buffers_after_last_attachment() {
        let mut context = Context::new(4, 4, Attributes::default(), 0, MAX_BYTES).unwrap();
        let initial = context.allocated_bytes();
        assert!(matches!(
            context.execute("extension", &[], None, "OES_vertex_array_object"),
            Reply::Bool(true)
        ));
        let number = |reply| match reply {
            Reply::Number(n) => n,
            _ => panic!("expected object name"),
        };
        let buffer = number(context.execute("createBuffer", &[], None, ""));
        context.execute("bindBuffer", &[gl::ARRAY_BUFFER as f64, buffer], None, "");
        context.execute(
            "bufferData",
            &[gl::ARRAY_BUFFER as f64, 0., gl::STATIC_DRAW as f64],
            Some(&[0; 24]),
            "",
        );
        context.execute(
            "vertexAttribPointer",
            &[0., 2., gl::FLOAT as f64, 0., 0., 0.],
            None,
            "",
        );
        let array = number(context.execute("createVertexArrayOES", &[], None, ""));
        context.execute("bindVertexArrayOES", &[array], None, "");
        context.execute(
            "vertexAttribPointer",
            &[0., 2., gl::FLOAT as f64, 0., 0., 0.],
            None,
            "",
        );
        context.execute("bindVertexArrayOES", &[0.], None, "");
        context.execute("deleteBuffer", &[buffer], None, "");
        assert_eq!(context.allocated_bytes(), initial + 24);
        assert!(context.buffers[&(buffer as u32)].deleted);
        assert_eq!(context.attribs[0].buffer, 0, "current attachment detached");
        assert_eq!(
            context.vertex_arrays[&(array as u32)].attribs[0].buffer,
            buffer as u32
        );
        context.execute("deleteVertexArrayOES", &[array], None, "");
        assert_eq!(context.allocated_bytes(), initial);
        assert!(context.buffers.is_empty());
        assert!(context.retired_buffers.is_empty());
        assert!(matches!(
            context.execute("getError", &[], None, ""),
            Reply::Number(0.)
        ));
    }
}
