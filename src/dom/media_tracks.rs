//! The arena side of HTML's out-of-band text tracks (#sourcing-out-of-band-
//! text-tracks, local whatwg/html e5071a20, 2026-09-04): "when a track
//! element's parent element changes and the new parent is a media element"
//! its text track joins that media element's list of text tracks, and when
//! the old parent was one it leaves the list, each with a queued addtrack or
//! removetrack event, automatic text track selection and the track
//! processing model. A media element's list of text tracks starts with its
//! `track` element children in tree order, which the arena itself holds; so
//! only the parent changes need recording.
//!
//! Every tree operation that changes a node's parent (insertion, removal,
//! replace-all, moves, cloning, parsing, and arena transfers) reports a
//! `track` element entering or leaving an `audio` or `video` element here,
//! in order. Parser-created elements have no wrapper until script touches
//! them, which is why this is native: the platform prelude takes the
//! journal at its next mutation-observer microtask (or at the parser's
//! media element boundaries) and runs the steps the changes call for.

use super::*;

#[derive(Default)]
pub(crate) struct State {
    /// (track element, media element, entered): each parent change of a
    /// track element whose old or new parent is a media element.
    changes: Vec<(NodeId, NodeId, bool)>,
}

impl State {
    pub(super) fn retained_bytes(&self) -> usize {
        self.changes.capacity() * std::mem::size_of::<(NodeId, NodeId, bool)>()
    }
}

impl Dom {
    fn is_html_element_named(&self, id: NodeId, names: &[&str]) -> bool {
        matches!(&self.nodes[id].data, NodeData::Element { name, .. }
            if name.ns == ns!(html) && names.contains(&&*name.local))
    }

    /// HTML #media-element: an HTML `audio` or `video` element.
    pub(crate) fn is_media_element(&self, id: NodeId) -> bool {
        self.is_valid(id) && self.is_html_element_named(id, &["audio", "video"])
    }

    /// `child` entered (`entered`) or left `parent`'s children.
    pub(super) fn note_track_parent_change(
        &mut self,
        parent: NodeId,
        child: NodeId,
        entered: bool,
    ) {
        if self.is_html_element_named(child, &["track"])
            && self.is_html_element_named(parent, &["audio", "video"])
        {
            self.media_tracks.changes.push((child, parent, entered));
        }
    }

    /// Whether any track element parent change awaits the platform.
    pub(crate) fn has_track_changes(&self) -> bool {
        !self.media_tracks.changes.is_empty()
    }

    /// Take the recorded track element parent changes, in order.
    pub(crate) fn take_track_changes(&mut self) -> Vec<(NodeId, NodeId, bool)> {
        std::mem::take(&mut self.media_tracks.changes)
    }

    /// Return changes another Window Realm of this arena takes, ahead of
    /// any recorded since they were taken.
    pub(crate) fn restore_track_changes(&mut self, mut changes: Vec<(NodeId, NodeId, bool)>) {
        changes.append(&mut self.media_tracks.changes);
        self.media_tracks.changes = changes;
    }

    /// The pending changes keep their nodes until the platform takes them,
    /// as a queued task keeps its target.
    pub(super) fn visit_track_change_roots(&self, visit: &mut impl FnMut(NodeId)) {
        for &(track, media, _) in &self.media_tracks.changes {
            visit(track);
            visit(media);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_parent_changes_are_journaled_in_order() {
        // HTML #sourcing-out-of-band-text-tracks: parsing, insertion, a
        // move and removal each change a track element's parent.
        let mut dom = Dom::parse_document(
            "<video id=v><track id=t1><source></video><audio id=a></audio><div id=d></div>",
        );
        let video = dom.get_by_id("v").unwrap();
        let audio = dom.get_by_id("a").unwrap();
        let div = dom.get_by_id("d").unwrap();
        let t1 = dom.get_by_id("t1").unwrap();
        assert_eq!(dom.take_track_changes(), vec![(t1, video, true)]);
        dom.append(audio, t1);
        assert_eq!(
            dom.take_track_changes(),
            vec![(t1, video, false), (t1, audio, true)]
        );
        dom.append(div, t1);
        assert_eq!(dom.take_track_changes(), vec![(t1, audio, false)]);
        // A track in any other parent, and other children of media
        // elements, are not text tracks of a media element.
        let source = dom.create_element("source");
        dom.append(video, source);
        dom.detach(t1);
        assert!(!dom.has_track_changes());
        let t2 = dom.create_element("track");
        dom.append(video, t2);
        dom.replace_all_children(video, Vec::new());
        assert_eq!(
            dom.take_track_changes(),
            vec![(t2, video, true), (t2, video, false)]
        );
    }
}
