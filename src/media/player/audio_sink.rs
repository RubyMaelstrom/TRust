//! The audio half of playback: a bounded queue of decoded audio drained by
//! the output device's thread, and the map from output frames to media
//! time that the clock reads.
//!
//! HTML #playing-the-media-resource: audio plays at the effective media
//! volume, synchronised with the current playback position, and at a
//! playbackRate other than 1 a pitch-preserving algorithm applies unless
//! `preservesPitch` is false (#dom-media-preservespitch). Pitch is preserved
//! with WSOLA (waveform-similarity overlap-add), the method Chrome's
//! AudioRendererAlgorithm uses; without it, audio is resampled. Outside
//! 0.25×–4× audio is muted while the clock still advances, as in Chrome and
//! Firefox.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::media::audio::{Format, Output, Render};
use crate::media::frame::AudioBlock;
use crate::media::{MICROS_PER_SECOND, Micros};

/// Decoded audio the control thread keeps ahead of the device.
pub(crate) const TARGET_AHEAD: Micros = 1_000_000;
/// Below this much queued audio the control thread is woken to decode more.
pub(crate) const LOW_WATER: Micros = 500_000;

const MIN_AUDIBLE_RATE: f64 = 0.25;
const MAX_AUDIBLE_RATE: f64 = 4.0;

/// Where output frame `frame` maps onto the media timeline: media time
/// `pts`, advancing `speed` media seconds per output second (0 while the
/// queue ran dry).
#[derive(Clone, Copy, Debug)]
struct Checkpoint {
    frame: u64,
    pts: Micros,
    speed: f64,
}

struct Queue {
    /// Decoded audio in the output format, oldest first.
    blocks: VecDeque<AudioBlock>,
    /// Frames of `blocks[0]` already consumed.
    consumed: usize,
    /// Media time of the next frame to consume.
    next_pts: Option<Micros>,
    /// Frames queued in total.
    queued_frames: usize,
    end_of_stream: bool,
    /// The device ran dry (and the clock froze) since the last data.
    starved: bool,
    stretch: Stretch,
}

pub(crate) struct AudioSink {
    format: Format,
    queue: Mutex<Queue>,
    checkpoints: Mutex<VecDeque<Checkpoint>>,
    /// Output frames rendered so far.
    rendered: AtomicU64,
    volume: AtomicU32,
    muted: AtomicBool,
    rate: AtomicU64,
    preserves_pitch: AtomicBool,
    output: Mutex<Option<Output>>,
    /// Woken when the queue falls below the low-water mark or runs dry.
    pub(crate) need_data: Arc<(Mutex<bool>, Condvar)>,
}

impl AudioSink {
    pub(crate) fn new(format: Format, need_data: Arc<(Mutex<bool>, Condvar)>) -> Arc<Self> {
        Arc::new(Self {
            format,
            queue: Mutex::new(Queue {
                blocks: VecDeque::new(),
                consumed: 0,
                next_pts: None,
                queued_frames: 0,
                end_of_stream: false,
                starved: false,
                stretch: Stretch::new(format),
            }),
            checkpoints: Mutex::new(VecDeque::new()),
            rendered: AtomicU64::new(0),
            volume: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            rate: AtomicU64::new(1.0f64.to_bits()),
            preserves_pitch: AtomicBool::new(true),
            output: Mutex::new(None),
            need_data,
        })
    }

    pub(crate) fn format(&self) -> Format {
        self.format
    }

    /// Attach the device stream (opened with this sink as its renderer).
    pub(crate) fn attach(&self, output: Output) {
        *self.output.lock().unwrap_or_else(|e| e.into_inner()) = Some(output);
    }

