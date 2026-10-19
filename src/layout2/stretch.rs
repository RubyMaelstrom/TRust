//! CSS Sizing 3 #stretch-fit-sizing (CSSWG 81c27f686901, 2026-09-06).
//! The stretch keyword sizes a margin box, independently of box-sizing.
//! Its block-flow margin exception inspects the element establishing the
//! containing block, passing through inline and anonymous box wrappers.

use super::style::{BOTTOM, BoxStyle, LEFT, RIGHT, TOP};
use super::value::Len;
use super::{Dom, NO_NODE, NodeId};

#[allow(clippy::too_many_arguments)]
pub(super) fn content_size(
    dom: &Dom,
    node: NodeId,
    style: &BoxStyle,
    value: &Len,
    axis: usize,
    basis: Option<f32>,
    percentage_basis: Option<f32>,
    border_padding: f32,
) -> Option<f32> {
    if !matches!(value, Len::Stretch) {
        return None;
    }
    let basis = basis?;
    let sides = if axis == 0 {
        [LEFT, RIGHT]
    } else {
        [TOP, BOTTOM]
    };
    let ignored = ignored_block_margins(dom, node, style, axis, percentage_basis);
    let margins: f32 = sides
        .into_iter()
        .enumerate()
        .map(|(index, side)| {
            if ignored[index] {
                0.
            } else {
                style.margin[side].resolve(percentage_basis).unwrap_or(0.)
            }
        })
        .sum();
    Some((basis - border_padding - margins).max(0.))
}

fn ignored_block_margins(
    dom: &Dom,
    node: NodeId,
    style: &BoxStyle,
    axis: usize,
    percentage_basis: Option<f32>,
) -> [bool; 2] {
    if style.float.is_some() || style.position.out_of_flow() {
        return [false; 2];
    }
    let own = if node == NO_NODE {
        style.pseudo.map(|(node, _)| node)
    } else {
        Some(node)
    };
    let Some(own) = own else { return [false; 2] };
    if node != NO_NODE
        && dom
            .effective_display(own)
            .as_deref()
            .is_some_and(|d| d.starts_with("inline") || d == "none")
    {
        return [false; 2];
    }
    let mut parent = if style.pseudo.is_some() {
        Some(own)
    } else {
        dom.parent_flat(own)
    };
    let parent = loop {
        let Some(candidate) = parent else {
            return [false; 2];
        };
        let display = dom.effective_display(candidate);
        if dom.tag_name(candidate).is_some()
            && !matches!(
                display.as_deref(),
                None | Some("inline" | "contents" | "ruby" | "ruby-base" | "ruby-text")
            )
        {
            break candidate;
        }
        parent = dom.parent_flat(candidate);
    };
    let display = dom.effective_display(parent);
    // Flex/grid/table children participate in their own formatting algorithm;
    // an inline wrapper does not change which block container owns this one.
    if !matches!(
        display.as_deref(),
        Some("block" | "list-item" | "flow-root" | "inline-block" | "table-cell" | "table-caption")
    ) {
        return [false; 2];
    }
    let parent_style = BoxStyle::of(
        dom,
        parent,
        super::value::Vp {
            w: dom.viewport_px().0,
            h: dom.viewport_px().1,
        },
    );
    let block_axis = usize::from(parent_style.vertical.is_none());
    if axis != block_axis || independent(dom, parent, &parent_style, display.as_deref()) {
        return [false; 2];
    }
    let sides = if axis == 0 {
        [LEFT, RIGHT]
    } else {
        [TOP, BOTTOM]
    };
    sides.map(|side| {
        parent_style.border[side] == 0.
            && parent_style.padding[side]
                .resolve(percentage_basis)
                .unwrap_or(0.)
                == 0.
    })
}

fn independent(dom: &Dom, node: NodeId, style: &BoxStyle, display: Option<&str>) -> bool {
    if dom.is_document_element(node)
        || matches!(
            display,
            Some("flow-root" | "inline-block" | "table-cell" | "table-caption")
        )
        || style.float.is_some()
        || style.position.out_of_flow()
        || style.size_container != 0
        || style.vertical.is_some()
        || style.contain_layout
        || style.contain_paint
        || super::style::block_content_align(dom, node).is_some()
        || (dom.overflow_axes(node).iter().any(|axis| axis.scrollable())
            && !(dom.tag_name(node) == Some("body")
                && dom.parent_flat(node).is_some_and(|root| {
                    dom.is_document_element(root)
                        && super::overflow::viewport_overflow_source_for(dom, root) == Some(node)
                })))
        || dom
            .computed_value_resolved(node, "column-count")
            .as_deref()
            .is_some_and(|v| v != "auto")
        || dom
            .computed_value_resolved(node, "column-width")
            .as_deref()
            .is_some_and(|v| v != "auto")
        || dom.computed_value_resolved(node, "column-span").as_deref() == Some("all")
    {
        return true;
    }
    dom.parent_flat(node)
        .and_then(|parent| dom.effective_display(parent))
        .as_deref()
        .is_some_and(|d| matches!(d, "flex" | "inline-flex" | "grid" | "inline-grid"))
}
