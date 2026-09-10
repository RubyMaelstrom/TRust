//! Computed values for the component types permitted by a registration.
//! CSS Values 4, CSS Color 4 #resolving-color-values, CSS Images 3's computed
//! <image>, and CSS Transforms 1/2's transform-function grammars.
use super::*;

pub(super) fn parse<'i>(
    p: &mut Parser<'i, '_>,
    kind: &Kind,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    if depth > MAX_DEPTH {
        return Err(p.new_custom_error(()));
    }
    match kind {
        Kind::Length
        | Kind::LengthPercentage
        | Kind::Number
        | Kind::Integer
        | Kind::Percentage
        | Kind::Angle
        | Kind::AnglePercentage
        | Kind::Time
        | Kind::Resolution => math::parse(p, kind, ctx, depth),
        Kind::String => Ok(string_text(&p.expect_string_cloned()?)),
        Kind::CustomIdent | Kind::Ident(_) => {
            let name = p.expect_ident_cloned()?.to_string();
            if !custom_ident(&name) || matches!(kind,Kind::Ident(expected) if expected != &name) {
                return Err(p.new_custom_error(()));
            }
            Ok(identifier_text(&name))
        }
        Kind::Url => parse_url(p, ctx),
        Kind::Color => parse_color(p, ctx, depth),
        Kind::Image => parse_image(p, ctx, depth),
        Kind::TransformFunction => parse_transform(p, ctx, depth),
        Kind::TransformList => {
            let mut values = vec![parse_transform(p, ctx, depth)?];
            while !p.is_exhausted() {
                if values.len() > MAX_COMPONENTS {
                    return Err(p.new_custom_error(()));
                }
                match p.try_parse(|p| parse_transform(p, ctx, depth)) {
                    Ok(value) => values.push(value),
                    Err(_) => break,
                }
            }
            Ok(values.join(" "))
        }
    }
}

