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
//! Two more things a caller needs are reported rather than assumed. The closing
//! line carries what the protocol answered — `HTTP 404 (text/html)` — because a
//! dump of an error page is worthless if it cannot be told from a dump of the
//! page that was asked for; the page's own status note is the wrong source, since
//! a resident script actor replaces it the moment it repaints. And the text is
//! capped at `--max-chars` characters, because a page of plain text is unbounded.
//!
//! Geometry: layout and the CSSOM viewport are reported against the synthetic
//! `--width`/`--height` viewport exactly as the desktop frontend reports them
//! against its window. The terminal cell model does not exist here, so the
//! 1x1-cell desktop adapter is the only meaningful mapping.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::time::{Duration, Instant};

use url::Url;

use trust::accessibility::SemanticTree;
use trust::core::{BrowserController, BrowserPage, CssSize, FetchedDocument, UserAction};
use trust::doc::{Doc, Link};
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
    max_chars: usize,
    js_diagnostics: bool,
}

const USAGE: &str = "\
usage: trust-headless [options] URL

  --width N       CSS viewport width in CSS pixels (default 1024)
  --height N      CSS viewport height in CSS pixels (default 768)
  --timeout SECS  stop waiting after this long (default 10)
  --settle SECS   for a page that never goes inert, stop once it has been this
                  long quiet (default 0.75; 0 waits for the engine verdict only)
  --max-chars N   print at most N characters of page text (default 8000;
                  unlimited keeps the whole page)
  --format F      text (default) or semantic
  --links         also list every linked target found in the page
  --js-diagnostics  print the last page-script outcome: JS errors, console
                  output, module skips, panic flag, and fetch count
  -h, --help      show this message

Exit status: 0 when the dump is complete, 1 when no page loaded, 2 on a bad
command line, 3 when text was dumped but the page never stopped changing. A
page that answers 404 or 500 still exits 0: the fetch worked, and what the
server said is in the closing line for the caller to decide about.
";

fn parse_args() -> Result<Option<Options>, String> {
    let mut address: Option<String> = None;
    let mut width = 1024.0f32;
    let mut height = 768.0f32;
    let mut timeout = 10.0f32;
    let mut settle = 0.75f32;
    let mut format = Format::Text;
    let mut list_links = false;
    let mut max_chars = 8000usize;
    let mut js_diagnostics = false;

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
            "--js-diagnostics" => js_diagnostics = true,
            "--max-chars" => {
                let raw = string("--max-chars", &mut args)?;
                max_chars = match raw.as_str() {
                    "unlimited" => 0,
                    _ => raw
                        .parse::<usize>()
                        .map_err(|_| format!("--max-chars needs a count, got {raw}"))?,
                };
            }
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
        max_chars,
        js_diagnostics,
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

    let outcome = match try_run(options) {
        Ok(complete) => Ok(complete),
        Err(error) => Err(error.to_string()),
    };
    // Navigation boundaries are where the allocator's arenas get their largest
    // one-off reclaim, and this process exits after exactly one of them.
    trust::release_allocator_memory();
    match outcome {
        Ok(true) => {}
        Ok(false) => std::process::exit(3),
        Err(error) => {
            eprintln!("trust-headless: {error}");
            std::process::exit(1);
        }
    }
}

