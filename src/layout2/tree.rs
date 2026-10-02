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
//! An inline-level element whose subtree holds block-level boxes is promoted to
//! a block box (the §9.2.1.1 block-in-inline split, approximated
//! structurally — the visual result for real-world markup like
//! `<a><div>…</div></a>` is the same).

use std::sync::Arc;
use url::Url;

use crate::doc::{FieldKind, Form};
use crate::dom::{DOCUMENT, Dom, NodeData, NodeId, PseudoEl};
use crate::layout2::{ControlMap, Units, css_length_px, is_collapsible_space};

use super::style::{BoxStyle, Disp, Pos, display_of};
use super::value::Vp;

/// A replaced (atomic) box: its element plus what the layout pass needs to
/// size and emit it.
#[derive(Clone, Debug)]
pub(crate) struct Atom {
    pub node: NodeId,
    pub kind: AtomKind,
}

#[derive(Clone, Debug)]
pub(crate) enum AtomKind {
    /// An `<img>`: the resolved absolute URL (http(s)/`data:`/`blob:`) and the
    /// alt text fallback for the not-yet-decoded state.
    Img {
        url: Option<String>,
        density: f32,
        dimension_source: NodeId,
        alt: String,
        /// The form control an `input` in the Image Button state submits,
        /// as indices into the document's forms.
        control: Option<(usize, usize)>,
    },
    /// CSS generated content's anonymous replaced image, sized by its natural
    /// dimensions rather than the originating element's dimensions.
    GeneratedImage { url: String },
    /// A form control, rendered as its widget label (`Field::row_label`).
    Control { form: usize, field: usize },
    /// A `<video>`/`<audio>` media representation (the "play in mpv"
    /// affordance — a terminal renders no player).
    Media { video: bool },
}

/// Inline-level content inside an inline formatting context.
#[derive(Clone, Debug)]
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
        style: Arc<BoxStyle>,
        kids: Arc<[Inline]>,
    },
    Atom(Atom),
    /// `<br>` — a forced line break (HTML §14.3.8).
    Br,
    /// An out-of-flow (`position:absolute`/`fixed`) box, riding the inline
    /// list ONLY to mark its static position (§10.3.7/§10.6.4 — the position
    /// its hypothetical in-flow first box would have had). It contributes no
    /// inline content; the flow lays it against its containing block in the
    /// positioned post-pass. Its display is already blockified (§9.7).
    OutOfFlow(SharedBox),
    /// A float (`float:left`/`right` — §9.5), out of normal flow and shifted to
    /// an edge. It rides the inline list at the point it appears in source (its
    /// margin-box top can be no higher than the line box it occurs on — §9.5.1
    /// rule 6); the IFC pulls it aside and shortens the line boxes beside it.
    /// Its display is blockified (§9.7), so the box is a block-level box.
    Float(SharedBox),
    /// An ATOMIC INLINE-LEVEL box (`inline-block`/`inline-flex`/`inline-grid`
    /// — CSS-Display-3 §2.5): its content is laid as its own INDEPENDENT
    /// formatting context (block/flex/grid) at the element's used width, then
    /// the whole box is placed on the parent's line as ONE opaque unit (like a
    /// replaced box — §9.4.2/§10.8). The inner box carries the blockified
    /// display in `content` (Blocks/Inlines/Flex/Grid/Table); the block flow
    /// pre-lays it (`item_frag`) and hands its used cell size to the IFC.
    AtomBox(SharedBox),
}

pub(crate) type SharedBox = Arc<BoxNode>;

/// One box in the tree.
#[derive(Clone, Debug)]
pub(crate) struct BoxNode {
    /// The generating element (`NO_NODE` for anonymous boxes).
    pub node: NodeId,
    pub style: BoxStyle,
    pub content: Content,
    /// The `::marker` text of a `list-item`, pre-formatted (counters are
    /// document-order state, so they resolve here, not at layout).
    pub marker: Option<String>,
    /// `list-style-image` source for the anonymous marker replaced element.
    /// The source remains in authored URL form; paint resolves it against the
    /// page base exactly like an `<img>` source.
    pub marker_image: Option<String>,
    /// `list-style-position: inside` — the marker joins the IFC as leading
    /// text instead of sitting in the gutter.
    pub marker_inside: bool,
    /// Out-of-flow children of a FLEX/GRID container (they don't participate
    /// in flex/grid layout — css-flexbox §4.1/css-grid §9; their static
    /// position is the container's content-box origin). Block containers
    /// carry their out-of-flow children inside the content lists instead
    /// (`Inline::OutOfFlow`), which records the inline static position.
    pub oof: Vec<(usize, SharedBox)>,
}

/// What a block container holds (§9.2: all block-level, or an IFC).
#[derive(Clone, Debug)]
pub(crate) enum Content {
    Blocks(Vec<SharedBox>),
    Inlines(Vec<Inline>),
    /// A block-level replaced element (`<img style="display:block">`).
    Atomic(Atom),
    /// A flex container's items (css-flexbox §4: every in-flow child
    /// blockified into an item; text runs wrapped in anonymous items).
    Flex(Vec<SharedBox>),
    /// A grid container's items (css-grid §6 forms them identically).
    Grid(Vec<SharedBox>),
    /// A table wrapper's grid + captions (CSS 2.1 §17). Boxed — a `TableBox`
    /// is large and tables are rare, so an unboxed variant would bloat every
    /// `Content`.
    Table(Box<TableBox>),
}

/// A `display:table` element's resolved structure (CSS 2.1 §17), built once:
/// its cells placed on a grid (`colspan`/`rowspan` resolved), its column
/// width preferences, and its caption boxes.
#[derive(Clone, Debug)]
pub(crate) struct TableBox {
    /// Caption boxes rendered ABOVE the grid (`caption-side: top`, the
    /// default) and BELOW it (`caption-side: bottom`) — §17.4.
    pub top_captions: Vec<SharedBox>,
    pub bottom_captions: Vec<SharedBox>,
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
#[derive(Clone, Debug)]
pub(crate) struct TableCell {
    pub b: SharedBox,
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

type CellRows = Vec<Vec<SharedBox>>;

#[cfg(test)]
mod table_contract_tests {
    use super::*;

    fn table(html: &str, check: impl FnOnce(&Dom, &TableBox)) {
        let dom = Dom::parse_document(&format!("<!doctype html>{html}"));
        let base = Url::parse("https://tables.invalid/").unwrap();
        let controls = ControlMap::new();
        let mut builder = Builder {
            dom: &dom,
            base: &base,
            controls: &controls,
            forms: &[],
            vp: Vp { w: 800., h: 600. },
            lists: Vec::new(),
            reuse: false,
        };
        let root = builder.table(dom.get_by_id("table").unwrap());
        let Content::Table(table) = &root.content else {
            panic!("table must remain a table")
        };
        check(&dom, table);
    }

