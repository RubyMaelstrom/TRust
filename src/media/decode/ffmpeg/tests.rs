//! Tests against a real FFmpeg.
//!
//! The streams in `testdata/` are synthetic (see `testdata/generate.sh`);
//! each video stream's `.md5` is `ffmpeg -f framemd5` of the stream decoded
//! by the FFmpeg command line tool. Tests skip, saying so, when no FFmpeg
//! (or no decoder for the stream) is installed. `TRUST_FFMPEG_LIB` selects
//! the library under test.

use std::collections::HashSet;
use std::ffi::{CStr, c_int};
use std::sync::Arc;

use super::abi::FrameFields;
use super::audio::convert as convert_audio;
use super::library::{Settings, load_with};
use super::video::convert as convert_video;
use super::*;

// ---------------------------------------------------------------------------
// Fixtures and helpers
// ---------------------------------------------------------------------------

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!("testdata/", $name)).as_slice()
    };
}

macro_rules! expected_md5 {
    ($name:literal) => {
        include_str!(concat!("testdata/", $name, ".md5"))
    };
}

/// The library under test, or `None` (after saying so) to skip.
fn library() -> Option<Arc<Library>> {
    let lib = Library::get();
    if lib.is_none() {
        eprintln!("skipped: no FFmpeg ({:?})", Library::status().err());
    }
    lib
}

/// The library, if it has a decoder for `codec`.
fn library_with(codec: CodecId) -> Option<Arc<Library>> {
    let lib = library()?;
    if !lib.has_decoder(codec) {
        eprintln!("skipped: this FFmpeg has no {codec:?} decoder");
        return None;
    }
    Some(lib)
}

