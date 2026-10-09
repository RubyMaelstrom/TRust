//! CSS Masking 1 §5 and CSS Shapes 1 §3: clipping paths in CSS pixels.
//! Local reference: csswg-drafts snapshot 81c27f686901 (2026-09-06).
//! Shape construction is shared by graphical paint, hit testing and terminal
//! clipping bounds. `clip-path: <url>` references an SVG `clipPath`, which
//! [`ClipPath::resolve`] turns into a document-free source.
use std::sync::Arc;

use super::Units;
use super::value::{Len, Vp};
use crate::core::CssPoint;
use crate::render::{ClipGeometry, CornerRadii, CssRect, PaintShape, PathElement};

/// CSS Masking 1 #the-clip-path: `<clip-source> | [ <basic-shape> ||
/// <geometry-box> ] | none`, with CSS Shapes 1 #basic-shape-functions.
/// Published Shapes 1 §3.1 also defines percentage circle radii against the
/// normalized diagonal (2014 CR); retain that interoperable value grammar.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ClipPath {
    reference: ReferenceBox,
    source: Source,
    /// The element's computed border radii (horizontal, vertical) and
    /// margins, captured by `resolve` for box-shaped clips and `margin-box`.
    corners: Option<Arc<[[Len; 2]; 4]>>,
    margins: Option<Arc<[Len; 4]>>,
}

/// CSS Masking 1 #typedef-geometry-box. For a box with a CSS layout box,
/// `fill-box` is used as `content-box` and `stroke-box`/`view-box` as
/// `border-box` (#the-clip-path).
#[derive(Clone, Copy, Debug, PartialEq)]
enum ReferenceBox {
    Border,
    Padding,
    Content,
    Margin,
}

#[derive(Clone, Debug, PartialEq)]
enum Source {
    Shape(BasicShape),
    /// A `<geometry-box>` alone: the box's edges, including corner shaping
    /// (CSS Shapes 1 #shapes-from-box-values).
    Box,
    /// An unresolved `<clip-source>`.
    Url(String),
    /// A resolved SVG `clipPath` (#ClipPathElement).
    Svg(Arc<crate::dom::svg_geometry::SvgClipSource>),
}

#[derive(Clone, Debug, PartialEq)]
enum BasicShape {
    /// `inset()`, and `rect()`/`xywh()`, which compute to it
    /// (#basic-shape-computed-values).
    Inset(Box<Inset>),
    Ellipse {
        circle: bool,
        radii: [Radius; 2],
        position: [Len; 2],
    },
    Polygon {
        points: Vec<[Len; 2]>,
        evenodd: bool,
        round: Option<Len>,
    },
    /// `path()`: SVG path data in px of the reference box.
    Path {
        elements: Vec<PathElement>,
        evenodd: bool,
    },
    /// `shape()`.
    Shape {
        evenodd: bool,
        from: [Len; 2],
        commands: Vec<ShapeCommand>,
    },
}

#[derive(Clone, Debug, PartialEq)]
enum Radius {
    Length(Len),
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
}

/// One `<shape-command>` (#shape-function).
#[derive(Clone, Debug, PartialEq)]
enum ShapeCommand {
    Move(EndPoint),
    Line(EndPoint),
    HLine {
        by: bool,
        value: Len,
    },
    VLine {
        by: bool,
        value: Len,
    },
    Curve {
        end: EndPoint,
        controls: Vec<ControlPoint>,
        smooth: bool,
    },
    Arc {
        end: EndPoint,
        radii: [Len; 2],
        /// One radius given: percentages use the direction-agnostic size.
        single: bool,
        clockwise: bool,
        large: bool,
        rotate: f32,
    },
    Close,
}

#[derive(Clone, Debug, PartialEq)]
enum EndPoint {
    /// `to <position>`: from the reference box's top left.
    To([Len; 2]),
    /// `by <coordinate-pair>`: from the command's start.
    By([Len; 2]),
}

