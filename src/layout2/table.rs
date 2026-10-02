//! Table layout (CSS 2.1 §17), ported onto the box-tree inputs.
//!
//! A `Content::Table` box carries its cells already placed on a grid
//! (`colspan`/`rowspan` resolved in `tree.rs`), its column width preferences,
//! and its captions. This module computes the used column widths — the
//! automatic algorithm (§17.5.2.2), or the fixed algorithm (§17.5.2.1) when
//! `table-layout:fixed` meets a definite width — lays each cell as its own
//! independent formatting context at its spanned width (via `item_frag`, so
//! nested tables recurse), sizes each row to its tallest cell (§17.5.3), and
//! places the cells with vertical alignment (§17.5.4).
//!
//! Everything is f32 CSS px; only the terminal adapter quantizes the resulting
//! fragments. Cell fragments retain their row and row-group background
//! positioning rectangles for CSS 2.2 §17.5.1's layered painting model.

use crate::dom::NodeId;
use crate::layout2::{Units, css_length_px};

use super::flow::{Flow, Frag, FragKind};
use super::intrinsic::IMode;
use super::style::{Align2, BOTTOM, InlineStyle, LEFT, RIGHT, TOP};
use super::tree::{ColSpec, TableBox, declared_track_width};

/// The resolved column geometry of a table.
pub(super) struct TableCols {
    /// Per-column used width (px): the border-box width available to a
    /// single-span cell in that column.
    pub widths: Vec<f32>,
    /// Horizontal border-spacing between columns (px).
    pub bs: f32,
    pub bs_y: f32,
    /// The table's used content width (px): `Σ widths + bs·(ncols−1)`.
    pub table_w: f32,
}

