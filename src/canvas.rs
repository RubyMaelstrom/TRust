//! Native Canvas 2D backing storage, owned by the canonical DOM.
//!
//! HTML canvas / pixel manipulation / drawing state (local WHATWG snapshot
//! e5071a20, 2026-09-06). Pixel loops stay native: ImageData is never expanded
//! into a JS array or JSON. Presentation encodes at most once per changed bitmap,
//! lazily, through the same image pipeline as other replaced content.

use std::sync::Arc;

use resvg::tiny_skia as sk;
use vello_cpu::color::{ColorSpaceTag, DynamicColor};
use vello_cpu::kurbo::{Affine, Arc as EllipseArc, BezPath, PathEl, Point, Shape};

const MAX_BITMAP_BYTES: usize = 256 * 1024 * 1024;

/// Canvas-neutral gradient data. The API keeps the live JS object; a paint
/// operation snapshots its stops into native memory. Saving native state shares
/// this immutable snapshot instead of duplicating stop arrays.
pub(crate) struct Gradient {
    kind: u8,
    coordinates: [f64; 6],
    stops: Vec<[f32; 5]>,
}

impl Gradient {
    fn vello_paint(&self) -> vello_common::paint::PaintType {
        use vello_cpu::color::{AlphaColor, Srgb};
        use vello_cpu::peniko::{ColorStop, Gradient as PaintGradient, InterpolationAlphaSpace};
        let c = self.coordinates;
        if self.stops.is_empty()
            || (self.kind == 0 && c[0] == c[2] && c[1] == c[3])
            || (self.kind == 1
                && ((c[0] == c[3] && c[1] == c[4] && c[2] == c[5]) || (c[2] == 0. && c[5] == 0.)))
        {
            return AlphaColor::<Srgb>::new([0.; 4]).into();
        }
        let mut stops: Vec<_> = self
            .stops
            .iter()
            .map(|s| ColorStop {
                offset: s[0],
                color: DynamicColor::from_alpha_color(AlphaColor::<Srgb>::new([
                    s[1], s[2], s[3], s[4],
                ])),
            })
            .collect();
        if stops.len() == 1 {
            let color = stops[0].color;
            stops = vec![
                ColorStop { offset: 0., color },
                ColorStop { offset: 1., color },
            ];
        }
        let mut gradient = match self.kind {
            0 => PaintGradient::new_linear((c[0], c[1]), (c[2], c[3])),
            1 => PaintGradient::new_two_point_radial(
                (c[0], c[1]),
                c[2] as f32,
                (c[3], c[4]),
                c[5] as f32,
            ),
            // Rotate a full sweep via its paint transform. Extending the
            // angular interval past 2pi makes Vello pad, not wrap, its seam.
            _ => PaintGradient::new_sweep((c[1], c[2]), 0., std::f32::consts::TAU),
        }
        .with_stops(stops.as_slice());
        gradient.interpolation_alpha_space = InterpolationAlphaSpace::Unpremultiplied;
        gradient.into()
    }

    fn vello_transform(&self) -> Affine {
        let c = self.coordinates;
        if self.kind == 2 {
            Affine::translate((c[1], c[2]))
                * Affine::rotate(c[0])
                * Affine::translate((-c[1], -c[2]))
        } else {
            Affine::IDENTITY
        }
    }

    pub fn from_numbers(numbers: &[f64]) -> Option<Self> {
        if numbers.len() < 7
            || !(numbers.len() - 7).is_multiple_of(5)
            || numbers.iter().any(|n| !n.is_finite())
        {
            return None;
        }
        let kind = numbers[0] as u8;
        if kind > 2 {
            return None;
        }
        let mut coordinates = [0.; 6];
        coordinates.copy_from_slice(&numbers[1..7]);
        let mut stops: Vec<[f32; 5]> = numbers[7..]
            .as_chunks::<5>()
            .0
            .iter()
            .map(|s| {
                [
                    s[0] as f32,
                    s[1] as f32,
                    s[2] as f32,
                    s[3] as f32,
                    s[4] as f32,
                ]
            })
            .collect();
        // Stable order is required for coincident color stops.
        stops.sort_by(|a, b| a[0].total_cmp(&b[0]));
        Some(Self {
            kind,
            coordinates,
            stops,
        })
    }

    fn shader(&self, transform: Affine, alpha: f32) -> sk::Shader<'static> {
        let c = self.coordinates;
        let transparent = sk::Shader::SolidColor(sk::Color::TRANSPARENT);
        // HTML #dom-context-2d-createLinearGradient / -createRadialGradient.
        // Skia's degenerate pad fallback differs, so handle these before it.
        if self.stops.is_empty()
            || (self.kind == 0 && c[0] == c[2] && c[1] == c[3])
            || (self.kind == 1
                && ((c[0] == c[3] && c[1] == c[4] && c[2] == c[5]) || (c[2] == 0. && c[5] == 0.)))
        {
            return transparent;
        }
        let mut stops: Vec<_> = self
            .stops
            .iter()
            .map(|s| {
                sk::GradientStop::new(
                    s[0],
                    sk::Color::from_rgba(s[1], s[2], s[3], s[4] * alpha)
                        .unwrap_or(sk::Color::TRANSPARENT),
                )
            })
            .collect();
        if stops.len() == 1 {
            // A solid shader would incorrectly fill OUTSIDE a radial cone.
            let color = &self.stops[0];
            let color = sk::Color::from_rgba(color[1], color[2], color[3], color[4] * alpha)
                .unwrap_or(sk::Color::TRANSPARENT);
            stops = vec![
                sk::GradientStop::new(0., color),
                sk::GradientStop::new(1., color),
            ];
        }
        let point = |x: f64, y: f64| sk::Point::from_xy(x as f32, y as f32);
        let shader = match self.kind {
            0 => sk::LinearGradient::new(
                point(c[0], c[1]),
                point(c[2], c[3]),
                stops,
                sk::SpreadMode::Pad,
                sk_transform(transform),
            ),
            1 => sk::RadialGradient::new(
                point(c[0], c[1]),
                c[2] as f32,
                point(c[3], c[4]),
                c[5] as f32,
                stops,
                sk::SpreadMode::Pad,
                sk_transform(transform),
            ),
            _ => {
                // HTML conic starts at +x, clockwise, and accepts radians.
                // Rotate the full-turn shader, not its clamped color interval.
                let rotation = Affine::translate((c[1], c[2]))
                    * Affine::rotate(c[0].rem_euclid(std::f64::consts::TAU))
                    * Affine::translate((-c[1], -c[2]));
                sk::SweepGradient::new(
                    point(c[1], c[2]),
                    0.,
                    360.,
                    stops,
                    sk::SpreadMode::Pad,
                    sk_transform(transform * rotation),
                )
            }
        };
        shader.unwrap_or(transparent)
    }
}

