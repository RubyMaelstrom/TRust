//! Media Source Extensions conformance tests over fragmented ISO BMFF and
//! WebM byte streams (local w3c/media-source@8e0611a and the ISO BMFF and
//! WebM byte stream formats).
//!
//! `mse_video.mp4`: AV1, 10 frames/s, 2 s, a keyframe and a movie fragment
//! every 0.5 s. `mse_audio.mp4`: FLAC, 8 kHz, 2 s, a fragment every 0.5 s.
//! `mse_av1_vorbis.webm`: AV1 and Vorbis, 2 s, a Cluster every 0.5 s.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::media::demux::Demuxer;

const VIDEO: &str = "video/mp4; codecs=\"av01.0.00M.08\"";
const AUDIO: &str = "audio/mp4; codecs=\"flac\"";
const WEBM: &str = "video/webm; codecs=\"av01.0.00M.08, vorbis\"";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/fixtures/media")
            .join(name),
    )
    .unwrap()
}

/// A fragmented MP4 file as MSE segments: the initialization segment
/// (ftyp, moov) and each media segment (moof, mdat).
fn segments(name: &str) -> (Vec<u8>, Vec<Vec<u8>>) {
    let bytes = fixture(name);
    let mut boxes = Vec::new();
    let mut at = 0;
    while at + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        boxes.push((
            bytes[at + 4..at + 8].to_vec(),
            bytes[at..at + size].to_vec(),
        ));
        at += size;
    }
    let mut init = Vec::new();
    let mut media = Vec::new();
    for (kind, data) in boxes {
        match kind.as_slice() {
            b"ftyp" | b"moov" => init.extend_from_slice(&data),
            b"moof" => media.push(data),
            b"mdat" => media.last_mut().unwrap().extend_from_slice(&data),
            _ => {}
        }
    }
    (init, media)
}

fn open_source() -> MediaSource {
    let source = MediaSource::new();
    assert!(source.attach());
    assert_eq!(source.ready_state(), ReadyState::Open);
    source
}

fn append(source: &MediaSource, id: u32, bytes: &[u8]) -> Outcome {
    source.prepare_append(id, bytes.len(), 0).unwrap();
    source.append(id, bytes).unwrap();
    source.process(id)
}

/// Ranges rounded to milliseconds.
fn ms(ranges: Vec<(f64, f64)>) -> Vec<(i64, i64)> {
    ranges
        .into_iter()
        .map(|(start, end)| {
            (
                (start * 1000.0).round() as i64,
                (end * 1000.0).round() as i64,
            )
        })
        .collect()
}

fn video_source() -> (MediaSource, u32, Vec<Vec<u8>>) {
    let source = open_source();
    let id = source.add_source_buffer(VIDEO, false).unwrap();
    let (init, media) = segments("mse_video.mp4");
    let outcome = append(&source, id, &init);
    assert_eq!(outcome.append_error, None);
    (source, id, media)
}

#[test]
fn the_initialization_segment_creates_tracks_and_reaches_metadata() {
    let source = open_source();
    assert!(source.duration().is_infinite() || source.duration().is_nan());
    let id = source.add_source_buffer(VIDEO, false).unwrap();
    let (init, _) = segments("mse_video.mp4");
    let outcome = append(&source, id, &init);
    assert_eq!(outcome.append_error, None);
    assert_eq!(outcome.tracks.len(), 1);
    let track = &outcome.tracks[0];
    assert_eq!(
        (
            track.kind,
            track.id.as_str(),
            track.kind_name,
            track.enabled
        ),
        (TrackKind::Video, "1", "main", true)
    );
    assert!(outcome.activated && outcome.metadata && outcome.duration_changed);
    // The fragmented file states no duration: positive Infinity.
    assert_eq!(source.duration(), f64::INFINITY);
    assert!(source.buffered(id).is_empty());
    // A second initialization segment is not a new set of tracks.
    let again = append(&source, id, &init);
    assert_eq!(again, Outcome::default());
}

