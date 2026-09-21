//! EGL 1.5 context ownership. All GL calls occur on the creating actor thread.
//! A process-wide display keeps eglTerminate from invalidating another page's
//! or the desktop compositor's contexts. Contexts and surfaces are individually
//! released; the display and dynamic loader have process lifetime.

use glow::HasContext;
use khronos_egl as egl;
use std::{cell::Cell, marker::PhantomData, rc::Rc, sync::OnceLock};

thread_local! {
    // EGL 1.5 §3.7.3: bindings belong to the calling thread and persist until
    // eglMakeCurrent changes them. This module owns every EGL binding on page
    // actor threads; the desktop compositor has its own thread. Avoid a native
    // EGL/GLVND query for every WebGL command, while retaining real switches
    // between canvases. Driver is !Send, so a context cannot migrate threads.
    static CURRENT_CONTEXT: Cell<usize> = const { Cell::new(0) };
}

struct Display {
    api: egl::DynamicInstance<egl::EGL1_5>,
    // EGLDisplay is an opaque, process-wide handle. EGL §2.6 permits concurrent
    // calls from different threads; contexts themselves are kept !Send below.
    handle: usize,
}

impl Display {
    fn handle(&self) -> egl::Display {
        // SAFETY: initialized by this API instance and never terminated.
        unsafe { egl::Display::from_ptr(self.handle as *mut _) }
    }

    fn load() -> Result<Self, String> {
        // SAFETY: only the installed system EGL library is loaded. Its exported
        // functions have the EGL ABI described by these bindings.
        // khronos-egl's convenience loader uses Unix .so names on every OS.
        // WebGL 1.0 §2.1 permits context creation to fail when no suitable
        // driver exists, but an installed Windows EGL driver must be found.
        #[cfg(windows)]
        let api = unsafe {
            egl::DynamicInstance::<egl::EGL1_5>::load_required_from_filename("libEGL.dll")
        };
        #[cfg(not(windows))]
        let api = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() };
        let api = api.map_err(|e| format!("EGL loader: {e}"))?;
        let mut last = String::from("No EGL display");
        for surfaceless in [false, true] {
            // EGL_DEFAULT_DISPLAY uses the platform default; the Mesa platform
            // is a fallback for machines with no window-system connection.
            let display = unsafe {
                if surfaceless {
                    api.get_platform_display(0x31DD, std::ptr::null_mut(), &[egl::ATTRIB_NONE])
                        .ok()
                } else {
                    api.get_display(egl::DEFAULT_DISPLAY)
                }
            };
            if let Some(display) = display {
                match api.initialize(display) {
                    Ok(_) => {
                        return Ok(Self {
                            api,
                            handle: display.as_ptr() as usize,
                        });
                    }
                    Err(e) => last = format!("EGL initialization: {e}"),
                }
            }
        }
        Err(last)
    }
}

fn display() -> Result<&'static Display, String> {
    static DISPLAY: OnceLock<Result<Display, String>> = OnceLock::new();
    DISPLAY
        .get_or_init(Display::load)
        .as_ref()
        .map_err(Clone::clone)
}

pub(super) struct Driver {
    pub gl: glow::Context,
    display: &'static Display,
    context: egl::Context,
    surface: egl::Surface,
    _actor_thread: PhantomData<Rc<()>>,
}

