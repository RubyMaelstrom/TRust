//! The protocol-agnostic document model the browser view renders.
//!
//! Gopher menus, gopher text files, and gemtext all parse into a `Doc`:
//! a flat list of styled lines, some carrying links. The gopherus-style
//! navigation, wrapping, and history code operate on this model only and
//! never need to know which protocol produced it.

use std::fmt;

use crate::gemini::GeminiUrl;
use crate::gopher::GopherUrl;

/// A followable (or at least displayable) link target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Gopher(GopherUrl),
    Gemini(GeminiUrl),
    Http(url::Url),
    /// finger://, whois://, or dict:// one-shot queries.
    OneShot(crate::oneshot::OneShotUrl),
    /// An HTML form control: indices into the document's `forms`.
    Form {
        form: usize,
        field: usize,
    },
    /// A clickable element on a living JS page: the arena NodeId to
    /// dispatch a click to, plus the original href (empty for buttons)
    /// for the status hint and navigation fallback.
    JsClick {
        node: usize,
        href: String,
    },
    /// A media representation (a `<video>`/`<audio>` element): following it
    /// hands this URL to mpv. For an inline-playable element it is the direct
    /// media file; for an MSE/blob stream with no `src`/`<source>` (Twitch,
    /// YouTube, any modern player) it is the PAGE URL, which yt-dlp resolves.
    /// The terminal can't play video, so this representation IS the "play in
    /// mpv" affordance shown where the video would be.
    Media(url::Url),
    /// A scheme we don't speak (mailto, irc, ...) — shown, not followed.
    External(String),
    /// A generated carousel scroll control (the CSS `::scroll-button`
    /// model): activating it pages the nearest carousel by `dir` (−1 toward
    /// the start, +1 toward the end) rather than navigating.
    CarouselScroll(i8),
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Link::Gopher(url) => url.fmt(f),
            Link::Gemini(url) => url.fmt(f),
            Link::Http(url) => url.fmt(f),
            Link::OneShot(url) => url.fmt(f),
            Link::Form { .. } => f.write_str("form control"),
            Link::JsClick { href, .. } if !href.is_empty() => f.write_str(href),
            Link::JsClick { .. } => f.write_str("page script"),
            Link::Media(url) => write!(f, "▶ {url}"),
            Link::External(url) => f.write_str(url),
            Link::CarouselScroll(dir) if *dir < 0 => f.write_str("scroll back"),
            Link::CarouselScroll(_) => f.write_str("scroll forward"),
        }
    }
}

/// How an HTML form submits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormMethod {
    Get,
    Post,
}

/// What sort of control a form field is, and how Enter acts on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// text/search/email/... — edited through the input prompt.
    Text,
    /// Edited like Text but the value renders masked.
    Password,
    /// Submitted but never rendered.
    Hidden,
    Textarea,
    /// Enter toggles.
    Checkbox,
    /// Enter picks it within its name group.
    Radio,
    /// (label, value) options; Enter cycles through them.
    Select(Vec<(String, String)>),
    /// Enter submits the form.
    Submit,
}

/// One control in an HTML form, in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub value: String,
    /// Checkbox / radio state.
    pub checked: bool,
    /// Placeholder text (editable fields) or button label (submits).
    pub label: String,
    pub kind: FieldKind,
    /// Original DOM node id inside the living page actor, when this
    /// field came from a live JS render.
    pub live_node: Option<usize>,
}

