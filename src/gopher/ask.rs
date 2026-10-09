//! UMN Gopher+ (Anklesaria et al., July 30 1993) §2.8 "Gopher+ Interactive
//! Query items": the +ASK attribute block, its presentation as a form, and the
//! client data block (§§2.3–2.4, Appendix II "Retrieving a document") that
//! returns the answers with `selector TAB + TAB 1`. RFC 4266 §2.5 describes the
//! same exchange for `?`-tagged URLs.
//!
//! Local copy: /big/web-standards/documents/gopher-plus-*/ (retrieved
//! 2026-10-08 from the Floodgap Gopher archive; the UMN FTP original cited by
//! RFC 4266 is offline). The memo names the question types but leaves each
//! answer's line encoding implicit. The reference UMN client (gopher 3.0.x,
//! `ourutils.c` AskBlock and `GSgopherobj.c` GStransmit) fixes it, and this
//! module follows it:
//!
//! * `Note:` is a label with no answer line; a line without a colon is an
//!   empty label.
//! * `Ask:`, `AskP:`, `AskF:` and unrecognized types answer with the typed
//!   text; a TAB introduces a default answer.
//! * `AskL:` answers with a line holding the number of lines, then the lines
//!   up to the last non-empty one (§2.8).
//! * `Choose:` lists TAB-separated options; the chosen option's text is the
//!   answer.
//! * `Select:` is one check box; a `:1` or `:0` suffix is its default, and
//!   the answer is `1` or `0`.
//! * `ChooseF:` selects the one file a form may send (§2.8): it is the
//!   remainder of the data block, after every answer line.
//!
//! The data block is `+-1` period-terminated text (§2.3) with RFC 1436's
//! transparency rule (Appendix, TextFile note: an answer line beginning with
//! a period gets an extra period). A form carrying a file uses the exact byte
//! count form (`+<length>`) instead, because arbitrary file bytes cannot be
//! period-terminated. Answers are bounded; the file must be no larger than
//! [`MAX_FILE`].

use super::{Encoding, GopherUrl, decode, split_lines};
use crate::doc::{DocLine, Kind, Link};
use std::fmt;

/// Questions in one form. UMN allotted a fixed screen; this is a memory bound.
pub const MAX_CONTROLS: usize = 256;
/// Options in one `Choose:` question (UMN's CURChoice limit is 99).
pub const MAX_OPTIONS: usize = 99;
/// Bytes in one answer line, after encoding.
pub const MAX_ANSWER: usize = 4096;
/// Lines in one `AskL:` answer.
pub const MAX_LINES: usize = 1000;
/// All answer lines together, including line breaks and the count lines.
pub const MAX_TEXT: usize = 64 * 1024;
/// The one file a `ChooseF:` question may send.
pub const MAX_FILE: u64 = 16 * 1024 * 1024;

const ACTION_PREFIX: &str = "about:gopher-form/";

/// An answer keeps the server's default bytes until the user types, so an
/// untouched default is returned exactly as received, whatever the display
/// encoding.
#[derive(Clone, PartialEq, Eq)]
enum Answer {
    Default(Vec<u8>),
    Typed(String),
}

