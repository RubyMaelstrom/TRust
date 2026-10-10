//! Media container MIME types and the support decisions built on them:
//! HTML's `canPlayType()` (local whatwg/html e5071a2 #dom-navigator-canplaytype)
//! and Media Source Extensions' `isTypeSupported()` (local w3c/media-source
//! 8e0611a0 #dom-mediasource-istypesupported).
//!
//! Which codecs each container may carry follows the MSE byte stream format
//! registry (local w3c/mse-byte-stream-format-registry 1c88a2f9) and the
//! formats major browsers accept for each MIME type. Whether a codec can be
//! decoded is asked of a [`Capabilities`] implementation, normally the
//! decoder registry, so every answer reflects the decoders this process
//! actually has.

use super::codec::{self, Codec, CodecString};

/// A parsed MIME type (MIME Sniffing #mime-type-representation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MimeType {
    pub kind: String,
    pub subtype: String,
    /// Parameters in insertion order, names lowercased.
    pub parameters: Vec<(String, String)>,
}

impl MimeType {
    pub fn essence(&self) -> String {
        format!("{}/{}", self.kind, self.subtype)
    }

    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn http_whitespace(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\t' | ' ')
}

fn http_token(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn quoted_string_token(c: char) -> bool {
    c == '\t' || (' '..='~').contains(&c) || ('\u{80}'..='\u{FF}').contains(&c)
}

/// MIME Sniffing #parse-a-mime-type (local whatwg/mimesniff snapshot).
pub fn parse_mime_type(input: &str) -> Option<MimeType> {
    let input = input.trim_matches(http_whitespace);
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0;
    let collect = |pos: &mut usize, stop: &dyn Fn(char) -> bool| -> String {
        let start = *pos;
        while *pos < chars.len() && !stop(chars[*pos]) {
            *pos += 1;
        }
        chars[start..*pos].iter().collect()
    };
    let kind = collect(&mut pos, &|c| c == '/');
    if kind.is_empty() || !kind.chars().all(http_token) || pos >= chars.len() {
        return None;
    }
    pos += 1;
    let subtype = collect(&mut pos, &|c| c == ';');
    let subtype = subtype.trim_end_matches(http_whitespace).to_string();
    if subtype.is_empty() || !subtype.chars().all(http_token) {
        return None;
    }
    let mut mime = MimeType {
        kind: kind.to_ascii_lowercase(),
        subtype: subtype.to_ascii_lowercase(),
        parameters: Vec::new(),
    };
    while pos < chars.len() {
        pos += 1;
        while pos < chars.len() && http_whitespace(chars[pos]) {
            pos += 1;
        }
        let name = collect(&mut pos, &|c| c == ';' || c == '=').to_ascii_lowercase();
        if pos < chars.len() {
            if chars[pos] == ';' {
                continue;
            }
            pos += 1;
        }
        if pos >= chars.len() {
            break;
        }
        let value;
        if chars[pos] == '"' {
            // Collect an HTTP quoted string, extracting the value.
            pos += 1;
            let mut collected = String::new();
            loop {
                let start = pos;
                while pos < chars.len() && chars[pos] != '"' && chars[pos] != '\\' {
                    pos += 1;
                }
                collected.extend(&chars[start..pos]);
                if pos >= chars.len() {
                    break;
                }
                let quote_or_backslash = chars[pos];
                pos += 1;
                if quote_or_backslash == '\\' {
                    if pos >= chars.len() {
                        collected.push('\\');
                        break;
                    }
                    collected.push(chars[pos]);
                    pos += 1;
                } else {
                    break;
                }
            }
            value = collected;
            collect(&mut pos, &|c| c == ';');
        } else {
            let raw = collect(&mut pos, &|c| c == ';');
            value = raw.trim_end_matches(http_whitespace).to_string();
            if value.is_empty() {
                continue;
            }
        }
        if !name.is_empty()
            && name.chars().all(http_token)
            && value.chars().all(quoted_string_token)
            && !mime.parameters.iter().any(|(key, _)| *key == name)
        {
            mime.parameters.push((name, value));
        }
    }
    Some(mime)
}

/// A container format TRust parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Container {
    Mp4,
    WebM,
    Matroska,
    Ogg,
    MpegAudio,
    Adts,
    Flac,
    Wav,
}

impl Container {
    /// The container for a MIME essence, and whether it is an audio-only
    /// type (`audio/*`, which cannot carry video).
    pub fn from_essence(essence: &str) -> Option<(Self, bool)> {
        let (container, audio_only) = match essence {
            "video/mp4" | "video/x-m4v" => (Self::Mp4, false),
            "audio/mp4" | "audio/x-m4a" | "audio/m4a" => (Self::Mp4, true),
            "video/webm" => (Self::WebM, false),
            "audio/webm" => (Self::WebM, true),
            "video/x-matroska" => (Self::Matroska, false),
            "audio/x-matroska" => (Self::Matroska, true),
            "video/ogg" | "application/ogg" => (Self::Ogg, false),
            "audio/ogg" => (Self::Ogg, true),
            "audio/mpeg" | "audio/mp3" | "audio/x-mp3" | "audio/mpeg3" | "audio/x-mpeg" => {
                (Self::MpegAudio, true)
            }
            "audio/aac" | "audio/aacp" | "audio/x-aac" => (Self::Adts, true),
            "audio/flac" | "audio/x-flac" => (Self::Flac, true),
            "audio/wav" | "audio/wave" | "audio/x-wav" | "audio/vnd.wave" | "audio/x-pn-wav" => {
                (Self::Wav, true)
            }
            _ => return None,
        };
        Some((container, audio_only))
    }

