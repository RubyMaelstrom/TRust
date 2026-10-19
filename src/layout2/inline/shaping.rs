//! CSS Text 3 #boundary-shaping. One normalized context spans inline DOM
//! boundaries without changing each leaf's source, box, paint or bidi owner.
//! Contexts span Text-node boundaries and preserve joining during emergency
//! wrapping. Other single-leaf text keeps the ordinary shaping cache path.

use super::*;
use crate::text::context::{ContextShape, ContextSlice};
use std::sync::Arc;

#[derive(Clone)]
struct Part {
    bytes: std::ops::Range<usize>,
    map: Vec<(usize, usize)>,
    group: usize,
}

#[derive(Default)]
pub(super) struct Plan {
    parts: rustc_hash::FxHashMap<(usize, usize), Arc<[Part]>>,
    groups: Vec<Option<Arc<ContextShape>>>,
}

pub(super) struct Cursor {
    parts: Arc<[Part]>,
    pub byte: usize,
}

struct Group {
    text: String,
    style: crate::text::TextStyle,
    leaves: usize,
    previous: Option<usize>,
    next: Option<usize>,
    emergency_wrap: bool,
}
struct Builder<'a, 'd, 'i, 't> {
    ifc: &'a Ifc<'d, 'i, 't>,
    groups: Vec<Group>,
    current: Option<usize>,
    parts: rustc_hash::FxHashMap<(usize, usize), Vec<Part>>,
}

fn glyph_style(mut style: crate::text::TextStyle) -> crate::text::TextStyle {
    style.underline = false;
    style.strikethrough = false;
    // Line-height and decoration do not select or position glyphs. The
    // retained slice later restores its own line metrics and decoration.
    style.line_height = crate::text::CssLineHeight::Normal;
    style
}

impl Plan {
    pub fn new(ifc: &Ifc<'_, '_, '_>, content: &[Inline], root: &InlineStyle) -> Self {
        let mut builder = Builder {
            ifc,
            groups: Vec::new(),
            current: None,
            parts: Default::default(),
        };
        builder.collect(content, root);
        // CSS Text 3 #boundary-shaping recommends retaining cursive joining
        // even when a font or glyph-affecting style changes. Unicode 17.0
        // §9.2.2, R1–R7 defines joining through transparent characters. A
        // joiner supplies the neighboring join without asking either font
        // to paint the other font's letters or form a cross-font ligature.
        let joining = icu_properties::CodePointMapData::<icu_properties::props::JoiningType>::new();
        use icu_properties::props::JoiningType as J;
        let edge = |mut at: Option<usize>, before: bool| {
            while let Some(index) = at {
                let group = &builder.groups[index];
                let kind = if before {
                    group
                        .text
                        .chars()
                        .rev()
                        .map(|c| joining.get(c))
                        .find(|k| *k != J::Transparent)
                } else {
                    group
                        .text
                        .chars()
                        .map(|c| joining.get(c))
                        .find(|k| *k != J::Transparent)
                };
                if kind.is_some() {
                    return kind;
                }
                at = if before { group.previous } else { group.next };
            }
            None
        };
        let causes_after =
            |kind| matches!(kind, Some(J::DualJoining | J::LeftJoining | J::JoinCausing));
        let causes_before = |kind| {
            matches!(
                kind,
                Some(J::DualJoining | J::RightJoining | J::JoinCausing)
            )
        };
        let joins: Vec<_> = builder
            .groups
            .iter()
            .enumerate()
            .map(|(index, group)| {
                (
                    causes_after(edge(group.previous, true))
                        && causes_before(edge(Some(index), false)),
                    causes_after(edge(Some(index), true)) && causes_before(edge(group.next, false)),
                )
            })
            .collect();
        for parts in builder.parts.values_mut() {
            for part in parts {
                if joins[part.group].0 {
                    for (_, offset) in &mut part.map {
                        *offset += '\u{200d}'.len_utf8();
                    }
                }
            }
        }
        Self {
            parts: builder
                .parts
                .into_iter()
                .map(|(key, value)| (key, value.into()))
                .collect(),
            groups: builder
                .groups
                .into_iter()
                .enumerate()
                .map(|(index, mut group)| {
                    let (before, after) = joins[index];
                    let content_start = if before { '\u{200d}'.len_utf8() } else { 0 };
                    let content_end = content_start + group.text.len();
                    if before {
                        group.text.insert(0, '\u{200d}');
                    }
                    if after {
                        group.text.push('\u{200d}');
                    }
                    (group.leaves > 1 || before || after || group.emergency_wrap).then(|| {
                        Arc::new(ContextShape::with_content(
                            group.text,
                            &group.style,
                            content_start..content_end,
                        ))
                    })
                })
                .collect(),
        }
    }
    pub fn cursor(&self, text: &str) -> Option<Cursor> {
        Some(Cursor {
            parts: self
                .parts
                .get(&(text.as_ptr() as usize, text.len()))?
                .clone(),
            byte: 0,
        })
    }
    pub fn range(&self, cursor: &Cursor, len: usize) -> Option<ContextSlice> {
        let part = cursor
            .parts
            .iter()
            .find(|part| part.bytes.start <= cursor.byte && part.bytes.end >= cursor.byte + len)?;
        let map = |at| {
            part.map
                .binary_search_by_key(&at, |(byte, _)| *byte)
                .ok()
                .map(|i| part.map[i].1)
        };
        Some(ContextSlice::new(
            self.groups.get(part.group)?.as_ref()?.clone(),
            map(cursor.byte)?..map(cursor.byte + len)?,
        ))
    }
    pub fn prefix_advance(&self, text: &str, len: usize) -> Option<f32> {
        let cursor = self.cursor(text)?;
        Some(self.range(&cursor, len)?.advance())
    }
}

