//! Gemini 0.24.1 §Client certificates (status 60–62): identities the user
//! creates, chooses and scopes (local copy: /big/web-standards/documents/
//! gemini-protocol-*/, retrieved 2026-10-08).
//!
//! * Status 60: a certificate's scope "should be limited to the host and port
//!   from which the status code was received and the path of the URL in the
//!   original request plus all paths below it. A server MAY require a
//!   different certificate for a different path on the same host and port."
//!   An identity therefore holds any number of scopes, and a request presents
//!   the identity whose matching scope is the most specific (longest path).
//! * "Clients MUST NOT automatically generate a client certificate and use it
//!   to repeat the request without the active involvement of the user", and
//!   must not use one for another host, port or a path above its scope
//!   "unless directed to do so by the user". Every scope recorded here comes
//!   from an explicit choice; nothing is inferred from a filename.
//! * Status 61/62: "the request should be repeated with a different
//!   certificate". Choosing another identity for the exact scope supersedes
//!   the one that was rejected, which no longer holds that scope.
//!
//! Storage: the `identities` directory beside TRust's other TLS state
//! (`TRUST_IDENTITIES` overrides it). An identity is a PEM file holding a
//! certificate and its private key, owner-only (0600); its scopes are listed
//! one `gemini://host[:port]/path` per line in `<stem>.scopes`. PEM files
//! from older TRust versions or from openssl (`<host>.pem`, blocks in any
//! order) are listed too and need an explicit first authorization.

use super::GeminiUrl;
use crate::tls::x509;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

/// Identity files considered; a bound on directory scans.
pub const MAX_IDENTITIES: usize = 256;
const MAX_SCOPES_FILE: u64 = 1024 * 1024;
const MAX_PEM_FILE: u64 = 1024 * 1024;
const MAX_NAME: usize = 64;

#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    pub path: PathBuf,
    /// The certificate's common name, else the file's stem.
    pub name: String,
    /// SHA-256 of the certificate's DER encoding.
    pub fingerprint: [u8; 32],
    pub certificate: Option<x509::Certificate>,
    pub scopes: Vec<GeminiUrl>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("name", &self.name)
            .field(
                "fingerprint",
                &super::tofu::fingerprint_hex(&self.fingerprint),
            )
            .finish_non_exhaustive()
    }
}

impl Identity {
    pub fn scopes_path(&self) -> PathBuf {
        self.path.with_extension("scopes")
    }

    /// The scope of this identity that covers `url`, if any.
    pub fn scope_for(&self, url: &GeminiUrl) -> Option<&GeminiUrl> {
        self.scopes
            .iter()
            .filter(|scope| {
                scope.host == url.host
                    && scope.port == url.port
                    && scope_contains(&scope.path, &url.path)
            })
            .max_by_key(|scope| specificity(&scope.path))
    }

    pub fn summary(&self) -> String {
        let mut parts = vec![format!(
            "SHA-256 {}…",
            &super::tofu::fingerprint_text(&self.fingerprint)[..23]
        )];
        if let Some(to) = self.certificate.as_ref().and_then(|c| c.not_after) {
            parts.push(format!(
                "valid until {}{}",
                x509::date(to),
                if to < x509::now() { " (expired)" } else { "" }
            ));
        }
        parts.push(match self.scopes.len() {
            0 => "not used anywhere yet".into(),
            1 => format!("used for {}", self.scopes[0]),
            n => format!("used for {} and {} more", self.scopes[0], n - 1),
        });
        parts.join(" · ")
    }
}

pub fn directory() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TRUST_IDENTITIES") {
        return Some(PathBuf::from(dir));
    }
    // Unit tests never touch the user's identities.
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join(format!("trust-test-identities-{}", std::process::id())))
    }
    #[cfg(not(test))]
    {
        crate::tls::config_directory().map(|dir| dir.join("identities"))
    }
}

