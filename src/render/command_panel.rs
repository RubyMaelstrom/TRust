//! Desktop COMMAND instrumentation. Geometry belongs to browser chrome and is
//! shared by paint, editor hit testing, and native accessibility, never layout.

use super::*;

const SURFACE: PaintColor = PaintColor::Rgba(12, 9, 28, 255);
const INSET: PaintColor = PaintColor::Rgba(5, 8, 19, 255);
const EDGE: PaintColor = PaintColor::Rgba(44, 67, 80, 255);
const MUTED: PaintColor = PaintColor::Rgba(147, 156, 179, 255);
const TEXT: PaintColor = theme_color(crate::theme::TEXT);

/// Facts from the displayed response. A pending address must not be presented
/// as a completed connection, and internal/protocol pages need no HTTP badge.
#[derive(Clone, Debug)]
pub struct CommandResponse {
    pub status: u16,
    pub content_type: String,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct CommandPanelGeometry {
    pub panel: CssRect,
    pub input: CssRect,
    /// Exact text origin and clip, including the space reserved for `trust>`.
    pub editor: CssRect,
}

impl CommandPanelGeometry {
    pub fn new(viewport: CssSize) -> Self {
        let margin = 12.0_f32
            .min(viewport.width / 8.0)
            .min(viewport.height / 8.0);
        let width = (viewport.width - 2.0 * margin).max(0.0);
        let height = 204.0_f32.min((viewport.height - 2.0 * margin).max(0.0));
        let panel = CssRect::new(margin, viewport.height - margin - height, width, height);
        let input = CssRect::new(
            panel.x + 16.0,
            panel.y + 40.0,
            (width - 32.0).max(0.0),
            42.0,
        );
        let line_height = crate::text::shape("M", &command_text_style()).line_height;
        let editor = CssRect::new(
            input.x + 78.0,
            input.y + (input.height - line_height) / 2.0,
            (input.width - 94.0).max(1.0),
            line_height,
        );
        Self {
            panel,
            input,
            editor,
        }
    }
}

pub(super) fn paint(
    scene: &mut Scene,
    browser: &BrowserSnapshot,
    editor: &EditorVisual,
    model: &ChromeModel,
) {
    let geometry = CommandPanelGeometry::new(scene.viewport.css);
    let panel = geometry.panel;
    let right = panel.x + panel.width;
    let bottom = panel.y + panel.height;
    let primitives = &mut scene.primitives;

    // A static, bounded shadow and etched edge: opening the console creates no
    // animation timer or continuous redraw work on an otherwise idle page.
    for (spread, alpha) in [(8.0, 16), (4.0, 28), (2.0, 48)] {
        fill(
            primitives,
            CssRect::new(
                panel.x - spread,
                panel.y - spread,
                panel.width + spread * 2.0,
                panel.height + spread * 2.0,
            ),
            PaintColor::Rgba(0, 0, 0, alpha),
        );
    }
    let outline = PaintShape::Path(vec![
        PathElement::MoveTo(CssPoint::new(panel.x + 10.0, panel.y)),
        PathElement::LineTo(CssPoint::new(right - 22.0, panel.y)),
        PathElement::LineTo(CssPoint::new(right, panel.y + 22.0)),
        PathElement::LineTo(CssPoint::new(right, bottom - 10.0)),
        PathElement::LineTo(CssPoint::new(right - 10.0, bottom)),
        PathElement::LineTo(CssPoint::new(panel.x, bottom)),
        PathElement::LineTo(CssPoint::new(panel.x, panel.y + 10.0)),
        PathElement::Close,
    ]);
    primitives.push(Primitive::Fill {
        shape: outline.clone(),
        brush: PaintBrush::Solid(SURFACE),
    });
    primitives.push(Primitive::Stroke {
        shape: outline.clone(),
        brush: PaintBrush::Solid(EDGE),
        style: StrokeStyle::solid(1.0),
    });
    primitives.push(Primitive::PushClip(outline));
    fill(
        primitives,
        CssRect::new(panel.x + 10.0, panel.y, 138.0, 2.0),
        UI_PINK,
    );
    fill(
        primitives,
        CssRect::new(right - 132.0, bottom - 1.0, 108.0, 1.0),
        UI_CYAN,
    );

    // Decorative traces follow the frame rather than competing with text.
    let mut traces = Vec::new();
    for i in 0..4 {
        let x = right - 62.0 + i as f32 * 9.0;
        traces.extend([
            PathElement::MoveTo(CssPoint::new(x, panel.y + 5.0)),
            PathElement::LineTo(CssPoint::new(x + 10.0, panel.y + 15.0)),
        ]);
    }
    primitives.push(Primitive::Stroke {
        shape: PaintShape::Path(traces),
        brush: PaintBrush::Solid(EDGE),
        style: StrokeStyle::solid(2.0),
    });

    let badge = CssRect::new(panel.x + 16.0, panel.y + 10.0, 106.0, 22.0);
    fill(primitives, badge, UI_PINK);
    label(primitives, "COMMAND", badge, 13.0, INSET, true);
    let address = url::Url::parse(&browser.address).ok();
    let endpoint = address.as_ref().map_or_else(
        || String::from("TRust"),
        |url| match url.host_str() {
            Some(host) => format!(
                "{}  /  {}{}",
                url.scheme().to_ascii_uppercase(),
                host,
                url.port()
                    .map_or_else(String::new, |port| format!(":{port}"))
            ),
            None => url.scheme().to_ascii_uppercase(),
        },
    );
    label(
        primitives,
        &endpoint,
        CssRect::new(
            panel.x + 138.0,
            badge.y,
            (panel.width - 212.0).max(0.0),
            badge.height,
        ),
        11.0,
        MUTED,
        false,
    );

    fill(primitives, geometry.input, INSET);
    primitives.push(Primitive::Stroke {
        shape: PaintShape::Rect(geometry.input),
        brush: PaintBrush::Solid(EDGE),
        style: StrokeStyle::solid(1.0),
    });
    fill(
        primitives,
        CssRect::new(
            geometry.input.x,
            geometry.input.y,
            2.0,
            geometry.input.height,
        ),
        UI_CYAN,
    );
    label(
        primitives,
        "trust>",
        CssRect::new(
            geometry.input.x + 12.0,
            geometry.input.y,
            58.0,
            geometry.input.height,
        ),
        crate::theme::TERMINAL_FONT_SIZE_CSS_PX,
        UI_CYAN,
        false,
    );
    paint_editor(primitives, editor, geometry.editor);

    let status_y = panel.y + 89.0;
    fill(
        primitives,
        CssRect::new(panel.x + 18.0, status_y + 7.0, 5.0, 5.0),
        if browser.loading { UI_AMBER } else { UI_CYAN },
    );
    let state = if model.status_label.is_empty() {
        "TRUST"
    } else {
        &model.status_label
    };
    let state_width = crate::text::shape(state, &command_text_style_with_size(11.0)).advance + 12.0;
    label(
        primitives,
        state,
        CssRect::new(panel.x + 31.0, status_y, state_width, 20.0),
        11.0,
        UI_CYAN,
        false,
    );
    label(
        primitives,
        &model.status,
        CssRect::new(
            panel.x + 31.0 + state_width,
            status_y,
            (panel.width - state_width - 49.0).max(0.0),
            20.0,
        ),
        11.0,
        TEXT,
        false,
    );

    let response = model.response.as_ref().map_or_else(
        || String::from("—"),
        |response| {
            let mime = response.content_type.split(';').next().unwrap_or("").trim();
            format!("{}  {}", response.status, mime)
        },
    );
    let source_size = model.response.as_ref().map_or_else(
        || String::from("—"),
        |response| crate::download::human_bytes(response.bytes as u64),
    );
    let viewport = format!(
        "{:.0} × {:.0}  / {:.2}×",
        scene.viewport.css.width,
        scene.viewport.css.height,
        scene.viewport.scale_factor.get()
    );
    let canvas = if scene.page_size.height > 0.0 {
        format!(
            "{:.0} × {:.0} px",
            scene.page_size.width, scene.page_size.height
        )
    } else {
        String::from("—")
    };
    let cells = if panel.width >= 900.0 {
        5
    } else if panel.width >= 680.0 {
        4
    } else {
        2
    };
    let cell_width = (panel.width - 32.0).max(0.0) / cells as f32;
    for (i, (title, value)) in [
        ("RESPONSE", response),
        ("VIEWPORT / CSS PX", viewport),
        ("DOCUMENT / DECODED", source_size),
        (
            "IMAGE CACHE / RAM",
            crate::download::human_bytes(scene.image_store.desktop_page_image_bytes() as u64),
        ),
        ("CANVAS / CSS PX", canvas),
    ]
    .into_iter()
    .take(cells)
    .enumerate()
    {
        let x = panel.x + 16.0 + i as f32 * cell_width;
        let rect = CssRect::new(x, panel.y + 118.0, cell_width, 41.0);
        fill(
            primitives,
            CssRect::new(x, rect.y, cell_width - 10.0, 1.0),
            EDGE,
        );
        label(
            primitives,
            title,
            CssRect::new(x, rect.y + 4.0, cell_width - 14.0, 14.0),
            9.0,
            MUTED,
            false,
        );
        label(
            primitives,
            &value,
            CssRect::new(x, rect.y + 19.0, cell_width - 14.0, 20.0),
            12.0,
            TEXT,
            false,
        );
    }

    let mut x = panel.x + 16.0;
    for (key, action) in [
        ("TAB", "close"),
        ("ENTER", "run/open"),
        ("↑↓", "history"),
        ("ESC", "stop"),
        ("help", "reference"),
    ] {
        let style = command_text_style_with_size(10.0);
        let key_width = crate::text::shape(key, &style).advance + 12.0;
        let action_width = crate::text::shape(action, &style).advance;
        if x + key_width + action_width + 8.0 > right - 16.0 {
            continue;
        }
        let key_rect = CssRect::new(x, panel.y + 174.0, key_width, 19.0);
        fill(primitives, key_rect, PaintColor::Rgba(31, 27, 48, 255));
        label(primitives, key, key_rect, 10.0, UI_AMBER, true);
        label(
            primitives,
            action,
            CssRect::new(
                x + key_width + 6.0,
                key_rect.y,
                action_width + 1.0,
                key_rect.height,
            ),
            10.0,
            MUTED,
            false,
        );
        x += key_width + action_width + 22.0;
    }
    primitives.push(Primitive::PopClip);

    scene.controls.push(ControlRegion {
        id: ControlId::CommandPanel,
        rect: panel,
        enabled: true,
    });
    scene.controls.push(ControlRegion {
        id: ControlId::Command,
        rect: geometry.input,
        enabled: true,
    });
}

fn fill(primitives: &mut Vec<Primitive>, rect: CssRect, color: PaintColor) {
    if rect.width > 0.0 && rect.height > 0.0 {
        primitives.push(Primitive::FillRect { rect, color });
    }
}

fn label(
    primitives: &mut Vec<Primitive>,
    text: &str,
    rect: CssRect,
    size: f32,
    color: PaintColor,
    centered: bool,
) {
    if rect.width <= 0.0 || text.is_empty() {
        return;
    }
    let style = command_text_style_with_size(size);
    let end = crate::text::first_line_end(
        text,
        &style,
        rect.width,
        crate::text::TextBreakStyle {
            wrap: false,
            ..Default::default()
        },
    );
    let shaped = crate::text::shape(&text[..end], &style);
    let origin = CssPoint::new(
        rect.x
            + if centered {
                (rect.width - shaped.advance) / 2.0
            } else {
                0.0
            },
        rect.y + (rect.height - shaped.line_height) / 2.0,
    );
    primitives.push(Primitive::GlyphRun {
        origin,
        shaped,
        color,
        decoration: TextDecorationPaint {
            color,
            style: DecorationStyle::Solid,
        },
        shadows: Vec::new(),
        clip: Some(rect),
        node: 0,
        link: None,
    });
}

fn paint_editor(primitives: &mut Vec<Primitive>, editor: &EditorVisual, rect: CssRect) {
    primitives.push(Primitive::PushClip(PaintShape::Rect(rect)));
    for selection in &editor.selection {
        fill(
            primitives,
            selection.translate(rect.x, rect.y),
            PaintColor::Rgba(0, 255, 249, 48),
        );
    }
    paint_ui_text(
        primitives,
        &editor.text,
        CssPoint::new(rect.x, rect.y),
        UI_PINK,
        rect.width,
        command_text_style(),
    );
    if let Some(caret) = editor.caret {
        fill(primitives, caret.translate(rect.x, rect.y), HEART_LIGHT);
    }
    primitives.push(Primitive::PopClip);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{PhysicalSize, ScaleFactor};

    #[test]
    fn command_overlay_keeps_fixed_boxes_viewport_units_and_scroll_geometry() {
        // CSS Position 3 #fixed-cb; CSS Values 4 #viewport-variants. A UA
        // overlay changes neither the initial fixed CB nor viewport lengths.
        let mut dom = crate::dom::Dom::parse_document(
            r#"
            <style>
              body { margin:0; height:200vh }
              #footer { position:fixed; bottom:0; width:100vw; height:24px }
              #middle { position:fixed; top:50vh; width:10vw; height:10vh }
              @media (min-height:500px) { #footer { height:30px } }
            </style>
            <div id=footer>fixed footer</div><div id=middle>middle</div>
        "#,
        );
        let base = url::Url::parse("https://example.com/").unwrap();
        let footer = dom.get_by_id("footer").unwrap();
        let middle = dom.get_by_id("middle").unwrap();
        for scale in [1.0, 1.25, 2.0] {
            let metrics = ViewportMetrics::from_physical(
                PhysicalSize::new((800.0 * scale) as u32, (600.0 * scale) as u32),
                ScaleFactor::new(scale),
            );
            let mut baseline = None;
            for command in [None, Some(EditorVisual::default()), None] {
                let model = ChromeModel {
                    command,
                    ..Default::default()
                };
                let mut scene = desktop_chrome(metrics, &super::super::tests::snapshot(), &model);
                let viewport = crate::layout2::Viewport::new(
                    scene.content_viewport.width,
                    scene.content_viewport.height,
                );
                dom.set_viewport_px(scene.content_viewport.width, scene.content_viewport.height);
                let layout = crate::layout2::lay_out_graphical(
                    &dom,
                    &base,
                    viewport,
                    &[],
                    &Default::default(),
                    &Default::default(),
                );
                assert_eq!(scene.content_viewport, CssRect::new(0.0, 0.0, 800.0, 600.0));
                assert!(
                    (layout.boxes[&footer].top - 570.0).abs() < 0.01,
                    "{:?}",
                    layout.boxes[&footer]
                );
                assert!((layout.boxes[&middle].top - 300.0).abs() < 0.01);
                assert!((layout.boxes[&middle].height - 60.0).abs() < 0.01);
                scene.append_page(&layout.paint, CssPoint::new(0.0, 450.0));
                let geometry = (
                    layout.boxes[&footer],
                    layout.boxes[&middle],
                    scene.page_size,
                );
                if let Some(expected) = &baseline {
                    assert_eq!(&geometry, expected);
                } else {
                    baseline = Some(geometry);
                }
                let page_commands = scene.primitives.clone();
                paint_desktop_overlay(&mut scene, &super::super::tests::snapshot(), &model);
                assert_eq!(&scene.primitives[..page_commands.len()], &page_commands);
            }
        }
    }

    #[test]
    fn command_surface_owns_input_over_page_and_scrollbars() {
        for (width, height, scale) in [(480, 320, 1.0), (960, 640, 1.25), (1920, 1080, 2.0)] {
            let metrics = ViewportMetrics::from_physical(
                PhysicalSize::new(width, height),
                ScaleFactor::new(scale),
            );
            let model = ChromeModel {
                command: Some(EditorVisual::default()),
                heart: HeartVisual {
                    vertical_visible: true,
                    vertical_fraction: Some(0.9),
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut scene = desktop_chrome(metrics, &super::super::tests::snapshot(), &model);
            scene.page_size = CssSize::new(metrics.css.width, 2_000.0);
            paint_desktop_overlay(&mut scene, &super::super::tests::snapshot(), &model);
            let geometry = CommandPanelGeometry::new(metrics.css);
            let panel = geometry.panel;
            assert_eq!(
                scene.control_at(CssPoint::new(panel.x + 20.0, panel.y + 20.0)),
                Some(ControlId::CommandPanel)
            );
            assert_eq!(
                scene.control_at(CssPoint::new(
                    geometry.editor.x + 4.0,
                    geometry.editor.y + 4.0
                )),
                Some(ControlId::Command)
            );
            assert_eq!(
                scene.control_at(CssPoint::new(metrics.css.width - 17.0, panel.y + 150.0)),
                Some(ControlId::CommandPanel)
            );
            assert_eq!(scene.control_at(CssPoint::new(50.0, panel.y - 12.0)), None);
            assert!(geometry.input.y >= panel.y);
            assert!(geometry.input.y + geometry.input.height <= panel.y + panel.height);
            assert!(geometry.editor.x + geometry.editor.width < panel.x + panel.width);
        }
    }
}
