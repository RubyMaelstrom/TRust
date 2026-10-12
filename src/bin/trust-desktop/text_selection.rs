//! The user's selection of page text (Selection API §2 "User Interactions",
//! local w3c/selection-api b475d4b): the primary button's press, drag and
//! release, double and triple clicks, Shift+click, select-all, autoscroll
//! and copying.
//!
//! A live page owns its Document's selection: the frontend paints its own
//! gesture at once, sends the page the boundary point under the pointer
//! with each input, and paints the page's report once the page has
//! processed the frontend's latest gesture. A static page or line document
//! has no page to own one, so its selection is the frontend's alone, in the
//! order its text paints.

use super::*;

use trust::core::TextPoint;
use trust::js::PointerSelection;
use trust::render::PageSelection;

/// The platform's multi-click interval and slop, as the page's own click
/// counting (`detail`) uses them.
const CLICK_INTERVAL: Duration = Duration::from_millis(500);
const CLICK_SLOP: f32 = 4.0;
/// How often a drag beyond the content viewport scrolls it.
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(16);

/// How a gesture grows: by characters, or by the words or paragraphs a
/// double or triple click selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectionGranularity {
    Character,
    Word,
    Paragraph,
}

/// The primary button's selection gesture, from press to release.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SelectionGesture {
    pub(super) granularity: SelectionGranularity,
    /// The granule the press selected; a drag keeps it whole.
    pub(super) origin: (TextPoint, TextPoint),
    /// Dragging extends the selection. A hyperlink's press is its
    /// activation, unless Alt makes it select the link's text.
    pub(super) drag: bool,
}

#[derive(Default)]
pub(super) struct TextSelectionState {
    /// What the frontend paints.
    pub(super) paint: Option<PageSelection>,
    pub(super) anchor: Option<TextPoint>,
    pub(super) focus: Option<TextPoint>,
    pub(super) gesture: Option<SelectionGesture>,
    /// The serial of the last gesture input sent to the page.
    pub(super) serial: u32,
    /// The generation of the page report last applied.
    pub(super) report_generation: u64,
    /// The document the selection is in.
    pub(super) document_generation: u64,
    /// The last press of the click series: when, where, and its count.
    pub(super) clicks: Option<(Instant, CssPoint, u32)>,
    /// A drag beyond the content viewport scrolls it on a timer.
    pub(super) autoscroll_scheduled: bool,
}

impl TextSelectionState {
    /// Whether anything is selected.
    pub(super) fn selects(&self) -> bool {
        self.paint
            .as_ref()
            .is_some_and(|selection| !selection.is_collapsed())
    }
}

impl DesktopApp {
    /// The paint list the selection's boundary points are laid out in: the
    /// HTML page's, or the line document's.
    pub(super) fn selection_page(&self) -> Option<&trust::render::PagePaint> {
        if self.tab.terminal.is_some() {
            return None;
        }
        self.tab
            .page_layout
            .as_ref()
            .map(|page| &page.layout.paint)
            .or_else(|| {
                self.tab
                    .protocol_page
                    .as_ref()
                    .map(|page| &page.layout.paint)
            })
    }

    /// Whether a live page's Document owns the selection.
    fn selection_owned_by_page(&self) -> bool {
        self.tab.terminal.is_none()
            && self.tab.page_layout.is_some()
            && self.tab.browser.page_is_live()
    }

    /// The pointer, kept inside the content viewport, as a gesture extends
    /// the selection to the text nearest it.
    fn selection_pointer(&self) -> Option<CssPoint> {
        let content = self.scene.as_ref()?.content_viewport;
        Some(CssPoint::new(
            self.pointer
                .x
                .clamp(content.x, (content.x + content.width - 1.0).max(content.x)),
            self.pointer
                .y
                .clamp(content.y, (content.y + content.height - 1.0).max(content.y)),
        ))
    }

    fn selection_point_at_pointer(&self) -> Option<TextPoint> {
        self.scene
            .as_ref()?
            .nearest_text_point(self.selection_pointer()?)
    }

