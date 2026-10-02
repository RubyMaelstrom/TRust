//! CSS Filter Effects 1 #FilterProperty / #ShorthandEquivalents.
//! Each matrix operates on straight-alpha sRGB and is clamped separately;
//! combining matrices would incorrectly discard intermediate clipping.

use crate::render::{CssFilter, PaintColor};

/// Parse a `filter` value into its ordered filter functions: `None` when
/// invalid, empty for `none`. `length` resolves a `<length>` to px and
/// `current_color` is drop-shadow()'s default color.
pub(crate) fn filters(
    value: &str,
    length: &dyn Fn(&str) -> Option<f32>,
    current_color: PaintColor,
) -> Option<Vec<CssFilter>> {
    use cssparser::{Parser, ParserInput};
    let mut input = ParserInput::new(value);
    let mut parser = Parser::new(&mut input);
    if parser
        .try_parse(|p| p.expect_ident_matching("none"))
        .is_ok()
    {
        return parser.is_exhausted().then(Vec::new);
    }
    let mut filters = Vec::new();
    while !parser.is_exhausted() {
        let name = parser.expect_function().ok()?.to_ascii_lowercase();
        let filter = parser
            .parse_nested_block(|p| -> Result<CssFilter, cssparser::ParseError<'_, ()>> {
                let filter = match name.as_str() {
                    "blur" => CssFilter::Blur(if p.is_exhausted() {
                        0.
                    } else {
                        one_length(p, length)?
                    }),
                    "drop-shadow" => drop_shadow(p, length, current_color)?,
                    "hue-rotate" => CssFilter::ColorMatrix(hue_rotate(if p.is_exhausted() {
                        0.
                    } else {
                        angle(p)?
                    })),
                    _ => CssFilter::ColorMatrix(
                        amount_matrix(&name, amount(p)?).ok_or_else(|| p.new_custom_error(()))?,
                    ),
                };
                p.expect_exhausted()?;
                Ok(filter)
            })
            .ok()?;
        filters.push(filter);
    }
    (!filters.is_empty()).then_some(filters)
}

/// Whether `value` is a valid `filter` (CSSOM's parse check).
pub(crate) fn valid(value: &str) -> bool {
    let length = |text: &str| {
        super::value::Len::parse(
            text,
            super::Units::default(),
            super::value::Vp { w: 0., h: 0. },
        )?
        .resolve(None)
    };
    filters(value, &length, PaintColor::Rgba(0, 0, 0, 255)).is_some()
}

type Failure<'i> = cssparser::ParseError<'i, ()>;

/// A non-percentage `<length>` (unitless only for zero).
fn length_token<'i>(
    p: &mut cssparser::Parser<'i, '_>,
    length: &dyn Fn(&str) -> Option<f32>,
) -> Result<f32, Failure<'i>> {
    use cssparser::Token;
    let px = match p.next()?.clone() {
        Token::Number { value: 0., .. } => Some(0.),
        Token::Dimension { value, unit, .. } => length(&format!("{value}{unit}")),
        _ => None,
    };
    px.filter(|px| px.is_finite())
        .ok_or_else(|| p.new_custom_error(()))
}

fn one_length<'i>(
    p: &mut cssparser::Parser<'i, '_>,
    length: &dyn Fn(&str) -> Option<f32>,
) -> Result<f32, Failure<'i>> {
    let px = length_token(p, length)?;
    if px < 0. {
        return Err(p.new_custom_error(()));
    }
    Ok(px)
}

