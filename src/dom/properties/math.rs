//! CSS Values 4 #calc-syntax, #calc-type-checking, #calc-computed-value and
//! #calc-serialize. Numeric dimensions survive intermediate products; a
//! length-percentage retains its percentage basis until used-value time.
use super::*;

// length, angle, time, frequency, resolution, flex, percentage
type Dim = [i8; 7];
const NUMBER: Dim = [0; 7];
const LENGTH: Dim = [1, 0, 0, 0, 0, 0, 0];
const ANGLE: Dim = [0, 1, 0, 0, 0, 0, 0];
const TIME: Dim = [0, 0, 1, 0, 0, 0, 0];
const RESOLUTION: Dim = [0, 0, 0, 0, 1, 0, 0];
const PERCENT: Dim = [0, 0, 0, 0, 0, 0, 1];

#[derive(Clone, Debug)]
enum Node {
    Keyword(String),
    Value(f64, &'static str),
    Sum(Vec<Node>),
    Product(Box<Node>, Box<Node>, bool),
    Function(String, Vec<Node>),
}

#[derive(Clone, Debug)]
struct Numeric {
    dim: Dim,
    node: Node,
}

pub(super) fn number(value: f64) -> String {
    if value == 0. {
        "0".into()
    } else if value.is_nan() {
        "NaN".into()
    } else if value == f64::INFINITY {
        "infinity".into()
    } else if value == f64::NEG_INFINITY {
        "-infinity".into()
    } else {
        // CSS Values 4 #numeric-types: fit the supported finite range rather
        // than emitting Rust's non-CSS `inf` spelling after a narrowing cast.
        let rounded = value.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32;
        if rounded == 0. {
            f32::from_bits(1).copysign(value as f32).to_string()
        } else {
            rounded.to_string()
        }
    }
}

fn unit(dim: Dim) -> &'static str {
    match dim {
        NUMBER => "",
        LENGTH => "px",
        ANGLE => "deg",
        TIME => "s",
        RESOLUTION => "dppx",
        PERCENT => "%",
        [0, 0, 0, 1, 0, 0, 0] => "Hz",
        [0, 0, 0, 0, 0, 1, 0] => "fr",
        _ => "",
    }
}

impl Node {
    fn scalar(&self) -> Option<(f64, &'static str)> {
        if let Self::Value(v, u) = self {
            Some((*v, *u))
        } else {
            None
        }
    }
    fn contains_percentage(&self) -> bool {
        match self {
            Self::Keyword(_) => false,
            Self::Value(_, u) => *u == "%",
            Self::Sum(v) | Self::Function(_, v) => v.iter().any(Self::contains_percentage),
            Self::Product(a, b, _) => a.contains_percentage() || b.contains_percentage(),
        }
    }
    fn text(&self) -> String {
        match self {
            Self::Keyword(value) => value.clone(),
            Self::Value(v, u) if !v.is_finite() && !u.is_empty() => {
                format!("({} * 1{u})", number(*v))
            }
            Self::Value(v, u) => format!("{}{u}", number(*v)),
            Self::Sum(values) => {
                let mut out = String::new();
                for (index, value) in values.iter().enumerate() {
                    if index == 0 {
                        out.push_str(&value.text());
                    } else if let Self::Value(n, u) = value
                        && *n < 0.
                    {
                        out.push_str(&format!(" - {}{u}", number(-n)));
                    } else {
                        out.push_str(" + ");
                        out.push_str(&value.text());
                    }
                }
                format!("({out})")
            }
            Self::Product(a, b, divide) => format!(
                "({} {} {})",
                a.text(),
                if *divide { "/" } else { "*" },
                b.text()
            ),
            Self::Function(name, args) => format!(
                "{name}({})",
                args.iter().map(Self::text).collect::<Vec<_>>().join(", ")
            ),
        }
    }
    fn computed_text(&self) -> String {
        match self {
            Self::Value(v, u) if !v.is_finite() => {
                if u.is_empty() {
                    format!("calc({})", number(*v))
                } else {
                    format!("calc({} * 1{u})", number(*v))
                }
            }
            Self::Value(..) | Self::Function(..) => self.text(),
            _ => format!("calc{}", self.text()),
        }
    }
}

