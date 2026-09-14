//! Local-file URL parsing and dereferencing.
//!
//! WHATWG URL's `file` parser and RFC 8089 §§2–3 define the URL/path boundary:
//! TRust accepts an empty authority or `localhost`, while a URL naming
//! another authority must not silently become a local filesystem access. The
//! Fetch Standard lists `file` among fetch schemes but leaves its transport
//! algorithm to the user agent, so TRust owns that small, bounded adapter here.
//!
//! The local authoritative snapshots read for this boundary were WHATWG URL
//! commit `55d6699373ba68a16ec182f34222a74ed8bc3dac`, Fetch commit
//! `394d20d144ed1401c2c0e02c35bc3608cb2a2269`, and MIME Sniffing commit
//! `39aa53511b13953d84fef8d4131d6f61d0ccbde6` (all downloaded 2026-09-06),
//! together with RFC 8089 (2017-02), as updated by RFC 9844 §3 (2025-08).
//! Opaque-origin inheritance also follows HTML #same-origin and File API
//! #url-model (local File API commit `1341d2687a5dbfc1fa6168e4e45a1c6afb0d6fb4`).

use std::path::Path;

use tokio::io::AsyncReadExt as _;
use url::Url;

/// Keep local navigation subject to the same per-response memory ceiling as
/// HTTP bodies. A file is read only after the URL has been converted to a
/// local path, and the extra byte distinguishes an oversized file.
const MAX_FILE_BODY: u64 = 512 * 1024 * 1024;

/// Whether `input` has the URL Standard's `file` scheme spelling.
pub fn has_scheme(input: &str) -> bool {
    input
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("file"))
}

/// Parse an absolute `file:` URL with the URL Standard's parser.
pub fn parse_url(input: &str) -> Option<Url> {
    let url = Url::parse(input).ok()?;
    url.scheme().eq_ignore_ascii_case("file").then_some(url)
}

/// Turn a command-line or address-bar local path into a canonical file URL.
/// Existing bare relative files are accepted; an explicit `./` or `../` also
/// remains a file URL when the path does not exist yet, so the resulting error
/// names the local file instead of trying DNS.
pub fn url_from_input(input: &str) -> Result<Option<Url>, String> {
    if input.is_empty() {
        return Ok(None);
    }
    if let Some(url) = parse_url(input) {
        return Ok(Some(url));
    }
    // Do not reinterpret a malformed file URL as a relative filename.
    if has_scheme(input.trim()) {
        return Err(String::from("Invalid file URL."));
    }
    let path = Path::new(input);
    let explicit_path = path.is_absolute() || input.starts_with("./") || input.starts_with("../");
    // An explicit URL always wins over a coincidentally named relative file.
    // Also avoid a filesystem probe for every network URL typed in the UI.
    if !explicit_path && (crate::command::has_url_scheme(input.trim()) || !path.is_file()) {
        return Ok(None);
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("cannot resolve relative file path: {error}"))?
            .join(path)
    };
    let url = Url::from_file_path(path)
        .map_err(|_| String::from("cannot represent local path as a file URL"))?;
    // from_file_path percent-encodes filesystem bytes; parsing its result
    // applies URL's dot-segment algorithm as it does for explicit file URLs.
    Url::parse(url.as_str())
        .map(Some)
        .map_err(|error| format!("invalid local file URL: {error}"))
}

/// URL's file-host state normalizes `localhost` to the empty host. TRust
/// deliberately does not resolve other hostnames or mount network shares.
pub(crate) fn is_local_url(url: &Url) -> bool {
    url.scheme() == "file" && url.host_str().is_none_or(str::is_empty)
}

/// RFC 8089 §5 / URL #concept-url-origin: only local documents may embed or
/// navigate to local files. This permission is NOT a same-origin grant; file
/// documents retain opaque origins for CORS, canvas, DOM access and storage.
pub(crate) fn allowed_from(client: &Url, target: &Url) -> bool {
    target.scheme() != "file" || (is_local_url(client) && is_local_url(target))
}

