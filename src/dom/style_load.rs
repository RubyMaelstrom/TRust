//! The arena side of a style sheet's load processing (local whatwg/html
//! e5071a20, 2026-09-04): a `style` element runs "update a style block"
//! when the parser pops it, and when, outside the parser, it becomes
//! connected or disconnected or its children change (#the-style-element);
//! once the attempts to obtain its style sheet's critical subresources (the
//! sheets its `@import` rules import, transitively: #critical-subresources)
//! are complete, it fires `load` (or `error` when one failed), and it delays
//! its document's load event until then. A linked style sheet's critical
//! subresources likewise delay the load event (#link-type-stylesheet).
//!
//! The arena records each style block update (the sheet is removed and, for
//! a connected element, created again) while a page Realm takes the record,
//! and answers whether a sheet's critical subresources are complete from the
//! `@import` rules of its source and the imported sheets fetched so far
//! (CSS Cascade 5 #fetch-an-import). The platform prelude runs the steps.

use super::*;

#[derive(Default)]
pub(crate) struct State {
    /// Whether style block updates are recorded (a page Realm takes them).
    recording: bool,
    /// `style` elements whose style block was updated, in order, each once.
    updates: Vec<NodeId>,
}

impl State {
    pub(super) fn retained_bytes(&self) -> usize {
        self.updates.capacity() * std::mem::size_of::<NodeId>()
    }

    /// A collected element has no load event to fire: the record holds its
    /// elements weakly.
    pub(super) fn retain(&mut self, keep: impl Fn(NodeId) -> bool) {
        self.updates.retain(|&id| keep(id));
    }
}

/// Whether obtaining a style sheet's critical subresources is complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CriticalSubresources {
    /// Some are still being obtained.
    Pending,
    /// All attempts are complete and one failed.
    Failed,
    /// All attempts succeeded (or there are none).
    Complete,
}

impl CriticalSubresources {
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Failed => "failed",
            Self::Complete => "done",
        }
    }
}

/// How far obtaining one `@import` URL got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImportState {
    Obtained,
    /// Requested, and its fetch is queued or in flight.
    Pending,
    /// Requested, and its fetch completed without a style sheet.
    Failed,
    /// Never requested.
    Unrequested,
}

impl Dom {
    /// The style block of `id` was updated.
    pub(super) fn note_style_block_update(&mut self, id: NodeId) {
        if self.style_load.recording
            && self.tag_name(id) == Some("style")
            && !self.style_load.updates.contains(&id)
        {
            self.style_load.updates.push(id);
        }
    }

    /// The parser popped `style` element `id` off the stack of open elements.
    pub(crate) fn note_parser_style_block(&mut self, id: NodeId) {
        self.note_style_block_update(id);
    }

    /// Start or stop recording style block updates; stopping forgets them.
    pub(crate) fn set_style_load_recording(&mut self, recording: bool) {
        self.style_load.recording = recording;
        if !recording {
            self.style_load.updates = Vec::new();
        }
    }

    pub(crate) fn has_style_block_updates(&self) -> bool {
        !self.style_load.updates.is_empty()
    }

