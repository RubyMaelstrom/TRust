//! Retained CSS transform values, independent of normal-flow coordinates.
//!
//! CSS Transforms 1 #transformation-matrix-computation / #transform-box and
//! Transforms 2 #ctm (CSSWG snapshot 81c27f686901, 2026-09-06): compose the
//! individual translate, rotate and scale before the authored function list,
//! about the specified reference-box origin. A non-none identity still forms
//! a containing block. Percentages and non-linear math remain unresolved
//! until the fragment's reference box is known.

use std::sync::Arc;

use crate::core::CssPoint;
use crate::render::matrix3d as m3;
use crate::render::{Affine2d, CssRect, TransformStep};

use super::Units;
use super::flow::Frag;
use super::value::{Len, Vp};

#[derive(Clone, Debug, PartialEq)]
enum Operation {
    Translate([Len; 2]),
    Matrix(Affine2d),
    /// A three-dimensional transform function (CSS Transforms 2
    /// #three-d-transform-functions) as its 4x4 matrix.
    Matrix3d(crate::render::matrix3d::Matrix3d),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Transform {
    operations: Vec<Operation>,
    origin: [Len; 2],
    /// The `transform-origin` z offset in CSS px.
    origin_z: f32,
    content_box: bool,
}

impl Transform {
    pub(super) fn parse(
        value: impl Fn(&str) -> Option<String>,
        units: Units,
        viewport: Vp,
    ) -> Option<Arc<Self>> {
        let mut operations = Vec::new();
        let mut present = false;
        if let Some(text) = value("translate").filter(|v| !none(v))
            && let Some(parts) = components(&text)
            && (1..=3).contains(&parts.len())
            && let Some(x) = length(parts[0], units, viewport)
            && let Some(y) = parts
                .get(1)
                .map_or(Some(Len::px(0.)), |s| length(s, units, viewport))
            && let Some(z) = parts
                .get(2)
                .map_or(Some(0.), |s| absolute_length(s, units, viewport))
        {
            operations.push(Operation::Translate([x, y]));
            if z != 0. {
                operations.push(Operation::Matrix3d(m3::translate(0., 0., z)));
            }
            present = true;
        }
        if let Some(text) = value("rotate").filter(|v| !none(v))
            && let Some(rotation) = individual_rotation(&text)
        {
            operations.push(rotation);
            present = true;
        }
        if let Some(text) = value("scale").filter(|v| !none(v))
            && let Some(parts) = components(&text)
            && (1..=3).contains(&parts.len())
            && let Some(x) = scale(parts[0])
            && let Some(y) = parts.get(1).map_or(Some(x), |s| scale(s))
            && let Some(z) = parts.get(2).map_or(Some(1.), |s| scale(s))
        {
            operations.push(if z == 1. {
                Operation::Matrix(Affine2d::scale(x, y))
            } else {
                Operation::Matrix3d(m3::scale(x, y, z))
            });
            present = true;
        }
        if let Some(text) = value("transform").filter(|v| !none(v))
            && let Some(list) = functions(&text, units, viewport)
        {
            operations.extend(list);
            present = true;
        }
        if !present {
            return None;
        }
        let origin_value = value("transform-origin");
        let origin_z = origin_value
            .as_deref()
            .and_then(|s| origin_depth(s, units, viewport))
            .unwrap_or(0.);
        let origin = origin_value
            .and_then(|s| origin(&s, units, viewport))
            .unwrap_or_else(|| [percent(50.), percent(50.)]);
        Some(Arc::new(Self {
            operations,
            origin,
            origin_z,
            // Transforms 1 #transform-box: for CSS layout boxes fill-box
            // maps to content-box; stroke-box and view-box map to border-box.
            content_box: value("transform-box")
                .is_some_and(|v| matches!(v.trim(), "content-box" | "fill-box")),
        }))
    }

