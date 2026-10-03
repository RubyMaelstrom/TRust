//! Document context for inline SVG.
//!
//! SVG Integration #referencing-modes: "SVG document fragments that are
//! included inline in a host document use a referencing mode that matches
//! that of the host document." Inline `<svg>` in HTML is therefore document
//! content: its text uses the document's `@font-face` rules (CSS Fonts 4
//! #font-face-rule) and its `<image href>` loads like the document's other
//! images (SVG 2 linking.html#processingURL-fetch). SVG referenced by `<img>`
//! or CSS `url()` instead uses the static image document mode, whose secure
//! static processing mode allows no external references
//! (#secure-static-mode), and it is a separate document without the page's
//! downloaded fonts.
//!
//! TRust rasterizes inline SVG through the shared image pipeline, so its
//! serialized markup travels as a `data:` URL like any author data image.
//! The serializer (`Dom::svg_image_data`) prefixes the markup with a
//! `<?trust-document-svg …?>` processing instruction that carries a
//! per-process random nonce, the document font environment and the
//! document images it references. Every decode path, including the
//! paint-size re-rasterization that only sees the retained bytes, recognizes
//! that marker; author content never learns the nonce, so an
//! `<img src="data:image/svg+xml,…">` or a fetched SVG cannot opt into the
//! document context by copying the marker.
//!
//! External `<image>` resources are discovered during serialization and
//! requested through the frontends' ordinary page-image fetch path, which
//! applies the page's subresource, cookie, and referrer policies. The fetch
//! chokepoint records the bytes here; the marker lists each image's current
//! availability, so the inline SVG's image source changes, and every frontend
//! decodes it again, once an image arrives.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use super::{SVG_MIME, base64_decode, bounded_svg_data, looks_like_svg};

const MARKER: &str = "trust-document-svg";
const DATA_PREFIX: &str = "data:image/svg+xml;base64,";
/// Fragment that marks a page-image request made for an inline SVG `<image>`.
/// The frontends key image loads by source string; a distinct key makes the
/// document image load even when the same URL already loaded for an `<img>`
/// before the SVG referenced it. Fragments never reach the network.
const REQUEST_FRAGMENT: &str = "trust-document-svg-image-";
/// The marker's image list is bounded like any other serializer output; a
/// document image beyond it remains an unresolved (invalid) reference.
const MAX_MARKER_IMAGES: usize = 64;
const MAX_MARKER_BYTES: usize = 256 * 1024;
const MAX_STORED_IMAGES: usize = 256;
const MAX_STORED_BYTES: usize = 64 * 1024 * 1024;

/// A 128-bit secret drawn once per process. It never enters author-visible
/// state: the inline SVG `data:` URLs that carry it exist only in layout and
/// image-loading structures.
fn nonce() -> &'static str {
    static NONCE: LazyLock<String> = LazyLock::new(|| {
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_err() {
            // The standard library seeds RandomState from the OS as well.
            use std::hash::{BuildHasher as _, Hasher as _};
            for chunk in bytes.chunks_mut(8) {
                let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
                hasher.write_usize(chunk.as_ptr() as usize);
                chunk.copy_from_slice(&hasher.finish().to_le_bytes()[..chunk.len()]);
            }
        }
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    });
    NONCE.as_str()
}

/// Whether a resolved `<image>` URL can be a document image dependency.
/// `file:` is excluded: this process-wide store has no document provenance,
/// so a later web document could otherwise display a local file another
/// document loaded (the same restriction as `dom::prime_sprite_sheet`).
/// Characters that could end the marker are rejected; special-scheme
/// serializations percent-encode them anyway.
pub(crate) fn document_svg_image_url(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "blob")
        && url.fragment().is_none()
        && !url
            .as_str()
            .contains(|c: char| c.is_ascii_whitespace() || matches!(c, '<' | '>' | '"'))
}

