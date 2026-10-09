//! CSS Transforms 1 #interpolation-of-transforms and Transforms 2
//! #interpolation-of-transform-functions / #matrix-interpolation (CSSWG
//! snapshot 81c27f686901). Preserve the function list and percentage basis;
//! `none` and the shorter list pad with the identity functions matching the
//! longer list. Pairs sharing a primitive interpolate numerically (rotations
//! keep their authored turns); from the first pair that does not, the rest of
//! each list composes into a 4x4 matrix and interpolates by decomposition.
//!
//! A remainder holding a percentage translation cannot be composed until
//! layout knows the reference box (CSS Transforms 1 #transform-box): its
//! interpolation is deferred. The computed value keeps both remainders and
//! the progress (`-trust-interpolate-matrix()`, an internal function author
//! style sheets cannot declare), and `layout2::transform` composes,
//! decomposes and interpolates them against the box, as Gecko's
//! `interpolatematrix()` does.
use std::sync::Arc;

use super::{Len, Length, Linear, Vp};
use crate::render::matrix3d::{self, Matrix3d};

#[derive(Clone, Debug, PartialEq)]
pub(in crate::dom) struct Transform(Vec<Primitive>);

/// A matrix interpolation deferred until the reference box is known.
#[derive(Clone, Debug, PartialEq)]
struct Deferred {
    from: Vec<Primitive>,
    to: Vec<Primitive>,
    progress: f32,
}

#[derive(Clone, Debug, PartialEq)]
enum Primitive {
    Translate(Linear, Linear),
    /// `translate3d()`; the z component is an absolute length in px.
    Translate3d(Linear, Linear, f32),
    Scale(f32, f32),
    Scale3d(f32, f32, f32),
    /// Degrees.
    Rotate(f32),
    /// `rotate3d()` about an axis, degrees.
    Rotate3d([f32; 3], f32),
    Skew(f32, f32),
    /// `perspective()`: its depth in px, infinite for `none`.
    Perspective(f32),
    /// `matrix()`/`matrix3d()`, and interpolated matrix remainders.
    Matrix(Matrix3d),
    /// An interpolated matrix remainder whose composition needs the
    /// reference box.
    Deferred(Arc<Deferred>),
}

/// Transforms 2 #transform-primitives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Translate,
    Scale,
    Rotate,
    Skew,
    Matrix,
}

impl Primitive {
    fn family(&self) -> Family {
        match self {
            Self::Translate(..) | Self::Translate3d(..) => Family::Translate,
            Self::Scale(..) | Self::Scale3d(..) => Family::Scale,
            Self::Rotate(_) | Self::Rotate3d(..) => Family::Rotate,
            Self::Skew(..) => Family::Skew,
            // `matrix()`, `matrix3d()` and `perspective()` interpolate as
            // matrices.
            Self::Perspective(_) | Self::Matrix(_) | Self::Deferred(_) => Family::Matrix,
        }
    }

    fn identity(&self) -> Self {
        let zero = Linear { px: 0., pct: 0. };
        match self {
            Self::Translate(..) => Self::Translate(zero, zero),
            Self::Translate3d(..) => Self::Translate3d(zero, zero, 0.),
            Self::Scale(..) => Self::Scale(1., 1.),
            Self::Scale3d(..) => Self::Scale3d(1., 1., 1.),
            Self::Rotate(_) => Self::Rotate(0.),
            Self::Rotate3d(axis, _) => Self::Rotate3d(*axis, 0.),
            Self::Skew(..) => Self::Skew(0., 0.),
            Self::Perspective(_) => Self::Perspective(f32::INFINITY),
            Self::Matrix(_) | Self::Deferred(_) => Self::Matrix(matrix3d::IDENTITY),
        }
    }

    /// The 4x4 matrix, when no percentage needs a reference box.
    fn matrix(&self) -> Option<Matrix3d> {
        let absolute = |l: Linear| (l.pct == 0.).then_some(l.px);
        Some(match self {
            Self::Translate(x, y) => matrix3d::translate(absolute(*x)?, absolute(*y)?, 0.),
            Self::Translate3d(x, y, z) => matrix3d::translate(absolute(*x)?, absolute(*y)?, *z),
            Self::Scale(x, y) => matrix3d::scale(*x, *y, 1.),
            Self::Scale3d(x, y, z) => matrix3d::scale(*x, *y, *z),
            Self::Rotate(a) => matrix3d::rotate(0., 0., 1., a.to_radians()),
            Self::Rotate3d([x, y, z], a) => matrix3d::rotate(*x, *y, *z, a.to_radians()),
            Self::Skew(x, y) => {
                let mut m = matrix3d::IDENTITY;
                m[4] = x.to_radians().tan();
                m[1] = y.to_radians().tan();
                m
            }
            Self::Perspective(depth) => matrix3d::perspective(*depth),
            Self::Matrix(m) => *m,
            Self::Deferred(_) => return None,
        })
    }