#[derive(Clone)]
pub(crate) struct State {
    pub transform: Affine,
    pub fill: [f32; 4],
    pub stroke_color: [f32; 4],
    pub fill_text: String,
    pub stroke_text: String,
    pub gradients: [Option<Arc<Gradient>>; 2],
    pub text: crate::canvas_text::TextState,
    pub shadow: crate::canvas_shadow::Shadow,
    pub alpha: f32,
    pub composite: String,
    pub stroke: sk::Stroke,
    pub dash: Vec<f64>,
    pub dash_offset: f64,
    pub smoothing: bool,
    pub clip: Option<Arc<sk::Mask>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            transform: Affine::IDENTITY,
            fill: [0., 0., 0., 1.],
            stroke_color: [0., 0., 0., 1.],
            fill_text: "#000000".into(),
            stroke_text: "#000000".into(),
            gradients: [None, None],
            text: crate::canvas_text::TextState::default(),
            shadow: crate::canvas_shadow::Shadow::default(),
            alpha: 1.,
            composite: "source-over".into(),
            stroke: sk::Stroke::default(),
            dash: Vec::new(),
            dash_offset: 0.,
            smoothing: true,
            clip: None,
        }
    }
}

pub(crate) struct Canvas {
    pub width: u32,
    pub height: u32,
    pub alpha: bool,
    pub origin_clean: bool,
    /// Captured at Realm bootstrap, independent of author-controlled <base>.
    pub document_origin: String,
    pub state: State,
    stack: Vec<State>,
    path: BezPath,
    current: Option<Point>,
    start: Option<Point>,
    bitmap: Option<sk::Pixmap>,
    presentation: Option<crate::render::CanvasImage>,
    image_handle: crate::render::ImageHandle,
    bitmap_revision: u64,
    pub generation: u64,
}

