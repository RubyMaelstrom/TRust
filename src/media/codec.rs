//! Codec identification from RFC 6381 `codecs` parameter strings and from
//! container sample descriptions.
//!
//! The string forms follow the WebCodecs Codec Registry registrations (local
//! w3c/webcodecs snapshot db320a8, `*_codec_registration.src.html`), which
//! defer to RFC 6381 §3.3–3.4 (`avc1`/`mp4a`), ISO/IEC 14496-15 Annex E
//! (`hev1`/`hvc1`), the VP9 ISOBMFF binding "Codecs Parameter String"
//! (`vp09`) and the AV1 ISOBMFF binding §5 (`av01`). Legacy short forms that
//! browsers accept in WebM and Ogg (`vp8`, `vp9`, `opus`, `vorbis`, ...) are
//! recognised too.

/// A coding format TRust can identify. Whether it can also be decoded is the
/// decoder registry's question.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    Av1,
    Vp9,
    Vp8,
    H264,
    Hevc,
    Theora,
    Aac,
    Mp3,
    Opus,
    Vorbis,
    Flac,
    /// Linear PCM (WAV format tag 1 and its float/µ-law/A-law relatives).
    Pcm,
    Alac,
    Ac3,
    Eac3,
}

impl Codec {
    pub fn is_video(self) -> bool {
        matches!(
            self,
            Self::Av1 | Self::Vp9 | Self::Vp8 | Self::H264 | Self::Hevc | Self::Theora
        )
    }

    pub fn is_audio(self) -> bool {
        !self.is_video()
    }

    /// A short lowercase name for diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
            Self::Vp8 => "vp8",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Theora => "theora",
            Self::Aac => "aac",
            Self::Mp3 => "mp3",
            Self::Opus => "opus",
            Self::Vorbis => "vorbis",
            Self::Flac => "flac",
            Self::Pcm => "pcm",
            Self::Alac => "alac",
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
        }
    }
}

/// What a codec string (or a sample description) says about the stream,
/// beyond the coding format itself. `None` fields were not stated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodecDetails {
    /// Format profile: H.264 `profile_idc`, HEVC `general_profile_idc`, VP9
    /// profile 0–3, AV1 `seq_profile` 0–2.
    pub profile: Option<u8>,
    /// Format level (H.264 `level_idc`, HEVC `general_level_idc`, VP9 level
    /// ×10, AV1 `seq_level_idx`).
    pub level: Option<u8>,
    /// Luma bit depth.
    pub bit_depth: Option<u8>,
    /// Chroma subsampling as (horizontal, vertical) shifts; (0, 0) is 4:4:4.
    pub chroma_shift: Option<(u8, u8)>,
    /// MPEG-4 Audio Object Type (2 = AAC LC, 5 = SBR, 29 = PS, 42 = USAC).
    pub audio_object_type: Option<u8>,
}

/// A parsed `codecs` parameter entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecString {
    pub codec: Codec,
    pub details: CodecDetails,
    /// The string names its format without the parameters that make it
    /// unambiguous (`avc1`, `mp4a`, `mp4a.40`): HTML's canPlayType should
    /// then not answer "probably".
    pub ambiguous: bool,
}

impl CodecString {
    fn plain(codec: Codec) -> Self {
        Self {
            codec,
            details: CodecDetails::default(),
            ambiguous: false,
        }
    }

    fn ambiguous(codec: Codec) -> Self {
        Self {
            codec,
            details: CodecDetails::default(),
            ambiguous: true,
        }
    }
}

