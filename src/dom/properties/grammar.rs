//! Declaration-time value grammars of the supported properties that
//! `cssom::accepts_longhand` does not check with a dedicated parser of
//! their own. CSS Syntax 3 #parse-a-declaration and CSS Cascade 5
//! #declared: a declaration whose value does not match its property's
//! grammar is invalid and dropped while parsing, so an earlier valid
//! declaration stays in force. The component types (`<length>`, `<time>`,
//! `<transform-list>`, …) are the registry's own parsers
//! (`properties::values`, CSS Values 4), and colors, images, positions,
//! counters, content and transitions reuse the engine's existing grammars.
//!
//! Grammars follow the local CSSWG snapshot 81c27f686901 (2026-09-06): CSS
//! Animations 1 #animation-*, CSS Backgrounds 3, CSS Box Alignment 3 #props,
//! CSS Color Adjust 1 #color-scheme-prop, CSS Conditional 5 #container-name,
//! CSS Containment 2 #contain-property, CSS Content 3 #content-property and
//! #quotes, CSS Fonts 4 #font-family-prop, CSS Grid 2 #track-sizing,
//! #grid-template-areas-property and #line-placement, CSS Inline 3
//! #line-height-property, CSS Lists 3, CSS Masking 1 #clip-property, CSS
//! Scroll Snap 1, CSS Sizing 4 #aspect-ratio, CSS Text 3/4, CSS Text
//! Decoration 4, CSS Transforms 1/2, CSS UI 4 #cursor and #accent-color, and
//! SVG 2 #PaintProperties / CSS Fill and Stroke 3 for the SVG properties.
use super::values;
use super::*;