pub(super) fn length_scale(unit: &str, ctx: &Context<'_>) -> Option<f64> {
    let absolute = match unit {
        "px" => Some(1.),
        "in" => Some(96.),
        "cm" => Some(96. / 2.54),
        "mm" => Some(96. / 25.4),
        "q" => Some(96. / 101.6),
        "pt" => Some(96. / 72.),
        "pc" => Some(16.),
        _ => None,
    };
    if absolute.is_some() {
        return absolute;
    }
    let font = matches!(
        unit,
        "em" | "rem" | "ex" | "rex" | "cap" | "rcap" | "ch" | "rch" | "ic" | "ric" | "lh" | "rlh"
    );
    let container = matches!(unit, "cqw" | "cqh" | "cqi" | "cqb" | "cqmin" | "cqmax");
    if ctx.independent && (font || container) {
        return None;
    }
    if font {
        let Some(dom) = ctx.dom else {
            return Some(match unit {
                "ch" | "rch" | "ex" | "rex" => 8.,
                "lh" | "rlh" => 19.2,
                _ => 16.,
            });
        };
        let root = dom.style_scope_root_element(ctx.id).unwrap_or(DOCUMENT);
        let root_relative = unit.starts_with('r');
        let id = if root_relative { root } else { ctx.id };
        let pseudo = if root_relative { None } else { ctx.pseudo };
        let size = property_font_size(dom, id, pseudo);
        let style = || text_style(dom, id, pseudo, size);
        let local_unit = if root_relative { &unit[1..] } else { unit };
        let n = match local_unit {
            "em" => size,
            "ch" => crate::text::zero_advance(&style()),
            "ic" => crate::text::shape("水", &style()).advance,
            // CSS Values permits the x-height fallback when measuring is
            // impractical; cap and normal line-height use actual font ascent.
            "ex" => size * 0.5,
            "cap" => crate::text::shape("H", &style()).ascent,
            "lh" => {
                let raw = match pseudo {
                    Some(which) => dom.pseudo_layout_value(id, which, "line-height"),
                    None => dom.computed_value_resolved(id, "line-height"),
                };
                match raw.as_deref() {
                    Some(v) if v.parse::<f32>().is_ok() => v.parse::<f32>().ok()? * size,
                    Some(v) if v.ends_with("px") => v[..v.len() - 2].parse().ok()?,
                    _ => crate::text::shape(" ", &style()).line_height,
                }
            }
            _ => return None,
        };
        return Some(f64::from(n));
    }
    let (w, h) = ctx.dom.map_or((1000., 800.), |d| d.viewport_px);
    let vertical_at = |id| {
        ctx.dom.is_some_and(|dom| {
            dom.computed_value_resolved(id, "writing-mode")
                .is_some_and(|value| value.starts_with("vertical") || value.starts_with("sideways"))
        })
    };
    let root = ctx
        .dom
        .and_then(|dom| dom.style_scope_root_element(ctx.id))
        .unwrap_or(DOCUMENT);
    let viewport_unit = unit
        .strip_prefix(['s', 'l', 'd'])
        .filter(|_| unit.len() >= 3)
        .unwrap_or(unit);
    let viewport = match viewport_unit {
        "vw" => Some(w),
        "vh" => Some(h),
        "vi" => Some(if vertical_at(root) { h } else { w }),
        "vb" => Some(if vertical_at(root) { w } else { h }),
        "vmin" => Some(w.min(h)),
        "vmax" => Some(w.max(h)),
        _ => None,
    };
    if let Some(size) = viewport {
        return Some(f64::from(size) / 100.);
    }
    if !container {
        return None;
    }
    // Each physical/logical axis independently selects its nearest eligible
    // query container (CSS Conditional 5 #container-lengths).
    let resolve = |axis: usize| {
        if let Some(dom) = ctx.dom {
            let mut current = dom.style_parent(ctx.id);
            while let Some(node) = current {
                let kind = dom
                    .computed_value_resolved(node, "container-type")
                    .unwrap_or_default();
                let vertical = vertical_at(node);
                let physical = match axis {
                    0 | 1 => axis,
                    2 => usize::from(vertical),
                    _ => usize::from(!vertical),
                };
                let eligible = kind.split_ascii_whitespace().any(|part| {
                    part == "size" || part == "inline-size" && physical == usize::from(vertical)
                });
                if eligible && let Some(size) = dom.container_sizes.borrow().get(&node) {
                    return size[physical];
                }
                current = dom.style_parent(node);
            }
        }
        match axis {
            0 => w,
            1 => h,
            2 => {
                if vertical_at(root) {
                    h
                } else {
                    w
                }
            }
            _ => {
                if vertical_at(root) {
                    w
                } else {
                    h
                }
            }
        }
    };
    Some(
        f64::from(match unit {
            "cqw" => resolve(0),
            "cqh" => resolve(1),
            "cqi" => resolve(2),
            "cqb" => resolve(3),
            "cqmin" => resolve(2).min(resolve(3)),
            _ => resolve(2).max(resolve(3)),
        }) / 100.,
    )
}

fn property_font_size(dom: &Dom, id: NodeId, pseudo: Option<PseudoEl>) -> f32 {
    let size = dom.font_px(id);
    match pseudo {
        Some(which) => dom
            .pseudo_layout_value(id, which, "font-size")
            .as_deref()
            .and_then(|v| font_size_px(v, size, dom.root_font_px()))
            .unwrap_or(size),
        None => size,
    }
}

fn text_style(
    dom: &Dom,
    id: NodeId,
    pseudo: Option<PseudoEl>,
    size: f32,
) -> crate::text::TextStyle {
    let read = |name| match pseudo {
        Some(which) => dom.pseudo_layout_value(id, which, name),
        None => dom.computed_value_resolved(id, name),
    };
    crate::text::TextStyle {
        family: read("font-family").unwrap_or_else(|| "sans-serif".into()),
        size,
        weight: read("font-weight")
            .as_deref()
            .and_then(crate::layout2::css_font_weight)
            .unwrap_or(400.),
        italic: read("font-style")
            .as_deref()
            .is_some_and(crate::layout2::css_is_italic),
        ..Default::default()
    }
}