/// RFC 1321.
fn md5(data: &[u8]) -> String {
    const SHIFTS: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    let constants: Vec<u32> = (1..=64u32)
        .map(|i| ((f64::from(i).sin().abs()) * 4294967296.0) as u32)
        .collect();
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64) * 8).to_le_bytes());
    for block in message.as_chunks::<64>().0 {
        let words: Vec<u32> = block
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word))
            .collect();
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(constants[i])
                .wrapping_add(words[g])
                .rotate_left(SHIFTS[(i / 16) * 4 + i % 4]);
            (a, d, c) = (d, c, b);
            b = b.wrapping_add(rotated);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d]) {
            *slot = slot.wrapping_add(value);
        }
    }
    state
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn md5_matches_rfc_1321_vectors() {
    assert_eq!(md5(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(
        md5(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"),
        "57edf4a22be3c955ac49da2e2107b67a"
    );
}

fn plane_len(plane: &Plane) -> usize {
    match &plane.data {
        PlaneData::U8(samples) => samples.len(),
        PlaneData::U16(samples) => samples.len(),
    }
}

/// The hash `ffmpeg -f framemd5` prints for a picture: its planes, tightly
/// packed, 16-bit samples little-endian.
fn picture_md5(frame: &VideoFrame) -> String {
    let mut bytes = Vec::new();
    for plane in &frame.planes {
        assert_eq!(plane.stride, plane.width);
        match &plane.data {
            PlaneData::U8(samples) => bytes.extend_from_slice(samples),
            PlaneData::U16(samples) => {
                bytes.extend(samples.iter().flat_map(|sample| sample.to_le_bytes()))
            }
        }
        assert_eq!(plane_len(plane), plane.width * plane.height);
    }
    md5(&bytes)
}

fn framemd5_hashes(listing: &str) -> Vec<String> {
    listing
        .lines()
        .map(|line| line.rsplit(',').next().unwrap().trim().to_owned())
        .collect()
}

// -- Stream splitters --------------------------------------------------------

/// Start-code positions: (start of the code including a leading zero byte,
/// first byte after it).
fn start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            found.push((if i > 0 && data[i - 1] == 0 { i - 1 } else { i }, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    found
}

#[derive(PartialEq)]
enum Nal {
    /// A parameter set, SEI or delimiter that precedes a picture.
    Leading,
    FirstSlice,
    Slice,
    Other,
}

/// Splits an Annex B stream into access units (each starting at its first
/// leading NAL unit or first slice), given the codec's NAL classification.
fn access_units(data: &[u8], classify: fn(&[u8]) -> Nal) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    let mut unit_start = 0;
    let mut seen_slice = false;
    for (start, payload) in start_codes(data) {
        match classify(&data[payload..]) {
            Nal::Leading if seen_slice => {
                units.push(data[unit_start..start].to_vec());
                (unit_start, seen_slice) = (start, false);
            }
            Nal::FirstSlice if seen_slice => {
                units.push(data[unit_start..start].to_vec());
                unit_start = start;
            }
            Nal::FirstSlice | Nal::Slice => seen_slice = true,
            Nal::Leading | Nal::Other => {}
        }
    }
    units.push(data[unit_start..].to_vec());
    units
}

fn classify_h264(nal: &[u8]) -> Nal {
    match nal[0] & 0x1f {
        1 | 5 if nal[1] & 0x80 != 0 => Nal::FirstSlice,
        1 | 5 => Nal::Slice,
        6..=9 => Nal::Leading,
        _ => Nal::Other,
    }
}

fn classify_hevc(nal: &[u8]) -> Nal {
    match (nal[0] >> 1) & 0x3f {
        0..=31 if nal[2] & 0x80 != 0 => Nal::FirstSlice,
        0..=31 => Nal::Slice,
        32..=35 | 39 => Nal::Leading,
        _ => Nal::Other,
    }
}

fn h264_units(data: &[u8]) -> Vec<Vec<u8>> {
    access_units(data, classify_h264)
}

fn hevc_units(data: &[u8]) -> Vec<Vec<u8>> {
    access_units(data, classify_hevc)
}

fn ivf_frames(data: &[u8]) -> Vec<Vec<u8>> {
    assert_eq!(&data[..4], b"DKIF");
    let mut frames = Vec::new();
    let mut at = usize::from(u16::from_le_bytes([data[6], data[7]]));
    while at + 12 <= data.len() {
        let size = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        frames.push(data[at + 12..at + 12 + size].to_vec());
        at += 12 + size;
    }
    frames
}

/// Splits a stream of self-describing frames; `length` reads a frame's size
/// from its first bytes.
fn split_frames(data: &[u8], length: fn(&[u8]) -> usize) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let size = length(&data[at..]);
        frames.push(data[at..at + size].to_vec());
        at += size;
    }
    frames
}

fn adts_length(frame: &[u8]) -> usize {
    assert!(frame[0] == 0xff && frame[1] & 0xf0 == 0xf0, "ADTS sync");
    (usize::from(frame[3] & 3) << 11) | (usize::from(frame[4]) << 3) | usize::from(frame[5] >> 5)
}

/// AC-3 at 48 kHz only (what the fixture uses): A/52 table 4.13.
fn ac3_length(frame: &[u8]) -> usize {
    const WORDS: [usize; 19] = [
        64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 640, 768, 896, 1024, 1152,
        1280,
    ];
    assert_eq!(frame[..2], [0x0b, 0x77]);
    assert_eq!(frame[4] >> 6, 0, "48 kHz");
    WORDS[usize::from(frame[4] & 0x3f) / 2] * 2
}

fn eac3_length(frame: &[u8]) -> usize {
    assert_eq!(frame[..2], [0x0b, 0x77]);
    ((usize::from(frame[2] & 7) << 8 | usize::from(frame[3])) + 1) * 2
}

/// MPEG-1 Layer III, no padding tricks beyond the header's bit.
fn mp3_length(frame: &[u8]) -> usize {
    const KBPS: [usize; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const HZ: [usize; 3] = [44100, 48000, 32000];
    assert!(
        frame[0] == 0xff && frame[1] & 0xfe == 0xfa,
        "MPEG-1 Layer III sync"
    );
    let kbps = KBPS[usize::from(frame[2] >> 4)];
    let hz = HZ[usize::from((frame[2] >> 2) & 3)];
    144 * kbps * 1000 / hz + usize::from((frame[2] >> 1) & 1)
}

/// The two-byte AudioSpecificConfig of AAC-LC at 48 kHz, stereo.
const AAC_LC_48K_STEREO: [u8; 2] = [0x11, 0x90];

/// An ADTS frame's raw AAC payload.
fn adts_payload(frame: &[u8]) -> &[u8] {
    &frame[if frame[1] & 1 == 1 { 7 } else { 9 }..]
}

// -- Decoding helpers --------------------------------------------------------

/// The frames of a `drain()` that decoded cleanly.
fn drained<T>(outputs: Vec<Result<T, Error>>) -> Vec<T> {
    outputs
        .into_iter()
        .map(|output| output.expect("drained frame"))
        .collect()
}

/// Decodes `units` the way a player loop does: send, receive what is ready,
/// and drain at the end. Packet `i` carries pts `i * 1000`.
fn decode_video(
    lib: &Arc<Library>,
    codec: CodecId,
    config: VideoConfig,
    units: &[Vec<u8>],
) -> Vec<VideoFrame> {
    let mut decoder = VideoDecoder::new(lib, codec, config).expect("decoder opens");
    let mut frames = Vec::new();
    for (i, unit) in units.iter().enumerate() {
        decoder.send(unit, i as i64 * 1000, i == 0).expect("send");
        while let Some(frame) = decoder.receive().expect("receive") {
            frames.push(frame);
        }
    }
    frames.extend(drained(decoder.drain()));
    assert!(decoder.receive().unwrap().is_none());
    frames
}

fn decode_audio(
    lib: &Arc<Library>,
    codec: CodecId,
    config: AudioConfig,
    packets: &[Vec<u8>],
) -> Vec<AudioFrame> {
    let mut decoder = AudioDecoder::new(lib, codec, config).expect("decoder opens");
    let mut frames = Vec::new();
    for (i, packet) in packets.iter().enumerate() {
        decoder.send(packet, i as i64 * 1000, true).expect("send");
        while let Some(frame) = decoder.receive().expect("receive") {
            frames.push(frame);
        }
    }
    frames.extend(drained(decoder.drain()));
    frames
}

/// What a stream should decode to.
struct Expect {
    format: PixelFormat,
    full_range: bool,
}

/// Decodes a video stream and compares every picture with the FFmpeg
/// command line tool's: size, format and the hash of all plane data.
fn check_video(codec: CodecId, units: Vec<Vec<u8>>, md5_listing: &str, expect: Expect) {
    let Some(lib) = library_with(codec) else {
        return;
    };
    let hashes = framemd5_hashes(md5_listing);
    for threads in [0, 1] {
        let config = VideoConfig {
            threads,
            extradata: None,
        };
        let frames = decode_video(&lib, codec, config, &units);
        assert_eq!(
            frames.len(),
            hashes.len(),
            "frame count (threads {threads})"
        );
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!((frame.width, frame.height), (160, 90));
            assert_eq!(frame.format, expect.format, "pixel format");
            assert_eq!(frame.color_range_full, expect.full_range, "range");
            assert_eq!(
                picture_md5(frame),
                hashes[i],
                "frame {i} (threads {threads})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[test]
fn disabled_setting_reports_disabled() {
    let settings = Settings {
        disabled: true,
        ..Settings::default()
    };
    assert_eq!(load_with(&settings).unwrap_err(), LoadError::Disabled);
}

#[test]
fn environment_switch_disables_ffmpeg() {
    // `set_var` is unsafe and racy in a threaded test binary, so a child
    // process runs the check with the variable in its environment.
    if std::env::var_os("FFMPEG_DL_TEST_CHILD").is_some() {
        assert_eq!(Library::load().unwrap_err(), LoadError::Disabled);
        assert!(Library::get().is_none());
        assert_eq!(Library::status().err(), Some(&LoadError::Disabled));
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["environment_switch_disables_ffmpeg", "--nocapture"])
        .env("TRUST_FFMPEG", "0")
        .env("FFMPEG_DL_TEST_CHILD", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "child failed:\n{stdout}");
    assert!(stdout.contains("1 passed"), "child ran no test:\n{stdout}");
}

#[test]
fn environment_library_override_is_the_only_candidate() {
    if std::env::var_os("FFMPEG_DL_TEST_CHILD").is_some() {
        let Err(LoadError::Unavailable(attempts)) = Library::load() else {
            panic!("a nonexistent library must not load");
        };
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].path, "/nonexistent/libavcodec.so.63");
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "environment_library_override_is_the_only_candidate",
            "--nocapture",
        ])
        .env("TRUST_FFMPEG_LIB", "/nonexistent/libavcodec.so.63")
        .env("FFMPEG_DL_TEST_CHILD", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "child failed:\n{stdout}");
    assert!(stdout.contains("1 passed"), "child ran no test:\n{stdout}");
}

#[test]
fn a_library_that_is_not_ffmpeg_fails_cleanly() {
    // libm exists everywhere glibc does, loads, and has no avcodec_version.
    let settings = Settings {
        library: Some("libm.so.6".into()),
        ..Settings::default()
    };
    let Err(LoadError::Unavailable(attempts)) = load_with(&settings) else {
        panic!("libm is not FFmpeg");
    };
    assert_eq!(attempts.len(), 1);
    assert!(
        attempts[0].reason.contains("avcodec_version"),
        "{:?}",
        attempts[0]
    );
    assert!(
        LoadError::Unavailable(attempts)
            .to_string()
            .contains("libm.so.6")
    );
}

#[test]
fn a_missing_library_fails_cleanly_with_one_attempt_per_candidate() {
    let settings = Settings {
        library: Some("/nonexistent/libavcodec.so.63".into()),
        ..Settings::default()
    };
    let Err(LoadError::Unavailable(attempts)) = load_with(&settings) else {
        panic!("a nonexistent library must not load");
    };
    assert_eq!(attempts.len(), 1);
}

/// A directory of stand-in libraries built with the system C compiler, removed
/// on drop.
struct FakeLibraries(std::path::PathBuf);

impl FakeLibraries {
    fn new(name: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("ffmpeg-dl-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        FakeLibraries(directory)
    }

    /// Builds `file` exporting one function, `symbol`, that returns `version`.
    /// `None` when there is no C compiler.
    fn add(
        &self,
        file: &str,
        symbol: &str,
        version: (u32, u32, u32),
    ) -> Option<std::path::PathBuf> {
        let source = self.0.join(format!("{file}.c"));
        let packed = (version.0 << 16) | (version.1 << 8) | version.2;
        std::fs::write(
            &source,
            format!("unsigned {symbol}(void) {{ return {packed}u; }}\n"),
        )
        .ok()?;
        let library = self.0.join(file);
        let status = std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-o"])
            .arg(&library)
            .arg(&source)
            .status()
            .ok()?;
        status.success().then_some(library)
    }

    /// The failure of loading `avcodec` (beside whatever else was added).
    fn load_error(&self, avcodec: std::path::PathBuf) -> String {
        let settings = Settings {
            library: Some(avcodec),
            ..Settings::default()
        };
        match load_with(&settings) {
            Err(LoadError::Unavailable(attempts)) => attempts[0].reason.clone(),
            other => panic!("a stand-in library must not load: {other:?}"),
        }
    }
}

impl Drop for FakeLibraries {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn unsupported_and_incomplete_libraries_are_refused_by_name() {
    let fakes = FakeLibraries::new("refused");
    // FFmpeg 4.4's libavcodec: older than the supported range.
    let Some(old) = fakes.add("libavcodec.so.58", "avcodec_version", (58, 134, 100)) else {
        eprintln!("skipped: no C compiler");
        return;
    };
    assert!(
        fakes
            .load_error(old)
            .contains("outside the supported range 59 to 63")
    );
    // Libav numbers its micro versions below 100.
    let libav = fakes
        .add("libavcodec.so.60", "avcodec_version", (60, 3, 7))
        .unwrap();
    assert!(fakes.load_error(libav).contains("not FFmpeg"));
    // A libavutil of another release than its libavcodec.
    let mismatched = fakes
        .add("libavcodec.so.62", "avcodec_version", (62, 11, 103))
        .unwrap();
    fakes
        .add("libavutil.so.60", "avutil_version", (59, 2, 100))
        .unwrap();
    assert!(fakes.load_error(mismatched).contains("does not belong"));
    // Right versions, no functions: the first one missing is named.
    let hollow = fakes
        .add("libavcodec.so.63", "avcodec_version", (63, 1, 102))
        .unwrap();
    fakes
        .add("libavutil.so.61", "avutil_version", (61, 1, 102))
        .unwrap();
    let reason = fakes.load_error(hollow);
    assert!(reason.starts_with("missing symbol "), "{reason}");
}

#[test]
fn loaded_versions_pair_up() {
    let Some(lib) = library() else { return };
    let (avcodec, avutil) = (lib.avcodec_version(), lib.avutil_version());
    let expected_avutil = match avcodec.major {
        59 => 57,
        60 => 58,
        61 => 59,
        62 => 60,
        63 => 61,
        other => panic!("unexpected libavcodec {other}"),
    };
    assert_eq!(avutil.major, expected_avutil);
    assert!(avcodec.micro >= 100 && avutil.micro >= 100);
    eprintln!("libavcodec {avcodec}, libavutil {avutil}");
}

#[test]
fn ids_and_formats_resolve_to_the_header_values() {
    let Some(lib) = library() else { return };
    // AV_CODEC_ID_* from libavcodec/codec_id.h of n5.0.3 .. n9.0.2.
    let (hevc, vp9, av1) = match lib.avcodec_version().major {
        59 | 60 => (173, 167, 226),
        61 | 62 => (173, 167, 225),
        63 => (172, 166, 222),
        other => panic!("unexpected libavcodec {other}"),
    };
    let expected = [
        (CodecId::H264, 27),
        (CodecId::Hevc, hevc),
        (CodecId::Aac, 86018),
        (CodecId::Ac3, 86019),
        (CodecId::Eac3, 86056),
        (CodecId::Vp8, 139),
        (CodecId::Vp9, vp9),
        (CodecId::Av1, av1),
        (CodecId::Mp3, 86017),
        (CodecId::Opus, 86076),
        (CodecId::Vorbis, 86021),
        (CodecId::Flac, 86028),
    ];
    for (codec, id) in expected {
        assert_eq!(lib.codec_id(codec), id, "{codec:?}");
    }
    // AV_PIX_FMT_* from libavutil/pixfmt.h: P010LE moved with 7.0.
    let p010 = if lib.avcodec_version().major <= 60 {
        159
    } else {
        158
    };
    let pixel = [
        (0, PixelFormat::Yuv420p, false),
        (12, PixelFormat::Yuv420p, true),
        (4, PixelFormat::Yuv422p, false),
        (13, PixelFormat::Yuv422p, true),
        (5, PixelFormat::Yuv444p, false),
        (14, PixelFormat::Yuv444p, true),
        (8, PixelFormat::Gray8, false),
        (23, PixelFormat::Nv12, false),
        (62, PixelFormat::Yuv420p10, false),
        (64, PixelFormat::Yuv422p10, false),
        (68, PixelFormat::Yuv444p10, false),
        (123, PixelFormat::Yuv420p12, false),
        (p010, PixelFormat::P010, false),
    ];
    for (raw, format, full) in pixel {
        assert_eq!(
            lib.formats.pixel(raw),
            Some((format, full)),
            "AVPixelFormat {raw}"
        );
    }
    assert!(lib.formats.pixel(-1).is_none());
    // AV_SAMPLE_FMT_*: U8 S16 S32 FLT DBL, then the planar five.
    for raw in 0..10 {
        let (_, planar) = lib.formats.sample(raw).expect("sample format");
        assert_eq!(planar, raw >= 5);
    }
    assert!(lib.formats.sample(10).is_none());
}

#[test]
fn has_decoder_reports_the_installation() {
    let Some(lib) = library() else { return };
    for codec in CodecId::ALL {
        eprintln!("{codec:?}: {}", lib.has_decoder(codec));
    }
    // A decoder can be created exactly when one exists.
    for codec in CodecId::ALL {
        let result = if codec.is_video() {
            VideoDecoder::new(&lib, codec, VideoConfig::default()).map(drop)
        } else {
            // Opus, Vorbis and FLAC refuse to open without their headers.
            AudioDecoder::new(&lib, codec, AudioConfig::default()).map(drop)
        };
        if !lib.has_decoder(codec) {
            assert!(matches!(result, Err(Error::NoDecoder(c)) if c == codec));
        }
    }
}

#[test]
fn wrong_media_type_is_refused() {
    let Some(lib) = library() else { return };
    assert!(matches!(
        VideoDecoder::new(&lib, CodecId::Aac, VideoConfig::default()),
        Err(Error::WrongMediaType(CodecId::Aac))
    ));
    assert!(matches!(
        AudioDecoder::new(&lib, CodecId::H264, AudioConfig::default()),
        Err(Error::WrongMediaType(CodecId::H264))
    ));
}

#[test]
fn handles_are_send() {
    fn send<T: Send>() {}
    fn send_sync<T: Send + Sync>() {}
    send::<VideoDecoder>();
    send::<AudioDecoder>();
    send_sync::<Library>();
    send_sync::<VideoFrame>();
}

// ---------------------------------------------------------------------------
// Video streams against the ffmpeg command line tool
// ---------------------------------------------------------------------------

#[test]
fn h264_baseline() {
    check_video(
        CodecId::H264,
        h264_units(fixture!("h264_baseline.h264")),
        expected_md5!("h264_baseline"),
        Expect {
            format: PixelFormat::Yuv420p,
            full_range: false,
        },
    );
}

#[test]
fn h264_high_profile_with_reordering() {
    check_video(
        CodecId::H264,
        h264_units(fixture!("h264_high_bframes.h264")),
        expected_md5!("h264_high_bframes"),
        Expect {
            format: PixelFormat::Yuv420p,
            full_range: false,
        },
    );
}

#[test]
fn h264_10_bit() {
    check_video(
        CodecId::H264,
        h264_units(fixture!("h264_high10.h264")),
        expected_md5!("h264_high10"),
        Expect {
            format: PixelFormat::Yuv420p10,
            full_range: false,
        },
    );
}

#[test]
fn h264_chroma_formats() {
    for (units, listing, format) in [
        (
            fixture!("h264_high422.h264"),
            expected_md5!("h264_high422"),
            PixelFormat::Yuv422p,
        ),
        (
            fixture!("h264_high444.h264"),
            expected_md5!("h264_high444"),
            PixelFormat::Yuv444p,
        ),
        (
            fixture!("h264_high422_10.h264"),
            expected_md5!("h264_high422_10"),
            PixelFormat::Yuv422p10,
        ),
        (
            fixture!("h264_high444_10.h264"),
            expected_md5!("h264_high444_10"),
            PixelFormat::Yuv444p10,
        ),
    ] {
        check_video(
            CodecId::H264,
            h264_units(units),
            listing,
            Expect {
                format,
                full_range: false,
            },
        );
    }
}

#[test]
fn h264_full_range_is_reported_as_yuv420p_with_the_range_flag() {
    for (data, listing) in [
        (
            fixture!("h264_fullrange.h264"),
            expected_md5!("h264_fullrange"),
        ),
        (
            fixture!("h264_monochrome.h264"),
            expected_md5!("h264_monochrome"),
        ),
    ] {
        check_video(
            CodecId::H264,
            h264_units(data),
            listing,
            Expect {
                format: PixelFormat::Yuv420p,
                full_range: true,
            },
        );
    }
}

#[test]
fn hevc() {
    check_video(
        CodecId::Hevc,
        hevc_units(fixture!("hevc_main.hevc")),
        expected_md5!("hevc_main"),
        Expect {
            format: PixelFormat::Yuv420p,
            full_range: false,
        },
    );
}

#[test]
fn hevc_monochrome_is_gray8() {
    check_video(
        CodecId::Hevc,
        hevc_units(fixture!("hevc_gray.hevc")),
        expected_md5!("hevc_gray"),
        Expect {
            format: PixelFormat::Gray8,
            full_range: true,
        },
    );
}

#[test]
fn vp8() {
    check_video(
        CodecId::Vp8,
        ivf_frames(fixture!("vp8.ivf")),
        expected_md5!("vp8"),
        Expect {
            format: PixelFormat::Yuv420p,
            full_range: false,
        },
    );
}

#[test]
fn vp9() {
    check_video(
        CodecId::Vp9,
        ivf_frames(fixture!("vp9.ivf")),
        expected_md5!("vp9"),
        Expect {
            format: PixelFormat::Yuv420p,
            full_range: false,
        },
    );
}

#[test]
fn vp9_4_4_4() {
    check_video(
        CodecId::Vp9,
        ivf_frames(fixture!("vp9_444.ivf")),
        expected_md5!("vp9_444"),
        Expect {
            format: PixelFormat::Yuv444p,
            full_range: false,
        },
    );
}

#[test]
fn av1() {
    check_video(
        CodecId::Av1,
        ivf_frames(fixture!("av1.ivf")),
        expected_md5!("av1"),
        Expect {
            format: PixelFormat::Yuv420p,
            full_range: false,
        },
    );
}

#[test]
fn av1_10_and_12_bit() {
    check_video(
        CodecId::Av1,
        ivf_frames(fixture!("av1_10bit.ivf")),
        expected_md5!("av1_10bit"),
        Expect {
            format: PixelFormat::Yuv420p10,
            full_range: false,
        },
    );
    check_video(
        CodecId::Av1,
        ivf_frames(fixture!("av1_12bit.ivf")),
        expected_md5!("av1_12bit"),
        Expect {
            format: PixelFormat::Yuv420p12,
            full_range: false,
        },
    );
}

#[test]
fn pts_travels_with_the_picture() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    // Without reordering, pictures come back in order with their pts.
    let units = h264_units(fixture!("h264_baseline.h264"));
    let frames = decode_video(&lib, CodecId::H264, VideoConfig::default(), &units);
    let pts: Vec<i64> = frames.iter().map(|frame| frame.pts).collect();
    assert_eq!(pts, (0..10).map(|i| i * 1000).collect::<Vec<_>>());
    // With B-frames, each packet's pts still comes out exactly once.
    let units = h264_units(fixture!("h264_high_bframes.h264"));
    let frames = decode_video(&lib, CodecId::H264, VideoConfig::default(), &units);
    let pts: HashSet<i64> = frames.iter().map(|frame| frame.pts).collect();
    assert_eq!(pts.len(), 10);
    assert!(
        pts.iter()
            .all(|pts| pts % 1000 == 0 && (0..10_000).contains(pts))
    );
}

#[test]
fn color_description_and_aspect_ratio_are_reported() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let units = h264_units(fixture!("h264_baseline.h264"));
    let frame = &decode_video(&lib, CodecId::H264, VideoConfig::default(), &units)[0];
    // The test pattern carries no VUI colour description or aspect ratio.
    assert_eq!((frame.matrix, frame.primaries, frame.transfer), (2, 2, 2));
    assert_eq!(frame.sample_aspect, (1, 1));
}

#[test]
fn h264_in_avcc_form_with_extradata() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let stream = fixture!("h264_high_bframes.h264");
    // Repackage as ISO BMFF carries it: an avcC record (SPS and PPS) and
    // length-prefixed NAL units.
    let codes = start_codes(stream);
    let nal = |i: usize| {
        let end = codes.get(i + 1).map_or(stream.len(), |&(next, _)| next);
        &stream[codes[i].1..end]
    };
    let (mut sps, mut pps) = (None, None);
    let mut units: Vec<Vec<u8>> = Vec::new();
    let mut started = false;
    for i in 0..codes.len() {
        let nal = nal(i);
        match nal[0] & 0x1f {
            7 => sps = sps.or(Some(nal)),
            8 => pps = pps.or(Some(nal)),
            1 | 5 => {
                if nal[1] & 0x80 != 0 || !started {
                    units.push(Vec::new());
                    started = true;
                }
                let unit = units.last_mut().unwrap();
                unit.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                unit.extend_from_slice(nal);
            }
            _ => {}
        }
    }
    let (sps, pps) = (sps.unwrap(), pps.unwrap());
    let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1);
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(pps);

    let config = VideoConfig {
        threads: 1,
        extradata: Some(avcc),
    };
    let frames = decode_video(&lib, CodecId::H264, config, &units);
    let hashes = framemd5_hashes(expected_md5!("h264_high_bframes"));
    assert_eq!(frames.len(), 10);
    for (frame, hash) in frames.iter().zip(&hashes) {
        assert_eq!(&picture_md5(frame), hash);
    }
}