/// The grammar of `property` (a physical longhand or a tracked shorthand
/// such as `flex-flow`), or `None` when another check owns it.
pub(in crate::dom) fn valid(property: &str, value: &str) -> Option<bool> {
    let lower = value.trim().to_ascii_lowercase();
    let one_of = |words: &[&str]| words.contains(&lower.as_str());
    Some(match property {
        "container-name" => {
            lower == "none"
                || all(value, |p| {
                    repeat1(p, |p| custom_ident(p, &["none", "and", "or", "not"]))
                })
        }
        "-webkit-text-stroke" => all(value, |p| any_order(p, &mut [&mut line_width, &mut color])),
        "accent-color" => lower == "auto" || all(value, color),
        "fill" | "stroke" => all(value, paint),
        "fill-rule" | "clip-rule" => one_of(&["nonzero", "evenodd"]),
        "stroke-width" | "stroke-dashoffset" => all(value, |p| {
            let min = (property == "stroke-width").then_some(0.0);
            component(p, Kind::LengthPercentage, min) || component(p, Kind::Number, min)
        }),
        "stroke-linecap" => one_of(&["butt", "round", "square"]),
        "stroke-linejoin" => one_of(&["miter", "miter-clip", "round", "bevel", "arcs"]),
        // SVG 2 #StrokeMiterlimitProperty: a value below 1 is an error.
        "stroke-miterlimit" => all(value, |p| component(p, Kind::Number, Some(1.0))),
        "stroke-dasharray" => {
            lower == "none"
                || all(value, |p| {
                    comma_list(p, |p| {
                        repeat1(p, |p| {
                            component(p, Kind::LengthPercentage, Some(0.0))
                                || component(p, Kind::Number, Some(0.0))
                        })
                    })
                })
        }
        "paint-order" => {
            lower == "normal"
                || all(value, |p| {
                    any_order(
                        p,
                        &mut [
                            &mut |p: &mut Parser| keyword(p, &["fill"]),
                            &mut |p: &mut Parser| keyword(p, &["stroke"]),
                            &mut |p: &mut Parser| keyword(p, &["markers"]),
                        ],
                    )
                })
        }
        "vector-effect" => one_of(&[
            "none",
            "non-scaling-stroke",
            "non-scaling-size",
            "non-rotation",
            "fixed-position",
        ]),
        "shape-rendering" => one_of(&["auto", "optimizespeed", "crispedges", "geometricprecision"]),
        // Filter Effects 1 #FloodColorProperty, #LightingColorProperty.
        "stop-color" | "flood-color" | "lighting-color" => all(value, color),
        // Filter Effects 1 #FloodOpacityProperty: <'opacity'>.
        "flood-opacity" => all(value, |p| {
            component(p, Kind::Number, None) || component(p, Kind::Percentage, None)
        }),
        "color-interpolation-filters" => one_of(&["auto", "srgb", "linearrgb"]),
        // SVG 2 painting.html #ColorInterpolationProperty.
        "color-interpolation" => one_of(&["auto", "srgb", "linearrgb"]),
        // CSS Masking 1 #the-mask-type.
        "mask-type" => one_of(&["luminance", "alpha"]),
        // CSS UI 4 #content-selection.
        "user-select" => one_of(&["auto", "text", "none", "contain", "all"]),
        "pointer-events" => one_of(&[
            "auto",
            "bounding-box",
            "visiblepainted",
            "visiblefill",
            "visiblestroke",
            "visible",
            "painted",
            "fill",
            "stroke",
            "all",
            "none",
        ]),
        "animation-name" => all(value, |p| {
            comma_list(p, |p| {
                keyword(p, &["none"]) || string(p) || custom_ident(p, &["none"])
            })
        }),
        "animation-duration" => all(value, |p| {
            comma_list(p, |p| {
                keyword(p, &["auto"]) || component(p, Kind::Time, Some(0.0))
            })
        }),
        "animation-delay" => all(value, |p| comma_list(p, |p| component(p, Kind::Time, None))),
        "animation-timing-function" => all(value, |p| comma_list(p, easing)),
        "animation-iteration-count" => all(value, |p| {
            comma_list(p, |p| {
                keyword(p, &["infinite"]) || component(p, Kind::Number, Some(0.0))
            })
        }),
        "animation-direction" => all(value, |p| {
            comma_list(p, |p| {
                keyword(p, &["normal", "reverse", "alternate", "alternate-reverse"])
            })
        }),
        "animation-fill-mode" => all(value, |p| {
            comma_list(p, |p| {
                keyword(p, &["none", "forwards", "backwards", "both"])
            })
        }),
        "animation-play-state" => all(value, |p| {
            comma_list(p, |p| keyword(p, &["running", "paused"]))
        }),
        "animation" => all(value, |p| comma_list(p, single_animation)),
        "transition-property"
        | "transition-duration"
        | "transition-timing-function"
        | "transition-delay" => crate::dom::transitions::valid_longhand(property, value),
        "text-align-last" => one_of(&[
            "auto",
            "start",
            "end",
            "left",
            "right",
            "center",
            "justify",
            "match-parent",
        ]),
        // CSS Color Adjust 1: normal | [ light | dark | <custom-ident> ]+
        // && only?
        "color-scheme" => {
            lower == "normal"
                || all(value, |p| {
                    let mut only = false;
                    let mut schemes = 0;
                    while !p.is_exhausted() {
                        if !only && keyword(p, &["only"]) {
                            only = true;
                        } else if keyword(p, &["light", "dark"])
                            || custom_ident(p, &["normal", "light", "dark", "only"])
                        {
                            schemes += 1;
                        } else {
                            return false;
                        }
                    }
                    schemes > 0
                })
        }
        "font-family" => all(value, |p| comma_list(p, family_name)),
        "line-height" => {
            lower == "normal"
                || all(value, |p| {
                    component(p, Kind::Number, Some(0.0))
                        || component(p, Kind::LengthPercentage, Some(0.0))
                })
        }
        "tab-size" => all(value, |p| {
            component(p, Kind::Number, Some(0.0)) || component(p, Kind::Length, Some(0.0))
        }),
        "list-style-type" => all(value, |p| {
            keyword(p, &["none"]) || string(p) || symbols(p) || custom_ident(p, &["none"])
        }),
        "list-style-image" | "border-image-source" => lower == "none" || all(value, image),
        "text-decoration-line" => {
            lower == "none"
                || one_of(&["spelling-error", "grammar-error"])
                || all(value, |p| {
                    any_order(
                        p,
                        &mut [
                            &mut |p: &mut Parser| keyword(p, &["underline"]),
                            &mut |p: &mut Parser| keyword(p, &["overline"]),
                            &mut |p: &mut Parser| keyword(p, &["line-through"]),
                            &mut |p: &mut Parser| keyword(p, &["blink"]),
                        ],
                    )
                })
        }
        "text-decoration-thickness" => {
            one_of(&["auto", "from-font"])
                || all(value, |p| component(p, Kind::LengthPercentage, None))
        }
        "text-underline-offset" => {
            lower == "auto" || all(value, |p| component(p, Kind::LengthPercentage, None))
        }
        "text-shadow" => lower == "none" || all(value, |p| comma_list(p, |p| shadow(p, false))),
        "box-shadow" => lower == "none" || all(value, |p| comma_list(p, |p| shadow(p, true))),
        "content" => crate::dom::generated::valid_content(value),
        "quotes" => {
            one_of(&["auto", "none"])
                || all(value, |p| {
                    let mut strings = 0;
                    while !p.is_exhausted() {
                        if !string(p) {
                            return false;
                        }
                        strings += 1;
                    }
                    strings > 0 && strings % 2 == 0
                })
        }
        // CSS Sizing 4: auto || <ratio>.
        "aspect-ratio" => all(value, |p| {
            any_order(
                p,
                &mut [&mut |p: &mut Parser| keyword(p, &["auto"]), &mut ratio],
            )
        }),
        "object-position" => all(value, |p| position(p, false)),
        "flex-flow" => all(value, |p| {
            any_order(
                p,
                &mut [
                    &mut |p: &mut Parser| {
                        keyword(p, &["row", "row-reverse", "column", "column-reverse"])
                    },
                    &mut |p: &mut Parser| keyword(p, &["nowrap", "wrap", "wrap-reverse"]),
                ],
            )
        }),
        "clip" => lower == "auto" || all(value, clip_rect),
        "scroll-snap-type" => {
            lower == "none"
                || all(value, |p| {
                    keyword(p, &["x", "y", "block", "inline", "both"])
                        && (p.is_exhausted() || keyword(p, &["mandatory", "proximity"]))
                })
        }
        "scroll-snap-align" => all(value, |p| {
            keyword(p, &["none", "start", "end", "center"])
                && (p.is_exhausted() || keyword(p, &["none", "start", "end", "center"]))
        }),
        "cursor" => all(value, cursor),
        "background-image" => all(value, |p| {
            comma_list(p, |p| keyword(p, &["none"]) || image_or(p, true))
        }),
        "background-repeat" => all(value, |p| comma_list(p, repeat_style)),
        "background-position" => all(value, |p| comma_list(p, |p| position(p, true))),
        "background-position-x" | "background-position-y" => {
            let (edges, logical): (&[&str], &[&str]) = if property.ends_with('x') {
                (&["left", "right"], &["x-start", "x-end"])
            } else {
                (&["top", "bottom"], &["y-start", "y-end"])
            };
            all(value, |p| {
                comma_list(p, |p| {
                    if keyword(p, &["center"]) {
                        return true;
                    }
                    let edge = keyword(p, edges) || keyword(p, logical);
                    let offset = component(p, Kind::LengthPercentage, None);
                    edge || offset
                })
            })
        }
        "background-size" => crate::dom::split_top_level_commas(value)
            .into_iter()
            .all(|layer| crate::dom::valid_bg_size(layer.trim())),
        "background-origin" => all(value, |p| {
            comma_list(p, |p| {
                keyword(p, &["border-box", "padding-box", "content-box"])
            })
        }),
        "background-clip" => all(value, |p| {
            comma_list(p, |p| {
                keyword(
                    p,
                    &[
                        "border-box",
                        "padding-box",
                        "content-box",
                        "border-area",
                        "text",
                    ],
                )
            })
        }),
        "background-attachment" => all(value, |p| {
            comma_list(p, |p| keyword(p, &["scroll", "fixed", "local"]))
        }),
        "mask-image" | "mask-mode" | "mask-repeat" | "mask-position" | "mask-clip"
        | "mask-origin" | "mask-size" | "mask-composite" => {
            crate::dom::valid_mask_longhand(property, value)
        }
        "counter-reset" | "counter-increment" | "counter-set" => {
            crate::dom::generated::valid_changes(value, property)
        }
        "transform" => {
            lower == "none"
                || all(value, |p| {
                    values::parse(p, &Kind::TransformList, &Context::validation(), 0).is_ok()
                })
        }
        "transform-origin" => all(value, transform_origin),
        "transform-box" => one_of(&[
            "content-box",
            "border-box",
            "fill-box",
            "stroke-box",
            "view-box",
        ]),
        "translate" => {
            lower == "none"
                || all(value, |p| {
                    component(p, Kind::LengthPercentage, None)
                        && (p.is_exhausted()
                            || component(p, Kind::LengthPercentage, None)
                                && (p.is_exhausted() || component(p, Kind::Length, None)))
                })
        }
        "rotate" => lower == "none" || all(value, rotate),
        "scale" => {
            lower == "none"
                || all(value, |p| {
                    let mut count = 0;
                    while !p.is_exhausted() && count < 3 {
                        if !(component(p, Kind::Number, None)
                            || component(p, Kind::Percentage, None)
                            || component(p, Kind::NumberPercentage, None))
                        {
                            return false;
                        }
                        count += 1;
                    }
                    count > 0
                })
        }
        "grid-template-columns" | "grid-template-rows" => {
            lower == "none" || all(value, track_list_or_subgrid)
        }
        "grid-auto-columns" | "grid-auto-rows" => all(value, |p| repeat1(p, track_size)),
        "grid-auto-flow" => all(value, |p| {
            any_order(
                p,
                &mut [
                    &mut |p: &mut Parser| keyword(p, &["row", "column"]),
                    &mut |p: &mut Parser| keyword(p, &["dense"]),
                ],
            )
        }),
        "grid-row-start" | "grid-row-end" | "grid-column-start" | "grid-column-end" => {
            all(value, grid_line)
        }
        "grid-row" | "grid-column" => all(value, |p| slash_list(p, 2, grid_line)),
        "grid-area" => all(value, |p| slash_list(p, 4, grid_line)),
        "grid-template-areas" => lower == "none" || crate::layout2::valid_grid_areas(value),
        "contain" => {
            one_of(&["none", "strict", "content"])
                || all(value, |p| {
                    any_order(
                        p,
                        &mut [
                            &mut |p: &mut Parser| keyword(p, &["size", "inline-size"]),
                            &mut |p: &mut Parser| keyword(p, &["layout"]),
                            &mut |p: &mut Parser| keyword(p, &["style"]),
                            &mut |p: &mut Parser| keyword(p, &["paint"]),
                        ],
                    )
                })
        }
        "align-content" => all(value, |p| content_alignment(p, false)),
        "justify-content" => all(value, |p| content_alignment(p, true)),
        "align-self" => all(value, |p| self_alignment(p, Axis::AlignSelf)),
        "justify-self" => all(value, |p| self_alignment(p, Axis::JustifySelf)),
        "align-items" => all(value, |p| self_alignment(p, Axis::AlignItems)),
        "justify-items" => all(value, |p| self_alignment(p, Axis::JustifyItems)),
        "place-self" => all(value, |p| {
            self_alignment(p, Axis::AlignSelf)
                && (p.is_exhausted() || self_alignment(p, Axis::JustifySelf))
        }),
        "place-items" => all(value, |p| {
            self_alignment(p, Axis::AlignItems)
                && (p.is_exhausted() || self_alignment(p, Axis::JustifyItems))
        }),
        "border-spacing" => all(value, |p| {
            component(p, Kind::Length, Some(0.0))
                && (p.is_exhausted() || component(p, Kind::Length, Some(0.0)))
        }),
        "border-image-slice" => all(value, |p| {
            let mut fill = false;
            let mut numbers = 0;
            while !p.is_exhausted() {
                if !fill && keyword(p, &["fill"]) {
                    fill = true;
                    // `fill` comes before or after the numbers, not inside.
                    if numbers > 0 && !p.is_exhausted() {
                        return false;
                    }
                } else if numbers < 4
                    && (component(p, Kind::Number, Some(0.0))
                        || component(p, Kind::Percentage, Some(0.0)))
                {
                    numbers += 1;
                } else {
                    return false;
                }
            }
            numbers > 0
        }),
        "border-image-width" => all(value, |p| {
            up_to(p, 4, |p| {
                keyword(p, &["auto"])
                    || component(p, Kind::LengthPercentage, Some(0.0))
                    || component(p, Kind::Number, Some(0.0))
            })
        }),
        "border-image-outset" => all(value, |p| {
            up_to(p, 4, |p| {
                component(p, Kind::Length, Some(0.0)) || component(p, Kind::Number, Some(0.0))
            })
        }),
        "border-image-repeat" => all(value, |p| {
            up_to(p, 2, |p| {
                keyword(p, &["stretch", "repeat", "round", "space"])
            })
        }),
        // CSS UI 4 #outline: <'outline-width'> || <'outline-style'> ||
        // <'outline-color'>.
        "outline" => all(value, |p| {
            any_order(
                p,
                &mut [
                    &mut line_width,
                    &mut |p: &mut Parser| {
                        keyword(
                            p,
                            &[
                                "auto", "none", "dotted", "dashed", "solid", "double", "groove",
                                "ridge", "inset", "outset",
                            ],
                        )
                    },
                    &mut |p: &mut Parser| keyword(p, &["auto"]) || color(p),
                ],
            )
        }),
        "display" => display(&lower),
        // Tracked shorthands (CSS Text Decoration 4 #text-decoration-property,
        // CSS Flexbox 1 #flex-property): valid when they expand into valid
        // longhands.
        "text-decoration" | "flex" => {
            let expanded = crate::dom::expand_box_shorthand(property, value);
            expanded.iter().any(|(name, _)| name != property)
                && expanded
                    .iter()
                    .filter(|(name, _)| name != property)
                    .all(|(name, value)| crate::dom::cssom::accepts_longhand(name, value))
        }
        _ => return None,
    })
}

