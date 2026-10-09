//! Gemini 0.24.1 §TLS Server certificate validation: trust on first use.
//!
//! "The first time a Gemini client connects to a server, it accepts whichever
//! certificate it is presented. That certificate's fingerprint and expiry date
//! are saved in a persistent database, associated with the server's hostname
//! and port. ... If the fingerprints do not match but the previous
//! certificate's expiry date has not passed, this is considered potential
//! evidence of a Man-In-The-Middle attack." (local copy: /big/web-standards/
//! documents/gemini-protocol-*/, retrieved 2026-10-08).
//!
//! * A pin is the SHA-256 of the end-entity certificate's DER and its
//!   notAfter (RFC 5280 §4.1.2.5), keyed by `host:port` (IPv6 hosts are
//!   bracketed).
//! * A new pin is written only after the TLS handshake proves possession of
//!   the certificate's key (RFC 8446 §4.4.3), so a failed handshake cannot
//!   plant one.
//! * A different certificate after the pinned one expired replaces the pin
//!   (the specification's exception); before that, the connection stops and
//!   the user chooses whether to trust the new certificate.
//! * Pins persist in `gemini_known_hosts` beside TRust's other TLS state
//!   (private 0600 file, atomically replaced), overridable with
//!   `TRUST_GEMINI_KNOWN_HOSTS`. An unreadable store never weakens checks
//!   made within this session.

use crate::tls::x509;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Pins kept at once; the least recently confirmed are dropped beyond this.
pub const MAX_PINS: usize = 4096;
const MAX_FILE: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub fingerprint: [u8; 32],
    /// notAfter of the pinned certificate; `None` when unreadable, which is
    /// treated as unexpired (the cautious reading).
    pub not_after: Option<i64>,
    /// When this certificate was first trusted.
    pub since: i64,
}

/// What the server presented during one handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presented {
    pub key: String,
    pub host: String,
    pub fingerprint: [u8; 32],
    pub certificate: Option<x509::Certificate>,
}

impl Presented {
    pub fn new(host: &str, port: u16, der: &[u8]) -> Self {
        Self {
            key: key(host, port),
            host: host.to_string(),
            fingerprint: Sha256::digest(der).into(),
            certificate: x509::parse(der),
        }
    }
    fn not_after(&self) -> Option<i64> {
        self.certificate.as_ref().and_then(|c| c.not_after)
    }
    /// Facts the user may want before trusting this certificate.
    pub fn summary(&self, now: i64) -> Vec<String> {
        let mut lines = vec![format!("SHA-256 {}", fingerprint_text(&self.fingerprint))];
        match &self.certificate {
            Some(certificate) => {
                if let (Some(from), Some(to)) = (certificate.not_before, certificate.not_after) {
                    lines.push(format!(
                        "Valid {} to {}{}",
                        x509::date(from),
                        x509::date(to),
                        if to < now {
                            " (expired)"
                        } else if from > now {
                            " (not yet valid)"
                        } else {
                            ""
                        }
                    ));
                }
                let names = certificate.names();
                lines.push(if names.is_empty() {
                    "Names no host".into()
                } else {
                    format!(
                        "Names {}{}",
                        names.join(", "),
                        if certificate.names_host(&self.host) {
                            ""
                        } else {
                            " (not this host)"
                        }
                    )
                });
            }
            None => lines.push("The certificate's fields could not be read.".into()),
        }
        lines
    }
}

/// The outcome of comparing a presentation with the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No pin yet: trust it once the handshake completes.
    First,
    /// The pinned certificate.
    Known,
    /// A new certificate after the pinned one expired (spec exception).
    Renewed(Pin),
    /// A different certificate while the pin is unexpired.
    Changed(Pin),
}

