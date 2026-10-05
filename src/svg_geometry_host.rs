//! Host backing for the SVG 2 geometry DOM methods (`__svg_geometry`).
//!
//! `SVGGraphicsElement.getBBox/getCTM/getScreenCTM`, the
//! `SVGGeometryElement` path queries and the `SVGTextContentElement` text
//! metrics read geometry that `Dom` derives with the inline-SVG paint
//! pipeline (`dom::svg_geometry`). This adapter supplies the one input the
//! DOM cannot know by itself — the size of the outermost `svg` element's
//! viewport, which CSS layout determines — and converts results to values.
//! The platform performs the Web IDL argument conversion and brand checks.

use super::{Ctx, Value, ensure_host_geometry, host_arg_node, host_dom};
use crate::dom::svg_geometry::BoundingBoxOptions;

/// CSS Images 3 #default-object-size, also used for an outermost `svg`
/// element that has no box (display:none or not in a document).
const DEFAULT_VIEWPORT: (f64, f64) = (300.0, 150.0);

const BBOX: u8 = 0;
const CTM: u8 = 1;
const SCREEN_CTM: u8 = 2;
const TOTAL_LENGTH: u8 = 3;
const POINT_AT_LENGTH: u8 = 4;
const POINT_IN_FILL: u8 = 5;
const POINT_IN_STROKE: u8 = 6;
const NUMBER_OF_CHARS: u8 = 7;
const COMPUTED_TEXT_LENGTH: u8 = 8;
const SUBSTRING_LENGTH: u8 = 9;

fn number(args: &[Value], index: usize) -> f64 {
    args.get(index)
        .and_then(Value::as_num_opt)
        .unwrap_or(f64::NAN)
}

/// The viewport of `id`'s outermost `svg` element: its definite cascaded
/// size, otherwise its laid-out `width`/`height` used values. Only consulted
/// when a result can depend on it: always for matrices, and for the other
/// queries when the outermost `svg` element has no `viewBox` (whose size
/// would otherwise establish the user space).
fn outer_viewport(ctx: &mut Ctx, id: usize, matrix: bool) -> (f64, f64) {
    let (outermost, definite) = {
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        let Some(outermost) = dom.svg_outermost(id) else {
            return DEFAULT_VIEWPORT;
        };
        if !matrix
            && dom
                .attr(outermost, "viewBox")
                .is_some_and(svg_view_box_is_valid)
        {
            return DEFAULT_VIEWPORT;
        }
        let connected = dom.is_connected(outermost);
        (
            connected.then_some(outermost),
            dom.svg_definite_viewport(outermost),
        )
    };
    if let Some(size) = definite {
        return size;
    }
    let Some(outermost) = outermost else {
        return DEFAULT_VIEWPORT;
    };
    let cache = ensure_host_geometry(ctx, "svg-viewport", Some(outermost));
    let cached = cache.borrow();
    cached
        .boxes
        .get(&outermost)
        .map_or(DEFAULT_VIEWPORT, |rect| {
            (
                rect.css_width.unwrap_or(rect.width),
                rect.css_height.unwrap_or(rect.height),
            )
        })
}

fn svg_view_box_is_valid(value: &str) -> bool {
    value
        .parse::<svgtypes::ViewBox>()
        .is_ok_and(|view_box| view_box.w > 0.0 && view_box.h > 0.0)
}

/// `__svg_geometry(node, operation, a, b, c)`. Returns `null` for a stale or
/// inaccessible node, for a matrix of a node outside the document, and for a
/// substring that starts past the last addressable character.
pub(super) fn call(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let operation = number(args, 1);
    let operation = if operation.is_finite() && (0.0..=255.0).contains(&operation) {
        operation as u8
    } else {
        return Ok(Value::Null);
    };
    let Some(id) = ({
        let dom = host_dom(ctx);
        let dom = dom.borrow();
        host_arg_node(&dom, args, 0)
            .filter(|&id| dom.namespace_uri(id) == Some("http://www.w3.org/2000/svg"))
    }) else {
        return Ok(Value::Null);
    };
    let outer = outer_viewport(ctx, id, matches!(operation, CTM | SCREEN_CTM));
    let dom = host_dom(ctx);
    let dom = dom.borrow();
    Ok(match operation {
        BBOX => {
            let flags = number(args, 2);
            let flags = if flags.is_finite() { flags as u32 } else { 1 };
            let options = BoundingBoxOptions {
                fill: flags & 1 != 0,
                stroke: flags & 2 != 0,
                markers: flags & 4 != 0,
                clipped: flags & 8 != 0,
            };
            let rect = dom.svg_bbox(id, options, outer);
            drop(dom);
            ctx.make_array(rect.into_iter().map(Value::Num).collect())
        }
        CTM | SCREEN_CTM => match dom.svg_ctm(id, operation == SCREEN_CTM, outer) {
            Some(matrix) => {
                drop(dom);
                ctx.make_array(matrix.into_iter().map(Value::Num).collect())
            }
            None => Value::Null,
        },
        TOTAL_LENGTH => Value::Num(dom.svg_geometry_path(id, outer).total_length()),
        POINT_AT_LENGTH => {
            let (x, y) = dom
                .svg_geometry_path(id, outer)
                .point_at_length(number(args, 2));
            drop(dom);
            ctx.make_array(vec![Value::Num(x), Value::Num(y)])
        }
        POINT_IN_FILL => Value::Bool(
            dom.svg_geometry_path(id, outer)
                .contains_in_fill(number(args, 2), number(args, 3)),
        ),
        POINT_IN_STROKE => Value::Bool(
            dom.svg_geometry_path(id, outer)
                .contains_in_stroke(number(args, 2), number(args, 3)),
        ),
        NUMBER_OF_CHARS => Value::Num(f64::from(dom.svg_text_metrics(id, outer).number_of_chars())),
        COMPUTED_TEXT_LENGTH => Value::Num(dom.svg_text_metrics(id, outer).computed_length()),
        SUBSTRING_LENGTH => {
            let index = |value: f64| {
                (value.is_finite() && value >= 0.0).then(|| value.min(f64::from(u32::MAX)) as u32)
            };
            match (index(number(args, 2)), index(number(args, 3))) {
                (Some(charnum), Some(nchars)) => dom
                    .svg_text_metrics(id, outer)
                    .substring_length(charnum, nchars)
                    .map_or(Value::Null, Value::Num),
                _ => Value::Null,
            }
        }
        _ => Value::Null,
    })
}