/// Read a local file as a Fetch response.
///
/// RFC 8089 §§3–5: check authority and native request provenance BEFORE I/O.
/// `Url::to_file_path` then percent-decodes the path exactly once and ignores
/// query/fragment for filesystem identity. Never construct a path by stripping
/// a `file://` prefix, or allow page-supplied headers to authorize access.
pub(crate) async fn fetch(request: &crate::http::Request) -> Result<crate::http::Response, String> {
    if !is_local_url(&request.url) {
        return Err(String::from(
            "only local file URLs can be opened (empty host or localhost)",
        ));
    }
    if request
        .timing_client
        .as_ref()
        .is_some_and(|client| !allowed_from(client, &request.url))
        || request
            .fetch_policy
            .as_ref()
            .is_some_and(|policy| !allowed_from(&policy.origin, &request.url))
    {
        return Err(String::from(
            "local file access blocked for a non-file document",
        ));
    }
    // Fetch #main-fetch: with opaque file origins, same-origin mode fails,
    // and CORS requires an HTTP(S) scheme. Reject before even stat'ing a file.
    if request
        .fetch_policy
        .as_ref()
        .is_some_and(|policy| policy.mode != crate::http::RequestMode::NoCors)
    {
        return Err(String::from(
            "file URLs have opaque origins; CORS/same-origin requests are not allowed",
        ));
    }
    if !request.method.eq_ignore_ascii_case("GET") {
        return Err(String::from("file URLs only support GET"));
    }
    let path = request
        .url
        .to_file_path()
        .map_err(|_| String::from("only local file URLs can be opened"))?;
    let mut timing = crate::performance::FetchTiming::new();
    timing.request_start = crate::performance::now_ms();
    // Avoid opening devices/directories/sockets. Recheck the open descriptor
    // below: a pathname can change between metadata and open. On Unix,
    // O_NONBLOCK also prevents a raced FIFO open from occupying a worker
    // indefinitely; a timeout cannot cancel an already blocking OS open.
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    check_metadata(&path, &metadata)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags((rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOCTTY).bits() as i32);
    let file = options
        .open(&path)
        .await
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
    check_metadata(&path, &metadata)?;
    timing.final_response_start = crate::performance::now_ms();

    let mut body = Vec::new();
    let read = file
        .take(MAX_FILE_BODY + 1)
        .read_to_end(&mut body)
        .await
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if read as u64 > MAX_FILE_BODY {
        return Err(format!(
            "{} is larger than the {} MiB local-file limit",
            path.display(),
            MAX_FILE_BODY / (1024 * 1024)
        ));
    }

    let content_type = mime_type_for_path(&path).unwrap_or_default().to_string();
    let mut headers = vec![(String::from("content-length"), body.len().to_string())];
    if !content_type.is_empty() {
        headers.push((String::from("content-type"), content_type.clone()));
    }
    timing.response_end = crate::performance::now_ms();
    timing.response_status = 200;
    timing.content_type = content_type.clone();
    timing.encoded_body_size = body.len();
    timing.decoded_body_size = body.len();

    Ok(crate::http::Response {
        url: request.url.clone(),
        status: 200,
        content_type,
        headers,
        body,
        rendered: None,
        js: None,
        blobs: None,
        live: None,
        declarative_refresh: None,
        challenge: None,
        from_post: false,
        timing: Some(Box::new(timing)),
    })
}

fn check_metadata(path: &Path, metadata: &std::fs::Metadata) -> Result<(), String> {
    if !metadata.is_file() {
        return Err(format!(
            "{} is not a regular file (directory listings and special files are unsupported)",
            path.display()
        ));
    }
    if metadata.len() > MAX_FILE_BODY {
        return Err(format!(
            "{} is larger than the {} MiB local-file limit",
            path.display(),
            MAX_FILE_BODY / (1024 * 1024)
        ));
    }
    Ok(())
}

