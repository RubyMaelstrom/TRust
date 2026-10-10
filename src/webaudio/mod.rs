//! The Web Audio API's rendering engine (Web Audio 1.1, local
//! WebAudio/web-audio-api@2047f16).
//!
//! Script owns the API objects on the control thread (the prelude's Web
//! Audio block, through `webaudio_host` and the private
//! `__audio_context_binding`); each
//! BaseAudioContext renders its graph on its own rendering thread
//! ([`engine`]): an AudioContext into its audio device at the device's
//! pace, an OfflineAudioContext as fast as it can into a buffer.

pub mod bus;
pub mod decode;
pub mod engine;
pub mod graph;
pub mod nodes;
pub mod param;
pub(crate) mod resample;
mod ring;
pub mod wave;
pub mod worklet;

/// The most channels an AudioBuffer or node may have (#BaseAudioContext
/// createBuffer: at least 32 must be supported).
pub const MAX_CHANNELS: usize = 32;

/// An AudioBuffer's acquired content (#acquire-the-content): immutable
/// sample data the rendering thread reads.
#[derive(Debug)]
pub struct AudioData {
    pub rate: f32,
    pub length: usize,
    pub channels: Vec<Box<[f32]>>,
}

impl AudioData {
    pub fn bytes(&self) -> usize {
        self.length * self.channels.len() * std::mem::size_of::<f32>()
    }
}
