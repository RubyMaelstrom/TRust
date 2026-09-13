//! Top-level navigation download classification and bounded file transfer.
//!
//! WHATWG HTML's navigation algorithm keeps the active document in place when
//! a response is an attachment or is handed to a download/external handler.
//! This module is the shared terminal/desktop boundary for that decision. MIME
//! computation follows the MIME Sniffing Standard's browsing-context rules;
//! RFC 6266 supplies attachment and suggested-filename semantics.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use url::Url;

use crate::http;

/// Disk-use ceiling for one background transfer. Downloads do not consume the
/// much smaller in-memory page-body budget, but remain bounded.
const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const SNIFF_BYTES: usize = 1445;

#[derive(Clone, Debug)]
pub struct DownloadOffer {
    pub url: Url,
    pub content_type: String,
    pub suggested_filename: String,
    pub content_length: Option<u64>,
    pub body: Vec<u8>,
    pub referrer: Option<Url>,
    pub(crate) fetch_body: bool,
    pub(crate) gopher: Option<crate::gopher::GopherUrl>,
}

impl DownloadOffer {
    /// Offer bytes already owned by a protocol page. Saving this snapshot
    /// never opens another connection, including when the snapshot is empty.
    pub fn from_bytes(url: Url, suggested_filename: String, body: Vec<u8>) -> Self {
        Self {
            url,
            content_type: "text/plain".into(),
            suggested_filename,
            content_length: Some(body.len() as u64),
            body,
            referrer: None,
            fetch_body: false,
            gopher: None,
        }
    }

    /// Stream a Gopher download after Save/Open is chosen. A bounded type
    /// check may already have inspected its prefix without retaining the file.
    /// Keep the opaque target independently of the generic URI display record.
    pub fn from_gopher(target: crate::gopher::GopherUrl) -> Result<Self, String> {
        target.request()?;
        let mut name = target
            .filename()
            .chars()
            .filter(|c| !c.is_control() && !matches!(c, '/' | '\\'))
            .take(180)
            .collect::<String>();
        if name.is_empty() || name == "." || name == ".." {
            name = "gopher-download.bin".into();
        }
        Ok(Self {
            url: Url::parse(&target.to_string()).map_err(|e| e.to_string())?,
            content_type: "application/octet-stream".into(),
            suggested_filename: name,
            content_length: None,
            body: Vec::new(),
            referrer: None,
            fetch_body: true,
            gopher: Some(target),
        })
    }

    pub fn from_response(mut response: http::Response, referrer: Option<Url>) -> Self {
        let content_type = computed_mime_type(&response);
        let suggested_filename = suggested_filename(&response, &content_type);
        let content_length =
            header(&response.headers, "content-length").and_then(|value| value.parse().ok());
        // Top-level GET responses may have been deliberately stopped after
        // their headers so the file never enters the page-body memory budget.
        // Never replay a POST as a GET merely because its response was empty.
        let fetch_body =
            response.body.is_empty() && !response.from_post && !matches!(content_length, Some(0));
        Self {
            url: response.url,
            content_type,
            suggested_filename,
            content_length,
            body: std::mem::take(&mut response.body),
            referrer,
            fetch_body,
            gopher: None,
        }
    }

