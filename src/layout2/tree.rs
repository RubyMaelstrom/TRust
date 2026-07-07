//! Box-tree construction (CSS 2.1 §9.2) for the layout2 engine.
//!
//! One walk over the styled DOM decides, ONCE, what box every rendered
//! element generates: its display class, its box style snapshot, and the
//! formatting context of its content. A block container either contains only
//! block-level boxes or establishes an inline formatting context — mixed
//! content grows anonymous block boxes around the inline runs (§9.2.1.1),
//! and a whitespace-only run between blocks generates nothing. Replaced
//! elements (`<img>`, form controls) become atomic boxes sized at layout.
//!
//! Out-of-flow elements (`position:absolute`/`fixed` — §9.3) generate boxes
//! that ride the inline lists only as STATIC-POSITION marks
//! (`Inline::OutOfFlow`); their display blockifies (§9.7) and the flow's
//! positioned post-pass lays them against their containing blocks. Flex/grid
//! containers carry theirs separately (`BoxNode::oof` — they don't
//! participate in flex/grid layout, css-flexbox §4.1).
//!
//! Honesty notes (staged phases, not policy): floats are not yet taken out
//! of flow; an
//! inline-level element whose subtree holds block-level boxes is promoted to
//! a block box (the §9.2.1.1 block-in-inline split, approximated
//! structurally — the visual result for real-world markup like
//! `<a><div>…</div></a>` is the same).

use url::Url;

use crate::doc::{FieldKind, Form, Link};
use crate::dom::{DOCUMENT, Dom, NodeData, NodeId};
use crate::layout::{ControlMap, Units, css_length_px, format_list_marker, is_collapsible_space};

use super::style::{BoxStyle, Disp, Pos, display_of};
use super::value::Vp;

/// A replaced (atomic) box: its element plus what the layout pass needs to
/// size and emit it.
#[derive(Debug)]
pub(crate) struct Atom {
    pub node: NodeId,
    pub kind: AtomKind,
}

#[derive(Debug)]
pub(crate) enum AtomKind {
    /// An `<img>`: the resolved absolute URL (http(s)/`data:`/`blob:`) and the
    /// alt text fallback for the not-yet-decoded state.
    Img { url: Option<String>, alt: String },
    /// A form control, rendered as its widget label (`Field::row_label`).
    Control { form: usize, field: usize },
    /// A `<video>`/`<audio>` media representation (the "play in mpv"
    /// affordance — a terminal renders no player).
    Media { video: bool },
}

/// Inline-level content inside an inline formatting context.
#[derive(Debug)]
pub(crate) enum Inline {
    /// A text run (raw — white-space collapsing happens at line building).
    /// The originating element is the enclosing `Box`/IFC root.
    Text(String),
    /// An inline box (`<a>`, `<b>`, `<span>`, …): style context plus its own
    /// horizontal margins/borders/padding, which occupy real inline space.
    /// The style snapshot is boxed — text runs vastly outnumber element
    /// boxes, and an unboxed `BoxStyle` would quintuple every variant.
    Box {
        node: NodeId,
        style: Box<BoxStyle>,
        kids: Vec<Inline>,
    },
    Atom(Atom),
    /// `<br>` — a forced line break (HTML §14.3.8).
    Br,
    /// An out-of-flow (`position:absolute`/`fixed`) box, riding the inline
    /// list ONLY to mark its static position (§10.3.7/§10.6.4 — the position
    /// its hypothetical in-flow first box would have had). It contributes no
    /// inline content; the flow lays it against its containing block in the
    /// positioned post-pass. Its display is already blockified (§9.7).
    OutOfFlow(Box<BoxNode>),
    /// A float (`float:left`/`right` — §9.5), out of normal flow and shifted to
    /// an edge. It rides the inline list at the point it appears in source (its
    /// margin-box top can be no higher than the line box it occurs on — §9.5.1
    /// rule 6); the IFC pulls it aside and shortens the line boxes beside it.
    /// Its display is blockified (§9.7), so the box is a block-level box.
    Float(Box<BoxNode>),
    /// An ATOMIC INLINE-LEVEL box (`inline-block`/`inline-flex`/`inline-grid`
    /// — CSS-Display-3 §2.5): its content is laid as its own INDEPENDENT
    /// formatting context (block/flex/grid) at the element's used width, then
    /// the whole box is placed on the parent's line as ONE opaque unit (like a
    /// replaced box — §9.4.2/§10.8). The inner box carries the blockified
    /// display in `content` (Blocks/Inlines/Flex/Grid/Table); the block flow
    /// pre-lays it (`item_frag`) and hands its used cell size to the IFC.
    AtomBox(Box<BoxNode>),
}

/// One box in the tree.
#[derive(Debug)]
pub(crate) struct BoxNode {
    /// The generating element (`NO_NODE` for anonymous boxes).
    pub node: NodeId,
    pub style: BoxStyle,
    pub content: Content,
    /// The `::marker` text of a `list-item`, pre-formatted (counters are
    /// document-order state, so they resolve here, not at layout).
    pub marker: Option<String>,
    /// `list-style-position: inside` — the marker joins the IFC as leading
    /// text instead of sitting in the gutter.
    pub marker_inside: bool,
    /// Out-of-flow children of a FLEX/GRID container (they don't participate
    /// in flex/grid layout — css-flexbox §4.1/css-grid §9; their static
    /// position is the container's content-box origin). Block containers
    /// carry their out-of-flow children inside the content lists instead
    /// (`Inline::OutOfFlow`), which records the inline static position.
    pub oof: Vec<BoxNode>,
}

