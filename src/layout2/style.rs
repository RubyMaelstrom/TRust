//! Per-box style snapshots for the layout2 engine.
//!
//! Reads go through dom.rs's cascade (`computed_value` — the single
//! inheritance authority, memoized per epoch) exactly once per box when the
//! box tree is built; layout then works from typed values, never strings.
//! The UA stylesheet lives here too: the WHATWG HTML Rendering section's
//! margins/padding (body 8px, `p` 1em block margins, list 40px gutters, the
//! heading scale), expressed in px at snapshot time through the element's own
//! font size (`Dom::font_px` already applies the h1-h6/small/big factors).

use url::Url;

use crate::dom::{Dom, NodeId, PseudoEl};
use crate::layout2::{Emphasis, NO_NODE};
use crate::layout2::{ItemKind, TextTransform, Units, WhiteSpace, css_is_italic, css_length_px};

use super::value::{Len, Node, Vp};

/// Sides index order used throughout: Top, Right, Bottom, Left.
pub(crate) const TOP: usize = 0;
pub(crate) const RIGHT: usize = 1;
pub(crate) const BOTTOM: usize = 2;
pub(crate) const LEFT: usize = 3;

/// The CSS UI outline decoration retained with a fragment. Outlines are
/// paint-only: unlike borders they do not participate in sizing or flow.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Outline {
    pub width: f32,
    pub offset: f32,
    pub style: OutlineStyle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum OutlineStyle {
    #[default]
    None,
    Hidden,
    Solid,
    Dashed,
    Dotted,
    Double,
    Groove,
    Ridge,
    Inset,
    Outset,
    Auto,
}

impl Outline {
    pub const NONE: Self = Self {
        width: 0.0,
        offset: 0.0,
        style: OutlineStyle::None,
    };

    pub fn paints(self) -> bool {
        self.width > 0.0 && !matches!(self.style, OutlineStyle::None | OutlineStyle::Hidden)
    }
}

/// The positioning scheme (CSS 2.1 §9.3.1; css-position-3 adds `sticky`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Pos {
    Static,
    /// Laid in normal flow, offset afterwards (§9.4.3).
    Relative,
    /// Out of flow; placed against its containing block (§9.6).
    Absolute,
    /// Absolute whose containing block is the viewport (§9.6.1); pinned.
    Fixed,
    /// Identical to relative at layout — its offsets are SCROLL-driven
    /// (css-position-3 §3.4), so at the initial scroll position the box sits
    /// exactly at its flow position. Always a stacking context (§2.2).
    Sticky,
}

impl Pos {
    pub fn of(dom: &Dom, id: NodeId) -> Pos {
        match dom
            .computed_value_resolved(id, "position")
            .as_deref()
            .map(str::trim)
        {
            Some("relative") => Pos::Relative,
            Some("absolute") => Pos::Absolute,
            Some("fixed") => Pos::Fixed,
            Some("sticky") | Some("-webkit-sticky") => Pos::Sticky,
            _ => Pos::Static,
        }
    }

    /// §9.3.2: "positioned" = position other than static.
    pub fn positioned(self) -> bool {
        self != Pos::Static
    }

    /// Removed from normal flow entirely (§9.3/§9.6).
    pub fn out_of_flow(self) -> bool {
        matches!(self, Pos::Absolute | Pos::Fixed)
    }
}

/// The box-generation class of an element, resolved ONCE at box-tree build
/// (CSS 2.1 §9.2.4 `display`, through `Dom::effective_display` = author
/// cascade else the UA table). Block/inline/flex/grid/table formatting
/// contexts are all real; the remaining honest degradation is atomic
/// inline-levels (inline-block/inline-flex/inline-table) flowing as plain
/// inlines until the atomic-inline phase.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Disp {
    /// No box, no descendant boxes (`display:none`, `hidden`).
    None,
    /// No box of its own; children hoist to the parent (`display:contents`).
    Contents,
    /// Block-level block container.
    Block,
    /// Inline-level (includes the not-yet-atomic inline-block).
    Inline,
    /// Block-level with a `::marker` (CSS Lists).
    ListItem,
    /// Flex container (`flex`/`inline-flex`).
    Flex,
    /// Grid container (`grid`/`inline-grid`).
    Grid,
    /// Table wrapper (`table`/`inline-table`, or a block that holds
    /// misparented table rows — CSS 2.1 §17.2.1 "generate missing parents").
    Table,
}

pub(crate) fn display_of(dom: &Dom, id: NodeId) -> Disp {
    if dom.is_hidden(id) {
        return Disp::None;
    }
    let Some(d) = dom.effective_display(id) else {
        return Disp::None; // not an element
    };
    match d.as_str() {
        "none" => Disp::None,
        "contents" => Disp::Contents,
        "list-item" => Disp::ListItem,
        "flex" | "inline-flex" => Disp::Flex,
        "grid" | "inline-grid" => Disp::Grid,
        // inline-table degrades to a block-level table (no atomic-inline
        // boxes yet — the same honest degradation the old engine made).
        "table" | "inline-table" => Disp::Table,
        // A block that holds "proper table children" (rows/row-groups) with
        // no table of its own establishes an anonymous table around them
        // (§17.2.1) — the markdown-CSS `display:block` `<table>` (GitHub).
        "block" | "flow-root" | "inline-block" if dom.establishes_anonymous_table(id) => {
            Disp::Table
        }
        "block" | "flow-root" => Disp::Block,
        // A `table-caption` is a block box (rendered above/below the grid);
        // other table internals reaching here are misparented and block-stack.
        "table-caption" => Disp::Block,
        d if d.starts_with("table-") => Disp::Block,
        // Everything inline-level, including the not-yet-atomic inline-block.
        _ => Disp::Inline,
    }
}

