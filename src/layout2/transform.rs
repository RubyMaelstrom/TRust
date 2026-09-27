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
use crate::render::{Affine2d, CssRect};

use super::Units;
use super::flow::Frag;
use super::value::{Len, Vp};

#[derive(Clone, Debug, PartialEq)]
enum Operation {
    Translate([Len; 2]),
    Matrix(Affine2d),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Transform {
    operations: Vec<Operation>,
    origin: [Len; 2],
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
        {
            operations.push(Operation::Translate([x, y]));
            present = true;
        }
        if let Some(text) = value("rotate").filter(|v| !none(v))
            && let Some(rotation) = individual_rotation(&text)
        {
            operations.push(Operation::Matrix(rotation));
            present = true;
        }
        if let Some(text) = value("scale").filter(|v| !none(v))
            && let Some(parts) = components(&text)
            && (1..=3).contains(&parts.len())
            && let Some(x) = scale(parts[0])
            && let Some(y) = parts.get(1).map_or(Some(x), |s| scale(s))
        {
            operations.push(Operation::Matrix(Affine2d::scale(x, y)));
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
        let origin = value("transform-origin")
            .and_then(|s| origin(&s, units, viewport))
            .unwrap_or_else(|| [percent(50.), percent(50.)]);
        Some(Arc::new(Self {
            operations,
            origin,
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
        let mut matrix = Affine2d::translate(origin.x, origin.y);
        for op in &self.operations {
            matrix = matrix.then(match op {
                Operation::Translate([x, y]) => Affine2d::translate(
                    x.resolve(Some(reference.width)).unwrap_or(0.),
                    y.resolve(Some(reference.height)).unwrap_or(0.),
                ),
                Operation::Matrix(matrix) => *matrix,
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
                    Operation::Matrix(_) => 0,
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
    let mut input = cssparser::ParserInput::new(text);
    let mut parser = cssparser::Parser::new(&mut input);
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

fn individual_rotation(text: &str) -> Option<Affine2d> {
    let mut parts = components(text)?;
    let index = parts.iter().position(|s| angle(s).is_some())?;
    let radians = angle(parts.remove(index))?;
    match parts.as_slice() {
        [] | ["z"] => Some(rotate(radians)),
        [x, y, z] if number(x)? == 0. && number(y)? == 0. => {
            let z = number(z)?;
            Some(if z == 0. {
                Affine2d::IDENTITY
            } else {
                rotate(radians * z.signum())
            })
        }
        // Non-planar transforms still require the renderer's 3D rendering
        // context contract; do not misrepresent them as a 2D rotation.
        _ => None,
    }
}

fn functions(text: &str, units: Units, viewport: Vp) -> Option<Vec<Operation>> {
    let mut input = cssparser::ParserInput::new(text);
    let mut parser = cssparser::Parser::new(&mut input);
    let mut operations = Vec::new();
    while !parser.is_exhausted() {
        let name = parser.expect_function().ok()?.to_ascii_lowercase();
        let args = parser
            .parse_nested_block(|p| {
                p.parse_comma_separated(|p| {
                    let start = p.position();
                    while p.next().is_ok() {}
                    Ok::<_, cssparser::ParseError<'_, ()>>(p.slice_from(start).trim())
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
            ("translate", [x, y]) | ("translate3d", [x, y, _]) => {
                operations.push(Operation::Translate([
                    length(x, units, viewport)?,
                    length(y, units, viewport)?,
                ]));
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
                Affine2d([m[0], m[1], m[4], m[5], m[12], m[13]])
            }
            ("scale", [x]) => Affine2d::scale(scale(x)?, scale(x)?),
            ("scale", [x, y]) | ("scale3d", [x, y, _]) => Affine2d::scale(scale(x)?, scale(y)?),
            ("scalex", [x]) => Affine2d::scale(scale(x)?, 1.),
            ("scaley", [y]) => Affine2d::scale(1., scale(y)?),
            ("rotate" | "rotatez", [a]) => rotate(angle(a)?),
            ("rotate3d", [x, y, z, a]) if number(x)? == 0. && number(y)? == 0. => {
                let z = number(z)?;
                if z == 0. {
                    Affine2d::IDENTITY
                } else {
                    rotate(angle(a)? * z.signum())
                }
            }
            ("skewx", [a]) | ("skew", [a]) => Affine2d([1., 0., angle(a)?.tan(), 1., 0., 0.]),
            ("skewy", [a]) => Affine2d([1., angle(a)?.tan(), 0., 1., 0., 0.]),
            ("skew", [x, y]) => Affine2d([1., angle(y)?.tan(), angle(x)?.tan(), 1., 0., 0.]),
            // Preserve the existing 2D adapter's treatment of genuinely 3D
            // functions pending its separate projective/flattening contract.
            ("translatez" | "scalez" | "rotatex" | "rotatey" | "perspective", [_])
            | ("rotate3d", [_, _, _, _]) => Affine2d::IDENTITY,
            _ => return None,
        };
        operations.push(Operation::Matrix(matrix));
    }
    (!operations.is_empty()).then_some(operations)
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
