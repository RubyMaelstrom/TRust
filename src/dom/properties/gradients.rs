//! CSS Images 3/4 gradient grammars and CSS Values 4 <position>.
use super::*;
use values::{parse as value, parse_color};

fn keyword<'i>(p: &mut Parser<'i, '_>, choices: &[&str]) -> ParseResult<'i, String> {
    let name = p.expect_ident_cloned()?.to_ascii_lowercase();
    if choices.contains(&name.as_str()) {
        Ok(name)
    } else {
        Err(p.new_custom_error(()))
    }
}

fn angle<'i>(p: &mut Parser<'i, '_>, ctx: &Context<'_>, depth: usize) -> ParseResult<'i, String> {
    if p.try_parse(|p| {
        p.expect_number().and_then(|n| {
            if n == 0. {
                Ok(())
            } else {
                Err(p.new_basic_unexpected_token_error(Token::Delim('?')))
            }
        })
    })
    .is_ok()
    {
        Ok("0deg".into())
    } else {
        value(p, &Kind::Angle, ctx, depth)
    }
}

fn interpolation<'i>(p: &mut Parser<'i, '_>) -> ParseResult<'i, String> {
    p.expect_ident_matching("in")?;
    let space = keyword(
        p,
        &[
            "srgb",
            "srgb-linear",
            "display-p3",
            "display-p3-linear",
            "a98-rgb",
            "prophoto-rgb",
            "rec2020",
            "lab",
            "oklab",
            "xyz",
            "xyz-d50",
            "xyz-d65",
            "hsl",
            "hwb",
            "lch",
            "oklch",
        ],
    )?;
    let hue = if matches!(space.as_str(), "hsl" | "hwb" | "lch" | "oklch") {
        p.try_parse(|p| {
            let hue = keyword(p, &["shorter", "longer", "increasing", "decreasing"])?;
            p.expect_ident_matching("hue")?;
            Ok::<_, cssparser::ParseError<'i, ()>>(hue)
        })
        .ok()
    } else {
        None
    };
    Ok(format!(
        "in {space}{}",
        hue.map(|h| format!(" {h} hue")).unwrap_or_default()
    ))
}

fn position<'i>(
    p: &mut Parser<'i, '_>,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    // Try the unambiguous four-component grammar first, then the two- and
    // one-component alternatives. `try_parse` restores the input on failure.
    if let Ok((x, y)) = p.try_parse(|p| {
        let first = keyword(p, &["left", "right", "top", "bottom"])?;
        let a = value(p, &Kind::LengthPercentage, ctx, depth)?;
        let second = if first == "left" || first == "right" {
            keyword(p, &["top", "bottom"])?
        } else {
            keyword(p, &["left", "right"])?
        };
        let b = value(p, &Kind::LengthPercentage, ctx, depth)?;
        let first_horizontal = first == "left" || first == "right";
        let a = format!("{first} {a}");
        let b = format!("{second} {b}");
        Ok::<_, cssparser::ParseError<'i, ()>>(if first_horizontal { (a, b) } else { (b, a) })
    }) {
        return Ok(format!("{x} {y}"));
    }
    let component = |p: &mut Parser<'i, '_>| {
        p.try_parse(|p| keyword(p, &["left", "right", "top", "bottom", "center"]))
            .or_else(|_| value(p, &Kind::LengthPercentage, ctx, depth))
    };
    let first = component(p)?;
    let state = p.state();
    let second = p.try_parse(component).ok();
    let h = |s: &str| matches!(s, "left" | "right" | "center");
    let v = |s: &str| matches!(s, "top" | "bottom" | "center");
    if let Some(second) = second {
        if h(&first) && v(&second) {
            return Ok(format!("{first} {second}"));
        }
        if v(&first) && h(&second) {
            return Ok(format!("{second} {first}"));
        }
        if !matches!(first.as_str(), "top" | "bottom")
            && !matches!(second.as_str(), "left" | "right")
        {
            return Ok(format!("{first} {second}"));
        }
        p.reset(&state);
    }
    Ok(if matches!(first.as_str(), "top" | "bottom") {
        format!("center {first}")
    } else {
        format!("{first} center")
    })
}

