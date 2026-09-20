//! CSS Filter Effects 1 #FilterProperty / #ShorthandEquivalents.
//! Each matrix operates on straight-alpha sRGB and is clamped separately;
//! combining matrices would incorrectly discard intermediate clipping.

pub(crate) fn color_filters(value: &str) -> Option<Vec<[f32; 20]>> {
    use cssparser::{Parser, ParserInput, Token};
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
        if !matches!(
            name.as_str(),
            "brightness" | "contrast" | "invert" | "opacity" | "grayscale" | "sepia"
        ) {
            return None;
        }
        let amount = parser
            .parse_nested_block(|p| -> Result<f32, cssparser::ParseError<'_, ()>> {
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
                p.expect_exhausted()?;
                Ok(amount)
            })
            .ok()?;
        let mut matrix = [0.; 20];
        for i in [0, 6, 12, 18] {
            matrix[i] = 1.;
        }
        match name.as_str() {
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
            _ => unreachable!(),
        }
        filters.push(matrix);
    }
    (!filters.is_empty()).then_some(filters)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn color_filter_grammar_preserves_order_defaults_and_ranges() {
        let filters = color_filters("brightness(60%) contrast(2) invert() opacity(150%)").unwrap();
        assert_eq!(filters.len(), 4);
        assert_eq!(filters[0][0], 0.6);
        assert_eq!((filters[1][0], filters[1][4]), (2., -0.5));
        assert_eq!((filters[2][0], filters[2][4]), (-1., 1.));
        assert_eq!(filters[3][18], 1.);
        assert!(color_filters("none").unwrap().is_empty());
        for invalid in [
            "",
            "brightness(-1)",
            "brightness(1,2)",
            "brightness(1px)",
            "none brightness(1)",
            "brightness(1) blur(2px)",
        ] {
            assert!(color_filters(invalid).is_none(), "{invalid}");
        }
    }
}