impl Canvas {
    pub fn new(width: u32, height: u32, alpha: bool) -> Option<Self> {
        let mut out = Self {
            width,
            height,
            alpha,
            origin_clean: true,
            document_origin: String::new(),
            state: State::default(),
            stack: Vec::new(),
            path: BezPath::new(),
            current: None,
            start: None,
            bitmap: None,
            presentation: None,
            image_handle: crate::render::ImageHandle::for_canvas(),
            bitmap_revision: 0,
            generation: 0,
        };
        out.resize(width, height)?;
        Some(out)
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Option<()> {
        self.width = width;
        self.height = height;
        self.bitmap = None;
        self.presentation = None;
        self.bitmap_revision = self.bitmap_revision.wrapping_add(1);
        self.origin_clean = true;
        self.state = State::default();
        self.stack.clear();
        self.begin();
        self.generation = self.generation.wrapping_add(1);
        let bytes = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if bytes > MAX_BITMAP_BYTES {
            return None;
        }
        if width != 0 && height != 0 {
            self.bitmap = sk::Pixmap::new(width, height);
            let bitmap = self.bitmap.as_mut()?;
            if !self.alpha {
                bitmap.fill(sk::Color::BLACK);
            }
        }
        Some(())
    }

    pub fn lost(&self) -> bool {
        self.width != 0 && self.height != 0 && self.bitmap.is_none()
    }

    pub fn retained_bytes(&self) -> usize {
        let mut gradients = std::collections::HashSet::new();
        let gradient_bytes: usize = std::iter::once(&self.state)
            .chain(self.stack.iter())
            .flat_map(|state| state.gradients.iter().flatten())
            .filter(|gradient| gradients.insert(Arc::as_ptr(gradient)))
            .map(|gradient| {
                std::mem::size_of::<Gradient>()
                    + gradient.stops.capacity() * std::mem::size_of::<[f32; 5]>()
            })
            .sum();
        self.bitmap.as_ref().map_or(0, |p| p.data().len())
            + self
                .presentation
                .as_ref()
                .map_or(0, crate::render::CanvasImage::retained_bytes)
            + std::mem::size_of_val(self.path.elements())
            + self.stack.capacity() * std::mem::size_of::<State>()
            + self.state.clip.as_ref().map_or(0, |m| m.data().len())
            + self.document_origin.capacity()
            + gradient_bytes
            + std::iter::once(&self.state)
                .chain(self.stack.iter())
                .map(|state| state.text.retained_bytes() + state.shadow.serialized.capacity())
                .sum::<usize>()
    }

    pub fn save(&mut self) {
        self.stack.push(self.state.clone());
    }
    pub fn restore(&mut self) {
        if let Some(state) = self.stack.pop() {
            self.state = state;
        }
    }
    pub fn begin(&mut self) {
        self.path = BezPath::new();
        self.current = None;
        self.start = None;
    }

    fn point(&self, x: f64, y: f64) -> Point {
        self.state.transform * Point::new(x, y)
    }
    fn move_point(&mut self, p: Point) {
        self.path.move_to(p);
        self.current = Some(p);
        self.start = Some(p);
    }
    fn line_point(&mut self, p: Point) {
        if self.current.is_none() {
            self.move_point(p);
        } else {
            self.path.line_to(p);
            self.current = Some(p);
        }
    }

    pub fn path_command(&mut self, op: &str, n: &[f64]) {
        if n.iter().any(|v| !v.is_finite()) {
            return;
        }
        match op {
            "moveTo" => self.move_point(self.point(n[0], n[1])),
            "lineTo" => self.line_point(self.point(n[0], n[1])),
            "closePath" => {
                if let Some(start) = self.start {
                    self.path.close_path();
                    self.move_point(start);
                }
            }
            "quadraticCurveTo" => {
                let p = self.point(n[0], n[1]);
                if self.current.is_none() {
                    self.move_point(p);
                }
                let end = self.point(n[2], n[3]);
                self.path.quad_to(p, end);
                self.current = Some(end);
            }
            "bezierCurveTo" => {
                let p = self.point(n[0], n[1]);
                if self.current.is_none() {
                    self.move_point(p);
                }
                let end = self.point(n[4], n[5]);
                self.path.curve_to(p, self.point(n[2], n[3]), end);
                self.current = Some(end);
            }
            "rect" => {
                self.move_point(self.point(n[0], n[1]));
                self.line_point(self.point(n[0] + n[2], n[1]));
                self.line_point(self.point(n[0] + n[2], n[1] + n[3]));
                self.line_point(self.point(n[0], n[1] + n[3]));
                self.path_command("closePath", &[]);
            }
            "ellipse" => self.ellipse(n),
            "arcTo" => self.arc_to(n),
            "roundRect" => {
                let old = self.state.transform;
                let (w, h) = (n[2].abs(), n[3].abs());
                self.state.transform *= Affine::translate((n[0], n[1]))
                    * Affine::scale_non_uniform(
                        if n[2] < 0. { -1. } else { 1. },
                        if n[3] < 0. { -1. } else { 1. },
                    );
                let p = std::f64::consts::PI;
                self.path_command("moveTo", &[n[4], 0.]);
                for (cx, cy, rx, ry, start, end) in [
                    (w - n[6], n[7], n[6], n[7], -p / 2., 0.),
                    (w - n[8], h - n[9], n[8], n[9], 0., p / 2.),
                    (n[10], h - n[11], n[10], n[11], p / 2., p),
                    (n[4], n[5], n[4], n[5], p, p * 1.5),
                ] {
                    self.ellipse(&[cx, cy, rx, ry, 0., start, end, 0.]);
                }
                self.path_command("closePath", &[]);
                self.state.transform = old;
                self.move_point(self.point(n[0], n[1]));
            }
            _ => {}
        }
    }

    fn ellipse(&mut self, n: &[f64]) {
        let tau = std::f64::consts::TAU;
        let delta = n[6] - n[5];
        let ccw = n[7] != 0.;
        let sweep = if !ccw && delta >= tau {
            tau
        } else if ccw && -delta >= tau {
            -tau
        } else if ccw {
            -(-delta).rem_euclid(tau)
        } else {
            delta.rem_euclid(tau)
        };
        let arc = EllipseArc::new((n[0], n[1]), (n[2], n[3]), n[5] % tau, sweep, n[4] % tau);
        // Bounded approximation in device space, including enormous author radii.
        let scale = self.state.transform.as_coeffs()[..4]
            .iter()
            .fold(1f64, |a, b| a.max(b.abs()));
        let tolerance = (0.1 / scale).max(n[2].max(n[3]) / 1e8);
        for el in arc.path_elements(tolerance) {
            match self.state.transform * el {
                PathEl::MoveTo(p) => self.line_point(p),
                PathEl::CurveTo(a, b, p) => {
                    self.path.curve_to(a, b, p);
                    self.current = Some(p);
                }
                _ => {}
            }
        }
    }

    fn arc_to(&mut self, n: &[f64]) {
        let corner = Point::new(n[0], n[1]);
        if self.current.is_none() {
            self.move_point(self.state.transform * corner);
        }
        if n[4] < 0. || self.state.transform.determinant() == 0. {
            return;
        }
        let current = self.state.transform.inverse() * self.current.unwrap();
        let a = current - corner;
        let b = Point::new(n[2], n[3]) - corner;
        let cross = a.cross(b);
        if n[4] == 0. || a.hypot() == 0. || b.hypot() == 0. || cross == 0. {
            self.line_point(self.state.transform * corner);
            return;
        }
        let (a, b) = (a / a.hypot(), b / b.hypot());
        let angle = a.dot(b).clamp(-1., 1.).acos();
        let distance = n[4] / (angle / 2.).tan();
        let start = corner + a * distance;
        let end = corner + b * distance;
        let bisector = a + b;
        let center = corner + bisector / bisector.hypot() * (n[4] / (angle / 2.).sin());
        self.ellipse(&[
            center.x,
            center.y,
            n[4],
            n[4],
            0.,
            (start.y - center.y).atan2(start.x - center.x),
            (end.y - center.y).atan2(end.x - center.x),
            if cross > 0. { 1. } else { 0. },
        ]);
    }

    pub fn update_dash(&mut self) {
        self.state.stroke.dash = sk::StrokeDash::new(
            self.state.dash.iter().map(|v| *v as f32).collect(),
            self.state.dash_offset as f32,
        );
    }

    pub fn color(value: &str) -> Option<([f32; 4], String)> {
        let parsed: DynamicColor = value.parse().ok()?;
        let rgba = parsed
            .convert(ColorSpaceTag::Srgb)
            .components
            .map(|v| v.clamp(0., 1.));
        let [r, g, b, a] = rgba.map(|v| (v * 255.).round() as u8);
        let text = if a == 255 {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            format!("rgba({r}, {g}, {b}, {})", rgba[3])
        };
        Some((rgba, text))
    }

    pub fn paint_path(&mut self, stroke: bool, evenodd: bool) {
        self.paint(self.path.clone(), stroke, evenodd, false);
    }

    pub fn external_path(&mut self, commands: &str, op: &str, evenodd: bool) {
        let Ok(commands) = serde_json::from_str::<serde_json::Value>(commands) else {
            return;
        };
        let mut builder = Canvas::new(0, 0, true).unwrap();
        builder.read_path_commands(&commands, 0);
        let path = self.state.transform * builder.path;
        if op == "clipPath" {
            let old = std::mem::replace(&mut self.path, path);
            self.clip(evenodd);
            self.path = old;
        } else {
            self.paint(path, op == "strokePath", evenodd, false);
        }
    }

    fn read_path_commands(&mut self, commands: &serde_json::Value, depth: usize) {
        if depth > 64 {
            return;
        }
        let Some(commands) = commands.as_array() else {
            return;
        };
        for command in commands {
            let Some(op) = command.get(0).and_then(|s| s.as_str()) else {
                continue;
            };
            if op == "svg" {
                let Some(svg) = command.get(1).and_then(|s| s.as_str()) else {
                    continue;
                };
                use svgtypes::SimplePathSegment as S;
                for segment in svgtypes::SimplifyingPathParser::from(svg) {
                    let Ok(segment) = segment else {
                        break;
                    }; // SVG keeps the valid prefix.
                    match segment {
                        S::MoveTo { x, y } => self.path_command("moveTo", &[x, y]),
                        S::LineTo { x, y } => self.path_command("lineTo", &[x, y]),
                        S::CurveTo {
                            x1,
                            y1,
                            x2,
                            y2,
                            x,
                            y,
                        } => self.path_command("bezierCurveTo", &[x1, y1, x2, y2, x, y]),
                        S::Quadratic { x1, y1, x, y } => {
                            self.path_command("quadraticCurveTo", &[x1, y1, x, y])
                        }
                        S::ClosePath => self.path_command("closePath", &[]),
                    }
                }
                if let Some(current) = self.current {
                    self.move_point(current);
                }
            } else if op == "add" {
                let mut child_path = Canvas::new(0, 0, true).unwrap();
                child_path.state.transform = self.state.transform;
                if let Some(matrix) = command.get(2).and_then(|m| m.as_array())
                    && matrix.len() == 6
                {
                    let values = std::array::from_fn(|i| matrix[i].as_f64().unwrap_or(f64::NAN));
                    if values.iter().all(|v| v.is_finite()) {
                        child_path.state.transform *= Affine::new(values);
                    } else {
                        continue;
                    }
                }
                if let Some(child) = command.get(1) {
                    child_path.read_path_commands(child, depth + 1);
                }
                self.path.extend(child_path.path.elements().iter().copied());
                if let Some(current) = child_path.current {
                    self.move_point(current);
                }
            } else if let Some(values) = command.get(1).and_then(|v| v.as_array()) {
                let values: Vec<_> = values
                    .iter()
                    .map(|v| v.as_f64().unwrap_or(f64::NAN))
                    .collect();
                let length = match op {
                    "closePath" => 0,
                    "moveTo" | "lineTo" => 2,
                    "quadraticCurveTo" | "rect" => 4,
                    "arcTo" => 5,
                    "bezierCurveTo" => 6,
                    "ellipse" => 8,
                    "roundRect" => 12,
                    _ => continue,
                };
                if values.len() == length {
                    self.path_command(op, &values);
                }
            }
        }
    }

    pub fn rectangle(&mut self, op: &str, n: &[f64]) {
        if n.iter().any(|v| !v.is_finite()) {
            return;
        }
        let [x, y, w, h] = [n[0], n[1], n[2], n[3]];
        if op != "strokeRect" && (w == 0. || h == 0.) {
            return;
        }
        let mut path = BezPath::new();
        path.move_to(self.point(x, y));
        if op == "strokeRect" && (w == 0. || h == 0.) {
            path.line_to(self.point(x + w, y + h));
        } else {
            path.line_to(self.point(x + w, y));
            path.line_to(self.point(x + w, y + h));
            path.line_to(self.point(x, y + h));
            path.close_path();
        }
        self.paint(path, op == "strokeRect", false, op == "clearRect");
    }

    pub fn draw_text(&mut self, text: &crate::canvas_text::PreparedText, n: &[f64], stroke: bool) {
        if n.len() < 2
            || n.iter().any(|x| !x.is_finite())
            || n.get(2).is_some_and(|x| *x <= 0.)
            || text.shaped.runs.is_empty()
            || self.state.transform.determinant() == 0.
        {
            return;
        }
        let Some(bitmap) = self.bitmap.as_mut() else {
            return;
        };
        let Some(mut layer) = sk::Pixmap::new(self.width, self.height) else {
            return;
        };
        crate::canvas_shadow::paint(bitmap, &self.state, |layer, shift| {
            Self::text_source(layer, &self.state, text, n, stroke, shift);
        });
        Self::text_source(&mut layer, &self.state, text, n, stroke, Affine::IDENTITY);
        composite_full(
            bitmap,
            &layer,
            blend(&self.state.composite).unwrap_or_default(),
            self.state.clip.as_deref(),
        );
        self.changed();
    }

    fn text_source(
        layer: &mut sk::Pixmap,
        state: &State,
        text: &crate::canvas_text::PreparedText,
        n: &[f64],
        stroke: bool,
        shift: Affine,
    ) {
        use vello_cpu::kurbo::{Cap, Diagonal2, Join, Stroke};
        use vello_cpu::{RenderContext, Resources};
        let (width, height) = (layer.width(), layer.height());
        let ctm = shift * state.transform;
        let scale = n
            .get(2)
            .filter(|width| **width < f64::from(text.shaped.advance))
            .map_or(1., |width| *width / f64::from(text.shaped.advance));
        let transform = ctm
            * Affine::translate((n[0] - text.anchor_x * scale, n[1] + text.baseline_y))
            * Affine::scale_non_uniform(scale, 1.);
        let color = if stroke {
            state.stroke_color
        } else {
            state.fill
        };
        let paint: vello_common::paint::PaintType =
            if let Some(gradient) = &state.gradients[usize::from(stroke)] {
                gradient.vello_paint()
            } else {
                vello_cpu::color::AlphaColor::<vello_cpu::color::Srgb>::new(color).into()
            };
        let paint_transform = transform.inverse()
            * ctm
            * state.gradients[usize::from(stroke)]
                .as_ref()
                .map_or(Affine::IDENTITY, |gradient| gradient.vello_transform());
        let cap = match state.stroke.line_cap {
            sk::LineCap::Butt => Cap::Butt,
            sk::LineCap::Round => Cap::Round,
            sk::LineCap::Square => Cap::Square,
        };
        let join = match state.stroke.line_join {
            sk::LineJoin::Round => Join::Round,
            sk::LineJoin::Bevel => Join::Bevel,
            _ => Join::Miter,
        };
        let pen = Stroke::new(f64::from(state.stroke.width))
            .with_caps(cap)
            .with_join(join)
            .with_miter_limit(f64::from(state.stroke.miter_limit))
            .with_dashes(state.dash_offset, state.dash.iter().copied());
        let mut resources = Resources::new();
        let mut stroke_cache = glifo::GlyphPrepCache::default();
        // Vello uses u16 surfaces. Tile instead of silently omitting text on a
        // wide canvas; each scratch surface is bounded to 16 MiB of RGBA pixels.
        for top in (0..height).step_by(2048) {
            for left in (0..width).step_by(2048) {
                let w = (width - left).min(2048) as u16;
                let h = (height - top).min(2048) as u16;
                let mut context = RenderContext::new(w, h);
                let local = Affine::translate((-(left as f64), -(top as f64))) * transform;
                context.set_transform(local);
                context.set_paint(paint.clone());
                context.set_paint_transform(paint_transform);
                context.set_stroke(pen.clone());
                for run in &text.shaped.runs {
                    if stroke {
                        crate::canvas_text::stroke_run(
                            &mut context,
                            &mut stroke_cache,
                            run,
                            text.shaped.baseline,
                            Affine::translate((-(left as f64), -(top as f64))) * ctm,
                        );
                        continue;
                    }
                    let mut builder = context
                        .glyph_run(&mut resources, run.font.data())
                        .font_size(run.font_size)
                        .normalized_coords(&run.normalized_coords)
                        .hint(false);
                    if run.synth_bold {
                        let amount = f64::from(run.font_size) * 0.025;
                        builder = builder.font_embolden(glifo::FontEmbolden::new(Diagonal2::new(
                            amount, amount,
                        )));
                    }
                    if let Some(degrees) = run.synth_skew_degrees {
                        builder = builder.glyph_transform(Affine::skew(
                            f64::from(degrees).to_radians().tan(),
                            0.,
                        ));
                    }
                    let glyphs = run.glyphs.iter().map(|glyph| vello_cpu::Glyph {
                        id: glyph.id,
                        x: glyph.x,
                        y: glyph.y - text.shaped.baseline,
                    });
                    builder.fill_glyphs(glyphs);
                }
                let mut pixels = vello_common::pixmap::Pixmap::new(w, h);
                context.render(&mut pixels, &mut resources);
                for (y, row) in pixels
                    .data_as_u8_slice()
                    .chunks_exact(w as usize * 4)
                    .enumerate()
                {
                    let start = ((top as usize + y) * width as usize + left as usize) * 4;
                    layer.data_mut()[start..start + row.len()].copy_from_slice(row);
                }
            }
        }
        if state.alpha != 1. {
            for byte in layer.data_mut() {
                *byte = (f32::from(*byte) * state.alpha).round() as u8;
            }
        }
    }

    fn paint(&mut self, path: BezPath, stroke: bool, evenodd: bool, clear: bool) {
        let Some(bitmap) = self.bitmap.as_mut() else {
            return;
        };
        let mut paint = sk::Paint::default();
        let color = if stroke {
            self.state.stroke_color
        } else {
            self.state.fill
        };
        paint.set_color(
            sk::Color::from_rgba(color[0], color[1], color[2], color[3] * self.state.alpha)
                .unwrap_or(sk::Color::TRANSPARENT),
        );
        if !clear && let Some(gradient) = &self.state.gradients[usize::from(stroke)] {
            // Strokes are sent to tiny-skia in local coordinates; its painter
            // transforms BOTH geometry and shader. Fills already use device
            // paths, so only their shader needs the current transform here.
            let transform = if stroke {
                Affine::IDENTITY
            } else {
                self.state.transform
            };
            paint.shader = gradient.shader(transform, self.state.alpha);
        }
        // tiny-skia masks modulate source alpha; Clear ignores source alpha,
        // so express rectangle erasure as DestinationOut with an opaque source.
        // This also keeps clearRect independent of the drawing color/alpha.
        if clear {
            paint.set_color(sk::Color::BLACK);
        }
        paint.blend_mode = if clear {
            sk::BlendMode::DestinationOut
        } else {
            blend(&self.state.composite).unwrap_or_default()
        };
        let rule = if evenodd {
            sk::FillRule::EvenOdd
        } else {
            sk::FillRule::Winding
        };
        let clip = self.state.clip.as_deref();
        // Canvas default paths already contain transformed coordinates. Stroke
        // thickness is transformed at paint time, even for an old default path.
        let (path, transform) = if stroke {
            if self.state.transform.determinant() == 0. {
                return;
            }
            (
                self.state.transform.inverse() * path,
                sk_transform(self.state.transform),
            )
        } else {
            (path, sk::Transform::identity())
        };
        let Some(path) = sk_path(&path) else {
            return;
        };
        if !clear && self.state.shadow.drawn() {
            let mut source_paint = paint.clone();
            source_paint.blend_mode = sk::BlendMode::SourceOver;
            crate::canvas_shadow::paint(bitmap, &self.state, |layer, shift| {
                let matrix = if stroke {
                    shift * self.state.transform
                } else {
                    shift
                };
                if stroke {
                    layer.stroke_path(
                        &path,
                        &source_paint,
                        &self.state.stroke,
                        sk_transform(matrix),
                        None,
                    );
                } else {
                    layer.fill_path(&path, &source_paint, rule, sk_transform(matrix), None);
                }
            });
        }
        // Operators that discard the destination outside the source must see a
        // full transparent source bitmap, not just the shape's covered pixels.
        let full_source = needs_full_source(paint.blend_mode);
        if full_source && !clear {
            let Some(mut layer) = sk::Pixmap::new(self.width, self.height) else {
                return;
            };
            let mode = paint.blend_mode;
            paint.blend_mode = sk::BlendMode::SourceOver;
            if stroke {
                layer.stroke_path(&path, &paint, &self.state.stroke, transform, None);
            } else {
                layer.fill_path(&path, &paint, rule, transform, None);
            }
            composite_full(bitmap, &layer, mode, clip);
        } else if stroke {
            bitmap.stroke_path(&path, &paint, &self.state.stroke, transform, clip);
        } else {
            bitmap.fill_path(&path, &paint, rule, transform, clip);
        }
        self.changed();
    }

    pub fn clip(&mut self, evenodd: bool) {
        let Some(mut mask) = sk::Mask::new(self.width, self.height) else {
            return;
        };
        let rule = if evenodd {
            sk::FillRule::EvenOdd
        } else {
            sk::FillRule::Winding
        };
        if let Some(path) = sk_path(&self.path) {
            mask.fill_path(&path, rule, true, sk::Transform::identity());
        }
        if let Some(old) = &self.state.clip {
            for (out, old) in mask.data_mut().iter_mut().zip(old.data()) {
                *out = ((*out as u32 * *old as u32 + 127) / 255) as u8;
            }
        }
        self.state.clip = Some(Arc::new(mask));
    }

    fn changed(&mut self) {
        self.bitmap_revision = self.bitmap_revision.wrapping_add(1);
        if !self.alpha
            && let Some(bitmap) = self.bitmap.as_mut()
        {
            for pixel in bitmap.data_mut().as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }
        self.presentation = None;
    }

    /// HTML put-pixels algorithm: dirty-rectangle clipping in source coordinates;
    /// no transform, compositing, clipping path, or global alpha.
    pub fn put(
        &mut self,
        bytes: &[u8],
        width: u32,
        height: u32,
        n: &[f64],
        half: bool,
        color_space: u8,
    ) {
        let Some(bitmap) = self.bitmap.as_mut() else {
            return;
        };
        let stride = if half { 8 } else { 4 };
        if bytes.len() != width as usize * height as usize * stride {
            return;
        }
        let (dx, dy, mut x, mut y, mut w, mut h) = (
            n[0] as i64,
            n[1] as i64,
            n[2] as i64,
            n[3] as i64,
            n[4] as i64,
            n[5] as i64,
        );
        if w < 0 {
            x += w;
            w = -w;
        }
        if h < 0 {
            y += h;
            h = -h;
        }
        let right = (x + w).min(width as i64).min(self.width as i64 - dx);
        let bottom = (y + h).min(height as i64).min(self.height as i64 - dy);
        x = x.max(0).max(-dx);
        y = y.max(0).max(-dy);
        for sy in y..bottom {
            for sx in x..right {
                let source = (sy * width as i64 + sx) as usize * stride;
                let target = (((sy + dy) * self.width as i64 + sx + dx) * 4) as usize;
                let mut rgba = [0.; 4];
                for c in 0..4 {
                    rgba[c] = if half {
                        half::f16::from_ne_bytes([bytes[source + c * 2], bytes[source + c * 2 + 1]])
                            .to_f32()
                    } else {
                        bytes[source + c] as f32 / 255.
                    };
                }
                let rgb = convert([rgba[0], rgba[1], rgba[2]], color_space, 0);
                let alpha = if self.alpha {
                    (rgba[3].clamp(0., 1.) * 255.).round() as u8
                } else {
                    255
                };
                let out = &mut bitmap.data_mut()[target..target + 4];
                for c in 0..3 {
                    out[c] = (rgb[c].clamp(0., 1.) * alpha as f32).round() as u8;
                }
                out[3] = alpha;
            }
        }
        self.changed();
    }

    pub fn get(&self, x: i64, y: i64, width: u32, height: u32) -> Option<Vec<u8>> {
        let len = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if len > MAX_BITMAP_BYTES {
            return None;
        }
        let mut out = vec![0; len];
        let Some(bitmap) = &self.bitmap else {
            return Some(out);
        };
        for sy in y.max(0)..(y + height as i64).min(self.height as i64) {
            for sx in x.max(0)..(x + width as i64).min(self.width as i64) {
                let pixel = bitmap.pixels()[(sy * self.width as i64 + sx) as usize].demultiply();
                let target = (((sy - y) * width as i64 + sx - x) * 4) as usize;
                out[target..target + 4].copy_from_slice(&[
                    pixel.red(),
                    pixel.green(),
                    pixel.blue(),
                    pixel.alpha(),
                ]);
            }
        }
        Some(out)
    }

    pub fn snapshot(&self) -> Option<sk::Pixmap> {
        self.bitmap.clone()
    }

    pub fn get_converted(
        &self,
        x: i64,
        y: i64,
        width: u32,
        height: u32,
        half: bool,
        color_space: u8,
    ) -> Option<Vec<u8>> {
        let bytes = self.get(x, y, width, height)?;
        if !half && color_space == 0 {
            return Some(bytes);
        }
        let mut out = Vec::with_capacity(bytes.len() * if half { 2 } else { 1 });
        for pixel in bytes.as_chunks::<4>().0 {
            let rgb = convert(
                [
                    pixel[0] as f32 / 255.,
                    pixel[1] as f32 / 255.,
                    pixel[2] as f32 / 255.,
                ],
                0,
                color_space,
            );
            for value in [rgb[0], rgb[1], rgb[2], pixel[3] as f32 / 255.] {
                if half {
                    out.extend_from_slice(&half::f16::from_f32(value).to_ne_bytes());
                } else {
                    out.push((value.clamp(0., 1.) * 255.).round() as u8);
                }
            }
        }
        Some(out)
    }

    pub fn draw(&mut self, source: &sk::Pixmap, n: &[f64]) {
        let Some(bitmap) = self.bitmap.as_mut() else {
            return;
        };
        if n.iter().any(|n| !n.is_finite()) {
            return;
        }
        let mut r = [n[0], n[1], n[2], n[3], n[4], n[5], n[6], n[7]];
        for (p, d) in [(0, 2), (1, 3), (4, 6), (5, 7)] {
            if r[d] < 0. {
                r[p] += r[d];
                r[d] = -r[d];
            }
        }
        let [sx, sy, sw, sh, dx, dy, dw, dh] = r;
        if sw == 0. || sh == 0. || dw == 0. || dh == 0. {
            return;
        }
        let x0 = sx.max(0.);
        let y0 = sy.max(0.);
        let x1 = (sx + sw).min(source.width() as f64);
        let y1 = (sy + sh).min(source.height() as f64);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let mapping = Affine::translate((dx, dy))
            * Affine::scale_non_uniform(dw / sw, dh / sh)
            * Affine::translate((-sx, -sy));
        let transform = self.state.transform * mapping;
        let Some(rect) = sk::Rect::from_ltrb(x0 as f32, y0 as f32, x1 as f32, y1 as f32) else {
            return;
        };
        let quality = if self.state.smoothing {
            sk::FilterQuality::Bilinear
        } else {
            sk::FilterQuality::Nearest
        };
        let mut paint = sk::Paint {
            shader: sk::Pattern::new(
                source.as_ref(),
                sk::SpreadMode::Pad,
                quality,
                self.state.alpha,
                sk::Transform::identity(),
            ),
            blend_mode: blend(&self.state.composite).unwrap_or_default(),
            ..Default::default()
        };
        if self.state.shadow.drawn() {
            let mut source_paint = paint.clone();
            source_paint.blend_mode = sk::BlendMode::SourceOver;
            crate::canvas_shadow::paint(bitmap, &self.state, |layer, shift| {
                layer.fill_rect(rect, &source_paint, sk_transform(shift * transform), None);
            });
        }
        if needs_full_source(paint.blend_mode) {
            let Some(mut layer) = sk::Pixmap::new(self.width, self.height) else {
                return;
            };
            let mode = paint.blend_mode;
            paint.blend_mode = sk::BlendMode::SourceOver;
            layer.fill_rect(rect, &paint, sk_transform(transform), None);
            composite_full(bitmap, &layer, mode, self.state.clip.as_deref());
        } else {
            bitmap.fill_rect(
                rect,
                &paint,
                sk_transform(transform),
                self.state.clip.as_deref(),
            );
        }
        self.changed();
    }

    pub(crate) fn image(&mut self) -> Option<crate::render::CanvasImage> {
        if let Some(image) = &self.presentation {
            return Some(image.clone());
        }
        self.bitmap.as_ref()?;
        let rgba = self.get(0, 0, self.width, self.height)?;
        let image = crate::render::CanvasImage::new(
            self.image_handle,
            self.bitmap_revision,
            crate::render::ImageResource {
                width: self.width,
                height: self.height,
                has_alpha: rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 255),
                rgba: rgba.into(),
            },
        );
        self.presentation = Some(image.clone());
        Some(image)
    }