    /// Whether this container may carry `codec`.
    pub fn allows(self, codec: Codec) -> bool {
        use Codec::*;
        match self {
            Self::Mp4 => matches!(
                codec,
                H264 | Hevc | Vp9 | Av1 | Aac | Mp3 | Opus | Flac | Ac3 | Eac3 | Alac
            ),
            Self::WebM => matches!(codec, Vp8 | Vp9 | Av1 | Opus | Vorbis),
            Self::Matroska => !matches!(codec, Alac),
            Self::Ogg => matches!(codec, Opus | Vorbis | Flac | Theora),
            Self::MpegAudio => codec == Mp3,
            Self::Adts => codec == Aac,
            Self::Flac => codec == Flac,
            Self::Wav => codec == Pcm,
        }
    }

    /// Codecs a resource of this type plausibly uses when no `codecs`
    /// parameter says which.
    pub fn implied_codecs(self, audio_only: bool) -> &'static [Codec] {
        use Codec::*;
        match (self, audio_only) {
            (Self::Mp4, false) => &[H264, Av1, Vp9, Hevc, Aac],
            (Self::Mp4, true) => &[Aac, Mp3, Opus, Flac],
            (Self::WebM, false) => &[Vp9, Vp8, Av1, Opus, Vorbis],
            (Self::WebM, true) => &[Opus, Vorbis],
            (Self::Matroska, false) => &[H264, Vp9, Av1, Vp8, Hevc, Aac, Opus, Vorbis, Flac],
            (Self::Matroska, true) => &[Aac, Opus, Vorbis, Flac, Mp3],
            (Self::Ogg, _) => &[Opus, Vorbis, Flac],
            (Self::MpegAudio, _) => &[Mp3],
            (Self::Adts, _) => &[Aac],
            (Self::Flac, _) => &[Flac],
            (Self::Wav, _) => &[Pcm],
        }
    }

    /// Byte stream formats this implementation accepts in a SourceBuffer
    /// (the MSE registry's `webm` and `isobmff` formats, as Firefox).
    pub fn mse(self) -> bool {
        matches!(self, Self::Mp4 | Self::WebM)
    }
}

/// What this process can decode.
pub trait Capabilities {
    /// Whether `codec` (with whatever details the string stated) decodes.
    fn decodes(&self, codec: &CodecString) -> bool;
    /// Whether decoding `codec` is hardware accelerated.
    fn hardware(&self, _codec: &CodecString) -> bool {
        false
    }
}

/// HTML's CanPlayTypeResult.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanPlay {
    No,
    Maybe,
    Probably,
}

impl CanPlay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::No => "",
            Self::Maybe => "maybe",
            Self::Probably => "probably",
        }
    }
}

/// Split a `codecs` parameter into entries (RFC 6381 §3.2).
fn codec_entries(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim)
}