/// The box-model snapshot of one element, in parse-once [`Len`]s (percentages
/// resolve against the containing block at layout). Border widths are already
/// used values in px (0 when the side's style is `none`/`hidden`); whether
/// borders paint is a frontend concern; their canonical geometry is always
/// honored and retained at fractional CSS-pixel precision.
#[derive(Clone, Debug)]
pub(crate) struct BoxStyle {
    pub margin: [Len; 4],
    pub padding: [Len; 4],
    pub border: [f32; 4],
    pub outline: Outline,
    pub width: Len,
    pub min_width: Len,
    pub max_width: Len,
    pub height: Len,
    pub min_height: Len,
    pub max_height: Len,
    /// `box-sizing: border-box` — declared width/height include border+padding.
    pub border_box: bool,
    pub position: Pos,
    /// The `top`/`right`/`bottom`/`left` inset properties (TRBL). Horizontal
    /// percentages resolve against the CB width, vertical against its height.
    pub inset: [Len; 4],
    /// `z-index`: the integer stack level; `auto` → `None` (§9.9.1).
    pub z_index: Option<i32>,
    /// The accumulated TRANSLATION of `transform` + the individual
    /// `translate` property, per axis as `(pct-of-own-border-box, px)` —
    /// the transform subset currently represented by the fragment tree
    /// (css-transforms-1; scale/rotate/skew remain future paint operations).
    pub tx: (f32, f32),
    pub ty: (f32, f32),
    /// Any non-none `transform`/`translate`: a stacking-context AND
    /// containing-block former for out-of-flow descendants (transforms-1 §3)
    /// even when the translation component is zero.
    pub has_transform: bool,
    /// Used `opacity`, clamped to the CSS `<alpha-value>` range. Retaining the
    /// number (rather than the old terminal-only boolean) lets graphical paint
    /// composite the entire stacking context as one group.
    pub opacity: f32,
    /// Declares a painted background (color ≠ transparent, or an image).
    /// Backends decide how to represent unsupported background-image syntax.
    pub bg: bool,
    /// `float` side (§9.5): the box leaves normal flow and shifts to this edge.
    /// `None` = `float:none`. An out-of-flow box computes `float:none` (§9.7),
    /// so this is `None` whenever `position` is absolute/fixed.
    pub float: Option<super::float::Side>,
    /// `clear` (§9.5.2): which sides may not be adjacent to an earlier float.
    pub clear: super::float::Clear,
}

impl BoxStyle {
    /// The style of an anonymous box (CSS 2.1 §9.2.1.1: anonymous boxes take
    /// the initial value for every non-inherited property).
    pub fn anonymous() -> BoxStyle {
        BoxStyle {
            margin: [Len::px(0.0), Len::px(0.0), Len::px(0.0), Len::px(0.0)],
            padding: [Len::px(0.0), Len::px(0.0), Len::px(0.0), Len::px(0.0)],
            border: [0.0; 4],
            outline: Outline::NONE,
            width: Len::Auto,
            min_width: Len::Auto,
            max_width: Len::None,
            height: Len::Auto,
            min_height: Len::Auto,
            max_height: Len::None,
            border_box: false,
            position: Pos::Static,
            inset: [Len::Auto, Len::Auto, Len::Auto, Len::Auto],
            z_index: None,
            tx: (0.0, 0.0),
            ty: (0.0, 0.0),
            has_transform: false,
            opacity: 1.0,
            bg: false,
            float: None,
            clear: super::float::Clear::default(),
        }
    }

