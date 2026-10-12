//! Browser-owned media element state kept beside the tree.
//!
//! Inline presentation: a media element whose player shows frames carries
//! a [`MediaPresentation`] — the frame surface layout paints, the video's
//! natural dimensions (HTML #the-video-element: those of the video when
//! available, else the poster's) and whether the poster is shown (HTML's
//! "show poster flag").
//!
//! External playback (the terminal): HTML #the-video-element permits an
//! external playback link; the dedicated media source failure steps still
//! fire the author's error event. An error handler may replace the entire
//! player with an error message. Keep an offered resource on its surviving
//! container, without retaining detached DOM nodes or changing author markup,
//! layout, or the media error state.
//! Local WHATWG HTML snapshot e5071a20c8569d8a3ec02ed27dd01b948773f850.

use super::{Dom, NodeId};
use std::collections::HashMap;
use url::Url;

#[derive(Default)]
pub(super) struct MediaFallbacks {
    entries: Vec<Entry>,
    presentations: HashMap<NodeId, MediaPresentation>,
}

/// What an inline media element shows (see the module documentation).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MediaPresentation {
    /// The player's frame surface (`crate::media::player::surface`).
    pub surface: u64,
    /// The video's display size, once metadata or a frame established it.
    pub natural: Option<(u32, u32)>,
    pub show_poster: bool,
    /// The user agent's controls (HTML #expose-a-user-interface-to-the-user),
    /// when the element exposes them.
    pub controls: Option<MediaControlsState>,
}

/// What the user agent's media controls show.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MediaControlsState {
    pub paused: bool,
    pub muted: bool,
    /// The official playback position and duration, in seconds (the
    /// duration NaN while unknown, infinite for a stream).
    pub time: f32,
    pub duration: f32,
    /// Shown now (paused, or recently pointed at); a video hides its
    /// controls during playback otherwise.
    pub visible: bool,
}

struct Entry {
    media: NodeId,
    document: NodeId,
    ancestors: Vec<NodeId>,
    target: Url,
}

impl MediaFallbacks {
    pub(super) fn retained_bytes(&self) -> usize {
        self.presentations.capacity() * std::mem::size_of::<(NodeId, MediaPresentation)>()
            + self.entries.capacity() * std::mem::size_of::<Entry>()
            + self
                .entries
                .iter()
                .map(|entry| {
                    entry.ancestors.capacity() * std::mem::size_of::<NodeId>()
                        + entry.target.as_str().len()
                })
                .sum::<usize>()
    }