impl Numeric {
    fn literal(v: f64, dim: Dim, unit: &'static str) -> Self {
        Self {
            dim,
            node: Node::Value(v, unit),
        }
    }
    fn sum(self, other: Self, subtract: bool) -> Option<Self> {
        if self.dim != other.dim {
            return None;
        }
        let dim = self.dim;
        let mut values = match self.node {
            Node::Sum(values) => values,
            value => vec![value],
        };
        let other = if subtract {
            other.scale(-1.).node
        } else {
            other.node
        };
        let others = match other {
            Node::Sum(values) => values,
            value => vec![value],
        };
        for value in others {
            if let Node::Value(n, u) = value {
                if let Some(Node::Value(old, _)) = values
                    .iter_mut()
                    .find(|v| matches!(v,Node::Value(_,old_unit) if *old_unit == u))
                {
                    *old += n;
                } else {
                    values.push(Node::Value(n, u));
                }
            } else {
                values.push(value);
            }
        }
        // Retain zero percentages: they still carry a used-value dependency.
        values.sort_by_key(|node| match node {
            Node::Value(_, u) => (0, *u),
            _ => (1, ""),
        });
        let node = if values.len() == 1 {
            values.pop()?
        } else {
            Node::Sum(values)
        };
        Some(Self { dim, node })
    }
    fn scale(mut self, factor: f64) -> Self {
        self.node = match self.node {
            Node::Value(n, u) => Node::Value(n * factor, u),
            Node::Sum(v) => Node::Sum(
                v.into_iter()
                    .map(|n| {
                        Self {
                            dim: self.dim,
                            node: n,
                        }
                        .scale(factor)
                        .node
                    })
                    .collect(),
            ),
            node => Node::Product(Box::new(Node::Value(factor, "")), Box::new(node), false),
        };
        self
    }
    fn product(self, other: Self, divide: bool, hint: Option<Dim>) -> Option<Self> {
        let mut dim = NUMBER;
        for (i, d) in dim.iter_mut().enumerate() {
            *d = if divide {
                self.dim[i].checked_sub(other.dim[i])?
            } else {
                self.dim[i].checked_add(other.dim[i])?
            };
        }
        if let Node::Value(n, "") = other.node
            && other.dim == NUMBER
        {
            let mut scaled = self.scale(if divide { 1. / n } else { n });
            scaled.dim = dim;
            return Some(scaled);
        }
        if !divide
            && let Node::Value(n, "") = self.node
            && self.dim == NUMBER
        {
            let mut scaled = other.scale(n);
            scaled.dim = dim;
            return Some(scaled);
        }
        if let (Some((a, _)), Some((b, _))) = (self.node.scalar(), other.node.scalar())
            && (hint.is_none()
                || !self.node.contains_percentage() && !other.node.contains_percentage())
        {
            return Some(Self::literal(
                if divide { a / b } else { a * b },
                dim,
                unit(dim),
            ));
        }
        Some(Self {
            dim,
            node: Node::Product(Box::new(self.node), Box::new(other.node), divide),
        })
    }
}

pub(super) fn parse<'i>(
    p: &mut Parser<'i, '_>,
    kind: &Kind,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    parse_range(p, kind, ctx, depth, f64::NEG_INFINITY, f64::INFINITY)
}

/// Apply a grammar's range only at its top-level numeric argument. A literal
/// outside that range is invalid; a calculation is clamped (CSS Values 4
/// #functional-numeric and #calc-ieee). Unresolved percentages keep their
/// calculation until the consuming property supplies a used-value basis.
pub(super) fn parse_range<'i>(
    p: &mut Parser<'i, '_>,
    kind: &Kind,
    ctx: &Context<'_>,
    depth: usize,
    mut minimum: f64,
    maximum: f64,
) -> ParseResult<'i, String> {
    let hint = match kind {
        Kind::LengthPercentage => Some(LENGTH),
        Kind::AnglePercentage => Some(ANGLE),
        _ => None,
    };
    let state = p.state();
    let first = p.next()?.clone();
    let literal_integer = matches!(
        first,
        Token::Number {
            int_value: Some(_),
            ..
        }
    );
    let calculation = matches!(first, Token::Function(_));
    p.reset(&state);
    let mut value = value(p, ctx, hint, depth, false)?;
    let expected = match kind {
        Kind::Length | Kind::LengthPercentage => LENGTH,
        Kind::Percentage => PERCENT,
        Kind::Angle | Kind::AnglePercentage => ANGLE,
        Kind::Time => TIME,
        Kind::Resolution => RESOLUTION,
        Kind::Number | Kind::Integer => NUMBER,
        _ => return Err(p.new_custom_error(())),
    };
    if matches!(kind, Kind::Length | Kind::LengthPercentage)
        && value.dim == NUMBER
        && matches!(value.node, Node::Value(0., ""))
    {
        // Unitless zero is a length only as a literal, not calc(0).
        let mut probe = ParserInput::new(p.slice_from(state.position()));
        if matches!(
            Parser::new(&mut probe).next(),
            Ok(Token::Number { value: 0., .. })
        ) {
            value = Numeric::literal(0., LENGTH, "px");
        }
    }
    if value.dim != expected {
        return Err(p.new_custom_error(()));
    }
    if *kind == Kind::Integer {
        if !literal_integer && !calculation {
            return Err(p.new_custom_error(()));
        }
        if let Node::Value(n, u) = value.node {
            value.node = Node::Value((n + 0.5).floor(), u);
        }
    }
    if *kind == Kind::Resolution {
        minimum = minimum.max(0.);
    }
    if let Node::Value(n, u) = &mut value.node {
        if calculation && n.is_nan() {
            *n = 0.;
        }
        if !calculation && (*n < minimum || *n > maximum) {
            return Err(p.new_custom_error(()));
        }
        *n = n.clamp(minimum, maximum);
        let supported = f64::from(f32::MAX);
        if *u == "deg" && n.abs() > supported {
            // Overflowing angles clamp to a whole number of turns.
            *n = ((supported / 360.).floor() * 360.).copysign(*n);
        } else {
            *n = n.clamp(-supported, supported);
        }
    }
    Ok(value.node.computed_text())
}