    /// Snapshot `id`'s box style: author cascade first, else the UA default.
    pub fn of(dom: &Dom, id: NodeId, vp: Vp) -> BoxStyle {
        let u = Units::of(dom, id);
        let tag = dom.tag_name(id).unwrap_or("");
        let (ua_margin, ua_padding) = ua_box(dom, id, tag, u.fs);
        let (has_transform, tx, ty) = transform_translation(dom, id, u, vp);
        // CSS Custom Properties §3: substitute var() references at computed-
        // value time, before the typed layout snapshot parses lengths. Feeding
        // raw `var(--x)` into Len made a valid declaration silently fall back
        // to `auto` (and could leak an intrinsic probe width into layout).
        let cv = |prop: &str| dom.computed_value_resolved(id, prop);
        let side = |prop: &str, i: usize, ua: [f32; 4]| -> Len {
            Len::parse_or(cv(prop).as_deref(), u, vp, Len::px(ua[i]))
        };
        BoxStyle {
            margin: [
                side("margin-top", TOP, ua_margin),
                side("margin-right", RIGHT, ua_margin),
                side("margin-bottom", BOTTOM, ua_margin),
                side("margin-left", LEFT, ua_margin),
            ],
            padding: [
                side("padding-top", TOP, ua_padding),
                side("padding-right", RIGHT, ua_padding),
                side("padding-bottom", BOTTOM, ua_padding),
                side("padding-left", LEFT, ua_padding),
            ],
            border: [
                border_side(dom, id, "top", u),
                border_side(dom, id, "right", u),
                border_side(dom, id, "bottom", u),
                border_side(dom, id, "left", u),
            ],
            outline: outline_of(dom, id, u),
            width: Len::parse_or(cv("width").as_deref(), u, vp, Len::Auto),
            min_width: Len::parse_or(cv("min-width").as_deref(), u, vp, Len::Auto),
            max_width: Len::parse_or(cv("max-width").as_deref(), u, vp, Len::None),
            height: Len::parse_or(cv("height").as_deref(), u, vp, Len::Auto),
            min_height: Len::parse_or(cv("min-height").as_deref(), u, vp, Len::Auto),
            max_height: Len::parse_or(cv("max-height").as_deref(), u, vp, Len::None),
            border_box: matches!(cv("box-sizing").as_deref(), Some("border-box")),
            position: Pos::of(dom, id),
            inset: [
                Len::parse_or(cv("top").as_deref(), u, vp, Len::Auto),
                Len::parse_or(cv("right").as_deref(), u, vp, Len::Auto),
                Len::parse_or(cv("bottom").as_deref(), u, vp, Len::Auto),
                Len::parse_or(cv("left").as_deref(), u, vp, Len::Auto),
            ],
            z_index: cv("z-index").and_then(|v| v.trim().parse::<i32>().ok()),
            tx,
            ty,
            has_transform,
            opacity: dom.effective_opacity(id),
            bg: declares_background(dom, id),
            // §9.7: an out-of-flow box computes `float:none` (positioning wins).
            float: if Pos::of(dom, id).out_of_flow() {
                None
            } else {
                super::float::float_side(dom, id)
            },
            clear: super::float::clear_of(dom, id),
        }
    }

    /// Snapshot a generated `::before`/`::after` box. CSS Pseudo 4 §4 makes
    /// these fully styleable boxes which inherit from their originating
    /// element. `Dom::pseudo_layout_value` supplies that computed-value model
    /// from either the live cascade or the declarations baked into a serialized
    /// live snapshot.
    pub fn of_pseudo(dom: &Dom, id: NodeId, which: PseudoEl, vp: Vp) -> BoxStyle {
        let u = Units::of(dom, id);
        let cv = |prop: &str| dom.pseudo_layout_value(id, which, prop);
        let len = |prop: &str, default: Len| Len::parse_or(cv(prop).as_deref(), u, vp, default);
        let border = |side: &str| match cv(&format!("border-{side}-style"))
            .as_deref()
            .map(str::trim)
        {
            None | Some("none" | "hidden") => 0.0,
            _ => match cv(&format!("border-{side}-width"))
                .as_deref()
                .map(str::trim)
            {
                None | Some("medium") => 3.0,
                Some("thin") => 1.0,
                Some("thick") => 5.0,
                Some(width) => css_length_px(width, u).unwrap_or(3.0).max(0.0),
            },
        };
        let position = match cv("position").as_deref().map(str::trim) {
            Some("relative") => Pos::Relative,
            Some("absolute") => Pos::Absolute,
            Some("fixed") => Pos::Fixed,
            Some("sticky" | "-webkit-sticky") => Pos::Sticky,
            _ => Pos::Static,
        };
        let float = if position.out_of_flow() {
            None
        } else {
            match cv("float")
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("left" | "inline-start") => Some(super::float::Side::Left),
                Some("right" | "inline-end") => Some(super::float::Side::Right),
                _ => None,
            }
        };
        let clear = match cv("clear")
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("left" | "inline-start") => super::float::Clear {
                left: true,
                right: false,
            },
            Some("right" | "inline-end") => super::float::Clear {
                left: false,
                right: true,
            },
            Some("both") => super::float::Clear {
                left: true,
                right: true,
            },
            _ => super::float::Clear::default(),
        };
        let bg = |prop: &str| {
            cv(prop).is_some_and(|value| {
                let value = value.trim().to_ascii_lowercase();
                !value.is_empty()
                    && !matches!(
                        value.as_str(),
                        "none"
                            | "transparent"
                            | "initial"
                            | "inherit"
                            | "unset"
                            | "revert"
                            | "revert-layer"
                            | "rgba(0, 0, 0, 0)"
                    )
            })
        };
        BoxStyle {
            margin: [
                len("margin-top", Len::px(0.0)),
                len("margin-right", Len::px(0.0)),
                len("margin-bottom", Len::px(0.0)),
                len("margin-left", Len::px(0.0)),
            ],
            padding: [
                len("padding-top", Len::px(0.0)),
                len("padding-right", Len::px(0.0)),
                len("padding-bottom", Len::px(0.0)),
                len("padding-left", Len::px(0.0)),
            ],
            border: [
                border("top"),
                border("right"),
                border("bottom"),
                border("left"),
            ],
            outline: Outline::NONE,
            width: len("width", Len::Auto),
            min_width: len("min-width", Len::Auto),
            max_width: len("max-width", Len::None),
            height: len("height", Len::Auto),
            min_height: len("min-height", Len::Auto),
            max_height: len("max-height", Len::None),
            border_box: matches!(cv("box-sizing").as_deref(), Some("border-box")),
            position,
            inset: [
                len("top", Len::Auto),
                len("right", Len::Auto),
                len("bottom", Len::Auto),
                len("left", Len::Auto),
            ],
            z_index: cv("z-index").and_then(|value| value.trim().parse().ok()),
            // Translation of generated boxes is a paint-only extension of the
            // same model; keep the geometry standards-correct at the declared
            // box position until pseudo transforms join the compositor.
            tx: (0.0, 0.0),
            ty: (0.0, 0.0),
            has_transform: cv("transform")
                .as_deref()
                .is_some_and(|value| !matches!(value.trim(), "" | "none")),
            opacity: cv("opacity")
                .as_deref()
                .and_then(|value| {
                    let value = value.trim();
                    if let Some(percent) = value.strip_suffix('%') {
                        percent.trim().parse::<f32>().ok().map(|n| n / 100.0)
                    } else {
                        value.parse::<f32>().ok()
                    }
                })
                .unwrap_or(1.0)
                .clamp(0.0, 1.0),
            bg: bg("background-color") || bg("background-image"),
            float,
            clear,
        }
    }

    /// Whether this box forms a stacking context (§9.9.1: positioned with
    /// non-auto z-index; css-position-3 §2.2: fixed/sticky always;
    /// css-transforms-1 §3: any transform; css-color: opacity < 1;
    /// css-flexbox §4.3 / css-grid §5.4: a flex/grid ITEM with non-auto
    /// z-index, position notwithstanding — `item` says this box is one).
    pub fn stacking_context(&self, item: bool) -> bool {
        (self.position.positioned() && self.z_index.is_some())
            || matches!(self.position, Pos::Fixed | Pos::Sticky)
            || self.has_transform
            || self.opacity < 1.0
            || (item && self.z_index.is_some())
    }
}

