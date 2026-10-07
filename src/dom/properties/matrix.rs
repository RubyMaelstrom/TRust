//! Geometry Interfaces 1 #dommatrix-parse (CSSWG snapshot 81c27f686901,
//! 2026-09-06): "parse a string into an abstract matrix" for the
//! DOMMatrix(ReadOnly) constructors and DOMMatrix.setMatrixValue().
//!
//! The string is a value of the CSS `transform` property: `none` or a
//! <transform-list> (CSS Transforms 1 #transform-property and Transforms 2
//! #transform-functions). A DOMMatrix has no element, so every length must
//! use an absolute unit: relative, viewport, container and percentage lengths
//! are failures. Each function becomes its 4x4 matrix (Transforms 2
//! #mathematical-description) and the matrices are post-multiplied from left
//! to right.

use super::*;

/// Column-major m11..m44 of the identity matrix.
const IDENTITY: [f64; 16] = [
    1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.,
];

#[derive(Clone, Copy, PartialEq)]
enum Argument {
    Number,
    /// <number> | <percentage> (Transforms 2 scale functions).
    Scale,
    Length,
    /// <length [0,∞]>, or `none` as infinity.
    Perspective,
    /// <angle> | <zero>, in degrees.
    Angle,
}

/// The column-major m11..m44 elements of `text`'s matrix and whether it used
/// only two-dimensional transform functions, or `None` for failure.
pub(crate) fn transform_list_matrix(text: &str) -> Option<([f64; 16], bool)> {
    // Step 1: the empty string means "matrix(1, 0, 0, 1, 0, 0)".
    if text.is_empty() {
        return Some((IDENTITY, true));
    }
    if !valid_tokens(text) || !absolute_units(text) {
        return None;
    }
    let mut parser = Parser::new(text);
    // Step 3: `none` is a list holding one identity matrix. It is the only
    // keyword the grammar admits, so CSS-wide keywords are failures.
    if parser
        .try_parse(|p| {
            p.expect_ident_matching("none")?;
            p.expect_exhausted()
        })
        .is_ok()
    {
        return Some((IDENTITY, true));
    }
    let mut matrix = IDENTITY;
    let mut two_dimensional = true;
    loop {
        let (function, three_dimensional) = transform_function(&mut parser).ok()?;
        matrix = multiply(&matrix, &function);
        // Step 4: any three-dimensional transform function (Transforms 2
        // #three-d-transform-functions) makes the result a 3D matrix.
        two_dimensional &= !three_dimensional;
        if parser.is_exhausted() {
            return Some((matrix, two_dimensional));
        }
    }
}

/// Every dimension must be an absolute length or an angle: nothing else can
/// be resolved without an element (Geometry 1 #dommatrix-parse step 2).
fn absolute_units(text: &str) -> bool {
    fn scan<'i>(p: &mut Parser<'i>, depth: usize) -> ParseResult<()> {
        if depth > MAX_DEPTH {
            return Err(cssparser::ParseError::custom(()));
        }
        while !p.is_exhausted() {
            match p.next()?.clone() {
                Token::Dimension { unit, .. }
                    if length_unit(&unit).is_none() && angle_unit(&unit).is_none() =>
                {
                    return Err(cssparser::ParseError::custom(()));
                }
                Token::Function(_)
                | Token::ParenthesisBlock
                | Token::SquareBracketBlock
                | Token::CurlyBracketBlock => p.parse_nested_block(|p| scan(p, depth + 1))?,
                _ => {}
            }
        }
        Ok(())
    }
    scan(&mut Parser::new(text), 0).is_ok()
}

/// CSS Values 4 #absolute-lengths, in CSS pixels.
fn length_unit(unit: &str) -> Option<f64> {
    Some(match unit.to_ascii_lowercase().as_str() {
        "px" => 1.,
        "in" => 96.,
        "cm" => 96. / 2.54,
        "mm" => 96. / 25.4,
        "q" => 96. / 101.6,
        "pt" => 96. / 72.,
        "pc" => 16.,
        _ => return None,
    })
}