pub fn key(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

pub fn fingerprint_text(fingerprint: &[u8; 32]) -> String {
    fingerprint
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn fingerprint_hex(fingerprint: &[u8; 32]) -> String {
    fingerprint.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_fingerprint(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

struct Store {
    pins: BTreeMap<String, Pin>,
    path: Option<PathBuf>,
    /// Pins this process removed, so a merge does not bring them back.
    forgotten: std::collections::BTreeSet<String>,
}

fn path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TRUST_GEMINI_KNOWN_HOSTS") {
        return Some(PathBuf::from(path));
    }
    // Unit tests never touch the user's real trust store.
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join(format!(
            "trust-test-gemini-known-hosts-{}",
            std::process::id()
        )))
    }
    #[cfg(not(test))]
    {
        crate::tls::config_directory().map(|dir| dir.join("gemini_known_hosts"))
    }
}

/// `host:port sha256-hex not-after|- since`, one pin per line.
fn parse_store(text: &str) -> BTreeMap<String, Pin> {
    let mut pins = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_ascii_whitespace();
        let (Some(key), Some(fingerprint), Some(not_after), Some(since)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(fingerprint) = parse_fingerprint(fingerprint) else {
            continue;
        };
        let not_after = if not_after == "-" {
            None
        } else {
            match not_after.parse() {
                Ok(value) => Some(value),
                Err(_) => continue,
            }
        };
        let Ok(since) = since.parse() else { continue };
        if pins.len() < MAX_PINS && key.len() <= 300 && !key.contains(char::is_control) {
            pins.insert(
                key.to_string(),
                Pin {
                    fingerprint,
                    not_after,
                    since,
                },
            );
        }
    }
    pins
}

fn format_store(pins: &BTreeMap<String, Pin>) -> String {
    let mut out = String::from(
        "# TRust Gemini TOFU pins (Gemini specification, TLS server certificate validation)\n\
         # <host:port> <sha256 of the certificate> <notAfter, Unix seconds> <first trusted>\n",
    );
    for (key, pin) in pins {
        out.push_str(&format!(
            "{key} {} {} {}\n",
            fingerprint_hex(&pin.fingerprint),
            pin.not_after
                .map_or_else(|| "-".to_string(), |t| t.to_string()),
            pin.since
        ));
    }
    out
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(|| {
        let path = path();
        let pins = path
            .as_deref()
            .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() <= MAX_FILE))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| parse_store(&text))
            .unwrap_or_default();
        Mutex::new(Store {
            pins,
            path,
            forgotten: Default::default(),
        })
    })
}

/// Persist with private permissions; RAM keeps the pin if the disk refuses.
/// Pins another TRust instance saved meanwhile are merged in, not lost.
fn save(store: &mut Store) -> Result<(), String> {
    let Some(path) = store.path.clone() else {
        return Ok(());
    };
    if let Some(text) = std::fs::metadata(&path)
        .ok()
        .filter(|m| m.len() <= MAX_FILE)
        .and_then(|_| std::fs::read_to_string(&path).ok())
    {
        for (key, pin) in parse_store(&text) {
            if store.pins.len() < MAX_PINS && !store.forgotten.contains(&key) {
                store.pins.entry(key).or_insert(pin);
            }
        }
    }
    crate::storage::write_private_atomic(&path, format_store(&store.pins).as_bytes())
        .map_err(|e| format!("Could not save Gemini certificate pins: {e}"))
}

/// Compare a presentation with its pin. Nothing is written here.
pub fn check(presented: &Presented, now: i64) -> Verdict {
    let store = store().lock().unwrap_or_else(|e| e.into_inner());
    match store.pins.get(&presented.key) {
        None => Verdict::First,
        Some(pin) if pin.fingerprint == presented.fingerprint => Verdict::Known,
        Some(pin) if pin.not_after.is_some_and(|expiry| expiry < now) => {
            Verdict::Renewed(pin.clone())
        }
        Some(pin) => Verdict::Changed(pin.clone()),
    }
}