#[test]
fn media_segments_buffer_and_end_of_stream_sets_the_duration() {
    let (source, id, media) = video_source();
    for segment in &media {
        assert_eq!(append(&source, id, segment).append_error, None);
    }
    assert_eq!(ms(source.buffered(id)), vec![(0, 2000)]);
    assert_eq!(ms(source.element_buffered()), vec![(0, 2000)]);
    let outcome = source.end_of_stream(false).unwrap();
    assert!(outcome.duration_changed);
    assert_eq!(source.ready_state(), ReadyState::Ended);
    assert!((source.duration() - 2.0).abs() < 1e-6);
    // An ended MediaSource reopens for more data.
    assert!(source.reopen());
    assert_eq!(source.ready_state(), ReadyState::Open);
    assert!(!source.end_of_stream(false).unwrap().duration_changed);
}

#[test]
fn segments_may_arrive_in_pieces() {
    let (source, id, media) = video_source();
    let all: Vec<u8> = media.concat();
    let mut parsing_media_seen = false;
    for chunk in all.chunks(97) {
        assert_eq!(append(&source, id, chunk).append_error, None);
        parsing_media_seen |= source.parsing_media(id);
    }
    assert!(parsing_media_seen);
    assert!(!source.parsing_media(id));
    assert_eq!(ms(source.buffered(id)), vec![(0, 2000)]);
}

#[test]
fn out_of_order_and_overlapping_appends() {
    let (source, id, media) = video_source();
    append(&source, id, &media[2]);
    append(&source, id, &media[0]);
    assert_eq!(ms(source.buffered(id)), vec![(0, 500), (1000, 1500)]);
    append(&source, id, &media[1]);
    assert_eq!(ms(source.buffered(id)), vec![(0, 1500)]);
    let frames = |source: &MediaSource| source.lock().buffers[0].tracks[0].frames.len();
    let before = frames(&source);
    // Appending the same media again replaces it.
    append(&source, id, &media[1]);
    assert_eq!(frames(&source), before);
    assert_eq!(ms(source.buffered(id)), vec![(0, 1500)]);
}

#[test]
fn timestamp_offset_shifts_appended_frames() {
    let (source, id, media) = video_source();
    let (reopened, result) = source.set_timestamp_offset(id, 10.0);
    assert!(!reopened && result.is_ok());
    append(&source, id, &media[0]);
    assert_eq!(ms(source.buffered(id)), vec![(10_000, 10_500)]);
    // Data beyond a finite duration extends it.
    assert!(source.duration().is_infinite());
}

#[test]
fn sequence_mode_places_segments_back_to_back() {
    let (source, id, media) = video_source();
    assert!(source.set_mode(id, AppendMode::Sequence).1.is_ok());
    append(&source, id, &media[2]);
    assert_eq!(ms(source.buffered(id)), vec![(0, 500)]);
    // The offset moved the segment from 1 s to the group start, 0.
    assert!((source.timestamp_offset(id) + 1.0).abs() < 1e-6);
    append(&source, id, &media[0]);
    // Its decode timestamps jump backwards: a new coded frame group starts
    // at the previous group's end.
    assert_eq!(ms(source.buffered(id)), vec![(0, 1000)]);
    assert!((source.timestamp_offset(id) - 0.5).abs() < 1e-6);
}

#[test]
fn the_append_window_drops_frames_outside_it() {
    let (source, id, media) = video_source();
    source.set_append_window(id, 0.0, 0.3);
    append(&source, id, &media[0]);
    assert_eq!(ms(source.buffered(id)), vec![(0, 300)]);
    // abort() resets the append window.
    source.abort(id);
    assert_eq!(source.append_window(id), (0.0, f64::INFINITY));
}

#[test]
fn removal_takes_frames_up_to_the_next_random_access_point() {
    let (source, id, media) = video_source();
    for segment in &media {
        append(&source, id, segment);
    }
    source.remove(id, 0.2, 0.7);
    assert_eq!(ms(source.buffered(id)), vec![(0, 200), (1000, 2000)]);
    source.remove(id, 1.5, f64::INFINITY);
    assert_eq!(ms(source.buffered(id)), vec![(0, 200), (1000, 1500)]);
}

