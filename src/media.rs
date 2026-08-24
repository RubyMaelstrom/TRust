//! Shared external-media delegation used by both native frontends.
//!
//! YouTube playback detection operates on a parsed [`url::Url`], never on a
//! substring. The WHATWG URL Standard gives host, path, and query separate
//! semantics; preserving that boundary prevents lookalike hosts and navigation
//! pages from being mistaken for videos.

use std::process::{Command, Stdio};

/// Parse `input` as an absolute or address-bar-style YouTube playback URL.
///
/// This accepts the same schemeless host form as TRust's COMMAND surface, but
/// only returns HTTP(S) URLs that [`is_youtube_video_url`] classifies as a
/// concrete playback page.
pub fn youtube_video_url(input: &str) -> Option<url::Url> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Ok(url) = url::Url::parse(input) {
        return is_youtube_video_url(&url).then_some(url);
    }
    let url = url::Url::parse(&format!("https://{input}")).ok()?;
    is_youtube_video_url(&url).then_some(url)
}

/// Whether an HTTP(S) URL is a YouTube video playback page.
///
/// Ordinary YouTube browsing remains in TRust. `/watch` requires a nonempty
/// `v` parameter, and path-shaped players require a video-id segment. The
/// short `youtu.be/<id>` host is video-specific and accepts its normal sharing
/// query parameters.
pub fn is_youtube_video_url(url: &url::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };

    match host {
        "youtu.be" | "www.youtu.be" => short_video_id(url).is_some(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            youtube_playback_path(url)
        }
        "youtube-nocookie.com" | "www.youtube-nocookie.com" => {
            path_video_id(url, "embed").is_some()
        }
        _ => false,
    }
}

fn youtube_playback_path(url: &url::Url) -> bool {
    if matches!(url.path(), "/watch" | "/watch/") {
        return url
            .query_pairs()
            .any(|(name, value)| name == "v" && valid_video_id(&value));
    }
    ["shorts", "embed", "live", "v"]
        .into_iter()
        .any(|route| path_video_id(url, route).is_some())
}

fn short_video_id(url: &url::Url) -> Option<&str> {
    let mut segments = url.path_segments()?;
    let id = segments.next()?;
    if !valid_video_id(id) || segments.any(|segment| !segment.is_empty()) {
        return None;
    }
    Some(id)
}

fn path_video_id<'a>(url: &'a url::Url, route: &str) -> Option<&'a str> {
    let mut segments = url.path_segments()?;
    if segments.next()? != route {
        return None;
    }
    let id = segments.next()?;
    if !valid_video_id(id) || segments.any(|segment| !segment.is_empty()) {
        return None;
    }
    Some(id)
}

fn valid_video_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub fn launch_mpv(url: &str, referrer: Option<&url::Url>) -> Result<(), String> {
    let mut command = Command::new("mpv");
    if let Some(referrer) = referrer {
        command.arg(format!("--referrer={referrer}"));
    }
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                String::from("mpv not found on PATH")
            } else {
                format!("mpv failed to launch: {error}")
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yt(input: &str) -> bool {
        url::Url::parse(input)
            .ok()
            .is_some_and(|url| is_youtube_video_url(&url))
    }

    #[test]
    fn youtube_video_urls_cover_documented_and_shared_players() {
        assert!(yt("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert!(yt("https://youtube.com/watch/?t=10&v=dQw4w9WgXcQ"));
        assert!(yt("https://m.youtube.com/watch?v=abc_123-XYZ"));
        assert!(yt("https://music.youtube.com/watch?v=abc"));
        assert!(yt("https://www.youtube.com/shorts/abc123?feature=share"));
        assert!(yt("https://youtube.com/live/abc123"));
        assert!(yt("https://youtube.com/v/abc123/"));
        assert!(yt("https://www.youtube.com/embed/M7lc1UVf-VE?autoplay=1"));
        assert!(yt("https://www.youtube-nocookie.com/embed/abc123"));
        assert!(yt("https://youtu.be/dQw4w9WgXcQ?si=share-token&t=43"));
        assert!(yt("https://www.youtu.be/abc123#fragment"));
    }

    #[test]
    fn youtube_navigation_and_lookalike_urls_stay_in_the_browser() {
        for input in [
            "https://youtube.com/",
            "https://www.youtube.com/results?search_query=rust",
            "https://www.youtube.com/feed/subscriptions",
            "https://www.youtube.com/channel/UC123",
            "https://www.youtube.com/@GoogleDevelopers/videos",
            "https://www.youtube.com/playlist?list=PL123",
            "https://www.youtube.com/watch",
            "https://www.youtube.com/watch?list=PL123",
            "https://www.youtube.com/watch?v=",
            "https://youtu.be/",
            "https://youtu.be/abc/extra",
            "https://www.youtube-nocookie.com/watch?v=abc",
            "https://youtube.com.evil.example/watch?v=abc",
            "https://youtube.com@evil.example/watch?v=abc",
            "https://example.com/?next=https://youtube.com/watch?v=abc",
            "ftp://youtube.com/watch?v=abc",
        ] {
            assert!(!yt(input), "must remain an ordinary navigation: {input}");
        }
    }

    #[test]
    fn address_bar_parser_normalizes_schemeless_playback_links_only() {
        assert_eq!(
            youtube_video_url("youtu.be/dQw4w9WgXcQ?si=abc")
                .as_ref()
                .map(url::Url::as_str),
            Some("https://youtu.be/dQw4w9WgXcQ?si=abc")
        );
        assert!(youtube_video_url("youtube.com/watch?v=abc").is_some());
        assert!(youtube_video_url("HTTPS://WWW.YOUTUBE.COM/watch?v=abc").is_some());
        assert!(youtube_video_url("youtube.com/results?search_query=rust").is_none());
        assert!(youtube_video_url("notyoutube.com/watch?v=abc").is_none());
    }
}