// ---------------------------------------------------------------------------
// Audio streams
// ---------------------------------------------------------------------------

/// Samples of one channel.
fn channel(frames: &[AudioFrame], index: usize) -> Vec<f32> {
    frames
        .iter()
        .flat_map(|frame| {
            frame
                .samples
                .chunks_exact(usize::from(frame.channels))
                .map(move |s| s[index])
        })
        .collect()
}

/// Amplitude of the `hz` component of `samples` (Goertzel's algorithm),
/// skipping the codec's start-up samples. A pure tone of amplitude `a` gives
/// `a`; zero-crossing counts are no good here, as lossy encoders add noise.
fn tone_amplitude(samples: &[f32], hz: f64, sample_rate: u32) -> f64 {
    let steady = &samples[4096..];
    let coefficient = 2.0 * (2.0 * std::f64::consts::PI * hz / f64::from(sample_rate)).cos();
    let (mut previous, mut before) = (0.0f64, 0.0f64);
    for &sample in steady {
        let current = f64::from(sample) + coefficient * previous - before;
        before = previous;
        previous = current;
    }
    let power = previous * previous + before * before - coefficient * previous * before;
    2.0 * power.max(0.0).sqrt() / steady.len() as f64
}

/// Whether `samples` is (nearly) a pure tone at `hz`, and silent at `other`.
/// The fixtures' sine has amplitude 0.125 (the lavfi default).
fn is_tone(samples: &[f32], hz: f64, other: f64) -> bool {
    let wanted = tone_amplitude(samples, hz, 48000);
    let unwanted = tone_amplitude(samples, other, 48000);
    (wanted - 0.125).abs() < 0.01 && unwanted < 0.005
}

