// CSSWG 81c27f686901 (2026-09-06): CSS Text 3 #boundary-shaping,
// CSS Inline 3 #invisible-line-boxes, CSS Sizing 3 #stretch-fit-sizing.

#[test]
fn servo_audit_shaping_crosses_inline_wrappers_without_losing_dom_sources() {
    for (text, split, direction) in [
        (
            "affinity",
            "a<span>f</span><span style='text-decoration:underline'>fi</span>nity",
            "ltr",
        ),
        ("سلام", "س<span>ل</span><span>ا</span>م", "rtl"),
    ] {
        let html = format!(
            "<!doctype html><body style='margin:0;font:32px serif;direction:{direction}'>\
            <div id=plain style='width:max-content'>{text}</div>\
            <div id=split style='width:max-content'>{split}</div></body>"
        );
        let dom = Dom::parse_document(&html);
        let layout = lay_graphical(&html, 800., &HashMap::new());
        let a = layout.boxes[&dom.get_by_id("plain").unwrap()];
        let b = layout.boxes[&dom.get_by_id("split").unwrap()];
        assert_near(a.width, b.width, "inline wrapper must not break shaping");
        let mut lines: Vec<(i32, std::collections::BTreeSet<(u32, i32)>)> = Vec::new();
        let mut sources = std::collections::HashSet::new();
        for primitive in &layout.paint.primitives {
            if let crate::render::Primitive::GlyphRun {
                origin,
                shaped,
                selection,
                ..
            } = primitive
            {
                let line = (origin.y * 10.).round() as i32;
                if lines.last().is_none_or(|(y, _)| *y != line) {
                    lines.push((line, Default::default()));
                }
                lines
                    .last_mut()
                    .unwrap()
                    .1
                    .extend(shaped.runs.iter().flat_map(|run| {
                        run.glyphs
                            .iter()
                            .map(|glyph| (glyph.id, ((origin.x + glyph.x) * 100.).round() as i32))
                    }));
                for span in selection.sources.as_deref().unwrap_or_default().iter() {
                    sources.insert(span.node);
                }
            }
        }
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[0].1, lines[1].1,
            "{text}: wrapper changes retained glyphs"
        );
        assert!(
            sources.len() >= 5,
            "each authored Text node keeps its own source mapping: {sources:?}"
        );
    }
}

#[test]
fn servo_audit_shaping_observes_required_boundaries_and_cursive_style_context() {
    let style = crate::text::TextStyle {
        family: "sans-serif".into(),
        size: 32.,
        ..Default::default()
    };
    let glyphs = |text: &str| {
        let mut ids: Vec<_> = crate::text::shape(text, &style)
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|g| g.id))
            .collect();
        ids.sort_unstable();
        ids
    };
    for (middle, split) in [
        ("", false),
        ("color:red;text-decoration:underline", false),
        ("padding-top:5px;border-bottom:1px solid", false),
        ("padding-left:1px;padding-right:1px", true),
        (
            "margin-left:-1px;padding-left:1px;margin-right:-1px;padding-right:1px",
            true,
        ),
        ("vertical-align:0px", true),
        ("unicode-bidi:isolate", true),
    ] {
        let html = format!(
            "<!doctype html><body style='margin:0;font:32px sans-serif;direction:rtl'><div>ب<span style='{middle}'>ب</span>ب</div></body>"
        );
        let layout = lay_graphical(&html, 500., &HashMap::new());
        let mut visual: Vec<_> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|p| match p {
                crate::render::Primitive::GlyphRun { origin, shaped, .. } => Some((origin, shaped)),
                _ => None,
            })
            .flat_map(|(origin, s)| {
                s.runs.iter().flat_map(move |r| {
                    r.glyphs
                        .iter()
                        .map(move |g| (g.id, ((origin.x + g.x) * 64.).round() as i32))
                })
            })
            .collect();
        visual.sort_unstable();
        visual.dedup(); // A full glyph can be retained in adjacent clipped pieces.
        let mut ids: Vec<_> = visual.into_iter().map(|(id, _)| id).collect();
        ids.sort_unstable();
        let expected = if split {
            glyphs("ب").repeat(3)
        } else {
            glyphs("ببب")
        };
        let mut expected = expected;
        expected.sort_unstable();
        assert_eq!(ids, expected, "{middle}: required boundary={split}");
    }
    // A weight change prevents a shared ligature, but must preserve Arabic
    // initial/medial/final forms through transparent marks and style groups.
    for (weight, split) in [
        (500., "ب<span style='font-weight:500'>ب</span>ب"),
        (
            300.,
            "ب<span style='font-weight:500'>\u{64e}</span><span style='font-weight:300'>ب</span>ب",
        ),
    ] {
        let html = format!(
            "<!doctype html><body style='margin:0;font:32px sans-serif;direction:rtl'><div>{split}</div></body>"
        );
        let layout = lay_graphical(&html, 500., &HashMap::new());
        if split.contains('\u{64e}') {
            assert!(layout.paint.primitives.iter().any(|primitive| matches!(primitive,
                crate::render::Primitive::GlyphRun{shaped,..} if shaped.text=="\u{64e}" && shaped.runs.iter().any(|run|!run.glyphs.is_empty()))),"synthetic joining context must not own and discard an authored mark");
        }
        let mut actual: Vec<_> = layout
            .paint
            .primitives
            .iter()
            .filter_map(|p| match p {
                crate::render::Primitive::GlyphRun { shaped, .. }
                    if !shaped.text.chars().all(|c| c == '\u{64e}') =>
                {
                    Some(shaped)
                }
                _ => None,
            })
            .flat_map(|s| s.runs.iter())
            .flat_map(|r| r.glyphs.iter().map(|g| g.id))
            .filter(|id| *id != 3)
            .collect();
        actual.sort_unstable();
        let middle = crate::text::shape(
            "\u{200d}ب\u{200d}",
            &crate::text::TextStyle {
                weight,
                ..style.clone()
            },
        );
        let mut expected: Vec<_> = glyphs("ب\u{200d}")
            .into_iter()
            .chain(glyphs("\u{200d}ب"))
            .chain(
                middle
                    .runs
                    .iter()
                    .flat_map(|r| r.glyphs.iter().map(|g| g.id)),
            )
            .filter(|id| *id != 3)
            .collect();
        expected.sort_unstable();
        assert_eq!(
            actual, expected,
            "style boundary must retain cursive forms: {split}"
        );
    }
}