/// CSS Display 3 #the-display-properties: the multi-keyword values with a
/// legacy single keyword the engine implements, and the single keywords.
fn display(value: &str) -> bool {
    normalize_display(value).is_some()
}

/// CSS Display 3 #display-value-summary: the legacy keyword equivalent to a
/// supported `display` value (`block flex` is `flex`, `inline flow-root`
/// is `inline-block`), or `None` when it is not supported.
pub(in crate::dom) fn normalize_display(value: &str) -> Option<&'static str> {
    const SINGLE: &[&str] = &[
        "none",
        "contents",
        "block",
        "inline",
        "inline-block",
        "flow-root",
        "list-item",
        "flex",
        "inline-flex",
        "grid",
        "inline-grid",
        "table",
        "inline-table",
        "table-caption",
        "table-cell",
        "table-row",
        "table-row-group",
        "table-header-group",
        "table-footer-group",
        "table-column",
        "table-column-group",
        "-webkit-box",
        "-webkit-inline-box",
    ];
    let tokens: Vec<&str> = value.split_ascii_whitespace().collect();
    if let [one] = tokens.as_slice() {
        return SINGLE.iter().find(|keyword| **keyword == *one).copied();
    }
    // <display-outside> || <display-inside>, with `list-item` for flow and
    // flow-root block containers.
    let mut outside = None;
    let mut inside = None;
    let mut list_item = false;
    for token in tokens {
        match token {
            "block" | "inline" if outside.is_none() => outside = Some(token),
            "flow" | "flow-root" | "flex" | "grid" | "table" if inside.is_none() => {
                inside = Some(token)
            }
            "list-item" if !list_item => list_item = true,
            _ => return None,
        }
    }
    let outside = outside.unwrap_or("block");
    if list_item {
        // Only a block-level flow list item has a legacy keyword.
        return (outside == "block" && matches!(inside, None | Some("flow")))
            .then_some("list-item");
    }
    Some(match (outside, inside.unwrap_or("flow")) {
        ("block", "flow") => "block",
        ("block", "flow-root") => "flow-root",
        ("inline", "flow") => "inline",
        ("inline", "flow-root") => "inline-block",
        ("block", "flex") => "flex",
        ("inline", "flex") => "inline-flex",
        ("block", "grid") => "grid",
        ("inline", "grid") => "inline-grid",
        ("block", "table") => "table",
        ("inline", "table") => "inline-table",
        _ => return None,
    })
}

