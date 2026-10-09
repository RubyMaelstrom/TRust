//! XDG Base Directory Specification 0.8, §§2–4 (2021-05-08).
//! https://specifications.freedesktop.org/basedir/latest/
//! Resolve paths without creating anything. Existing TLS paths keep their
//! established overrides; migration is deliberately a separate operation.
//! Windows defaults follow Microsoft's KNOWNFOLDERID definitions for
//! RoamingAppData (%APPDATA%) and LocalAppData (%LOCALAPPDATA%):
//! https://learn.microsoft.com/windows/win32/shell/knownfolderid
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}
impl Paths {
    pub fn discover() -> Result<Self, String> {
        Self::resolve(|name| std::env::var_os(name).map(PathBuf::from))
    }
    /// Resolve only the directory needed by this operation. An explicit
    /// XDG_DATA_HOME works even when HOME and the other XDG variables are absent.
    pub fn bookmarks_file() -> Result<PathBuf, String> {
        directory(&mut environment, "XDG_DATA_HOME", ".local/share")
            .map(|p| p.join("bookmarks.json"))
    }
    fn resolve(mut get: impl FnMut(&str) -> Option<PathBuf>) -> Result<Self, String> {
        Ok(Self {
            config: directory(&mut get, "XDG_CONFIG_HOME", ".config")?,
            data: directory(&mut get, "XDG_DATA_HOME", ".local/share")?,
            state: directory(&mut get, "XDG_STATE_HOME", ".local/state")?,
            cache: directory(&mut get, "XDG_CACHE_HOME", ".cache")?,
        })
    }
    pub fn bookmarks(&self) -> PathBuf {
        self.data.join("bookmarks.json")
    }
    pub fn report(&self) -> String {
        let show = |path: Option<PathBuf>| {
            path.map_or_else(|| String::from("unavailable"), |p| p.display().to_string())
        };
        let dict_servers = crate::dict::credentials::servers();
        format!(
            "Bookmarks: {}\nSite data: {}\nConfig: {}\nData: {}\nState (reserved): {}\nCache (reserved): {}\nGemini identities: {}\nGemini capsule pins: {}\nDICT credentials: {}{}",
            self.bookmarks().display(),
            self.data.join("site-data").display(),
            self.config.display(),
            self.data.display(),
            self.state.display(),
            self.cache.display(),
            show(crate::gemini::identity::directory()),
            show(crate::gemini::tofu::store_path()),
            show(crate::dict::credentials::store_path()),
            if dict_servers.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    dict_servers
                        .iter()
                        .map(|(server, user)| format!("{user}@{server}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        )
    }
}
fn environment(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

pub(crate) fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    let name = "USERPROFILE";
    #[cfg(not(windows))]
    let name = "HOME";
    environment(name).filter(|p| p.is_absolute())
}

pub(crate) fn config_directory() -> Result<PathBuf, String> {
    directory(&mut environment, "XDG_CONFIG_HOME", ".config")
}

fn directory(
    get: &mut impl FnMut(&str) -> Option<PathBuf>,
    name: &str,
    unix_fallback: &str,
) -> Result<PathBuf, String> {
    // XDG §2: relative/empty overrides must be ignored, including on Windows.
    if let Some(path) = get(name).filter(|p| p.is_absolute()) {
        return Ok(path.join("trust"));
    }
    #[cfg(windows)]
    {
        let _ = unix_fallback;
        let (variable, fallback, suffix) = match name {
            "XDG_CONFIG_HOME" | "XDG_DATA_HOME" => ("APPDATA", "AppData/Roaming", ""),
            "XDG_STATE_HOME" => ("LOCALAPPDATA", "AppData/Local", "state"),
            _ => ("LOCALAPPDATA", "AppData/Local", "cache"),
        };
        get(variable)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                get("USERPROFILE")
                    .filter(|p| p.is_absolute())
                    .map(|p| p.join(fallback))
            })
            .map(|p| p.join("trust").join(suffix))
            .ok_or_else(|| {
                format!("Set an absolute {name}, {variable}, or USERPROFILE to store TRust data")
            })
    }
    #[cfg(not(windows))]
    {
        get("HOME")
            .filter(|p| p.is_absolute())
            .map(|p| p.join(unix_fallback).join("trust"))
            .ok_or_else(|| format!("Set an absolute {name} or HOME to store TRust data"))
    }
}
pub(crate) fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
pub(crate) fn private_file() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
/// Replace `path` with `bytes`, readable only by the user. A private
/// temporary file beside it is synced and then renamed over the old file,
/// so a reader sees the old or the new contents, never a partial file.
pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let directory = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no directory")
    })?;
    create_private_dir(directory)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = directory.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = private_file().create_new(true).open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_atomic_writes_replace_contents_with_owner_only_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("secret.txt");
        write_private_atomic(&path, b"first").unwrap();
        write_private_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
    #[test]
    fn explicit_data_home_does_not_require_other_directories() {
        let root = std::env::temp_dir().join("trust-path-test");
        assert_eq!(
            directory(
                &mut |name| (name == "XDG_DATA_HOME").then(|| root.clone()),
                "XDG_DATA_HOME",
                ".local/share"
            )
            .unwrap(),
            root.join("trust")
        );
    }
    #[cfg(not(windows))]
    #[test]
    fn xdg_defaults_and_invalid_relative_paths() {
        let p = Paths::resolve(|key| match key {
            "HOME" => Some("/home/test".into()),
            "XDG_DATA_HOME" => Some("relative".into()),
            "XDG_CONFIG_HOME" => Some("".into()),
            "XDG_STATE_HOME" => Some("/state".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(p.data, Path::new("/home/test/.local/share/trust"));
        assert_eq!(p.config, Path::new("/home/test/.config/trust"));
        assert_eq!(p.state, Path::new("/state/trust"));
        assert_eq!(p.cache, Path::new("/home/test/.cache/trust"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_directories_work_without_home_or_xdg() {
        let p = Paths::resolve(|key| match key {
            "APPDATA" => Some(r"D:\Roaming".into()),
            "LOCALAPPDATA" => Some(r"E:\Local".into()),
            "XDG_CONFIG_HOME" => Some("relative".into()),
            "XDG_DATA_HOME" => Some(r"F:\BrowserData".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(p.config, Path::new(r"D:\Roaming\trust"));
        assert_eq!(p.data, Path::new(r"F:\BrowserData\trust"));
        assert_eq!(p.state, Path::new(r"E:\Local\trust\state"));
        assert_eq!(p.cache, Path::new(r"E:\Local\trust\cache"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_profile_defaults_reject_drive_relative_paths() {
        let p = Paths::resolve(|key| match key {
            "USERPROFILE" => Some(r"C:\Users\Ruby".into()),
            "APPDATA" => Some(r"C:relative".into()),
            "LOCALAPPDATA" => Some(r"\rooted-without-drive".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(p.config, Path::new(r"C:\Users\Ruby\AppData\Roaming\trust"));
        assert_eq!(p.data, p.config);
        assert_eq!(
            p.state,
            Path::new(r"C:\Users\Ruby\AppData\Local\trust\state")
        );
        assert_eq!(
            p.cache,
            Path::new(r"C:\Users\Ruby\AppData\Local\trust\cache")
        );
    }

    #[test]
    fn missing_directories_do_not_fall_back_to_working_directory() {
        assert!(Paths::resolve(|_| None).is_err());
        assert!(
            directory(
                &mut |_| Some("relative".into()),
                "XDG_DATA_HOME",
                ".local/share"
            )
            .is_err()
        );
    }
}
