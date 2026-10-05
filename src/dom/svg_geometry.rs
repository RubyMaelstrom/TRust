//! Geometry behind the SVG 2 `SVGGraphicsElement`, `SVGGeometryElement` and
//! `SVGTextContentElement` DOM interfaces.
//!
//! TRust paints inline SVG by serializing it for usvg (`Dom::svg_image_data`).
//! These queries reuse that geometry engine instead of keeping a second one:
//! each builds a small *geometry document* holding a copy of the queried
//! subtree, parses it with usvg using the inline SVG's font environment, and
//! reads the converted shapes, paths and laid-out text back. The copy keeps
//! what determines geometry (attributes, cascaded declarations, the inherited
//! context of every SVG ancestor, the nearest viewport's size and the resources
//! the subtree references) and drops what must not affect the answer:
//!
//! - SVG 2 coords.html#BoundingBoxes: a bounding box is computed in the
//!   element's user space ("using the element's user coordinate system"), so
//!   the element's own `transform` is not applied. The opacity, visibility,
//!   fill and dash properties have no effect on it, and an element that is not
//!   in the rendering tree (display:none, inside `<defs>`, not in the document)
//!   "still has a bounding box ... as if the element were rendered".
//! - SVG 2 types.html#__svg__SVGGeometryElement__isPointInFill and
//!   #__svg__SVGGeometryElement__isPointInStroke: the result is "independent of
//!   any visual CSS property but" `fill-rule` respectively the stroke geometry
//!   properties, so the copy paints both fill and stroke.
//! - SVG 2 text.html#__svg__SVGTextContentElement__getSubStringLength: lengths
//!   are sums of typographic character advances, without `x`/`y`/`dx`/`dy`
//!   positioning, so the copy drops those attributes.
//!
//! Text content children (`tspan`, `textPath`) are only laid out inside their
//! `text` element. Their queries copy the whole `text` with every other
//! character hidden: usvg excludes `visibility: hidden` clusters from the text
//! bounding box and layout output while keeping the layout of the rest.

use std::borrow::Cow;
use std::collections::HashSet;

use html5ever::ns;
use resvg::usvg;
use vello_cpu::kurbo::{
    self, Affine, BezPath, ParamCurve, ParamCurveArclen, ParamCurveNearest, PathEl, Point, Rect,
    Shape,
};

use super::{
    DOCUMENT, Dom, NodeData, NodeId, SVG_PRESENTATION_PROPERTIES, SvgImageRefs, escape_attr,
    escape_text,
};

const SVG_NS: &str = "http://www.w3.org/2000/svg";
/// The id of the group that holds the copied subtree in a geometry document.
const TARGET_ID: &str = "trust-svg-geometry-target";
/// Referenced resources copied into one geometry document, bounding the work a
/// reference chain can cause.
const MAX_RESOURCES: usize = 64;
const MAX_RESOURCE_BYTES: usize = 4 << 20;
/// Accuracy (user units) of arc-length measurement along curves.
const ARC_LENGTH_ACCURACY: f64 = 1e-6;
/// Distance (user units) within which a point counts as lying on a path.
const ON_PATH_TOLERANCE: f64 = 1e-6;

/// Inherited properties that can change an element's geometry, stroke shape
/// or text layout (SVG 2 property index and CSS Fonts/Text/Writing Modes). The
/// SVG ancestors of a copied subtree contribute these, as presentation
/// attributes or cascaded declarations, through one wrapper group each.
const INHERITED_GEOMETRY_PROPERTIES: &[&str] = &[
    "font",
    "font-family",
    "font-size",
    "font-size-adjust",
    "font-stretch",
    "font-style",
    "font-variant",
    "font-variant-caps",
    "font-variant-east-asian",
    "font-variant-ligatures",
    "font-variant-numeric",
    "font-weight",
    "font-kerning",
    "font-feature-settings",
    "font-variation-settings",
    "font-optical-sizing",
    "letter-spacing",
    "word-spacing",
    "text-anchor",
    "direction",
    "writing-mode",
    "text-orientation",
    "glyph-orientation-vertical",
    "white-space",
    "white-space-collapse",
    "dominant-baseline",
    "text-rendering",
    "fill",
    "fill-rule",
    "stroke",
    "stroke-width",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-dasharray",
    "stroke-dashoffset",
    "paint-order",
    "clip-rule",
    "marker",
    "marker-start",
    "marker-mid",
    "marker-end",
];

/// CSS properties an outermost `<svg>` inherits from its HTML ancestors that
/// affect text layout. Paint inheritance is `Dom::svg_inherited_paint_style`.
const HTML_INHERITED_TEXT_PROPERTIES: &[&str] = &[
    "font-family",
    "font-size",
    "font-stretch",
    "font-style",
    "font-variant",
    "font-weight",
    "letter-spacing",
    "word-spacing",
    "direction",
    "writing-mode",
    "white-space",
];

/// The `SVGBoundingBoxOptions` dictionary (SVG 2 types.html#SVGBoundingBoxOptions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BoundingBoxOptions {
    pub(crate) fill: bool,
    pub(crate) stroke: bool,
    pub(crate) markers: bool,
    pub(crate) clipped: bool,
}

impl BoundingBoxOptions {
    /// The dictionary defaults: the object bounding box.
    pub(crate) const OBJECT: Self = Self {
        fill: true,
        stroke: false,
        markers: false,
        clipped: false,
    };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    BoundingBox(BoundingBoxOptions),
    Path,
    Text,
}

impl Purpose {
    fn markers(self) -> bool {
        matches!(self, Self::BoundingBox(options) if options.markers)
    }

    fn clipped(self) -> bool {
        matches!(self, Self::BoundingBox(options) if options.clipped)
    }
}

#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
    Other,
}

/// The equivalent path of a geometry element with the properties its hit
/// tests use, in the element's user space.
pub(crate) struct GeometryPath {
    path: BezPath,
    even_odd: bool,
    stroke: Option<kurbo::Stroke>,
}

impl GeometryPath {
    fn empty() -> Self {
        Self {
            path: BezPath::new(),
            even_odd: false,
            stroke: None,
        }
    }

    /// SVG 2 #__svg__SVGGeometryElement__getTotalLength: the user agent's
    /// computed length of the path in user units.
    pub(crate) fn total_length(&self) -> f64 {
        self.path
            .segments()
            .map(|segment| segment.arclen(ARC_LENGTH_ACCURACY))
            .sum()
    }

    /// SVG 2 #__svg__SVGGeometryElement__getPointAtLength steps 2-4: clamp
    /// `distance` to [0, length] and return the point at that distance.
    pub(crate) fn point_at_length(&self, distance: f64) -> (f64, f64) {
        let total = self.total_length();
        let mut remaining = distance.clamp(0.0, total);
        let mut last = None;
        for segment in self.path.segments() {
            let length = segment.arclen(ARC_LENGTH_ACCURACY);
            if remaining <= length {
                let t = segment.inv_arclen(remaining, ARC_LENGTH_ACCURACY);
                let point = segment.eval(t);
                return (point.x, point.y);
            }
            remaining -= length;
            last = Some(segment.end());
        }
        // An empty path is positioned at its first moveto, or at (0, 0)
        // (SVG 2 coords.html#BoundingBoxes on elements with no position).
        let point = last
            .or_else(|| first_point(&self.path))
            .unwrap_or(Point::ZERO);
        (point.x, point.y)
    }