impl Driver {
    pub fn new() -> Result<Self, String> {
        let display = display()?;
        let api = &display.api;
        let dpy = display.handle();
        api.bind_api(egl::OPENGL_ES_API)
            .map_err(|e| e.to_string())?;
        let config = api
            .choose_first_config(
                dpy,
                &[
                    egl::SURFACE_TYPE,
                    egl::PBUFFER_BIT,
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_ES2_BIT,
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::ALPHA_SIZE,
                    8,
                    egl::NONE,
                ],
            )
            .map_err(|e| e.to_string())?
            .ok_or("No EGL GLES configuration")?;
        // WebGL #OUT_OF_RANGE_ARRAY_ACCESS requires robust shader memory access.
        // Never silently retry with a non-robust context.
        let context = api
            .create_context(
                dpy,
                config,
                None,
                &[
                    egl::CONTEXT_MAJOR_VERSION,
                    2,
                    egl::CONTEXT_OPENGL_ROBUST_ACCESS,
                    egl::TRUE as i32,
                    egl::CONTEXT_OPENGL_RESET_NOTIFICATION_STRATEGY,
                    egl::LOSE_CONTEXT_ON_RESET,
                    egl::NONE,
                ],
            )
            .map_err(|e| format!("Robust GLES context: {e}"))?;
        let surface = match api.create_pbuffer_surface(
            dpy,
            config,
            &[egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE],
        ) {
            Ok(s) => s,
            Err(e) => {
                let _ = api.destroy_context(dpy, context);
                return Err(e.to_string());
            }
        };
        if let Err(e) = api.make_current(dpy, Some(surface), Some(surface), Some(context)) {
            let _ = api.destroy_surface(dpy, surface);
            let _ = api.destroy_context(dpy, context);
            return Err(e.to_string());
        }
        CURRENT_CONTEXT.set(context.as_ptr() as usize);
        // SAFETY: this thread has the newly created GLES context current.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                api.get_proc_address(name)
                    .map_or(std::ptr::null(), |p| p as *const _)
            })
        };
        let out = Self {
            gl,
            display,
            context,
            surface,
            _actor_thread: PhantomData,
        };
        let extensions = out.gl.supported_extensions();
        if !extensions.contains("GL_KHR_robust_buffer_access_behavior")
            && !extensions.contains("GL_ARB_robust_buffer_access_behavior")
            && !(out.gl.version().is_embedded
                && (out.gl.version().major, out.gl.version().minor) >= (3, 2))
        {
            return Err("Driver does not guarantee robust shader buffer access".into());
        }
        Ok(out)
    }

    pub fn make_current(&self) -> Result<(), String> {
        let context = self.context.as_ptr() as usize;
        if CURRENT_CONTEXT.get() == context {
            return Ok(());
        }
        self.display
            .api
            .make_current(
                self.display.handle(),
                Some(self.surface),
                Some(self.surface),
                Some(self.context),
            )
            .map_err(|e| e.to_string())?;
        CURRENT_CONTEXT.set(context);
        Ok(())
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let api = &self.display.api;
        let dpy = self.display.handle();
        // EGL §3.7.2: deletion releases all objects owned by the unshared context.
        // Unbind only this context, so dropping an idle canvas does not disturb
        // another context currently in use on the same actor thread.
        if CURRENT_CONTEXT.get() == self.context.as_ptr() as usize {
            let _ = api.make_current(dpy, None, None, None);
            CURRENT_CONTEXT.set(0);
        }
        let _ = api.destroy_surface(dpy, self.surface);
        let _ = api.destroy_context(dpy, self.context);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an installed EGL/GLES driver"]
    fn system_gles_context_is_robust_and_can_read_pixels() {
        let driver = Driver::new().expect("system EGL/GLES context");
        driver.make_current().unwrap();
        unsafe {
            eprintln!(
                "{} / {}",
                driver.gl.get_parameter_string(glow::VENDOR),
                driver.gl.get_parameter_string(glow::RENDERER)
            );
            eprintln!("{}", driver.gl.get_parameter_string(glow::VERSION));
            driver.gl.clear_color(1., 0., 0., 1.);
            driver.gl.clear(glow::COLOR_BUFFER_BIT);
            let mut pixel = [0; 4];
            driver.gl.read_pixels(
                0,
                0,
                1,
                1,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixel)),
            );
            assert_eq!(pixel, [255, 0, 0, 255]);
            assert_eq!(driver.gl.get_error(), glow::NO_ERROR);
        }
        let other = Driver::new().expect("second independent context");
        // Switching back must reach EGL, and repeated commands may reuse it.
        driver.make_current().unwrap();
        driver.make_current().unwrap();
        assert_eq!(
            driver.display.api.get_current_context(),
            Some(driver.context)
        );
        other.make_current().unwrap();
        drop(driver);
        assert_eq!(other.display.api.get_current_context(), Some(other.context));
        other.make_current().unwrap();
        drop(other);
        assert!(display().unwrap().api.get_current_context().is_none());
        assert_eq!(CURRENT_CONTEXT.get(), 0);
        let replacement = Driver::new().expect("context after dropping the current one");
        replacement.make_current().unwrap();
        assert_eq!(
            replacement.display.api.get_current_context(),
            Some(replacement.context)
        );
    }
}
