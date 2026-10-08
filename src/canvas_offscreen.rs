//! HTML #the-offscreencanvas-interface and #dom-canvas-transfercontroltooffscreen
//! (local WHATWG snapshot e5071a20, 2026-09-06).
//!
//! An OffscreenCanvas's bitmap and rendering-context state belong to its
//! Realm's agent. Transferring one moves only its dimensions, inherited
//! language and direction and the weak placeholder reference (the transfer
//! steps reset the bitmap), so no engine value or bitmap crosses threads. A
//! placeholder `canvas` element receives committed frames through a shared,
//! single-slot mailbox: a busy page coalesces them, and the newest wins.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, Weak};

use resvg::tiny_skia as sk;

use crate::canvas::Canvas;

/// HTML #offscreencanvas-context-mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    None,
    TwoD,
    BitmapRenderer,
    WebGl,
    Detached,
}

/// One OffscreenCanvas object's native state.
pub(crate) struct Offscreen {
    pub width: u64,
    pub height: u64,
    pub mode: Mode,
    /// The bitmap shared with the bound rendering context (2D state included),
    /// or the bitmaprenderer's output bitmap. `None` until a context exists:
    /// a context-less OffscreenCanvas is transparent black, without storage.
    pub canvas: Option<Canvas>,
    /// HTML #offscreencanvas-inherited-lang; `None` is "explicitly unknown".
    pub language: Option<String>,
    /// HTML #offscreencanvas-inherited-direction.
    pub rtl: bool,
    pub placeholder: Option<Weak<Placeholder>>,
    /// The bitmap changed since its last push to the placeholder.
    pub dirty: bool,
    /// The Document of the OffscreenCanvas's Window (a node of it), whose SVG
    /// filters its `url()` filter references name; none in a worker.
    pub document: Option<usize>,
    /// The origin of the Realm's settings object, for origin-clean checks
    /// of image sources.
    pub origin: String,
}

impl Offscreen {
    pub fn new(width: u64, height: u64) -> Self {
        Self {
            width,
            height,
            mode: Mode::None,
            canvas: None,
            language: None,
            rtl: false,
            placeholder: None,
            dirty: false,
            document: None,
            origin: String::new(),
        }
    }

    /// The bitmap's dimensions as the 2D bitmap stores them; a dimension
    /// beyond u32 can never be allocated, so its context is lost.
    pub fn bitmap_size(&self) -> (u32, u32) {
        (
            u32::try_from(self.width).unwrap_or(u32::MAX),
            u32::try_from(self.height).unwrap_or(u32::MAX),
        )
    }

    pub fn retained_bytes(&self) -> usize {
        self.canvas.as_ref().map_or(0, Canvas::retained_bytes)
            + self.language.as_ref().map_or(0, String::capacity)
            + self.origin.capacity()
    }

    /// A premultiplied copy of the bitmap and its origin-clean flag, or None
    /// when it has no pixels. A context-less bitmap is transparent black.
    pub fn snapshot(&self) -> Option<(sk::Pixmap, bool)> {
        if self.mode == Mode::Detached {
            return None;
        }
        match &self.canvas {
            Some(canvas) => canvas
                .snapshot()
                .map(|bitmap| (bitmap, canvas.origin_clean)),
            None => {
                let (width, height) = self.bitmap_size();
                Canvas::new(width, height, true)?
                    .snapshot()
                    .map(|bitmap| (bitmap, true))
            }
        }
    }

    /// HTML OffscreenCanvas width/height setters: a 2D context is reset to
    /// its default state and its bitmap resized. The bitmaprenderer's and a
    /// context-less bitmap simply take the new dimensions.
    pub fn resize(&mut self, width: u64, height: u64) {
        self.width = width;
        self.height = height;
        let (w, h) = self.bitmap_size();
        if let Some(canvas) = self.canvas.as_mut() {
            match self.mode {
                Mode::TwoD | Mode::BitmapRenderer => {
                    let _ = canvas.resize(w, h);
                }
                // WebGL 1.0 §2.2: the drawing buffer follows the canvas size
                // at its next use (`Canvas::generation`).
                Mode::WebGl => {
                    canvas.width = w;
                    canvas.height = h;
                    canvas.generation = canvas.generation.wrapping_add(1);
                }
                Mode::None | Mode::Detached => {}
            }
        }
        self.dirty = true;
    }
}

/// A committed frame for a placeholder canvas element.
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Option<sk::Pixmap>,
    pub origin_clean: bool,
}

/// The placeholder canvas element side of a transferred control: owned by
/// the element's native canvas, weakly referenced by its OffscreenCanvas
/// wherever that is transferred (HTML keeps "weak reference semantics").
pub(crate) struct Placeholder {
    /// The element's node id in its page's DOM.
    pub node: usize,
    frame: Mutex<Option<Frame>>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Placeholder {
    pub fn new(node: usize, wake: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            node,
            frame: Mutex::new(None),
            wake,
        }
    }