    /// The matrix acts on untransformed absolute layout coordinates. Keeping
    /// the origin relative to the fragment means relocating cached fragments
    /// cannot leave a stale, baked-in transform origin behind.
    pub(super) fn matrix(&self, fragment: &Frag) -> Affine2d {
        let reference = if self.content_box {
            fragment.content_box()
        } else {
            CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h)
        };
        let origin = CssPoint::new(
            reference.x + self.origin[0].resolve(Some(reference.width)).unwrap_or(0.),
            reference.y + self.origin[1].resolve(Some(reference.height)).unwrap_or(0.),
        );
        let translation = |[x, y]: &[Len; 2]| {
            (
                x.resolve(Some(reference.width)).unwrap_or(0.),
                y.resolve(Some(reference.height)).unwrap_or(0.),
            )
        };
        if self
            .operations
            .iter()
            .any(|op| matches!(op, Operation::Matrix3d(_)))
        {
            // CSS Transforms 2 #ctm: compose in three dimensions, about the
            // origin's depth too, then flatten the box into its parent's
            // plane (`transform-style: flat`).
            let mut matrix = m3::translate(origin.x, origin.y, self.origin_z);
            for op in &self.operations {
                let factor = match op {
                    Operation::Translate(lengths) => {
                        let (x, y) = translation(lengths);
                        m3::translate(x, y, 0.)
                    }
                    Operation::Matrix(matrix) => m3::from_affine(*matrix),
                    Operation::Matrix3d(matrix) => *matrix,
                };
                matrix = m3::multiply(&matrix, &factor);
            }
            let matrix = m3::multiply(
                &matrix,
                &m3::translate(-origin.x, -origin.y, -self.origin_z),
            );
            return m3::flatten(&matrix, origin);
        }
        let mut matrix = Affine2d::translate(origin.x, origin.y);
        for op in &self.operations {
            matrix = matrix.then(match op {
                Operation::Translate(lengths) => {
                    let (x, y) = translation(lengths);
                    Affine2d::translate(x, y)
                }
                Operation::Matrix(matrix) => *matrix,
                Operation::Matrix3d(_) => unreachable!("composed in three dimensions above"),
            });
        }
        matrix.then(Affine2d::translate(-origin.x, -origin.y))
    }

    pub(super) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + 2 * std::mem::size_of::<usize>()
            + self.operations.capacity() * std::mem::size_of::<Operation>()
            + self
                .origin
                .iter()
                .map(super::memo::len_bytes)
                .sum::<usize>()
            + self
                .operations
                .iter()
                .map(|op| match op {
                    Operation::Translate(values) => values.iter().map(super::memo::len_bytes).sum(),
                    Operation::Matrix(_) | Operation::Matrix3d(_) => 0,
                })
                .sum::<usize>()
    }
}

pub(super) fn matrix(fragment: &Frag) -> Affine2d {
    fragment
        .paint
        .transform
        .as_ref()
        .map_or(Affine2d::IDENTITY, |t| t.matrix(fragment))
}

pub(super) fn bounds(matrix: Affine2d, rect: CssRect) -> CssRect {
    if matrix.is_identity() {
        return rect;
    }
    let points = [
        CssPoint::new(rect.x, rect.y),
        CssPoint::new(rect.x + rect.width, rect.y),
        CssPoint::new(rect.x, rect.y + rect.height),
        CssPoint::new(rect.x + rect.width, rect.y + rect.height),
    ]
    .map(|p| matrix.map_point(p));
    let mut left = points[0].x;
    let mut top = points[0].y;
    let mut right = left;
    let mut bottom = top;
    for p in &points[1..] {
        left = left.min(p.x);
        top = top.min(p.y);
        right = right.max(p.x);
        bottom = bottom.max(p.y);
    }
    CssRect::new(left, top, right - left, bottom - top)
}

fn none(text: &str) -> bool {
    text.trim().is_empty() || text.trim().eq_ignore_ascii_case("none")
}

fn length(text: &str, units: Units, viewport: Vp) -> Option<Len> {
    // Len also serves legacy HTML sizing. Transform CSS does not inherit its
    // unitless-nonzero length quirk.
    if text.trim().parse::<f32>().is_ok_and(|n| n != 0.) {
        return None;
    }
    match Len::parse(text, units, viewport)? {
        value @ Len::Val(_) => Some(value),
        _ => None,
    }
}

