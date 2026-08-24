use std::io::Write;
use std::process::ExitCode;

use trust::frontend::tui;

/// Pop the terminal's title stack (CSI 23;2t, the counterpart to the 22;2t
/// push in `main`), restoring whatever title was showing before TRust took
/// it over. Best-effort: a terminal that never pushed just ignores this.
fn pop_terminal_title() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[23;2t");
    let _ = out.flush();
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let host = args.next();
    // The port is OPTIONAL: with no port a bare host opens as the web (https,
    // falling back to http) — HTTP is the default now. A GIVEN port picks the
    // protocol (80/443→web, 70→gopher, 1965→gemini, ...; ANY OTHER port→telnet,
    // since odd ports are MUDs/BBSes, not the web; see `dispatch_open`).
    let start_port = match args.next() {
        // Numeric, or a well-known service name ("telnet", "smtp", ...)
        // like GNU telnet's getservbyname.
        Some(p) => match tui::parse_port(&p) {
            Some(p) => Some(p),
            None => {
                eprintln!("trust: bad port or service name: {p}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    // An OS URL handler may invoke TRust without an interactive terminal.
    // Delegate concrete YouTube playback URLs before Ratatui touches the TTY;
    // normal YouTube browsing URLs continue through the browser below.
    if let Some(url) = host.as_deref().and_then(trust::media::youtube_video_url) {
        return match trust::media::launch_mpv(url.as_str(), None) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("trust: {error}");
                ExitCode::FAILURE
            }
        };
    }

    let terminal = ratatui::init();
    // This thread (the `#[tokio::main]` `block_on` driver) owns the live
    // terminal, and the run loop never migrates off it (verified). Claim it
    // BEFORE installing the hook below, which gates on this flag.
    tui::TERMINAL_OWNER.with(|c| c.set(true));
    // `ratatui::init()` just installed a panic hook that calls
    // `ratatui::restore()` UNCONDITIONALLY, on EVERY panic, on ANY thread,
    // before the previous hook. That is the partial-crash bug: background
    // work — the `trust-*` JS workers, the tokio fetch and image-load tasks,
    // the blocking image decode/encode pool — is all sandboxed by
    // `catch_unwind`/tokio (a panic there costs one operation, the page
    // degrades), but ratatui's hook tears the alt screen down and disables
    // raw mode (leaking the mouse SGR stream as text) out from under a run
    // loop that's still running and that the user can still type into. Wrap
    // ratatui's hook with an ownership gate: restore (and print the
    // backtrace) ONLY for a panic on THIS terminal-owner thread — a genuine
    // render/run-loop fault — and leave the live TUI untouched for every
    // background-thread panic. See `app::TERMINAL_OWNER`.
    let ratatui_hook = std::panic::take_hook(); // = restore(); default(info)
    std::panic::set_hook(Box::new(move |info| {
        // Optional diagnostic: log EVERY panic (thread + backtrace) to a file,
        // regardless of thread. Off unless the env var is set; this is how we
        // pin down a background-op panic that the gate (correctly) swallows.
        if let Ok(path) = std::env::var("TRUST_PANIC_LOG") {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let name = std::thread::current()
                    .name()
                    .unwrap_or("<unnamed>")
                    .to_string();
                let owner = tui::TERMINAL_OWNER.with(|c| c.get());
                let bt = std::backtrace::Backtrace::force_capture();
                let _ = writeln!(
                    f,
                    "=== PANIC thread={name:?} terminal_owner={owner} ===\n{info}\n{bt}\n"
                );
            }
        }
        if !tui::TERMINAL_OWNER.with(|c| c.get()) {
            return; // background panic, caught downstream — keep the TUI clean
        }
        // A real render/run-loop panic: drop mouse capture and paste mode,
        // pop the terminal title back, then let ratatui's hook restore the
        // screen and the default hook print.
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::event::DisableBracketedPaste
        );
        pop_terminal_title();
        ratatui_hook(info);
    }));

    // Query the terminal for its graphics protocol and font size. This
    // talks on stdin/stdout, so it must happen before the event stream
    // exists (which would eat the reply) — hence here, not in App.
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    // Capture the mouse so wheel events scroll our scrollback instead of
    // being translated into arrow keys by the terminal emulator. Bracketed
    // paste makes a paste arrive as ONE event instead of replayed
    // keystrokes (a pasted Tab used to open the console).
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    );
    // Take over the terminal title for the run. Push the terminal's current
    // title onto its title stack first (CSI 22;2t — title-only, no icon:
    // foot's ctlseqs(7) documents 22/23 with param 2 as exactly this, and
    // it's the same xterm-family sequence elsewhere) so it can be popped
    // back (CSI 23;2t) on exit instead of guessing/hardcoding what it was.
    let _ = std::io::stdout().write_all(b"\x1b[22;2t");
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle("TRust"));
    let mut app = tui::App::new(host, start_port.unwrap_or(23));
    app.set_start_port(start_port);
    app.set_picker(picker);
    let result = app.run(terminal).await;
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::event::DisableBracketedPaste
    );
    pop_terminal_title();
    ratatui::restore();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("trust: {err}");
            ExitCode::FAILURE
        }
    }
}