    #[test]
    fn zero_rowspan_ends_at_its_row_group_and_group_occupancy_resets() {
        table(
            "<table id=table><tbody><tr><td id=growing rowspan=0>grow</td><td>A</td></tr><tr><td id=second>B</td></tr><tr><td>C</td></tr></tbody><tbody><tr><td id=next>D</td></tr></tbody></table>",
            |dom, table| {
                let cell = |id| {
                    table
                        .cells
                        .iter()
                        .find(|cell| Some(cell.b.node) == dom.get_by_id(id))
                        .unwrap()
                };
                assert_eq!((cell("growing").rowspan, cell("second").col), (3, 1));
                assert_eq!((cell("next").row, cell("next").col), (3, 0));
                assert_eq!((table.nrows, table.ncols), (4, 2));
            },
        );
    }

    #[test]
    fn span_area_never_limits_content_and_ghost_tracks_merge_by_coverage() {
        for (style, columns) in [("", 1), ("table-layout:fixed;width:1000px", 1000)] {
            table(
                &format!(
                    "<table id=table style='{style}'><tr><td colspan=1000 rowspan=65534>large span</td></tr></table>"
                ),
                |_, table| {
                    assert_eq!((table.ncols, table.nrows), (columns, 1));
                    assert_eq!(
                        (table.cells[0].colspan, table.cells[0].rowspan),
                        (columns, 1)
                    );
                },
            );
        }
        // An explicit column box prevents merging even in auto layout.
        table(
            "<table id=table><colgroup span=1000></colgroup><tr><td colspan=1000 rowspan=65534>large span</td></tr></table>",
            |_, table| {
                assert_eq!((table.ncols, table.nrows), (1000, 1));
                assert_eq!(table.cells[0].colspan, 1000);
            },
        );
    }

    #[test]
    fn spans_use_html_integer_parsing_and_only_apply_to_html_table_elements() {
        table(
            "<table id=table style='table-layout:fixed;width:100px'><tr><td colspan=' +2trailing'>A</td><td colspan='-2'>B</td><td colspan='0'>C</td></tr></table>",
            |_, table| {
                assert_eq!(
                    table.cells.iter().map(|c| c.colspan).collect::<Vec<_>>(),
                    [2, 1, 1]
                );
                assert_eq!(table.ncols, 4);
            },
        );
        table(
            "<div id=table style='display:table;table-layout:fixed;width:100px'><div style='display:table-row'><div style='display:table-cell' colspan=9 rowspan=8>not HTML td</div></div></div>",
            |_, table| {
                assert_eq!((table.ncols, table.nrows), (1, 1));
            },
        );
    }

    #[test]
    fn only_first_header_and_footer_groups_are_reordered() {
        table(
            "<div id=table style='display:table'><div style='display:table-footer-group'><div style='display:table-row'><div id=f1 style='display:table-cell'>f1</div></div></div><div style='display:table-row-group'><div style='display:table-row'><div id=b style='display:table-cell'>b</div></div></div><div style='display:table-header-group'><div style='display:table-row'><div id=h1 style='display:table-cell'>h1</div></div></div><div style='display:table-footer-group'><div style='display:table-row'><div id=f2 style='display:table-cell'>f2</div></div></div><div style='display:table-header-group'><div style='display:table-row'><div id=h2 style='display:table-cell'>h2</div></div></div></div>",
            |dom, table| {
                assert_eq!(
                    table
                        .cells
                        .iter()
                        .map(|c| dom.attr(c.b.node, "id").unwrap())
                        .collect::<Vec<_>>(),
                    ["h1", "b", "f2", "h2", "f1"]
                );
            },
        );
    }