fn parse_url<'i>(p: &mut Parser<'i, '_>, ctx: &Context<'_>) -> ParseResult<'i, String> {
    let (name, url, modifiers) = match p.next()?.clone() {
        Token::UnquotedUrl(value) => ("url".to_string(), value.to_string(), String::new()),
        Token::Function(name)
            if name.eq_ignore_ascii_case("url") || name.eq_ignore_ascii_case("src") =>
        {
            p.parse_nested_block(|p| {
                let value = p.expect_string_cloned()?.to_string();
                let start = p.position();
                while !p.is_exhausted() {
                    match p.next()? {
                        Token::Ident(_) => {}
                        Token::Function(_) => {
                            p.parse_nested_block(|p| p.expect_no_error_token().map_err(Into::into))?
                        }
                        _ => return Err(p.new_custom_error(())),
                    }
                }
                Ok((
                    name.to_ascii_lowercase(),
                    value,
                    p.slice_from(start).trim().to_string(),
                ))
            })?
        }
        _ => return Err(p.new_custom_error(())),
    };
    // CSS Values 4 #local-urls / #url-empty: preserve fragment-only URLs and
    // empty references, including their observable computed serialization.
    let resolved = ctx
        .base
        .filter(|_| !url.is_empty() && !url.starts_with('#'))
        .and_then(|base| base.join(&url).ok())
        .map(|url| url.to_string())
        .unwrap_or(url);
    Ok(format!(
        "{name}({}{})",
        string_text(&resolved),
        if modifiers.is_empty() {
            String::new()
        } else {
            format!(" {modifiers}")
        }
    ))
}

/// Consume exactly one component value, retaining its source for parsers that
/// already implement the associated CSS type (notably the color library).
fn component_text<'i>(p: &mut Parser<'i, '_>) -> ParseResult<'i, String> {
    p.skip_whitespace();
    let start = p.position();
    let token = p.next()?.clone();
    if token.is_parse_error() {
        return Err(p.new_custom_error(()));
    }
    if matches!(
        token,
        Token::Function(_)
            | Token::ParenthesisBlock
            | Token::CurlyBracketBlock
            | Token::SquareBracketBlock
    ) {
        p.parse_nested_block(|p| p.expect_no_error_token().map_err(Into::into))?;
    }
    Ok(p.slice_from(start).to_string())
}