/// What a block container holds (§9.2: all block-level, or an IFC).
#[derive(Debug)]
pub(crate) enum Content {
    Blocks(Vec<BoxNode>),
    Inlines(Vec<Inline>),
    /// A block-level replaced element (`<img style="display:block">`).
    Atomic(Atom),
    /// A flex container's items (css-flexbox §4: every in-flow child
    /// blockified into an item; text runs wrapped in anonymous items).
    Flex(Vec<BoxNode>),
    /// A grid container's items (css-grid §6 forms them identically).
    Grid(Vec<BoxNode>),
    /// A table wrapper's grid + captions (CSS 2.1 §17). Boxed — a `TableBox`
    /// is large and tables are rare, so an unboxed variant would bloat every
    /// `Content`.
    Table(Box<TableBox>),
}

/// A `display:table` element's resolved structure (CSS 2.1 §17), built once:
/// its cells placed on a grid (`colspan`/`rowspan` resolved), its column
/// width preferences, and its caption boxes.
#[derive(Debug)]
pub(crate) struct TableBox {
    /// Caption boxes rendered ABOVE the grid (`caption-side: top`, the
    /// default) and BELOW it (`caption-side: bottom`) — §17.4.
    pub top_captions: Vec<BoxNode>,
    pub bottom_captions: Vec<BoxNode>,
    /// Per-column width preference from `<col>`/`<colgroup>` (§17.5.2),
    /// expanded over columns (`<col span=N>` repeats). May be shorter than
    /// `ncols`; the layout indexes with `.get()`.
    pub col_specs: Vec<Option<ColSpec>>,
    /// The placed cells, in row-major document order.
    pub cells: Vec<TableCell>,
    pub ncols: usize,
    pub nrows: usize,
    /// `table-layout: fixed` (§17.5.2.1).
    pub fixed_layout: bool,
}

/// One cell placed in the grid: its box plus the top-left coordinates and
/// span it occupies after `colspan`/`rowspan` resolution (CSS 2.1 §17.5).
#[derive(Debug)]
pub(crate) struct TableCell {
    pub b: BoxNode,
    pub row: usize,
    pub col: usize,
    pub rowspan: usize,
    pub colspan: usize,
}

/// A declared `width` on a table column/cell: a used pixel length, or a
/// fraction of the table width (CSS 2.1 §17.5.2 — "a percentage width for a
/// column is relative to the table width").
#[derive(Copy, Clone, Debug)]
pub(crate) enum ColSpec {
    Px(f32),
    Pct(f32),
}

/// How deeply tables may nest before a table degrades to block-stacked
/// content: past the lid the whole table subtree lays as ordinary blocks.
/// A hard recursion lid on the box-tree build (and thus on the per-cell
/// intrinsic/layout descents) so a pathologically deep table tree (some
/// wikis nest navboxes very deep) can't overflow the stack.
const MAX_TABLE_DEPTH: usize = 8;

/// Hostile-input lid on one cell's occupancy footprint: `colspan` and
/// `rowspan` are each clamped to 1000, but their PRODUCT drives the grid
/// inserts, so the colspan (the visually meaningful axis) is kept and the
/// rowspan clamped to fit this area (a page of `colspan=1000 rowspan=1000`
/// cells would otherwise do 10^6 inserts per cell).
const MAX_CELL_SPAN_AREA: usize = 10_000;

/// Build the box tree for a document. `None` when there is no root element
/// (nothing to render).
pub(crate) fn build(
    dom: &Dom,
    base: &Url,
    controls: &ControlMap,
    forms: &[Form],
    vp: Vp,
) -> Option<BoxNode> {
    let root = dom
        .children(DOCUMENT)
        .into_iter()
        .find(|&c| dom.tag_name(c).is_some())?;
    let mut b = Builder {
        dom,
        base,
        controls,
        forms,
        vp,
        lists: Vec::new(),
        table_depth: 0,
    };
    match b.element(root) {
        Built::Block(bx) => Some(*bx),
        Built::Inline(inl) => Some(BoxNode {
            node: root,
            style: BoxStyle::of(dom, root, vp),
            content: Content::Inlines(vec![inl]),
            marker: None,
            marker_inside: false,
            oof: Vec::new(),
        }),
        _ => None,
    }
}

/// Build a box tree rooted at an arbitrary element `boundary` (an incremental-
/// layout relayout boundary — a scroll region or an inline IFC box), for a
/// subtree fragment re-lay (INCREMENTAL_LAYOUT_PLAN.md). Same machinery as
/// `build`, entered at `boundary` instead of the document root; `None` when the
/// node generates no box (`display:none`/skipped).
pub(crate) fn build_at(
    dom: &Dom,
    base: &Url,
    controls: &ControlMap,
    forms: &[Form],
    vp: Vp,
    boundary: NodeId,
) -> Option<BoxNode> {
    let mut b = Builder {
        dom,
        base,
        controls,
        forms,
        vp,
        lists: Vec::new(),
        table_depth: 0,
    };
    match b.element(boundary) {
        Built::Block(bx) => Some(*bx),
        Built::Inline(inl) => Some(BoxNode {
            node: boundary,
            style: BoxStyle::of(dom, boundary, vp),
            content: Content::Inlines(vec![inl]),
            marker: None,
            marker_inside: false,
            oof: Vec::new(),
        }),
        _ => None,
    }
}