impl Builder<'_, '_, '_, '_> {
    fn group(&mut self, ctx: &InlineStyle) -> usize {
        let style = glyph_style(ctx.text_style());
        if let Some(index) = self.current
            && self.groups[index].style == style
        {
            self.groups[index].emergency_wrap |= ctx.ws.wraps()
                && matches!(
                    ctx.brk,
                    super::super::style::WordBrk::Anywhere
                        | super::super::style::WordBrk::BreakWord
                        | super::super::style::WordBrk::BreakAll
                );
            return index;
        }
        let index = self.groups.len();
        self.groups.push(Group {
            text: String::new(),
            style,
            leaves: 0,
            previous: self.current,
            next: None,
            emergency_wrap: ctx.ws.wraps()
                && matches!(
                    ctx.brk,
                    super::super::style::WordBrk::Anywhere
                        | super::super::style::WordBrk::BreakWord
                        | super::super::style::WordBrk::BreakAll
                ),
        });
        if let Some(previous) = self.current {
            self.groups[previous].next = Some(index);
        }
        self.current = Some(index);
        index
    }
    fn text(&mut self, text: &str, origin: TextOrigin, ctx: &InlineStyle) {
        if ctx.font_zero {
            return;
        }
        let transformed = self.ifc.transformed_text(text, origin, ctx);
        let raw = transformed.text.as_ref();
        let key = (text.as_ptr() as usize, text.len());
        let mut start = 0;
        while start < raw.len() {
            let tail = &raw[start..];
            let separator = tail
                .char_indices()
                .find(|(_, c)| {
                    (*c == '\n' && (!ctx.ws.collapses_spaces() || ctx.ws.preserves_newlines()))
                        || (*c == '\t' && !ctx.ws.collapses_spaces())
                })
                .map(|(i, c)| (start + i, c.len_utf8()));
            let end = separator.map_or(raw.len(), |(i, _)| i);
            if end > start {
                let group = self.group(ctx);
                let entry = &mut self.groups[group];
                entry.leaves += 1;
                let mut map = vec![(start, entry.text.len())];
                for (at, c) in raw[start..end].char_indices() {
                    if ctx.ws.collapses_spaces() && is_collapsible_space(c) {
                        if !entry.text.ends_with(' ') {
                            entry.text.push(' ')
                        }
                    } else {
                        entry.text.push(c)
                    }
                    map.push((start + at + c.len_utf8(), entry.text.len()));
                }
                self.parts.entry(key).or_default().push(Part {
                    bytes: start..end,
                    map,
                    group,
                });
            }
            let Some((at, len)) = separator else { break };
            self.current = None;
            start = at + len;
        }
    }
    fn collect(&mut self, items: &[Inline], ctx: &InlineStyle) {
        for item in items {
            match item {
                Inline::Text(text, origin) => self.text(text, *origin, ctx),
                Inline::Box { node, style, kids } => {
                    let inner = if *node == NO_NODE {
                        ctx.with_pseudo(self.ifc.dom, style.pseudo)
                    } else {
                        self.ifc.form_context(*node, ctx)
                    };
                    let isolated = matches!(
                        style.unicode_bidi,
                        super::super::style::UnicodeBidi::Isolate
                            | super::super::style::UnicodeBidi::IsolateOverride
                            | super::super::style::UnicodeBidi::Plaintext
                    );
                    let align = style.pseudo.map_or_else(
                        || {
                            self.ifc
                                .dom
                                .computed_value_resolved(*node, "vertical-align")
                        },
                        |(node, pseudo)| {
                            self.ifc
                                .dom
                                .pseudo_layout_value(node, pseudo, "vertical-align")
                        },
                    );
                    let split =
                        isolated || align.as_deref().is_some_and(|value| value != "baseline");
                    let side = |side: usize| {
                        style.border[side] != 0.
                            || style.margin[side]
                                .resolve(Some(self.ifc.cb_w_px))
                                .unwrap_or(0.)
                                != 0.
                            || style.padding[side]
                                .resolve(Some(self.ifc.cb_w_px))
                                .unwrap_or(0.)
                                != 0.
                    };
                    let (first, last) = if style.vertical.is_some() {
                        if style.rtl {
                            (BOTTOM, TOP)
                        } else {
                            (TOP, BOTTOM)
                        }
                    } else if style.rtl {
                        (RIGHT, LEFT)
                    } else {
                        (LEFT, RIGHT)
                    };
                    if split || (style.inline_edges[0] && side(first)) {
                        self.current = None
                    }
                    self.collect(kids, &inner);
                    if split || (style.inline_edges[1] && side(last)) {
                        self.current = None
                    }
                }
                Inline::OutOfFlow(_) | Inline::Float(_) => {}
                Inline::Atom(_) | Inline::AtomBox(_) | Inline::Br => self.current = None,
            }
        }
    }
}