fn value<'i>(
    p: &mut Parser<'i, '_>,
    ctx: &Context<'_>,
    hint: Option<Dim>,
    depth: usize,
    group: bool,
) -> ParseResult<'i, Numeric> {
    if depth > MAX_DEPTH {
        return Err(p.new_custom_error(()));
    }
    let value = match p.next()?.clone() {
        Token::Number { value, .. } => Numeric::literal(value.into(), NUMBER, ""),
        Token::Percentage { unit_value, .. } => {
            Numeric::literal(f64::from(unit_value) * 100., hint.unwrap_or(PERCENT), "%")
        }
        Token::Dimension {
            value, unit: raw, ..
        } => {
            let raw = raw.to_ascii_lowercase();
            let (scale, dim, unit) = match raw.as_str() {
                "deg" => (1., ANGLE, "deg"),
                "grad" => (0.9, ANGLE, "deg"),
                "rad" => (180. / std::f64::consts::PI, ANGLE, "deg"),
                "turn" => (360., ANGLE, "deg"),
                "s" => (1., TIME, "s"),
                "ms" => (0.001, TIME, "s"),
                "dppx" | "x" => (1., RESOLUTION, "dppx"),
                "dpi" => (1. / 96., RESOLUTION, "dppx"),
                "dpcm" => (2.54 / 96., RESOLUTION, "dppx"),
                "hz" => (1., [0, 0, 0, 1, 0, 0, 0], "Hz"),
                "khz" => (1000., [0, 0, 0, 1, 0, 0, 0], "Hz"),
                "fr" => (1., [0, 0, 0, 0, 0, 1, 0], "fr"),
                _ => (
                    values::length_scale(&raw, ctx).ok_or_else(|| p.new_custom_error(()))?,
                    LENGTH,
                    "px",
                ),
            };
            Numeric::literal(f64::from(value) * scale, dim, unit)
        }
        Token::Ident(name) if group => {
            let n = match name.to_ascii_lowercase().as_str() {
                "e" => std::f64::consts::E,
                "pi" => std::f64::consts::PI,
                "infinity" => f64::INFINITY,
                "-infinity" => f64::NEG_INFINITY,
                "nan" => f64::NAN,
                _ => return Err(p.new_custom_error(())),
            };
            Numeric::literal(n, NUMBER, "")
        }
        Token::Function(name) => {
            p.parse_nested_block(|p| function(p, &name.to_ascii_lowercase(), ctx, hint, depth + 1))?
        }
        Token::ParenthesisBlock if group => {
            p.parse_nested_block(|p| sum(p, ctx, hint, depth + 1))?
        }
        _ => return Err(p.new_custom_error(())),
    };
    Ok(value)
}

fn product<'i>(
    p: &mut Parser<'i, '_>,
    ctx: &Context<'_>,
    hint: Option<Dim>,
    depth: usize,
) -> ParseResult<'i, Numeric> {
    let mut result = value(p, ctx, hint, depth, true)?;
    let mut count = 0;
    loop {
        let state = p.state();
        let divide = match p.next() {
            Ok(Token::Delim('*')) => false,
            Ok(Token::Delim('/')) => true,
            _ => {
                p.reset(&state);
                break;
            }
        };
        count += 1;
        if count > MAX_COMPONENTS {
            return Err(p.new_custom_error(()));
        }
        let rhs = value(p, ctx, hint, depth, true)?;
        result = result
            .product(rhs, divide, hint)
            .ok_or_else(|| p.new_custom_error(()))?;
    }
    Ok(result)
}

