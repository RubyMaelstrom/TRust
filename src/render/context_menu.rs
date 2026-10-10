//! Desktop context menu chrome: the menu the secondary button opens over a
//! page. Like COMMAND it is browser chrome; its geometry is shared by paint,
//! pointer hit testing and native accessibility, and it never reaches the
//! page's DOM or layout. The page only decides, through `contextmenu`,
//! whether it is shown (Pointer Events #maybe-show-context-menu).

use super::*;

const SURFACE: PaintColor = PaintColor::Rgba(12, 9, 28, 250);
const INSET: PaintColor = PaintColor::Rgba(5, 8, 19, 255);
const EDGE: PaintColor = PaintColor::Rgba(44, 67, 80, 255);
const RULE: PaintColor = PaintColor::Rgba(36, 48, 66, 255);
const MUTED: PaintColor = PaintColor::Rgba(147, 156, 179, 255);
const DISABLED: PaintColor = PaintColor::Rgba(88, 92, 112, 255);
const KEY: PaintColor = PaintColor::Rgba(31, 27, 48, 255);
const TEXT: PaintColor = theme_color(crate::theme::TEXT);

const WIDTH: f32 = 252.0;
const HEADER: f32 = 32.0;
const ROW: f32 = 30.0;
const RULE_SPACE: f32 = 9.0;
const FOOT: f32 = 7.0;
/// The distance kept from the content viewport's edges.
const MARGIN: f32 = 4.0;
const SHADOW_BLUR: f32 = 14.0;
const SHADOW_OFFSET: f32 = 6.0;

