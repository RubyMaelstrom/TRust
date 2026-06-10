//! TLS support built for the small net: rustls with trust-on-first-use.
//!
//! Geminispace and TLS-enabled BBSes overwhelmingly run self-signed
//! certificates, which WebPKI validation would reject outright. Instead
//! we follow the Gemini community convention: accept whatever certificate
//! a host presents the first time, pin its SHA-256 fingerprint, and
//! refuse the connection if the same host:port later presents a
//! different one. Pins persist in `~/.config/trust/known_hosts`
//! (overridable via `TRUST_KNOWN_HOSTS` — the tests point it at a temp
//! file); remove a line there to re-trust a host whose cert changed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};

/// Fingerprint pins keyed by `host:port`, mirrored to the known-hosts
/// file whenever a new pin is added.
struct PinStore {
    pins: HashMap<String, [u8; 32]>,
    path: Option<PathBuf>,
}

static PINS: OnceLock<Mutex<PinStore>> = OnceLock::new();

fn known_hosts_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TRUST_KNOWN_HOSTS") {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/trust/known_hosts"))
}

fn store() -> &'static Mutex<PinStore> {
    PINS.get_or_init(|| {
        let path = known_hosts_path();
        let pins = path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| parse_known_hosts(&text))
            .unwrap_or_default();
        Mutex::new(PinStore { pins, path })
    })
}

/// One line per pin: `host:port <64 hex chars>`. Comments and lines we
/// can't parse are ignored (and dropped on the next rewrite).
fn parse_known_hosts(text: &str) -> HashMap<String, [u8; 32]> {
    let mut pins = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        if let (Some(key), Some(hex)) = (fields.next(), fields.next())
            && let Some(fp) = hex_to_fp(hex)
        {
            pins.insert(key.to_string(), fp);
        }
    }
    pins
}

fn format_known_hosts(pins: &HashMap<String, [u8; 32]>) -> String {
    let mut lines: Vec<String> = pins
        .iter()
        .map(|(key, fp)| format!("{key} {}", hex(fp)))
        .collect();
    lines.sort();
    format!(
        "# TRust TOFU pins: <host:port> <sha256 of the server certificate>\n\
         # Remove a line to re-trust a server whose certificate changed.\n{}\n",
        lines.join("\n")
    )
}

fn hex_to_fp(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut fp = [0u8; 32];
    for (i, byte) in fp.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(fp)
}

/// Best-effort save; a read-only config dir still leaves the pin in RAM.
fn save(store: &PinStore) {
    let Some(path) = &store.path else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, format_known_hosts(&store.pins)).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn fingerprint(cert: &CertificateDer<'_>) -> [u8; 32] {
    Sha256::digest(cert.as_ref()).into()
}

fn hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug)]
struct Tofu {
    /// `host:port` this connection dialed — the pin key. The verifier
    /// callback only sees the SNI name, which lacks the port, so the
    /// key is baked in per connection.
    key: String,
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for Tofu {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let fp = fingerprint(end_entity);
        let mut store = store().lock().unwrap();
        match store.pins.get(&self.key) {
            None => {
                store.pins.insert(self.key.clone(), fp);
                save(&store);
                Ok(ServerCertVerified::assertion())
            }
            Some(pinned) if *pinned == fp => Ok(ServerCertVerified::assertion()),
            Some(pinned) => {
                let file = store
                    .path
                    .as_ref()
                    .map(|p| format!(" — remove its line from {} to re-trust", p.display()))
                    .unwrap_or_default();
                Err(Error::General(format!(
                    "certificate for {} changed since first use \
                     (pinned sha256:{}.., got sha256:{}..){file}",
                    self.key,
                    &hex(pinned)[..16],
                    &hex(&fp)[..16],
                )))
            }
        }
    }

    // The signatures themselves are still verified; only the trust
    // anchor check is replaced by the fingerprint pin above.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        let provider = CryptoProvider::get_default().expect("provider installed");
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        let provider = CryptoProvider::get_default().expect("provider installed");
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

/// Install the process-wide crypto provider (idempotent).
pub fn ensure_provider() -> Arc<CryptoProvider> {
    match CryptoProvider::get_default() {
        Some(provider) => provider.clone(),
        None => {
            let _ = CryptoProvider::install_default(
                tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
            );
            CryptoProvider::get_default()
                .expect("just installed")
                .clone()
        }
    }
}

/// A TLS connector with standard WebPKI validation against the bundled
/// Mozilla roots — for the public web, where certificates rotate
/// constantly and TOFU pinning would only cry wolf. The small net
/// (gemini, telnets) keeps the TOFU `connector` below.
pub fn webpki_connector() -> TlsConnector {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        ensure_provider();
        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    });
    TlsConnector::from(config.clone())
}

/// A TLS connector whose TOFU pin is keyed to this `host:port`.
pub fn connector(host: &str, port: u16) -> TlsConnector {
    let provider = ensure_provider();
    let schemes = provider
        .signature_verification_algorithms
        .supported_schemes();
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Tofu {
            key: format!("{host}:{port}"),
            schemes,
        }))
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// Resolve a host string into the SNI name rustls requires.
pub fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_string()).map_err(|_| format!("invalid host name: {host}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hosts_roundtrip() {
        let mut pins = HashMap::new();
        pins.insert(String::from("bbs.example:992"), [0xab; 32]);
        pins.insert(String::from("capsule.example:1965"), [0x07; 32]);
        let text = format_known_hosts(&pins);
        assert_eq!(parse_known_hosts(&text), pins);

        // Comments, blanks, and garbage are skipped.
        let messy = format!("# comment\n\nnot a valid line\nbad:1 deadbeef\n{text}");
        assert_eq!(parse_known_hosts(&messy), pins);
    }
}
