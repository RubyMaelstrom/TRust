//! TLS support built for the small net: rustls with trust-on-first-use.
//!
//! Geminispace and TLS-enabled BBSes overwhelmingly run self-signed
//! certificates, which WebPKI validation would reject outright. Instead
//! we follow the Gemini community convention: accept whatever certificate
//! a host presents the first time, pin its SHA-256 fingerprint, and
//! refuse the connection if the same host later presents a different
//! one. Pins live in process memory only (like the histories); a
//! persistent known-hosts store is a decision for the Gemini pass.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};

/// In-RAM fingerprint pins, keyed by server name.
static PINS: Mutex<Option<HashMap<String, [u8; 32]>>> = Mutex::new(None);

fn fingerprint(cert: &CertificateDer<'_>) -> [u8; 32] {
    Sha256::digest(cert.as_ref()).into()
}

fn hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug)]
struct Tofu {
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for Tofu {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let fp = fingerprint(end_entity);
        let name = server_name.to_str().into_owned();
        let mut pins = PINS.lock().unwrap();
        let pins = pins.get_or_insert_with(HashMap::new);
        match pins.get(&name) {
            None => {
                pins.insert(name, fp);
                Ok(ServerCertVerified::assertion())
            }
            Some(pinned) if *pinned == fp => Ok(ServerCertVerified::assertion()),
            Some(pinned) => Err(Error::General(format!(
                "certificate for {name} changed since first use \
                 (pinned sha256:{}.., got sha256:{}..)",
                &hex(pinned)[..16],
                &hex(&fp)[..16],
            ))),
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

/// The shared TLS connector (TOFU verification, no client certificate).
pub fn connector() -> TlsConnector {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let provider = match CryptoProvider::get_default() {
            Some(provider) => provider.clone(),
            None => {
                let _ = CryptoProvider::install_default(
                    tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
                );
                CryptoProvider::get_default()
                    .expect("just installed")
                    .clone()
            }
        };
        let schemes = provider
            .signature_verification_algorithms
            .supported_schemes();
        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Tofu { schemes }))
            .with_no_client_auth();
        Arc::new(config)
    });
    TlsConnector::from(config.clone())
}

/// Resolve a host string into the SNI name rustls requires.
pub fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_string()).map_err(|_| format!("invalid host name: {host}"))
}