/// Parse one entry of a `codecs` parameter (RFC 6381 §3.2: entries are
/// separated by commas; surrounding whitespace is not part of an entry).
/// `None` for strings TRust does not recognise or that are malformed.
pub fn parse(entry: &str) -> Option<CodecString> {
    let entry = entry.trim();
    let (fourcc, rest) = match entry.split_once('.') {
        Some((fourcc, rest)) => (fourcc, Some(rest)),
        None => (entry, None),
    };
    match fourcc {
        "avc1" | "avc3" => Some(rest.map_or(Some(CodecString::ambiguous(Codec::H264)), parse_avc)?),
        "hev1" | "hvc1" => {
            Some(rest.map_or(Some(CodecString::ambiguous(Codec::Hevc)), parse_hevc)?)
        }
        "vp09" => parse_vp09(rest?),
        "av01" => parse_av01(rest?),
        "mp4a" => match rest {
            None => Some(CodecString::ambiguous(Codec::Aac)),
            Some(rest) => parse_mp4a(rest),
        },
        _ if rest.is_none() => parse_short(fourcc),
        // `vp8.0`/`vp9.0` (the WebM forms Chrome and Firefox accept).
        "vp8" | "vp9" if rest == Some("0") => parse_short(fourcc),
        _ => None,
    }
}

/// Bare names: RFC 6381 sample-entry four-character codes used without
/// parameters, and the conventional WebM/Ogg names.
fn parse_short(name: &str) -> Option<CodecString> {
    let codec = match name {
        "vp8" | "vp08" => Codec::Vp8,
        "vp9" => {
            // The legacy WebM spelling means profile 0, 8-bit.
            return Some(CodecString {
                codec: Codec::Vp9,
                details: CodecDetails {
                    profile: Some(0),
                    bit_depth: Some(8),
                    ..Default::default()
                },
                ambiguous: false,
            });
        }
        "theora" => Codec::Theora,
        "opus" | "Opus" => Codec::Opus,
        "vorbis" => Codec::Vorbis,
        "flac" | "fLaC" => Codec::Flac,
        "mp3" | ".mp3" => Codec::Mp3,
        "alac" => Codec::Alac,
        "ac-3" => Codec::Ac3,
        "ec-3" => Codec::Eac3,
        // WAVE format tags (RFC 2361): 1 = PCM, 3 = IEEE float, 6 = A-law,
        // 7 = µ-law.
        "1" | "3" | "6" | "7" => Codec::Pcm,
        _ => return None,
    };
    Some(CodecString::plain(codec))
}

/// RFC 6381 §3.3: `avc1.PPCCLL`, three hex bytes (profile_idc,
/// constraint_set flags, level_idc).
fn parse_avc(rest: &str) -> Option<CodecString> {
    if rest.len() != 6 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let profile = u8::from_str_radix(&rest[0..2], 16).ok()?;
    let level = u8::from_str_radix(&rest[4..6], 16).ok()?;
    if profile == 0 {
        return None;
    }
    // H.264 Annex A: High 10 and up carry more than 8 bits per sample; the
    // High 4:2:2/4:4:4 profiles carry other chroma formats.
    let bit_depth = match profile {
        110 | 122 | 244 => None,
        _ => Some(8),
    };
    Some(CodecString {
        codec: Codec::H264,
        details: CodecDetails {
            profile: Some(profile),
            level: Some(level),
            bit_depth,
            chroma_shift: match profile {
                122 => Some((1, 0)),
                244 => None,
                _ => Some((1, 1)),
            },
            audio_object_type: None,
        },
        ambiguous: false,
    })
}

/// ISO/IEC 14496-15 Annex E.3: `hvc1.[A-C]?P.F.(L|H)LV.C1[.C2…C6]`.
fn parse_hevc(rest: &str) -> Option<CodecString> {
    let mut fields = rest.split('.');
    let profile_field = fields.next()?;
    let profile_digits = profile_field.trim_start_matches(['A', 'B', 'C']);
    if profile_field.len() - profile_digits.len() > 1 {
        return None;
    }
    let profile: u8 = profile_digits.parse().ok()?;
    let compatibility = fields.next()?;
    if compatibility.is_empty()
        || compatibility.len() > 8
        || !compatibility.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let tier_level = fields.next()?;
    let level: u8 = tier_level
        .strip_prefix('L')
        .or_else(|| tier_level.strip_prefix('H'))?
        .parse()
        .ok()?;
    let mut constraints = 0;
    for constraint in fields {
        constraints += 1;
        if constraints > 6
            || constraint.is_empty()
            || constraint.len() > 2
            || !constraint.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
    }
    // H.265 Annex A: profile 1 Main (8-bit), 2 Main 10, 3 Main Still Picture.
    let bit_depth = match profile {
        1 | 3 => Some(8),
        2 => Some(10),
        _ => None,
    };
    Some(CodecString {
        codec: Codec::Hevc,
        details: CodecDetails {
            profile: Some(profile),
            level: Some(level),
            bit_depth,
            chroma_shift: matches!(profile, 1..=3).then_some((1, 1)),
            audio_object_type: None,
        },
        ambiguous: false,
    })
}

