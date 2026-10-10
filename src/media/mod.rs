//! Inline audio and video playback (HTML #media-elements), plus the shared
//! external-player delegation the terminal frontend keeps using.
//!
//! The design follows how Firefox sources media (and Ladybird since
//! 2026-09): royalty-free formats are decoded by code bundled with TRust,
//! while H.264, AAC and HEVC come only from the operating system — Media
//! Foundation on Windows, the system FFmpeg (`libavcodec`, loaded at run time
//! and never linked) on Linux. Every answer to `canPlayType()`,
//! `MediaSource.isTypeSupported()` and the Media Capabilities API is derived
//! from the decoders actually available in this process ([`decode`]), so
//! feature detection never promises a format TRust cannot play.
//!
//! Layers, from bytes to output:
//!
//! - [`codec`]: RFC 6381 `codecs` parameter strings and the codec they name.
//! - [`container`]: MIME container types and the canPlayType decision.
//! - [`isobmff`] and [`webm`]: ISO BMFF (MP4) and Matroska/WebM parsing,
//!   shared by progressive playback and Media Source Extensions.
//! - [`demux`]: per-track packet readers for media resources.
//! - [`decode`]: the decoder registry and the bundled/system decoders.
//! - [`frame`]: decoded video frames and audio blocks.
//! - [`audio`]: audio output devices.
//!
//! Times are integer microseconds ([`Micros`]) throughout, converted from
//! container timescales with exact 128-bit arithmetic.

pub mod audio;
pub mod codec;
pub mod container;
pub mod decode;
pub mod demux;
pub mod eme;
pub mod frame;
pub mod isobmff;
pub mod mse;
pub mod player;
pub mod resource;
pub mod track;
pub mod webm;

mod external;
pub use external::*;

/// A media time or duration in microseconds.
pub type Micros = i64;

/// `TRUST_MEDIA_LOG`: media diagnostics on stderr (`media_log!`).
pub fn logging() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("TRUST_MEDIA_LOG").is_some_and(|value| value != "0"))
}

/// Print a `trust-media:` diagnostic line when `TRUST_MEDIA_LOG` is set.
#[macro_export]
macro_rules! media_log {
    ($($arg:tt)*) => {
        if $crate::media::logging() {
            eprintln!("trust-media: {}", format_args!($($arg)*));
        }
    };
}

/// Microseconds in one second.
pub const MICROS_PER_SECOND: i64 = 1_000_000;

/// Convert `value` ticks of a `timescale`-per-second clock to microseconds,
/// rounding toward negative infinity. A zero timescale yields zero.
pub fn ticks_to_micros(value: i64, timescale: u64) -> Micros {
    if timescale == 0 {
        return 0;
    }
    let scaled = i128::from(value) * i128::from(MICROS_PER_SECOND);
    let timescale = i128::from(timescale);
    let quotient = scaled.div_euclid(timescale);
    quotient.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// Convert microseconds to seconds as HTML's media timeline reports them.
pub fn micros_to_seconds(value: Micros) -> f64 {
    value as f64 / MICROS_PER_SECOND as f64
}

/// Convert seconds (a finite, possibly negative double) to microseconds,
/// rounding to the nearest microsecond and saturating at the i64 range.
pub fn seconds_to_micros(value: f64) -> Micros {
    if !value.is_finite() {
        return if value > 0.0 { i64::MAX } else { i64::MIN };
    }
    let micros = (value * MICROS_PER_SECOND as f64).round();
    if micros >= i64::MAX as f64 {
        i64::MAX
    } else if micros <= i64::MIN as f64 {
        i64::MIN
    } else {
        micros as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timescale_conversion_is_exact_and_floors() {
        assert_eq!(ticks_to_micros(90_000, 90_000), 1_000_000);
        assert_eq!(ticks_to_micros(1, 3), 333_333);
        assert_eq!(ticks_to_micros(-1, 3), -333_334);
        assert_eq!(ticks_to_micros(48_000 * 3600, 48_000), 3_600_000_000);
        assert_eq!(ticks_to_micros(i64::MAX, 1), i64::MAX);
        assert_eq!(ticks_to_micros(5, 0), 0);
    }

    #[test]
    fn seconds_round_trip() {
        assert_eq!(seconds_to_micros(1.5), 1_500_000);
        assert_eq!(seconds_to_micros(-0.0000004), 0);
        assert_eq!(seconds_to_micros(f64::INFINITY), i64::MAX);
        assert_eq!(micros_to_seconds(2_500_000), 2.5);
    }
}

#[cfg(test)]
mod pipeline_tests;