/// The pictogram drawn beside an item, in the menu's line style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextMenuIcon {
    Copy,
    Reload,
    Command,
    Kill,
    BookmarkAdd,
    BookmarkRemove,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextMenuItem {
    pub label: String,
    /// The keyboard shortcut doing the same, shown as COMMAND shows keys.
    pub shortcut: Option<&'static str>,
    pub icon: ContextMenuIcon,
    pub enabled: bool,
    /// A rule separates this item from the one before it.
    pub separated: bool,
    /// The item ends something (the page's scripts): an amber accent.
    pub caution: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextMenuGeometry {
    pub panel: CssRect,
    /// One row per item, in item order.
    pub rows: Vec<CssRect>,
}

impl ContextMenuGeometry {
    /// Lay the menu out from the pointer, `anchor`: below and to the right
    /// of it where it fits, otherwise flipped above or to its left, and
    /// always inside `bounds` (the page's content viewport).
    pub fn new(anchor: CssPoint, bounds: CssRect, items: &[ContextMenuItem]) -> Self {
        let height = HEADER
            + items
                .iter()
                .map(|item| ROW + if item.separated { RULE_SPACE } else { 0.0 })
                .sum::<f32>()
            + FOOT;
        let width = WIDTH.min((bounds.width - 2.0 * MARGIN).max(0.0));
        let height_fit = height.min((bounds.height - 2.0 * MARGIN).max(0.0));
        let place = |at: f32, size: f32, start: f32, extent: f32| {
            let end = start + extent - MARGIN;
            let at = if at + size > end && at - size >= start + MARGIN {
                at - size
            } else {
                at
            };
            at.min(end - size).max(start + MARGIN)
        };
        let panel = CssRect::new(
            place(anchor.x, width, bounds.x, bounds.width).round(),
            place(anchor.y, height_fit, bounds.y, bounds.height).round(),
            width,
            height,
        );
        let mut y = panel.y + HEADER;
        let rows = items
            .iter()
            .map(|item| {
                if item.separated {
                    y += RULE_SPACE;
                }
                let row = CssRect::new(panel.x + 6.0, y, width - 12.0, ROW);
                y += ROW;
                row
            })
            .collect();
        Self { panel, rows }
    }

    /// The item whose row is under `point`.
    pub fn item_at(&self, point: CssPoint) -> Option<usize> {
        self.rows.iter().position(|row| row.contains(point))
    }

    /// The menu with its static shadow: what showing, highlighting or
    /// hiding it repaints.
    pub fn damage(&self) -> CssRect {
        // A blurred shadow reaches twice its blur radius (as the raster
        // backends and scene damage bound it), offset downward.
        expand_rect(self.panel, SHADOW_BLUR * 2.0 + SHADOW_OFFSET + 2.0)
    }
}

/// Paint the menu: COMMAND's surface, etched edge and accents, one row per
/// item with its icon, label and shortcut, `highlighted` the item the
/// pointer or keyboard is on. `title` names the page it acts on.
pub fn paint_context_menu(
    primitives: &mut Vec<Primitive>,
    geometry: &ContextMenuGeometry,
    items: &[ContextMenuItem],
    highlighted: Option<usize>,
    title: &str,
) {
    let panel = geometry.panel;
    let (right, bottom) = (panel.x + panel.width, panel.y + panel.height);
    let corners = [
        CssPoint::new(panel.x + 8.0, panel.y),
        CssPoint::new(right - 16.0, panel.y),
        CssPoint::new(right, panel.y + 16.0),
        CssPoint::new(right, bottom - 8.0),
        CssPoint::new(right - 8.0, bottom),
        CssPoint::new(panel.x, bottom),
        CssPoint::new(panel.x, panel.y + 8.0),
    ];
    // A static, soft drop shadow following the chamfered outline: no
    // animation timer or continuous redraw.
    primitives.push(Primitive::Shadow {
        shape: PaintShape::Polygon {
            points: corners.to_vec(),
            evenodd: false,
        },
        color: PaintColor::Rgba(0, 0, 0, 110),
        offset: CssPoint::new(0.0, SHADOW_OFFSET),
        blur_radius: SHADOW_BLUR,
        spread: 0.0,
        inset: false,
    });
    let mut outline = vec![PathElement::MoveTo(corners[0])];
    outline.extend(
        corners[1..]
            .iter()
            .map(|&corner| PathElement::LineTo(corner)),
    );
    outline.push(PathElement::Close);
    let outline = PaintShape::Path(outline);
    primitives.push(Primitive::Fill {
        shape: outline.clone(),
        brush: PaintBrush::Solid(SURFACE),
    });
    primitives.push(Primitive::Stroke {
        shape: outline,
        brush: PaintBrush::Solid(EDGE),
        style: StrokeStyle::solid(1.0),
    });
    // Everything below lies inside the chamfered outline, so the menu needs
    // no clip scope: without one, scene damage stays the menu's rectangle.
    fill(
        primitives,
        CssRect::new(panel.x + 8.0, panel.y, 64.0, 2.0),
        UI_PINK,
    );
    fill(
        primitives,
        CssRect::new(right - 84.0, bottom - 1.0, 68.0, 1.0),
        UI_CYAN,
    );
    let mut traces = Vec::new();
    for i in 0..3 {
        let x = right - 44.0 + i as f32 * 8.0;
        traces.extend([
            PathElement::MoveTo(CssPoint::new(x, panel.y + 5.0)),
            PathElement::LineTo(CssPoint::new(x + 8.0, panel.y + 13.0)),
        ]);
    }
    primitives.push(Primitive::Stroke {
        shape: PaintShape::Path(traces),
        brush: PaintBrush::Solid(EDGE),
        style: StrokeStyle::solid(2.0),
    });

    let badge = CssRect::new(panel.x + 10.0, panel.y + 9.0, 46.0, 16.0);
    fill(primitives, badge, UI_PINK);
    label(primitives, "PAGE", badge, 10.0, INSET, true);
    label(
        primitives,
        title,
        CssRect::new(
            badge.x + badge.width + 9.0,
            badge.y,
            (panel.width - badge.width - 76.0).max(0.0),
            badge.height,
        ),
        10.0,
        MUTED,
        false,
    );

    for (index, (item, row)) in items.iter().zip(&geometry.rows).enumerate() {
        if item.separated {
            fill(
                primitives,
                CssRect::new(
                    row.x + 6.0,
                    row.y - RULE_SPACE / 2.0 - 0.5,
                    row.width - 12.0,
                    1.0,
                ),
                RULE,
            );
        }
        let active = item.enabled && highlighted == Some(index);
        if active {
            fill(primitives, *row, INSET);
            primitives.push(Primitive::Stroke {
                shape: PaintShape::Rect(*row),
                brush: PaintBrush::Solid(EDGE),
                style: StrokeStyle::solid(1.0),
            });
            fill(
                primitives,
                CssRect::new(row.x, row.y, 2.0, row.height),
                if item.caution { UI_AMBER } else { UI_CYAN },
            );
        }
        let (icon_color, text_color) = match (item.enabled, active, item.caution) {
            (false, ..) => (DISABLED, DISABLED),
            (true, true, true) => (UI_AMBER, HEART_LIGHT),
            (true, true, false) => (UI_PINK, HEART_LIGHT),
            (true, false, true) => (UI_AMBER, TEXT),
            (true, false, false) => (UI_CYAN, TEXT),
        };
        paint_icon(
            primitives,
            item.icon,
            CssRect::new(row.x + 11.0, row.y + 8.0, 14.0, 14.0),
            icon_color,
        );
        let mut label_width = row.width - 46.0;
        if let Some(shortcut) = item.shortcut {
            let style = command_text_style_with_size(10.0);
            let key_width = crate::text::shape(shortcut, &style).advance + 12.0;
            let key = CssRect::new(
                row.x + row.width - key_width - 8.0,
                row.y + 6.0,
                key_width,
                18.0,
            );
            fill(primitives, key, KEY);
            label(
                primitives,
                shortcut,
                key,
                10.0,
                if item.enabled { UI_AMBER } else { DISABLED },
                true,
            );
            label_width -= key_width + 12.0;
        }
        label(
            primitives,
            &item.label,
            CssRect::new(row.x + 36.0, row.y, label_width.max(0.0), row.height),
            12.5,
            text_color,
            false,
        );
    }
}

fn paint_icon(
    primitives: &mut Vec<Primitive>,
    icon: ContextMenuIcon,
    rect: CssRect,
    color: PaintColor,
) {
    let at = |x: f32, y: f32| CssPoint::new(rect.x + x, rect.y + y);
    let mut stroke = |path: Vec<PathElement>| {
        primitives.push(Primitive::Stroke {
            shape: PaintShape::Path(path),
            brush: PaintBrush::Solid(color),
            style: StrokeStyle {
                cap: LineCap::Round,
                ..StrokeStyle::solid(1.5)
            },
        });
    };
    match icon {
        ContextMenuIcon::Copy => {
            stroke(vec![
                PathElement::MoveTo(at(4.5, 4.5)),
                PathElement::LineTo(at(13.0, 4.5)),
                PathElement::LineTo(at(13.0, 13.5)),
                PathElement::LineTo(at(4.5, 13.5)),
                PathElement::Close,
            ]);
            stroke(vec![
                PathElement::MoveTo(at(2.0, 10.5)),
                PathElement::LineTo(at(1.0, 10.5)),
                PathElement::LineTo(at(1.0, 0.75)),
                PathElement::LineTo(at(9.5, 0.75)),
                PathElement::LineTo(at(9.5, 2.0)),
            ]);
        }
        ContextMenuIcon::Reload => {
            // Three quarters of a circle and its arrowhead.
            let (cx, cy, r) = (7.0, 7.5, 5.25);
            let k = 0.5523 * r;
            stroke(vec![
                PathElement::MoveTo(at(cx + r, cy)),
                PathElement::CurveTo(at(cx + r, cy + k), at(cx + k, cy + r), at(cx, cy + r)),
                PathElement::CurveTo(at(cx - k, cy + r), at(cx - r, cy + k), at(cx - r, cy)),
                PathElement::CurveTo(at(cx - r, cy - k), at(cx - k, cy - r), at(cx, cy - r)),
                PathElement::LineTo(at(cx + 1.5, cy - r)),
            ]);
            stroke(vec![
                PathElement::MoveTo(at(cx - 0.5, cy - r - 3.0)),
                PathElement::LineTo(at(cx + 2.5, cy - r)),
                PathElement::LineTo(at(cx - 0.5, cy - r + 3.0)),
            ]);
        }
        ContextMenuIcon::Command => {
            stroke(vec![
                PathElement::MoveTo(at(1.5, 3.0)),
                PathElement::LineTo(at(6.0, 7.0)),
                PathElement::LineTo(at(1.5, 11.0)),
            ]);
            stroke(vec![
                PathElement::MoveTo(at(7.5, 12.5)),
                PathElement::LineTo(at(13.0, 12.5)),
            ]);
        }
        ContextMenuIcon::Kill => {
            // A stop sign holding a cross.
            let points = [
                (4.5, 1.0),
                (9.5, 1.0),
                (13.0, 4.5),
                (13.0, 9.5),
                (9.5, 13.0),
                (4.5, 13.0),
                (1.0, 9.5),
                (1.0, 4.5),
            ];
            let mut path = vec![PathElement::MoveTo(at(points[0].0, points[0].1))];
            path.extend(
                points[1..]
                    .iter()
                    .map(|&(x, y)| PathElement::LineTo(at(x, y))),
            );
            path.push(PathElement::Close);
            stroke(path);
            stroke(vec![
                PathElement::MoveTo(at(5.0, 5.0)),
                PathElement::LineTo(at(9.0, 9.0)),
                PathElement::MoveTo(at(9.0, 5.0)),
                PathElement::LineTo(at(5.0, 9.0)),
            ]);
        }
        ContextMenuIcon::BookmarkAdd | ContextMenuIcon::BookmarkRemove => {
            stroke(vec![
                PathElement::MoveTo(at(2.5, 1.0)),
                PathElement::LineTo(at(11.5, 1.0)),
                PathElement::LineTo(at(11.5, 13.5)),
                PathElement::LineTo(at(7.0, 10.0)),
                PathElement::LineTo(at(2.5, 13.5)),
                PathElement::Close,
            ]);
            let mut sign = vec![
                PathElement::MoveTo(at(4.75, 5.5)),
                PathElement::LineTo(at(9.25, 5.5)),
            ];
            if icon == ContextMenuIcon::BookmarkAdd {
                sign.extend([
                    PathElement::MoveTo(at(7.0, 3.25)),
                    PathElement::LineTo(at(7.0, 7.75)),
                ]);
            }
            stroke(sign);
        }
    }
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
                ((rect.width - shaped.advance) / 2.0).round()
            } else {
                0.0
            },
        (rect.y + (rect.height - shaped.line_height) / 2.0).round(),
    );
    primitives.push(Primitive::GlyphRun {
        origin,
        shaped,
        color,
        decoration: TextDecorationPaint {
            color,
            style: DecorationStyle::Solid,
            thickness: None,
            underline_offset: None,
        },
        shadows: Vec::new(),
        clip: Some(rect),
        node: 0,
        link: None,
        selection: Default::default(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(count: usize) -> Vec<ContextMenuItem> {
        (0..count)
            .map(|index| ContextMenuItem {
                label: format!("Item {index}"),
                shortcut: None,
                icon: ContextMenuIcon::Reload,
                enabled: true,
                separated: index == 1,
                caution: false,
            })
            .collect()
    }

    /// `TRUST_MENU_PREVIEW=DIR cargo test --lib context_menu_preview -- --ignored`
    /// writes the menu over a page with a selection, light and dark, as
    /// the CPU reference renderer paints them.
    #[test]
    #[ignore = "writes preview images; set TRUST_MENU_PREVIEW"]
    fn context_menu_preview() {
        let Some(dir) = std::env::var_os("TRUST_MENU_PREVIEW") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        for (name, html, author) in [
            (
                "light",
                "<body style='margin:24px;font:17px/1.5 serif'><h2>Ruby's House</h2>\
                 <p id=p>Come relax at Ruby's House, where the strawberries are always \
                 in season and the kettle is always on. <b>Bold claims</b> follow.</p>\
                 <p>Second paragraph of ordinary page text for the selection.</p></body>",
                false,
            ),
            (
                "dark",
                "<body style='margin:24px;font:17px/1.5 sans-serif;color-scheme:dark;\
                 background:#1c1b22;color:#fbfbfe'><h2>Night mode</h2>\
                 <p id=p>A dark page keeps its own text color under the highlight, \
                 with the scheme's Highlight.</p><p>More text below it.</p></body>",
                false,
            ),
            (
                "author",
                "<style>::selection{background:#ff3fa4;color:#fff}</style>\
                 <body style='margin:24px;font:17px/1.5 serif'><h2>Author colors</h2>\
                 <p id=p>The page styles ::selection, so the highlight uses its colors \
                 exactly and redraws the selected text.</p></body>",
                true,
            ),
        ] {
            let dom = crate::dom::Dom::parse_document(html);
            let layout = crate::layout2::lay_out_graphical(
                &dom,
                &url::Url::parse("https://rubymaelstrom.com/").unwrap(),
                crate::layout2::Viewport::new(720.0, 420.0),
                &[],
                &Default::default(),
                &Default::default(),
            );
            let p = dom.get_by_id("p").unwrap();
            let texts: Vec<usize> = dom
                .descendants(p)
                .filter(|&node| dom.tag_name(node).is_none())
                .collect();
            let selection = crate::render::PageSelection::Painted {
                anchor: crate::core::TextPoint {
                    node: texts[0],
                    offset: 9,
                },
                focus: crate::core::TextPoint {
                    node: *texts.last().unwrap(),
                    offset: if author { 30 } else { 8 },
                },
            };
            let physical = PhysicalSize::new(720, 420);
            let mut scene = Scene {
                viewport: ViewportMetrics::from_physical(physical, Default::default()),
                primitives: Vec::new(),
                controls: Vec::new(),
                content_viewport: CssRect::new(0.0, 0.0, 720.0, 420.0),
                image_store: Default::default(),
                canvas_images: Default::default(),
                page_scroll_containers: Vec::new(),
                page_size: Default::default(),
            };
            scene.append_page_with_selection(
                &layout.paint,
                CssPoint::default(),
                0.0,
                &HashMap::new(),
                Some(&selection),
            );
            let items = vec![
                ContextMenuItem {
                    label: "Copy".into(),
                    shortcut: Some("Ctrl+C"),
                    icon: ContextMenuIcon::Copy,
                    enabled: true,
                    separated: false,
                    caution: false,
                },
                ContextMenuItem {
                    label: "Reload".into(),
                    shortcut: Some("Ctrl+R"),
                    icon: ContextMenuIcon::Reload,
                    enabled: true,
                    separated: true,
                    caution: false,
                },
                ContextMenuItem {
                    label: "Open COMMAND".into(),
                    shortcut: Some("Tab"),
                    icon: ContextMenuIcon::Command,
                    enabled: true,
                    separated: false,
                    caution: false,
                },
                ContextMenuItem {
                    label: "Kill Page".into(),
                    shortcut: None,
                    icon: ContextMenuIcon::Kill,
                    enabled: name != "dark",
                    separated: false,
                    caution: true,
                },
                ContextMenuItem {
                    label: if name == "light" {
                        "Remove Bookmark"
                    } else {
                        "Add Bookmark"
                    }
                    .into(),
                    shortcut: (name != "light").then_some("Ctrl+B"),
                    icon: if name == "light" {
                        ContextMenuIcon::BookmarkRemove
                    } else {
                        ContextMenuIcon::BookmarkAdd
                    },
                    enabled: true,
                    separated: true,
                    caution: false,
                },
            ];
            let geometry = ContextMenuGeometry::new(
                CssPoint::new(390.0, 120.0),
                CssRect::new(0.0, 0.0, 720.0, 420.0),
                &items,
            );
            paint_context_menu(
                &mut scene.primitives,
                &geometry,
                &items,
                Some(if name == "author" { 3 } else { 2 }),
                "HTTPS  /  rubymaelstrom.com",
            );
            let frame = super::super::vello_cpu::VelloCpuRenderer::new()
                .render_rgba(&scene)
                .unwrap();
            super::super::headless::write_png(&frame, dir.join(format!("context-menu-{name}.png")))
                .unwrap();
        }
    }

    #[test]
    fn the_menu_opens_from_the_pointer_and_flips_to_stay_inside() {
        let bounds = CssRect::new(0.0, 40.0, 800.0, 560.0);
        let menu = ContextMenuGeometry::new(CssPoint::new(100.0, 100.0), bounds, &items(4));
        assert_eq!((menu.panel.x, menu.panel.y), (100.0, 100.0));
        assert_eq!(menu.rows.len(), 4);
        assert_eq!(menu.rows[1].y - menu.rows[0].y, ROW + RULE_SPACE);
        assert_eq!(
            menu.item_at(CssPoint::new(150.0, menu.rows[2].y + 3.0)),
            Some(2)
        );
        assert_eq!(menu.item_at(CssPoint::new(150.0, menu.panel.y + 3.0)), None);
        let corner = ContextMenuGeometry::new(CssPoint::new(790.0, 590.0), bounds, &items(4));
        assert!(corner.panel.x + corner.panel.width <= 800.0 - MARGIN);
        assert!(corner.panel.y + corner.panel.height <= 600.0 - MARGIN);
        assert_eq!(
            corner.panel.x,
            790.0 - WIDTH,
            "flipped to the pointer's left"
        );
    }

    #[test]
    fn painting_is_bounded_by_the_menus_damage() {
        let bounds = CssRect::new(0.0, 0.0, 800.0, 600.0);
        let list = items(5);
        let menu = ContextMenuGeometry::new(CssPoint::new(300.0, 200.0), bounds, &list);
        let mut primitives = Vec::new();
        paint_context_menu(
            &mut primitives,
            &menu,
            &list,
            Some(2),
            "HTTPS / example.test",
        );
        let damage = menu.damage();
        for primitive in &primitives {
            if let Primitive::FillRect { rect, .. } = primitive {
                assert!(
                    rect.x >= damage.x
                        && rect.y >= damage.y
                        && rect.x + rect.width <= damage.x + damage.width
                        && rect.y + rect.height <= damage.y + damage.height,
                    "{rect:?} outside {damage:?}"
                );
            }
        }
        assert!(
            !primitives
                .iter()
                .any(|p| matches!(p, Primitive::PushClip(_) | Primitive::PushLayer(_))),
            "no paint-state scope, so damage stays the menu's"
        );
    }
}