#[derive(Clone, Debug, PartialEq)]
enum ControlPoint {
    Absolute([Len; 2]),
    Relative([Len; 2], Anchor),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Anchor {
    Start,
    End,
    Origin,
}

impl ClipPath {
    pub(crate) fn parse(value: &str, units: Units, vp: Vp) -> Option<Self> {
        let trimmed = value.trim();
        if let Some(url) = crate::dom::css_url_prefix(trimmed)
            .and_then(|(url, rest)| rest.trim().is_empty().then_some(url))
        {
            return Some(Self {
                reference: ReferenceBox::Border,
                source: Source::Url(url),
                corners: None,
                margins: None,
            });
        }
        let lower = trimmed.to_ascii_lowercase();
        let mut parts = crate::dom::split_top_level_ws(&lower);
        let reference = |name: &str| match name {
            "border-box" | "stroke-box" | "view-box" => Some(ReferenceBox::Border),
            "padding-box" => Some(ReferenceBox::Padding),
            "content-box" | "fill-box" => Some(ReferenceBox::Content),
            "margin-box" => Some(ReferenceBox::Margin),
            _ => None,
        };
        let mut box_kind = None;
        if let Some(kind) = parts.first().and_then(|v| reference(v)) {
            box_kind = Some(kind);
            parts.remove(0);
        } else if let Some(kind) = parts.last().and_then(|v| reference(v)) {
            box_kind = Some(kind);
            parts.pop();
        }
        if parts.is_empty() {
            // A <geometry-box> by itself.
            return box_kind.map(|reference| Self {
                reference,
                source: Source::Box,
                corners: None,
                margins: None,
            });
        }
        if parts.len() != 1 {
            return None;
        }
        // path() keeps its string's case; re-slice the original text.
        let start = parts[0].as_ptr() as usize - lower.as_ptr() as usize;
        let original = &trimmed[start..start + parts[0].len()];
        let text = parts[0];
        let open = text.find('(')?;
        let kind = &text[..open];
        let inner = text[open + 1..].strip_suffix(')')?;
        if !crate::dom::cssom::valid_value(text) {
            return None;
        }
        let basic = match kind {
            "inset" => BasicShape::Inset(Box::new(Inset::parse(text, units, vp)?)),
            "rect" => BasicShape::Inset(Box::new(Inset::parse_rect(inner, units, vp)?)),
            "xywh" => BasicShape::Inset(Box::new(Inset::parse_xywh(inner, units, vp)?)),
            "circle" | "ellipse" => {
                let tokens = crate::dom::split_top_level_ws(inner);
                let at = tokens.iter().position(|v| *v == "at");
                let radii = &tokens[..at.unwrap_or(tokens.len())];
                let radius = |text| match text {
                    "closest-side" => Some(Radius::ClosestSide),
                    "farthest-side" => Some(Radius::FarthestSide),
                    "closest-corner" => Some(Radius::ClosestCorner),
                    "farthest-corner" => Some(Radius::FarthestCorner),
                    _ => shape_length(text, units, vp, true).map(Radius::Length),
                };
                let circle = kind == "circle";
                let radii = match radii {
                    [] => [Radius::ClosestSide, Radius::ClosestSide],
                    [one] if circle => {
                        let r = radius(one)?;
                        [r.clone(), r]
                    }
                    // CSS Images 4 #typedef-radial-size: one extent keyword
                    // sizes both ellipse radii.
                    [one] if !circle && !matches!(radius(one)?, Radius::Length(_)) => {
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
                // `<'fill-rule'>? [ round <length> ]?` before the first comma.
                let mut evenodd = false;
                let mut round = None;
                if let Some(first) = parts.first() {
                    let tokens = crate::dom::split_top_level_ws(first);
                    let mut rest = tokens.as_slice();
                    let mut prelude = false;
                    if let Some(rule) = rest
                        .first()
                        .filter(|t| matches!(**t, "evenodd" | "nonzero"))
                    {
                        evenodd = *rule == "evenodd";
                        rest = &rest[1..];
                        prelude = true;
                    }
                    if rest.first() == Some(&"round") {
                        let [_, length] = rest else {
                            return None;
                        };
                        round = Some(shape_length(length, units, vp, true)?);
                        if round
                            .as_ref()
                            .is_some_and(|len| len.resolve(None).is_none())
                        {
                            return None;
                        }
                        rest = &[];
                        prelude = true;
                    }
                    if prelude {
                        if !rest.is_empty() {
                            return None;
                        }
                        parts.remove(0);
                    }
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
                BasicShape::Polygon {
                    points,
                    evenodd,
                    round,
                }
            }
            "path" => {
                let open = original.find('(')?;
                let inner = original[open + 1..].strip_suffix(')')?;
                parse_path_function(inner)?
            }
            "shape" => parse_shape_function(inner, units, vp)?,
            _ => return None,
        };
        Some(Self {
            reference: box_kind.unwrap_or(ReferenceBox::Border),
            source: Source::Shape(basic),
            corners: None,
            margins: None,
        })
    }

    /// Resolve against `node`'s document: a `<url>` to its `clipPath`
    /// (`None` for an invalid reference, which applies no clipping), and
    /// the box values a box-shaped clip needs.
    pub(crate) fn resolve(
        mut self,
        dom: &crate::dom::Dom,
        node: crate::dom::NodeId,
        document_url: Option<&url::Url>,
    ) -> Option<Self> {
        if let Source::Url(url) = &self.source {
            let target = dom.svg_resource_target(node, url, document_url);
            dom.note_svg_resource_use(node, target);
            let target = target?;
            self.source = Source::Svg(Arc::new(dom.svg_clip_source(target)?));
            return Some(self);
        }
        let units = Units::of(dom, node);
        let (w, h) = dom.viewport_px();
        let vp = Vp { w, h };
        let parse = |property: &str| {
            dom.computed_value_resolved(node, property)
                .and_then(|value| Len::parse(value.trim(), units, vp))
                .unwrap_or(Len::Val(super::value::Node::Lin { k: 0., b: 0. }))
        };
        if matches!(self.source, Source::Box) {
            let corner = |property: &str| -> [Len; 2] {
                let value = dom
                    .computed_value_resolved(node, property)
                    .unwrap_or_default();
                let parts = crate::dom::split_top_level_ws(&value);
                let len = |text: &str| {
                    Len::parse(text, units, vp)
                        .unwrap_or(Len::Val(super::value::Node::Lin { k: 0., b: 0. }))
                };
                let x = parts.first().map_or_else(|| len("0"), |v| len(v));
                let y = parts.get(1).map_or_else(|| x.clone(), |v| len(v));
                [x, y]
            };
            self.corners = Some(Arc::new([
                corner("border-top-left-radius"),
                corner("border-top-right-radius"),
                corner("border-bottom-right-radius"),
                corner("border-bottom-left-radius"),
            ]));
        }
        if self.reference == ReferenceBox::Margin {
            self.margins = Some(Arc::new([
                parse("margin-top"),
                parse("margin-right"),
                parse("margin-bottom"),
                parse("margin-left"),
            ]));
        }
        Some(self)
    }

    pub(super) fn shape_for(&self, fragment: &super::flow::Frag) -> Option<PaintShape> {
        let border_box = CssRect::new(fragment.x, fragment.y, fragment.w, fragment.h);
        let [t, r, b, l] = fragment.border;
        let padding_box = CssRect::new(
            fragment.x + l,
            fragment.y + t,
            (fragment.w - l - r).max(0.),
            (fragment.h - t - b).max(0.),
        );
        let content_box = fragment.content_box();
        let margins = self
            .margins
            .as_ref()
            .map(|margins| used_margins(margins, fragment))
            .unwrap_or([0.; 4]);
        let margin_box = CssRect::new(
            border_box.x - margins[3],
            border_box.y - margins[0],
            (border_box.width + margins[1] + margins[3]).max(0.),
            (border_box.height + margins[0] + margins[2]).max(0.),
        );
        let reference = match self.reference {
            ReferenceBox::Border => border_box,
            ReferenceBox::Padding => padding_box,
            ReferenceBox::Content => content_box,
            ReferenceBox::Margin => margin_box,
        };
        match &self.source {
            Source::Shape(_) => self.shape(reference),
            Source::Url(_) => None,
            Source::Svg(source) => {
                let geometry = source.geometry(border_box)?;
                Some(match geometry.single_path() {
                    Some((elements, false)) => PaintShape::Path(elements.to_vec()),
                    _ => PaintShape::Geometry(Arc::new(geometry)),
                })
            }
            Source::Box => {
                let radii = self
                    .corners
                    .as_ref()
                    .map_or_else(CornerRadii::default, |corners| {
                        border_radii(corners, border_box)
                    });
                // CSS Backgrounds 3 #corner-shaping: inner edges subtract
                // the border (and padding) widths; the margin edge adds the
                // margins with the outset-adjusted border radius.
                let radii = match self.reference {
                    ReferenceBox::Border => radii,
                    ReferenceBox::Padding => inset_corners(radii, border_box, padding_box),
                    ReferenceBox::Content => inset_corners(radii, border_box, content_box),
                    ReferenceBox::Margin => margin_corners(radii, border_box, margins),
                };
                Some(if radii.corners.iter().all(|&(x, y)| x <= 0. || y <= 0.) {
                    PaintShape::Rect(reference)
                } else {
                    PaintShape::RoundedRect {
                        rect: reference,
                        radii,
                    }
                })
            }
        }
    }

    /// The basic shape on `reference`; `None` for a box or reference clip.
    pub(crate) fn shape(&self, reference: CssRect) -> Option<PaintShape> {
        let Source::Shape(basic) = &self.source else {
            return None;
        };
        match basic {
            BasicShape::Inset(inset) => inset.shape(reference),
            BasicShape::Polygon {
                points,
                evenodd,
                round,
            } => {
                let points = points
                    .iter()
                    .map(|[x, y]| {
                        Some(CssPoint::new(
                            reference.x + finite(x.resolve(Some(reference.width))?)?,
                            reference.y + finite(y.resolve(Some(reference.height))?)?,
                        ))
                    })
                    .collect::<Option<Vec<_>>>()?;
                let radius = round
                    .as_ref()
                    .and_then(|round| round.resolve(None))
                    .filter(|radius| *radius > 0.);
                match radius {
                    Some(radius) if points.len() >= 3 => {
                        Some(geometry_shape(rounded_polygon(&points, radius), *evenodd))
                    }
                    _ => Some(PaintShape::Polygon {
                        points,
                        evenodd: *evenodd,
                    }),
                }
            }
            BasicShape::Path { elements, evenodd } => Some(geometry_shape(
                elements
                    .iter()
                    .map(|element| offset_element(element, reference.x, reference.y))
                    .collect(),
                *evenodd,
            )),
            BasicShape::Shape {
                evenodd,
                from,
                commands,
            } => Some(geometry_shape(
                shape_elements(from, commands, reference)?,
                *evenodd,
            )),
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
                let side = |closest: bool, axis: usize| {
                    let (a, b) = distances[axis];
                    if closest { a.min(b) } else { a.max(b) }
                };
                // CSS Images 4 #radial-gradient-syntax: corner extents keep
                // the side extent's aspect ratio for an ellipse.
                let corner = |closest: bool, axis: usize| -> f32 {
                    let (dx, dy) = (side(closest, 0), side(closest, 1));
                    if *circle {
                        return dx.hypot(dy);
                    }
                    let (sx, sy) = (side(true, 0), side(true, 1));
                    if sx <= 0. || sy <= 0. {
                        return 0.;
                    }
                    let scale = ((dx / sx).powi(2) + (dy / sy).powi(2)).sqrt();
                    scale * if axis == 0 { sx } else { sy }
                };
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
                        Radius::ClosestSide => {
                            if *circle {
                                side(true, 0).min(side(true, 1))
                            } else {
                                side(true, axis)
                            }
                        }
                        Radius::FarthestSide => {
                            if *circle {
                                side(false, 0).max(side(false, 1))
                            } else {
                                side(false, axis)
                            }
                        }
                        Radius::ClosestCorner => corner(true, axis),
                        Radius::FarthestCorner => corner(false, axis),
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

/// A nonzero path stays a plain path; an even-odd one needs clip geometry.
fn geometry_shape(elements: Vec<PathElement>, evenodd: bool) -> PaintShape {
    if evenodd {
        PaintShape::Geometry(Arc::new(ClipGeometry::Path { elements, evenodd }))
    } else {
        PaintShape::Path(elements)
    }
}

fn offset_element(element: &PathElement, dx: f32, dy: f32) -> PathElement {
    let p = |p: &CssPoint| CssPoint::new(p.x + dx, p.y + dy);
    match element {
        PathElement::MoveTo(a) => PathElement::MoveTo(p(a)),
        PathElement::LineTo(a) => PathElement::LineTo(p(a)),
        PathElement::QuadTo(a, b) => PathElement::QuadTo(p(a), p(b)),
        PathElement::CurveTo(a, b, c) => PathElement::CurveTo(p(a), p(b), p(c)),
        PathElement::Close => PathElement::Close,
    }
}

/// CSS Box 4 #margin-physical with CSS 2 §10.3.3: used margins, percentages
/// of the containing block width. Auto margins of a block-level box in
/// normal flow share the containing block's remaining width; every other
/// auto margin is zero.
fn used_margins(margins: &[Len; 4], fragment: &super::flow::Frag) -> [f32; 4] {
    let cb = fragment.flow.used.cb[0];
    let basis = (!cb.is_nan()).then_some(cb);
    let resolved = margins.clone().map(|len| match len {
        Len::Auto => None,
        len => Some(len.resolve(basis).filter(|v| v.is_finite()).unwrap_or(0.)),
    });
    let mut used = resolved.map(|value| value.unwrap_or(0.));
    let in_flow_block = matches!(fragment.kind, super::flow::FragKind::Block)
        && !fragment.paint.float
        && !matches!(
            fragment.paint.position,
            super::style::Pos::Absolute | super::style::Pos::Fixed
        );
    if in_flow_block && let Some(cb) = basis {
        let remaining = cb - fragment.w;
        match (resolved[3], resolved[1]) {
            (None, None) => {
                let half = (remaining / 2.).max(0.);
                used[3] = half;
                used[1] = half;
            }
            (None, Some(right)) => used[3] = remaining - right,
            (Some(left), None) => used[1] = remaining - left,
            _ => {}
        }
    }
    used
}

/// CSS Backgrounds 3 #border-radius and #corner-overlap: the border box's
/// corner radii, reduced proportionally when they overlap.
fn border_radii(corners: &[[Len; 2]; 4], rect: CssRect) -> CornerRadii {
    let mut radii = corners.clone().map(|[x, y]| {
        (
            x.resolve(Some(rect.width))
                .filter(|v| v.is_finite())
                .unwrap_or(0.)
                .max(0.),
            y.resolve(Some(rect.height))
                .filter(|v| v.is_finite())
                .unwrap_or(0.)
                .max(0.),
        )
    });
    let factor = [
        (radii[0].0 + radii[1].0, rect.width),
        (radii[3].0 + radii[2].0, rect.width),
        (radii[0].1 + radii[3].1, rect.height),
        (radii[1].1 + radii[2].1, rect.height),
    ]
    .into_iter()
    .filter(|(sum, _)| *sum > 0.)
    .map(|(sum, side)| side / sum)
    .fold(1.0f32, f32::min);
    for corner in &mut radii {
        corner.0 *= factor;
        corner.1 *= factor;
    }
    CornerRadii { corners: radii }
}

/// The radii of an inner edge: each reduced by the distance between the
/// edges on its sides (#corner-shaping), floored at zero.
fn inset_corners(radii: CornerRadii, outer: CssRect, inner: CssRect) -> CornerRadii {
    let left = (inner.x - outer.x).max(0.);
    let top = (inner.y - outer.y).max(0.);
    let right = (outer.x + outer.width - inner.x - inner.width).max(0.);
    let bottom = (outer.y + outer.height - inner.y - inner.height).max(0.);
    let mut corners = radii.corners;
    for (corner, (x, y)) in
        corners
            .iter_mut()
            .zip([(left, top), (right, top), (right, bottom), (left, bottom)])
    {
        corner.0 = (corner.0 - x).max(0.);
        corner.1 = (corner.1 - y).max(0.);
    }
    CornerRadii { corners }
}

/// CSS Shapes 1 #shapes-from-box-values `margin-box` and CSS Backgrounds 3
/// #outset-adjusted-border-radius: each corner grows by its two adjacent
/// margins, less so where the radius is small for its box.
fn margin_corners(
    radii: CornerRadii,
    edge: CssRect,
    [top, right, bottom, left]: [f32; 4],
) -> CornerRadii {
    let outsets = [(left, top), (right, top), (right, bottom), (left, bottom)];
    let mut corners = radii.corners;
    for ((x, y), (ox, oy)) in corners.iter_mut().zip(outsets) {
        let ratio = |radius: f32, length: f32| if length > 0. { radius / length } else { 0. };
        let coverage = 2. * ratio(*x, edge.width).min(ratio(*y, edge.height));
        let adjust = |radius: f32, outset: f32| {
            if outset <= 0. {
                return (radius + outset).max(0.);
            }
            if radius > outset || coverage > 1. {
                return radius + outset;
            }
            let ratio = radius / outset;
            radius + outset * (1. - (1. - ratio).powi(3) * (1. - coverage.powi(3)))
        };
        // A square corner stays square.
        if *x > 0. && *y > 0. {
            *x = adjust(*x, ox);
            *y = adjust(*y, oy);
        }
    }
    CornerRadii { corners }
}

/// CSS Shapes 1 #funcdef-basic-shape-polygon `round <length>`: each vertex
/// replaced by an arc of the circle of that radius tangent to both edges,
/// the radius clamped so no more than half of either edge is rounded.
fn rounded_polygon(points: &[CssPoint], radius: f32) -> Vec<PathElement> {
    let n = points.len();
    let mut elements = Vec::with_capacity(n * 2 + 1);
    for i in 0..n {
        let prev = points[(i + n - 1) % n];
        let vertex = points[i];
        let next = points[(i + 1) % n];
        let (ax, ay) = (prev.x - vertex.x, prev.y - vertex.y);
        let (bx, by) = (next.x - vertex.x, next.y - vertex.y);
        let (la, lb) = (ax.hypot(ay), bx.hypot(by));
        let angle = if la > 0. && lb > 0. {
            ((ax * bx + ay * by) / (la * lb)).clamp(-1., 1.).acos()
        } else {
            0.
        };
        let half_tan = (angle / 2.).tan();
        // Tangent distance d = r / tan(θ/2), at most half of each edge.
        let r = if half_tan > 0. && half_tan.is_finite() {
            radius.min(half_tan * la / 2.).min(half_tan * lb / 2.)
        } else {
            0.
        };
        let d = if r > 0. { r / half_tan } else { 0. };
        let start = if la > 0. {
            CssPoint::new(vertex.x + ax / la * d, vertex.y + ay / la * d)
        } else {
            vertex
        };
        let end = if lb > 0. {
            CssPoint::new(vertex.x + bx / lb * d, vertex.y + by / lb * d)
        } else {
            vertex
        };
        elements.push(if i == 0 {
            PathElement::MoveTo(start)
        } else {
            PathElement::LineTo(start)
        });
        if r > 0. {
            // The tangent arc as one cubic: control points along the edges
            // at k·d from each tangent point toward the vertex.
            let sweep = std::f32::consts::PI - angle;
            let k = 4. / 3. * (sweep / 4.).tan() * r / d.max(f32::EPSILON);
            let c1 = CssPoint::new(
                start.x + (vertex.x - start.x) * k,
                start.y + (vertex.y - start.y) * k,
            );
            let c2 = CssPoint::new(
                end.x + (vertex.x - end.x) * k,
                end.y + (vertex.y - end.y) * k,
            );
            elements.push(PathElement::CurveTo(c1, c2, end));
        }
    }
    elements.push(PathElement::Close);
    elements
}

/// `path( <'fill-rule'>? , <string> )`: the SVG path data, which must
/// parse completely and be non-empty (#funcdef-basic-shape-path).
fn parse_path_function(inner: &str) -> Option<BasicShape> {
    let parts = crate::dom::split_top_level(inner, ',');
    let (rule, data) = match parts.as_slice() {
        [data] => (None, *data),
        [rule, data] => (Some(rule.trim()), *data),
        _ => return None,
    };
    let evenodd = match rule {
        None => false,
        Some(rule) if rule.eq_ignore_ascii_case("evenodd") => true,
        Some(rule) if rule.eq_ignore_ascii_case("nonzero") => false,
        Some(_) => return None,
    };
    let data = data.trim();
    let quote = data.chars().next()?;
    if !matches!(quote, '"' | '\'') || !data.ends_with(quote) || data.len() < 2 {
        return None;
    }
    let data = &data[1..data.len() - 1];
    let mut elements = Vec::new();
    let mut started = false;
    for segment in svgtypes::SimplifyingPathParser::from(data) {
        use svgtypes::SimplePathSegment as S;
        let p = |x: f64, y: f64| CssPoint::new(x as f32, y as f32);
        elements.push(match segment.ok()? {
            S::MoveTo { x, y } => {
                started = true;
                PathElement::MoveTo(p(x, y))
            }
            S::LineTo { x, y } => PathElement::LineTo(p(x, y)),
            S::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => PathElement::CurveTo(p(x1, y1), p(x2, y2), p(x, y)),
            S::Quadratic { x1, y1, x, y } => PathElement::QuadTo(p(x1, y1), p(x, y)),
            S::ClosePath => PathElement::Close,
        });
    }
    // An empty path is invalid; a path's first command must be a moveto.
    if !started || elements.is_empty() {
        return None;
    }
    if !matches!(elements.last(), Some(PathElement::Close)) {
        elements.push(PathElement::Close);
    }
    Some(BasicShape::Path { elements, evenodd })
}

/// `shape( <'fill-rule'>? from <position> , <shape-command># )`
/// (#shape-function).
fn parse_shape_function(inner: &str, units: Units, vp: Vp) -> Option<BasicShape> {
    let parts = crate::dom::split_top_level(inner, ',');
    let (first, commands) = parts.split_first()?;
    if commands.is_empty() {
        return None;
    }
    let tokens = crate::dom::split_top_level_ws(first);
    let mut rest = tokens.as_slice();
    let mut evenodd = false;
    if let Some(rule) = rest
        .first()
        .filter(|t| matches!(**t, "evenodd" | "nonzero"))
    {
        evenodd = *rule == "evenodd";
        rest = &rest[1..];
    }
    let ["from", position @ ..] = rest else {
        return None;
    };
    let from = shape_position(position, units, vp)?;
    let commands = commands
        .iter()
        .map(|command| parse_shape_command(command, units, vp))
        .collect::<Option<Vec<_>>>()?;
    Some(BasicShape::Shape {
        evenodd,
        from,
        commands,
    })
}

fn coordinate_pair(tokens: &[&str], units: Units, vp: Vp) -> Option<[Len; 2]> {
    match tokens {
        [x, y] => Some([
            shape_length(x, units, vp, false)?,
            shape_length(y, units, vp, false)?,
        ]),
        _ => None,
    }
}

fn end_point(tokens: &[&str], units: Units, vp: Vp) -> Option<EndPoint> {
    match tokens {
        ["to", position @ ..] => Some(EndPoint::To(shape_position(position, units, vp)?)),
        ["by", pair @ ..] => Some(EndPoint::By(coordinate_pair(pair, units, vp)?)),
        _ => None,
    }
}

/// `<control-point>` (`relative`: only `<relative-control-point>`).
fn control_point(tokens: &[&str], relative: bool, units: Units, vp: Vp) -> Option<ControlPoint> {
    if let [pair @ .., "from", anchor] = tokens {
        let anchor = match *anchor {
            "start" => Anchor::Start,
            "end" => Anchor::End,
            "origin" => Anchor::Origin,
            _ => return None,
        };
        return Some(ControlPoint::Relative(
            coordinate_pair(pair, units, vp)?,
            anchor,
        ));
    }
    if relative {
        return Some(ControlPoint::Relative(
            coordinate_pair(tokens, units, vp)?,
            Anchor::Start,
        ));
    }
    // A plain pair after `to … with` is a <position>, which the grammar
    // tries first.
    shape_position(tokens, units, vp)
        .map(ControlPoint::Absolute)
        .or_else(|| {
            Some(ControlPoint::Relative(
                coordinate_pair(tokens, units, vp)?,
                Anchor::Start,
            ))
        })
}

fn parse_shape_command(text: &str, units: Units, vp: Vp) -> Option<ShapeCommand> {
    let tokens = crate::dom::split_top_level_ws(text.trim());
    let (keyword, rest) = tokens.split_first()?;
    match *keyword {
        "close" if rest.is_empty() => Some(ShapeCommand::Close),
        "move" => Some(ShapeCommand::Move(end_point(rest, units, vp)?)),
        "line" => Some(ShapeCommand::Line(end_point(rest, units, vp)?)),
        "hline" | "vline" => {
            let horizontal = *keyword == "hline";
            match rest {
                ["by", value] => Some(if horizontal {
                    ShapeCommand::HLine {
                        by: true,
                        value: shape_length(value, units, vp, false)?,
                    }
                } else {
                    ShapeCommand::VLine {
                        by: true,
                        value: shape_length(value, units, vp, false)?,
                    }
                }),
                ["to", value] => {
                    let keyword = match (*value, horizontal) {
                        ("left" | "x-start", true) | ("top" | "y-start", false) => Some(0.),
                        ("center", _) => Some(0.5),
                        ("right" | "x-end", true) | ("bottom" | "y-end", false) => Some(1.),
                        _ => None,
                    };
                    let value = match keyword {
                        Some(k) => percent(k),
                        None => shape_length(value, units, vp, false)?,
                    };
                    Some(if horizontal {
                        ShapeCommand::HLine { by: false, value }
                    } else {
                        ShapeCommand::VLine { by: false, value }
                    })
                }
                _ => None,
            }
        }
        "curve" | "smooth" => {
            let smooth = *keyword == "smooth";
            let with = rest.iter().position(|t| *t == "with");
            let end = end_point(&rest[..with.unwrap_or(rest.len())], units, vp)?;
            let relative = matches!(end, EndPoint::By(_));
            let controls = match with {
                Some(with) => {
                    let controls: Vec<&[&str]> = rest[with + 1..].split(|t| *t == "/").collect();
                    if controls.len() > if smooth { 1 } else { 2 }
                        || controls.iter().any(|c| c.is_empty())
                    {
                        return None;
                    }
                    controls
                        .into_iter()
                        .map(|tokens| control_point(tokens, relative, units, vp))
                        .collect::<Option<Vec<_>>>()?
                }
                // `curve` requires its control points; `smooth` may omit one.
                None if smooth => Vec::new(),
                None => return None,
            };
            Some(ShapeCommand::Curve {
                end,
                controls,
                smooth,
            })
        }
        "arc" => {
            let of = rest.iter().position(|t| *t == "of")?;
            let end = end_point(&rest[..of], units, vp)?;
            let mut radii = Vec::new();
            let mut clockwise = false;
            let mut large = false;
            let mut rotate = 0.;
            let mut seen = (false, false, false);
            let mut i = of + 1;
            while i < rest.len() {
                match rest[i] {
                    "cw" | "ccw" if !seen.0 => {
                        clockwise = rest[i] == "cw";
                        seen.0 = true;
                    }
                    "large" | "small" if !seen.1 => {
                        large = rest[i] == "large";
                        seen.1 = true;
                    }
                    "rotate" if !seen.2 => {
                        i += 1;
                        rotate = angle_degrees(rest.get(i)?)?;
                        seen.2 = true;
                    }
                    value if radii.len() < 2 && !seen.0 && !seen.1 && !seen.2 => {
                        radii.push(shape_length(value, units, vp, true)?);
                    }
                    _ => return None,
                }
                i += 1;
            }
            let single = radii.len() == 1;
            let radii = match radii.as_slice() {
                [r] => [r.clone(), r.clone()],
                [x, y] => [x.clone(), y.clone()],
                _ => return None,
            };
            Some(ShapeCommand::Arc {
                end,
                radii,
                single,
                clockwise,
                large,
                rotate,
            })
        }
        _ => None,
    }
}

fn angle_degrees(text: &str) -> Option<f32> {
    let text = text.trim();
    let value = if let Some(v) = text.strip_suffix("deg") {
        v.parse::<f32>().ok()?
    } else if let Some(v) = text.strip_suffix("grad") {
        v.parse::<f32>().ok()? * 0.9
    } else if let Some(v) = text.strip_suffix("rad") {
        v.parse::<f32>().ok()?.to_degrees()
    } else if let Some(v) = text.strip_suffix("turn") {
        v.parse::<f32>().ok()? * 360.
    } else if text == "0" {
        0.
    } else {
        return None;
    };
    value.is_finite().then_some(value)
}

/// The path data commands a `shape()` represents, in reference box px
/// placed at `reference`.
fn shape_elements(
    from: &[Len; 2],
    commands: &[ShapeCommand],
    reference: CssRect,
) -> Option<Vec<PathElement>> {
    let (w, h) = (reference.width, reference.height);
    let resolve = |pair: &[Len; 2]| -> Option<(f32, f32)> {
        Some((
            finite(pair[0].resolve(Some(w))?)?,
            finite(pair[1].resolve(Some(h))?)?,
        ))
    };
    let at = |(x, y): (f32, f32)| CssPoint::new(reference.x + x, reference.y + y);
    let mut current = resolve(from)?;
    let mut subpath_start = current;
    // The previous curve's last control point, for `smooth` reflection.
    let mut previous_control: Option<(f32, f32)> = None;
    let mut elements = vec![PathElement::MoveTo(at(current))];
    for command in commands {
        let target = |end: &EndPoint, current: (f32, f32)| -> Option<(f32, f32)> {
            Some(match end {
                EndPoint::To(position) => resolve(position)?,
                EndPoint::By(pair) => {
                    let (dx, dy) = resolve(pair)?;
                    (current.0 + dx, current.1 + dy)
                }
            })
        };
        let mut control = None;
        match command {
            ShapeCommand::Move(end) => {
                current = target(end, current)?;
                subpath_start = current;
                elements.push(PathElement::MoveTo(at(current)));
            }
            ShapeCommand::Line(end) => {
                current = target(end, current)?;
                elements.push(PathElement::LineTo(at(current)));
            }
            ShapeCommand::HLine { by, value } => {
                let x = finite(value.resolve(Some(w))?)?;
                current.0 = if *by { current.0 + x } else { x };
                elements.push(PathElement::LineTo(at(current)));
            }
            ShapeCommand::VLine { by, value } => {
                let y = finite(value.resolve(Some(h))?)?;
                current.1 = if *by { current.1 + y } else { y };
                elements.push(PathElement::LineTo(at(current)));
            }
            ShapeCommand::Curve {
                end,
                controls,
                smooth,
            } => {
                let start = current;
                let end = target(end, current)?;
                let point = |control: &ControlPoint| -> Option<(f32, f32)> {
                    Some(match control {
                        ControlPoint::Absolute(position) => resolve(position)?,
                        ControlPoint::Relative(pair, anchor) => {
                            let (dx, dy) = resolve(pair)?;
                            let base = match anchor {
                                Anchor::Start => start,
                                Anchor::End => end,
                                Anchor::Origin => (0., 0.),
                            };
                            (base.0 + dx, base.1 + dy)
                        }
                    })
                };
                let mut points = controls.iter().map(point).collect::<Option<Vec<_>>>()?;
                if *smooth {
                    // The reflection of the previous curve's last control
                    // point about the start, or the start itself.
                    let reflected = previous_control
                        .map_or(start, |(cx, cy)| (2. * start.0 - cx, 2. * start.1 - cy));
                    points.insert(0, reflected);
                }
                match points.as_slice() {
                    [c] => elements.push(PathElement::QuadTo(at(*c), at(end))),
                    [c1, c2] => elements.push(PathElement::CurveTo(at(*c1), at(*c2), at(end))),
                    _ => return None,
                }
                control = points.last().copied();
                current = end;
            }
            ShapeCommand::Arc {
                end,
                radii,
                single,
                clockwise,
                large,
                rotate,
            } => {
                let start = current;
                let end = target(end, current)?;
                let basis = |axis: usize| {
                    if *single {
                        w.hypot(h) / std::f32::consts::SQRT_2
                    } else if axis == 0 {
                        w
                    } else {
                        h
                    }
                };
                let rx = finite(radii[0].resolve(Some(basis(0)))?)?.abs();
                let ry = finite(radii[1].resolve(Some(basis(1)))?)?.abs();
                let arc = vello_cpu::kurbo::SvgArc {
                    from: (f64::from(start.0), f64::from(start.1)).into(),
                    to: (f64::from(end.0), f64::from(end.1)).into(),
                    radii: (f64::from(rx), f64::from(ry)).into(),
                    x_rotation: f64::from(rotate.to_radians()),
                    large_arc: *large,
                    sweep: *clockwise,
                };
                match vello_cpu::kurbo::Arc::from_svg_arc(&arc) {
                    Some(arc) => arc.to_cubic_beziers(0.1, |p1, p2, p| {
                        let q = |p: vello_cpu::kurbo::Point| {
                            CssPoint::new(reference.x + p.x as f32, reference.y + p.y as f32)
                        };
                        elements.push(PathElement::CurveTo(q(p1), q(p2), q(p)));
                    }),
                    // A degenerate arc (a zero radius) is a straight line;
                    // one ending at its start draws nothing.
                    None if start != end => elements.push(PathElement::LineTo(at(end))),
                    None => {}
                }
                current = end;
            }
            ShapeCommand::Close => {
                elements.push(PathElement::Close);
                current = subpath_start;
            }
        }
        previous_control = control;
    }
    // Clipping requires a closed loop (#funcdef-basic-shape-path).
    if !matches!(elements.last(), Some(PathElement::Close)) {
        elements.push(PathElement::Close);
    }
    Some(elements)
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
    /// From `rect()`: the right and bottom edges are floored by the left
    /// and top ones (#funcdef-basic-shape-rect).
    floor: bool,
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

/// `[ round <'border-radius'> ]?` of a `<basic-shape-rect>`.
fn parse_round(parts: &[&str], units: Units, vp: Vp) -> Option<Option<([Len; 4], [Len; 4])>> {
    if parts.is_empty() {
        return Some(None);
    }
    let ["round", parts @ ..] = parts else {
        return None;
    };
    let slash = parts.iter().position(|part| *part == "/");
    let x = lengths(&parts[..slash.unwrap_or(parts.len())], units, vp, true)?;
    let y = if let Some(slash) = slash {
        lengths(&parts[slash + 1..], units, vp, true)?
    } else {
        x.clone()
    };
    Some(Some((x, y)))
}

impl Inset {
    pub(crate) fn parse(value: &str, units: Units, vp: Vp) -> Option<Self> {
        let lower = value.trim().to_ascii_lowercase();
        let mut value = lower.as_str();
        let boxes = [
            "border-box",
            "stroke-box",
            "view-box",
            "padding-box",
            "content-box",
            "fill-box",
            "margin-box",
        ];
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
        let radii = parse_round(&parts[round.unwrap_or(parts.len())..], units, vp)?;
        Some(Self {
            offsets,
            radii,
            floor: false,
        })
    }

    /// `rect( [ <length-percentage> | auto ]{4} [ round … ]? )` as its
    /// equivalent `inset(t calc(100% - r) calc(100% - b) l)`; the right and
    /// bottom edges are floored by the left and top ones when used.
    fn parse_rect(inner: &str, units: Units, vp: Vp) -> Option<Self> {
        let parts = tokens(inner)?;
        let round = parts.iter().position(|part| *part == "round");
        let edges = &parts[..round.unwrap_or(parts.len())];
        let [t, r, b, l] = edges else {
            return None;
        };
        let edge = |text: &str, auto: f32| -> Option<Len> {
            if text == "auto" {
                Some(percent(auto))
            } else {
                shape_length(text, units, vp, false)
            }
        };
        let complement = |len: Len| -> Option<Len> {
            let Len::Val(node) = len else { return None };
            Some(Len::Val(super::value::Node::Sum(
                Box::new(super::value::Node::Lin { k: 1., b: 0. }),
                Box::new(node),
                -1.,
            )))
        };
        let (top, left) = (edge(t, 0.)?, edge(l, 0.)?);
        let (right, bottom) = (complement(edge(r, 1.)?)?, complement(edge(b, 1.)?)?);
        Some(Self {
            offsets: [top, right, bottom, left],
            radii: parse_round(&parts[round.unwrap_or(parts.len())..], units, vp)?,
            floor: true,
        })
    }

    /// `xywh( <length-percentage>{2} <length-percentage [0,∞]>{2} [ round … ]? )`
    /// as `inset(y calc(100% - x - w) calc(100% - y - h) x)`.
    fn parse_xywh(inner: &str, units: Units, vp: Vp) -> Option<Self> {
        let parts = tokens(inner)?;
        let round = parts.iter().position(|part| *part == "round");
        let [x, y, w, h] = &parts[..round.unwrap_or(parts.len())] else {
            return None;
        };
        let value = |text: &str, nonnegative| shape_length(text, units, vp, nonnegative);
        let node = |len: Len| -> Option<super::value::Node> {
            match len {
                Len::Val(node) => Some(node),
                _ => None,
            }
        };
        let (x, y, w, h) = (
            node(value(x, false)?)?,
            node(value(y, false)?)?,
            node(value(w, true)?)?,
            node(value(h, true)?)?,
        );
        let far = |origin: super::value::Node, size: super::value::Node| {
            Len::Val(super::value::Node::Sum(
                Box::new(super::value::Node::Sum(
                    Box::new(super::value::Node::Lin { k: 1., b: 0. }),
                    Box::new(origin),
                    -1.,
                )),
                Box::new(size),
                -1.,
            ))
        };
        Some(Self {
            offsets: [
                Len::Val(y.clone()),
                far(x.clone(), w),
                far(y, h),
                Len::Val(x),
            ],
            radii: parse_round(&parts[round.unwrap_or(parts.len())..], units, vp)?,
            floor: false,
        })
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
        if self.floor {
            // rect(): an edge crossing its opposite edge stops there.
            offsets[1] = offsets[1].min(reference.width - offsets[3]);
            offsets[2] = offsets[2].min(reference.height - offsets[0]);
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
                && !super::value::is_math_function(part)
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

    #[test]
    fn rect_and_xywh_compute_to_insets() {
        // CSS Shapes 1 #basic-shape-computed-values: rect(t r b l) is
        // inset(t calc(100% - r) calc(100% - b) l), and xywh(x y w h) is
        // inset(y calc(100% - x - w) calc(100% - y - h) x).
        let reference = CssRect::new(0., 0., 200., 100.);
        assert_eq!(
            parse("rect(10px 150px 90% auto)").shape(reference),
            Some(PaintShape::Rect(CssRect::new(0., 10., 150., 80.)))
        );
        assert_eq!(
            parse("xywh(20px 10% 50% 30px round 4px)").shape(reference),
            Some(PaintShape::RoundedRect {
                rect: CssRect::new(20., 10., 100., 30.),
                radii: CornerRadii {
                    corners: [(4., 4.); 4]
                },
            })
        );
        // rect(10px 0 0 20px) is rect(10px 20px 10px 20px): empty.
        assert_eq!(
            parse("rect(10px 0 0 20px)").shape(reference),
            Some(PaintShape::Rect(CssRect::new(20., 10., 0., 0.)))
        );
        for invalid in [
            "rect(1px 2px 3px)",
            "xywh(0 0 -1px 10px)",
            "xywh(0 0 1px)",
            "rect(auto auto auto auto round)",
        ] {
            assert!(!supports(invalid), "{invalid}");
        }
    }

    #[test]
    fn path_function_keeps_its_rule_and_closes_the_outline() {
        let reference = CssRect::new(5., 7., 100., 100.);
        let shape = parse(r#"path(evenodd, "M0 0 h10 v10 z M2 2 H8 V8 Z")"#)
            .shape(reference)
            .unwrap();
        let PaintShape::Geometry(geometry) = shape else {
            panic!("even-odd path");
        };
        let (elements, evenodd) = geometry.single_path().unwrap();
        assert!(evenodd);
        assert_eq!(elements[0], PathElement::MoveTo(CssPoint::new(5., 7.)));
        // The hole in the middle is outside under even-odd.
        assert!(!geometry.contains(CssPoint::new(10., 12.)));
        assert!(geometry.contains(CssPoint::new(6., 8.)));
        let PaintShape::Path(open) = parse(r#"path("M 0 0 L 10 0 L 10 10")"#)
            .shape(reference)
            .unwrap()
        else {
            panic!("nonzero path");
        };
        assert_eq!(open.last(), Some(&PathElement::Close));
        for invalid in [
            r#"path("")"#,
            r#"path("L 1 1")"#,
            r#"path("M 0 0 X")"#,
            "path(M0 0)",
        ] {
            assert!(!supports(invalid), "{invalid}");
        }
    }

    #[test]
    fn shape_function_follows_its_commands() {
        let reference = CssRect::new(0., 0., 100., 50.);
        let PaintShape::Path(elements) = parse(
            "shape(from 0 0, hline to 100%, vline by 50%, smooth to 0 100% with 50% 100%, close)",
        )
        .shape(reference)
        .unwrap() else {
            panic!("shape");
        };
        assert_eq!(elements[0], PathElement::MoveTo(CssPoint::new(0., 0.)));
        assert_eq!(elements[1], PathElement::LineTo(CssPoint::new(100., 0.)));
        assert_eq!(elements[2], PathElement::LineTo(CssPoint::new(100., 25.)));
        // smooth after a line reflects nothing: its first control point is
        // the start (#shape-function).
        assert_eq!(
            elements[3],
            PathElement::CurveTo(
                CssPoint::new(100., 25.),
                CssPoint::new(50., 50.),
                CssPoint::new(0., 50.)
            )
        );
        assert_eq!(elements[4], PathElement::Close);
        let PaintShape::Path(arc) = parse("shape(from 0 50%, arc to 100% 50% of 50% cw, close)")
            .shape(reference)
            .unwrap()
        else {
            panic!("arc");
        };
        assert!(arc.iter().any(|e| matches!(e, PathElement::CurveTo(..))));
        assert!(supports(
            "shape(evenodd from 10px 10px, line by 10px 0, curve to 0 0 with 5px 5px / 2px 2px from end)"
        ));
        for invalid in [
            "shape(from 0 0)",
            "shape(0 0, line to 1px 1px)",
            "shape(from 0 0, curve to 1px 1px)",
            "shape(from 0 0, arc to 1px 1px)",
            "shape(from 0 0, jump to 1px 1px)",
        ] {
            assert!(!supports(invalid), "{invalid}");
        }
    }

    #[test]
    fn polygon_round_and_corner_extents() {
        let reference = CssRect::new(0., 0., 100., 100.);
        let PaintShape::Path(rounded) =
            parse("polygon(round 10px, 0 0, 100% 0, 100% 100%, 0 100%)")
                .shape(reference)
                .unwrap()
        else {
            panic!("rounded polygon");
        };
        // A right-angle vertex is tangent 10px along each edge.
        assert_eq!(rounded[0], PathElement::MoveTo(CssPoint::new(0., 10.)));
        assert!(matches!(rounded[1], PathElement::CurveTo(_, _, p) if p == CssPoint::new(10., 0.)));
        let PaintShape::RoundedRect { rect, .. } = parse("circle(farthest-corner at 0 0)")
            .shape(reference)
            .unwrap()
        else {
            panic!("circle");
        };
        assert!((rect.width / 2. - 100f32.hypot(100.)).abs() < 1e-3);
        let PaintShape::RoundedRect { radii, .. } = parse("ellipse(closest-corner at 25% 50%)")
            .shape(reference)
            .unwrap()
        else {
            panic!("ellipse");
        };
        // Side extents 25×50 keep their ratio through the corner (25, 50).
        let (rx, ry) = radii.corners[0];
        assert!(((25. / rx).powi(2) + (50. / ry).powi(2) - 1.).abs() < 1e-3);
        assert!((ry / rx - 2.).abs() < 1e-3);
    }

    #[test]
    fn box_values_and_urls_parse_alone() {
        assert!(supports("margin-box"));
        assert!(supports("fill-box"));
        assert!(supports("url(#clip)"));
        assert!(supports("url(shapes.svg#clip)"));
        assert!(!supports("url(#a) border-box"));
        assert!(!supports("border-box padding-box"));
    }
}