/// Whether the element declares a rendered background: a background-color
/// that isn't fully transparent, or any background-image. (The `background`
/// shorthand was expanded into these longhands by the cascade.)
fn declares_background(dom: &Dom, id: NodeId) -> bool {
    if let Some(c) = dom.computed_value_resolved(id, "background-color") {
        let t = c.trim().to_ascii_lowercase();
        if !t.is_empty()
            && !matches!(
                t.as_str(),
                "transparent"
                    | "none"
                    | "initial"
                    | "inherit"
                    | "unset"
                    | "revert"
                    | "revert-layer"
            )
            && !zero_alpha_color(&t)
        {
            return true;
        }
    }
    if let Some(i) = dom.computed_value_resolved(id, "background-image") {
        let t = i.trim().to_ascii_lowercase();
        if !t.is_empty()
            && !matches!(
                t.as_str(),
                "none" | "initial" | "inherit" | "unset" | "revert" | "revert-layer"
            )
        {
            return true;
        }
    }
    false
}

/// The written ALPHA of a color function's argument list (`0,0,0,.5`,
/// `0 0 0 / 50%`), `None` when no alpha component is present (opaque).
fn color_args_alpha(args: &str) -> Option<f32> {
    let alpha = if let Some((_, a)) = args.rsplit_once('/') {
        a
    } else {
        let parts: Vec<&str> = args.split(',').collect();
        if parts.len() < 4 {
            return None; // no alpha component: opaque
        }
        parts[3]
    };
    let a = alpha.trim();
    let (v, pct) = match a.strip_suffix('%') {
        Some(p) => (p.trim(), true),
        None => (a, false),
    };
    let n = v.parse::<f32>().ok()?;
    Some(if pct { n / 100.0 } else { n })
}

/// A function color whose ALPHA component is written as zero
/// (`rgba(0,0,0,0)`, `rgb(0 0 0 / 0%)`, `hsla(…, 0)`) — fully transparent,
/// so not a fill. A color with no alpha component is opaque.
fn zero_alpha_color(t: &str) -> bool {
    if !(t.starts_with("rgb") || t.starts_with("hsl") || t.starts_with("hwb")) {
        return false;
    }
    let Some((_, args)) = t.split_once('(') else {
        return false;
    };
    let args = args.trim_end_matches(')');
    color_args_alpha(args).is_some_and(|v| v == 0.0)
}

