//! One browser process per profile and display. A launch while the browser
//! runs hands its window to the running process, which opens it as a new
//! window (`windowing`), and ends: the windows share one process's pages,
//! caches and site storage, and the profile is never written by two
//! processes at once.
//!
//! The first launch holds an exclusive lock on a file beside the endpoint
//! for its whole life; the operating system releases it when the process
//! ends however it ends, so a stale endpoint never stops a later launch
//! from becoming the browser. Launches that find the lock taken connect to
//! the endpoint instead: a Unix domain socket in `$XDG_RUNTIME_DIR` (XDG
//! Base Directory Specification 0.8: "Applications should use this
//! directory for communication and synchronization purposes"), or a named
//! pipe on Windows. A launch sends one JSON line (its address, made
//! absolute against its own working directory, and its activation token,
//! which xdg-activation-v1 lets clients pass "through means not described
//! in this protocol") and waits for the browser's one-line answer.
//!
//! The instance is keyed by the executable, the profile directory and the
//! display, so a development build and the installed browser, other
//! profiles (`XDG_DATA_HOME`), and other displays each get their own.
//! `--new-instance` opts out entirely.

use super::*;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// The launch protocol's version. A browser refuses versions it does not
/// speak, and the launch then opens its own process.
const PROTOCOL_VERSION: u32 = 1;
/// The longest launch request a browser reads (an address can be a long
/// `data:` URL).
const MAX_REQUEST_BYTES: u64 = 4 * 1024 * 1024;
/// How long a launch waits for the running browser to accept it and answer.
const HAND_OFF_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a launch that found the lock taken keeps trying to reach the
/// endpoint, which the running browser opens just after taking the lock.
const CONNECT_PATIENCE: Duration = Duration::from_secs(5);
const CONNECT_RETRY: Duration = Duration::from_millis(50);
/// How long the browser waits after its endpoint failed to accept.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug, Serialize, Deserialize)]
struct LaunchRequest {
    version: u32,
    address: Option<String>,
    activation_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LaunchReply {
    opened: bool,
    error: Option<String>,
}

/// Opens a launch's window in the browser; false once the browser is closing.
pub(super) type OpenWindow = Arc<dyn Fn(NewWindow) -> bool + Send + Sync>;

/// The outcome of `claim`.
pub(super) enum Claim {
    /// This process is the browser; it serves later launches.
    Primary(Server),
    /// The running browser opened this launch's window.
    HandedOff,
}

/// Where this launch's instance lives.
struct Endpoint {
    lock: PathBuf,
    /// The socket's path, or the pipe's name.
    address: PathBuf,
}

/// The browser's side: the lock it holds and the endpoint it answers on.
pub(super) struct Server {
    endpoint: Endpoint,
    lock: std::fs::File,
    #[cfg(unix)]
    listener: Option<std::os::unix::net::UnixListener>,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Become the browser, or hand `launch` to the running one. An error means
/// neither is possible (no profile directory, an unusable runtime
/// directory, a browser that does not answer): the caller runs alone.
pub(super) fn claim(launch: &NewWindow) -> Result<Claim, String> {
    let endpoint = Endpoint::discover()?;
    let mut attempts = 0;
    loop {
        attempts += 1;
        if let Some(server) = endpoint.try_serve()? {
            return Ok(Claim::Primary(server));
        }
        match endpoint.hand_off(launch) {
            Ok(()) => return Ok(Claim::HandedOff),
            // A browser that is closing gives up its lock: try again.
            Err(HandOff::Closing) if attempts < 20 => {
                std::thread::sleep(CONNECT_RETRY);
            }
            Err(HandOff::Closing) => {
                return Err(String::from("the running browser is closing"));
            }
            Err(HandOff::Failed(error)) => return Err(error),
        }
    }
}

enum HandOff {
    /// The browser is ending and opens no more windows.
    Closing,
    Failed(String),
}

/// The address a launch sends: a local path made a `file:` URL against this
/// launch's working directory, which the browser does not share.
pub(super) fn absolute_address(address: Option<String>) -> Option<String> {
    let address = address?;
    match trust::file::url_from_input(&address) {
        Ok(Some(url)) => Some(url.into()),
        _ => Some(address),
    }
}

/// A short, stable name for this executable, profile and display: SHA-256,
/// whose output never changes between builds as `std`'s hashers may.
fn instance_key(executable: &Path, profile: &Path, display: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for part in [
        executable.as_os_str().as_encoded_bytes(),
        profile.as_os_str().as_encoded_bytes(),
        display.as_bytes(),
    ] {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize()[..10]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The display the windows open on: launches on another display start
/// their own browser there.
fn display_name() -> String {
    if cfg!(windows) {
        return String::new();
    }
    match (std::env::var("WAYLAND_DISPLAY"), std::env::var("DISPLAY")) {
        (Ok(wayland), _) if !wayland.is_empty() => format!("wayland:{wayland}"),
        (_, Ok(x11)) if !x11.is_empty() => format!("x11:{x11}"),
        _ => String::new(),
    }
}

impl Endpoint {
    fn discover() -> Result<Self, String> {
        let executable = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| format!("cannot locate the browser executable: {error}"))?;
        let profile = trust::storage::Paths::bookmarks_file()?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| String::from("the profile has no directory"))?;
        let key = instance_key(&executable, &profile, &display_name());
        Self::for_key(&key, &profile)
    }

    #[cfg(unix)]
    fn for_key(key: &str, profile: &Path) -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        let directory = match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
            Some(runtime) if runtime.is_absolute() => runtime.join("trust"),
            // XDG Base Directory 0.8: "applications should fall back to a
            // replacement directory with similar capabilities and print a
            // warning message". The profile directory is the user's own.
            _ => {
                eprintln!(
                    "trust-desktop: XDG_RUNTIME_DIR is not set; keeping the browser's \
                     socket in {}",
                    profile.display()
                );
                profile.to_path_buf()
            }
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
        Ok(Self {
            lock: directory.join(format!("desktop-{key}.lock")),
            address: directory.join(format!("desktop-{key}.sock")),
        })
    }