/// A `<length>` that cannot be a percentage (z offsets, depths), in px.
fn absolute_length(text: &str, units: Units, viewport: Vp) -> Option<f32> {
    if text.contains('%') {
        return None;
    }
    length(text, units, viewport)?.resolve(None)
}

/// The z component of `transform-origin` (its third value), in px.
fn origin_depth(text: &str, units: Units, viewport: Vp) -> Option<f32> {
    let parts = components(text)?;
    absolute_length(parts.get(2)?, units, viewport)
}

fn percent(value: f32) -> Len {
    Len::Val(super::value::Node::Lin {
        k: value / 100.,
        b: 0.,
    })
}

fn number(text: &str) -> Option<f32> {
    crate::dom::css_transform_number(text, false, false)
}

fn scale(text: &str) -> Option<f32> {
    crate::dom::css_transform_number(text, false, true)
}

fn angle(text: &str) -> Option<f32> {
    crate::dom::css_transform_number(text, true, false).map(f32::to_radians)
}

fn rotate(radians: f32) -> Affine2d {
    let (sin, cos) = radians.sin_cos();
    Affine2d([cos, sin, -sin, cos, 0., 0.])
}

/// Top-level component slices, retaining nested CSS math as one value.
fn components(text: &str) -> Option<Vec<&str>> {
    let mut parser = cssparser::Parser::new(text);
    let mut parts = Vec::new();
    while !parser.is_exhausted() {
        parser.skip_whitespace();
        let start = parser.position();
        if matches!(parser.next().ok()?, cssparser::Token::Comma) {
            return None;
        }
        // Advancing the tokenizer skips an unread nested block in its entirety.
        parser.skip_whitespace();
        parts.push(parser.slice_from(start).trim());
    }
    Some(parts)
}

fn origin(text: &str, units: Units, viewport: Vp) -> Option<[Len; 2]> {
    let parts = components(text)?;
    if parts.len() > 3 {
        return None;
    }
    let (x, y) = match parts.as_slice() {
        ["top" | "bottom"] => ("center", parts[0]),
        [x] => (*x, "center"),
        [y @ ("top" | "bottom"), x, ..] => (*x, *y),
        [y, x @ ("left" | "right"), ..] => (*x, *y),
        [x, y, ..] => (*x, *y),
        _ => return None,
    };
    let axis = |text, horizontal| match text {
        "left" if horizontal => Some(percent(0.)),
        "right" if horizontal => Some(percent(100.)),
        "top" if !horizontal => Some(percent(0.)),
        "bottom" if !horizontal => Some(percent(100.)),
        "center" => Some(percent(50.)),
        _ => length(text, units, viewport),
    };
    Some([axis(x, true)?, axis(y, false)?])
}

/// CSS Transforms 2 #individual-transforms `rotate`: an angle about `z`,
/// a named axis, or an `x y z` vector.
fn individual_rotation(text: &str) -> Option<Operation> {
    let mut parts = components(text)?;
    // The <angle> is the component with an angle unit; a bare zero may
    // be one only when nothing else is (`0 0 1 45deg`).
    let index = parts
        .iter()
        .position(|s| angle(s).is_some() && number(s).is_none())
        .or_else(|| parts.iter().position(|s| angle(s).is_some()))?;
    let radians = angle(parts.remove(index))?;
    let axis = match parts.as_slice() {
        [] => [0., 0., 1.],
        [name] => match name.to_ascii_lowercase().as_str() {
            "x" => [1., 0., 0.],
            "y" => [0., 1., 0.],
            "z" => [0., 0., 1.],
            _ => return None,
        },
        [x, y, z] => [number(x)?, number(y)?, number(z)?],
        _ => return None,
    };
    Some(rotation_operation(axis, radians))
}