/// The translation component of `transform` + the `translate` property:
/// `(has_any_transform, tx, ty)` with each axis as a linear
/// `(pct-of-own-border-box, px)` pair, summed across the function list
/// (exact when the list is translation-only; a translation mixed with
/// rotate/scale sums components — the documented quantization). An invalid
/// transform list is dropped whole, per CSS parse rules.
fn transform_translation(
    dom: &Dom,
    id: NodeId,
    u: Units,
    vp: Vp,
) -> (bool, (f32, f32), (f32, f32)) {
    let mut has = false;
    let mut tx = (0.0f32, 0.0f32);
    let mut ty = (0.0f32, 0.0f32);
    let add = |acc: &mut (f32, f32), v: &str| {
        if let Some(Len::Val(node)) = Len::parse(v, u, vp) {
            match node {
                Node::Lin { k, b } => {
                    acc.0 += k;
                    acc.1 += b;
                }
                // A min()/max() tree isn't linear in the box size; take its
                // basis-free resolution (px-only args), else contribute 0.
                n => acc.1 += n.resolve(None).unwrap_or(0.0),
            }
        }
    };
    if let Some(t) = dom.computed_value_resolved(id, "transform") {
        let t = t.trim();
        if !t.is_empty()
            && !t.eq_ignore_ascii_case("none")
            && let Some(fns) = parse_transform_list(t)
        {
            has = true;
            for (name, args) in fns {
                match name.as_str() {
                    "translate" => {
                        if let Some(x) = args.first() {
                            add(&mut tx, x);
                        }
                        if let Some(y) = args.get(1) {
                            add(&mut ty, y);
                        }
                    }
                    "translatex" => {
                        if let Some(x) = args.first() {
                            add(&mut tx, x);
                        }
                    }
                    "translatey" => {
                        if let Some(y) = args.first() {
                            add(&mut ty, y);
                        }
                    }
                    "translate3d" => {
                        if let Some(x) = args.first() {
                            add(&mut tx, x);
                        }
                        if let Some(y) = args.get(1) {
                            add(&mut ty, y);
                        }
                    }
                    // matrix(a,b,c,d,e,f): e/f are the px translation.
                    "matrix" => {
                        if let (Some(e), Some(f)) = (args.get(4), args.get(5)) {
                            tx.1 += e.trim().parse::<f32>().unwrap_or(0.0);
                            ty.1 += f.trim().parse::<f32>().unwrap_or(0.0);
                        }
                    }
                    // matrix3d(...m41,m42,m43,m44): m41/m42 translate.
                    "matrix3d" => {
                        if let (Some(e), Some(f)) = (args.get(12), args.get(13)) {
                            tx.1 += e.trim().parse::<f32>().unwrap_or(0.0);
                            ty.1 += f.trim().parse::<f32>().unwrap_or(0.0);
                        }
                    }
                    _ => {} // scale/rotate/skew/perspective: SC+CB, no offset
                }
            }
        }
    }
    // The individual `translate: x y?` property (css-transforms-2).
    if let Some(t) = dom.computed_value_resolved(id, "translate") {
        let t = t.trim();
        if !t.is_empty() && !t.eq_ignore_ascii_case("none") {
            has = true;
            let mut parts = t.split_whitespace();
            if let Some(x) = parts.next() {
                add(&mut tx, x);
            }
            if let Some(y) = parts.next() {
                add(&mut ty, y);
            }
        }
    }
    (has, tx, ty)
}

/// Parse a `transform` function list into lowercased `(name, args)` pairs.
/// `None` when any function is unrecognized/malformed — CSS drops the whole
/// declaration, so an invalid list must not form a stacking context.
fn parse_transform_list(t: &str) -> Option<Vec<(String, Vec<String>)>> {
    const KNOWN: &[&str] = &[
        "matrix",
        "translate",
        "translatex",
        "translatey",
        "scale",
        "scalex",
        "scaley",
        "rotate",
        "skew",
        "skewx",
        "skewy",
        "matrix3d",
        "translate3d",
        "translatez",
        "scale3d",
        "scalez",
        "rotate3d",
        "rotatex",
        "rotatey",
        "rotatez",
        "perspective",
    ];
    let mut out = Vec::new();
    let mut rest = t.trim();
    while !rest.is_empty() {
        let open = rest.find('(')?;
        let name = rest[..open].trim().to_ascii_lowercase();
        if !KNOWN.contains(&name.as_str()) {
            return None;
        }
        let close = rest[open..].find(')')? + open;
        let args = rest[open + 1..close]
            .split(',')
            .map(|a| a.trim().to_string())
            .collect();
        out.push((name, args));
        rest = rest[close + 1..].trim_start();
    }
    Some(out)
}

/// One side's used border width in px: 0 when the side's `border-style` is
/// absent/`none`/`hidden` (CSS 2.1 §8.5.3), else the declared width
/// (`thin`/`medium`/`thick` = 1/3/5px per the usual UA mapping; `medium` is
/// the initial width).
fn border_side(dom: &Dom, id: NodeId, side: &str, u: Units) -> f32 {
    match dom
        .computed_value_resolved(id, &format!("border-{side}-style"))
        .as_deref()
    {
        Some("none") | Some("hidden") | None => return 0.0,
        _ => {}
    }
    match dom
        .computed_value_resolved(id, &format!("border-{side}-width"))
        .as_deref()
        .map(str::trim)
    {
        None | Some("medium") => 3.0,
        Some("thin") => 1.0,
        Some("thick") => 5.0,
        Some(w) => css_length_px(w, u).unwrap_or(3.0).max(0.0),
    }
}