    pub(super) fn clear_text_selection(&mut self) {
        let selection = &mut self.tab.text_selection;
        selection.anchor = None;
        selection.focus = None;
        selection.gesture = None;
        if selection.paint.take().is_some() {
            self.request_redraw();
        }
    }

    fn set_local_selection(&mut self, anchor: TextPoint, focus: TextPoint) {
        let selection = &mut self.tab.text_selection;
        selection.anchor = Some(anchor);
        selection.focus = Some(focus);
        selection.paint = Some(PageSelection::Painted { anchor, focus });
        self.request_redraw();
    }

    /// A new document has no selection; a live page's report about its
    /// Document's selection replaces the frontend's once the page has
    /// processed every gesture input sent to it. Returns whether the
    /// painted selection changed.
    pub(super) fn sync_page_selection(&mut self) -> bool {
        let generation = self.tab.browser.document_generation();
        if generation != self.tab.text_selection.document_generation {
            self.tab.text_selection = TextSelectionState {
                document_generation: generation,
                report_generation: self.tab.text_selection.report_generation,
                serial: self.tab.text_selection.serial,
                ..TextSelectionState::default()
            };
            return true;
        }
        let (report_generation, report) = self.tab.browser.page_selection();
        if report_generation == self.tab.text_selection.report_generation {
            return false;
        }
        self.tab.text_selection.report_generation = report_generation;
        let Some(report) = report else {
            // The live page went away, and its Document's selection.
            if matches!(
                self.tab.text_selection.paint,
                Some(PageSelection::Document(_))
            ) {
                self.tab.text_selection.paint = None;
                self.tab.text_selection.anchor = None;
                self.tab.text_selection.focus = None;
                return true;
            }
            return false;
        };
        if report.serial < self.tab.text_selection.serial
            || self.tab.text_selection.gesture.is_some()
        {
            return false;
        }
        let selection = &mut self.tab.text_selection;
        selection.paint =
            (!report.segments.is_empty()).then(|| PageSelection::Document(report.segments.clone()));
        selection.anchor = report.anchor;
        selection.focus = report.focus;
        true
    }

    /// Selection API §2: the primary button's press on page content begins
    /// a gesture. Returns what the page is told of it.
    pub(super) fn begin_selection_gesture(&mut self, link_press: bool) -> Option<PointerSelection> {
        self.selection_page()?;
        let now = Instant::now();
        let count = match self.tab.text_selection.clicks {
            Some((at, point, count))
                if now.duration_since(at) < CLICK_INTERVAL
                    && (point.x - self.pointer.x).abs() <= CLICK_SLOP
                    && (point.y - self.pointer.y).abs() <= CLICK_SLOP =>
            {
                count + 1
            }
            _ => 1,
        };
        self.tab.text_selection.clicks = Some((now, self.pointer, count));
        self.tab.text_selection.serial += 1;
        let point = self.selection_point_at_pointer();
        let report = Some(PointerSelection::new(point, self.tab.text_selection.serial));
        self.tab.text_selection.gesture = None;
        // CSS UI 4 #content-selection: a press in `user-select: none`
        // content neither starts a selection nor affects the one there is.
        let scene = self.scene.as_ref()?;
        if scene.unselectable_text_at(self.pointer) {
            return report;
        }
        let Some(point) = point else {
            // §2: a user action collapses a selection rather than emptying it.
            if self.tab.text_selection.selects() {
                self.tab.text_selection.paint = None;
                self.request_redraw();
            }
            return report;
        };
        let pointer = self.pointer;
        let shift = self.modifiers.shift_key();
        let (granularity, origin) = match (shift, self.tab.text_selection.anchor, count) {
            (true, Some(anchor), _) => (SelectionGranularity::Character, (anchor, anchor)),
            (_, _, 2) => (
                SelectionGranularity::Word,
                scene.word_at(pointer).unwrap_or((point, point)),
            ),
            (_, _, 3..) => (
                SelectionGranularity::Paragraph,
                scene.paragraph_at(pointer).unwrap_or((point, point)),
            ),
            _ => (SelectionGranularity::Character, (point, point)),
        };
        self.tab.text_selection.gesture = Some(SelectionGesture {
            granularity,
            origin,
            drag: !link_press || self.modifiers.alt_key(),
        });
        if shift && self.tab.text_selection.anchor.is_some() {
            self.set_local_selection(origin.0, point);
        } else {
            self.set_local_selection(origin.0, origin.1);
        }
        report
    }

