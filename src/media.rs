//! Shared external-media delegation used by both native frontends.

use std::process::{Command, Stdio};

pub fn launch_mpv(url: &str, referrer: Option<&url::Url>) -> Result<(), String> {
    let mut command = Command::new("mpv");
    if let Some(referrer) = referrer {
        command.arg(format!("--referrer={referrer}"));
    }
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                String::from("mpv not found on PATH")
            } else {
                format!("mpv failed to launch: {error}")
            }
        })
}