impl Flow<'_> {
    /// Resolve the table's column widths (CSS 2.1 §17.5.2). `avail_w` is the
    /// width available to the table content (its containing block's content
    /// width, less the table's own margins/border/padding — from §10.3.3);
    /// `width_auto` means the `width` property is indefinite, so the table
    /// shrinks to fit rather than filling `avail_w`.
    pub(super) fn table_columns(
        &self,
        tb: &TableBox,
        table_node: NodeId,
        avail_w: f32,
        width_auto: bool,
        inl: &InlineStyle,
    ) -> TableCols {
        let ncols = tb.ncols;
        let (bs, bs_y) = self.table_border_spacing(table_node);
        if ncols == 0 {
            return TableCols {
                widths: Vec::new(),
                bs,
                bs_y,
                table_w: 0.0,
            };
        }
        let spacing = bs * (ncols + 1) as f32;
        // Space available to the columns' content (the band, less spacing).
        let avail = (avail_w - spacing).max(1.0);

        // Per-column min/max content widths + explicit width preferences (the
        // shared metrics; a declared px cap clamps to the band for layout).
        let (col_min, col_max, col_w) = self.table_col_metrics(tb, bs, Some(avail), avail, inl);

        // Fixed layout: a definite width + `table-layout:fixed` ignores
        // content and divides by declared column widths (§17.5.2.1).
        if tb.fixed_layout && !width_auto {
            return TableCols {
                widths: fixed_columns(&col_w, ncols, avail),
                bs,
                bs_y,
                // `avail` already == the definite content width − spacing.
                table_w: avail + spacing,
            };
        }

        let min_sum: f32 = col_min.iter().sum();
        let max_sum: f32 = col_max.iter().sum();
        // Used table content width (§17.5.2.2): auto uses MAX when it fits the
        // band, else the band; a definite width fills the band (which already
        // equals `width − spacing`); never below MIN.
        let used = if width_auto {
            if max_sum <= avail {
                max_sum.max(min_sum)
            } else {
                avail.max(min_sum)
            }
        } else {
            avail.max(min_sum)
        };

        let target = distribute_widths(&col_min, &col_max, &col_w, used);
        let table_w = target.iter().sum::<f32>() + spacing;
        TableCols {
            widths: target,
            bs,
            bs_y,
            table_w,
        }
    }

    /// Lay the table's cells at their resolved column widths and return the
    /// cell fragments (positioned absolutely at `content_x`/`content_top`) and
    /// the grid's height (px). Row heights come from the laid cell heights
    /// (§17.5.3); vertical alignment places each cell in its row band
    /// (§17.5.4).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn table_grid(
        &self,
        tb: &TableBox,
        cols: &TableCols,
        content_x: f32,
        content_top: f32,
        def_ch: Option<f32>,
        inl: &InlineStyle,
        anchors: &mut Vec<(NodeId, f32)>,
    ) -> (Vec<Frag>, f32) {
        let ncols = tb.ncols;
        let nrows = tb.nrows;
        if ncols == 0 || nrows == 0 {
            return (Vec::new(), 0.0);
        }
        let bs = cols.bs;
        let bs_y = cols.bs_y;
        // Column left edges (with inter-column spacing).
        let mut col_x = vec![0.0f32; ncols];
        let mut acc = bs;
        for (c, x) in col_x.iter_mut().enumerate() {
            *x = acc;
            acc += cols.widths.get(c).copied().unwrap_or(1.0) + bs;
        }

        // Lay every cell at its spanned-column width. HTML's `cellpadding`
        // is an ordinary padding hint on the cells (HTML Rendering #tables-2).
        struct Laid {
            frag: Frag,
            anchors: Vec<(NodeId, f32)>,
            /// The cell's spanned border-box width (px) — its CB for %.
            cell_w: f32,
            /// The border-box height its content needs, which vertical
            /// alignment distributes the rest of the row against (CSS 2.2
            /// §17.5.3), even when its 'height' made the cell taller.
            content_h: f32,
        }
        let mut laid: Vec<Laid> = Vec::with_capacity(tb.cells.len());
        for cell in &tb.cells {
            let end = (cell.col + cell.colspan).min(ncols);
            let span = end.saturating_sub(cell.col).max(1);
            let cell_w =
                (cols.widths[cell.col..end].iter().sum::<f32>() + bs * (span - 1) as f32).max(1.0);
            let s = &cell.b.style;
            // The cell's own border+padding wrap its content (item_frag adds
            // them around the imposed content width).
            let cbp = s.border[LEFT]
                + s.border[RIGHT]
                + self.pad(s, LEFT, cell_w)
                + self.pad(s, RIGHT, cell_w);
            let content_w = (cell_w - cbp).max(0.0);
            // CSS 2.2 §17.5.3: a cell's 'height' is a minimum for its row;
            // content taller than it grows the cell. Quirks Mode #the-table-
            // cell-height-box-sizing-quirk measures it as a border box.
            let vertical_edges = s.border[TOP]
                + s.border[BOTTOM]
                + self.pad(s, TOP, cell_w)
                + self.pad(s, BOTTOM, cell_w);
            let def_h = s.height.resolve(def_ch).map(|v| {
                if self.in_quirks_document(cell.b.node) {
                    (v - vertical_edges).max(0.0)
                } else {
                    v.max(0.0)
                }
            });
            let natural = self.item_frag(&cell.b, content_w, cell_w, None, inl);
            let content_h = natural.0.h;
            let (frag, anc) = match def_h {
                Some(h) if natural.0.h - vertical_edges < h => {
                    self.item_frag(&cell.b, content_w, cell_w, Some(h), inl)
                }
                _ => natural,
            };
            laid.push(Laid {
                frag,
                anchors: anc,
                cell_w,
                content_h,
            });
        }

        // CSS 2.2 §17.5.1: row backgrounds extend through cells originating
        // in that row (including rowspans), but their image positioning area
        // remains the ordinary row rectangle. Separate-border gaps retain
        // the TABLE background. Row-group layers precede row layers.
        // https://www.w3.org/TR/CSS22/tables.html#table-layers
        let mut group_rows = std::collections::HashMap::<NodeId, (usize, usize)>::new();
        let cell_rows: Vec<_> = tb
            .cells
            .iter()
            .map(|cell| {
                let row = (cell.b.node != super::NO_NODE)
                    .then(|| self.dom.parent_flat(cell.b.node))
                    .flatten()
                    .filter(|&n| self.dom.effective_display(n).as_deref() == Some("table-row"));
                let group = row.and_then(|r| self.dom.parent_flat(r)).filter(|&n| {
                    matches!(
                        self.dom.effective_display(n).as_deref(),
                        Some("table-row-group" | "table-header-group" | "table-footer-group")
                    )
                });
                if let Some(group) = group {
                    let end = (cell.row + cell.rowspan).min(nrows);
                    group_rows
                        .entry(group)
                        .and_modify(|span| {
                            span.0 = span.0.min(cell.row);
                            span.1 = span.1.max(end);
                        })
                        .or_insert((cell.row, end));
                }
                (row, group)
            })
            .collect();

        // Row heights (§17.5.3): the tallest single-row cell sets each row; a
        // row-spanning cell whose box exceeds its spanned rows pushes the
        // deficit onto its last row.
        let mut row_h = vec![0.0f32; nrows];
        for (cell, l) in tb.cells.iter().zip(&laid) {
            if cell.rowspan <= 1 && cell.row < nrows {
                row_h[cell.row] = row_h[cell.row].max(l.frag.h);
            }
        }
        // A row's own 'height' is also a minimum (HTML maps `tr height`).
        // Rows sized only by their content are the auto-height rows of CSS
        // Tables 3 #height-distribution-algorithm.
        let mut sized = vec![false; nrows];
        for (cell, &(row, _)) in tb.cells.iter().zip(&cell_rows) {
            if cell.row >= nrows {
                continue;
            }
            if cell.rowspan <= 1 && !cell.b.style.height.is_auto() {
                sized[cell.row] = true;
            }
            if let Some(height) = row.and_then(|row| self.row_height(row, def_ch)) {
                row_h[cell.row] = row_h[cell.row].max(height);
                sized[cell.row] = true;
            }
        }
        for (cell, l) in tb.cells.iter().zip(&laid) {
            if cell.rowspan <= 1 {
                continue;
            }
            let end = (cell.row + cell.rowspan).min(nrows);
            if end <= cell.row {
                continue;
            }
            let need = l.frag.h;
            let have: f32 =
                row_h[cell.row..end].iter().sum::<f32>() + bs_y * (end - cell.row - 1) as f32;
            if need > have {
                row_h[end - 1] += need - have;
            }
        }
        // CSS Tables 3 #height-distribution-algorithm: a definite table
        // height beyond its rows grows them. Like Gecko, auto-height rows
        // share the extra in proportion to their heights (equally when all
        // are empty); with no auto-height row, every row does.
        if let Some(target) = def_ch {
            let used = row_h.iter().sum::<f32>() + bs_y * (nrows + 1) as f32;
            let extra = target - used;
            if extra > 0.0 {
                let auto: Vec<usize> = (0..nrows).filter(|&r| !sized[r]).collect();
                let rows = if auto.is_empty() {
                    (0..nrows).collect()
                } else {
                    auto
                };
                let total: f32 = rows.iter().map(|&r| row_h[r]).sum();
                for &r in &rows {
                    row_h[r] += if total > 0.0 {
                        extra * row_h[r] / total
                    } else {
                        extra / rows.len() as f32
                    };
                }
            }
        }
        let mut row_y = vec![0.0f32; nrows];
        let mut acc = bs_y;
        for r in 0..nrows {
            row_y[r] = acc;
            acc += row_h[r] + bs_y;
        }
        let table_h = acc.max(0.0);

        // Place each cell at its column/row origin, vertically aligned in its
        // (possibly taller) row band per `vertical-align`/`valign`.
        let mut frags: Vec<Frag> = Vec::with_capacity(laid.len());
        for ((cell, mut l), (row, group)) in tb.cells.iter().zip(laid).zip(cell_rows) {
            let end = (cell.row + cell.rowspan).min(nrows);
            let span_h = row_h[cell.row..end].iter().sum::<f32>()
                + bs_y * (end.saturating_sub(cell.row + 1)) as f32;
            let dy_valign = self.cell_valign_offset(cell.b.node, l.content_h, span_h);
            // §9.4.3 relative offset / transform translation — a cell's CB for
            // percentages is its own box.
            let (rx, ry) =
                self.paint_offset(&cell.b.style, l.cell_w, Some(span_h), l.frag.w, l.frag.h);
            let x = content_x + col_x[cell.col] + rx;
            let y = content_top + row_y[cell.row] + ry;
            // Vertical alignment moves content, not the cell's background
            // and border. Every cell occupies its full resolved row span.
            for child in &mut l.frag.children {
                Flow::offset_frag(child, 0.0, dy_valign);
            }
            l.frag.h = span_h.max(l.frag.h);
            let mut layers = Vec::new();
            if let Some(group) = group {
                let (first, end) = group_rows[&group];
                let h = row_y[end - 1] + row_h[end - 1] - row_y[first];
                layers.push((
                    group,
                    [
                        content_x + bs - x,
                        content_top + row_y[first] - y,
                        cols.table_w - 2.0 * bs,
                        h,
                    ],
                ));
            }
            if let Some(row) = row {
                layers.push((
                    row,
                    [
                        content_x + bs - x,
                        content_top + row_y[cell.row] - y,
                        cols.table_w - 2.0 * bs,
                        row_h[cell.row],
                    ],
                ));
            }
            if !layers.is_empty() {
                l.frag.kind = FragKind::TableCell(layers.into_boxed_slice());
            }
            Flow::offset_frag(&mut l.frag, x, y);
            for (n, ay) in l.anchors {
                anchors.push((n, ay + y + dy_valign));
            }
            frags.push(l.frag);
        }
        (frags, table_h)
    }

    /// Per-column min/max content widths (px) and the explicit width
    /// preferences (CSS 2.1 §17.5.2.2): single-column cell contributions
    /// first, then spanning cells widen their columns, then a declared px
    /// column width raises that column's max-content. Shared by the layout
    /// (a definite `cap`/`pct_basis` = the band) and the intrinsic-size query
    /// (`cap` = None, `pct_basis` = 0 — percentages behave as auto, declared
    /// widths uncapped, per css-sizing-3 §5.2.2).
    fn table_col_metrics(
        &self,
        tb: &TableBox,
        bs: f32,
        cap: Option<f32>,
        pct_basis: f32,
        inl: &InlineStyle,
    ) -> (Vec<f32>, Vec<f32>, Vec<Option<ColSpec>>) {
        let ncols = tb.ncols;
        // Explicit width preference: a `<col>`/`<colgroup>` width first
        // (§17.5.2.1 lists column elements ahead of first-row cells), else a
        // declared width on the column's first single-span cell.
        let mut col_w: Vec<Option<ColSpec>> = (0..ncols)
            .map(|c| tb.col_specs.get(c).copied().flatten())
            .collect();
        for cell in &tb.cells {
            if cell.colspan == 1 && cell.col < ncols && col_w[cell.col].is_none() {
                col_w[cell.col] = declared_track_width(self.dom, cell.b.node);
            }
        }

        let mut col_min = vec![0.0f32; ncols];
        let mut col_max = vec![0.0f32; ncols];
        // A fixed width replaces the max-content preference of its column:
        // as in Gecko and Blink, the column's other cells then contribute
        // only their min-content.
        let fixed_cols: Vec<bool> = (0..ncols)
            .map(|c| {
                matches!(col_w[c], Some(ColSpec::Px(_)))
                    || tb.cells.iter().any(|cell| {
                        cell.colspan == 1
                            && cell.col == c
                            && matches!(
                                declared_track_width(self.dom, cell.b.node),
                                Some(ColSpec::Px(_))
                            )
                    })
            })
            .collect();
        // Single-column cells first.
        for cell in &tb.cells {
            if cell.colspan != 1 || cell.col >= ncols {
                continue;
            }
            let (mn, mx) = self.cell_min_max(cell, cap, pct_basis, inl);
            let fixed = matches!(
                declared_track_width(self.dom, cell.b.node),
                Some(ColSpec::Px(_))
            );
            col_min[cell.col] = col_min[cell.col].max(mn);
            col_max[cell.col] = col_max[cell.col].max(if fixed_cols[cell.col] && !fixed {
                mn
            } else {
                mx
            });
        }
        // CSS Tables 3 #computing-column-measures: cells spanning N columns
        // update the measures based on cells of span up to N-1, in order of
        // increasing span.
        let mut spans: Vec<usize> = tb
            .cells
            .iter()
            .map(|cell| {
                (cell.col + cell.colspan)
                    .min(ncols)
                    .saturating_sub(cell.col)
            })
            .filter(|&span| span > 1)
            .collect();
        spans.sort_unstable();
        spans.dedup();
        if !tb.fixed_layout {
            span_percentages(self.dom, tb, &spans, &col_max, &mut col_w);
        }
        for n in spans {
            let (base_min, base_max) = (col_min.clone(), col_max.clone());
            for cell in &tb.cells {
                let end = (cell.col + cell.colspan).min(ncols);
                if end.saturating_sub(cell.col) != n {
                    continue;
                }
                let (mn, mx) = self.cell_min_max(cell, cap, pct_basis, inl);
                let columns = cell.col..end;
                let contributions = span_contributions(
                    &base_min[columns.clone()],
                    &base_max[columns.clone()],
                    mn,
                    mx,
                    bs * (n - 1) as f32,
                );
                for (c, (min, max)) in columns.zip(contributions) {
                    col_min[c] = col_min[c].max(min);
                    col_max[c] = col_max[c].max(max);
                }
            }
        }
        // A declared px column width is its preference, never below its
        // min-content.
        for c in 0..ncols {
            if let Some(ColSpec::Px(px)) = col_w[c] {
                col_max[c] = col_max[c].max(col_min[c].max(cap.map_or(px, |a| px.min(a))));
            }
        }
        (col_min, col_max, col_w)
    }

    /// The intrinsic (min-/max-content) width of a whole table, px: the sum of
    /// its columns' min/max plus border-spacing (CSS 2.1 §17.5.2.2 read as an
    /// intrinsic query — a table sizes to its column set). Percentages behave
    /// as auto and declared widths are uncapped (`cap = None`, `pct_basis = 0`).
    pub(super) fn table_intrinsic(
        &self,
        tb: &TableBox,
        table_node: NodeId,
        mode: IMode,
        inl: &InlineStyle,
    ) -> f32 {
        let ncols = tb.ncols;
        if ncols == 0 {
            return 0.0;
        }
        let (bs, _) = self.table_border_spacing(table_node);
        let (col_min, col_max, _) = self.table_col_metrics(tb, bs, None, 0.0, inl);
        let cols = match mode {
            IMode::Min => col_min,
            IMode::Max => col_max,
        };
        cols.iter().sum::<f32>() + bs * (ncols + 1) as f32
    }

    /// A cell's min-content and max-content OUTER (border-box) widths (px):
    /// its content intrinsic widths plus its own border and padding (margins
    /// don't apply to table cells — §17.5.1). CSS Tables 3 #outer-min-content:
    /// a declared width is no minimum (unlike CSS 2's informative §17.5.2.2),
    /// so a table narrower than its cells' declared widths shrinks them to
    /// their content, as Gecko and Blink do; it replaces the max-content
    /// preference, clamped to `cap` when set (the band) so one huge declared
    /// cell can't dominate the layout. `pct_basis` resolves percentage padding.
    fn cell_min_max(
        &self,
        cell: &super::tree::TableCell,
        cap: Option<f32>,
        pct_basis: f32,
        inl: &InlineStyle,
    ) -> (f32, f32) {
        let s = &cell.b.style;
        let bp = s.border[LEFT]
            + s.border[RIGHT]
            + self.pad(s, LEFT, pct_basis)
            + self.pad(s, RIGHT, pct_basis);
        let mn = self.intrinsic_w(&cell.b, IMode::Min, inl) + bp;
        let mut mx = self.intrinsic_w(&cell.b, IMode::Max, inl) + bp;
        if let Some(ColSpec::Px(px)) = declared_track_width(self.dom, cell.b.node) {
            // CSS Tables 3 #outer-max-content: the declared width is the
            // cell's box-sizing box; outer widths include padding and border.
            let outer = if s.border_box { px } else { px + bp };
            let outer = cap.map_or(outer, |a| outer.min(a));
            mx = mn.max(outer);
        }
        (mn.max(1.0), mx.max(mn))
    }

    /// The used border-spacing (px) from the cascade: the HTML UA default
    /// is 2px, and `cellspacing` is a presentational hint for it (HTML
    /// Rendering #tables-2). CSS 2.2 §17.6.1: one length sets both axes, two set horizontal and
    /// vertical. Spacing includes the outside edges and does not participate
    /// in the collapsed border model. Read inherited values from the cascade.
    fn table_border_spacing(&self, table: NodeId) -> (f32, f32) {
        if self
            .dom
            .computed_value_resolved(table, "border-collapse")
            .as_deref()
            == Some("collapse")
        {
            return (0.0, 0.0);
        }
        let Some(raw) = self.dom.computed_value_resolved(table, "border-spacing") else {
            return (0.0, 0.0);
        };
        let mut parts = raw.split_whitespace();
        let first = parts.next().unwrap_or("0");
        let second = parts.next().unwrap_or(first);
        let u = Units::of(self.dom, table);
        let length = |value: &str| {
            css_length_px(value, u)
                .or_else(|| value.parse::<f32>().ok())
                .unwrap_or(0.0)
                .max(0.0)
        };
        (length(first), length(second))
    }

    /// A table row's used minimum height from its computed 'height' (px),
    /// percentages against the table's definite height.
    fn row_height(&self, row: NodeId, table_h: Option<f32>) -> Option<f32> {
        let value = self.dom.computed_value_resolved(row, "height")?;
        let length = super::value::Len::parse_or(
            Some(&value),
            Units::of(self.dom, row),
            self.vp,
            super::value::Len::Auto,
        );
        length.resolve(table_h).filter(|height| *height > 0.0)
    }

    /// Vertical offset of a cell within its (possibly taller) row band (CSS
    /// 2.1 §17.5.4 + the HTML rendering hints): author `vertical-align` beats
    /// the `valign` presentational hint; an undeclared cell inherits through
    /// its row and row group (the UA `td,th,tr { vertical-align: inherit }` +
    /// `thead,tbody,tfoot { vertical-align: middle }`), so a bare cell defaults
    /// to MIDDLE. `baseline`/inline-only values ≈ top in the cell line model.
    fn cell_valign_offset(&self, cell: NodeId, cell_h: f32, span_h: f32) -> f32 {
        // Anonymous cells have initial (baseline) alignment, not HTML's
        // td/th presentational hints, and no DOM identity to query.
        if cell == super::NO_NODE {
            return 0.0;
        }
        let slack = (span_h - cell_h).max(0.0);
        if slack <= 0.0 {
            return 0.0;
        }
        let mut v = None;
        let mut cur = Some(cell);
        while let Some(n) = cur {
            v = self
                .dom
                .computed_style(n, "vertical-align")
                .or_else(|| self.dom.attr(n, "valign").map(str::to_owned))
                .map(|s| s.trim().to_ascii_lowercase());
            if v.is_some() {
                break;
            }
            // Climb cell → row → row group only.
            cur = self.dom.parent_composed(n).filter(|&p| {
                matches!(
                    self.dom.tag_name(p),
                    Some("tr" | "tbody" | "thead" | "tfoot")
                )
            });
        }
        match v.as_deref() {
            Some("bottom") => slack,
            Some("top" | "baseline") => 0.0,
            Some("middle") | None => slack / 2.0,
            _ => 0.0,
        }
    }

    /// Position an auto-width table narrower than its band (CSS 2.1 §17.4 /
    /// HTML `align`): centered for `margin:0 auto` or a centering context,
    /// right-aligned for `margin-left:auto`/`align=right`, else flush left.
    /// A DEFINITE-width table is already positioned by §10.3.3 auto margins in
    /// `horizontal`, so this only runs for the shrink-to-fit case.
    pub(super) fn table_lead(&self, id: NodeId, table_w: f32, band: f32) -> f32 {
        let slack = (band - table_w).max(0.0);
        if slack <= 0.0 {
            return 0.0;
        }
        let ml_auto = self.dom.computed_style(id, "margin-left").as_deref() == Some("auto");
        let mr_auto = self.dom.computed_style(id, "margin-right").as_deref() == Some("auto");
        match (ml_auto, mr_auto) {
            (true, false) => slack,      // margin-left:auto → right
            (true, true) => slack / 2.0, // margin:0 auto → center
            // HTML Rendering #tables-2 maps a table's own `align=center` to
            // auto inline margins; otherwise only `<center>` and legacy
            // `align` ancestors move it (#align-descendants). `text-align`
            // positions inline content, never this block-level box.
            _ => {
                let own = self.dom.attr(id, "align").map(str::trim);
                let align = if own.is_some_and(|v| v.eq_ignore_ascii_case("center")) {
                    Some(Align2::Center)
                } else {
                    super::style::legacy_descendant_align(self.dom, id)
                };
                match align {
                    Some(Align2::Center) => slack / 2.0,
                    Some(Align2::Right) => slack,
                    _ => 0.0,
                }
            }
        }
    }
}