#[test]
fn muxed_tracks_buffer_the_intersection() {
    let source = open_source();
    let id = source.add_source_buffer(WEBM, false).unwrap();
    let outcome = append(&source, id, &fixture("mse_av1_vorbis.webm"));
    assert_eq!(outcome.append_error, None);
    let kinds: Vec<TrackKind> = outcome.tracks.iter().map(|t| t.kind).collect();
    assert_eq!(kinds, vec![TrackKind::Video, TrackKind::Audio]);
    assert!(
        outcome
            .tracks
            .iter()
            .all(|t| t.enabled && t.kind_name == "main")
    );
    assert!(outcome.metadata);
    // The file states its duration.
    assert!(
        (source.duration() - 2.0).abs() < 0.05,
        "{}",
        source.duration()
    );
    let buffered = ms(source.buffered(id));
    assert_eq!(buffered.len(), 1, "{buffered:?}");
    assert!(buffered[0].0 <= 10 && buffered[0].1 >= 1900, "{buffered:?}");
}

#[test]
fn webm_media_segments_in_pieces_match_one_append() {
    let bytes = fixture("mse_av1_vorbis.webm");
    let whole = open_source();
    let a = whole.add_source_buffer(WEBM, false).unwrap();
    append(&whole, a, &bytes);
    let pieces = open_source();
    let b = pieces.add_source_buffer(WEBM, false).unwrap();
    for chunk in bytes.chunks(61) {
        assert_eq!(append(&pieces, b, chunk).append_error, None);
    }
    assert_eq!(ms(whole.buffered(a)), ms(pieces.buffered(b)));
}

#[test]
fn media_before_initialization_is_an_append_error() {
    let source = open_source();
    let id = source.add_source_buffer(VIDEO, false).unwrap();
    let (_, media) = segments("mse_video.mp4");
    let outcome = append(&source, id, &media[0]);
    assert!(outcome.append_error.is_some());
    // The end of stream algorithm ran with a decode error.
    assert_eq!(source.ready_state(), ReadyState::Ended);
}

#[test]
fn a_changed_set_of_tracks_is_an_append_error() {
    let (source, id, _) = video_source();
    let (audio_init, _) = segments("mse_audio.mp4");
    assert!(append(&source, id, &audio_init).append_error.is_some());
}

#[test]
fn add_source_buffer_checks_the_type_and_state() {
    let source = MediaSource::new();
    assert_eq!(
        source.add_source_buffer(VIDEO, false),
        Err(Exception::InvalidState)
    );
    assert!(source.attach());
    assert!(!source.attach());
    assert_eq!(
        source.add_source_buffer("video/x-unknown", false),
        Err(Exception::NotSupported)
    );
    assert_eq!(
        source.add_source_buffer("video/ogg", false),
        Err(Exception::NotSupported)
    );
    assert_eq!(
        source.add_source_buffer(VIDEO, true),
        Err(Exception::QuotaExceeded)
    );
    let id = source.add_source_buffer(VIDEO, false).unwrap();
    assert!(source.remove_source_buffer(id).is_ok());
    assert_eq!(source.remove_source_buffer(id), Err(Exception::NotFound));
    source.detach();
    assert_eq!(source.ready_state(), ReadyState::Closed);
    assert!(source.duration().is_nan());
}

#[test]
fn the_duration_cannot_truncate_buffered_media() {
    let (source, id, media) = video_source();
    append(&source, id, &media[0]);
    assert_eq!(source.set_duration(0.3), Err(Exception::InvalidState));
    // Above the last frame's start, below its end: the end wins.
    assert!(source.set_duration(0.45).unwrap().duration_changed);
    assert!((source.duration() - 0.5).abs() < 1e-6);
    assert!(source.set_duration(30.0).unwrap().duration_changed);
    assert_eq!(source.duration(), 30.0);
}

#[test]
fn mode_and_offset_cannot_change_inside_a_media_segment() {
    let (source, id, media) = video_source();
    source.prepare_append(id, 10, 0).unwrap();
    source.append(id, &media[0][..media[0].len() / 2]).unwrap();
    source.process(id);
    assert!(source.parsing_media(id));
    assert_eq!(
        source.set_mode(id, AppendMode::Sequence).1,
        Err(Exception::InvalidState)
    );
    assert_eq!(
        source.set_timestamp_offset(id, 1.0).1,
        Err(Exception::InvalidState)
    );
    // abort() discards the partial segment.
    source.abort(id);
    assert!(!source.parsing_media(id));
    assert!(source.set_mode(id, AppendMode::Sequence).1.is_ok());
}

