//! Window-free, scriptable page dump: navigate through the shared browser
//! controller, wait for the page to stop changing, and print its text.
//!
//! This is deliberately NOT a third frontend with its own pipeline. It drives
//! the same [`BrowserController`](trust::core::BrowserController) the terminal
//! and desktop frontends share — navigation, fetch, the resident JavaScript
//! actor, and the one CSS-pixel layout product — and then reads the
//! renderer-neutral display list those frontends paint from. Nothing here
//! initializes Ratatui, Crossterm, winit, or a terminal graphics protocol, so
//! it runs with no TTY at all.
//!
//! Completion is answered in two tiers. The engine knows exactly when it is
//! done with a document: a fetch that commits without a resident actor can
//! never render again, and an actor that classifies its document inert sends
//! `PageEvt::Static` and retires itself. The controller's
//! [`page_render_is_final`](trust::core::BrowserController::page_render_is_final)
//! verdict, and the driver trusts it outright, so a script-free page stops the
//! moment its render lands. Only a page that keeps its actor open — timers,
//! workers, hover, in-flight fetches — can refuse to answer, and for that page
//! `--settle` is a quiet-period fallback with the imprecision that implies.
//! `--timeout` is the hard wall-clock bound either way.
//!
//! Geometry: layout and the CSSOM viewport are reported against the synthetic
//! `--width`/`--height` viewport exactly as the desktop frontend reports them
//! against its window. The terminal cell model does not exist here, so the
//! 1x1-cell desktop adapter is the only meaningful mapping.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::time::{Duration, Instant};

use trust::accessibility::SemanticTree;
use trust::core::{BrowserController, CssSize, UserAction};
use trust::doc::Doc;
use trust::render::{DisplayCommand, PagePaint};

/// One positioned text run lifted out of the display list.
struct Run {
    y: f32,
    x: f32,
    width: f32,
    height: f32,
    text: String,
    link: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    /// Visual-order text read from the display list.
    Text,
    /// The accessible role/name/bounds tree.
    Semantic,
}

/// How the driver decided to stop waiting, reported on every run so the
/// difference between an engine verdict and a clock guess stays visible.
#[derive(PartialEq, Eq)]
enum Settle {
    /// The engine says no further render will arrive for this document.
    Engine,
    /// The page kept its actor open and merely stopped changing.
    Quiet,
    /// The caller's patience ran out.
    Timeout,
}

struct Options {
    address: String,
    width: f32,
    height: f32,
    timeout: Duration,
    settle: Duration,
    format: Format,
    list_links: bool,
}

const USAGE: &str = "\
usage: trust-headless [options] URL

  --width N       CSS viewport width in CSS pixels (default 1024)
  --height N      CSS viewport height in CSS pixels (default 768)
  --timeout SECS  stop waiting after this long (default 30)
  --settle SECS   for a page that never goes inert, stop once it has been this
                  long quiet (default 0.75; 0 waits for the engine verdict only)
  --format F      text (default) or semantic
  --links         also list every linked target found in the page
  -h, --help      show this message
";

fn parse_args() -> Result<Option<Options>, String> {
    let mut address: Option<String> = None;
    let mut width = 1024.0f32;
    let mut height = 768.0f32;
    let mut timeout = 30.0f32;
    let mut settle = 0.75f32;
    let mut format = Format::Text;
    let mut list_links = false;

    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "--width" => width = number("--width", &mut args)?,
            "--height" => height = number("--height", &mut args)?,
            "--timeout" => timeout = number("--timeout", &mut args)?,
            "--settle" => settle = number("--settle", &mut args)?,
            "--format" => {
                format = match string("--format", &mut args)?.as_str() {
                    "text" => Format::Text,
                    "semantic" => Format::Semantic,
                    other => {
                        return Err(format!(
                            "unknown format {other:?} (expected text or semantic)"
                        ));
                    }
                };
            }
            "--links" => list_links = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown option {other} (try --help)"));
            }
            _ if address.is_none() => address = Some(argument),
            _ => return Err(String::from("more than one URL was given")),
        }
    }

    let Some(address) = address else {
        return Err(String::from("a URL is required (try --help)"));
    };
    Ok(Some(Options {
        address,
        width: positive(width, "the viewport")?,
        height: positive(height, "the viewport")?,
        timeout: Duration::from_secs_f32(timeout.max(0.05)),
        settle: Duration::from_secs_f32(settle.max(0.0)),
        format,
        list_links,
    }))
}

