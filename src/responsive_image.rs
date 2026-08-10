//! HTML responsive-image source selection (`<picture>`, `srcset`, and
//! `sizes`).
//!
//! This follows WHATWG HTML §4.8.4.3.7–§4.8.4.3.12: build the source set in
//! picture-child order, parse candidates with the specified tokenizer,
//! resolve the source size, normalize width descriptors to pixel densities,
//! discard later duplicate densities, and make the user-agent choice from the
//! normalized set. CSS Syntax §5.4.10, CSS Values §6/§10, Media Queries §3,
//! and MIME Sniffing §4 provide the component-value, length, media-condition,
//! and supported-type definitions used by those HTML algorithms.

use url::Url;

use crate::dom::{Dom, NodeId};
use crate::layout2::Viewport;

/// One source selected for an `img` element.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedImage {
    /// Absolute URL serialization, as exposed by `HTMLImageElement.currentSrc`.
    pub source: String,
    /// The normalized pixel density. Decoded natural dimensions are divided by
    /// this value before they participate in CSS replaced-element sizing.
    pub density: f32,
    /// HTML's "dimension attribute source": a selected `<source>` can supply
    /// the presentational `width`/`height` hints for its following `<img>`.
    pub dimension_source: NodeId,
}

#[derive(Clone, Debug)]
struct Candidate {
    url: String,
    width: Option<u32>,
    density: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DescriptorState {
    InDescriptor,
    InParens,
    AfterDescriptor,
}

/// Select an image using the current viewport and device pixel ratio.
///
/// HTML intentionally leaves the final choice implementation-defined. TRust
/// uses the conventional quality-preserving policy: the least dense candidate
/// at or above the device density, otherwise the densest available candidate.
pub fn select(
    dom: &Dom,
    img: NodeId,
    page_url: &Url,
    viewport: Viewport,
    device_pixel_ratio: f32,
) -> Option<SelectedImage> {
    if dom.tag_name(img) != Some("img") {
        return None;
    }
    let base = document_base(dom, page_url);
    let (mut candidates, dimension_source) =
        update_source_set(dom, img, viewport, device_pixel_ratio);
    if candidates.is_empty() {
        return None;
    }

    // HTML §4.8.4.3.7 removes every later entry with the same normalized
    // density as an earlier one, preserving source-set order.
    let mut normalized = Vec::<(Candidate, f32)>::new();
    for candidate in candidates.drain(..) {
        let density = candidate.density.unwrap_or(1.0);
        if normalized.iter().any(|(_, seen)| *seen == density) {
            continue;
        }
        normalized.push((candidate, density));
    }
    let target = if device_pixel_ratio.is_finite() && device_pixel_ratio > 0.0 {
        device_pixel_ratio
    } else {
        1.0
    };
    normalized.sort_by(|a, b| a.1.total_cmp(&b.1));
    let (candidate, density) = normalized
        .iter()
        .find(|(_, density)| *density >= target)
        .or_else(|| normalized.last())?;
    let source = resolve_url(&base, &candidate.url)?;
    Some(SelectedImage {
        source,
        density: *density,
        dimension_source,
    })
}

/// Whether the selected URL can be consumed by TRust's image pipeline.
pub fn loadable_source(selected: &SelectedImage) -> bool {
    Url::parse(&selected.source)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https" | "data" | "blob"))
}

/// Density-correct decoded resource dimensions for CSS natural sizing.
pub fn density_corrected_size(raw: Option<&(u32, u32)>, density: f32) -> Option<(f32, f32)> {
    let &(width, height) = raw.filter(|&&(width, height)| width > 0 && height > 0)?;
    if (width, height) == (u32::MAX, u32::MAX) {
        return Some((300.0, 150.0));
    }
    if density <= 0.0 {
        // HTML notes that 0x implies infinite natural dimensions and expects
        // user agents to impose limits. Infinite geometry poisons transforms,
        // clips, and renderer allocations, so TRust's finite safety limit is
        // the decoded resource's own dimensions (the largest box this byte
        // resource would have produced without a density descriptor).
        return Some((width as f32, height as f32));
    }
    let density = if density.is_nan() { 1.0 } else { density };
    Some((width as f32 / density, height as f32 / density))
}

/// HTML §4.8.4.3.9, "update the source set".
fn update_source_set(
    dom: &Dom,
    img: NodeId,
    viewport: Viewport,
    device_pixel_ratio: f32,
) -> (Vec<Candidate>, NodeId) {
    if let Some(parent) = dom.node(img).parent
        && dom.tag_name(parent) == Some("picture")
    {
        // Only preceding siblings of this particular img participate. Invalid
        // children and another img in the same picture are ignored.
        for child in dom.child_iter(parent) {
            if child == img {
                break;
            }
            if dom.tag_name(child) != Some("source") {
                continue;
            }
            let Some(srcset) = dom.attr(child, "srcset") else {
                continue;
            };
            let mut candidates = parse_srcset(srcset);
            if candidates.is_empty() {
                continue;
            }
            if dom.attr(child, "media").is_some_and(|media| {
                !dom.media_matches_at_density(
                    media,
                    viewport.width,
                    viewport.height,
                    device_pixel_ratio,
                )
            }) {
                continue;
            }
            // HTML deliberately parses `sizes` before checking `type`; keep
            // the source-set state transition in normative order even though
            // neither operation currently has an author-observable side
            // effect in TRust.
            let source_size = parse_sizes(dom, child, Some(img), viewport, device_pixel_ratio);
            if dom
                .attr(child, "type")
                .is_some_and(|mime| !supported_image_mime(mime))
            {
                continue;
            }
            normalize_densities(&mut candidates, source_size);
            let dimension_source =
                if dom.attr(child, "width").is_some() || dom.attr(child, "height").is_some() {
                    child
                } else {
                    img
                };
            return (candidates, dimension_source);
        }
    }

    let default_source = dom.attr(img, "src").unwrap_or("");
    let srcset = dom.attr(img, "srcset").unwrap_or("");
    let mut candidates = if srcset.is_empty() {
        Vec::new()
    } else {
        parse_srcset(srcset)
    };
    let source_size = parse_sizes(dom, img, Some(img), viewport, device_pixel_ratio);
    // The default source is appended only when there is no 1x candidate and
    // no width descriptor. This is why a width-descriptor srcset must not
    // download a giant `src` fallback.
    if !default_source.is_empty()
        && !candidates
            .iter()
            .any(|candidate| candidate.density == Some(1.0))
        && !candidates.iter().any(|candidate| candidate.width.is_some())
    {
        candidates.push(Candidate {
            url: default_source.to_string(),
            width: None,
            density: None,
        });
    }
    normalize_densities(&mut candidates, source_size);
    (candidates, img)
}

/// HTML §4.8.4.3.10, "parse a srcset attribute".
fn parse_srcset(input: &str) -> Vec<Candidate> {
    let chars: Vec<char> = input.chars().collect();
    let mut position = 0;
    let mut candidates = Vec::new();
    loop {
        while position < chars.len()
            && (chars[position].is_ascii_whitespace() || chars[position] == ',')
        {
            position += 1;
        }
        if position >= chars.len() {
            return candidates;
        }

        let start = position;
        while position < chars.len() && !chars[position].is_ascii_whitespace() {
            position += 1;
        }
        let mut url: String = chars[start..position].iter().collect();
        let mut descriptors = Vec::new();
        if url.ends_with(',') {
            while url.ends_with(',') {
                url.pop();
            }
        } else {
            while position < chars.len() && chars[position].is_ascii_whitespace() {
                position += 1;
            }
            let mut current = String::new();
            let mut state = DescriptorState::InDescriptor;
            loop {
                let c = chars.get(position).copied();
                match state {
                    DescriptorState::InDescriptor => match c {
                        Some(c) if c.is_ascii_whitespace() => {
                            if !current.is_empty() {
                                descriptors.push(std::mem::take(&mut current));
                                state = DescriptorState::AfterDescriptor;
                            }
                        }
                        Some(',') => {
                            position += 1;
                            if !current.is_empty() {
                                descriptors.push(std::mem::take(&mut current));
                            }
                            break;
                        }
                        Some('(') => {
                            current.push('(');
                            state = DescriptorState::InParens;
                        }
                        None => {
                            if !current.is_empty() {
                                descriptors.push(current);
                            }
                            break;
                        }
                        Some(c) => current.push(c),
                    },
                    DescriptorState::InParens => match c {
                        Some(')') => {
                            current.push(')');
                            state = DescriptorState::InDescriptor;
                        }
                        None => {
                            descriptors.push(current);
                            break;
                        }
                        Some(c) => current.push(c),
                    },
                    DescriptorState::AfterDescriptor => match c {
                        Some(c) if c.is_ascii_whitespace() => {}
                        None => break,
                        Some(_) => {
                            state = DescriptorState::InDescriptor;
                            continue;
                        }
                    },
                }
                position += 1;
            }
        }

        let mut width = None;
        let mut density = None;
        let mut future_h = None;
        let mut error = url.is_empty();
        for descriptor in descriptors {
            if let Some(value) = descriptor.strip_suffix('w')
                && valid_non_negative_integer(value)
            {
                if width.is_some() || density.is_some() {
                    error = true;
                }
                match value.parse::<u32>() {
                    Ok(0) | Err(_) => error = true,
                    Ok(value) => width = Some(value),
                }
            } else if let Some(value) = descriptor.strip_suffix('x')
                && valid_floating_point(value)
            {
                if width.is_some() || density.is_some() || future_h.is_some() {
                    error = true;
                }
                match value.parse::<f32>() {
                    Ok(value) if value >= 0.0 && value.is_finite() => density = Some(value),
                    _ => error = true,
                }
            } else if let Some(value) = descriptor.strip_suffix('h')
                && valid_non_negative_integer(value)
            {
                // HTML reports this descriptor as a conformance parse error,
                // but deliberately retains a candidate carrying `w` + `h`
                // for forward compatibility. Only the algorithm's separate
                // internal `error` flag decides whether to discard it.
                if future_h.is_some() || density.is_some() {
                    error = true;
                }
                match value.parse::<u32>() {
                    Ok(0) | Err(_) => error = true,
                    Ok(value) => future_h = Some(value),
                }
            } else {
                error = true;
            }
        }
        if future_h.is_some() && width.is_none() {
            error = true;
        }
        if !error {
            candidates.push(Candidate {
                url,
                width,
                density,
            });
        }
    }
}

fn valid_non_negative_integer(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// HTML's valid floating-point-number syntax (without Infinity/NaN or a
/// leading `+`, neither of which Rust's generic parser should admit here).
fn valid_floating_point(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes[0] == b'+' {
        return false;
    }
    let mut i = usize::from(bytes[0] == b'-');
    let integer_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let integer_digits = i - integer_start;
    let mut fraction_digits = 0;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let fraction_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        fraction_digits = i - fraction_start;
        // HTML requires one or more digits after a present decimal point;
        // `1.` and `1.e2` are not valid floating-point-number strings.
        if fraction_digits == 0 {
            return false;
        }
    }
    if integer_digits == 0 && fraction_digits == 0 {
        return false;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exponent_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exponent_start {
            return false;
        }
    }
    i == bytes.len()
}

/// HTML §4.8.4.3.11, "parse a sizes attribute".
fn parse_sizes(
    dom: &Dom,
    element: NodeId,
    img: Option<NodeId>,
    viewport: Viewport,
    device_pixel_ratio: f32,
) -> f32 {
    // If the following img allows auto-sizes, an omitted `sizes` on a picture
    // source is equivalent to `auto` (HTML §4.8.2). This differs from an
    // explicitly empty attribute, which falls through to 100vw.
    let raw_sizes = match dom.attr(element, "sizes") {
        Some(value) => value,
        None if dom.tag_name(element) == Some("source")
            && img.is_some_and(|img| allows_auto_sizes(dom, img)) =>
        {
            "auto"
        }
        None => "",
    };
    let input = strip_css_comments(raw_sizes);
    let entries = split_component_list(&input);
    for (index, entry) in entries.iter().enumerate() {
        let entry = entry.trim_end();
        if entry.is_empty() {
            continue;
        }
        let Some((condition, value)) = trailing_component(entry) else {
            continue;
        };
        let value = value.trim();
        let size = if value.eq_ignore_ascii_case("auto") {
            img.and_then(|img| auto_source_size(dom, img, viewport))
        } else {
            crate::layout2::image_source_size_px(dom, element, value, viewport)
        };
        let Some(size) = size.filter(|size| *size >= 0.0) else {
            continue;
        };
        let condition = condition.trim_end();
        if condition.is_empty() {
            // A bare source size is accepted immediately; being non-final is
            // a conformance error, not a parser-recovery change.
            let _ = index;
            return size;
        }
        if dom.media_matches_at_density(
            condition,
            viewport.width,
            viewport.height,
            device_pixel_ratio,
        ) {
            return size;
        }
    }
    viewport.width.max(0.0) // the specified 100vw fallback
}

/// `sizes=auto` is permitted only for lazy images. At source-selection time a
/// declared CSS/HTML width is the concrete object width available without the
/// selected resource; otherwise `auto` remains unresolved and the next list
/// item is considered, exactly as the HTML algorithm requires.
fn auto_source_size(dom: &Dom, img: NodeId, viewport: Viewport) -> Option<f32> {
    if !allows_auto_sizes(dom, img) {
        return None;
    }
    if let Some(width) = dom
        .attr(img, "width")
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
    {
        return Some(width);
    }
    dom.computed_value(img, "width")
        .and_then(|value| crate::layout2::image_source_size_px(dom, img, &value, viewport))
}

/// HTML's exact `allows auto-sizes` predicate. Leading whitespace is
/// significant: the value must be `auto` or start with `auto,`.
fn allows_auto_sizes(dom: &Dom, img: NodeId) -> bool {
    let Some(sizes) = dom.attr(img, "sizes") else {
        return false;
    };
    dom.attr(img, "loading")
        .is_some_and(|value| value.eq_ignore_ascii_case("lazy"))
        && (sizes.eq_ignore_ascii_case("auto")
            || sizes
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("auto,")))
}