/// The fixtures play 440 Hz on the left and 880 Hz on the right for one
/// second, so channel order, interleaving and scaling are all visible.
fn check_stereo_tones(frames: &[AudioFrame], samples_per_packet: usize, packets: usize) {
    assert!(
        frames
            .iter()
            .all(|f| f.channels == 2 && f.sample_rate == 48000)
    );
    assert_eq!(frames.len(), packets, "one frame per packet");
    assert_eq!(
        frames.iter().map(|f| f.samples.len() / 2).sum::<usize>(),
        samples_per_packet * packets
    );
    let (left, right) = (channel(frames, 0), channel(frames, 1));
    assert!(is_tone(&left, 440.0, 880.0), "left channel");
    assert!(is_tone(&right, 880.0, 440.0), "right channel");
}

#[test]
fn aac_adts() {
    let Some(lib) = library_with(CodecId::Aac) else {
        return;
    };
    let packets = split_frames(fixture!("aac_stereo.adts"), adts_length);
    // The stream states its format; a matching hint changes nothing.
    for config in [
        AudioConfig::default(),
        AudioConfig {
            sample_rate: 48000,
            channels: 2,
            extradata: None,
        },
    ] {
        let frames = decode_audio(&lib, CodecId::Aac, config, &packets);
        check_stereo_tones(&frames, 1024, packets.len());
        let pts: Vec<i64> = frames.iter().map(|f| f.pts).collect();
        assert_eq!(
            pts,
            (0..packets.len() as i64)
                .map(|i| i * 1000)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn aac_mono() {
    let Some(lib) = library_with(CodecId::Aac) else {
        return;
    };
    let packets = split_frames(fixture!("aac_mono.adts"), adts_length);
    let config = AudioConfig {
        sample_rate: 48000,
        channels: 1,
        extradata: None,
    };
    let frames = decode_audio(&lib, CodecId::Aac, config, &packets);
    assert!(
        frames
            .iter()
            .all(|f| f.channels == 1 && f.sample_rate == 48000)
    );
    let tone = channel(&frames, 0);
    assert_eq!(tone.len(), 1024 * packets.len());
    assert!(is_tone(&tone, 440.0, 880.0));
}

#[test]
fn aac_raw_frames_with_audio_specific_config() {
    let Some(lib) = library_with(CodecId::Aac) else {
        return;
    };
    let adts = split_frames(fixture!("aac_stereo.adts"), adts_length);
    let raw: Vec<Vec<u8>> = adts
        .iter()
        .map(|frame| adts_payload(frame).to_vec())
        .collect();
    let config = AudioConfig {
        sample_rate: 48000,
        channels: 2,
        extradata: Some(AAC_LC_48K_STEREO.to_vec()),
    };
    let from_raw = decode_audio(&lib, CodecId::Aac, config, &raw);
    let from_adts = decode_audio(&lib, CodecId::Aac, AudioConfig::default(), &adts);
    assert_eq!(from_raw, from_adts);
}

#[test]
fn ac3_and_eac3() {
    for (codec, data, length) in [
        (
            CodecId::Ac3,
            fixture!("ac3_stereo.ac3"),
            ac3_length as fn(&[u8]) -> usize,
        ),
        (CodecId::Eac3, fixture!("eac3_stereo.eac3"), eac3_length),
    ] {
        let Some(lib) = library_with(codec) else {
            continue;
        };
        let packets = split_frames(data, length);
        let frames = decode_audio(&lib, codec, AudioConfig::default(), &packets);
        check_stereo_tones(&frames, 1536, packets.len());
    }
}

#[test]
fn mp3() {
    let Some(lib) = library_with(CodecId::Mp3) else {
        return;
    };
    let packets = split_frames(fixture!("mp3_stereo.mp3"), mp3_length);
    let frames = decode_audio(&lib, CodecId::Mp3, AudioConfig::default(), &packets);
    assert!(
        frames
            .iter()
            .all(|f| f.channels == 2 && f.sample_rate == 48000)
    );
    assert_eq!(
        frames.iter().map(|f| f.samples.len() / 2).sum::<usize>(),
        1152 * packets.len()
    );
    let (left, right) = (channel(&frames, 0), channel(&frames, 1));
    assert!(is_tone(&left, 440.0, 880.0) && is_tone(&right, 880.0, 440.0));
}

// ---------------------------------------------------------------------------
// Decoder behaviour: EAGAIN, drain, flush, threads, corrupt input
// ---------------------------------------------------------------------------

/// Hashes of all the pictures of a decode.
fn hashes(frames: &[VideoFrame]) -> Vec<String> {
    frames.iter().map(picture_md5).collect()
}

#[test]
fn send_never_needs_receive_first() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let units = h264_units(fixture!("h264_high_bframes.h264"));
    let expected = framemd5_hashes(expected_md5!("h264_high_bframes"));
    for threads in [1, 0] {
        let mut decoder = VideoDecoder::new(
            &lib,
            CodecId::H264,
            VideoConfig {
                threads,
                extradata: None,
            },
        )
        .unwrap();
        // All input first: FFmpeg answers EAGAIN once a picture is waiting,
        // and the decoder receives it on the caller's behalf.
        for (i, unit) in units.iter().enumerate() {
            decoder.send(unit, i as i64, i == 0).unwrap();
        }
        let mut frames = Vec::new();
        while let Some(frame) = decoder.receive().unwrap() {
            frames.push(frame);
        }
        frames.extend(drained(decoder.drain()));
        assert_eq!(hashes(&frames), expected, "threads {threads}");
    }
}

#[test]
fn a_caller_that_never_receives_gets_queue_full_not_unbounded_memory() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let units = h264_units(fixture!("h264_baseline.h264"));
    let mut decoder = VideoDecoder::new(
        &lib,
        CodecId::H264,
        VideoConfig {
            threads: 1,
            extradata: None,
        },
    )
    .unwrap();
    let mut sent = 0;
    let rejected = loop {
        let unit = &units[sent % units.len()];
        match decoder.send(unit, sent as i64, sent % units.len() == 0) {
            Ok(()) => sent += 1,
            Err(error) => break error,
        }
        assert!(sent < 1000, "the queue must be bounded");
    };
    assert!(matches!(rejected, Error::QueueFull), "{rejected:?}");
    // The rejected packet was not consumed: once the output is received it
    // goes through, and the pictures come out in order.
    let unit = &units[sent % units.len()];
    let mut received = 0;
    while decoder.receive().unwrap().is_some() {
        received += 1;
    }
    assert!(received >= 32, "{received} pictures were waiting");
    decoder.send(unit, sent as i64, false).unwrap();
}

#[test]
fn drain_resets_the_decoder_for_a_new_sequence() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let units = h264_units(fixture!("h264_high_bframes.h264"));
    let expected = framemd5_hashes(expected_md5!("h264_high_bframes"));
    let mut decoder = VideoDecoder::new(&lib, CodecId::H264, VideoConfig::default()).unwrap();
    for round in 0..3 {
        let mut frames = Vec::new();
        for (i, unit) in units.iter().enumerate() {
            decoder.send(unit, i as i64, i == 0).unwrap();
            frames.extend(std::iter::from_fn(|| decoder.receive().unwrap()));
        }
        frames.extend(drained(decoder.drain()));
        assert_eq!(hashes(&frames), expected, "round {round}");
    }
}

