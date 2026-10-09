//! SVG resource elements referenced by CSS: Filter Effects 1 `<filter>`
//! (#FilterProperty `<url>`, #FilterElement) for any element, resolved into
//! the renderer-neutral [`FilterGraph`] for one referencing box.
//!
//! Local reference: csswg-drafts 81c27f686901 (2026-09-04),
//! `filter-effects-1/Overview.bs`. CSS Masking `<clipPath>` and `<mask>`
//! references go through the SVG geometry engine instead (`svg_geometry.rs`).
//!
//! Lengths and regions come out in the referencing element's local CSS px:
//! its user coordinate system has its origin at the top left of its border
//! box (CSS Transforms 1 #transform-box, `view-box` for a CSS box), which
//! is also its object bounding box. Percentages of `userSpaceOnUse` lengths
//! refer to that box as the element's viewport.

use super::{Dom, NodeData, NodeId};
use crate::render::filter_graph::{
    CompositeOp, ConvolveMatrix, EdgeMode, FilterGraph, FilterInput, FilterPrimitive,
    FilterPrimitiveKind, LightSource, Lighting, TransferFunction, Turbulence,
};
use crate::render::{Affine2d, BlendMode, CssRect, PaintColor};

const SVG_NS: &str = "http://www.w3.org/2000/svg";
/// Primitives read from one `filter` element; later children are ignored.
const MAX_FILTER_CHILDREN: usize = 512;
/// The largest convolution kernel accepted (orderX × orderY).
const MAX_KERNEL_CELLS: usize = 1024;

/// How `filterUnits`/`primitiveUnits` map attribute values to local px.
#[derive(Clone, Copy)]
struct Units {
    bounding_box: bool,
    bbox: CssRect,
    font_size: f32,
}

impl Units {
    /// An `x`-like (axis 0) or `y`-like (axis 1) coordinate, or a size
    /// (`offset == false`), as local px.
    fn length(self, value: &str, axis: usize, offset: bool) -> Option<f32> {
        use svgtypes::LengthUnit as Unit;
        let length: svgtypes::Length = value.trim().parse().ok()?;
        let number = length.number as f32;
        let extent = if axis == 0 {
            self.bbox.width
        } else {
            self.bbox.height
        };
        let origin = if !offset {
            0.0
        } else if axis == 0 {
            self.bbox.x
        } else {
            self.bbox.y
        };
        let px = if self.bounding_box {
            // Fractions or percentages of the object bounding box.
            let fraction = match length.unit {
                Unit::Percent => number / 100.0,
                Unit::None => number,
                // Absolute units are meaningless in bounding box units;
                // browsers treat them as user units of the box.
                _ => absolute(number, length.unit, self.font_size)?,
            };
            origin + fraction * extent
        } else {
            let user = match length.unit {
                Unit::Percent => number / 100.0 * extent,
                unit => absolute(number, unit, self.font_size)?,
            };
            origin + user
        };
        px.is_finite().then_some(px)
    }

    /// A primitive attribute number (stdDeviation, dx, radius …) in the
    /// `primitiveUnits` coordinate system, as local px along `axis`.
    fn scalar(self, number: f32, axis: usize) -> f32 {
        if self.bounding_box {
            number
                * if axis == 0 {
                    self.bbox.width
                } else {
                    self.bbox.height
                }
        } else {
            number
        }
    }

    /// Local px to the `primitiveUnits` user space (feTurbulence's noise
    /// coordinates).
    fn to_user_space(self) -> Affine2d {
        let translate = Affine2d::translate(-self.bbox.x, -self.bbox.y);
        if self.bounding_box && self.bbox.width > 0.0 && self.bbox.height > 0.0 {
            Affine2d::scale(1.0 / self.bbox.width, 1.0 / self.bbox.height).then(translate)
        } else {
            translate
        }
    }
}

fn absolute(number: f32, unit: svgtypes::LengthUnit, font_size: f32) -> Option<f32> {
    use svgtypes::LengthUnit as Unit;
    Some(match unit {
        Unit::None | Unit::Px => number,
        Unit::Em => number * font_size,
        // CSS Values 4 #ex: 0.5em when the x-height is impractical to find.
        Unit::Ex => number * font_size / 2.0,
        Unit::In => number * 96.0,
        Unit::Cm => number * 96.0 / 2.54,
        Unit::Mm => number * 96.0 / 25.4,
        Unit::Pt => number * 96.0 / 72.0,
        Unit::Pc => number * 16.0,
        Unit::Percent => return None,
    })
}

/// A whitespace- and/or comma-separated list of numbers; `None` when any
/// entry is not a number (SVG 2 #DataTypeNumberList).
pub(crate) fn number_list(value: &str) -> Option<Vec<f32>> {
    value
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<f32>().ok().filter(|n| n.is_finite()))
        .collect()
}

/// `<number-optional-number>`: one or two numbers, the second defaulting to
/// the first.
fn number_optional_number(value: &str) -> Option<(f32, f32)> {
    match number_list(value)?.as_slice() {
        [one] => Some((*one, *one)),
        [x, y] => Some((*x, *y)),
        _ => None,
    }
}

