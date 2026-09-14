//! Gemini protocol: one TLS request/response, gemtext documents.
//!
//! A transaction is: TLS-connect (SNI, unpinned server certificates — see
//! `tls.rs`), send `gemini://host/path\r\n`, read a `<status> <meta>`
//! header line, then for 2x responses the body until close. Redirects
//! (3x) are followed here in the fetch task, capped to avoid loops.

#[cfg(test)]
use tokio::io::AsyncReadExt;

#[cfg(test)]
use crate::doc::Kind;
use crate::doc::{Doc, Link};
#[cfg(test)]
use crate::tls;

mod media;
mod transport;
mod url;
pub use media::MediaType;
pub(crate) use transport::Transfer;
pub use transport::{fetch, fetch_updates};
pub use url::GeminiUrl;

/// Interpret a target as an absolute URL of any scheme, if it is one:
/// gemini/gopher links are followable, everything else (`http:`,
/// `mailto:`, ...) is External. Relative references return None.
pub fn absolute_link(target: &str) -> Option<Link> {
    if let Some((host, port, tls)) = crate::command::telnet_target(target) {
        return Some(Link::Telnet { host, port, tls });
    }
    if let Some(url) = GeminiUrl::parse(target) {
        return Some(Link::Gemini(url));
    }
    if let Some(url) = crate::gopher::GopherUrl::parse(target) {
        return Some(Link::Gopher(url));
    }
    if let Some(url) = crate::file::parse_url(target) {
        return Some(Link::Http(url));
    }
    if let Some(url) = crate::http::parse_url(target) {
        return Some(Link::Http(url));
    }
    if let Ok(url) = crate::dict::Target::parse(target) {
        return Some(Link::Dict(url));
    }
    if let Some(url) = crate::oneshot::OneShotUrl::parse(target) {
        return Some(Link::OneShot(url));
    }
    let (scheme, _) = target.split_once(':')?;
    let mut bytes = scheme.bytes();
    if bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
    {
        Some(Link::External(target.to_string()))
    } else {
        None
    }
}

/// RFC 3986 §5.2, including empty/query/fragment-only references.
pub fn resolve(base: &GeminiUrl, target: &str) -> Link {
    resolve_reference(base, target, false)
}

fn resolve_reference(base: &GeminiUrl, target: &str, redirect: bool) -> Link {
    if let Some(mut link) = absolute_link(target) {
        if let Link::Gemini(url) = &mut link {
            url.path = url::normalize(&url.path);
            if redirect {
                url.inherit_sensitivity(base);
            }
        }
        return link;
    }
    base.resolve(target, redirect)
        .map(|mut url| {
            if redirect {
                url.inherit_sensitivity(base);
            }
            Link::Gemini(url)
        })
        .unwrap_or_else(|| Link::External(target.to_string()))
}

pub(crate) fn normalize(path: &str) -> String {
    url::normalize(path)
}
pub(crate) fn normalize_path(path: &str) -> String {
    url::remove_dot_segments(path)
}

/// Percent-encode a user query for a 1x input prompt.
pub fn encode_query(query: &str) -> String {
    let mut out = String::new();
    for byte in query.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A gemini response: header (always) plus body (2x only).
#[derive(Clone, Debug)]
pub struct Response {
    /// The URL that finally answered, after redirects.
    pub url: GeminiUrl,
    pub status: u8,
    pub meta: String,
    pub body: Vec<u8>,
    /// Whether a client identity was presented for this request.
    pub identity: bool,
    /// A usable, scoped identity was configured, independently of TLS selection.
    pub identity_configured: bool,
    pub finished: bool,
    pub notice: Option<String>,
    pub download: Option<crate::download::DownloadOffer>,
    pub view: View,
}

impl Response {
    pub fn new(url: GeminiUrl, status: u8, meta: String) -> Self {
        Self {
            url: url.public_url(),
            status,
            meta,
            body: Vec::new(),
            identity: false,
            identity_configured: false,
            finished: true,
            notice: None,
            download: None,
            view: View::default(),
        }
    }
    pub fn media_type(&self) -> Result<MediaType, String> {
        MediaType::parse(&self.meta)
    }
    pub fn status_text(&self) -> String {
        let name = status_name(self.status);
        let id = if self.identity { " · ID" } else { "" };
        let note = self
            .notice
            .as_deref()
            .unwrap_or(if self.finished { "" } else { "Loading…" });
        let meta = if self.meta.is_empty() {
            String::new()
        } else {
            format!(" — {}", self.meta)
        };
        format!(
            "{} — {name} ({}){meta}{id}{}",
            self.url,
            self.status,
            if note.is_empty() {
                String::new()
            } else {
                format!(" · {note}")
            }
        )
    }
    pub fn certificate_prompt(&self) -> bool {
        self.status >= 60
            && self.status <= 69
            && !matches!(self.status, 61 | 62)
            && !self.identity
            && !self.identity_configured
    }
}

pub fn status_name(status: u8) -> &'static str {
    match status {
        11 => "Sensitive input",
        10..=19 => "Input requested",
        20..=29 => "Success",
        31 => "Moved permanently",
        30..=39 => "Redirect",
        41 => "Server unavailable",
        42 => "Server application error",
        43 => "Proxy error",
        44 => "Slow down; wait before retrying",
        40..=49 => "Temporary failure",
        51 => "Not found",
        52 => "Gone",
        53 => "Proxy request refused",
        59 => "Bad request",
        50..=59 => "Permanent failure",
        61 => "Certificate not authorized",
        62 => "Certificate not valid",
        60..=69 => "Client certificate required",
        _ => "Invalid status",
    }
}