/// `rotate3d()` about `axis`: a 2D rotation when the axis is ±z.
fn rotation_operation([x, y, z]: [f32; 3], radians: f32) -> Operation {
    if x == 0. && y == 0. {
        Operation::Matrix(if z == 0. {
            Affine2d::IDENTITY
        } else {
            rotate(radians * z.signum())
        })
    } else {
        Operation::Matrix3d(m3::rotate(x, y, z, radians))
    }
}

fn functions(text: &str, units: Units, viewport: Vp) -> Option<Vec<Operation>> {
    let mut parser = cssparser::Parser::new(text);
    let mut operations = Vec::new();
    while !parser.is_exhausted() {
        let name = parser.expect_function().ok()?.to_ascii_lowercase();
        let args = parser
            .parse_nested_block(|p| {
                p.parse_comma_separated(|p| {
                    let start = p.position();
                    while p.next().is_ok() {}
                    Ok::<_, cssparser::ParseError<()>>(p.slice_from(start).trim())
                })
            })
            .ok()?;
        let matrix = match (name.as_str(), args.as_slice()) {
            ("translate", [x]) | ("translatex", [x]) => {
                operations.push(Operation::Translate([
                    length(x, units, viewport)?,
                    Len::px(0.),
                ]));
                continue;
            }
            ("translatey", [y]) => {
                operations.push(Operation::Translate([
                    Len::px(0.),
                    length(y, units, viewport)?,
                ]));
                continue;
            }
            ("translate", [x, y]) => {
                operations.push(Operation::Translate([
                    length(x, units, viewport)?,
                    length(y, units, viewport)?,
                ]));
                continue;
            }
            ("translate3d", [x, y, z]) => {
                operations.push(Operation::Translate([
                    length(x, units, viewport)?,
                    length(y, units, viewport)?,
                ]));
                let z = absolute_length(z, units, viewport)?;
                if z != 0. {
                    operations.push(Operation::Matrix3d(m3::translate(0., 0., z)));
                }
                continue;
            }
            ("translatez", [z]) => {
                operations.push(Operation::Matrix3d(m3::translate(
                    0.,
                    0.,
                    absolute_length(z, units, viewport)?,
                )));
                continue;
            }
            ("matrix", [a, b, c, d, e, f]) => Affine2d([
                number(a)?,
                number(b)?,
                number(c)?,
                number(d)?,
                number(e)?,
                number(f)?,
            ]),
            ("matrix3d", values) if values.len() == 16 => {
                let m = values
                    .iter()
                    .map(|v| number(v))
                    .collect::<Option<Vec<_>>>()?;
                operations.push(Operation::Matrix3d(std::array::from_fn(|i| m[i])));
                continue;
            }
            ("scale", [x]) => Affine2d::scale(scale(x)?, scale(x)?),
            ("scale", [x, y]) => Affine2d::scale(scale(x)?, scale(y)?),
            ("scale3d", [x, y, z]) => {
                operations.push(Operation::Matrix3d(m3::scale(
                    scale(x)?,
                    scale(y)?,
                    scale(z)?,
                )));
                continue;
            }
            ("scalex", [x]) => Affine2d::scale(scale(x)?, 1.),
            ("scaley", [y]) => Affine2d::scale(1., scale(y)?),
            ("scalez", [z]) => {
                operations.push(Operation::Matrix3d(m3::scale(1., 1., scale(z)?)));
                continue;
            }
            ("rotate" | "rotatez", [a]) => rotate(angle(a)?),
            ("rotatex", [a]) => {
                operations.push(Operation::Matrix3d(m3::rotate(1., 0., 0., angle(a)?)));
                continue;
            }
            ("rotatey", [a]) => {
                operations.push(Operation::Matrix3d(m3::rotate(0., 1., 0., angle(a)?)));
                continue;
            }
            ("rotate3d", [x, y, z, a]) => {
                operations.push(rotation_operation(
                    [number(x)?, number(y)?, number(z)?],
                    angle(a)?,
                ));
                continue;
            }
            ("perspective", [depth]) => {
                let depth = if depth.trim().eq_ignore_ascii_case("none") {
                    f32::INFINITY
                } else {
                    absolute_length(depth, units, viewport).filter(|d| *d >= 0.)?
                };
                operations.push(Operation::Matrix3d(m3::perspective(depth)));
                continue;
            }
            ("skewx", [a]) | ("skew", [a]) => Affine2d([1., 0., angle(a)?.tan(), 1., 0., 0.]),
            ("skewy", [a]) => Affine2d([1., angle(a)?.tan(), 0., 1., 0., 0.]),
            ("skew", [x, y]) => Affine2d([1., angle(y)?.tan(), angle(x)?.tan(), 1., 0., 0.]),
            _ => return None,
        };
        operations.push(Operation::Matrix(matrix));
    }
    (!operations.is_empty()).then_some(operations)
}