#[test]
fn eviction_frees_media_before_the_playback_position() {
    let (source, id, media) = video_source();
    for segment in &media[..3] {
        append(&source, id, segment);
    }
    let held = source.memory_bytes();
    source.set_capacity(held + 100);
    // Nothing before the position can go: the append does not fit.
    assert_eq!(
        source.prepare_append(id, media[3].len(), 0),
        Err(Exception::QuotaExceeded)
    );
    // At 1.2 s, everything before the 1 s random access point can go.
    assert_eq!(source.prepare_append(id, media[3].len(), 1_200_000), Ok(()));
    assert_eq!(ms(source.buffered(id)), vec![(1000, 1500)]);
}

#[test]
fn the_demuxer_reads_appended_frames_and_waits_for_more() {
    let (source, id, media) = video_source();
    let mut demuxer = source.demuxer();
    demuxer.wait_for_tracks(&AtomicBool::new(false)).unwrap();
    let track = demuxer.tracks()[0].id;
    assert_eq!(demuxer.tracks()[0].kind, TrackKind::Video);
    assert!(matches!(demuxer.read(track), Err(demux::Error::Pending)));
    append(&source, id, &media[0]);
    let mut times = Vec::new();
    loop {
        match demuxer.read(track) {
            Ok(Some(packet)) => {
                if times.is_empty() {
                    assert!(packet.keyframe && packet.config.is_some());
                } else {
                    assert!(packet.config.is_none());
                }
                times.push(packet.pts / 1000);
            }
            Err(demux::Error::Pending) => break,
            other => panic!("{:?}", other.map(|p| p.map(|p| p.pts))),
        }
    }
    assert_eq!(times, vec![0, 100, 200, 300, 400]);
    append(&source, id, &media[1]);
    assert_eq!(demuxer.read(track).unwrap().unwrap().pts, 500_000);
    // Seeking to unbuffered media waits for it, then starts at the random
    // access point before the target.
    assert_eq!(demuxer.seek(1_200_000).unwrap(), 1_200_000);
    assert!(matches!(demuxer.read(track), Err(demux::Error::Pending)));
    append(&source, id, &media[2]);
    let packet = demuxer.read(track).unwrap().unwrap();
    assert_eq!((packet.pts, packet.keyframe), (1_000_000, true));
    // A new initialization segment reaches the reader as a configuration.
    let (init, _) = segments("mse_video.mp4");
    append(&source, id, &init);
    append(&source, id, &media[3]);
    let mut configured = false;
    while let Ok(Some(packet)) = demuxer.read(track) {
        if packet.pts == 1_500_000 {
            configured = packet.config.is_some();
        }
        if packet.pts >= 1_900_000 {
            break;
        }
    }
    assert!(configured);
    // endOfStream(): the end.
    source.end_of_stream(false).unwrap();
    assert!(matches!(demuxer.read(track), Ok(None)));
}

#[test]
fn a_gap_stops_the_reader_until_it_is_filled() {
    let (source, id, media) = video_source();
    append(&source, id, &media[0]);
    append(&source, id, &media[2]);
    let mut demuxer = source.demuxer();
    demuxer.wait_for_tracks(&AtomicBool::new(false)).unwrap();
    let track = demuxer.tracks()[0].id;
    let mut last = 0;
    while let Ok(Some(packet)) = demuxer.read(track) {
        last = packet.pts;
    }
    assert_eq!(last, 400_000);
    append(&source, id, &media[1]);
    assert_eq!(demuxer.read(track).unwrap().unwrap().pts, 500_000);
}

#[test]
fn detaching_ends_a_wait_for_tracks() {
    let source = open_source();
    source.add_source_buffer(VIDEO, false).unwrap();
    let mut demuxer = source.demuxer();
    let waiter =
        std::thread::spawn(move || demuxer.wait_for_tracks(&AtomicBool::new(false)).is_err());
    std::thread::sleep(std::time::Duration::from_millis(50));
    source.detach();
    assert!(waiter.join().unwrap());
}