    #[test]
    fn nested_tables_keep_their_formatting_context_at_every_depth() {
        let depth = 40;
        let mut html = "content".to_owned();
        for index in 0..depth {
            html = format!(
                "<table {}><tr><td>{html}</td><td>adjacent</td></tr></table>",
                if index == depth - 1 { "id=table" } else { "" }
            );
        }
        table(&html, |_, table| {
            fn nested(node: &BoxNode) -> usize {
                match &node.content {
                    Content::Table(table) => {
                        1 + table.cells.iter().map(|c| nested(&c.b)).sum::<usize>()
                    }
                    Content::Blocks(children) => children.iter().map(|c| nested(c)).sum(),
                    _ => 0,
                }
            }
            assert_eq!(
                1 + table.cells.iter().map(|c| nested(&c.b)).sum::<usize>(),
                depth
            );
        });
    }
}

/// Build the box tree for a document. `None` when there is no root element
/// (nothing to render).
#[cfg(test)]
pub(crate) fn build(
    dom: &Dom,
    base: &Url,
    controls: &ControlMap,
    forms: &[Form],
    vp: Vp,
) -> Option<BoxNode> {
    build_document(dom, base, controls, forms, vp, false).map(Arc::unwrap_or_clone)
}

pub(super) fn build_document(
    dom: &Dom,
    base: &Url,
    controls: &ControlMap,
    forms: &[Form],
    vp: Vp,
    reuse: bool,
) -> Option<SharedBox> {
    dom.flush_style_invalidations();
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
        reuse,
    };
    match b.element(root) {
        Built::Block(bx) => Some(bx),
        Built::Inline(inl) => Some(Arc::new(BoxNode {
            node: root,
            style: BoxStyle::of(dom, root, vp),
            content: Content::Inlines(vec![inl]),
            marker: None,
            marker_image: None,
            marker_inside: false,
            oof: Vec::new(),
        })),
        _ => None,
    }
}

/// Build a box tree rooted at an arbitrary element `boundary` (an incremental-
/// layout relayout boundary — a scroll region or an inline IFC box), for a
/// subtree fragment re-lay (incremental-layout contract). Same machinery as
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
    dom.flush_style_invalidations();
    let mut b = Builder {
        dom,
        base,
        controls,
        forms,
        vp,
        lists: Vec::new(),
        reuse: false,
    };
    match b.element(boundary) {
        Built::Block(bx) => Some(Arc::unwrap_or_clone(bx)),
        Built::Inline(inl) => Some(BoxNode {
            node: boundary,
            style: BoxStyle::of(dom, boundary, vp),
            content: Content::Inlines(vec![inl]),
            marker: None,
            marker_image: None,
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
#[derive(Clone, Debug)]
pub(super) enum Built {
    Block(SharedBox),
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
/// what remains here has no terminal rendering. HTML Rendering
/// #hidden-elements hides `head`, `title`, `style`, `script` and the other
/// metadata elements with an ordinary UA `display: none` rule instead, so an
/// author's `display` brings them back (`title { display: block }` shows a
/// heading written as a body `<title>`, as in Gecko and Blink). A scripting
/// UA hides `noscript` with `!important`; a template's contents are not its
/// children.
const SKIP: &[&str] = &["math", "noscript", "template", "wbr", "area", "map"];

/// How an `embed` or `object` element renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Embedded {
    /// HTML #the-embed-element: it represents nothing; no box.
    Nothing,
    /// HTML Rendering #embedded-content-rendering-rules: a replaced element.
    Replaced,
    /// HTML #the-object-element "fallback": the object represents its
    /// children and renders as an ordinary element.
    Fallback,
}

/// HTML #the-embed-element / #the-object-element and HTML Rendering
/// #embedded-content-rendering-rules, for a user agent without plugins
/// that does not load these elements' resources. `None` for other elements.
///
/// An `embed` with a non-empty `src` is a replaced element; one with no
/// `src` and no `type` represents nothing. Gecko and Blink agree on both.
/// For `type` alone or an empty `src` (which never runs the setup steps
/// or displays no plugin) Blink still draws a box and Gecko draws none;
/// TRust follows Gecko, consistent with representing nothing.
///
/// An `object` represents its resource (an image or a content navigable)
/// only once that resource is available; until then, and whenever it
/// cannot be shown, the object "represents the element's children"
/// (#the-object-element, the steps labeled fallback). TRust fetches no
/// object resource, so it never reaches that state: an object always shows
/// its fallback content, never a blank box in the resource's place.
pub(crate) fn embedded_representation(dom: &Dom, id: NodeId) -> Option<Embedded> {
    if dom.namespace_uri(id) != Some("http://www.w3.org/1999/xhtml") {
        return None;
    }
    match dom.tag_name(id)? {
        "embed" => Some(if dom.attr(id, "src").is_some_and(|src| !src.is_empty()) {
            Embedded::Replaced
        } else {
            Embedded::Nothing
        }),
        "object" => Some(Embedded::Fallback),
        _ => None,
    }
}

struct Builder<'a> {
    dom: &'a Dom,
    base: &'a Url,
    controls: &'a ControlMap,
    forms: &'a [Form],
    vp: Vp,
    /// Open lists' counters: `(next value, step)` per nesting level (`<ol
    /// reversed>` counts down — HTML §4.4.5, through zero into negatives).
    lists: Vec<(i64, i64)>,
    reuse: bool,
}

impl Builder<'_> {
    fn element(&mut self, id: NodeId) -> Built {
        if self.reuse {
            let cached = self.dom.box_tree_cache.borrow_mut().get(id, &self.lists);
            if let Some((built, lists)) = cached {
                self.lists = lists;
                return built;
            }
        }
        let lists = self.lists.clone();
        let built = self.element_uncached(id);
        if self.reuse {
            // A changed counter/context can rebuild a box even when its own
            // DOM node was not dirty. Its previous flow result is not valid.
            self.dom.layout_cache.borrow_mut().invalidate(id);
            self.dom
                .box_tree_cache
                .borrow_mut()
                .insert(id, lists, &self.lists, &built);
        }
        built
    }

    fn element_uncached(&mut self, id: NodeId) -> Built {
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
        // HTML Rendering §15.4.1: iframe/frame are replaced elements. A
        // realized nested document still needs its own viewport border box;
        // projecting its body as an unconstrained sibling loses the frame's
        // CSS width/height, containing block, and clipping (notably hCaptcha).
        // Keep the body as the contents of one independently laid atomic box,
        // with the standard 300×150 default object size when neither CSS nor
        // dimension attributes supplies a size.
        if matches!(tag, "iframe" | "frame") {
            return self.frame(id, disp);
        }
        // HTML Rendering #embedded-content-rendering-rules: an `embed`, and an
        // `object` representing its resource, are replaced elements too.
        // TRust neither has plugins nor loads their resources, so the box is
        // an empty one of the same default object size, its width/height
        // attributes mapping to the dimension properties (#dimRendering).
        // A hidden 0×0 music player is still an atomic inline on its own
        // line (CSS 2 §9.4.2), not a phantom line.
        match embedded_representation(self.dom, id) {
            Some(Embedded::Nothing) => return Built::Skip,
            Some(Embedded::Replaced) => return self.frame(id, disp),
            Some(Embedded::Fallback) | None => {}
        }
        // Replaced elements are atomic regardless of their content model.
        if tag == "br" {
            return Built::Inline(Inline::Br);
        }
        // Inline SVG is a replaced box whose pixels come from the shared image
        // pipeline. Keep the SVG node and its computed box intact; direct
        // layout no longer mutates a presentation clone into an `<img>`.
        //
        // Classification as replaced does not exempt SVG from CSS Position 3
        // §2: an absolutely positioned box is still out of flow. Feed SVG into
        // the same position/float classification below as every other replaced
        // element instead of returning an in-flow atom early.
        let rep = if let Some(source) = self.dom.content_replacement_image(id) {
            Replaced::Atom(AtomKind::Img {
                url: self
                    .dom
                    .style_resource_base(id, self.base)
                    .join(&source)
                    .ok()
                    .map(|u| u.to_string()),
                density: 1.0,
                dimension_source: id,
                alt: String::new(),
                control: None,
            })
        } else if tag == "svg" {
            match self.dom.svg_image_data(id, Some(self.base)) {
                Some((source, alt)) => Replaced::Atom(AtomKind::Img {
                    url: Some(source),
                    density: 1.0,
                    dimension_source: id,
                    alt,
                    control: None,
                }),
                None => Replaced::Skip,
            }
        } else {
            self.replaced(id, tag)
        };
        if matches!(rep, Replaced::Skip) {
            return Built::Skip;
        }
        if let Replaced::Atom(kind) = &rep
            && let Some(alt) = self.alt_text_box(id, disp, kind)
        {
            let alt = Arc::new(alt);
            return if Pos::of(self.dom, id).out_of_flow() {
                Built::Inline(Inline::OutOfFlow(alt))
            } else if super::float::float_side(self.dom, id).is_some() {
                Built::Inline(Inline::Float(alt))
            } else if atomic_inline_disp(self.dom, id).is_some() {
                Built::Inline(Inline::AtomBox(alt))
            } else {
                Built::Block(alt)
            };
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
                    marker_image: None,
                    marker_inside: false,
                    oof: Vec::new(),
                },
                _ => match blockify(disp) {
                    Disp::Table => self.table(id),
                    d => self.container(id, d),
                },
            };
            return Built::Inline(Inline::OutOfFlow(Arc::new(b)));
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
                    marker_image: None,
                    marker_inside: false,
                    oof: Vec::new(),
                },
                _ => match blockify(disp) {
                    Disp::Table => self.table(id),
                    d => self.container(id, d),
                },
            };
            return Built::Inline(Inline::Float(Arc::new(b)));
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
            return Built::Inline(Inline::AtomBox(Arc::new(b)));
        }
        match disp {
            Disp::Table => Built::Block(Arc::new(self.table(id))),
            Disp::Block | Disp::ListItem | Disp::Flex | Disp::Grid => {
                Built::Block(Arc::new(self.container(id, disp)))
            }
            Disp::Inline => {
                let kids = self.children(id);
                if kids.iter().any(Built::is_block) {
                    // Block-in-inline: promote (see module docs). The element
                    // is still a non-replaced inline box, to which the sizing
                    // properties do not apply (CSS 2 §§10.2, 10.4, 10.5, 10.7;
                    // css-sizing-4 #aspect-ratio): its blocks keep the full
                    // width, as with an object's fallback whose dimension
                    // attributes map to width/height (HTML #dimRendering).
                    let mut style = BoxStyle::of(self.dom, id, self.vp);
                    style.width = super::value::Len::Auto;
                    style.height = super::value::Len::Auto;
                    style.min_width = super::value::Len::Auto;
                    style.min_height = super::value::Len::Auto;
                    style.max_width = super::value::Len::None;
                    style.max_height = super::value::Len::None;
                    style.aspect_ratio = None;
                    Built::Block(Arc::new(self.assemble(id, style, kids, None, None, false)))
                } else {
                    Built::Inline(Inline::Box {
                        node: id,
                        style: Arc::new(BoxStyle::of(self.dom, id, self.vp)),
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

    fn frame(&mut self, id: NodeId, disp: Disp) -> Built {
        let root = self.dom.frame_root(id);
        let mut style = BoxStyle::of(self.dom, id, self.vp);
        let dimension = |name: &str, fallback: f32| {
            // Iframe, embed and object dimension attributes already
            // participate in the cascade. An authored auto must use the
            // default object size, not revive the lower-priority attribute.
            // Legacy <frame> still uses its existing fallback path.
            if self.dom.tag_name(id) != Some("frame") {
                return fallback;
            }
            self.dom
                .attr(id, name)
                .and_then(|value| value.trim().parse::<f32>().ok())
                .filter(|value| value.is_finite() && *value >= 0.0)
                .unwrap_or(fallback)
        };
        if matches!(style.width, super::value::Len::Auto) {
            style.width = super::value::Len::px(dimension("width", 300.0));
        }
        if matches!(style.height, super::value::Len::Auto) {
            style.height = super::value::Len::px(dimension("height", 150.0));
        }
        // HTML Rendering #the-page and CSS Display #root: the content box
        // contains a whole Document, including its independent root box.
        // Skipping HTML loses its margins, borders, transforms, display type
        // and CSSOM geometry. BODY is an ordinary child formatting box.
        let root = root.and_then(|root| match self.element(root) {
            Built::Block(root) => Some(root),
            Built::Inline(inline) => Some(Arc::new(BoxNode {
                node: crate::layout2::NO_NODE,
                style: BoxStyle::anonymous(),
                content: Content::Inlines(vec![inline]),
                marker: None,
                marker_image: None,
                marker_inside: false,
                oof: Vec::new(),
            })),
            Built::Hoist(_) | Built::Skip => None,
        });
        let frame = BoxNode {
            node: id,
            style,
            content: Content::Blocks(root.into_iter().collect()),
            marker: None,
            marker_image: None,
            marker_inside: false,
            oof: Vec::new(),
        };
        if Pos::of(self.dom, id).out_of_flow() {
            return Built::Inline(Inline::OutOfFlow(Arc::new(frame)));
        }
        if super::float::float_side(self.dom, id).is_some() {
            return Built::Inline(Inline::Float(Arc::new(frame)));
        }
        match disp {
            Disp::Block | Disp::ListItem | Disp::Flex | Disp::Grid | Disp::Table => {
                Built::Block(Arc::new(frame))
            }
            _ => Built::Inline(Inline::AtomBox(Arc::new(frame))),
        }
    }

    /// Classify a replaced element (shared by the in-flow and out-of-flow
    /// paths): its atom kind, a skip (nothing to draw), or not-replaced.
    fn replaced(&mut self, id: NodeId, tag: &str) -> Replaced {
        if tag == "canvas" {
            return Replaced::Atom(AtomKind::Img {
                // Canvas pixels are captured at paint, not serialized into an
                // image request while building (or reusing) layout geometry.
                url: None,
                density: 1.0,
                dimension_source: id,
                alt: String::new(),
                control: None,
            });
        }
        if tag == "img" {
            let selected = self.image_src(id);
            return Replaced::Atom(AtomKind::Img {
                url: selected.as_ref().map(|selected| selected.source.clone()),
                density: selected.as_ref().map_or(1.0, |selected| selected.density),
                dimension_source: selected
                    .as_ref()
                    .map_or(id, |selected| selected.dimension_source),
                alt: self
                    .dom
                    .attr(id, "alt")
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string(),
                control: None,
            });
        }
        // HTML Rendering #images-3 renders an input in the Image Button state
        // like an img: a replaced element showing its `src` (#dimRendering
        // maps its width and height), falling back to its alt text. The
        // picture remains the form's submit control.
        if tag == "input" && self.dom.input_type(id) == "image" {
            return Replaced::Atom(AtomKind::Img {
                url: self
                    .dom
                    .attr(id, "src")
                    .map(str::trim)
                    .filter(|src| !src.is_empty())
                    .and_then(|src| self.base.join(src).ok())
                    .map(|url| url.to_string()),
                density: 1.0,
                dimension_source: id,
                alt: self
                    .dom
                    .attr(id, "alt")
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string(),
                control: self.controls.get(&id).copied(),
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
            // HTML Rendering §15.5.3 puts a button's authored child boxes in
            // its anonymous button content box. Live pages retain the real
            // form field for submission semantics, but the canonical box tree
            // must flow those authored children instead of replacing them
            // with the terminal adapter's synthetic "Button" atom label.
            if tag == "button" && self.dom.render_live() {
                return Replaced::No;
            }
            match mapped {
                Some((form, field)) => return Replaced::Atom(AtomKind::Control { form, field }),
                // An unmapped input/select has no widget to draw; an
                // unmapped button/textarea flows as a normal element — its
                // visible content renders and `data-trust-click` supplies the
                // live-page activation semantics without an extra layout box.
                None if matches!(tag, "input" | "select") => return Replaced::Skip,
                None => {}
            }
        }
        // HTML #attr-contenteditable changes editability, not CSS display or
        // replaced-element status. Keep the editor's authored descendants
        // (including styled paragraphs and generated placeholders). The form
        // binding remains available for input/focus without replacing paint.
        Replaced::No
    }

    /// HTML Rendering #images-3: an img representing its alt text is a
    /// non-replaced element whose content is that text. Displayed `inline`
    /// it is an inline box, which the inline formatting context lays as the
    /// text itself; with any other display (the common `img { display:
    /// inline-block }` reset, a block, a float, a positioned box) it is a box
    /// of that display holding the text. #dimRendering still maps the img's
    /// width and height attributes to its dimension properties, and their
    /// pixel pair to `aspect-ratio: auto w / h`, which a non-replaced box
    /// uses; author declarations outrank both hints.
    fn alt_text_box(&self, id: NodeId, disp: Disp, kind: &AtomKind) -> Option<BoxNode> {
        let AtomKind::Img {
            url,
            dimension_source,
            alt,
            ..
        } = kind
        else {
            return None;
        };
        let inline = disp == Disp::Inline
            && atomic_inline_disp(self.dom, id).is_none()
            && !Pos::of(self.dom, id).out_of_flow()
            && super::float::float_side(self.dom, id).is_none();
        if inline
            || !super::replaced::represents_alt_text(
                self.dom,
                id,
                *dimension_source,
                url.as_deref(),
                self.vp,
            )
        {
            return None;
        }
        let mut style = BoxStyle::of(self.dom, id, self.vp);
        if !self.dom.author_declares(id, "width")
            && let Some(width) =
                super::replaced::dimension_attribute(self.dom, *dimension_source, "width")
        {
            style.width = width;
        }
        if !self.dom.author_declares(id, "height")
            && let Some(height) =
                super::replaced::dimension_attribute(self.dom, *dimension_source, "height")
        {
            style.height = height;
        }
        if !self.dom.author_declares(id, "aspect-ratio")
            && let Some(ratio) =
                super::replaced::dimension_attribute_ratio(self.dom, *dimension_source)
        {
            style.aspect_ratio = Some(ratio);
        }
        Some(BoxNode {
            node: id,
            style,
            content: Content::Inlines(vec![Inline::Text(alt.clone())]),
            marker: None,
            marker_image: None,
            marker_inside: false,
            oof: Vec::new(),
        })
    }

    /// A replaced element: inline-level by default, block-level when its
    /// computed display says so (`display:block` images stack on their own
    /// line and can center through auto margins).
    fn atom(&mut self, id: NodeId, disp: Disp, kind: AtomKind) -> Built {
        let atom = Atom { node: id, kind };
        let style = BoxStyle::of(self.dom, id, self.vp);
        // CSS Transforms 1 #transform-rendering applies to inline replaced
        // elements too. Retain their stacking context as an atomic inline
        // fragment, so its background, contents and hit region share the
        // same transform/compositing scope without changing inline flow.
        let stacking_context = style.stacking_context(false);
        match disp {
            Disp::Block | Disp::ListItem | Disp::Flex | Disp::Grid => {
                Built::Block(Arc::new(BoxNode {
                    node: id,
                    style,
                    content: Content::Atomic(atom),
                    marker: None,
                    marker_image: None,
                    marker_inside: false,
                    oof: Vec::new(),
                }))
            }
            _ if stacking_context => Built::Inline(Inline::AtomBox(Arc::new(BoxNode {
                node: id,
                style,
                content: Content::Atomic(atom),
                marker: None,
                marker_image: None,
                marker_inside: false,
                oof: Vec::new(),
            }))),
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
        let (marker, marker_image, inside) = if disp == Disp::ListItem {
            self.marker(id)
        } else {
            (None, None, false)
        };
        let kids = self.children(id);
        if list {
            self.lists.pop();
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
                marker_image,
                marker_inside: inside,
                oof,
            };
        }
        let mut node = self.assemble(
            id,
            BoxStyle::of(self.dom, id, self.vp),
            kids,
            marker,
            marker_image,
            inside,
        );
        if self.dom.has_first_letter_style(id) {
            self.first_letter(id, &mut node.content);
        }
        node
    }

    /// CSS Pseudo 4 #first-letter-styling: the `::first-letter` box is an
    /// inline box around its text, or a float when it floats; `display` and
    /// `position` do not apply to it.
    fn first_letter(&self, id: NodeId, content: &mut Content) {
        let mut style = BoxStyle::of_pseudo(self.dom, id, PseudoEl::FirstLetter, self.vp);
        style.position = Pos::Static;
        style.inset = std::array::from_fn(|_| super::value::Len::Auto);
        let _ = super::first_letter::wrap(content, &mut |kids| {
            if style.float.is_some() {
                Inline::Float(Arc::new(BoxNode {
                    node: crate::layout2::NO_NODE,
                    style: style.clone(),
                    content: Content::Inlines(kids),
                    marker: None,
                    marker_image: None,
                    marker_inside: false,
                    oof: Vec::new(),
                }))
            } else {
                Inline::Box {
                    node: crate::layout2::NO_NODE,
                    style: Arc::new(style.clone()),
                    kids: kids.into(),
                }
            }
        });
    }

    /// css-flexbox §4: each in-flow element child becomes a flex item
    /// (blockified — an inline box turns into a block-level box holding its
    /// inline content); each contiguous run of text becomes an anonymous
    /// item; a run of only collapsible white space generates nothing.
    /// Out-of-flow children don't participate (§4.1) — returned separately.
    fn itemize(&mut self, kids: Vec<Built>) -> (Vec<SharedBox>, Vec<(usize, SharedBox)>) {
        let mut items: Vec<SharedBox> = Vec::new();
        let mut oof = Vec::new();
        let mut run: Vec<Inline> = Vec::new();
        let flush = |run: &mut Vec<Inline>, items: &mut Vec<SharedBox>| {
            if run.iter().any(inline_has_content) {
                // A `<br>` with no text beside it is an item of its own, but
                // an empty one (as in Gecko and Blink): it takes a grid cell
                // and flex gaps, not a line. Next to text it breaks the line
                // inside that text's anonymous item.
                let only_breaks = run.iter().all(|inline| {
                    matches!(inline, Inline::Br)
                        || matches!(inline, Inline::Text(text) if text.chars().all(is_collapsible_space))
                });
                if only_breaks {
                    run.clear();
                }
                items.push(Arc::new(BoxNode {
                    node: crate::layout2::NO_NODE,
                    style: BoxStyle::anonymous(),
                    content: Content::Inlines(std::mem::take(run)),
                    marker: None,
                    marker_image: None,
                    marker_inside: false,
                    oof: Vec::new(),
                }));
            } else {
                run.clear();
            }
        };
        for k in kids {
            match k {
                Built::Block(b) => {
                    flush(&mut run, &mut items);
                    items.push(b);
                }
                Built::Inline(Inline::OutOfFlow(b)) => {
                    flush(&mut run, &mut items);
                    oof.push((items.len(), b));
                }
                // css-flexbox §4.1 / css-grid §6: `float` is ignored on a
                // flex/grid item — the blockified box becomes an ordinary item.
                Built::Inline(Inline::Float(b)) => {
                    flush(&mut run, &mut items);
                    items.push(b);
                }
                Built::Inline(Inline::Box { node, style, kids }) => {
                    flush(&mut run, &mut items);
                    items.push(Arc::new(BoxNode {
                        node,
                        style: (*style).clone(),
                        content: Content::Inlines(kids.to_vec()),
                        marker: None,
                        marker_image: None,
                        marker_inside: false,
                        oof: Vec::new(),
                    }));
                }
                Built::Inline(Inline::Atom(a)) => {
                    flush(&mut run, &mut items);
                    items.push(Arc::new(BoxNode {
                        node: a.node,
                        style: BoxStyle::of(self.dom, a.node, self.vp),
                        content: Content::Atomic(a),
                        marker: None,
                        marker_image: None,
                        marker_inside: false,
                        oof: Vec::new(),
                    }));
                }
                // css-flexbox §4.1 / css-grid §6: an atomic inline box child of
                // a flex/grid container is BLOCKIFIED into an ordinary item (its
                // atomic-inline-ness is stripped); the inner box already carries
                // the blockified content.
                Built::Inline(Inline::AtomBox(b)) => {
                    flush(&mut run, &mut items);
                    items.push(b);
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
    fn marker(&mut self, id: NodeId) -> (Option<String>, Option<String>, bool) {
        // Retain the list sequence in box-cache keys. CSS counter values use
        // the canonical flattened-tree pass below, including author overrides.
        if let Some(v) = self
            .dom
            .attr(id, "value")
            .and_then(|v| v.trim().parse::<i64>().ok())
            && let Some(top) = self.lists.last_mut()
        {
            top.0 = v;
        }
        if let Some(top) = self.lists.last_mut() {
            top.0 = top.0.saturating_add(top.1);
        }
        let inside = matches!(
            self.dom
                .computed_value_resolved(id, "list-style-position")
                .as_deref(),
            Some("inside")
        );
        // CSS Lists 3 #content-property: `content` on the ::marker other than
        // `normal` fills the marker box as for ::before (its images are not
        // rendered yet), ahead of list-style-image and list-style-type (even
        // `none`); `content: none` generates no marker box.
        if let Some(items) = self.dom.marker_content(id) {
            let text: String = items
                .into_iter()
                .filter_map(|item| match item {
                    crate::dom::GeneratedContent::Text(text) => Some(text),
                    crate::dom::GeneratedContent::Image(_) => None,
                })
                .collect();
            return ((!text.is_empty()).then_some(text), None, inside);
        }
        let image = self
            .dom
            .computed_value_resolved(id, "list-style-image")
            .and_then(|value| Self::list_style_image_url(&value))
            .map(|source| {
                // CSS Values 4 §4.5 gives URL values a base-dependent absolute
                // identity. Resolve it with the presentation base supplied to
                // layout, as ordinary replaced images are: graphical paint
                // used to fix this downstream, but the terminal decoded-image
                // cache has no URL base and therefore could never find a
                // relative marker image.
                self.dom
                    .style_resource_base(id, self.base)
                    .join(&source)
                    .map_or(source, |url| url.to_string())
            });
        let kind = self
            .dom
            .computed_value_resolved(id, "list-style-type")
            .unwrap_or_else(|| "disc".to_string());
        // CSS Lists 3 §3.2: a valid list-style-image replaces the type marker
        // and is followed by one U+0020 space. If no usable image is present,
        // retain the ordinary counter marker as the fallback.
        let text = image
            .as_ref()
            .map(|_| " ".to_string())
            .unwrap_or_else(|| self.dom.css_list_marker(id, kind.trim()));
        ((!text.is_empty()).then_some(text), image, inside)
    }

    /// Extract a URL image from a computed `list-style-image`. CSS Lists also
    /// permits other `<image>` functions; those are left to the normal marker
    /// fallback until the graphical image pipeline can rasterize them.
    fn list_style_image_url(value: &str) -> Option<String> {
        let value = value.trim();
        let open = value.find('(')?;
        let close = value.rfind(')')?;
        if close <= open || !value[..open].trim().eq_ignore_ascii_case("url") {
            return None;
        }
        let source = value[open + 1..close].trim().trim_matches(['\'', '"']);
        (!source.is_empty()).then(|| source.to_string())
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
        // CSS Shadow 1 #flattening precedes CSS Display 3 #box-generation:
        // assigned nodes remain children when contents removes a slot's box.
        // Keep nested slots as nodes so authored display can create a box
        // (or suppress their subtree). Do not flatten away their styles.
        let child_ids = if let Some(shadow) = self.dom.shadow_root(id) {
            self.dom.children(shadow)
        } else if self.dom.tag_name(id) == Some("slot") {
            let assigned = self.dom.slot_assigned_nodes(id);
            if assigned.is_empty() {
                self.dom.children(id)
            } else {
                assigned
            }
        } else {
            self.dom.children(id)
        };
        let mut out = self.build_child_list(&child_ids, closed_details);
        // Living pages used to gain these compact handles as synthetic HTML
        // during serialization. Direct layout keeps the canonical DOM intact
        // and generates the equivalent anonymous UA content in the box tree.
        if let Some(text) = self.dom.render_clickable_fallback(id) {
            out.insert(0, Built::Inline(Inline::Text(text)));
        }
        // CSS Pseudo 4 §4.1: generated `::before`/`::after` boxes are the
        // originating element's first/last children. `content:""` still
        // creates a fully styleable EMPTY box; dropping it loses percentage
        // padding aspect-ratio reservations and collapses abspos descendants.
        // Live snapshots bake content + pseudo declarations into data attrs;
        // direct layouts read the same values from the resident cascade.
        if let Some(pseudo) = self.pseudo(id, PseudoEl::Before) {
            match pseudo {
                Built::Hoist(kids) => {
                    out.splice(0..0, kids);
                }
                other => out.insert(0, other),
            }
        }
        if let Some(pseudo) = self.pseudo(id, PseudoEl::After) {
            match pseudo {
                Built::Hoist(kids) => out.extend(kids),
                other => out.push(other),
            }
        }
        out
    }

    /// Build one generated-content pseudo as a real child box. Pseudo boxes do
    /// not have DOM node identities, so their fragments use `NO_NODE`; their
    /// originating element remains the inheritance source in
    /// `pseudo_layout_value` and the surrounding inline context.
    fn pseudo(&self, id: NodeId, which: PseudoEl) -> Option<Built> {
        let content = self.dom.pseudo_content_items(id, which)?;
        let kids: Vec<Inline> = content
            .into_iter()
            .filter_map(|item| match item {
                crate::dom::GeneratedContent::Text(text) => {
                    (!text.is_empty()).then_some(Inline::Text(text))
                }
                crate::dom::GeneratedContent::Image(source) => {
                    let url = self
                        .dom
                        .style_resource_base(id, self.base)
                        .join(&source)
                        .ok()?
                        .to_string();
                    Some(Inline::Atom(Atom {
                        node: crate::layout2::NO_NODE,
                        kind: AtomKind::GeneratedImage { url },
                    }))
                }
            })
            .collect();
        let display = self
            .dom
            .pseudo_layout_value(id, which, "display")
            .unwrap_or_else(|| "inline".to_string());
        let display = display.trim().to_ascii_lowercase();
        if display == "none" {
            return None;
        }
        if display == "contents" {
            return Some(Built::Hoist(kids.into_iter().map(Built::Inline).collect()));
        }

        let style = BoxStyle::of_pseudo(self.dom, id, which, self.vp);
        let block_level = matches!(
            display.as_str(),
            "block" | "flow-root" | "list-item" | "flex" | "grid" | "table"
        );
        if style.position.out_of_flow() || style.float.is_some() || block_level {
            let b = BoxNode {
                node: crate::layout2::NO_NODE,
                style,
                content: Content::Inlines(kids),
                marker: None,
                marker_image: None,
                marker_inside: false,
                oof: Vec::new(),
            };
            if b.style.position.out_of_flow() {
                return Some(Built::Inline(Inline::OutOfFlow(Arc::new(b))));
            }
            if b.style.float.is_some() {
                return Some(Built::Inline(Inline::Float(Arc::new(b))));
            }
            return Some(Built::Block(Arc::new(b)));
        }
        if matches!(
            display.as_str(),
            "inline-block" | "inline-flex" | "inline-grid" | "inline-table"
        ) {
            return Some(Built::Inline(Inline::AtomBox(Arc::new(BoxNode {
                node: crate::layout2::NO_NODE,
                style,
                content: Content::Inlines(kids),
                marker: None,
                marker_image: None,
                marker_inside: false,
                oof: Vec::new(),
            }))));
        }
        Some(Built::Inline(Inline::Box {
            node: crate::layout2::NO_NODE,
            style: Arc::new(style),
            kids: kids.into(),
        }))
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
        marker_image: Option<String>,
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
                marker_image,
                marker_inside,
                oof: Vec::new(),
            };
        }
        let mut blocks: Vec<SharedBox> = Vec::new();
        let mut run: Vec<Inline> = Vec::new();
        let flush = |run: &mut Vec<Inline>, blocks: &mut Vec<SharedBox>| {
            if run.iter().any(inline_has_content) {
                blocks.push(Arc::new(BoxNode {
                    node: crate::layout2::NO_NODE,
                    style: BoxStyle::anonymous(),
                    content: Content::Inlines(std::mem::take(run)),
                    marker: None,
                    marker_image: None,
                    marker_inside: false,
                    oof: Vec::new(),
                }));
            } else {
                run.clear();
            }
        };
        for k in kids {
            match k {
                Built::Block(b) => {
                    flush(&mut run, &mut blocks);
                    blocks.push(b);
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
            marker_image,
            marker_inside,
            oof: Vec::new(),
        }
    }

    /// Build a `display:table` element's box (CSS 2.1 §17). Nesting does not
    /// change the element's display type. Intrinsic memoization bounds repeated
    /// probes; optional cache limits must not replace a table with block flow.
    fn table(&mut self, id: NodeId) -> BoxNode {
        let mut style = BoxStyle::of(self.dom, id, self.vp);
        // CSS Tables 3 #collapsed-style-overrides: table-root padding has
        // zero used value in the collapsed border model. Cell padding still
        // applies, including cells generated by the anonymous-table fixup.
        if self
            .dom
            .computed_value_resolved(id, "border-collapse")
            .as_deref()
            == Some("collapse")
        {
            style.padding = std::array::from_fn(|_| super::value::Len::px(0.0));
        }
        let fixed_layout = self
            .dom
            .computed_value_resolved(id, "table-layout")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("fixed"));

        // Captions (§17.4): `table-caption` children render as block boxes
        // above the grid, or below it for `caption-side: bottom`.
        let mut top_captions = Vec::new();
        let mut bottom_captions = Vec::new();
        for c in self.dom.flat_children(id) {
            if self.dom.effective_display(c).as_deref() != Some("table-caption")
                || Pos::of(self.dom, c).out_of_flow()
            {
                continue;
            }
            let bottom = self
                .dom
                .computed_value_resolved(c, "caption-side")
                .as_deref()
                .map(str::trim)
                == Some("bottom");
            let cap = Arc::new(self.container(c, Disp::Block));
            if bottom {
                bottom_captions.push(cap);
            } else {
                top_captions.push(cap);
            }
        }

        // Rows in visual order (§17.2.1: header group → body/implicit rows →
        // footer group), placed on the grid with `colspan`/`rowspan`.
        let rows = self.table_cell_rows(id);
        let col_specs = self.table_col_specs(id);
        let (placed, ncols, nrows) = self.build_grid(&rows, col_specs.len(), fixed_layout);
        let cells = placed
            .into_iter()
            .map(|(cell, row, col, rowspan, colspan)| TableCell {
                b: cell,
                row,
                col,
                rowspan,
                colspan,
            })
            .collect();

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
            marker_image: None,
            marker_inside: false,
            oof: Vec::new(),
        }
    }

    /// The cells of each table row, in visual order (header-group rows first,
    /// then body/implicit rows, then footer-group rows — CSS 2.1 §17.2.1).
    /// CSS 2.2 §17.2.1 generates anonymous row/cell boxes around improper
    /// children. In particular, positioned children are zero-size inline
    /// placeholders during fixup: their complete boxes MUST survive for the
    /// positioned post-pass. Filtering only explicit rows/cells lost them.
    fn table_cell_rows(&mut self, table: NodeId) -> Vec<CellRows> {
        let mut header = Vec::new();
        let mut body = Vec::new();
        let mut footer = Vec::new();
        let mut implicit = Vec::new();
        let mut stray = Vec::new();
        for child in self.dom.flat_children(table) {
            let display = self.table_child_display(child);
            if matches!(
                display.as_deref(),
                Some(
                    "table-header-group"
                        | "table-footer-group"
                        | "table-row-group"
                        | "table-row"
                        | "table-column"
                        | "table-column-group"
                        | "table-caption"
                )
            ) {
                self.flush_anonymous_table_row(&mut stray, &mut implicit);
            }
            if matches!(
                display.as_deref(),
                Some("table-header-group" | "table-footer-group" | "table-row-group")
            ) && !implicit.is_empty()
            {
                body.push(std::mem::take(&mut implicit));
            }
            match display.as_deref() {
                Some("table-header-group") if header.is_empty() => {
                    header.push(self.group_rows(child))
                }
                Some("table-footer-group") if footer.is_empty() => {
                    footer.push(self.group_rows(child))
                }
                Some("table-header-group" | "table-footer-group" | "table-row-group") => {
                    body.push(self.group_rows(child))
                }
                Some("table-row") => implicit.push(self.row_cells(self.dom.flat_children(child))),
                Some("table-column" | "table-column-group" | "table-caption" | "none") => {}
                _ => stray.push(child),
            }
        }
        self.flush_anonymous_table_row(&mut stray, &mut implicit);
        if !implicit.is_empty() {
            body.push(implicit);
        }
        header.extend(body);
        header.extend(footer);
        header
    }

    fn table_child_display(&self, node: NodeId) -> Option<String> {
        if Pos::of(self.dom, node).out_of_flow() {
            None
        } else {
            self.dom.effective_display(node)
        }
    }

    /// Row groups generate missing rows around consecutive non-row children.
    fn group_rows(&mut self, group: NodeId) -> Vec<Vec<SharedBox>> {
        let mut rows = Vec::new();
        let mut pending = Vec::new();
        for child in self.dom.flat_children(group) {
            if self.table_child_display(child).as_deref() == Some("table-row") {
                self.flush_anonymous_table_row(&mut pending, &mut rows);
                rows.push(self.row_cells(self.dom.flat_children(child)));
            } else {
                pending.push(child);
            }
        }
        self.flush_anonymous_table_row(&mut pending, &mut rows);
        rows
    }

    fn flush_anonymous_table_row(
        &mut self,
        pending: &mut Vec<NodeId>,
        rows: &mut Vec<Vec<SharedBox>>,
    ) {
        if pending.is_empty() {
            return;
        }
        let cells = self.row_cells(std::mem::take(pending));
        if !cells.is_empty() {
            rows.push(cells);
        }
    }

    /// Explicit cells retain their element style; other consecutive children
    /// share one anonymous cell. Collapsible whitespace alone creates no cell.
    fn row_cells(&mut self, children: Vec<NodeId>) -> Vec<SharedBox> {
        let mut cells = Vec::new();
        let mut pending = Vec::new();
        for child in children {
            if self.table_child_display(child).as_deref() == Some("table-cell") {
                self.flush_anonymous_table_cell(&mut pending, &mut cells);
                cells.push(Arc::new(self.container(child, Disp::Block)));
            } else {
                pending.push(child);
            }
        }
        self.flush_anonymous_table_cell(&mut pending, &mut cells);
        cells
    }

    fn flush_anonymous_table_cell(
        &mut self,
        pending: &mut Vec<NodeId>,
        cells: &mut Vec<SharedBox>,
    ) {
        if pending.is_empty() {
            return;
        }
        let parent = self.dom.parent_flat(pending[0]);
        let mut kids = self.build_child_list(&std::mem::take(pending), false);
        if !kids.iter().any(|child| match child {
            Built::Block(_) => true,
            Built::Inline(inline) => inline_has_content(inline),
            _ => false,
        }) {
            return;
        }
        // Anonymous boxes inherit from their box-tree parent (§17.2.1).
        // Explicit descendant elements carry their own computed inheritance;
        // bare text needs the row/group's inline context, not the table's.
        if let Some(parent) = parent {
            for child in &mut kids {
                if let Built::Inline(Inline::Text(text)) = child {
                    *child = Built::Inline(Inline::Box {
                        node: parent,
                        style: Arc::new(BoxStyle::anonymous()),
                        kids: vec![Inline::Text(std::mem::take(text))].into(),
                    });
                }
            }
        }
        cells.push(Arc::new(self.assemble(
            crate::layout2::NO_NODE,
            BoxStyle::anonymous(),
            kids,
            None,
            None,
            false,
        )));
    }

    /// Place the rows' cells on a grid, resolving `colspan`/`rowspan` into
    /// top-left coordinates + spans (CSS 2.1 §17.5). Returns
    /// `(cell, row, col, rowspan, colspan)` in document order plus the grid's
    /// column and row counts.
    #[allow(clippy::type_complexity)]
    fn build_grid(
        &self,
        groups: &[CellRows],
        declared_columns: usize,
        fixed: bool,
    ) -> (Vec<(SharedBox, usize, usize, usize, usize)>, usize, usize) {
        let mut cells: Vec<(SharedBox, usize, usize, usize, usize)> = Vec::new();
        let mut ncols = declared_columns;
        let mut nrows = 0usize;
        let mut explicit_rows = Vec::new();
        // HTML #algorithm-for-processing-rows only queries the current row.
        // Store exclusive occupied end rows per column, not colspan*rowspan
        // individual slots. max() preserves overlaps (table-model errors).
        let mut occupied: Vec<usize> = Vec::new();
        for rows in groups {
            let start = nrows;
            let mut growing = Vec::new();
            occupied.fill(0);
            for (local_row, row) in rows.iter().enumerate() {
                let r = start + local_row;
                explicit_rows.push(r);
                nrows = nrows.max(r + 1);
                let mut c = 0usize;
                for cell in row {
                    while occupied.get(c).is_some_and(|&end| end > r) {
                        c += 1;
                    }
                    let colspan = self.cell_span(cell.node, "colspan");
                    let span = self.cell_span(cell.node, "rowspan");
                    let rowspan = span.max(1);
                    if occupied.len() < c + colspan {
                        occupied.resize(c + colspan, 0);
                    }
                    let end = if span == 0 { usize::MAX } else { r + rowspan };
                    for occupied in &mut occupied[c..c + colspan] {
                        *occupied = (*occupied).max(end);
                    }
                    if span == 0 {
                        growing.push(cells.len());
                    }
                    cells.push((cell.clone(), r, c, rowspan, colspan));
                    ncols = ncols.max(c + colspan);
                    nrows = nrows.max(r + rowspan);
                    c += colspan;
                }
            }
            // HTML #algorithm-for-ending-a-row-group: zero spans include rows
            // introduced by other spans, but cannot leak into the next group.
            for index in growing {
                cells[index].3 = nrows - cells[index].1;
            }
        }

        // CSS Tables 3 #dimensioning-the-row-column-grid--step2: tracks with
        // identical covering cell sets merge unless explicitly defined. Sets
        // change only at cell start/end boundaries; no dense matrix is needed.
        let tracks = |length: usize, explicit: Vec<usize>, columns: bool| {
            let mut kept = explicit;
            if length > 0 {
                kept.push(0);
            }
            for (_, row, col, rowspan, colspan) in &cells {
                let (start, span) = if columns {
                    (*col, *colspan)
                } else {
                    (*row, *rowspan)
                };
                kept.push(start);
                if start + span < length {
                    kept.push(start + span);
                }
            }
            kept.sort_unstable();
            kept.dedup();
            kept
        };
        let row_tracks = tracks(nrows, explicit_rows, false);
        let col_tracks = (!fixed).then(|| tracks(ncols, (0..declared_columns).collect(), true));
        for (_, row, col, rowspan, colspan) in &mut cells {
            let start = row_tracks.partition_point(|&track| track < *row);
            *rowspan = row_tracks.partition_point(|&track| track < *row + *rowspan) - start;
            *row = start;
            if let Some(tracks) = &col_tracks {
                let start = tracks.partition_point(|&track| track < *col);
                *colspan = tracks.partition_point(|&track| track < *col + *colspan) - start;
                *col = start;
            }
        }
        (
            cells,
            col_tracks.map_or(ncols, |tracks| tracks.len()),
            row_tracks.len(),
        )
    }

    /// HTML #rules-for-parsing-non-negative-integers and table span limits.
    /// CSS Tables 3 uses HTML attributes only on the corresponding HTML type.
    fn cell_span(&self, id: NodeId, attr: &str) -> usize {
        if self.dom.namespace_uri(id) != Some("http://www.w3.org/1999/xhtml")
            || !matches!(
                (self.dom.tag_name(id), attr),
                (Some("td" | "th"), "colspan" | "rowspan") | (Some("col" | "colgroup"), "span")
            )
        {
            return 1;
        }
        let Some(value) = self.dom.attr(id, attr) else {
            return 1;
        };
        let value = value.trim_start_matches(['\t', '\n', '\x0c', '\r', ' ']);
        let negative = value.starts_with('-');
        let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
        if !digits.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            return 1;
        }
        let maximum = if attr == "rowspan" {
            65_534usize
        } else {
            1000usize
        };
        let mut number = 0;
        for digit in digits.bytes().take_while(u8::is_ascii_digit) {
            number = (number * 10 + usize::from(digit - b'0')).min(maximum);
        }
        if negative && number != 0 {
            return 1;
        }
        if attr == "rowspan" {
            number
        } else {
            number.max(1)
        }
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

    /// WHATWG HTML's selected responsive-image source. One selector owns URL,
    /// density, picture ordering, media/type filtering, and dimension hints for
    /// discovery, layout, and the live `currentSrc` API.
    fn image_src(&self, id: NodeId) -> Option<crate::responsive_image::SelectedImage> {
        let selected = crate::responsive_image::select(
            self.dom,
            id,
            self.base,
            crate::layout2::Viewport::new(self.vp.w, self.vp.h),
            self.dom.device_pixel_ratio(),
        )?;
        crate::responsive_image::loadable_source(&selected).then_some(selected)
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
    if id == crate::layout2::NO_NODE {
        return None;
    }
    // HTML Rendering #tables-2 maps the width attribute to 'width' with the
    // rules for parsing non-zero dimension values (`width="450px;"` is 450).
    let raw = dom
        .computed_style(id, "width")
        .or_else(|| dom.nonzero_dimension_attr(id, "width"))?;
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
