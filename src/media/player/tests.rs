//! Playback scenarios over the media fixtures with silent audio output.

use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use super::*;
use crate::media::demux::MemorySource;

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/fixtures/media")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn open(name: &str) -> (Player, Receiver<Event>) {
    let (sender, receiver) = channel();
    let sender = std::sync::Mutex::new(sender);
    let player = Player::new(
        Input::Resource {
            source: Box::new(MemorySource::new(fixture(name))),
            hint: None,
        },
        Options {
            audio: AudioMode::Silent,
        },
        move |event| {
            let _ = sender.lock().unwrap().send(event);
        },
    );
    (player, receiver)
}

/// Wait for the first event matching `predicate`, collecting the others.
fn wait_for(
    events: &Receiver<Event>,
    seen: &mut Vec<Event>,
    timeout: Duration,
    predicate: impl Fn(&Event) -> bool,
) -> Event {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(event) if predicate(&event) => return event,
            Ok(event) => seen.push(event),
            Err(_) => panic!("timed out; saw {seen:?}"),
        }
    }
}

#[test]
fn loads_metadata_and_shows_the_first_frame_paused() {
    let (player, events) = open("av1_mp3.mp4");
    let mut seen = Vec::new();
    let metadata = wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
        matches!(e, Event::Metadata { .. })
    });
    let Event::Metadata {
        duration,
        seekable,
        tracks,
        video_size,
    } = metadata
    else {
        unreachable!()
    };
    assert_eq!(duration, Some(1_000_000));
    assert!(seekable);
    assert_eq!(video_size, Some((96, 64)));
    assert_eq!(tracks.len(), 2);
    assert!(tracks.iter().all(|t| t.active));
    wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
        *e == Event::ReadyState(ReadyState::EnoughData)
    });
    let (frame, _) = player
        .surface()
        .current()
        .expect("first frame shown while paused");
    assert_eq!(frame.pts, 0);
    assert_eq!(player.current_time(), 0);
}

#[test]
fn plays_to_the_end_in_real_time() {
    let (player, events) = open("av1_mp3.mp4");
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
        *e == Event::ReadyState(ReadyState::EnoughData)
    });
    let started = Instant::now();
    player.play();
    wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
        *e == Event::Ended
    });
    let elapsed = started.elapsed();
    // One second of media, played in real time.
    assert!(
        elapsed >= Duration::from_millis(850) && elapsed < Duration::from_millis(2500),
        "{elapsed:?}"
    );
    let time = player.current_time();
    assert!((950_000..=1_100_000).contains(&time), "{time}");
    let (frame, _) = player.surface().current().unwrap();
    assert_eq!(frame.pts, 900_000);
    let quality = player.quality();
    assert!(quality.presented >= 8, "{quality:?}");
}

#[test]
fn seeks_land_accurately() {
    let (player, events) = open("av1_mp3.mp4");
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
        *e == Event::ReadyState(ReadyState::EnoughData)
    });
    player.seek(750_000, false);
    let done = wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
        matches!(e, Event::SeekDone(_))
    });
    assert_eq!(done, Event::SeekDone(750_000));
    // The frame at 0.75 s is the one presented from 0.7 s.
    let (frame, _) = player.surface().current().unwrap();
    assert_eq!(frame.pts, 700_000);
    assert_eq!(player.current_time(), 750_000);
    // A fast seek lands on the keyframe at or before the target.
    player.seek(800_000, true);
    let done = wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
        matches!(e, Event::SeekDone(_))
    });
    assert_eq!(done, Event::SeekDone(500_000));
}

#[test]
fn double_rate_plays_twice_as_fast() {
    let (player, events) = open("av1_mp3.mp4");
    let mut seen = Vec::new();
    wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
        *e == Event::ReadyState(ReadyState::EnoughData)
    });
    player.set_rate(2.0, true);
    let started = Instant::now();
    player.play();
    wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
        *e == Event::Ended
    });
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(400) && elapsed < Duration::from_millis(1400),
        "{elapsed:?}"
    );
}

#[test]
fn audio_only_and_video_only_resources_play() {
    for name in ["sine.mp3", "av1.mp4"] {
        let (player, events) = open(name);
        let mut seen = Vec::new();
        wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
            matches!(e, Event::ReadyState(ReadyState::EnoughData))
        });
        player.play();
        wait_for(&events, &mut seen, Duration::from_secs(10), |e| {
            *e == Event::Ended
        });
        let time = player.current_time();
        assert!((900_000..=1_200_000).contains(&time), "{name}: {time}");
    }
}

#[test]
fn unusable_resources_report_src_not_supported() {
    let (sender, receiver) = channel();
    let sender = std::sync::Mutex::new(sender);
    let _player = Player::new(
        Input::Resource {
            source: Box::new(MemorySource::new(b"<html>not media</html>".to_vec())),
            hint: None,
        },
        Options {
            audio: AudioMode::Silent,
        },
        move |event| {
            let _ = sender.lock().unwrap().send(event);
        },
    );
    let mut seen = Vec::new();
    let error = wait_for(&receiver, &mut seen, Duration::from_secs(5), |e| {
        matches!(e, Event::Error(..))
    });
    assert!(matches!(error, Event::Error(ErrorKind::SrcNotSupported, _)));
}