/// Wrap an inline SVG's serialized markup as an image source carrying its
/// document context: `fonts` is the font environment token from
/// `font_system` (0: installed fonts only) and `images` the absolute URLs of
/// its external `<image>` references, in document order.
pub(crate) fn document_svg_data_url(svg: &str, fonts: u64, images: &[String]) -> String {
    let mut marker = format!("<?{MARKER} {} f:{fonts}", nonce());
    let mut seen = HashSet::new();
    for url in images.iter().take(MAX_MARKER_IMAGES) {
        if seen.insert(url.as_str()) {
            marker.push_str(&format!(" i:{}:{url}", image_generation(url)));
        }
    }
    marker.push_str("?>");
    let mut source = String::with_capacity(marker.len() + svg.len());
    source.push_str(&marker);
    source.push_str(svg);
    super::svg_data_url(&source)
}

/// The decode-time context an inline SVG's marker grants.
pub(super) struct DocumentSvgContext {
    fonts: u64,
    images: HashSet<String>,
}

impl DocumentSvgContext {
    /// Recognize the marker at the very start of the decoded SVG text. A
    /// marker with any other nonce is an ordinary processing instruction.
    pub(super) fn of(text: &str) -> Option<Self> {
        let body = text.strip_prefix("<?")?.strip_prefix(MARKER)?;
        let body = body.strip_prefix(' ')?;
        let end = body.find("?>")?;
        let mut fields = body[..end].split(' ');
        if fields.next()? != nonce() {
            return None;
        }
        let mut context = Self {
            fonts: 0,
            images: HashSet::new(),
        };
        for field in fields {
            if let Some(fonts) = field.strip_prefix("f:") {
                context.fonts = fonts.parse().ok()?;
            } else if let Some(image) = field.strip_prefix("i:") {
                let (_, url) = image.split_once(':')?;
                context.images.insert(url.to_string());
            }
        }
        Some(context)
    }

    /// usvg options for the inline SVG's document. Images resolve only to
    /// the document images the serializer listed, and a referenced SVG image
    /// is itself a separate image document processed in secure static mode
    /// (SVG 2 embedded.html#ImageElement; SVG Integration
    /// #static-image-document-mode), with neither the page's fonts nor its
    /// external references.
    pub(super) fn options(self) -> resvg::usvg::Options<'static> {
        let mut options = super::secure_svg_options();
        let (fontdb, font_resolver) = crate::font_system::svg_document_font_options(self.fonts);
        options.fontdb = fontdb;
        options.font_resolver = font_resolver;
        let images = self.images;
        options.image_href_resolver = resvg::usvg::ImageHrefResolver {
            resolve_data: Box::new(|mime, data, _| image_kind(mime, data)),
            resolve_string: Box::new(move |href, _| {
                let href = href.trim();
                if !images.contains(href) {
                    return None;
                }
                let bytes = stored_image(href)?;
                image_kind("", bytes)
            }),
        };
        options
    }
}

/// Decode one image referenced from inline SVG. Raster formats are handed to
/// resvg as-is; SVG is parsed with fresh secure static options.
fn image_kind(mime: &str, data: Arc<Vec<u8>>) -> Option<resvg::usvg::ImageKind> {
    if data.len() > super::MAX_SVG_BYTES {
        return None;
    }
    let svg = mime == SVG_MIME || (!data.is_empty() && looks_like_svg(data.as_slice()));
    if svg {
        let xml = bounded_svg_data(data.as_slice()).ok()?.into_owned();
        let secure = super::secure_svg_options();
        return (resvg::usvg::ImageHrefResolver::default_data_resolver())(
            SVG_MIME,
            Arc::new(xml),
            &secure,
        );
    }
    let mime = match image::guess_format(data.as_slice()).ok()? {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => return None,
    };
    let secure = super::secure_svg_options();
    (resvg::usvg::ImageHrefResolver::default_data_resolver())(mime, data, &secure)
}