/// The whole value is matched by `grammar`.
fn all(value: &str, grammar: impl FnOnce(&mut Parser) -> bool) -> bool {
    let mut parser = Parser::new(value);
    let matched = grammar(&mut parser);
    parser.skip_whitespace();
    matched && parser.is_exhausted()
}

/// Run `grammar` as one alternative: on failure the input is rewound.
fn attempt(p: &mut Parser, grammar: impl FnOnce(&mut Parser) -> bool) -> bool {
    let start = p.state();
    if grammar(p) {
        true
    } else {
        p.reset(&start);
        false
    }
}

/// One of `words` (ASCII case-insensitive).
fn keyword(p: &mut Parser, words: &[&str]) -> bool {
    attempt(p, |p| {
        p.expect_ident()
            .is_ok_and(|ident| words.iter().any(|word| ident.eq_ignore_ascii_case(word)))
    })
}

/// A `<custom-ident>` other than the CSS-wide keywords, `default` and the
/// `excluded` keywords of the property's own grammar.
fn custom_ident(p: &mut Parser, excluded: &[&str]) -> bool {
    attempt(p, |p| {
        p.expect_ident().is_ok_and(|ident| {
            super::custom_ident(ident)
                && !excluded.iter().any(|word| ident.eq_ignore_ascii_case(word))
        })
    })
}

fn string(p: &mut Parser) -> bool {
    attempt(p, |p| p.expect_string().is_ok())
}

/// One component of `kind`. A literal below `min` is out of range (CSS
/// Values 4 #numeric-ranges); a math function is clamped instead.
fn component(p: &mut Parser, kind: Kind, min: Option<f32>) -> bool {
    attempt(p, |p| {
        let start = p.state();
        let literal = match p.next() {
            Ok(Token::Number { value, .. } | Token::Dimension { value, .. }) => Some(*value),
            Ok(Token::Percentage { unit_value, .. }) => Some(*unit_value * 100.0),
            _ => None,
        };
        p.reset(&start);
        values::parse(p, &kind, &Context::validation(), 0).is_ok()
            && min.is_none_or(|min| literal.is_none_or(|value| value >= min))
    })
}

/// The text of the next component value (a token, or a function or block
/// with its contents).
fn next_text<'i>(p: &mut Parser<'i>) -> Option<&'i str> {
    p.skip_whitespace();
    let start = p.position();
    let nested = matches!(
        p.next().ok()?,
        Token::Function(_)
            | Token::ParenthesisBlock
            | Token::SquareBracketBlock
            | Token::CurlyBracketBlock
    );
    if nested {
        p.parse_nested_block(|p| {
            while p.next().is_ok() {}
            Ok::<_, cssparser::ParseError<()>>(())
        })
        .ok()?;
    }
    Some(p.slice_from(start))
}

fn color(p: &mut Parser) -> bool {
    attempt(p, |p| {
        next_text(p).is_some_and(|text| crate::dom::supports_color_value(text.trim()))
    })
}

/// `<image>`, including CSS Color 5 #light-dark's image form when both
/// branches are images (or `none` where `none_ok`).
fn image(p: &mut Parser) -> bool {
    image_or(p, false)
}

fn image_or(p: &mut Parser, none_ok: bool) -> bool {
    attempt(p, |p| {
        next_text(p).is_some_and(|text| {
            let text = text.trim();
            let image = |text: &str| {
                (none_ok && text.eq_ignore_ascii_case("none"))
                    || crate::dom::valid_background_image(text)
            };
            image(text)
                || text.len() > 11
                    && text[..11].eq_ignore_ascii_case("light-dark(")
                    && [
                        crate::dom::color_scheme::ColorScheme::Light,
                        crate::dom::color_scheme::ColorScheme::Dark,
                    ]
                    .into_iter()
                    .all(|scheme| {
                        crate::dom::color_scheme::resolve(text, scheme)
                            .is_some_and(|resolved| image(resolved.trim()))
                    })
        })
    })
}

fn url(p: &mut Parser) -> bool {
    attempt(p, |p| {
        values::parse(p, &Kind::Url, &Context::validation(), 0).is_ok()
    })
}

/// `<line-width>`: a non-negative `<length>`, `thin`, `medium` or `thick`.
fn line_width(p: &mut Parser) -> bool {
    keyword(p, &["thin", "medium", "thick"]) || component(p, Kind::Length, Some(0.0))
}

/// `<easing-function>`, as `transitions::Easing` implements it.
fn easing(p: &mut Parser) -> bool {
    attempt(p, |p| {
        next_text(p).is_some_and(|text| crate::dom::transitions::Easing::parse(text).is_some())
    })
}

/// Whether the next token is a comma (left for `comma_list`).
fn at_comma(p: &mut Parser) -> bool {
    let start = p.state();
    let comma = matches!(p.next(), Ok(Token::Comma));
    p.reset(&start);
    comma
}

/// One or more `grammar` items.
fn repeat1(p: &mut Parser, mut grammar: impl FnMut(&mut Parser) -> bool) -> bool {
    let mut count = 0;
    while !p.is_exhausted() && grammar(p) {
        count += 1;
    }
    count > 0
}