    pub fn summary(&self) -> String {
        match self.content_length {
            Some(length) => format!(
                "{} · {} · {}",
                self.suggested_filename,
                self.content_type,
                human_bytes(length)
            ),
            None => format!("{} · {}", self.suggested_filename, self.content_type),
        }
    }
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Whether a top-level response must bypass document parsing. RFC 6266 §4.2
/// gives attachment precedence even when TRust could otherwise render it.
pub fn response_needs_download(response: &http::Response, supports_images: bool) -> bool {
    content_disposition(response)
        .is_some_and(|disposition| !disposition.eq_ignore_ascii_case("inline"))
        || !mime_is_renderable(&computed_mime_type(response), supports_images)
}

/// Types for which TRust has a safe top-level presentation model. HTML's
/// "loading a document" algorithm treats JavaScript, JSON, CSS, plain text,
/// VTT and XML as text/XML documents; TRust displays their source as text.
pub fn mime_is_renderable(mime: &str, supports_images: bool) -> bool {
    let essence = essence(mime);
    essence == "text/html"
        || essence == "application/xhtml+xml"
        || essence == "application/xml"
        || essence == "text/xml"
        || essence.ends_with("+xml")
        || essence == "application/json"
        || essence == "text/json"
        || essence.ends_with("+json")
        || essence.starts_with("text/")
        || is_javascript_mime(essence)
        || (supports_images && essence.starts_with("image/"))
}

/// Compute the browsing-context MIME type. This implements the decision points
/// that affect document-vs-download dispatch from MIME Sniffing §§5 and 7:
/// HTML/XML are authoritative, missing/unknown values are identified from the
/// bounded resource header, exact legacy Apache text/plain defaults are tested
/// for binary bytes, and otherwise the supplied type is retained.
pub fn computed_mime_type(response: &http::Response) -> String {
    let supplied = parse_mime(&response.content_type);
    let supplied_essence = supplied.as_deref().map(essence);
    if supplied_essence.is_some_and(|mime| {
        mime == "text/html"
            || mime == "application/xhtml+xml"
            || mime == "application/xml"
            || mime == "text/xml"
            || mime.ends_with("+xml")
    }) {
        return supplied.unwrap();
    }

    let no_sniff = header(&response.headers, "x-content-type-options")
        .is_some_and(|value| value.eq_ignore_ascii_case("nosniff"));
    if supplied_essence
        .is_none_or(|mime| matches!(mime, "unknown/unknown" | "application/unknown" | "*/*"))
    {
        return identify_unknown(&response.body, !no_sniff).to_string();
    }
    if no_sniff {
        return supplied.unwrap();
    }
    if matches!(
        response.content_type.as_str(),
        "text/plain"
            | "text/plain; charset=ISO-8859-1"
            | "text/plain; charset=iso-8859-1"
            | "text/plain; charset=UTF-8"
    ) {
        return distinguish_text_or_binary(&response.body).to_string();
    }
    supplied.unwrap()
}

pub(crate) fn identify_unknown(bytes: &[u8], sniff_scriptable: bool) -> &'static str {
    let bytes = &bytes[..bytes.len().min(SNIFF_BYTES)];
    let trimmed = bytes
        .iter()
        .position(|byte| !matches!(byte, b'\t' | b'\n' | 0x0c | b'\r' | b' '))
        .map_or(bytes, |start| &bytes[start..]);
    if sniff_scriptable {
        let ascii = trimmed
            .iter()
            .take(32)
            .map(u8::to_ascii_lowercase)
            .collect::<Vec<_>>();
        if ascii.starts_with(b"<!doctype html")
            || ascii.starts_with(b"<html")
            || ascii.starts_with(b"<head")
            || ascii.starts_with(b"<script")
            || ascii.starts_with(b"<iframe")
            || ascii.starts_with(b"<h1")
            || ascii.starts_with(b"<div")
            || ascii.starts_with(b"<font")
            || ascii.starts_with(b"<table")
            || ascii.starts_with(b"<a")
            || ascii.starts_with(b"<style")
            || ascii.starts_with(b"<title")
            || ascii.starts_with(b"<b")
            || ascii.starts_with(b"<body")
            || ascii.starts_with(b"<br")
            || ascii.starts_with(b"<p")
            || ascii.starts_with(b"<!--")
        {
            return "text/html";
        }
        if trimmed.starts_with(b"<?xml") {
            return "text/xml";
        }
        if trimmed.starts_with(b"%PDF-") {
            return "application/pdf";
        }
    }
    if let Some(image) = crate::img::sniff(bytes)
        && image != "image/svg+xml"
    {
        return image;
    }
    if bytes.starts_with(b"OggS\0") {
        return "application/ogg";
    }
    if bytes.starts_with(b"ID3") || looks_like_mp3_frame(bytes) {
        return "audio/mpeg";
    }
    if bytes.starts_with(b"fLaC") {
        return "audio/flac";
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        return "audio/wave";
    }
    if bytes.starts_with(b"MThd") {
        return "audio/midi";
    }
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return "video/webm";
    }
    if bytes.len() >= 12 && bytes.get(4..8) == Some(b"ftyp") {
        return "video/mp4";
    }
    if bytes.starts_with(&[0x1f, 0x8b, 0x08]) {
        return "application/x-gzip";
    }
    if bytes.starts_with(b"PK\x03\x04") {
        return "application/zip";
    }
    if bytes.starts_with(b"Rar!\x1a\x07\0") {
        return "application/x-rar-compressed";
    }
    distinguish_text_or_binary(bytes)
}

