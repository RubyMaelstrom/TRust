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
//! Everything is f32 CSS px, quantized to cells only when the fragments reach
//! the painter — the same discipline as the flex/grid modules. We paint no
//! cell borders/grid lines (a terminal row is precious; the columns alone
//! carry the layout, which is the whole point for the ubiquitous
//! table-as-layout page). A cell's own `background` still fills, because a
//! cell is a real fragment; row/row-group backgrounds don't (they generate no
//! fragment — documented deferral).

use crate::dom::NodeId;
use crate::layout2::{Units, css_length_px};

use super::flow::{Flow, Frag};
use super::intrinsic::IMode;
use super::style::{Align2, InlineStyle, LEFT, RIGHT};
use super::tree::{ColSpec, TableBox, declared_track_width};

/// The resolved column geometry of a table.
pub(super) struct TableCols {
    /// Per-column used width (px): the border-box width available to a
    /// single-span cell in that column.
    pub widths: Vec<f32>,
    /// Horizontal border-spacing between columns (px).
    pub bs: f32,
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
        let bs = self.table_border_spacing(table_node);
        if ncols == 0 {
            return TableCols {
                widths: Vec::new(),
                bs,
                table_w: 0.0,
            };
        }
        let spacing = bs * (ncols - 1) as f32;
        // Space available to the columns' content (the band, less spacing).
        let avail = (avail_w - spacing).max(1.0);
        let cellpad = self.table_cellpadding(table_node);

        // Per-column min/max content widths + explicit width preferences (the
        // shared metrics; a declared px cap clamps to the band for layout).
        let (col_min, col_max, col_w) =
            self.table_col_metrics(tb, bs, cellpad, Some(avail), avail, inl);