    /// SVG 2 #__svg__SVGGeometryElement__isPointInFill: inside per `fill-rule`
    /// with open subpaths implicitly closed; points on the path are inside.
    pub(crate) fn contains_in_fill(&self, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let point = Point::new(x, y);
        let closed = implicitly_closed(&self.path);
        let winding = closed.winding(point);
        let inside = if self.even_odd {
            winding % 2 != 0
        } else {
            winding != 0
        };
        inside || near_path(&closed, point)
    }

    /// SVG 2 #__svg__SVGGeometryElement__isPointInStroke: in or on the stroke
    /// outline computed from the stroke width, caps, joins, miter limit and
    /// dash pattern (painting.html#StrokeShape).
    pub(crate) fn contains_in_stroke(&self, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let Some(stroke) = &self.stroke else {
            return false;
        };
        let outline = kurbo::stroke(
            self.path.iter(),
            stroke,
            &kurbo::StrokeOpts::default(),
            0.01,
        );
        let point = Point::new(x, y);
        outline.winding(point) != 0 || near_path(&outline, point)
    }
}

fn first_point(path: &BezPath) -> Option<Point> {
    path.elements().iter().find_map(|element| match element {
        PathEl::MoveTo(point) => Some(*point),
        _ => None,
    })
}

/// A copy of `path` with each open subpath closed, as filling requires.
fn implicitly_closed(path: &BezPath) -> BezPath {
    let mut closed = BezPath::new();
    let mut open = false;
    for element in path.elements() {
        match element {
            PathEl::MoveTo(_) => {
                if open {
                    closed.close_path();
                }
                open = true;
            }
            PathEl::ClosePath => open = false,
            _ => {}
        }
        closed.push(*element);
    }
    if open {
        closed.close_path();
    }
    closed
}

fn near_path(path: &BezPath, point: Point) -> bool {
    let tolerance = ON_PATH_TOLERANCE * ON_PATH_TOLERANCE;
    path.segments()
        .any(|segment| segment.nearest(point, ARC_LENGTH_ACCURACY).distance_sq <= tolerance)
        || path.elements().iter().any(|element| {
            matches!(element, PathEl::MoveTo(start) if start.distance_squared(point) <= tolerance)
        })
}

/// Typographic characters of a text content element, in logical order, with
/// the index of each one's first addressable character (UTF-16 code units).
pub(crate) struct TextMetrics {
    characters: u32,
    clusters: Vec<(u32, f64)>,
}

impl TextMetrics {
    fn empty() -> Self {
        Self {
            characters: 0,
            clusters: Vec::new(),
        }
    }

    /// SVG 2 #__svg__SVGTextContentElement__getNumberOfChars.
    pub(crate) fn number_of_chars(&self) -> u32 {
        self.characters
    }

    /// SVG 2 #__svg__SVGTextContentElement__getComputedTextLength: the
    /// substring length of every addressable character.
    pub(crate) fn computed_length(&self) -> f64 {
        self.substring_length(0, self.characters).unwrap_or(0.0)
    }

    /// SVG 2 #__svg__SVGTextContentElement__getSubStringLength: the advances
    /// of the typographic characters whose first addressable character lies in
    /// `charnum..charnum + nchars`. `None` when `charnum` exceeds the highest
    /// index (IndexSizeError).
    pub(crate) fn substring_length(&self, charnum: u32, nchars: u32) -> Option<f64> {
        if self.characters == 0 && charnum == 0 && nchars == 0 {
            return Some(0.0);
        }
        if charnum >= self.characters {
            return None;
        }
        let end = u64::from(charnum) + u64::from(nchars);
        Some(
            self.clusters
                .iter()
                .filter(|(first, _)| *first >= charnum && u64::from(*first) < end)
                .map(|(_, advance)| advance)
                .sum(),
        )
    }
}

fn utf16_len(text: &str) -> u32 {
    u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX)
}

impl Dom {
    fn is_svg_element(&self, id: NodeId) -> bool {
        self.namespace_uri(id) == Some(SVG_NS)
    }

