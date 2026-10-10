//! ALSA output through `libasound.so.2`, loaded at run time (as Firefox's
//! cubeb and Chrome load it): the fallback when no PulseAudio-protocol
//! server is reachable. Uses the "default" PCM, which distributions route
//! through a mixing plugin (dmix, pipewire-alsa).
//!
//! A writer thread renders 10 ms periods and blocks in `snd_pcm_writei`;
//! the audible position is frames written minus `snd_pcm_delay`.

use std::ffi::{CString, c_char, c_int, c_long, c_uint, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{Backend, Error, Format, Position, Render};

type SndPcm = c_void;

const SND_PCM_STREAM_PLAYBACK: c_int = 0;
const SND_PCM_FORMAT_FLOAT_LE: c_int = 14;
const SND_PCM_ACCESS_RW_INTERLEAVED: c_int = 3;

struct Api {
    _library: libloading::Library,
    open: unsafe extern "C" fn(*mut *mut SndPcm, *const c_char, c_int, c_int) -> c_int,
    set_params:
        unsafe extern "C" fn(*mut SndPcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int,
    writei: unsafe extern "C" fn(*mut SndPcm, *const c_void, c_long) -> c_long,
    recover: unsafe extern "C" fn(*mut SndPcm, c_int, c_int) -> c_int,
    delay: unsafe extern "C" fn(*mut SndPcm, *mut c_long) -> c_int,
    drop: unsafe extern "C" fn(*mut SndPcm) -> c_int,
    prepare: unsafe extern "C" fn(*mut SndPcm) -> c_int,
    close: unsafe extern "C" fn(*mut SndPcm) -> c_int,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: libasound is a system library with no load-time side
        // effects TRust depends on; each symbol is resolved with its
        // documented C signature (alsa-lib pcm.h).
        unsafe {
            let library = libloading::Library::new("libasound.so.2").ok()?;
            macro_rules! symbol {
                ($name:literal) => {
                    *library.get($name).ok()?
                };
            }
            Some(Api {
                open: symbol!(b"snd_pcm_open\0"),
                set_params: symbol!(b"snd_pcm_set_params\0"),
                writei: symbol!(b"snd_pcm_writei\0"),
                recover: symbol!(b"snd_pcm_recover\0"),
                delay: symbol!(b"snd_pcm_delay\0"),
                drop: symbol!(b"snd_pcm_drop\0"),
                prepare: symbol!(b"snd_pcm_prepare\0"),
                close: symbol!(b"snd_pcm_close\0"),
                _library: library,
            })
        }
    })
    .as_ref()
}

/// An open PCM handle, used only by the writer thread after setup.
struct Pcm(*mut SndPcm);

// SAFETY: the handle is moved to the writer thread, which alone uses it
// until it closes it.
unsafe impl Send for Pcm {}

struct State {
    paused: bool,
    frames: u64,
    at: Instant,
    running: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    stop: AtomicBool,
}

pub(super) struct Alsa {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Alsa {
    pub(super) fn open(
        format: Format,
        latency: Duration,
        render: Arc<dyn Render>,
    ) -> Result<Self, Error> {
        let api = api().ok_or_else(|| Error(String::from("libasound.so.2 unavailable")))?;
        let name = CString::new("default").expect("no NUL");
        let mut pcm = std::ptr::null_mut();
        // SAFETY: valid out-pointer and NUL-terminated name.
        let status = unsafe { (api.open)(&mut pcm, name.as_ptr(), SND_PCM_STREAM_PLAYBACK, 0) };
        if status < 0 || pcm.is_null() {
            return Err(Error(format!("snd_pcm_open failed ({status})")));
        }
        // SAFETY: `pcm` is open; the requested latency with soft resampling.
        let status = unsafe {
            (api.set_params)(
                pcm,
                SND_PCM_FORMAT_FLOAT_LE,
                SND_PCM_ACCESS_RW_INTERLEAVED,
                c_uint::from(format.channels),
                format.rate,
                1,
                latency.as_micros().clamp(5_000, 1_000_000) as c_uint,
            )
        };
        if status < 0 {
            // SAFETY: closing the handle opened above.
            unsafe { (api.close)(pcm) };
            return Err(Error(format!("snd_pcm_set_params failed ({status})")));
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                paused: true,
                frames: 0,
                at: Instant::now(),
                running: false,
            }),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let thread_shared = shared.clone();
        let pcm = Pcm(pcm);
        let thread = std::thread::Builder::new()
            .name(String::from("trust-audio-alsa"))
            .spawn(move || write_loop(api, pcm, format, render, &thread_shared))
            .map_err(|e| Error(e.to_string()))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }
}

fn write_loop(api: &Api, pcm: Pcm, format: Format, render: Arc<dyn Render>, shared: &Shared) {
    let period = (format.rate as usize / 100).max(1);
    let channels = usize::from(format.channels);
    let mut buffer = vec![0f32; period * channels];
    let mut written: u64 = 0;
    loop {
        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.paused {
                // Discard what the device holds so a paused element is
                // silent at once; the position stays at what was heard.
                // SAFETY: `pcm` is open and owned by this thread.
                unsafe {
                    (api.drop)(pcm.0);
                    (api.prepare)(pcm.0);
                }
                written = state.frames;
                state.running = false;
                while state.paused && !shared.stop.load(Ordering::Acquire) {
                    state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                state.at = Instant::now();
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        buffer.fill(0.0);
        render.render(&mut buffer);
        let mut offset = 0usize;
        while offset < period {
            // SAFETY: the pointer covers `period - offset` whole frames.
            let result = unsafe {
                (api.writei)(
                    pcm.0,
                    buffer[offset * channels..].as_ptr().cast(),
                    (period - offset) as c_long,
                )
            };
            if result < 0 {
                // SAFETY: recover from underrun/suspend on our handle.
                if unsafe { (api.recover)(pcm.0, result as c_int, 1) } < 0 {
                    shared.stop.store(true, Ordering::Release);
                    break;
                }
                continue;
            }
            offset += result as usize;
        }
        written += period as u64;
        let mut delay: c_long = 0;
        // SAFETY: valid out-pointer on our handle.
        let delayed = if unsafe { (api.delay)(pcm.0, &mut delay) } == 0 {
            delay.max(0) as u64
        } else {
            0
        };
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.frames = written.saturating_sub(delayed).max(state.frames);
        state.at = Instant::now();
        state.running = true;
    }
    // SAFETY: final close of the handle this thread owns.
    unsafe {
        (api.drop)(pcm.0);
        (api.close)(pcm.0);
    }
}

impl Backend for Alsa {
    fn set_paused(&self, paused: bool) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.paused = paused;
        self.shared.wake.notify_all();
    }

    fn position(&self) -> Position {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        Position {
            frames: state.frames,
            at: state.at,
            running: state.running && !state.paused,
        }
    }

    fn name(&self) -> &'static str {
        "alsa"
    }
}

impl Drop for Alsa {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.paused = false;
        }
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