/// An animated `transform` value's functions as interpolable steps (CSS
/// Transforms 1 #interpolation-of-transforms): lengths resolved against the
/// `reference` box, angles in radians. `none` is the empty list.
pub(super) fn animation_steps(
    text: &str,
    units: Units,
    viewport: Vp,
    reference: CssRect,
) -> Option<Vec<TransformStep>> {
    if none(text) {
        return Some(Vec::new());
    }
    let mut parser = cssparser::Parser::new(text);
    let x_length = |text: &str| length(text, units, viewport)?.resolve(Some(reference.width));
    let y_length = |text: &str| length(text, units, viewport)?.resolve(Some(reference.height));
    let mut steps = Vec::new();
    while !parser.is_exhausted() {
        let name = parser.expect_function().ok()?.to_ascii_lowercase();
        let args = parser
            .parse_nested_block(|p| {
                p.parse_comma_separated(|p| {
                    let start = p.position();
                    while p.next().is_ok() {}
                    Ok::<_, cssparser::ParseError<()>>(p.slice_from(start).trim())
                })
            })
            .ok()?;
        steps.push(match (name.as_str(), args.as_slice()) {
            ("translate" | "translatex", [x]) => TransformStep::Translate(x_length(x)?, 0.),
            ("translatey", [y]) => TransformStep::Translate(0., y_length(y)?),
            ("translate", [x, y]) => TransformStep::Translate(x_length(x)?, y_length(y)?),
            ("translate3d", [x, y, z]) => TransformStep::Translate3d(
                x_length(x)?,
                y_length(y)?,
                absolute_length(z, units, viewport)?,
            ),
            ("translatez", [z]) => {
                TransformStep::Translate3d(0., 0., absolute_length(z, units, viewport)?)
            }
            ("scale", [x]) => TransformStep::Scale(scale(x)?, scale(x)?),
            ("scale", [x, y]) => TransformStep::Scale(scale(x)?, scale(y)?),
            ("scale3d", [x, y, z]) => TransformStep::Scale3d(scale(x)?, scale(y)?, scale(z)?),
            ("scalex", [x]) => TransformStep::Scale(scale(x)?, 1.),
            ("scaley", [y]) => TransformStep::Scale(1., scale(y)?),
            ("scalez", [z]) => TransformStep::Scale3d(1., 1., scale(z)?),
            ("rotate" | "rotatez", [a]) => TransformStep::Rotate(angle(a)?),
            ("rotatex", [a]) => TransformStep::Rotate3d([1., 0., 0.], angle(a)?),
            ("rotatey", [a]) => TransformStep::Rotate3d([0., 1., 0.], angle(a)?),
            ("rotate3d", [x, y, z, a]) => {
                let (x, y, z, a) = (number(x)?, number(y)?, number(z)?, angle(a)?);
                if x == 0. && y == 0. {
                    TransformStep::Rotate(a * if z == 0. { 0. } else { z.signum() })
                } else {
                    TransformStep::Rotate3d([x, y, z], a)
                }
            }
            ("skew" | "skewx", [a]) => TransformStep::Skew(angle(a)?, 0.),
            ("skewy", [a]) => TransformStep::Skew(0., angle(a)?),
            ("skew", [x, y]) => TransformStep::Skew(angle(x)?, angle(y)?),
            ("matrix", [a, b, c, d, e, f]) => TransformStep::Matrix(Affine2d([
                number(a)?,
                number(b)?,
                number(c)?,
                number(d)?,
                number(e)?,
                number(f)?,
            ])),
            ("matrix3d", values) if values.len() == 16 => {
                let m = values
                    .iter()
                    .map(|v| number(v))
                    .collect::<Option<Vec<_>>>()?;
                TransformStep::Matrix3d(std::array::from_fn(|i| m[i]))
            }
            ("perspective", [depth]) => {
                TransformStep::Perspective(if depth.trim().eq_ignore_ascii_case("none") {
                    f32::INFINITY
                } else {
                    absolute_length(depth, units, viewport).filter(|d| *d >= 0.)?
                })
            }
            _ => return None,
        });
    }
    Some(steps)
}

