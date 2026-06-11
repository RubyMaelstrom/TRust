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
    /// An HTML form control: indices into the document's `forms`.
    Form {
        form: usize,
        field: usize,
    },
    /// A scheme we don't speak (mailto, irc, ...) — shown, not followed.
    External(String),
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Link::Gopher(url) => url.fmt(f),
            Link::Gemini(url) => url.fmt(f),
            Link::Http(url) => url.fmt(f),
            Link::Form { .. } => f.write_str("form control"),
            Link::External(url) => f.write_str(url),
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
}

impl Field {
    /// The widget row the browser shows for this control.
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
                format!("[{name}: {shown} ▾]")
            }
            FieldKind::Password => {
                format!("[{name}: {}]", "•".repeat(self.value.chars().count()))
            }
            FieldKind::Text | FieldKind::Textarea => {
                let shown = if self.value.is_empty() {
                    self.label.as_str()
                } else {
                    self.value.as_str()
                };
                format!("[{name}: {shown}]")
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
    /// HTML form input widget row.
    Input,
    /// HTML form submit button row.
    Button,
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
    /// HTML forms on the page; `Link::Form` rows index into this. Field
    /// values are live state — re-parses must seed from it.
    pub forms: Vec<Form>,
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
            "[msg: ]",
            "empty value, no placeholder"
        );
        let mut f = field("pw", "secret", FieldKind::Password);
        assert_eq!(f.row_label(), "[pw: ••••••]");
        f = field("box", "", FieldKind::Checkbox);
        assert_eq!(f.row_label(), "[ ] box");
        f.checked = true;
        assert_eq!(f.row_label(), "[x] box");
    }
}