fn looks_like_mp3_frame(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && bytes[0] == 0xff
        && bytes[1] & 0xe0 == 0xe0
        && bytes[1] & 0x18 != 0x08
        && bytes[2] & 0xf0 != 0xf0
        && bytes[2] & 0x0c != 0x0c
}

fn distinguish_text_or_binary(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xfe, 0xff])
        || bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[0xef, 0xbb, 0xbf])
        || !bytes
            .iter()
            .take(SNIFF_BYTES)
            .any(|byte| matches!(*byte, 0x00..=0x08 | 0x0b | 0x0e..=0x1a | 0x1c..=0x1f))
    {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

fn parse_mime(value: &str) -> Option<String> {
    let value = value.trim();
    let (raw_essence, parameters) = value.split_once(';').unwrap_or((value, ""));
    let essence = raw_essence.trim().to_ascii_lowercase();
    let (kind, subtype) = essence.split_once('/')?;
    if kind.is_empty()
        || subtype.is_empty()
        || kind.bytes().any(|byte| !mime_token(byte))
        || subtype.bytes().any(|byte| !mime_token(byte))
    {
        return None;
    }
    Some(if parameters.is_empty() {
        essence
    } else {
        format!("{essence};{parameters}")
    })
}

fn mime_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn essence(value: &str) -> &str {
    value.split(';').next().unwrap_or("").trim()
}

fn is_javascript_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/ecmascript"
            | "application/javascript"
            | "application/x-ecmascript"
            | "application/x-javascript"
            | "text/ecmascript"
            | "text/javascript"
            | "text/javascript1.0"
            | "text/javascript1.1"
            | "text/javascript1.2"
            | "text/javascript1.3"
            | "text/javascript1.4"
            | "text/javascript1.5"
            | "text/jscript"
            | "text/livescript"
            | "text/x-ecmascript"
            | "text/x-javascript"
    )
}

/// MIME Sniffing #minimize-a-supported-mime-type: timing exposes a processing
/// category, never MIME parameters or an arbitrary server-selected subtype.
pub(crate) fn minimized_mime_type(value: &str) -> String {
    let Some(mime) = parse_mime(value) else {
        return String::new();
    };
    let mime = essence(&mime);
    if is_javascript_mime(mime) {
        return String::from("text/javascript");
    }
    if matches!(mime, "application/json" | "text/json") || mime.ends_with("+json") {
        return String::from("application/json");
    }
    if mime == "image/svg+xml" {
        return mime.to_string();
    }
    if matches!(mime, "application/xml" | "text/xml") || mime.ends_with("+xml") {
        return String::from("application/xml");
    }
    if mime_is_renderable(mime, false)
        || mime == "application/wasm"
        || matches!(
            mime,
            "image/png"
                | "image/jpeg"
                | "image/gif"
                | "image/webp"
                | "image/x-icon"
                | "image/vnd.microsoft.icon"
                | "font/woff"
                | "font/woff2"
                | "font/ttf"
                | "font/otf"
                | "application/font-woff"
                | "application/x-font-ttf"
                | "application/x-font-opentype"
        )
    {
        return mime.to_string();
    }
    String::new()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .rev()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn content_disposition(response: &http::Response) -> Option<String> {
    let value = header(&response.headers, "content-disposition")?;
    let first = disposition_parts(value).into_iter().next()?;
    let disposition = first.trim().to_ascii_lowercase();
    (!disposition.is_empty()).then_some(disposition)
}

fn suggested_filename(response: &http::Response, mime: &str) -> String {
    let from_header = header(&response.headers, "content-disposition").and_then(|value| {
        let parts = disposition_parts(value);
        let mut plain = None;
        let mut extended = None;
        for parameter in parts.into_iter().skip(1) {
            let Some((name, value)) = parameter.split_once('=') else {
                continue;
            };
            let name = name.trim();
            let value = unquote(value.trim());
            if name.eq_ignore_ascii_case("filename*") {
                extended = decode_extended_filename(&value);
            } else if name.eq_ignore_ascii_case("filename") {
                plain = Some(value);
            }
        }
        extended.or(plain)
    });
    let candidate = from_header
        .or_else(|| {
            response
                .url
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .filter(|name| !name.is_empty())
                .map(percent_decode)
        })
        .unwrap_or_else(|| String::from("download"));
    sanitize_filename(&candidate, mime)
}

fn disposition_parts(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if quoted && character == '\\' {
            current.push(character);
            escaped = true;
        } else if character == '"' {
            current.push(character);
            quoted = !quoted;
        } else if character == ';' && !quoted {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(character);
        }
    }
    parts.push(current);
    parts
}

