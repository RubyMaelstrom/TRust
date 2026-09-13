use super::{Operation, Reply, Target, decode, encode};
use crate::{
    doc::{Doc, DocLine, Kind, Link},
    text_reply,
};
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Section {
    #[default]
    Definition,
    Sources,
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Section(Section),
    Select(usize),
    Remember(String),
    Lookup,
    LookupIn(String),
    LookupMode(String),
    Filter,
    Save,
}

impl Action {
    pub fn link(&self) -> Link {
        let action = match self {
            Self::Section(Section::Definition) => "read".into(),
            Self::Section(Section::Sources) => "sources".into(),
            Self::Section(Section::Raw) => "raw".into(),
            Self::Select(n) => format!("select/{n}"),
            Self::Remember(db) => format!("remember/{}", encode(db)),
            Self::LookupIn(db) => format!("lookup/{}", encode(db)),
            Self::LookupMode(strategy) => format!("mode/{}", encode(strategy)),
            Self::Lookup => "lookup".into(),
            Self::Filter => "filter".into(),
            Self::Save => "save".into(),
        };
        Link::External(format!("about:dict/{action}"))
    }
    pub fn from_link(link: &Link) -> Option<Self> {
        let Link::External(value) = link else {
            return None;
        };
        let value = value.strip_prefix("about:dict/")?;
        Some(match value {
            "read" => Self::Section(Section::Definition),
            "sources" => Self::Section(Section::Sources),
            "raw" => Self::Section(Section::Raw),
            "lookup" => Self::Lookup,
            "filter" => Self::Filter,
            "save" => Self::Save,
            _ => {
                let (name, value) = value.split_once('/')?;
                match name {
                    "select" => Self::Select(value.parse().ok()?),
                    "remember" => Self::Remember(decode(value).ok()?),
                    "lookup" => Self::LookupIn(decode(value).ok()?),
                    "mode" => Self::LookupMode(decode(value).ok()?),
                    _ => return None,
                }
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub target: Target,
    pub reply: Reply,
    pub section: Section,
    pub selected: usize,
    pub filter: String,
    pub view: text_reply::View,
}

/// History retains presentation choices without retaining reply buffers.
#[derive(Clone, Debug)]
pub struct SavedView {
    section: Section,
    selected: usize,
    filter: String,
    wrap: bool,
}

impl Page {
    pub fn saved_view(&self) -> SavedView {
        SavedView {
            section: self.section,
            selected: self.selected,
            filter: self.filter.clone(),
            wrap: self.view.wrap,
        }
    }
    pub fn restore_view(&mut self, view: &SavedView) {
        self.section = view.section;
        self.selected = view.selected;
        self.filter = view.filter.clone();
        self.view.wrap = view.wrap;
    }
    pub fn new(target: Target, reply: Reply) -> Self {
        let selected = target.number.unwrap_or(1).saturating_sub(1);
        let view = text_reply::View {
            wrap: true,
            loading: !reply.finished,
            ..Default::default()
        };
        Self {
            target,
            reply,
            section: Section::Definition,
            selected,
            filter: String::new(),
            view,
        }
    }
    pub fn refreshed(reply: Reply, old: &Self) -> Self {
        let mut page = old.clone();
        page.update(reply);
        page
    }
    pub fn update(&mut self, reply: Reply) {
        self.view.loading = !reply.finished;
        self.reply = reply;
    }
    pub fn stop(&mut self, reason: &str) {
        self.reply.finished = true;
        self.reply.complete = false;
        self.reply.notice = Some(reason.into());
        self.view.loading = false;
    }
    pub fn status(&self) -> String {
        let state = if !self.reply.finished {
            "receiving …"
        } else if !self.reply.complete {
            "incomplete"
        } else if self.reply.notice.is_some() {
            "request failed"
        } else {
            "complete"
        };
        let count = match self.target.operation {
            Operation::Define => format!("{} definitions", self.reply.definitions.len()),
            Operation::Match => format!("{} matches", self.reply.entries.len()),
            Operation::Databases => format!("{} dictionaries", self.reply.entries.len()),
            Operation::Strategies => format!("{} search modes", self.reply.entries.len()),
            Operation::Info => "dictionary information".into(),
        };
        format!("{} · {count} · {state}", self.target.server())
    }
    pub fn view_action(
        &mut self,
        action: &str,
        enabled: Option<bool>,
    ) -> Result<&'static str, &'static str> {
        if action != "wrap" {
            return Err(
                "DICT has definition and source navigation; comparisons apply to Finger and WHOIS.",
            );
        }
        text_reply::view_action(&mut self.view, action, enabled, false)
    }
    pub fn apply(&mut self, action: &Action) -> Result<(), String> {
        match action {
            Action::Section(section) => self.section = *section,
            Action::Select(n) if *n < self.reply.definitions.len() => {
                self.selected = *n;
                self.section = Section::Definition;
            }
            Action::Remember(db)
                if db == "*"
                    || self.reply.definitions.iter().any(|d| &d.database == db)
                    || (self.target.operation == Operation::Databases
                        && self.reply.entries.iter().any(|e| &e.name == db)) =>
            {
                super::remember(&self.target, db)?;
                self.view.notice = Some(format!(
                    "Future dict commands on {} use {db}.",
                    self.target.server()
                ));
            }
            _ => return Err("This dictionary action is unavailable.".into()),
        }
        self.view.horizontal = 0;
        Ok(())
    }
    pub fn command_for(&self, action: &Action) -> Option<String> {
        match action {
            Action::Lookup => {
                let mut target = self.target.clone();
                if matches!(target.database.as_str(), "*" | "!")
                    && let Some(database) = super::preferred(&target)
                {
                    target.database = database;
                }
                Some(super::command_seed(&target))
            }
            Action::LookupIn(db)
                if self.target.operation == Operation::Databases
                    && (db == "*" || self.reply.entries.iter().any(|e| &e.name == db)) =>
            {
                Some(super::command_seed(&self.target.lookup("", db)))
            }
            Action::Filter => Some("dict-filter ".into()),
            Action::LookupMode(strategy)
                if self.target.operation == Operation::Strategies
                    && self.reply.entries.iter().any(|e| &e.name == strategy) =>
            {
                Some(format!(
                    "{}--match {} ",
                    super::command_seed(&self.target),
                    super::quote(strategy)
                ))
            }
            _ => None,
        }
    }
    pub fn set_filter(&mut self, input: &str) -> Result<(), String> {
        if input.len() > 256 || input.chars().any(char::is_control) {
            return Err("Dictionary filter is limited to 256 bytes of text.".into());
        }
        self.filter = input.trim().to_string();
        if self.target.operation == Operation::Define && !self.reply.definitions.is_empty() {
            self.section = Section::Sources;
        }
        Ok(())
    }
    pub fn export(&self) -> crate::download::DownloadOffer {
        let name: String = self
            .target
            .word
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
            .take(64)
            .collect();
        crate::download::DownloadOffer::from_bytes(
            url::Url::parse(&self.target.to_string()).expect("validated DICT address"),
            format!("dict-{}.txt", if name.is_empty() { "reply" } else { &name }),
            self.reply.raw.as_ref().clone(),
        )
    }
}

/// Repeated lines retain their hanging indent when the viewport is narrower
/// than the server's formatting. The desktop uses the same prefix in pixels.
pub fn hanging_indent(line: &str) -> usize {
    let leading = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[leading..];
    let marker = rest
        .find([':', '.'])
        .filter(|&n| {
            n < 6
                && rest[..n]
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == ' ' || "nvar".contains(c))
                && rest.get(n + 1..).is_some_and(|s| s.starts_with(' '))
        })
        .map_or(0, |n| n + 2);
    (leading + marker).min(24)
}

fn label(text: &str) -> String {
    let (clean, _) = text_reply::display_text(text.as_bytes(), false);
    let clean: String = clean
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect();
    clean
}
fn line(out: &mut Vec<DocLine>, kind: Kind, text: &str, link: Option<Link>, width: usize) {
    crate::registration::line(out, kind, &label(text), link, width);
}
fn button(out: &mut Vec<DocLine>, text: &str, action: Action, width: usize) {
    line(out, Kind::OtherLink, text, Some(action.link()), width);
}
fn target_link(out: &mut Vec<DocLine>, text: &str, target: Target, width: usize) {
    line(out, Kind::OtherLink, text, Some(Link::Dict(target)), width);
}

fn body(out: &mut Vec<DocLine>, text: &[u8], page: &Page, width: usize) {
    let (clean, mut clipped) = text_reply::display_text(text, !page.reply.finished);
    let available = text_reply::MAX_ROWS.saturating_sub(out.len() + 32);
    let mut rows = Vec::new();
    for physical in clean.lines() {
        if rows.len() >= available {
            clipped = true;
            break;
        }
        if !page.view.wrap || width > text_reply::MAX_COLUMNS {
            rows.push(DocLine {
                kind: Kind::Pre,
                text: physical.into(),
                link: None,
            });
        } else {
            let indent = " ".repeat(hanging_indent(physical).min(width.saturating_sub(8)));
            let options = textwrap::Options::new(width.max(10))
                .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit)
                .subsequent_indent(&indent);
            for piece in textwrap::wrap(physical, options) {
                if rows.len() >= available {
                    clipped = true;
                    break;
                }
                rows.push(DocLine {
                    kind: Kind::Pre,
                    text: piece.into_owned(),
                    link: None,
                });
            }
        }
    }
    out.extend(rows);
    if clipped {
        line(
            out,
            Kind::Info,
            "Display limited; all received bytes remain available in Save original text.",
            None,
            width,
        );
    }
}

/// Braces are a common dictionary convention, not RFC markup. Keep the body
/// verbatim and expose conservative, deduplicated references separately.
fn references(text: &str, target: &Target, db: &str) -> Vec<(String, Link)> {
    let (text, _) = text_reply::display_text(text.as_bytes(), false);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut word = None::<String>;
    for c in text.chars() {
        if c == '{' {
            word = Some(String::new());
        } else if c == '}' {
            if let Some(value) = word.take() {
                let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
                if !value.is_empty() && seen.insert(value.to_lowercase()) {
                    out.push((value.clone(), Link::Dict(target.lookup(&value, db))));
                    if out.len() == 64 {
                        break;
                    }
                }
            }
        } else if let Some(value) = &mut word {
            if value.len() > 160
                || !(c.is_alphanumeric() || c.is_whitespace() || "-'’+._/".contains(c))
            {
                word = None;
            } else {
                value.push(c);
            }
        }
    }
    for link in text_reply::links(&text).into_iter().take(64) {
        let name = link.to_string();
        if seen.insert(name.clone()) {
            out.push((name, link));
        }
    }
    out
}

pub fn render(page: Page, width: usize) -> Doc {
    let width = width.max(10);
    let mut lines = Vec::new();
    let title = if page.target.word.is_empty() {
        "Dictionary browser"
    } else {
        &page.target.word
    };
    line(
        &mut lines,
        Kind::Heading(1),
        &format!("DICT · {title}"),
        None,
        width,
    );
    line(&mut lines, Kind::Info, &page.status(), None, width);
    if let Some(notice) = &page.reply.notice {
        line(&mut lines, Kind::Error, notice, None, width);
    }
    if let Some(notice) = &page.view.notice {
        line(&mut lines, Kind::Info, notice, None, width);
    }
    button(&mut lines, "Look up another word", Action::Lookup, width);
    if page.section == Section::Sources {
        button(
            &mut lines,
            "Read definition",
            Action::Section(Section::Definition),
            width,
        );
    } else if !page.reply.definitions.is_empty() {
        button(
            &mut lines,
            "Definitions & sources",
            Action::Section(Section::Sources),
            width,
        );
    }
    target_link(
        &mut lines,
        "Browse dictionaries",
        page.target.catalog(Operation::Databases),
        width,
    );
    target_link(
        &mut lines,
        "Search modes",
        page.target.catalog(Operation::Strategies),
        width,
    );
    button(
        &mut lines,
        if page.section == Section::Raw {
            "Read results"
        } else {
            "Original text"
        },
        Action::Section(if page.section == Section::Raw {
            Section::Definition
        } else {
            Section::Raw
        }),
        width,
    );
    button(&mut lines, "Save original text", Action::Save, width);
    line(&mut lines, Kind::Text, "", None, width);
    if page.section == Section::Raw {
        body(&mut lines, &page.reply.raw, &page, width);
    } else if page.section == Section::Sources {
        button(&mut lines, "Filter sources", Action::Filter, width);
        if !page.filter.is_empty() {
            line(
                &mut lines,
                Kind::Info,
                &format!("Filter: {}", page.filter),
                None,
                width,
            );
        }
        let filter = page.filter.to_lowercase();
        let mut found = false;
        for (index, definition) in page.reply.definitions.iter().enumerate() {
            if !format!(
                "{} {} {}",
                definition.database, definition.description, definition.word
            )
            .to_lowercase()
            .contains(&filter)
            {
                continue;
            }
            let count = page
                .reply
                .definitions
                .iter()
                .filter(|d| d.database == definition.database)
                .count();
            button(
                &mut lines,
                &format!(
                    "{}. {} · {} — {} ({count})",
                    index + 1,
                    definition.word,
                    definition.database,
                    definition.description
                ),
                Action::Select(index),
                width,
            );
            found = true;
        }
        if !found {
            line(
                &mut lines,
                Kind::Info,
                "No sources match this filter.",
                None,
                width,
            );
        }
    } else if matches!(
        page.target.operation,
        Operation::Databases | Operation::Strategies
    ) {
        button(&mut lines, "Filter this list", Action::Filter, width);
        if !page.filter.is_empty() {
            line(
                &mut lines,
                Kind::Info,
                &format!("Filter: {}", page.filter),
                None,
                width,
            );
        }
        if page.target.operation == Operation::Databases {
            if page.target.word.is_empty() {
                button(
                    &mut lines,
                    "Search all dictionaries",
                    Action::LookupIn("*".into()),
                    width,
                );
            } else {
                target_link(
                    &mut lines,
                    "Search all dictionaries",
                    page.target.lookup(&page.target.word, "*"),
                    width,
                );
            }
            button(
                &mut lines,
                "Use all dictionaries by default",
                Action::Remember("*".into()),
                width,
            );
        }
        let filter = page.filter.to_lowercase();
        let mut found = false;
        for entry in &page.reply.entries {
            if !format!("{} {}", entry.name, entry.description)
                .to_lowercase()
                .contains(&filter)
            {
                continue;
            }
            let text = format!("{} — {}", entry.name, entry.description);
            if page.target.operation == Operation::Databases && page.target.word.is_empty() {
                button(
                    &mut lines,
                    &text,
                    Action::LookupIn(entry.name.clone()),
                    width,
                );
            } else if page.target.operation == Operation::Strategies && page.target.word.is_empty()
            {
                button(
                    &mut lines,
                    &text,
                    Action::LookupMode(entry.name.clone()),
                    width,
                );
            } else {
                let mut target = page.target.lookup(&page.target.word, &page.target.database);
                if page.target.operation == Operation::Databases {
                    target.database = entry.name.clone();
                } else {
                    target.operation = Operation::Match;
                    target.strategy = entry.name.clone();
                }
                target_link(&mut lines, &text, target, width);
            }
            found = true;
        }
        if !found && page.reply.finished {
            line(
                &mut lines,
                Kind::Info,
                "No entries match this filter.",
                None,
                width,
            );
        }
    } else if page.target.operation == Operation::Info {
        body(&mut lines, page.reply.information.as_bytes(), &page, width);
    } else if page.target.operation == Operation::Match || page.reply.no_match {
        if page.reply.no_match {
            line(
                &mut lines,
                Kind::Info,
                if page.target.operation == Operation::Define {
                    "No definitions found. Spelling suggestions:"
                } else {
                    "No words matched this search."
                },
                None,
                width,
            );
        }
        let entries: Vec<_> = page
            .reply
            .entries
            .iter()
            .enumerate()
            .filter(|(n, _)| {
                page.target.operation != Operation::Match
                    || page.target.number.is_none_or(|v| *n + 1 == v)
            })
            .collect();
        if entries.is_empty() && !page.reply.entries.is_empty() && page.reply.finished {
            line(
                &mut lines,
                Kind::Info,
                "The requested match number was not returned.",
                None,
                width,
            );
        }
        for (_, entry) in entries {
            target_link(
                &mut lines,
                &format!("{} · {}", entry.description, entry.name),
                page.target.lookup(&entry.description, &entry.name),
                width,
            );
        }
        if page.reply.finished && page.reply.entries.is_empty() {
            line(
                &mut lines,
                Kind::Info,
                "No matching words returned. Try another dictionary or search mode.",
                None,
                width,
            );
        } else if !page.reply.finished {
            line(
                &mut lines,
                Kind::Info,
                "Looking for matching words …",
                None,
                width,
            );
        }
    } else if let Some(definition) = page.reply.definitions.get(page.selected) {
        line(
            &mut lines,
            Kind::Info,
            &format!(
                "Definition {} of {} · {}{}",
                page.selected + 1,
                page.reply.definitions.len(),
                definition.database,
                if definition.complete {
                    ""
                } else {
                    " · receiving …"
                }
            ),
            None,
            width,
        );
        if page.selected > 0 {
            button(
                &mut lines,
                "Previous definition",
                Action::Select(page.selected - 1),
                width,
            );
        }
        if page.selected + 1 < page.reply.definitions.len() {
            button(
                &mut lines,
                "Next definition",
                Action::Select(page.selected + 1),
                width,
            );
        }
        line(
            &mut lines,
            Kind::Heading(2),
            &definition.description,
            None,
            width,
        );
        body(&mut lines, definition.body.as_bytes(), &page, width);
        if !definition.complete && page.reply.finished {
            line(
                &mut lines,
                Kind::Error,
                "This definition is incomplete.",
                None,
                width,
            );
        }
        let refs = references(&definition.body, &page.target, &definition.database);
        if !refs.is_empty() {
            line(&mut lines, Kind::Heading(2), "References", None, width);
            for (name, link) in refs {
                line(&mut lines, Kind::OtherLink, &name, Some(link), width);
            }
        }
        button(
            &mut lines,
            &format!("Use {} by default", definition.database),
            Action::Remember(definition.database.clone()),
            width,
        );
        let mut info = page.target.catalog(Operation::Info);
        info.database = definition.database.clone();
        target_link(&mut lines, "About this dictionary", info, width);
        if page.target.database != "*" {
            target_link(
                &mut lines,
                "Search all dictionaries",
                page.target.lookup(&page.target.word, "*"),
                width,
            );
        }
    } else {
        line(
            &mut lines,
            Kind::Info,
            if page.reply.finished {
                "The requested definition was not returned."
            } else {
                "Looking up definitions …"
            },
            None,
            width,
        );
    }
    let mut page = page;
    page.view.horizontal = page.view.horizontal.min(
        lines
            .iter()
            .map(|l| UnicodeWidthStr::width(l.text.as_str()))
            .max()
            .unwrap_or(0)
            .saturating_sub(width),
    );
    let mut doc = Doc::from_lines(
        Link::Dict(page.target.clone()),
        lines,
        page.reply.raw.as_ref().clone(),
        width,
        false,
        None,
    );
    doc.dict = Some(page);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dict_references_join_phrases_and_keep_source_context() {
        let target = Target::parse("dict://dict.org/d:neon:wn").unwrap();
        let refs = references(
            "{atomic number\n   10}, {argon} {argon} {x=y} https://example.test/",
            &target,
            "wn",
        );
        assert_eq!(refs.len(), 3);
        let Link::Dict(word) = &refs[0].1 else {
            panic!()
        };
        assert_eq!((&*word.word, &*word.database), ("atomic number 10", "wn"));
    }
    #[test]
    fn dict_hanging_wrap_and_raw_export_preserve_dictionary_content() {
        let target = Target::parse("dict://dict.org/d:neon").unwrap();
        let raw = b"original\r\n\x1b[31m".to_vec();
        let mut reply = Reply {
            raw: std::sync::Arc::new(raw.clone()),
            finished: true,
            complete: true,
            ..Default::default()
        };
        reply.definitions.push(super::super::Definition {
            word: "neon".into(),
            database: "wn".into(),
            description: "WordNet".into(),
            body: std::sync::Arc::new(
                "    n 1: a colorless element in the atmosphere\n\x1b[31mred\x1b[0m\0\n".into(),
            ),
            complete: true,
        });
        let page = Page::new(target, reply);
        assert_eq!(page.export().body, raw);
        let doc = render(page, 26);
        let body: Vec<_> = doc.lines.iter().filter(|l| l.kind == Kind::Pre).collect();
        assert!(body[1].text.starts_with("         "));
        assert!(!doc.lines.iter().any(|l| l.text.contains(['\x1b', '\0'])));
    }
}
