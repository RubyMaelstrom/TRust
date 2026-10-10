//! The Unicode Bidirectional Algorithm in a block container's inline
//! formatting context: CSS Writing Modes 4 #text-direction (local
//! csswg-drafts 81c27f68, `css-writing-modes-4/Overview.bs`), with UAX #9
//! itself from the `unicode-bidi` crate (P/X/W/N/I rules and L2).
//!
//! Lines are broken in logical order. Meanwhile the formatting context
//! keeps each bidi paragraph's text (#bidi-algo: the inline content between
//! forced paragraph breaks): its characters, the collapsed spaces and tabs
//! between pieces, an object replacement character per atomic inline
//! (#bidi-atomic-inlines) and the controls each inline box's `unicode-bidi`
//! injects (HL3, #bidi-control-codes-injection-table), closed at a
//! paragraph break and reopened after it (#bidi-embedding-breaks). Once the
//! paragraph ends its levels are resolved over all of it, with the base
//! level from the block's `direction` (HL1) or, for `unicode-bidi:
//! plaintext`, P2/P3 (#bidi-para-direction). Then each of its lines applies
//! L1, splits text pieces where the level changes, shapes each in its
//! level's direction, and permutes the pieces' positions in visual order
//! (L2) without reordering `LineOut::pieces`, which stays logical for
//! selection and hit testing. Inline boxes get their margins, borders and
//! padding in visual order and split into one fragment per visually
//! contiguous run (#bidi-box-model, #bidi-fragmentation).
//!
//! A paragraph without right-to-left characters, an RTL base level, a
//! right-to-left inline box or a right-to-left override needs none of this:
//! its lines are left exactly as laid.

use std::ops::Range;
use std::sync::Arc;

use unicode_bidi::{BidiClass, BidiInfo, Level};

use super::{BoxBound, Ifc, InlineBoxEntry, InlineBoxKey, LineOut, Piece};
use crate::dom::NodeId;
use crate::layout2::style::{Align2, BoxStyle, UnicodeBidi};

/// How a replaced element takes part in its paragraph (CSS Writing Modes 4
/// #bidi-atomic-inlines) and the horizontal edges laid around it.
pub(crate) struct AtomBidi {
    edges: [f32; 2],
    strong: Option<bool>,
}

impl AtomBidi {
    /// A replaced element is a neutral, except that `embed` and
    /// `bidi-override` make it a strong character of its `direction`.
    pub(super) fn replaced(style: &BoxStyle, edges: [f32; 2]) -> Self {
        AtomBidi {
            edges,
            strong: matches!(
                style.unicode_bidi,
                UnicodeBidi::Embed | UnicodeBidi::BidiOverride
            )
            .then_some(style.rtl),
        }
    }
}

/// The bidi paragraph being laid and the context that carries over to the
/// next one.
pub(crate) struct BidiState {
    /// The paragraph's text as UAX #9 sees it.
    pub(super) text: String,
    /// The replaced element whose piece is placed next.
    pub(super) next_atom: Option<AtomBidi>,
    /// The paragraph needs resolution: it can hold an odd level or a
    /// right-to-left box.
    needs: bool,
    /// The paragraph's first line.
    first_line: usize,
    /// The start controls of the inline boxes still open, outermost first.
    open: Vec<&'static str>,
    /// Inline boxes still open whose `direction` is `rtl`.
    rtl_boxes: usize,
    /// The block container's `direction` is `rtl`.
    block_rtl: bool,
    /// The block container's `unicode-bidi` is `plaintext`.
    plaintext: bool,
    /// LRO/RLO for a block container with `bidi-override` or
    /// `isolate-override`.
    root_override: Option<&'static str>,
    /// Whether `text-align` and the last line's alignment are relative to
    /// the start edge, which a plaintext line box can move.
    flip_align: [bool; 2],
    /// The inline base direction of the last line box laid (CSS Text 3
    /// #bidi-linebox).
    line_rtl: bool,
    /// Margins (and borders and padding) laid beside replaced elements:
    /// (their text position, left and right).
    atom_edges: Vec<(u32, [f32; 2])>,
    /// Empty inline boxes and static positions met in the paragraph.
    inserts: Vec<Insert>,
}

/// A logical insertion point of the paragraph that holds no character: an
/// empty inline box (CSS 2 §10.8) or an inline-level out-of-flow box's
/// static position (CSS 2 §10.3.7). Reordering places it where its logical
/// position falls in the visual line.
pub(super) struct Insert {
    /// Its position in the paragraph text.
    at: u32,
    /// The inline boxes enclosing it: the first `depth` entries of `chain`.
    chain: Option<Arc<[InlineBoxEntry]>>,
    depth: usize,
    kind: InsertKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InsertKind {
    /// An empty inline box, by its key.
    Empty(InlineBoxKey),
    /// A static position, by its index in `Ifc::oofs`.
    Static(usize),
}

const LRM: &str = "\u{200E}";
const RLM: &str = "\u{200F}";
const OBJECT: &str = "\u{FFFC}";

impl BidiState {
    fn new(block_rtl: bool, unicode_bidi: UnicodeBidi, flip_align: [bool; 2]) -> Self {
        let mut state = BidiState {
            text: String::new(),
            next_atom: None,
            needs: false,
            first_line: 0,
            open: Vec::new(),
            rtl_boxes: 0,
            block_rtl,
            plaintext: unicode_bidi == UnicodeBidi::Plaintext,
            root_override: unicode_bidi.root_override(block_rtl),
            flip_align,
            line_rtl: block_rtl,
            atom_edges: Vec::new(),
            inserts: Vec::new(),
        };
        state.begin_paragraph();
        state
    }

    /// Start the next paragraph: the block's override and the controls of
    /// the boxes still open apply again (#bidi-embedding-breaks).
    fn begin_paragraph(&mut self) {
        self.text.clear();
        self.atom_edges.clear();
        self.inserts.clear();
        self.needs = self.block_rtl || self.rtl_boxes > 0;
        if let Some(control) = self.root_override {
            self.text.push_str(control);
        }
        for control in &self.open {
            self.text.push_str(control);
        }
    }

    /// Append a piece's text, returning where it begins.
    pub(super) fn push_text(&mut self, text: &str) -> u32 {
        let at = self.text.len() as u32;
        self.text.push_str(text);
        if !self.needs && !text.is_ascii() {
            self.needs = text.chars().any(|c| {
                matches!(
                    unicode_bidi::bidi_class(c),
                    BidiClass::R
                        | BidiClass::AL
                        | BidiClass::AN
                        | BidiClass::RLE
                        | BidiClass::RLO
                        | BidiClass::RLI
                )
            });
        }
        at
    }

    /// Append the next atomic inline's character, returning where it is.
    pub(super) fn push_atom(&mut self) -> u32 {
        let atom = self.next_atom.take();
        let at = self.text.len() as u32;
        self.text
            .push_str(match atom.as_ref().and_then(|atom| atom.strong) {
                Some(true) => {
                    self.needs = true;
                    RLM
                }
                Some(false) => LRM,
                None => OBJECT,
            });
        if let Some(atom) = atom.filter(|atom| atom.edges != [0.0; 2]) {
            self.atom_edges.push((at, atom.edges));
        }
        at
    }

    /// Whether text appended now continues the piece at `at` of `len` bytes
    /// (after its collapsed space): no control intervened.
    pub(super) fn continues(&self, at: u32, len: usize, space: bool) -> bool {
        at != u32::MAX && self.text.len() == at as usize + len + usize::from(space)
    }

    /// An inline box closed without content here; `chain` holds the boxes
    /// open at its end, itself innermost.
    pub(super) fn insert_empty(&mut self, key: InlineBoxKey, chain: Option<Arc<[InlineBoxEntry]>>) {
        let depth = chain
            .as_deref()
            .map_or(0, |chain| chain.len().saturating_sub(1));
        self.inserts.push(Insert {
            at: self.text.len() as u32,
            chain,
            depth,
            kind: InsertKind::Empty(key),
        });
    }

    /// The static position of the out-of-flow box `index`, inside `chain`.
    pub(super) fn insert_static(&mut self, index: usize, chain: Option<Arc<[InlineBoxEntry]>>) {
        let depth = chain.as_deref().map_or(0, <[InlineBoxEntry]>::len);
        self.inserts.push(Insert {
            at: self.text.len() as u32,
            chain,
            depth,
            kind: InsertKind::Static(index),
        });
    }

    pub(super) fn open_box(&mut self, controls: Option<(&'static str, &'static str)>, rtl: bool) {
        if rtl {
            self.rtl_boxes += 1;
            self.needs = true;
        }
        if let Some((start, _)) = controls {
            self.text.push_str(start);
            self.open.push(start);
        }
    }