    fn svg_parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id]
            .parent
            .filter(|&parent| self.is_svg_element(parent))
    }

    /// The outermost `svg` element of `id`'s SVG document fragment (struct.html
    /// #TermOutermostSVGElement), or `None` outside any `svg` element.
    pub(crate) fn svg_outermost(&self, id: NodeId) -> Option<NodeId> {
        let mut outermost = None;
        let mut current = Some(id).filter(|&id| self.is_svg_element(id));
        while let Some(node) = current {
            if self.tag_name(node) == Some("svg") {
                outermost = Some(node);
            }
            current = self.svg_parent(node);
        }
        outermost
    }

    /// The nearest ancestor `svg` element: the element establishing the SVG
    /// viewport whose user space resolves `id`'s percentage lengths.
    fn svg_nearest_viewport(&self, id: NodeId) -> Option<NodeId> {
        let mut current = self.svg_parent(id);
        while let Some(node) = current {
            if self.tag_name(node) == Some("svg") {
                return Some(node);
            }
            current = self.svg_parent(node);
        }
        None
    }

    /// The `text` element whose layout contains a text content child.
    fn svg_text_root(&self, id: NodeId) -> Option<NodeId> {
        let mut current = self.svg_parent(id);
        while let Some(node) = current {
            if self.tag_name(node) == Some("text") {
                return Some(node);
            }
            current = self.svg_parent(node);
        }
        None
    }

    fn svg_font_size(&self, id: NodeId) -> f64 {
        f64::from(self.font_px(id)).max(0.0)
    }

    /// Resolve an SVG `<length>` or `<percentage>` to user units (SVG 2
    /// coords.html#Units). Percentages refer to the nearest viewport's user
    /// space: its width, its height, or its normalized diagonal.
    fn svg_length(
        &self,
        id: NodeId,
        value: &str,
        axis: Axis,
        reference: (f64, f64),
    ) -> Option<f64> {
        use svgtypes::LengthUnit;
        let length: svgtypes::Length = value.trim().parse().ok()?;
        let number = length.number;
        let value = match length.unit {
            LengthUnit::None | LengthUnit::Px => number,
            LengthUnit::Em => number * self.svg_font_size(id),
            LengthUnit::Ex => number * self.svg_font_size(id) / 2.0,
            LengthUnit::In => number * 96.0,
            LengthUnit::Cm => number * 96.0 / 2.54,
            LengthUnit::Mm => number * 96.0 / 25.4,
            LengthUnit::Pt => number * 4.0 / 3.0,
            LengthUnit::Pc => number * 16.0,
            LengthUnit::Percent => {
                let (width, height) = reference;
                number / 100.0
                    * match axis {
                        Axis::X => width,
                        Axis::Y => height,
                        Axis::Other => ((width * width + height * height) / 2.0).sqrt(),
                    }
            }
        };
        value.is_finite().then_some(value)
    }

    fn svg_length_attr(
        &self,
        id: NodeId,
        name: &str,
        default: f64,
        axis: Axis,
        reference: (f64, f64),
    ) -> f64 {
        self.attr(id, name)
            .and_then(|value| self.svg_length(id, value, axis, reference))
            .unwrap_or(default)
    }

    fn svg_view_box(&self, id: NodeId) -> Option<svgtypes::ViewBox> {
        let view_box: svgtypes::ViewBox = self.attr(id, "viewBox")?.parse().ok()?;
        (view_box.w > 0.0 && view_box.h > 0.0).then_some(view_box)
    }

    /// The definite size of an outermost `svg` element's viewport from its
    /// cascaded `width`/`height` (which include the presentation attributes,
    /// SVG 2 §8.12), or `None` when one of them needs layout.
    pub(crate) fn svg_definite_viewport(&self, svg: NodeId) -> Option<(f64, f64)> {
        let units = crate::layout2::Units::of(self, svg);
        let viewport = self.viewport_px();
        let dimension = |property: &str| {
            let value = self.computed_value_resolved(svg, property)?;
            let resolved = crate::layout2::svg_resource_dimension(&value, units, viewport)?;
            resolved.parse::<f64>().ok().filter(|px| px.is_finite())
        };
        Some((dimension("width")?, dimension("height")?))
    }

    /// The size of the SVG viewport `svg` establishes. `outer` is the
    /// outermost `svg` element's viewport, which CSS layout determines.
    fn svg_viewport_size(&self, svg: NodeId, outer: (f64, f64)) -> (f64, f64) {
        let Some(parent_viewport) = self.svg_nearest_viewport(svg) else {
            return outer;
        };
        let reference = self.svg_user_space_size(parent_viewport, outer);
        (
            self.svg_length_attr(svg, "width", reference.0, Axis::X, reference),
            self.svg_length_attr(svg, "height", reference.1, Axis::Y, reference),
        )
    }

    /// The size of the user space `svg` establishes for its children: its
    /// `viewBox` when it has one, otherwise its viewport.
    fn svg_user_space_size(&self, svg: NodeId, outer: (f64, f64)) -> (f64, f64) {
        match self.svg_view_box(svg) {
            Some(view_box) => (view_box.w, view_box.h),
            None => self.svg_viewport_size(svg, outer),
        }
    }

    /// SVG 2 coords.html#ComputingAViewportsTransform for an `svg` element.
    fn svg_view_box_transform(&self, svg: NodeId, viewport: (f64, f64)) -> Affine {
        let Some(view_box) = self.svg_view_box(svg) else {
            return Affine::IDENTITY;
        };
        let aspect: svgtypes::AspectRatio = self
            .attr(svg, "preserveAspectRatio")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        view_box_transform(view_box, aspect, viewport)
    }

    /// The transform an element's `transform` property applies to its user
    /// space. A cascaded CSS declaration wins over the presentation attribute
    /// (SVG 2 coords.html#TransformProperty); CSS `px`/`deg` units are
    /// accepted in the SVG transform syntax that the paint path also uses.
    fn svg_element_transform(&self, id: NodeId) -> Affine {
        let css = self
            .cascaded(id, "transform")
            .map(|value| self.resolve_vars(id, &value))
            .filter(|value| !value.trim().eq_ignore_ascii_case("none"))
            .and_then(|value| parse_svg_transform(&value.replace("px", "").replace("deg", "")));
        css.or_else(|| self.attr(id, "transform").and_then(parse_svg_transform))
            .unwrap_or(Affine::IDENTITY)
    }

    /// The transform from the coordinate space of one element to that of its
    /// parent. For an `svg` element this is the viewport transform: its
    /// `x`/`y` position when nested, then its `viewBox` mapping.
    fn svg_local_coordinate_transform(&self, id: NodeId, outer: (f64, f64)) -> Affine {
        match self.tag_name(id) {
            Some("svg") => {
                let viewport = self.svg_viewport_size(id, outer);
                let position = match self.svg_nearest_viewport(id) {
                    Some(parent) => {
                        let reference = self.svg_user_space_size(parent, outer);
                        Affine::translate((
                            self.svg_length_attr(id, "x", 0.0, Axis::X, reference),
                            self.svg_length_attr(id, "y", 0.0, Axis::Y, reference),
                        ))
                    }
                    None => Affine::IDENTITY,
                };
                position * self.svg_view_box_transform(id, viewport)
            }
            _ => self.svg_element_transform(id),
        }
    }

    /// SVG 2 types.html#__svg__SVGGraphicsElement__getCTM (`screen` false):
    /// the matrix from `id`'s coordinate space to its nearest viewport's,
    /// including that viewport's `viewBox` transform; for the outermost `svg`
    /// element, its own `viewBox` transform. With `screen` true
    /// (#__svg__SVGGraphicsElement__getScreenCTM) the matrix continues to the
    /// outermost `svg` element's viewport; the caller adds the CSS box offset.
    /// `None` for a node not in the document.
    pub(crate) fn svg_ctm(&self, id: NodeId, screen: bool, outer: (f64, f64)) -> Option<[f64; 6]> {
        if !self.is_svg_element(id) || !self.is_connected(id) {
            return None;
        }
        let mut ctm = Affine::IDENTITY;
        let mut current = Some(id);
        while let Some(node) = current {
            ctm = self.svg_local_coordinate_transform(node, outer) * ctm;
            if !screen
                && node != id
                && matches!(
                    self.tag_name(node),
                    Some("svg" | "symbol" | "foreignObject")
                )
            {
                break;
            }
            current = self.svg_parent(node);
        }
        let coefficients = ctm.as_coeffs();
        coefficients
            .iter()
            .all(|value| value.is_finite())
            .then_some(coefficients)
    }

    /// The font environment the inline SVG containing `id` lays text out with.
    fn svg_geometry_fonts(&self, id: NodeId) -> u64 {
        self.scope_font_set(id)
            .map_or_else(crate::font_system::page_svg_font_environment, |set| {
                set.svg_font_environment()
            })
    }

    fn svg_geometry_tree(
        &self,
        id: NodeId,
        purpose: Purpose,
        outer: (f64, f64),
    ) -> Option<std::rc::Rc<usvg::Tree>> {
        let markup = self.svg_geometry_document(id, purpose, outer)?;
        crate::img::document_svg_geometry_tree(&markup, self.svg_geometry_fonts(id))
    }

    /// SVG 2 types.html#__svg__SVGGraphicsElement__getBBox: the bounding box
    /// algorithm (coords.html#BoundingBoxes) in `id`'s user space, as
    /// `[x, y, width, height]`.
    pub(crate) fn svg_bbox(
        &self,
        id: NodeId,
        options: BoundingBoxOptions,
        outer: (f64, f64),
    ) -> [f64; 4] {
        let tag = self.tag_name(id).unwrap_or("");
        let computed = self
            .svg_geometry_tree(id, Purpose::BoundingBox(options), outer)
            .and_then(|tree| match tree.node_by_id(TARGET_ID) {
                Some(usvg::Node::Group(group)) => {
                    group_box(group, tree.fontdb(), usvg::Transform::identity(), options)
                }
                _ => None,
            });
        match computed {
            Some(rect) => [rect.x0, rect.y0, rect.width(), rect.height()],
            // Only the fill part has a position without area.
            None if options.fill => self.svg_degenerate_bbox(id, tag, outer),
            None => [0.0; 4],
        }
    }

    /// The bounding box of an element usvg does not convert because its
    /// geometry has no area: a zero-sized rect or circle still "has a bounding
    /// box, with a positive value for the positive dimension", and a `use` of
    /// an unresolved reference is positioned at its `x`/`y`. Invalid
    /// (negative) dimensions give the zero rectangle.
    fn svg_degenerate_bbox(&self, id: NodeId, tag: &str, outer: (f64, f64)) -> [f64; 4] {
        let reference = self
            .svg_nearest_viewport(id)
            .map_or(outer, |viewport| self.svg_user_space_size(viewport, outer));
        let length = |name: &str, axis| self.svg_length_attr(id, name, 0.0, axis, reference);
        let zero = [0.0; 4];
        match tag {
            "rect" | "image" | "foreignObject" => {
                let (width, height) = (length("width", Axis::X), length("height", Axis::Y));
                if width < 0.0 || height < 0.0 {
                    zero
                } else {
                    [length("x", Axis::X), length("y", Axis::Y), width, height]
                }
            }
            "circle" | "ellipse" => {
                let (rx, ry) = if tag == "circle" {
                    let r = length("r", Axis::Other);
                    (r, r)
                } else {
                    (length("rx", Axis::X), length("ry", Axis::Y))
                };
                if rx < 0.0 || ry < 0.0 {
                    zero
                } else {
                    let (cx, cy) = (length("cx", Axis::X), length("cy", Axis::Y));
                    [cx - rx, cy - ry, 2.0 * rx, 2.0 * ry]
                }
            }
            "line" => {
                let (x1, y1) = (length("x1", Axis::X), length("y1", Axis::Y));
                let (x2, y2) = (length("x2", Axis::X), length("y2", Axis::Y));
                [x1.min(x2), y1.min(y2), (x2 - x1).abs(), (y2 - y1).abs()]
            }
            "path" => self
                .attr(id, "d")
                .and_then(|d| match svgtypes::SimplifyingPathParser::from(d).next() {
                    Some(Ok(svgtypes::SimplePathSegment::MoveTo { x, y })) => {
                        Some([x, y, 0.0, 0.0])
                    }
                    _ => None,
                })
                .unwrap_or(zero),
            "use" => [length("x", Axis::X), length("y", Axis::Y), 0.0, 0.0],
            _ => zero,
        }
    }

    /// The equivalent path of a geometry element (SVG 2 paths.html and
    /// shapes.html#ShapeElements), as usvg converts it for painting.
    pub(crate) fn svg_geometry_path(&self, id: NodeId, outer: (f64, f64)) -> GeometryPath {
        let Some(tree) = self.svg_geometry_tree(id, Purpose::Path, outer) else {
            return GeometryPath::empty();
        };
        let Some(usvg::Node::Group(group)) = tree.node_by_id(TARGET_ID) else {
            return GeometryPath::empty();
        };
        let Some((path, transform)) = first_path(group, usvg::Transform::identity()) else {
            return GeometryPath::empty();
        };
        let affine = to_affine(transform);
        let mut bez = BezPath::new();
        for segment in path.data().segments() {
            use usvg::tiny_skia_path::PathSegment;
            let point = |p: usvg::tiny_skia_path::Point| {
                affine * Point::new(f64::from(p.x), f64::from(p.y))
            };
            match segment {
                PathSegment::MoveTo(p) => bez.move_to(point(p)),
                PathSegment::LineTo(p) => bez.line_to(point(p)),
                PathSegment::QuadTo(p1, p) => bez.quad_to(point(p1), point(p)),
                PathSegment::CubicTo(p1, p2, p) => bez.curve_to(point(p1), point(p2), point(p)),
                PathSegment::Close => bez.close_path(),
            }
        }
        let even_odd = path
            .fill()
            .is_some_and(|fill| fill.rule() == usvg::FillRule::EvenOdd);
        let stroke = path.stroke().map(|stroke| {
            let cap = match stroke.linecap() {
                usvg::LineCap::Butt => kurbo::Cap::Butt,
                usvg::LineCap::Round => kurbo::Cap::Round,
                usvg::LineCap::Square => kurbo::Cap::Square,
            };
            let join = match stroke.linejoin() {
                usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => kurbo::Join::Miter,
                usvg::LineJoin::Round => kurbo::Join::Round,
                usvg::LineJoin::Bevel => kurbo::Join::Bevel,
            };
            let mut result = kurbo::Stroke::new(f64::from(stroke.width().get()))
                .with_caps(cap)
                .with_join(join)
                .with_miter_limit(f64::from(stroke.miterlimit().get()));
            if let Some(dashes) = stroke.dasharray() {
                result = result.with_dashes(
                    f64::from(stroke.dashoffset()),
                    dashes.iter().map(|dash| f64::from(*dash)),
                );
            }
            result
        });
        GeometryPath {
            path: bez,
            even_odd,
            stroke,
        }
    }

    /// Whether `id` and its ancestors are rendered: in the document and not
    /// `display: none`, whether by the cascade or by the SVG presentation
    /// attribute, which only an author declaration overrides (SVG 2 §6.6).
    fn svg_is_rendered(&self, id: NodeId) -> bool {
        let hidden = |node: NodeId| {
            self.is_hidden(node)
                || (self.is_svg_element(node)
                    && self.cascaded(node, "display").is_none()
                    && self
                        .attr(node, "display")
                        .is_some_and(|value| value.trim().eq_ignore_ascii_case("none")))
        };
        self.is_connected(id)
            && std::iter::successors(Some(id), |&node| self.nodes[node].parent)
                .take_while(|&node| node != DOCUMENT)
                .filter(|&node| matches!(self.nodes[node].data, NodeData::Element { .. }))
                .all(|node| !hidden(node))
    }

    /// The typographic characters of a text content element (SVG 2
    /// text.html#InterfaceSVGTextContentElement). An element that is not
    /// rendered has no addressable characters.
    pub(crate) fn svg_text_metrics(&self, id: NodeId, outer: (f64, f64)) -> TextMetrics {
        if !self.svg_is_rendered(id) {
            return TextMetrics::empty();
        }
        let Some(tree) = self.svg_geometry_tree(id, Purpose::Text, outer) else {
            return TextMetrics::empty();
        };
        let Some(usvg::Node::Group(group)) = tree.node_by_id(TARGET_ID) else {
            return TextMetrics::empty();
        };
        let Some(text) = first_text(group) else {
            return TextMetrics::empty();
        };
        text_metrics(text, tree.fontdb())
    }

    /// Serialize the geometry document for `target` (see the module comment).
    fn svg_geometry_document(
        &self,
        target: NodeId,
        purpose: Purpose,
        outer: (f64, f64),
    ) -> Option<String> {
        if !self.is_svg_element(target) {
            return None;
        }
        let tag = self.tag_name(target)?;
        let text_root = matches!(tag, "tspan" | "textPath" | "tref" | "a")
            .then(|| self.svg_text_root(target))
            .flatten();
        // An `svg` element's user space is the one inside its viewport: copy
        // its children, with the element itself as their inherited context.
        let children_only = tag == "svg";
        let copy_root = text_root.unwrap_or(target);
        let viewport = if children_only {
            Some(target)
        } else {
            self.svg_nearest_viewport(copy_root)
        };
        let (width, height) = viewport.map_or(outer, |svg| self.svg_user_space_size(svg, outer));
        let (width, height) = (positive_size(width), positive_size(height));

        let mut chain = Vec::new();
        let mut current = self.svg_parent(copy_root);
        while let Some(node) = current {
            chain.push(node);
            current = self.svg_parent(node);
        }
        chain.reverse();
        if children_only {
            chain.push(target);
        }
        let html_context = chain
            .first()
            .copied()
            .or(Some(copy_root))
            .and_then(|top| Some((top, self.nodes[top].parent?)))
            .filter(|&(_, parent)| matches!(self.nodes[parent].data, NodeData::Element { .. }));

        let mut forced = HashSet::new();
        let mut node = Some(target);
        while let Some(current) = node {
            forced.insert(current);
            if current == copy_root {
                break;
            }
            node = self.svg_parent(current);
        }
        let copy = GeometryCopy {
            target,
            purpose,
            forced,
            hide_others: text_root.is_some(),
        };

        let mut body = String::new();
        for &ancestor in &chain {
            body.push_str("<g");
            self.write_svg_inherited_context(ancestor, purpose, &mut body);
            body.push('>');
        }
        body.push_str("<g id=\"");
        body.push_str(TARGET_ID);
        body.push_str("\" style=\"visibility:");
        body.push_str(if copy.hide_others {
            "hidden"
        } else {
            "visible"
        });
        body.push_str("\">");
        if children_only {
            for child in self.child_iter(target) {
                self.write_svg_geometry_copy(child, &copy, false, &mut body);
            }
        } else {
            self.write_svg_geometry_copy(copy_root, &copy, false, &mut body);
        }
        body.push_str("</g>");
        for _ in &chain {
            body.push_str("</g>");
        }

        let mut root_style = String::new();
        if let Some((top, parent)) = html_context {
            for &property in HTML_INHERITED_TEXT_PROPERTIES {
                if let Some(value) = self.computed_value_resolved(parent, property)
                    && !value.trim().is_empty()
                    && !value.contains([';', '"', '<'])
                {
                    root_style.push_str(property);
                    root_style.push(':');
                    root_style.push_str(value.trim());
                    root_style.push(';');
                }
            }
            if self.tag_name(top) == Some("svg") {
                let color = self.svg_used_color(top);
                root_style.push_str(&self.svg_inherited_paint_style(top, &color));
            }
        }
        let defs = self.svg_geometry_resources(copy_root, &body);
        Some(format!(
            "<svg xmlns=\"{SVG_NS}\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
             width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\" \
             style=\"{}\"><defs>{defs}</defs>{body}</svg>",
            escape_attr(&root_style)
        ))
    }

    /// One wrapper group's attributes: the inherited geometry properties an
    /// SVG ancestor of the copied subtree specifies, as presentation
    /// attributes and as cascaded declarations.
    fn write_svg_inherited_context(&self, id: NodeId, purpose: Purpose, out: &mut String) {
        let NodeData::Element { attrs, .. } = &self.nodes[id].data else {
            return;
        };
        let keep = |name: &str| {
            INHERITED_GEOMETRY_PROPERTIES.contains(&name)
                && (purpose.markers() || !name.starts_with("marker"))
        };
        for attr in attrs {
            let local: &str = &attr.name.local;
            if attr.name.ns == ns!(xml) && local == "space" {
                write_attr(out, "xml:space", &attr.value);
            } else if attr.name.ns == ns!() && keep(local) {
                write_attr(out, local, &self.resolve_vars(id, &attr.value));
            }
        }
        let style = self
            .svg_element_declarations(id)
            .into_iter()
            .filter(|(name, _)| keep(name))
            .fold(String::new(), |mut style, (name, value)| {
                push_declaration(&mut style, &name, &value);
                style
            });
        if !style.is_empty() {
            write_attr(out, "style", &style);
        }
    }

    /// The declarations an element contributes for its own rendering, as the
    /// image serializer bakes them: the authored `style` attribute, then the
    /// cascaded box/font properties and SVG presentation property winners.
    fn svg_element_declarations(&self, id: NodeId) -> Vec<(String, String)> {
        let mut style = String::new();
        if let Some(authored) = self.attr(id, "style") {
            style.push_str(authored);
            style.push(';');
        }
        style.push_str(&self.baked_element_style(id, false));
        style.push(';');
        style.push_str(&self.svg_resource_style(id));
        style_declarations(&style)
            .into_iter()
            .map(|(name, value)| {
                let value = self.resolve_vars(id, &value);
                (name, value)
            })
            .collect()
    }

    /// Serialize one node of the copied subtree for a geometry document.
    fn write_svg_geometry_copy(
        &self,
        id: NodeId,
        copy: &GeometryCopy,
        in_text: bool,
        out: &mut String,
    ) {
        let (name, attrs) = match &self.nodes[id].data {
            NodeData::Text(text) | NodeData::CData(text) => {
                out.push_str(&escape_text(&xml_chars(text)));
                return;
            }
            NodeData::Element { name, attrs, .. } => (name, attrs),
            _ => return,
        };
        if name.ns != ns!(svg) {
            return;
        }
        let tag: &str = &name.local;
        if !xml_name(tag)
            || matches!(
                tag,
                "script" | "style" | "title" | "desc" | "metadata" | "animate" | "set"
            )
        {
            return;
        }
        let is_target = id == copy.target;
        // The queried element and the ancestors copied with it answer as if
        // rendered; any other `display: none` element is not rendered and so
        // does not contribute (coords.html#BoundingBoxes).
        let forced = copy.forced.contains(&id);
        if !forced && self.is_hidden(id) {
            return;
        }
        let text_content = in_text || tag == "text";
        // SVG 2 coords.html#BoundingBoxes: an image or foreignObject is its
        // positioning rectangle, which a rect with the same geometric
        // properties reproduces without fetching or laying anything out.
        let stand_in = matches!(tag, "image" | "foreignObject");
        let element = if stand_in {
            "rect"
        } else if tag == "a" && !in_text {
            "g"
        } else {
            tag
        };
        let keep_attr = |local: &str| -> bool {
            if stand_in {
                return match local {
                    "id" | "x" | "y" | "width" | "height" => true,
                    "transform" => !is_target,
                    "display" => !forced,
                    "clip-path" => copy.purpose.clipped(),
                    _ => false,
                };
            }
            if local.starts_with("on") {
                return false;
            }
            match local {
                "style" | "class" | "visibility" | "opacity" | "filter" | "mask" => false,
                "display" => !forced,
                "transform" => !is_target,
                "clip-path" => copy.purpose.clipped(),
                "marker" | "marker-start" | "marker-mid" | "marker-end" => copy.purpose.markers(),
                "x" | "y" | "dx" | "dy" | "rotate" => {
                    !(copy.purpose == Purpose::Text && matches!(tag, "text" | "tspan" | "tref"))
                }
                _ => true,
            }
        };
        out.push('<');
        out.push_str(element);
        for attr in attrs {
            let local: &str = &attr.name.local;
            if attr.name.ns == ns!(xml) && local == "space" {
                write_attr(out, "xml:space", &attr.value);
                continue;
            }
            if !xml_name(local) || !keep_attr(local) {
                continue;
            }
            // `href` is the local name of both `href` and the legacy
            // `xlink:href` (SVG 2 linking.html#XLinkRefAttrs).
            let value = if SVG_PRESENTATION_PROPERTIES.contains(&local) {
                self.resolve_vars(id, &attr.value)
            } else {
                attr.value.to_string()
            };
            write_attr(out, local, &value);
        }
        let mut style = String::new();
        if !stand_in {
            for (property, value) in self.svg_element_declarations(id) {
                let keep = match property.as_str() {
                    "visibility" | "opacity" | "filter" | "mask" => false,
                    "display" => !forced,
                    "transform" | "transform-origin" | "transform-box" | "translate" | "rotate"
                    | "scale" => !is_target,
                    "clip-path" => copy.purpose.clipped(),
                    "marker" | "marker-start" | "marker-mid" | "marker-end" => {
                        copy.purpose.markers()
                    }
                    _ => true,
                };
                if keep {
                    push_declaration(&mut style, &property, &value);
                }
            }
        }
        if is_target {
            if copy.hide_others {
                push_declaration(&mut style, "visibility", "visible");
            }
            if copy.purpose == Purpose::Path {
                push_declaration(&mut style, "fill", "#000");
                push_declaration(&mut style, "stroke", "#000");
            }
        }
        if !style.is_empty() {
            write_attr(out, "style", &style);
        }
        out.push('>');
        if !stand_in {
            for child in self.child_iter(id) {
                self.write_svg_geometry_copy(child, copy, text_content, out);
            }
        }
        out.push_str("</");
        out.push_str(element);
        out.push('>');
    }

    /// The elements a geometry document's copy references by fragment
    /// (`href` targets and `url(#…)` paint servers, markers and clip paths),
    /// transitively, serialized as the image pipeline paints them. They are
    /// placed in `<defs>`, where usvg resolves but does not render them.
    fn svg_geometry_resources(&self, scope_node: NodeId, body: &str) -> String {
        let scope = self.tree_scope(scope_node);
        let mut queue = std::collections::VecDeque::new();
        collect_fragment_references(body, &mut queue);
        let mut seen = HashSet::new();
        let mut defs = String::new();
        while let Some(fragment) = queue.pop_front() {
            if seen.len() >= MAX_RESOURCES {
                break;
            }
            if !seen.insert(fragment.clone()) {
                continue;
            }
            let Some(node) = self.descendants(scope).find(|&node| {
                self.is_svg_element(node) && self.attr(node, "id") == Some(fragment.as_str())
            }) else {
                continue;
            };
            let markup = self.serialize_svg_for_image_with(
                node,
                &mut SvgImageRefs {
                    page: None,
                    urls: Vec::new(),
                },
            );
            if defs.len() + markup.len() > MAX_RESOURCE_BYTES {
                break;
            }
            collect_fragment_references(&markup, &mut queue);
            defs.push_str(&markup);
        }
        defs
    }
}