fn unquote(value: &str) -> String {
    let Some(value) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.to_string();
    };
    let mut out = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            out.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            out.push(character);
        }
    }
    out
}

fn decode_extended_filename(value: &str) -> Option<String> {
    let (charset, tail) = value.split_once('\'')?;
    let (_, encoded) = tail.split_once('\'')?;
    let bytes = percent_decode_bytes(encoded);
    if charset.eq_ignore_ascii_case("utf-8") {
        String::from_utf8(bytes).ok()
    } else if charset.eq_ignore_ascii_case("iso-8859-1") {
        Some(bytes.into_iter().map(char::from).collect())
    } else {
        None
    }
}

fn percent_decode(value: &str) -> String {
    String::from_utf8_lossy(&percent_decode_bytes(value)).into_owned()
}

fn percent_decode_bytes(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn sanitize_filename(candidate: &str, mime: &str) -> String {
    let leaf = candidate.rsplit(['/', '\\']).next().unwrap_or("");
    let mut name: String = leaf
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '/' | '\\' | '|' | ':' | '*') {
                '_'
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .chars()
        .take(240)
        .collect();
    if name.is_empty() || matches!(name.as_str(), "." | ".." | "~") {
        name = String::from("download");
    }
    if let Some(extension) = extension_for_mime(essence(mime)) {
        let matches = Path::new(&name)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(extension));
        if !matches {
            name.push('.');
            name.push_str(extension);
        }
    }
    name
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "application/pdf" | "text/pdf" => "pdf",
        "application/zip" => "zip",
        "application/x-gzip" => "gz",
        "audio/mpeg" => "mp3",
        "audio/flac" => "flac",
        "audio/wave" => "wav",
        "audio/midi" => "mid",
        "application/ogg" => "ogg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "application/json" | "text/json" => "json",
        "text/plain" => "txt",
        _ => return None,
    })
}

/// A unique user-visible destination. RFC 6266 filenames are advisory; this
/// never trusts directory components and never overwrites an existing file.
pub fn save_destination(filename: &str) -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&base).map_err(|error| error.to_string())?;
    unique_path(&base, filename)
}

pub fn open_destination(filename: &str) -> Result<PathBuf, String> {
    let base = std::env::temp_dir().join(format!("trust-open-{}", std::process::id()));
    std::fs::create_dir_all(&base).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    unique_path(&base, filename)
}

fn unique_path(directory: &Path, filename: &str) -> Result<PathBuf, String> {
    let filename = sanitize_filename(filename, "");
    let path = directory.join(&filename);
    if !path.exists() {
        return Ok(path);
    }
    let file = Path::new(&filename);
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let extension = file.extension().and_then(|s| s.to_str());
    for copy in 1..10_000 {
        let candidate = match extension {
            Some(extension) => directory.join(format!("{stem} ({copy}).{extension}")),
            None => directory.join(format!("{stem} ({copy})")),
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(String::from("could not choose a unique download filename"))
}

/// Save a buffered response or stream a fresh authenticated GET directly to a
/// `.part` file. Publishing the completed file is the only point at which the
/// final name appears.
/// RFC 1436 appendix: binary data ends at EOF, with no dot unstuffing.
async fn stream_gopher(target: &crate::gopher::GopherUrl, path: &Path) -> Result<u64, String> {
    let mut stream = crate::gopher::connect(target).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|e| e.to_string())?;
    let mut count = 0u64;
    let mut bytes = [0; 65536];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1800);
    loop {
        let idle = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let n = tokio::time::timeout_at(deadline.min(idle), stream.read(&mut bytes))
            .await
            .map_err(|_| "Gopher download timed out")?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count > MAX_DOWNLOAD_BYTES {
            return Err("Download exceeds 2 GiB limit".into());
        }
        file.write_all(&bytes[..n])
            .await
            .map_err(|e| e.to_string())?;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())?;
    Ok(count)
}