impl Answer {
    fn text(&self, encoding: Encoding) -> String {
        match self {
            Self::Default(bytes) => decode(bytes, encoding),
            Self::Typed(text) => text.clone(),
        }
    }
    fn bytes(&self, encoding: Encoding, utf8: bool) -> Result<Vec<u8>, String> {
        match self {
            Self::Default(bytes) => Ok(bytes.clone()),
            Self::Typed(text) => encode(text, encoding, utf8),
        }
    }
    fn is_empty(&self) -> bool {
        match self {
            Self::Default(bytes) => bytes.is_empty(),
            Self::Typed(text) => text.is_empty(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    /// `Ask:` and question types this client does not recognize (UMN treats
    /// them as ordinary prompts).
    Line,
    /// `AskP:` — never echoed (§2.8 "bullets out the response").
    Password,
    /// `AskF:` — a new local filename for the server's reply (§2.8).
    Filename,
}

#[derive(Clone, PartialEq, Eq)]
enum Control {
    Note(Vec<u8>),
    Text {
        kind: TextKind,
        prompt: Vec<u8>,
        default: Vec<u8>,
        answer: Answer,
    },
    Lines {
        prompt: Vec<u8>,
        default: Vec<Answer>,
        lines: Vec<Answer>,
    },
    Choose {
        prompt: Vec<u8>,
        options: Vec<Vec<u8>>,
        selected: usize,
    },
    Select {
        prompt: Vec<u8>,
        default: bool,
        checked: bool,
    },
    File {
        prompt: Vec<u8>,
        /// Only the first `ChooseF:` can send its file (§2.8).
        usable: bool,
        file: Option<crate::file_select::SelectedFile>,
    },
}

/// A Gopher+ item's ASK form and the user's answers. The item is the public
/// `?` destination: it carries no answers, so cloning it into history or a
/// bookmark never discloses them.
#[derive(Clone, PartialEq, Eq)]
pub struct Form {
    item: GopherUrl,
    title: Vec<u8>,
    controls: Vec<Control>,
    /// Automatic decoding showed this block as UTF-8 (else Latin-1).
    utf8: bool,
    /// Questions beyond the bounds above were not shown.
    truncated: bool,
}

impl fmt::Debug for Form {
    // Answers can be passwords: never format them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Form")
            .field("item", &self.item)
            .field("questions", &self.controls.len())
            .finish_non_exhaustive()
    }
}

/// What activating one of the form's links asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Edit(usize),
    EditLine(usize, usize),
    AddLine(usize),
    Choose(usize, usize),
    Toggle(usize),
    File(usize),
    ClearFile(usize),
    Submit,
    Reset,
}

impl Action {
    pub fn link(self) -> Link {
        let path = match self {
            Self::Edit(n) => format!("edit/{n}"),
            Self::EditLine(n, line) => format!("line/{n}/{line}"),
            Self::AddLine(n) => format!("add-line/{n}"),
            Self::Choose(n, option) => format!("choose/{n}/{option}"),
            Self::Toggle(n) => format!("toggle/{n}"),
            Self::File(n) => format!("file/{n}"),
            Self::ClearFile(n) => format!("clear-file/{n}"),
            Self::Submit => "submit".into(),
            Self::Reset => "reset".into(),
        };
        Link::External(format!("{ACTION_PREFIX}{path}"))
    }

    /// Only links of this exact shape are form actions. A frontend must also
    /// check that the current document is this form before acting on one:
    /// any page can spell such an address.
    pub fn from_link(link: &Link) -> Option<Self> {
        let Link::External(value) = link else {
            return None;
        };
        let rest = value.strip_prefix(ACTION_PREFIX)?;
        let mut parts = rest.split('/');
        let name = parts.next()?;
        let mut number = || -> Option<usize> {
            let part = parts.next()?;
            (!part.is_empty() && part.len() <= 6 && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse().ok())
                .flatten()
        };
        let action = match name {
            "edit" => Self::Edit(number()?),
            "line" => Self::EditLine(number()?, number()?),
            "add-line" => Self::AddLine(number()?),
            "choose" => Self::Choose(number()?, number()?),
            "toggle" => Self::Toggle(number()?),
            "file" => Self::File(number()?),
            "clear-file" => Self::ClearFile(number()?),
            "submit" => Self::Submit,
            "reset" => Self::Reset,
            _ => return None,
        };
        parts.next().is_none().then_some(action)
    }
}

/// A text answer the frontend should collect with its one-line editor.
#[derive(Clone, PartialEq, Eq)]
pub struct Edit {
    pub control: usize,
    pub line: Option<usize>,
    pub label: String,
    /// Prefill for the editor; always empty for a password.
    pub initial: String,
    pub sensitive: bool,
}

impl fmt::Debug for Edit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Edit")
            .field("control", &self.control)
            .field("line", &self.line)
            .field("sensitive", &self.sensitive)
            .finish_non_exhaustive()
    }
}

/// The request a submitted form becomes: the item's `+` view with the
/// answers as its data block. `save_as` is an `AskF:` filename.
#[derive(Clone, Debug)]
pub struct Submission {
    pub url: GopherUrl,
    pub save_as: Option<String>,
}

/// Encode typed text in the page's display encoding. Automatic decoding
/// answers in the encoding it showed the form in.
fn encode(text: &str, encoding: Encoding, utf8: bool) -> Result<Vec<u8>, String> {
    match encoding {
        Encoding::Utf8 => Ok(text.as_bytes().to_vec()),
        Encoding::Auto if utf8 => Ok(text.as_bytes().to_vec()),
        Encoding::Auto | Encoding::Latin1 => text
            .chars()
            .map(|c| u8::try_from(u32::from(c)).map_err(|_| c))
            .collect::<Result<Vec<u8>, char>>()
            .map_err(|c| {
                format!("'{c}' cannot be sent in Latin-1; press E to choose UTF-8 for this form.")
            }),
        Encoding::Cp437 => crate::cp437::encode(text).map_err(|c| {
            format!("'{c}' cannot be sent in CP437; press E to choose another encoding.")
        }),
    }
}