/// The container and parsed codecs of a type string. `None` when the type is
/// one TRust knows it cannot render.
pub fn classify(input: &str) -> Option<(Container, bool, Option<Vec<CodecString>>)> {
    let mime = parse_mime_type(input)?;
    let (container, audio_only) = Container::from_essence(&mime.essence())?;
    let Some(codecs) = mime.parameter("codecs") else {
        return Some((container, audio_only, None));
    };
    let mut parsed = Vec::new();
    for entry in codec_entries(codecs) {
        let codec = codec::parse(entry)?;
        if !container.allows(codec.codec) || (audio_only && codec.codec.is_video()) {
            return None;
        }
        parsed.push(codec);
    }
    if parsed.is_empty() {
        return None;
    }
    Some((container, audio_only, Some(parsed)))
}

/// HTML #dom-navigator-canplaytype.
pub fn can_play_type(input: &str, caps: &dyn Capabilities) -> CanPlay {
    let Some((container, audio_only, codecs)) = classify(input) else {
        return CanPlay::No;
    };
    match codecs {
        None => {
            let plausible = container
                .implied_codecs(audio_only)
                .iter()
                .any(|&codec| caps.decodes(&plain(codec)));
            if plausible {
                CanPlay::Maybe
            } else {
                CanPlay::No
            }
        }
        Some(codecs) => {
            if !codecs.iter().all(|codec| caps.decodes(codec)) {
                return CanPlay::No;
            }
            if codecs.iter().any(|codec| codec.ambiguous) {
                CanPlay::Maybe
            } else {
                CanPlay::Probably
            }
        }
    }
}

/// MSE #dom-mediasource-istypesupported: the byte stream format must be one
/// a SourceBuffer accepts, and every codec named must decode.
pub fn is_type_supported_mse(input: &str, caps: &dyn Capabilities) -> bool {
    let Some((container, audio_only, codecs)) = classify(input) else {
        return false;
    };
    if !container.mse() {
        return false;
    }
    match codecs {
        None => container
            .implied_codecs(audio_only)
            .iter()
            .any(|&codec| caps.decodes(&plain(codec))),
        Some(codecs) => codecs
            .iter()
            .all(|codec| !codec.ambiguous && caps.decodes(codec)),
    }
}

/// Media Capabilities (local w3c/media-capabilities@dbc6ff7)
/// #check-mime-type-validity for a track of `media` ("audio" or "video"):
/// the type is `media` or application; a type that allows a single codec
/// has no parameters, one that allows several has exactly a `codecs`
/// parameter naming one codec.
pub fn valid_capability_type(input: &str, media: &str) -> bool {
    let Some(mime) = parse_mime_type(input) else {
        return false;
    };
    if mime.kind != media && mime.kind != "application" {
        return false;
    }
    match Container::from_essence(&mime.essence()) {
        Some((Container::MpegAudio | Container::Adts | Container::Flac, _)) => {
            mime.parameters.is_empty()
        }
        Some(_) => {
            mime.parameters.len() == 1
                && mime.parameter("codecs").is_some_and(|codecs| {
                    codec_entries(codecs)
                        .filter(|entry| !entry.is_empty())
                        .count()
                        == 1
                })
        }
        // A type TRust does not know is valid and unsupported.
        None => true,
    }
}

/// Media Capabilities' answer for one track (#media-capabilities-info).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodingInfo {
    pub supported: bool,
    pub smooth: bool,
    pub power_efficient: bool,
}

/// Decoded pixels per second software decoding (and presenting) keeps up
/// with: 1080p at 60 frames per second, and 30 for AV1 and HEVC.
const SOFTWARE_PIXEL_RATE: f64 = 1920.0 * 1080.0 * 60.0;
const SOFTWARE_PIXEL_RATE_HEAVY: f64 = 1920.0 * 1080.0 * 30.0;