/// CSS Values 4 #angles, in degrees.
fn angle_unit(unit: &str) -> Option<f64> {
    Some(match unit.to_ascii_lowercase().as_str() {
        "deg" => 1.,
        "grad" => 0.9,
        "rad" => 180. / std::f64::consts::PI,
        "turn" => 360.,
        _ => return None,
    })
}

fn transform_function<'i>(p: &mut Parser<'i>) -> ParseResult<([f64; 16], bool)> {
    use Argument::*;
    let name = p.expect_function()?.to_ascii_lowercase();
    let (arguments, minimum, three_dimensional): (&[Argument], usize, bool) = match name.as_str() {
        "matrix" => (&[Number; 6], 6, false),
        "matrix3d" => (&[Number; 16], 16, true),
        "translate" => (&[Length; 2], 1, false),
        "translatex" | "translatey" => (&[Length], 1, false),
        "translatez" => (&[Length], 1, true),
        "translate3d" => (&[Length; 3], 3, true),
        "scale" => (&[Scale; 2], 1, false),
        "scalex" | "scaley" => (&[Scale], 1, false),
        "scalez" => (&[Scale], 1, true),
        "scale3d" => (&[Scale; 3], 3, true),
        "rotate" | "skewx" | "skewy" => (&[Angle], 1, false),
        "rotatex" | "rotatey" | "rotatez" => (&[Angle], 1, true),
        "rotate3d" => (&[Number, Number, Number, Angle], 4, true),
        "skew" => (&[Angle; 2], 1, false),
        "perspective" => (&[Perspective], 1, true),
        _ => return Err(cssparser::ParseError::custom(())),
    };
    let values = p.parse_nested_block(|p| {
        let mut values = Vec::with_capacity(arguments.len());
        loop {
            let Some(&kind) = arguments.get(values.len()) else {
                return Err(cssparser::ParseError::custom(()));
            };
            values.push(argument(p, kind)?);
            if p.is_exhausted() {
                break;
            }
            p.expect_comma()?;
        }
        if values.len() < minimum {
            return Err(cssparser::ParseError::custom(()));
        }
        Ok(values)
    })?;
    Ok((function_matrix(&name, &values), three_dimensional))
}

fn argument<'i>(p: &mut Parser<'i>, kind: Argument) -> ParseResult<f64> {
    if let Ok(value) = p.try_parse(|p| literal(p, kind)) {
        return Ok(value);
    }
    if kind == Argument::Perspective && p.try_parse(|p| p.expect_ident_matching("none")).is_ok() {
        return Ok(f64::INFINITY);
    }
    // A math function resolves to one canonical number and unit.
    let context = Context::independent();
    let (computed, unit, scale) = match kind {
        Argument::Number => (math::parse(p, &Kind::Number, &context, 0)?, "", 1.),
        Argument::Length => (math::parse(p, &Kind::Length, &context, 0)?, "px", 1.),
        Argument::Perspective => (
            math::parse_range(p, &Kind::Length, &context, 0, 0., f64::INFINITY)?,
            "px",
            1.,
        ),
        Argument::Angle => (math::parse(p, &Kind::Angle, &context, 0)?, "deg", 1.),
        Argument::Scale => match p.try_parse(|p| math::parse(p, &Kind::Number, &context, 0)) {
            Ok(number) => (number, "", 1.),
            Err(_) => (math::parse(p, &Kind::Percentage, &context, 0)?, "%", 0.01),
        },
    };
    computed
        .strip_suffix(unit)
        .and_then(|number| number.parse::<f64>().ok())
        .map(|number| number * scale)
        .ok_or_else(|| cssparser::ParseError::custom(()))
}

