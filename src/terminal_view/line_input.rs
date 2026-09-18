//! Native Telnet line entry. Glyphs, caret, selection and hit testing all use
//! the editor's retained layout, translated once into the visible field.

use std::sync::Arc;

use crate::core::CssPoint;
use crate::render::{
    CssRect, DisplayCommand, PaintBrush, PaintColor, PaintShape, StrokeStyle, TextDecorationPaint,
};
use crate::text::{EditorLine, EditorRect, TextEditor};

pub fn terminal_input_rect(content: CssRect) -> CssRect {
    CssRect::new(
        content.x + 8.0,
        content.y + content.height - 30.0,
        (content.width - 16.0).max(1.0),
        26.0,
    )
}

#[derive(Default)]
pub struct TerminalInputView {
    bounds: CssRect,
    clip: CssRect,
    origin: CssPoint,
    scroll_x: f32,
    line: Option<Arc<EditorLine>>,
    masked: bool,
    pub selecting: bool,
}

impl TerminalInputView {
    pub fn update(&mut self, editor: &mut TextEditor, bounds: CssRect, masked: bool) {
        let line = editor.line_layout(masked);
        let padding = 8.0_f32.min((bounds.width - 1.0).max(0.0) / 2.0);
        self.bounds = bounds;
        self.clip = CssRect::new(
            bounds.x + padding,
            bounds.y + 1.0,
            (bounds.width - 2.0 * padding).max(1.0),
            (bounds.height - 2.0).max(1.0),
        );
        let caret = line.caret.unwrap_or(line.ime);
        let max_scroll = (line.shaped.advance + caret.width.max(1.5) - self.clip.width).max(0.0);
        self.scroll_x = self.scroll_x.clamp(0.0, max_scroll);
        if caret.x < self.scroll_x {
            self.scroll_x = caret.x.max(0.0);
        } else if caret.x + caret.width > self.scroll_x + self.clip.width {
            self.scroll_x = (caret.x + caret.width - self.clip.width).min(max_scroll);
        }
        // CSS Inline 3 #ascent-descent / #inline-height: center the complete
        // typographic line box, retaining its baseline and half-leading.
        self.origin = CssPoint::new(
            self.clip.x - self.scroll_x,
            bounds.y + (bounds.height - line.shaped.line_height) / 2.0,
        );
        self.masked = masked;
        self.line = Some(line);
    }

    pub fn paint(&self, commands: &mut Vec<DisplayCommand>, focused: bool) {
        let Some(line) = &self.line else {
            return;
        };
        let color = PaintColor::Rgba(222, 232, 242, 255);
        commands.push(DisplayCommand::FillRect {
            rect: self.bounds,
            color: PaintColor::Rgba(20, 25, 33, 255),
        });
        commands.push(DisplayCommand::PushClip(PaintShape::Rect(self.clip)));
        for rect in &line.selection {
            commands.push(DisplayCommand::FillRect {
                rect: self.translate(*rect),
                color: PaintColor::Rgba(88, 148, 255, if focused { 110 } else { 55 }),
            });
        }
        commands.push(DisplayCommand::GlyphRun {
            origin: self.origin,
            shaped: line.shaped.clone(),
            color,
            decoration: TextDecorationPaint {
                color,
                style: crate::render::DecorationStyle::Solid,
            },
            shadows: Vec::new(),
            clip: Some(self.clip),
            node: 0,
            link: None,
        });
        for rect in &line.underlines {
            commands.push(DisplayCommand::FillRect {
                rect: self.translate(*rect),
                color,
            });
        }
        if focused && let Some(caret) = line.caret {
            commands.push(DisplayCommand::FillRect {
                rect: self.translate(caret),
                color,
            });
        }
        commands.push(DisplayCommand::PopClip);
        commands.push(DisplayCommand::Stroke {
            shape: PaintShape::Rect(self.bounds),
            brush: PaintBrush::Solid(if focused { PaintColor::Accent } else { color }),
            style: StrokeStyle::solid(1.0),
        });
    }