fn positive(value: f32, what: &str) -> Result<f32, String> {
    if !value.is_finite() || value < 1.0 {
        return Err(format!("{what} must be at least one CSS pixel"));
    }
    Ok(value)
}

fn number(option: &str, args: &mut impl Iterator<Item = String>) -> Result<f32, String> {
    let raw = string(option, args)?;
    raw.parse::<f32>()
        .map_err(|_| format!("{option} needs a number, got {raw}"))
}

fn string(option: &str, args: &mut impl Iterator<Item = String>) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn main() {
    let options = match parse_args() {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return;
        }
        Err(error) => {
            eprintln!("trust-headless: {error}");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    };

    let failure = match try_run(options) {
        Ok(()) => None,
        Err(error) => Some(error.to_string()),
    };
    // Navigation boundaries are where the allocator's arenas get their largest
    // one-off reclaim, and this process exits after exactly one of them.
    trust::release_allocator_memory();
    if let Some(error) = failure {
        eprintln!("trust-headless: {error}");
        std::process::exit(1);
    }
}

fn try_run(options: Options) -> Result<(), Box<dyn Error>> {
    // Multi-thread like the desktop frontend: the controller spawns fetch,
    // image, and resident-actor work on this handle and reports back over its
    // channel, so the pump loop must let those tasks advance.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("trust-headless-net")
        .build()?;
    runtime.block_on(navigate_and_settle(&options))
}