/// #create-media-capabilities-decoding-info for one valid track
/// configuration: `media_source` for type "media-source" (else "file");
/// `video` is the width, height and framerate of a video track.
pub fn decoding_info(
    input: &str,
    media_source: bool,
    video: Option<(u32, u32, f64)>,
    caps: &dyn Capabilities,
) -> DecodingInfo {
    let supported = if media_source {
        is_type_supported_mse(input, caps)
    } else {
        can_play_type(input, caps) != CanPlay::No
    };
    if !supported {
        return DecodingInfo::default();
    }
    let codec = classify(input).and_then(|(container, audio_only, codecs)| match codecs {
        Some(codecs) => codecs.into_iter().next(),
        None => container
            .implied_codecs(audio_only)
            .first()
            .map(|&codec| plain(codec)),
    });
    let Some((width, height, framerate)) = video else {
        // Audio decodes cheaply in software.
        return DecodingInfo {
            supported,
            smooth: true,
            power_efficient: true,
        };
    };
    let hardware = codec.as_ref().is_some_and(|codec| caps.hardware(codec));
    let budget = match codec.map(|codec| codec.codec) {
        Some(Codec::Av1 | Codec::Hevc) => SOFTWARE_PIXEL_RATE_HEAVY,
        _ => SOFTWARE_PIXEL_RATE,
    };
    let rate = f64::from(width) * f64::from(height) * framerate;
    DecodingInfo {
        supported,
        smooth: hardware || rate <= budget,
        power_efficient: hardware,
    }
}