    pub(crate) fn set_paused(&self, paused: bool) {
        if let Some(output) = self
            .output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            output.set_paused(paused);
        }
    }

    pub(crate) fn set_volume(&self, volume: f32, muted: bool) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub(crate) fn set_rate(&self, rate: f64, preserves_pitch: bool) {
        self.rate.store(rate.to_bits(), Ordering::Relaxed);
        self.preserves_pitch
            .store(preserves_pitch, Ordering::Relaxed);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue a decoded block (already in the output format).
    pub(crate) fn push(&self, block: AudioBlock) {
        if block.frames() == 0 {
            return;
        }
        let mut queue = self.lock();
        if queue.next_pts.is_none() {
            queue.next_pts = Some(block.pts);
        }
        queue.queued_frames += block.frames();
        queue.blocks.push_back(block);
    }

    pub(crate) fn set_end_of_stream(&self) {
        self.lock().end_of_stream = true;
        self.wake();
    }

    /// Microseconds of audio queued ahead of the device.
    pub(crate) fn queued(&self) -> Micros {
        let queue = self.lock();
        queue.queued_frames as i64 * MICROS_PER_SECOND / i64::from(self.format.rate)
    }

    /// Whether the queue holds everything up to the end of the stream.
    pub(crate) fn finished_queueing(&self) -> bool {
        self.lock().end_of_stream
    }

    /// The device has played everything, end of stream included.
    pub(crate) fn drained(&self) -> bool {
        let queue = self.lock();
        queue.end_of_stream && queue.queued_frames == 0
    }

    /// The queue ran dry before the end of the stream.
    pub(crate) fn starved(&self) -> bool {
        self.lock().starved
    }

    /// Forget everything queued; playback resumes at `time` (a seek).
    pub(crate) fn flush(&self, time: Micros) {
        let mut queue = self.lock();
        queue.blocks.clear();
        queue.consumed = 0;
        queue.queued_frames = 0;
        queue.next_pts = None;
        queue.end_of_stream = false;
        queue.starved = false;
        queue.stretch.reset();
        drop(queue);
        let frame = self.rendered.load(Ordering::Acquire);
        let mut checkpoints = self.checkpoints.lock().unwrap_or_else(|e| e.into_inner());
        checkpoints.clear();
        checkpoints.push_back(Checkpoint {
            frame,
            pts: time,
            speed: 0.0,
        });
    }

    fn wake(&self) {
        let (flag, condvar) = &*self.need_data;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        condvar.notify_all();
    }

    fn checkpoint(&self, frame: u64, pts: Micros, speed: f64) {
        let mut checkpoints = self.checkpoints.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(last) = checkpoints.back()
            && last.speed == speed
        {
            // Continuous with the previous segment: no new checkpoint.
            let expected = last.pts
                + ((frame - last.frame) as f64 * speed * MICROS_PER_SECOND as f64
                    / f64::from(self.format.rate)) as i64;
            if (expected - pts).abs() < 2_000 {
                return;
            }
        }
        checkpoints.push_back(Checkpoint { frame, pts, speed });
        while checkpoints.len() > 256 {
            checkpoints.pop_front();
        }
    }

    /// Media time of the audio heard at `now`, or `None` before any audio
    /// was rendered.
    pub(crate) fn media_time(&self, now: Instant) -> Option<Micros> {
        let position = self
            .output
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .position();
        let heard = position
            .frames_at(now, self.format.rate)
            .min(self.rendered.load(Ordering::Acquire));
        let checkpoints = self.checkpoints.lock().unwrap_or_else(|e| e.into_inner());
        let checkpoint = checkpoints
            .iter()
            .rev()
            .find(|c| c.frame <= heard)
            .or(checkpoints.front())?;
        let frames = heard.saturating_sub(checkpoint.frame) as f64;
        Some(
            checkpoint.pts
                + (frames * checkpoint.speed * MICROS_PER_SECOND as f64
                    / f64::from(self.format.rate)) as i64,
        )
    }

    /// Take up to `frames` source frames from the queue into `out`
    /// (interleaved), returning how many were taken and the media time of
    /// the first.
    fn take(
        queue: &mut Queue,
        channels: usize,
        frames: usize,
        out: &mut Vec<f32>,
    ) -> (usize, Option<Micros>, u32) {
        let start_pts = queue.next_pts;
        let mut taken = 0;
        let mut rate = 0;
        while taken < frames {
            let Some(block) = queue.blocks.front() else {
                break;
            };
            rate = block.rate;
            let block_frames = block.frames();
            let available = block_frames - queue.consumed;
            let count = available.min(frames - taken);
            let start = queue.consumed * channels;
            out.extend_from_slice(&block.samples[start..start + count * channels]);
            queue.consumed += count;
            taken += count;
            if queue.consumed == block_frames {
                queue.blocks.pop_front();
                queue.consumed = 0;
                let advanced = queue
                    .next_pts
                    .map(|pts| pts + frames_to_micros(count, rate));
                queue.next_pts = queue.blocks.front().map(|b| b.pts).or(advanced);
            } else if let Some(pts) = queue.next_pts.as_mut() {
                *pts += frames_to_micros(count, rate);
            }
        }
        queue.queued_frames -= taken;
        (taken, start_pts, rate)
    }
}