#[test]
fn flush_discards_buffered_pictures_and_restarts_at_a_keyframe() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let units = h264_units(fixture!("h264_high_bframes.h264"));
    let expected = framemd5_hashes(expected_md5!("h264_high_bframes"));
    let mut decoder = VideoDecoder::new(&lib, CodecId::H264, VideoConfig::default()).unwrap();
    for unit in &units[..4] {
        decoder.send(unit, 0, false).unwrap();
    }
    decoder.flush(); // a seek
    assert!(decoder.receive().unwrap().is_none());
    let mut frames = Vec::new();
    for (i, unit) in units.iter().enumerate() {
        decoder.send(unit, i as i64, i == 0).unwrap();
        frames.extend(std::iter::from_fn(|| decoder.receive().unwrap()));
    }
    frames.extend(drained(decoder.drain()));
    assert_eq!(hashes(&frames), expected);
}

#[test]
fn thread_counts_give_identical_pictures() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let units = h264_units(fixture!("h264_high_bframes.h264"));
    let reference = hashes(&decode_video(
        &lib,
        CodecId::H264,
        VideoConfig {
            threads: 1,
            extradata: None,
        },
        &units,
    ));
    for threads in [0, 2, 4, 1000] {
        let config = VideoConfig {
            threads,
            extradata: None,
        };
        assert_eq!(
            hashes(&decode_video(&lib, CodecId::H264, config, &units)),
            reference,
            "threads {threads}"
        );
    }
}

