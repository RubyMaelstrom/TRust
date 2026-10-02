//! CSS Pseudo 4 `::first-letter` (#first-letter-pseudo): finding a block
//! container's first-letter text and wrapping it in the pseudo-element's box.

use std::ops::Range;
use std::sync::Arc;

use icu_properties::CodePointMapData;
use icu_properties::props::{GeneralCategory, GeneralCategoryGroup};
use unicode_segmentation::UnicodeSegmentation;

use super::is_collapsible_space;
use super::tree::{Content, Inline};
use crate::dom::PseudoEl;

/// Where a text run's first-letter text is, if it has one.
#[derive(Debug, PartialEq)]
pub(super) enum Scan {
    /// The byte range of the first-letter text.
    Letter(Range<usize>),
    /// Only collapsible white space: the search continues past the run.
    Blank,
    /// Other content comes first, so there is no first letter.
    None,
}

/// CSS Pseudo 4 #first-letter-pattern, over typographic character units
/// (extended grapheme clusters): preceding punctuation with intervening
/// typographic space, the first Letter, Number or Symbol, then following
/// punctuation other than Ps/Pd with intervening typographic space other
/// than word separators, e.g. `“A”`, `O’` of `O’er`, `I.` of `I. Smith`.
pub(super) fn scan(text: &str) -> Scan {
    let start = text.len() - text.trim_start_matches(is_collapsible_space).len();
    let rest = &text[start..];
    if rest.is_empty() {
        return Scan::Blank;
    }
    let mut units = rest.grapheme_indices(true);
    let mut after_punctuation = false;
    let letter_end = loop {
        let Some((at, unit)) = units.next() else {
            return Scan::None;
        };
        let category = category(unit);
        if GeneralCategoryGroup::Letter.contains(category)
            || GeneralCategoryGroup::Number.contains(category)
            || GeneralCategoryGroup::Symbol.contains(category)
        {
            break at + unit.len();
        }
        if GeneralCategoryGroup::Punctuation.contains(category) {
            after_punctuation = true;
        } else if !(after_punctuation && typographic_space(unit, category)) {
            return Scan::None;
        }
    };
    let mut end = letter_end;
    for (at, unit) in units {
        let category = category(unit);
        if GeneralCategoryGroup::Punctuation.contains(category)
            && !matches!(
                category,
                GeneralCategory::OpenPunctuation | GeneralCategory::DashPunctuation
            )
        {
            end = at + unit.len();
        } else if !typographic_space(unit, category) || word_separator(unit) {
            break;
        }
    }
    Scan::Letter(start..start + end)
}

fn category(unit: &str) -> GeneralCategory {
    let first = unit.chars().next().expect("grapheme clusters are nonempty");
    CodePointMapData::<GeneralCategory>::new().get(first)
}

/// `Zs` other than U+3000 IDEOGRAPHIC SPACE.
fn typographic_space(unit: &str, category: GeneralCategory) -> bool {
    category == GeneralCategory::SpaceSeparator && unit != "\u{3000}"
}

/// CSS Text 3 #word-separator.
fn word_separator(unit: &str) -> bool {
    matches!(
        unit,
        " " | "\u{a0}" | "\u{1361}" | "\u{10100}" | "\u{10101}" | "\u{1039f}" | "\u{1091f}"
    )
}

/// Whether the search for the first-letter text is over.
#[derive(Debug, PartialEq)]
pub(super) enum Search {
    Wrapped,
    /// Content precedes any letter on the first formatted line.
    Blocked,
    /// Nothing on the line yet: keep looking in later content.
    Continue,
}