/// One to `max` `grammar` items.
fn up_to(p: &mut Parser, max: usize, mut grammar: impl FnMut(&mut Parser) -> bool) -> bool {
    let mut count = 0;
    while !p.is_exhausted() && count < max && grammar(p) {
        count += 1;
    }
    count > 0
}

/// A comma-separated list of `grammar` items (CSS Values 4 `#`).
fn comma_list(p: &mut Parser, mut grammar: impl FnMut(&mut Parser) -> bool) -> bool {
    loop {
        if !grammar(p) {
            return false;
        }
        p.skip_whitespace();
        if p.is_exhausted() {
            return true;
        }
        if p.expect_comma().is_err() {
            return false;
        }
    }
}

/// Up to `max` `grammar` items separated by `/`.
fn slash_list(p: &mut Parser, max: usize, mut grammar: impl FnMut(&mut Parser) -> bool) -> bool {
    for count in 1..=max {
        if !grammar(p) {
            return false;
        }
        p.skip_whitespace();
        if p.is_exhausted() {
            return true;
        }
        if count == max || p.expect_delim('/').is_err() {
            return false;
        }
    }
    false
}

/// `a || b || …` (CSS Values 4 #comb-any): each alternative at most once,
/// in any order, at least one.
fn any_order(p: &mut Parser, grammars: &mut [&mut dyn FnMut(&mut Parser) -> bool]) -> bool {
    let mut used = vec![false; grammars.len()];
    let mut any = false;
    'outer: while !p.is_exhausted() {
        for (index, grammar) in grammars.iter_mut().enumerate() {
            if !used[index] && grammar(p) {
                used[index] = true;
                any = true;
                p.skip_whitespace();
                continue 'outer;
            }
        }
        return false;
    }
    any
}

/// SVG 2 `<paint>`: none | <color> | <url> [none | <color>]? |
/// context-fill | context-stroke.
fn paint(p: &mut Parser) -> bool {
    if keyword(p, &["none", "context-fill", "context-stroke"]) {
        return true;
    }
    if url(p) {
        p.skip_whitespace();
        return p.is_exhausted() || keyword(p, &["none"]) || color(p);
    }
    color(p)
}

/// CSS Animations 1 `<single-animation>`: `<time [0s,∞]> || <easing-function>
/// || <time> || <single-animation-iteration-count> ||
/// <single-animation-direction> || <single-animation-fill-mode> ||
/// <single-animation-play-state> || [ none | <keyframes-name> ]`.
fn single_animation(p: &mut Parser) -> bool {
    let mut times = 0;
    let (mut easing_seen, mut count, mut direction, mut fill, mut state, mut name) =
        (false, false, false, false, false, false);
    let mut any = false;
    loop {
        p.skip_whitespace();
        if p.is_exhausted() || at_comma(p) {
            break;
        }
        let matched = if times < 2 && component(p, Kind::Time, (times == 0).then_some(0.0)) {
            times += 1;
            true
        } else if !easing_seen && easing(p) {
            easing_seen = true;
            true
        } else if !count && (keyword(p, &["infinite"]) || component(p, Kind::Number, Some(0.0))) {
            count = true;
            true
        } else if !direction && keyword(p, &["normal", "reverse", "alternate", "alternate-reverse"])
        {
            direction = true;
            true
        } else if !fill && keyword(p, &["forwards", "backwards", "both"]) {
            fill = true;
            true
        } else if !state && keyword(p, &["running", "paused"]) {
            state = true;
            true
        } else if !name && (string(p) || custom_ident(p, &[])) {
            name = true;
            true
        } else if !fill && keyword(p, &["none"]) {
            // `none` is a fill mode first, then the animation name.
            fill = true;
            true
        } else {
            false
        };
        if !matched {
            return false;
        }
        any = true;
    }
    any
}

/// CSS Fonts 4 `<family-name> | <generic-family>`: a string, or a sequence
/// of identifiers (none of them a CSS-wide keyword or `default`).
fn family_name(p: &mut Parser) -> bool {
    if string(p) {
        return true;
    }
    let mut idents = 0;
    while attempt(p, |p| {
        p.expect_ident()
            .is_ok_and(|ident| super::custom_ident(ident))
    }) {
        idents += 1;
    }
    idents > 0
}

/// CSS Counter Styles 3 #symbols-function: `symbols( <symbols-type>?
/// [ <string> | <image> ]+ )`.
fn symbols(p: &mut Parser) -> bool {
    attempt(p, |p| {
        p.expect_function_matching("symbols").is_ok()
            && p.parse_nested_block(|p| {
                keyword(p, &["cyclic", "numeric", "alphabetic", "symbolic", "fixed"]);
                let ok = repeat1(p, |p| string(p) || image(p));
                if ok && p.is_exhausted() {
                    Ok(())
                } else {
                    Err(cssparser::ParseError::<()>::custom(()))
                }
            })
            .is_ok()
    })
}

/// `<shadow>` (CSS Backgrounds 3, with `spread` and `inset`) or
/// `<shadow-t>` (CSS Text Decoration 4): a color, two lengths, an optional
/// non-negative blur and (box shadows) spread, in either order.
fn shadow(p: &mut Parser, box_shadow: bool) -> bool {
    let (mut lengths_done, mut color_seen, mut inset) = (false, false, false);
    loop {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        if at_comma(p) {
            break;
        }
        if !lengths_done && component(p, Kind::Length, None) {
            if !component(p, Kind::Length, None) {
                return false;
            }
            if component(p, Kind::Length, Some(0.0)) && box_shadow {
                component(p, Kind::Length, None);
            }
            lengths_done = true;
        } else if !color_seen && color(p) {
            color_seen = true;
        } else if box_shadow && !inset && keyword(p, &["inset"]) {
            inset = true;
        } else {
            return false;
        }
    }
    lengths_done
}

/// `<ratio>`: `<number [0,∞]> [ / <number [0,∞]> ]?`.
fn ratio(p: &mut Parser) -> bool {
    attempt(p, |p| {
        if !component(p, Kind::Number, Some(0.0)) {
            return false;
        }
        p.skip_whitespace();
        !attempt(p, |p| p.expect_delim('/').is_ok()) || component(p, Kind::Number, Some(0.0))
    })
}