struct GeometryCopy {
    target: NodeId,
    purpose: Purpose,
    /// The target and its ancestors up to the copy root: copied even when
    /// `display: none`, since the queries answer as if they were rendered.
    forced: HashSet<NodeId>,
    /// Hide every character but the target's (a text content child).
    hide_others: bool,
}

fn positive_size(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        // usvg requires a positive viewport; percentages of it stay ~0.
        1e-3
    }
}

fn write_attr(out: &mut String, name: &str, value: &str) {
    out.push(' ');
    out.push_str(name);
    out.push_str("=\"");
    out.push_str(&escape_attr(&xml_chars(value)));
    out.push('"');
}

/// Whether HTML parsing produced a name that is also an XML name without a
/// prefix (XML 1.0 §2.3 `Name`, restricted to the characters SVG uses). Others,
/// such as a framework's `:class` or `@click`, cannot affect SVG geometry.
fn xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Text without the characters XML 1.0 §2.2 excludes from documents.
fn xml_chars(text: &str) -> Cow<'_, str> {
    let excluded = |c: char| {
        (c < ' ' && !matches!(c, '\t' | '\n' | '\r')) || c == '\u{fffe}' || c == '\u{ffff}'
    };
    if text.contains(excluded) {
        Cow::Owned(text.chars().filter(|&c| !excluded(c)).collect())
    } else {
        Cow::Borrowed(text)
    }
}