/// Two-digit decimal field of the VP9 and AV1 codec strings.
fn two_digits(field: &str) -> Option<u8> {
    (field.len() == 2 && field.bytes().all(|b| b.is_ascii_digit()))
        .then(|| field.parse().ok())
        .flatten()
}

/// VP9 ISOBMFF binding, "Codecs Parameter String":
/// `vp09.PP.LL.DD[.CC.cp.tc.mc.FF]`. The three mandatory fields are profile,
/// level and bit depth; the optional fields come as a group.
fn parse_vp09(rest: &str) -> Option<CodecString> {
    let fields: Vec<&str> = rest.split('.').collect();
    if !(fields.len() == 3 || (4..=8).contains(&fields.len())) {
        return None;
    }
    let profile = two_digits(fields[0])?;
    let level = two_digits(fields[1])?;
    let bit_depth = two_digits(fields[2])?;
    if profile > 3
        || !matches!(
            level,
            10 | 11 | 20 | 21 | 30 | 31 | 40 | 41 | 50 | 51 | 52 | 60 | 61 | 62
        )
        || !matches!(bit_depth, 8 | 10 | 12)
        || (profile < 2 && bit_depth != 8)
        || (profile >= 2 && bit_depth == 8)
    {
        return None;
    }
    let mut chroma_shift = None;
    if let Some(field) = fields.get(3) {
        chroma_shift = Some(match two_digits(field)? {
            0 | 1 => (1, 1),
            2 => (1, 0),
            3 => (0, 0),
            _ => return None,
        });
        // Profiles 0 and 2 are 4:2:0 only; 1 and 3 carry the others.
        if (profile % 2 == 0) != matches!(chroma_shift, Some((1, 1))) {
            return None;
        }
        for (index, field) in fields.iter().enumerate().skip(4) {
            let value = two_digits(field)?;
            if index == 7 && value > 1 {
                return None;
            }
        }
    }
    Some(CodecString {
        codec: Codec::Vp9,
        details: CodecDetails {
            profile: Some(profile),
            level: Some(level),
            bit_depth: Some(bit_depth),
            // Without the optional fields, even profiles are 4:2:0; odd
            // profiles leave the subsampling unstated.
            chroma_shift: chroma_shift.or((profile % 2 == 0).then_some((1, 1))),
            audio_object_type: None,
        },
        ambiguous: false,
    })
}

/// AV1 ISOBMFF binding §5: `av01.P.LLT.DD[.M.CCC.cp.tc.mc.F]`.
fn parse_av01(rest: &str) -> Option<CodecString> {
    let fields: Vec<&str> = rest.split('.').collect();
    // The optional fields come as a group (§5): 3 or 9 fields.
    if !(fields.len() == 3 || fields.len() == 9) {
        return None;
    }
    let profile: u8 = (fields[0].len() == 1)
        .then(|| fields[0].parse().ok())
        .flatten()?;
    let level_tier = fields[1];
    if level_tier.len() != 3 || !matches!(level_tier.as_bytes()[2], b'M' | b'H') {
        return None;
    }
    let level = two_digits(&level_tier[..2])?;
    let bit_depth = two_digits(fields[2])?;
    if profile > 2 || level > 31 || !matches!(bit_depth, 8 | 10 | 12) {
        return None;
    }
    let mut chroma_shift = None;
    if fields.len() == 9 {
        if !matches!(fields[3], "0" | "1") || !matches!(fields[8], "0" | "1") {
            return None;
        }
        let subsampling = fields[4];
        if subsampling.len() != 3
            || !subsampling
                .bytes()
                .all(|b| matches!(b, b'0' | b'1' | b'2' | b'3'))
        {
            return None;
        }
        let bytes = subsampling.as_bytes();
        chroma_shift = Some((bytes[0] - b'0', bytes[1] - b'0'));
        for field in &fields[5..8] {
            two_digits(field)?;
        }
    }
    Some(CodecString {
        codec: Codec::Av1,
        details: CodecDetails {
            profile: Some(profile),
            level: Some(level),
            bit_depth: Some(bit_depth),
            chroma_shift,
            audio_object_type: None,
        },
        ambiguous: false,
    })
}

