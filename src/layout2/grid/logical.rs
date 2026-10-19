//! Grid 2 #grid-model, #subgrid-indexing, #algo-grid-sizing and Writing
//! Modes 4 #abstract-box / #logical-direction-layout (CSSWG 81c27f686901,
//! local 2026-09-06 snapshot). Track algorithms operate in inline/block
//! coordinates; only item layout and fragment placement use physical pixels.
//! An orthogonal subgrid exchanges which parent axis it inherits, and each
//! axis reverses independently according to the two writing modes.

use super::super::flex::AlignItem;
use super::super::inline::Ifc;
use super::super::style::{Align2, BoxStyle};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Inline,
    Block,
}

impl Axis {
    fn other(self) -> Self {
        if self == Self::Inline {
            Self::Block
        } else {
            Self::Inline
        }
    }
}

#[derive(Clone, Copy)]
struct Axes {
    vertical: bool,
    rtl: bool,
    block_reverse: bool,
}

struct SizedAxis {
    positions: Vec<f32>,
    size: f32,
}

impl Axes {
    fn of(style: &BoxStyle) -> Self {
        Self {
            vertical: style.vertical.is_some(),
            rtl: style.rtl,
            block_reverse: style.vertical == Some(true),
        }
    }
    fn physical_x(self, axis: Axis) -> bool {
        (axis == Axis::Inline) != self.vertical
    }
    fn reversed(self, axis: Axis) -> bool {
        if axis == Axis::Inline {
            self.rtl
        } else {
            self.block_reverse
        }
    }
    fn sides(self, axis: Axis) -> (usize, usize) {
        let sides = if self.physical_x(axis) {
            (LEFT, RIGHT)
        } else {
            (TOP, BOTTOM)
        };
        if self.reversed(axis) {
            (sides.1, sides.0)
        } else {
            sides
        }
    }
    fn child_axis(self, child: Self, axis: Axis) -> Axis {
        if self.vertical == child.vertical {
            axis
        } else {
            axis.other()
        }
    }
    fn physical_size(self, inline: f32, block: f32) -> [f32; 2] {
        if self.vertical {
            [block, inline]
        } else {
            [inline, block]
        }
    }
}

pub(super) fn needed(dom: &crate::dom::Dom, b: &BoxNode, items: &[SharedBox]) -> bool {
    b.style.vertical.is_some()
        || items.iter().any(|item| {
            matches!(item.content, Content::Grid(_))
                && item.style.vertical.is_some()
                && (dom.is_column_subgrid_item(item.node) || dom.is_row_subgrid_item(item.node))
        })
}

fn tracks<'a>(setup: &'a GridSetup<'_>, axis: Axis) -> &'a [Track] {
    if axis == Axis::Inline {
        &setup.cols
    } else {
        &setup.rows
    }
}
fn tracks_mut<'a>(setup: &'a mut GridSetup<'_>, axis: Axis) -> &'a mut Vec<Track> {
    if axis == Axis::Inline {
        &mut setup.cols
    } else {
        &mut setup.rows
    }
}
fn gap(setup: &GridSetup<'_>, axis: Axis) -> f32 {
    if axis == Axis::Inline {
        setup.gap_col
    } else {
        setup.gap_row
    }
}
fn span(placed: &Placed, axis: Axis) -> Range<usize> {
    if axis == Axis::Inline {
        placed.cols.clone()
    } else {
        placed.rows.clone()
    }
}
fn inherited<'a>(setup: &'a GridSetup<'_>, axis: Axis) -> Option<&'a AxisInput> {
    if axis == Axis::Inline {
        setup.inherited_columns.as_ref()
    } else {
        setup.inherited.as_ref()
    }
}
fn names<'a>(setup: &'a GridSetup<'_>, axis: Axis) -> (&'a [Vec<String>], &'a AxisAreas) {
    if axis == Axis::Inline {
        (&setup.col_names, &setup.col_areas)
    } else {
        (&setup.row_names, &setup.row_areas)
    }
}
fn total(tracks: &[Track], gap: f32) -> f32 {
    tracks.iter().map(|track| track.base).sum::<f32>()
        + gap
            * tracks
                .iter()
                .filter(|track| !track.collapsed)
                .count()
                .saturating_sub(1) as f32
}
fn positions(
    tracks: &[Track],
    gap: f32,
    available: Option<f32>,
    distribution: Justify,
) -> Vec<f32> {
    let live = tracks.iter().filter(|track| !track.collapsed).count();
    let (lead, between) = justify_offsets(
        distribution,
        available.map_or(0., |size| size - total(tracks, gap)),
        live,
    );
    let mut offset = lead.max(0.);
    let mut seen = 0;
    tracks
        .iter()
        .map(|track| {
            if !track.collapsed && seen > 0 {
                offset += gap + between;
            }
            let result = offset;
            if !track.collapsed {
                offset += track.base;
                seen += 1;
            }
            result
        })
        .collect()
}
fn area_size(tracks: &[Track], positions: &[f32], span: Range<usize>) -> f32 {
    let last = span.end - 1;
    (positions[last] + tracks[last].base - positions[span.start]).max(0.)
}