/// Conservative decoding prevents an encoded traversal from widening consent:
/// both the URI paths and their decoded forms must agree, and ambiguous
/// escapes never grant a scope.
pub(crate) fn scope_contains(scope: &str, path: &str) -> bool {
    fn canonical(path: &str) -> Option<String> {
        let mut bytes = Vec::new();
        let mut iter = path.bytes();
        while let Some(byte) = iter.next() {
            bytes.push(if byte == b'%' {
                ((iter.next()? as char).to_digit(16)? * 16 + (iter.next()? as char).to_digit(16)?)
                    as u8
            } else {
                byte
            });
        }
        let decoded = String::from_utf8(bytes).ok()?;
        if decoded.contains(['%', '\\']) || decoded.chars().any(char::is_control) {
            return None;
        }
        Some(super::normalize_path(&decoded))
    }
    fn contains(scope: &str, path: &str) -> bool {
        path == scope || path.starts_with(&format!("{}/", scope.trim_end_matches('/')))
    }
    if !contains(&super::normalize_path(scope), &super::normalize_path(path)) {
        return false;
    }
    let (Some(scope), Some(path)) = (canonical(scope), canonical(path)) else {
        return false;
    };
    contains(&scope, &path)
}

fn specificity(path: &str) -> usize {
    super::normalize_path(path).trim_end_matches('/').len()
}

/// The canonical scope recorded for a request URL: origin and path only.
pub fn scope_of(url: &GeminiUrl) -> GeminiUrl {
    GeminiUrl::new(url.host.clone(), url.port, url.path.clone())
}

fn same_scope(a: &GeminiUrl, b: &GeminiUrl) -> bool {
    a.host == b.host
        && a.port == b.port
        && super::normalize_path(&a.path).trim_end_matches('/')
            == super::normalize_path(&b.path).trim_end_matches('/')
}

fn read_scopes(path: &Path) -> Result<Vec<GeminiUrl>, String> {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
        Ok(metadata) if metadata.len() > MAX_SCOPES_FILE => {
            return Err(format!("{}: exceeds its 1 MiB limit", path.display()));
        }
        Ok(_) => {}
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text
        .lines()
        .filter_map(GeminiUrl::parse)
        .map(|url| scope_of(&url))
        .collect())
}

