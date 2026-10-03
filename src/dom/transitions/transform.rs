//! CSS Transforms 1 #interpolation-of-transforms and
//! #interpolation-of-transform-functions (CSSWG snapshot 81c27f686901).
//! Preserve the function list and percentage basis; `none` pads with matching
//! identities. Matching primitives interpolate numerically, so rotations keep
//! their authored turns. Non-matching lists and 3D projection remain unsupported.
use super::{Len, Length, Linear, NodeId, Vp};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Transform(Vec<Primitive>);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Primitive {
    Translate(Linear, Linear),
    Scale(f32, f32),
    Rotate(f32),
    Skew(f32, f32),
}
impl Primitive {
    fn identity(self) -> Self {
        match self {
            Self::Translate(..) => {
                Self::Translate(Linear { px: 0., pct: 0. }, Linear { px: 0., pct: 0. })
            }
            Self::Scale(..) => Self::Scale(1., 1.),
            Self::Rotate(_) => Self::Rotate(0.),
            Self::Skew(..) => Self::Skew(0., 0.),
        }
    }
    fn mix(self, end: Self, t: f32) -> Self {
        let mix = |a: f32, b: f32| a + (b - a) * t;
        match (self, end) {
            (Self::Translate(x, y), Self::Translate(u, v)) => {
                Self::Translate(x.mix(u, t), y.mix(v, t))
            }
            (Self::Scale(x, y), Self::Scale(u, v)) => Self::Scale(mix(x, u), mix(y, v)),
            (Self::Rotate(x), Self::Rotate(y)) => Self::Rotate(mix(x, y)),
            (Self::Skew(x, y), Self::Skew(u, v)) => Self::Skew(mix(x, u), mix(y, v)),
            _ => unreachable!("matching transform primitives"),
        }
    }
}
impl Transform {
    pub(super) fn parse<D: crate::layout2::UnitSource + ?Sized>(
        value: &str,
        dom: &D,
        id: NodeId,
        vp: Vp,
    ) -> Option<Self> {
        if value.eq_ignore_ascii_case("none") {
            return Some(Self(Vec::new()));
        }
        let mut result = Vec::new();
        let mut rest = value.trim();
        let length = |v: &str| match Len::parse(v, crate::layout2::Units::of(dom, id), vp)? {
            Len::Val(Length::Lin { k, b }) => Some(Linear { px: b, pct: k }),
            _ => None,
        };
        let number = |v: &str| v.parse::<f32>().ok().filter(|v| v.is_finite());
        let angle = |v: &str| {
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
                "translate3d" if args.len() == 3 && length(args[2])? == zero => {
                    Primitive::Translate(length(first)?, length(second?)?)
                }
                "scale" if args.len() <= 2 => {
                    Primitive::Scale(number(first)?, number(second.unwrap_or(first))?)
                }
                "scalex" if args.len() == 1 => Primitive::Scale(number(first)?, 1.),
                "scaley" if args.len() == 1 => Primitive::Scale(1., number(first)?),
                "scale3d" if args.len() == 3 && number(args[2])? == 1. => {
                    Primitive::Scale(number(first)?, number(second?)?)
                }
                "rotate" | "rotatez" if args.len() == 1 => Primitive::Rotate(angle(first)?),
                "skew" if args.len() <= 2 => {
                    Primitive::Skew(angle(first)?, angle(second.unwrap_or("0"))?)
                }
                "skewx" if args.len() == 1 => Primitive::Skew(angle(first)?, 0.),
                "skewy" if args.len() == 1 => Primitive::Skew(0., angle(first)?),
                _ => return None,
            };
            result.push(primitive);
            rest = rest[close + 1..].trim_start();
        }
        Some(Self(result))
    }
    pub(super) fn compatible(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(&other.0)
            .all(|(a, b)| std::mem::discriminant(a) == std::mem::discriminant(b))
    }
    pub(super) fn mix(&self, end: &Self, t: f32) -> Self {
        Self(
            (0..self.0.len().max(end.0.len()))
                .map(|i| {
                    let a = self
                        .0
                        .get(i)
                        .copied()
                        .unwrap_or_else(|| end.0[i].identity());
                    let b = end.0.get(i).copied().unwrap_or_else(|| a.identity());
                    a.mix(b, t)
                })
                .collect(),
        )
    }
    pub(super) fn css(&self) -> String {
        if self.0.is_empty() {
            return "none".into();
        }
        self.0
            .iter()
            .map(|p| match p {
                Primitive::Translate(x, y) => format!("translate({}, {})", x.css(0), y.css(0)),
                Primitive::Scale(x, y) => format!("scale({x}, {y})"),
                Primitive::Rotate(a) => format!("rotate({a}deg)"),
                Primitive::Skew(x, y) => format!("skew({x}deg, {y}deg)"),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
    pub(super) fn retained_bytes(&self) -> usize {
        self.0.capacity() * std::mem::size_of::<Primitive>()
    }
}