/// CSS Pseudo 4 #first-letter-application / #first-letter-tree: wrap the
/// first-letter text of `content`'s first formatted line, descending into
/// in-flow block containers and inline boxes, so the pseudo-element sits
/// immediately around the text and inherits from the innermost inline box.
/// A descendant's own `::first-letter` box nests inside the ancestor's.
pub(super) fn wrap(content: &mut Content, wrap: &mut dyn FnMut(Vec<Inline>) -> Inline) -> Search {
    match content {
        Content::Inlines(inlines) => wrap_inlines(inlines, wrap),
        Content::Blocks(blocks) => {
            for block in blocks {
                // Only block containers in this flow share the first line.
                if !matches!(block.content, Content::Inlines(_) | Content::Blocks(_)) {
                    return Search::Blocked;
                }
                match self::wrap(&mut Arc::make_mut(block).content, wrap) {
                    Search::Continue => {}
                    done => return done,
                }
            }
            Search::Continue
        }
        _ => Search::Blocked,
    }
}

fn wrap_inlines(inlines: &mut Vec<Inline>, wrap: &mut dyn FnMut(Vec<Inline>) -> Inline) -> Search {
    for index in 0..inlines.len() {
        match &mut inlines[index] {
            Inline::Text(text) => match scan(text) {
                Scan::Letter(range) => {
                    let after = text[range.end..].to_string();
                    let letter = text[range.clone()].to_string();
                    text.truncate(range.start);
                    let mut replacement = Vec::with_capacity(3);
                    if !text.is_empty() {
                        replacement.push(Inline::Text(std::mem::take(text)));
                    }
                    replacement.push(wrap(vec![Inline::Text(letter)]));
                    if !after.is_empty() {
                        replacement.push(Inline::Text(after));
                    }
                    inlines.splice(index..=index, replacement);
                    return Search::Wrapped;
                }
                Scan::Blank => {}
                Scan::None => return Search::Blocked,
            },
            inline if is_first_letter(inline) => {
                let inner = inlines[index].clone();
                inlines[index] = wrap(vec![inner]);
                return Search::Wrapped;
            }
            Inline::Box { kids, .. } => {
                let mut list = kids.to_vec();
                match wrap_inlines(&mut list, wrap) {
                    Search::Wrapped => {
                        *kids = list.into();
                        return Search::Wrapped;
                    }
                    Search::Blocked => return Search::Blocked,
                    Search::Continue => {}
                }
            }
            Inline::Float(_) | Inline::OutOfFlow(_) => {}
            Inline::Atom(_) | Inline::AtomBox(_) | Inline::Br => return Search::Blocked,
        }
    }
    Search::Continue
}

/// A descendant block's `::first-letter` box, inline or floated.
fn is_first_letter(inline: &Inline) -> bool {
    let style = match inline {
        Inline::Box { style, .. } => &**style,
        Inline::Float(float) => &float.style,
        _ => return false,
    };
    style
        .pseudo
        .is_some_and(|(_, which)| which == PseudoEl::FirstLetter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letter(text: &str) -> Option<&str> {
        match scan(text) {
            Scan::Letter(range) => Some(&text[range]),
            _ => None,
        }
    }

    #[test]
    fn first_letter_text_follows_the_punctuation_pattern() {
        // CSS Pseudo 4 #first-letter-pattern.
        assert_eq!(letter("O’er the strange woods"), Some("O’"));
        assert_eq!(letter("  \n Hello"), Some("H"));
        assert_eq!(letter("“A” is first"), Some("“A”"));
        assert_eq!(letter("« Bonjour"), Some("« B"));
        assert_eq!(letter("I. Smith"), Some("I."));
        assert_eq!(letter("67 million"), Some("6"));
        assert_eq!(letter("a-b"), Some("a"), "dashes do not follow");
        assert_eq!(letter("a(b)"), Some("a"), "nor does opening punctuation");
        assert_eq!(
            letter("e\u{301}tude"),
            Some("e\u{301}"),
            "a grapheme cluster"
        );
        assert_eq!(scan("   "), Scan::Blank);
        assert_eq!(
            scan("\u{a0}x"),
            Scan::None,
            "space without punctuation first"
        );
        assert_eq!(scan("“"), Scan::None);
    }
}