fn frames_to_micros(frames: usize, rate: u32) -> Micros {
    frames as i64 * MICROS_PER_SECOND / i64::from(rate.max(1))
}

impl Render for AudioSink {
    fn render(&self, out: &mut [f32]) -> usize {
        let channels = usize::from(self.format.channels);
        let frames = out.len() / channels;
        let start = self.rendered.fetch_add(frames as u64, Ordering::AcqRel);
        let rate = f64::from_bits(self.rate.load(Ordering::Relaxed));
        let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
        let muted = self.muted.load(Ordering::Relaxed);
        let preserve = self.preserves_pitch.load(Ordering::Relaxed);
        let audible = (MIN_AUDIBLE_RATE..=MAX_AUDIBLE_RATE).contains(&rate);
        let gain = if muted || !audible { 0.0 } else { volume };
        let mut queue = self.lock();
        let mut produced = 0usize;
        let mut first_pts = None;
        if (rate - 1.0).abs() < 1e-9 {
            queue.stretch.reset();
            let mut scratch = Vec::with_capacity(frames * channels);
            let (taken, pts, _) = Self::take(&mut queue, channels, frames, &mut scratch);
            first_pts = pts;
            for (dst, src) in out.iter_mut().zip(scratch.iter()) {
                *dst = src * gain;
            }
            produced = taken;
        } else if rate > 0.0 {
            // Stretch (pitch preserved) or resample (pitch follows) by
            // `rate`, consuming about `frames * rate` source frames.
            let mode = if preserve && audible {
                StretchMode::Wsola
            } else {
                StretchMode::Resample
            };
            let pts = queue.next_pts;
            let needed = queue.stretch.input_needed(frames, rate, mode);
            let mut input = Vec::with_capacity(needed * channels);
            let (taken, _, _) = Self::take(&mut queue, channels, needed, &mut input);
            queue.stretch.feed(&input[..taken * channels]);
            produced = queue.stretch.produce(out, frames, rate, mode);
            if produced > 0 {
                first_pts = pts.map(|pts| pts - queue.stretch.lag_micros(rate));
            }
            for sample in out[..produced * channels].iter_mut() {
                *sample *= gain;
            }
        }
        out[produced * channels..].fill(0.0);
        let starved_now = produced < frames && !queue.end_of_stream;
        let ran_out = produced < frames;
        let next_pts = queue.next_pts;
        let low = queue.queued_frames
            < (self.format.rate as usize * LOW_WATER as usize / MICROS_PER_SECOND as usize);
        if starved_now {
            queue.starved = true;
        } else if produced > 0 {
            queue.starved = false;
        }
        drop(queue);
        if let Some(pts) = first_pts {
            self.checkpoint(start, pts, rate);
        }
        if ran_out {
            // The clock holds where the audio stopped.
            let pts = next_pts.or_else(|| {
                let checkpoints = self.checkpoints.lock().unwrap_or_else(|e| e.into_inner());
                checkpoints.back().map(|c| {
                    c.pts
                        + ((start + produced as u64).saturating_sub(c.frame) as f64
                            * c.speed
                            * MICROS_PER_SECOND as f64
                            / f64::from(self.format.rate)) as i64
                })
            });
            if let Some(pts) = pts {
                self.checkpoint(start + produced as u64, pts, 0.0);
            }
        }
        if low || ran_out {
            self.wake();
        }
        frames
    }
}