    pub fn move_to_point(&mut self, editor: &mut TextEditor, point: CssPoint, extend: bool) {
        editor.move_to_line_point(
            point.x - self.origin.x,
            point.y.clamp(self.clip.y, self.clip.y + self.clip.height) - self.origin.y,
            extend,
            self.masked,
        );
        self.update(editor, self.bounds, self.masked);
    }

    pub fn ime_area(&self) -> Option<CssRect> {
        let line = self.line.as_ref()?;
        let area = self.translate(line.caret.unwrap_or(line.ime));
        // Candidate windows stay attached to the visible caret after scrolling.
        Some(CssRect::new(
            area.x
                .clamp(self.clip.x, self.clip.x + self.clip.width - 1.0),
            self.clip.y,
            area.width.max(1.0).min(self.clip.width),
            self.clip.height,
        ))
    }

    fn translate(&self, rect: EditorRect) -> CssRect {
        CssRect::new(
            self.origin.x + rect.x,
            self.origin.y + rect.y,
            rect.width,
            rect.height,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Key, KeyInput, KeyState, Modifiers};
    use crate::terminal_view::{TerminalView, terminal_text_style};

    fn key(key: Key, shift: bool) -> KeyInput {
        KeyInput {
            key,
            code: String::new(),
            location: 0,
            state: KeyState::Pressed,
            modifiers: Modifiers {
                shift,
                ..Default::default()
            },
            repeat: false,
            composing: false,
        }
    }

    #[test]
    fn terminal_input_glyphs_caret_and_selection_share_the_terminal_font() {
        let style = terminal_text_style();
        let mut terminal = TerminalView::new(1, 80);
        terminal.process(b"@");
        let terminal_glyph = terminal
            .paint()
            .primitives
            .iter()
            .find_map(|command| {
                if let DisplayCommand::GlyphRun { shaped, .. } = command {
                    Some(shaped.clone())
                } else {
                    None
                }
            })
            .unwrap();
        let (cell, _) = terminal.cell_size();
        for text in [
            "",
            "centered",
            "@sym",
            "@ @@@",
            "fdsfs dsfdsf   sdfdsfdsf    sdfdsfdsfds   sdfdsffsdf",
            "     ",
            "trailing     ",
        ] {
            let mut editor = TextEditor::new(text, &style, 900.0, false);
            let mut view = TerminalInputView::default();
            let bounds = CssRect::new(8.0, 40.0, 944.0, 26.0);
            view.update(&mut editor, bounds, false);
            let line = view.line.as_ref().unwrap();
            let caret = line.caret.unwrap();
            assert!(
                (caret.x - text.len() as f32 * cell).abs() < 0.02,
                "{text:?}: {caret:?}, cell {cell}"
            );
            assert!(
                (view.origin.y + line.shaped.line_height / 2.0 - (bounds.y + bounds.height / 2.0))
                    .abs()
                    < 0.01
            );
            if !text.is_empty() {
                assert_eq!(line.shaped.runs[0].font, terminal_glyph.runs[0].font);
                assert_eq!(
                    line.shaped.runs[0].font_size,
                    terminal_glyph.runs[0].font_size
                );
                editor.select_all();
                view.update(&mut editor, bounds, false);
                let line = view.line.as_ref().unwrap();
                assert!(
                    (line.selection.iter().map(|rect| rect.width).sum::<f32>() - caret.x).abs()
                        < 0.02
                );
            }
            let mut commands = Vec::new();
            view.paint(&mut commands, true);
            assert!(commands.iter().any(|command| matches!(command,
                DisplayCommand::GlyphRun { shaped, origin, .. }
                if *shaped == view.line.as_ref().unwrap().shaped && *origin == view.origin)));
        }
    }

    #[test]
    fn terminal_input_scrolls_to_the_active_selection_end_and_hit_tests_visible_text() {
        let mut editor =
            TextEditor::new(&"@ ab  ".repeat(50), &terminal_text_style(), 200.0, false);
        let mut view = TerminalInputView::default();
        let bounds = CssRect::new(8.0, 40.0, 180.0, 26.0);
        view.update(&mut editor, bounds, false);
        assert!(view.scroll_x > 100.0);
        let end = editor.raw_text().len();
        let caret = view.ime_area().unwrap();
        assert!(
            caret.x >= view.clip.x && caret.x + caret.width <= view.clip.x + view.clip.width + 0.01
        );
        view.move_to_point(&mut editor, CssPoint::new(caret.x, caret.y + 8.0), false);
        assert_eq!(editor.selection(), end..end);
        editor.handle_key(&key(Key::Home, true));
        view.update(&mut editor, bounds, false);
        assert_eq!(view.scroll_x, 0.0);
        assert_eq!(editor.selected_text().unwrap().len(), end);
        editor.handle_key(&key(Key::End, false));
        view.update(&mut editor, bounds, false);
        assert!(view.scroll_x > 100.0);
        editor.handle_key(&key(Key::Home, false));
        view.update(&mut editor, bounds, false);
        let cell = crate::text::shape("@", &terminal_text_style()).advance;
        view.move_to_point(
            &mut editor,
            CssPoint::new(view.origin.x + cell * 3.0, bounds.y + 13.0),
            false,
        );
        assert_eq!(editor.selection(), 3..3);
        view.move_to_point(
            &mut editor,
            CssPoint::new(view.origin.x + cell * 7.0, bounds.y + 13.0),
            true,
        );
        assert_eq!(editor.selection(), 3..7);
        editor.handle_key(&key(Key::End, false));
        view.update(&mut editor, CssRect::new(8.0, 40.0, 4000.0, 26.0), false);
        assert_eq!(view.scroll_x, 0.0);
    }

    #[test]
    fn terminal_input_masks_graphemes_without_leaking_their_geometry_and_retains_layout() {
        let mut editor = TextEditor::new("@ 界e\u{301}👩‍💻", &terminal_text_style(), 400.0, false);
        let mut view = TerminalInputView::default();
        let bounds = CssRect::new(0.0, 0.0, 400.0, 26.0);
        view.update(&mut editor, bounds, true);
        let line = view.line.clone().unwrap();
        assert_eq!(line.shaped.text, "•••••");
        assert!((line.caret.unwrap().x - line.shaped.advance).abs() < 0.01);
        editor.set_width(400.0);
        view.update(&mut editor, bounds, true);
        assert!(
            Arc::ptr_eq(&line, view.line.as_ref().unwrap()),
            "unchanged native frames reuse the layout"
        );
        let bullet = line.shaped.advance / 5.0;
        view.move_to_point(
            &mut editor,
            CssPoint::new(view.origin.x + bullet * 3.0, 13.0),
            false,
        );
        assert_eq!(editor.selection(), "@ 界".len().."@ 界".len());
        editor.handle_key(&key(Key::End, true));
        assert_eq!(editor.selected_text(), Some("e\u{301}👩‍💻"));
        view.update(&mut editor, bounds, true);
        let mut commands = Vec::new();
        view.paint(&mut commands, true);
        assert!(
            commands
                .iter()
                .filter_map(|command| match command {
                    DisplayCommand::GlyphRun { shaped, .. } => Some(&shaped.text),
                    _ => None,
                })
                .all(|text| text == "•••••")
        );
    }

    #[test]
    fn terminal_input_preedit_uses_the_same_geometry_and_unfocused_fields_hide_the_caret() {
        let mut editor = TextEditor::new("@ ", &terminal_text_style(), 400.0, false);
        editor.handle_ime(&crate::core::ImeAction::Preedit {
            text: "日本".into(),
            cursor: Some((6, 6)),
        });
        let mut view = TerminalInputView::default();
        view.update(&mut editor, CssRect::new(0.0, 0.0, 400.0, 26.0), false);
        assert_eq!(editor.text(), "@ ");
        let line = view.line.as_ref().unwrap();
        assert_eq!(line.shaped.text, "@ 日本");
        assert!(!line.underlines.is_empty());
        let mut active = Vec::new();
        let mut inactive = Vec::new();
        view.paint(&mut active, true);
        view.paint(&mut inactive, false);
        assert_eq!(active.len(), inactive.len() + 1);
    }

    #[test]
    fn terminal_input_raster_preview() {
        use crate::core::{CssSize, PhysicalSize, ScaleFactor, ViewportMetrics};
        use crate::render::{Scene, vello_cpu::VelloCpuRenderer};
        let mut hybrid = std::env::var_os("TRUST_TERMINAL_INPUT_HYBRID").map(|_| {
            let renderer = futures::executor::block_on(
                crate::render::vello_hybrid::VelloHybridRenderer::new_headless(),
            )
            .expect("requested Hybrid input-field verification");
            eprintln!("terminal input raster adapter: {}", renderer.adapter_name());
            renderer
        });
        for scale in [1.0, 1.5, 2.0] {
            let mut scene = Scene {
                viewport: ViewportMetrics::from_physical(
                    PhysicalSize::new((960.0 * scale) as u32, (220.0 * scale) as u32),
                    ScaleFactor::new(scale),
                ),
                primitives: vec![DisplayCommand::FillRect {
                    rect: CssRect::new(0.0, 0.0, 960.0, 220.0),
                    color: PaintColor::Rgba(12, 16, 23, 255),
                }],
                controls: Vec::new(),
                content_viewport: CssRect::new(0.0, 0.0, 960.0, 220.0),
                image_store: Default::default(),
                canvas_images: Default::default(),
                page_scroll_containers: Vec::new(),
                page_size: CssSize::new(960.0, 220.0),
            };
            let mut terminal = TerminalView::new(1, 100);
            terminal.process(b"Terminal font: centered @sym   multiple    spaces");
            scene.primitives.extend(terminal.paint().primitives.clone());
            for (index, text) in [
                "centered",
                "@sym",
                "fdsfs dsfdsf   sdfdsfdsf    sdfdsfdsfds   sdfdsffsdf",
                "selected @symbol and spaces  ",
                "long draft: ",
            ]
            .into_iter()
            .enumerate()
            {
                let text = if index == 4 {
                    format!("{text}{}", "keep typing @ here   ".repeat(12))
                } else {
                    text.into()
                };
                let mut editor = TextEditor::new(&text, &terminal_text_style(), 900.0, false);
                if index == 3 {
                    editor.select_all();
                }
                let mut view = TerminalInputView::default();
                view.update(
                    &mut editor,
                    CssRect::new(8.0, 35.0 + index as f32 * 36.0, 944.0, 26.0),
                    false,
                );
                view.paint(&mut scene.primitives, true);
            }
            let frame = VelloCpuRenderer::new().render_rgba(&scene).unwrap();
            assert!(
                frame
                    .pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200)
            );
            if let Some(path) = std::env::var_os("TRUST_TERMINAL_INPUT_SNAPSHOT") {
                image::save_buffer(
                    format!("{}-{scale}.png", path.to_string_lossy()),
                    &frame.pixels,
                    frame.size.width,
                    frame.size.height,
                    image::ColorType::Rgba8,
                )
                .unwrap();
            }
            if let Some(renderer) = &mut hybrid {
                let gpu = renderer.render_rgba(&scene).unwrap();
                assert_eq!(gpu.size, frame.size);
                let mismatches = frame
                    .pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(gpu.pixels.as_chunks::<4>().0)
                    .filter(|(cpu, gpu)| (0..3).any(|index| cpu[index].abs_diff(gpu[index]) > 20))
                    .count();
                assert!(
                    mismatches * 100 < frame.pixels.len() / 4,
                    "CPU/Hybrid input fields differ in {mismatches} pixels at scale {scale}"
                );
                if let Some(path) = std::env::var_os("TRUST_TERMINAL_INPUT_SNAPSHOT") {
                    image::save_buffer(
                        format!("{}-hybrid-{scale}.png", path.to_string_lossy()),
                        &gpu.pixels,
                        gpu.size.width,
                        gpu.size.height,
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                }
            }
        }
    }
}