/// Common filesystem MIME associations. The MIME Sniffing Standard's
/// supplied-type algorithm (§5.1) obtains this value from the filesystem; on
/// platforms where TRust has no native MIME database, these common mappings
/// provide that adapter and unknown files still go through byte sniffing.
fn mime_type_for_path(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "html" | "htm" => "text/html",
        "xhtml" => "application/xhtml+xml",
        "gmi" | "gemini" | "gemtext" | "gmni" => "text/gemini",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "json" | "map" => "application/json",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "txt" | "text" | "md" | "markdown" | "csv" | "log" | "conf" | "ini" | "toml" | "yaml"
        | "yml" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{CredentialsMode, FetchPolicy, Request, RequestMode};

    #[test]
    fn file_input_preserves_case_unicode_and_reserved_filename_characters() {
        let path = std::env::temp_dir().join("TRust 🩷 image #1%23?.PNG ");
        let url = url_from_input(path.to_str().unwrap()).unwrap().unwrap();
        assert_eq!(url.to_file_path().unwrap(), path);
        assert!(url.path().contains("%231%2523%3F.PNG%20"));
        assert_eq!(url.query(), None);
        assert_eq!(url.fragment(), None);
        for input in ["./missing file.png", "../missing file.png", "Cargo.toml"] {
            let url = url_from_input(input).unwrap().unwrap();
            assert_eq!(url.scheme(), "file");
            assert!(url.to_file_path().unwrap().is_absolute());
        }
        for input in [
            "https://example.com/file.png",
            "about:bookmarks",
            "example.test",
            "",
        ] {
            assert!(url_from_input(input).unwrap().is_none(), "{input}");
        }
        for input in [
            "file://user@localhost/a",
            "file://localhost:80/a",
            "file://[invalid]/a",
        ] {
            assert!(url_from_input(input).is_err(), "{input}");
        }
    }

    #[test]
    fn file_url_parser_normalizes_authorities_and_url_dot_segments() {
        for input in [
            "file:///tmp/a/../b%20c.png",
            "FILE://LOCALHOST/tmp/%2e/b%20c.png",
            "file:/tmp/a/%2e%2e/b%20c.png",
            r"file:\\localhost\tmp\b%20c.png",
        ] {
            let url = parse_url(input).unwrap();
            assert_eq!(url.as_str(), "file:///tmp/b%20c.png", "{input}");
            assert!(is_local_url(&url));
        }
        for input in [
            "file://server/share/a",
            "file://127.0.0.1/a",
            "file://[::1]/a",
        ] {
            assert!(!is_local_url(&parse_url(input).unwrap()), "{input}");
        }
    }

    #[tokio::test]
    async fn file_fetch_roundtrips_encoded_names_and_ignores_query_and_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Local 🩷 #?%23.HTML");
        std::fs::write(&path, b"<!doctype html><title>local</title>").unwrap();
        let mut url = Url::from_file_path(&path).unwrap();
        url.set_query(Some("version=1"));
        url.set_fragment(Some("anchor"));
        let response = crate::http::fetch(&Request::get(url.clone()))
            .await
            .unwrap();
        assert_eq!(response.url, url);
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/html");
        assert_eq!(response.body, b"<!doctype html><title>local</title>");
        assert_eq!(
            response.headers[0],
            ("content-length".into(), response.body.len().to_string())
        );
        let timing = response.timing.unwrap();
        assert!(timing.fetch_start <= timing.request_start);
        assert!(timing.request_start <= timing.final_response_start);
        assert!(timing.final_response_start <= timing.response_end);
        assert_eq!(timing.domain_lookup_start, 0.0);
        assert_eq!(timing.connect_start, 0.0);
        assert_eq!(timing.next_hop_protocol, "");
        assert!(!timing.navigation_cross_origin_redirect);

        let localhost =
            Url::parse(&url.as_str().replacen("file:///", "file://localhost/", 1)).unwrap();
        assert_eq!(
            crate::http::fetch(&Request::get(localhost))
                .await
                .unwrap()
                .body,
            response.body
        );
    }

    #[tokio::test]
    async fn file_fetch_rejects_nonlocal_authorities_methods_and_nonregular_files() {
        for input in [
            "file://remote.example/tmp/a",
            "file://127.0.0.1/tmp/a",
            "file://[::1]/tmp/a",
        ] {
            let error = crate::http::fetch(&Request::get(Url::parse(input).unwrap()))
                .await
                .unwrap_err();
            assert!(error.contains("only local file URLs"), "{error}");
        }
        let dir = tempfile::tempdir().unwrap();
        let url = Url::from_file_path(dir.path().join("missing")).unwrap();
        for method in ["POST", "PUT", "DELETE", "HEAD"] {
            let mut request = Request::get(url.clone());
            request.method = method.into();
            assert!(
                crate::http::fetch(&request)
                    .await
                    .unwrap_err()
                    .contains("only support GET")
            );
        }
        let error = crate::http::fetch(&Request::get(url)).await.unwrap_err();
        assert!(error.contains("cannot open"), "{error}");
        let directory = Url::from_directory_path(dir.path()).unwrap();
        assert!(
            crate::http::fetch(&Request::get(directory))
                .await
                .unwrap_err()
                .contains("not a regular file")
        );

        #[cfg(unix)]
        {
            let fifo = dir.path().join("pipe");
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                &fifo,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .unwrap();
            let request = Request::get(Url::from_file_path(fifo).unwrap());
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                crate::http::fetch(&request),
            )
            .await
            .unwrap();
            assert!(result.unwrap_err().contains("not a regular file"));
        }
    }

    #[tokio::test]
    async fn file_fetch_rejects_oversized_files_before_allocating_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BODY + 1)
            .unwrap();
        let error = crate::http::fetch(&Request::get(Url::from_file_path(path).unwrap()))
            .await
            .unwrap_err();
        assert!(error.contains("local-file limit"), "{error}");
    }

    #[tokio::test]
    async fn file_http_redirect_cannot_turn_a_web_navigation_into_a_local_read() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.txt");
        std::fs::write(&path, b"private local data").unwrap();
        let target = Url::from_file_path(path).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let n = stream.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&buffer[..n]);
            }
            let reply = format!(
                "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
        });
        // Even a direct, user-initiated web request with no document client
        // must not inherit permission to follow an HTTP Location into file:.
        let request = Request::get(Url::parse(&format!("http://{address}/")).unwrap());
        let error = crate::http::fetch(&request).await.unwrap_err();
        assert!(error.contains("redirect leaves the web: file"), "{error}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn file_access_requires_native_local_client_and_preserves_opaque_cors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resource.txt");
        std::fs::write(&path, b"local resource").unwrap();
        let target = Url::from_file_path(path).unwrap();
        let page = target.join("index.html").unwrap();
        let request = Request::subresource(target.clone(), &page, "script", None);
        assert_eq!(
            crate::http::fetch(&request).await.unwrap().body,
            b"local resource"
        );
        assert!(
            crate::http::referrer_for(&page, &Url::parse("https://example.test/").unwrap())
                .is_none()
        );
        assert!(!crate::http::same_origin_for_host(&page, &target));

        for client in [
            "https://example.test/",
            "http://localhost/",
            "data:text/html,x",
            "about:blank",
            "gopher://example.test/1/",
        ] {
            let client = Url::parse(client).unwrap();
            assert!(!crate::http::subresource_allowed(&client, &target));
            let mut request = Request::subresource(target.clone(), &client, "image", None);
            request.headers = vec![
                ("Referer".into(), page.to_string()),
                ("Sec-Fetch-Site".into(), "same-origin".into()),
            ];
            let error = crate::http::fetch(&request).await.unwrap_err();
            assert!(error.contains("non-file document"), "{error}");
            // Fetch-policy provenance independently protects API requests.
            request.timing_client = None;
            request.fetch_policy = Some(FetchPolicy {
                origin: client,
                mode: RequestMode::NoCors,
                credentials: CredentialsMode::Omit,
            });
            assert!(
                crate::http::fetch(&request)
                    .await
                    .unwrap_err()
                    .contains("non-file document")
            );
        }
        for mode in [
            RequestMode::Cors,
            RequestMode::SameOrigin,
            RequestMode::NoCors,
        ] {
            let mut request = request.clone();
            request.fetch_policy = Some(FetchPolicy {
                origin: page.clone(),
                mode,
                credentials: CredentialsMode::SameOrigin,
            });
            let result = crate::http::fetch_script(&request).await;
            if mode == RequestMode::NoCors {
                let response = result.unwrap();
                assert_eq!(response.status, 0);
                assert!(response.body.is_empty());
                assert!(response.headers.is_empty());
            } else {
                assert!(result.is_err(), "{mode:?} must not expose a local file");
            }
        }
    }

    #[tokio::test]
    async fn gemini_local_preview_resolves_relative_files_and_preserves_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.gmi");
        let source = b"\xef\xbb\xbf# Local preview\n=> child.gmi Child\n";
        std::fs::write(&path, source).unwrap();
        assert_eq!(mime_type_for_path(&path), Some("text/gemini"));
        let url = Url::from_file_path(&path).unwrap();
        let doc = crate::http::parse(&url, "text/gemini", source, 80, 24, &Default::default());
        assert_eq!(doc.lines[0].text, "Local preview");
        assert_eq!(doc.raw, source);
        assert_eq!(
            doc.lines[1].link,
            Some(crate::doc::Link::Http(url.join("child.gmi").unwrap()))
        );
        assert!(doc.gemini.is_some());
    }

    #[tokio::test]
    async fn file_mime_associations_and_unknown_sniffing_share_the_document_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body, supplied, computed) in [
            ("empty.txt", b"".as_slice(), "text/plain", "text/plain"),
            (
                "binary.txt",
                b"\x00\x01".as_slice(),
                "text/plain",
                "text/plain",
            ),
            (
                "unknown",
                b"<!doctype html><title>sniffed</title>".as_slice(),
                "",
                "text/html",
            ),
            (
                "image",
                include_bytes!("assets/IdleHeart30.png").as_slice(),
                "",
                "image/png",
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, body).unwrap();
            let response = crate::http::fetch(&Request::get(Url::from_file_path(path).unwrap()))
                .await
                .unwrap();
            assert_eq!(response.content_type, supplied, "{name}");
            assert_eq!(
                crate::download::computed_mime_type(&response),
                computed,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn file_html_loads_classic_and_dynamic_resources_without_granting_origin_access() {
        let dir = tempfile::tempdir().unwrap();
        let html = r#"<!doctype html><html><head><base href="assets/">
            <link rel="stylesheet" href="style.css"></head><body>
            <p id="styled">local page</p><script src="main.js"></script></body></html>"#;
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("index.html"), html).unwrap();
        for (name, bytes) in [
            ("style.css", b"#styled { color: rgb(1, 2, 3) }".as_slice()),
            ("extra.css", b"body { background: white }".as_slice()),
            ("extra.js", b"report('file-dynamic-script');".as_slice()),
            (
                "frame.html",
                b"<!doctype html><p>local child document</p>".as_slice(),
            ),
            (
                "secret.txt",
                b"not script-readable through Fetch".as_slice(),
            ),
            (
                "image.png",
                include_bytes!("assets/IdleHeart30.png").as_slice(),
            ),
        ] {
            std::fs::write(dir.path().join("assets").join(name), bytes).unwrap();
        }
        std::fs::write(
            dir.path().join("assets/main.js"),
            r#"
            // The actor emits presentation updates, not console-only events.
            // Make each asynchronous assertion visible in the document too.
            function report(value) {
                console.log(value);
                document.body.appendChild(document.createTextNode(value + ' '));
            }
            report('file-style:' + getComputedStyle(document.getElementById('styled')).color);
            report('file-origin:' + location.origin);
            try { localStorage; report('STORAGE EXPOSED'); }
            catch (e) { report('file-storage:' + e.name); }
            try { document.styleSheets[0].cssRules; report('CSS EXPOSED'); }
            catch (e) { report('file-css-rules:' + e.name); }
            const script = document.createElement('script');
            script.src = 'extra.js'; document.body.appendChild(script);
            const image = new Image();
            image.onload = () => {
                report('file-image:' + (image.naturalWidth > 0));
                const canvas = document.createElement('canvas');
                const context = canvas.getContext('2d');
                context.drawImage(image, 0, 0);
                try { context.getImageData(0, 0, 1, 1); report('CANVAS EXPOSED'); }
                catch (e) { report('file-canvas:' + e.name); }
            };
            image.src = 'image.png'; document.body.appendChild(image);
            const frame = document.createElement('iframe');
            frame.src = 'frame.html';
            frame.onload = () => report('file-frame:' + (frame.contentDocument === null));
            document.body.appendChild(frame);
            // A creator's Blob keeps its own opaque identity, unlike a
            // separately loaded file. Check both initial-Window reuse and a
            // subsequent navigation that must create a fresh Window.
            const blobFrame = document.createElement('iframe');
            let blobLoads = 0;
            function loadBlob() {
                const url = URL.createObjectURL(new Blob(['local blob'], {type:'text/plain'}));
                blobFrame.src = url;
                if (!blobFrame.isConnected) document.body.appendChild(blobFrame);
                URL.revokeObjectURL(url);
            }
            blobFrame.onload = () => {
                report('file-blob-' + ++blobLoads + ':' +
                    (blobFrame.contentDocument.body.textContent === 'local blob' &&
                     blobFrame.contentWindow.parent.document === document));
                if (blobLoads === 1) loadBlob();
            };
            loadBlob();
            const sheet = document.createElement('link');
            sheet.rel = 'stylesheet'; sheet.href = 'extra.css';
            sheet.onload = () => report('file-dynamic-sheet');
            document.head.appendChild(sheet);
            fetch('secret.txt').then(
                () => report('FETCH EXPOSED'),
                () => report('file-cors-blocked'));
            fetch('secret.txt', {mode:'no-cors'}).then(async response => {
                report('file-opaque:' + response.status + ':' + await response.text());
            });
        "#,
        )
        .unwrap();
        let url = Url::from_file_path(dir.path().join("index.html")).unwrap();
        let response = crate::http::fetch(&Request::get(url)).await.unwrap();
        let response =
            crate::http::execute_js(response, (80, 24), (8, 16), Default::default()).await;
        let expected = [
            "file-style:rgb(1, 2, 3)",
            "file-origin:null",
            "file-storage:SecurityError",
            "file-css-rules:SecurityError",
            "file-dynamic-script",
            "file-image:true",
            "file-canvas:SecurityError",
            "file-frame:true",
            "file-blob-1:true",
            "file-blob-2:true",
            "file-dynamic-sheet",
            "file-cors-blocked",
            "file-opaque:0:",
        ];
        assert_console(response, &expected).await;
    }

    #[tokio::test]
    async fn file_base_on_a_web_page_cannot_authorize_local_scripts_images_or_fetch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("secret.js"),
            "console.log('LOCAL SCRIPT EXPOSED')",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("image.png"),
            include_bytes!("assets/IdleHeart30.png"),
        )
        .unwrap();
        let base = Url::from_directory_path(dir.path()).unwrap();
        let html = format!(
            r#"<!doctype html><base href="{base}">
            <body><script src="secret.js"></script><script>
            function report(value) {{
                console.log(value);
                document.body.appendChild(document.createTextNode(value + ' '));
            }}
            report('remote-client:' + location.origin);
            const img = new Image();
            img.onload = () => report('LOCAL IMAGE EXPOSED');
            img.onerror = () => report('remote-file-image-blocked');
            img.src = 'image.png';
            fetch('secret.js', {{mode:'no-cors'}}).then(
                () => report('LOCAL FETCH EXPOSED'),
                () => report('remote-file-fetch-blocked'));
            </script>"#
        );
        let path = dir.path().join("remote.html");
        std::fs::write(&path, html).unwrap();
        let mut response = crate::http::fetch(&Request::get(Url::from_file_path(path).unwrap()))
            .await
            .unwrap();
        // Simulate a fetched HTTPS document without requiring external I/O.
        response.url = Url::parse("https://example.test/page").unwrap();
        let response =
            crate::http::execute_js(response, (80, 24), (8, 16), Default::default()).await;
        assert_console(
            response,
            &[
                "remote-client:https://example.test",
                "remote-file-image-blocked",
                "remote-file-fetch-blocked",
            ],
        )
        .await;
    }

    async fn assert_console(mut response: crate::http::Response, expected: &[&str]) {
        let mut outcome = response
            .js
            .take()
            .expect("external classic script executes");
        // Each actor event drains that dispatch's console; retain earlier
        // observations rather than expecting the last event to replay them.
        let mut console = outcome.console.clone();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        while !expected.iter().all(|line| {
            console
                .iter()
                .any(|entry| entry.strip_prefix("log: ") == Some(*line))
        }) {
            let Some(live) = response.live.as_mut() else {
                break;
            };
            match tokio::time::timeout_at(deadline, live.events.recv()).await {
                Ok(Some(
                    crate::js::PageEvt::Updated { outcome: next, .. }
                    | crate::js::PageEvt::Patched { outcome: next, .. }
                    | crate::js::PageEvt::Static { outcome: next, .. },
                )) => {
                    assert!(!next.panicked, "{next:?}");
                    assert!(next.errors.is_empty(), "{next:?}");
                    console.extend_from_slice(&next.console);
                    outcome = next;
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        assert!(!outcome.panicked, "{outcome:?}");
        assert!(outcome.errors.is_empty(), "{outcome:?}");
        assert!(
            !console.iter().any(|entry| entry.contains("EXPOSED")),
            "{console:?}"
        );
        for line in expected {
            assert!(
                console
                    .iter()
                    .any(|entry| entry.strip_prefix("log: ") == Some(*line)),
                "missing {line}: {console:?}; {outcome:?}"
            );
        }
    }
}