fn computed_color(text: &str) -> Option<String> {
    let decoded = ident(text);
    let text = decoded.as_deref().unwrap_or(text);
    if text.eq_ignore_ascii_case("currentcolor") {
        return Some("currentcolor".into());
    }
    // CSS Color 4 #css-system-colors permits a fixed UA palette. Keep each
    // foreground/background pairing legible, including deprecated aliases.
    let system = match text.to_ascii_lowercase().as_str() {
        "canvas" | "field" | "activecaption" | "appworkspace" | "background"
        | "inactivecaption" | "infobackground" | "menu" | "scrollbar" | "window" => Some("#000000"),
        "canvastext" | "fieldtext" | "buttontext" | "captiontext" | "infotext" | "menutext"
        | "windowtext" => Some("#ffffff"),
        "buttonface" | "buttonhighlight" | "buttonshadow" | "threedface" => Some("#404040"),
        "buttonborder" | "activeborder" | "inactiveborder" | "threeddarkshadow"
        | "threedhighlight" | "threedlightshadow" | "threedshadow" | "windowframe" => {
            Some("#c0c0c0")
        }
        "graytext" | "inactivecaptiontext" => Some("#808080"),
        "highlight" | "selecteditem" | "accentcolor" => Some("#00ffff"),
        "highlighttext" | "selecteditemtext" | "accentcolortext" | "marktext" => Some("#000000"),
        "linktext" => Some("#80b0ff"),
        "visitedtext" => Some("#d090ff"),
        "activetext" => Some("#ff8080"),
        "mark" => Some("#ffff00"),
        _ => None,
    };
    if let Some(color) = system {
        return computed_color(color);
    }
    if !valid_legacy_color(text) {
        return None;
    }
    let color = color::parse_color(text).ok()?;
    use color::ColorSpaceTag as Space;
    if !color.flags.missing().is_empty() {
        return modern_color(color);
    }
    let legacy = !text.trim_start().to_ascii_lowercase().starts_with("color(")
        && matches!(color.cs, Space::Srgb | Space::Hsl | Space::Hwb);
    if legacy {
        let color = color.convert(Space::Srgb);
        let [r, g, b, a] = color.components;
        let channels = [r, g, b].map(|v| math::number(f64::from(v.clamp(0., 1.)) * 255.));
        let rgb = channels.join(", ");
        return Some(if a >= 1. {
            format!("rgb({rgb})")
        } else {
            format!("rgba({rgb}, {})", math::number(f64::from(a.clamp(0., 1.))))
        });
    }
    // Reify through the explicit color space, discarding named-color spelling
    // while retaining wide-gamut channels and missing-component information.
    modern_color(color)
}

fn modern_color(color: color::DynamicColor) -> Option<String> {
    use color::ColorSpaceTag as Space;
    let prefix = match color.cs {
        Space::Srgb => "color(srgb",
        Space::LinearSrgb => "color(srgb-linear",
        Space::DisplayP3 => "color(display-p3",
        Space::A98Rgb => "color(a98-rgb",
        Space::ProphotoRgb => "color(prophoto-rgb",
        Space::Rec2020 => "color(rec2020",
        Space::XyzD50 => "color(xyz-d50",
        Space::XyzD65 => "color(xyz-d65",
        Space::Hsl => "hsl(",
        Space::Hwb => "hwb(",
        Space::Lab => "lab(",
        Space::Lch => "lch(",
        Space::Oklab => "oklab(",
        Space::Oklch => "oklch(",
        _ => return None,
    };
    let missing = color.flags.missing();
    let channel = |i: usize| {
        if missing.contains(i) {
            "none".into()
        } else {
            let n = color.components[i];
            if i == 3 {
                math::number(f64::from(n.clamp(0., 1.)))
            } else if i > 0 && matches!(color.cs, Space::Hsl | Space::Hwb) {
                format!("{}%", math::number(f64::from(n.clamp(0., 100.))))
            } else {
                math::number(f64::from(n))
            }
        }
    };
    Some(format!(
        "{prefix}{}{} {} {}{})",
        if prefix.ends_with('(') { "" } else { " " },
        channel(0),
        channel(1),
        channel(2),
        if missing.contains(3) || color.components[3] < 1. {
            format!(" / {}", channel(3))
        } else {
            String::new()
        }
    ))
}

