mod app;
mod cp437;
mod gopher;
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
    // Capture the mouse so wheel events scroll our scrollback instead of
    // being translated into arrow keys by the terminal emulator.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let result = app::App::new(host, port).run(terminal).await;
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