    /// The selection a primary press on page content makes: a form
    /// control's press selects in its own editor (and only collapses the
    /// page's), any other begins a gesture; a hyperlink's only collapses
    /// the selection, unless Alt.
    pub(super) fn page_press_selection(
        &mut self,
        hit: Option<&PageHit>,
    ) -> Option<PointerSelection> {
        let form = hit.is_some_and(|hit| {
            matches!(hit.link, Some(Link::Form { .. })) || self.form_target_for_hit(hit).is_some()
        });
        if form {
            self.selection_page()?;
            self.tab.text_selection.gesture = None;
            if self.tab.text_selection.selects() {
                self.tab.text_selection.paint = None;
                self.request_redraw();
            }
            self.tab.text_selection.serial += 1;
            return Some(PointerSelection::new(None, self.tab.text_selection.serial));
        }
        let link = hit
            .and_then(|hit| hit.link.as_ref())
            .is_some_and(|link| !matches!(link, Link::JsClick { href, .. } if href.is_empty()));
        self.begin_selection_gesture(link)
    }

    /// The gesture's motion: the focus follows the text nearest the
    /// pointer, by whole granules. Returns what the page is told of it,
    /// when it changed the selection.
    pub(super) fn extend_selection_gesture(&mut self) -> Option<PointerSelection> {
        let gesture = self
            .tab
            .text_selection
            .gesture
            .filter(|gesture| gesture.drag)?;
        self.schedule_selection_autoscroll();
        let point = self.selection_point_at_pointer()?;
        let (anchor, focus) = match gesture.granularity {
            SelectionGranularity::Character => (gesture.origin.0, point),
            granularity => {
                let pointer = self.selection_pointer()?;
                let scene = self.scene.as_ref()?;
                let granule = if granularity == SelectionGranularity::Word {
                    scene.word_at(pointer)
                } else {
                    scene.paragraph_at(pointer)
                }
                .unwrap_or((point, point));
                let before = self.selection_page().and_then(|page| {
                    trust::render::selection::paint_order(page, point, gesture.origin.0)
                }) == Some(std::cmp::Ordering::Less);
                if before {
                    (gesture.origin.1, granule.0)
                } else {
                    (gesture.origin.0, granule.1)
                }
            }
        };
        if self.tab.text_selection.anchor == Some(anchor)
            && self.tab.text_selection.focus == Some(focus)
        {
            return None;
        }
        self.set_local_selection(anchor, focus);
        self.tab.text_selection.serial += 1;
        Some(PointerSelection::new(
            Some(point),
            self.tab.text_selection.serial,
        ))
    }

    /// The release ends the gesture at the pointer.
    pub(super) fn end_selection_gesture(&mut self) -> Option<PointerSelection> {
        self.selection_page()?;
        let gesture = self.tab.text_selection.gesture.take();
        self.tab.text_selection.serial += 1;
        let point = gesture
            .filter(|gesture| gesture.drag)
            .and_then(|_| self.selection_point_at_pointer());
        Some(PointerSelection::new(point, self.tab.text_selection.serial))
    }

    /// While a drag holds the pointer beyond the content viewport, scroll
    /// it toward the pointer, faster the farther the pointer is, and keep
    /// extending the selection (the platform's drag autoscroll).
    fn schedule_selection_autoscroll(&mut self) {
        if self.tab.text_selection.autoscroll_scheduled || self.autoscroll_step().is_none() {
            return;
        }
        self.tab.text_selection.autoscroll_scheduled = true;
        let proxy = self.event_proxy.clone();
        self.runtime.spawn(async move {
            tokio::time::sleep(AUTOSCROLL_INTERVAL).await;
            let _ = proxy.send_event(DesktopEvent::SelectionAutoscroll);
        });
    }