    /// Interpolate a pair sharing a primitive (Transforms 2
    /// #interpolation-of-transform-functions); `None` for other pairs and
    /// for matrices that cannot be composed or decomposed.
    fn mix(&self, end: &Self, t: f32) -> Option<Self> {
        if self.family() != end.family() {
            return None;
        }
        let mix = |a: f32, b: f32| a + (b - a) * t;
        Some(match (self, end) {
            (Self::Translate(x, y), Self::Translate(u, v)) => {
                Self::Translate(x.mix(*u, t), y.mix(*v, t))
            }
            (Self::Scale(x, y), Self::Scale(u, v)) => Self::Scale(mix(*x, *u), mix(*y, *v)),
            (Self::Rotate(x), Self::Rotate(y)) => Self::Rotate(mix(*x, *y)),
            (Self::Skew(x, y), Self::Skew(u, v)) => Self::Skew(mix(*x, *u), mix(*y, *v)),
            // A 2D and a 3D function share the 3D primitive.
            _ if self.family() == Family::Translate => {
                let (x0, y0, z0) = self.translation3d();
                let (x1, y1, z1) = end.translation3d();
                Self::Translate3d(x0.mix(x1, t), y0.mix(y1, t), mix(z0, z1))
            }
            _ if self.family() == Family::Scale => {
                let [x0, y0, z0] = self.scale3d();
                let [x1, y1, z1] = end.scale3d();
                Self::Scale3d(mix(x0, x1), mix(y0, y1), mix(z0, z1))
            }
            _ if self.family() == Family::Rotate => {
                let (axis0, a0) = self.rotation3d();
                let (axis1, a1) = end.rotation3d();
                let normalize = |[x, y, z]: [f32; 3]| {
                    let length = (x * x + y * y + z * z).sqrt();
                    if length == 0. {
                        [0., 0., 1.]
                    } else {
                        [x / length, y / length, z / length]
                    }
                };
                let (n0, n1) = (normalize(axis0), normalize(axis1));
                let same = n0.iter().zip(n1).all(|(a, b)| (a - b).abs() < 1e-6);
                if !same && a0 != 0. && a1 != 0. {
                    return Some(Self::Matrix(matrix3d::interpolate(
                        &self.matrix()?,
                        &end.matrix()?,
                        t,
                    )?));
                }
                let axis = if a0 != 0. {
                    n0
                } else if a1 != 0. {
                    n1
                } else {
                    [0., 0., 1.]
                };
                Self::Rotate3d(axis, mix(a0, a1))
            }
            _ => Self::Matrix(interpolate_matrices(&self.matrix()?, &end.matrix()?, t)?),
        })
    }

    fn translation3d(&self) -> (Linear, Linear, f32) {
        match *self {
            Self::Translate(x, y) => (x, y, 0.),
            Self::Translate3d(x, y, z) => (x, y, z),
            _ => unreachable!("translate primitive"),
        }
    }

    fn scale3d(&self) -> [f32; 3] {
        match *self {
            Self::Scale(x, y) => [x, y, 1.],
            Self::Scale3d(x, y, z) => [x, y, z],
            _ => unreachable!("scale primitive"),
        }
    }

    fn rotation3d(&self) -> ([f32; 3], f32) {
        match *self {
            Self::Rotate(a) => ([0., 0., 1.], a),
            Self::Rotate3d(axis, a) => (axis, a),
            _ => unreachable!("rotate primitive"),
        }
    }

    fn css(&self) -> String {
        let number = |v: f32| {
            let text = format!("{v:.6}");
            let text = text.trim_end_matches('0').trim_end_matches('.');
            if text == "-0" {
                "0".to_string()
            } else {
                text.to_string()
            }
        };
        match *self {
            Self::Translate(x, y) => format!("translate({}, {})", x.css(0), y.css(0)),
            Self::Translate3d(x, y, z) => {
                format!("translate3d({}, {}, {}px)", x.css(0), y.css(0), number(z))
            }
            Self::Scale(x, y) => format!("scale({x}, {y})"),
            Self::Scale3d(x, y, z) => format!("scale3d({x}, {y}, {z})"),
            Self::Rotate(a) => format!("rotate({a}deg)"),
            Self::Rotate3d([x, y, z], a) => format!("rotate3d({x}, {y}, {z}, {a}deg)"),
            Self::Skew(x, y) => format!("skew({x}deg, {y}deg)"),
            Self::Perspective(depth) if depth.is_infinite() => "perspective(none)".into(),
            Self::Perspective(depth) => format!("perspective({}px)", number(depth)),
            Self::Matrix(m) if matrix3d::is_2d(&m) => format!(
                "matrix({}, {}, {}, {}, {}, {})",
                number(m[0]),
                number(m[1]),
                number(m[4]),
                number(m[5]),
                number(m[12]),
                number(m[13])
            ),
            Self::Matrix(m) => format!(
                "matrix3d({})",
                m.iter().map(|v| number(*v)).collect::<Vec<_>>().join(", ")
            ),
            // `layout2::transform` resolves the remainders against the
            // reference box; an empty remainder is `none`.
            Self::Deferred(ref deferred) => {
                let list = |list: &[Primitive]| {
                    if list.is_empty() {
                        "none".to_owned()
                    } else {
                        list.iter()
                            .map(Primitive::css)
                            .collect::<Vec<_>>()
                            .join(" ")
                    }
                };
                format!(
                    "{}({}, {}, {})",
                    crate::layout2::DEFERRED_TRANSFORM_INTERPOLATION,
                    number(deferred.progress),
                    list(&deferred.from),
                    list(&deferred.to)
                )
            }
        }
    }
}