#[test]
fn separate_audio_and_video_buffers_play_to_the_end() {
    use crate::media::player::{AudioMode, Event, Input, Options, Player};
    let source = open_source();
    let video = source.add_source_buffer(VIDEO, false).unwrap();
    let audio = source.add_source_buffer(AUDIO, false).unwrap();
    let (video_init, video_media) = segments("mse_video.mp4");
    let (audio_init, audio_media) = segments("mse_audio.mp4");
    let (sender, receiver) = std::sync::mpsc::channel();
    let sender = std::sync::Mutex::new(sender);
    let player = Player::new(
        Input::Demuxer(Box::new(source.demuxer())),
        Options {
            audio: AudioMode::Silent,
        },
        move |event| {
            let _ = sender.lock().unwrap().send(event);
        },
    );
    // Metadata waits for both initialization segments.
    assert!(!append(&source, video, &video_init).metadata);
    assert!(append(&source, audio, &audio_init).metadata);
    player.play();
    // Media arrives while the player waits for it.
    for (v, a) in video_media.iter().zip(&audio_media) {
        std::thread::sleep(std::time::Duration::from_millis(30));
        append(&source, video, v);
        append(&source, audio, a);
    }
    source.end_of_stream(false).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut metadata = None;
    loop {
        match receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Event::Metadata { tracks, .. }) => metadata = Some(tracks.len()),
            Ok(Event::Ended) => break,
            Ok(Event::Error(kind, message)) => panic!("{kind:?}: {message}"),
            Ok(_) => {}
            Err(error) => panic!("no end: {error}"),
        }
    }
    assert_eq!(metadata, Some(2));
    assert!(
        player.current_time() >= 1_900_000,
        "{}",
        player.current_time()
    );
}

/// Replay a `TRUST_MEDIA_DUMP` byte stream (`TRUST_MSE_REPLAY=path` and
/// `TRUST_MSE_REPLAY_TYPE=mime`) through a SourceBuffer and the decoders,
/// printing each packet and decode result (diagnostics).
#[test]
#[ignore]
fn replay_dumped_byte_stream() {
    let Some(path) = std::env::var_os("TRUST_MSE_REPLAY") else {
        return;
    };
    let mime = std::env::var("TRUST_MSE_REPLAY_TYPE").unwrap_or_else(|_| String::from("video/mp4"));
    let source = open_source();
    let id = source.add_source_buffer(&mime, false).unwrap();
    let outcome = append(&source, id, &std::fs::read(path).unwrap());
    println!("outcome {outcome:?}");
    println!("buffered {:?}", source.buffered(id));
    source.end_of_stream(false).unwrap();
    let mut demuxer = source.demuxer();
    demuxer.wait_for_tracks(&AtomicBool::new(false)).unwrap();
    let registry = crate::media::decode::Registry::get();
    for track in demuxer.tracks().to_vec() {
        println!(
            "track {} {:?} {} config {} bytes",
            track.id,
            track.kind,
            track.codec_string,
            track.config.len()
        );
        let mut video =
            (track.kind == TrackKind::Video).then(|| registry.video_decoder(&track).unwrap());
        let mut audio =
            (track.kind == TrackKind::Audio).then(|| registry.audio_decoder(&track).unwrap());
        let mut count = 0;
        while let Ok(Some(packet)) = demuxer.read(track.id) {
            let result = match (&mut video, &mut audio) {
                (Some(decoder), _) => decoder.decode(&packet).map(|frames| frames.len()),
                (_, Some(decoder)) => decoder.decode(&packet).map(|blocks| blocks.len()),
                _ => Ok(0),
            };
            if count < 12 || result.is_err() {
                println!(
                    "packet pts {} dts {} size {} key {} config {} -> {:?}",
                    packet.pts,
                    packet.dts,
                    packet.data.len(),
                    packet.keyframe,
                    packet.config.is_some(),
                    result.map_err(|e| e.to_string())
                );
            }
            count += 1;
        }
        println!("{count} packets");
    }
}