/// Convert a decoded block to the sink's output format (channel layout and
/// sample rate), so one device stream serves a whole track even when a
/// stream changes format (HE-AAC, MSE track switches).
pub(crate) fn adapt(block: AudioBlock, format: Format) -> AudioBlock {
    let channels_in = usize::from(block.channels);
    let channels_out = usize::from(format.channels);
    let remixed: Arc<[f32]> = if channels_in == channels_out {
        block.samples.clone()
    } else {
        let frames = block.frames();
        let mut out = vec![0f32; frames * channels_out];
        for frame in 0..frames {
            let input = &block.samples[frame * channels_in..(frame + 1) * channels_in];
            let output = &mut out[frame * channels_out..(frame + 1) * channels_out];
            remix(input, output);
        }
        Arc::from(out)
    };
    if block.rate == format.rate || block.rate == 0 {
        return AudioBlock {
            pts: block.pts,
            rate: format.rate,
            channels: format.channels,
            samples: remixed,
        };
    }
    // Linear resampling to the output rate.
    let frames_in = remixed.len() / channels_out;
    let ratio = f64::from(block.rate) / f64::from(format.rate);
    let frames_out = ((frames_in as f64) / ratio).floor() as usize;
    let mut out = vec![0f32; frames_out * channels_out];
    for index in 0..frames_out {
        let position = index as f64 * ratio;
        let base = position.floor() as usize;
        let fraction = (position - base as f64) as f32;
        let next = (base + 1).min(frames_in.saturating_sub(1));
        for channel in 0..channels_out {
            let a = remixed[base * channels_out + channel];
            let b = remixed[next * channels_out + channel];
            out[index * channels_out + channel] = a + (b - a) * fraction;
        }
    }
    AudioBlock {
        pts: block.pts,
        rate: format.rate,
        channels: format.channels,
        samples: Arc::from(out),
    }
}