fn plain(codec: Codec) -> CodecString {
    CodecString {
        codec,
        details: Default::default(),
        ambiguous: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Free;
    impl Capabilities for Free {
        fn decodes(&self, codec: &CodecString) -> bool {
            matches!(
                codec.codec,
                Codec::Av1
                    | Codec::Vp9
                    | Codec::Opus
                    | Codec::Vorbis
                    | Codec::Flac
                    | Codec::Mp3
                    | Codec::Pcm
            ) && codec.details.bit_depth.is_none_or(|depth| depth <= 10)
        }
    }

    struct WithSystem;
    impl Capabilities for WithSystem {
        fn decodes(&self, codec: &CodecString) -> bool {
            Free.decodes(codec) || matches!(codec.codec, Codec::H264 | Codec::Aac)
        }
    }

    #[test]
    fn mime_parsing() {
        let mime = parse_mime_type(" Video/MP4 ; codecs=\"avc1.42E01E, mp4a.40.2\" ").unwrap();
        assert_eq!(mime.essence(), "video/mp4");
        assert_eq!(mime.parameter("codecs"), Some("avc1.42E01E, mp4a.40.2"));
        let mime = parse_mime_type("text/html;charset=\"shift_jis\"iso-2022-jp").unwrap();
        assert_eq!(mime.parameter("charset"), Some("shift_jis"));
        let mime = parse_mime_type("a/b;x=1;x=2;;y").unwrap();
        assert_eq!(
            mime.parameters,
            vec![(String::from("x"), String::from("1"))]
        );
        assert!(parse_mime_type("video").is_none());
        assert!(parse_mime_type("/mp4").is_none());
        assert!(parse_mime_type("vid eo/mp4").is_none());
        let escaped = parse_mime_type("a/b; q=\"x\\\"y\"").unwrap();
        assert_eq!(escaped.parameter("q"), Some("x\"y"));
    }

    #[test]
    fn can_play_type_follows_capabilities() {
        assert_eq!(
            can_play_type("video/webm; codecs=\"vp9, opus\"", &Free),
            CanPlay::Probably
        );
        assert_eq!(
            can_play_type("video/webm; codecs=\"vp8, vorbis\"", &Free),
            CanPlay::No
        );
        assert_eq!(can_play_type("video/webm", &Free), CanPlay::Maybe);
        assert_eq!(
            can_play_type("video/mp4; codecs=\"avc1.42E01E\"", &Free),
            CanPlay::No
        );
        assert_eq!(
            can_play_type("video/mp4; codecs=\"avc1.42E01E\"", &WithSystem),
            CanPlay::Probably
        );
        assert_eq!(
            can_play_type("video/mp4; codecs=avc1", &WithSystem),
            CanPlay::Maybe
        );
        // AV1 alone makes bare video/mp4 plausible (as in Firefox).
        assert_eq!(can_play_type("video/mp4", &Free), CanPlay::Maybe);
        assert_eq!(can_play_type("audio/mp4", &Free), CanPlay::Maybe);
        assert_eq!(can_play_type("audio/aac", &Free), CanPlay::No);
        assert_eq!(can_play_type("audio/mpeg", &Free), CanPlay::Maybe);
        assert_eq!(
            can_play_type("audio/ogg; codecs=opus", &Free),
            CanPlay::Probably
        );
        assert_eq!(
            can_play_type("video/ogg; codecs=theora", &Free),
            CanPlay::No
        );
        // Audio types cannot carry video codecs.
        assert_eq!(can_play_type("audio/webm; codecs=vp9", &Free), CanPlay::No);
        assert_eq!(
            can_play_type("application/octet-stream", &Free),
            CanPlay::No
        );
        assert_eq!(
            can_play_type("video/x-new-fictional-format", &Free),
            CanPlay::No
        );
        assert_eq!(
            can_play_type("video/webm; codecs=\"vp09.02.10.12\"", &Free),
            CanPlay::No
        );
        assert_eq!(
            can_play_type("audio/wav; codecs=1", &Free),
            CanPlay::Probably
        );
        assert_eq!(can_play_type("video/mp4; codecs=\"\"", &Free), CanPlay::No);
    }

    #[test]
    fn mse_types() {
        assert!(is_type_supported_mse(
            "video/webm; codecs=\"vp09.00.10.08\"",
            &Free
        ));
        assert!(is_type_supported_mse(
            "video/mp4; codecs=\"av01.0.05M.08\"",
            &Free
        ));
        assert!(is_type_supported_mse("audio/webm; codecs=opus", &Free));
        assert!(is_type_supported_mse("video/mp4", &Free));
        assert!(!is_type_supported_mse(
            "video/mp4; codecs=\"avc1.4d401f\"",
            &Free
        ));
        assert!(is_type_supported_mse(
            "video/mp4; codecs=\"avc1.4d401f,mp4a.40.2\"",
            &WithSystem
        ));
        assert!(!is_type_supported_mse(
            "video/mp4; codecs=avc1",
            &WithSystem
        ));
        assert!(!is_type_supported_mse("audio/ogg; codecs=opus", &Free));
        assert!(!is_type_supported_mse("audio/mpeg", &Free));
        assert!(!is_type_supported_mse("", &Free));
    }

    #[test]
    fn media_capabilities_mime_validity() {
        assert!(valid_capability_type(
            "video/webm; codecs=\"vp09.00.10.08\"",
            "video"
        ));
        assert!(valid_capability_type(
            "application/mp4; codecs=avc1.4d401f",
            "video"
        ));
        // Containers that carry several codecs name exactly one.
        assert!(!valid_capability_type("video/mp4", "video"));
        assert!(!valid_capability_type(
            "video/mp4; codecs=\"avc1.4d401f,mp4a.40.2\"",
            "video"
        ));
        assert!(!valid_capability_type(
            "video/webm; codecs=vp9; profile=0",
            "video"
        ));
        // A single-codec type takes no parameters.
        assert!(valid_capability_type("audio/mpeg", "audio"));
        assert!(!valid_capability_type("audio/mpeg; codecs=mp3", "audio"));
        assert!(!valid_capability_type("audio/webm; codecs=opus", "video"));
        assert!(!valid_capability_type("not a type", "audio"));
    }

    #[test]
    fn media_capabilities_decoding_info() {
        let vp9 = "video/webm; codecs=\"vp09.00.10.08\"";
        let info = decoding_info(vp9, true, Some((1920, 1080, 30.0)), &Free);
        assert_eq!(
            info,
            DecodingInfo {
                supported: true,
                smooth: true,
                power_efficient: false
            }
        );
        // 4K60 is beyond software decoding.
        assert!(!decoding_info(vp9, true, Some((3840, 2160, 60.0)), &Free).smooth);
        assert!(
            !decoding_info(
                "video/mp4; codecs=avc1.4d401f",
                true,
                Some((640, 360, 30.0)),
                &Free
            )
            .supported
        );
        // Ogg is not an MSE byte stream format, but plays from a file.
        assert!(!decoding_info("audio/ogg; codecs=opus", true, None, &Free).supported);
        let audio = decoding_info("audio/ogg; codecs=opus", false, None, &Free);
        assert_eq!(
            audio,
            DecodingInfo {
                supported: true,
                smooth: true,
                power_efficient: true
            }
        );
    }
}
