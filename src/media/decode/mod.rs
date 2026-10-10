//! The decoder registry: every decoder this process can use, in the order
//! it prefers them, and the single source of truth for what TRust reports
//! it can play.
//!
//! The model is Firefox's platform decoder modules (dom/media/platforms):
//! each module answers whether it supports a codec, and creating a decoder
//! tries the supporting modules in order, falling through when one fails to
//! initialise. Bundled modules come first (royalty-free formats), then the
//! operating system's (H.264, AAC, HEVC and anything else the system
//! provides). Support is probed at run time, so `canPlayType('video/mp4;
//! codecs="avc1.42E01E"')` is "" on a Linux system without an H.264-capable
//! FFmpeg, exactly as in Firefox.

use std::fmt;
use std::sync::OnceLock;

use super::codec::CodecString;
use super::container::Capabilities;
use super::frame::{AudioBlock, VideoFrame};
use super::track::{Packet, TrackInfo};

// Annex B conversion for decoders that take start codes (Media
// Foundation); built everywhere so it is tested everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
mod annexb;
pub mod bundled;
#[cfg(target_os = "linux")]
pub mod ffmpeg;
#[cfg(windows)]
mod mf;
#[cfg(target_os = "linux")]
mod system;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// No module can decode this track.
    Unsupported(String),
    /// The bitstream is corrupt; decoding may continue at the next
    /// keyframe.
    Corrupt(String),
    /// The decoder failed and cannot continue.
    Failed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "unsupported media: {what}"),
            Self::Corrupt(what) => write!(f, "corrupt media data: {what}"),
            Self::Failed(what) => write!(f, "media decoder failed: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// A video decoder. Packets arrive in decode order; frames leave in
/// presentation order with the packet timestamps they belong to.
pub trait VideoDecoder: Send {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<VideoFrame>, Error>;
    /// End of stream: return frames still held for reordering.
    fn drain(&mut self) -> Result<Vec<VideoFrame>, Error>;
    /// Discard all state (before a seek); the next packet is a keyframe.
    fn flush(&mut self);
    fn name(&self) -> &'static str;
    fn hardware(&self) -> bool {
        false
    }
}

/// An audio decoder producing interleaved float PCM.
pub trait AudioDecoder: Send {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<AudioBlock>, Error>;
    fn drain(&mut self) -> Result<Vec<AudioBlock>, Error> {
        Ok(Vec::new())
    }
    fn flush(&mut self);
    fn name(&self) -> &'static str;
}

/// One source of decoders (Firefox's PlatformDecoderModule).
pub trait DecoderModule: Send + Sync {
    fn name(&self) -> &'static str;
    fn supports(&self, codec: &CodecString) -> bool;
    /// Whether decoding `codec` is hardware accelerated (Media
    /// Capabilities' `powerEfficient`).
    fn hardware(&self, _codec: &CodecString) -> bool {
        false
    }
    fn video(&self, _track: &TrackInfo) -> Option<Result<Box<dyn VideoDecoder>, Error>> {
        None
    }
    fn audio(&self, _track: &TrackInfo) -> Option<Result<Box<dyn AudioDecoder>, Error>> {
        None
    }
}

pub struct Registry {
    modules: Vec<Box<dyn DecoderModule>>,
}

impl Registry {
    pub fn new(modules: Vec<Box<dyn DecoderModule>>) -> Self {
        Self { modules }
    }

    /// The process-wide registry, probed on first use.
    pub fn get() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            let mut modules: Vec<Box<dyn DecoderModule>> = vec![Box::new(bundled::Bundled)];
            modules.extend(system_modules());
            Registry::new(modules)
        })
    }

    pub fn modules(&self) -> impl Iterator<Item = &dyn DecoderModule> {
        self.modules.iter().map(|module| module.as_ref())
    }

    fn query(track: &TrackInfo) -> CodecString {
        CodecString {
            codec: track.codec,
            details: track.details,
            ambiguous: false,
        }
    }

    /// Whether some module decodes `codec` with hardware acceleration.
    pub fn hardware(&self, codec: &CodecString) -> bool {
        self.modules
            .iter()
            .find(|module| module.supports(codec))
            .is_some_and(|module| module.hardware(codec))
    }

    /// Create a video decoder, falling back through the supporting modules.
    pub fn video_decoder(&self, track: &TrackInfo) -> Result<Box<dyn VideoDecoder>, Error> {
        let query = Self::query(track);
        let mut last = Error::Unsupported(format!("no decoder for {}", track.codec_string));
        for module in self.modules.iter().filter(|module| module.supports(&query)) {
            match module.video(track) {
                Some(Ok(decoder)) => return Ok(decoder),
                Some(Err(error)) => last = error,
                None => {}
            }
        }
        Err(last)
    }

    /// Create an audio decoder, falling back through the supporting modules.
    pub fn audio_decoder(&self, track: &TrackInfo) -> Result<Box<dyn AudioDecoder>, Error> {
        let query = Self::query(track);
        let mut last = Error::Unsupported(format!("no decoder for {}", track.codec_string));
        for module in self.modules.iter().filter(|module| module.supports(&query)) {
            match module.audio(track) {
                Some(Ok(decoder)) => return Ok(decoder),
                Some(Err(error)) => last = error,
                None => {}
            }
        }
        Err(last)
    }
}

impl Capabilities for Registry {
    fn decodes(&self, codec: &CodecString) -> bool {
        self.modules.iter().any(|module| module.supports(codec))
    }

    fn hardware(&self, codec: &CodecString) -> bool {
        Registry::hardware(self, codec)
    }
}

/// The operating system's decoders.
fn system_modules() -> Vec<Box<dyn DecoderModule>> {
    #[cfg(target_os = "linux")]
    {
        vec![Box::new(system::SystemFfmpeg)]
    }
    #[cfg(windows)]
    {
        vec![Box::new(mf::MediaFoundation)]
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Vec::new()
    }
}