mod presentation;
pub(crate) use presentation::LIST_MARKER;
pub use presentation::{View, heading_row, parse_gemtext, render, source_offer};

pub fn parse(url: &GeminiUrl, meta: &str, body: &[u8], width: usize) -> Doc {
    render(
        Link::Gemini(url.public_url()),
        meta,
        body,
        width,
        View::default(),
    )
}

/// Shared prompt data; neither browser chrome nor history stores submitted secrets.
#[derive(Clone, Debug)]
pub struct Prompt {
    pub url: GeminiUrl,
    pub meta: String,
    pub certificate: bool,
    pub sensitive: bool,
}
impl Prompt {
    pub fn from_response(response: &Response) -> Option<Self> {
        if !(10..20).contains(&response.status) && !response.certificate_prompt() {
            return None;
        }
        Some(Self {
            url: response.url.public_url(),
            meta: response.meta.clone(),
            certificate: response.certificate_prompt(),
            sensitive: response.status == 11,
        })
    }
    pub fn label(&self) -> String {
        if !self.certificate {
            return self.meta.clone();
        }
        let existing = crate::tls::identity_path(&self.url.host).is_some_and(|p| p.exists());
        format!(
            "{} — {}. Enter {} for this path and its descendants.",
            self.url,
            self.meta,
            if existing {
                "authorizes the existing identity"
            } else {
                "creates and authorizes an identity with this name"
            }
        )
    }
    pub fn submit(&self, text: &str) -> Result<GeminiUrl, String> {
        if self.certificate {
            crate::tls::authorize_identity(
                &self.url,
                if text.trim().is_empty() {
                    "anonymous"
                } else {
                    text.trim()
                },
            )?;
            Ok(self.url.clone())
        } else {
            self.url.with_input(text, self.sensitive)
        }
    }
}

pub fn view_action(
    view: &mut View,
    action: &str,
    enabled: Option<bool>,
) -> Result<&'static str, &'static str> {
    if action == "outline" {
        view.outline = enabled.unwrap_or(!view.outline);
        Ok(if view.outline {
            "Heading outline — use heading N to jump."
        } else {
            "Reading the full page."
        })
    } else if action == "alt" {
        view.show_alt = enabled.unwrap_or(!view.show_alt);
        Ok(if view.show_alt {
            "Preformatted descriptions on."
        } else {
            "Preformatted descriptions off."
        })
    } else {
        crate::text_reply::view_action(&mut view.controls, action, enabled, false)
    }
}

pub(crate) fn image_response(response: Response) -> Result<crate::http::Response, String> {
    let mime = response.media_type()?.essence;
    let response = crate::http::Response {
        url: ::url::Url::parse(&response.url.public_url().to_string())
            .map_err(|e| e.to_string())?,
        status: 200,
        content_type: mime.clone(),
        body: response.body,
        headers: Vec::new(),
        rendered: None,
        js: None,
        blobs: None,
        live: None,
        declarative_refresh: None,
        challenge: None,
        from_post: false,
        timing: None,
    };
    Ok(crate::http::image_navigation_response(response, &mime))
}