impl Flow<'_> {
    fn logical_subgrids(&self, item: &BoxNode, axis: Axis) -> bool {
        matches!(item.content, Content::Grid(_))
            && if axis == Axis::Inline {
                self.dom.is_column_subgrid_item(item.node)
            } else {
                self.dom.is_row_subgrid_item(item.node)
            }
    }

    fn with_logical_grid_inputs<R>(
        &self,
        node: NodeId,
        inputs: [Option<AxisInput>; 2],
        run: impl FnOnce() -> R,
    ) -> R {
        let old_cols = {
            let mut state = self.subgrid_columns.borrow_mut();
            let old = state.inputs.remove(&node);
            if let Some(input) = &inputs[0] {
                state.inputs.insert(node, input.clone());
            }
            old
        };
        let old_rows = {
            let mut state = self.subgrid_rows.borrow_mut();
            let old = state.inputs.remove(&node);
            if let Some(input) = &inputs[1] {
                state.inputs.insert(node, input.clone());
            }
            old
        };
        let result = run();
        for (state, old) in [
            (&self.subgrid_columns, old_cols),
            (&self.subgrid_rows, old_rows),
        ] {
            let mut state = state.borrow_mut();
            state.inputs.remove(&node);
            if let Some(old) = old {
                state.inputs.insert(node, old);
            }
        }
        result
    }

    fn logical_child_inputs(
        &self,
        b: &BoxNode,
        item: &BoxNode,
        p: &Placed,
        setup: &GridSetup<'_>,
        positions: Option<&[Vec<f32>; 2]>,
        percentage_basis: f32,
    ) -> [Option<AxisInput>; 2] {
        let parent = Axes::of(&b.style);
        let child = Axes::of(&item.style);
        let mut inputs = [None, None];
        for axis in [Axis::Inline, Axis::Block] {
            let child_axis = parent.child_axis(child, axis);
            if !self.logical_subgrids(item, child_axis) {
                continue;
            }
            let span = span(p, axis);
            let (line_names, areas) = names(setup, axis);
            let (names, areas) = subgrid_names(line_names, areas, &span);
            let (start, end) = parent.sides(axis);
            let style = &item.style;
            let edge = |side: usize| {
                style.margin[side]
                    .resolve(Some(percentage_basis))
                    .unwrap_or(0.)
                    + style.border[side]
                    + self.pad(style, side, percentage_basis)
            };
            let used_positions =
                positions.map(|positions| &positions[usize::from(axis == Axis::Block)]);
            let mut input = AxisInput {
                tracks: tracks(setup, axis)[span.clone()].to_vec(),
                names,
                areas,
                positions: used_positions.map(|positions| {
                    positions[span.clone()]
                        .iter()
                        .map(|p| p - positions[span.start])
                        .collect()
                }),
                gap: used_positions.filter(|_| span.len() > 1).map_or(
                    gap(setup, axis),
                    |positions| {
                        positions[span.start + 1]
                            - positions[span.start]
                            - tracks(setup, axis)[span.start].base
                    },
                ),
                start_edge: edge(start),
                end_edge: edge(end),
            };
            if parent.reversed(axis) != child.reversed(child_axis) {
                input = input.reversed();
            }
            inputs[usize::from(child_axis == Axis::Block)] = Some(input);
        }
        inputs
    }

    /// Intrinsic contributions in physical y use the same line breaker as
    /// physical x. This is measurement in the text's inline axis, not a
    /// rotation of a completed fragment or an alternate text implementation.
    fn logical_height_contribution(
        &self,
        item: &BoxNode,
        mode: IMode,
        inl: &InlineStyle,
        opposite: Option<f32>,
    ) -> f32 {
        let s = &item.style;
        let bp = s.border[TOP] + s.border[BOTTOM] + self.pad(s, TOP, 0.) + self.pad(s, BOTTOM, 0.);
        let margins = s.margin[TOP].resolve(Some(0.)).unwrap_or(0.)
            + s.margin[BOTTOM].resolve(Some(0.)).unwrap_or(0.);
        let here = InlineStyle::for_box(self.dom, item.node, s, inl, self.base);
        let preferred = self.height_px(
            &s.height,
            s,
            s.border[TOP] + self.pad(s, TOP, 0.),
            s.border[BOTTOM] + self.pad(s, BOTTOM, 0.),
            None,
        );
        let content = if let Some(height) = preferred {
            height
        } else if let Content::Inlines(inlines) = &item.content
            && s.vertical.is_some()
        {
            let mut ifc = Ifc::new(
                self.dom,
                self.base,
                self.images,
                self.forms,
                self.vp,
                if mode == IMode::Min { 0. } else { 10_000_000. },
                None,
                Align2::Left,
                0.,
                None,
                &[],
            );
            ifc.mark_sideways();
            ifc.run(inlines, &here);
            ifc.finish()
                .0
                .iter()
                .map(|line| line.width)
                .fold(0., f32::max)
        } else {
            let width = opposite.unwrap_or_else(|| self.intrinsic_w(item, IMode::Max, &here));
            let horizontal_bp =
                s.border[LEFT] + s.border[RIGHT] + self.pad(s, LEFT, 0.) + self.pad(s, RIGHT, 0.);
            let horizontal_margin = s.margin[LEFT].resolve(Some(0.)).unwrap_or(0.)
                + s.margin[RIGHT].resolve(Some(0.)).unwrap_or(0.);
            let area_width = width;
            let preferred_width = |value: &Len| {
                self.intrinsic_width_value(value, item, opposite, &here)
                    .or_else(|| {
                        value.resolve(opposite).map(|value| {
                            (value - if s.border_box { horizontal_bp } else { 0. }).max(0.)
                        })
                    })
            };
            let width = preferred_width(&s.width)
                .unwrap_or_else(|| (area_width - horizontal_bp - horizontal_margin).max(0.));
            let min_width = preferred_width(&s.min_width).unwrap_or(0.);
            let max_width = preferred_width(&s.max_width)
                .unwrap_or(f32::INFINITY)
                .max(min_width);
            let width = width.clamp(min_width, max_width);
            let (fragment, _) = self.item_frag(item, width, width, None, &here);
            (fragment.h - bp).max(0.)
        };
        let minimum = self.height_px(&s.min_height, s, 0., bp, None).unwrap_or(0.);
        let maximum = self
            .height_px(&s.max_height, s, 0., bp, None)
            .unwrap_or(f32::INFINITY)
            .max(minimum);
        content.clamp(minimum, maximum) + bp + margins
    }

    fn logical_grid_contributions(
        &self,
        b: &BoxNode,
        setup: &GridSetup<'_>,
        axis: Axis,
        opposite: &[Option<f32>],
        inl: &InlineStyle,
    ) -> Vec<Contrib> {
        let axes = Axes::of(&b.style);
        let mut result = Vec::new();
        for (index, (item, p)) in setup.ordered.iter().zip(&setup.placed).enumerate() {
            let area = span(p, axis);
            let opposite = opposite.get(index).copied().flatten();
            let child_axes = Axes::of(&item.style);
            let child_axis = axes.child_axis(child_axes, axis);
            if let Content::Grid(children) = &item.content
                && self.logical_subgrids(item, child_axis)
            {
                let inputs = self.logical_child_inputs(b, item, p, setup, None, 0.);
                let input = inputs[usize::from(child_axis == Axis::Block)]
                    .as_ref()
                    .unwrap()
                    .clone();
                let child_inline =
                    InlineStyle::for_box(self.dom, item.node, &item.style, inl, self.base);
                let (inline_basis, block_basis) = if child_axis == Axis::Inline {
                    (None, opposite)
                } else {
                    (opposite, None)
                };
                let exported = self.with_logical_grid_inputs(item.node, inputs, || {
                    let child = self.grid_placement(item, children, inline_basis, block_basis);
                    let other = self.logical_initial_opposite(&child, child_axis, None);
                    let contributions = self.logical_grid_contributions(
                        item,
                        &child,
                        child_axis,
                        &other,
                        &child_inline,
                    );
                    input.export(contributions, gap(&child, child_axis))
                });
                let reversed = axes.reversed(axis) != child_axes.reversed(child_axis);
                result.extend(exported.into_iter().map(|mut contribution| {
                    if reversed {
                        contribution.tracks = area.len() - contribution.tracks.end
                            ..area.len() - contribution.tracks.start;
                    }
                    contribution.tracks = contribution.tracks.start + area.start
                        ..contribution.tracks.end + area.start;
                    contribution
                }));
                continue;
            }
            let physical_x = axes.physical_x(axis);
            let min = if physical_x {
                self.contribution(item, IMode::Min, inl, opposite)
            } else {
                self.logical_height_contribution(item, IMode::Min, inl, opposite)
            };
            let max = if physical_x {
                self.contribution(item, IMode::Max, inl, opposite)
            } else {
                self.logical_height_contribution(item, IMode::Max, inl, opposite)
            };
            let (start, end) = axes.sides(axis);
            let s = &item.style;
            let bp =
                s.border[start] + s.border[end] + self.pad(s, start, 0.) + self.pad(s, end, 0.);
            let margins = s.margin[start].resolve(Some(0.)).unwrap_or(0.)
                + s.margin[end].resolve(Some(0.)).unwrap_or(0.);
            result.push(Contrib {
                tracks: area.clone(),
                minimum: self.grid_minimum_contribution(
                    item,
                    &tracks(setup, axis)[area],
                    physical_x,
                    min,
                    max,
                    bp,
                    margins,
                ),
                min,
                max,
            });
        }
        result
    }

    fn logical_initial_opposite(
        &self,
        setup: &GridSetup<'_>,
        axis: Axis,
        available: Option<f32>,
    ) -> Vec<Option<f32>> {
        setup
            .placed
            .iter()
            .map(|placed| {
                let span = span(placed, axis.other());
                tracks(setup, axis.other())[span.clone()]
                    .iter()
                    .map(|track| {
                        if track.collapsed {
                            Some(0.)
                        } else {
                            track.size.max.definite(available)
                        }
                    })
                    .collect::<Option<Vec<_>>>()
                    .map(|sizes| {
                        sizes.iter().sum::<f32>()
                            + gap(setup, axis.other()) * span.len().saturating_sub(1) as f32
                    })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn logical_size_axis(
        &self,
        b: &BoxNode,
        setup: &mut GridSetup<'_>,
        axis: Axis,
        available: Option<f32>,
        contributions: &[Contrib],
        minimum: f32,
        mode: Option<IMode>,
    ) -> SizedAxis {
        let distribution = self.dom.computed_value_resolved(
            b.node,
            if axis == Axis::Inline {
                "justify-content"
            } else {
                "align-content"
            },
        );
        let spacing = gap(setup, axis);
        if let Some(input) = inherited(setup, axis).cloned() {
            let state = if axis == Axis::Inline {
                &self.subgrid_columns
            } else {
                &self.subgrid_rows
            };
            state
                .borrow_mut()
                .contributions
                .insert(b.node, input.export(contributions.to_vec(), spacing));
            if let Some((tracks, positions)) = input.resolved(spacing) {
                let size = available.unwrap_or_else(|| {
                    positions
                        .last()
                        .zip(tracks.last())
                        .map_or(0., |(start, track)| start + track.base)
                        .max(minimum)
                });
                *tracks_mut(setup, axis) = tracks;
                return SizedAxis { positions, size };
            }
        }
        let track_list = tracks_mut(setup, axis);
        size_tracks_constrained(
            track_list,
            contributions,
            available,
            spacing,
            dist_stretches(distribution.as_deref()),
            mode == Some(IMode::Min),
        );
        if available.is_none() && minimum > 0. && dist_stretches(distribution.as_deref()) {
            stretch_auto_tracks(track_list, minimum, spacing);
        }
        let mut resolved = available;
        if available.is_none() && has_indefinite_percentage(track_list) {
            let basis = total(track_list, spacing).max(minimum);
            size_tracks_constrained(
                track_list,
                contributions,
                Some(basis),
                spacing,
                dist_stretches(distribution.as_deref()),
                mode == Some(IMode::Min),
            );
            resolved = Some(basis);
        }
        let size = resolved.unwrap_or_else(|| total(track_list, spacing).max(minimum));
        SizedAxis {
            positions: positions(
                track_list,
                spacing,
                resolved,
                dist_of(distribution.as_deref()),
            ),
            size,
        }
    }

    pub(super) fn logical_grid_sized<'t>(
        &self,
        b: &BoxNode,
        items: &'t [SharedBox],
        available: [Option<f32>; 2],
        minimum: [f32; 2],
        inl: &InlineStyle,
        mode: Option<IMode>,
    ) -> (GridSetup<'t>, [Vec<f32>; 2], [f32; 2]) {
        let mut setup = self.grid_placement(b, items, available[0], available[1]);
        let other = self.logical_initial_opposite(&setup, Axis::Inline, available[1]);
        let mut columns = self.logical_grid_contributions(b, &setup, Axis::Inline, &other, inl);
        let mut column_positions = self.logical_size_axis(
            b,
            &mut setup,
            Axis::Inline,
            available[0],
            &columns,
            minimum[0],
            mode,
        );
        let other: Vec<_> = setup
            .placed
            .iter()
            .map(|p| {
                Some(area_size(
                    &setup.cols,
                    &column_positions.positions,
                    p.cols.clone(),
                ))
            })
            .collect();
        let mut rows = self.logical_grid_contributions(b, &setup, Axis::Block, &other, inl);
        let mut row_positions = self.logical_size_axis(
            b,
            &mut setup,
            Axis::Block,
            available[1],
            &rows,
            minimum[1],
            mode,
        );
        // Grid 2 #algo-grid-sizing: one feedback pass in each axis, including
        // row content-distribution gaps. This dependency must not spin.
        let other: Vec<_> = setup
            .placed
            .iter()
            .map(|p| {
                Some(area_size(
                    &setup.rows,
                    &row_positions.positions,
                    p.rows.clone(),
                ))
            })
            .collect();
        let changed = self.logical_grid_contributions(b, &setup, Axis::Inline, &other, inl);
        if changed.len() != columns.len()
            || changed
                .iter()
                .zip(&columns)
                .any(|(a, b)| a.tracks != b.tracks || (a.min - b.min).abs() > 0.01)
        {
            columns = changed;
            column_positions = self.logical_size_axis(
                b,
                &mut setup,
                Axis::Inline,
                available[0],
                &columns,
                minimum[0],
                mode,
            );
            let other: Vec<_> = setup
                .placed
                .iter()
                .map(|p| {
                    Some(area_size(
                        &setup.cols,
                        &column_positions.positions,
                        p.cols.clone(),
                    ))
                })
                .collect();
            rows = self.logical_grid_contributions(b, &setup, Axis::Block, &other, inl);
            row_positions = self.logical_size_axis(
                b,
                &mut setup,
                Axis::Block,
                available[1],
                &rows,
                minimum[1],
                mode,
            );
        }
        (
            setup,
            [column_positions.positions, row_positions.positions],
            [column_positions.size, row_positions.size],
        )
    }

    pub(super) fn logical_grid_intrinsic_width(
        &self,
        b: &BoxNode,
        items: &[SharedBox],
        mode: IMode,
        inl: &InlineStyle,
    ) -> f32 {
        let (_, _, sizes) =
            self.logical_grid_sized(b, items, [None, None], [0., 0.], inl, Some(mode));
        if b.style.vertical.is_some() {
            sizes[1]
        } else {
            sizes[0]
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn logical_grid_content(
        &self,
        b: &BoxNode,
        items: &[SharedBox],
        content_x: f32,
        content_top: f32,
        content_w: f32,
        def_ch: Option<f32>,
        min_ch: f32,
        inl: &InlineStyle,
        anchors: &mut Vec<(NodeId, f32)>,
    ) -> (Vec<Frag>, f32) {
        let axes = Axes::of(&b.style);
        let available = if axes.vertical {
            [def_ch, Some(content_w)]
        } else {
            [Some(content_w), def_ch]
        };
        let minimum = if axes.vertical {
            [min_ch, 0.]
        } else {
            [0., min_ch]
        };
        let (setup, positions, extents) =
            self.logical_grid_sized(b, items, available, minimum, inl, None);
        if b.node != NO_NODE {
            self.grid_tracks.borrow_mut().insert(
                b.node,
                (
                    setup.cols.iter().map(|t| t.base).collect(),
                    setup.rows.iter().map(|t| t.base).collect(),
                ),
            );
        }
        let cv = |property: &str| self.dom.computed_value_resolved(b.node, property);
        let defaults = [
            self_align(cv("justify-items").as_deref(), AlignItem::Stretch),
            self_align(cv("align-items").as_deref(), AlignItem::Stretch),
        ];
        let mut fragments = Vec::with_capacity(items.len());
        for (item, p) in setup.ordered.iter().zip(&setup.placed) {
            let logical_sizes = [
                area_size(&setup.cols, &positions[0], p.cols.clone()),
                area_size(&setup.rows, &positions[1], p.rows.clone()),
            ];
            let [area_w, area_h] = axes.physical_size(logical_sizes[0], logical_sizes[1]);
            let percentage_basis = logical_sizes[0];
            let inputs =
                self.logical_child_inputs(b, item, p, &setup, Some(&positions), percentage_basis);
            let child_axes = Axes::of(&item.style);
            let forced = [Axis::Inline, Axis::Block]
                .map(|axis| self.logical_subgrids(item, axes.child_axis(child_axes, axis)));
            let alignment = ["justify-self", "align-self"]
                .map(|property| self.dom.computed_value_resolved(item.node, property));
            let alignments = [0, 1].map(|i| {
                if forced[i] {
                    AlignItem::Stretch
                } else {
                    self_align(alignment[i].as_deref(), defaults[i])
                }
            });
            let x_axis = if axes.vertical { 1 } else { 0 };
            let y_axis = 1 - x_axis;
            let s = &item.style;
            let (m, auto) = self.margins_of(s, percentage_basis);
            let bp = [
                s.border[LEFT]
                    + s.border[RIGHT]
                    + self.pad(s, LEFT, percentage_basis)
                    + self.pad(s, RIGHT, percentage_basis),
                s.border[TOP]
                    + s.border[BOTTOM]
                    + self.pad(s, TOP, percentage_basis)
                    + self.pad(s, BOTTOM, percentage_basis),
            ];
            let available_w = (area_w - m[LEFT] - m[RIGHT] - bp[0]).max(0.);
            let width_value = |value: &Len| {
                self.intrinsic_width_value(value, item, Some(area_w), inl)
                    .or_else(|| {
                        value
                            .resolve(Some(area_w))
                            .map(|value| (value - if s.border_box { bp[0] } else { 0. }).max(0.))
                    })
            };
            let width = if forced[x_axis] {
                available_w
            } else {
                let value = width_value(&s.width).unwrap_or_else(|| {
                    if alignments[x_axis] == AlignItem::Stretch && !auto[LEFT] && !auto[RIGHT] {
                        available_w
                    } else {
                        let min = self.intrinsic_w(item, IMode::Min, inl);
                        let max = self.intrinsic_w(item, IMode::Max, inl).max(min);
                        available_w.clamp(min, max)
                    }
                });
                let min = width_value(&s.min_width).unwrap_or(0.);
                let max = width_value(&s.max_width).unwrap_or(f32::INFINITY).max(min);
                value.clamp(min, max)
            };
            let height_value = |value: &Len| {
                self.height_value(
                    value,
                    item,
                    s.border[TOP] + self.pad(s, TOP, percentage_basis),
                    s.border[BOTTOM] + self.pad(s, BOTTOM, percentage_basis),
                    Some(area_h),
                    percentage_basis,
                )
            };
            let min_h = if forced[y_axis] {
                0.
            } else {
                height_value(&s.min_height).unwrap_or(0.)
            };
            let max_h = if forced[y_axis] {
                f32::INFINITY
            } else {
                height_value(&s.max_height)
                    .unwrap_or(f32::INFINITY)
                    .max(min_h)
            };
            let height = if forced[y_axis]
                || (matches!(s.height, Len::Auto)
                    && alignments[y_axis] == AlignItem::Stretch
                    && !auto[TOP]
                    && !auto[BOTTOM])
            {
                Some(
                    (area_h - m[TOP] - m[BOTTOM] - bp[1])
                        .max(0.)
                        .clamp(min_h, max_h),
                )
            } else {
                height_value(&s.height).map(|height| height.clamp(min_h, max_h))
            };
            let (mut fragment, mut item_anchors) =
                self.with_logical_grid_inputs(item.node, inputs.clone(), || {
                    self.item_frag(item, width, percentage_basis, height, inl)
                });
            if height.is_none() {
                let actual = (fragment.h - bp[1]).max(0.);
                let clamped = actual.clamp(min_h, max_h);
                if (clamped - actual).abs() > 0.01 {
                    (fragment, item_anchors) =
                        self.with_logical_grid_inputs(item.node, inputs, || {
                            self.item_frag(item, width, percentage_basis, Some(clamped), inl)
                        });
                }
            }
            let frag_sizes = axes.physical_size(fragment.w, fragment.h);
            let (rx, ry) = self.paint_offset(s, area_w, Some(area_h), &mut fragment);
            let mut offsets = [positions[0][p.cols.start], positions[1][p.rows.start]];
            for (index, axis) in [Axis::Inline, Axis::Block].into_iter().enumerate() {
                let (start, end) = axes.sides(axis);
                let size = frag_sizes[index];
                let extra = logical_sizes[index] - size - m[start] - m[end];
                let shift = super::super::flow::cross_shift(
                    extra,
                    auto[start],
                    auto[end],
                    alignments[index],
                );
                offsets[index] += shift + m[start];
                if axes.reversed(axis) {
                    offsets[index] = extents[index] - offsets[index] - size;
                }
            }
            let [x, y] = axes.physical_size(offsets[0], offsets[1]);
            let dx = content_x + x + rx;
            let dy = content_top + y + ry;
            Flow::offset_frag(&mut fragment, dx, dy);
            anchors.extend(item_anchors.into_iter().map(|(node, y)| (node, y + dy)));
            fragments.push(fragment);
        }
        let height = if axes.vertical {
            extents[0]
        } else {
            extents[1]
        };
        (fragments, height)
    }
}