/// The color conversion library intentionally accepts some non-CSS legacy
/// mixtures. Enforce CSS Color 4's comma-form grammar at TRust's boundary.
fn valid_legacy_color(text: &str) -> bool {
    let mut input = ParserInput::new(text);
    let mut p = Parser::new(&mut input);
    let Ok(name) = p.expect_function().map(|name| name.to_ascii_lowercase()) else {
        return true;
    };
    if !matches!(name.as_str(), "rgb" | "rgba" | "hsl" | "hsla") {
        return true;
    }
    p.parse_nested_block(|p| {
        let start = p.state();
        let first = component_text(p)?;
        if p.try_parse(|p| p.expect_comma()).is_err() {
            p.expect_no_error_token()?;
            return Ok(true);
        }
        p.reset(&start);
        let parts = p.parse_comma_separated(component_text)?;
        if !(3..=4).contains(&parts.len()) {
            return Ok(false);
        }
        let matches = |kind: Kind, text: &str| {
            Syntax::Alternatives(vec![Component {
                kind,
                multiplier: None,
            }])
            .compute(text, &Context::validation())
            .is_some()
        };
        let rgb = name.starts_with("rgb");
        let percent = matches(Kind::Percentage, &first);
        let channels = parts[..3].iter().enumerate().all(|(i, text)| {
            if rgb {
                matches(
                    if percent {
                        Kind::Percentage
                    } else {
                        Kind::Number
                    },
                    text,
                )
            } else if i == 0 {
                matches(Kind::Angle, text) || matches(Kind::Number, text)
            } else {
                matches(Kind::Percentage, text)
            }
        });
        Ok::<_, cssparser::ParseError<'_, ()>>(
            channels
                && parts.get(3).is_none_or(|alpha| {
                    matches(Kind::Number, alpha) || matches(Kind::Percentage, alpha)
                }),
        )
    })
    .unwrap_or(false)
}

pub(super) fn parse_color<'i>(
    p: &mut Parser<'i, '_>,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    let text = component_text(p)?;
    if let Some(color) = computed_color(&text) {
        return Ok(color);
    }
    // Color channels can themselves be math functions. Normalize each typed
    // numeric sub-expression without quantizing to a paint surface.
    let mut input = ParserInput::new(&text);
    let mut nested = Parser::new(&mut input);
    let name = nested
        .expect_function()
        .map_err(|_| p.new_custom_error(()))?
        .to_ascii_lowercase();
    if !matches!(
        name.as_str(),
        "rgb" | "rgba" | "hsl" | "hsla" | "hwb" | "lab" | "lch" | "oklab" | "oklch" | "color"
    ) {
        return Err(p.new_custom_error(()));
    }
    let computed = nested
        .parse_nested_block(|p| compute_components(p, ctx, depth + 1))
        .map_err(|_| p.new_custom_error(()))?;
    computed_color(&format!("{name}({computed})")).ok_or_else(|| p.new_custom_error(()))
}

fn parse_transform<'i>(
    p: &mut Parser<'i, '_>,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    let name = p.expect_function()?.to_ascii_lowercase();
    let (types, min, max): (Vec<Kind>, usize, usize) = match name.as_str() {
        "matrix" => (vec![Kind::Number; 6], 6, 6),
        "matrix3d" => (vec![Kind::Number; 16], 16, 16),
        "translate" => (vec![Kind::LengthPercentage; 2], 1, 2),
        "translatex" | "translatey" => (vec![Kind::LengthPercentage], 1, 1),
        "translatez" => (vec![Kind::Length], 1, 1),
        "translate3d" => (
            vec![Kind::LengthPercentage, Kind::LengthPercentage, Kind::Length],
            3,
            3,
        ),
        "scale" => (vec![Kind::Number; 2], 1, 2),
        "scalex" | "scaley" | "scalez" => (vec![Kind::Number], 1, 1),
        "scale3d" => (vec![Kind::Number; 3], 3, 3),
        "rotate" | "rotatex" | "rotatey" | "rotatez" => (vec![Kind::Angle], 1, 1),
        "rotate3d" => (
            vec![Kind::Number, Kind::Number, Kind::Number, Kind::Angle],
            4,
            4,
        ),
        "skew" => (vec![Kind::Angle; 2], 1, 2),
        "skewx" | "skewy" => (vec![Kind::Angle], 1, 1),
        "perspective" => (vec![Kind::Length], 1, 1),
        _ => return Err(p.new_custom_error(())),
    };
    let args = p.parse_nested_block(|p| {
        let mut args = Vec::new();
        loop {
            let Some(kind) = types.get(args.len()) else {
                return Err(p.new_custom_error(()));
            };
            if name == "perspective" && p.try_parse(|p| p.expect_ident_matching("none")).is_ok() {
                args.push("none".into());
            } else if name == "perspective" {
                args.push(math::parse_range(
                    p,
                    kind,
                    ctx,
                    depth + 1,
                    0.,
                    f64::INFINITY,
                )?);
            } else if *kind == Kind::Angle
                && p.try_parse(|p| {
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
                args.push("0deg".into());
            } else if name.starts_with("scale") {
                if let Ok(value) =
                    p.try_parse(|p| math::parse(p, &Kind::Percentage, ctx, depth + 1))
                {
                    let number = value
                        .trim_end_matches('%')
                        .parse::<f64>()
                        .map_err(|_| p.new_custom_error(()))?;
                    args.push(math::number(number / 100.));
                } else {
                    args.push(parse(p, kind, ctx, depth + 1)?);
                }
            } else {
                args.push(parse(p, kind, ctx, depth + 1)?);
            }
            if p.is_exhausted() {
                break;
            }
            p.expect_comma()?;
        }
        if args.len() < min || args.len() > max {
            return Err(p.new_custom_error(()));
        }
        Ok(args)
    })?;
    let name = match name.as_str() {
        "translatex" => "translateX",
        "translatey" => "translateY",
        "translatez" => "translateZ",
        "scalex" => "scaleX",
        "scaley" => "scaleY",
        "scalez" => "scaleZ",
        "rotatex" => "rotateX",
        "rotatey" => "rotateY",
        "rotatez" => "rotateZ",
        "skewx" => "skewX",
        "skewy" => "skewY",
        _ => &name,
    };
    Ok(format!("{name}({})", args.join(", ")))
}

