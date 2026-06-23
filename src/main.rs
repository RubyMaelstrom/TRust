// mimalloc as the global allocator (default-on `mimalloc` feature): ~17%
// faster JS parse+compile, which are dominated by millions of tiny AST/
// CodeBlock allocations. `--no-default-features` falls back to the system
// allocator (pure Rust). See the feature note in Cargo.toml.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod app;
mod cp437;
mod doc;
mod dom;
mod gemini;
mod gopher;
mod http;
mod img;
mod js;
mod layout;
mod oneshot;
mod telnet;
mod tls;
mod ui;
mod ws;

use std::process::ExitCode;

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
        Some(p) => match app::parse_port(&p) {
            Some(p) => Some(p),
            None => {
                eprintln!("trust: bad port or service name: {p}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let terminal = ratatui::init();
    // This thread (the `#[tokio::main]` `block_on` driver) owns the live
    // terminal, and the run loop never migrates off it (verified). Claim it
    // BEFORE installing the hook below, which gates on this flag.
    app::TERMINAL_OWNER.with(|c| c.set(true));
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
                let owner = app::TERMINAL_OWNER.with(|c| c.get());
                let bt = std::backtrace::Backtrace::force_capture();
                let _ = writeln!(
                    f,
                    "=== PANIC thread={name:?} terminal_owner={owner} ===\n{info}\n{bt}\n"
                );
            }
        }
        if !app::TERMINAL_OWNER.with(|c| c.get()) {
            return; // background panic, caught downstream — keep the TUI clean
        }
        // A real render/run-loop panic: drop mouse capture, then let
        // ratatui's hook restore the screen and the default hook print.
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        ratatui_hook(info);
    }));

    // Query the terminal for its graphics protocol and font size. This
    // talks on stdin/stdout, so it must happen before the event stream
    // exists (which would eat the reply) — hence here, not in App.
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    // Capture the mouse so wheel events scroll our scrollback instead of
    // being translated into arrow keys by the terminal emulator.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let mut app = app::App::new(host, start_port.unwrap_or(23));
    app.start_port = start_port;
    app.set_picker(picker);
    let result = app.run(terminal).await;
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("trust: {err}");
            ExitCode::FAILURE
        }
    }
}
