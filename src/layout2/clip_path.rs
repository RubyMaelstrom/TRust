//! CSS Masking 1 §5 and CSS Shapes 1 §3: basic-shape clips in CSS pixels.
//! Local reference: csswg-drafts snapshot 81c27f686901 (2026-09-06).
//! Shape construction is shared by graphical paint, hit testing and terminal
//! clipping bounds; referenced SVG clip sources require the SVG resource tree.
use super::Units;
use super::value::{Len, Vp};
use crate::render::{CornerRadii, CssRect, PaintShape};

/// CSS Masking 1 #the-clip-path and CSS Shapes 1 #supported-basic-shapes.
/// Published Shapes 1 §3.1 also defines percentage circle radii against the
/// normalized diagonal (2014 CR); retain that interoperable value grammar.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ClipPath {
    reference: ReferenceBox,
    basic: BasicShape,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum ReferenceBox {
    Border,
    Padding,
    Content,
}
#[derive(Clone, Debug, PartialEq)]
enum BasicShape {
    Inset(Box<Inset>),
    Ellipse {
        circle: bool,
        radii: [Radius; 2],
        position: [Len; 2],
    },
    Polygon {
        points: Vec<[Len; 2]>,
        evenodd: bool,
    },
}
#[derive(Clone, Debug, PartialEq)]
enum Radius {
    Length(Len),
    Closest,
    Farthest,
}

impl ClipPath {
    pub(crate) fn parse(value: &str, units: Units, vp: Vp) -> Option<Self> {
        let lower = value.trim().to_ascii_lowercase();
        let mut parts = crate::dom::split_top_level_ws(&lower);
        let reference = |name: &str| match name {
            "border-box" | "stroke-box" | "view-box" => Some(ReferenceBox::Border),
            "padding-box" => Some(ReferenceBox::Padding),
            "content-box" | "fill-box" => Some(ReferenceBox::Content),
            _ => None,
        };
        let mut box_kind = ReferenceBox::Border;
        if let Some(kind) = parts.first().and_then(|v| reference(v)) {
            box_kind = kind;
            parts.remove(0);
        } else if let Some(kind) = parts.last().and_then(|v| reference(v)) {
            box_kind = kind;
            parts.pop();
        }
        if parts.len() != 1 {
            return None;
        }
        let text = parts[0];
        let open = text.find('(')?;
        let kind = &text[..open];
        let inner = text[open + 1..].strip_suffix(')')?;
        if !crate::dom::cssom::valid_value(text) {
            return None;
        }
        let basic = match kind {
            "inset" => BasicShape::Inset(Box::new(Inset::parse(text, units, vp)?)),
            "circle" | "ellipse" => {
                let tokens = crate::dom::split_top_level_ws(inner);
                let at = tokens.iter().position(|v| *v == "at");
                let radii = &tokens[..at.unwrap_or(tokens.len())];
                let radius = |text| match text {
                    "closest-side" => Some(Radius::Closest),
                    "farthest-side" => Some(Radius::Farthest),
                    _ => shape_length(text, units, vp, true).map(Radius::Length),
                };
                let circle = kind == "circle";
                let radii = match radii {
                    [] => [Radius::Closest, Radius::Closest],
                    [one] if circle => {
                        let r = radius(one)?;
                        [r.clone(), r]
                    }
                    [x, y] if !circle => [radius(x)?, radius(y)?],
                    _ => return None,
                };
                let position = if let Some(at) = at {
                    shape_position(&tokens[at + 1..], units, vp)?
                } else {
                    [percent(0.5), percent(0.5)]
                };
                BasicShape::Ellipse {
                    circle,
                    radii,
                    position,
                }
            }
            "polygon" => {
                let mut parts = crate::dom::split_top_level(inner, ',');
                let evenodd = parts.first().is_some_and(|v| v.trim() == "evenodd");
                if parts
                    .first()
                    .is_some_and(|v| matches!(v.trim(), "evenodd" | "nonzero"))
                {
                    parts.remove(0);
                }
                if parts.is_empty() || parts.len() > 4096 {
                    return None;
                }
                let points = parts
                    .into_iter()
                    .map(|part| {
                        let values = crate::dom::split_top_level_ws(part);
                        if values.len() != 2 {
                            return None;
                        }
                        Some([
                            shape_length(values[0], units, vp, false)?,
                            shape_length(values[1], units, vp, false)?,
                        ])
                    })
                    .collect::<Option<_>>()?;
                BasicShape::Polygon { points, evenodd }
            }
            _ => return None,
        };
        Some(Self {
            reference: box_kind,
            basic,
        })
    }