fn normalize_densities(candidates: &mut [Candidate], source_size: f32) {
    for candidate in candidates {
        if candidate.density.is_some() {
            continue;
        }
        candidate.density = Some(match candidate.width {
            Some(_) if source_size == 0.0 => f32::INFINITY,
            Some(width) => width as f32 / source_size,
            None => 1.0,
        });
    }
}

/// CSS Syntax's comma-separated component-value list boundary. Commas inside
/// functions, blocks, strings, and escaped text do not split an entry.
fn split_component_list(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut stack = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
            continue;
        }
        match ch {
            '(' | '[' | '{' => stack.push(ch),
            ')' | ']' | '}' => {
                stack.pop();
            }
            ',' if stack.is_empty() => {
                out.push(input[start..index].to_string());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    out.push(input[start..].to_string());
    out
}

/// Split one sizes entry into its optional media condition and final CSS
/// component value. A function is one component even when it contains spaces.
fn trailing_component(entry: &str) -> Option<(&str, &str)> {
    let end = entry.trim_end().len();
    if end == 0 {
        return None;
    }
    let bytes = entry.as_bytes();
    if bytes[end - 1] == b')' {
        let mut depth = 0usize;
        let mut quote = None;
        let mut escaped = false;
        for (index, ch) in entry[..end].char_indices().rev() {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                }
                continue;
            }
            if matches!(ch, '\'' | '"') {
                quote = Some(ch);
                continue;
            }
            match ch {
                ')' => depth += 1,
                '(' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let mut start = index;
                        while start > 0 {
                            let c = entry[..start].chars().next_back().unwrap();
                            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                                start -= c.len_utf8();
                            } else {
                                break;
                            }
                        }
                        return Some((&entry[..start], &entry[start..end]));
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    let start = entry[..end].rfind(char::is_whitespace).map_or(0, |index| {
        index + entry[index..].chars().next().unwrap().len_utf8()
    });
    Some((&entry[..start], &entry[start..end]))
}

fn strip_css_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes.get(index..index + 2) == Some(b"/*") {
            out.push(' ');
            index += 2;
            while index < bytes.len() && bytes.get(index..index + 2) != Some(b"*/") {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
        } else {
            let ch = input[index..].chars().next().unwrap();
            out.push(ch);
            index += ch.len_utf8();
        }
    }
    out
}