fn split_default(rest: &[u8]) -> (&[u8], Option<&[u8]>) {
    match rest.iter().position(|b| *b == b'\t') {
        Some(tab) => (&rest[..tab], Some(&rest[tab + 1..])),
        None => (rest, None),
    }
}

impl Form {
    /// Build the form from an attribute reply (`selector TAB !`). Returns
    /// `None` when the reply holds no +ASK block. `item` is the `?` item the
    /// user selected; answers always go back to it, not to a destination the
    /// reply names.
    pub fn parse(item: &GopherUrl, body: &[u8]) -> Option<Self> {
        let mut lines = split_lines(body);
        let mut title = Vec::new();
        let mut in_ask = false;
        let mut found = false;
        let mut controls = Vec::new();
        let mut truncated = false;
        let mut files = 0;
        for raw in &mut lines {
            if raw.starts_with(b"+") {
                if let Some(info) = raw.strip_prefix(b"+INFO:")
                    && title.is_empty()
                {
                    let info = info.strip_prefix(b" ").unwrap_or(info);
                    if let Some((&_kind, rest)) = info.split_first() {
                        title = rest.split(|b| *b == b'\t').next().unwrap_or(&[]).to_vec();
                    }
                }
                // Block names are case-sensitive (Appendix II Blockname).
                in_ask = raw.starts_with(b"+ASK:");
                found |= in_ask;
                continue;
            }
            if !in_ask {
                continue;
            }
            if controls.len() == MAX_CONTROLS {
                truncated = true;
                continue;
            }
            // Attrval ::= SPACE {UNASCII} CR-LF
            let line = raw.strip_prefix(b" ").unwrap_or(raw);
            let Some(colon) = line.iter().position(|b| *b == b':') else {
                controls.push(Control::Note(Vec::new()));
                continue;
            };
            let kind = &line[..colon];
            let rest = &line[colon + 1..];
            let rest = rest.strip_prefix(b" ").unwrap_or(rest);
            let (prompt, default) = split_default(rest);
            let is = |name: &str| kind.eq_ignore_ascii_case(name.as_bytes());
            let control = if is("Note") {
                Control::Note(rest.to_vec())
            } else if is("Choose") {
                let mut options: Vec<Vec<u8>> = default
                    .map(|options| options.split(|b| *b == b'\t').map(<[u8]>::to_vec).collect())
                    .unwrap_or_default();
                truncated |= options.len() > MAX_OPTIONS;
                options.truncate(MAX_OPTIONS);
                Control::Choose {
                    prompt: prompt.to_vec(),
                    options,
                    selected: 0,
                }
            } else if is("Select") {
                // UMN: the default follows the prompt's last colon.
                let (prompt, checked) = match prompt.iter().rposition(|b| *b == b':') {
                    Some(at) => (&prompt[..at], prompt.get(at + 1) == Some(&b'1')),
                    None => (prompt, false),
                };
                Control::Select {
                    prompt: prompt.to_vec(),
                    default: checked,
                    checked,
                }
            } else if is("AskL") {
                let default: Vec<Answer> = default
                    .filter(|d| !d.is_empty())
                    .map(|d| vec![Answer::Default(d.to_vec())])
                    .unwrap_or_default();
                Control::Lines {
                    prompt: prompt.to_vec(),
                    lines: default.clone(),
                    default,
                }
            } else if is("ChooseF") {
                files += 1;
                Control::File {
                    prompt: prompt.to_vec(),
                    usable: files == 1,
                    file: None,
                }
            } else {
                let kind = if is("AskP") {
                    TextKind::Password
                } else if is("AskF") {
                    TextKind::Filename
                } else {
                    TextKind::Line
                };
                let default = default.unwrap_or_default().to_vec();
                Control::Text {
                    kind,
                    prompt: prompt.to_vec(),
                    answer: Answer::Default(default.clone()),
                    default,
                }
            };
            controls.push(control);
        }
        found.then(|| Self {
            item: item.public(),
            title,
            controls,
            utf8: std::str::from_utf8(body).is_ok(),
            truncated,
        })
    }

    pub fn item(&self) -> &GopherUrl {
        &self.item
    }