/// `drop-shadow( [ <color>? && <length>{2,3} ] )`, the third length being
/// a standard deviation.
fn drop_shadow<'i>(
    p: &mut cssparser::Parser<'i, '_>,
    length: &dyn Fn(&str) -> Option<f32>,
    current_color: PaintColor,
) -> Result<CssFilter, Failure<'i>> {
    let mut color = None;
    let mut lengths = Vec::new();
    // How many lengths preceded the color: the lengths are one run, before
    // or after it.
    let mut color_after = 0;
    while !p.is_exhausted() {
        if let Ok(px) = p.try_parse(|p| length_token(p, length)) {
            if color.is_some() && color_after > 0 {
                return Err(p.new_custom_error(()));
            }
            lengths.push(px);
            continue;
        }
        let start = p.position();
        // A color function's arguments come with it.
        if matches!(p.next()?, cssparser::Token::Function(_)) {
            p.parse_nested_block(|p| -> Result<(), Failure<'i>> {
                while p.next().is_ok() {}
                Ok(())
            })?;
        }
        let text = p.slice_from(start);
        let parsed = if text.trim().eq_ignore_ascii_case("currentcolor") {
            Some(current_color)
        } else {
            PaintColor::parse_css(text)
        };
        if color.is_some() || lengths.len() == 1 {
            return Err(p.new_custom_error(()));
        }
        color_after = lengths.len();
        color = Some(parsed.ok_or_else(|| p.new_custom_error(()))?);
    }
    if !(2..=3).contains(&lengths.len()) || lengths.get(2).is_some_and(|blur| *blur < 0.) {
        return Err(p.new_custom_error(()));
    }
    Ok(CssFilter::DropShadow {
        dx: lengths[0],
        dy: lengths[1],
        std_deviation: lengths.get(2).copied().unwrap_or(0.),
        color: color.unwrap_or(current_color),
    })
}

/// `[ <number> | <percentage> ]?`, defaulting to 1.
fn amount<'i>(p: &mut cssparser::Parser<'i, '_>) -> Result<f32, Failure<'i>> {
    use cssparser::Token;
    if p.is_exhausted() {
        return Ok(1.);
    }
    let amount = match p.next()? {
        Token::Number { value, .. } => *value,
        Token::Percentage { unit_value, .. } => *unit_value,
        _ => return Err(p.new_custom_error(())),
    };
    if !amount.is_finite() || amount < 0. {
        return Err(p.new_custom_error(()));
    }
    Ok(amount)
}

/// `<angle> | <zero>` in radians.
fn angle<'i>(p: &mut cssparser::Parser<'i, '_>) -> Result<f32, Failure<'i>> {
    use cssparser::Token;
    let radians = match p.next()?.clone() {
        Token::Number { value: 0., .. } => Some(0.),
        Token::Dimension { value, unit, .. } => match unit.to_ascii_lowercase().as_str() {
            "deg" => Some(value.to_radians()),
            "rad" => Some(value),
            "grad" => Some(value * std::f32::consts::PI / 200.),
            "turn" => Some(value * std::f32::consts::TAU),
            _ => None,
        },
        _ => None,
    };
    radians
        .filter(|radians| radians.is_finite())
        .ok_or_else(|| p.new_custom_error(()))
}

/// #huerotateEquivalent: feColorMatrix type="hueRotate".
fn hue_rotate(radians: f32) -> [f32; 20] {
    let (sin, cos) = radians.sin_cos();
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
    let mut matrix = [0.; 20];
    for row in 0..3 {
        for col in 0..3 {
            matrix[row * 5 + col] =
                luma[col] + cos * cos_terms[row][col] + sin * sin_terms[row][col];
        }
    }
    matrix[18] = 1.;
    matrix
}

