// Copyright 2026 the TRust Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! CSS Text transforms change glyphs without changing SVG 2's addressable
//! character indices (text.html#TermAddressableCharacter). Shape the converted
//! text, then map its clusters back before positioning and DOM geometry.

use crate::{FontResolver, TextChunk};
use std::ops::Range;

/// One span's original text and inherited CSS transformation settings.
#[derive(Debug)]
pub struct TextTransformInput<'a> {
    /// Whitespace-processed original text.
    pub text: &'a str,
    /// Computed text-transform value.
    pub transform: &'a str,
    /// Inherited language, if any.
    pub language: Option<&'a str>,
    /// Whether spaces are preserved.
    pub preserve_spaces: bool,
    /// Whether the original Text node contains exactly one character.
    pub single_text_node: bool,
}

/// A transformed span, mapped to its original UTF-8 bytes.
#[derive(Debug)]
pub struct TextTransformOutput {
    /// Text to shape.
    pub text: String,
    /// Ordered transformed byte ranges and their original byte ranges.
    /// Repeated source ranges keep expansions in one typographic character.
    pub mappings: Vec<(Range<usize>, Range<usize>)>,
}

#[derive(Clone, Debug)]
pub(crate) struct ShapingText {
    pub text: String,
    pub spans: Vec<Range<usize>>,
    pub mappings: Vec<(Range<usize>, Range<usize>)>,
}

impl ShapingText {
    pub fn source_range(&self, range: Range<usize>) -> Range<usize> {
        let end = range.end.max(range.start + 1);
        let first = self
            .mappings
            .partition_point(|(laid, _)| laid.end <= range.start);
        let mut result = self
            .mappings
            .get(first)
            .map_or(0..0, |(_, source)| source.clone());
        for (laid, source) in &self.mappings[first..] {
            if laid.start >= end && !(laid.is_empty() && laid.start == end) {
                break;
            }
            result.start = result.start.min(source.start);
            result.end = result.end.max(source.end);
        }
        result
    }
}

pub(crate) fn prepare(chunks: &mut [TextChunk], resolver: &FontResolver) {
    let Some(transform) = &resolver.text_transform else {
        return;
    };
    if !chunks
        .iter()
        .flat_map(|chunk| &chunk.spans)
        .any(|span| span.text_transform != "none")
    {
        return;
    }
    let inputs: Vec<_> = chunks
        .iter()
        .flat_map(|chunk| {
            chunk.spans.iter().map(|span| TextTransformInput {
                text: &chunk.text[span.start..span.end],
                transform: &span.text_transform,
                language: span.language.as_deref(),
                preserve_spaces: span.preserve_spaces,
                single_text_node: span.single_text_node,
            })
        })
        .collect();
    let outputs = transform(&inputs);
    if outputs.len() != inputs.len() {
        return;
    }
    drop(inputs);
    let mut outputs = outputs.into_iter();
    for chunk in chunks {
        let mut shaping = ShapingText {
            text: String::new(),
            spans: Vec::new(),
            mappings: Vec::new(),
        };
        for span in &chunk.spans {
            let output = outputs.next().unwrap();
            let start = shaping.text.len();
            shaping.text.push_str(&output.text);
            shaping.spans.push(start..shaping.text.len());
            shaping
                .mappings
                .extend(output.mappings.into_iter().map(|(laid, source)| {
                    (
                        start + laid.start..start + laid.end,
                        span.start + source.start..span.start + source.end,
                    )
                }));
        }
        chunk.shaping = Some(shaping);
    }
}