fn blend_mode(value: &str) -> Option<BlendMode> {
    Some(match value.trim().to_ascii_lowercase().as_str() {
        "normal" => BlendMode::Normal,
        "multiply" => BlendMode::Multiply,
        "screen" => BlendMode::Screen,
        "overlay" => BlendMode::Overlay,
        "darken" => BlendMode::Darken,
        "lighten" => BlendMode::Lighten,
        "color-dodge" => BlendMode::ColorDodge,
        "color-burn" => BlendMode::ColorBurn,
        "hard-light" => BlendMode::HardLight,
        "soft-light" => BlendMode::SoftLight,
        "difference" => BlendMode::Difference,
        "exclusion" => BlendMode::Exclusion,
        "hue" => BlendMode::Hue,
        "saturation" => BlendMode::Saturation,
        "color" => BlendMode::Color,
        "luminosity" => BlendMode::Luminosity,
        "plus-lighter" => BlendMode::PlusLighter,
        _ => return None,
    })
}

fn edge_mode(value: Option<&str>, initial: EdgeMode) -> EdgeMode {
    match value.map(str::trim) {
        Some("duplicate") => EdgeMode::Duplicate,
        Some("wrap") => EdgeMode::Wrap,
        Some("mirror") => EdgeMode::Mirror,
        Some("none") => EdgeMode::None,
        _ => initial,
    }
}

impl Dom {
    /// The element a CSS `url()` names, if it is in the same document as
    /// `node`: a fragment-only URL, or a URL equal to the document URL apart
    /// from its fragment (CSS Values 4 #local-urls). The fragment is looked
    /// up in `node`'s tree scope, first element in tree order.
    pub(crate) fn svg_resource_target(
        &self,
        node: NodeId,
        url: &str,
        document_url: Option<&url::Url>,
    ) -> Option<NodeId> {
        let url = url.trim();
        let fragment = if let Some(fragment) = url.strip_prefix('#') {
            fragment.to_string()
        } else {
            let document = document_url?;
            let resolved = document.join(url).ok()?;
            let mut without = resolved.clone();
            without.set_fragment(None);
            let mut current = document.clone();
            current.set_fragment(None);
            if without != current {
                return None;
            }
            resolved.fragment()?.to_string()
        };
        let fragment = percent_encoding_decode(&fragment);
        if fragment.is_empty() {
            return None;
        }
        let scope = self.tree_scope(node);
        self.descendants(scope).into_iter().find(|&candidate| {
            matches!(self.nodes[candidate].data, NodeData::Element { .. })
                && self.attr(candidate, "id") == Some(fragment.as_str())
        })
    }

    /// Record that `consumer`'s rendering reads `resource` (a `filter`,
    /// `clipPath` or `mask` it references from CSS, or `None` for a reference
    /// that did not resolve): the resource subtree and what it references,
    /// and the ID bindings of the consumer's tree scope. A mutation of either
    /// then invalidates the consumer like an inline SVG's dependents
    /// (`svg_dependencies`), so its effect is resolved again.
    pub(crate) fn note_svg_resource_use(&self, consumer: NodeId, resource: Option<NodeId>) {
        let mut proof = match resource {
            Some(resource) => self.svg_input_dependencies(resource, &[], true),
            None => super::svg_dependencies::Proof::default(),
        };
        proof.bindings.push(self.tree_scope(consumer));
        self.svg_dependencies
            .borrow_mut()
            .publish_candidate(consumer, proof);
    }

    /// The value of an SVG property on `id`: its author cascade winner,
    /// else its `style` attribute declaration, else its presentation
    /// attribute; for an inherited property, the nearest ancestor's.
    fn svg_property(&self, id: NodeId, property: &str, inherited: bool) -> Option<String> {
        let mut current = Some(id);
        while let Some(node) =
            current.filter(|&node| matches!(self.nodes[node].data, NodeData::Element { .. }))
        {
            let value = self
                .cascaded(node, property)
                .or_else(|| {
                    self.attr(node, "style").and_then(|style| {
                        style.split(';').rev().find_map(|declaration| {
                            let (name, value) = declaration.split_once(':')?;
                            name.trim().eq_ignore_ascii_case(property).then(|| {
                                let value = value.trim();
                                value
                                    .strip_suffix("!important")
                                    .unwrap_or(value)
                                    .trim()
                                    .to_string()
                            })
                        })
                    })
                })
                .or_else(|| {
                    (self.namespace_uri(node) == Some(SVG_NS))
                        .then(|| self.attr(node, property).map(|v| v.trim().to_string()))
                        .flatten()
                })
                .filter(|value| !value.is_empty());
            match value {
                // CSS Cascade 4 #inherit: the parent's computed value.
                Some(value) if value.eq_ignore_ascii_case("inherit") => {}
                Some(value)
                    if value.eq_ignore_ascii_case("initial")
                        || (value.eq_ignore_ascii_case("unset") && !inherited) =>
                {
                    return None;
                }
                Some(value) if !value.eq_ignore_ascii_case("unset") => return Some(value),
                _ if !inherited => return None,
                _ => {}
            }
            current = self.nodes[node].parent;
        }
        None
    }

