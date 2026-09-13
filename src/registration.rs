//! Shared, source-attributed presentation for WHOIS and RDAP records.
//!
//! This is a view of the received data, never a replacement for the original
//! replies. WHOIS has no universal response schema (RFC 3912); RDAP object
//! boundaries and roles are defined by RFC 9083 §§4–5. Parsers retain those
//! boundaries rather than treating every occurrence of a field as equivalent.

use crate::doc::{DocLine, Kind, Link};
use crate::text_reply;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Section {
    #[default]
    Summary,
    Contacts,
    Details,
    Raw,
}

impl Section {
    pub fn action(self) -> Link {
        Link::External(format!(
            "about:registration/{}",
            match self {
                Self::Summary => "summary",
                Self::Contacts => "contacts",
                Self::Details => "details",
                Self::Raw => "raw",
            }
        ))
    }

    pub fn from_link(link: &Link) -> Option<Self> {
        let Link::External(value) = link else {
            return None;
        };
        Some(match value.as_str() {
            "about:registration/summary" => Self::Summary,
            "about:registration/contacts" => Self::Contacts,
            "about:registration/details" => Self::Details,
            "about:registration/raw" => Self::Raw,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub section: Section,
    pub label: String,
    pub value: String,
    pub sources: Vec<String>,
    pub link: Option<Link>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub title: String,
    pub fields: Vec<Field>,
    /// Substantive notices remain visible on the summary, including truncation
    /// and redaction. Legal text and diagnostics belong in Record details.
    pub notices: Vec<String>,
    pub clipped: bool,
}

pub fn clean(text: &str) -> String {
    text_reply::display_text(text.as_bytes(), false)
        .0
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn redacted(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "redacted" | "redacted for privacy" | "data protected" | "not disclosed" | "not disclosing"
    )
}

impl Record {
    pub fn add(
        &mut self,
        section: Section,
        label: &str,
        value: &str,
        source: &str,
        link: Option<Link>,
    ) {
        let value = clean(value);
        if value.is_empty() {
            return;
        }
        if redacted(&value) {
            self.notice("Some contact information is redacted by the server.");
            return;
        }
        let source = clean(source);
        if let Some(field) = self.fields.iter_mut().find(|field| {
            field.section == section
                && field.label == label
                && field.value.eq_ignore_ascii_case(&value)
        }) {
            if !source.is_empty() && !field.sources.contains(&source) {
                field.sources.push(source);
            }
            return;
        }
        // Reserve independent budgets so an early routing policy cannot crowd
        // out identity or contacts that occur near the end of a WHOIS reply.
        let limit = match section {
            Section::Summary => 128,
            Section::Contacts => 256,
            _ => 384,
        };
        if self
            .fields
            .iter()
            .filter(|field| field.section == section)
            .count()
            >= limit
        {
            self.clipped = true;
            return;
        }
        self.fields.push(Field {
            section,
            label: clean(label),
            value,
            sources: if source.is_empty() {
                Vec::new()
            } else {
                vec![source]
            },
            link,
        });
    }

    pub fn notice(&mut self, message: &str) {
        let message = clean(message);
        if !message.is_empty() && !self.notices.contains(&message) && self.notices.len() < 32 {
            self.notices.push(message);
        }
    }

    /// Stable field order also keeps partially received records readable.
    fn ordered(&self, section: Section) -> Vec<&Field> {
        let mut fields: Vec<_> = self
            .fields
            .iter()
            .filter(|f| section == Section::Details || f.section == section)
            .collect();
        fields.sort_by_key(|f| match f.label.as_str() {
            "Registrant" | "Organization" => 0,
            "Country" => 1,
            "Registrar" => 2,
            "Network" | "AS name" | "Range" => 3,
            "Registered" => 4,
            "Registry expiry" | "Registrar expiry" | "Expiry" => 5,
            "Updated" => 6,
            "Status" => 7,
            "DNSSEC" | "Zone signed" => 8,
            "Nameserver" => 9,
            _ => 10,
        });
        fields
    }

    pub fn comparison(&self) -> String {
        let mut fields: Vec<_> = self
            .fields
            .iter()
            .filter(|f| f.section != Section::Details)
            .map(|f| format!("{}: {}", f.label, f.value))
            .collect();
        fields.sort();
        fields.dedup();
        fields.join("\n")
    }

    pub fn render(&self, section: Section, width: usize, lines: &mut Vec<DocLine>) {
        self.render_with_columns(section, width, width >= 52, lines);
    }

    pub fn render_with_columns(
        &self,
        section: Section,
        width: usize,
        columns: bool,
        lines: &mut Vec<DocLine>,
    ) {
        let fields = self.ordered(section);
        let mut abbreviated = section == Section::Summary && fields.len() > 48;
        let label_width = if section == Section::Summary {
            16
        } else {
            fields
                .iter()
                .map(|f| f.label.chars().count())
                .max()
                .unwrap_or(0)
                .min(24)
        };
        if fields.is_empty() {
            line(
                lines,
                Kind::Info,
                match section {
                    Section::Contacts => "No public contact details were returned.",
                    _ => "No structured record fields were returned; full reply available below.",
                },
                None,
                width,
            );
        }
        let mut displayed = std::collections::HashSet::new();
        for field in fields.into_iter().take(if section == Section::Summary {
            48
        } else {
            usize::MAX
        }) {
            let value = if section == Section::Summary
                && matches!(
                    field.label.as_str(),
                    "Registered" | "Updated" | "Expiry" | "Registry expiry" | "Registrar expiry"
                ) {
                // Preserve the full timestamp and its source in Details. Do not
                // parse or invent a timezone for WHOIS's free-form dates.
                field
                    .value
                    .get(..10)
                    .filter(|v| {
                        v.as_bytes().get(4) == Some(&b'-') && v.as_bytes().get(7) == Some(&b'-')
                    })
                    .unwrap_or(&field.value)
            } else {
                &field.value
            };
            if section != Section::Details && !displayed.insert((field.label.as_str(), value)) {
                continue;
            }
            let value = if section == Section::Summary && field.label == "Status" {
                status_label(value)
            } else {
                value
            };
            let short;
            let value = if section == Section::Summary && value.chars().count() > 160 {
                use unicode_segmentation::UnicodeSegmentation;
                short = format!("{}…", value.graphemes(true).take(160).collect::<String>());
                abbreviated = true;
                &short
            } else {
                value
            };
            line(
                lines,
                if field.link.is_some() {
                    Kind::OtherLink
                } else {
                    Kind::Text
                },
                &if columns {
                    format!("{:<label_width$}  {value}", field.label)
                } else {
                    format!("{}: {value}", field.label)
                },
                field.link.clone(),
                width,
            );
            if section == Section::Details && !field.sources.is_empty() {
                line(
                    lines,
                    Kind::Quote,
                    &format!("  Source: {}", field.sources.join(", ")),
                    None,
                    width,
                );
            }
        }
        if section == Section::Summary {
            let mut conflicts = Vec::new();
            for label in [
                "Registrant",
                "Organization",
                "Registrar",
                "Registered",
                "Updated",
                "Registry expiry",
                "Registrar expiry",
                "DNSSEC",
            ] {
                if self
                    .fields
                    .iter()
                    .filter(|f| f.section == Section::Summary && f.label == label)
                    .count()
                    > 1
                {
                    conflicts.push(label);
                }
            }
            if !conflicts.is_empty() {
                line(
                    lines,
                    Kind::Info,
                    &format!(
                        "Sources differ for {}; see Record details.",
                        conflicts.join(", ")
                    ),
                    None,
                    width,
                );
            }
        }
        for notice in &self.notices {
            let short;
            let notice = if section == Section::Summary && notice.chars().count() > 240 {
                short = format!(
                    "{}… (see Record details)",
                    notice.chars().take(240).collect::<String>()
                );
                &short
            } else {
                notice
            };
            line(lines, Kind::Info, notice, None, width);
        }
        if abbreviated {
            line(
                lines,
                Kind::Info,
                "More registration details are available in Record details.",
                None,
                width,
            );
        }
        if self.clipped {
            line(
                lines,
                Kind::Info,
                "More fields are available in the full reply.",
                None,
                width,
            );
        }
    }
}

pub fn navigation(lines: &mut Vec<DocLine>, section: Section, rdap: bool, width: usize) {
    if lines.len() > text_reply::MAX_ROWS - 128 {
        lines.truncate(text_reply::MAX_ROWS - 128);
        line(
            lines,
            Kind::Info,
            "Display truncated; complete received data remains available to save.",
            None,
            width,
        );
    }
    line(lines, Kind::Text, "", None, width);
    for (next, label) in [
        (Section::Summary, "Record summary"),
        (Section::Contacts, "Contacts"),
        (Section::Details, "Record details"),
        (
            Section::Raw,
            if rdap {
                "Original JSON"
            } else {
                "Full server replies"
            },
        ),
    ] {
        if next != section {
            line(lines, Kind::OtherLink, label, Some(next.action()), width);
        }
    }
}

pub fn line(lines: &mut Vec<DocLine>, kind: Kind, text: &str, link: Option<Link>, width: usize) {
    if lines.len() < text_reply::MAX_ROWS - 2 {
        if kind == Kind::Pre || width >= text_reply::MAX_COLUMNS + 2 {
            text_reply::push_line(lines, kind, text.to_string(), link, width);
        } else {
            // Prose and fields wrap at words. Keep the greedy algorithm and
            // bounded input/output; raw protocol columns use push_line above.
            let options = textwrap::Options::new(width.max(2))
                .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit);
            let mut link = link;
            for piece in textwrap::wrap(text, options) {
                if lines.len() >= text_reply::MAX_ROWS - 2 {
                    break;
                }
                lines.push(DocLine {
                    kind,
                    text: piece.into_owned(),
                    link: link.take(),
                });
            }
        }
        lines.truncate(text_reply::MAX_ROWS - 2);
    }
}

/// RFC 5731 §2.3 / RFC 9083 §10.2.2. Keep other statuses verbatim.
pub fn status_label(status: &str) -> &str {
    match status {
        "clientTransferProhibited" | "client transfer prohibited" => {
            "Transfer prohibited by registrar"
        }
        "serverTransferProhibited" | "server transfer prohibited" => {
            "Transfer prohibited by registry"
        }
        "clientHold" | "client hold" => "DNS publication suspended by registrar",
        "serverHold" | "server hold" => "DNS publication suspended by registry",
        _ => status,
    }
}

/// Preserve the content at the top of a scrolled text view when a field is
/// inserted above it. Prefer link identity; plain rows use their exact text.
pub fn anchor(old: &[DocLine], new: &[DocLine], row: usize) -> Option<usize> {
    let line = old.get(row)?;
    if line.text.is_empty() && line.link.is_none() {
        return None;
    }
    new.iter()
        .enumerate()
        .filter(|(_, candidate)| {
            if let Some(link) = &line.link {
                candidate.link.as_ref() == Some(link)
            } else {
                candidate.text == line.text
            }
        })
        .min_by_key(|(index, _)| index.abs_diff(row))
        .map(|(index, _)| index)
}