/// RFC 6381 §3.3: `mp4a.OO[.A]`, where OO is the hexadecimal MPEG-4 Systems
/// ObjectTypeIndication and, for 0x40 (MPEG-4 Audio), A is the decimal Audio
/// Object Type.
fn parse_mp4a(rest: &str) -> Option<CodecString> {
    let (oti, aot) = match rest.split_once('.') {
        Some((oti, aot)) => (oti, Some(aot)),
        None => (rest, None),
    };
    if oti.len() != 2 || !oti.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let oti = u8::from_str_radix(oti, 16).ok()?;
    match (oti, aot) {
        (0x40, None) => Some(CodecString::ambiguous(Codec::Aac)),
        (0x40, Some(aot)) => {
            if aot.is_empty() || aot.len() > 2 || !aot.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let aot: u8 = aot.parse().ok()?;
            match aot {
                // MPEG-1/2 Layer 1–3 carried as MPEG-4 Audio Object Types.
                32..=34 => Some(CodecString::plain(Codec::Mp3)),
                1..=6 | 17 | 22 | 23 | 29 | 39 | 42 => Some(CodecString {
                    codec: Codec::Aac,
                    details: CodecDetails {
                        audio_object_type: Some(aot),
                        ..Default::default()
                    },
                    ambiguous: false,
                }),
                _ => None,
            }
        }
        // MPEG-2 AAC Main/LC/SSR: report the corresponding MPEG-4 AOT.
        (0x66..=0x68, None) => Some(CodecString {
            codec: Codec::Aac,
            details: CodecDetails {
                audio_object_type: Some(oti - 0x65),
                ..Default::default()
            },
            ambiguous: false,
        }),
        (0x69 | 0x6B, None) => Some(CodecString::plain(Codec::Mp3)),
        (0xA5, None) => Some(CodecString::plain(Codec::Ac3)),
        (0xA6, None) => Some(CodecString::plain(Codec::Eac3)),
        (0xAD, None) => Some(CodecString::plain(Codec::Opus)),
        _ => None,
    }
}