    pub(super) fn close_box(&mut self, controls: Option<(&'static str, &'static str)>, rtl: bool) {
        if rtl {
            self.rtl_boxes = self.rtl_boxes.saturating_sub(1);
        }
        if let Some((_, end)) = controls {
            self.text.push_str(end);
            self.open.pop();
        }
    }
}

/// A piece of a line: an entry of `LineOut::pieces` or `atom_boxes`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Piece(usize),
    Atom(usize),
}

fn piece(line: &LineOut, slot: Slot) -> &Piece {
    match slot {
        Slot::Piece(index) => &line.pieces[index],
        Slot::Atom(index) => &line.atom_boxes[index],
    }
}

fn piece_mut(line: &mut LineOut, slot: Slot) -> &mut Piece {
    match slot {
        Slot::Piece(index) => &mut line.pieces[index],
        Slot::Atom(index) => &mut line.atom_boxes[index],
    }
}

/// One unit of a line's visual order, in logical order: a piece (an atomic
/// inline with its own margins), the space laid between pieces, or an
/// insertion point among them.
struct Unit<'r> {
    level: Level,
    width: f32,
    kind: UnitKind,
    /// Enclosing boxes: the first `depth` entries of `chain`.
    chain: Option<Arc<[InlineBoxEntry]>>,
    depth: usize,
    /// A collapsed space lies in this gap.
    space: bool,
    /// An atomic inline's left and right edges.
    atom: [f32; 2],
    /// Which fragment of each enclosing box the unit belongs to on the
    /// paragraph's infinitely long line (`InfiniteRuns`).
    runs: Option<&'r [(InlineBoxKey, u32)]>,
}

#[derive(Clone, Copy)]
enum UnitKind {
    Piece(Slot),
    Gap,
    /// An entry of the line's inserts.
    Insert(usize),
}

impl Unit<'_> {
    fn boxes(&self) -> &[InlineBoxEntry] {
        self.chain
            .as_deref()
            .map_or(&[], |chain| &chain[..self.depth.min(chain.len())])
    }

    fn run(&self, key: InlineBoxKey) -> Option<u32> {
        self.runs?
            .iter()
            .find(|(other, _)| *other == key)
            .map(|&(_, run)| run)
    }
}

/// CSS Writing Modes 4 #bidi-fragmentation: an inline box is broken into
/// fragments where reordering would divide it with intervening content on
/// an infinitely long line, whether or not line breaking then puts the
/// fragments side by side. This is that line for one paragraph: one L2 pass
/// over all its pieces and the spaces between them, numbering each box's
/// visually contiguous runs.
struct InfiniteRuns {
    /// Level-uniform parts of pieces, in logical order: their byte ranges
    /// and the run of each enclosing box.
    parts: Vec<(Range<usize>, BoxRuns)>,
    /// The spaces between pieces, by the following piece's text position.
    gaps: Vec<(usize, BoxRuns)>,
}

/// Each enclosing box's fragment number on the infinitely long line.
type BoxRuns = Vec<(InlineBoxKey, u32)>;

impl InfiniteRuns {
    fn part(&self, at: usize) -> Option<&[(InlineBoxKey, u32)]> {
        let index = self.parts.partition_point(|(range, _)| range.end <= at);
        self.parts
            .get(index)
            .filter(|(range, _)| range.start <= at)
            .map(|(_, runs)| runs.as_slice())
    }

    fn gap(&self, at: usize) -> Option<&[(InlineBoxKey, u32)]> {
        self.gaps
            .binary_search_by_key(&at, |(position, _)| *position)
            .ok()
            .map(|index| self.gaps[index].1.as_slice())
    }
}

/// The text range of a piece's characters (an atomic inline's object
/// replacement character), if it is the piece's own text.
fn piece_range(text: &str, piece: &Piece) -> Option<Range<usize>> {
    let at = piece.bidi_pos as usize;
    if at >= text.len() {
        return None;
    }
    let len = if piece.is_text() {
        let len = piece.item.text.len();
        (text.get(at..at + len)? == piece.item.text).then_some(len)?
    } else {
        text.get(at..)?.chars().next()?.len_utf8()
    };
    Some(at..at + len)
}

/// The last space or tab in `text[from..to]`: what lies between two pieces.
fn separator(text: &str, info: &BidiInfo<'_>, from: usize, to: usize) -> Option<usize> {
    text.get(from..to)?
        .char_indices()
        .rev()
        .find(|&(offset, _)| {
            matches!(
                info.original_classes[from + offset],
                BidiClass::WS | BidiClass::S
            )
        })
        .map(|(offset, _)| from + offset)
}

/// The number of boxes two chains share, outermost first.
fn common(a: Option<&Arc<[InlineBoxEntry]>>, b: Option<&Arc<[InlineBoxEntry]>>) -> usize {
    let (Some(a), Some(b)) = (a, b) else {
        return 0;
    };
    a.iter()
        .zip(b.iter())
        .take_while(|(a, b)| a.key == b.key)
        .count()
}

/// Lay the paragraph of `lines` on one infinitely long line (L1 then only
/// resets the whitespace ending the paragraph) and number each box's
/// fragments there.
fn infinite_runs(text: &str, info: &BidiInfo<'_>, lines: &[LineOut]) -> Option<InfiniteRuns> {
    let mut parts: Vec<(Range<usize>, &Piece)> = Vec::new();
    for line in lines {
        for piece in line.pieces.iter().chain(&line.atom_boxes) {
            parts.push((piece_range(text, piece)?, piece));
        }
    }
    parts.sort_by_key(|(range, _)| range.start);
    if parts.windows(2).any(|pair| pair[1].0.start < pair[0].0.end) {
        return None;
    }
    let lo = parts.first()?.0.start;
    let hi = parts.iter().map(|(range, _)| range.end).max()?;
    let para = info
        .paragraphs
        .iter()
        .find(|para| para.range.contains(&lo))?;
    let mut levels = info.levels[lo..hi].to_vec();
    apply_l1(
        &info.original_classes[lo..hi],
        &mut levels,
        &text[lo..hi],
        para.level,
    );
    struct Item<'p> {
        level: Level,
        chain: Option<&'p Arc<[InlineBoxEntry]>>,
        depth: usize,
        part: Option<Range<usize>>,
        gap: usize,
    }
    let mut items: Vec<Item<'_>> = Vec::new();
    for (i, (range, piece)) in parts.iter().enumerate() {
        let chain = piece.boxes.as_ref().map(|boxes| &boxes.chain);
        if i > 0
            && let Some(at) = separator(text, info, parts[i - 1].0.end, range.start)
        {
            let previous = parts[i - 1].1.boxes.as_ref().map(|boxes| &boxes.chain);
            items.push(Item {
                level: levels[at - lo],
                chain,
                depth: common(previous, chain),
                part: None,
                gap: range.start,
            });
        }
        let depth = chain.map_or(0, |chain| chain.len());
        let runs = if piece.is_text() {
            level_runs(&piece.item.text, &levels[range.start - lo..range.end - lo])
        } else {
            vec![(0..range.len(), levels[range.start - lo])]
        };
        for (run, level) in runs {
            items.push(Item {
                level,
                chain,
                depth,
                part: Some(range.start + run.start..range.start + run.end),
                gap: 0,
            });
        }
    }
    let order = BidiInfo::reorder_visual(&items.iter().map(|item| item.level).collect::<Vec<_>>());
    let mut runs: Vec<BoxRuns> = vec![Vec::new(); items.len()];
    // Per box: its last visual unit and current run.
    let mut last: Vec<(InlineBoxKey, usize, u32)> = Vec::new();
    let mut next = 0u32;
    for (v, &u) in order.iter().enumerate() {
        let item = &items[u];
        for entry in item.chain.map_or(&[][..], |chain| &chain[..item.depth]) {
            let run = match last.iter_mut().find(|(key, _, _)| *key == entry.key) {
                Some(record) => {
                    if record.1 + 1 != v {
                        next += 1;
                        record.2 = next;
                    }
                    record.1 = v;
                    record.2
                }
                None => {
                    next += 1;
                    last.push((entry.key, v, next));
                    next
                }
            };
            runs[u].push((entry.key, run));
        }
    }
    let mut result = InfiniteRuns {
        parts: Vec::new(),
        gaps: Vec::new(),
    };
    for (item, runs) in items.into_iter().zip(runs) {
        match item.part {
            Some(part) => result.parts.push((part, runs)),
            None => result.gaps.push((item.gap, runs)),
        }
    }
    Some(result)
}

/// The position within a text piece of the logical caret before its byte
/// `offset`.
fn caret_x(piece: &Piece, offset: usize) -> f32 {
    let Some(shaped) = piece.shaped.as_ref() else {
        return piece.box_width * offset as f32 / piece.item.text.len().max(1) as f32;
    };
    let start = |cluster: &crate::text::Cluster| {
        if cluster.rtl {
            cluster.x + cluster.advance
        } else {
            cluster.x
        }
    };
    if let Some(cluster) = shaped
        .clusters
        .iter()
        .find(|cluster| cluster.text_range.contains(&offset))
    {
        return start(cluster);
    }
    shaped
        .clusters
        .iter()
        .find(|cluster| cluster.text_range.end == offset)
        .map_or(piece.box_width, |cluster| {
            if cluster.rtl {
                cluster.x
            } else {
                cluster.x + cluster.advance
            }
        })
}

