mod app;
mod cp437;
mod doc;
mod gemini;
mod gopher;
mod http;
mod img;
mod telnet;
mod tls;
mod ui;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let host = args.next();
    let port = match args.next() {
        // TODO: GNU telnet also accepts service names (e.g. "telnet", "smtp")
        // via getservbyname; numeric ports only for now.
        Some(p) => match p.parse::<u16>() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("trust: bad port number: {p}");
                return ExitCode::FAILURE;
            }
        },
        None => 23,
    };

    let terminal = ratatui::init();
    // Query the terminal for its graphics protocol and font size. This
    // talks on stdin/stdout, so it must happen before the event stream
    // exists (which would eat the reply) — hence here, not in App.
    let picker = ratatui_image::picker::Picker::from_query_stdio()
        .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks());
    // Capture the mouse so wheel events scroll our scrollback instead of
    // being translated into arrow keys by the terminal emulator.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let mut app = app::App::new(host, port);
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