/// The page-image requests a document SVG source needs: one per external
/// `<image>` its marker lists. Empty for every other source, including an
/// author data image that merely resembles one.
pub(crate) fn document_svg_image_requests(source: &str) -> Vec<String> {
    let Some(payload) = source.strip_prefix(DATA_PREFIX) else {
        return Vec::new();
    };
    // Decode only the prefix that holds the marker.
    let mut chars = 1024usize;
    let text = loop {
        let take = chars.min(payload.len()) / 4 * 4;
        let Some(bytes) = base64_decode(&payload[..take]) else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.contains("?>") || take == payload.len() / 4 * 4 || chars >= MAX_MARKER_BYTES {
            break text;
        }
        chars *= 4;
    };
    let Some(context) = DocumentSvgContext::of(&text) else {
        return Vec::new();
    };
    // Document order is preserved by re-reading the marker's list.
    let end = text.find("?>").unwrap_or(text.len());
    text[..end]
        .split(' ')
        .filter_map(|field| field.strip_prefix("i:")?.split_once(':'))
        .map(|(_, url)| url)
        .filter(|url| context.images.contains(*url))
        .map(|url| format!("{url}#{REQUEST_FRAGMENT}{}", nonce()))
        .collect()
}

/// For an image source the fetch chokepoints are about to load, the document
/// image it was requested for (that URL is what they fetch), or `None` for an
/// ordinary page image.
pub(crate) fn document_svg_image_target(source: &str) -> Option<&str> {
    let (url, fragment) = source.rsplit_once('#')?;
    (fragment.strip_prefix(REQUEST_FRAGMENT)? == nonce()).then_some(url)
}

struct StoredImage {
    generation: u64,
    bytes: Arc<Vec<u8>>,
}

#[derive(Default)]
struct DocumentImages {
    entries: HashMap<String, StoredImage>,
    order: VecDeque<String>,
    bytes: usize,
    next_generation: u64,
}

static DOCUMENT_IMAGES: LazyLock<Mutex<DocumentImages>> = LazyLock::new(Mutex::default);
/// Advanced whenever a document image arrives, changes or fails. Retained
/// layout includes it in its environment, so the next layout re-serializes
/// inline SVG with the new availability.
static DOCUMENT_SVG_REVISION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn document_svg_revision() -> u64 {
    DOCUMENT_SVG_REVISION.load(Ordering::Acquire)
}

fn image_generation(url: &str) -> u64 {
    DOCUMENT_IMAGES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entries
        .get(url)
        .map_or(0, |image| image.generation)
}

fn stored_image(url: &str) -> Option<Arc<Vec<u8>>> {
    DOCUMENT_IMAGES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entries
        .get(url)
        .map(|image| image.bytes.clone())
}

/// Record the outcome of a document image load made by a frontend's page-image
/// fetch (`None`: the fetch failed or was refused). HTML #img-error: a broken
/// image is not rendered, so a failure also withdraws older bytes.
pub(crate) fn record_document_svg_image(url: &str, bytes: Option<&[u8]>) {
    #[cfg(test)]
    let _inputs = crate::layout2::global_layout_input_change();
    let parsed = url::Url::parse(url).ok();
    if parsed
        .as_ref()
        .is_none_or(|url| !document_svg_image_url(url))
    {
        return;
    }
    let bytes = bytes.filter(|bytes| {
        bytes.len() <= super::MAX_SVG_BYTES
            && matches!(
                super::sniff(bytes),
                Some("image/png" | "image/jpeg" | "image/gif" | "image/webp" | SVG_MIME)
            )
    });
    let mut store = DOCUMENT_IMAGES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let changed = match bytes {
        Some(bytes) => {
            if store
                .entries
                .get(url)
                .is_some_and(|image| image.bytes.as_slice() == bytes)
            {
                false
            } else {
                store.remove(url);
                store.next_generation += 1;
                let generation = store.next_generation;
                store.bytes += bytes.len();
                store.order.push_back(url.to_string());
                store.entries.insert(
                    url.to_string(),
                    StoredImage {
                        generation,
                        bytes: Arc::new(bytes.to_vec()),
                    },
                );
                while store.entries.len() > MAX_STORED_IMAGES || store.bytes > MAX_STORED_BYTES {
                    let Some(oldest) = store.order.pop_front() else {
                        break;
                    };
                    if let Some(image) = store.entries.remove(&oldest) {
                        store.bytes -= image.bytes.len();
                    }
                }
                true
            }
        }
        None => store.remove(url),
    };
    drop(store);
    if changed {
        DOCUMENT_SVG_REVISION.fetch_add(1, Ordering::Release);
    }
}