/// CSS Tables 3 #width-distribution-algorithm: the used column widths for
/// an assignable width `assignable` (the table's content width less border
/// spacing). Four sizing-guesses — min-content, min-content-percentage,
/// min-content-specified and max-content — are nondecreasing per column;
/// the used widths interpolate linearly between the two consecutive guesses
/// whose sums bound the assignable width, or start from the max-content
/// guess and distribute the excess width to columns.
fn distribute_widths(
    min: &[f32],
    max: &[f32],
    specs: &[Option<ColSpec>],
    assignable: f32,
) -> Vec<f32> {
    let ncols = min.len();
    let percent = |c: usize| match specs[c] {
        Some(ColSpec::Pct(p)) if p > 0.0 => Some(p),
        _ => None,
    };
    let constrained = |c: usize| matches!(specs[c], Some(ColSpec::Px(_)));
    let percent_width = |c: usize, p: f32| (p * assignable).max(min[c]);
    let guesses: [Vec<f32>; 4] = [
        min.to_vec(),
        (0..ncols)
            .map(|c| percent(c).map_or(min[c], |p| percent_width(c, p)))
            .collect(),
        (0..ncols)
            .map(|c| match percent(c) {
                Some(p) => percent_width(c, p),
                None if constrained(c) => max[c].max(min[c]),
                None => min[c],
            })
            .collect(),
        (0..ncols)
            .map(|c| percent(c).map_or(max[c].max(min[c]), |p| percent_width(c, p)))
            .collect(),
    ];
    let sums: Vec<f32> = guesses.iter().map(|g| g.iter().sum()).collect();
    if assignable <= sums[0] {
        return guesses[0].iter().map(|w| w.max(1.0)).collect();
    }
    for i in 0..3 {
        if assignable <= sums[i + 1] {
            let span = sums[i + 1] - sums[i];
            let t = if span > 0.0 {
                (assignable - sums[i]) / span
            } else {
                1.0
            };
            return (0..ncols)
                .map(|c| (guesses[i][c] + (guesses[i + 1][c] - guesses[i][c]) * t).max(1.0))
                .collect();
        }
    }
    // #distributing-width-to-columns: the excess over the max-content guess.
    let mut widths = guesses[3].clone();
    let excess = assignable - sums[3];
    let unconstrained: Vec<usize> = (0..ncols)
        .filter(|&c| percent(c).is_none() && !constrained(c))
        .collect();
    let with_content =
        |cols: &[usize]| -> Vec<usize> { cols.iter().copied().filter(|&c| max[c] > 0.0).collect() };
    let constrained_cols: Vec<usize> = (0..ncols)
        .filter(|&c| percent(c).is_none() && constrained(c))
        .collect();
    let percent_cols: Vec<usize> = (0..ncols).filter(|&c| percent(c).is_some()).collect();
    let all: Vec<usize> = (0..ncols).collect();
    let by_max = |cols: &[usize]| cols.iter().map(|&c| max[c]).sum::<f32>();
    if !with_content(&unconstrained).is_empty() {
        let cols = with_content(&unconstrained);
        grow_by_weight(&mut widths, &cols, excess, |c| max[c], by_max(&cols));
    } else if !unconstrained.is_empty() {
        grow_by_weight(
            &mut widths,
            &unconstrained,
            excess,
            |_| 1.0,
            unconstrained.len() as f32,
        );
    } else if !with_content(&constrained_cols).is_empty() {
        let cols = with_content(&constrained_cols);
        grow_by_weight(&mut widths, &cols, excess, |c| max[c], by_max(&cols));
    } else if !percent_cols.is_empty() {
        let total: f32 = percent_cols.iter().filter_map(|&c| percent(c)).sum();
        grow_by_weight(
            &mut widths,
            &percent_cols,
            excess,
            |c| percent(c).unwrap_or(0.0),
            total,
        );
    } else {
        grow_by_weight(&mut widths, &all, excess, |_| 1.0, ncols as f32);
    }
    widths.iter().map(|w| w.max(1.0)).collect()
}