    pub(crate) fn take_style_block_updates(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.style_load.updates)
    }

    /// Return updates another Window Realm of this arena takes, ahead of any
    /// recorded since they were taken.
    pub(crate) fn restore_style_block_updates(&mut self, mut updates: Vec<NodeId>) {
        updates.append(&mut self.style_load.updates);
        self.style_load.updates = updates;
    }

    /// How far obtaining the sheet an `@import` of absolute URL `url`
    /// imports got; `in_flight` reports the fetches the host has started.
    pub(crate) fn import_state(&self, url: &str, in_flight: &dyn Fn(&str) -> bool) -> ImportState {
        if self.imported_sheets.contains_key(url) {
            return ImportState::Obtained;
        }
        let requests = self.import_requests.borrow();
        if in_flight(url) || requests.0.iter().any(|(_, queued)| queued == url) {
            ImportState::Pending
        } else if requests.1.contains(url) {
            ImportState::Failed
        } else {
            ImportState::Unrequested
        }
    }

    /// HTML #critical-subresources of the style sheet `owner` (a `style`
    /// element, or a `link` whose sheet arrived) has from its source: the
    /// sheets its applicable `@import` rules import, transitively (a cycle
    /// imports nothing). An import nothing requested yet is requested for
    /// `owner`.
    pub(crate) fn critical_subresources(
        &self,
        owner: NodeId,
        in_flight: &dyn Fn(&str) -> bool,
    ) -> CriticalSubresources {
        if !self.is_valid(owner) || !self.is_dom_connected(owner) {
            return CriticalSubresources::Complete;
        }
        let (text, base) = match self.tag_name(owner) {
            Some("style") => (
                self.text_content(owner),
                self.doc_url
                    .as_ref()
                    .map(|page| self.resource_base_url(owner, page)),
            ),
            Some("link") => match self.external_sheets.get(&owner) {
                Some(sheet) => (sheet.text.clone(), url::Url::parse(&sheet.base).ok()),
                None => return CriticalSubresources::Complete,
            },
            _ => return CriticalSubresources::Complete,
        };
        let mut seen = Vec::new();
        let mut failed = false;
        if self.pending_imports_of(
            owner,
            &text,
            base.as_ref(),
            in_flight,
            &mut seen,
            &mut failed,
        ) {
            CriticalSubresources::Pending
        } else if failed {
            CriticalSubresources::Failed
        } else {
            CriticalSubresources::Complete
        }
    }

    fn pending_imports_of(
        &self,
        owner: NodeId,
        css: &str,
        base: Option<&url::Url>,
        in_flight: &dyn Fn(&str) -> bool,
        seen: &mut Vec<String>,
        failed: &mut bool,
    ) -> bool {
        if !cssom::contains_import(css) {
            return false;
        }
        for import in crate::http::stylesheet_imports(css) {
            // CSS Cascade 5 #fetch-an-import: an unparsable URL or a false
            // supports() condition fetches nothing.
            let url = match base {
                Some(base) => base.join(&import.url),
                None => url::Url::parse(&import.url),
            };
            let Ok(url) = url else {
                continue;
            };
            if !crate::http::import_supports_holds(&import.condition) {
                continue;
            }
            let url = url.to_string();
            if seen.contains(&url) {
                continue;
            }
            seen.push(url.clone());
            match self.import_state(&url, in_flight) {
                ImportState::Obtained => {
                    let Some(sheet) = self.imported_sheets.get(&url) else {
                        continue;
                    };
                    let base = url::Url::parse(&sheet.base).ok();
                    if self.pending_imports_of(
                        owner,
                        &sheet.text,
                        base.as_ref(),
                        in_flight,
                        seen,
                        failed,
                    ) {
                        return true;
                    }
                }
                ImportState::Pending => return true,
                ImportState::Failed => *failed = true,
                ImportState::Unrequested => {
                    self.request_import(owner, &url);
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_block_updates_are_recorded_once_while_asked_for() {
        let mut dom = Dom::parse_document("<!doctype html><head id=head></head><body></body>");
        let head = dom.get_by_id("head").unwrap();
        let quiet = dom.create_element("style");
        dom.append(head, quiet);
        assert!(!dom.has_style_block_updates());
        dom.set_style_load_recording(true);
        let style = dom.create_element("style");
        let text = dom.create_text("p{color:red}");
        dom.append(style, text);
        dom.append(head, style);
        let replaced = dom.create_text("p{color:blue}");
        dom.replace_all_children(style, vec![replaced]);
        assert_eq!(dom.take_style_block_updates(), vec![style]);
        dom.detach(style);
        assert_eq!(dom.take_style_block_updates(), vec![style]);
    }

    #[test]
    fn critical_subresources_follow_the_import_graph() {
        let mut dom = Dom::parse_document(
            "<!doctype html><style id=s>@import url(a.css); @import url(b.css) supports(nonsense: 1);\
             p{color:red}</style><style id=plain>p{}</style>",
        );
        dom.set_doc_url(Some(url::Url::parse("https://example.com/page").unwrap()));
        let style = dom.get_by_id("s").unwrap();
        let plain = dom.get_by_id("plain").unwrap();
        let idle = |_: &str| false;
        assert_eq!(
            dom.critical_subresources(plain, &idle),
            CriticalSubresources::Complete
        );
        // The unrequested import is requested for its owner.
        assert_eq!(
            dom.critical_subresources(style, &idle),
            CriticalSubresources::Pending
        );
        assert_eq!(
            dom.take_import_requests(),
            vec![(style, String::from("https://example.com/a.css"))]
        );
        let in_flight = |url: &str| url == "https://example.com/a.css";
        assert_eq!(
            dom.critical_subresources(style, &in_flight),
            CriticalSubresources::Pending
        );
        // Requested and no longer in flight without a sheet: failed.
        assert_eq!(
            dom.critical_subresources(style, &idle),
            CriticalSubresources::Failed
        );
        // An obtained sheet's own imports are critical too.
        dom.attach_imported_sheets(vec![crate::http::PageSheet {
            link_href: None,
            url: String::from("https://example.com/a.css"),
            base: String::from("https://example.com/a.css"),
            text: String::from("@import url(c.css);"),
            cors_same_origin: true,
        }]);
        assert_eq!(
            dom.critical_subresources(style, &idle),
            CriticalSubresources::Pending
        );
        dom.attach_imported_sheets(vec![crate::http::PageSheet {
            link_href: None,
            url: String::from("https://example.com/c.css"),
            base: String::from("https://example.com/c.css"),
            text: String::from("@import url(a.css); p{}"),
            cors_same_origin: true,
        }]);
        assert_eq!(
            dom.critical_subresources(style, &idle),
            CriticalSubresources::Complete
        );
    }
}
