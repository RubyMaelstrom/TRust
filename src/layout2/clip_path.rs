//! CSS Masking 1 §5 and CSS Shapes 1 §3: inset clipping in CSS-pixel space.
//! Local reference: csswg-drafts snapshot 81c27f686901 (2026-09-06).
//! The supported basic shape is inset(), including rounded corners, on the
//! border box (and its CSS-layout stroke-box/view-box aliases). Other basic
//! shapes, geometry boxes and URL clip sources remain unsupported here.
use super::Units;
use super::value::{Len, Vp};
use crate::render::{CornerRadii, CssRect, PaintShape};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Inset {
    offsets: [Len; 4],
    radii: Option<([Len; 4], [Len; 4])>,
}

pub(crate) fn supports(value: &str) -> bool {
    Inset::parse(
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