/// One page, dumped: returns whether the text is a complete picture of the page
/// as opposed to whatever it looked like when `--timeout` expired.
fn try_run(options: Options) -> Result<bool, Box<dyn Error>> {
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
async fn navigate_and_settle(options: &Options) -> Result<bool, Box<dyn Error>> {
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

    // Relative `href` values are resolved against the address that answered,
    // exactly as a click resolves them.
    let base = document_base(page);

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
            let runs = collect_runs(&rendered.layout.paint, base.as_ref());
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
    // What the protocol answered is a fact about the document, and the
    // controller's status is not it: the status set when a fetch commits is
    // replaced by a resident actor's own note as soon as it repaints, so a
    // server's 404 vanishes behind "Page updated". A caller reading this dump
    // needs to know whether the page it asked for is the page it got.
    let verdict = describe_fetch(&page.document);
    let full = body.chars().count();
    let body = cap_to_chars(body, options.max_chars);
    let capped = body.chars().count() != full;
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
        "trust-headless: {} · {}{} · {}x{} CSS px · {} in {}ms ({})",
        page.address(),
        verdict,
        if capped {
            format!(
                " · text capped at {} of {full} characters",
                options.max_chars
            )
        } else {
            String::new()
        },
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
    if options.js_diagnostics {
        match controller.last_js_outcome() {
            Some(outcome) => {
                if !outcome.errors.is_empty() {
                    eprintln!("[js-errors] {} error(s):", outcome.errors.len());
                    for error in &outcome.errors {
                        eprintln!("  {error}");
                    }
                }
                if !outcome.console.is_empty() {
                    eprintln!("[js-console] {} line(s):", outcome.console.len());
                    for line in &outcome.console {
                        eprintln!("  {line}");
                    }
                }
                eprintln!(
                    "[js-outcome] panicked={} modules_skipped={} fetches={}",
                    outcome.panicked, outcome.modules_skipped, outcome.fetches
                );
            }
            None => eprintln!("[js-outcome] none (page ran no script render event)"),
        }
    }
    let _ = out.flush();
    if let Some(path) = std::env::var_os("TRUST_HEADLESS_PNG")
        && let Some(rendered) = controller
            .current_page()
            .and_then(|page| page.rendered_page())
    {
        let frame = trust::render::headless::render_paint(
            &rendered.layout.paint,
            CssSize::new(options.width, options.height),
        )?;
        trust::render::headless::write_png(&frame, path)?;
        eprintln!(
            "[snapshot] canonical display list with inline image resources (no additional image fetches)"
        );
    }
    Ok(settled != Settle::Timeout)
}

/// What the protocol said about a fetched document, in the same words the
/// terminal frontend puts in its status line (`core::fetched_status`) minus the
/// address, which this driver prints separately. The two copies are worth
/// keeping in step by hand rather than exporting a helper from the shared
/// controller for one driver's status line.
fn describe_fetch(document: &FetchedDocument) -> String {
    match document {
        FetchedDocument::Http(response) => {
            let media = response.content_type.split(';').next().unwrap_or("").trim();
            format!("HTTP {} ({media})", response.status)
        }
        FetchedDocument::Gemini(response) => {
            format!("Gemini {} {}", response.status, response.meta)
        }
        FetchedDocument::Whois(page) => format!(
            "WHOIS: {} bytes, {} servers",
            page.reply.bytes(),
            page.reply.hops.len()
        ),
        FetchedDocument::Gopher(bytes) | FetchedDocument::OneShot(bytes) => {
            format!("{} bytes", bytes.len())
        }
        FetchedDocument::Dict(page) => page.status(),
        FetchedDocument::Finger(page) => format!("Finger: {} bytes", page.reply.body.len()),
        FetchedDocument::Rdap(page) => {
            format!("RDAP: {} bytes (HTTP {})", page.raw.len(), page.status)
        }
        // The driver prints the address of an in-process document anyway.
        FetchedDocument::Internal(_) => String::from("in-process document"),
    }
}

/// Trim text to a character budget without ending in the middle of a line when
/// a line ended recently anyway: give back whatever follows the last line break
/// in the final quarter of the budget, so a reader is not handed half a
/// sentence and nothing to tell them so.
fn cap_to_chars(text: String, max_chars: usize) -> String {
    if max_chars == 0 || text.chars().count() <= max_chars {
        return text;
    }
    let kept: String = text.chars().take(max_chars).collect();
    let earliest_useful = max_chars - max_chars / 4;
    let mut cut = None;
    // Only a break in the last quarter of the budget is worth losing text for.
    for (char_index, (byte_offset, ch)) in kept.char_indices().enumerate() {
        if ch == '\n' && char_index >= earliest_useful {
            cut = Some(byte_offset);
        }
    }
    match cut {
        Some(byte_offset) if byte_offset > 0 => kept[..byte_offset].to_string(),
        _ => kept,
    }
}

/// Lift every positioned glyph run out of the renderer-neutral display list.
///
/// The scrolled document, the viewport-fixed layers, and the top layer are all
/// part of one page's readable content, so all of them contribute. Transform
/// scopes (scroll, sticky, fixed, animated) carry the document-space offset
/// into the run's own origin, which is all a text dump needs.
fn collect_runs(paint: &PagePaint, base: Option<&Url>) -> Vec<Run> {
    let mut runs = Vec::new();
    for layer in [
        &paint.primitives,
        &paint.fixed_under_primitives,
        &paint.fixed_primitives,
    ] {
        gather(layer, &mut runs, base);
    }
    for entry in &paint.top_layer {
        gather(&entry.primitives, &mut runs, base);
    }
    runs
}

fn gather(commands: &[DisplayCommand], runs: &mut Vec<Run>, base: Option<&Url>) {
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
                link: link.as_ref().and_then(|link| reported_link(link, base)),
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
/// Word spaces measured across real pages are about a fifth of a line, and
/// gaps between unrelated boxes on the same visual line are three line-heights
/// or more. Anything past a line and a half is therefore not a word space but a
/// column or an unconnected widget, and boxes that overlap by more than a fifth
/// of a line are not adjacent text at all. Ratios are used rather than fixed
/// pixels so the rule holds at any font size or device pixel ratio.
const COLUMN_GAP_LINES: f32 = 1.5;
const OVERLAPPING_LINES: f32 = -0.2;

impl Run {
    fn bottom(&self) -> f32 {
        self.y + self.height
    }
}

/// Group runs into the visual lines they were painted on.
///
/// Runs must not simply be sorted by `y`. A heading whose second fragment sits
/// a few pixels above the first (docs.python.org's `dataclasses — Data Classes`,
/// 149.3 against 152.3) sorts first, and then every run to its left is measured
/// as though it continued rightward from the fragment: a negative gap, no space,
/// and the fragments of one heading arrive as `— Data Classesdataclasses`. So
/// runs are collected into lines by how much their vertical bands overlap, and
/// each line is read left to right on its own.
fn visual_lines<'a>(runs: &[&'a Run]) -> Vec<Vec<&'a Run>> {
    let mut ordered = runs.to_vec();
    ordered.sort_by(|a, b| {
        let center_a = a.y + a.height / 2.0;
        let center_b = b.y + b.height / 2.0;
        center_a
            .partial_cmp(&center_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut lines: Vec<Vec<&Run>> = Vec::new();
    let mut line: Vec<&Run> = Vec::new();
    // The band a line is measured against stays the band of the run that
    // opened it; letting it grow to the union of its members lets one tall run
    // reach down into the next line and merge them.
    let mut band = (f32::NAN, f32::NAN);
    for run in ordered {
        if line.is_empty() {
            band = (run.y, run.bottom());
            line.push(run);
            continue;
        }
        let overlap = band.1.min(run.bottom()) - band.0.max(run.y);
        if overlap >= run.height.min(band.1 - band.0) * 0.5 {
            line.push(run);
        } else {
            lines.push(std::mem::take(&mut line));
            band = (run.y, run.bottom());
            line.push(run);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Group runs into the blocks they were painted in.
///
/// The display list is already in the order the page drew its content, which for
/// in-flow content is document order, and geometry alone cannot separate an
/// article line from the sidebar entry beside it: a heading in one column sits in
/// the vertical band of a line in the other, so any reading order built from
/// coordinates alone interleaves them. A run continues the block being built when
/// it is on the same line, or below the previous line and still inside the same
/// column; otherwise it opens a new block.
fn painted_blocks<'a>(runs: &[&'a Run]) -> Vec<Vec<&'a Run>> {
    let mut blocks: Vec<Vec<&Run>> = Vec::new();
    let mut block: Vec<&Run> = Vec::new();
    let mut last_band = (f32::NAN, f32::NAN);
    let mut column = (f32::NAN, f32::NAN);
    for run in runs {
        if block.is_empty() {
            block.push(run);
            last_band = (run.y, run.bottom());
            column = (run.x, run.x + run.width);
            continue;
        }
        // Reaching the same band is not enough: a Wikipedia infobox title sits
        // in the vertical band of the article line beside it, and letting that
        // merge the two drops the infobox into the middle of a paragraph. A
        // block is text that is actually next to other text in it.
        let on_the_last_line = last_band.1.min(run.bottom()) - last_band.0.max(run.y)
            >= run.height.min(last_band.1 - last_band.0) * 0.5
            && run.x + run.width >= column.0 - run.height
            && run.x <= column.1 + run.height * COLUMN_GAP_LINES;
        let continues_down = run.y > last_band.0
            && run.y - last_band.1 < run.height
            && run.x + run.width > column.0
            && run.x < column.1;
        if on_the_last_line || continues_down {
            block.push(run);
            if !on_the_last_line {
                last_band = (run.y, run.bottom());
            }
            column.0 = column.0.min(run.x);
            column.1 = column.1.max(run.x + run.width);
        } else {
            blocks.push(std::mem::take(&mut block));
            block.push(run);
            last_band = (run.y, run.bottom());
            column = (run.x, run.x + run.width);
        }
    }
    if !block.is_empty() {
        blocks.push(block);
    }
    blocks
}

fn lines_of_runs(runs: &[Run]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut previous_y = f32::NAN;

    let ordered = painted_blocks(&runs.iter().collect::<Vec<_>>())
        .into_iter()
        .flat_map(|block| visual_lines(&block))
        .collect::<Vec<Vec<_>>>();

    for mut line in ordered {
        let Some(first) = line.first().copied() else {
            continue;
        };
        // A break between paragraphs is a property of where the line sits, so
        // it is measured before the line is read, and always follows the line
        // that came before it.
        if !previous_y.is_nan() && first.y - previous_y > first.height * 1.6 {
            lines.push(String::new());
        }
        previous_y = first.y;

        // A visual line is read left to right whatever order the page drew it
        // in, which is the only reordering done here: the blocks themselves
        // stay in the order the page painted them.
        line.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        let mut row = String::new();
        let mut right = f32::NAN;
        for run in line {
            if row.is_empty() {
                row.push_str(&run.text);
                right = run.x + run.width;
                continue;
            }
            let gap = run.x - right;
            if gap > run.height * COLUMN_GAP_LINES || gap < run.height * OVERLAPPING_LINES {
                // Either a gap no word space explains (a second column or an
                // unconnected widget on the same line) or boxes that would
                // collide if read as one sentence.
                lines.push(std::mem::take(&mut row));
            } else if gap > 0.5 {
                row.push(' ');
            }
            row.push_str(&run.text);
            right = right.max(run.x + run.width);
        }
        if !row.is_empty() {
            lines.push(row);
        }
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

/// The address that actually answered, after redirects: what a relative
/// `href` resolves against. Everything except a fetched HTTP document already
/// prints its own complete address.
fn document_base(page: &BrowserPage) -> Option<Url> {
    match &page.document {
        FetchedDocument::Http(response) => Some(response.url.clone()),
        _ => Url::parse(&page.address()).ok(),
    }
}

/// A link worth printing. A button or form control has no address to report,
/// and `Link::JsClick` carries the *raw* attribute, which is a relative
/// reference on almost every page: `/foo` on one host and `../bar.html` in one
/// directory are useless to a caller that has to guess where it was reading
/// from, so they are resolved against the document here exactly as a click
/// would resolve them.
fn reported_link(link: &Link, base: Option<&Url>) -> Option<String> {
    match link {
        Link::JsClick { href, .. } if href.trim().is_empty() => None,
        Link::JsClick { href, .. } => Some(resolve_reference(base, href.trim())),
        Link::Form { .. } => None,
        other => Some(other.to_string()),
    }
}

fn resolve_reference(base: Option<&Url>, href: &str) -> String {
    // Already absolute, including `mailto:` and friends: leave the author's
    // spelling alone.
    if Url::parse(href).is_ok() {
        return href.to_string();
    }
    base.and_then(|base| base.join(href).ok())
        .map(|resolved| resolved.to_string())
        // A reference the base cannot resolve (`javascript:…`, a bare `#` with
        // no URL to anchor) is still what the author wrote, so report it as
        // written rather than dropping the link.
        .unwrap_or_else(|| href.to_string())
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
    use super::{
        Run, cap_to_chars, describe_fetch, finish_lines, lines_of_runs, reported_link,
        resolve_reference,
    };
    use trust::core::FetchedDocument;

    #[test]
    fn a_document_reports_the_verdict_its_protocol_gave() {
        // The status a resident script actor writes over the controller's says
        // nothing about whether the server answered 404, so the driver reads the
        // verdict from the document itself.
        let missing = FetchedDocument::Http(Box::new(trust::http::Response {
            url: Url::parse("https://example.com/gone").unwrap(),
            status: 404,
            content_type: String::from("text/html; charset=utf-8"),
            headers: Vec::new(),
            body: b"<p>gone</p>".to_vec(),
            rendered: None,
            js: None,
            blobs: None,
            live: None,
            declarative_refresh: None,
            challenge: None,
            from_post: false,
            timing: None,
        }));
        assert_eq!(describe_fetch(&missing), "HTTP 404 (text/html)");
        assert_eq!(
            describe_fetch(&FetchedDocument::Gopher(vec![0u8; 7])),
            "7 bytes"
        );
    }

    #[test]
    fn text_over_its_budget_ends_on_a_line_and_under_it_is_untouched() {
        let budget = 20;
        assert_eq!(cap_to_chars(String::new(), 0), String::new());
        let unlimited = "x".repeat(budget * 4);
        assert_eq!(cap_to_chars(unlimited.clone(), 0), unlimited);
        let short = "one\ntwo".to_string();
        assert_eq!(cap_to_chars(short.clone(), budget), short);
        // One break, but 16 characters into a 20-character budget: the quarter
        // worth giving back starts at 15, so the break at 16 is the last one
        // close enough to the end, and the tail after it is dropped rather than
        // reported as a line.
        let break_near_end = format!("{}\n{}", "a".repeat(16), "b".repeat(9));
        assert_eq!(cap_to_chars(break_near_end, budget), "a".repeat(16));
        // A break only at the very start is too expensive to reach for.
        let break_at_front = format!("\n{}", "c".repeat(40));
        assert_eq!(
            cap_to_chars(break_at_front, budget),
            "\n".to_string() + &"c".repeat(19)
        );
    }

    use trust::doc::Link;
    use url::Url;

    fn run(x: f32, y: f32, width: f32, text: &str) -> Run {
        sized_run(x, y, width, 20.0, text)
    }

    fn sized_run(x: f32, y: f32, width: f32, height: f32, text: &str) -> Run {
        Run {
            y,
            x,
            width,
            height,
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
        let lines = lines_of_runs(&[run(0.0, 0.0, 40.0, "Total"), run(50.0, 0.0, 20.0, "42")]);
        assert_eq!(lines, vec!["Total 42".to_string()]);
    }

    #[test]
    fn a_gap_no_word_space_explains_starts_a_new_line_instead() {
        // This used to assert "Total 42" for the pair below. A gap of seven
        // line-heights is not a space in front of a number, it is the space
        // between two unrelated boxes on one visual line, and joining them is
        // how one column of a page ended up inside another's sentence.
        let lines = lines_of_runs(&[run(0.0, 0.0, 40.0, "Total"), run(180.0, 0.0, 20.0, "42")]);
        assert_eq!(lines, vec!["Total".to_string(), "42".to_string()]);
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

    #[test]
    fn a_heading_fragment_sitting_above_the_rest_still_reads_left_to_right() {
        // docs.python.org/3/library/dataclasses.html, as painted: the h1's
        // classifier sits 3px above the word it classifies. Sorting by y would
        // emit it first and append the word with no gap to measure.
        let lines = lines_of_runs(&[
            sized_run(504.7, 149.3, 220.5, 38.4, "\u{2014} Data Classes"),
            sized_run(292.2, 152.3, 204.5, 35.9, "dataclasses"),
        ]);
        assert_eq!(lines, vec!["dataclasses \u{2014} Data Classes".to_string()]);
    }

    #[test]
    fn text_ordered_by_x_is_not_reordered_into_the_wrong_line() {
        // A classifier above and to the right, then two lines that start well
        // to its left: sorting runs by x alone would put the short indented
        // lines before the classifier they follow, and the paragraph would
        // arrive with its tail first.
        let heading = Run {
            y: 152.3,
            x: 44.0,
            width: 90.0,
            height: 38.4,
            text: "dataclasses".into(),
            link: None,
        };
        let a = Run {
            y: 175.0,
            x: 516.5,
            width: 115.7,
            height: 19.2,
            text: "— Data Classes".into(),
            link: None,
        };
        let b = Run {
            y: 194.2,
            x: 44.0,
            width: 200.0,
            height: 19.2,
            text: "or a variable that already".into(),
            link: None,
        };
        let c = Run {
            y: 213.4,
            x: 56.0,
            width: 180.0,
            height: 19.2,
            text: "existed".into(),
            link: None,
        };
        assert_eq!(
            lines_of_runs(&[heading, a, b, c]),
            vec![
                "dataclasses".to_string(),
                "— Data Classes".to_string(),
                "or a variable that already".to_string(),
                "existed".to_string(),
            ]
        );
    }

    #[test]
    fn a_sidebar_label_and_the_text_beside_it_do_not_become_one_sentence() {
        // Same page: the contents entry ends at x=218.0 and "Source code:" does
        // not start until x=291.2, a gap of 4.6 line-heights. Nothing about
        // those boxes reads as one sentence.
        let lines = lines_of_runs(&[
            sized_run(37.0, 211.1, 85.2, 15.0, "dataclasses"),
            sized_run(126.1, 210.1, 91.9, 16.0, "\u{2014} Data Classes"),
            sized_run(291.2, 213.8, 92.9, 19.2, "Source code:"),
        ]);
        assert_eq!(
            lines,
            vec![
                "dataclasses \u{2014} Data Classes".to_string(),
                "Source code:".to_string(),
            ]
        );
    }

    #[test]
    fn boxes_that_overlap_are_not_read_as_adjacent_words() {
        // keras.rstudio.com paints a screen-reader-only label on top of the
        // logo, sharing a baseline.
        let lines = lines_of_runs(&[
            sized_run(0.0, 0.0, 100.0, 20.0, "keras3"),
            sized_run(60.0, 0.0, 80.0, 20.0, "keras3 1.5.1"),
        ]);
        assert_eq!(
            lines,
            vec!["keras3".to_string(), "keras3 1.5.1".to_string()]
        );
    }

    #[test]
    fn relative_references_resolve_against_the_address_that_answered() {
        let base = Url::parse("https://docs.python.org/3/library/dataclasses.html").unwrap();
        assert_eq!(
            resolve_reference(Some(&base), "../bugs.html"),
            "https://docs.python.org/3/bugs.html"
        );
        assert_eq!(
            resolve_reference(Some(&base), "#mutable-default-values"),
            "https://docs.python.org/3/library/dataclasses.html#mutable-default-values"
        );
        assert_eq!(
            resolve_reference(Some(&base), "/3/search.html"),
            "https://docs.python.org/3/search.html"
        );
        // Already absolute, and schemes with no base to resolve against.
        assert_eq!(
            resolve_reference(Some(&base), "mailto:ruby@example.com"),
            "mailto:ruby@example.com"
        );
        assert_eq!(
            resolve_reference(None, "../bugs.html"),
            "../bugs.html".to_string()
        );
        // Something no resolver can mean anything by is reported as written.
        assert_eq!(
            resolve_reference(Some(&base), "javascript:void(0)"),
            "javascript:void(0)".to_string()
        );
    }

    #[test]
    fn a_button_reports_no_link_and_a_js_anchor_reports_its_resolved_href() {
        let base = Url::parse("https://example.com/blog/post.html").unwrap();
        assert_eq!(
            reported_link(
                &Link::JsClick {
                    node: 7,
                    href: String::new(),
                },
                Some(&base),
            ),
            None
        );
        assert_eq!(
            reported_link(&Link::Form { form: 0, field: 1 }, Some(&base),),
            None
        );
        assert_eq!(
            reported_link(
                &Link::JsClick {
                    node: 7,
                    href: "/comments/".to_string(),
                },
                Some(&base),
            ),
            Some("https://example.com/comments/".to_string())
        );
    }
}