    fn prompt_text(prompt: &[u8], encoding: Encoding) -> String {
        let text = decode(prompt, encoding);
        let text = text.trim();
        if text.is_empty() {
            "Answer".into()
        } else {
            text.into()
        }
    }

    /// The form as browser lines; links are [`Action`]s.
    pub fn lines(&self, encoding: Encoding) -> Vec<DocLine> {
        let line = |kind, text: String, link: Option<Link>| DocLine { kind, text, link };
        let mut out = Vec::new();
        let title = decode(&self.title, encoding);
        out.push(line(
            Kind::Heading(1),
            if title.trim().is_empty() {
                "Gopher+ form".into()
            } else {
                title.trim().into()
            },
            None,
        ));
        out.push(line(
            Kind::Info,
            format!(
                "Gopher+ form · your answers are sent to {}:{}{} when you submit.",
                self.item.host,
                self.item.port,
                if self.item.tls { " over TLS" } else { "" }
            ),
            None,
        ));
        for (index, control) in self.controls.iter().enumerate() {
            match control {
                Control::Note(text) => {
                    out.push(line(Kind::Info, decode(text, encoding), None));
                }
                Control::Text {
                    kind,
                    prompt,
                    answer,
                    ..
                } => {
                    let value = answer.text(encoding);
                    let shown = match kind {
                        TextKind::Password => "•".repeat(value.chars().count()),
                        _ => value,
                    };
                    out.push(line(
                        Kind::Search,
                        format!("{}: [{shown}]", Self::prompt_text(prompt, encoding)),
                        Some(Action::Edit(index).link()),
                    ));
                    if *kind == TextKind::Filename {
                        out.push(line(
                            Kind::Info,
                            "  A filename here offers the reply for saving under that name.".into(),
                            None,
                        ));
                    }
                }
                Control::Lines { prompt, lines, .. } => {
                    out.push(line(Kind::Text, Self::prompt_text(prompt, encoding), None));
                    for (number, value) in lines.iter().enumerate() {
                        out.push(line(
                            Kind::Search,
                            format!("  {}│ {}", number + 1, value.text(encoding)),
                            Some(Action::EditLine(index, number).link()),
                        ));
                    }
                    if lines.len() < MAX_LINES {
                        out.push(line(
                            Kind::OtherLink,
                            "  + Add a line".into(),
                            Some(Action::AddLine(index).link()),
                        ));
                    }
                }
                Control::Choose {
                    prompt,
                    options,
                    selected,
                } => {
                    out.push(line(Kind::Text, Self::prompt_text(prompt, encoding), None));
                    if options.is_empty() {
                        out.push(line(
                            Kind::Error,
                            "  The server listed no choices for this question.".into(),
                            None,
                        ));
                    }
                    for (option, text) in options.iter().enumerate() {
                        out.push(line(
                            Kind::OtherLink,
                            format!(
                                "  ({}) {}",
                                if option == *selected { "•" } else { " " },
                                decode(text, encoding)
                            ),
                            Some(Action::Choose(index, option).link()),
                        ));
                    }
                }
                Control::Select {
                    prompt, checked, ..
                } => out.push(line(
                    Kind::OtherLink,
                    format!(
                        "[{}] {}",
                        if *checked { "x" } else { " " },
                        Self::prompt_text(prompt, encoding)
                    ),
                    Some(Action::Toggle(index).link()),
                )),
                Control::File {
                    prompt,
                    usable,
                    file,
                } => {
                    out.push(line(Kind::Text, Self::prompt_text(prompt, encoding), None));
                    if !usable {
                        out.push(line(
                            Kind::Info,
                            "  Gopher+ sends one file per form; the earlier file question is used."
                                .into(),
                            None,
                        ));
                        continue;
                    }
                    out.push(line(
                        Kind::OtherLink,
                        match file {
                            Some(file) => format!(
                                "  [Choose file…] {} ({})",
                                file.name,
                                crate::download::human_bytes(file.size)
                            ),
                            None => "  [Choose file…] No file selected.".into(),
                        },
                        Some(Action::File(index).link()),
                    ));
                    if file.is_some() {
                        out.push(line(
                            Kind::OtherLink,
                            "  Remove file".into(),
                            Some(Action::ClearFile(index).link()),
                        ));
                    }
                }
            }
        }
        if self.truncated {
            out.push(line(
                Kind::Error,
                "Some questions or choices exceed TRust's form limits and are not shown; the form cannot be submitted."
                    .into(),
                None,
            ));
        }
        out.push(line(Kind::Text, String::new(), None));
        out.push(line(
            Kind::OtherLink,
            "[ Submit ]".into(),
            Some(Action::Submit.link()),
        ));
        out.push(line(
            Kind::OtherLink,
            "[ Reset answers ]".into(),
            Some(Action::Reset.link()),
        ));
        out.push(line(
            Kind::Document,
            "Item information".into(),
            Some(Link::Gopher(self.item.with_plus(b"!"))),
        ));
        out
    }