/// UAX #9 L1 on one line's `levels` (indexed like `text` and the original
/// `classes`): segment and paragraph separators, and whitespace and isolate
/// controls before them or at the end of the line, take the paragraph
/// level. Retained explicit controls and BN take the preceding level, and
/// count with the whitespace (UAX #9 §5.2), as the `unicode-bidi` crate's
/// own line reordering does.
fn apply_l1(classes: &[BidiClass], levels: &mut [Level], text: &str, para: Level) {
    use BidiClass::*;
    let mut reset_from: Option<usize> = Some(0);
    let mut previous = para;
    for (index, character) in text.char_indices() {
        let end = index + character.len_utf8();
        match classes[index] {
            B | S => {
                let from = reset_from.take().unwrap_or(index);
                levels[from..end].fill(para);
            }
            WS | FSI | LRI | RLI | PDI => {
                reset_from.get_or_insert(index);
            }
            RLE | LRE | RLO | LRO | PDF | BN => {
                reset_from.get_or_insert(index);
                levels[index..end].fill(previous);
            }
            _ => reset_from = None,
        }
        previous = levels[index];
    }
    if let Some(from) = reset_from {
        levels[from..].fill(para);
    }
}

/// Reshape a text piece, keeping its baseline where it was.
fn reshape(piece: &mut Piece, shaped: crate::text::ShapedText) {
    let baseline = piece.y + piece.ascent;
    piece.box_width = shaped.advance;
    piece.paint_width = shaped.advance;
    piece.box_height = shaped.line_height;
    piece.paint_height = shaped.line_height;
    piece.ascent = shaped.baseline;
    piece.descent = shaped.line_height - shaped.baseline;
    piece.y = baseline - piece.ascent;
    piece.shaped = Some(shaped);
}

/// Give a level-uniform text piece its level's direction (L2/L4), unless
/// its shaping already reads that way. Returns whether it was reshaped.
fn direct(piece: &mut Piece, level: Level, cells: bool) -> bool {
    piece.bidi_level = level.number();
    let rtl = level.is_rtl();
    let Some(style) = piece.text_style.clone() else {
        return false;
    };
    let reads = piece.shaped.as_ref().is_some_and(|shaped| {
        if cells {
            !rtl || shaped.clusters.iter().all(|cluster| cluster.rtl)
        } else {
            shaped.runs.iter().all(|run| run.rtl == rtl)
        }
    });
    if reads {
        return false;
    }
    let shaped = crate::text::shape_directed(&piece.item.text, &style, rtl);
    reshape(piece, shaped);
    true
}

/// The level-uniform parts of a text piece: byte ranges and their levels.
fn level_runs(text: &str, levels: &[Level]) -> Vec<(Range<usize>, Level)> {
    let mut runs = Vec::new();
    let mut start = 0;
    for (index, _) in text.char_indices().skip(1) {
        if levels[index] != levels[start] {
            runs.push((start..index, levels[start]));
            start = index;
        }
    }
    if !text.is_empty() {
        runs.push((start..text.len(), levels[start]));
    }
    runs
}

fn keys_in(bounds: &[BoxBound], key: InlineBoxKey) -> bool {
    bounds.iter().any(|bound| bound.key == key)
}

/// A box on the line being reordered: its visual extent and whether it
/// begins and ends here.
struct LineBox {
    entry: InlineBoxEntry,
    first: usize,
    last: usize,
    starts: bool,
    ends: bool,
}

impl LineBox {
    /// CSS Writing Modes 4 #bidi-box-model / CSS 2 §8.6: the start edge
    /// goes to the start-most fragment on the box's first line and the end
    /// edge to the end-most on its last; for a right-to-left box the start
    /// is its right.
    fn left_edge(&self) -> bool {
        if self.entry.rtl {
            self.ends
        } else {
            self.starts
        }
    }

    fn right_edge(&self) -> bool {
        if self.entry.rtl {
            self.starts
        } else {
            self.ends
        }
    }
}

struct OpenFragment {
    index: usize,
    left: f32,
    has_left: bool,
    first: Option<Slot>,
    last: Option<Slot>,
    /// The box's fragment on the infinitely long line, once known.
    run: Option<u32>,
}

struct Fragment {
    key: InlineBoxKey,
    left: f32,
    right: f32,
    has_left: bool,
    has_right: bool,
    first: Option<Slot>,
    last: Option<Slot>,
}

/// What reordering one line needs from its formatting context.
struct LineInput<'a> {
    text: &'a str,
    info: &'a BidiInfo<'a>,
    atom_edges: &'a [(u32, [f32; 2])],
    /// This line's text indent.
    indent: f32,
    /// The line box's inline base direction (CSS Text 3 #bidi-linebox).
    rtl: bool,
    /// The alignment offset to lay it at.
    offset: f32,
    cells: bool,
    /// The paragraph's boxes' fragments on an infinitely long line.
    infinite: Option<&'a InfiniteRuns>,
}

impl Ifc<'_, '_, '_> {
    /// Keep a bidi paragraph's text for this block container's content
    /// (`node`, NO_NODE for an anonymous one) so its lines can be
    /// reordered. Call after `set_rtl` and before any content.
    pub fn enable_bidi(&mut self, node: NodeId) {
        if self.measuring || self.sideways {
            return;
        }
        let unicode_bidi = if node == crate::layout2::NO_NODE {
            UnicodeBidi::Normal
        } else {
            BoxStyle::of(self.dom, node, self.vp).unicode_bidi
        };
        // CSS Text 3 #bidi-linebox: a plaintext block's line boxes take
        // their paragraph's direction, which moves start/end alignment.
        let flip_align = if unicode_bidi == UnicodeBidi::Plaintext {
            let keyword = |property: &str| {
                self.dom
                    .computed_value_resolved(node, property)
                    .map(|value| value.trim().to_ascii_lowercase())
                    .unwrap_or_default()
            };
            let align = keyword("text-align");
            let relative = matches!(align.as_str(), "start" | "end" | "");
            let last = match keyword("text-align-last").as_str() {
                "start" | "end" => true,
                "auto" | "" => relative || align == "justify",
                _ => false,
            };
            [relative, last]
        } else {
            [false; 2]
        };
        self.bidi = Some(Box::new(BidiState::new(self.rtl, unicode_bidi, flip_align)));
    }

    /// End the current bidi paragraph, at a forced paragraph break or the
    /// end of the content, and reorder its lines.
    pub(super) fn end_bidi_paragraph(&mut self) {
        let Some(mut state) = self.bidi.take() else {
            return;
        };
        let lines = state.first_line..self.lines.len();
        state.first_line = self.lines.len();
        if state.needs && !lines.is_empty() && !state.text.is_empty() {
            self.reorder_paragraph(&mut state, lines);
        }
        state.begin_paragraph();
        self.bidi = Some(state);
    }

    fn reorder_paragraph(&mut self, state: &mut BidiState, lines: Range<usize>) {
        let text = state.text.as_str();
        // HL1 (#bidi-para-direction): the block's direction sets the
        // paragraph level, except that `plaintext` uses P2/P3.
        let base = (!state.plaintext).then(|| {
            if state.block_rtl {
                Level::rtl()
            } else {
                Level::ltr()
            }
        });
        let info = BidiInfo::new(text, base);
        let infinite = infinite_runs(text, &info, &self.lines[lines.clone()]);
        let last_line = self.lines.len() - 1;
        for index in lines {
            let line = &self.lines[index];
            let indent = if index == 0 { self.indent } else { 0.0 };
            let start = line
                .pieces
                .iter()
                .chain(&line.atom_boxes)
                .map(|piece| piece.bidi_pos)
                .min();
            let Some(start) = start.filter(|&start| start != u32::MAX) else {
                continue;
            };
            // CSS Text 3 #bidi-linebox: a plaintext block's line box takes
            // the direction of its paragraph, if strong, else the previous
            // line box's.
            let rtl = if state.plaintext {
                let para = info
                    .paragraphs
                    .iter()
                    .find(|para| para.range.contains(&(start as usize)))
                    .map_or(0..text.len(), |para| para.range.clone());
                match unicode_bidi::get_base_direction(&text[para]) {
                    unicode_bidi::Direction::Ltr => false,
                    unicode_bidi::Direction::Rtl => true,
                    unicode_bidi::Direction::Mixed => state.line_rtl,
                }
            } else {
                state.block_rtl
            };
            state.line_rtl = rtl;
            let offset = if rtl == state.block_rtl {
                line.alignment_offset
            } else {
                let last = line.forced || (self.finishing && index == last_line);
                let align = if last { self.align_last } else { self.align };
                let align = match align {
                    Align2::Left if state.flip_align[usize::from(last)] => Align2::Right,
                    Align2::Right if state.flip_align[usize::from(last)] => Align2::Left,
                    other => other,
                };
                let free = line.right - line.left - line.width;
                if free < 0.0 {
                    if rtl { free } else { 0.0 }
                } else {
                    match align {
                        Align2::Center => free / 2.0,
                        Align2::Right => free,
                        Align2::Left | Align2::Justify => 0.0,
                    }
                }
            };
            let input = LineInput {
                text,
                info: &info,
                atom_edges: &state.atom_edges,
                indent,
                rtl,
                offset,
                cells: self.cells.is_some(),
                infinite: infinite.as_ref(),
            };
            let line = &self.lines[index];
            let inserts: Vec<&Insert> = state
                .inserts
                .iter()
                .filter(|insert| match insert.kind {
                    InsertKind::Static(mark) => self
                        .oofs
                        .get(mark)
                        .is_some_and(|mark| mark.line == index && mark.aligned),
                    InsertKind::Empty(key) => {
                        line.empty_boxes.iter().any(|empty| empty.entry.key == key)
                    }
                })
                .collect();
            let Some(statics) = reorder_line(&mut self.lines[index], &input, &inserts) else {
                continue;
            };
            let line = &self.lines[index];
            for atom in line.atom_boxes.iter().filter(|atom| atom.atom_box) {
                if let Some(place) = self
                    .atom_places
                    .iter_mut()
                    .find(|place| place.line == index && place.node == atom.item.node)
                {
                    place.x = atom.x;
                }
            }
            for (mark, x) in statics {
                self.oofs[mark].x_px = x;
            }
        }
    }
}