impl Field {
    /// The widget row the browser shows for this control. Editable and
    /// select widgets lead with the value/placeholder (like a real browser),
    /// not the form field's `name` — the `name` is internal, redundant with
    /// the visible `<label>` we render, and verbose enough (e.g.
    /// `search[category_id]`) to push a row of fields onto separate lines.
    /// It survives only as a last-resort identifier when nothing else names
    /// the control.
    pub fn row_label(&self) -> String {
        let name = if self.name.is_empty() {
            self.label.as_str()
        } else {
            self.name.as_str()
        };
        match &self.kind {
            FieldKind::Hidden => String::new(),
            FieldKind::Submit => {
                let label = if self.label.is_empty() {
                    "Submit"
                } else {
                    self.label.as_str()
                };
                format!("[ {label} ]")
            }
            FieldKind::Checkbox => {
                // The name is this control's visible label here.
                format!("[{}] {name}", if self.checked { "x" } else { " " })
            }
            FieldKind::Radio => {
                let shown = if self.value.is_empty() {
                    name
                } else {
                    self.value.as_str()
                };
                format!("({}) {shown}", if self.checked { "*" } else { " " })
            }
            FieldKind::Select(options) => {
                let shown = options
                    .iter()
                    .find(|(_, value)| *value == self.value)
                    .map_or(self.value.as_str(), |(label, _)| label.as_str());
                format!("[{shown} ▾]")
            }
            FieldKind::Password if self.value.is_empty() => format!("[{name}]"),
            FieldKind::Password => format!("[{}]", "•".repeat(self.value.chars().count())),
            FieldKind::Text | FieldKind::Textarea => {
                // Value, else the placeholder (carried in `label`), else name.
                let shown = if !self.value.is_empty() {
                    self.value.as_str()
                } else if !self.label.is_empty() {
                    self.label.as_str()
                } else {
                    name
                };
                format!("[{shown}]")
            }
        }
    }
}

/// A parsed HTML form: where it goes and what it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    pub method: FormMethod,
    pub action: url::Url,
    pub fields: Vec<Field>,
    /// Original `<form>` DOM node id inside the living page actor.
    pub live_node: Option<usize>,
}

impl Form {
    /// Serialize the successful fields urlencoded, `pressed` being the
    /// index of the submit control that fired (only that one is sent).
    pub fn encode(&self, pressed: usize) -> String {
        let mut out = url::form_urlencoded::Serializer::new(String::new());
        for (i, field) in self.fields.iter().enumerate() {
            if field.name.is_empty() {
                continue;
            }
            match &field.kind {
                FieldKind::Submit => {
                    if i == pressed {
                        out.append_pair(&field.name, &field.value);
                    }
                }
                FieldKind::Checkbox | FieldKind::Radio => {
                    if field.checked {
                        // "on" is the HTML default for value-less boxes.
                        let value = if field.value.is_empty() {
                            "on"
                        } else {
                            &field.value
                        };
                        out.append_pair(&field.name, value);
                    }
                }
                _ => {
                    out.append_pair(&field.name, &field.value);
                }
            }
        }
        out.finish()
    }
}

/// Styling class of a document line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Plain document text (gopher text files, gemtext paragraphs).
    Text,
    /// Gopher menu info lines.
    Info,
    /// Gopher error items (type 3).
    Error,
    /// Gopher menu link (type 1).
    Dir,
    /// Gopher text-file link (type 0).
    Document,
    /// Gopher search link (type 7).
    Search,
    /// Any other link: binary gopher items, `h` items, foreign schemes.
    OtherLink,
    /// Gemtext `=>` link line.
    GemLink,
    /// Gemtext heading, level 1-3.
    Heading(u8),
    /// Gemtext `* ` list item.
    List,
    /// Gemtext `> ` quote.
    Quote,
    /// Gemtext preformatted block content (never wrapped).
    Pre,
}

/// One display line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocLine {
    pub kind: Kind,
    pub text: String,
    pub link: Option<Link>,
}

/// A parsed, wrapped document plus everything needed to re-parse it
/// (terminal resize re-wraps; encoding switches re-decode).
/// Doc-carried handle to the page's blob byte mirror (`js::BlobMap`).
/// Equality is Arc IDENTITY — re-parses of the same live page share one map,
/// which is all the Doc's derived `PartialEq` needs.
#[derive(Clone, Debug)]
pub struct BlobsHandle(pub crate::js::BlobMap);

