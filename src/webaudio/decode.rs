//! decodeAudioData()'s decoding thread (#dom-baseaudiocontext-decodeaudiodata
//! "queue a decoding operation"): sniff the bytes (MIME Sniffing
//! #matching-an-audio-or-video-type-pattern), decode the first audio track
//! to linear PCM with the media element's demuxers and decoders, and
//! resample it to the context's rate.
//!
//! Decoding runs on up to two shared threads, which exit when idle.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::AudioData;
use super::resample::resample;
use crate::media::decode::Registry;
use crate::media::demux::{self, MemorySource};
use crate::media::track::TrackKind;

/// The most samples a decoded buffer may hold (AudioBuffer's limit).
const MAX_SAMPLES: usize = 134_217_728;
const WORKERS: usize = 2;

/// Decode `bytes` into audio at `rate` frames per second; `None` when the
/// bytes are no supported audio (EncodingError).
pub fn decode(bytes: Vec<u8>, rate: f32) -> Option<AudioData> {
    let header = &bytes[..bytes.len().min(1445)];
    demux::sniff(header)?;
    let mut demuxer = demux::open(Box::new(MemorySource::new(bytes)), None).ok()?;
    let track = demuxer
        .tracks()
        .iter()
        .find(|track| track.kind == TrackKind::Audio)?
        .clone();
    if track.encryption.is_some() {
        return None;
    }
    let mut decoder = Registry::get().audio_decoder(&track).ok()?;
    let mut pcm = Pcm::default();
    loop {
        match demuxer.read(track.id) {
            Ok(Some(packet)) => {
                // A corrupt packet is skipped when decoding can go on.
                for block in decoder.decode(&packet).unwrap_or_default() {
                    pcm.append(block, packet.discard_padding)?;
                }
            }
            Ok(None) => break,
            Err(_) if !pcm.channels.is_empty() => break,
            Err(_) => return None,
        }
    }
    for block in decoder.drain().unwrap_or_default() {
        pcm.append(block, 0)?;
    }
    let Pcm {
        channels,
        rate: source_rate,
        ..
    } = pcm;
    if channels.is_empty() || channels[0].is_empty() {
        return None;
    }
    let channels: Vec<Box<[f32]>> = channels
        .iter()
        .map(|data| resample(data, f64::from(source_rate), f64::from(rate)).into_boxed_slice())
        .collect();
    Some(AudioData {
        rate,
        length: channels[0].len(),
        channels,
    })
}

/// Decoded audio collected in planar channels.
#[derive(Default)]
struct Pcm {
    channels: Vec<Vec<f32>>,
    rate: u32,
    total: usize,
}

impl Pcm {
    /// Add a decoded block; `None` when the buffer grows too large.
    fn append(
        &mut self,
        mut block: crate::media::frame::AudioBlock,
        discard_padding: i64,
    ) -> Option<()> {
        block.trim_end(discard_padding);
        // Audio before the start of the timeline (codec delay, priming).
        if block.end() <= 0 {
            return Some(());
        }
        if block.pts < 0 {
            block.trim_start(-block.pts);
        }
        if block.channels == 0 || block.frames() == 0 {
            return Some(());
        }
        if self.channels.is_empty() {
            self.channels = vec![Vec::new(); usize::from(block.channels)];
            self.rate = block.rate;
        }
        // The first block's channel layout holds for the buffer.
        let count = usize::from(block.channels);
        let frames = block.frames();
        self.total += frames * self.channels.len();
        if self.total > MAX_SAMPLES {
            return None;
        }
        for (channel, data) in self.channels.iter_mut().enumerate() {
            let source = channel.min(count - 1);
            data.extend((0..frames).map(|frame| block.samples[frame * count + source]));
        }
        Some(())
    }
}

type Job = Box<dyn FnOnce() + Send>;

struct Pool {
    jobs: Mutex<(VecDeque<Job>, usize)>,
    ready: Condvar,
}

fn pool() -> &'static Arc<Pool> {
    static POOL: std::sync::OnceLock<Arc<Pool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        Arc::new(Pool {
            jobs: Mutex::new((VecDeque::new(), 0)),
            ready: Condvar::new(),
        })
    })
}

/// Decode on a decoding thread and hand the result to `done` there.
pub fn spawn(bytes: Vec<u8>, rate: f32, done: impl FnOnce(Option<AudioData>) + Send + 'static) {
    let pool = pool().clone();
    let mut state = pool.jobs.lock().unwrap_or_else(|e| e.into_inner());
    state
        .0
        .push_back(Box::new(move || done(decode(bytes, rate))));
    if state.1 < WORKERS {
        state.1 += 1;
        let worker = pool.clone();
        let started = std::thread::Builder::new()
            .name(String::from("trust-audio-decode"))
            .spawn(move || run(&worker));
        if started.is_err() {
            state.1 -= 1;
        }
    }
    pool.ready.notify_one();
}

fn run(pool: &Pool) {
    let mut state = pool.jobs.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(job) = state.0.pop_front() {
            drop(state);
            // A decoder failure must not take the thread's other jobs.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            state = pool.jobs.lock().unwrap_or_else(|e| e.into_inner());
            continue;
        }
        let (next, timeout) = pool
            .ready
            .wait_timeout(state, Duration::from_secs(30))
            .unwrap_or_else(|e| e.into_inner());
        state = next;
        if timeout.timed_out() && state.0.is_empty() {
            state.1 -= 1;
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-bit PCM WAV file of `frames` frames per channel.
    fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data = samples.len() * 2;
        let mut bytes = Vec::new();
        bytes.extend(b"RIFF");
        bytes.extend(&(36 + data as u32).to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(&16u32.to_le_bytes());
        bytes.extend(&1u16.to_le_bytes());
        bytes.extend(&channels.to_le_bytes());
        bytes.extend(&rate.to_le_bytes());
        bytes.extend(&(rate * u32::from(channels) * 2).to_le_bytes());
        bytes.extend(&(channels * 2).to_le_bytes());
        bytes.extend(&16u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend(&(data as u32).to_le_bytes());
        for sample in samples {
            bytes.extend(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn decodes_and_resamples_pcm() {
        let samples: Vec<i16> = (0..8000)
            .flat_map(|n| [(n % 100) as i16 * 100, -16384])
            .collect();
        let decoded = decode(wav(8000, 2, &samples), 8000.0).expect("decodes");
        assert_eq!((decoded.length, decoded.channels.len()), (8000, 2));
        assert!((decoded.channels[0][3] - 300.0 / 32768.0).abs() < 1e-6);
        assert!((decoded.channels[1][10] + 0.5).abs() < 1e-6);
        let resampled = decode(wav(8000, 2, &samples), 16_000.0).expect("decodes");
        assert_eq!(resampled.length, 16_000);
        assert!((resampled.channels[1][5000] + 0.5).abs() < 1e-3);
    }

    #[test]
    fn rejects_what_is_not_audio() {
        assert!(decode(b"<!doctype html><p>not audio".to_vec(), 48_000.0).is_none());
        assert!(decode(Vec::new(), 48_000.0).is_none());
    }

    #[test]
    fn decoding_threads_report() {
        let (sender, receiver) = std::sync::mpsc::channel();
        for _ in 0..4 {
            let sender = sender.clone();
            spawn(
                wav(8000, 1, &[0, 16384, 0, -16384]),
                8000.0,
                move |result| {
                    let _ = sender.send(result.map(|data| data.length));
                },
            );
        }
        for _ in 0..4 {
            assert_eq!(
                receiver.recv_timeout(Duration::from_secs(10)).unwrap(),
                Some(4)
            );
        }
    }
}