/// Reorder one line of a resolved paragraph (UAX #9 L1, L2; CSS Writing
/// Modes 4 #bidi-box-model, #bidi-fragmentation), and place the line's
/// `inserts` at their logical insertion points in it. Returns the static
/// positions found (`Ifc::oofs` index, x); `None` leaves the line as laid.
fn reorder_line(
    line: &mut LineOut,
    input: &LineInput<'_>,
    inserts: &[&Insert],
) -> Option<Vec<(usize, f32)>> {
    let text = input.text;
    // The line's participants in logical order.
    let mut parts: Vec<(usize, Slot)> = line
        .pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| (piece.bidi_pos, Slot::Piece(index)))
        .chain(
            line.atom_boxes
                .iter()
                .enumerate()
                .map(|(index, piece)| (piece.bidi_pos, Slot::Atom(index))),
        )
        .map(|(at, slot)| (at as usize, slot))
        .collect();
    if parts.is_empty() {
        return None;
    }
    parts.sort_by_key(|&(at, _)| at);
    let mut ranges = Vec::with_capacity(parts.len());
    for &(_, slot) in &parts {
        ranges.push(piece_range(text, piece(line, slot))?);
    }
    if ranges.windows(2).any(|pair| pair[1].start < pair[0].end) {
        return None;
    }
    let n = parts.len();
    let lo = ranges[0].start;
    let hi = ranges.iter().map(|range| range.end).max().unwrap_or(lo);
    let para = input
        .info
        .paragraphs
        .iter()
        .find(|para| para.range.contains(&lo))?;
    let mut levels = input.info.levels[lo..hi].to_vec();
    apply_l1(
        &input.info.original_classes[lo..hi],
        &mut levels,
        &text[lo..hi],
        para.level,
    );
    let level_at = |at: usize| levels[at - lo];

    // The logical line: where it began (after its indent) and ended.
    let start_x = line.left + input.indent + line.alignment_offset;
    let end_x = line.left + line.width + line.alignment_offset;
    let chain_of = |piece: &Piece| piece.boxes.as_ref().map(|boxes| boxes.chain.clone());
    let atom_edges = |at: usize| {
        input
            .atom_edges
            .iter()
            .find(|&&(pos, _)| pos as usize == at)
            .map_or([0.0; 2], |&(_, edges)| edges)
    };

    // The gap before each participant in the laid line, less the inline box
    // edges and atomic margins the visual walk lays again: the collapsed
    // space, tab advance or other space there.
    struct Lead {
        width: f32,
        level: Level,
        depth: usize,
        space: bool,
        separator: Option<usize>,
    }
    let mut leads = Vec::with_capacity(n);
    let mut prev_end = start_x;
    let mut prev_atom_right = 0.0;
    for (i, &(at, slot)) in parts.iter().enumerate() {
        let current = piece(line, slot);
        let chain = chain_of(current);
        let opens: &[BoxBound] = current.boxes.as_ref().map_or(&[], |boxes| &boxes.opens);
        let (depth, close_width) = if i == 0 {
            let depth = chain.as_deref().map_or(0, |chain| {
                chain
                    .iter()
                    .take_while(|entry| !keys_in(opens, entry.key))
                    .count()
            });
            (depth, 0.0)
        } else {
            let previous = piece(line, parts[i - 1].1);
            let previous_chain = chain_of(previous);
            let depth = common(previous_chain.as_ref(), chain.as_ref());
            let closed = previous.boxes.as_ref().map_or(0.0, |boxes| {
                previous_chain.as_deref().map_or(0.0, |previous_chain| {
                    previous_chain[depth..]
                        .iter()
                        .filter(|entry| keys_in(&boxes.closes, entry.key))
                        .map(InlineBoxEntry::end_edge)
                        .sum()
                })
            });
            (depth, closed)
        };
        let open_width: f32 = chain.as_deref().map_or(0.0, |chain| {
            chain[depth.min(chain.len())..]
                .iter()
                .filter(|entry| keys_in(opens, entry.key))
                .map(InlineBoxEntry::start_edge)
                .sum()
        });
        let atom = if current.is_text() {
            [0.0; 2]
        } else {
            atom_edges(at)
        };
        // The level of the space or tab before the piece; a gap without
        // one goes with the piece.
        let separator = (i > 0)
            .then(|| separator(text, input.info, ranges[i - 1].end, at))
            .flatten();
        leads.push(Lead {
            width: current.x - prev_end - close_width - open_width - atom[0] - prev_atom_right,
            level: separator.map_or_else(|| level_at(at), level_at),
            depth,
            space: current.space_before,
            separator,
        });
        prev_end = current.x + current.box_width;
        prev_atom_right = atom[1];
    }
    let last_piece = piece(line, parts[n - 1].1);
    let last_chain = chain_of(last_piece);
    let (trail_depth, trail_close) = match (&last_piece.boxes, last_chain.as_deref()) {
        (Some(boxes), Some(chain)) => (
            chain
                .iter()
                .take_while(|entry| !keys_in(&boxes.closes, entry.key))
                .count(),
            chain
                .iter()
                .filter(|entry| keys_in(&boxes.closes, entry.key))
                .map(InlineBoxEntry::end_edge)
                .sum::<f32>(),
        ),
        _ => (0, 0.0),
    };
    let mut trail = end_x - prev_end - trail_close - prev_atom_right;

    // Each insertion point lies inside a piece's text, or in the gap before
    // a participant (`n`: after the last). An empty box's margin box is part
    // of the gap it lies in; it becomes a unit of its own.
    let mut in_piece: Vec<(usize, usize)> = Vec::new();
    let mut in_gap: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
    for (k, insert) in inserts.iter().enumerate() {
        let at = insert.at as usize;
        match ranges
            .iter()
            .position(|range| range.start < at && at < range.end)
        {
            Some(i) => in_piece.push((k, i)),
            None => {
                let gap = ranges
                    .iter()
                    .position(|range| at <= range.start)
                    .unwrap_or(n);
                in_gap[gap].push(k);
            }
        }
    }
    let empty_of = |line: &LineOut, k: usize| match inserts[k].kind {
        InsertKind::Empty(key) => line
            .empty_boxes
            .iter()
            .position(|empty| empty.entry.key == key),
        InsertKind::Static(_) => None,
    };
    let mut insert_width = vec![0.0f32; inserts.len()];
    for (gap, list) in in_gap.iter_mut().enumerate() {
        list.sort_by_key(|&k| inserts[k].at);
        for &k in list.iter() {
            let Some(empty) = empty_of(line, k) else {
                continue;
            };
            let width: f32 = line.empty_boxes[empty].entry.edges.iter().sum();
            let room = if gap < n {
                &mut leads[gap].width
            } else {
                &mut trail
            };
            if *room >= width - 0.01 {
                *room -= width;
                insert_width[k] = width;
            }
        }
    }

    // L2 needs level-uniform pieces: split text where its level changes,
    // and shape each part in its level's direction.
    let mut changed = false;
    let mut segments: Vec<Vec<Slot>> = Vec::with_capacity(n);
    let mut pieces: Vec<Piece> = Vec::with_capacity(line.pieces.len());
    let mut renumber = vec![0usize; line.pieces.len()];
    let mut split: Vec<Vec<(Range<usize>, Level)>> = vec![Vec::new(); line.pieces.len()];
    for (i, &(at, slot)) in parts.iter().enumerate() {
        if let Slot::Piece(index) = slot
            && line.pieces[index].is_text()
        {
            split[index] = level_runs(
                &line.pieces[index].item.text,
                &levels[at - lo..ranges[i].end - lo],
            );
        }
    }
    for (index, mut piece) in std::mem::take(&mut line.pieces).into_iter().enumerate() {
        renumber[index] = pieces.len();
        let runs = std::mem::take(&mut split[index]);
        if runs.len() <= 1 {
            if let Some((_, level)) = runs.first() {
                changed |= direct(&mut piece, *level, input.cells);
            }
            pieces.push(piece);
            continue;
        }
        changed = true;
        piece.shaped = None;
        let count = runs.len();
        for (n, (range, level)) in runs.into_iter().enumerate() {
            let mut part = piece.clone();
            part.item.text = piece.item.text[range.clone()].to_owned();
            part.sources = crate::render::selection::slice_sources(
                &piece.item.text,
                &piece.sources,
                range.clone(),
            );
            part.bidi_pos = piece.bidi_pos + range.start as u32;
            if n > 0 {
                part.space_before = false;
            }
            if let Some(boxes) = part.boxes.as_mut() {
                if n > 0 {
                    boxes.opens.clear();
                }
                if n + 1 < count {
                    boxes.closes.clear();
                }
            }
            part.bidi_level = level.number();
            let style = part.text_style.clone().unwrap_or_default();
            let shaped = crate::text::shape_directed(&part.item.text, &style, level.is_rtl());
            reshape(&mut part, shaped);
            pieces.push(part);
        }
    }
    let piece_count = pieces.len();
    line.pieces = pieces;
    for &(at, slot) in &parts {
        segments.push(match slot {
            Slot::Piece(index) => {
                let first = renumber[index];
                let end = renumber.get(index + 1).copied().unwrap_or(piece_count);
                // A replaced element's level is its object replacement
                // character's.
                if !line.pieces[first].is_text() {
                    line.pieces[first].bidi_level = level_at(at).number();
                }
                (first..end).map(Slot::Piece).collect()
            }
            Slot::Atom(index) => {
                line.atom_boxes[index].bidi_level = level_at(at).number();
                vec![Slot::Atom(index)]
            }
        });
    }
    let level_of = |line: &LineOut, slot: Slot| {
        Level::new(piece(line, slot).bidi_level).unwrap_or_else(|_| Level::ltr())
    };

    // The units in logical order. An insertion point takes the level of the
    // participant it precedes (of the last one, at the end of the line), as
    // a box's start does.
    let insert_unit = |k: usize, level: Level| Unit {
        level,
        width: insert_width[k],
        kind: UnitKind::Insert(k),
        chain: inserts[k].chain.clone(),
        depth: inserts[k].depth,
        space: false,
        atom: [0.0; 2],
        runs: None,
    };
    let mut units: Vec<Unit<'_>> = Vec::with_capacity(n * 2 + inserts.len() + 1);
    for (i, (&(at, _), lead)) in parts.iter().zip(&leads).enumerate() {
        let level = level_of(line, segments[i][0]);
        let (before, after): (Vec<usize>, Vec<usize>) = in_gap[i].iter().partition(|&&k| {
            lead.separator
                .is_none_or(|space| inserts[k].at as usize <= space)
        });
        units.extend(before.into_iter().map(|k| insert_unit(k, level)));
        if lead.width != 0.0 || lead.space {
            units.push(Unit {
                level: lead.level,
                width: lead.width,
                kind: UnitKind::Gap,
                chain: chain_of(piece(line, segments[i][0])),
                depth: lead.depth,
                space: lead.space,
                atom: [0.0; 2],
                runs: input.infinite.and_then(|runs| runs.gap(at)),
            });
        }
        units.extend(after.into_iter().map(|k| insert_unit(k, level)));
        for &slot in &segments[i] {
            let piece = piece(line, slot);
            let atom = if piece.is_text() {
                [0.0; 2]
            } else {
                atom_edges(at)
            };
            let chain = chain_of(piece);
            units.push(Unit {
                level: level_of(line, slot),
                width: piece.box_width,
                kind: UnitKind::Piece(slot),
                depth: chain.as_deref().map_or(0, <[InlineBoxEntry]>::len),
                chain,
                space: false,
                atom,
                runs: input
                    .infinite
                    .and_then(|runs| runs.part(piece.bidi_pos as usize)),
            });
        }
    }
    let end_level = level_of(line, *segments[n - 1].last().expect("a segment"));
    units.extend(in_gap[n].iter().map(|&k| insert_unit(k, end_level)));
    if trail != 0.0 {
        units.push(Unit {
            level: para.level,
            width: trail,
            kind: UnitKind::Gap,
            chain: last_chain,
            depth: trail_depth,
            space: false,
            atom: [0.0; 2],
            runs: None,
        });
    }
    let uniform = units
        .iter()
        .all(|unit| unit.level == units[0].level && unit.level.is_ltr());
    let rtl_boxes = units
        .iter()
        .any(|unit| unit.boxes().iter().any(|entry| entry.rtl));
    if uniform && !rtl_boxes && !input.rtl && !changed && input.offset == line.alignment_offset {
        return None;
    }

    // L2: the units in visual order (visual index → logical index).
    let unit_levels: Vec<Level> = units.iter().map(|unit| unit.level).collect();
    let order = BidiInfo::reorder_visual(&unit_levels);
    let mut boxes: Vec<LineBox> = Vec::new();
    for (v, &u) in order.iter().enumerate() {
        for entry in units[u].boxes() {
            match boxes.iter_mut().find(|other| other.entry.key == entry.key) {
                Some(other) => {
                    other.first = other.first.min(v);
                    other.last = other.last.max(v);
                }
                None => boxes.push(LineBox {
                    entry: *entry,
                    first: v,
                    last: v,
                    starts: false,
                    ends: false,
                }),
            }
        }
    }
    for unit in &units {
        let UnitKind::Piece(slot) = unit.kind else {
            continue;
        };
        let Some(bounds) = piece(line, slot).boxes.as_ref() else {
            continue;
        };
        for item in &mut boxes {
            item.starts |= keys_in(&bounds.opens, item.entry.key);
            item.ends |= keys_in(&bounds.closes, item.entry.key);
        }
    }

    // Lay the units left to right. A box's fragment opens where a unit
    // inside it follows one outside it, and closes where one outside it
    // follows, or one of another of its fragments on the infinitely long
    // line (#bidi-fragmentation).
    let mut x = 0.0f32;
    let mut open: Vec<OpenFragment> = Vec::new();
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut space = false;
    let mut placed: Vec<Option<f32>> = vec![None; inserts.len()];
    for v in 0..=order.len() {
        let unit = order.get(v).map(|&u| &units[u]);
        let entries: &[InlineBoxEntry] = unit.map_or(&[], Unit::boxes);
        let keep = open
            .iter()
            .zip(entries)
            .take_while(|(fragment, entry)| {
                boxes[fragment.index].entry.key == entry.key
                    && match (fragment.run, unit.and_then(|unit| unit.run(entry.key))) {
                        (Some(run), Some(other)) => run == other,
                        _ => true,
                    }
            })
            .count();
        while open.len() > keep {
            let fragment = open.pop().expect("open fragment");
            let item = &boxes[fragment.index];
            let has_right = item.right_edge() && item.last + 1 == v;
            let right = if has_right {
                x += item.entry.edges[2];
                let right = x;
                x += item.entry.edges[3];
                right
            } else {
                x
            };
            fragments.push(Fragment {
                key: item.entry.key,
                left: fragment.left,
                right,
                has_left: fragment.has_left,
                has_right,
                first: fragment.first,
                last: fragment.last,
            });
        }
        let Some(unit) = unit else {
            break;
        };
        for (fragment, entry) in open.iter_mut().zip(entries) {
            if let Some(run) = unit.run(entry.key) {
                fragment.run = Some(run);
            }
        }
        for entry in &entries[keep..] {
            let Some(index) = boxes.iter().position(|item| item.entry.key == entry.key) else {
                continue;
            };
            let item = &boxes[index];
            let has_left = item.left_edge() && item.first == v;
            let left = if has_left {
                x += item.entry.edges[0];
                let left = x;
                x += item.entry.edges[1];
                left
            } else {
                x
            };
            open.push(OpenFragment {
                index,
                left,
                has_left,
                first: None,
                last: None,
                run: unit.run(entry.key),
            });
        }
        match unit.kind {
            UnitKind::Piece(slot) => {
                x += unit.atom[0];
                let piece = piece_mut(line, slot);
                piece.x = x;
                piece.space_before = space;
                space = false;
                x += unit.width + unit.atom[1];
                for fragment in &mut open {
                    fragment.first.get_or_insert(slot);
                    fragment.last = Some(slot);
                }
            }
            UnitKind::Gap => {
                x += unit.width;
                space |= unit.space;
            }
            UnitKind::Insert(k) => {
                placed[k] = Some(x);
                x += unit.width;
            }
        }
    }

    // The visual line keeps the laid line's start: its indent at the start
    // edge (CSS Text 3 #text-indent-property), which is the right edge of a
    // right-to-left line box.
    let laid_width = end_x - start_x;
    let origin = if input.rtl {
        line.left + input.offset + laid_width - x
    } else {
        line.left + input.offset + input.indent
    };
    for unit in &units {
        if let UnitKind::Piece(slot) = unit.kind {
            let piece = piece_mut(line, slot);
            piece.x += origin;
            if let Some(boxes) = piece.boxes.as_mut() {
                boxes.opens.clear();
                boxes.closes.clear();
            }
        }
    }
    for fragment in fragments {
        let (Some(first), Some(last)) = (fragment.first, fragment.last) else {
            continue;
        };
        let piece = piece_mut(line, first);
        let distance = piece.x - (fragment.left + origin);
        if let Some(boxes) = piece.boxes.as_mut() {
            boxes.opens.push(BoxBound {
                key: fragment.key,
                distance,
                edge: fragment.has_left,
            });
        }
        let piece = piece_mut(line, last);
        let distance = fragment.right + origin - (piece.x + piece.box_width);
        if let Some(boxes) = piece.boxes.as_mut() {
            boxes.closes.push(BoxBound {
                key: fragment.key,
                distance,
                edge: fragment.has_right,
            });
        }
    }
    line.alignment_offset = input.offset;

    // The insertion points: inside a piece at the caret before their byte,
    // otherwise where their unit was laid.
    for &(k, i) in &in_piece {
        let at = inserts[k].at as usize;
        placed[k] = segments[i].iter().find_map(|&slot| {
            let piece = piece(line, slot);
            let start = piece.bidi_pos as usize;
            (start <= at && at < start + piece.item.text.len())
                .then(|| piece.x + caret_x(piece, at - start) - origin)
        });
    }
    let mut statics = Vec::new();
    for (k, insert) in inserts.iter().enumerate() {
        let Some(x) = placed[k].map(|x| x + origin) else {
            continue;
        };
        match insert.kind {
            InsertKind::Static(mark) => {
                // CSS 2 §9.4.3: with the enclosing box's relative offset.
                let offset = insert.chain.as_deref().map_or(0.0, |chain| {
                    chain[..insert.depth.min(chain.len())]
                        .last()
                        .map_or(0.0, |entry| entry.offset[0])
                });
                statics.push((mark, x + offset));
            }
            InsertKind::Empty(_) => {
                let Some(empty) = empty_of(line, k) else {
                    continue;
                };
                let empty = &mut line.empty_boxes[empty];
                let [left, left_bp, right_bp, _] = empty.entry.edges;
                empty.x0 = x + left;
                empty.x1 = empty.x0 + left_bp + right_bp;
            }
        }
    }
    Some(statics)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use url::Url;

    use crate::dom::{Dom, NodeId};
    use crate::layout2::flow::{Frag, FragKind, LineFrag};
    use crate::layout2::inline::{Piece, line_box_fragments};
    use crate::layout2::{TerminalViewport, Viewport};

    /// The laid line boxes of `html` at `width` CSS px, with their
    /// fragments' x.
    fn lines(html: &str, width: f32) -> (Dom, Vec<(f32, Arc<LineFrag>)>) {
        let dom = Dom::parse_document(html);
        let base = Url::parse("http://e.com/").unwrap();
        fn collect(frag: &Frag, out: &mut Vec<(f32, Arc<LineFrag>)>) {
            if let FragKind::Line(line) = &frag.kind {
                out.push((frag.x, line.clone()));
            }
            for child in &frag.children {
                collect(child, out);
            }
        }
        let lines = crate::layout2::session::with_layout(
            &dom,
            &base,
            Viewport::new(width, 600.0),
            &[],
            &Default::default(),
            &Default::default(),
            |layout| {
                let mut out = Vec::new();
                collect(&layout.expect("layout").root, &mut out);
                out
            },
        );
        (dom, lines)
    }

    fn id(dom: &Dom, id: &str) -> NodeId {
        dom.get_by_id(id).unwrap()
    }

    /// The pieces of `line` holding `text`.
    fn find<'l>(line: &'l LineFrag, text: &str) -> &'l Piece {
        line.pieces
            .iter()
            .find(|piece| piece.item.text.trim() == text)
            .unwrap_or_else(|| {
                let texts: Vec<_> = line.pieces.iter().map(|p| &p.item.text).collect();
                panic!("no piece {text:?} in {texts:?}")
            })
    }

    /// The pieces' trimmed texts, left to right.
    fn visual(line: &LineFrag) -> Vec<String> {
        let mut pieces: Vec<&Piece> = line.pieces.iter().collect();
        pieces.sort_by(|a, b| a.x.total_cmp(&b.x));
        pieces
            .iter()
            .map(|piece| piece.item.text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .collect()
    }

    const BODY: &str = "<body style='margin:0;font:16px sans-serif'>";

    /// UAX #9 L2 over a left-to-right paragraph: Hebrew words keep their
    /// place among Latin ones, two of them separated by a collapsed space
    /// reverse as one run, and the line's pieces stay in logical order.
    #[test]
    fn hebrew_run_reverses_inside_a_left_to_right_paragraph() {
        let (_, lines) = lines(
            &format!("{BODY}<p>abc <b>אבג</b> <i>דהו</i> xyz</p>"),
            600.0,
        );
        let line = &lines[0].1;
        assert_eq!(visual(line), ["abc", "דהו", "אבג", "xyz"]);
        let order: Vec<_> = line.pieces.iter().map(|p| p.item.text.trim()).collect();
        assert_eq!(order, ["abc", "אבג", "דהו", "xyz"], "pieces stay logical");
        assert_eq!(find(line, "אבג").bidi_level, 1);
        assert_eq!(find(line, "abc").bidi_level, 0);
        assert!(
            find(line, "אבג")
                .shaped
                .as_ref()
                .unwrap()
                .runs
                .iter()
                .all(|r| r.rtl)
        );
    }

    /// HL1: a right-to-left block's paragraph has level 1; Latin text in it
    /// is a level-2 run, and the merged piece splits at the level changes.
    #[test]
    fn right_to_left_paragraph_orders_runs_from_the_right() {
        let (_, lines) = lines(&format!("{BODY}<p dir=rtl>אבג abc def דהו</p>"), 600.0);
        let line = &lines[0].1;
        assert_eq!(visual(line), ["דהו", "abc def", "אבג"]);
        let levels: Vec<_> = line.pieces.iter().map(|p| p.bidi_level).collect();
        assert_eq!(levels, [1, 2, 1]);
        // Start-aligned at the right edge.
        let right = line
            .pieces
            .iter()
            .map(|p| p.x + p.box_width)
            .fold(0.0f32, f32::max);
        assert!((right - 600.0).abs() < 0.5, "{right}");
    }

    /// N1/N2 across the collapsed space between two pieces: between two
    /// right-to-left runs it is right-to-left, so the run reverses whole;
    /// between right-to-left and left-to-right text it takes the paragraph
    /// level.
    #[test]
    fn collapsed_spaces_take_part_in_neutral_resolution() {
        let (_, lines) = lines(
            &format!("{BODY}<p><b>אבג</b> <i>דהו</i></p><p><b>אבג</b> <i>abc</i></p>"),
            600.0,
        );
        let (first, second) = (&lines[0].1, &lines[1].1);
        assert_eq!(visual(first), ["דהו", "אבג"]);
        let (left, right) = (find(first, "דהו"), find(first, "אבג"));
        let space = crate::text::shape(" ", &crate::text::TextStyle::default()).advance;
        assert!((right.x - (left.x + left.box_width) - space).abs() < 0.5);
        assert!(
            right.space_before,
            "the space is visually before the right piece"
        );
        assert_eq!(visual(second), ["אבג", "abc"]);
        assert!(find(second, "abc").space_before);
    }

    /// HTML #bidi-rendering: `bdi` isolates its content, so a following
    /// number is not drawn into the right-to-left run; a plain span is not.
    #[test]
    fn bdi_isolates_its_content_from_following_numbers() {
        let (_, lines) = lines(
            &format!(
                "{BODY}<p>user <span>אבג</span> 123</p><p>user <bdi>אבג</bdi> 123</p>\
                 <p>user <span dir=auto>אבג</span> 123</p>"
            ),
            600.0,
        );
        assert_eq!(visual(&lines[0].1), ["user", "123", "אבג"]);
        assert_eq!(visual(&lines[1].1), ["user", "אבג", "123"]);
        assert_eq!(visual(&lines[2].1), ["user", "אבג", "123"]);
    }

    /// `bdo dir=rtl` overrides: Latin letters read right to left.
    #[test]
    fn bdo_overrides_the_direction_of_its_text() {
        let (_, lines) = lines(&format!("{BODY}<p>x<bdo dir=rtl>abc</bdo>y</p>"), 600.0);
        let line = &lines[0].1;
        let piece = find(line, "abc");
        assert_eq!(piece.bidi_level % 2, 1);
        let shaped = piece.shaped.as_ref().unwrap();
        let x = |byte: usize| {
            shaped
                .clusters
                .iter()
                .find(|cluster| cluster.text_range.start == byte)
                .unwrap()
                .x
        };
        assert!(x(0) > x(1) && x(1) > x(2), "{:?}", shaped.clusters);
        assert_eq!(visual(line), ["x", "abc", "y"]);
    }

    /// The #bidi-control-codes-injection-table footnote: `bidi-override` on
    /// a block container overrides its root inline box, so every line reads
    /// in the block's direction; `plaintext` takes each paragraph's base
    /// direction from its first strong character (#bidi-para-direction), and
    /// its line boxes follow it (CSS Text 3 #bidi-linebox).
    #[test]
    fn block_overrides_and_plaintext_paragraphs() {
        let (_, lines) = lines(
            &format!(
                "{BODY}<p style='unicode-bidi:bidi-override;direction:rtl'>abc def</p>\
                 <pre dir=auto>abc אבג\nאבג abc</pre>"
            ),
            600.0,
        );
        let line = &lines[0].1;
        let piece = find(line, "abc def");
        // X4: RLO raises the paragraph's level 1 to the next odd level.
        assert_eq!(piece.bidi_level, 3);
        let shaped = piece.shaped.as_ref().unwrap();
        assert!(shaped.runs.iter().all(|run| run.rtl));
        let first = shaped
            .clusters
            .iter()
            .find(|c| c.text_range.start == 0)
            .unwrap();
        assert!(first.x > shaped.advance / 2.0, "'a' is drawn at the right");
        // The pre's first paragraph is left to right and starts at the left;
        // the second is right to left and starts at the right.
        let (ltr, rtl) = (&lines[1].1, &lines[2].1);
        assert_eq!(visual(ltr), ["abc", "אבג"]);
        assert!(find(ltr, "abc").x < 1.0);
        assert_eq!(visual(rtl), ["abc", "אבג"]);
        let right = rtl
            .pieces
            .iter()
            .map(|p| p.x + p.box_width)
            .fold(0.0, f32::max);
        assert!((right - 600.0).abs() < 1.0, "{right}");
    }

    /// CSS Writing Modes 4 #unicode-bidi: an embedding's boundary acts as a
    /// strong character to the content after it, an isolate's as a neutral.
    #[test]
    fn embed_and_isolate_differ_towards_surrounding_numbers() {
        let (_, lines) = lines(
            &format!(
                "{BODY}<p>abc <span style='unicode-bidi:embed;direction:rtl'>def</span> 123</p>\
                 <p>abc <span style='unicode-bidi:isolate;direction:rtl'>def</span> 123</p>"
            ),
            600.0,
        );
        assert_eq!(visual(&lines[0].1), ["abc", "123", "def"]);
        assert_eq!(visual(&lines[1].1), ["abc", "def", "123"]);
    }

    /// CSS Writing Modes 4 #bidi-fragmentation / #bidi-box-model, the
    /// spec's example: reordering splits the `em` into two fragments around
    /// the outside text; the left one has its left edge, the right one its
    /// right edge.
    #[test]
    fn reordering_splits_an_inline_box_into_fragments() {
        let (dom, lines) = lines(
            &format!(
                "{BODY}<p>here is <em id=e style='border:2px solid;padding:0 5px'>some אבגדה</em> וזחט.</p>"
            ),
            600.0,
        );
        let em = id(&dom, "e");
        let line = &lines[0].1;
        assert_eq!(visual(line), ["here is", "some", "וזחט", "אבגדה", "."]);
        let fragments: Vec<_> =
            line_box_fragments(line.pieces.iter(), &line.empty_boxes, |entry| {
                entry.key == (em, None)
            });
        assert_eq!(fragments.len(), 2, "{fragments:?}");
        let (left, right) = if fragments[0].x0 < fragments[1].x0 {
            (fragments[0], fragments[1])
        } else {
            (fragments[1], fragments[0])
        };
        assert!(left.left_edge && !left.right_edge);
        assert!(!right.left_edge && right.right_edge);
        let outside = find(line, "וזחט");
        assert!(left.x1 <= outside.x + 0.01 && outside.x + outside.box_width <= right.x0 + 0.01);
        // The fragments hold their edges: 2px border and 5px padding.
        let some = find(line, "some");
        assert!((some.x - left.x0 - 7.0).abs() < 0.01);
        let hebrew = find(line, "אבגדה");
        assert!((right.x1 - (hebrew.x + hebrew.box_width) - 7.0).abs() < 0.01);
    }

    /// CSS Writing Modes 4 #bidi-fragmentation, the spec's narrow example:
    /// line breaking puts the `em`'s two fragments side by side, but on an
    /// infinitely long line the outside text divides them, so they stay two
    /// fragments with no edges between them. Isolated, the `em` is one.
    #[test]
    fn fragmentation_follows_the_infinitely_long_line() {
        let style = crate::text::TextStyle::default();
        let width = crate::text::shape("here is ", &style).advance
            + crate::text::shape("some אבגדה", &style).advance
            + 14.0
            + 6.0;
        let html = |em: &str| {
            format!(
                "{BODY}<p style='width:{width}px'>here is <em id=e {em}style='border:2px solid;\
                 padding:0 5px'>some אבגדה</em> וזחט.</p>"
            )
        };
        let (dom, laid) = lines(&html(""), 600.0);
        assert_eq!(laid.len(), 2, "the outside text wraps");
        let em = id(&dom, "e");
        let line = &laid[0].1;
        assert_eq!(visual(line), ["here is", "some", "אבגדה"]);
        let mut fragments = line_box_fragments(line.pieces.iter(), &line.empty_boxes, |entry| {
            entry.key == (em, None)
        });
        fragments.sort_by(|a, b| a.x0.total_cmp(&b.x0));
        assert_eq!(fragments.len(), 2, "{fragments:?}");
        let (left, right) = (fragments[0], fragments[1]);
        assert!(
            (left.x1 - right.x0).abs() < 0.01,
            "adjacent: {left:?} {right:?}"
        );
        assert!(left.left_edge && !left.right_edge);
        assert!(!right.left_edge && right.right_edge);
        // `dir=rtl` isolates the em: nothing outside can divide it.
        let (dom, laid) = lines(&html("dir=rtl "), 600.0);
        let em = id(&dom, "e");
        let fragments = line_box_fragments(laid[0].1.pieces.iter(), &[], |entry| {
            entry.key == (em, None)
        });
        assert_eq!(fragments.len(), 1, "{fragments:?}");
        assert!(fragments[0].left_edge && fragments[0].right_edge);
    }

    /// The x of the fragment generated by element `id`.
    fn frag_x(html: &str, id: &str) -> f32 {
        let dom = Dom::parse_document(html);
        let node = dom.get_by_id(id).unwrap();
        let base = Url::parse("http://e.com/").unwrap();
        fn find(frag: &Frag, node: NodeId) -> Option<f32> {
            if frag.node == node {
                return Some(frag.x);
            }
            frag.children.iter().find_map(|child| find(child, node))
        }
        crate::layout2::session::with_layout(
            &dom,
            &base,
            Viewport::new(600.0, 600.0),
            &[],
            &Default::default(),
            &Default::default(),
            |layout| {
                let layout = layout.expect("layout");
                std::iter::once(&layout.root)
                    .chain(&layout.fixed)
                    .find_map(|frag| find(frag, node))
                    .expect("fragment")
            },
        )
    }

    /// CSS 2 §10.3.7 on a reordered line: a static position is the visual
    /// place of its logical insertion point, here between two right-to-left
    /// words (so at the left edge of the first) or inside one of them.
    #[test]
    fn static_positions_sit_at_their_logical_insertion_point() {
        let html = format!(
            "{BODY}<p>abc <b>אבג</b><span id=g style='position:absolute'>X</span> <i>דהו</i> xyz</p>"
        );
        let (_, laid) = lines(&html, 600.0);
        let (line_x, line) = (laid[0].0, &laid[0].1);
        assert_eq!(visual(line), ["abc", "דהו", "אבג", "xyz"]);
        let first = find(line, "אבג");
        assert!((frag_x(&html, "g") - (line_x + first.x)).abs() < 0.01);
        // Inside a text run: before ד, the caret at the right edge of its
        // right-to-left glyph.
        let html =
            format!("{BODY}<p>abc אבג<span id=m style='position:absolute'>X</span>דהו xyz</p>");
        let (_, laid) = lines(&html, 600.0);
        let (line_x, line) = (laid[0].0, &laid[0].1);
        let word = find(line, "אבגדהו");
        let shaped = word.shaped.as_ref().unwrap();
        let dalet = shaped
            .clusters
            .iter()
            .find(|cluster| cluster.text_range.start == "אבג".len())
            .unwrap();
        assert!(dalet.rtl);
        let expected = line_x + word.x + dalet.x + dalet.advance;
        assert!((frag_x(&html, "m") - expected).abs() < 0.01);
    }

    /// CSS 2 §10.8 on a reordered line: an empty inline box keeps its
    /// margin box at its logical insertion point.
    #[test]
    fn empty_inline_boxes_sit_at_their_logical_insertion_point() {
        let (dom, lines) = lines(
            &format!(
                "{BODY}<p>abc <b>אבג</b><span id=e style='border:2px solid;padding:0 3px'></span> <i>דהו</i></p>"
            ),
            600.0,
        );
        let line = &lines[0].1;
        assert_eq!(visual(line), ["abc", "דהו", "אבג"]);
        let empty = line
            .empty_boxes
            .iter()
            .find(|empty| empty.entry.key == (id(&dom, "e"), None))
            .expect("empty box");
        let first = find(line, "אבג");
        assert!((empty.x1 - first.x).abs() < 0.01, "{empty:?} {}", first.x);
        assert!((empty.x1 - empty.x0 - 10.0).abs() < 0.01);
        let (left, space) = (find(line, "דהו"), find(line, "abc"));
        assert!(left.x + left.box_width < empty.x0);
        assert!(space.x + space.box_width < left.x);
    }

    /// CSS 2 §8.6: in a right-to-left box the start edge is the right one,
    /// on the box's first line.
    #[test]
    fn a_right_to_left_box_starts_at_its_right_edge() {
        let (dom, lines) = lines(
            &format!(
                "{BODY}<p dir=rtl style='width:120px'><span id=s style='border-right:3px solid;\
                 border-left:1px solid'>אבג דהו זחט טיכ למנ</span></p>"
            ),
            600.0,
        );
        let span = id(&dom, "s");
        assert!(lines.len() > 1);
        let fragment = |line: &LineFrag| {
            line_box_fragments(line.pieces.iter(), &line.empty_boxes, |entry| {
                entry.key == (span, None)
            })[0]
        };
        let first = fragment(&lines[0].1);
        assert!(first.right_edge && !first.left_edge, "{first:?}");
        let last = fragment(&lines.last().unwrap().1);
        assert!(last.left_edge && !last.right_edge, "{last:?}");
    }

    /// CSS Writing Modes 4 #bidi-atomic-inlines: an inline-block or a
    /// replaced element is a neutral, so between two right-to-left words it
    /// joins their run; a replaced element with `bidi-override` is a strong
    /// character of its direction instead.
    #[test]
    fn an_atomic_inline_is_a_neutral() {
        let dom_html = format!(
            "{BODY}<p><b>אבג</b> <span id=a style='display:inline-block;width:20px;height:10px'></span> <i>דהו</i></p>\
             <p><b>אבג</b> <img id=img src=a.png width=20 height=10> <i>דהו</i></p>\
             <p><b>אבג</b> <img id=ltr src=a.png width=20 height=10 style='unicode-bidi:bidi-override;direction:ltr'> <i>דהו</i></p>"
        );
        let (dom, lines) = lines(&dom_html, 600.0);
        let line = &lines[0].1;
        assert_eq!(visual(line), ["דהו", "אבג"]);
        let atom = line.atom_boxes.first().expect("inline-block placeholder");
        assert_eq!(atom.item.node, id(&dom, "a"));
        assert_eq!(atom.bidi_level, 1);
        let (left, right) = (find(line, "דהו"), find(line, "אבג"));
        assert!(left.x + left.box_width <= atom.x && atom.x + atom.box_width <= right.x);
        let image = |line: &LineFrag, node: NodeId| {
            line.pieces
                .iter()
                .find(|piece| piece.item.node == node)
                .expect("image piece")
                .clone()
        };
        let line = &lines[1].1;
        let img = image(line, id(&dom, "img"));
        assert_eq!(img.bidi_level, 1);
        let (left, right) = (find(line, "דהו"), find(line, "אבג"));
        assert!(left.x + left.box_width <= img.x && img.x + img.box_width <= right.x);
        // A strong left-to-right image splits the run: each word keeps its
        // logical side.
        let line = &lines[2].1;
        let img = image(line, id(&dom, "ltr"));
        assert_eq!(img.bidi_level, 0);
        let (first, second) = (find(line, "אבג"), find(line, "דהו"));
        assert!(first.x + first.box_width <= img.x && img.x + img.box_width <= second.x);
    }

    /// CSS Text 3 #text-indent-property: the indent is at the start edge,
    /// the right of a right-to-left line.
    #[test]
    fn text_indent_is_at_the_start_edge_of_a_right_to_left_line() {
        let (_, lines) = lines(
            &format!("{BODY}<p dir=rtl style='text-indent:40px'>אבג</p>"),
            600.0,
        );
        let piece = find(&lines[0].1, "אבג");
        assert!(
            (piece.x + piece.box_width - 560.0).abs() < 0.5,
            "{}",
            piece.x
        );
    }

    /// The fast path: left-to-right content is laid exactly as before, every
    /// piece at level 0 in increasing x.
    #[test]
    fn left_to_right_content_is_untouched() {
        let (_, lines) = lines(
            &format!("{BODY}<p>Hello <b>bold</b> and <i>italic</i> text, 42.</p>"),
            600.0,
        );
        let line = &lines[0].1;
        assert!(line.pieces.iter().all(|p| p.bidi_level == 0));
        let xs: Vec<f32> = line.pieces.iter().map(|p| p.x).collect();
        assert!(xs.windows(2).all(|pair| pair[0] < pair[1]), "{xs:?}");
        assert_eq!(
            visual(line),
            ["Hello", "bold", "and", "italic", "text, 42."]
        );
    }

    /// The paragraph text records collapsed spaces, controls and atoms in
    /// logical order, and stays on the fast path without right-to-left
    /// characters.
    #[test]
    fn paragraph_text_needs_resolution_only_for_right_to_left_content() {
        let mut state = super::BidiState::new(false, super::UnicodeBidi::Normal, [false; 2]);
        state.push_text("héllo wörld");
        state.open_box(super::UnicodeBidi::Isolate.controls(false), false);
        assert_eq!(state.push_atom(), "héllo wörld\u{2066}".len() as u32);
        state.close_box(super::UnicodeBidi::Isolate.controls(false), false);
        assert!(!state.needs);
        state.push_text("שלום");
        assert!(state.needs);
        let mut rtl = super::BidiState::new(true, super::UnicodeBidi::BidiOverride, [false; 2]);
        assert!(rtl.needs);
        assert_eq!(rtl.text, "\u{202E}");
        rtl.open_box(super::UnicodeBidi::Embed.controls(false), false);
        rtl.begin_paragraph();
        assert_eq!(
            rtl.text, "\u{202E}\u{202A}",
            "#bidi-embedding-breaks reopens"
        );
    }

    /// The terminal cell adapter paints a right-to-left run's characters in
    /// visual order.
    #[test]
    fn terminal_rows_spell_right_to_left_text_visually() {
        let html = format!("{BODY}<p>abc <b>אבג</b> xyz</p><p dir=rtl>שלום (abc) עולם</p>");
        let output = std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(move || {
                let dom = Dom::parse_document(&html);
                let base = Url::parse("http://e.com/").unwrap();
                crate::layout2::lay_out_document(
                    &dom,
                    &base,
                    TerminalViewport::new(40, 10, 8.0, 16.0),
                    &[],
                    &HashMap::new(),
                    &HashMap::new(),
                    &HashMap::new(),
                )
            })
            .unwrap()
            .join()
            .unwrap();
        let rows: Vec<(u16, String)> = output
            .rows
            .iter()
            .filter(|row| !row.items.is_empty())
            .map(|row| {
                let mut items: Vec<_> = row.items.iter().collect();
                items.sort_by_key(|item| item.col);
                let text = items
                    .iter()
                    .map(|item| item.text.as_str())
                    .collect::<String>();
                let end = items
                    .iter()
                    .map(|item| item.col + item.width)
                    .max()
                    .unwrap_or(0);
                (end, text)
            })
            .collect();
        let row = &rows[0].1;
        assert!(row.contains("גבא"), "{row:?}");
        assert!(row.find("abc") < row.find("גבא") && row.find("גבא") < row.find("xyz"));
        // A right-to-left paragraph reads from the right edge; the Latin run
        // in it keeps its order and the parentheses around it are mirrored
        // into the right-to-left runs (L4).
        let (end, row) = &rows[1];
        assert_eq!(row.trim(), "םלוע (abc) םולש", "{rows:?}");
        assert_eq!(*end, 40, "start-aligned at the right edge");
    }
}