fn parse_image<'i>(
    p: &mut Parser<'i, '_>,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    image_value(p, ctx, depth, false)
}

fn image_value<'i>(
    p: &mut Parser<'i, '_>,
    ctx: &Context<'_>,
    depth: usize,
    in_set: bool,
) -> ParseResult<'i, String> {
    if depth > MAX_DEPTH {
        return Err(p.new_custom_error(()));
    }
    if let Ok(url) = p.try_parse(|p| parse_url(p, ctx)) {
        return Ok(url);
    }
    let name = p.expect_function()?.to_ascii_lowercase();
    // CSS Images 4 Appendix A requires the prefixed spelling to become the
    // standard function at parse time, including its computed serialization.
    let name = if name == "-webkit-image-set" {
        "image-set".into()
    } else {
        name
    };
    let result = p.parse_nested_block(|p| match name.as_str() {
        "linear-gradient"
        | "repeating-linear-gradient"
        | "radial-gradient"
        | "repeating-radial-gradient"
        | "conic-gradient"
        | "repeating-conic-gradient" => super::gradients::parse(p, &name, ctx, depth + 1),
        "image-set" => {
            if in_set {
                return Err(p.new_custom_error(()));
            }
            let options = p.parse_comma_separated(|p| {
                let image = if let Ok(value) = p.try_parse(|p| p.expect_string_cloned()) {
                    let url = ctx
                        .base
                        .filter(|_| !value.is_empty() && !value.starts_with('#'))
                        .and_then(|base| base.join(&value).ok())
                        .map(|u| u.to_string())
                        .unwrap_or(value.to_string());
                    format!("url({})", string_text(&url))
                } else {
                    image_value(p, ctx, depth + 1, true)?
                };
                let mut resolution = None;
                let mut mime = None;
                while !p.is_exhausted() {
                    if let Ok(value) =
                        p.try_parse(|p| math::parse(p, &Kind::Resolution, ctx, depth + 1))
                    {
                        if resolution.replace(value).is_some() {
                            return Err(p.new_custom_error(()));
                        }
                    } else {
                        p.expect_function_matching("type")?;
                        let value =
                            p.parse_nested_block(|p| Ok(p.expect_string_cloned()?.to_string()))?;
                        if mime.replace(value).is_some() {
                            return Err(p.new_custom_error(()));
                        }
                    }
                }
                Ok(format!(
                    "{image} {}{}",
                    resolution.unwrap_or_else(|| "1dppx".into()),
                    mime.map(|v| format!(" type({})", string_text(&v)))
                        .unwrap_or_default()
                ))
            })?;
            Ok(options.join(", "))
        }
        "cross-fade" => {
            let images = p.parse_comma_separated(|p| {
                let before = p
                    .try_parse(|p| {
                        math::parse_range(p, &Kind::Percentage, ctx, depth + 1, 0., 100.)
                    })
                    .ok();
                let image = p
                    .try_parse(|p| image_value(p, ctx, depth + 1, in_set))
                    .or_else(|_| parse_color(p, ctx, depth + 1))?;
                let percentage = before.or_else(|| {
                    p.try_parse(|p| {
                        math::parse_range(p, &Kind::Percentage, ctx, depth + 1, 0., 100.)
                    })
                    .ok()
                });
                let percentage = percentage
                    .map(|v| v.trim_end_matches('%').parse::<f64>())
                    .transpose()
                    .map_err(|_| p.new_custom_error(()))?;
                if percentage.is_some_and(|v| !(0. ..=100.).contains(&v)) {
                    return Err(p.new_custom_error(()));
                }
                Ok((image, percentage))
            })?;
            let unspecified = images.iter().filter(|(_, p)| p.is_none()).count();
            let remainder = (100. - images.iter().filter_map(|(_, p)| *p).sum::<f64>()).max(0.);
            Ok(images
                .into_iter()
                .map(|(image, p)| {
                    format!(
                        "{image} {}%",
                        math::number(p.unwrap_or(remainder / unspecified.max(1) as f64))
                    )
                })
                .collect::<Vec<_>>()
                .join(", "))
        }
        "element" => match p.next()?.clone() {
            Token::IDHash(id) => Ok(format!("#{}", identifier_text(&id))),
            _ => Err(p.new_custom_error(())),
        },
        "image" => parse_color(p, ctx, depth + 1),
        _ => Err(p.new_custom_error(())),
    })?;
    Ok(format!("{name}({result})"))
}