/// What classifying a possibly-replaced element produced.
enum Replaced {
    Atom(AtomKind),
    Skip,
    No,
}

/// §9.7: an out-of-flow box's computed display blockifies (inline and
/// inline-block become block; `display_of` already collapsed the inline
/// variants of flex/grid onto their block-level classes).
fn blockify(d: Disp) -> Disp {
    match d {
        Disp::Inline => Disp::Block,
        d => d,
    }
}

/// The INNER (blockified) display of an atomic inline-level box, or `None` when
/// the element is not one. `inline-block`/`inline-flex`/`inline-grid`/
/// `inline-table` lay their content as a block/flex/grid/table formatting
/// context and ride the parent's line as one opaque box (CSS-Display-3 §2.5); an
/// `inline-block` holding misparented table rows becomes an anonymous table
/// (§17.2.1), mirroring `display_of`.
fn atomic_inline_disp(dom: &Dom, id: NodeId) -> Option<Disp> {
    match dom.effective_display(id)?.as_str() {
        "inline-block" if dom.establishes_anonymous_table(id) => Some(Disp::Table),
        "inline-block" => Some(Disp::Block),
        "inline-flex" => Some(Disp::Flex),
        "inline-grid" => Some(Disp::Grid),
        "inline-table" => Some(Disp::Table),
        _ => None,
    }
}

/// What building one DOM child produced.
enum Built {
    Block(Box<BoxNode>),
    Inline(Inline),
    /// `display:contents`: no box — the children hoist into the parent.
    Hoist(Vec<Built>),
    Skip,
}

impl Built {
    fn is_block(&self) -> bool {
        match self {
            Built::Block(_) => true,
            Built::Hoist(kids) => kids.iter().any(Built::is_block),
            _ => false,
        }
    }
}

/// Elements whose subtree never renders as page content. Renderable inline
/// `<svg>` was already rewritten to `<img data:…>` by `rewrite_inline_svgs`;
/// what remains here has no terminal rendering.
const SKIP: &[&str] = &[
    "base", "canvas", "head", "iframe", "link", "math", "meta", "noscript", "object", "script",
    "style", "svg", "template", "title", "wbr", "area", "map", "datalist",
];

struct Builder<'a> {
    dom: &'a Dom,
    base: &'a Url,
    controls: &'a ControlMap,
    forms: &'a [Form],
    vp: Vp,
    /// Open lists' counters: `(next value, step)` per nesting level (`<ol
    /// reversed>` counts down — HTML §4.4.5, through zero into negatives).
    lists: Vec<(i64, i64)>,
    /// How many `display:table` wrappers enclose the current box — the
    /// `MAX_TABLE_DEPTH` recursion lid.
    table_depth: usize,
}