    pub(super) fn shape_for(&self, fragment: &super::flow::Frag<'_>) -> Option<PaintShape> {
        let reference = match self.reference {
            ReferenceBox::Border => CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h),
            ReferenceBox::Content => fragment.content_box(),
            ReferenceBox::Padding => {
                let [t, r, b, l] = fragment.border;
                CssRect::new(
                    fragment.x + l,
                    fragment.y + t,
                    (fragment.w - l - r).max(0.),
                    (fragment.h - t - b).max(0.),
                )
            }
        };
        self.shape(reference)
    }

    pub(crate) fn shape(&self, reference: CssRect) -> Option<PaintShape> {
        match &self.basic {
            BasicShape::Inset(inset) => inset.shape(reference),
            BasicShape::Polygon { points, evenodd } => {
                let points = points
                    .iter()
                    .map(|[x, y]| {
                        Some(crate::core::CssPoint::new(
                            reference.x + finite(x.resolve(Some(reference.width))?)?,
                            reference.y + finite(y.resolve(Some(reference.height))?)?,
                        ))
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(PaintShape::Polygon {
                    points,
                    evenodd: *evenodd,
                })
            }
            BasicShape::Ellipse {
                circle,
                radii,
                position,
            } => {
                let cx = finite(position[0].resolve(Some(reference.width))?)?;
                let cy = finite(position[1].resolve(Some(reference.height))?)?;
                let distances = [
                    (cx.abs(), (reference.width - cx).abs()),
                    (cy.abs(), (reference.height - cy).abs()),
                ];
                let radius = |value: &Radius, axis: usize| -> Option<f32> {
                    Some(match value {
                        Radius::Length(len) => finite(len.resolve(Some(if *circle {
                            reference.width.hypot(reference.height) / std::f32::consts::SQRT_2
                        } else if axis == 0 {
                            reference.width
                        } else {
                            reference.height
                        }))?)?
                        .max(0.),
                        Radius::Closest => {
                            if *circle {
                                distances
                                    .iter()
                                    .map(|(a, b)| a.min(*b))
                                    .fold(f32::INFINITY, f32::min)
                            } else {
                                distances[axis].0.min(distances[axis].1)
                            }
                        }
                        Radius::Farthest => {
                            if *circle {
                                distances.iter().map(|(a, b)| a.max(*b)).fold(0., f32::max)
                            } else {
                                distances[axis].0.max(distances[axis].1)
                            }
                        }
                    })
                };
                let rx = radius(&radii[0], 0)?;
                let ry = radius(&radii[1], 1)?;
                Some(PaintShape::RoundedRect {
                    rect: CssRect::new(
                        reference.x + cx - rx,
                        reference.y + cy - ry,
                        2. * rx,
                        2. * ry,
                    ),
                    radii: CornerRadii {
                        corners: [(rx, ry); 4],
                    },
                })
            }
        }
    }
}

fn percent(k: f32) -> Len {
    Len::Val(super::value::Node::Lin { k, b: 0. })
}
fn shape_length(text: &str, units: Units, vp: Vp, nonnegative: bool) -> Option<Len> {
    let values = lengths(&[text], units, vp, nonnegative)?;
    Some(values[0].clone())
}
fn shape_position(tokens: &[&str], units: Units, vp: Vp) -> Option<[Len; 2]> {
    let axis = |token: &str, vertical: bool| match token {
        "center" => Some(percent(0.5)),
        "left" if !vertical => Some(percent(0.)),
        "right" if !vertical => Some(percent(1.)),
        "top" if vertical => Some(percent(0.)),
        "bottom" if vertical => Some(percent(1.)),
        _ => shape_length(token, units, vp, false),
    };
    match tokens {
        [one] if matches!(*one, "top" | "bottom") => Some([percent(0.5), axis(one, true)?]),
        [one] => Some([axis(one, false)?, percent(0.5)]),
        [x, y] => {
            if matches!(*x, "top" | "bottom") || matches!(*y, "left" | "right") {
                Some([axis(y, false)?, axis(x, true)?])
            } else {
                Some([axis(x, false)?, axis(y, true)?])
            }
        }
        values if (3..=4).contains(&values.len()) => {
            let mut result: [Option<Len>; 2] = [None, None];
            let mut i = 0;
            while i < values.len() {
                let name = values[i];
                let a = match name {
                    "left" | "right" => 0,
                    "top" | "bottom" => 1,
                    _ => return None,
                };
                if result[a].is_some() {
                    return None;
                }
                let end = matches!(name, "right" | "bottom");
                let offset = values
                    .get(i + 1)
                    .and_then(|v| shape_length(v, units, vp, false));
                let value = if let Some(offset) = offset {
                    i += 1;
                    if let Len::Val(node) = offset {
                        Len::Val(super::value::Node::Sum(
                            Box::new(super::value::Node::Lin {
                                k: if end { 1. } else { 0. },
                                b: 0.,
                            }),
                            Box::new(node),
                            if end { -1. } else { 1. },
                        ))
                    } else {
                        return None;
                    }
                } else {
                    percent(if end { 1. } else { 0. })
                };
                result[a] = Some(value);
                i += 1;
            }
            Some([result[0].take()?, result[1].take()?])
        }
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Inset {
    offsets: [Len; 4],
    radii: Option<([Len; 4], [Len; 4])>,
}

pub(crate) fn supports(value: &str) -> bool {
    ClipPath::parse(
        value,
        Units {
            fs: 16.,
            root: 16.,
            ch: 8.,
        },
        Vp { w: 100., h: 100. },
    )
    .is_some()
}

impl Inset {
    pub(crate) fn parse(value: &str, units: Units, vp: Vp) -> Option<Self> {
        let lower = value.trim().to_ascii_lowercase();
        let mut value = lower.as_str();
        let boxes = ["border-box", "stroke-box", "view-box"];
        for name in boxes {
            if let Some(rest) = value
                .strip_prefix(name)
                .filter(|rest| rest.starts_with(char::is_whitespace))
            {
                value = rest.trim();
                break;
            }
        }
        let inner = value.strip_prefix("inset(")?;
        let mut depth = 1;
        let end = inner.char_indices().find_map(|(i, c)| {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(i)
        })?;
        let tail = inner[end + 1..].trim();
        if !tail.is_empty() && !boxes.contains(&tail) {
            return None;
        }
        // A geometry-box may occur only once, on either side.
        if lower.as_str() != value && !tail.is_empty() {
            return None;
        }
        let parts = tokens(&inner[..end])?;
        let round = parts.iter().position(|part| *part == "round");
        let offsets = lengths(&parts[..round.unwrap_or(parts.len())], units, vp, false)?;
        let radii = if let Some(round) = round {
            let parts = &parts[round + 1..];
            let slash = parts.iter().position(|part| *part == "/");
            let x = lengths(&parts[..slash.unwrap_or(parts.len())], units, vp, true)?;
            let y = if let Some(slash) = slash {
                lengths(&parts[slash + 1..], units, vp, true)?
            } else {
                x.clone()
            };
            Some((x, y))
        } else {
            None
        };
        Some(Self { offsets, radii })
    }

    pub(crate) fn shape(&self, reference: CssRect) -> Option<PaintShape> {
        let mut offsets = [0.; 4];
        for (i, len) in self.offsets.iter().enumerate() {
            offsets[i] = finite(len.resolve(Some(if i % 2 == 0 {
                reference.height
            } else {
                reference.width
            }))?)?;
        }
        // CSS Shapes 1: opposing insets exceeding the reference dimension
        // shrink proportionally, retaining an active empty clip.
        for (a, b, size) in [(0, 2, reference.height), (1, 3, reference.width)] {
            let sum = offsets[a] + offsets[b];
            if sum > size && sum > 0. {
                let factor = size / sum;
                offsets[a] *= factor;
                offsets[b] *= factor;
            }
        }
        let [top, right, bottom, left] = offsets;
        let rect = CssRect::new(
            reference.x + left,
            reference.y + top,
            (reference.width - left - right).max(0.),
            (reference.height - top - bottom).max(0.),
        );
        let Some((xs, ys)) = &self.radii else {
            return Some(PaintShape::Rect(rect));
        };
        let mut corners = [(0., 0.); 4];
        for i in 0..4 {
            // Basic-shape percentages refer to the reference box, including
            // corner radii; the resulting corners then fit the inset rectangle.
            corners[i] = (
                finite(xs[i].resolve(Some(reference.width))?)?.max(0.),
                finite(ys[i].resolve(Some(reference.height))?)?.max(0.),
            );
        }
        let factor = [
            (corners[0].0 + corners[1].0, rect.width),
            (corners[3].0 + corners[2].0, rect.width),
            (corners[0].1 + corners[3].1, rect.height),
            (corners[1].1 + corners[2].1, rect.height),
        ]
        .into_iter()
        .filter(|(sum, _)| *sum > 0.)
        .map(|(sum, side)| side / sum)
        .fold(1., f32::min);
        for (x, y) in &mut corners {
            *x *= factor;
            *y *= factor;
        }
        Some(PaintShape::RoundedRect {
            rect,
            radii: CornerRadii { corners },
        })
    }
}

fn finite(value: f32) -> Option<f32> {
    value.is_finite().then_some(value)
}

fn lengths(parts: &[&str], units: Units, vp: Vp, nonnegative: bool) -> Option<[Len; 4]> {
    if !(1..=4).contains(&parts.len()) {
        return None;
    }
    let values = parts
        .iter()
        .map(|part| {
            // Unlike SVG presentation syntax, CSS allows only zero without a unit.
            if part.parse::<f32>().is_ok_and(|n| n != 0.) {
                return None;
            }
            let length = Len::parse(part, units, vp)?;
            if !matches!(length, Len::Val(_)) {
                return None;
            }
            if nonnegative
                && !part.starts_with("calc(")
                && length.resolve(Some(100.)).is_some_and(|n| n < 0.)
            {
                return None;
            }
            Some(length)
        })
        .collect::<Option<Vec<_>>>()?;
    Some([
        values[0].clone(),
        values.get(1).unwrap_or(&values[0]).clone(),
        values.get(2).unwrap_or(&values[0]).clone(),
        values
            .get(3)
            .or(values.get(1))
            .unwrap_or(&values[0])
            .clone(),
    ])
}

/// Whitespace/slash tokens with math functions intact. Reject unbalanced
/// parentheses and trailing junk instead of applying a partial clipping shape.
fn tokens(value: &str) -> Option<Vec<&str>> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth = 0u32;
    for (i, c) in value.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.checked_sub(1)?,
            _ => {}
        }
        if depth == 0 && (c.is_whitespace() || c == '/') {
            if start < i {
                result.push(&value[start..i]);
            }
            if c == '/' {
                result.push(&value[i..i + 1]);
            }
            start = i + c.len_utf8();
        }
    }
    if depth != 0 {
        return None;
    }
    if start < value.len() {
        result.push(&value[start..]);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(value: &str) -> ClipPath {
        ClipPath::parse(
            value,
            Units {
                fs: 16.,
                root: 16.,
                ch: 8.,
            },
            Vp { w: 800., h: 600. },
        )
        .unwrap()
    }
    #[test]
    fn basic_clip_shapes_resolve_percentages_positions_and_fill_rules() {
        let reference = CssRect::new(10., 20., 200., 100.);
        let PaintShape::RoundedRect { rect, radii } =
            parse("ellipse(25% 40% at right 10px bottom 20px) padding-box")
                .shape(reference)
                .unwrap()
        else {
            panic!("ellipse");
        };
        assert_eq!(rect, CssRect::new(150., 60., 100., 80.));
        assert_eq!(radii.corners, [(50., 40.); 4]);
        let PaintShape::RoundedRect { rect, .. } =
            parse("circle(50% at 0 0)").shape(reference).unwrap()
        else {
            panic!("circle");
        };
        assert!((rect.width - 200f32.hypot(100.) / std::f32::consts::SQRT_2).abs() < 0.001);
        let PaintShape::RoundedRect { rect, .. } = parse("circle(closest-side at 150% 50%)")
            .shape(reference)
            .unwrap()
        else {
            panic!("outside center");
        };
        assert_eq!(rect.width, 100.);
        let PaintShape::Polygon { points, evenodd } =
            parse("content-box polygon(evenodd, 0 0, calc(100% - 10px) 0, 100% 100%)")
                .shape(reference)
                .unwrap()
        else {
            panic!("polygon");
        };
        assert!(evenodd);
        assert_eq!(
            points,
            vec![
                crate::core::CssPoint::new(10., 20.),
                crate::core::CssPoint::new(200., 20.),
                crate::core::CssPoint::new(210., 120.)
            ]
        );
        for value in [
            "circle(-1px)",
            "circle(10px 20px)",
            "ellipse(10px)",
            "circle(at left left)",
            "polygon(0 0, 10px)",
            "polygon(0 0, 1 1)",
            "inset(0) padding-box border-box",
        ] {
            assert!(!supports(value), "invalid: {value}");
        }
    }
}