/// Compute numeric/color/URL components in image geometry and color functions.
/// Function identity and separators are retained; nested values are tokenized.
fn compute_components<'i>(
    p: &mut Parser<'i, '_>,
    ctx: &Context<'_>,
    depth: usize,
) -> ParseResult<'i, String> {
    if depth > MAX_DEPTH {
        return Err(p.new_custom_error(()));
    }
    let mut out = String::new();
    let mut count = 0;
    while !p.is_exhausted() {
        count += 1;
        if count > MAX_COMPONENTS {
            return Err(p.new_custom_error(()));
        }
        if p.try_parse(|p| p.expect_comma()).is_ok() {
            out.push_str(", ");
            continue;
        }
        let mut value = None;
        for kind in [
            Kind::Url,
            Kind::Color,
            Kind::Number,
            Kind::LengthPercentage,
            Kind::Angle,
            Kind::Resolution,
        ] {
            // Avoid recursive color parsing of an unrecognized function.
            if kind == Kind::Color {
                let state = p.state();
                if let Ok(text) = component_text(p)
                    && let Some(color) = computed_color(&text)
                {
                    value = Some(color);
                    break;
                }
                p.reset(&state);
            } else if let Ok(v) = p.try_parse(|p| parse(p, &kind, ctx, depth + 1)) {
                value = Some(v);
                break;
            }
        }
        let value = if let Some(value) = value {
            value
        } else {
            match p.next()?.clone() {
                Token::Ident(value) => identifier_text(&value),
                Token::QuotedString(value) => string_text(&value),
                Token::Delim('/') => "/".into(),
                _ => return Err(p.new_custom_error(())),
            }
        };
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push_str(&value);
    }
    Ok(out)
}