impl PartialEq for BlobsHandle {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for BlobsHandle {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Doc {
    /// The URL this document was fetched from.
    pub url: Link,
    pub lines: Vec<DocLine>,
    /// The body bytes as fetched.
    pub raw: Vec<u8>,
    /// Width `lines` was wrapped to.
    pub wrapped_to: usize,
    /// Whether `lines` was decoded as CP437 (gopher only).
    pub cp437: bool,
    /// The gemini response meta (media type), needed to re-parse.
    pub meta: Option<String>,
    /// HTML forms on the page; `Link::Form` rows index into this. Field
    /// values are live state — re-parses must seed from it.
    pub forms: Vec<Form>,
    /// HTTP-mode 2D layout: rows of positioned items. Empty for gopher/
    /// gemini/oneshot (which use the line model and gopherus nav) and for
    /// HTTP `text/*`; populated for HTML, where the browser renders and
    /// navigates these instead of `lines`.
    pub rows: Vec<crate::layout::Row>,
    /// Absolute http(s) URLs of every `<img>` on the page (HTML only), in
    /// document order. The app's decode pipeline fetches these; once
    /// decoded, a re-layout turns the alt-text placeholders into pixels.
    pub image_urls: Vec<String>,
    /// The page's `blob:` URL byte mirror (see `js::BlobMap`), set by the app
    /// from the JS response. The image pipeline decodes `<img src="blob:…">`
    /// from it (a client-generated image — Steam's login QR); it rides the Doc
    /// into history so a frozen page's blob images still render on back.
    pub blobs: Option<BlobsHandle>,
    /// Horizontally-scrollable strips (carousels) in `rows`, with their
    /// scroll offset. Empty except for HTML pages that have one.
    pub carousels: Vec<crate::layout::Carousel>,
    /// `position:fixed` boxes captured into the PINNED overlay layer, in
    /// viewport coordinates. The renderer draws these on top of the scrolling
    /// `rows` window at a fixed screen position (the document scrolls beneath
    /// them). Empty except for HTML pages with a pinned fixed box (a sidebar/
    /// header rail — Mastodon). See `crate::layout::FixedItem`.
    pub fixed: Vec<crate::layout::FixedItem>,
    /// Vertical inner-scroll viewports (`overflow-y:auto|scroll` regions) in
    /// `rows`. Each reserves blank rows in `rows` and holds its content in its
    /// own buffer (the view windows it). Empty except for HTML pages with one.
    pub regions: Vec<crate::layout::Region>,
    /// The CLIP box `(live_node, client_h_rows, client_w_cells)` of EVERY
    /// definite-height `overflow-y:auto|scroll` box — whether it overflowed into
    /// a `Region` or its content currently fits (no region). The app pushes these
    /// to the live engine as each element's `clientHeight`/`clientWidth` (Phase 3
    /// inner scroll) so a chat's `atBottom` is correct from the first message,
    /// before its content overflows into a region. Empty except for HTML pages.
    pub scroll_clips: Vec<(usize, u16, u16)>,
    /// Independent-formatting-context boundaries that lay their content INLINE in
    /// `rows` (NOT in a region/carousel buffer) — the cache for incremental
    /// layout's general subtree splice (INCREMENTAL_LAYOUT_PLAN.md §14). A live
    /// `Patched{node}` whose boundary is here re-lays ONLY that subtree and
    /// splices it back into `rows` (Tier 1 in-place / Tier 2 shift), leaving the
    /// rest of the document untouched. Captured on every full HTTP render of a
    /// live page; empty otherwise.
    pub boundaries: Vec<crate::layout::BoundaryBox>,
    /// Layout-DOM node → live actor node of its nearest enclosing hover host
    /// (the serializer's `data-trust-hover` markers, resolved at parse time —
    /// the parsed DOM doesn't survive layout, so hover targets must be mapped
    /// while it's alive). The app's hover hit-test reads an item's `node`
    /// here to learn which actor node should hear the pointer. Empty except
    /// for live HTML pages with hover listeners.
    pub hover_ids: std::collections::HashMap<crate::dom::NodeId, usize>,
    /// Fragment scroll targets: element `id` (and `<a name>`) → the FIRST
    /// `rows` index that element's box occupies, captured at parse time (the
    /// layout DOM doesn't survive into `Doc`). The app scrolls here when a
    /// `#fragment` link / URL / live hash-change targets that anchor — HTML's
    /// "scroll to the fragment". Empty for line-model docs (gopher/gemini/text).
    pub anchor_rows: std::collections::HashMap<String, usize>,
}

impl Doc {
    /// A line-model document (gopher / gemini / oneshot / plain text): only
    /// `lines` is populated; the 2D-layout artifacts (`rows`, `image_urls`,
    /// `carousels`, `fixed`, `regions`, `scroll_clips`, `boundaries`) and
    /// `forms` all start empty. Protocol parsers build through this so a new
    /// HTTP/layout field defaults HERE, never in them — those parsers are meant
    /// to stay simple and never change for an HTTP-only feature.
    pub fn from_lines(
        url: Link,
        lines: Vec<DocLine>,
        raw: Vec<u8>,
        wrapped_to: usize,
        cp437: bool,
        meta: Option<String>,
    ) -> Doc {
        Doc {
            url,
            lines,
            raw,
            wrapped_to,
            cp437,
            meta,
            forms: Vec::new(),
            rows: Vec::new(),
            image_urls: Vec::new(),
            blobs: None,
            carousels: Vec::new(),
            fixed: Vec::new(),
            regions: Vec::new(),
            scroll_clips: Vec::new(),
            boundaries: Vec::new(),
            hover_ids: std::collections::HashMap::new(),
            anchor_rows: std::collections::HashMap::new(),
        }
    }