    /// The editor request for a text answer, or `None` for other controls.
    pub fn edit(&self, control: usize, line: Option<usize>, encoding: Encoding) -> Option<Edit> {
        match (self.controls.get(control)?, line) {
            (
                Control::Text {
                    kind,
                    prompt,
                    answer,
                    ..
                },
                None,
            ) => {
                let sensitive = *kind == TextKind::Password;
                Some(Edit {
                    control,
                    line: None,
                    label: Self::prompt_text(prompt, encoding),
                    // Never place a secret in an editor the user did not type in.
                    initial: if sensitive {
                        String::new()
                    } else {
                        answer.text(encoding)
                    },
                    sensitive,
                })
            }
            (Control::Lines { prompt, lines, .. }, Some(number)) => {
                let initial = if number < lines.len() {
                    lines[number].text(encoding)
                } else if number == lines.len() && number < MAX_LINES {
                    String::new()
                } else {
                    return None;
                };
                Some(Edit {
                    control,
                    line: Some(number),
                    label: format!(
                        "{} — line {}",
                        Self::prompt_text(prompt, encoding),
                        number + 1
                    ),
                    initial,
                    sensitive: false,
                })
            }
            _ => None,
        }
    }

    fn check_line(text: &str) -> Result<(), String> {
        if text.contains(['\r', '\n', '\0']) {
            return Err("A Gopher+ answer is one line: it cannot contain line breaks.".into());
        }
        if text.len() > MAX_ANSWER {
            return Err(format!(
                "A Gopher+ answer is limited to {MAX_ANSWER} bytes."
            ));
        }
        Ok(())
    }

    /// Store an answer typed for `edit`.
    pub fn set_text(
        &mut self,
        control: usize,
        line: Option<usize>,
        text: &str,
    ) -> Result<(), String> {
        Self::check_line(text)?;
        match (self.controls.get_mut(control), line) {
            (Some(Control::Text { answer, .. }), None) => {
                *answer = Answer::Typed(text.into());
                Ok(())
            }
            (Some(Control::Lines { lines, .. }), Some(number)) if number < lines.len() => {
                lines[number] = Answer::Typed(text.into());
                Ok(())
            }
            (Some(Control::Lines { lines, .. }), Some(number))
                if number == lines.len() && number < MAX_LINES =>
            {
                lines.push(Answer::Typed(text.into()));
                Ok(())
            }
            _ => Err("This form question no longer exists.".into()),
        }
    }

    /// Apply a non-text action. `Edit`/`EditLine`/`AddLine`/`File`/`Submit`
    /// need frontend input and return the edit or an error instead.
    pub fn apply(&mut self, action: Action) -> Result<(), String> {
        match action {
            Action::Choose(control, option) => match self.controls.get_mut(control) {
                Some(Control::Choose {
                    options, selected, ..
                }) if option < options.len() => {
                    *selected = option;
                    Ok(())
                }
                _ => Err("This choice is not part of the form.".into()),
            },
            Action::Toggle(control) => match self.controls.get_mut(control) {
                Some(Control::Select { checked, .. }) => {
                    *checked = !*checked;
                    Ok(())
                }
                _ => Err("This check box is not part of the form.".into()),
            },
            Action::ClearFile(control) => match self.controls.get_mut(control) {
                Some(Control::File { file, .. }) => {
                    *file = None;
                    Ok(())
                }
                _ => Err("This file question is not part of the form.".into()),
            },
            Action::Reset => {
                self.reset();
                Ok(())
            }
            _ => Err("This form action needs input.".into()),
        }
    }

    /// The number of lines an `AskL:` answer has so far.
    pub fn line_count(&self, control: usize) -> Option<usize> {
        match self.controls.get(control)? {
            Control::Lines { lines, .. } => Some(lines.len()),
            _ => None,
        }
    }

    /// The editor request for an [`Action`] that collects text.
    pub fn edit_for(&self, action: Action, encoding: Encoding) -> Option<Edit> {
        match action {
            Action::Edit(control) => self.edit(control, None, encoding),
            Action::EditLine(control, line) => self.edit(control, Some(line), encoding),
            Action::AddLine(control) => {
                self.edit(control, Some(self.line_count(control)?), encoding)
            }
            _ => None,
        }
    }