/// A single numeric token keeps its source's double precision.
fn literal<'i>(p: &mut Parser<'i>, kind: Argument) -> ParseResult<f64> {
    p.skip_whitespace();
    let start = p.position();
    let token = p.next()?.clone();
    let source = p.slice_from(start);
    let precise = |rounded: f32, suffix: usize| {
        source
            .get(..source.len().saturating_sub(suffix))
            .and_then(|number| number.parse::<f64>().ok())
            .unwrap_or(f64::from(rounded))
    };
    let value = match (kind, token) {
        (Argument::Number | Argument::Scale, Token::Number { value, .. }) => precise(value, 0),
        (Argument::Scale, Token::Percentage { unit_value, .. }) => {
            precise(unit_value * 100., 1) / 100.
        }
        // Unitless zero is a <length> and an angle's <zero>.
        (
            Argument::Length | Argument::Perspective | Argument::Angle,
            Token::Number { value: 0., .. },
        ) => 0.,
        (Argument::Length | Argument::Perspective, Token::Dimension { value, unit, .. }) => {
            let scale = length_unit(&unit).ok_or_else(|| cssparser::ParseError::custom(()))?;
            precise(value, unit.len()) * scale
        }
        (Argument::Angle, Token::Dimension { value, unit, .. }) => {
            let scale = angle_unit(&unit).ok_or_else(|| cssparser::ParseError::custom(()))?;
            precise(value, unit.len()) * scale
        }
        _ => return Err(cssparser::ParseError::custom(())),
    };
    if kind == Argument::Perspective && value < 0. {
        return Err(cssparser::ParseError::custom(()));
    }
    Ok(value)
}

/// Transforms 2 #mathematical-description, column-major.
fn function_matrix(name: &str, v: &[f64]) -> [f64; 16] {
    let mut m = IDENTITY;
    let at = |index: usize, fallback: f64| v.get(index).copied().unwrap_or(fallback);
    match name {
        "matrix" => {
            [m[0], m[1], m[4], m[5], m[12], m[13]] = [v[0], v[1], v[2], v[3], v[4], v[5]];
        }
        "matrix3d" => m.copy_from_slice(&v[..16]),
        "translate" => [m[12], m[13]] = [v[0], at(1, 0.)],
        "translatex" => m[12] = v[0],
        "translatey" => m[13] = v[0],
        "translatez" => m[14] = v[0],
        "translate3d" => [m[12], m[13], m[14]] = [v[0], v[1], v[2]],
        "scale" => [m[0], m[5]] = [v[0], at(1, v[0])],
        "scalex" => m[0] = v[0],
        "scaley" => m[5] = v[0],
        "scalez" => m[10] = v[0],
        "scale3d" => [m[0], m[5], m[10]] = [v[0], v[1], v[2]],
        "rotate" | "rotatez" => return rotation(0., 0., 1., v[0]),
        "rotatex" => return rotation(1., 0., 0., v[0]),
        "rotatey" => return rotation(0., 1., 0., v[0]),
        "rotate3d" => return rotation(v[0], v[1], v[2], v[3]),
        "skew" => [m[4], m[1]] = [tan_degrees(v[0]), tan_degrees(at(1, 0.))],
        "skewx" => m[4] = tan_degrees(v[0]),
        "skewy" => m[1] = tan_degrees(v[0]),
        // A depth below 1px is treated as 1px when converted to a matrix
        // (Transforms 2 #funcdef-perspective); `none` is the identity.
        "perspective" if v[0].is_finite() => m[11] = -1. / v[0].max(1.),
        _ => {}
    }
    m
}

fn tan_degrees(degrees: f64) -> f64 {
    (degrees * std::f64::consts::PI / 180.).tan()
}

/// Sine and cosine of an angle in degrees, exact at quarter turns.
fn sin_cos_degrees(degrees: f64) -> (f64, f64) {
    let turn = degrees % 360.;
    if turn % 90. == 0. {
        return match ((turn / 90.) as i64).rem_euclid(4) {
            0 => (0., 1.),
            1 => (1., 0.),
            2 => (0., -1.),
            _ => (-1., 0.),
        };
    }
    let radians = degrees * std::f64::consts::PI / 180.;
    (radians.sin(), radians.cos())
}