impl Builder<'_> {
    fn element(&mut self, id: NodeId) -> Built {
        let Some(tag) = self.dom.tag_name(id) else {
            return Built::Skip;
        };
        if SKIP.contains(&tag) {
            return Built::Skip;
        }
        let disp = display_of(self.dom, id);
        if disp == Disp::None {
            return Built::Skip;
        }
        if disp == Disp::Contents {
            let kids = self.children(id);
            return Built::Hoist(kids);
        }
        // A `<slot>` in a shadow tree is TRANSPARENT (HTML §4.8.2): it renders
        // the host's assigned light nodes in its place, or its own fallback
        // content when nothing is assigned. Hoisting mirrors the serializer and
        // completes the flat tree `children` starts (host → shadow root). A bare
        // `<slot>` outside any shadow tree has no host, so `slot_assigned_nodes`
        // is empty and it falls back to its own children.
        if tag == "slot" {
            let assigned = self.dom.slot_assigned_nodes(id);
            let kids = if assigned.is_empty() {
                self.children(id)
            } else {
                self.build_child_list(&assigned, false)
            };
            return Built::Hoist(kids);
        }
        // Replaced elements are atomic regardless of their content model.
        if tag == "br" {
            return Built::Inline(Inline::Br);
        }
        let rep = self.replaced(id, tag);
        if matches!(rep, Replaced::Skip) {
            return Built::Skip;
        }
        // Out-of-flow (§9.3/§9.7): the box is removed from normal flow, its
        // display blockified, and it rides the inline list as a
        // static-position mark for the positioned post-pass.
        if Pos::of(self.dom, id).out_of_flow() {
            let b = match rep {
                Replaced::Atom(kind) => BoxNode {
                    node: id,
                    style: BoxStyle::of(self.dom, id, self.vp),
                    content: Content::Atomic(Atom { node: id, kind }),
                    marker: None,
                    marker_inside: false,
                    oof: Vec::new(),
                },
                _ => match blockify(disp) {
                    Disp::Table => self.table(id),
                    d => self.container(id, d),
                },
            };
            return Built::Inline(Inline::OutOfFlow(Box::new(b)));
        }
        // Float (§9.5): out of normal flow, its display blockified (§9.7). Like
        // the out-of-flow path, it rides the inline list — but it is NOT a
        // block-level box for the §9.2.1.1 anonymous-box split (the inline
        // content around it forms one IFC), so it stays a `Built::Inline`.
        if super::float::float_side(self.dom, id).is_some() {
            let b = match rep {
                Replaced::Atom(kind) => BoxNode {
                    node: id,
                    style: BoxStyle::of(self.dom, id, self.vp),
                    content: Content::Atomic(Atom { node: id, kind }),
                    marker: None,
                    marker_inside: false,
                    oof: Vec::new(),
                },
                _ => match blockify(disp) {
                    Disp::Table => self.table(id),
                    d => self.container(id, d),
                },
            };
            return Built::Inline(Inline::Float(Box::new(b)));
        }
        if let Replaced::Atom(kind) = rep {
            return self.atom(id, disp, kind);
        }
        // ATOMIC INLINE-LEVEL box (`inline-block`/`inline-flex`/`inline-grid` —
        // CSS-Display-3 §2.5): in-flow, not floated, not replaced. Its content
        // lays as its own formatting context (the blockified inner display),
        // and the box rides the parent's line as one opaque unit. The block
        // flow pre-lays `AtomBox` and hands its used size to the IFC.
        if let Some(inner) = atomic_inline_disp(self.dom, id) {
            let b = match inner {
                Disp::Table => self.table(id),
                d => self.container(id, d),
            };
            return Built::Inline(Inline::AtomBox(Box::new(b)));
        }
        match disp {
            Disp::Table => Built::Block(Box::new(self.table(id))),
            Disp::Block | Disp::ListItem | Disp::Flex | Disp::Grid => {
                Built::Block(Box::new(self.container(id, disp)))
            }
            Disp::Inline => {
                let kids = self.children(id);
                if kids.iter().any(Built::is_block) {
                    // Block-in-inline: promote (see module docs).
                    Built::Block(Box::new(self.assemble(
                        id,
                        BoxStyle::of(self.dom, id, self.vp),
                        kids,
                        None,
                        false,
                    )))
                } else {
                    Built::Inline(Inline::Box {
                        node: id,
                        style: Box::new(BoxStyle::of(self.dom, id, self.vp)),
                        kids: kids
                            .into_iter()
                            .filter_map(|k| match k {
                                Built::Inline(i) => Some(i),
                                _ => None,
                            })
                            .collect(),
                    })
                }
            }
            Disp::None | Disp::Contents => unreachable!("handled above"),
        }
    }

    /// Classify a replaced element (shared by the in-flow and out-of-flow
    /// paths): its atom kind, a skip (nothing to draw), or not-replaced.
    fn replaced(&mut self, id: NodeId, tag: &str) -> Replaced {
        if tag == "img" {
            return Replaced::Atom(AtomKind::Img {
                url: self.image_src(id),
                alt: self
                    .dom
                    .attr(id, "alt")
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        if matches!(tag, "video" | "audio") {
            // Children (sources/tracks/fallback) are consumed by the media
            // representation itself, never flowed as content.
            return Replaced::Atom(AtomKind::Media {
                video: tag == "video",
            });
        }
        if matches!(tag, "input" | "button" | "select" | "textarea") {
            let mapped = self.controls.get(&id).copied().filter(|&(form, field)| {
                self.forms
                    .get(form)
                    .and_then(|f| f.fields.get(field))
                    .is_some_and(|f| f.kind != FieldKind::Hidden)
            });
            match mapped {
                Some((form, field)) => return Replaced::Atom(AtomKind::Control { form, field }),
                // An unmapped input/select has no widget to draw; an
                // unmapped button/textarea flows as a normal element — its
                // visible content renders and the ambient live-page link
                // (the x-trust-js anchor around a React-style button) keeps
                // it clickable through the ordinary inline context.
                None if matches!(tag, "input" | "select") => return Replaced::Skip,
                None => {}
            }
        }
        // A `contenteditable` host bound to a field (http::walk_forms_arena made
        // it a synthetic textarea) is ONE editable widget: render it as a
        // control atom and skip its subtree (the editor's own markup isn't ours
        // to flow) — the same path a real `<textarea>` takes.
        if self.dom.is_contenteditable_host(id)
            && let Some(&(form, field)) = self.controls.get(&id)
            && self
                .forms
                .get(form)
                .and_then(|f| f.fields.get(field))
                .is_some_and(|f| f.kind != FieldKind::Hidden)
        {
            return Replaced::Atom(AtomKind::Control { form, field });
        }
        Replaced::No
    }

    /// A replaced element: inline-level by default, block-level when its
    /// computed display says so (`display:block` images stack on their own
    /// line and can center through auto margins).
    fn atom(&mut self, id: NodeId, disp: Disp, kind: AtomKind) -> Built {
        let atom = Atom { node: id, kind };
        match disp {
            Disp::Block | Disp::ListItem | Disp::Flex | Disp::Grid => {
                Built::Block(Box::new(BoxNode {
                    node: id,
                    style: BoxStyle::of(self.dom, id, self.vp),
                    content: Content::Atomic(atom),
                    marker: None,
                    marker_inside: false,
                    oof: Vec::new(),
                }))
            }
            _ => Built::Inline(Inline::Atom(atom)),
        }
    }

    /// A block container: build children (list counters opened around them),
    /// then wrap mixed content per §9.2.1.1 — or, for a flex container,
    /// blockify every child into a flex item per css-flexbox §4.
    fn container(&mut self, id: NodeId, disp: Disp) -> BoxNode {
        let tag = self.dom.tag_name(id).unwrap_or("");
        let list = matches!(tag, "ul" | "ol" | "menu" | "dir");
        if list {
            self.lists.push(self.list_counter(id, tag));
        }
        let (marker, inside) = if disp == Disp::ListItem {
            self.marker(id)
        } else {
            (None, false)
        };
        let mut kids = self.children(id);
        if list {
            self.lists.pop();
        }
        // A button-less form carries its synthetic submit keyed on the form node
        // itself (http::walk_forms_arena — there is no submit ELEMENT to map).
        // Append it as a trailing control atom: a block form gives it its own
        // row, a flex/inline-flex form flows it as the last item (the old
        // engine's `place_form_stub` block-vs-inline split, structurally).
        if tag == "form"
            && let Some(&(form, field)) = self.controls.get(&id)
            && self
                .forms
                .get(form)
                .and_then(|f| f.fields.get(field))
                .is_some_and(|f| f.kind == FieldKind::Submit)
        {
            kids.push(Built::Block(Box::new(BoxNode {
                node: id,
                style: BoxStyle::anonymous(),
                content: Content::Atomic(Atom {
                    node: id,
                    kind: AtomKind::Control { form, field },
                }),
                marker: None,
                marker_inside: false,
                oof: Vec::new(),
            })));
        }
        if matches!(disp, Disp::Flex | Disp::Grid) {
            let (its, oof) = self.itemize(kids);
            return BoxNode {
                node: id,
                style: BoxStyle::of(self.dom, id, self.vp),
                content: if disp == Disp::Flex {
                    Content::Flex(its)
                } else {
                    Content::Grid(its)
                },
                marker,
                marker_inside: inside,
                oof,
            };
        }
        self.assemble(
            id,
            BoxStyle::of(self.dom, id, self.vp),
            kids,
            marker,
            inside,
        )
    }

    /// css-flexbox §4: each in-flow element child becomes a flex item
    /// (blockified — an inline box turns into a block-level box holding its
    /// inline content); each contiguous run of text becomes an anonymous
    /// item; a run of only collapsible white space generates nothing.
    /// Out-of-flow children don't participate (§4.1) — returned separately.
    fn itemize(&mut self, kids: Vec<Built>) -> (Vec<BoxNode>, Vec<BoxNode>) {
        let mut items: Vec<BoxNode> = Vec::new();
        let mut oof: Vec<BoxNode> = Vec::new();
        let mut run: Vec<Inline> = Vec::new();
        let flush = |run: &mut Vec<Inline>, items: &mut Vec<BoxNode>| {
            if run.iter().any(inline_has_content) {
                items.push(BoxNode {
                    node: crate::layout::NO_NODE,
                    style: BoxStyle::anonymous(),
                    content: Content::Inlines(std::mem::take(run)),
                    marker: None,
                    marker_inside: false,
                    oof: Vec::new(),
                });
            } else {
                run.clear();
            }
        };
        for k in kids {
            match k {
                Built::Block(b) => {
                    flush(&mut run, &mut items);
                    items.push(*b);
                }
                Built::Inline(Inline::OutOfFlow(b)) => oof.push(*b),
                // css-flexbox §4.1 / css-grid §6: `float` is ignored on a
                // flex/grid item — the blockified box becomes an ordinary item.
                Built::Inline(Inline::Float(b)) => {
                    flush(&mut run, &mut items);
                    items.push(*b);
                }
                Built::Inline(Inline::Box { node, style, kids }) => {
                    flush(&mut run, &mut items);
                    items.push(BoxNode {
                        node,
                        style: *style,
                        content: Content::Inlines(kids),
                        marker: None,
                        marker_inside: false,
                        oof: Vec::new(),
                    });
                }
                Built::Inline(Inline::Atom(a)) => {
                    flush(&mut run, &mut items);
                    items.push(BoxNode {
                        node: a.node,
                        style: BoxStyle::of(self.dom, a.node, self.vp),
                        content: Content::Atomic(a),
                        marker: None,
                        marker_inside: false,
                        oof: Vec::new(),
                    });
                }
                // css-flexbox §4.1 / css-grid §6: an atomic inline box child of
                // a flex/grid container is BLOCKIFIED into an ordinary item (its
                // atomic-inline-ness is stripped); the inner box already carries
                // the blockified content.
                Built::Inline(Inline::AtomBox(b)) => {
                    flush(&mut run, &mut items);
                    items.push(*b);
                }
                Built::Inline(i) => run.push(i),
                Built::Hoist(_) | Built::Skip => {}
            }
        }
        flush(&mut run, &mut items);
        (items, oof)
    }

    /// The opening counter state for a list container: `<ol start>`, and
    /// `<ol reversed>` counting down from the item count (HTML §4.4.5).
    fn list_counter(&self, id: NodeId, tag: &str) -> (i64, i64) {
        if tag != "ol" {
            return (1, 1);
        }
        let reversed = self.dom.attr(id, "reversed").is_some();
        let step = if reversed { -1 } else { 1 };
        let start = self
            .dom
            .attr(id, "start")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or_else(|| {
                if reversed {
                    self.dom
                        .child_iter(id)
                        .filter(|&c| self.dom.tag_name(c) == Some("li"))
                        .count() as i64
                } else {
                    1
                }
            });
        (start, step)
    }

    /// The formatted `::marker` for a list item, advancing the counter.
    fn marker(&mut self, id: NodeId) -> (Option<String>, bool) {
        if let Some(v) = self
            .dom
            .attr(id, "value")
            .and_then(|v| v.trim().parse::<i64>().ok())
            && let Some(top) = self.lists.last_mut()
        {
            top.0 = v;
        }
        let n = match self.lists.last_mut() {
            Some(top) => {
                let n = top.0;
                top.0 += top.1;
                n
            }
            None => 1,
        };
        let kind = self
            .dom
            .computed_value(id, "list-style-type")
            .unwrap_or_else(|| "disc".to_string());
        let text = format_list_marker(kind.trim(), n);
        let inside = matches!(
            self.dom
                .computed_value(id, "list-style-position")
                .as_deref(),
            Some("inside")
        );
        ((!text.is_empty()).then_some(text), inside)
    }

    /// Build the box-level children of `id`, flattening `display:contents`
    /// hoists and applying the HTML rendering rules that gate children
    /// (a closed `<details>` shows only its first `<summary>`).
    ///
    /// COMPOSES the shadow tree (HTML §4.8.2, the "flat tree"): a shadow HOST
    /// renders its shadow root's children IN PLACE of its light children (which
    /// reach the box tree only through `<slot>`s — handled in `element`). This
    /// is the same flattening the serializer does, and it is load-bearing for
    /// `measure_boxes`, which lays the LIVE ARENA (real shadow roots) rather than
    /// the pre-flattened `Doc.raw` the main render uses: without it every
    /// shadow-hosted element (archive.org's whole `<router-slot>`/`<home-page>`
    /// app, Twitch's web components) has NO box, so `getBoundingClientRect`/
    /// `offset*`/`client*` and the Resize/IntersectionObservers all read 0 — a
    /// virtualized scroller then computes zero columns and renders nothing. The
    /// main render is unaffected (`Doc.raw` has no shadow roots, so this reduces
    /// to the light children).
    fn children(&mut self, id: NodeId) -> Vec<Built> {
        let closed_details =
            self.dom.tag_name(id) == Some("details") && self.dom.attr(id, "open").is_none();
        let child_ids = match self.dom.shadow_root(id) {
            Some(shadow) => self.dom.children(shadow),
            None => self.dom.children(id),
        };
        self.build_child_list(&child_ids, closed_details)
    }

    /// Build a list of child node ids into box-level `Built`s: text runs become
    /// anonymous inline text, elements build (with `display:contents` and
    /// `<slot>` hoists flattened in), and a closed `<details>` keeps only its
    /// first `<summary>`. Shared by `children` and the `<slot>` projection.
    fn build_child_list(&mut self, ids: &[NodeId], closed_details: bool) -> Vec<Built> {
        let mut out = Vec::new();
        let mut summary_shown = false;
        for &c in ids {
            if closed_details {
                let is_summary = self.dom.tag_name(c) == Some("summary");
                if !is_summary || summary_shown {
                    continue;
                }
                summary_shown = true;
            }
            match &self.dom.node(c).data {
                NodeData::Text(t) if !t.is_empty() => {
                    out.push(Built::Inline(Inline::Text(t.clone())));
                }
                NodeData::Element { .. } => match self.element(c) {
                    Built::Hoist(kids) => out.extend(kids),
                    Built::Skip => {}
                    b => out.push(b),
                },
                _ => {}
            }
        }
        out
    }

    /// §9.2.1.1: if any child is block-level, wrap each run of inline-level
    /// children in an anonymous block box — except runs that are only
    /// collapsible white space, which generate nothing.
    fn assemble(
        &self,
        node: NodeId,
        style: BoxStyle,
        kids: Vec<Built>,
        marker: Option<String>,
        marker_inside: bool,
    ) -> BoxNode {
        let any_block = kids.iter().any(Built::is_block);
        if !any_block {
            let inlines = kids
                .into_iter()
                .filter_map(|k| match k {
                    Built::Inline(i) => Some(i),
                    _ => None,
                })
                .collect();
            return BoxNode {
                node,
                style,
                content: Content::Inlines(inlines),
                marker,
                marker_inside,
                oof: Vec::new(),
            };
        }
        let mut blocks: Vec<BoxNode> = Vec::new();
        let mut run: Vec<Inline> = Vec::new();
        let flush = |run: &mut Vec<Inline>, blocks: &mut Vec<BoxNode>| {
            if run.iter().any(inline_has_content) {
                blocks.push(BoxNode {
                    node: crate::layout::NO_NODE,
                    style: BoxStyle::anonymous(),
                    content: Content::Inlines(std::mem::take(run)),
                    marker: None,
                    marker_inside: false,
                    oof: Vec::new(),
                });
            } else {
                run.clear();
            }
        };
        for k in kids {
            match k {
                Built::Block(b) => {
                    flush(&mut run, &mut blocks);
                    blocks.push(*b);
                }
                Built::Inline(i) => run.push(i),
                Built::Hoist(_) | Built::Skip => {}
            }
        }
        flush(&mut run, &mut blocks);
        BoxNode {
            node,
            style,
            content: Content::Blocks(blocks),
            marker,
            marker_inside,
            oof: Vec::new(),
        }
    }

    /// Build a `display:table` element's box (CSS 2.1 §17). Past the
    /// `MAX_TABLE_DEPTH` recursion lid the whole subtree degrades to
    /// block-stacked content (its rows/cells flow as ordinary blocks — the
    /// innermost content still renders, and the descent terminates).
    fn table(&mut self, id: NodeId) -> BoxNode {
        if self.table_depth >= MAX_TABLE_DEPTH {
            return self.container(id, Disp::Block);
        }
        self.table_depth += 1;
        let style = BoxStyle::of(self.dom, id, self.vp);
        // `table-layout`/`caption-side` aren't tracked/inherited properties,
        // so they read through the author cascade (`computed_style`), not
        // `computed_value` (which is registry-driven).
        let fixed_layout = self
            .dom
            .computed_style(id, "table-layout")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("fixed"));

        // Captions (§17.4): `table-caption` children render as block boxes
        // above the grid, or below it for `caption-side: bottom`.
        let mut top_captions = Vec::new();
        let mut bottom_captions = Vec::new();
        for c in self.dom.flat_children(id) {
            if self.dom.effective_display(c).as_deref() != Some("table-caption") {
                continue;
            }
            let bottom = self
                .dom
                .computed_style(c, "caption-side")
                .as_deref()
                .map(str::trim)
                == Some("bottom");
            let cap = self.container(c, Disp::Block);
            if bottom {
                bottom_captions.push(cap);
            } else {
                top_captions.push(cap);
            }
        }

        // Rows in visual order (§17.2.1: header group → body/implicit rows →
        // footer group), placed on the grid with `colspan`/`rowspan`.
        let rows = self.table_cell_rows(id);
        let (placed, ncols, nrows) = self.build_grid(&rows);
        let cells = placed
            .into_iter()
            .map(|(cell, row, col, rowspan, colspan)| TableCell {
                b: self.container(cell, Disp::Block),
                row,
                col,
                rowspan,
                colspan,
            })
            .collect();
        let col_specs = self.table_col_specs(id);
        self.table_depth -= 1;

        BoxNode {
            node: id,
            style,
            content: Content::Table(Box::new(TableBox {
                top_captions,
                bottom_captions,
                col_specs,
                cells,
                ncols,
                nrows,
                fixed_layout,
            })),
            marker: None,
            marker_inside: false,
            oof: Vec::new(),
        }
    }

    /// The cells of each table row, in visual order (header-group rows first,
    /// then body/implicit rows, then footer-group rows — CSS 2.1 §17.2.1).
    /// An empty row (a spacer `<tr>`) yields an empty inner vec so it still
    /// takes a grid row. Misparented cells directly under the table are
    /// approximated by collecting them into one trailing row.
    fn table_cell_rows(&self, table: NodeId) -> Vec<Vec<NodeId>> {
        let mut header = Vec::new();
        let mut body = Vec::new();
        let mut footer = Vec::new();
        let mut stray = Vec::new();
        for child in self.dom.flat_children(table) {
            match self.dom.effective_display(child).as_deref() {
                Some("table-header-group") => header.extend(self.group_rows(child)),
                Some("table-footer-group") => footer.extend(self.group_rows(child)),
                Some("table-row-group") => body.extend(self.group_rows(child)),
                Some("table-row") => body.push(self.row_cells(child)),
                Some("table-cell") => stray.push(child),
                _ => {} // columns, captions, whitespace: not rows
            }
        }
        if !stray.is_empty() {
            body.push(stray);
        }
        header.extend(body);
        header.extend(footer);
        header
    }

    /// The `table-row` children of a row group, each resolved to its cells.
    fn group_rows(&self, group: NodeId) -> Vec<Vec<NodeId>> {
        self.dom
            .flat_children(group)
            .into_iter()
            .filter(|&r| self.dom.effective_display(r).as_deref() == Some("table-row"))
            .map(|r| self.row_cells(r))
            .collect()
    }

    /// The `table-cell` children of a row.
    fn row_cells(&self, row: NodeId) -> Vec<NodeId> {
        self.dom
            .flat_children(row)
            .into_iter()
            .filter(|&c| self.dom.effective_display(c).as_deref() == Some("table-cell"))
            .collect()
    }

    /// Place the rows' cells on a grid, resolving `colspan`/`rowspan` into
    /// top-left coordinates + spans (CSS 2.1 §17.5). Returns
    /// `(cell, row, col, rowspan, colspan)` in document order plus the grid's
    /// column and row counts.
    #[allow(clippy::type_complexity)]
    fn build_grid(
        &self,
        rows: &[Vec<NodeId>],
    ) -> (Vec<(NodeId, usize, usize, usize, usize)>, usize, usize) {
        let mut cells = Vec::new();
        let mut ncols = 0usize;
        let mut nrows = 0usize;
        // Slots occupied by a rowspan reaching down from an earlier row.
        let mut occupied: std::collections::HashSet<(usize, usize)> =
            std::collections::HashSet::new();
        for (r, row) in rows.iter().enumerate() {
            let mut c = 0usize;
            for &cell in row {
                while occupied.contains(&(r, c)) {
                    c += 1;
                }
                let colspan = self.cell_span(cell, "colspan");
                // Cap the occupancy PRODUCT: keep the colspan, clamp rowspan.
                let rowspan = self
                    .cell_span(cell, "rowspan")
                    .min((MAX_CELL_SPAN_AREA / colspan).max(1));
                for rr in r..r + rowspan {
                    for cc in c..c + colspan {
                        occupied.insert((rr, cc));
                    }
                }
                cells.push((cell, r, c, rowspan, colspan));
                ncols = ncols.max(c + colspan);
                nrows = nrows.max(r + rowspan);
                c += colspan;
            }
        }
        (cells, ncols, nrows)
    }

    /// A cell's `colspan`/`rowspan` (HTML attributes), clamped to ≥1 and a
    /// sane ceiling (a hostile `colspan=100000` can't blow up the grid).
    fn cell_span(&self, id: NodeId, attr: &str) -> usize {
        self.dom
            .attr(id, attr)
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 1000)
    }

    /// Per-column width preferences from `<col>`/`<colgroup>` (CSS 2.1
    /// §17.5.2; HTML §4.9.3/§4.9.4). `<col span=N>` repeats its width over N
    /// columns; a CHILDLESS `<colgroup span=N width=…>` acts as N such
    /// columns, while one with `<col>` children defers to them. Tag-matched
    /// (`<col>`/`<colgroup>` are table-only markup).
    fn table_col_specs(&self, table: NodeId) -> Vec<Option<ColSpec>> {
        let mut specs = Vec::new();
        let push_cols = |el: NodeId, specs: &mut Vec<Option<ColSpec>>| {
            let w = declared_track_width(self.dom, el);
            for _ in 0..self.cell_span(el, "span") {
                specs.push(w);
            }
        };
        for child in self.dom.flat_children(table) {
            match self.dom.tag_name(child) {
                Some("colgroup") => {
                    let cols: Vec<NodeId> = self
                        .dom
                        .flat_children(child)
                        .into_iter()
                        .filter(|&c| self.dom.tag_name(c) == Some("col"))
                        .collect();
                    if cols.is_empty() {
                        push_cols(child, &mut specs);
                    } else {
                        for col in cols {
                            push_cols(col, &mut specs);
                        }
                    }
                }
                Some("col") => push_cols(child, &mut specs),
                _ => {}
            }
        }
        specs
    }

    /// The absolute URL of an `<img>`'s `src`: `data:`/`blob:` key on the URL
    /// itself (blob bytes come from `Doc.blobs`, never the wire); http(s)
    /// resolves against the base; other schemes have no image pipeline.
    fn image_src(&self, id: NodeId) -> Option<String> {
        let src = self.dom.attr(id, "src")?.trim();
        if src.is_empty() {
            return None;
        }
        if src.starts_with("data:") || src.starts_with("blob:") {
            return Some(src.to_string());
        }
        match crate::http::resolve(self.base, src) {
            Link::Http(u) => Some(u.to_string()),
            _ => None,
        }
    }
}