    #[cfg(windows)]
    fn for_key(key: &str, profile: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(profile)
            .map_err(|error| format!("cannot create {}: {error}", profile.display()))?;
        Ok(Self {
            lock: profile.join(format!("desktop-{key}.lock")),
            address: PathBuf::from(format!(r"\\.\pipe\trust-desktop-{key}")),
        })
    }

    /// Take the lock and open the endpoint: `None` when another process
    /// holds the lock.
    fn try_serve(&self) -> Result<Option<Server>, String> {
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock)
            .map_err(|error| format!("cannot open {}: {error}", self.lock.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!("cannot lock {}: {error}", self.lock.display()));
            }
        }
        let endpoint = Endpoint {
            lock: self.lock.clone(),
            address: self.address.clone(),
        };
        #[cfg(unix)]
        {
            // The lock proves that no browser answers here: whatever socket
            // remains was left by one that ended.
            let _ = std::fs::remove_file(&self.address);
            let listener = std::os::unix::net::UnixListener::bind(&self.address)
                .map_err(|error| format!("cannot listen on {}: {error}", self.address.display()))?;
            Ok(Some(Server {
                endpoint,
                lock,
                listener: Some(listener),
                task: None,
            }))
        }
        #[cfg(windows)]
        Ok(Some(Server {
            endpoint,
            lock,
            task: None,
        }))
    }

    /// Send `launch` to the browser holding the lock.
    fn hand_off(&self, launch: &NewWindow) -> Result<(), HandOff> {
        let started = Instant::now();
        let stream = loop {
            match self.connect() {
                Ok(stream) => break stream,
                Err(error) if started.elapsed() < CONNECT_PATIENCE => {
                    let _ = error;
                    std::thread::sleep(CONNECT_RETRY);
                }
                Err(error) => {
                    return Err(HandOff::Failed(format!(
                        "the running browser does not answer at {}: {error}",
                        self.address.display()
                    )));
                }
            }
        };
        exchange(stream, launch)
    }

    #[cfg(unix)]
    fn connect(&self) -> std::io::Result<std::os::unix::net::UnixStream> {
        let stream = std::os::unix::net::UnixStream::connect(&self.address)?;
        stream.set_read_timeout(Some(HAND_OFF_TIMEOUT))?;
        stream.set_write_timeout(Some(HAND_OFF_TIMEOUT))?;
        Ok(stream)
    }

    #[cfg(windows)]
    fn connect(&self) -> std::io::Result<std::fs::File> {
        // The browser may take the foreground for the window it opens: this
        // launch, which the user just started, may grant that.
        // SAFETY: a plain Win32 call without pointers.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
                windows_sys::Win32::UI::WindowsAndMessaging::ASFW_ANY,
            );
        }
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.address)
    }
}