/// CSS Masking 1 #clip-property: `rect( <top>, <right>, <bottom>, <left> )`
/// with `<length> | auto` components (and the CSS 2 space-separated form).
fn clip_rect(p: &mut Parser) -> bool {
    attempt(p, |p| {
        p.expect_function_matching("rect").is_ok()
            && p.parse_nested_block(|p| {
                let side =
                    |p: &mut Parser| keyword(p, &["auto"]) || component(p, Kind::Length, None);
                let mut sides = 0;
                let mut commas = 0;
                while !p.is_exhausted() {
                    if sides > commas && attempt(p, |p| p.expect_comma().is_ok()) {
                        commas += 1;
                        continue;
                    }
                    if !side(p) {
                        return Err(cssparser::ParseError::<()>::custom(()));
                    }
                    sides += 1;
                }
                if sides == 4 && (commas == 0 || commas == 3) {
                    Ok(())
                } else {
                    Err(cssparser::ParseError::<()>::custom(()))
                }
            })
            .is_ok()
    })
}

/// CSS UI 4 #cursor: `[ [ <url> | <image> ] [ <number> <number> ]? , ]*
/// <cursor-predefined>`.
fn cursor(p: &mut Parser) -> bool {
    const PREDEFINED: &[&str] = &[
        "auto",
        "default",
        "none",
        "context-menu",
        "help",
        "pointer",
        "progress",
        "wait",
        "cell",
        "crosshair",
        "text",
        "vertical-text",
        "alias",
        "copy",
        "move",
        "no-drop",
        "not-allowed",
        "grab",
        "grabbing",
        "e-resize",
        "n-resize",
        "ne-resize",
        "nw-resize",
        "s-resize",
        "se-resize",
        "sw-resize",
        "w-resize",
        "ew-resize",
        "ns-resize",
        "nesw-resize",
        "nwse-resize",
        "col-resize",
        "row-resize",
        "all-scroll",
        "zoom-in",
        "zoom-out",
        // WHATWG Compatibility #css-keyword-mappings.
        "-webkit-grab",
        "-webkit-grabbing",
        "-webkit-zoom-in",
        "-webkit-zoom-out",
    ];
    loop {
        if keyword(p, PREDEFINED) {
            return true;
        }
        if !(url(p) || image(p)) {
            return false;
        }
        if component(p, Kind::Number, None) && !component(p, Kind::Number, None) {
            return false;
        }
        p.skip_whitespace();
        if p.expect_comma().is_err() {
            return false;
        }
    }
}

/// CSS Backgrounds 3 `<repeat-style>`.
fn repeat_style(p: &mut Parser) -> bool {
    keyword(p, &["repeat-x", "repeat-y"])
        || up_to(p, 2, |p| {
            keyword(p, &["repeat", "space", "round", "no-repeat"])
        })
}

/// CSS Values 4 `<position>`, or (`background`) CSS Backgrounds 3
/// `<bg-position>`, which also allows three-value forms.
fn position(p: &mut Parser, background: bool) -> bool {
    let start = p.state();
    let mut items: Vec<Option<&'static str>> = Vec::new();
    while items.len() < 4 {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        if at_comma(p) {
            break;
        }
        if let Some(word) = ["left", "right", "top", "bottom", "center"]
            .into_iter()
            .find(|word| keyword(p, &[word]))
        {
            items.push(Some(word));
        } else if component(p, Kind::LengthPercentage, None) {
            items.push(None);
        } else {
            break;
        }
    }
    let horizontal = |item: Option<&str>| matches!(item, Some("left" | "right"));
    let vertical = |item: Option<&str>| matches!(item, Some("top" | "bottom"));
    let edge = |item: Option<&str>| horizontal(item) || vertical(item);
    let valid = match items.as_slice() {
        [_] => true,
        [a, b] => {
            // [left|center|right|<lp>] [top|center|bottom|<lp>], or the
            // two keywords in either order.
            (!vertical(*a) && !horizontal(*b))
                || (a.is_some()
                    && b.is_some()
                    && !(horizontal(*a) && horizontal(*b))
                    && !(vertical(*a) && vertical(*b)))
        }
        // [center | [left|right] <lp>?] && [center | [top|bottom] <lp>?]
        // with one offset: an edge with its offset, and a keyword of the
        // other axis (or `center`) before or after it.
        [a, b, c] if background => {
            let other_axis = |keyword: Option<&str>, edge_word: Option<&str>| {
                keyword == Some("center")
                    || (edge(keyword) && horizontal(keyword) != horizontal(edge_word))
            };
            (edge(*a) && b.is_none() && other_axis(*c, *a))
                || (c.is_none() && edge(*b) && other_axis(*a, *b))
        }
        [a, b, c, d] => {
            edge(*a) && b.is_none() && edge(*c) && d.is_none() && horizontal(*a) != horizontal(*c)
        }
        _ => false,
    };
    if !valid {
        p.reset(&start);
    }
    valid
}

/// CSS Transforms 1 #transform-origin-property: one position value, or
/// two followed by an optional `<length>` (never a percentage) for z.
fn transform_origin(p: &mut Parser) -> bool {
    let mut items: Vec<Option<&'static str>> = Vec::new();
    while items.len() < 2 && !p.is_exhausted() {
        if let Some(word) = ["left", "right", "top", "bottom", "center"]
            .into_iter()
            .find(|word| keyword(p, &[word]))
        {
            items.push(Some(word));
        } else if component(p, Kind::LengthPercentage, None) {
            items.push(None);
        } else {
            return false;
        }
        p.skip_whitespace();
    }
    let horizontal = |item: Option<&str>| matches!(item, Some("left" | "right"));
    let vertical = |item: Option<&str>| matches!(item, Some("top" | "bottom"));
    match items.as_slice() {
        [_] => true,
        [a, b] => {
            ((!vertical(*a) && !horizontal(*b))
                || (a.is_some()
                    && b.is_some()
                    && !(horizontal(*a) && horizontal(*b))
                    && !(vertical(*a) && vertical(*b))))
                && (p.is_exhausted() || component(p, Kind::Length, None))
        }
        _ => false,
    }
}

/// CSS Transforms 2 #individual-transforms `rotate`: `<angle> | [ x | y | z
/// | <number>{3} ] && <angle>`.
fn rotate(p: &mut Parser) -> bool {
    let axis = |p: &mut Parser| {
        keyword(p, &["x", "y", "z"])
            || attempt(p, |p| (0..3).all(|_| component(p, Kind::Number, None)))
    };
    if component(p, Kind::Angle, None) {
        p.skip_whitespace();
        return p.is_exhausted() || axis(p);
    }
    axis(p) && component(p, Kind::Angle, None)
}