        // Fixed layout: a definite width + `table-layout:fixed` ignores
        // content and divides by declared column widths (§17.5.2.1).
        if tb.fixed_layout && !width_auto {
            return TableCols {
                widths: fixed_columns(&col_w, ncols, avail),
                bs,
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

        // Target per column: an explicit width (px, or % of the used table
        // width) floored at its min; otherwise its max-content.
        let table_used_w = used + spacing;
        let mut target: Vec<f32> = (0..ncols)
            .map(|c| {
                let t = match col_w[c] {
                    Some(ColSpec::Px(px)) => px,
                    Some(ColSpec::Pct(p)) => p * table_used_w,
                    None => col_max[c],
                };
                t.max(col_min[c]).max(1.0)
            })
            .collect();

        let target_sum: f32 = target.iter().sum();
        if target_sum < used {
            // Grow: distribute slack to the auto (no explicit width) columns by
            // their max-content; if none are auto, grow all proportionally.
            let extra = used - target_sum;
            let auto: Vec<usize> = (0..ncols).filter(|&c| col_w[c].is_none()).collect();
            if !auto.is_empty() {
                let weight: f32 = auto.iter().map(|&c| col_max[c].max(1.0)).sum();
                grow_by_weight(&mut target, &auto, extra, |c| col_max[c].max(1.0), weight);
            } else {
                let snapshot: Vec<f32> = target.iter().map(|&w| w.max(1.0)).collect();
                let all: Vec<usize> = (0..ncols).collect();
                let weight: f32 = snapshot.iter().sum::<f32>().max(1.0);
                grow_by_weight(&mut target, &all, extra, |c| snapshot[c], weight);
            }
        } else if target_sum > used {
            // Shrink toward each column's min, proportional to the slack above
            // it (exact in f32 — no integer-rounding residual to sweep up).
            let over = target_sum - used;
            let head: f32 = (0..ncols).map(|c| target[c] - col_min[c]).sum();
            if head > 0.0 {
                for c in 0..ncols {
                    let slack_above = target[c] - col_min[c];
                    target[c] -= (over * slack_above / head).min(slack_above);
                }
            }
        }
        let table_w = target.iter().sum::<f32>() + spacing;
        TableCols {
            widths: target,
            bs,
            table_w,
        }
    }

    /// Lay the table's cells at their resolved column widths and return the
    /// cell fragments (positioned absolutely at `content_x`/`content_top`) and
    /// the grid's height (px). Row heights come from the laid cell heights
    /// (§17.5.3); vertical alignment places each cell in its row band
    /// (§17.5.4).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn table_grid<'t>(
        &self,
        tb: &'t TableBox,
        table_node: NodeId,
        cols: &TableCols,
        content_x: f32,
        content_top: f32,
        def_ch: Option<f32>,
        inl: &InlineStyle,
        anchors: &mut Vec<(NodeId, f32)>,
    ) -> (Vec<Frag<'t>>, f32) {
        let ncols = tb.ncols;
        let nrows = tb.nrows;
        if ncols == 0 || nrows == 0 {
            return (Vec::new(), 0.0);
        }
        let bs = cols.bs;
        // Column left edges (with inter-column spacing).
        let mut col_x = vec![0.0f32; ncols];
        let mut acc = 0.0;
        for (c, x) in col_x.iter_mut().enumerate() {
            *x = acc;
            acc += cols.widths.get(c).copied().unwrap_or(1.0) + bs;
        }

        // `cellpadding` (a legacy HTML attribute) insets a cell's content when
        // the cell sets NO CSS padding of its own — the CSS padding, applied by
        // the cell's own box, wins per the presentational-hint priority. It is
        // already folded into the column widths (via `cell_min_max`), so a
        // cellpadded auto column is wide enough for content + padding.
        let cellpad = self.table_cellpadding(table_node);

        // Lay every cell at its spanned-column width. Each entry:
        // (fragment, its local anchors, horizontal pad, vertical pad).
        struct Laid<'t> {
            frag: Frag<'t>,
            anchors: Vec<(NodeId, f32)>,
            /// The cell's spanned border-box width (px) — its CB for %.
            cell_w: f32,
            ph: f32,
            pv: f32,
        }
        let mut laid: Vec<Laid<'t>> = Vec::with_capacity(tb.cells.len());
        for cell in &tb.cells {
            let end = (cell.col + cell.colspan).min(ncols);
            let span = end.saturating_sub(cell.col).max(1);
            let cell_w =
                (cols.widths[cell.col..end].iter().sum::<f32>() + bs * (span - 1) as f32).max(1.0);
            let s = &cell.b.style;
            let has_css_pad = cell_has_css_padding(self, cell.b.node);
            let (ph, pv) = if has_css_pad || cellpad == 0.0 {
                (0.0, 0.0)
            } else {
                (cellpad, cellpad)
            };
            // The cell's own border+padding wrap its content (item_frag adds
            // them around the imposed content width). The cellpadding inset is
            // an extra frame OUTSIDE the cell's fragment.
            let cbp = s.border[LEFT]
                + s.border[RIGHT]
                + self.pad(s, LEFT, cell_w)
                + self.pad(s, RIGHT, cell_w);
            let content_w = (cell_w - 2.0 * ph - cbp).max(0.0);
            let def_h = s.height.resolve(def_ch).map(|v| v.max(0.0));
            let (frag, anc) = self.item_frag(&cell.b, content_w, cell_w, def_h, inl);
            laid.push(Laid {
                frag,
                anchors: anc,
                cell_w,
                ph,
                pv,
            });
        }

