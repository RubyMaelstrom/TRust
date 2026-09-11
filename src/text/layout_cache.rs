//! CSS Text 3 #line-break-details / #word-break-shaping: retain complete
//! paragraph results at identical shaping and line-breaking inputs. Never
//! splice independently shaped prefixes into a changing Arabic/bidi run.
//! Completed paragraphs can survive streaming updates elsewhere in the page.

use super::*;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 8192;

#[derive(Clone, PartialEq, Eq, Hash)]
enum Operation {
    First(u32),
    Lines(u32, u32),
    Widths,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    shape: ShapeKey,
    breaks: TextBreakStyle,
    operation: Operation,
}

impl Key {
    fn new(text: &str, style: &TextStyle, breaks: TextBreakStyle, operation: Operation) -> Self {
        Self {
            shape: ShapeKey {
                text: text.into(),
                style: style.into(),
                quantize: true,
            },
            breaks,
            operation,
        }
    }
}

#[derive(Clone)]
enum Value {
    First(usize),
    Lines(Vec<ShapedText>),
    Widths((f32, f32)),
}

#[derive(Default)]
pub(super) struct Cache {
    entries: HashMap<Key, (Value, usize)>,
    order: VecDeque<Key>,
    bytes: usize,
}

impl Cache {
    fn insert(&mut self, key: Key, value: Value) {
        let bytes = 2
            * (std::mem::size_of::<Key>()
                + key.shape.text.capacity()
                + key.shape.style.family.capacity()
                + key
                    .shape
                    .style
                    .language
                    .as_ref()
                    .map_or(0, String::capacity))
            + std::mem::size_of::<(Value, usize)>()
            + match &value {
                Value::Lines(lines) => {
                    lines.capacity() * std::mem::size_of::<ShapedText>()
                        + lines
                            .iter()
                            .map(|line| shape_cost(&key.shape, line))
                            .sum::<usize>()
                }
                _ => 0,
            };
        if bytes > MAX_BYTES / 4 {
            return;
        }
        self.bytes += bytes;
        self.order.push_back(key.clone());
        self.entries.insert(key, (value, bytes));
        while self.bytes > MAX_BYTES || self.entries.len() > MAX_ENTRIES {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            if let Some((_, bytes)) = self.entries.remove(&key) {
                self.bytes -= bytes;
            }
        }
    }
}

impl TextSystem {
    pub(super) fn first_line_end(
        &mut self,
        text: &str,
        style: &TextStyle,
        width: f32,
        breaks: TextBreakStyle,
    ) -> usize {
        self.refresh_page_fonts();
        let key = Key::new(text, style, breaks, Operation::First(width.to_bits()));
        if let Some((Value::First(value), _)) = self.layout_cache.entries.get(&key) {
            return *value;
        }
        let value = self.first_line_end_uncached(text, style, width, breaks);
        self.layout_cache.insert(key, Value::First(value));
        value
    }

    pub(super) fn wrapped_lines(
        &mut self,
        text: &str,
        style: &TextStyle,
        first: f32,
        width: f32,
        breaks: TextBreakStyle,
    ) -> Vec<ShapedText> {
        self.refresh_page_fonts();
        let key = Key::new(
            text,
            style,
            breaks,
            Operation::Lines(first.to_bits(), width.to_bits()),
        );
        if let Some((Value::Lines(value), _)) = self.layout_cache.entries.get(&key) {
            return value.clone();
        }
        let value = self.wrapped_lines_uncached(text, style, first, width, breaks);
        self.layout_cache.insert(key, Value::Lines(value.clone()));
        value
    }

    pub(super) fn content_widths(
        &mut self,
        text: &str,
        style: &TextStyle,
        breaks: TextBreakStyle,
    ) -> (f32, f32) {
        self.refresh_page_fonts();
        let key = Key::new(text, style, breaks, Operation::Widths);
        if let Some((Value::Widths(value), _)) = self.layout_cache.entries.get(&key) {
            return *value;
        }
        let value = self.content_widths_uncached(text, style, breaks);
        self.layout_cache.insert(key, Value::Widths(value));
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_paragraph_layout_reuses_work_and_matches_fresh_shaping() {
        let mut system = TextSystem::new();
        let text = "Mixed العربية text, emoji 👩🏽‍💻 and preserved\nnewlines. ".repeat(12);
        for width in [83., 220.] {
            for breaks in [
                TextBreakStyle {
                    wrap: true,
                    ..Default::default()
                },
                TextBreakStyle {
                    wrap: true,
                    word_break: TextWordBreak::BreakAll,
                    overflow_wrap: TextOverflowWrap::Anywhere,
                },
            ] {
                for size in [16., 21.] {
                    let style = TextStyle {
                        size,
                        ..Default::default()
                    };
                    let fresh =
                        system.wrapped_lines_uncached(&text, &style, width / 2., width, breaks);
                    assert_eq!(
                        system.wrapped_lines(&text, &style, width / 2., width, breaks),
                        fresh
                    );
                    let shaped = system.shaped_input_bytes;
                    assert_eq!(
                        system.wrapped_lines(&text, &style, width / 2., width, breaks),
                        fresh
                    );
                    assert_eq!(
                        system.shaped_input_bytes, shaped,
                        "completed paragraph was reshaped"
                    );
                    assert_eq!(
                        system.first_line_end(&text, &style, width, breaks),
                        system.first_line_end_uncached(&text, &style, width, breaks)
                    );
                    assert_eq!(
                        system.content_widths(&text, &style, breaks),
                        system.content_widths_uncached(&text, &style, breaks)
                    );
                }
            }
        }
        system.page_font_epoch = system.page_font_epoch.wrapping_sub(1);
        system.refresh_page_fonts();
        assert!(
            system.layout_cache.entries.is_empty(),
            "font loads expire every shaping input"
        );
    }

    #[test]
    fn streaming_text_cache_has_a_strict_ownership_bound() {
        let mut cache = Cache::default();
        for i in 0..MAX_ENTRIES * 2 {
            cache.insert(
                Key::new(
                    &format!("{i} {}", "text ".repeat(300)),
                    &TextStyle::default(),
                    Default::default(),
                    Operation::Widths,
                ),
                Value::Widths((1., 2.)),
            );
        }
        assert!(cache.bytes <= MAX_BYTES);
        assert!(cache.entries.len() <= MAX_ENTRIES);
        assert_eq!(cache.entries.len(), cache.order.len());
    }
}