/// The reference box and absolute transform origin (CSS Transforms 1
/// #transform-box, #transform-origin-property) of a fragment whose
/// `transform` animates, with or without a static transform.
pub(super) fn animation_origin(
    value: impl Fn(&str) -> Option<String>,
    units: Units,
    viewport: Vp,
    fragment: &Frag,
) -> (CssRect, CssPoint) {
    let reference =
        if value("transform-box").is_some_and(|v| matches!(v.trim(), "content-box" | "fill-box")) {
            fragment.content_box()
        } else {
            CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h)
        };
    let [x, y] = value("transform-origin")
        .and_then(|s| origin(&s, units, viewport))
        .unwrap_or_else(|| [percent(50.), percent(50.)]);
    (
        reference,
        CssPoint::new(
            reference.x + x.resolve(Some(reference.width)).unwrap_or(0.),
            reference.y + y.resolve(Some(reference.height)).unwrap_or(0.),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment() -> Frag {
        let mut f = Frag::empty();
        f.x = 13.;
        f.y = 17.;
        f.w = 140.;
        f.h = 100.;
        f.content_offset = [20., 10.];
        f.content_size = Some([100., 80.]);
        f
    }

    fn transform(values: &[(&str, &str)]) -> Arc<Transform> {
        Transform::parse(
            |p| {
                values
                    .iter()
                    .find(|(name, _)| *name == p)
                    .map(|(_, v)| (*v).into())
            },
            Units::default(),
            Vp { w: 800., h: 600. },
        )
        .expect("valid transform")
    }

    #[test]
    fn composition_keeps_translation_in_its_matrix_order() {
        let f = fragment();
        for (text, x) in [
            ("translateX(10px) scale(2)", 23.),
            ("scale(2) translateX(10px)", 33.),
        ] {
            let t = transform(&[("transform", text), ("transform-origin", "0 0")]);
            let p = t.matrix(&f).map_point(CssPoint::new(f.x, f.y));
            assert_eq!((p.x, p.y), (x, 17.));
        }
    }

    #[test]
    fn individual_transforms_and_reference_box_use_the_same_snapshot() {
        let f = fragment();
        let t = transform(&[
            ("translate", "min(50%, 90px) calc(25% + 1px)"),
            ("scale", "200% 50%"),
            ("rotate", "z calc(50grad + 45deg)"),
            ("transform-origin", "top left"),
            ("transform-box", "content-box"),
        ]);
        let p = t.matrix(&f).map_point(CssPoint::new(43., 47.));
        assert!((p.x - 73.).abs() < 0.0001, "{p:?}");
        assert!((p.y - 68.).abs() < 0.0001, "{p:?}");
    }

    #[test]
    fn percentages_and_nonlinear_math_resolve_after_resize() {
        let mut f = fragment();
        let t = transform(&[("transform", "translate(min(50%, 100px), calc(20% + 3px))")]);
        assert_eq!(
            t.matrix(&f).map_point(CssPoint::default()),
            CssPoint::new(70., 23.)
        );
        f.w = 300.;
        f.h = 200.;
        assert_eq!(
            t.matrix(&f).map_point(CssPoint::default()),
            CssPoint::new(100., 43.)
        );
    }

    /// CSS Transforms 2 #ctm with `transform-style: flat`: 3D functions
    /// compose as 4x4 matrices and the box flattens into its parent's plane.
    #[test]
    fn three_dimensional_functions_flatten_orthographically() {
        let f = fragment();
        let map = |values: &[(&str, &str)], x: f32, y: f32| {
            let mut values = values.to_vec();
            values.push(("transform-origin", "0 0"));
            transform(&values)
                .matrix(&f)
                .map_point(CssPoint::new(f.x + x, f.y + y))
        };
        let near = |p: CssPoint, x: f32, y: f32| {
            assert!(
                (p.x - (f.x + x)).abs() < 1e-3 && (p.y - (f.y + y)).abs() < 1e-3,
                "{p:?} vs ({x}, {y})"
            );
        };
        near(map(&[("transform", "rotateY(60deg)")], 100., 10.), 50., 10.);
        near(map(&[("transform", "rotateX(60deg)")], 10., 100.), 10., 50.);
        near(
            map(&[("transform", "rotate3d(0, 1, 0, 60deg)")], 100., 0.),
            50.,
            0.,
        );
        near(map(&[("rotate", "y 60deg")], 100., 0.), 50., 0.);
        near(map(&[("rotate", "0 1 0 60deg")], 100., 0.), 50., 0.);
        // Two quarter turns about x compose before flattening.
        near(
            map(&[("transform", "rotateX(90deg) rotateX(-90deg)")], 10., 20.),
            10.,
            20.,
        );
        // A z translation or z scale does not move a flat box.
        near(
            map(&[("transform", "translateZ(40px) scaleZ(3)")], 10., 20.),
            10.,
            20.,
        );
        near(map(&[("translate", "5px 6px 70px")], 0., 0.), 5., 6.);
        near(
            map(
                &[("transform", "matrix3d(2,0,0,0, 0,1,0,0, 0,0,1,0, 7,8,9,1)")],
                10.,
                10.,
            ),
            27.,
            18.,
        );
        // The origin's depth: a quarter turn about y about a point 20px
        // behind the box moves its origin 20px right.
        let t = transform(&[
            ("transform", "rotateY(90deg)"),
            ("transform-origin", "0 0 -20px"),
        ]);
        near(t.matrix(&f).map_point(CssPoint::new(f.x, f.y)), 20., 0.);
    }

    #[test]
    fn three_dimensional_animation_steps_interpolate_their_angles() {
        let steps = |text: &str| {
            animation_steps(
                text,
                Units::default(),
                Vp { w: 800., h: 600. },
                CssRect::default(),
            )
            .unwrap()
        };
        // rotateY(0) → rotateY(180deg) passes through 60deg at one third.
        let m = crate::render::interpolate_transform_steps_for_test(
            &steps("rotateY(0deg)"),
            &steps("rotateY(180deg)"),
            1. / 3.,
        );
        assert!(
            (m.0[0] - 0.5).abs() < 1e-4 && (m.0[3] - 1.).abs() < 1e-4,
            "{m:?}"
        );
        // A 2D and a 3D translate share the translate3d primitive.
        let m = crate::render::interpolate_transform_steps_for_test(
            &steps("translateX(10px)"),
            &steps("translateZ(10px) "),
            0.5,
        );
        assert!((m.0[4] - 5.).abs() < 1e-4, "{m:?}");
    }

    #[test]
    fn identity_is_not_none_and_origin_follows_relocation() {
        let mut f = fragment();
        let t = transform(&[("transform", "rotate(0deg)")]);
        assert!(t.matrix(&f).is_identity());
        let t = transform(&[
            ("transform", "scale(2)"),
            ("transform-origin", "bottom right"),
        ]);
        let first = bounds(t.matrix(&f), CssRect::new(f.x, f.y, f.w, f.h));
        f.x += 100.;
        f.y -= 20.;
        let next = bounds(t.matrix(&f), CssRect::new(f.x, f.y, f.w, f.h));
        assert_eq!(
            next,
            CssRect::new(first.x + 100., first.y - 20., first.width, first.height)
        );
    }
}