/// `mse_video_cenc.mp4`: `mse_video.mp4` encrypted with ISO Common
/// Encryption's `cenc` scheme under KEY and KID, with a Common SystemID
/// pssh box in its moov.
const KEY: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];
const KID: [u8; 16] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
];

fn read_all(source: &MediaSource) -> Vec<Packet> {
    let mut demuxer = source.demuxer();
    demuxer.wait_for_tracks(&AtomicBool::new(false)).unwrap();
    let track = demuxer.tracks()[0].id;
    let mut packets = Vec::new();
    while let Ok(Some(packet)) = demuxer.read(track) {
        packets.push(packet);
    }
    packets
}

#[test]
fn encrypted_segments_report_init_data_and_decrypt_to_the_clear_stream() {
    use crate::media::eme::{ClearKey, KeyRing};
    let encrypted = open_source();
    let id = encrypted.add_source_buffer(VIDEO, false).unwrap();
    let (init, media) = segments("mse_video_cenc.mp4");
    let outcome = append(&encrypted, id, &init);
    assert_eq!(outcome.init_data.len(), 1);
    let (kind, data) = &outcome.init_data[0];
    assert_eq!(kind, "cenc");
    // Clear Key reads the key ID from the Common SystemID pssh.
    let (_, request) = ClearKey::new().generate_request(kind, data).unwrap();
    assert!(
        String::from_utf8(request)
            .unwrap()
            .contains(&crate::media::eme::base64url_encode(&KID))
    );
    for segment in &media {
        assert_eq!(append(&encrypted, id, segment).append_error, None);
    }
    encrypted.end_of_stream(false).unwrap();
    let (clear, clear_id, clear_media) = video_source();
    for segment in &clear_media {
        append(&clear, clear_id, segment);
    }
    clear.end_of_stream(false).unwrap();
    let ring = KeyRing::default();
    let locked = read_all(&encrypted);
    assert!(
        locked
            .iter()
            .all(|packet| packet.encryption.as_ref().is_some_and(|e| e.kid == KID))
    );
    assert!(matches!(
        ring.decrypt(&locked[0]),
        Err(crate::media::eme::DecryptError::NoKey)
    ));
    ring.insert(1, &[(KID.to_vec(), KEY)]);
    let plain = read_all(&clear);
    assert_eq!(locked.len(), plain.len());
    for (locked, plain) in locked.iter().zip(&plain) {
        let unlocked = ring.decrypt(locked).unwrap();
        assert_eq!(unlocked.data, plain.data, "packet at {}", plain.pts);
        assert!(unlocked.encryption.is_none());
    }
}

#[test]
fn encrypted_playback_waits_for_its_key() {
    use crate::media::player::{AudioMode, Event, Input, Options, Player};
    let source = open_source();
    let id = source.add_source_buffer(VIDEO, false).unwrap();
    let (init, media) = segments("mse_video_cenc.mp4");
    let (sender, receiver) = std::sync::mpsc::channel();
    let sender = std::sync::Mutex::new(sender);
    let player = Player::new(
        Input::Demuxer(Box::new(source.demuxer())),
        Options {
            audio: AudioMode::Silent,
        },
        move |event| {
            let _ = sender.lock().unwrap().send(event);
        },
    );
    append(&source, id, &init);
    for segment in &media {
        append(&source, id, segment);
    }
    source.end_of_stream(false).unwrap();
    player.play();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let next = || {
        receiver
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("events")
    };
    loop {
        match next() {
            Event::WaitingForKey => break,
            Event::Ended => panic!("encrypted media played without a key"),
            Event::Error(kind, message) => panic!("{kind:?}: {message}"),
            _ => {}
        }
    }
    let ring = crate::media::eme::KeyRing::default();
    player.set_keys(Some(ring.clone()));
    ring.insert(7, &[(KID.to_vec(), KEY)]);
    loop {
        match next() {
            Event::Ended => break,
            Event::Error(kind, message) => panic!("{kind:?}: {message}"),
            _ => {}
        }
    }
    assert!(player.current_time() >= 1_900_000);
    assert!(player.quality().presented >= 15, "{:?}", player.quality());
}
