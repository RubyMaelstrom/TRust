//! Tracks and compressed packets: the boundary between container parsing and
//! decoding.

use std::sync::Arc;

use super::Micros;
use super::codec::{Codec, CodecDetails};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Video,
    Text,
}

/// Video properties stated by the container.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoInfo {
    /// Coded picture size.
    pub width: u32,
    pub height: u32,
    /// Display size after the pixel aspect ratio (`pasp`, Matroska
    /// DisplayWidth/Height); equal to the coded size when square.
    pub display_width: u32,
    pub display_height: u32,
    /// ISO/IEC 23091-4 colour code points, when the container states them.
    pub color: Option<ColorInfo>,
}

/// H.273 colour description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    /// Bits per sample for PCM; zero when the codec decides.
    pub bits_per_sample: u16,
}

/// Common Encryption scheme (ISO/IEC 23001-7 §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncryptionScheme {
    /// AES-CTR, full-sample or subsample (`cenc`, WebM's only mode).
    Cenc,
    /// AES-CBC with a 1:9 style pattern (`cbcs`).
    Cbcs,
}

/// Default encryption parameters of a track (`tenc`, Matroska
/// ContentEncryption).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackEncryption {
    pub scheme: EncryptionScheme,
    pub default_kid: [u8; 16],
    pub default_is_protected: bool,
    /// Bytes of per-sample IV (0, 8 or 16); zero means `constant_iv`.
    pub per_sample_iv_size: u8,
    pub constant_iv: Option<Vec<u8>>,
    /// `cbcs` pattern: encrypted and skipped 16-byte blocks.
    pub crypt_byte_block: u8,
    pub skip_byte_block: u8,
}

/// One clear/protected subsample pair (ISO/IEC 23001-7 §7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subsample {
    pub clear: u32,
    pub protected: u32,
}

/// Per-sample decryption information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleEncryption {
    pub scheme: EncryptionScheme,
    pub kid: [u8; 16],
    /// The 8- or 16-byte IV, already resolved against a constant IV.
    pub iv: Vec<u8>,
    /// Empty means the whole sample is protected.
    pub subsamples: Vec<Subsample>,
    pub crypt_byte_block: u8,
    pub skip_byte_block: u8,
}

/// A track of a media resource.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    /// Container track identifier (ISO BMFF track_ID, Matroska TrackNumber).
    pub id: u32,
    pub kind: TrackKind,
    pub codec: Codec,
    pub details: CodecDetails,
    /// The RFC 6381 string for this track, for diagnostics and HTML's
    /// track lists.
    pub codec_string: String,
    /// Codec configuration: avcC/hvcC/av1C/vpcC payloads, the
    /// AudioSpecificConfig, OpusHead, FLAC STREAMINFO, the three Vorbis
    /// headers in Xiph lacing, or empty.
    pub config: Arc<[u8]>,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    /// Media duration of the track, when the container states it.
    pub duration: Option<Micros>,
    /// Samples to discard after decoding, at the start (Opus pre-skip,
    /// Matroska CodecDelay, AAC priming from an edit list).
    pub codec_delay: Micros,
    /// Time to pre-roll before a seek target (Matroska SeekPreRoll; Opus
    /// recommends 80 ms).
    pub seek_preroll: Micros,
    /// BCP 47 or ISO 639-2 language, empty when unknown or "und".
    pub language: String,
    pub label: String,
    /// The container marks this track as a default track.
    pub default: bool,
    pub encryption: Option<TrackEncryption>,
    /// Decoder parameters a container reader already holds in its own
    /// library's form (Symphonia's for Ogg, MP3, FLAC and WAV files).
    pub native: Option<NativeParams>,
}

/// Library-specific decoder parameters carried beside a track.
#[derive(Clone)]
pub struct NativeParams(pub Arc<dyn std::any::Any + Send + Sync>);

impl std::fmt::Debug for NativeParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeParams(..)")
    }
}

impl TrackInfo {
    pub fn new(id: u32, kind: TrackKind, codec: Codec) -> Self {
        Self {
            id,
            kind,
            codec,
            details: CodecDetails::default(),
            codec_string: String::new(),
            config: Arc::from(&[][..]),
            video: None,
            audio: None,
            duration: None,
            codec_delay: 0,
            seek_preroll: 0,
            language: String::new(),
            label: String::new(),
            default: true,
            encryption: None,
            native: None,
        }
    }
}

/// One compressed sample (an access unit, temporal unit or audio frame).
#[derive(Clone, Debug)]
pub struct Packet {
    pub track: u32,
    /// Presentation timestamp.
    pub pts: Micros,
    /// Decode timestamp (equal to `pts` without frame reordering).
    pub dts: Micros,
    pub duration: Micros,
    /// A random access point (sync sample, Matroska keyframe).
    pub keyframe: bool,
    pub data: Arc<[u8]>,
    pub encryption: Option<Box<SampleEncryption>>,
    /// Matroska DiscardPadding: decoded audio to drop from the end.
    pub discard_padding: Micros,
    /// A new track description takes effect from this packet on (a Media
    /// Source Extensions initialization segment that changed the codec
    /// configuration, as Firefox's `MediaRawData::mTrackInfo`): the decoder
    /// drains and is recreated for it when it differs.
    pub config: Option<Arc<TrackInfo>>,
}

impl Packet {
    pub fn end(&self) -> Micros {
        self.pts.saturating_add(self.duration)
    }
}
