//! Regression coverage for native editor feedback inside composed page scopes.

use super::*;
use crate::core::{PhysicalSize, ScaleFactor};
use crate::layout2::{PendingEditorText, RichEditorPresentation};

fn fixture() -> (Scene, RichEditorPresentation, EditorVisual) {
    let viewport =
        ViewportMetrics::from_physical(PhysicalSize::new(200, 140), ScaleFactor::default());
    let style = crate::text::TextStyle::default();
    let text = crate::text::shape("old", &style);
    let region = CssRect::new(10.0, 16.0, 100.0, 25.0);
    let commands = vec![
        Primitive::FillRect {
            rect: CssRect::new(0.0, 0.0, 200.0, 140.0),
            color: PaintColor::Rgba(255, 255, 255, 255),
        },
        Primitive::PushClip(PaintShape::Rect(CssRect::new(0.0, 0.0, 180.0, 120.0))),
        Primitive::PushTransform(Affine2d::translate(12.0, 9.0)),
        Primitive::PushTransform(Affine2d::scale(1.5, 1.5)),
        Primitive::HitRegion(HitRegion {
            rect: region,
            node: 11,
            actor: Some(11),
            link: None,
            cursor: None,
        }),
        Primitive::PushClip(PaintShape::Rect(CssRect::new(5.0, 5.0, 60.0, 25.0))),
        Primitive::PushTransform(Affine2d::translate(-5.0, -4.0)),
        Primitive::GlyphRun {
            origin: CssPoint::new(10.0, 16.0),
            shaped: text,
            color: PaintColor::Rgba(25, 30, 35, 255),
            decoration: TextDecorationPaint {
                color: PaintColor::Rgba(25, 30, 35, 255),
                style: DecorationStyle::Solid,
                thickness: None,
                underline_offset: None,
            },
            shadows: Vec::new(),
            clip: None,
            node: 12,
            link: None,
            selection: RunSelection {
                sources: Some(Arc::from([TextSourceSpan {
                    start: 0,
                    end: 3,
                    node: 13,
                    offset: 0,
                    offset_end: 3,
                }])),
                ..Default::default()
            },
        },
        Primitive::PopTransform,
        Primitive::PopClip,
        Primitive::PopTransform,
        Primitive::PopTransform,
        Primitive::PopClip,
    ];
    let scene = Scene {
        viewport,
        primitives: commands,
        controls: Vec::new(),
        content_viewport: CssRect::new(0.0, 0.0, 200.0, 140.0),
        image_store: ImageStore::default(),
        canvas_images: Default::default(),
        page_scroll_containers: Vec::new(),
        page_size: CssSize::default(),
    };
    let presentation = RichEditorPresentation {
        style,
        caret_color: PaintColor::Rgba(255, 0, 0, 255),
        origin: CssPoint::default(),
        width: 100.0,
        pending_text: Some(PendingEditorText {
            text: "old".into(),
            nodes: vec![11, 12, 13],
            text_nodes: vec![13],
            line_right: Some(110.0),
            preserves_edge_spaces: false,
            color: PaintColor::Rgba(25, 30, 35, 255),
        }),
        native_input: None,
    };
    let editor = EditorVisual {
        text: "oldz".into(),
        selection: Vec::new(),
        caret: Some(CssRect::new(15.0, 0.0, 1.0, 16.0)),
    };
    (scene, presentation, editor)
}

#[test]
fn scoped_editor_retains_transform_scroll_clip_and_paint_order() {
    let (mut actual, presentation, editor) = fixture();
    let mut expected = actual.clone();
    let index = expected
        .primitives
        .iter()
        .position(|item| matches!(item, Primitive::GlyphRun { node: 12, .. }))
        .unwrap();
    let Primitive::GlyphRun {
        shaped, selection, ..
    } = &mut expected.primitives[index]
    else {
        unreachable!()
    };
    *shaped = crate::text::shape("oldz", &presentation.style);
    selection.paint_timing_sources = selection.sources.take();
    expected.primitives.insert(
        index + 1,
        Primitive::FillRect {
            rect: CssRect::new(25.0, 16.0, 1.0, 16.0),
            color: presentation.caret_color,
        },
    );

    assert!(can_paint_scoped_editor(
        &actual.primitives,
        11,
        &presentation,
        "old",
        "oldz"
    ));
    let result = paint_scoped_editor(&mut actual.primitives, 11, &editor, &presentation, true)
        .expect("ordinary plain editor has one provable scope");
    assert_eq!(result.caret, Some(CssRect::new(42.0, 27.0, 1.5, 24.0)));
    assert_eq!(actual.primitives, expected.primitives);

    let mut renderer = vello_cpu::VelloCpuRenderer::new();
    assert_eq!(
        renderer.render_rgba(&actual).unwrap().pixels,
        renderer.render_rgba(&expected).unwrap().pixels,
    );
    if let Ok(mut renderer) =
        futures::executor::block_on(vello_hybrid::VelloHybridRenderer::new_headless())
    {
        assert_eq!(
            renderer.render_rgba(&actual).unwrap().pixels,
            renderer.render_rgba(&expected).unwrap().pixels,
        );
    }
}