    /// Whether this document uses the HTTP 2D layout (rows of items)
    /// rather than the gopher/gemini line model.
    pub fn laid_out(&self) -> bool {
        !self.rows.is_empty()
    }

    /// The vertical extent for scrolling: layout rows when laid out, else
    /// document lines.
    pub fn extent(&self) -> usize {
        if self.laid_out() {
            self.rows.len()
        } else {
            self.lines.len()
        }
    }
}

/// Wrap a plain-text body (gemini `text/*`, web `text/plain`) into
/// document lines.
pub fn wrap_plain(text: &str, width: usize) -> Vec<DocLine> {
    let mut out = Vec::new();
    for line in text.lines() {
        push_wrapped(
            &mut out,
            Kind::Text,
            line.trim_end().to_string(),
            None,
            width,
        );
    }
    out
}

/// Push a display line, word-wrapping it to `width`. Continuation rows
/// keep the kind (for styling) but never the link, so only an item's
/// first row is selectable.
pub fn push_wrapped(
    out: &mut Vec<DocLine>,
    kind: Kind,
    text: String,
    link: Option<Link>,
    width: usize,
) {
    if text.chars().count() <= width {
        out.push(DocLine { kind, text, link });
        return;
    }
    let mut link = link;
    for piece in textwrap::wrap(&text, width) {
        out.push(DocLine {
            kind,
            text: piece.into_owned(),
            link: link.take(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, value: &str, kind: FieldKind) -> Field {
        Field {
            name: name.into(),
            value: value.into(),
            checked: false,
            label: String::new(),
            kind,
            live_node: None,
        }
    }

    #[test]
    fn encodes_form_submissions() {
        let mut form = Form {
            method: FormMethod::Post,
            action: url::Url::parse("https://example.com/chat").unwrap(),
            fields: vec![
                field("session", "cafe123", FieldKind::Hidden),
                field("msg", "hello there & good night", FieldKind::Text),
                field("box", "", FieldKind::Checkbox),
                field("pick", "b", FieldKind::Radio),
                field("", "anonymous", FieldKind::Text), // nameless: skipped
                field("go", "Send", FieldKind::Submit),
                field("alt", "Other", FieldKind::Submit),
            ],
            live_node: None,
        };
        // Unchecked boxes stay home; only the pressed submit is sent;
        // spaces and ampersands urlencode.
        assert_eq!(
            form.encode(5),
            "session=cafe123&msg=hello+there+%26+good+night&go=Send"
        );
        form.fields[2].checked = true;
        form.fields[3].checked = true;
        assert_eq!(
            form.encode(6),
            "session=cafe123&msg=hello+there+%26+good+night&box=on&pick=b&alt=Other"
        );
    }

    #[test]
    fn renders_field_widgets() {
        assert_eq!(
            field("msg", "", FieldKind::Text).row_label(),
            "[msg]",
            "empty value, no placeholder falls back to the name"
        );
        let mut f = field("pw", "secret", FieldKind::Password);
        assert_eq!(f.row_label(), "[••••••]");
        f = field("box", "", FieldKind::Checkbox);
        assert_eq!(f.row_label(), "[ ] box");
        f.checked = true;
        assert_eq!(f.row_label(), "[x] box");
    }
}