        // Row heights (§17.5.3): the tallest single-row cell sets each row; a
        // row-spanning cell whose box exceeds its spanned rows pushes the
        // deficit onto its last row.
        let mut row_h = vec![0.0f32; nrows];
        for (cell, l) in tb.cells.iter().zip(&laid) {
            if cell.rowspan <= 1 && cell.row < nrows {
                row_h[cell.row] = row_h[cell.row].max(l.frag.h + 2.0 * l.pv);
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
            let need = l.frag.h + 2.0 * l.pv;
            let have: f32 =
                row_h[cell.row..end].iter().sum::<f32>() + bs * (end - cell.row - 1) as f32;
            if need > have {
                row_h[end - 1] += need - have;
            }
        }
        let mut row_y = vec![0.0f32; nrows];
        let mut acc = 0.0;
        for r in 0..nrows {
            row_y[r] = acc;
            acc += row_h[r] + bs;
        }
        let table_h = (acc - bs).max(0.0);

        // Place each cell at its column/row origin, vertically aligned in its
        // (possibly taller) row band per `vertical-align`/`valign`.
        let mut frags: Vec<Frag<'t>> = Vec::with_capacity(laid.len());
        for (cell, mut l) in tb.cells.iter().zip(laid) {
            let end = (cell.row + cell.rowspan).min(nrows);
            let span_h = row_h[cell.row..end].iter().sum::<f32>()
                + bs * (end.saturating_sub(cell.row + 1)) as f32;
            let cell_outer_h = l.frag.h + 2.0 * l.pv;
            let dy_valign = self.cell_valign_offset(cell.b.node, cell_outer_h, span_h);
            // §9.4.3 relative offset / transform translation — a cell's CB for
            // percentages is its own box.
            let (rx, ry) =
                self.paint_offset(&cell.b.style, l.cell_w, Some(span_h), l.frag.w, l.frag.h);
            let x = content_x + col_x[cell.col] + l.ph + rx;
            let y = content_top + row_y[cell.row] + dy_valign + l.pv + ry;
            Flow::offset_frag(&mut l.frag, x, y);
            for (n, ay) in l.anchors {
                anchors.push((n, ay + y));
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
        cellpad: f32,
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
        // Single-column cells first.
        for cell in &tb.cells {
            if cell.colspan != 1 || cell.col >= ncols {
                continue;
            }
            let (mn, mx) = self.cell_min_max(cell, cellpad, cap, pct_basis, inl);
            col_min[cell.col] = col_min[cell.col].max(mn);
            col_max[cell.col] = col_max[cell.col].max(mx);
        }
        // Spanning cells widen the spanned columns so the span fits (§17.5.2.2
        // step 3 — widen all spanned columns by ~the same amount).
        for cell in &tb.cells {
            if cell.colspan <= 1 {
                continue;
            }
            let end = (cell.col + cell.colspan).min(ncols);
            let span = end.saturating_sub(cell.col);
            if span == 0 {
                continue;
            }
            let (mn, mx) = self.cell_min_max(cell, cellpad, cap, pct_basis, inl);
            let inner_bs = bs * (span - 1) as f32;
            distribute_deficit(&mut col_min[cell.col..end], (mn - inner_bs).max(0.0));
            distribute_deficit(&mut col_max[cell.col..end], (mx - inner_bs).max(0.0));
        }
        // A declared px column width raises the column's max-content.
        for c in 0..ncols {
            if let Some(ColSpec::Px(px)) = col_w[c] {
                col_max[c] = col_max[c].max(cap.map_or(px, |a| px.min(a)));
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
        let bs = self.table_border_spacing(table_node);
        let cellpad = self.table_cellpadding(table_node);
        let (col_min, col_max, _) = self.table_col_metrics(tb, bs, cellpad, None, 0.0, inl);
        let cols = match mode {
            IMode::Min => col_min,
            IMode::Max => col_max,
        };
        cols.iter().sum::<f32>() + bs * (ncols - 1) as f32
    }

    /// A cell's min-content and max-content OUTER (border-box) widths (px):
    /// its content intrinsic widths plus its own border and padding (margins
    /// don't apply to table cells — §17.5.1). A declared width raises both
    /// (§17.5.2.2 step 1 — "if W is greater than MCW, W is the minimum"); it is
    /// clamped to `cap` when set (the band) so one huge declared cell can't
    /// dominate the layout. `pct_basis` resolves percentage padding.
    fn cell_min_max(
        &self,
        cell: &super::tree::TableCell,
        cellpad: f32,
        cap: Option<f32>,
        pct_basis: f32,
        inl: &InlineStyle,
    ) -> (f32, f32) {
        let s = &cell.b.style;
        // The cell's own border + padding, plus the legacy `cellpadding`
        // inset when the cell declares no CSS padding of its own (so an
        // auto column reserves room for it — matching how `table_grid` lays
        // the cell).
        let extra_pad = if cellpad > 0.0 && !cell_has_css_padding(self, cell.b.node) {
            2.0 * cellpad
        } else {
            0.0
        };
        let bp = s.border[LEFT]
            + s.border[RIGHT]
            + self.pad(s, LEFT, pct_basis)
            + self.pad(s, RIGHT, pct_basis)
            + extra_pad;
        let mut mn = self.intrinsic_w(&cell.b, IMode::Min, inl) + bp;
        let mut mx = self.intrinsic_w(&cell.b, IMode::Max, inl) + bp;
        if let Some(ColSpec::Px(px)) = declared_track_width(self.dom, cell.b.node) {
            let px = cap.map_or(px, |a| px.min(a));
            mn = mn.max(px);
            mx = mx.max(px);
        }
        (mn.max(1.0), mx.max(mn))
    }

    /// The table's `cellpadding` attribute (px). Legacy HTML; 0 when unset.
    fn table_cellpadding(&self, table: NodeId) -> f32 {
        self.dom
            .attr(table, "cellpadding")
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(0.0)
            .max(0.0)
    }

    /// Horizontal border-spacing (px): CSS `border-spacing` if set, else the
    /// HTML `cellspacing` attribute (HTML §15.3.13 maps it to `border-spacing`).
    /// Default 0 — a terminal's columns are separated by content/cellpadding.
    /// Read through the author cascade (`computed_style`); `border-spacing`
    /// isn't a registry-tracked property.
    fn table_border_spacing(&self, table: NodeId) -> f32 {
        let raw = self
            .dom
            .computed_style(table, "border-spacing")
            .or_else(|| {
                self.dom
                    .attr(table, "cellspacing")
                    .map(|s| s.trim().to_string())
            });
        let Some(raw) = raw else { return 0.0 };
        let first = raw.split_whitespace().next().unwrap_or("0");
        let u = Units::of(self.dom, table);
        css_length_px(first, u)
            .or_else(|| first.parse::<f32>().ok())
            .unwrap_or(0.0)
            .max(0.0)
    }

    /// Vertical offset of a cell within its (possibly taller) row band (CSS
    /// 2.1 §17.5.4 + the HTML rendering hints): author `vertical-align` beats
    /// the `valign` presentational hint; an undeclared cell inherits through
    /// its row and row group (the UA `td,th,tr { vertical-align: inherit }` +
    /// `thead,tbody,tfoot { vertical-align: middle }`), so a bare cell defaults
    /// to MIDDLE. `baseline`/inline-only values ≈ top in the cell line model.
    fn cell_valign_offset(&self, cell: NodeId, cell_h: f32, span_h: f32) -> f32 {
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
            _ => match super::style::block_align(self.dom, id) {
                Align2::Center => slack / 2.0,
                Align2::Right => slack,
                _ => 0.0,
            },
        }
    }
}

/// Whether the cell sets any CSS padding of its own (so `cellpadding` loses).
fn cell_has_css_padding(flow: &Flow<'_>, node: NodeId) -> bool {
    [
        "padding",
        "padding-left",
        "padding-right",
        "padding-top",
        "padding-bottom",
    ]
    .iter()
    .any(|p| flow.dom.computed_style(node, p).is_some())
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

/// Raise the widths in `slice` so their sum is at least `need`, adding the
/// deficit in equal parts (CSS 2.1 §17.5.2.2 step 3 — widen all spanned
/// columns by ~the same amount).
fn distribute_deficit(slice: &mut [f32], need: f32) {
    let n = slice.len();
    if n == 0 {
        return;
    }
    let have: f32 = slice.iter().sum();
    if need <= have {
        return;
    }
    let extra = (need - have) / n as f32;
    for w in slice.iter_mut() {
        *w += extra;
    }
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