    /// Whether `control` is the form's file question.
    pub fn accepts_file(&self, control: usize) -> bool {
        matches!(
            self.controls.get(control),
            Some(Control::File { usable: true, .. })
        )
    }

    /// Record the file the user chose for the `ChooseF:` question.
    pub fn set_file(&mut self, control: usize, path: &std::path::Path) -> Result<String, String> {
        let Some(Control::File {
            usable: true, file, ..
        }) = self.controls.get_mut(control)
        else {
            return Err("This file question is not part of the form.".into());
        };
        let selected = crate::file_select::SelectedFile::from_path(path, String::new())?;
        if selected.size > MAX_FILE {
            return Err(format!(
                "Gopher+ forms send files of up to {}.",
                crate::download::human_bytes(MAX_FILE)
            ));
        }
        let name = selected.name.clone();
        *file = Some(selected);
        Ok(name)
    }

    pub fn reset(&mut self) {
        for control in &mut self.controls {
            match control {
                Control::Text {
                    default, answer, ..
                } => *answer = Answer::Default(default.clone()),
                Control::Lines { default, lines, .. } => *lines = default.clone(),
                Control::Choose { selected, .. } => *selected = 0,
                Control::Select {
                    default, checked, ..
                } => *checked = *default,
                Control::File { file, .. } => *file = None,
                Control::Note(_) => {}
            }
        }
    }

    /// Forget typed passwords: a submitted or abandoned form keeps no secret
    /// in history (§2.8 AskP).
    pub fn clear_secrets(&mut self) {
        for control in &mut self.controls {
            if let Control::Text {
                kind: TextKind::Password,
                default,
                answer,
                ..
            } = control
            {
                *answer = Answer::Default(default.clone());
            }
        }
    }

    /// Encode the answers as the §2.4 data block and pair them with the
    /// item's default view. Reads the chosen file now, against its snapshot.
    pub fn submission(&self, encoding: Encoding) -> Result<Submission, String> {
        if self.truncated {
            return Err(
                "This form exceeds TRust's limits, so it cannot be answered completely.".into(),
            );
        }
        let mut answers: Vec<Vec<u8>> = Vec::new();
        let mut file = None;
        let mut save_as = None;
        let mut text = 0usize;
        let mut push = |answers: &mut Vec<Vec<u8>>, bytes: Vec<u8>| -> Result<(), String> {
            if bytes.iter().any(|b| matches!(b, b'\r' | b'\n' | b'\0')) {
                return Err("A Gopher+ answer is one line: it cannot contain line breaks.".into());
            }
            if bytes.len() > MAX_ANSWER {
                return Err(format!(
                    "A Gopher+ answer is limited to {MAX_ANSWER} bytes."
                ));
            }
            text += bytes.len() + 3;
            if text > MAX_TEXT {
                return Err("The form's answers exceed 64 KiB.".into());
            }
            answers.push(bytes);
            Ok(())
        };
        for control in &self.controls {
            match control {
                Control::Note(_) => {}
                Control::Text { kind, answer, .. } => {
                    let bytes = answer.bytes(encoding, self.utf8)?;
                    if *kind == TextKind::Filename && !answer.is_empty() {
                        save_as = Some(answer.text(encoding));
                    }
                    push(&mut answers, bytes)?;
                }
                Control::Lines { lines, .. } => {
                    // UMN counts lines up to the last non-empty one.
                    let used = lines
                        .iter()
                        .rposition(|line| !line.is_empty())
                        .map_or(0, |last| last + 1);
                    push(&mut answers, used.to_string().into_bytes())?;
                    for line in &lines[..used] {
                        push(&mut answers, line.bytes(encoding, self.utf8)?)?;
                    }
                }
                Control::Choose {
                    options, selected, ..
                } => push(
                    &mut answers,
                    options.get(*selected).cloned().unwrap_or_default(),
                )?,
                Control::Select { checked, .. } => {
                    push(&mut answers, if *checked { b"1" } else { b"0" }.to_vec())?
                }
                Control::File {
                    usable: true,
                    file: Some(selected),
                    ..
                } => {
                    if selected.size > MAX_FILE {
                        return Err(format!(
                            "Gopher+ forms send files of up to {}.",
                            crate::download::human_bytes(MAX_FILE)
                        ));
                    }
                    let bytes = selected
                        .read()
                        .map_err(|error| format!("{}: {}", selected.name, error.message()))?;
                    if bytes.len() as u64 > MAX_FILE {
                        return Err(format!(
                            "Gopher+ forms send files of up to {}.",
                            crate::download::human_bytes(MAX_FILE)
                        ));
                    }
                    file = Some(bytes);
                }
                Control::File { .. } => {}
            }
        }
        let block = data_block(&answers, file.as_deref());
        let mut url = self.item.with_plus(b"+");
        url.ask = Some(super::AskData::new(block));
        url.request()?;
        Ok(Submission { url, save_as })
    }
}

