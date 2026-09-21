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
        format!(
            "Bookmarks: {}\nSite data: {}\nConfig: {}\nData: {}\nState (reserved): {}\nCache (reserved): {}",
            self.bookmarks().display(),
            self.data.join("site-data").display(),
            self.config.display(),
            self.data.display(),
            self.state.display(),
            self.cache.display()
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
#[cfg(test)]
mod tests {
    use super::*;
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