pub async fn save(offer: &DownloadOffer, destination: &Path) -> Result<u64, String> {
    static PART_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let part_id = PART_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let partial = destination.with_file_name(format!(
        ".{}.part-{}-{part_id}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("download"),
        std::process::id()
    ));
    let result = if offer.fetch_body {
        if let Some(target) = &offer.gopher {
            stream_gopher(target, &partial).await
        } else {
            stream_get(&offer.url, offer.referrer.as_ref(), &partial).await
        }
    } else {
        if offer.body.len() as u64 > MAX_DOWNLOAD_BYTES {
            return Err(String::from("download exceeds 2 GiB limit"));
        }
        async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)
                .await
                .map_err(|error| error.to_string())?;
            file.write_all(&offer.body)
                .await
                .map_err(|error| error.to_string())?;
            file.flush().await.map_err(|error| error.to_string())?;
            file.sync_all().await.map_err(|error| error.to_string())?;
            Ok(offer.body.len() as u64)
        }
        .await
    };
    match result {
        Ok(bytes) => {
            // Publish only when the final name is still absent. Unlike Unix
            // rename, hard_link can never overwrite a file created after the
            // prompt chose its destination.
            if let Err(error) = tokio::fs::hard_link(&partial, destination).await {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(error.to_string());
            }
            // The destination is already a complete hard link. Cleanup failure
            // must not turn a successful Save/Open into a false failure.
            let _ = tokio::fs::remove_file(&partial).await;
            Ok(bytes)
        }
        Err(error) => {
            let _ = tokio::fs::remove_file(&partial).await;
            Err(error)
        }
    }
}

/// Launch a completed local file without a shell. The external application is
/// reached only after the user selected Open in a frontend prompt.
pub fn open_external(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut command = Command::new("xdg-open");
    command
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open {}: {error}", path.display()))
}

async fn stream_get(url: &Url, referrer: Option<&Url>, partial: &Path) -> Result<u64, String> {
    let mut current = url.clone();
    for _ in 0..=10 {
        if !matches!(current.scheme(), "http" | "https") {
            return Err(format!(
                "download redirect uses unsupported {} scheme",
                current.scheme()
            ));
        }
        let host = current.host_str().ok_or("download URL has no host")?;
        let port = current.port_or_known_default().unwrap_or(80);
        let mut io = http::download_connection(&current).await?;
        let mut path = current.path().to_string();
        if let Some(query) = current.query() {
            path.push('?');
            path.push_str(query);
        }
        let host_header = match (current.scheme(), port) {
            ("http", 80) | ("https", 443) => host.to_string(),
            _ => format!("{host}:{port}"),
        };
        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: {}\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
            http::USER_AGENT
        );
        let cookies = http::download_cookies(&current);
        if !cookies.is_empty() {
            request.push_str(&format!("Cookie: {cookies}\r\n"));
        }
        if let Some(referrer) =
            referrer.and_then(|source| http::download_referrer(source, &current))
        {
            request.push_str(&format!("Referer: {referrer}\r\n"));
        }
        request.push_str("\r\n");
        io.write_all(request.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        io.flush().await.map_err(|error| error.to_string())?;

        let status_line = download_line(&mut io).await?;
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .ok_or_else(|| format!("malformed download response: {status_line:?}"))?;
        let mut headers = std::collections::HashMap::new();
        loop {
            let line = download_line(&mut io).await?;
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_string();
                if name == "set-cookie" {
                    http::download_store_cookie(&current, &value);
                }
                headers.insert(name, value);
            }
        }
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let location = headers
                .get("location")
                .ok_or_else(|| format!("HTTP {status} redirect without Location"))?;
            current = current
                .join(location)
                .map_err(|error| format!("bad download redirect: {error}"))?;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(format!("download returned HTTP {status}"));
        }
        if headers
            .get("content-encoding")
            .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
        {
            return Err(String::from(
                "server encoded a download despite requesting identity",
            ));
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(partial)
            .await
            .map_err(|error| error.to_string())?;
        let bytes = if headers
            .get("transfer-encoding")
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
        {
            stream_chunked(&mut io, &mut file).await?
        } else if let Some(length) = headers
            .get("content-length")
            .and_then(|value| value.parse::<u64>().ok())
        {
            if length > MAX_DOWNLOAD_BYTES {
                return Err(String::from("download exceeds 2 GiB limit"));
            }
            stream_exact(&mut io, &mut file, length).await?
        } else {
            stream_eof(&mut io, &mut file).await?
        };
        file.flush().await.map_err(|error| error.to_string())?;
        file.sync_all().await.map_err(|error| error.to_string())?;
        return Ok(bytes);
    }
    Err(String::from("too many download redirects"))
}