    /// Store `frame` as the newest commit, replacing any frame the element's
    /// agent has not taken yet; wake that agent only for a new mailbox entry.
    pub fn commit(&self, frame: Frame) {
        let fresh = {
            let mut slot = self.frame.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = slot.is_none();
            *slot = Some(frame);
            fresh
        };
        if fresh {
            (self.wake)();
        }
    }

    pub fn take(&self) -> Option<Frame> {
        self.frame.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    pub fn pending_bytes(&self) -> usize {
        self.frame
            .lock()
            .map(|slot| {
                slot.as_ref()
                    .and_then(|frame| frame.pixels.as_ref())
                    .map_or(0, |pixels| pixels.data().len())
            })
            .unwrap_or(0)
    }
}

impl Offscreen {
    /// HTML "update the rendering" of the OffscreenCanvas's agent: push the
    /// bitmap to its placeholder canvas element, when it has one and the
    /// bitmap changed. Returns false when the placeholder no longer exists.
    pub fn push(&mut self) -> bool {
        let Some(placeholder) = self.placeholder.as_ref().and_then(Weak::upgrade) else {
            self.placeholder = None;
            return false;
        };
        if !self.dirty {
            return true;
        }
        self.dirty = false;
        let (width, height) = self.bitmap_size();
        let (pixels, origin_clean) = match self.snapshot() {
            Some((pixels, clean)) => (Some(pixels), clean),
            None => (None, true),
        };
        placeholder.commit(Frame {
            width,
            height,
            pixels,
            origin_clean,
        });
        true
    }
}

/// HTML OffscreenCanvas transfer steps' data holder.
pub(crate) struct DataHolder {
    pub width: u64,
    pub height: u64,
    pub language: Option<String>,
    pub rtl: bool,
    pub placeholder: Option<Weak<Placeholder>>,
}

/// In-flight transfers, by unguessable single-use key. Bounded: the oldest
/// unreceived holder (a message that was never delivered) is dropped first.
const MAX_IN_FLIGHT: usize = 4096;

/// Holders by key, and keys in arrival order.
type InFlight = (HashMap<String, DataHolder>, VecDeque<String>);

fn in_flight() -> &'static Mutex<InFlight> {
    static TRANSFERS: std::sync::OnceLock<Mutex<InFlight>> = std::sync::OnceLock::new();
    TRANSFERS.get_or_init(Default::default)
}

/// Record `holder` for a receiving Realm; the key is the wire capability.
pub(crate) fn send(holder: DataHolder) -> Option<String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).ok()?;
    let key: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut transfers = in_flight().lock().unwrap_or_else(|e| e.into_inner());
    let (holders, order) = &mut *transfers;
    while holders.len() >= MAX_IN_FLIGHT {
        let Some(oldest) = order.pop_front() else {
            break;
        };
        holders.remove(&oldest);
    }
    holders.insert(key.clone(), holder);
    order.push_back(key.clone());
    Some(key)
}

/// Claim a transferred holder once.
pub(crate) fn receive(key: &str) -> Option<DataHolder> {
    let mut transfers = in_flight().lock().unwrap_or_else(|e| e.into_inner());
    let (holders, order) = &mut *transfers;
    let holder = holders.remove(key)?;
    order.retain(|entry| entry != key);
    Some(holder)
}

/// Shared by the page actor: install `frame` as the placeholder element's
/// bitmap. The natural dimensions follow the committed bitmap.
pub(crate) fn present(canvas: &mut Canvas, frame: Frame) {
    canvas.adopt(frame.width, frame.height, frame.pixels, frame.origin_clean);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn transfers_are_single_use_and_bounded() {
        let key = send(DataHolder {
            width: 3,
            height: 4,
            language: Some("fr".into()),
            rtl: true,
            placeholder: None,
        })
        .unwrap();
        let holder = receive(&key).unwrap();
        assert_eq!((holder.width, holder.height, holder.rtl), (3, 4, true));
        assert_eq!(holder.language.as_deref(), Some("fr"));
        assert!(receive(&key).is_none());
        assert!(receive("guess").is_none());
    }

    #[test]
    fn placeholder_mailbox_coalesces_frames_and_wakes_once() {
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = wakes.clone();
        let link = Arc::new(Placeholder::new(
            7,
            Box::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }),
        ));
        let mut offscreen = Offscreen::new(2, 1);
        offscreen.placeholder = Some(Arc::downgrade(&link));
        offscreen.mode = Mode::TwoD;
        offscreen.canvas = Canvas::new(2, 1, true);
        // An unchanged bitmap pushes nothing.
        assert!(offscreen.push());
        assert!(link.take().is_none());
        for _ in 0..3 {
            offscreen.dirty = true;
            assert!(offscreen.push());
        }
        assert_eq!(wakes.load(std::sync::atomic::Ordering::Relaxed), 1);
        let frame = link.take().unwrap();
        assert_eq!((frame.width, frame.height), (2, 1));
        assert!(frame.pixels.is_some() && frame.origin_clean);
        drop(link);
        offscreen.dirty = true;
        assert!(!offscreen.push(), "a collected placeholder is weakly held");
        assert!(offscreen.placeholder.is_none());
    }
}