/// Remix one frame between channel counts (WAVE channel order): mono
/// spreads, stereo-or-more folds into mono or stereo with the usual −3 dB
/// centre and surround coefficients, other mismatches copy by index.
fn remix(input: &[f32], output: &mut [f32]) {
    const C: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match (input.len(), output.len()) {
        (1, _) => output.iter_mut().for_each(|o| *o = input[0]),
        (n, 1) => output[0] = input.iter().sum::<f32>() / n as f32,
        (6, 2) => {
            // FL FR FC LFE BL BR.
            output[0] = (input[0] + C * input[2] + C * input[4]) / (1.0 + 2.0 * C);
            output[1] = (input[1] + C * input[2] + C * input[5]) / (1.0 + 2.0 * C);
        }
        (8, 2) => {
            // FL FR FC LFE BL BR SL SR.
            output[0] = (input[0] + C * input[2] + C * input[4] + C * input[6]) / (1.0 + 3.0 * C);
            output[1] = (input[1] + C * input[2] + C * input[5] + C * input[7]) / (1.0 + 3.0 * C);
        }
        (_, _) => {
            for (index, o) in output.iter_mut().enumerate() {
                *o = input.get(index).copied().unwrap_or(0.0);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StretchMode {
    Wsola,
    Resample,
}

/// WSOLA time-stretching and plain resampling for playback rates other
/// than 1.
struct Stretch {
    channels: usize,
    /// Overlap-add window length in frames (20 ms).
    window: usize,
    /// Search radius in frames (15 ms).
    radius: usize,
    /// Pending source frames, interleaved.
    input: Vec<f32>,
    /// Fractional read position into `input` (frames).
    position: f64,
    /// The previous window's second half, already windowed, to overlap.
    tail: Vec<f32>,
    /// Stretched output not yet handed out, interleaved.
    pending: VecDeque<f32>,
    hann: Vec<f32>,
}

impl Stretch {
    fn new(format: Format) -> Self {
        let window = (format.rate as usize / 50).max(64) & !1;
        let hann = (0..window)
            .map(|i| {
                let x = std::f32::consts::PI * 2.0 * i as f32 / window as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        Self {
            channels: usize::from(format.channels),
            window,
            radius: (format.rate as usize * 15 / 1000).max(16),
            input: Vec::new(),
            position: 0.0,
            tail: Vec::new(),
            pending: VecDeque::new(),
            hann,
        }
    }

    fn reset(&mut self) {
        self.input.clear();
        self.position = 0.0;
        self.tail.clear();
        self.pending.clear();
    }

    /// Source frames to request to produce `frames` output frames.
    fn input_needed(&self, frames: usize, rate: f64, mode: StretchMode) -> usize {
        let held = self.input.len() / self.channels;
        let pending = self.pending.len() / self.channels;
        let missing = frames.saturating_sub(pending);
        let consumed = (missing as f64 * rate).ceil() as usize;
        let lookahead = match mode {
            StretchMode::Wsola => self.window + 2 * self.radius,
            StretchMode::Resample => 2,
        };
        (self.position.floor() as usize + consumed + lookahead).saturating_sub(held)
    }

    fn feed(&mut self, samples: &[f32]) {
        self.input.extend_from_slice(samples);
    }

    /// How far the output lags the source read position.
    fn lag_micros(&self, rate: f64) -> Micros {
        let pending = (self.pending.len() / self.channels) as f64 * rate;
        (pending * 1_000_000.0 / (self.window as f64 * 50.0)) as Micros
    }

    fn produce(&mut self, out: &mut [f32], frames: usize, rate: f64, mode: StretchMode) -> usize {
        let channels = self.channels;
        while self.pending.len() / channels < frames {
            let progressed = match mode {
                StretchMode::Resample => self.resample_step(rate),
                StretchMode::Wsola => self.wsola_step(rate),
            };
            if !progressed {
                break;
            }
        }
        let available = (self.pending.len() / channels).min(frames);
        for (dst, src) in out[..available * channels]
            .iter_mut()
            .zip(self.pending.drain(..available * channels))
        {
            *dst = src;
        }
        // Drop consumed input.
        let consumed = (self.position.floor() as usize).saturating_sub(self.radius + 1);
        if consumed > 0 {
            self.input.drain(..consumed * channels);
            self.position -= consumed as f64;
        }
        available
    }

    fn frame(&self, index: usize, channel: usize) -> f32 {
        self.input
            .get(index * self.channels + channel)
            .copied()
            .unwrap_or(0.0)
    }

    fn resample_step(&mut self, rate: f64) -> bool {
        let held = self.input.len() / self.channels;
        let mut produced = false;
        for _ in 0..256 {
            let base = self.position.floor() as usize;
            if base + 1 >= held {
                break;
            }
            let fraction = (self.position - base as f64) as f32;
            for channel in 0..self.channels {
                let a = self.frame(base, channel);
                let b = self.frame(base + 1, channel);
                self.pending.push_back(a + (b - a) * fraction);
            }
            self.position += rate;
            produced = true;
        }
        produced
    }

    /// One WSOLA hop: output `window / 2` frames.
    fn wsola_step(&mut self, rate: f64) -> bool {
        let channels = self.channels;
        let hop = self.window / 2;
        let held = self.input.len() / channels;
        let target = self.position.round() as usize;
        if target + self.radius + self.window >= held {
            return false;
        }
        // Find the offset whose window best continues the previous output
        // (normalised cross-correlation against the tail).
        let mut best = target;
        if !self.tail.is_empty() {
            let low = target.saturating_sub(self.radius);
            let high = target + self.radius;
            let score = |start: usize| -> f32 {
                let mut dot = 0f32;
                let mut energy = 1e-9f32;
                for i in (0..hop).step_by(2) {
                    let mut candidate = 0f32;
                    let mut reference = 0f32;
                    for channel in 0..channels {
                        candidate += self.frame(start + i, channel);
                        reference += self.tail[i * channels + channel];
                    }
                    dot += candidate * reference;
                    energy += candidate * candidate;
                }
                dot / energy.sqrt()
            };
            let mut best_score = f32::MIN;
            let mut start = low;
            while start <= high {
                let value = score(start);
                if value > best_score {
                    best_score = value;
                    best = start;
                }
                start += 4;
            }
            for start in best.saturating_sub(3).max(low)..=(best + 3).min(high) {
                let value = score(start);
                if value > best_score {
                    best_score = value;
                    best = start;
                }
            }
        }
        // Overlap-add the first half of the chosen window onto the tail.
        for i in 0..hop {
            let weight = self.hann[i];
            for channel in 0..channels {
                let sample = self.frame(best + i, channel) * weight;
                let tail = self
                    .tail
                    .get(i * channels + channel)
                    .copied()
                    .unwrap_or(0.0);
                self.pending.push_back(sample + tail);
            }
        }
        self.tail.clear();
        for i in hop..self.window {
            let weight = self.hann[i];
            for channel in 0..channels {
                self.tail.push(self.frame(best + i, channel) * weight);
            }
        }
        self.position += hop as f64 * rate;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(rate: u32, channels: u16) -> Arc<AudioSink> {
        AudioSink::new(
            Format { rate, channels },
            Arc::new((Mutex::new(false), Condvar::new())),
        )
    }

    fn tone(pts: Micros, rate: u32, frames: usize) -> AudioBlock {
        AudioBlock {
            pts,
            rate,
            channels: 1,
            samples: Arc::from(
                (0..frames)
                    .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5)
                    .collect::<Vec<_>>(),
            ),
        }
    }

    #[test]
    fn renders_in_order_with_volume_and_checkpoints() {
        let sink = sink(1000, 1);
        sink.push(AudioBlock {
            pts: 0,
            rate: 1000,
            channels: 1,
            samples: Arc::from(vec![1.0f32; 500]),
        });
        sink.push(AudioBlock {
            pts: 500_000,
            rate: 1000,
            channels: 1,
            samples: Arc::from(vec![0.5f32; 500]),
        });
        sink.set_volume(0.5, false);
        let mut out = vec![0f32; 600];
        sink.render(&mut out);
        assert_eq!(out[0], 0.5);
        assert_eq!(out[599], 0.25);
        assert_eq!(sink.queued(), 400_000);
        let checkpoints = sink.checkpoints.lock().unwrap();
        assert_eq!(checkpoints[0].pts, 0);
        assert_eq!(checkpoints.len(), 1);
    }

    #[test]
    fn running_dry_freezes_the_media_time() {
        let sink = sink(1000, 1);
        sink.push(AudioBlock {
            pts: 0,
            rate: 1000,
            channels: 1,
            samples: Arc::from(vec![0.1f32; 100]),
        });
        let mut out = vec![0f32; 300];
        sink.render(&mut out);
        assert!(sink.starved());
        let checkpoints = sink.checkpoints.lock().unwrap();
        let last = checkpoints.back().unwrap();
        assert_eq!((last.frame, last.pts, last.speed), (100, 100_000, 0.0));
    }

    #[test]
    fn stretching_consumes_input_at_the_rate() {
        for (rate, preserve) in [(1.5, true), (0.5, true), (2.0, false)] {
            let sink = sink(48_000, 1);
            sink.push(tone(0, 48_000, 48_000 * 4));
            sink.set_rate(rate, preserve);
            let mut out = vec![0f32; 4800];
            let mut produced = 0;
            for _ in 0..10 {
                produced += sink.render(&mut out);
            }
            assert_eq!(produced, 48_000);
            let remaining = sink.queued() as f64 / 1_000_000.0;
            let consumed = 4.0 - remaining;
            // One second of output consumed `rate` seconds of input, give or
            // take the lookahead window.
            assert!((consumed - rate).abs() < 0.1, "{rate}: consumed {consumed}");
            assert!(out.iter().any(|s| s.abs() > 0.1), "{rate}: silent");
        }
    }

    #[test]
    fn adapting_formats() {
        let stereo = adapt(
            AudioBlock {
                pts: 7,
                rate: 1000,
                channels: 1,
                samples: Arc::from(vec![0.25f32; 10]),
            },
            Format {
                rate: 1000,
                channels: 2,
            },
        );
        assert_eq!(stereo.samples.len(), 20);
        assert_eq!(stereo.samples[1], 0.25);
        let resampled = adapt(
            AudioBlock {
                pts: 7,
                rate: 2000,
                channels: 2,
                samples: Arc::from(vec![0.5f32; 200]),
            },
            Format {
                rate: 1000,
                channels: 2,
            },
        );
        assert_eq!(resampled.frames(), 50);
        assert_eq!(resampled.pts, 7);
        let surround = adapt(
            AudioBlock {
                pts: 0,
                rate: 1000,
                channels: 6,
                samples: Arc::from(vec![0.3f32; 60]),
            },
            Format {
                rate: 1000,
                channels: 2,
            },
        );
        assert!((surround.samples[0] - 0.3).abs() < 1e-5);
    }
}