/// The WebCodecs-style string for a track, used for MediaCapabilities and
/// diagnostics when a container did not provide one.
pub fn describe(codec: Codec, details: &CodecDetails) -> String {
    match codec {
        Codec::Vp9 => format!(
            "vp09.{:02}.{:02}.{:02}",
            details.profile.unwrap_or(0),
            details.level.unwrap_or(10),
            details.bit_depth.unwrap_or(8)
        ),
        Codec::Av1 => format!(
            "av01.{}.{:02}M.{:02}",
            details.profile.unwrap_or(0),
            details.level.unwrap_or(0),
            details.bit_depth.unwrap_or(8)
        ),
        Codec::H264 => format!(
            "avc1.{:02X}00{:02X}",
            details.profile.unwrap_or(66),
            details.level.unwrap_or(30)
        ),
        Codec::Hevc => format!(
            "hvc1.{}.6.L{}.B0",
            details.profile.unwrap_or(1),
            details.level.unwrap_or(93)
        ),
        Codec::Aac => format!("mp4a.40.{}", details.audio_object_type.unwrap_or(2)),
        Codec::Mp3 => String::from("mp3"),
        Codec::Opus => String::from("opus"),
        Codec::Vorbis => String::from("vorbis"),
        Codec::Flac => String::from("flac"),
        Codec::Vp8 => String::from("vp8"),
        Codec::Theora => String::from("theora"),
        Codec::Pcm => String::from("1"),
        Codec::Alac => String::from("alac"),
        Codec::Ac3 => String::from("ac-3"),
        Codec::Eac3 => String::from("ec-3"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avc_strings() {
        let high = parse("avc1.640028").unwrap();
        assert_eq!(high.codec, Codec::H264);
        assert_eq!(high.details.profile, Some(100));
        assert_eq!(high.details.level, Some(0x28));
        assert!(!high.ambiguous);
        assert_eq!(parse("avc3.42E01E").unwrap().details.profile, Some(66));
        assert!(parse("avc1").unwrap().ambiguous);
        assert!(parse("avc1.64002").is_none());
        assert!(parse("avc1.6400zz").is_none());
    }

    #[test]
    fn hevc_strings() {
        let main = parse("hvc1.1.6.L93.B0").unwrap();
        assert_eq!(main.codec, Codec::Hevc);
        assert_eq!(main.details.profile, Some(1));
        assert_eq!(main.details.level, Some(93));
        let main10 = parse("hev1.2.4.H120.B0.00").unwrap();
        assert_eq!(main10.details.bit_depth, Some(10));
        assert!(parse("hvc1.A1.6.L93").is_some());
        assert!(parse("hvc1.1.6.X93.B0").is_none());
        assert!(parse("hvc1.1.6.L93.B0.0.0.0.0.0.0").is_none());
    }

    #[test]
    fn vp9_strings() {
        let basic = parse("vp09.00.10.08").unwrap();
        assert_eq!(basic.codec, Codec::Vp9);
        assert_eq!(basic.details.profile, Some(0));
        assert_eq!(basic.details.bit_depth, Some(8));
        let hdr = parse("vp09.02.10.10.01.09.16.09.01").unwrap();
        assert_eq!(hdr.details.bit_depth, Some(10));
        assert_eq!(hdr.details.profile, Some(2));
        // Profile and bit depth must agree; levels come from a fixed set.
        assert!(parse("vp09.00.10.10").is_none());
        assert!(parse("vp09.00.12.08").is_none());
        assert!(parse("vp09.00.10").is_none());
        assert!(parse("vp9").is_some());
        assert!(parse("vp9.0").is_some());
        assert!(parse("vp8").is_some());
    }

    #[test]
    fn av1_strings() {
        let main = parse("av01.0.04M.08").unwrap();
        assert_eq!(main.codec, Codec::Av1);
        assert_eq!(main.details.level, Some(4));
        let full = parse("av01.0.04M.10.0.112.09.16.09.0").unwrap();
        assert_eq!(full.details.bit_depth, Some(10));
        assert_eq!(full.details.chroma_shift, Some((1, 1)));
        assert!(parse("av01.0.04X.08").is_none());
        assert!(parse("av01.3.04M.08").is_none());
        assert!(parse("av01.0.04M.08.0").is_none());
    }

    #[test]
    fn audio_strings() {
        assert_eq!(
            parse("mp4a.40.2").unwrap().details.audio_object_type,
            Some(2)
        );
        assert_eq!(
            parse("mp4a.40.02").unwrap().details.audio_object_type,
            Some(2)
        );
        assert_eq!(parse("mp4a.40.29").unwrap().codec, Codec::Aac);
        assert_eq!(parse("mp4a.6B").unwrap().codec, Codec::Mp3);
        assert_eq!(parse("mp4a.69").unwrap().codec, Codec::Mp3);
        assert_eq!(parse("mp4a.40.34").unwrap().codec, Codec::Mp3);
        assert_eq!(parse("mp4a.67").unwrap().details.audio_object_type, Some(2));
        assert!(parse("mp4a.40").unwrap().ambiguous);
        assert!(parse("mp4a.40.99").is_none());
        assert_eq!(parse("opus").unwrap().codec, Codec::Opus);
        assert_eq!(parse("Opus").unwrap().codec, Codec::Opus);
        assert_eq!(parse("fLaC").unwrap().codec, Codec::Flac);
        assert_eq!(parse(" vorbis ").unwrap().codec, Codec::Vorbis);
        assert_eq!(parse("1").unwrap().codec, Codec::Pcm);
        assert!(parse("dvh1.05.06").is_none());
        assert!(parse("").is_none());
    }
}