    /// A color property (`flood-color`, `lighting-color`) as straight sRGB,
    /// `currentcolor` taking the element's own color.
    fn svg_color_property(&self, id: NodeId, property: &str, initial: [f32; 4]) -> [f32; 4] {
        let Some(value) = self.svg_property(id, property, false) else {
            return initial;
        };
        let value = if value.trim().eq_ignore_ascii_case("currentcolor") {
            self.svg_used_color(id)
        } else {
            value
        };
        match PaintColor::parse_css(&value) {
            Some(PaintColor::Rgba(r, g, b, a)) => [r, g, b, a].map(|c| f32::from(c) / 255.0),
            _ => initial,
        }
    }

    fn svg_opacity_property(&self, id: NodeId, property: &str) -> f32 {
        self.svg_property(id, property, false)
            .and_then(|value| {
                let value = value.trim();
                match value.strip_suffix('%') {
                    Some(percent) => percent.trim().parse::<f32>().ok().map(|n| n / 100.0),
                    None => value.parse::<f32>().ok(),
                }
            })
            .filter(|n| n.is_finite())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0)
    }

    /// Filter Effects 1 #FilterElement / #FilterEffectsRegion /
    /// #FilterPrimitivesOverview: resolve `filter` (a `filter` element) for
    /// the element `target` whose border box (its object bounding box and
    /// user space origin) is `bbox` in local CSS px.
    pub(crate) fn svg_filter_graph(
        &self,
        filter: NodeId,
        target: NodeId,
        bbox: CssRect,
    ) -> FilterGraph {
        let font_size = self.font_px(filter).max(0.0);
        let units_of = |attribute: &str, initial_bbox: bool| Units {
            bounding_box: match self.attr(filter, attribute).map(str::trim) {
                Some("userSpaceOnUse") => false,
                Some("objectBoundingBox") => true,
                _ => initial_bbox,
            },
            bbox,
            font_size,
        };
        // #FilterEffectsRegion: filterUnits defaults to objectBoundingBox,
        // x/y to -10% and width/height to 120%.
        let filter_units = units_of("filterUnits", true);
        let region_value = |name: &str, initial: &str, axis: usize, offset: bool| {
            self.attr(filter, name)
                .and_then(|value| filter_units.length(value, axis, offset))
                .or_else(|| filter_units.length(initial, axis, offset))
                .unwrap_or(0.0)
        };
        let region = CssRect::new(
            region_value("x", "-10%", 0, true),
            region_value("y", "-10%", 1, true),
            region_value("width", "120%", 0, false),
            region_value("height", "120%", 1, false),
        );
        let primitive_units = units_of("primitiveUnits", false);
        let paint = |property: &str| -> [f32; 4] {
            // SVG 2 #FillProperty / #StrokeProperty initial values.
            let value = self
                .computed_value_resolved(target, property)
                .unwrap_or_else(|| String::from(if property == "fill" { "black" } else { "none" }));
            let value = value.trim();
            if value.eq_ignore_ascii_case("currentcolor") {
                return match PaintColor::parse_css(&self.svg_used_color(target)) {
                    Some(PaintColor::Rgba(r, g, b, a)) => {
                        [r, g, b, a].map(|c| f32::from(c) / 255.0)
                    }
                    _ => [0.0; 4],
                };
            }
            match PaintColor::parse_css(value) {
                Some(PaintColor::Rgba(r, g, b, a)) => [r, g, b, a].map(|c| f32::from(c) / 255.0),
                _ => [0.0; 4],
            }
        };
        let mut graph = FilterGraph {
            region,
            primitives: Vec::new(),
            primitive_space: primitive_units.to_user_space(),
            // #FillPaint / #StrokePaint: SVG 2 fill and stroke initial values.
            fill_paint: paint("fill"),
            stroke_paint: paint("stroke"),
        };
        if !(region.width > 0.0 && region.height > 0.0) {
            // A negative or zero region disables the effect of the filter:
            // nothing renders, like a filter without primitives.
            return graph;
        }
        // `result` names in order, for `in`/`in2` lookups.
        let mut results: Vec<Option<String>> = Vec::new();
        for child in self.child_iter(filter).take(MAX_FILTER_CHILDREN) {
            if self.namespace_uri(child) != Some(SVG_NS) {
                continue;
            }
            let Some(tag) = self.tag_name(child) else {
                continue;
            };
            let index = graph.primitives.len();
            let input = |name: &str| self.filter_input(child, name, index, &results);
            let kind = match tag {
                "feBlend" => FilterPrimitiveKind::Blend {
                    input: input("in"),
                    input2: input("in2"),
                    mode: self
                        .attr(child, "mode")
                        .map_or(Some(BlendMode::Normal), blend_mode)
                        .unwrap_or(BlendMode::Normal),
                    composite: self.attr(child, "no-composite").is_none(),
                },
                "feColorMatrix" => FilterPrimitiveKind::ColorMatrix {
                    input: input("in"),
                    matrix: color_matrix(self.attr(child, "type"), self.attr(child, "values")),
                },
                "feComponentTransfer" => FilterPrimitiveKind::ComponentTransfer {
                    input: input("in"),
                    functions: Box::new(self.transfer_functions(child)),
                },
                "feComposite" => FilterPrimitiveKind::Composite {
                    input: input("in"),
                    input2: input("in2"),
                    operator: match self.attr(child, "operator").map(str::trim) {
                        Some("in") => CompositeOp::In,
                        Some("out") => CompositeOp::Out,
                        Some("atop") => CompositeOp::Atop,
                        Some("xor") => CompositeOp::Xor,
                        Some("lighter") => CompositeOp::Lighter,
                        Some("arithmetic") => {
                            let k = |name: &str| {
                                self.attr(child, name)
                                    .and_then(|v| v.trim().parse::<f32>().ok())
                                    .filter(|n| n.is_finite())
                                    .unwrap_or(0.0)
                            };
                            CompositeOp::Arithmetic {
                                k1: k("k1"),
                                k2: k("k2"),
                                k3: k("k3"),
                                k4: k("k4"),
                            }
                        }
                        _ => CompositeOp::Over,
                    },
                },
                "feConvolveMatrix" => match self.convolve_matrix(child) {
                    Some(matrix) => FilterPrimitiveKind::ConvolveMatrix {
                        input: input("in"),
                        matrix: Box::new(matrix),
                    },
                    // A kernel that does not match its order: a pass
                    // through filter.
                    None => FilterPrimitiveKind::ColorMatrix {
                        input: input("in"),
                        matrix: None,
                    },
                },
                "feDisplacementMap" => {
                    let scale = self
                        .attr(child, "scale")
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .filter(|n| n.is_finite())
                        .unwrap_or(0.0);
                    let channel = |name: &str| match self.attr(child, name).map(str::trim) {
                        Some("R") => 0,
                        Some("G") => 1,
                        Some("B") => 2,
                        _ => 3,
                    };
                    FilterPrimitiveKind::DisplacementMap {
                        input: input("in"),
                        input2: input("in2"),
                        scale: (
                            primitive_units.scalar(scale, 0),
                            primitive_units.scalar(scale, 1),
                        ),
                        channels: (channel("xChannelSelector"), channel("yChannelSelector")),
                    }
                }
                "feDropShadow" => {
                    let number = |name: &str, initial: f32| {
                        self.attr(child, name)
                            .and_then(|v| v.trim().parse::<f32>().ok())
                            .filter(|n| n.is_finite())
                            .unwrap_or(initial)
                    };
                    let deviation = self
                        .attr(child, "stdDeviation")
                        .and_then(number_optional_number)
                        .unwrap_or((2.0, 2.0));
                    let mut color =
                        self.svg_color_property(child, "flood-color", [0.0, 0.0, 0.0, 1.0]);
                    color[3] *= self.svg_opacity_property(child, "flood-opacity");
                    FilterPrimitiveKind::DropShadow {
                        input: input("in"),
                        dx: primitive_units.scalar(number("dx", 2.0), 0),
                        dy: primitive_units.scalar(number("dy", 2.0), 1),
                        std_deviation: (
                            primitive_units.scalar(deviation.0, 0),
                            primitive_units.scalar(deviation.1, 1),
                        ),
                        color,
                    }
                }
                "feFlood" => {
                    let mut color =
                        self.svg_color_property(child, "flood-color", [0.0, 0.0, 0.0, 1.0]);
                    color[3] *= self.svg_opacity_property(child, "flood-opacity");
                    FilterPrimitiveKind::Flood { color }
                }
                "feGaussianBlur" => {
                    let deviation = self
                        .attr(child, "stdDeviation")
                        .and_then(number_optional_number)
                        .unwrap_or((0.0, 0.0));
                    FilterPrimitiveKind::GaussianBlur {
                        input: input("in"),
                        std_deviation: (
                            primitive_units.scalar(deviation.0, 0),
                            primitive_units.scalar(deviation.1, 1),
                        ),
                        edge: edge_mode(self.attr(child, "edgeMode"), EdgeMode::None),
                    }
                }
                // #feImageElement: TRust does not fetch or render feImage
                // sources inside filter graphs; its result is transparent.
                "feImage" => FilterPrimitiveKind::Transparent,
                "feMerge" => FilterPrimitiveKind::Merge {
                    inputs: self
                        .child_iter(child)
                        .filter(|&node| self.tag_name(node) == Some("feMergeNode"))
                        .map(|node| self.filter_input(node, "in", index, &results))
                        .collect(),
                },
                "feMorphology" => {
                    let radius = self
                        .attr(child, "radius")
                        .and_then(number_optional_number)
                        .unwrap_or((0.0, 0.0));
                    FilterPrimitiveKind::Morphology {
                        input: input("in"),
                        dilate: self.attr(child, "operator").map(str::trim) == Some("dilate"),
                        radius: (
                            primitive_units.scalar(radius.0, 0),
                            primitive_units.scalar(radius.1, 1),
                        ),
                    }
                }
                "feOffset" => {
                    let number = |name: &str| {
                        self.attr(child, name)
                            .and_then(|v| v.trim().parse::<f32>().ok())
                            .filter(|n| n.is_finite())
                            .unwrap_or(0.0)
                    };
                    FilterPrimitiveKind::Offset {
                        input: input("in"),
                        dx: primitive_units.scalar(number("dx"), 0),
                        dy: primitive_units.scalar(number("dy"), 1),
                    }
                }
                "feTile" => FilterPrimitiveKind::Tile { input: input("in") },
                "feTurbulence" => {
                    let base_frequency = self
                        .attr(child, "baseFrequency")
                        .and_then(number_optional_number)
                        .unwrap_or((0.0, 0.0));
                    let octaves = self
                        .attr(child, "numOctaves")
                        .and_then(|v| v.trim().parse::<i64>().ok())
                        .unwrap_or(1);
                    // Negative values are unsupported: the primitive is in
                    // error and renders transparent black.
                    if octaves < 0 || base_frequency.0 < 0.0 || base_frequency.1 < 0.0 {
                        FilterPrimitiveKind::Transparent
                    } else {
                        FilterPrimitiveKind::Turbulence(Turbulence {
                            base_frequency,
                            octaves: octaves.min(i64::from(u32::MAX)) as u32,
                            seed: self
                                .attr(child, "seed")
                                .and_then(|v| v.trim().parse::<f32>().ok())
                                .filter(|n| n.is_finite())
                                .unwrap_or(0.0),
                            stitch: self.attr(child, "stitchTiles").map(str::trim)
                                == Some("stitch"),
                            fractal_noise: self.attr(child, "type").map(str::trim)
                                == Some("fractalNoise"),
                        })
                    }
                }
                "feDiffuseLighting" | "feSpecularLighting" => {
                    match self.lighting(child, tag == "feSpecularLighting", primitive_units) {
                        Some(lighting) => FilterPrimitiveKind::Lighting {
                            input: input("in"),
                            lighting,
                        },
                        // Without a light source the result is transparent.
                        None => FilterPrimitiveKind::Transparent,
                    }
                }
                _ => continue,
            };
            let subregion = self.primitive_subregion(child, &kind, &graph, primitive_units);
            let linear_rgb = !self
                .svg_property(child, "color-interpolation-filters", true)
                .is_some_and(|value| {
                    let value = value.trim();
                    // `auto` lets the user agent choose; TRust uses sRGB.
                    value.eq_ignore_ascii_case("srgb") || value.eq_ignore_ascii_case("auto")
                });
            graph.primitives.push(FilterPrimitive {
                subregion,
                linear_rgb,
                kind,
            });
            results.push(
                self.attr(child, "result")
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
            );
        }
        graph
    }

    /// #CommonAttributes `in`: a keyword, or the closest preceding
    /// primitive whose `result` matches; anything else (a missing or
    /// forward reference) acts as if unspecified: the previous result, or
    /// SourceGraphic for the first primitive.
    fn filter_input(
        &self,
        node: NodeId,
        attribute: &str,
        index: usize,
        results: &[Option<String>],
    ) -> FilterInput {
        let unspecified = if index == 0 {
            FilterInput::SourceGraphic
        } else {
            FilterInput::Result(index - 1)
        };
        let Some(value) = self.attr(node, attribute).map(str::trim) else {
            return unspecified;
        };
        match value {
            "SourceGraphic" => FilterInput::SourceGraphic,
            "SourceAlpha" => FilterInput::SourceAlpha,
            "BackgroundImage" => FilterInput::BackgroundImage,
            "BackgroundAlpha" => FilterInput::BackgroundAlpha,
            "FillPaint" => FilterInput::FillPaint,
            "StrokePaint" => FilterInput::StrokePaint,
            name => results[..index]
                .iter()
                .rposition(|result| result.as_deref() == Some(name))
                .map_or(unspecified, FilterInput::Result),
        }
    }

    /// #FilterPrimitiveSubRegion: x, y, width and height default to the
    /// union of the referenced results' subregions, or to the filter region
    /// when there are none, a standard input is referenced, or the primitive
    /// is feTile.
    fn primitive_subregion(
        &self,
        node: NodeId,
        kind: &FilterPrimitiveKind,
        graph: &FilterGraph,
        units: Units,
    ) -> CssRect {
        let inputs = filter_kind_inputs(kind);
        let default = if matches!(kind, FilterPrimitiveKind::Tile { .. })
            || inputs.is_empty()
            || inputs
                .iter()
                .any(|input| !matches!(input, FilterInput::Result(_)))
        {
            graph.region
        } else {
            inputs
                .iter()
                .filter_map(|input| match input {
                    FilterInput::Result(index) => Some(graph.primitives[*index].subregion),
                    _ => None,
                })
                .reduce(crate::render::union_rect)
                .unwrap_or(graph.region)
        };
        let value = |name: &str, axis: usize, offset: bool, fallback: f32| {
            self.attr(node, name)
                .and_then(|value| units.length(value, axis, offset))
                .unwrap_or(fallback)
        };
        CssRect::new(
            value("x", 0, true, default.x),
            value("y", 1, true, default.y),
            value("width", 0, false, default.width),
            value("height", 1, false, default.height),
        )
    }

    /// #feComponentTransferElement: the last transfer function element of
    /// each kind; unspecified channels are the identity.
    fn transfer_functions(&self, node: NodeId) -> [TransferFunction; 4] {
        let mut functions: [TransferFunction; 4] = Default::default();
        for child in self.child_iter(node) {
            let channel = match self.tag_name(child) {
                Some("feFuncR") => 0,
                Some("feFuncG") => 1,
                Some("feFuncB") => 2,
                Some("feFuncA") => 3,
                _ => continue,
            };
            let number = |name: &str, initial: f32| {
                self.attr(child, name)
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .filter(|n| n.is_finite())
                    .unwrap_or(initial)
            };
            let table = || {
                self.attr(child, "tableValues")
                    .and_then(number_list)
                    .unwrap_or_default()
            };
            functions[channel] = match self.attr(child, "type").map(str::trim) {
                Some("table") => TransferFunction::Table(table()),
                Some("discrete") => TransferFunction::Discrete(table()),
                Some("linear") => TransferFunction::Linear {
                    slope: number("slope", 1.0),
                    intercept: number("intercept", 0.0),
                },
                Some("gamma") => TransferFunction::Gamma {
                    amplitude: number("amplitude", 1.0),
                    exponent: number("exponent", 1.0),
                    offset: number("offset", 0.0),
                },
                _ => TransferFunction::Identity,
            };
        }
        functions
    }

    /// #feConvolveMatrixElement's attributes; `None` makes the primitive a
    /// pass through filter (a kernel that does not match its order, or an
    /// invalid order or target).
    fn convolve_matrix(&self, node: NodeId) -> Option<ConvolveMatrix> {
        let order = match self.attr(node, "order") {
            Some(value) => {
                let (x, y) = number_optional_number(value)?;
                // Truncated toward zero; must be greater than zero.
                (x.trunc() as i64, y.trunc() as i64)
            }
            None => (3, 3),
        };
        if order.0 <= 0 || order.1 <= 0 {
            return None;
        }
        let order = (order.0 as usize, order.1 as usize);
        if order.0.saturating_mul(order.1) > MAX_KERNEL_CELLS {
            return None;
        }
        let kernel = number_list(self.attr(node, "kernelMatrix")?)?;
        if kernel.len() != order.0 * order.1 {
            return None;
        }
        let sum: f32 = kernel.iter().sum();
        let divisor = self
            .attr(node, "divisor")
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|n| n.is_finite() && *n != 0.0)
            .unwrap_or(if sum == 0.0 { 1.0 } else { sum });
        let target = |name: &str, order: usize| -> Option<usize> {
            match self.attr(node, name) {
                Some(value) => {
                    let target = value.trim().parse::<i64>().ok()?;
                    (0..order as i64)
                        .contains(&target)
                        .then_some(target as usize)
                }
                None => Some(order / 2),
            }
        };
        Some(ConvolveMatrix {
            order,
            kernel,
            divisor,
            bias: self
                .attr(node, "bias")
                .and_then(|v| v.trim().parse::<f32>().ok())
                .filter(|n| n.is_finite())
                .unwrap_or(0.0),
            target: (target("targetX", order.0)?, target("targetY", order.1)?),
            edge: edge_mode(self.attr(node, "edgeMode"), EdgeMode::Duplicate),
            preserve_alpha: self.attr(node, "preserveAlpha").map(str::trim) == Some("true"),
        })
    }

    /// #feDiffuseLightingElement / #feSpecularLightingElement and their
    /// light source child (#LightSourceDefinitions), positions mapped to
    /// local px through `primitiveUnits`.
    fn lighting(&self, node: NodeId, specular: bool, units: Units) -> Option<Lighting> {
        let number = |node: NodeId, name: &str, initial: f32| {
            self.attr(node, name)
                .and_then(|v| v.trim().parse::<f32>().ok())
                .filter(|n| n.is_finite())
                .unwrap_or(initial)
        };
        let source = self.child_iter(node).find(|&child| {
            matches!(
                self.tag_name(child),
                Some("feDistantLight" | "fePointLight" | "feSpotLight")
            )
        })?;
        let point = |x: &str, y: &str, z: &str| {
            let (px, py) = (number(source, x, 0.0), number(source, y, 0.0));
            (
                units.bbox.x + units.scalar(px, 0),
                units.bbox.y + units.scalar(py, 1),
                // Z uses the normalized diagonal of the bounding box.
                number(source, z, 0.0)
                    * if units.bounding_box {
                        units.bbox.width.hypot(units.bbox.height) / std::f32::consts::SQRT_2
                    } else {
                        1.0
                    },
            )
        };
        let light = match self.tag_name(source)? {
            "feDistantLight" => LightSource::Distant {
                azimuth: number(source, "azimuth", 0.0),
                elevation: number(source, "elevation", 0.0),
            },
            "fePointLight" => {
                let (x, y, z) = point("x", "y", "z");
                LightSource::Point { x, y, z }
            }
            _ => {
                let (x, y, z) = point("x", "y", "z");
                let at = point("pointsAtX", "pointsAtY", "pointsAtZ");
                LightSource::Spot {
                    x,
                    y,
                    z,
                    at,
                    exponent: number(source, "specularExponent", 1.0),
                    cone: self
                        .attr(source, "limitingConeAngle")
                        .and_then(|v| v.trim().parse::<f32>().ok())
                        .filter(|n| n.is_finite()),
                }
            }
        };
        let lighting_color = self.svg_color_property(node, "lighting-color", [1.0; 4]);
        Some(Lighting {
            surface_scale: number(node, "surfaceScale", 1.0),
            constant: if specular {
                number(node, "specularConstant", 1.0)
            } else {
                number(node, "diffuseConstant", 1.0)
            },
            specular_exponent: specular
                .then(|| number(node, "specularExponent", 1.0).clamp(1.0, 128.0)),
            color: [lighting_color[0], lighting_color[1], lighting_color[2]],
            light,
        })
    }
}