    pub fn data_url(&mut self) -> String {
        self.image()
            .and_then(|image| image.data_url())
            .unwrap_or_else(|| "data:,".into())
    }
}

// CSS Color 4 color-conversion algorithm, relative-colorimetric; all four
// predefined canvas spaces use D65. Linear P3 shares the sRGB transfer curve.
pub(crate) fn convert(mut rgb: [f32; 3], source: u8, destination: u8) -> [f32; 3] {
    if source == destination {
        return rgb;
    }
    let tag = |space| match space {
        1 => ColorSpaceTag::LinearSrgb,
        2 | 3 => ColorSpaceTag::DisplayP3,
        _ => ColorSpaceTag::Srgb,
    };
    if source == 3 {
        rgb = rgb.map(|v| {
            if v.abs() <= 0.0031308 {
                12.92 * v
            } else {
                v.signum() * (1.055 * v.abs().powf(1. / 2.4) - 0.055)
            }
        });
    }
    rgb = tag(source).convert(tag(destination), rgb);
    if destination == 3 {
        rgb = rgb.map(|v| {
            if v.abs() <= 0.04045 {
                v / 12.92
            } else {
                v.signum() * ((v.abs() + 0.055) / 1.055).powf(2.4)
            }
        });
    }
    rgb
}

fn sk_transform(matrix: Affine) -> sk::Transform {
    let [a, b, c, d, e, f] = matrix.as_coeffs().map(|v| v as f32);
    sk::Transform::from_row(a, b, c, d, e, f)
}