fn push_declaration(style: &mut String, name: &str, value: &str) {
    if value.trim().is_empty() {
        return;
    }
    style.push_str(name);
    style.push(':');
    style.push_str(value.trim());
    style.push(';');
}

/// Split a CSS declaration block into lower-cased property names and values,
/// respecting quotes and parentheses. Custom properties are skipped: the
/// values that use them were already substituted.
fn style_declarations(style: &str) -> Vec<(String, String)> {
    let mut declarations = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut quote = None;
    let bytes = style.as_bytes();
    let mut push = |text: &str| {
        if let Some((name, value)) = text.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if !name.is_empty() && !name.starts_with("--") && !value.is_empty() {
                declarations.push((name, value.to_string()));
            }
        }
    };
    for (index, &byte) in bytes.iter().enumerate() {
        match (quote, byte) {
            (Some(q), _) if byte == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(byte),
            (None, b'(') => depth += 1,
            (None, b')') => depth = depth.saturating_sub(1),
            (None, b';') if depth == 0 => {
                push(&style[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    push(&style[start..]);
    declarations
}

/// Fragment identifiers referenced as `url(#id)` (with optional raw or
/// XML-escaped quotes) or as `href="#id"` in serialized SVG markup.
fn collect_fragment_references(markup: &str, out: &mut std::collections::VecDeque<String>) {
    let mut rest = markup;
    while let Some(index) = rest.find("url(") {
        rest = &rest[index + 4..];
        let mut value = rest.trim_start();
        for quote in ["&quot;", "&apos;", "&#39;", "\"", "'"] {
            if let Some(stripped) = value.strip_prefix(quote) {
                value = stripped;
                break;
            }
        }
        if let Some(fragment) = value.strip_prefix('#') {
            let end = fragment
                .find(|c: char| matches!(c, ')' | '"' | '\'' | '&') || c.is_whitespace())
                .unwrap_or(fragment.len());
            if end > 0 {
                out.push_back(fragment[..end].to_string());
            }
        }
    }
    let mut rest = markup;
    while let Some(index) = rest.find("href=\"#") {
        rest = &rest[index + 7..];
        let end = rest.find('"').unwrap_or(rest.len());
        if end > 0 {
            out.push_back(rest[..end].to_string());
        }
    }
}

fn parse_svg_transform(value: &str) -> Option<Affine> {
    let transform: svgtypes::Transform = value.parse().ok()?;
    let affine = Affine::new([
        transform.a,
        transform.b,
        transform.c,
        transform.d,
        transform.e,
        transform.f,
    ]);
    affine
        .as_coeffs()
        .iter()
        .all(|v| v.is_finite())
        .then_some(affine)
}

/// SVG 2 coords.html#ComputingAViewportsTransform.
fn view_box_transform(
    view_box: svgtypes::ViewBox,
    aspect: svgtypes::AspectRatio,
    viewport: (f64, f64),
) -> Affine {
    use svgtypes::Align;
    let (width, height) = viewport;
    let mut scale_x = width / view_box.w;
    let mut scale_y = height / view_box.h;
    if aspect.align != Align::None {
        let scale = if aspect.slice {
            scale_x.max(scale_y)
        } else {
            scale_x.min(scale_y)
        };
        scale_x = scale;
        scale_y = scale;
    }
    let mut translate_x = -view_box.x * scale_x;
    let mut translate_y = -view_box.y * scale_y;
    let free_x = width - view_box.w * scale_x;
    let free_y = height - view_box.h * scale_y;
    match aspect.align {
        Align::XMidYMin | Align::XMidYMid | Align::XMidYMax => translate_x += free_x / 2.0,
        Align::XMaxYMin | Align::XMaxYMid | Align::XMaxYMax => translate_x += free_x,
        _ => {}
    }
    match aspect.align {
        Align::XMinYMid | Align::XMidYMid | Align::XMaxYMid => translate_y += free_y / 2.0,
        Align::XMinYMax | Align::XMidYMax | Align::XMaxYMax => translate_y += free_y,
        _ => {}
    }
    Affine::new([scale_x, 0.0, 0.0, scale_y, translate_x, translate_y])
}

fn to_affine(transform: usvg::Transform) -> Affine {
    Affine::new([
        f64::from(transform.sx),
        f64::from(transform.ky),
        f64::from(transform.kx),
        f64::from(transform.sy),
        f64::from(transform.tx),
        f64::from(transform.ty),
    ])
}

fn to_rect(rect: usvg::Rect) -> Rect {
    Rect::new(
        f64::from(rect.left()),
        f64::from(rect.top()),
        f64::from(rect.right()),
        f64::from(rect.bottom()),
    )
}

fn union(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(b)),
        (a, b) => a.or(b),
    }
}

/// The bounding rectangle of `rect` mapped through `transform`.
fn transformed_rect(rect: usvg::Rect, transform: usvg::Transform) -> Option<Rect> {
    if transform.is_identity() {
        return Some(to_rect(rect));
    }
    Some(to_affine(transform).transform_rect_bbox(to_rect(rect)))
}

/// The tight bounds of path data mapped through `transform`: transforming
/// the curves first keeps a rotated curve's box tight, as the definition of
/// a bounding box requires.
fn path_bounds(data: &usvg::tiny_skia_path::Path, transform: usvg::Transform) -> Option<Rect> {
    if transform.is_identity() {
        return data.compute_tight_bounds().map(to_rect);
    }
    data.clone()
        .transform(transform)?
        .compute_tight_bounds()
        .map(to_rect)
}

/// The bounding box algorithm (SVG 2 coords.html#BoundingBoxes) over the
/// converted subtree of `group`, in the coordinate space `transform` maps to.
fn group_box(
    group: &usvg::Group,
    fontdb: &usvg::fontdb::Database,
    transform: usvg::Transform,
    options: BoundingBoxOptions,
) -> Option<Rect> {
    let mut result = None;
    for child in group.children() {
        result = union(result, node_box(child, fontdb, transform, options));
    }
    if options.clipped
        && let Some(clip) = group.clip_path()
        && let Some(result_rect) = result
    {
        let clip_transform = transform.pre_concat(clip.transform());
        result = Some(
            match group_box(
                clip.root(),
                fontdb,
                clip_transform,
                BoundingBoxOptions::OBJECT,
            ) {
                Some(clip_rect) => {
                    let x0 = result_rect.x0.max(clip_rect.x0);
                    let y0 = result_rect.y0.max(clip_rect.y0);
                    Rect::new(
                        x0,
                        y0,
                        result_rect.x1.min(clip_rect.x1).max(x0),
                        result_rect.y1.min(clip_rect.y1).max(y0),
                    )
                }
                None => Rect::new(
                    result_rect.x0,
                    result_rect.y0,
                    result_rect.x0,
                    result_rect.y0,
                ),
            },
        );
    }
    result
}

fn node_box(
    node: &usvg::Node,
    fontdb: &usvg::fontdb::Database,
    transform: usvg::Transform,
    options: BoundingBoxOptions,
) -> Option<Rect> {
    match node {
        usvg::Node::Group(group) => group_box(
            group,
            fontdb,
            transform.pre_concat(group.transform()),
            options,
        ),
        usvg::Node::Path(path) => {
            let mut result = None;
            if options.fill {
                result = path_bounds(path.data(), transform);
            }
            // The stroke bounding box assumes no dash pattern (usvg's stroke
            // bounding box already drops it).
            if options.stroke && path.stroke().is_some() {
                result = union(
                    result,
                    transformed_rect(path.stroke_bounding_box(), transform),
                );
            }
            result
        }
        usvg::Node::Text(text) => text_box(text, fontdb, transform, options),
        usvg::Node::Image(image) => transformed_rect(image.bounding_box(), transform),
    }
}

fn first_path(
    group: &usvg::Group,
    transform: usvg::Transform,
) -> Option<(&usvg::Path, usvg::Transform)> {
    group.children().iter().find_map(|child| match child {
        usvg::Node::Path(path) => Some((path.as_ref(), transform)),
        usvg::Node::Group(inner) => first_path(inner, transform.pre_concat(inner.transform())),
        _ => None,
    })
}

fn first_text(group: &usvg::Group) -> Option<&usvg::Text> {
    group.children().iter().find_map(|child| match child {
        usvg::Node::Text(text) => Some(text.as_ref()),
        usvg::Node::Group(inner) => first_text(inner),
        _ => None,
    })
}

/// One typographic character of a laid-out text element: the glyph cluster
/// usvg positioned, in logical order.
struct Cluster {
    /// Whether its span is visible: the geometry document hides every
    /// character outside the queried element.
    visible: bool,
    /// Addressable characters (UTF-16 code units) it corresponds to.
    characters: u32,
    /// Its origin on the baseline, in user units, with any rotation.
    origin: usvg::Transform,
    /// Its advance in the inline direction, including letter and word
    /// spacing, in user units.
    advance: f64,
    ascent: f64,
    descent: f64,
    /// Half the span's stroke width, when it is stroked.
    stroke: f64,
}

#[derive(Clone, Copy)]
struct FaceMetrics {
    units_per_em: f64,
    ascent: f64,
    descent: f64,
}

/// The clusters of a laid-out text element. usvg's shaping gives each
/// cluster's text to its last glyph and an empty string to the others. A
/// cluster's advance is the distance to the next cluster's origin when that
/// continues the same line (it then includes kerning and spacing); at the
/// end of a line or text chunk it is the sum of its glyphs' advances.
fn text_clusters(text: &usvg::Text, fontdb: &usvg::fontdb::Database) -> Vec<Cluster> {
    use skrifa::MetadataProvider;
    let mut faces: std::collections::HashMap<usvg::fontdb::ID, Option<FaceMetrics>> =
        std::collections::HashMap::new();
    let mut clusters = Vec::new();
    // Ends of the clusters' glyph advances, kept apart from `Cluster` until
    // the line continuity of each cluster is known.
    let mut own_advances = Vec::new();
    for span in text.layouted() {
        let stroke = span
            .stroke
            .as_ref()
            .map_or(0.0, |stroke| f64::from(stroke.width().get()) / 2.0);
        let mut pending: Option<(usvg::Transform, f64, FaceMetrics, f64)> = None;
        for glyph in &span.positioned_glyphs {
            let font_size = f64::from(glyph.font_size());
            let measured = fontdb
                .with_face_data(glyph.font, |data, index| {
                    let font = skrifa::FontRef::from_index(data, index).ok()?;
                    let location = font.axes().location(
                        span.variations
                            .iter()
                            .map(|variation| (skrifa::Tag::new(&variation.tag), variation.value)),
                    );
                    let size = skrifa::instance::Size::unscaled();
                    let metrics = font.metrics(size, &location);
                    let advance = font
                        .glyph_metrics(size, &location)
                        .advance_width(skrifa::GlyphId::from(glyph.id))
                        .unwrap_or(0.0);
                    Some((
                        FaceMetrics {
                            units_per_em: f64::from(metrics.units_per_em),
                            ascent: f64::from(metrics.ascent),
                            descent: f64::from(metrics.descent),
                        },
                        f64::from(advance),
                    ))
                })
                .flatten();
            let face = *faces
                .entry(glyph.font)
                .or_insert_with(|| measured.map(|(face, _)| face));
            let Some(face) = face.filter(|face| face.units_per_em > 0.0) else {
                continue;
            };
            let scale = font_size / face.units_per_em;
            let advance = measured.map_or(0.0, |(_, advance)| advance) * scale;
            let entry = pending.get_or_insert_with(|| {
                let inverse = (1.0 / scale) as f32;
                (
                    glyph.transform().pre_scale(inverse, inverse),
                    0.0,
                    face,
                    scale,
                )
            });
            entry.1 += advance;
            if !glyph.text.is_empty() {
                let (origin, own, face, scale) = *entry;
                clusters.push(Cluster {
                    visible: span.visible,
                    characters: utf16_len(&glyph.text),
                    origin,
                    advance: own,
                    ascent: face.ascent * scale,
                    descent: face.descent * scale,
                    stroke,
                });
                own_advances.push(own);
                pending = None;
            }
        }
    }
    for index in 0..clusters.len() {
        let continued = clusters.get(index + 1).and_then(|next| {
            let inverse = clusters[index].origin.invert()?;
            let mut point = usvg::tiny_skia_path::Point::from_xy(next.origin.tx, next.origin.ty);
            inverse.map_point(&mut point);
            let (along, across) = (f64::from(point.x), f64::from(point.y));
            (across.abs() < 1e-3 && along > -1e-3).then_some(along.max(0.0))
        });
        clusters[index].advance = continued.unwrap_or(own_advances[index]);
    }
    clusters
}

/// SVG 2 coords.html#BoundingBoxes for text: each glyph occupies its full
/// glyph cell (its advance by the font's ascent and descent). With `stroke`,
/// each cell grows by half its span's stroke width.
fn text_box(
    text: &usvg::Text,
    fontdb: &usvg::fontdb::Database,
    transform: usvg::Transform,
    options: BoundingBoxOptions,
) -> Option<Rect> {
    let mut result = None;
    for cluster in text_clusters(text, fontdb)
        .iter()
        .filter(|cluster| cluster.visible)
    {
        let inflate = if options.stroke { cluster.stroke } else { 0.0 };
        if !options.fill && inflate == 0.0 {
            continue;
        }
        let cell = Rect::new(
            -inflate,
            -cluster.ascent - inflate,
            cluster.advance + inflate,
            -cluster.descent + inflate,
        );
        let mapped = to_affine(transform.pre_concat(cluster.origin)).transform_rect_bbox(cell);
        result = union(result, Some(mapped));
    }
    result
}

/// The visible typographic characters of a laid-out text element (the copy
/// hides every other character) with their advances. The geometry document
/// drops positioning attributes, so the characters follow one another.
fn text_metrics(text: &usvg::Text, fontdb: &usvg::fontdb::Database) -> TextMetrics {
    let mut characters = 0;
    for chunk in text.chunks() {
        for span in chunk.spans().iter().filter(|span| span.is_visible()) {
            if let Some(slice) = chunk.text().get(span.start()..span.end()) {
                characters += utf16_len(slice);
            }
        }
    }
    let mut first = 0u32;
    let clusters = text_clusters(text, fontdb)
        .into_iter()
        .filter(|cluster| cluster.visible)
        .map(|cluster| {
            let entry = (first, cluster.advance);
            first = first.saturating_add(cluster.characters);
            entry
        })
        .collect();
    TextMetrics {
        characters,
        clusters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_box_transform_follows_preserve_aspect_ratio() {
        let view_box: svgtypes::ViewBox = "0 0 100 50".parse().unwrap();
        let meet: svgtypes::AspectRatio = "xMidYMid meet".parse().unwrap();
        let slice: svgtypes::AspectRatio = "xMinYMax slice".parse().unwrap();
        let none: svgtypes::AspectRatio = "none".parse().unwrap();
        // SVG 2 coords.html#ComputingAViewportsTransform.
        assert_eq!(
            view_box_transform(view_box, meet, (200.0, 200.0)).as_coeffs(),
            [2.0, 0.0, 0.0, 2.0, 0.0, 50.0]
        );
        assert_eq!(
            view_box_transform(view_box, slice, (200.0, 200.0)).as_coeffs(),
            [4.0, 0.0, 0.0, 4.0, 0.0, 0.0]
        );
        assert_eq!(
            view_box_transform(view_box, none, (200.0, 200.0)).as_coeffs(),
            [2.0, 0.0, 0.0, 4.0, 0.0, 0.0]
        );
        let shifted: svgtypes::ViewBox = "10 20 100 50".parse().unwrap();
        assert_eq!(
            view_box_transform(shifted, none, (100.0, 50.0)).as_coeffs(),
            [1.0, 0.0, 0.0, 1.0, -10.0, -20.0]
        );
    }

    #[test]
    fn declarations_and_fragment_references_respect_css_syntax() {
        assert_eq!(
            style_declarations("Fill: url('#a;b') ; --x:1;stroke-width:2;;font-family:\"x;y\""),
            vec![
                ("fill".to_owned(), "url('#a;b')".to_owned()),
                ("stroke-width".to_owned(), "2".to_owned()),
                ("font-family".to_owned(), "\"x;y\"".to_owned()),
            ]
        );
        let mut references = std::collections::VecDeque::new();
        collect_fragment_references(
            r##"<use href="#icon"/><path style="marker-end:url(&quot;#arrow&quot;)" fill="url( #grad)"/><a href="https://x/#no"/>"##,
            &mut references,
        );
        assert_eq!(
            references.into_iter().collect::<Vec<_>>(),
            ["arrow", "grad", "icon"]
        );
        assert_eq!(
            parse_svg_transform("translate(10 20) scale(2)").map(|a| a.as_coeffs()),
            Some([2.0, 0.0, 0.0, 2.0, 10.0, 20.0])
        );
        assert!(parse_svg_transform("scale(").is_none());
    }

    #[test]
    fn substring_lengths_sum_whole_typographic_characters() {
        // SVG 2 #__svg__SVGTextContentElement__getSubStringLength: a ligature
        // cluster's advance belongs to its first addressable character.
        let metrics = TextMetrics {
            characters: 4,
            clusters: vec![(0, 5.0), (1, 7.0), (3, 2.0)],
        };
        assert_eq!(metrics.number_of_chars(), 4);
        assert_eq!(metrics.computed_length(), 14.0);
        assert_eq!(metrics.substring_length(1, 1), Some(7.0));
        assert_eq!(metrics.substring_length(2, 1), Some(0.0));
        assert_eq!(metrics.substring_length(0, u32::MAX), Some(14.0));
        assert_eq!(metrics.substring_length(4, 0), None);
        assert_eq!(TextMetrics::empty().substring_length(0, 0), Some(0.0));
    }

    #[test]
    fn geometry_queries_need_no_layout_for_definite_svg() {
        let dom = Dom::parse_document(
            r##"<!doctype html><svg id="s" width="40" height="20" viewBox="0 0 20 10"><g id="g" transform="scale(2)"><rect id="r" x="1" y="2" width="3" height="4"/></g><path id="p" d="M0 0 H3 V4"/></svg>"##,
        );
        let id = |name| dom.get_by_id(name).unwrap();
        assert_eq!(dom.svg_outermost(id("r")), Some(id("s")));
        assert_eq!(dom.svg_definite_viewport(id("s")), Some((40.0, 20.0)));
        let outer = (40.0, 20.0);
        assert_eq!(
            dom.svg_bbox(id("r"), BoundingBoxOptions::OBJECT, outer),
            [1.0, 2.0, 3.0, 4.0]
        );
        assert_eq!(
            dom.svg_bbox(id("g"), BoundingBoxOptions::OBJECT, outer),
            [1.0, 2.0, 3.0, 4.0]
        );
        assert_eq!(
            dom.svg_bbox(id("s"), BoundingBoxOptions::OBJECT, outer),
            [0.0, 0.0, 8.0, 12.0]
        );
        assert_eq!(
            dom.svg_ctm(id("r"), false, outer),
            Some([4.0, 0.0, 0.0, 4.0, 0.0, 0.0])
        );
        let path = dom.svg_geometry_path(id("p"), outer);
        assert!((path.total_length() - 7.0).abs() < 1e-9);
        assert_eq!(path.point_at_length(5.0), (3.0, 2.0));
        assert!(path.contains_in_fill(2.0, 1.0) && !path.contains_in_fill(1.0, 3.0));
    }
}