fn write_scopes(identity: &Identity) -> Result<(), String> {
    let path = identity.scopes_path();
    if identity.scopes.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("{}: {error}", path.display()))
            }
            _ => Ok(()),
        };
    }
    let text: String = identity
        .scopes
        .iter()
        .map(|scope| format!("{scope}\n"))
        .collect();
    if text.len() as u64 > MAX_SCOPES_FILE {
        return Err("Gemini identity scope file exceeds its 1 MiB limit.".into());
    }
    crate::storage::write_private_atomic(&path, text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn read_pem(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if metadata.len() > MAX_PEM_FILE {
        return Err(format!("{}: exceeds 1 MiB", path.display()));
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Read one identity file. A file that exists but cannot be used is an
/// error the user must hear about, never a silent anonymous request.
fn load(path: &Path) -> Result<Identity, String> {
    let pem = read_pem(path)?;
    let show = path.display();
    let certificate = CertificateDer::pem_slice_iter(&pem)
        .next()
        .ok_or_else(|| format!("{show}: no CERTIFICATE block"))?
        .map_err(|e| format!("{show}: {e}"))?;
    let parsed = x509::parse(certificate.as_ref());
    let name = parsed
        .as_ref()
        .and_then(|c| c.common_name.clone())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    Ok(Identity {
        path: path.to_path_buf(),
        name: name
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_NAME)
            .collect(),
        fingerprint: Sha256::digest(certificate.as_ref()).into(),
        certificate: parsed,
        scopes: read_scopes(&path.with_extension("scopes"))?,
    })
}

type KeyPair = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>);

/// The certificate chain and private key to present for an identity.
pub(crate) fn key_pair(identity: &Identity) -> Result<KeyPair, String> {
    let pem = read_pem(&identity.path)?;
    let show = identity.path.display();
    let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{show}: {e}"))?;
    if certificates.is_empty() {
        return Err(format!("{show}: no CERTIFICATE block"));
    }
    if <[u8; 32]>::from(Sha256::digest(certificates[0].as_ref())) != identity.fingerprint {
        return Err(format!(
            "{show}: the identity changed on disk; choose it again."
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(&pem)
        .map_err(|e| format!("{show}: no usable private key ({e})"))?;
    Ok((certificates, key))
}

fn pem_files() -> Vec<PathBuf> {
    let Some(dir) = directory() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .take(MAX_IDENTITIES * 4)
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|e| e == "pem")
                && !path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    files.sort();
    files.truncate(MAX_IDENTITIES);
    files
}

/// Every readable identity, and a description of each file that is not.
pub fn list() -> (Vec<Identity>, Vec<String>) {
    let mut identities = Vec::new();
    let mut problems = Vec::new();
    for path in pem_files() {
        match load(&path) {
            Ok(identity) => identities.push(identity),
            Err(problem) => problems.push(problem),
        }
    }
    (identities, problems)
}

pub fn find(fingerprint: &[u8; 32]) -> Result<Identity, String> {
    list()
        .0
        .into_iter()
        .find(|identity| &identity.fingerprint == fingerprint)
        .ok_or_else(|| "That identity no longer exists.".into())
}

/// The identity to present for `url`: the most specific explicit scope wins.
/// A scoped identity whose file is unusable is an error.
pub fn select(url: &GeminiUrl) -> Result<Option<Identity>, String> {
    let mut best: Option<(usize, Identity)> = None;
    for path in pem_files() {
        let scopes = read_scopes(&path.with_extension("scopes"))?;
        let Some(length) = scopes
            .iter()
            .filter(|scope| {
                scope.host == url.host
                    && scope.port == url.port
                    && scope_contains(&scope.path, &url.path)
            })
            .map(|scope| specificity(&scope.path))
            .max()
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(best, _)| length > *best) {
            best = Some((length, load(&path)?));
        }
    }
    Ok(best.map(|(_, identity)| identity))
}

/// Grant `identity` the scope of `url`. The same exact scope is withdrawn
/// from every other identity, so the user's latest choice is what is sent.
pub fn authorize(identity: &Identity, url: &GeminiUrl) -> Result<Identity, String> {
    url.request()?;
    if !scope_contains(&url.path, &url.path) {
        return Err("Cannot safely authorize this ambiguous Gemini path.".into());
    }
    let scope = scope_of(url);
    for mut other in list().0 {
        if other.fingerprint != identity.fingerprint
            && other.scopes.iter().any(|s| same_scope(s, &scope))
        {
            other.scopes.retain(|s| !same_scope(s, &scope));
            write_scopes(&other)?;
        }
    }
    let mut identity = load(&identity.path)?;
    if !identity.scopes.iter().any(|s| same_scope(s, &scope)) {
        identity.scopes.push(scope);
        write_scopes(&identity)?;
    }
    Ok(identity)
}

fn file_stem(host: &str) -> String {
    let stem: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-') {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_matches('.');
    if stem.is_empty() {
        "identity".into()
    } else {
        stem.into()
    }
}

/// The name a new identity's certificate carries (its common name). It is
/// sent to the capsule, so nothing is filled in from the local account.
pub fn validate_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    let name = if name.is_empty() { "anonymous" } else { name };
    if name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(format!(
            "An identity name is one line of at most {MAX_NAME} characters."
        ));
    }
    Ok(name.into())
}