/// Write the request line and read the answer line.
fn exchange<S: std::io::Read + Write>(mut stream: S, launch: &NewWindow) -> Result<(), HandOff> {
    let failed = |error: std::io::Error| HandOff::Failed(format!("hand-off failed: {error}"));
    let mut line = serde_json::to_vec(&LaunchRequest {
        version: PROTOCOL_VERSION,
        address: launch.address.clone(),
        activation_token: launch.activation_token.clone(),
    })
    .map_err(|error| HandOff::Failed(error.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line).map_err(failed)?;
    stream.flush().map_err(failed)?;
    let mut answer = String::new();
    BufReader::new(stream.take(64 * 1024))
        .read_line(&mut answer)
        .map_err(failed)?;
    if answer.is_empty() {
        // The browser ended the connection unanswered: it is closing.
        return Err(HandOff::Closing);
    }
    let reply: LaunchReply = serde_json::from_str(&answer)
        .map_err(|error| HandOff::Failed(format!("unreadable answer: {error}")))?;
    if reply.opened {
        Ok(())
    } else {
        Err(HandOff::Failed(reply.error.unwrap_or_else(|| {
            String::from("the browser refused the window")
        })))
    }
}

/// The browser's answer to one launch request; none, which ends the
/// connection unanswered, once the browser is closing (the launch then
/// tries to become the browser itself).
fn answer(request: &[u8], open: &OpenWindow) -> Option<LaunchReply> {
    let refuse = |error: String| {
        Some(LaunchReply {
            opened: false,
            error: Some(error),
        })
    };
    let request: LaunchRequest = match serde_json::from_slice(request) {
        Ok(request) => request,
        Err(error) => return refuse(format!("unreadable launch request: {error}")),
    };
    if request.version != PROTOCOL_VERSION {
        return refuse(format!(
            "launch protocol {} is not this browser's ({PROTOCOL_VERSION})",
            request.version
        ));
    }
    let window = NewWindow {
        address: request.address.filter(|address| !address.is_empty()),
        activation_token: request.activation_token,
        size: None,
    };
    open(window).then_some(LaunchReply {
        opened: true,
        error: None,
    })
}

/// Read one request line from `stream` and answer it.
async fn serve_connection<S>(stream: S, open: OpenWindow)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(reader.take(MAX_REQUEST_BYTES));
    let mut request = Vec::new();
    match tokio::time::timeout(HAND_OFF_TIMEOUT, reader.read_until(b'\n', &mut request)).await {
        Ok(Ok(read)) if read > 0 => {}
        _ => return,
    }
    let Some(reply) = answer(&request, &open) else {
        return;
    };
    let mut reply = serde_json::to_vec(&reply).unwrap_or_default();
    reply.push(b'\n');
    let _ = tokio::time::timeout(HAND_OFF_TIMEOUT, async {
        writer.write_all(&reply).await?;
        writer.flush().await
    })
    .await;
}

impl Server {
    /// Answer later launches on `runtime` until the server drops.
    pub(super) fn serve(&mut self, runtime: &Handle, open: OpenWindow) {
        #[cfg(unix)]
        {
            let Some(listener) = self.listener.take() else {
                return;
            };
            let _guard = runtime.enter();
            let listener = match listener
                .set_nonblocking(true)
                .and_then(|()| tokio::net::UnixListener::from_std(listener))
            {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!("trust-desktop: cannot serve later launches: {error}");
                    return;
                }
            };
            self.task = Some(runtime.spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            tokio::spawn(serve_connection(stream, open.clone()));
                        }
                        // Out of descriptors, say: wait rather than spin.
                        Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
                    }
                }
            }));
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let name = self.endpoint.address.clone();
            let _guard = runtime.enter();
            let first = match ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)
            {
                Ok(server) => server,
                Err(error) => {
                    eprintln!("trust-desktop: cannot serve later launches: {error}");
                    return;
                }
            };
            self.task = Some(runtime.spawn(async move {
                let mut server = first;
                loop {
                    let connected = server.connect().await;
                    // The next client connects to a fresh instance.
                    let next = loop {
                        match ServerOptions::new()
                            .reject_remote_clients(true)
                            .create(&name)
                        {
                            Ok(next) => break next,
                            Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
                        }
                    };
                    let previous = std::mem::replace(&mut server, next);
                    match connected {
                        Ok(()) => {
                            tokio::spawn(serve_connection(previous, open.clone()));
                        }
                        Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
                    }
                }
            }));
        }
    }
}

