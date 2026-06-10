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
    /// A scheme we don't speak (mailto, irc, ...) — shown, not followed.
    External(String),
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Link::Gopher(url) => url.fmt(f),
            Link::Gemini(url) => url.fmt(f),
            Link::Http(url) => url.fmt(f),
            Link::External(url) => f.write_str(url),
        }
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