#[test]
fn scoped_editor_declines_ambiguous_or_changing_layout_without_mutating_scene() {
    let (mut scene, mut presentation, mut editor) = fixture();
    let original = scene.primitives.clone();

    presentation.pending_text.as_mut().unwrap().text.clear();
    assert!(!can_paint_scoped_editor(
        &scene.primitives,
        11,
        &presentation,
        "",
        "a"
    ));
    assert!(paint_scoped_editor(&mut scene.primitives, 11, &editor, &presentation, true).is_none());
    assert_eq!(
        scene.primitives, original,
        "an empty editor may have authored pseudo-content"
    );

    presentation.pending_text.as_mut().unwrap().text = "old".into();
    editor.text = "old ".into();
    assert!(!can_paint_scoped_editor(
        &scene.primitives,
        11,
        &presentation,
        "old",
        &editor.text,
    ));
    assert!(paint_scoped_editor(&mut scene.primitives, 11, &editor, &presentation, true).is_none());
    presentation
        .pending_text
        .as_mut()
        .unwrap()
        .preserves_edge_spaces = true;
    assert!(can_paint_scoped_editor(
        &scene.primitives,
        11,
        &presentation,
        "old",
        &editor.text,
    ));
    presentation
        .pending_text
        .as_mut()
        .unwrap()
        .preserves_edge_spaces = false;
    editor.text = "old🙂".into();
    assert!(paint_scoped_editor(&mut scene.primitives, 11, &editor, &presentation, true).is_none());
    editor.text = "oldz".repeat(80);
    assert!(!can_paint_scoped_editor(
        &scene.primitives,
        11,
        &presentation,
        "old",
        &editor.text,
    ));
    assert!(paint_scoped_editor(&mut scene.primitives, 11, &editor, &presentation, true).is_none());
    assert_eq!(
        scene.primitives, original,
        "wrapping, whitespace and bidi stay canonical"
    );

    // A paragraph can be much narrower than its host, including beside a
    // float. The prospective run must fit the actual laid line band.
    let old_width = crate::text::shape("old", &presentation.style).advance;
    presentation.pending_text.as_mut().unwrap().line_right = Some(10.0 + old_width + 3.0);
    assert!(!can_paint_scoped_editor(
        &scene.primitives,
        11,
        &presentation,
        "old",
        "oldz"
    ));
    presentation.pending_text.as_mut().unwrap().line_right = Some(110.0);

    let index = scene
        .primitives
        .iter()
        .position(|item| matches!(item, Primitive::GlyphRun { node: 12, .. }))
        .unwrap();
    scene
        .primitives
        .insert(index + 1, scene.primitives[index].clone());
    let ambiguous = scene.primitives.clone();
    assert!(!can_paint_scoped_editor(
        &scene.primitives,
        11,
        &presentation,
        "old",
        "oldz"
    ));
    editor.text = "oldz".into();
    assert!(paint_scoped_editor(&mut scene.primitives, 11, &editor, &presentation, true).is_none());
    assert_eq!(
        scene.primitives, ambiguous,
        "multiple runs may have independent scopes"
    );

    // Matching glyph bytes are insufficient to identify editable text:
    // authored pseudo-content can coincidentally spell the same string.
    scene.primitives = original;
    let Primitive::GlyphRun { selection, .. } = &mut scene.primitives[index] else {
        unreachable!()
    };
    *selection = RunSelection::default();
    let generated = scene.primitives.clone();
    assert!(!can_paint_scoped_editor(
        &scene.primitives,
        11,
        &presentation,
        "old",
        "oldz"
    ));
    assert!(paint_scoped_editor(&mut scene.primitives, 11, &editor, &presentation, true).is_none());
    assert_eq!(scene.primitives, generated);
}

#[test]
fn scoped_editor_preview_preserves_user_select_none() {
    let (mut scene, presentation, editor) = fixture();
    let index = scene
        .primitives
        .iter()
        .position(|item| matches!(item, Primitive::GlyphRun { node: 12, .. }))
        .unwrap();
    let Primitive::GlyphRun { selection, .. } = &mut scene.primitives[index] else {
        unreachable!()
    };
    selection.paint_timing_sources = selection.sources.take();
    selection.unselectable = true;
    paint_scoped_editor(&mut scene.primitives, 11, &editor, &presentation, true).unwrap();
    assert!(matches!(
        &scene.primitives[index],
        Primitive::GlyphRun { selection, .. }
            if selection.unselectable && selection.sources.is_none()
    ));
}