pub(crate) fn outline_of(dom: &Dom, id: NodeId, u: Units) -> Outline {
    let style = match dom
        .computed_value_resolved(id, "outline-style")
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("hidden") => OutlineStyle::Hidden,
        Some("solid") => OutlineStyle::Solid,
        Some("dashed") => OutlineStyle::Dashed,
        Some("dotted") => OutlineStyle::Dotted,
        Some("double") => OutlineStyle::Double,
        Some("groove") => OutlineStyle::Groove,
        Some("ridge") => OutlineStyle::Ridge,
        Some("inset") => OutlineStyle::Inset,
        Some("outset") => OutlineStyle::Outset,
        Some("auto") => OutlineStyle::Auto,
        _ => OutlineStyle::None,
    };
    let width = match dom
        .computed_value_resolved(id, "outline-width")
        .as_deref()
        .map(str::trim)
    {
        None | Some("medium") => 3.0,
        Some("thin") => 1.0,
        Some("thick") => 5.0,
        Some(w) => css_length_px(w, u).unwrap_or(3.0).max(0.0),
    };
    let offset = dom
        .computed_value_resolved(id, "outline-offset")
        .as_deref()
        .and_then(|value| css_length_px(value, u))
        .unwrap_or(0.0);
    Outline {
        width,
        offset,
        style,
    }
}

/// The WHATWG HTML Rendering section's UA margins/padding for a tag, in px
/// (`fs` = the element's own computed font size, so the `em` values scale
/// with the heading factors `Dom::font_px` already applies). Returns
/// `(margin, padding)` in TRBL order.
fn ua_box(dom: &Dom, id: NodeId, tag: &str, fs: f32) -> ([f32; 4], [f32; 4]) {
    let m0 = [0.0f32; 4];
    let p0 = [0.0f32; 4];
    let em = fs;
    let block = |v: f32| [v, 0.0, v, 0.0];
    match tag {
        "body" => ([8.0; 4], p0),
        "p" | "dl" | "pre" | "listing" | "plaintext" | "xmp" => (block(em), p0),
        "blockquote" | "figure" => ([em, 40.0, em, 40.0], p0),
        "ul" | "ol" | "menu" | "dir" => {
            // Nested lists lose their block margins (the classic UA
            // `ul ul { margin-block: 0 }` family of rules).
            let nested = std::iter::successors(dom.node(id).parent, |&p| dom.node(p).parent)
                .any(|a| matches!(dom.tag_name(a), Some("ul" | "ol" | "menu" | "dir")));
            let m = if nested { m0 } else { block(em) };
            (m, [0.0, 0.0, 0.0, 40.0])
        }
        "dd" => ([0.0, 0.0, 0.0, 40.0], p0),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let factor = match tag {
                "h1" => 0.67,
                "h2" => 0.83,
                "h3" => 1.0,
                "h4" => 1.33,
                "h5" => 1.67,
                _ => 2.33,
            };
            (block(factor * em), p0)
        }
        "hr" => (block(0.5 * em), p0),
        "fieldset" => (
            [0.0, 2.0, 0.0, 2.0],
            [0.35 * em, 0.75 * em, 0.625 * em, 0.75 * em],
        ),
        _ => (m0, p0),
    }
}

/// Horizontal alignment of an IFC's line boxes (CSS Text §7.1). Unlike the
/// old engine's `Align`, `justify` is carried and honored with fractional
/// inter-word expansion; the last line and forced-break lines stay left.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Align2 {
    Left,
    Center,
    Right,
    Justify,
}

/// The alignment governing `id`'s line boxes: the cascade's inherited
/// `text-align` when set anywhere up the chain (via `computed_value`), else
/// the HTML presentational hints — an `align` attribute or a `<center>`
/// ancestor — which inherit like text-align but never enter the cascade.
pub(crate) fn block_align(dom: &Dom, id: NodeId) -> Align2 {
    if id == NO_NODE {
        return Align2::Left;
    }
    if let Some(v) = dom.computed_value_resolved(id, "text-align")
        && let Some(a) = align_from_css(&v)
    {
        return a;
    }
    // UA sheet: `caption { text-align: center }` (CSS 2.1 §17.4 / Appendix D)
    // when the cascade resolved no author alignment.
    if dom.effective_display(id).as_deref() == Some("table-caption") {
        return Align2::Center;
    }
    let mut cur = Some(id);
    while let Some(n) = cur {
        if dom.tag_name(n) == Some("center") {
            return Align2::Center;
        }
        if let Some(a) = dom.attr(n, "align").and_then(align_from_css) {
            return a;
        }
        cur = dom.node(n).parent;
    }
    Align2::Left
}

fn align_from_css(value: &str) -> Option<Align2> {
    match value.trim().to_ascii_lowercase().as_str() {
        "left" | "start" => Some(Align2::Left),
        "center" | "-webkit-center" | "-moz-center" => Some(Align2::Center),
        "right" | "end" => Some(Align2::Right),
        "justify" => Some(Align2::Justify),
        _ => None,
    }
}

/// Within-word break opportunities (CSS Text 3 §5.2 `word-break` × §5.5
/// `overflow-wrap`), resolved to what the line breaker needs to know.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum WordBrk {
    /// Breaks only at soft-wrap opportunities; a too-long word overflows.
    Normal,
    /// `overflow-wrap: break-word` — an otherwise-overflowing word breaks at
    /// the line edge, but contributes NO min-content break opportunities.
    BreakWord,
    /// `overflow-wrap: anywhere` (and the legacy `word-break: break-word`) —
    /// like `BreakWord`, but the opportunities count for min-content too.
    Anywhere,
    /// `word-break: break-all` — a break is allowed between any two
    /// characters, mid-line included.
    BreakAll,
}