/// Fixed table layout column widths (§17.5.2.1): declared column widths are
/// honored, remaining space is divided equally over the rest. `content` is the
/// table's content width less inter-column spacing.
fn fixed_columns(col_w: &[Option<ColSpec>], ncols: usize, content: f32) -> Vec<f32> {
    let content = content.max(1.0);
    let mut widths = vec![0.0f32; ncols];
    let mut fixed_total = 0.0f32;
    let mut autos = Vec::new();
    for c in 0..ncols {
        match col_w[c] {
            Some(ColSpec::Px(px)) => {
                widths[c] = px.min(content);
                fixed_total += widths[c];
            }
            Some(ColSpec::Pct(p)) => {
                widths[c] = (p * content).clamp(0.0, content);
                fixed_total += widths[c];
            }
            None => autos.push(c),
        }
    }
    let rest = (content - fixed_total).max(0.0);
    if !autos.is_empty() {
        let each = rest / autos.len() as f32;
        for &c in &autos {
            widths[c] = each;
        }
    }
    for w in &mut widths {
        *w = w.max(1.0);
    }
    widths
}

/// CSS Tables 3 #computing-column-measures, intrinsic percentage widths:
/// for spans of increasing size, a spanning cell's percentage width less
/// its columns' percentages goes to those of them without one, in
/// proportion to their non-spanning max-content widths (`nonspan_max`),
/// or equally when those are all zero. A column keeps the largest
/// contribution. Every column's percentage is then limited to 100% less
/// the columns before it. Columns with a length width keep it.
fn span_percentages(
    dom: &crate::dom::Dom,
    tb: &TableBox,
    spans: &[usize],
    nonspan_max: &[f32],
    col_w: &mut [Option<ColSpec>],
) {
    let ncols = col_w.len();
    let percent = |spec: &Option<ColSpec>| match spec {
        Some(ColSpec::Pct(p)) => *p,
        _ => 0.0,
    };
    for &n in spans {
        let mut contributions: Vec<f32> = vec![0.0; ncols];
        for cell in &tb.cells {
            let end = (cell.col + cell.colspan).min(ncols);
            if end.saturating_sub(cell.col) != n {
                continue;
            }
            let Some(ColSpec::Pct(cell_pct)) = declared_track_width(dom, cell.b.node) else {
                continue;
            };
            let columns = cell.col..end;
            let rest =
                (cell_pct - columns.clone().map(|c| percent(&col_w[c])).sum::<f32>()).max(0.0);
            let open: Vec<usize> = columns.filter(|&c| col_w[c].is_none()).collect();
            let weight: f32 = open.iter().map(|&c| nonspan_max[c]).sum();
            for &c in &open {
                let share = if weight > 0.0 {
                    nonspan_max[c] / weight
                } else {
                    1.0 / open.len() as f32
                };
                contributions[c] = contributions[c].max(rest * share);
            }
        }
        for (spec, contribution) in col_w.iter_mut().zip(contributions) {
            if spec.is_none() && contribution > 0.0 {
                *spec = Some(ColSpec::Pct(contribution));
            }
        }
    }
    let mut total = 0.0f32;
    for spec in col_w.iter_mut() {
        if let Some(ColSpec::Pct(p)) = spec {
            *p = p.min((1.0 - total).max(0.0));
            total += *p;
        }
    }
}