fn geometry<'i>(
    p: &mut Parser<'i, '_>,
    name: &str,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    if name.contains("linear") {
        if p.try_parse(|p| p.expect_ident_matching("to")).is_ok() {
            let first = keyword(p, &["left", "right", "top", "bottom"])?;
            let second = if first == "left" || first == "right" {
                p.try_parse(|p| keyword(p, &["top", "bottom"])).ok()
            } else {
                p.try_parse(|p| keyword(p, &["left", "right"])).ok()
            };
            return Ok(format!(
                "to {first}{}",
                second.map(|s| format!(" {s}")).unwrap_or_default()
            ));
        }
        return angle(p, ctx, depth);
    }
    let mut parts = Vec::new();
    if name.contains("conic") {
        if p.try_parse(|p| p.expect_ident_matching("from")).is_ok() {
            parts.push(format!("from {}", angle(p, ctx, depth)?));
        }
    } else {
        let mut shape = None;
        let mut size = None;
        for _ in 0..2 {
            if shape.is_none()
                && let Ok(value) = p.try_parse(|p| keyword(p, &["circle", "ellipse"]))
            {
                shape = Some(value);
                continue;
            }
            if size.is_none() {
                if let Ok(value) = p.try_parse(|p| {
                    let extents = &[
                        "closest-corner",
                        "closest-side",
                        "farthest-corner",
                        "farthest-side",
                    ];
                    let first = keyword(p, extents)?;
                    Ok::<_, cssparser::ParseError<'_, ()>>((
                        first,
                        p.try_parse(|p| keyword(p, extents)).ok(),
                    ))
                }) {
                    // CSS Images 4 #radial-size adds independent horizontal
                    // and vertical extents; circles still take one radius.
                    size = Some(match value {
                        (first, Some(second)) => (format!("{first} {second}"), 2),
                        (first, None) => (first, 0),
                    });
                    continue;
                }
                if let Ok((value, count)) = p.try_parse(|p| {
                    let radius = |p: &mut Parser<'i, '_>| {
                        math::parse_range(
                            p,
                            &Kind::LengthPercentage,
                            ctx,
                            depth + 1,
                            0.,
                            f64::INFINITY,
                        )
                    };
                    let first = radius(p)?;
                    let second = p.try_parse(radius).ok();
                    if let Some(second) = second {
                        Ok::<_, cssparser::ParseError<'_, ()>>((format!("{first} {second}"), 2))
                    } else {
                        Ok((first, 1))
                    }
                }) {
                    size = Some((value, count));
                    continue;
                }
            }
            break;
        }
        if let Some((_, count)) = &size
            && (shape.as_deref() == Some("circle") && *count == 2
                || shape.as_deref() == Some("ellipse") && *count == 1)
        {
            return Err(p.new_custom_error(()));
        }
        if let Some(shape) = shape {
            parts.push(shape);
        }
        if let Some((size, _)) = size {
            parts.push(size);
        }
    }
    if p.try_parse(|p| p.expect_ident_matching("at")).is_ok() {
        parts.push(format!("at {}", position(p, ctx, depth)?));
    }
    if parts.is_empty() {
        return Err(p.new_custom_error(()));
    }
    Ok(parts.join(" "))
}

fn prelude<'i>(
    p: &mut Parser<'i, '_>,
    name: &str,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    let before = p.try_parse(interpolation).ok();
    let geometry = p.try_parse(|p| geometry(p, name, ctx, depth)).ok();
    let after = if before.is_none() {
        p.try_parse(interpolation).ok()
    } else {
        None
    };
    if before.is_none() && geometry.is_none() && after.is_none() {
        return Err(p.new_custom_error(()));
    }
    p.expect_exhausted()?;
    Ok(geometry
        .into_iter()
        .chain(before.or(after))
        .collect::<Vec<_>>()
        .join(" "))
}

pub(super) fn parse<'i>(
    p: &mut Parser<'i, '_>,
    name: &str,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    let stop_kind = if name.contains("conic") {
        Kind::AnglePercentage
    } else {
        Kind::LengthPercentage
    };
    let mut stops = 0;
    let mut first = true;
    let mut previous_hint = false;
    let parts = p.parse_comma_separated(|p| {
        if let Ok(color) = p.try_parse(|p| parse_color(p, ctx, depth)) {
            stops += 1;
            previous_hint = false;
            first = false;
            let mut part = color;
            for _ in 0..2 {
                if let Ok(pos) = p.try_parse(|p| stop_position(p, &stop_kind, ctx, depth)) {
                    part.push(' ');
                    part.push_str(&pos);
                } else {
                    break;
                }
            }
            return Ok(part);
        }
        if first {
            first = false;
            return prelude(p, name, ctx, depth);
        }
        if previous_hint || stops == 0 {
            return Err(p.new_custom_error(()));
        }
        previous_hint = true;
        stop_position(p, &stop_kind, ctx, depth)
    })?;
    if stops == 0 || previous_hint || parts.len() > MAX_COMPONENTS {
        return Err(p.new_custom_error(()));
    }
    Ok(parts.join(", "))
}

fn stop_position<'i>(
    p: &mut Parser<'i, '_>,
    kind: &Kind,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    if *kind == Kind::AnglePercentage
        && let Ok(zero) = p.try_parse(|p| angle(p, ctx, depth))
    {
        return Ok(zero);
    }
    value(p, kind, ctx, depth)
}