/// The color matrix of an amount-taking filter function.
fn amount_matrix(name: &str, amount: f32) -> Option<[f32; 20]> {
    if !matches!(
        name,
        "brightness" | "contrast" | "invert" | "opacity" | "grayscale" | "sepia" | "saturate"
    ) {
        return None;
    }
    let mut matrix = [0.; 20];
    for i in [0, 6, 12, 18] {
        matrix[i] = 1.;
    }
    match name {
        "brightness" | "contrast" => {
            for channel in 0..3 {
                matrix[channel * 5 + channel] = amount;
                if name == "contrast" {
                    matrix[channel * 5 + 4] = 0.5 * (1. - amount);
                }
            }
        }
        "invert" => {
            let amount = amount.min(1.);
            for channel in 0..3 {
                matrix[channel * 5 + channel] = 1. - 2. * amount;
                matrix[channel * 5 + 4] = amount;
            }
        }
        "opacity" => matrix[18] = amount.min(1.),
        "grayscale" | "sepia" => {
            let amount = amount.min(1.);
            let target = if name == "grayscale" {
                [[0.2126, 0.7152, 0.0722]; 3]
            } else {
                [
                    [0.393, 0.769, 0.189],
                    [0.349, 0.686, 0.168],
                    [0.272, 0.534, 0.131],
                ]
            };
            for row in 0..3 {
                for col in 0..3 {
                    matrix[row * 5 + col] =
                        (if row == col { 1. - amount } else { 0. }) + amount * target[row][col];
                }
            }
        }
        "saturate" => {
            // #saturateEquivalent: feColorMatrix type="saturate".
            let s = amount;
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
            for row in 0..3 {
                matrix[row * 5..row * 5 + 3].copy_from_slice(&rows[row]);
            }
        }
        _ => unreachable!(),
    }
    Some(matrix)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(value: &str) -> Option<Vec<CssFilter>> {
        let length = |text: &str| text.strip_suffix("px")?.parse().ok();
        filters(value, &length, PaintColor::Rgba(1, 2, 3, 255))
    }

    fn matrix(filter: &CssFilter) -> [f32; 20] {
        match filter {
            CssFilter::ColorMatrix(matrix) => *matrix,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn filter_grammar_preserves_order_defaults_and_ranges() {
        let filters = parse("brightness(60%) contrast(2) invert() opacity(150%)").unwrap();
        assert_eq!(filters.len(), 4);
        assert_eq!(matrix(&filters[0])[0], 0.6);
        assert_eq!((matrix(&filters[1])[0], matrix(&filters[1])[4]), (2., -0.5));
        assert_eq!((matrix(&filters[2])[0], matrix(&filters[2])[4]), (-1., 1.));
        assert_eq!(matrix(&filters[3])[18], 1.);
        assert!(parse("none").unwrap().is_empty());
        for invalid in [
            "",
            "brightness(-1)",
            "brightness(1,2)",
            "brightness(1px)",
            "none brightness(1)",
            "blur(-2px)",
            "blur(10%)",
            "drop-shadow(1px)",
            "drop-shadow(1px 2px 3px 4px)",
            "drop-shadow(1px red 2px)",
            "drop-shadow(red 1px 2px blue)",
            "drop-shadow(1px 2px -3px)",
            "hue-rotate(10)",
        ] {
            assert!(parse(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn blur_and_drop_shadow_keep_their_lengths_and_colors() {
        // Filter Effects 1 #FilterFunction: drop-shadow's third length is a
        // standard deviation, its color defaults to currentcolor.
        assert_eq!(
            parse(
                "blur(2px) drop-shadow(1px 2px) drop-shadow(rgb(4 5 6) 0 0 3px) drop-shadow(0 1px red)"
            ),
            Some(vec![
                CssFilter::Blur(2.),
                CssFilter::DropShadow {
                    dx: 1.,
                    dy: 2.,
                    std_deviation: 0.,
                    color: PaintColor::Rgba(1, 2, 3, 255)
                },
                CssFilter::DropShadow {
                    dx: 0.,
                    dy: 0.,
                    std_deviation: 3.,
                    color: PaintColor::Rgba(4, 5, 6, 255)
                },
                CssFilter::DropShadow {
                    dx: 0.,
                    dy: 1.,
                    std_deviation: 0.,
                    color: PaintColor::Rgba(255, 0, 0, 255)
                },
            ])
        );
        assert_eq!(parse("blur()"), Some(vec![CssFilter::Blur(0.)]));
    }

    #[test]
    fn hue_rotate_and_saturate_follow_the_color_matrix_equivalents() {
        let identity = matrix(&parse("hue-rotate(0deg)").unwrap()[0]);
        for (index, value) in identity.iter().enumerate() {
            let expected = if [0, 6, 12, 18].contains(&index) {
                1.
            } else {
                0.
            };
            // The equivalent matrix's own constants sum to 0.9999.
            assert!((value - expected).abs() < 2e-4, "{index}: {value}");
        }
        let half_turn = matrix(&parse("hue-rotate(0.5turn)").unwrap()[0]);
        assert!((half_turn[0] - (0.2126 - 0.7873)).abs() < 1e-4);
        let gray = matrix(&parse("saturate(0)").unwrap()[0]);
        assert_eq!(gray[..3], [0.2126, 0.7152, 0.0722]);
        assert!((matrix(&parse("saturate()").unwrap()[0])[0] - 1.).abs() < 2e-4);
    }
}
