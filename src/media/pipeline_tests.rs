//! End-to-end checks over small synthetic fixtures (`src/fixtures/media`,
//! generated with FFmpeg from `testsrc2` and `sine`): container sniffing,
//! demuxing and bundled decoding, with AV1 output compared bit for bit to
//! FFmpeg's decode (`ffmpeg -i av1.mp4 -f framehash -hash sha256
//! -pix_fmt yuv420p`).

use sha2::{Digest, Sha256};

use super::codec::Codec;
use super::decode::Registry;
use super::demux::{self, MemorySource};
use super::frame::{PlaneData, VideoFrame};
use super::track::TrackKind;

/// FFmpeg's SHA-256 of each decoded 96×64 I420 frame of the AV1 fixtures.
const AV1_FRAMES: [&str; 10] = [
    "1799c3805a03ae8ddaa96d758528ed3f61987e5fbbd1377a1d8c34cd0845c0a1",
    "a3fb1c661eed28f1f6fe7764cbc820e510f6a89556ae4c77a0dcd61e4d3c0e7d",
    "c3eff13160a0965de13d2225745a9f41abb2d7ed2b26bee9c47360c5f5e207bf",
    "82307f9825ea2bd2ab67f834d4530489f03d88ca5ecca4b20acd4d34dc1c346b",
    "8f19ee6a0f2741241bf88be148fd63f35fcaee02082bc45d2fa2b58cef251c3e",
    "3b8a18cc0fd1cb5324b01ef691185f7a63de4743d7f0c79e081f09aafffa0ca6",
    "fdc3bc96a93224c9ccd0498b3b2d33cf7990ec869a131a3ba62db3523d95c1c8",
    "77529a73e20e2230c6c1e9e3172888e4fda64f25b5d6d281c6a9cba9ca27d5d3",
    "b85e3521d2f9f5cc5ab4334ceac27ee05b524c5382bc8f11a96f58c27c4bd79d",
    "ddfaa93250a8505536ff89457c394207ed8d4074a92cd6286e0174a2ce0fb713",
];

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/fixtures/media")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// SHA-256 of the picture as planar Y, U, V rows (a semi-planar picture's
/// interleaved chroma is split), so every decoder's output layout hashes
/// alike.
fn frame_hash(frame: &VideoFrame) -> String {
    let mut hasher = Sha256::new();
    let rows = |plane: &super::frame::Plane| {
        let PlaneData::U8(data) = &plane.data else {
            panic!("8-bit fixture");
        };
        (0..plane.height)
            .map(|row| data[row * plane.stride..row * plane.stride + plane.width].to_vec())
            .collect::<Vec<_>>()
    };
    match frame.layout {
        super::frame::Layout::SemiPlanar => {
            for row in rows(&frame.planes[0]) {
                hasher.update(&row);
            }
            let chroma = rows(&frame.planes[1]);
            for parity in 0..2 {
                for row in &chroma {
                    let samples: Vec<u8> = row.iter().skip(parity).step_by(2).copied().collect();
                    hasher.update(&samples);
                }
            }
        }
        _ => {
            for plane in &frame.planes {
                for row in rows(plane) {
                    hasher.update(&row);
                }
            }
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Decode every video packet of `name` and return the frames in output order.
fn decode_video(name: &str) -> Vec<VideoFrame> {
    let mut demuxer = demux::open(Box::new(MemorySource::new(fixture(name))), None).unwrap();
    let track = demuxer
        .tracks()
        .iter()
        .find(|t| t.kind == TrackKind::Video)
        .cloned()
        .expect("video track");
    let mut decoder = Registry::get().video_decoder(&track).unwrap();
    let mut frames = Vec::new();
    while let Some(packet) = demuxer.read(track.id).unwrap() {
        frames.extend(decoder.decode(&packet).unwrap());
    }
    frames.extend(decoder.drain().unwrap());
    frames
}

#[test]
fn av1_matches_ffmpeg_in_every_container() {
    for name in [
        "av1.mp4",
        "av1_mp3.mp4",
        "av1_opus.webm",
        "av1_flac_frag.mp4",
    ] {
        let frames = decode_video(name);
        assert_eq!(frames.len(), 10, "{name}");
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!((frame.width, frame.height), (96, 64), "{name}");
            assert_eq!(frame_hash(frame), AV1_FRAMES[index], "{name} frame {index}");
            assert_eq!(frame.pts, index as i64 * 100_000, "{name} frame {index}");
        }
    }
}

#[test]
fn tracks_describe_their_codecs() {
    let cases: [(&str, &[Codec]); 7] = [
        ("av1_opus.webm", &[Codec::Av1, Codec::Opus]),
        ("vp9_opus.webm", &[Codec::Vp9, Codec::Opus]),
        ("av1_mp3.mp4", &[Codec::Av1, Codec::Mp3]),
        ("av1_flac_frag.mp4", &[Codec::Av1, Codec::Flac]),
        ("h264_aac.mp4", &[Codec::H264, Codec::Aac]),
        ("opus.ogg", &[Codec::Opus]),
        ("aac.adts", &[Codec::Aac]),
    ];
    for (name, codecs) in cases {
        let demuxer = demux::open(Box::new(MemorySource::new(fixture(name))), None).unwrap();
        let found: Vec<Codec> = demuxer.tracks().iter().map(|t| t.codec).collect();
        assert_eq!(found, codecs, "{name}");
        assert!(
            demuxer
                .duration()
                .is_some_and(|d| (900_000..=1_100_000).contains(&d)),
            "{name}: {:?}",
            demuxer.duration()
        );
    }
    let h264 = demux::open(Box::new(MemorySource::new(fixture("h264_aac.mp4"))), None).unwrap();
    assert_eq!(h264.tracks()[0].codec_string, "avc1.64000A");
    assert_eq!(h264.tracks()[1].codec_string, "mp4a.40.2");
}

/// Decode an audio fixture's first track and return (frames, rate, channels).
fn decode_audio(name: &str) -> (usize, u32, u16) {
    let mut demuxer = demux::open(Box::new(MemorySource::new(fixture(name))), None).unwrap();
    let track = demuxer
        .tracks()
        .iter()
        .find(|t| t.kind == TrackKind::Audio)
        .cloned()
        .expect("audio track");
    let mut decoder = Registry::get().audio_decoder(&track).unwrap();
    let (mut frames, mut rate, mut channels) = (0, 0, 0);
    while let Some(packet) = demuxer.read(track.id).unwrap() {
        for block in decoder.decode(&packet).unwrap() {
            frames += block.frames();
            rate = block.rate;
            channels = block.channels;
            assert!(
                block
                    .samples
                    .iter()
                    .all(|s| s.is_finite() && s.abs() <= 1.01)
            );
        }
    }
    (frames, rate, channels)
}

#[test]
fn bundled_audio_decodes_a_second_of_sine() {
    for (name, rate) in [
        ("sine.mp3", 44_100),
        ("flac.flac", 44_100),
        ("vorbis.ogg", 44_100),
        ("pcm.wav", 8_000),
        ("av1_mp3.mp4", 44_100),
        ("av1_flac_frag.mp4", 48_000),
    ] {
        let (frames, decoded_rate, channels) = decode_audio(name);
        assert_eq!(decoded_rate, rate, "{name}");
        assert_eq!(channels, 1, "{name}");
        let expected = if name == "pcm.wav" {
            4000
        } else {
            rate as usize
        };
        // Encoder priming and final-frame padding are not trimmed here.
        assert!(
            frames >= expected - 64 && frames <= expected + 2304,
            "{name}: {frames}"
        );
    }
}

#[test]
fn seeking_lands_on_keyframes() {
    for name in ["av1.mp4", "av1_opus.webm", "av1_flac_frag.mp4"] {
        let mut demuxer = demux::open(Box::new(MemorySource::new(fixture(name))), None).unwrap();
        let video = demuxer
            .tracks()
            .iter()
            .find(|t| t.kind == TrackKind::Video)
            .unwrap()
            .id;
        // Keyframes every 5 frames (500 ms): a seek to 0.7 s lands at 0.5 s.
        let landed = demuxer.seek(700_000).unwrap();
        assert!(landed <= 700_000, "{name}: {landed}");
        let packet = demuxer.read(video).unwrap().unwrap();
        assert!(packet.keyframe, "{name}");
        assert!(
            packet.pts <= 700_000 && packet.pts >= 0,
            "{name}: {}",
            packet.pts
        );
        // Back to the start.
        demuxer.seek(0).unwrap();
        assert_eq!(demuxer.read(video).unwrap().unwrap().pts, 0, "{name}");
    }
}

#[test]
fn the_registry_reports_only_what_it_decodes() {
    use super::container::{CanPlay, can_play_type};
    let registry = Registry::get();
    assert_eq!(
        can_play_type("video/mp4; codecs=\"av01.0.04M.08\"", registry),
        CanPlay::Probably
    );
    assert_eq!(can_play_type("audio/mpeg", registry), CanPlay::Maybe);
    assert_eq!(
        can_play_type("audio/ogg; codecs=vorbis", registry),
        CanPlay::Probably
    );
    assert_eq!(can_play_type("audio/flac", registry), CanPlay::Maybe);
}

const H264_FRAMES: [&str; 10] = [
    "0170ed35256df553e7b271c7905ff2952f1624675c9f29f5f5a39ef267ee5e91",
    "222094df66276de5516bccc4ca91daec022da9f03e69318f4cf9010bd71a540a",
    "e318a5eeae1aa231b758b5fa5107ff01bfba3a72ea877fd60ec4a412a8356f9d",
    "7857c2526c0fc34177d92481531419bc09497373fc1f4de888724e1b5fd121d7",
    "8834e261bec1e4a51c3fa4debb562ca68ff31008d45d2586c605827538f8f963",
    "1cf68c72bfc3c6b551b802516b162c4c951c98be78602ba728528257edb83861",
    "3155d8c80b6a2524d7e4813734c3386412dd9d8a606e6f65cdd3f2d93570052c",
    "8ca540b29b70406c17546e36620dbd1a6a21d38f434b532455a56c0b9e00071b",
    "895d2503dd300fd26cc08df53c2acc5f3741366bde44554de2472764d530c515",
    "ffefe7b5689e65f73dc74d44adb168c66712da835b42636c546d227b0201388a",
];

const VP9_FRAMES: [&str; 10] = [
    "ef83df81edd85e4e9ce63cb169a739a9bedd28f4025851d5dc67768c8e0fe4d4",
    "a6798d247d30f542eaae5a23c04903f0b574205cb13866bc72eeac9e2844832b",
    "5a70666a4d9417c394cd7c79af6b18b7e4e3e6e2f6678623bf207be1065a5e43",
    "bac47432af76000db07bd5c2e81ffaaecd638c91c0cec90d98fe5abe7ed12b39",
    "a7670a27d76cdca9256c811f2e79998ff074c06518441425f06ca050e497927a",
    "a57b9b0e307431d57318654f887978615992493723270f9ec80b886ede09a736",
    "7c1337c3d6e04097b985e1a43b8086da783ce1d4126f8afd233da800277c041e",
    "f2b95e881f147db3ffc7e7ea3e4c49fee844c43e8cd600de13e1d0ff4f35afde",
    "a6430c8190d049856c5edc59affb858b3d3cf562432ec26ea9b3fd279b6ca340",
    "53184f71bb77be3fb18ef37a4b6cb6d9b1a8d1b0d39b92572773a65c8ddca36b",
];

/// The OS path: H.264/AAC from MP4 and VP9/Opus from WebM through the
/// system's decoders (FFmpeg on Linux, Media Foundation on Windows), when
/// this machine has them.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn system_decoders_match_ffmpeg() {
    use super::codec::CodecString;
    use super::container::Capabilities;
    let registry = Registry::get();
    let decodes = |codec| {
        registry.decodes(&CodecString {
            codec,
            details: Default::default(),
            ambiguous: false,
        })
    };
    for (name, codec, hashes, audio, rate) in [
        (
            "h264_aac.mp4",
            Codec::H264,
            &H264_FRAMES,
            Codec::Aac,
            44_100usize,
        ),
        (
            "vp9_opus.webm",
            Codec::Vp9,
            &VP9_FRAMES,
            Codec::Opus,
            48_000,
        ),
    ] {
        if !decodes(codec) || !decodes(audio) {
            eprintln!("skipping {name}: no system decoder");
            continue;
        }
        let frames = decode_video(name);
        assert_eq!(frames.len(), 10, "{name}");
        // Presentation order, with B-frame reordering undone.
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame_hash(frame), hashes[index], "{name} frame {index}");
            assert_eq!(frame.pts, index as i64 * 100_000, "{name} frame {index}");
        }
        let (decoded, decoded_rate, channels) = decode_audio(name);
        assert_eq!(decoded_rate as usize, rate, "{name}");
        assert_eq!(channels, 1, "{name}");
        assert!(
            decoded >= rate - 64 && decoded <= rate + 4096,
            "{name}: {decoded}"
        );
    }
}