fn sum<'i>(
    p: &mut Parser<'i, '_>,
    ctx: &Context<'_>,
    hint: Option<Dim>,
    depth: usize,
) -> ParseResult<'i, Numeric> {
    let mut result = product(p, ctx, hint, depth)?;
    let mut count = 0;
    loop {
        let state = p.state();
        if !matches!(p.next_including_whitespace(), Ok(Token::WhiteSpace(_))) {
            p.reset(&state);
            break;
        }
        let subtract = match p.next() {
            Ok(Token::Delim('+')) => false,
            Ok(Token::Delim('-')) => true,
            _ => {
                p.reset(&state);
                break;
            }
        };
        if !matches!(p.next_including_whitespace(), Ok(Token::WhiteSpace(_))) {
            return Err(p.new_custom_error(()));
        }
        count += 1;
        if count > MAX_COMPONENTS {
            return Err(p.new_custom_error(()));
        }
        let rhs = product(p, ctx, hint, depth)?;
        result = result
            .sum(rhs, subtract)
            .ok_or_else(|| p.new_custom_error(()))?;
    }
    Ok(result)
}

fn function<'i>(
    p: &mut Parser<'i, '_>,
    name: &str,
    ctx: &Context<'_>,
    hint: Option<Dim>,
    depth: usize,
) -> ParseResult<'i, Numeric> {
    if name == "calc" {
        return sum(p, ctx, hint, depth);
    }
    let mut strategy = "nearest".to_string();
    if name == "round" {
        if let Ok(value) = p.try_parse(|p| {
            let value = p.expect_ident_cloned()?;
            p.expect_comma()?;
            Ok::<_, cssparser::BasicParseError<'_>>(value)
        }) {
            strategy = value.to_ascii_lowercase();
        }
        if !matches!(
            strategy.as_str(),
            "nearest" | "up" | "down" | "to-zero" | "line-width"
        ) {
            return Err(p.new_custom_error(()));
        }
    }
    let mut args = p.parse_comma_separated(|p| {
        if name == "clamp" && p.try_parse(|p| p.expect_ident_matching("none")).is_ok() {
            Ok(None)
        } else {
            sum(p, ctx, hint, depth).map(Some)
        }
    })?;
    if args.is_empty() || args.len() > MAX_COMPONENTS {
        return Err(p.new_custom_error(()));
    }
    let none_bounds: Vec<_> = args.iter().map(Option::is_none).collect();
    if name == "clamp" {
        if args.len() != 3 || args[1].is_none() {
            return Err(p.new_custom_error(()));
        }
        let dim = args[1].as_ref().unwrap().dim;
        if args[0].is_none() {
            args[0] = Some(Numeric::literal(f64::NEG_INFINITY, dim, unit(dim)));
        }
        if args[2].is_none() {
            args[2] = Some(Numeric::literal(f64::INFINITY, dim, unit(dim)));
        }
    }
    let mut args: Vec<Numeric> = args
        .into_iter()
        .collect::<Option<_>>()
        .ok_or_else(|| p.new_custom_error(()))?;
    let density = ctx
        .dom
        .map_or(1., |dom| f64::from(dom.device_pixel_ratio()).max(0.001));
    let line_width = name == "round" && strategy == "line-width";
    if line_width && args[0].dim != LENGTH {
        return Err(p.new_custom_error(()));
    }
    let snap_only = line_width && args.len() == 1;
    if snap_only {
        args.push(Numeric::literal(1. / density, LENGTH, "px"));
    }
    if name == "round" && args.len() == 1 && args[0].dim == NUMBER {
        args.push(Numeric::literal(1., NUMBER, ""));
    }
    let same = args.iter().all(|a| a.dim == args[0].dim);
    let count = args.len();
    let input_dim = args[0].dim;
    let dim = match name {
        "min" | "max" | "hypot" if same => input_dim,
        "clamp" if same && count == 3 => input_dim,
        "round" | "mod" | "rem" if same && count == 2 => input_dim,
        "abs" if count == 1 => input_dim,
        "sign" if count == 1 => NUMBER,
        "sin" | "cos" | "tan" if count == 1 && matches!(input_dim, NUMBER | ANGLE) => NUMBER,
        "asin" | "acos" | "atan" if count == 1 && input_dim == NUMBER => ANGLE,
        "atan2" if same && count == 2 => ANGLE,
        "pow" if same && count == 2 && input_dim == NUMBER => NUMBER,
        "sqrt" | "exp" if count == 1 && input_dim == NUMBER => NUMBER,
        "log" if same && (count == 1 || count == 2) && input_dim == NUMBER => NUMBER,
        _ => return Err(p.new_custom_error(())),
    };
    let constants: Option<Vec<_>> = args.iter().map(|a| a.node.scalar().map(|v| v.0)).collect();
    if let Some(v) = constants
        && (hint.is_none() || !args.iter().any(|a| a.node.contains_percentage()))
    {
        let a = v[0];
        let b = v.get(1).copied().unwrap_or(0.);
        let radians = if input_dim == ANGLE {
            a.to_radians()
        } else {
            a
        };
        let n = if v.iter().any(|v| v.is_nan()) {
            f64::NAN
        } else {
            match name {
                "min" => v.iter().copied().fold(f64::INFINITY, f64::min),
                "max" => v.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                "clamp" => a.max(b.min(v[2])),
                "abs" => a.abs(),
                "sign" => {
                    if a == 0. {
                        a
                    } else {
                        a.signum()
                    }
                }
                "hypot" => v.iter().copied().fold(0., f64::hypot),
                "mod" => {
                    if b.is_infinite() && a.is_finite() {
                        if a.is_sign_negative() == b.is_sign_negative() {
                            a
                        } else {
                            f64::NAN
                        }
                    } else {
                        let rem = a % b;
                        if rem == 0. {
                            0_f64.copysign(b)
                        } else if rem.is_sign_negative() != b.is_sign_negative() {
                            rem + b
                        } else {
                            rem
                        }
                    }
                }
                "rem" => a % b,
                "round" => {
                    let result = if snap_only { a } else { round(a, b, &strategy) };
                    if line_width {
                        let pixels = result * density;
                        if pixels == 0. || !pixels.is_finite() {
                            result
                        } else {
                            pixels.signum() * pixels.abs().floor().max(1.) / density
                        }
                    } else {
                        result
                    }
                }
                "sin" => {
                    if radians.rem_euclid(std::f64::consts::PI) == 0. {
                        0.
                    } else {
                        radians.sin()
                    }
                }
                "cos" => {
                    if radians.rem_euclid(std::f64::consts::PI) == std::f64::consts::FRAC_PI_2 {
                        0.
                    } else {
                        radians.cos()
                    }
                }
                "tan" => radians.tan(),
                "asin" => a.asin().to_degrees(),
                "acos" => a.acos().to_degrees(),
                "atan" => a.atan().to_degrees(),
                "atan2" => a.atan2(b).to_degrees(),
                "pow" => a.powf(b),
                "sqrt" => a.sqrt(),
                "exp" => a.exp(),
                "log" => {
                    if count == 1 {
                        a.ln()
                    } else {
                        a.log(b)
                    }
                }
                _ => unreachable!(),
            }
        };
        return Ok(Numeric::literal(n, dim, unit(dim)));
    }
    let mut nodes: Vec<_> = args
        .into_iter()
        .enumerate()
        .map(|(i, a)| {
            if none_bounds.get(i) == Some(&true) {
                Node::Keyword("none".into())
            } else {
                a.node
            }
        })
        .collect();
    if snap_only {
        nodes.pop();
    }
    let name = if name == "round" {
        nodes.insert(0, Node::Keyword(strategy));
        "round"
    } else {
        name
    };
    Ok(Numeric {
        dim,
        node: Node::Function(name.to_owned(), nodes),
    })
}

fn round(a: f64, b: f64, strategy: &str) -> f64 {
    if b == 0. || a.is_infinite() && b.is_infinite() {
        return f64::NAN;
    }
    if a.is_infinite() {
        return a;
    }
    if b.is_infinite() {
        return match strategy {
            "up" if a > 0. => f64::INFINITY,
            "down" if a < 0. => f64::NEG_INFINITY,
            "line-width" if a != 0. => f64::INFINITY.copysign(a),
            _ => 0_f64.copysign(a),
        };
    }
    if a % b == 0. {
        return a;
    }
    let q = a / b.abs();
    let multiple = match strategy {
        "up" => q.ceil(),
        "down" => q.floor(),
        "to-zero" => q.trunc(),
        _ => (q + 0.5).floor(),
    };
    if strategy == "line-width" && multiple == 0. {
        b.abs().copysign(a)
    } else {
        (b.abs() * multiple).copysign(if multiple == 0. { a } else { multiple })
    }
}