/// Record a certificate as trusted for its `host:port`: on first use, after
/// the pin expired, or because the user chose to trust a changed one.
pub fn trust(presented: &Presented, now: i64) -> Result<(), String> {
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    let since = match store.pins.get(&presented.key) {
        Some(pin) if pin.fingerprint == presented.fingerprint => pin.since,
        _ => now,
    };
    if !store.pins.contains_key(&presented.key) && store.pins.len() >= MAX_PINS {
        // Drop the oldest trust decision rather than refusing new capsules.
        if let Some(oldest) = store
            .pins
            .iter()
            .min_by_key(|(_, pin)| pin.since)
            .map(|(key, _)| key.clone())
        {
            store.pins.remove(&oldest);
        }
    }
    store.pins.insert(
        presented.key.clone(),
        Pin {
            fingerprint: presented.fingerprint,
            not_after: presented.not_after(),
            since,
        },
    );
    store.forgotten.remove(&presented.key);
    save(&mut store)
}

/// Forget a pin; the next connection trusts what it is shown.
pub fn forget(key: &str) -> Result<bool, String> {
    let mut store = store().lock().unwrap_or_else(|e| e.into_inner());
    let removed = store.pins.remove(key).is_some();
    if removed {
        store.forgotten.insert(key.to_string());
        save(&mut store)?;
    }
    Ok(removed)
}

/// Every pin, for the certificate manager.
pub fn pins() -> Vec<(String, Pin)> {
    let store = store().lock().unwrap_or_else(|e| e.into_inner());
    store
        .pins
        .iter()
        .map(|(key, pin)| (key.clone(), pin.clone()))
        .collect()
}

pub fn store_path() -> Option<PathBuf> {
    store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .path
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presented(host: &str, port: u16, der: &[u8], not_after: Option<i64>) -> Presented {
        let mut presented = Presented::new(host, port, der);
        presented.certificate = Some(x509::Certificate {
            not_after,
            ..Default::default()
        });
        presented
    }

    #[test]
    fn first_use_known_renewal_and_change_follow_the_specification() {
        let first = presented("tofu-rules.test", 1965, b"first", Some(2_000));
        assert_eq!(check(&first, 1_000), Verdict::First);
        trust(&first, 1_000).unwrap();
        assert_eq!(check(&first, 1_500), Verdict::Known);
        // A different certificate before the pin's expiry: possible MITM.
        let second = presented("tofu-rules.test", 1965, b"second", Some(9_000));
        let Verdict::Changed(pin) = check(&second, 1_500) else {
            panic!()
        };
        assert_eq!(pin.fingerprint, first.fingerprint);
        // The same certificate on another port is a separate service.
        assert_eq!(
            check(&presented("tofu-rules.test", 1966, b"second", None), 1_500),
            Verdict::First
        );
        // After the pinned certificate expired, a replacement is accepted.
        assert!(matches!(check(&second, 2_001), Verdict::Renewed(_)));
        trust(&second, 2_001).unwrap();
        assert_eq!(check(&second, 2_002), Verdict::Known);
        // An unreadable expiry is never treated as expired.
        let unknown = presented("tofu-unknown.test", 1965, b"a", None);
        trust(&unknown, 10).unwrap();
        assert!(matches!(
            check(&presented("tofu-unknown.test", 1965, b"b", None), i64::MAX),
            Verdict::Changed(_)
        ));
        assert!(forget("tofu-unknown.test:1965").unwrap());
        assert_eq!(
            check(&presented("tofu-unknown.test", 1965, b"b", None), 11),
            Verdict::First
        );
    }

    #[test]
    fn store_roundtrips_skips_garbage_and_is_private() {
        let mut pins = BTreeMap::new();
        pins.insert(
            "[::1]:1965".to_string(),
            Pin {
                fingerprint: [7; 32],
                not_after: None,
                since: 5,
            },
        );
        pins.insert(
            "capsule.test:1966".to_string(),
            Pin {
                fingerprint: [0xab; 32],
                not_after: Some(-3),
                since: 9,
            },
        );
        let text = format_store(&pins);
        assert_eq!(parse_store(&text), pins);
        let messy = format!(
            "garbage\nhost:1 nothex - 1\nhost:2 {} soon 1\n{text}",
            "a".repeat(64)
        );
        assert_eq!(parse_store(&messy), pins);
        assert_eq!(key("::1", 1965), "[::1]:1965");
        trust(&presented("tofu-file.test", 1965, b"x", None), 1).unwrap();
        let path = store_path().unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("tofu-file.test:1965")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