async fn download_line<R: tokio::io::AsyncBufRead + Unpin>(io: &mut R) -> Result<String, String> {
    let mut bytes = Vec::new();
    io.read_until(b'\n', &mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    while bytes
        .last()
        .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
    {
        bytes.pop();
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn stream_exact<R: tokio::io::AsyncRead + Unpin>(
    io: &mut R,
    file: &mut tokio::fs::File,
    length: u64,
) -> Result<u64, String> {
    let mut remaining = length;
    let mut buffer = vec![0; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let read = io
            .read(&mut buffer[..want])
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err(String::from("download ended before Content-Length"));
        }
        file.write_all(&buffer[..read])
            .await
            .map_err(|error| error.to_string())?;
        remaining -= read as u64;
    }
    Ok(length)
}

async fn stream_eof<R: tokio::io::AsyncRead + Unpin>(
    io: &mut R,
    file: &mut tokio::fs::File,
) -> Result<u64, String> {
    let mut total = 0u64;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = io
            .read(&mut buffer)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(total);
        }
        total = total.saturating_add(read as u64);
        if total > MAX_DOWNLOAD_BYTES {
            return Err(String::from("download exceeds 2 GiB limit"));
        }
        file.write_all(&buffer[..read])
            .await
            .map_err(|error| error.to_string())?;
    }
}