fn filter_kind_inputs(kind: &FilterPrimitiveKind) -> Vec<FilterInput> {
    match kind {
        FilterPrimitiveKind::Blend { input, input2, .. }
        | FilterPrimitiveKind::Composite { input, input2, .. }
        | FilterPrimitiveKind::DisplacementMap { input, input2, .. } => vec![*input, *input2],
        FilterPrimitiveKind::ColorMatrix { input, .. }
        | FilterPrimitiveKind::ComponentTransfer { input, .. }
        | FilterPrimitiveKind::ConvolveMatrix { input, .. }
        | FilterPrimitiveKind::DropShadow { input, .. }
        | FilterPrimitiveKind::GaussianBlur { input, .. }
        | FilterPrimitiveKind::Lighting { input, .. }
        | FilterPrimitiveKind::Morphology { input, .. }
        | FilterPrimitiveKind::Offset { input, .. }
        | FilterPrimitiveKind::Tile { input } => vec![*input],
        FilterPrimitiveKind::Merge { inputs } => inputs.clone(),
        FilterPrimitiveKind::Flood { .. }
        | FilterPrimitiveKind::Turbulence(_)
        | FilterPrimitiveKind::Transparent => Vec::new(),
    }
}

/// #feColorMatrixElement: the matrix for `type` and `values`, or `None`
/// (a pass through filter) when the values do not fit the type.
fn color_matrix(kind: Option<&str>, values: Option<&str>) -> Option<[f32; 20]> {
    let values = match values {
        Some(values) => Some(number_list(values)?),
        None => None,
    };
    let identity = {
        let mut matrix = [0.0; 20];
        for i in [0, 6, 12, 18] {
            matrix[i] = 1.0;
        }
        matrix
    };
    match kind.map(str::trim).unwrap_or("matrix") {
        "saturate" => {
            let s = match values.as_deref() {
                None => 1.0,
                Some([s]) => *s,
                Some(_) => return None,
            };
            let mut matrix = identity;
            let rows = [
                [
                    0.2126 + 0.7873 * s,
                    0.7152 - 0.7152 * s,
                    0.0722 - 0.0722 * s,
                ],
                [
                    0.2126 - 0.2126 * s,
                    0.7152 + 0.2848 * s,
                    0.0722 - 0.0722 * s,
                ],
                [
                    0.2126 - 0.2126 * s,
                    0.7152 - 0.7152 * s,
                    0.0722 + 0.9278 * s,
                ],
            ];
            for (row, values) in rows.iter().enumerate() {
                matrix[row * 5..row * 5 + 3].copy_from_slice(values);
            }
            Some(matrix)
        }
        "hueRotate" => {
            let degrees = match values.as_deref() {
                None => 0.0,
                Some([degrees]) => *degrees,
                Some(_) => return None,
            };
            let (sin, cos) = degrees.to_radians().sin_cos();
            let luma = [0.2126, 0.7152, 0.0722];
            let cos_terms = [
                [0.7873, -0.7152, -0.0722],
                [-0.2126, 0.2848, -0.0722],
                [-0.2126, -0.7152, 0.9278],
            ];
            let sin_terms = [
                [-0.2126, -0.7152, 0.9278],
                [0.143, 0.140, -0.283],
                [-0.7873, 0.7152, 0.0722],
            ];
            let mut matrix = identity;
            for row in 0..3 {
                for col in 0..3 {
                    matrix[row * 5 + col] =
                        luma[col] + cos * cos_terms[row][col] + sin * sin_terms[row][col];
                }
            }
            Some(matrix)
        }
        "luminanceToAlpha" => {
            let mut matrix = [0.0; 20];
            matrix[15] = 0.2126;
            matrix[16] = 0.7152;
            matrix[17] = 0.0722;
            Some(matrix)
        }
        "matrix" => match values.as_deref() {
            None => Some(identity),
            Some(values) if values.len() == 20 => {
                let mut matrix = [0.0; 20];
                matrix.copy_from_slice(values);
                Some(matrix)
            }
            Some(_) => None,
        },
        _ => Some(identity),
    }
}

