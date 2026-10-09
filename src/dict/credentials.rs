//! RFC 2229 §3.11 AUTH credentials, one user per DICT server.
//!
//! "The authentication-string will be computed as in the APOP protocol ...
//! the MD5 checksum of the concatenation of the msg-id (obtained from the
//! initial banner) and the 'shared secret' that is stored in the server and
//! client configuration files." The secret must therefore be kept: it lives
//! in `dict-credentials.json` beside the per-server dictionary preferences
//! (`$XDG_CONFIG_HOME/trust/`), owner-only (0600) and atomically replaced.
//! It is never displayed, logged or sent: only the per-session digest is.
//! §5: "clear text passwords are not permitted in the URL"; a `user;AUTH@`
//! URL names the user, and TRust asks for the secret.

use std::path::PathBuf;

const MAX_FILE: u64 = 64 * 1024;
const MAX_SERVERS: usize = 64;
pub const MAX_SECRET: usize = 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// §3.11: `AUTH username authentication-string` for this session's
    /// banner msg-id (brackets included, §3.1).
    pub fn command(&self, msg_id: &str) -> String {
        let mut input = msg_id.as_bytes().to_vec();
        input.extend_from_slice(self.secret.as_bytes());
        let digest = super::md5::digest(&input);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("AUTH {} {hex}\r\n", self.user)
    }
}

fn path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TRUST_DICT_CREDENTIALS") {
        return Some(PathBuf::from(path));
    }
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join(format!(
            "trust-test-dict-credentials-{}.json",
            std::process::id()
        )))
    }
    #[cfg(not(test))]
    {
        crate::storage::config_directory()
            .ok()
            .map(|dir| dir.join("dict-credentials.json"))
    }
}

/// An AUTH user name is one `atom` parameter (RFC 2229 §§2.2–2.3).
pub fn validate_user(user: &str) -> Result<String, String> {
    let user = user.trim();
    if user.is_empty() || user.len() > 256 || !super::atom(user) {
        return Err("A DICT user name is one word without spaces, quotes or backslashes.".into());
    }
    Ok(user.into())
}

fn validate_secret(secret: &str) -> Result<(), String> {
    if secret.is_empty() {
        return Err("Enter the shared secret configured for this user.".into());
    }
    if secret.len() > MAX_SECRET || secret.contains(['\r', '\n', '\0']) {
        return Err(format!(
            "A DICT shared secret is one line of at most {MAX_SECRET} bytes."
        ));
    }
    Ok(())
}

/// Serializes read-modify-write updates of the credentials file.
fn update_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn read() -> serde_json::Map<String, serde_json::Value> {
    path()
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() <= MAX_FILE))
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn write(map: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let path = path().ok_or("No configuration directory is available.")?;
    let bytes = serde_json::to_vec_pretty(map).map_err(|e| e.to_string())?;
    crate::storage::write_private_atomic(&path, &bytes)
        .map_err(|e| format!("Could not save DICT credentials: {e}"))
}

/// The credentials saved for `server` (`host` or `host:port`).
pub fn lookup(server: &str) -> Option<Credentials> {
    let map = read();
    let entry = map.get(server)?.as_object()?;
    let user = validate_user(entry.get("user")?.as_str()?).ok()?;
    let secret = entry.get("secret")?.as_str()?.to_string();
    validate_secret(&secret).ok()?;
    Some(Credentials { user, secret })
}

pub fn save(server: &str, user: &str, secret: &str) -> Result<(), String> {
    let user = validate_user(user)?;
    validate_secret(secret)?;
    let _lock = update_lock();
    let mut map = read();
    if map.len() >= MAX_SERVERS && !map.contains_key(server) {
        return Err(format!(
            "DICT credentials are kept for at most {MAX_SERVERS} servers; forget one first."
        ));
    }
    let mut entry = serde_json::Map::new();
    entry.insert("user".into(), user.into());
    entry.insert("secret".into(), secret.into());
    map.insert(server.into(), entry.into());
    write(&map)
}

pub fn forget(server: &str) -> Result<bool, String> {
    let _lock = update_lock();
    let mut map = read();
    let removed = map.remove(server).is_some();
    if removed {
        write(&map)?;
    }
    Ok(removed)
}

/// Servers with saved credentials and their users, for `status`.
pub fn servers() -> Vec<(String, String)> {
    read()
        .iter()
        .filter_map(|(server, entry)| {
            Some((
                server.clone(),
                entry.as_object()?.get("user")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// The credentials a lookup authenticates with: those saved for its server,
/// which must be for the user a `user;AUTH@` address names (§5).
pub fn for_target(target: &super::Target) -> Result<Option<Credentials>, String> {
    let saved = lookup(&target.server());
    match &target.user {
        None => Ok(saved),
        Some(user) => match saved {
            Some(credentials) if &credentials.user == user => Ok(Some(credentials)),
            _ => Err(format!(
                "{} asks to authenticate as {user}: enter the shared secret (dict-auth).",
                target.server()
            )),
        },
    }
}

/// A `user;AUTH@` lookup without its secret must ask the user first.
pub fn needs_secret(target: &super::Target) -> bool {
    for_target(target).is_err()
}

pub fn store_path() -> Option<PathBuf> {
    path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apop_style_digest_matches_rfc_1939_example() {
        // RFC 1939 §7 APOP example: the same computation RFC 2229 §3.11
        // prescribes for AUTH.
        let credentials = Credentials {
            user: "mrose".into(),
            secret: "tanstaaf".into(),
        };
        assert_eq!(
            credentials.command("<1896.697170952@dbc.mtview.ca.us>"),
            "AUTH mrose c4c9334bac560ecc979e58001b3e22fb\r\n"
        );
        assert!(!format!("{credentials:?}").contains("tanstaaf"));
    }

    #[test]
    fn credentials_are_validated_saved_privately_and_forgotten() {
        assert!(validate_user("two words").is_err());
        assert!(validate_user("quote\"d").is_err());
        assert!(save("dict-cred.test", "joe", "a\nb").is_err());
        assert!(save("dict-cred.test", "joe", "").is_err());
        save(
            "dict-cred.test:2629",
            "joesmith",
            "a long shared passphrase",
        )
        .unwrap();
        let saved = lookup("dict-cred.test:2629").unwrap();
        assert_eq!(saved.user, "joesmith");
        assert!(
            lookup("dict-cred.test").is_none(),
            "ports are separate servers"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store_path().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(forget("dict-cred.test:2629").unwrap());
        assert!(lookup("dict-cred.test:2629").is_none());
    }
}