pub const HELP: &str = "# Gemini in TRust\n\nOpen a gemini:// URL, follow links with Enter, and use Back/Forward to navigate. Local .gmi, .gemini and .gemtext files open as Gemtext previews.\n\n## Reading\n* W or wrap [on|off] controls ordinary text wrapping. Preformatted blocks keep their spacing.\n* Shift+Left/Right pans wide preformatted lines.\n* S or save saves the received source bytes. A stopped or limited response saves its received prefix.\n* gemini-width 20..240 sets the reading column (default 96 characters).\n* outline toggles the numbered heading list.\n* heading next, heading previous or heading N jumps to a heading.\n* gemini-alt [on|off] shows preformatted block descriptions.\n\n## Input and identity\nInput prompts submit with Enter; Escape cancels. Sensitive input is masked, excluded from command history and removed from stored page addresses. A sensitive query is never replayed by history or reload.\n\nWhen a capsule requests a client identity, Enter explicitly authorizes the displayed path and its descendants at that host and port. Existing identity files are reused; new names create an identity only when none exists. Rejected certificates display the server's explanation.\n\n## Files and redirects\nImages open in the viewer. Other files offer Save or Open. The download uses the original response connection; choosing Open launches the saved file. Gemini redirects are limited to five hops. A redirect to another protocol shows a link for you to follow.\n\n## Limits\nText arrives progressively. Stop keeps received content. The display buffers up to 2 MiB and bounds rows, columns and active links; notices explain incomplete responses. Saves retain original bytes before display filtering. Supported text encodings are UTF-8, ASCII and ISO-8859-1; unknown charsets show a source-saving message.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetches_over_unverified_tls_with_rotating_certificates_redirects_and_input() {
        use tokio::io::AsyncWriteExt as _;

        // Dedicated loopback 127.0.0.2 keeps this off `status_60`'s host. Both
        // share the process-global TRUST_IDENTITIES dir, and `status_60`
        // does `create_identity("127.0.0.1")`, which create_new's an EMPTY
        // <host>.pem before writing it. Were we also on 127.0.0.1, our
        // per-connection `load_identity("127.0.0.1")` would intermittently
        // read that file mid-creation ("no CERTIFICATE block") and fail —
        // a real, load-dependent flake. On 127.0.0.2 we read 127.0.0.2.pem,
        // which nothing ever writes, so load_identity is always Ok(None).
        let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Each gemini request is its own connection; serve until dropped.
        let server = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                // Renew even between redirect hops, using an expired,
                // self-signed certificate whose name doesn't match the URL.
                let acceptor = tls::tests::unverified_acceptor(tokio_rustls::rustls::ALL_VERSIONS);
                let Ok(mut stream) = acceptor.accept(sock).await else {
                    continue;
                };
                let mut req = Vec::new();
                let mut byte = [0u8; 1];
                while !req.ends_with(b"\r\n") {
                    match stream.read(&mut byte).await {
                        Ok(1..) => req.push(byte[0]),
                        _ => break,
                    }
                }
                let req = String::from_utf8_lossy(&req);
                let path = req.trim().rsplit_once(":").map(|(_, hp)| {
                    hp.split_once('/')
                        .map(|(_, p)| format!("/{p}"))
                        .unwrap_or_default()
                });
                let reply: &[u8] = match path.as_deref() {
                    Some("/") => b"20 text/gemini\r\n# Welcome\n=> /next Next page\n",
                    Some("/redir") => b"31 /target\r\n",
                    Some("/target") => b"20 text/plain\r\nplain body",
                    Some("/ask") => b"10 What is your handle?\r\n",
                    _ => b"51 Not found\r\n",
                };
                let _ = stream.write_all(reply).await;
                let _ = stream.shutdown().await;
            }
        });

        let url = |path: &str| GeminiUrl::new("127.0.0.2", port, path);

        // Success: header parsed, gemtext body delivered.
        let response = fetch(&url("/")).await.unwrap();
        assert_eq!(
            (response.status, response.meta.as_str()),
            (20, "text/gemini")
        );
        let doc = parse(&response.url, &response.meta, &response.body, 80);
        assert_eq!(doc.lines[0].kind, Kind::Heading(1));
        assert_eq!(doc.lines[1].text, "Next page");
        assert!(matches!(doc.lines[1].link, Some(Link::Gemini(_))));

        // Redirect: followed to /target, final URL reported.
        let response = fetch(&url("/redir")).await.unwrap();
        assert_eq!(response.status, 20);
        assert_eq!(response.url.path, "/target");
        assert_eq!(response.meta, "text/plain");
        assert_eq!(response.body, b"plain body");

        // 1x input status comes back without a body.
        let response = fetch(&url("/ask")).await.unwrap();
        assert_eq!(
            (response.status, response.meta.as_str()),
            (10, "What is your handle?")
        );
        assert!(response.body.is_empty());

        server.abort();
    }

    /// A capsule that wants a client certificate: bare visits get 60,
    /// certified ones get content — the astrobotany flow.
    #[tokio::test]
    async fn status_60_then_identity_roundtrip() {
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt as _;
        use tokio_rustls::TlsAcceptor;
        use tokio_rustls::rustls::client::danger::HandshakeSignatureValid;
        use tokio_rustls::rustls::crypto::CryptoProvider;
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
        use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
        use tokio_rustls::rustls::{
            DigitallySignedStruct, DistinguishedName, Error as TlsError, ServerConfig,
            SignatureScheme,
        };

        unsafe {
            std::env::set_var(
                "TRUST_KNOWN_HOSTS",
                std::env::temp_dir().join(format!("trust-test-kh-{}", std::process::id())),
            );
            std::env::set_var(
                "TRUST_IDENTITIES",
                std::env::temp_dir().join(format!("trust-test-ids-{}", std::process::id())),
            );
        }
        tls::ensure_provider();

        /// Accepts any client certificate (the capsule convention:
        /// identity is the cert itself, pinned on first use).
        #[derive(Debug)]
        struct AnyClient;
        impl ClientCertVerifier for AnyClient {
            fn root_hint_subjects(&self) -> &[DistinguishedName] {
                &[]
            }
            fn client_auth_mandatory(&self) -> bool {
                false
            }
            fn verify_client_cert(
                &self,
                _: &CertificateDer<'_>,
                _: &[CertificateDer<'_>],
                _: UnixTime,
            ) -> Result<ClientCertVerified, TlsError> {
                Ok(ClientCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                m: &[u8],
                c: &CertificateDer<'_>,
                d: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, TlsError> {
                tokio_rustls::rustls::crypto::verify_tls12_signature(
                    m,
                    c,
                    d,
                    &CryptoProvider::get_default()
                        .unwrap()
                        .signature_verification_algorithms,
                )
            }
            fn verify_tls13_signature(
                &self,
                m: &[u8],
                c: &CertificateDer<'_>,
                d: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, TlsError> {
                tokio_rustls::rustls::crypto::verify_tls13_signature(
                    m,
                    c,
                    d,
                    &CryptoProvider::get_default()
                        .unwrap()
                        .signature_verification_algorithms,
                )
            }
            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                CryptoProvider::get_default()
                    .unwrap()
                    .signature_verification_algorithms
                    .supported_schemes()
            }
        }

        let signed = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let key = PrivateKeyDer::try_from(signed.signing_key.serialize_der()).unwrap();
        let config = ServerConfig::builder()
            .with_client_cert_verifier(Arc::new(AnyClient))
            .with_single_cert(vec![signed.cert.der().clone()], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut stream) = acceptor.accept(sock).await else {
                    continue;
                };
                let mut req = Vec::new();
                let mut byte = [0u8; 1];
                while !req.ends_with(b"\r\n") {
                    match stream.read(&mut byte).await {
                        Ok(1..) => req.push(byte[0]),
                        _ => break,
                    }
                }
                let reply: &[u8] = match stream.get_ref().1.peer_certificates() {
                    Some(_) => b"20 text/gemini\r\nWelcome back, certified user.\n",
                    None => b"60 Certificate required\r\n",
                };
                let _ = stream.write_all(reply).await;
                let _ = stream.shutdown().await;
            }
        });

        let url = GeminiUrl::new("127.0.0.1", port, "/garden");
        // Anonymous visit: turned away with a 60.
        let response = fetch(&url).await.unwrap();
        assert_eq!((response.status, response.identity), (60, false));
        assert_eq!(response.meta, "Certificate required");

        // Mint the identity (what the status-60 prompt does), retry:
        // recognized, and the response records that a cert was sent.
        tls::authorize_identity(&url, "talkie").unwrap();
        let response = fetch(&url).await.unwrap();
        assert_eq!((response.status, response.identity), (20, true));
        assert_eq!(response.body, b"Welcome back, certified user.\n");

        server.abort();
    }

    #[test]
    fn parses_urls() {
        let url = GeminiUrl::parse("gemini://example.org").unwrap();
        assert_eq!((url.port, url.path.as_str()), (1965, "/"));
        let url = GeminiUrl::parse("gemini://example.org:1966/foo/bar?q").unwrap();
        assert_eq!(
            (url.port, url.path.as_str(), url.query()),
            (1966, "/foo/bar", Some("q"))
        );
        assert!(GeminiUrl::parse("gopher://example.org").is_none());
        assert!(GeminiUrl::parse("gemini://").is_none());
    }

    #[test]
    fn resolves_relative_references() {
        let base = GeminiUrl::parse("gemini://e.org/dir/page.gmi").unwrap();
        let gem = |s: &str| match resolve(&base, s) {
            Link::Gemini(u) => u.to_string(),
            other => panic!("expected gemini link, got {other:?}"),
        };
        assert_eq!(gem("other.gmi"), "gemini://e.org/dir/other.gmi");
        assert_eq!(gem("/top.gmi"), "gemini://e.org/top.gmi");
        assert_eq!(gem("../up.gmi"), "gemini://e.org/up.gmi");
        assert_eq!(gem("./same.gmi"), "gemini://e.org/dir/same.gmi");
        assert_eq!(gem("sub/"), "gemini://e.org/dir/sub/");
        assert_eq!(gem("//other.host/x"), "gemini://other.host/x");
        assert_eq!(gem("gemini://abs.host:1966/y"), "gemini://abs.host:1966/y");
        assert!(matches!(
            resolve(&base, "https://example.com/"),
            Link::Http(_)
        ));
        assert!(matches!(
            resolve(&base, "mailto:sister@night.city"),
            Link::External(_)
        ));
        assert!(matches!(
            resolve(&base, "gopher://floodgap.com/"),
            Link::Gopher(_)
        ));
    }

    #[test]
    fn parses_gemtext() {
        let url = GeminiUrl::parse("gemini://e.org/dir/").unwrap();
        let body = b"# Title\n\
                     plain paragraph\n\
                     => /abs Label here\n\
                     => rel.gmi\n\
                     * item\n\
                     > wisdom\n\
                     ```alt text\n\
                     ascii  art   with   spacing\n\
                     => not/a/link inside pre\n\
                     ```\n\
                     after";
        let doc = parse(&url, "text/gemini", body, 80);
        let kinds: Vec<Kind> = doc.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::Heading(1),
                Kind::Text,
                Kind::GemLink,
                Kind::GemLink,
                Kind::List,
                Kind::Quote,
                Kind::Pre,
                Kind::Pre,
                Kind::Text,
            ]
        );
        assert_eq!(doc.lines[0].text, "Title");
        assert_eq!(doc.lines[2].text, "Label here");
        assert_eq!(
            doc.lines[2].link,
            Some(Link::Gemini(
                GeminiUrl::parse("gemini://e.org/abs").unwrap()
            ))
        );
        // Bare-target link uses the target as its label.
        assert_eq!(doc.lines[3].text, "rel.gmi");
        // Pre block content is untouched (no link parsing, no wrap).
        assert_eq!(doc.lines[7].text, "=> not/a/link inside pre");
        assert!(doc.lines[7].link.is_none());
    }

    #[test]
    fn pre_blocks_are_never_wrapped() {
        let url = GeminiUrl::parse("gemini://e.org/").unwrap();
        let long = "x".repeat(200);
        let body = format!("```\n{long}\n```\n{long}");
        let doc = parse(&url, "text/gemini", body.as_bytes(), 40);
        assert_eq!(doc.lines[0].text.len(), 200, "pre line untouched");
        assert!(
            doc.lines[1..].iter().all(|l| l.text.chars().count() <= 40),
            "regular text wraps"
        );
    }

    #[test]
    fn encodes_queries() {
        assert_eq!(encode_query("hello world&more"), "hello%20world%26more");
        assert_eq!(encode_query("safe-chars_1.2~"), "safe-chars_1.2~");
    }
}