impl DocumentImages {
    fn remove(&mut self, url: &str) -> bool {
        let Some(image) = self.entries.remove(url) else {
            return false;
        };
        self.bytes -= image.bytes.len();
        self.order.retain(|entry| entry != url);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str) -> image::RgbaImage {
        let bytes = super::super::decode_data_url(source).expect("data URL");
        super::super::decode(&bytes)
            .expect("SVG decodes")
            .0
            .to_rgba8()
    }

    fn png(color: [u8; 4]) -> Vec<u8> {
        super::super::rgba_png(4, 4, color)
    }

    fn count(image: &image::RgbaImage, color: [u8; 3]) -> usize {
        image
            .pixels()
            .filter(|pixel| pixel[3] > 200 && pixel.0[..3] == color)
            .count()
    }

    /// The horizontal extent of the drawn glyphs.
    fn ink_width(image: &image::RgbaImage) -> u32 {
        let columns = (0..image.width())
            .filter(|&x| (0..image.height()).any(|y| image.get_pixel(x, y)[3] > 64))
            .collect::<Vec<_>>();
        columns.last().map_or(0, |last| last - columns[0] + 1)
    }

    /// CSS Fonts 4 #font-face-rule: inline SVG text is document text and
    /// uses the document's `@font-face` faces. SVG used as an image is a
    /// separate document (SVG Integration #static-image-document-mode), so
    /// the same markup there, or under a marker without the process nonce,
    /// falls back to installed fonts.
    #[test]
    fn inline_svg_text_uses_document_faces_but_svg_images_do_not() {
        let mono = include_bytes!("../../assets/fonts/dejavu/DejaVuSansMono.ttf");
        let set = crate::font_system::FontSet::new(
            vec![crate::font_system::PageFont {
                family: String::from("TRust Inline Face"),
                bytes: mono.to_vec(),
            }],
            None,
            false,
        );
        let fonts = set.svg_font_environment();
        assert_ne!(fonts, 0);
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="240" height="40"><text x="4" y="30" font-family="'TRust Inline Face', serif" font-size="24" fill="black">iiiiiiii</text></svg>"#;

        let document = ink_width(&render(&document_svg_data_url(svg, fonts, &[])));
        let image = ink_width(&render(&super::super::svg_data_url(svg)));
        let forged = ink_width(&render(&super::super::svg_data_url(&format!(
            "<?{MARKER} {} f:{fonts}?>{svg}",
            "0".repeat(32)
        ))));
        // Monospaced `i` advances 0.6em; a proportional fallback is far narrower.
        assert!(
            document > image * 3 / 2,
            "document face width {document}, SVG-as-image width {image}"
        );
        assert_eq!(forged, image, "a marker without the nonce grants nothing");
        // An unknown environment token falls back to installed fonts only.
        assert_eq!(
            ink_width(&render(&document_svg_data_url(svg, u64::MAX, &[]))),
            image
        );
    }

    /// SVG 2 linking.html#processingURL-fetch: inline SVG's `<image>` is a
    /// document image, requested through the page-image path and drawn once
    /// that load records it. SVG-as-image still refuses the same reference
    /// (SVG Integration #secure-static-mode), even with the bytes present.
    #[test]
    fn inline_svg_external_image_renders_once_its_page_load_completes() {
        let url = "https://images.test/inline-svg-arrival/red.png";
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><image href="{url}" width="4" height="4"/></svg>"#
        );
        let images = [url.to_string()];
        let before = document_svg_data_url(&svg, 0, &images);
        assert_eq!(count(&render(&before), [255, 0, 0]), 0);