    /// The scroll a drag beyond the content viewport asks for now.
    fn autoscroll_step(&self) -> Option<CssPoint> {
        self.tab
            .text_selection
            .gesture
            .filter(|gesture| gesture.drag)?;
        let scene = self.scene.as_ref()?;
        let content = scene.content_viewport;
        let speed = |beyond: f32| (beyond * 0.6).clamp(2.0, 64.0);
        let dy = if self.pointer.y < content.y {
            -speed(content.y - self.pointer.y)
        } else if self.pointer.y > content.y + content.height {
            speed(self.pointer.y - content.y - content.height)
        } else {
            0.0
        };
        let dx = if self.pointer.x < content.x {
            -speed(content.x - self.pointer.x)
        } else if self.pointer.x > content.x + content.width {
            speed(self.pointer.x - content.x - content.width)
        } else {
            0.0
        };
        let scroll = self.tab.browser.interaction().scroll;
        let max = CssPoint::new(
            (scene.page_size.width - content.width).max(0.0),
            (scene.page_size.height - content.height).max(0.0),
        );
        let target = CssPoint::new(
            (scroll.x + dx).clamp(0.0, max.x),
            (scroll.y + dy).clamp(0.0, max.y),
        );
        (target != scroll).then_some(target)
    }

    pub(super) fn selection_autoscroll_tick(&mut self) {
        self.tab.text_selection.autoscroll_scheduled = false;
        if !self.selection_dragging() {
            return;
        }
        if let Some(target) = self.autoscroll_step() {
            self.dispatch(UserAction::SetViewportScroll(target));
            self.request_redraw();
            if let Some(selection) = self.extend_selection_gesture() {
                self.send_selection_motion(selection);
            }
            self.schedule_selection_autoscroll();
        }
    }

    fn selection_dragging(&self) -> bool {
        self.tab
            .text_selection
            .gesture
            .is_some_and(|gesture| gesture.drag)
            && self.page_pointer_buttons & 1 != 0
    }

    /// Tell a live page about the gesture's motion (its pointer motion
    /// carries the boundary point under the pointer).
    pub(super) fn send_selection_motion(&mut self, selection: PointerSelection) {
        if !self.selection_owned_by_page() {
            return;
        }
        let Some(scene) = self.scene.as_ref() else {
            return;
        };
        let position = CssPoint::new(
            self.pointer.x - scene.content_viewport.x,
            self.pointer.y - scene.content_viewport.y,
        );
        let actor = self.tab.hovered_actor;
        let mut metadata = self.pointer_metadata();
        metadata.selection = Some(selection);
        self.dispatch(UserAction::PageHover {
            actor,
            position,
            metadata,
        });
    }

    /// The platform's select-all for a page without a live Document: all
    /// of its selectable text in paint order. A live page selects its own
    /// as the default action of the key.
    pub(super) fn select_all_page_text(&mut self) -> bool {
        if self.selection_owned_by_page() {
            return false;
        }
        let Some((first, last)) = self
            .selection_page()
            .and_then(trust::render::selection::page_text_bounds)
        else {
            return false;
        };
        self.tab.text_selection.gesture = None;
        self.set_local_selection(first, last);
        true
    }

    /// What copying the page's selection puts on the clipboard, for a page
    /// without a live Document (a live page runs its copy action).
    pub(super) fn selected_page_text(&self) -> Option<String> {
        let selection = self.tab.text_selection.paint.as_ref()?;
        let text = trust::render::selection::selected_page_text(self.selection_page()?, selection);
        (!text.is_empty()).then_some(text)
    }

    /// The user's copy action on the page's selection (Clipboard API
    /// #copy-action): a live page fires `copy` and writes what it decides
    /// through its clipboard requests; otherwise the frontend writes the
    /// selected text. Returns whether there was a selection to copy.
    pub(super) fn copy_page_selection(&mut self) -> bool {
        if !self.tab.text_selection.selects() {
            return false;
        }
        if self.selection_owned_by_page() {
            self.dispatch(UserAction::Copy);
            return true;
        }
        if let Some(text) = self.selected_page_text()
            && let Some(clipboard) = &mut self.clipboard
        {
            let _ = clipboard.set_text(text);
        }
        true
    }
}
