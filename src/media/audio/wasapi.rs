//! WASAPI shared-mode output (Windows), as Firefox's cubeb and Chrome's
//! audio service use it: the default render endpoint, event driven, with
//! the engine converting our 32-bit float frames to the mix format
//! (`AUTOCONVERTPCM`). The audible position is `IAudioClock`'s.
//!
//! COM objects live on the stream's own thread, which initialises them and
//! reports whether that worked before `open` returns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
    IAudioClock, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eConsole, eRender,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::{Backend, Error, Format, Position, Render};

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// Requested buffer: 100 ms, in 100-nanosecond units.
const BUFFER_DURATION: i64 = 1_000_000;

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

pub(super) struct Wasapi {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The speaker layout of the usual channel counts (`KSAUDIO_SPEAKER_*`);
/// 0 lets the engine map other counts directly.
fn channel_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x4,
        2 => 0x3,
        4 => 0x33,
        6 => 0x3F,
        8 => 0x63F,
        _ => 0,
    }
}

/// The stream's COM objects, made and used on its thread.
struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock,
    event: HANDLE,
    buffer_frames: u32,
    frequency: u64,
}

impl Stream {
    /// # Safety
    /// COM must be initialised on the calling thread.
    unsafe fn open(format: Format) -> windows::core::Result<Self> {
        // SAFETY: plain COM calls with valid arguments on an initialised
        // apartment; the format structure outlives `Initialize`.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let block = format.frame_bytes() as u16;
            let wave = WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                    nChannels: format.channels,
                    nSamplesPerSec: format.rate,
                    nAvgBytesPerSec: format.rate * u32::from(block),
                    nBlockAlign: block,
                    wBitsPerSample: 32,
                    cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 {
                    wValidBitsPerSample: 32,
                },
                dwChannelMask: channel_mask(format.channels),
                SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_DURATION,
                0,
                (&raw const wave).cast(),
                None,
            )?;
            let event = CreateEventW(None, false, false, None)?;
            client.SetEventHandle(event)?;
            let buffer_frames = client.GetBufferSize()?;
            let render: IAudioRenderClient = client.GetService()?;
            let clock: IAudioClock = client.GetService()?;
            let frequency = clock.GetFrequency()?.max(1);
            Ok(Self {
                client,
                render,
                clock,
                event,
                buffer_frames,
                frequency,
            })
        }
    }

    /// Frames heard since the stream started or was last reset.
    fn heard(&self, rate: u32) -> u64 {
        let mut position = 0u64;
        // SAFETY: valid out-pointer on our clock.
        if unsafe { self.clock.GetPosition(&mut position, None) }.is_err() {
            return 0;
        }
        (u128::from(position) * u128::from(rate) / u128::from(self.frequency)) as u64
    }

    /// Fill the free part of the endpoint buffer.
    fn fill(&self, format: Format, render: &dyn Render) -> windows::core::Result<()> {
        // SAFETY: GetBuffer returns `available` writable frames of our
        // format, released before the next call.
        unsafe {
            let padding = self.client.GetCurrentPadding()?;
            let available = self.buffer_frames.saturating_sub(padding);
            if available == 0 {
                return Ok(());
            }
            let data = self.render.GetBuffer(available)?;
            let samples = available as usize * usize::from(format.channels);
            let out = std::slice::from_raw_parts_mut(data.cast::<f32>(), samples);
            out.fill(0.0);
            render.render(out);
            self.render.ReleaseBuffer(available, 0)
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: stopping our client and closing our event handle.
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

impl Wasapi {
    pub(super) fn open(format: Format, render: Arc<dyn Render>) -> Result<Self, Error> {
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
        let (opened, result) = sync_channel(1);
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name(String::from("trust-audio-wasapi"))
            .spawn(move || {
                // SAFETY: this thread's COM apartment, balanced below.
                if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
                    let _ = opened.send(Err(error.to_string()));
                    return;
                }
                // SAFETY: COM is initialised on this thread.
                match unsafe { Stream::open(format) } {
                    Ok(stream) => {
                        let _ = opened.send(Ok(()));
                        run(&stream, format, render.as_ref(), &thread_shared);
                    }
                    Err(error) => {
                        let _ = opened.send(Err(error.to_string()));
                    }
                }
                // SAFETY: balances the CoInitializeEx above, after the
                // stream's COM objects are released.
                unsafe { CoUninitialize() };
            })
            .map_err(|error| Error(error.to_string()))?;
        match result.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(Error(format!("WASAPI: {message}")))
            }
            Err(_) => Err(Error(String::from("WASAPI stream thread failed"))),
        }
    }
}

fn run(stream: &Stream, format: Format, render: &dyn Render, shared: &Shared) {
    // Frames heard before the last reset (resetting restarts the clock).
    let mut base = 0u64;
    loop {
        {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.paused {
                // Drop what the endpoint holds so pausing is immediate; the
                // position stays at what was heard.
                // SAFETY: stopping and resetting our client.
                unsafe {
                    let _ = stream.client.Stop();
                    let _ = stream.client.Reset();
                }
                base = state.frames;
                state.running = false;
                while state.paused && !shared.stop.load(Ordering::Acquire) {
                    state = shared.wake.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                if shared.stop.load(Ordering::Acquire) {
                    return;
                }
                // Prime the buffer, then start.
                drop(state);
                if stream.fill(format, render).is_err() {
                    shared.stop.store(true, Ordering::Release);
                    return;
                }
                // SAFETY: starting our client.
                if unsafe { stream.client.Start() }.is_err() {
                    shared.stop.store(true, Ordering::Release);
                    return;
                }
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                state.at = Instant::now();
                state.running = true;
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: waiting on our event handle, at most 200 ms.
        unsafe { WaitForSingleObject(stream.event, 200) };
        if stream.fill(format, render).is_err() {
            // The device went away: stop as an underrun would.
            shared.stop.store(true, Ordering::Release);
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.running = false;
            return;
        }
        let heard = base + stream.heard(format.rate);
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.frames = heard.max(state.frames);
        state.at = Instant::now();
        state.running = true;
    }
}

impl Backend for Wasapi {
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
        "wasapi"
    }
}

impl Drop for Wasapi {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