/// Whether an inline run generates an anonymous box at all. Pure collapsible
/// white space between blocks is the §9.2.1.1 "would subsequently be
/// collapsed away" case and generates nothing. An inline ELEMENT box is kept
/// even when empty: it renders nothing (its anonymous block self-collapses
/// to zero height), but it is a real box with a real flow position — an
/// empty `<a name>`/`<span id>` is a fragment scroll target.
fn inline_has_content(i: &Inline) -> bool {
    match i {
        Inline::Text(t) => !t.chars().all(is_collapsible_space),
        Inline::Box { .. } => true,
        Inline::Atom(_) => true,
        // An atomic inline box is opaque content on the line (like an atom).
        Inline::AtomBox(_) => true,
        Inline::Br => true,
        // Keeps its run alive so the static-position mark has a host box;
        // the box emits no lines, so a placeholder-only run still
        // self-collapses to zero height.
        Inline::OutOfFlow(_) => true,
        // A float keeps its run alive too — a block whose only content is a
        // float still places the float (and self-collapses, the float being
        // out of flow — the classic "collapsed float parent").
        Inline::Float(_) => true,
    }
}

/// A declared `width` on a table/column/cell — the CSS `width` if set, else
/// the HTML `width` presentational attribute (HTML §15.3.13 maps it to the
/// `width` property). `None` for `auto`/unset. A bare number is CSS pixels.
/// A free function so both the box-tree builder (col/colgroup specs) and the
/// layout algorithm (per-cell widths — table.rs) read it identically.
pub(super) fn declared_track_width(dom: &Dom, id: NodeId) -> Option<ColSpec> {
    let raw = dom
        .computed_style(id, "width")
        .or_else(|| dom.attr(id, "width").map(|s| s.trim().to_string()))?;
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("auto") || raw.is_empty() {
        return None;
    }
    if let Some(rest) = raw.strip_suffix('%')
        && let Ok(p) = rest.trim().parse::<f32>()
    {
        return Some(ColSpec::Pct(p / 100.0));
    }
    let u = Units::of(dom, id);
    if let Some(px) = css_length_px(raw, u) {
        return Some(ColSpec::Px(px.max(0.0)));
    }
    raw.parse::<f32>()
        .ok()
        .filter(|n| *n > 0.0)
        .map(ColSpec::Px)
}