async fn stream_chunked<R: tokio::io::AsyncBufRead + Unpin>(
    io: &mut R,
    file: &mut tokio::fs::File,
) -> Result<u64, String> {
    let mut total = 0u64;
    loop {
        let line = download_line(io).await?;
        let size = u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| String::from("malformed download chunk size"))?;
        if size == 0 {
            loop {
                if download_line(io).await?.is_empty() {
                    return Ok(total);
                }
            }
        }
        total = total.saturating_add(size);
        if total > MAX_DOWNLOAD_BYTES {
            return Err(String::from("download exceeds 2 GiB limit"));
        }
        stream_exact(io, file, size).await?;
        let mut crlf = [0; 2];
        io.read_exact(&mut crlf)
            .await
            .map_err(|error| error.to_string())?;
        if crlf != *b"\r\n" {
            return Err(String::from("malformed download chunk terminator"));
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn gopher_binary_download_keeps_dot_lines_and_opaque_selector() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = crate::gopher::GopherUrl::parse(&format!(
            "gopher://127.0.0.1:{}/9/a/../%FF.bin",
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while stream.read_exact(&mut byte).await.is_ok() {
                request.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            assert_eq!(request, b"/a/../\xff.bin\r\n");
            stream
                .write_all(b"\0binary\r\n.\r\n..dots\xff")
                .await
                .unwrap();
        });
        let offer = super::DownloadOffer::from_gopher(url).unwrap();
        let path =
            std::env::temp_dir().join(format!("trust-gopher-binary-{}.bin", std::process::id()));
        let _ = std::fs::remove_file(&path);
        super::save(&offer, &path).await.unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\0binary\r\n.\r\n..dots\xff"
        );
        std::fs::remove_file(path).unwrap();
        server.await.unwrap();
    }

    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "trust-download-test-{}-{}-{name}",
            std::process::id(),
            ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn response(content_type: &str, body: &[u8], headers: Vec<(String, String)>) -> http::Response {
        http::Response {
            url: Url::parse("https://example.test/files/report").unwrap(),
            status: 200,
            content_type: content_type.into(),
            headers,
            body: body.to_vec(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        }
    }

    #[test]
    fn attachment_wins_over_renderable_type_and_sanitizes_filename_star() {
        let response = response(
            "text/html",
            b"<p>download</p>",
            vec![(
                "content-disposition".into(),
                "attachment; filename=old.html; filename*=UTF-8''..%2Fsafe%20name.html".into(),
            )],
        );
        assert!(response_needs_download(&response, true));
        assert_eq!(suggested_filename(&response, "text/html"), "safe name.html");
    }

    #[test]
    fn unsupported_types_download_but_plain_text_and_images_follow_capability() {
        assert!(response_needs_download(
            &response("application/pdf", b"%PDF-1.7", vec![]),
            true
        ));
        assert!(!response_needs_download(
            &response("text/plain; charset=utf-8", b"hello", vec![]),
            true
        ));
        let image = response("image/png", b"\x89PNG\r\n\x1a\n", vec![]);
        assert!(!response_needs_download(&image, true));
        assert!(response_needs_download(&image, false));
    }

    #[test]
    fn missing_and_legacy_plain_types_do_not_turn_binary_into_a_document() {
        let pdf = response("", b"%PDF-1.7\n%binary\0", vec![]);
        assert_eq!(computed_mime_type(&pdf), "application/pdf");
        assert!(response_needs_download(&pdf, true));

        let mislabeled = response("text/plain", b"ID3\0\0\0music", vec![]);
        assert_eq!(computed_mime_type(&mislabeled), "application/octet-stream");
        assert!(response_needs_download(&mislabeled, true));
    }

    #[test]
    fn inline_does_not_override_an_unsupported_media_type() {
        let response = response(
            "application/pdf",
            b"%PDF-1.7",
            vec![("content-disposition".into(), "inline".into())],
        );
        assert!(response_needs_download(&response, true));
    }

    #[test]
    fn dangerous_or_mismatched_extension_does_not_override_claimed_type() {
        let response = response(
            "application/pdf",
            b"%PDF-1.7",
            vec![(
                "content-disposition".into(),
                "attachment; filename=invoice.exe".into(),
            )],
        );
        assert_eq!(
            suggested_filename(&response, "application/pdf"),
            "invoice.exe.pdf"
        );
    }

    #[test]
    fn an_empty_post_response_is_never_replayed_as_get() {
        let mut response = response(
            "application/pdf",
            b"",
            vec![("content-length".into(), "12".into())],
        );
        response.from_post = true;
        assert!(!DownloadOffer::from_response(response, None).fetch_body);
    }

    #[tokio::test]
    async fn deferred_get_streams_chunked_body_directly_to_disk() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"GET /report HTTP/1.1\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n%PDF-\r\n3\r\n1.7\r\n0\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let destination = temporary_path("report.pdf");
        let offer = DownloadOffer {
            url: Url::parse(&format!("http://{address}/report")).unwrap(),
            content_type: String::from("application/pdf"),
            suggested_filename: String::from("report.pdf"),
            content_length: None,
            body: Vec::new(),
            referrer: None,
            fetch_body: true,
            gopher: None,
        };

        assert_eq!(save(&offer, &destination).await.unwrap(), 8);
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"%PDF-1.7");
        server.await.unwrap();
        tokio::fs::remove_file(destination).await.unwrap();
    }

    #[tokio::test]
    async fn publishing_never_overwrites_a_racing_destination() {
        let destination = temporary_path("existing.pdf");
        tokio::fs::write(&destination, b"existing").await.unwrap();
        let offer = DownloadOffer {
            url: Url::parse("https://example.test/report.pdf").unwrap(),
            content_type: String::from("application/pdf"),
            suggested_filename: String::from("report.pdf"),
            content_length: Some(3),
            body: b"new".to_vec(),
            referrer: None,
            fetch_body: false,
            gopher: None,
        };

        assert!(save(&offer, &destination).await.is_err());
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"existing");
        tokio::fs::remove_file(destination).await.unwrap();
    }
}