/// Percent-decode a URL fragment into the id it names (URL Standard
/// #percent-decode, UTF-8).
fn percent_encoding_decode(fragment: &str) -> String {
    let bytes = fragment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            out.push((high * 16 + low) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dom(markup: &str) -> Dom {
        Dom::parse_document(markup)
    }

    #[test]
    fn filter_region_and_primitive_units_resolve_against_the_border_box() {
        let dom = dom(
            r##"<svg width=0 height=0><filter id=f x="0" y="-0.5" width="1" height="2"
              primitiveUnits="objectBoundingBox">
              <feOffset dx="0.1" dy="0.5"/>
              <feGaussianBlur stdDeviation="0.05 0" x="10%" width="50%"/>
            </filter>
            <filter id=u filterUnits="userSpaceOnUse" x="-5" y="-5" width="110" height="60">
              <feFlood flood-color="red" flood-opacity="50%" result="a"/>
              <feComposite in="SourceGraphic" in2="a" operator="arithmetic" k2="1"/>
            </filter></svg><div id=t></div>"##,
        );
        let target = dom.get_by_id("t").unwrap();
        let bbox = CssRect::new(10.0, 20.0, 100.0, 40.0);
        let graph = dom.svg_filter_graph(dom.get_by_id("f").unwrap(), target, bbox);
        assert_eq!(graph.region, CssRect::new(10.0, 0.0, 100.0, 80.0));
        assert_eq!(graph.primitives.len(), 2);
        assert!(matches!(
            graph.primitives[0].kind,
            FilterPrimitiveKind::Offset { dx, dy, .. } if (dx - 10.0).abs() < 1e-4 && (dy - 20.0).abs() < 1e-4
        ));
        // A primitive's default subregion is the union of its inputs'.
        assert_eq!(graph.primitives[0].subregion, graph.region);
        let blur = &graph.primitives[1];
        assert_eq!(blur.subregion, CssRect::new(20.0, 0.0, 50.0, 80.0));
        assert!(matches!(
            blur.kind,
            FilterPrimitiveKind::GaussianBlur { std_deviation: (x, y), input: FilterInput::Result(0), .. }
                if (x - 5.0).abs() < 1e-4 && y == 0.0
        ));
        assert!(blur.linear_rgb);
        let user = dom.svg_filter_graph(dom.get_by_id("u").unwrap(), target, bbox);
        assert_eq!(user.region, CssRect::new(5.0, 15.0, 110.0, 60.0));
        assert!(matches!(
            user.primitives[0].kind,
            FilterPrimitiveKind::Flood { color } if color == [1.0, 0.0, 0.0, 0.5]
        ));
        assert!(matches!(
            &user.primitives[1].kind,
            FilterPrimitiveKind::Composite {
                input: FilterInput::SourceGraphic,
                input2: FilterInput::Result(0),
                operator: CompositeOp::Arithmetic { k2, .. },
            } if *k2 == 1.0
        ));
    }

    #[test]
    fn inputs_results_and_interpolation_spaces_follow_the_common_attributes() {
        let dom = dom(r##"<svg color-interpolation-filters="sRGB"><filter id=f>
              <feOffset result="moved" in="SourceAlpha"/>
              <feFlood result="moved"/>
              <feMerge>
                <feMergeNode in="moved"/>
                <feMergeNode in="missing"/>
                <feMergeNode in="later"/>
                <feMergeNode in="SourceGraphic"/>
              </feMerge>
              <feColorMatrix type="saturate" values="1 2" color-interpolation-filters="linearRGB"/>
              <feTile result="later"/>
            </filter></svg><div id=t></div>"##);
        let graph = dom.svg_filter_graph(
            dom.get_by_id("f").unwrap(),
            dom.get_by_id("t").unwrap(),
            CssRect::new(0.0, 0.0, 10.0, 10.0),
        );
        assert!(matches!(
            graph.primitives[0].kind,
            FilterPrimitiveKind::Offset {
                input: FilterInput::SourceAlpha,
                ..
            }
        ));
        // The closest preceding result wins; missing and forward references
        // act as unspecified (the previous result).
        let FilterPrimitiveKind::Merge { inputs } = &graph.primitives[2].kind else {
            panic!("merge");
        };
        assert_eq!(
            inputs,
            &[
                FilterInput::Result(1),
                FilterInput::Result(1),
                FilterInput::Result(1),
                FilterInput::SourceGraphic
            ]
        );
        // color-interpolation-filters inherits; a wrong value count makes a
        // pass through filter.
        assert!(!graph.primitives[0].linear_rgb);
        assert!(graph.primitives[3].linear_rgb);
        assert!(matches!(
            graph.primitives[3].kind,
            FilterPrimitiveKind::ColorMatrix { matrix: None, .. }
        ));
    }

    #[test]
    fn css_resource_references_invalidate_their_consumers() {
        // A primitive edit, or an id binding change for a reference that did
        // not resolve yet, must reach the HTML element whose effect reads it.
        let dom = dom(
            r#"<svg><filter id="f"><feFlood id="flood"/></filter><g id="later"></g></svg>
            <div id=t style="filter:url(#f)"></div><div id=u style="filter:url(#nothing)"></div>"#,
        );
        let id = |name| dom.get_by_id(name).unwrap();
        dom.note_svg_resource_use(id("t"), Some(id("f")));
        dom.note_svg_resource_use(id("u"), None);
        assert!(
            dom.svg_dirty_consumers(&[id("flood")], false)
                .contains(&id("t"))
        );
        assert!(
            !dom.svg_dirty_consumers(&[id("flood")], false)
                .contains(&id("u"))
        );
        assert!(
            dom.svg_dirty_consumers(&[id("later")], true)
                .contains(&id("u"))
        );
    }

    #[test]
    fn local_url_references_resolve_by_fragment_in_the_document() {
        let dom =
            dom(r#"<svg><filter id="a b"></filter><filter id=c></filter></svg><div id=t></div>"#);
        let target = dom.get_by_id("t").unwrap();
        let document = url::Url::parse("https://example.com/page.html?q").unwrap();
        assert_eq!(
            dom.svg_resource_target(target, "#a%20b", Some(&document)),
            dom.get_by_id("a b")
        );
        assert_eq!(
            dom.svg_resource_target(target, "page.html?q#c", Some(&document)),
            dom.get_by_id("c")
        );
        assert_eq!(
            dom.svg_resource_target(target, "other.svg#c", Some(&document)),
            None
        );
        assert_eq!(dom.svg_resource_target(target, "#missing", None), None);
    }
}