/// Drive one navigation to a settled page and print it.
async fn navigate_and_settle(options: &Options) -> Result<(), Box<dyn Error>> {
    let mut controller = BrowserController::new(
        tokio::runtime::Handle::current(),
        || {},
        CssSize::new(options.width, options.height),
    );

    controller.handle_action(UserAction::Navigate(options.address.clone()));

    let started = Instant::now();
    let mut last_change = started;
    let mut revision = 0u64;
    let mut settled = Settle::Timeout;
    loop {
        let outcome = controller.process_async_events();
        let snapshot = controller.snapshot();
        let now = Instant::now();
        if outcome.invalidated || snapshot.page_revision != revision {
            revision = snapshot.page_revision;
            last_change = now;
        }
        let elapsed = now - started;
        if elapsed >= options.timeout {
            break;
        }
        // Prefer the engine's own verdict: a document is final once its fetch
        // committed without an actor, or once that actor classified the document
        // inert and retired itself. Nothing is guessed from a clock in that case,
        // so a script-free page never pays the settle window.
        if controller.page_render_is_final() {
            settled = Settle::Engine;
            break;
        }
        // A page that keeps its actor open (timers, workers, hover, fetches)
        // never goes final, so fall back to a quiet period and accept that it
        // only means "stopped changing", not "done".
        if options.settle > Duration::ZERO
            && !snapshot.loading
            && snapshot.page_revision > 0
            && now - last_change >= options.settle
        {
            settled = Settle::Quiet;
            break;
        }
        // Yield so the fetch, image, and actor tasks can run between drains.
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let snapshot = controller.snapshot();
    let Some(page) = controller.current_page() else {
        return Err(format!("no page loaded: {}", snapshot.status).into());
    };

    // A line-model protocol document has no layout product; fall back to the
    // protocol-neutral Doc the desktop adapter parses for it.
    let rendered = page.rendered_page();
    let fallback: Option<Doc> = match rendered {
        None => trust::render::documents::document(page),
        Some(_) => None,
    };

    let mut links: Vec<String> = Vec::new();
    let body = match (rendered, fallback.as_ref()) {
        (Some(rendered), _) if options.format == Format::Semantic => {
            semantic_text(&rendered.semantics, &mut links)
        }
        (Some(rendered), _) => {
            let runs = collect_runs(&rendered.layout.paint);
            if options.list_links {
                links_of(&runs, &mut links);
            }
            lines_of_runs(&runs).join("\n")
        }
        (None, Some(doc)) if doc.lines.is_empty() => {
            return Err(format!("page {} produced no readable text", page.address()).into());
        }
        (None, Some(doc)) => doc
            .lines
            .iter()
            .map(|line| {
                let text = line.text.trim_end();
                match &line.link {
                    Some(link) if options.list_links => format!("{text}\t{link}"),
                    _ => text.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        (None, None) => {
            return Err(format!(
                "no readable presentation for {}: {}",
                page.address(),
                snapshot.status
            )
            .into());
        }
    };

    let waited = started.elapsed();
    let how = match settled {
        Settle::Engine => String::from("the engine reports this render is final"),
        Settle::Quiet => format!(
            "quiet for {}s while the page kept its actor open",
            options.settle.as_secs_f32()
        ),
        Settle::Timeout => format!(
            "timed out after {}s without the engine calling it final",
            options.timeout.as_secs_f32()
        ),
    };
    eprintln!(
        "trust-headless: {} · {} · {}x{} CSS px · {} in {}ms ({})",
        page.address(),
        snapshot.status,
        options.width as u32,
        options.height as u32,
        how,
        waited.as_millis(),
        if settled == Settle::Timeout {
            "INCOMPLETE"
        } else {
            "ok"
        }
    );
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    use std::io::Write;
    let _ = out.write_all(body.as_bytes());
    if !body.is_empty() {
        let _ = out.write_all(b"\n");
    }
    if options.list_links && !links.is_empty() {
        let _ = out.write_all(b"\n--- links ---\n");
        for link in links {
            let _ = writeln!(out, "{link}");
        }
    }
    let _ = out.flush();
    Ok(())
}

/// Lift every positioned glyph run out of the renderer-neutral display list.
///
/// The scrolled document, the viewport-fixed layers, and the top layer are all
/// part of one page's readable content, so all of them contribute. Transform
/// scopes (scroll, sticky, fixed, animated) carry the document-space offset
/// into the run's own origin, which is all a text dump needs.
fn collect_runs(paint: &PagePaint) -> Vec<Run> {
    let mut runs = Vec::new();
    for layer in [
        &paint.primitives,
        &paint.fixed_under_primitives,
        &paint.fixed_primitives,
    ] {
        gather(layer, &mut runs);
    }
    for entry in &paint.top_layer {
        gather(&entry.primitives, &mut runs);
    }
    runs
}

fn gather(commands: &[DisplayCommand], runs: &mut Vec<Run>) {
    for command in commands {
        if let DisplayCommand::GlyphRun {
            origin,
            shaped,
            link,
            ..
        } = command
        {
            let text = shaped.text.replace('\n', " ");
            if text.trim().is_empty() {
                continue;
            }
            runs.push(Run {
                y: origin.y,
                x: origin.x,
                width: shaped.advance,
                height: (shaped.ascent + shaped.descent).max(1.0),
                text,
                link: link.as_ref().map(ToString::to_string),
            });
        }
    }
}

/// Group runs into visual lines and join them into readable rows.
///
/// Runs are ordered by baseline, then left-to-right within a line. Runs that
/// share a baseline (within half a line's height) are one output row; the
/// horizontal gap between two runs decides whether they join directly or with
/// a space, which is what keeps hyphenated inline styling from splitting words
/// while still separating table cells and columns. A vertical gap of more than
/// one-and-a-half lines becomes the blank line between paragraphs.
fn lines_of_runs(runs: &[Run]) -> Vec<String> {
    let mut sorted = runs.iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_y = f32::NAN;
    let mut current_right = f32::NAN;

    for run in sorted {
        let same_line = !current_y.is_nan() && (run.y - current_y).abs() <= run.height * 0.45;
        if same_line {
            // Only join with a space when the shaper left a visible gap; an
            // adjacent inline-styled fragment must not gain one.
            if !current.is_empty() && run.x - current_right > 0.5 {
                current.push(' ');
            }
        } else {
            if !current_y.is_nan() {
                // The gap being measured is between the row just completed and
                // the row starting now, so the break has to follow the row.
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                if run.y - current_y > run.height * 1.6 {
                    lines.push(String::new());
                }
            }
            current_y = run.y;
        }
        current.push_str(&run.text);
        current_right = run.x + run.width;
    }
    if !current.is_empty() {
        lines.push(current);
    }

    finish_lines(lines)
}

/// Trim trailing whitespace, collapse any run of blank rows to a single
/// paragraph break, and drop leading and trailing blank rows.
fn finish_lines(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines.iter().map(|line| line.trim_end().to_string()) {
        if line.trim().is_empty() {
            // A break is only meaningful between two rows of content.
            if out.is_empty() {
                continue;
            }
            if out.last().is_some_and(|last| last.trim().is_empty()) {
                continue;
            }
        }
        out.push(line);
    }
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    out
}

fn links_of(runs: &[Run], out: &mut Vec<String>) {
    let mut seen = HashSet::new();
    for run in runs {
        if let Some(link) = &run.link
            && seen.insert(link.clone())
        {
            out.push(link.clone());
        }
    }
}

/// Walk the accessibility tree in tree order. Bounds are the reader's cue for
/// visual position; the walk order is the composed DOM order, which is what a
/// screen reader would speak.
fn semantic_text(tree: &SemanticTree, links: &mut Vec<String>) -> String {
    let index: HashMap<u64, &trust::accessibility::SemanticNode> =
        tree.nodes.iter().map(|node| (node.id, node)).collect();
    let mut out = String::new();
    let mut seen = HashSet::new();
    let mut stack = vec![tree.root];
    while let Some(id) = stack.pop() {
        let Some(node) = index.get(&id).copied() else {
            continue;
        };
        if !node.name.trim().is_empty() {
            let role = format!("{:?}", node.role).to_ascii_lowercase();
            out.push_str(&format!("{role} \"{}\"\n", node.name.trim()));
        }
        if node.role == trust::accessibility::Role::Link
            && let Some(href) = node.value.as_ref()
            && seen.insert(href.clone())
        {
            links.push(href.clone());
        }
        for child in node.children.iter().rev() {
            stack.push(*child);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Run, finish_lines, lines_of_runs};

    fn run(x: f32, y: f32, width: f32, text: &str) -> Run {
        Run {
            y,
            x,
            width,
            height: 20.0,
            text: text.to_string(),
            link: None,
        }
    }

    #[test]
    fn adjacent_inline_fragments_share_one_row() {
        // An inline-styled fragment inside one sentence must not gain a space,
        // and must survive arriving out of display order.
        let lines = lines_of_runs(&[
            run(70.0, 0.0, 50.0, "world"),
            run(0.0, 0.0, 70.0, "Hello, "),
        ]);
        assert_eq!(lines, vec!["Hello, world".to_string()]);
    }

    #[test]
    fn separated_runs_join_with_one_space() {
        let lines = lines_of_runs(&[run(0.0, 0.0, 40.0, "Total"), run(180.0, 0.0, 20.0, "42")]);
        assert_eq!(lines, vec!["Total 42".to_string()]);
    }

    #[test]
    fn a_normal_leading_gap_wraps_without_a_paragraph_break() {
        let lines = lines_of_runs(&[
            run(0.0, 0.0, 60.0, "A paragraph"),
            run(0.0, 24.0, 60.0, "that wraps"),
        ]);
        assert_eq!(
            lines,
            vec!["A paragraph".to_string(), "that wraps".to_string()]
        );
    }

    #[test]
    fn a_wide_vertical_gap_becomes_one_paragraph_break() {
        let lines = lines_of_runs(&[
            run(0.0, 0.0, 60.0, "First."),
            run(0.0, 120.0, 60.0, "Second."),
            run(0.0, 240.0, 60.0, "Third."),
        ]);
        assert_eq!(
            lines,
            vec![
                "First.".to_string(),
                String::new(),
                "Second.".to_string(),
                String::new(),
                "Third.".to_string(),
            ]
        );
    }

    #[test]
    fn outer_and_repeated_blank_rows_are_dropped() {
        let kept = finish_lines(vec![
            String::new(),
            "  Body".to_string(),
            String::new(),
            String::new(),
            "Next".to_string(),
            "   ".to_string(),
        ]);
        // Leading indentation is the document's own; only trailing space and
        // whole blank rows are removable.
        assert_eq!(
            kept,
            vec!["  Body".to_string(), String::new(), "Next".to_string()]
        );
    }
}
