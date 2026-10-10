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

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.presentations.clear();
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