        let requests = document_svg_image_requests(&before);
        assert_eq!(requests.len(), 1);
        assert_eq!(document_svg_image_target(&requests[0]), Some(url));
        assert_eq!(document_svg_image_target(url), None);
        // A copied request fragment without the nonce is an ordinary image.
        assert_eq!(
            document_svg_image_target(&format!("{url}#{REQUEST_FRAGMENT}0")),
            None
        );
        // Author data images never yield document requests.
        assert!(document_svg_image_requests(&super::super::svg_data_url(&svg)).is_empty());

        let revision = document_svg_revision();
        record_document_svg_image(url, Some(&png([255, 0, 0, 255])));
        assert!(document_svg_revision() > revision);
        let after = document_svg_data_url(&svg, 0, &images);
        assert_ne!(before, after, "arrival must change the image source");
        assert_eq!(count(&render(&after), [255, 0, 0]), 16);

        // SVG used as an image never loads external references.
        assert_eq!(
            count(&render(&super::super::svg_data_url(&svg)), [255, 0, 0]),
            0
        );
        // Nor does a marked resource that did not list the reference.
        assert_eq!(
            count(&render(&document_svg_data_url(&svg, 0, &[])), [255, 0, 0]),
            0
        );

        // HTML #img-error: a later failed load withdraws the image.
        record_document_svg_image(url, None);
        let failed = document_svg_data_url(&svg, 0, &images);
        assert_ne!(failed, after);
        assert_eq!(count(&render(&failed), [255, 0, 0]), 0);
    }

    /// An SVG referenced by inline SVG's `<image>` is its own image document
    /// in secure static mode: it renders, but its own external references do
    /// not load even when they are document images of the page.
    #[test]
    fn svg_referenced_by_inline_svg_image_stays_in_secure_static_mode() {
        let nested = "https://images.test/inline-svg-nested/nested.svg";
        let red = "https://images.test/inline-svg-nested/red.png";
        record_document_svg_image(red, Some(&png([255, 0, 0, 255])));
        let nested_svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="2" fill="blue"/><image href="{red}" y="2" width="4" height="2"/></svg>"#
        );
        record_document_svg_image(nested, Some(nested_svg.as_bytes()));
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><image href="{nested}" width="4" height="4"/><image href="data:image/svg+xml;base64,{}" width="4" height="4"/></svg>"#,
            super::super::base64_encode(nested_svg.as_bytes())
        );
        let image = render(&document_svg_data_url(
            &svg,
            0,
            &[nested.to_string(), red.to_string()],
        ));
        assert!(count(&image, [0, 0, 255]) >= 8, "the nested SVG renders");
        assert_eq!(count(&image, [255, 0, 0]), 0, "its external image does not");
    }

    #[test]
    fn document_image_urls_exclude_local_files_and_marker_breaking_text() {
        let accepted = |url: &str| document_svg_image_url(&url::Url::parse(url).unwrap());
        assert!(accepted("https://a.test/x.png?size=2"));
        assert!(accepted(
            "blob:https://a.test/0f7c18cd-0475-4912-9865-1cd4adacebaa"
        ));
        assert!(!accepted("file:///home/user/secret.png"));
        assert!(!accepted("https://a.test/x.png#fragment"));
        assert!(!accepted("blob:https://a.test/a b"));
        assert!(!accepted("data:image/png;base64,AAAA"));
        // Recording ignores what could never be a dependency.
        let local = "file:///tmp/inline-svg-store-test.png";
        record_document_svg_image(local, Some(&png([0, 0, 0, 255])));
        assert_eq!(image_generation(local), 0);
        // Non-image bodies (an HTML error page) are not retained.
        let html = "https://images.test/inline-svg-store/error.png";
        record_document_svg_image(html, Some(b"<!doctype html><title>404</title>"));
        assert_eq!(image_generation(html), 0);
    }
}