/// Transforms 2 #matrix-interpolation; two 2D matrices decompose in two
/// dimensions (Transforms 1 #decomposing-a-2d-matrix).
fn interpolate_matrices(a: &Matrix3d, b: &Matrix3d, t: f32) -> Option<Matrix3d> {
    if matrix3d::is_2d(a) && matrix3d::is_2d(b) {
        let flat = |m: &Matrix3d| crate::render::Affine2d([m[0], m[1], m[4], m[5], m[12], m[13]]);
        return Some(matrix3d::from_affine(crate::render::interpolate_matrices(
            flat(a),
            flat(b),
            t,
        )));
    }
    matrix3d::interpolate(a, b, t)
}

impl Transform {
    /// Parse a computed `transform` with the target's font-relative
    /// `units`.
    pub(in crate::dom) fn parse(value: &str, units: crate::layout2::Units, vp: Vp) -> Option<Self> {
        if value.eq_ignore_ascii_case("none") {
            return Some(Self(Vec::new()));
        }
        let mut result = Vec::new();
        let mut rest = value.trim();
        let length = |v: &str| match Len::parse(v, units, vp)? {
            Len::Val(Length::Lin { k, b }) => Some(Linear { px: b, pct: k }),
            _ => None,
        };
        // A z offset or depth: a length that cannot be a percentage.
        let absolute = |v: &str| {
            length(v)
                .filter(|l| l.pct == 0. && !v.contains('%'))
                .map(|l| l.px)
        };
        let number = |v: &str| v.trim().parse::<f32>().ok().filter(|v| v.is_finite());
        let angle = |v: &str| {
            let v = v.trim();
            for (unit, factor) in [
                ("deg", 1.),
                ("grad", 0.9),
                ("rad", 180. / std::f32::consts::PI),
                ("turn", 360.),
            ] {
                if let Some(v) = v.strip_suffix(unit) {
                    return number(v).map(|v| v * factor);
                }
            }
            number(v).filter(|v| *v == 0.)
        };
        while !rest.is_empty() {
            // Bound retained transition state even for hostile declarations.
            if result.len() >= 128 {
                return None;
            }
            let open = rest.find('(')?;
            let name = rest[..open].trim().to_ascii_lowercase();
            let mut depth = 0;
            let close = rest[open..].char_indices().find_map(|(i, c)| {
                if c == '(' {
                    depth += 1;
                }
                if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(open + i);
                    }
                }
                None
            })?;
            let args = crate::layout2::value::split_args(&rest[open + 1..close]);
            let first = *args.first()?;
            let second = args.get(1).copied();
            let zero = Linear { px: 0., pct: 0. };
            let primitive = match name.as_str() {
                "translate" if args.len() <= 2 => {
                    Primitive::Translate(length(first)?, length(second.unwrap_or("0"))?)
                }
                "translatex" if args.len() == 1 => Primitive::Translate(length(first)?, zero),
                "translatey" if args.len() == 1 => Primitive::Translate(zero, length(first)?),
                "translatez" if args.len() == 1 => {
                    Primitive::Translate3d(zero, zero, absolute(first)?)
                }
                "translate3d" if args.len() == 3 => {
                    Primitive::Translate3d(length(first)?, length(second?)?, absolute(args[2])?)
                }
                "scale" if args.len() <= 2 => {
                    Primitive::Scale(number(first)?, number(second.unwrap_or(first))?)
                }
                "scalex" if args.len() == 1 => Primitive::Scale(number(first)?, 1.),
                "scaley" if args.len() == 1 => Primitive::Scale(1., number(first)?),
                "scalez" if args.len() == 1 => Primitive::Scale3d(1., 1., number(first)?),
                "scale3d" if args.len() == 3 => {
                    Primitive::Scale3d(number(first)?, number(second?)?, number(args[2])?)
                }
                "rotate" | "rotatez" if args.len() == 1 => Primitive::Rotate(angle(first)?),
                "rotatex" if args.len() == 1 => Primitive::Rotate3d([1., 0., 0.], angle(first)?),
                "rotatey" if args.len() == 1 => Primitive::Rotate3d([0., 1., 0.], angle(first)?),
                "rotate3d" if args.len() == 4 => Primitive::Rotate3d(
                    [number(first)?, number(second?)?, number(args[2])?],
                    angle(args[3])?,
                ),
                "skew" if args.len() <= 2 => {
                    Primitive::Skew(angle(first)?, angle(second.unwrap_or("0"))?)
                }
                "skewx" if args.len() == 1 => Primitive::Skew(angle(first)?, 0.),
                "skewy" if args.len() == 1 => Primitive::Skew(0., angle(first)?),
                "perspective" if args.len() == 1 => {
                    Primitive::Perspective(if first.trim().eq_ignore_ascii_case("none") {
                        f32::INFINITY
                    } else {
                        absolute(first).filter(|depth| *depth >= 0.)?
                    })
                }
                "matrix" if args.len() == 6 => {
                    let v = args.iter().map(|a| number(a)).collect::<Option<Vec<_>>>()?;
                    let mut m = matrix3d::IDENTITY;
                    [m[0], m[1], m[4], m[5], m[12], m[13]] = [v[0], v[1], v[2], v[3], v[4], v[5]];
                    Primitive::Matrix(m)
                }
                "matrix3d" if args.len() == 16 => {
                    let v = args.iter().map(|a| number(a)).collect::<Option<Vec<_>>>()?;
                    Primitive::Matrix(std::array::from_fn(|i| v[i]))
                }
                _ => return None,
            };
            result.push(primitive);
            rest = rest[close + 1..].trim_start();
        }
        Some(Self(result))
    }

    /// CSS Transforms 1 #interpolation-of-transforms at `t`. `None` when
    /// a remainder needs matrix interpolation but cannot be composed (a
    /// percentage translation, whose reference box only layout knows) or
    /// decomposed (a singular matrix).
    pub(in crate::dom) fn interpolate(&self, end: &Self, t: f32) -> Option<Self> {
        let length = self.0.len().max(end.0.len());
        let pad = |list: &[Primitive], other: &[Primitive]| -> Vec<Primitive> {
            (0..length)
                .map(|i| list.get(i).cloned().unwrap_or_else(|| other[i].identity()))
                .collect()
        };
        let (a, b) = (pad(&self.0, &end.0), pad(&end.0, &self.0));
        let mut result = Vec::with_capacity(length);
        for index in 0..length {
            if let Some(primitive) = a[index].mix(&b[index], t) {
                result.push(primitive);
                continue;
            }
            let compose = |list: &[Primitive]| {
                list.iter()
                    .try_fold(matrix3d::IDENTITY, |matrix, primitive| {
                        Some(matrix3d::multiply(&matrix, &primitive.matrix()?))
                    })
            };
            match (compose(&a[index..]), compose(&b[index..])) {
                (Some(from), Some(to)) => {
                    result.push(Primitive::Matrix(interpolate_matrices(&from, &to, t)?));
                }
                // A percentage translation (or an earlier deferred
                // remainder) needs the reference box: defer to layout.
                _ => result.push(Primitive::Deferred(Arc::new(Deferred {
                    from: a[index..].to_vec(),
                    to: b[index..].to_vec(),
                    progress: t,
                }))),
            }
            break;
        }
        Some(Self(result))
    }

    /// CSS Transitions 1 #transitionable. A deferred remainder is
    /// transitionable; a singular matrix there animates discretely when
    /// layout resolves it.
    pub(in crate::dom) fn compatible(&self, other: &Self) -> bool {
        self.interpolate(other, 0.5).is_some()
    }

    pub(in crate::dom) fn mix(&self, end: &Self, t: f32) -> Self {
        self.interpolate(end, t)
            .unwrap_or_else(|| if t < 0.5 { self.clone() } else { end.clone() })
    }

    pub(in crate::dom) fn css(&self) -> String {
        if self.0.is_empty() {
            return "none".into();
        }
        self.0.iter().map(|p| p.css()).collect::<Vec<_>>().join(" ")
    }

    pub(in crate::dom) fn retained_bytes(&self) -> usize {
        fn list(primitives: &[Primitive]) -> usize {
            primitives
                .iter()
                .map(|primitive| {
                    std::mem::size_of::<Primitive>()
                        + match primitive {
                            Primitive::Deferred(deferred) => {
                                std::mem::size_of::<Deferred>()
                                    + list(&deferred.from)
                                    + list(&deferred.to)
                            }
                            _ => 0,
                        }
                })
                .sum()
        }
        list(&self.0) + (self.0.capacity() - self.0.len()) * std::mem::size_of::<Primitive>()
    }
}