impl Drop for Server {
    /// The browser is ending: later launches start their own.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.endpoint.address);
        let _ = self.lock.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_keys_separate_executables_profiles_and_displays() {
        let key = |exe: &str, profile: &str, display: &str| {
            instance_key(Path::new(exe), Path::new(profile), display)
        };
        let base = key(
            "/usr/bin/trust-desktop",
            "/home/u/.local/share/trust",
            "wayland:wayland-0",
        );
        assert_eq!(base.len(), 20);
        assert_eq!(
            base,
            key(
                "/usr/bin/trust-desktop",
                "/home/u/.local/share/trust",
                "wayland:wayland-0"
            ),
            "a key is stable"
        );
        for other in [
            key(
                "/src/target/release/trust-desktop",
                "/home/u/.local/share/trust",
                "wayland:wayland-0",
            ),
            key(
                "/usr/bin/trust-desktop",
                "/tmp/profile",
                "wayland:wayland-0",
            ),
            key(
                "/usr/bin/trust-desktop",
                "/home/u/.local/share/trust",
                "wayland:wayland-1",
            ),
            // Length prefixes keep the parts from running into each other.
            key(
                "/usr/bin/trust-desktop/",
                "home/u/.local/share/trust",
                "wayland:wayland-0",
            ),
        ] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn launches_send_local_paths_as_file_urls() {
        let directory = std::env::temp_dir().join(format!("trust-launch-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("page.html");
        std::fs::write(&file, "<p>x</p>").unwrap();
        let sent = absolute_address(Some(file.to_string_lossy().into_owned())).unwrap();
        assert!(sent.starts_with("file:///"), "{sent}");
        assert_eq!(
            absolute_address(Some(String::from("https://example.com/"))).as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(
            absolute_address(Some(String::from("gemini://example.org/"))).as_deref(),
            Some("gemini://example.org/")
        );
        assert_eq!(absolute_address(None), None);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    fn recording() -> (OpenWindow, Arc<Mutex<Vec<NewWindow>>>) {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&opened);
        let open: OpenWindow = Arc::new(move |window| {
            record.lock().unwrap().push(window);
            true
        });
        (open, opened)
    }

    #[test]
    fn a_browser_answers_launches_and_refuses_unknown_protocols() {
        let (open, opened) = recording();
        let reply = answer(
            br#"{"version":1,"address":"https://example.com/","activation_token":"t"}"#,
            &open,
        )
        .unwrap();
        assert!(reply.opened, "{reply:?}");
        let window = opened.lock().unwrap().pop().unwrap();
        assert_eq!(window.address.as_deref(), Some("https://example.com/"));
        assert_eq!(window.activation_token.as_deref(), Some("t"));
        let refused = answer(
            br#"{"version":99,"address":null,"activation_token":null}"#,
            &open,
        )
        .unwrap();
        assert!(!refused.opened);
        assert!(refused.error.unwrap().contains("99"));
        assert!(!answer(b"not json", &open).unwrap().opened);
        // A closing browser ends the connection unanswered.
        let closing: OpenWindow = Arc::new(|_| false);
        assert!(
            answer(
                br#"{"version":1,"address":null,"activation_token":null}"#,
                &closing
            )
            .is_none()
        );
        assert!(opened.lock().unwrap().is_empty());
    }

    #[test]
    fn the_first_launch_serves_and_later_launches_hand_off() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let directory = std::env::temp_dir().join(format!("trust-instance-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let endpoint = || Endpoint {
            lock: directory.join("desktop-test.lock"),
            #[cfg(unix)]
            address: directory.join("desktop-test.sock"),
            #[cfg(windows)]
            address: PathBuf::from(format!(
                r"\\.\pipe\trust-desktop-test-{}",
                std::process::id()
            )),
        };
        // A socket left by a browser that ended does not stop the next.
        #[cfg(unix)]
        {
            drop(std::os::unix::net::UnixListener::bind(endpoint().address).unwrap());
            assert!(endpoint().address.exists());
        }
        let mut server = endpoint().try_serve().unwrap().expect("the lock is free");
        assert!(
            endpoint().try_serve().unwrap().is_none(),
            "a second launch finds the lock taken"
        );
        let (open, opened) = recording();
        server.serve(runtime.handle(), open);
        let launch = NewWindow {
            address: Some(String::from("gopher://example.org/")),
            activation_token: Some(String::from("token")),
            size: None,
        };
        for _ in 0..2 {
            assert!(endpoint().hand_off(&launch).is_ok());
            let window = opened.lock().unwrap().pop().expect("the browser opened it");
            assert_eq!(window.address.as_deref(), Some("gopher://example.org/"));
            assert_eq!(window.activation_token.as_deref(), Some("token"));
        }
        // A browser that ended released the lock (and its socket).
        drop(server);
        #[cfg(unix)]
        assert!(!endpoint().address.exists());
        let server = endpoint().try_serve().unwrap();
        assert!(server.is_some(), "the next launch becomes the browser");
        drop(server);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
