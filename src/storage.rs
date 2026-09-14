//! XDG Base Directory Specification 0.8, §§2–4 (2021-05-08).
//! https://specifications.freedesktop.org/basedir/latest/
//! Resolve paths without creating anything. Existing TLS paths keep their
//! established overrides; migration is deliberately a separate operation.
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
        let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
        let home = std::env::var_os("HOME").map(PathBuf::from);
        data_directory(data, home).map(|p| p.join("bookmarks.json"))
    }
    fn resolve(mut get: impl FnMut(&str) -> Option<PathBuf>) -> Result<Self, String> {
        let home = get("HOME").filter(|p| p.is_absolute());
        let mut directory = |name, fallback| -> Result<PathBuf, String> {
            get(name)
                .filter(|p| p.is_absolute())
                .or_else(|| home.as_ref().map(|p| p.join(fallback)))
                .map(|p| p.join("trust"))
                .ok_or_else(|| format!("Set an absolute {name} or HOME to store TRust data"))
        };
        Ok(Self {
            config: directory("XDG_CONFIG_HOME", ".config")?,
            data: directory("XDG_DATA_HOME", ".local/share")?,
            state: directory("XDG_STATE_HOME", ".local/state")?,
            cache: directory("XDG_CACHE_HOME", ".cache")?,
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
fn data_directory(data: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf, String> {
    data.filter(|p| p.is_absolute())
        .or_else(|| {
            home.filter(|p| p.is_absolute())
                .map(|p| p.join(".local/share"))
        })
        .map(|p| p.join("trust"))
        .ok_or_else(|| "Set an absolute XDG_DATA_HOME or HOME to store bookmarks".into())
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
        assert_eq!(
            data_directory(Some("/data".into()), None).unwrap(),
            Path::new("/data/trust")
        );
    }
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
}