pub(crate) fn needs_full_source(mode: sk::BlendMode) -> bool {
    matches!(
        mode,
        sk::BlendMode::Clear
            | sk::BlendMode::Source
            | sk::BlendMode::SourceIn
            | sk::BlendMode::SourceOut
            | sk::BlendMode::DestinationIn
            | sk::BlendMode::DestinationAtop
    )
}

fn composite_full(
    bitmap: &mut sk::Pixmap,
    layer: &sk::Pixmap,
    mode: sk::BlendMode,
    clip: Option<&sk::Mask>,
) {
    composite_region(bitmap, layer, 0, 0, mode, clip);
}

/// Composite a disjoint source region. Coverage interpolates the composited
/// result with the original destination, not merely the source alpha (important
/// for copy/source-in/etc.). Scratch copies are bounded by the source tile.
pub(crate) fn composite_region(
    bitmap: &mut sk::Pixmap,
    layer: &sk::Pixmap,
    left: u32,
    top: u32,
    mode: sk::BlendMode,
    clip: Option<&sk::Mask>,
) {
    debug_assert!(left + layer.width() <= bitmap.width());
    debug_assert!(top + layer.height() <= bitmap.height());
    let stride = bitmap.width() as usize;
    let width = layer.width() as usize;
    let height = layer.height() as usize;
    let original = clip.map(|_| {
        let mut saved = Vec::with_capacity(width * height * 4);
        for y in top as usize..top as usize + height {
            let start = (y * stride + left as usize) * 4;
            saved.extend_from_slice(&bitmap.data()[start..start + width * 4]);
        }
        saved
    });
    bitmap.draw_pixmap(
        left as i32,
        top as i32,
        layer.as_ref(),
        &sk::PixmapPaint {
            blend_mode: mode,
            ..Default::default()
        },
        sk::Transform::identity(),
        None,
    );
    if let (Some(clip), Some(original)) = (clip, original) {
        for (y, original) in original.chunks_exact(width * 4).enumerate() {
            let start = (top as usize + y) * stride + left as usize;
            let row = &mut bitmap.data_mut()[start * 4..(start + width) * 4];
            for ((out, original), &coverage) in row
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(original.as_chunks::<4>().0.iter())
                .zip(&clip.data()[start..start + width])
            {
                for channel in 0..4 {
                    out[channel] = ((out[channel] as u32 * coverage as u32
                        + original[channel] as u32 * (255 - coverage as u32)
                        + 127)
                        / 255) as u8;
                }
            }
        }
    }
}