/// CSS Tables 3 #computing-column-measures: a spanning cell's min- and
/// max-content contributions to each column it spans, given those columns'
/// measures based on cells of smaller span. Min-content beyond the columns'
/// min-content sum first fills each column's gap up to its max-content, in
/// proportion to that gap; anything beyond their max-content sum, and any
/// max-content excess, grows them in proportion to their max-content. With
/// no max-content to weigh by, the columns share it equally.
fn span_contributions(
    min: &[f32],
    max: &[f32],
    cell_min: f32,
    cell_max: f32,
    inner_spacing: f32,
) -> Vec<(f32, f32)> {
    let base_min: f32 = min.iter().sum();
    let base_max: f32 = max.iter().sum();
    let gap = base_max - base_min;
    let fill = (cell_min - base_min - inner_spacing).clamp(0.0, gap.max(0.0));
    let min_excess = (cell_min - base_max - inner_spacing).max(0.0);
    let max_excess = (cell_max - base_max - inner_spacing).max(0.0);
    let share = |c: usize| {
        if base_max > 0.0 {
            max[c] / base_max
        } else {
            1.0 / min.len() as f32
        }
    };
    (0..min.len())
        .map(|c| {
            let slack = if gap > 0.0 {
                (max[c] - min[c]) / gap
            } else {
                0.0
            };
            (
                min[c] + slack * fill + share(c) * min_excess,
                max[c] + share(c) * max_excess,
            )
        })
        .collect()
}

/// Grow the listed `cols` of `target` by `extra` px total, in proportion to
/// each column's `weight` (exact in f32 — no integer remainder to hand out).
fn grow_by_weight(
    target: &mut [f32],
    cols: &[usize],
    extra: f32,
    weight: impl Fn(usize) -> f32,
    total_weight: f32,
) {
    if extra <= 0.0 || cols.is_empty() || total_weight <= 0.0 {
        return;
    }
    for &c in cols {
        target[c] += extra * weight(c) / total_weight;
    }
}