/// Mint a self-signed identity named `name` and authorize it for `url`'s
/// scope. Files are never overwritten: capsules pin the certificate, so
/// replacing one would lose the account it represents.
pub fn create(url: &GeminiUrl, name: &str) -> Result<Identity, String> {
    use std::io::Write as _;
    url.request()?;
    let name = validate_name(name)?;
    let dir = directory().ok_or("No configuration directory is available for identities.")?;
    crate::storage::create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut params =
        rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name.as_str());
    // Gemini §62 rejects certificates whose validity starts in the future;
    // start a day back for clock skew. Capsules pin the certificate, so a
    // near expiry would only lock the user out of the account.
    let yesterday = x509::date(x509::now() - 86_400);
    let (year, rest) = yesterday.split_at(4);
    let (month, day) = (&rest[1..3], &rest[4..6]);
    params.not_before = rcgen::date_time_ymd(
        year.parse().unwrap_or(2025),
        month.parse().unwrap_or(1),
        day.parse().unwrap_or(1),
    );
    params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let cert = params.self_signed(&key).map_err(|e| e.to_string())?;
    let pem = format!("{}{}", cert.pem(), key.serialize_pem());
    // Write privately under a temporary name, then publish with a hard link,
    // which fails rather than replacing an existing identity. Readers never
    // observe an empty or partial key file.
    let stem = file_stem(&url.host);
    let temporary = dir.join(format!(".{stem}.{}.creating", std::process::id()));
    let mut file = crate::storage::private_file()
        .create_new(true)
        .open(&temporary)
        .map_err(|e| format!("{}: {e}", temporary.display()))?;
    let written = file.write_all(pem.as_bytes()).and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    let mut published = None;
    for n in 1..=999 {
        let path = if n == 1 {
            dir.join(format!("{stem}.pem"))
        } else {
            dir.join(format!("{stem}-{n}.pem"))
        };
        match std::fs::hard_link(&temporary, &path) {
            Ok(()) => {
                published = Some(path);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                let _ = std::fs::remove_file(&temporary);
                return Err(format!("{}: {error}", path.display()));
            }
        }
    }
    let _ = std::fs::remove_file(&temporary);
    let path = published.ok_or("Too many identities for this host.")?;
    let identity = load(&path)?;
    authorize(&identity, url)
}

/// Stop presenting `fingerprint` for `scope`.
pub fn forget_scope(fingerprint: &[u8; 32], scope: &GeminiUrl) -> Result<Identity, String> {
    let mut identity = find(fingerprint)?;
    identity.scopes.retain(|s| !same_scope(s, scope));
    write_scopes(&identity)?;
    Ok(identity)
}

/// Scopes a user can choose for a request to `url`, narrowest first: the
/// request's path and everything below it (the status 60 default), each
/// parent directory, and the whole capsule (host and port, path "/"). Any
/// wider choice is the user's explicit direction (§Status 60).
pub fn scope_options(url: &GeminiUrl) -> Vec<GeminiUrl> {
    let path = super::normalize_path(&url.path);
    let mut options = vec![scope_of(url)];
    let mut rest = path.trim_end_matches('/');
    while let Some(cut) = rest.rfind('/') {
        rest = &rest[..cut];
        let parent = GeminiUrl::new(url.host.clone(), url.port, format!("{rest}/"));
        if options.iter().all(|o| !same_scope(o, &parent)) {
            options.push(parent);
        }
    }
    let root = GeminiUrl::new(url.host.clone(), url.port, "/");
    if options.iter().all(|o| !same_scope(o, &root)) {
        options.push(root);
    }
    options.truncate(32);
    options
}

/// Replace `old` with `new` for one identity (the user's explicit choice
/// to widen or narrow where it is sent). Scopes the new one covers on the
/// same host and port become redundant and are removed.
pub fn change_scope(
    fingerprint: &[u8; 32],
    old: &GeminiUrl,
    new: &GeminiUrl,
) -> Result<Identity, String> {
    if old.host != new.host || old.port != new.port {
        return Err("A scope stays on its own host and port.".into());
    }
    let mut identity = find(fingerprint)?;
    identity.scopes.retain(|s| {
        !same_scope(s, old)
            && !(s.host == new.host && s.port == new.port && scope_contains(&new.path, &s.path))
    });
    write_scopes(&identity)?;
    authorize(&identity, new)
}