fn sk_path(path: &BezPath) -> Option<sk::Path> {
    let mut out = sk::PathBuilder::new();
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => out.move_to(p.x as f32, p.y as f32),
            PathEl::LineTo(p) => out.line_to(p.x as f32, p.y as f32),
            PathEl::QuadTo(a, p) => out.quad_to(a.x as f32, a.y as f32, p.x as f32, p.y as f32),
            PathEl::CurveTo(a, b, p) => out.cubic_to(
                a.x as f32, a.y as f32, b.x as f32, b.y as f32, p.x as f32, p.y as f32,
            ),
            PathEl::ClosePath => out.close(),
        }
    }
    out.finish()
}

pub(crate) fn blend(name: &str) -> Option<sk::BlendMode> {
    use sk::BlendMode::*;
    Some(match name {
        "source-over" => SourceOver,
        "destination-over" => DestinationOver,
        "source-in" => SourceIn,
        "destination-in" => DestinationIn,
        "source-out" => SourceOut,
        "destination-out" => DestinationOut,
        "source-atop" => SourceAtop,
        "destination-atop" => DestinationAtop,
        "clear" => Clear,
        "copy" => Source,
        "xor" => Xor,
        "lighter" => Plus,
        "multiply" => Multiply,
        "screen" => Screen,
        "overlay" => Overlay,
        "darken" => Darken,
        "lighten" => Lighten,
        "color-dodge" => ColorDodge,
        "color-burn" => ColorBurn,
        "hard-light" => HardLight,
        "soft-light" => SoftLight,
        "difference" => Difference,
        "exclusion" => Exclusion,
        "hue" => Hue,
        "saturation" => Saturation,
        "color" => Color,
        "luminosity" => Luminosity,
        _ => return None,
    })
}