#[test]
fn servo_audit_shaping_paints_partial_ligatures_in_each_source_color() {
    let html = "<!doctype html><body style='margin:0;font:48px DejaVu Sans'><div id=x style='width:max-content'>f<span style='color:red'>f</span><span style='color:blue'>i</span></div></body>";
    let dom = Dom::parse_document(html);
    let layout = lay_graphical(html, 500., &HashMap::new());
    let width = layout.boxes[&dom.get_by_id("x").unwrap()].width as f32;
    let runs: Vec<_> = layout
        .paint
        .primitives
        .iter()
        .filter_map(|p| match p {
            crate::render::Primitive::GlyphRun {
                origin,
                shaped,
                color,
                clip,
                selection,
                ..
            } => Some((origin, shaped, color, clip, selection)),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 3);
    let expected = crate::text::shape(
        "ffi",
        &crate::text::TextStyle {
            family: "DejaVu Sans".into(),
            size: 48.,
            ..Default::default()
        },
    );
    assert_near(
        f64::from(width),
        f64::from(expected.advance),
        "ligature width",
    );
    let expected_ids: Vec<_> = expected
        .runs
        .iter()
        .flat_map(|r| r.glyphs.iter().map(|g| g.id))
        .collect();
    assert!(
        expected_ids.len() < 3,
        "fixture font must have an ffi ligature"
    );
    let mut edges = Vec::new();
    for (origin, shaped, _, clip, selection) in &runs {
        assert_eq!(
            shaped
                .runs
                .iter()
                .flat_map(|r| r.glyphs.iter().map(|g| g.id))
                .collect::<Vec<_>>(),
            expected_ids
        );
        let clip = clip.expect("shared glyph pieces retain their own caret clipping");
        edges.push((clip.x.max(0.), (clip.x + clip.width).min(width)));
        assert_eq!(selection.sources.as_deref().unwrap_or_default().len(), 1);
        for (a, b) in shaped
            .runs
            .iter()
            .flat_map(|r| &r.glyphs)
            .zip(expected.runs.iter().flat_map(|r| &r.glyphs))
        {
            assert_near(
                f64::from(origin.x + a.x),
                f64::from(b.x),
                "retained full-glyph origin",
            );
        }
    }
    assert_near(f64::from(edges[0].0), 0., "first clip");
    assert_near(
        f64::from(edges[0].1),
        f64::from(edges[1].0),
        "first caret color edge",
    );
    assert_near(
        f64::from(edges[1].1),
        f64::from(edges[2].0),
        "second caret color edge",
    );
    assert_near(f64::from(edges[2].1), f64::from(width), "last clip");
    assert_ne!(runs[0].2, runs[1].2);
    assert_ne!(runs[1].2, runs[2].2);
}

#[test]
fn servo_audit_shaping_inline_slices_keep_line_metrics_and_wrapped_joining() {
    for (text, direction, width) in [
        ("affinity affinity affinity", "ltr", 85.),
        ("نوشتننوشتننوشتن", "rtl", 45.),
        ("abc سلام xyz", "ltr", 120.),
        ("a\u{301}b\u{301}c\u{301}d\u{301}", "ltr", 45.),
    ] {
        let split: String = text.chars().map(|c| format!("<span>{c}</span>")).collect();
        let html = format!(
            "<!doctype html><body style='margin:0;font:32px/40px sans-serif;direction:{direction}'><div id=plain style='width:{width}px;overflow-wrap:anywhere'>{text}</div><div id=split style='width:{width}px;overflow-wrap:anywhere'>{split}</div></body>"
        );
        let dom = Dom::parse_document(&html);
        let layout = lay_graphical(&html, 500., &HashMap::new());
        let plain = layout.boxes[&dom.get_by_id("plain").unwrap()];
        let divided = layout.boxes[&dom.get_by_id("split").unwrap()];
        assert_near(
            plain.height,
            divided.height,
            &format!("same line breaks and half-leading: {text}"),
        );
        let mut lines =
            std::collections::BTreeMap::<i32, std::collections::BTreeSet<(u32, i32)>>::new();
        for p in &layout.paint.primitives {
            if let crate::render::Primitive::GlyphRun { origin, shaped, .. } = p {
                let y = if f64::from(origin.y) >= divided.top {
                    f64::from(origin.y) - divided.top
                } else {
                    f64::from(origin.y) - plain.top
                };
                let key = (y * 100.).round() as i32;
                let half = usize::from(f64::from(origin.y) >= divided.top);
                lines.entry(key * 2 + half as i32).or_default().extend(
                    shaped
                        .runs
                        .iter()
                        .flat_map(|r| r.glyphs.iter())
                        .map(|g| (g.id, ((origin.x + g.x) * 100.).round() as i32)),
                );
            }
        }
        let keys: Vec<_> = lines.keys().copied().filter(|key| key % 2 == 0).collect();
        for key in keys {
            assert_eq!(
                lines.get(&key),
                lines.get(&(key + 1)),
                "same retained joining and glyph positions: {text}, line={key}"
            );
        }
    }
}

#[test]
fn servo_audit_shaping_visible_lines_do_not_reshape_or_lay_the_hidden_tail() {
    // HTML #the-textarea-element-2 and CSS Text 3 #word-break-shaping:
    // retain the entire paragraph's context, but a control's initial paint
    // needs only the lines reaching its scrollport. Count prepared source
    // bytes instead of depending on wall-clock speed or a particular font.
    let text = "word ".repeat(4_000);
    let html = format!(
        "<!doctype html><body style=margin:0><textarea id=t cols=20 rows=3 \
        style='font:12px/15px monospace;padding:4px;border:1px solid;margin:0'>{text}</textarea>"
    );
    let before = crate::text::shaping_work_bytes();
    let layout = lay_graphical(&html, 800., &HashMap::new());
    let work = crate::text::shaping_work_bytes() - before;
    assert!(
        work < text.len() * 8,
        "only a bounded number of whole-context passes, not one per line: {work} prepared bytes for {} source bytes",
        text.len(),
    );
    let lines: Vec<_> = layout
        .paint
        .primitives
        .iter()
        .filter_map(|primitive| match primitive {
            crate::render::Primitive::GlyphRun { shaped, .. } => Some(shaped.text.trim_end()),
            _ => None,
        })
        .collect();
    assert_eq!(lines, ["word word word word"; 5]);
}

#[test]
fn servo_audit_empty_inline_edges_create_lines_only_for_nonzero_inline_space() {
    for (style, expected) in [
        ("", 0.),
        ("padding-top:10px;border-top:2px solid", 0.),
        ("padding-left:10px", 40.),
        ("margin-right:-10px", 40.),
        ("border-left:1px solid", 40.),
    ] {
        let html = format!(
            "<!doctype html><body style='margin:0;font:20px/40px serif'>\
            <div id=p><span style='{style}'></span></div><div id=after style='height:5px'></div></body>"
        );
        let dom = Dom::parse_document(&html);
        let layout = lay_graphical(&html, 400., &HashMap::new());
        let p = layout.boxes[&dom.get_by_id("p").unwrap()];
        let after = layout.boxes[&dom.get_by_id("after").unwrap()];
        assert_near(p.height, expected, style);
        assert_near(after.top, expected, style);
    }
}

#[test]
fn servo_audit_stretch_height_uses_containing_block_through_inline_wrappers() {
    for (parent, expected) in [
        ("", 100.),
        ("outline:solid", 100.),
        ("border:3px solid", 50.),
        ("padding-top:1px", 75.),
        ("display:flow-root", 50.),
    ] {
        for contents in [
            "<div id=x class=test></div>",
            "<span><div id=x class=test></div></span>",
            "<div></div><span><div id=x class=test></div></span>",
            "<span style='border:3px solid'><div id=x class=test></div></span>",
        ] {
            let html = format!(
                "<!doctype html><style>.test {{height:stretch;margin:25px}}</style>\
                <body style='margin:0'><div style='width:100px;height:100px;margin:25px;{parent}'>{contents}</div></body>"
            );
            let dom = Dom::parse_document(&html);
            let layout = lay_graphical(&html, 400., &HashMap::new());
            let x = layout.boxes[&dom.get_by_id("x").unwrap()];
            assert_near(x.height, expected, &format!("{parent}: {contents}"));
        }
    }
}

#[test]
fn servo_audit_font_matching_ties_follow_live_stylesheet_order() {
    // CSS Fonts 4 #font-style-matching: overlapping identical descriptors
    // form a composite face in reverse @font-face rule order, including
    // after style elements move or are removed (Servo #47449).
    let mut dom = Dom::parse_document(
        "<!doctype html><head>\
        <style id=a>@font-face{font-family:Tie;src:url(a.ttf);font-display:swap}</style>\
        <style id=b>@font-face{font-family:Tie;src:url(b.ttf);font-display:swap}</style>\
        </head><body><p id=p style='font-family:Tie'>WWii</p></body>",
    );
    dom.set_doc_url(Some(Url::parse("https://fonts.test/").unwrap()));
    let p = dom.get_by_id("p").unwrap();
    let shape = |dom: &Dom| {
        crate::text::shape(
            "WWii",
            &crate::text::TextStyle {
                family: "Tie".into(),
                font_set: dom.scope_font_set(p),
                ..Default::default()
            },
        )
    };
    shape(&dom);
    let sans = include_bytes!("../../assets/fonts/dejavu/DejaVuSans.ttf");
    let serif = include_bytes!("../../assets/fonts/dejavu/DejaVuSerif.ttf");
    dom.install_downloaded_font("https://fonts.test/a.ttf", Some(sans.to_vec()));
    dom.install_downloaded_font("https://fonts.test/b.ttf", Some(serif.to_vec()));
    let expected = |data: &[u8]| {
        crate::text::shape(
            "WWii",
            &crate::text::TextStyle {
                family: "Tie".into(),
                font_set: Some(crate::font_system::FontSet::new(
                    vec![
                        crate::font_system::PageFont::from_web_resource(
                            "Tie".into(),
                            data.to_vec(),
                        )
                        .unwrap(),
                    ],
                    None,
                    false,
                )),
                ..Default::default()
            },
        )
        .advance
    };
    let a = dom.get_by_id("a").unwrap();
    let b = dom.get_by_id("b").unwrap();
    let head = dom.node(a).parent.unwrap();
    assert!((expected(sans) - expected(serif)).abs() > 0.1);
    assert_near(
        f64::from(shape(&dom).advance),
        f64::from(expected(serif)),
        "later font rule",
    );
    dom.detach(a);
    dom.append(head, a);
    assert_near(
        f64::from(shape(&dom).advance),
        f64::from(expected(sans)),
        "reordered font rule",
    );
    dom.detach(a);
    assert_near(
        f64::from(shape(&dom).advance),
        f64::from(expected(serif)),
        "removed font rule",
    );
    dom.detach(b);
}

#[test]
fn servo_audit_stretch_keyword_sizes_margin_boxes_across_formatting_contexts() {
    for (display, child) in [
        ("flow-root", "display:block"),
        ("flow-root", "display:inline-block"),
        ("flex;align-items:start", "flex-shrink:0"),
        (
            "grid;grid-template-columns:240px;grid-template-rows:160px;align-items:start;justify-items:start",
            "",
        ),
    ] {
        for sizing in ["content-box", "border-box"] {
            let html = format!(
                "<!doctype html><body style='margin:0'><div style='display:{display};width:240px;height:160px'>\
                <div id=x style='{child};width:stretch;height:stretch;margin:10px;border:2px solid;padding:4px;box-sizing:{sizing}'></div></div></body>"
            );
            let dom = Dom::parse_document(&html);
            let layout = lay_graphical(&html, 500., &HashMap::new());
            let x = layout.boxes[&dom.get_by_id("x").unwrap()];
            assert_near(x.width, 220., &format!("{display}: {sizing}: width"));
            assert_near(x.height, 140., &format!("{display}: {sizing}: height"));
        }
    }
    let html = "<!doctype html><body style='margin:0;font:20px/20px serif'>\
        <div id=auto style='width:100px;height:stretch;min-height:stretch;max-height:stretch'>text</div>\
        <div style='position:relative;width:240px;height:160px'>\
        <div id=abs style='position:absolute;inset:5px 15px 15px 5px;width:stretch;height:stretch;margin:10px;border:2px solid;padding:4px'></div>\
        </div></body>";
    let dom = Dom::parse_document(html);
    let layout = lay_graphical(html, 500., &HashMap::new());
    let a = layout.boxes[&dom.get_by_id("auto").unwrap()];
    let b = layout.boxes[&dom.get_by_id("abs").unwrap()];
    assert_near(
        a.height,
        20.,
        "stretch is auto against indefinite block size",
    );
    assert_near(
        b.width,
        200.,
        "positioned stretch uses inset-modified available width",
    );
    assert_near(
        b.height,
        120.,
        "positioned stretch uses inset-modified available height",
    );
}

#[test]
fn servo_audit_stretch_height_transfers_through_aspect_ratio() {
    // CSS Sizing 4 #aspect-ratio-automatic: a definite stretch-fit height
    // supplies the automatic ratio-dependent width, including transferred
    // min/max-height constraints and the selected sizing box.
    for child in ["display:block", "display:inline-block", "float:left"] {
        for height in [
            "height:stretch",
            "height:40px;min-height:stretch",
            "height:200px;max-height:stretch",
        ] {
            for (sizing, expected) in [("content-box", 148.), ("border-box", 160.)] {
                let html = format!(
                    "<!doctype html><body style='margin:0'><div style='display:flow-root;width:400px;height:100px'><div id=x style='{child};{height};aspect-ratio:2;box-sizing:{sizing};margin:10px;border:2px solid;padding:4px'></div></div></body>"
                );
                let dom = Dom::parse_document(&html);
                let layout = lay_graphical(&html, 500., &HashMap::new());
                let x = layout.boxes[&dom.get_by_id("x").unwrap()];
                assert_near(x.height, 80., &format!("{child};{height};{sizing}: height"));
                assert_near(
                    x.width,
                    expected,
                    &format!("{child};{height};{sizing}: ratio width"),
                );
            }
        }
    }
}

#[test]
fn servo_audit_stretch_inline_size_uses_float_band_with_real_percentage_basis() {
    for (sizing, left, width, top) in [
        ("width:stretch;margin:0 10px", 50., 160., 0.),
        ("width:stretch;max-width:100px;margin:0 auto", 74., 112., 0.),
        ("width:20px;min-width:stretch;margin:0 10px", 50., 160., 0.),
        (
            "width:stretch;min-width:200px;margin:0 10px",
            10.,
            220.,
            100.,
        ),
        ("width:stretch;margin:0 10%", 64., 132., 0.),
    ] {
        let html = format!(
            "<!doctype html><body style='margin:0'><div style='width:240px'><div style='float:left;width:40px;height:100px'></div><div style='float:right;width:20px;height:100px'></div><div id=x style='display:flow-root;{sizing};height:20px;padding:4px;border:2px solid'></div></div></body>"
        );
        let dom = Dom::parse_document(&html);
        let layout = lay_graphical(&html, 500., &HashMap::new());
        let x = layout.boxes[&dom.get_by_id("x").unwrap()];
        assert_near(x.left, left, sizing);
        assert_near(x.width, width, sizing);
        assert_near(x.top, top, sizing);
    }
}