#[test]
fn empty_input_is_refused() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let mut decoder = VideoDecoder::new(&lib, CodecId::H264, VideoConfig::default()).unwrap();
    assert!(matches!(
        decoder.send(&[], 0, false),
        Err(Error::InvalidInput(_))
    ));
    let mut audio = match AudioDecoder::new(&lib, CodecId::Aac, AudioConfig::default()) {
        Ok(audio) => audio,
        Err(_) => return,
    };
    assert!(matches!(
        audio.send(&[], 0, false),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn decoders_work_on_other_threads_at_the_same_time() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let units = h264_units(fixture!("h264_high_bframes.h264"));
    let expected = framemd5_hashes(expected_md5!("h264_high_bframes"));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (lib, units) = (Arc::clone(&lib), units.clone());
            std::thread::spawn(move || {
                let frames = decode_video(&lib, CodecId::H264, VideoConfig::default(), &units);
                hashes(&frames)
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().unwrap(), expected);
    }
}

#[test]
fn a_decoder_can_move_between_threads() {
    let Some(lib) = library_with(CodecId::H264) else {
        return;
    };
    let units = h264_units(fixture!("h264_baseline.h264"));
    let mut decoder = VideoDecoder::new(&lib, CodecId::H264, VideoConfig::default()).unwrap();
    for (i, unit) in units[..5].iter().enumerate() {
        decoder.send(unit, i as i64, i == 0).unwrap();
    }
    let rest = units[5..].to_vec();
    let frames = std::thread::spawn(move || {
        for (i, unit) in rest.iter().enumerate() {
            decoder.send(unit, 5 + i as i64, false).unwrap();
        }
        drained(decoder.drain()).len()
    })
    .join()
    .unwrap();
    assert!(frames > 0);
}

/// A deterministic byte source (xorshift).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

#[test]
fn garbage_is_an_error_not_a_crash() {
    let mut random = Random(0x9e3779b97f4a7c15);
    for (codec, valid_stream) in [
        (CodecId::Vp9, ivf_frames(fixture!("vp9.ivf"))),
        (CodecId::Vp8, ivf_frames(fixture!("vp8.ivf"))),
        (CodecId::H264, h264_units(fixture!("h264_baseline.h264"))),
        (CodecId::Hevc, hevc_units(fixture!("hevc_main.hevc"))),
    ] {
        let Some(lib) = library_with(codec) else {
            continue;
        };
        let mut decoder = VideoDecoder::new(&lib, codec, VideoConfig::default()).unwrap();
        // FFmpeg reports an undecodable packet from send(), or, with frame
        // threads, only later from receive() or when draining.
        let mut errors = 0;
        for len in [1, 2, 3, 17, 100, 4096] {
            let packet = random.bytes(len);
            errors += usize::from(decoder.send(&packet, 0, false).is_err());
            loop {
                match decoder.receive() {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => errors += 1,
                }
            }
        }
        errors += decoder
            .drain()
            .iter()
            .filter(|output| output.is_err())
            .count();
        if matches!(codec, CodecId::Vp8 | CodecId::Vp9) {
            assert!(errors > 0, "{codec:?} accepted random bytes");
        }
        // The decoder survives: a valid stream still decodes.
        let mut frames = 0;
        for (i, unit) in valid_stream.iter().enumerate() {
            decoder.send(unit, i as i64, i == 0).unwrap();
            while let Some(frame) = decoder.receive().unwrap() {
                assert_eq!((frame.width, frame.height), (160, 90));
                frames += 1;
            }
        }
        frames += drained(decoder.drain()).len();
        assert_eq!(frames, 10, "{codec:?} after garbage");
    }

    let Some(lib) = library_with(CodecId::Aac) else {
        return;
    };
    let mut decoder = AudioDecoder::new(&lib, CodecId::Aac, AudioConfig::default()).unwrap();
    let packet = random.bytes(300);
    let error = decoder
        .send(&packet, 0, true)
        .expect_err("AAC rejects random bytes");
    assert!(error.is_invalid_data(), "{error:?}");
}

#[test]
fn corrupted_streams_decode_without_crashing() {
    let mut random = Random(0x2545f4914f6cdd1d);
    let streams: [(CodecId, Vec<Vec<u8>>); 5] = [
        (
            CodecId::H264,
            h264_units(fixture!("h264_high_bframes.h264")),
        ),
        (CodecId::Hevc, hevc_units(fixture!("hevc_main.hevc"))),
        (CodecId::Vp8, ivf_frames(fixture!("vp8.ivf"))),
        (CodecId::Vp9, ivf_frames(fixture!("vp9.ivf"))),
        (CodecId::Av1, ivf_frames(fixture!("av1.ivf"))),
    ];
    for (codec, units) in streams {
        let Some(lib) = library_with(codec) else {
            continue;
        };
        for round in 0..40 {
            let mut decoder = VideoDecoder::new(&lib, codec, VideoConfig::default()).unwrap();
            for (i, unit) in units.iter().enumerate() {
                let mut unit = unit.clone();
                // Flip a few bytes (never the first: a start code or header).
                for _ in 0..1 + round % 4 {
                    let at = 1 + random.next() as usize % (unit.len() - 1);
                    unit[at] ^= 1 << (random.next() % 8);
                }
                if round % 7 == 3 {
                    unit.truncate(1 + random.next() as usize % unit.len());
                }
                let _ = decoder.send(&unit, i as i64, i == 0);
                while let Ok(Some(frame)) = decoder.receive() {
                    assert!(
                        frame.width > 0
                            && frame
                                .planes
                                .iter()
                                .all(|p| plane_len(p) == p.width * p.height)
                    );
                }
            }
            let _ = decoder.drain();
        }
    }

    let Some(lib) = library_with(CodecId::Aac) else {
        return;
    };
    let packets = split_frames(fixture!("aac_stereo.adts"), adts_length);
    for round in 0..40 {
        let mut decoder = AudioDecoder::new(&lib, CodecId::Aac, AudioConfig::default()).unwrap();
        for (i, packet) in packets.iter().enumerate() {
            let mut packet = packet.clone();
            let at = 7 + random.next() as usize % (packet.len() - 7);
            packet[at] ^= 1 << (random.next() % 8);
            if round % 5 == 1 {
                packet.truncate(8);
            }
            let _ = decoder.send(&packet, i as i64, true);
            while let Ok(Some(frame)) = decoder.receive() {
                assert!(frame.channels > 0 && !frame.samples.is_empty());
            }
        }
        let _ = decoder.drain();
    }
}

// ---------------------------------------------------------------------------
// Frame conversion, from frames built here (formats no software decoder here
// produces, odd sizes, padded and bottom-up lines)
// ---------------------------------------------------------------------------

fn pixel_fmt(lib: &Library, name: &CStr) -> c_int {
    unsafe { (lib.api.av_get_pix_fmt)(name.as_ptr()) }
}

fn sample_fmt(lib: &Library, name: &CStr) -> c_int {
    unsafe { (lib.api.av_get_sample_fmt)(name.as_ptr()) }
}

fn blank_fields() -> FrameFields {
    FrameFields {
        data: [std::ptr::null(); 8],
        linesize: [0; 8],
        extended_data: std::ptr::null(),
        width: 0,
        height: 0,
        nb_samples: 0,
        format: -1,
        pts: 7,
        sample_aspect_ratio: (0, 1),
        sample_rate: 0,
        color_range: 0,
        color_primaries: 2,
        color_trc: 2,
        colorspace: 2,
        channels: 0,
    }
}

/// Plane storage: `rows` lines of `pitch` bytes whose first `used` bytes are
/// `fill(row, byte)`, the rest 0xEE padding that must not be copied.
fn plane_bytes(
    rows: usize,
    pitch: usize,
    used: usize,
    fill: impl Fn(usize, usize) -> u8,
) -> Vec<u8> {
    let mut bytes = vec![0xee; rows * pitch];
    for row in 0..rows {
        for byte in 0..used {
            bytes[row * pitch + byte] = fill(row, byte);
        }
    }
    bytes
}

#[test]
fn every_pixel_format_is_cropped_to_the_picture_and_stripped_of_padding() {
    use PixelFormat::*;
    let Some(lib) = library() else { return };
    // The `yuvj*` aliases are the plain formats, flagged full range.
    let cases: [(&CStr, PixelFormat, bool); 13] = [
        (c"yuv420p", Yuv420p, false),
        (c"yuvj420p", Yuv420p, true),
        (c"yuv422p", Yuv422p, false),
        (c"yuvj422p", Yuv422p, true),
        (c"yuv444p", Yuv444p, false),
        (c"yuvj444p", Yuv444p, true),
        (c"gray", Gray8, false),
        (c"nv12", Nv12, false),
        (c"yuv420p10le", Yuv420p10, false),
        (c"yuv422p10le", Yuv422p10, false),
        (c"yuv444p10le", Yuv444p10, false),
        (c"yuv420p12le", Yuv420p12, false),
        (c"p010le", P010, false),
    ];
    for (name, format, full) in cases {
        // A 5x3 picture: odd, so halved chroma rounds up to 3x2.
        let sizes: &[(usize, usize)] = match format {
            Yuv420p | Yuv420p10 | Yuv420p12 => &[(5, 3), (3, 2), (3, 2)],
            Yuv422p | Yuv422p10 => &[(5, 3), (3, 3), (3, 3)],
            Yuv444p | Yuv444p10 => &[(5, 3), (5, 3), (5, 3)],
            Gray8 => &[(5, 3)],
            Nv12 | P010 => &[(5, 3), (6, 2)],
        };
        let sample_bytes = if format.is_high_bit_depth() { 2 } else { 1 };
        // Lines of 16 samples, of which only the picture's are real.
        let pitch = 16 * sample_bytes;
        let storage: Vec<Vec<u8>> = sizes
            .iter()
            .enumerate()
            .map(|(plane, &(width, height))| {
                plane_bytes(height, pitch, width * sample_bytes, |row, byte| {
                    (plane * 64 + row * 16 + byte) as u8
                })
            })
            .collect();
        let mut fields = blank_fields();
        fields.format = pixel_fmt(&lib, name);
        (fields.width, fields.height) = (5, 3);
        for (plane, bytes) in storage.iter().enumerate() {
            fields.data[plane] = bytes.as_ptr();
            fields.linesize[plane] = pitch as c_int;
        }

        let frame = convert_video(&lib, &fields).unwrap_or_else(|e| panic!("{name:?}: {e}"));
        assert_eq!(
            (frame.format, frame.color_range_full),
            (format, full),
            "{name:?}"
        );
        assert_eq!((frame.width, frame.height, frame.pts), (5, 3, 7));
        assert_eq!(frame.planes.len(), sizes.len(), "{name:?}");
        for (index, (plane, &(width, height))) in frame.planes.iter().zip(sizes).enumerate() {
            assert_eq!(
                (plane.width, plane.height, plane.stride),
                (width, height, width),
                "{name:?}"
            );
            // Sample `n` of a row is the bytes the fill function made of it.
            let bytes: Vec<u8> = match &plane.data {
                PlaneData::U8(samples) => samples.clone(),
                PlaneData::U16(samples) => samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
            };
            let expected: Vec<u8> = (0..height)
                .flat_map(|row| {
                    (0..width * sample_bytes).map(move |byte| (index * 64 + row * 16 + byte) as u8)
                })
                .collect();
            assert_eq!(bytes, expected, "{name:?} plane {index}");
        }
    }
}

#[test]
fn bottom_up_planes_are_read_top_row_first() {
    let Some(lib) = library() else { return };
    // Negative linesize: data[0] addresses the first picture row and the
    // following rows lie at lower addresses.
    let rows: Vec<u8> = (0..4u8).flat_map(|r| vec![r; 8]).collect(); // memory: rows 3..0 reversed below
    let mut fields = blank_fields();
    fields.format = pixel_fmt(&lib, c"gray");
    (fields.width, fields.height) = (8, 4);
    fields.data[0] = rows[3 * 8..].as_ptr(); // picture row 0 = the last stored line
    fields.linesize[0] = -8;
    let frame = convert_video(&lib, &fields).unwrap();
    let samples = frame.planes[0].data.as_u8().unwrap();
    assert_eq!(&samples[..8], &[3; 8]);
    assert_eq!(&samples[24..], &[0; 8]);
}

#[test]
fn color_description_and_aspect_ratio_pass_through() {
    let Some(lib) = library() else { return };
    let gray = [0u8; 16];
    let mut fields = blank_fields();
    fields.format = pixel_fmt(&lib, c"gray");
    (fields.width, fields.height) = (4, 4);
    fields.data[0] = gray.as_ptr();
    fields.linesize[0] = 4;
    fields.sample_aspect_ratio = (32, 27);
    (fields.colorspace, fields.color_primaries, fields.color_trc) = (9, 9, 16);
    fields.color_range = 2;
    let frame = convert_video(&lib, &fields).unwrap();
    assert_eq!((frame.matrix, frame.primaries, frame.transfer), (9, 9, 16));
    assert_eq!(frame.sample_aspect, (32, 27));
    assert!(frame.color_range_full);
    fields.sample_aspect_ratio = (0, 1);
    fields.colorspace = 4096; // beyond an H.273 byte: unspecified
    fields.color_range = 1;
    let frame = convert_video(&lib, &fields).unwrap();
    assert_eq!(frame.sample_aspect, (1, 1));
    assert_eq!(frame.matrix, 2);
    assert!(!frame.color_range_full);
}

#[test]
fn implausible_video_frames_are_rejected() {
    let Some(lib) = library() else { return };
    let storage = [0u8; 64];
    let mut fields = blank_fields();
    fields.format = pixel_fmt(&lib, c"gray");
    fields.data[0] = storage.as_ptr();
    fields.linesize[0] = 8;
    let rejected =
        |fields: &FrameFields| matches!(convert_video(&lib, fields), Err(Error::InvalidFrame(_)));
    for (width, height) in [(0, 8), (8, 0), (-1, 8), (8, -1), (20000, 8), (16384, 16384)] {
        (fields.width, fields.height) = (width, height);
        assert!(rejected(&fields), "{width}x{height}");
    }
    (fields.width, fields.height) = (8, 8);
    assert!(convert_video(&lib, &fields).is_ok());
    fields.linesize[0] = 7; // shorter than a row
    assert!(rejected(&fields));
    fields.linesize[0] = -7;
    assert!(rejected(&fields));
    fields.linesize[0] = 8;
    fields.data[0] = std::ptr::null();
    assert!(rejected(&fields));
}

#[test]
fn unsupported_pixel_formats_are_reported_by_name() {
    let Some(lib) = library() else { return };
    let storage = vec![0u8; 256];
    let mut fields = blank_fields();
    (fields.width, fields.height) = (4, 4);
    fields.data[0] = storage.as_ptr();
    fields.linesize[0] = 16;
    for name in [c"rgb24", c"yuv420p16le", c"nv16", c"yuva420p"] {
        fields.format = pixel_fmt(&lib, name);
        let Err(Error::UnsupportedFormat {
            raw,
            name: reported,
        }) = convert_video(&lib, &fields)
        else {
            panic!("{name:?} must be unsupported");
        };
        assert_eq!(raw, fields.format);
        assert_eq!(reported, name.to_str().unwrap());
    }
    fields.format = -1;
    assert!(matches!(
        convert_video(&lib, &fields),
        Err(Error::UnsupportedFormat { raw: -1, .. })
    ));
}

fn audio_fields(lib: &Library, name: &CStr, channels: c_int, samples: c_int) -> FrameFields {
    let mut fields = blank_fields();
    fields.format = sample_fmt(lib, name);
    fields.channels = channels;
    fields.nb_samples = samples;
    fields.sample_rate = 44100;
    fields
}

#[test]
fn every_sample_format_converts_to_interleaved_floats() {
    let Some(lib) = library() else { return };
    // Three sample times of two channels, each value exact in every format.
    let channels: [[f64; 3]; 2] = [[0.0, 0.5, -1.0], [0.0, -0.5, 0.75]];
    let encode = |name: &CStr, value: f64| -> Vec<u8> {
        match name.to_str().unwrap().trim_end_matches('p') {
            "u8" => vec![(value * 128.0 + 128.0) as u8],
            "s16" => ((value * 32768.0) as i16).to_ne_bytes().to_vec(),
            "s32" => ((value * 2147483648.0) as i32).to_ne_bytes().to_vec(),
            "flt" => (value as f32).to_ne_bytes().to_vec(),
            "dbl" => value.to_ne_bytes().to_vec(),
            other => unreachable!("{other}"),
        }
    };
    let bytes = |name: &CStr, values: &[f64]| -> Vec<u8> {
        values.iter().flat_map(|&v| encode(name, v)).collect()
    };
    let interleaved: Vec<f64> = (0..3)
        .flat_map(|t| [channels[0][t], channels[1][t]])
        .collect();
    for name in [
        c"u8", c"s16", c"s32", c"flt", c"dbl", c"u8p", c"s16p", c"s32p", c"fltp", c"dblp",
    ] {
        // Packed audio is one buffer; planar audio one per channel.
        let planes: Vec<Vec<u8>> = if name.to_bytes().ends_with(b"p") {
            channels.iter().map(|values| bytes(name, values)).collect()
        } else {
            vec![bytes(name, &interleaved)]
        };
        let pointers: Vec<*const u8> = planes.iter().map(|plane| plane.as_ptr()).collect();
        let mut fields = audio_fields(&lib, name, 2, 3);
        fields.data[0] = pointers[0];
        fields.extended_data = pointers.as_ptr();
        let frame = convert_audio(&lib, &fields).unwrap_or_else(|e| panic!("{name:?}: {e}"));
        assert_eq!(
            (frame.channels, frame.sample_rate, frame.pts),
            (2, 44100, 7)
        );
        assert_eq!(frame.samples, [0.0, 0.0, 0.5, -0.5, -1.0, 0.75], "{name:?}");
    }
}

#[test]
fn planar_audio_with_more_than_eight_channels_uses_extended_data() {
    let Some(lib) = library() else { return };
    let planes: Vec<Vec<u8>> = (0..10)
        .map(|channel| {
            (0..4)
                .flat_map(|t| (channel as f32 + t as f32 / 8.0).to_ne_bytes())
                .collect()
        })
        .collect();
    let pointers: Vec<*const u8> = planes.iter().map(|plane| plane.as_ptr()).collect();
    let mut fields = audio_fields(&lib, c"fltp", 10, 4);
    fields.data[..8].copy_from_slice(&pointers[..8]);
    fields.extended_data = pointers.as_ptr();
    let frame = convert_audio(&lib, &fields).unwrap();
    assert_eq!(frame.channels, 10);
    assert_eq!(frame.samples.len(), 40);
    for t in 0..4 {
        for channel in 0..10 {
            assert_eq!(
                frame.samples[t * 10 + channel],
                channel as f32 + t as f32 / 8.0
            );
        }
    }
}

#[test]
fn implausible_audio_frames_are_rejected() {
    let Some(lib) = library() else { return };
    let storage = vec![0u8; 4096];
    let planes = [storage.as_ptr(), storage.as_ptr()];
    let good = || {
        let mut fields = audio_fields(&lib, c"fltp", 2, 16);
        fields.data[0] = planes[0];
        fields.extended_data = planes.as_ptr();
        fields
    };
    assert!(convert_audio(&lib, &good()).is_ok());
    let mutations: [fn(&mut FrameFields); 7] = [
        |f| f.channels = 0,
        |f| f.channels = -1,
        |f| f.channels = 65,
        |f| f.nb_samples = 0,
        |f| f.nb_samples = -5,
        |f| f.nb_samples = 1 << 30,
        |f| f.sample_rate = 0,
    ];
    for mutate in mutations {
        let mut fields = good();
        mutate(&mut fields);
        assert!(matches!(
            convert_audio(&lib, &fields),
            Err(Error::InvalidFrame(_))
        ));
    }
    let missing = [planes[0], std::ptr::null()];
    let mut fields = good();
    fields.extended_data = missing.as_ptr();
    assert!(
        matches!(convert_audio(&lib, &fields), Err(Error::InvalidFrame(_))),
        "missing plane"
    );
    let mut packed = audio_fields(&lib, c"s16", 2, 16);
    assert!(
        matches!(convert_audio(&lib, &packed), Err(Error::InvalidFrame(_))),
        "no data plane"
    );
    packed.format = sample_fmt(&lib, c"s64");
    assert!(matches!(
        convert_audio(&lib, &packed),
        Err(Error::UnsupportedFormat { .. })
    ));
}

#[test]
fn decode_errors_are_damage_but_state_errors_are_not() {
    let error = |code| Error::Ffmpeg {
        call: "avcodec_send_packet",
        code,
        message: String::new(),
    };
    // H.264's frame threads report a damaged macroblock as -1.
    assert!(error(-1).is_decode_error());
    assert!(error(crate::media::decode::ffmpeg::abi::AVERROR_INVALIDDATA).is_decode_error());
    for code in [
        crate::media::decode::ffmpeg::abi::AVERROR_EAGAIN,
        crate::media::decode::ffmpeg::abi::AVERROR_EOF,
        -12,
        -22,
    ] {
        assert!(!error(code).is_decode_error(), "{code}");
    }
    assert!(!Error::OutOfMemory.is_decode_error());
}