/// Stop sending any identity for `url`: withdraw the scope that covers it.
/// Returns the identity's name and the scope that was withdrawn.
pub fn revoke(url: &GeminiUrl) -> Result<Option<(String, GeminiUrl)>, String> {
    let Some(identity) = select(url)? else {
        return Ok(None);
    };
    let scope = identity
        .scope_for(url)
        .cloned()
        .ok_or("The identity's scopes changed; try again.")?;
    let updated = forget_scope(&identity.fingerprint, &scope)?;
    Ok(Some((updated.name, scope)))
}

/// Delete an identity's key and scopes for good.
pub fn delete(fingerprint: &[u8; 32]) -> Result<String, String> {
    let identity = find(fingerprint)?;
    std::fs::remove_file(&identity.path)
        .map_err(|e| format!("{}: {e}", identity.path.display()))?;
    let _ = std::fs::remove_file(identity.scopes_path());
    Ok(identity.name)
}

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    // Identity tests share one directory per process (TRUST_IDENTITIES is
    // process-global); scans of it must not race a test's own edits.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_minted_atomically_and_selected_by_the_most_specific_scope() {
        let _lock = test_lock();
        let private = GeminiUrl::new("ids-scope.test", 1965, "/private/");
        let alice = create(&private, "alice").unwrap();
        assert!(alice.path.file_name().unwrap() == "ids-scope.test.pem");
        let pem = std::fs::read_to_string(&alice.path).unwrap();
        assert!(pem.contains("BEGIN CERTIFICATE") && pem.contains("PRIVATE KEY"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&alice.path, &alice.scopes_path()] {
                let mode = std::fs::metadata(path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{}", path.display());
            }
        }
        let cert = alice.certificate.as_ref().unwrap();
        assert!(cert.not_before.unwrap() <= x509::now());
        assert_eq!(alice.name, "alice");
        // Spec example: /private/ and /other/ need different certificates.
        let other = GeminiUrl::new("ids-scope.test", 1965, "/other/");
        let bob = create(&other, "bob").unwrap();
        assert_eq!(bob.path.file_name().unwrap(), "ids-scope.test-2.pem");
        let pick = |path: &str| {
            select(&GeminiUrl::new("ids-scope.test", 1965, path))
                .unwrap()
                .map(|i| i.name)
        };
        assert_eq!(pick("/private/r2/r3").as_deref(), Some("alice"));
        assert_eq!(pick("/other/r1").as_deref(), Some("bob"));
        assert_eq!(pick("/random"), None);
        assert_eq!(pick("/privateer"), None);
        assert_eq!(pick("/private/%2e%2e/other/x"), None);
        assert_eq!(
            select(&GeminiUrl::new("ids-scope.test", 1966, "/private/x")).unwrap(),
            None,
            "another port is another service"
        );
        // 61 on a sub-path: choose bob there; the narrower scope wins and
        // alice keeps the rest of her subtree.
        authorize(&bob, &GeminiUrl::new("ids-scope.test", 1965, "/private/r2")).unwrap();
        assert_eq!(pick("/private/r2/r3").as_deref(), Some("bob"));
        assert_eq!(pick("/private/r1").as_deref(), Some("alice"));
        // Re-choosing alice for that exact scope withdraws it from bob.
        authorize(
            &alice,
            &GeminiUrl::new("ids-scope.test", 1965, "/private/r2/"),
        )
        .unwrap();
        assert_eq!(pick("/private/r2/r3").as_deref(), Some("alice"));
        assert!(
            !find(&bob.fingerprint)
                .unwrap()
                .scopes
                .iter()
                .any(|s| s.path.starts_with("/private"))
        );
        // Widening by explicit choice: bob for the whole capsule. Scopes it
        // now covers are folded in; alice's more specific scopes still win.
        let root = GeminiUrl::new("ids-scope.test", 1965, "/");
        change_scope(&bob.fingerprint, &scope_of(&other), &root).unwrap();
        assert_eq!(pick("/").as_deref(), Some("bob"));
        assert_eq!(pick("/random").as_deref(), Some("bob"));
        assert_eq!(pick("/private/x").as_deref(), Some("alice"));
        assert_eq!(
            find(&bob.fingerprint).unwrap().scopes,
            std::slice::from_ref(&root)
        );
        // Narrowing back, then revoking the scope that covers a page.
        change_scope(&bob.fingerprint, &root, &scope_of(&other)).unwrap();
        assert_eq!(pick("/random"), None);
        let (name, withdrawn) = revoke(&GeminiUrl::new("ids-scope.test", 1965, "/other/x"))
            .unwrap()
            .unwrap();
        assert_eq!((name.as_str(), withdrawn.path.as_str()), ("bob", "/other/"));
        assert!(
            change_scope(
                &bob.fingerprint,
                &root,
                &GeminiUrl::new("x.test", 1965, "/")
            )
            .is_err()
        );
        assert_eq!(pick("/other/r1"), None);
        assert_eq!(delete(&bob.fingerprint).unwrap(), "bob");
        assert!(find(&bob.fingerprint).is_err());
        delete(&alice.fingerprint).unwrap();
    }

    #[test]
    fn scope_options_run_from_the_page_subtree_to_the_whole_capsule() {
        let paths = |url: &str| -> Vec<String> {
            scope_options(&GeminiUrl::parse(url).unwrap())
                .into_iter()
                .map(|s| s.to_string())
                .collect()
        };
        assert_eq!(
            paths("gemini://station.test/join?x"),
            ["gemini://station.test/join", "gemini://station.test/"]
        );
        assert_eq!(
            paths("gemini://e.test:1966/a/b/c"),
            [
                "gemini://e.test:1966/a/b/c",
                "gemini://e.test:1966/a/b/",
                "gemini://e.test:1966/a/",
                "gemini://e.test:1966/"
            ]
        );
        assert_eq!(
            paths("gemini://e.test/a/"),
            ["gemini://e.test/a/", "gemini://e.test/"]
        );
        assert_eq!(paths("gemini://e.test/"), ["gemini://e.test/"]);
        // Boundaries are path segments: /join never covers /joinx.
        assert!(scope_contains("/join", "/join"));
        assert!(scope_contains("/join", "/join/"));
        assert!(scope_contains("/join", "/join/x"));
        assert!(scope_contains("/join/", "/join/x"));
        assert!(!scope_contains("/join", "/joinx"));
        assert!(!scope_contains("/join", "/"));
        assert!(!scope_contains("/join/", "/join"));
        assert!(scope_contains("/", "/anything/at/all"));
    }

    #[test]
    fn existing_pem_files_need_explicit_authorization_and_broken_ones_are_reported() {
        let _lock = test_lock();
        let dir = directory().unwrap();
        crate::storage::create_private_dir(&dir).unwrap();
        // An openssl-style file with the key before the certificate.
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "legacy");
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let path = dir.join("legacy-host.test.pem");
        std::fs::write(&path, format!("{}{}", key.serialize_pem(), cert.pem())).unwrap();
        let url = GeminiUrl::new("legacy-host.test", 1965, "/");
        assert_eq!(select(&url).unwrap(), None, "possession is not consent");
        let legacy = list().0.into_iter().find(|i| i.path == path).unwrap();
        assert_eq!(legacy.name, "legacy");
        authorize(&legacy, &url).unwrap();
        let chosen = select(&url).unwrap().unwrap();
        assert!(key_pair(&chosen).is_ok());
        // Names are one bounded line and never default to the local account.
        assert_eq!(validate_name("  ").unwrap(), "anonymous");
        assert!(validate_name(&"x".repeat(65)).is_err());
        assert!(validate_name("a\nb").is_err());
        // A scoped identity whose file breaks is an error, not anonymity.
        std::fs::write(&path, "not pem at all").unwrap();
        assert!(select(&url).is_err());
        assert!(list().1.iter().any(|p| p.contains("legacy-host.test.pem")));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(path.with_extension("scopes")).unwrap();
    }
}