/// CSS Grid 2 `<line-names>`: `[ <custom-ident excluding span and auto>* ]`.
fn line_names(p: &mut Parser) -> bool {
    attempt(p, |p| {
        p.expect_square_bracket_block().is_ok()
            && p.parse_nested_block(|p| {
                while !p.is_exhausted() {
                    if !custom_ident(p, &["span", "auto"]) {
                        return Err(cssparser::ParseError::<()>::custom(()));
                    }
                }
                Ok(())
            })
            .is_ok()
    })
}

/// CSS Grid 2 `<track-breadth>`/`<inflexible-breadth>`.
fn breadth(p: &mut Parser, flex: bool) -> bool {
    keyword(p, &["auto", "min-content", "max-content"])
        || component(p, Kind::LengthPercentage, Some(0.0))
        || (flex && component(p, Kind::Flex, Some(0.0)))
}

/// CSS Grid 2 `<track-size>`: `<track-breadth> | minmax( <inflexible-breadth>
/// , <track-breadth> ) | fit-content( <length-percentage [0,∞]> )`.
fn track_size(p: &mut Parser) -> bool {
    breadth(p, true)
        || attempt(p, |p| {
            p.expect_function_matching("minmax").is_ok()
                && p.parse_nested_block(|p| {
                    let ok = breadth(p, false)
                        && p.expect_comma().is_ok()
                        && breadth(p, true)
                        && p.is_exhausted();
                    if ok {
                        Ok(())
                    } else {
                        Err(cssparser::ParseError::<()>::custom(()))
                    }
                })
                .is_ok()
        })
        || attempt(p, |p| {
            p.expect_function_matching("fit-content").is_ok()
                && p.parse_nested_block(|p| {
                    if component(p, Kind::LengthPercentage, Some(0.0)) && p.is_exhausted() {
                        Ok(())
                    } else {
                        Err(cssparser::ParseError::<()>::custom(()))
                    }
                })
                .is_ok()
        })
}

/// CSS Grid 2 #track-sizing: `<track-list> | <auto-track-list> | subgrid
/// <line-name-list>?`. `repeat()` takes a positive integer, `auto-fill` or
/// `auto-fit` (at most one automatic repetition per list).
fn track_list_or_subgrid(p: &mut Parser) -> bool {
    if keyword(p, &["subgrid"]) {
        loop {
            p.skip_whitespace();
            if p.is_exhausted() {
                return true;
            }
            let repeat = attempt(p, |p| {
                p.expect_function_matching("repeat").is_ok()
                    && p.parse_nested_block(|p| {
                        let count =
                            keyword(p, &["auto-fill"]) || component(p, Kind::Integer, Some(1.0));
                        let ok = count && p.expect_comma().is_ok() && repeat1(p, line_names);
                        if ok && p.is_exhausted() {
                            Ok(())
                        } else {
                            Err(cssparser::ParseError::<()>::custom(()))
                        }
                    })
                    .is_ok()
            });
            if !repeat && !line_names(p) {
                return false;
            }
        }
    }
    let mut tracks = 0;
    let mut auto_repeats = 0;
    loop {
        p.skip_whitespace();
        if p.is_exhausted() {
            break;
        }
        if line_names(p) {
            continue;
        }
        let mut automatic = false;
        let repeat = attempt(p, |p| {
            p.expect_function_matching("repeat").is_ok()
                && p.parse_nested_block(|p| {
                    automatic = keyword(p, &["auto-fill", "auto-fit"]);
                    let count = automatic || component(p, Kind::Integer, Some(1.0));
                    if !count || p.expect_comma().is_err() {
                        return Err(cssparser::ParseError::<()>::custom(()));
                    }
                    let mut sizes = 0;
                    while !p.is_exhausted() {
                        if line_names(p) {
                            continue;
                        }
                        if !track_size(p) {
                            return Err(cssparser::ParseError::<()>::custom(()));
                        }
                        sizes += 1;
                    }
                    if sizes > 0 {
                        Ok(())
                    } else {
                        Err(cssparser::ParseError::<()>::custom(()))
                    }
                })
                .is_ok()
        });
        if repeat {
            auto_repeats += usize::from(automatic);
            tracks += 1;
            continue;
        }
        if !track_size(p) {
            return false;
        }
        tracks += 1;
    }
    tracks > 0 && auto_repeats <= 1
}