    /// Retire what belongs to the document's previous URL: the retained
    /// external-playback targets (see [`Dom::retained_media_controls`]).
    /// Presentations are not among them. A presentation is the live state of
    /// one element's player, which `media_host` sets when the player starts
    /// and clears when it closes. The arena's URL changes only through
    /// same-document navigations (HTML #url-and-history-update-steps for
    /// `history.pushState()` and `replaceState()`, #navigate-fragid, and
    /// same-document history traversal): the Document and its media elements
    /// persist, so a playing video must keep painting its frames. Dropping its
    /// surface here left the player running with nothing to show (archive.org
    /// pushes `/details/ID/FILE` as its player starts).
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Dom {
    /// The inline presentation of a media element, when its player has one.
    pub(crate) fn media_presentation(&self, media: NodeId) -> Option<MediaPresentation> {
        self.media_fallbacks
            .borrow()
            .presentations
            .get(&media)
            .copied()
    }

    /// Set or clear a media element's presentation. A changed natural size
    /// changes the replaced box (relayout); a new surface or poster state
    /// changes only its pixels.
    pub(crate) fn set_media_presentation(
        &mut self,
        media: NodeId,
        presentation: Option<MediaPresentation>,
    ) {
        let previous = {
            let mut state = self.media_fallbacks.borrow_mut();
            match presentation {
                Some(presentation) => state.presentations.insert(media, presentation),
                None => state.presentations.remove(&media),
            }
        };
        if previous == presentation {
            return;
        }
        // Shown controls change no layout (HTML Rendering: they overlay the
        // content); only the box's own presence and natural size do.
        if previous.and_then(|p| p.natural) != presentation.and_then(|p| p.natural)
            || previous.is_none() != presentation.is_none()
        {
            // The natural size sizes the box and fits the frame into it
            // (`object-fit`), but unlike an image's it is not a URL read the
            // layout memo observes, so retained boxes of the element and of
            // the ancestors sized by it would otherwise outlive the change
            // (a frame laid out before the metadata arrived stayed stretched).
            self.invalidate_layout_paths([media]);
            self.image_changed(media);
        } else {
            self.replaced_pixels_changed(media);
        }
    }

    /// Whether any media element exposes the user agent's controls.
    pub(crate) fn has_media_controls(&self) -> bool {
        self.media_fallbacks
            .borrow()
            .presentations
            .values()
            .any(|presentation| presentation.controls.is_some())
    }

    /// The natural size layout uses for an inline video: the video's, else
    /// none (the poster's natural size or CSS's default object size apply).
    pub(crate) fn media_natural_size(&self, media: NodeId) -> Option<(u32, u32)> {
        self.media_presentation(media)?
            .natural
            .filter(|&(w, h)| w > 0 && h > 0)
    }

    /// The embedding document is not the playback page of a nested player.
    /// Frame installation retains the final response URL, including redirects.
    pub(crate) fn media_document_url(&self, node: NodeId, page: &Url) -> Url {
        self.frame_owner(node)
            .and_then(|frame| self.properties.document_bases.get(&frame))
            .unwrap_or(page)
            .clone()
    }

    /// Carry the inline presentations of media elements moving to another
    /// arena of the group (`Dom::transplant_subtree`), into which their
    /// players keep presenting; substitute controls remembered for the
    /// moving nodes stay behind.
    pub(super) fn transplant_media(&self, into: &Dom, moved: &rustc_hash::FxHashSet<NodeId>) {
        let mut from = self.media_fallbacks.borrow_mut();
        from.entries.retain(|entry| {
            !moved.contains(&entry.media) && !entry.ancestors.iter().any(|id| moved.contains(id))
        });
        let moving: Vec<NodeId> = from
            .presentations
            .keys()
            .copied()
            .filter(|media| moved.contains(media))
            .collect();
        if moving.is_empty() {
            return;
        }
        let mut to = into.media_fallbacks.borrow_mut();
        for media in moving {
            if let Some(presentation) = from.presentations.remove(&media) {
                to.presentations.insert(media, presentation);
            }
        }
    }

    pub(crate) fn forget_media(&self, media: NodeId) {
        self.media_fallbacks
            .borrow_mut()
            .entries
            .retain(|entry| entry.media != media);
    }

    pub(crate) fn remember_media(&self, media: NodeId, base: &Url) {
        if let Some(target) = crate::layout2::media_target(self, base, media) {
            self.remember_media_target(media, &target);
        }
    }

    pub(crate) fn remember_media_target(&self, media: NodeId, target: &Url) {
        if !matches!(self.tag_name(media), Some("video" | "audio"))
            || !self.is_connected(media)
            || self.is_hidden(media)
            || self.paint_suppressed(media)
            || self.visibility_hidden(media)
        {
            return;
        }
        let Some(document) = self.owner_document(media) else {
            return;
        };
        if self
            .attr(media, "src")
            .is_some_and(|src| src.trim().is_empty())
        {
            return;
        }
        if !matches!(target.scheme(), "http" | "https") || target.as_str().len() > 16_384 {
            return;
        }
        // IDs are never reused and do not root the removed player. Do not
        // attach a retired player's UI to a different document or the entire
        // page after its enclosing content has been removed.
        let mut ancestors = Vec::new();
        let in_frame = self.frame_owner(media).is_some();
        let mut parent = self.parent_composed(media);
        while let Some(node) = parent {
            if ancestors.len() == 64
                || self.owner_document(node) != Some(document)
                || matches!(self.tag_name(node), None | Some("html"))
                || (!in_frame && self.tag_name(node) == Some("body"))
            {
                break;
            }
            ancestors.push(node);
            // A child Document is already confined to its player frame. Its
            // error handler may replace the whole body; the outer page must
            // not inherit a departed player's control that way.
            if self.tag_name(node) == Some("body") {
                break;
            }
            parent = self.parent_composed(node);
        }
        if ancestors.is_empty() {
            return;
        }
        let mut media_fallbacks = self.media_fallbacks.borrow_mut();
        media_fallbacks.entries.retain(|entry| entry.media != media);
        if media_fallbacks.entries.len() == 64 {
            media_fallbacks.entries.remove(0);
        }
        media_fallbacks.entries.push(Entry {
            media,
            document,
            ancestors,
            target: target.clone(),
        });
    }

    /// Only removed players need a substitute control. Existing media keeps
    /// its ordinary control and CSS suppression. Ambiguous shared containers
    /// do not turn into a link to an arbitrary member of a gallery.
    pub(crate) fn retained_media_controls(&self) -> HashMap<NodeId, Url> {
        let mut controls = HashMap::new();
        let mut ambiguous = std::collections::HashSet::new();
        for entry in &self.media_fallbacks.borrow().entries {
            if self.is_connected(entry.media) {
                continue;
            }
            let Some(anchor) = entry.ancestors.iter().copied().find(|&node| {
                self.is_connected(node) && self.owner_document(node) == Some(entry.document)
            }) else {
                continue;
            };
            if self.is_hidden(anchor)
                || self.paint_suppressed(anchor)
                || self.visibility_hidden(anchor)
                || !self.point_hit_testable(anchor)
                || self
                    .flat_descendants(anchor)
                    .into_iter()
                    .any(|node| matches!(self.tag_name(node), Some("video" | "audio")))
            {
                continue;
            }
            if let Some(previous) = controls.insert(anchor, entry.target.clone())
                && previous != entry.target
            {
                ambiguous.insert(anchor);
            }
        }
        controls.retain(|node, _| !ambiguous.contains(node));
        controls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing() -> MediaPresentation {
        MediaPresentation {
            surface: 7,
            natural: Some((512, 384)),
            show_poster: false,
            controls: None,
        }
    }

    /// HTML #url-and-history-update-steps: `history.pushState()` and
    /// `replaceState()` set the Document's URL and neither reload nor stop a
    /// media element, so the player's presentation (its frame surface and the
    /// video's natural size, which sizes a `width: auto` box) outlives any
    /// same-document URL change: a different path, query or fragment.
    #[test]
    fn a_same_document_url_change_keeps_a_players_presentation() {
        let mut dom = Dom::parse_document("<body><video id=v></video><video id=w></video>");
        let (video, other) = (dom.get_by_id("v").unwrap(), dom.get_by_id("w").unwrap());
        let url = |text: &str| url::Url::parse(text).ok();
        dom.set_doc_url(url("https://example.test/details/clip"));
        dom.set_media_presentation(video, Some(playing()));
        let paused = MediaPresentation {
            surface: 8,
            natural: None,
            show_poster: true,
            controls: None,
        };
        dom.set_media_presentation(other, Some(paused));
        for next in [
            "https://example.test/details/clip/episode.avi",
            "https://example.test/search?q=clip",
            "https://example.test/search?q=clip#results",
        ] {
            dom.set_doc_url(url(next));
            assert_eq!(dom.media_presentation(video), Some(playing()), "{next}");
            assert_eq!(dom.media_presentation(other), Some(paused), "{next}");
            assert_eq!(dom.media_natural_size(video), Some((512, 384)), "{next}");
        }
        // The player alone ends a presentation (`media_host` clears it when
        // it closes).
        dom.set_media_presentation(video, None);
        assert_eq!(dom.media_presentation(video), None);
        assert_eq!(dom.media_presentation(other), Some(paused));
    }

    /// What does belong to the previous URL: the terminal's retained
    /// external-playback targets, retired by a route change but not by a
    /// fragment navigation (the pre-existing contract of
    /// `retained_media_controls`).
    #[test]
    fn a_route_change_retires_retained_playback_targets_but_not_presentations() {
        let mut dom = Dom::parse_document(
            r#"<body><div id=player><video id=v src="/clip.mp4"></video></div>"#,
        );
        let base = url::Url::parse("https://example.test/").unwrap();
        let video = dom.get_by_id("v").unwrap();
        dom.set_doc_url(Some(base.clone()));
        dom.remember_media(video, &base);
        dom.set_media_presentation(video, Some(playing()));
        dom.detach(video);
        assert_eq!(dom.retained_media_controls().len(), 1);
        dom.set_doc_url(Some(
            url::Url::parse("https://example.test/#chapter").unwrap(),
        ));
        assert_eq!(dom.retained_media_controls().len(), 1);
        dom.set_doc_url(Some(url::Url::parse("https://example.test/next").unwrap()));
        assert!(dom.retained_media_controls().is_empty());
        assert_eq!(dom.media_presentation(video), Some(playing()));
    }
}