/// The inline formatting context inherited down the tree during the box-tree
/// walk: everything a text run needs to become an inline fragment. The cascade-
/// inherited pieces (white-space, transform, letter-spacing, emphasis) are
/// re-read per element through `computed_value` (which already inherits);
/// what threads MANUALLY is what the cascade doesn't model: the enclosing
/// link, the semantic `ItemKind` (heading/quote/pre coloring), the STICKY
/// `opacity:0` chain (a subtree group — a descendant cannot re-reveal it,
/// unlike `visibility`), and `font-size:0` (`None` = inherit).
#[derive(Clone, Debug)]
pub(crate) struct InlineStyle {
    pub kind: ItemKind,
    pub emph: Emphasis,
    pub link: Option<crate::doc::Link>,
    pub node: NodeId,
    pub ws: WhiteSpace,
    pub transform: TextTransform,
    /// CSS Text spacing in CSS pixels. Parley applies both after shaping and
    /// bidi resolution, preserving fractional values.
    pub letter: f32,
    pub word: f32,
    pub font_family: String,
    pub font_size: f32,
    pub font_weight: f32,
    pub font_italic: bool,
    pub line_height: crate::text::CssLineHeight,
    pub vertical_align: VerticalAlign,
    /// Within-word break opportunities (`overflow-wrap`/`word-break`).
    pub brk: WordBrk,
    /// `word-break: keep-all` — no soft wraps at CJK ideograph boundaries.
    pub keep_all: bool,
    /// `tab-size`: a number of space advances or an absolute CSS-pixel length.
    pub tab: TabSize,
    pub font_zero: bool,
    /// Paint suppression for this element's text: the sticky opacity chain OR
    /// its own computed `visibility:hidden` (re-clearable per element).
    pub invisible: bool,
    /// The `opacity:0` chain alone — what element children derive from.
    opacity_chain: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TabSize {
    Spaces(f32),
    Length(f32),
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum VerticalAlign {
    #[default]
    Baseline,
    Top,
    Bottom,
    Middle,
    /// Positive values raise the inline box relative to the baseline.
    Shift(f32),
}

impl InlineStyle {
    pub fn root() -> InlineStyle {
        InlineStyle {
            kind: ItemKind::Text,
            emph: Emphasis::default(),
            link: None,
            node: NO_NODE,
            ws: WhiteSpace::Normal,
            transform: TextTransform::None,
            letter: 0.0,
            word: 0.0,
            font_family: String::from("sans-serif"),
            font_size: crate::dom::FONT_SIZE_INITIAL,
            font_weight: 400.0,
            font_italic: false,
            line_height: crate::text::CssLineHeight::Normal,
            vertical_align: VerticalAlign::Baseline,
            brk: WordBrk::Normal,
            keep_all: false,
            tab: TabSize::Spaces(8.0),
            font_zero: false,
            invisible: false,
            opacity_chain: false,
        }
    }

    /// The context inside element `id`, derived from the parent's.
    pub fn derive(dom: &Dom, id: NodeId, parent: &InlineStyle, base: &Url) -> InlineStyle {
        let u = Units::of(dom, id);
        let mut s = parent.clone();
        s.node = id;
        match dom.tag_name(id) {
            Some("a") => {
                if let Some(href) = dom.attr(id, "href") {
                    s.link = Some(if dom.render_clickable(id) {
                        crate::doc::Link::JsClick {
                            node: id,
                            href: href.to_string(),
                        }
                    } else {
                        crate::http::resolve(base, href)
                    });
                    s.kind = ItemKind::Link;
                }
            }
            // A resident actor supplies activation identity out-of-band. Keep
            // the authored box tree intact and attach the action to its inline
            // semantics; no synthetic anchor or `data-trust-*` attribute is
            // needed in a reparsed presentation DOM.
            Some(_) if dom.render_clickable(id) => {
                s.link = Some(crate::doc::Link::JsClick {
                    node: id,
                    href: String::new(),
                });
                s.kind = ItemKind::Link;
            }
            // Transitional compatibility for stored/history documents made by
            // the former serialization boundary. New direct layouts never
            // need these attributes, but old snapshots remain renderable.
            Some(_) if dom.attr(id, "data-trust-click").is_some() => {
                if let Some(node) = dom
                    .attr(id, "data-trust-node")
                    .and_then(|value| value.parse().ok())
                {
                    s.link = Some(crate::doc::Link::JsClick {
                        node,
                        href: String::new(),
                    });
                    s.kind = ItemKind::Link;
                }
            }
            Some("blockquote") => s.kind = ItemKind::Quote,
            Some("pre") => s.kind = ItemKind::Pre,
            Some(t) => {
                if let Some(level) = heading_level(t) {
                    s.kind = ItemKind::Heading(level);
                }
            }
            None => {}
        }
        s.font_family = dom
            .computed_value_resolved(id, "font-family")
            .unwrap_or_else(|| s.font_family.clone());
        s.font_size = dom.font_px(id);
        s.font_weight = dom
            .computed_value_resolved(id, "font-weight")
            .as_deref()
            .and_then(crate::layout2::css_font_weight)
            .unwrap_or(s.font_weight);
        s.font_italic = dom
            .computed_value_resolved(id, "font-style")
            .is_some_and(|v| css_is_italic(&v));
        s.emph.bold = s.font_weight >= 600.0;
        s.emph.italic = s.font_italic;
        (s.emph.underline, s.emph.strike) = dom.text_decoration(id);
        s.transform = dom
            .computed_value_resolved(id, "text-transform")
            .as_deref()
            .and_then(TextTransform::from_css)
            .unwrap_or(TextTransform::None);
        s.letter = dom
            .computed_value_resolved(id, "letter-spacing")
            .as_deref()
            .and_then(|v| css_length_px(v, u))
            .unwrap_or(0.0);
        s.word = dom
            .computed_value_resolved(id, "word-spacing")
            .as_deref()
            .filter(|value| value.trim() != "normal")
            .and_then(|value| css_length_px(value, u))
            .unwrap_or(0.0);
        s.line_height = dom
            .computed_value_resolved(id, "line-height")
            .as_deref()
            .and_then(|value| {
                let value = value.trim();
                if value == "normal" {
                    Some(crate::text::CssLineHeight::Normal)
                } else if let Ok(number) = value.parse::<f32>() {
                    Some(crate::text::CssLineHeight::Number(number.max(0.0)))
                } else {
                    css_length_px(value, u).map(crate::text::CssLineHeight::Length)
                }
            })
            .unwrap_or(crate::text::CssLineHeight::Normal);
        // `vertical-align` is not inherited. CSS Inline 3 §4.5 positions the
        // inline-level box relative to the line box's baseline.
        s.vertical_align = match dom.computed_value_resolved(id, "vertical-align").as_deref() {
            Some("top") | Some("text-top") => VerticalAlign::Top,
            Some("bottom") | Some("text-bottom") => VerticalAlign::Bottom,
            Some("middle") => VerticalAlign::Middle,
            Some("sub") => VerticalAlign::Shift(-0.2 * s.font_size),
            Some("super") => VerticalAlign::Shift(0.35 * s.font_size),
            Some(value) => css_length_px(value, u)
                .map(VerticalAlign::Shift)
                .unwrap_or_default(),
            None => VerticalAlign::Baseline,
        };
        let wb = dom
            .computed_value_resolved(id, "word-break")
            .map(|v| v.trim().to_ascii_lowercase());
        s.keep_all = wb.as_deref() == Some("keep-all");
        s.brk = match wb.as_deref() {
            Some("break-all") => WordBrk::BreakAll,
            // CSS Text 4 §5.2: the legacy `word-break: break-word` computes
            // to `normal` + `overflow-wrap: anywhere` behavior.
            Some("break-word") => WordBrk::Anywhere,
            _ => match dom
                .computed_value_resolved(id, "overflow-wrap")
                .as_deref()
                .map(str::trim)
            {
                Some("anywhere") => WordBrk::Anywhere,
                Some("break-word") => WordBrk::BreakWord,
                _ => WordBrk::Normal,
            },
        };
        s.tab = dom
            .computed_value_resolved(id, "tab-size")
            .as_deref()
            .map(str::trim)
            .and_then(|v| {
                if let Ok(n) = v.parse::<f32>() {
                    Some(TabSize::Spaces(n.max(0.0)))
                } else {
                    css_length_px(v, u).map(|px| TabSize::Length(px.max(0.0)))
                }
            })
            .unwrap_or(TabSize::Spaces(8.0));
        s.ws = dom
            .computed_value_resolved(id, "white-space")
            .as_deref()
            .and_then(WhiteSpace::from_css)
            .unwrap_or(WhiteSpace::Normal)
            .with_longhands(
                dom.computed_value_resolved(id, "white-space-collapse")
                    .as_deref(),
                nowrap_longhand(dom, id),
            );
        if let Some(zero) = dom.font_size_zero(id) {
            s.font_zero = zero;
        }
        s.opacity_chain = parent.opacity_chain || dom.paint_suppressed(id);
        s.invisible = s.opacity_chain || dom.visibility_hidden(id);
        s
    }

    /// The sticky `opacity:0` chain alone (a suppressed OUT-OF-FLOW box is
    /// skipped entirely — no paint, no scrollable extent; `visibility` stays
    /// re-clearable and lays as a ghost).
    pub fn opacity_suppressed(&self) -> bool {
        self.opacity_chain
    }

    pub fn text_style(&self) -> crate::text::TextStyle {
        crate::text::TextStyle {
            family: self.font_family.clone(),
            size: self.font_size,
            weight: self.font_weight,
            italic: self.font_italic,
            letter_spacing: self.letter,
            word_spacing: self.word,
            line_height: self.line_height,
            underline: self.emph.underline,
            strikethrough: self.emph.strike,
        }
    }
}

/// The CSS Text 4 wrap longhand (`text-wrap-mode`, or its `text-wrap`
/// shorthand) as a nowrap override for `WhiteSpace::with_longhands`.
fn nowrap_longhand(dom: &Dom, id: NodeId) -> Option<bool> {
    for prop in ["text-wrap-mode", "text-wrap"] {
        match dom
            .computed_value_resolved(id, prop)
            .as_deref()
            .map(str::trim)
        {
            Some("nowrap") => return Some(true),
            Some("wrap") | Some("balance") | Some("pretty") | Some("stable") => {
                return Some(false);
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn heading_level(tag: &str) -> Option<u8> {
    match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}