/// §2.3 DataBlock: `+-1` period-terminated answers, or the exact length when
/// a file follows them.
pub(super) fn data_block(answers: &[Vec<u8>], file: Option<&[u8]>) -> Vec<u8> {
    match file {
        None => {
            let mut block = b"+-1\r\n".to_vec();
            for answer in answers {
                if answer.starts_with(b".") {
                    block.push(b'.');
                }
                block.extend_from_slice(answer);
                block.extend_from_slice(b"\r\n");
            }
            block.extend_from_slice(b".\r\n");
            block
        }
        Some(file) => {
            let mut body = Vec::new();
            for answer in answers {
                body.extend_from_slice(answer);
                body.extend_from_slice(b"\r\n");
            }
            body.extend_from_slice(file);
            let mut block = format!("+{}\r\n", body.len()).into_bytes();
            block.extend_from_slice(&body);
            block
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> GopherUrl {
        GopherUrl::new("e.test".into(), 70, '0', b"/form".to_vec()).with_plus(b"?")
    }

    const BLOCK: &[u8] = b"+INFO: 0Inquisitive\t/form\te.test\t70\t?\r\n+ADMIN:\r\n Admin: Frank <fng@bogus.edu>\r\n+ASK:\r\n Ask: How many volts?\t110\r\n Choose: Deliver electric shock to administrator now?\tYes\tNot!\r\n AskP: Password\r\n Select: Send a copy:1\r\n AskL: Comments\r\n Note: Thank you.\r\n Frob: Unknown kind\r\n";

    #[test]
    fn umn_example_parses_every_question_type() {
        let form = Form::parse(&item(), BLOCK).unwrap();
        assert_eq!(form.title, b"Inquisitive");
        assert_eq!(form.controls.len(), 7);
        let lines = form.lines(Encoding::Auto);
        let text: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert!(text.contains(&"How many volts?: [110]"));
        assert!(text.contains(&"  (•) Yes"));
        assert!(text.contains(&"  ( ) Not!"));
        assert!(text.contains(&"Password: []"));
        assert!(text.contains(&"[x] Send a copy"));
        assert!(text.contains(&"Thank you."));
        assert!(text.contains(&"Unknown kind: []"));
        assert!(lines.iter().any(|l| l.link == Some(Action::Submit.link())));
    }

    #[test]
    fn answers_follow_the_reference_client_line_encoding() {
        let mut form = Form::parse(&item(), BLOCK).unwrap();
        form.apply(Action::Choose(1, 1)).unwrap();
        form.set_text(2, None, "hunter2").unwrap();
        form.apply(Action::Toggle(3)).unwrap();
        form.set_text(4, Some(0), "first").unwrap();
        form.set_text(4, Some(1), ".dot").unwrap();
        form.set_text(4, Some(2), "").unwrap();
        form.set_text(6, None, "x").unwrap();
        let submission = form.submission(Encoding::Auto).unwrap();
        assert_eq!(
            submission.url.request().unwrap(),
            b"/form\t+\t1\r\n+-1\r\n110\r\nNot!\r\nhunter2\r\n0\r\n2\r\nfirst\r\n..dot\r\nx\r\n.\r\n"
        );
        assert!(submission.save_as.is_none());
        // Display, Debug and the public destination never carry answers.
        let shown = format!("{} {:?} {:?}", submission.url, submission.url, form);
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(submission.url.public(), item());
        // A submitted form forgets the password but keeps other answers.
        form.clear_secrets();
        let again = form.submission(Encoding::Auto).unwrap();
        assert!(
            !again
                .url
                .request()
                .unwrap()
                .windows(7)
                .any(|w| w == b"hunter2")
        );
    }

    #[test]
    fn answers_are_single_bounded_lines_and_encodings_are_explicit() {
        let mut form = Form::parse(&item(), BLOCK).unwrap();
        assert!(form.set_text(0, None, "a\r\nQUIT").is_err());
        assert!(form.set_text(0, None, &"x".repeat(MAX_ANSWER + 1)).is_err());
        form.set_text(0, None, "café").unwrap();
        let utf8 = form.submission(Encoding::Utf8).unwrap();
        assert!(
            utf8.url
                .request()
                .unwrap()
                .windows(5)
                .any(|w| w == "café".as_bytes())
        );
        let latin1 = form.submission(Encoding::Latin1).unwrap();
        assert!(
            latin1
                .url
                .request()
                .unwrap()
                .windows(4)
                .any(|w| w == b"caf\xe9")
        );
        form.set_text(0, None, "日本").unwrap();
        assert!(form.submission(Encoding::Latin1).is_err());
        assert!(form.submission(Encoding::Cp437).is_err());
        // AskL beyond its bound is refused rather than silently truncated.
        let long = "l".repeat(100);
        for line in 0..MAX_LINES {
            form.set_text(4, Some(line), &long).unwrap();
        }
        assert!(form.set_text(4, Some(MAX_LINES), "l").is_err());
        assert!(form.submission(Encoding::Utf8).is_err(), "64 KiB total");
    }

    #[test]
    fn untouched_defaults_return_the_servers_exact_bytes() {
        let body = b"+ASK:\r\n Ask: Name\t\xe9t\xe9\r\n Choose: Pick\t\xff\tb\r\n".to_vec();
        let form = Form::parse(&item(), &body).unwrap();
        for encoding in [Encoding::Auto, Encoding::Utf8, Encoding::Cp437] {
            assert_eq!(
                form.submission(encoding).unwrap().url.request().unwrap(),
                b"/form\t+\t1\r\n+-1\r\n\xe9t\xe9\r\n\xff\r\n.\r\n"
            );
        }
    }

    #[test]
    fn a_file_is_the_remainder_of_an_exact_length_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload.bin");
        std::fs::write(&path, b"line\r\n.\r\nbinary\0").unwrap();
        let body = b"+ASK:\r\n Ask: Title\r\n ChooseF: Upload\r\n ChooseF: Ignored\r\n AskF: Save reply as\treply.txt\r\n";
        let mut form = Form::parse(&item(), body).unwrap();
        assert!(form.accepts_file(1));
        assert!(!form.accepts_file(2));
        assert!(form.set_file(2, &path).is_err());
        assert_eq!(form.set_file(1, &path).unwrap(), "upload.bin");
        form.set_text(0, None, "t").unwrap();
        let submission = form.submission(Encoding::Auto).unwrap();
        assert_eq!(submission.save_as.as_deref(), Some("reply.txt"));
        assert_eq!(
            submission.url.request().unwrap(),
            b"/form\t+\t1\r\n+30\r\nt\r\nreply.txt\r\nline\r\n.\r\nbinary\0"
        );
        // A changed file is not sent silently (File API snapshot state).
        std::fs::write(&path, b"changed contents").unwrap();
        assert!(form.submission(Encoding::Auto).is_err());
        form.apply(Action::ClearFile(1)).unwrap();
        assert!(form.submission(Encoding::Auto).is_ok());
    }

    #[test]
    fn search_items_keep_their_query_field_and_actions_roundtrip() {
        let url = GopherUrl::new("e.test".into(), 70, '7', b"/s".to_vec()).with_plus(b"?");
        let form = Form::parse(&url, b"+ASK:\r\n Select: Exact:0\r\n").unwrap();
        assert_eq!(
            form.submission(Encoding::Auto)
                .unwrap()
                .url
                .request()
                .unwrap(),
            b"/s\t\t+\t1\r\n+-1\r\n0\r\n.\r\n"
        );
        for action in [
            Action::Edit(3),
            Action::EditLine(1, 2),
            Action::AddLine(0),
            Action::Choose(4, 98),
            Action::Toggle(1),
            Action::File(2),
            Action::ClearFile(2),
            Action::Submit,
            Action::Reset,
        ] {
            assert_eq!(Action::from_link(&action.link()), Some(action));
        }
        for bad in [
            "about:gopher-form/edit",
            "about:gopher-form/edit/x",
            "about:gopher-form/edit/1/2",
            "about:gopher-form/submit/now",
            "about:gopher-forms/submit",
        ] {
            assert_eq!(
                Action::from_link(&Link::External(bad.into())),
                None,
                "{bad}"
            );
        }
        assert!(Form::parse(&url, b"+INFO: 7S\t/s\te.test\t70\t?\r\n").is_none());
    }
}