fn supported_image_mime(input: &str) -> bool {
    let essence = input
        .trim_matches(|ch: char| matches!(ch, ' ' | '\t' | '\n' | '\r'))
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/svg+xml"
    )
}

fn document_base(dom: &Dom, page_url: &Url) -> Url {
    dom.descendants(crate::dom::DOCUMENT)
        .find_map(|id| {
            (dom.tag_name(id) == Some("base"))
                .then(|| dom.attr(id, "href"))
                .flatten()
                .and_then(|href| page_url.join(href.trim()).ok())
        })
        .unwrap_or_else(|| page_url.clone())
}

fn resolve_url(base: &Url, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    base.join(raw)
        .or_else(|_| Url::parse(raw))
        .ok()
        .map(|url| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dom(html: &str) -> Dom {
        Dom::parse_document(html)
    }

    fn select_at(html: &str, width: f32, dpr: f32) -> SelectedImage {
        let mut dom = dom(html);
        dom.set_viewport_px(width, 600.0);
        let img = dom
            .descendants(crate::dom::DOCUMENT)
            .find(|&id| dom.tag_name(id) == Some("img"))
            .unwrap();
        select(
            &dom,
            img,
            &Url::parse("https://example.test/path/page").unwrap(),
            Viewport::new(width, 600.0),
            dpr,
        )
        .unwrap()
    }

    #[test]
    fn width_descriptors_ignore_giant_src_and_use_sizes() {
        let selected = select_at(
            r#"<img src="huge-3840.webp" srcset="small.webp 640w, medium.webp 960w, huge-3840.webp 3840w" sizes="50vw">"#,
            1000.0,
            1.0,
        );
        assert_eq!(selected.source, "https://example.test/path/small.webp");
        assert_eq!(selected.density, 1.28);
    }

    #[test]
    fn density_selection_uses_device_scale_and_first_duplicate() {
        let selected = select_at(
            r#"<img src="fallback.png" srcset="one.png, first-two.png 2x, ignored-two.png 2x, three.png 3x">"#,
            800.0,
            1.5,
        );
        assert_eq!(selected.source, "https://example.test/path/first-two.png");
        assert_eq!(selected.density, 2.0);
    }

    #[test]
    fn picture_uses_first_matching_supported_source_and_dimensions() {
        let html = r#"<picture>
            <source media="(max-width: 700px)" type="image/avif" srcset="unsupported.avif 1x">
            <source media="(max-width: 700px)" type="image/webp; codecs=vp8" srcset="narrow.webp 1x" width="320" height="200">
            <source srcset="wide.webp 1x">
            <img src="fallback.jpg" width="900" height="600">
        </picture>"#;
        let mut dom = dom(html);
        dom.set_viewport_px(600.0, 600.0);
        let img = dom
            .descendants(crate::dom::DOCUMENT)
            .find(|&id| dom.tag_name(id) == Some("img"))
            .unwrap();
        let selected = select(
            &dom,
            img,
            &Url::parse("https://example.test/").unwrap(),
            Viewport::new(600.0, 600.0),
            1.0,
        )
        .unwrap();
        assert_eq!(selected.source, "https://example.test/narrow.webp");
        assert_eq!(dom.tag_name(selected.dimension_source), Some("source"));
    }

    #[test]
    fn srcset_tokenizer_keeps_data_url_commas_and_drops_bad_descriptors() {
        let candidates =
            parse_srcset("data:image/svg+xml,%3Csvg%3E 1x, bad.png 1x 2x, good.png 800w");
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].url.starts_with("data:image/svg+xml,"));
        assert_eq!(candidates[1].width, Some(800));
    }

    #[test]
    fn future_compat_height_descriptor_keeps_candidate_only_with_width() {
        let candidates = parse_srcset("kept.png 800w 600h, missing-width.png 600h");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].url, "kept.png");
        assert_eq!(candidates[0].width, Some(800));
    }

    #[test]
    fn density_descriptor_uses_html_floating_point_syntax() {
        let candidates = parse_srcset("bad-dot.png 1.x, bad-exp.png 1.e2x, good.png .5x");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].url, "good.png");
        assert_eq!(candidates[0].density, Some(0.5));
    }

    #[test]
    fn unterminated_parenthesized_descriptor_is_processed_once() {
        assert!(parse_srcset("bad.png future(foo").is_empty());
    }

    #[test]
    fn omitted_picture_source_sizes_uses_lazy_img_auto_width() {
        let selected = select_at(
            r#"<picture><source srcset="small.webp 320w, large.webp 1280w"><img loading="lazy" sizes="auto" width="320" src="fallback.webp"></picture>"#,
            1200.0,
            1.0,
        );
        assert_eq!(selected.source, "https://example.test/path/small.webp");
        assert_eq!(selected.density, 1.0);
    }

    #[test]
    fn leading_whitespace_prevents_auto_sizes_state() {
        let selected = select_at(
            r#"<img loading="lazy" sizes=" auto, 320px" width="800" srcset="small.webp 320w, large.webp 1280w">"#,
            1200.0,
            1.0,
        );
        // The first entry's `auto` is ignored; the explicit fallback wins.
        assert_eq!(selected.source, "https://example.test/path/small.webp");
    }

    #[test]
    fn sizes_uses_first_matching_condition_and_math_length() {
        let selected = select_at(
            r#"<img srcset="a.png 400w, b.png 800w, c.png 1200w" sizes="(max-width: 700px) calc(100vw - 40px), 50vw">"#,
            600.0,
            1.0,
        );
        assert_eq!(selected.source, "https://example.test/path/b.png");
        assert!((selected.density - (800.0 / 560.0)).abs() < 0.001);
    }

    #[test]
    fn invalid_sizes_falls_back_to_100vw() {
        let selected = select_at(
            r#"<img srcset="a.png 400w, b.png 800w" sizes="(min-width: 1px) 50%">"#,
            600.0,
            1.0,
        );
        assert_eq!(selected.source, "https://example.test/path/b.png");
        assert!((selected.density - 800.0 / 600.0).abs() < 0.001);
    }
}