#[test]
fn scoped_editor_declines_different_native_shaping_geometry() {
    let (mut scene, mut presentation, editor) = fixture();
    presentation.style.typography = Some(Arc::new(crate::text::FontTypography {
        caps: crate::text::typography::Caps::SmallCaps,
        ..Default::default()
    }));
    let Primitive::GlyphRun { shaped, .. } = scene
        .primitives
        .iter_mut()
        .find(|command| matches!(command, Primitive::GlyphRun { .. }))
        .unwrap()
    else {
        unreachable!()
    };
    *shaped = crate::text::shape("old", &presentation.style);
    let original = scene.primitives.clone();
    assert!(!can_paint_scoped_editor(
        &scene.primitives,
        11,
        &presentation,
        "old",
        "oldz"
    ));
    assert!(paint_scoped_editor(&mut scene.primitives, 11, &editor, &presentation, true).is_none());
    assert_eq!(scene.primitives, original);
}

#[test]
fn scoped_editor_finds_real_dom_text_inside_a_scrolling_transformed_host() {
    let dom = crate::dom::Dom::parse_document(
        "<style>body{margin:0}#wrap{transform:scale(1.2);transform-origin:0 0}\
         #scroll{width:160px;height:60px;overflow:auto}\
         #pad{height:30px}#editor{width:100px}\
         #editor p{margin:0;white-space:pre-wrap;color:blue;\
         -webkit-text-fill-color:red}</style>\
         <div id=wrap><div id=scroll><div id=pad></div>\
         <div id=editor contenteditable><p>old</p></div></div></div>",
    );
    let base = url::Url::parse("https://example.test/").unwrap();
    let rendered = crate::http::render_arena(
        &dom,
        &base,
        crate::layout2::Viewport::new(200.0, 140.0),
        1.0,
        None,
        &Default::default(),
    );
    let node = dom.get_by_id("editor").unwrap();
    let scroll = dom.get_by_id("scroll").unwrap();
    let presentation = &rendered.rich_editors[&node];
    let (mut scene, _, _) = fixture();
    scene.primitives.clear();
    scene.append_page_with_scroll_offsets(
        &rendered.layout.paint,
        CssPoint::default(),
        0.0,
        &HashMap::from([(scroll, CssPoint::new(0.0, 10.0))]),
    );
    let original_color = scene
        .primitives
        .iter()
        .find_map(|command| match command {
            Primitive::GlyphRun { shaped, color, .. } if shaped.text == "old" => Some(*color),
            _ => None,
        })
        .expect("the page paints the editor text");
    assert_ne!(
        original_color,
        presentation.pending_text.as_ref().unwrap().color
    );
    assert!(
        can_paint_scoped_editor(&scene.primitives, node, presentation, "old", "oldz"),
        "a real Text-source run in a scrolling transform should be previewable"
    );
    let editor = EditorVisual {
        text: "oldz".into(),
        selection: Vec::new(),
        caret: Some(CssRect::new(20.0, 0.0, 1.0, 16.0)),
    };
    let result = paint_scoped_editor(&mut scene.primitives, node, &editor, presentation, true)
        .expect("scoped painting must retain the text's CSS ancestry");
    assert!(result.caret.is_some());
    assert!(scene.primitives.iter().any(|command| matches!(
        command, Primitive::GlyphRun { shaped, color, .. }
            if shaped.text == "oldz" && *color == original_color
    )));
}

#[test]
fn scoped_editor_declines_glyph_dependent_paint() {
    // These effects may paint outside the GlyphRun. Extending just that run
    // would leave stale outlines, masks or first-letter styling behind.
    for rule in [
        "#editor p::first-letter{color:red}",
        "#wrap::first-letter{color:red}",
        "#editor p{-webkit-text-stroke:2px red}",
        "#wrap{-webkit-text-stroke:2px red}",
        "#editor{background-clip:text}",
        "#wrap{background-clip:border-box,text}",
    ] {
        let html = format!(
            "<style>body{{margin:0}}#editor p{{margin:0}}{rule}</style>\
             <div id=wrap><div id=editor contenteditable><p>o</p></div></div>"
        );
        let dom = crate::dom::Dom::parse_document(&html);
        let base = url::Url::parse("https://example.test/").unwrap();
        let rendered = crate::http::render_arena(
            &dom,
            &base,
            crate::layout2::Viewport::new(200.0, 140.0),
            1.0,
            None,
            &Default::default(),
        );
        let node = dom.get_by_id("editor").unwrap();
        assert!(
            rendered.rich_editors[&node].pending_text.is_none(),
            "the editor with {rule} must use the actor's complete paint"
        );
    }
}