/// CSS Grid 2 `<grid-line>`: `auto | <custom-ident> | [ [ <integer [-∞,-1]>
/// | <integer [1,∞]> ] && <custom-ident>? ] | [ span && [ <integer [1,∞]> ||
/// <custom-ident> ] ]`.
fn grid_line(p: &mut Parser) -> bool {
    if keyword(p, &["auto"]) {
        return true;
    }
    let (mut span, mut integer, mut name) = (false, None::<i32>, false);
    loop {
        p.skip_whitespace();
        let start = p.state();
        match p.next() {
            Ok(Token::Delim('/')) | Err(_) => {
                p.reset(&start);
                break;
            }
            _ => p.reset(&start),
        }
        if !span && keyword(p, &["span"]) {
            span = true;
        } else if integer.is_none()
            && let Ok(value) = p.try_parse(|p| p.expect_integer())
        {
            integer = Some(value);
        } else if !name && custom_ident(p, &["span", "auto"]) {
            name = true;
        } else {
            return false;
        }
    }
    match (span, integer) {
        (_, Some(0)) => false,
        (true, Some(n)) => n > 0,
        (true, None) => name,
        (false, _) => integer.is_some() || name,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Axis {
    AlignSelf,
    JustifySelf,
    AlignItems,
    JustifyItems,
}

/// CSS Box Alignment 3 `<baseline-position>`: `[ first | last ]? baseline`.
fn baseline(p: &mut Parser) -> bool {
    attempt(p, |p| {
        keyword(p, &["first", "last"]);
        keyword(p, &["baseline"])
    })
}

/// CSS Box Alignment 3 #align-justify-content: `normal |
/// <baseline-position> (align only) | <content-distribution> |
/// <overflow-position>? <content-position>` (with `left | right` for
/// justify-content).
fn content_alignment(p: &mut Parser, justify: bool) -> bool {
    if keyword(
        p,
        &[
            "normal",
            "space-between",
            "space-around",
            "space-evenly",
            "stretch",
        ],
    ) {
        return true;
    }
    if !justify && baseline(p) {
        return true;
    }
    keyword(p, &["safe", "unsafe"]);
    keyword(p, &["center", "start", "end", "flex-start", "flex-end"])
        || (justify && keyword(p, &["left", "right"]))
}

/// CSS Box Alignment 3 #justify-self-property, #align-self-property,
/// #align-items-property and #justify-items-property (with CSS Anchor
/// Positioning 1 `anchor-center`).
fn self_alignment(p: &mut Parser, axis: Axis) -> bool {
    let justify = matches!(axis, Axis::JustifySelf | Axis::JustifyItems);
    if matches!(axis, Axis::AlignSelf | Axis::JustifySelf) && keyword(p, &["auto"]) {
        return true;
    }
    if keyword(p, &["normal", "stretch", "anchor-center"]) || baseline(p) {
        return true;
    }
    if axis == Axis::JustifyItems
        && attempt(p, |p| {
            if keyword(p, &["legacy"]) {
                keyword(p, &["left", "right", "center"]);
                true
            } else {
                keyword(p, &["left", "right", "center"]) && keyword(p, &["legacy"])
            }
        })
    {
        return true;
    }
    keyword(p, &["safe", "unsafe"]);
    keyword(
        p,
        &[
            "center",
            "start",
            "end",
            "self-start",
            "self-end",
            "flex-start",
            "flex-end",
        ],
    ) || (justify && keyword(p, &["left", "right"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CSS Syntax 3 #parse-a-declaration: every supported property has a
    /// declaration-time grammar, so none accepts an arbitrary value.
    #[test]
    fn every_supported_property_validates_its_value() {
        let open: Vec<&str> = crate::dom::PROPS
            .iter()
            .map(|p| p.name)
            .filter(|name| {
                ["@", "1px @", "%%"]
                    .iter()
                    .any(|value| crate::dom::cssom::accepts_longhand(name, value))
            })
            .collect();
        assert!(open.is_empty(), "{open:?}");
        // A declaration that fails its grammar keeps the earlier one.
        let dom = crate::dom::Dom::parse_document(
            "<style>#a{stroke-linecap:round;stroke-linecap:bogus;\
             grid-template-columns:1fr 2fr;grid-template-columns:1fr -2fr;\
             transform:rotate(1deg);transform:spin(1deg);\
             display:inline flex;cursor:pointer;cursor:url(a.cur)}</style><p id=a>",
        );
        let a = dom.get_by_id("a").unwrap();
        let value = |name: &str| dom.computed_value_resolved(a, name);
        assert_eq!(value("stroke-linecap").as_deref(), Some("round"));
        assert_eq!(value("grid-template-columns").as_deref(), Some("1fr 2fr"));
        assert_eq!(value("transform").as_deref(), Some("rotate(1deg)"));
        assert_eq!(value("display").as_deref(), Some("inline-flex"));
        assert_eq!(value("cursor").as_deref(), Some("pointer"));
    }

    fn ok(property: &str, value: &str) -> bool {
        valid(property, value).unwrap_or_else(|| panic!("{property} has no grammar here"))
    }

    #[test]
    fn grammars_accept_valid_values_and_reject_others() {
        for (property, good, bad) in [
            ("stroke-dasharray", "4 2, 1px 10%", "4 -2"),
            ("stroke-miterlimit", "4", "0.5"),
            ("fill", "url(#g) red", "url(#g) 3px"),
            ("paint-order", "stroke markers", "stroke stroke"),
            (
                "animation",
                "spin 1s infinite linear, fade 2s 1s both",
                "spin 1s 2s 3s",
            ),
            ("animation-duration", "1s, 200ms", "-1s"),
            ("color-scheme", "light dark only", "only only"),
            (
                "font-family",
                "\"Fira Sans\", Helvetica Neue, serif",
                "Helvetica, 12px",
            ),
            ("line-height", "1.5", "-1"),
            ("quotes", "'«' '»' '‹' '›'", "'«'"),
            ("aspect-ratio", "16 / 9 auto", "16 /"),
            ("object-position", "right 10px top 0", "right 10px top"),
            (
                "background-position",
                "right 10% top 5px, center",
                "top top",
            ),
            ("transform-origin", "left top 4px", "top 10% 5%"),
            ("translate", "10px 20% 3px", "10px 20% 30%"),
            ("rotate", "y 45deg", "45deg 45deg"),
            ("scale", "1 2 3", "1 2 3 4"),
            (
                "grid-template-columns",
                "[a] repeat(auto-fill, minmax(10px, 1fr)) [b c] 2fr",
                "repeat(auto-fit, 1px) repeat(auto-fill, 2px)",
            ),
            (
                "grid-template-rows",
                "subgrid [a] repeat(2, [b] [c])",
                "subgrid 1fr",
            ),
            ("grid-area", "1 / span 2 / a / -1", "1 / 2 / 3 / 4 / 5"),
            ("grid-column-start", "span 2 name", "0"),
            ("justify-content", "safe left", "baseline"),
            ("align-content", "last baseline", "left"),
            ("justify-items", "legacy center", "legacy legacy"),
            ("place-self", "start end", "start end center"),
            ("border-image-slice", "10% 20 fill", "fill 1 fill"),
            ("box-shadow", "inset 0 0 4px 2px red, 1px 1px", "1px red"),
            ("text-shadow", "1px 1px 2px red", "1px 1px 2px 3px"),
            (
                "cursor",
                "url(a.cur) 4 4, url(b.svg), pointer",
                "url(a.cur)",
            ),
            ("contain", "size layout paint", "size inline-size"),
            ("clip", "rect(1px, auto, 3px, 4px)", "rect(1px, 2px)"),
            ("list-style-type", "symbols(cyclic '*' '†')", "none none"),
            ("container-name", "card sidebar", "card none"),
            ("transform", "rotate(4deg) translateX(10%)", "rotate(4px)"),
            ("display", "inline flow-root", "inline inline"),
            ("outline", "thin dotted red", "red red"),
        ] {
            assert!(ok(property, good), "{property}: {good}");
            assert!(!ok(property, bad), "{property}: {bad}");
        }
        assert_eq!(normalize_display("inline flex"), Some("inline-flex"));
        assert!(ok("background-image", "light-dark(url(a.png), none)"));
        assert!(!ok("list-style-image", "light-dark(url(a.png), red)"));
        assert_eq!(normalize_display("list-item flow block"), Some("list-item"));
        assert_eq!(normalize_display("inline list-item"), None);
    }
}