/// Transforms 2 #Rotate3dDefined about the normalized [x, y, z]. A vector
/// that cannot be normalized applies no rotation.
fn rotation(x: f64, y: f64, z: f64, degrees: f64) -> [f64; 16] {
    let length = (x * x + y * y + z * z).sqrt();
    if length == 0. || !length.is_finite() {
        return IDENTITY;
    }
    let (x, y, z) = (x / length, y / length, z / length);
    let (s, c) = sin_cos_degrees(degrees);
    let mut m = IDENTITY;
    // Rotations about a coordinate axis keep that axis's elements exact.
    if x == 0. && y == 0. {
        [m[0], m[1], m[4], m[5]] = [c, z * s, -z * s, c];
        return m;
    }
    if y == 0. && z == 0. {
        [m[5], m[6], m[9], m[10]] = [c, x * s, -x * s, c];
        return m;
    }
    if x == 0. && z == 0. {
        [m[0], m[2], m[8], m[10]] = [c, -y * s, y * s, c];
        return m;
    }
    let t = 1. - c;
    m[0] = x * x * t + c;
    m[1] = x * y * t + z * s;
    m[2] = x * z * t - y * s;
    m[4] = x * y * t - z * s;
    m[5] = y * y * t + c;
    m[6] = y * z * t + x * s;
    m[8] = x * z * t + y * s;
    m[9] = y * z * t - x * s;
    m[10] = z * z * t + c;
    m
}

/// `a` post-multiplied by `b` (a · b), both column-major.
fn multiply(a: &[f64; 16], b: &[f64; 16]) -> [f64; 16] {
    std::array::from_fn(|index| {
        let (column, row) = (index / 4, index % 4);
        (0..4).map(|k| a[k * 4 + row] * b[column * 4 + k]).sum()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix(text: &str) -> Option<(Vec<f64>, bool)> {
        transform_list_matrix(text).map(|(m, two_d)| (m.to_vec(), two_d))
    }

    #[test]
    fn transform_lists_become_matrices_with_their_dimension() {
        let identity = Some((IDENTITY.to_vec(), true));
        assert_eq!(matrix(""), identity);
        assert_eq!(matrix(" none "), identity);
        let (m, two_d) = matrix("scale(2) translateX(5px) translateY(calc(2 * 2.5px))").unwrap();
        assert!(two_d);
        assert_eq!([m[0], m[5], m[12], m[13]], [2., 2., 10., 10.]);
        let (m, two_d) = matrix("translate(1in, 1Q) rotate(90deg)").unwrap();
        assert!(two_d);
        assert_eq!([m[0], m[1], m[4], m[5], m[12]], [0., 1., -1., 0., 96.]);
        assert!((m[13] - 96. / 101.6).abs() < 1e-12);
        assert_eq!(
            matrix("matrix(1.23456789, 0, 0, 1, 0, 0)").unwrap().0[0],
            1.23456789
        );
        for three_d in [
            "translateZ(0)",
            "scale3d(1, 1, 1)",
            "rotateZ(10deg)",
            "rotate3d(0, 0, 0, 40deg)",
            "perspective(none)",
        ] {
            assert!(!matrix(three_d).unwrap().1, "{three_d}");
        }
        assert_eq!(matrix("perspective(0)").unwrap().0[11], -1.);
        assert_eq!(matrix("perspective(100px)").unwrap().0[11], -0.01);
        assert_eq!(
            matrix("scale(50%, 2)").unwrap().0[..6],
            [0.5, 0., 0., 0., 0., 2.]
        );
    }

    #[test]
    fn unresolvable_or_invalid_transform_lists_fail() {
        for text in [
            " ",
            "/**/",
            "\0",
            ";",
            "none;",
            "inherit",
            "initial",
            "unset",
            "null",
            "translateX    (5px)",
            "scale(2 2) translateX(5) translateY(5)",
            "scale(2, 2), translateX(5)  ,translateY(5)",
            "scale(sign(1em))",
            "translateX(5em)",
            "translateX(5vw)",
            "translateX(5cqmin)",
            "translateX(5%)",
            "translateX(calc(5px + 0%))",
            "rotate(5)",
            "rotate(5deg, 5px, 5px)",
            "perspective(-1px)",
            "translate(5px) none",
            "translateX(var(--x))",
            "matrix(1, 0, 0, 1, 0, 0,)",
        ] {
            assert_eq!(matrix(text), None, "{text:?}");
        }
    }
}
