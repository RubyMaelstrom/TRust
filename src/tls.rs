//! TLS transport with protocol-specific certificate policies.
//!
//! HTTPS uses WebPKI. Gemini and Gophers accept unpinned server certificates,
//! including replacements, by the user's explicit policy: traffic is encrypted
//! but the server's identity is not verified. TLS handshake signatures are
//! still checked (RFC 8446 §4.4.3). Gemini client identities remain independent.
//! Telnet TLS uses trust-on-first-use, storing host:port fingerprints in
//! `~/.config/trust/known_hosts` (overridable via `TRUST_KNOWN_HOSTS`).

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
struct ServerVerifier {
    /// `host:port` this connection dialed — the pin key. The verifier
    /// callback only sees the SNI name, which lacks the port, so the
    /// key is baked in per connection.
    /// None accepts the server certificate without consulting or writing pins.
    pin: Option<String>,
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for ServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let Some(key) = &self.pin else {
            return Ok(ServerCertVerified::assertion());
        };
        let fp = fingerprint(end_entity);
        let mut store = store().lock().unwrap();
        match store.pins.get(key) {
            None => {
                store.pins.insert(key.clone(), fp);
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
                    key,
                    &hex(pinned)[..16],
                    &hex(&fp)[..16],
                )))
            }
        }
    }

    // RFC 8446 §4.4.3: verify proof of possession even when certificate
    // identity checks are disabled. Never bypass TLS handshake integrity.
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
/// constantly. Telnet TLS keeps the TOFU `connector` below.
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
        .with_custom_certificate_verifier(Arc::new(ServerVerifier {
            pin: Some(format!("{host}:{port}")),
            schemes,
        }))
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// Encrypted transport with no server-identity or certificate-pin checks.
/// User-selected policy for Gophers and Gemini; never used for HTTPS/Telnet.
/// Reuse the configuration and rustls's bounded session cache across requests.
pub fn unverified_connector() -> TlsConnector {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let provider = ensure_provider();
        Arc::new(
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(ServerVerifier {
                    pin: None,
                    schemes: provider
                        .signature_verification_algorithms
                        .supported_schemes(),
                }))
                .with_no_client_auth(),
        )
    });
    TlsConnector::from(config.clone())
}

/// Where client identities live: one `<host>.pem` per capsule, holding
/// the certificate and its private key (`TRUST_IDENTITIES` overrides
/// the directory; tests point it at temp space).
fn identities_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TRUST_IDENTITIES") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/trust/identities"))
}

/// The file that holds (or would hold) a host's client identity.
pub fn identity_path(host: &str) -> Option<PathBuf> {
    identities_dir().map(|dir| dir.join(format!("{host}.pem")))
}

type Identity = (
    Vec<CertificateDer<'static>>,
    tokio_rustls::rustls::pki_types::PrivateKeyDer<'static>,
);

/// Load a host's client identity. No file is fine (`Ok(None)`); a file
/// that exists but can't be used is an error the user must hear about.
/// Certificate and key blocks may appear in any order (openssl output,
/// concatenated .crt + .key — whatever she has lying around).
fn load_identity(host: &str) -> Result<Option<Identity>, String> {
    use tokio_rustls::rustls::pki_types::{PrivateKeyDer, pem::PemObject};
    let Some(path) = identity_path(host) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let show = path.display();
    let certs: Vec<CertificateDer> = CertificateDer::pem_file_iter(&path)
        .and_then(Iterator::collect)
        .map_err(|e| format!("{show}: {e}"))?;
    if certs.is_empty() {
        return Err(format!("{show}: no CERTIFICATE block"));
    }
    let key = PrivateKeyDer::from_pem_file(&path)
        .map_err(|e| format!("{show}: no usable private key ({e})"))?;
    Ok(Some((certs, key)))
}

/// TLS for Gemini: unpinned server certificates plus the host's client identity
/// when one is on file. The bool reports whether one was presented.
pub fn gemini_connector(host: &str, _port: u16) -> Result<(TlsConnector, bool), String> {
    let Some((certs, key)) = load_identity(host)? else {
        return Ok((unverified_connector(), false));
    };
    let provider = ensure_provider();
    let schemes = provider
        .signature_verification_algorithms
        .supported_schemes();
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ServerVerifier { pin: None, schemes }))
        .with_client_auth_cert(certs, key)
        .map_err(|e| format!("client certificate: {e}"))?;
    Ok((TlsConnector::from(Arc::new(config)), true))
}

/// Mint a self-signed client identity (CN = `name`) for a host and
/// save it where `identity_path` will find it. Refuses to overwrite —
/// capsules pin the certificate, so replacing one means losing the
/// account it represents.
pub fn create_identity(host: &str, name: &str) -> Result<PathBuf, String> {
    let path = identity_path(host).ok_or("no home directory")?;
    if path.exists() {
        return Err(format!(
            "{} already exists — remove it to mint a new identity",
            path.display()
        ));
    }
    let mut params =
        rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    // Capsules pin the cert; an expiry would only ever lock her out.
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let cert = params.self_signed(&key).map_err(|e| e.to_string())?;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600); // the file holds a private key
    }
    use std::io::Write;
    let mut file = options
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(format!("{}{}", cert.pem(), key.serialize_pem()).as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(path)
}

/// Resolve a host string into the SNI name rustls requires.
pub fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_string()).map_err(|_| format!("invalid host name: {host}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Every call has a new key and an expired, self-signed certificate for a
    /// different name. Shared by the Gemini/Gophers transport regressions.
    pub(crate) fn unverified_acceptor(
        versions: &[&'static tokio_rustls::rustls::SupportedProtocolVersion],
    ) -> tokio_rustls::TlsAcceptor {
        ensure_provider();
        let mut params = rcgen::CertificateParams::new(vec!["unrelated.invalid".into()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let config = tokio_rustls::rustls::ServerConfig::builder_with_protocol_versions(versions)
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                key.serialize_der().try_into().unwrap(),
            )
            .unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    #[test]
    fn connectors_advertise_ml_dsa_in_client_hello() {
        use tokio_rustls::rustls::{ClientConnection, server::Acceptor};

        // RFC 9846 §4.3.3: the ClientHello advertises the signatures the
        // client can verify. Exercise WebPKI and both custom policies so
        // verifier so rustls's ML-DSA defaults reach the actual wire.
        for (name, connector) in [
            ("WebPKI", webpki_connector()),
            ("TOFU", connector("localhost", 1965)),
            ("Unverified", unverified_connector()),
        ] {
            let mut client = ClientConnection::new(
                connector.config().clone(),
                server_name("localhost").unwrap(),
            )
            .unwrap();
            let mut wire = Vec::new();
            client.write_tls(&mut wire).unwrap();
            let mut acceptor = Acceptor::default();
            acceptor.read_tls(&mut wire.as_slice()).unwrap();
            let accepted = acceptor
                .accept()
                .map_err(|(error, _)| error)
                .unwrap()
                .expect("complete ClientHello");
            let hello = accepted.client_hello();
            for scheme in [
                SignatureScheme::ML_DSA_44,
                SignatureScheme::ML_DSA_65,
                SignatureScheme::ML_DSA_87,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::ED25519,
            ] {
                assert!(
                    hello.signature_schemes().contains(&scheme),
                    "{name} ClientHello did not advertise {scheme:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn unverified_tls_accepts_certificate_replacement_expiry_and_name_mismatch() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::rustls::version;

        for (protocol, connector) in [
            ("Gophers", unverified_connector()),
            (
                "Gemini",
                gemini_connector("accept-certificate.invalid", 1965)
                    .unwrap()
                    .0,
            ),
        ] {
            for version in [&version::TLS12, &version::TLS13] {
                for _ in 0..2 {
                    let acceptor = unverified_acceptor(&[version]);
                    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
                    let (client, server) =
                        tokio::time::timeout(std::time::Duration::from_secs(3), async {
                            tokio::join!(
                                connector.connect(
                                    server_name("accept-certificate.invalid").unwrap(),
                                    client_io
                                ),
                                acceptor.accept(server_io),
                            )
                        })
                        .await
                        .unwrap();
                    let mut client =
                        client.unwrap_or_else(|e| panic!("{protocol} {version:?}: {e}"));
                    let mut server = server.unwrap();
                    let mut bytes = [0; 5];
                    let (written, read) = tokio::join!(
                        async {
                            client.write_all(b"hello").await?;
                            client.flush().await
                        },
                        server.read_exact(&mut bytes),
                    );
                    written.unwrap();
                    read.unwrap();
                    assert_eq!(&bytes, b"hello");
                }
            }
        }
    }

    #[tokio::test]
    async fn unverified_tls_still_rejects_invalid_handshake_signatures() {
        use tokio_rustls::rustls::{
            ServerConfig,
            server::{ClientHello, ResolvesServerCert},
            sign::CertifiedKey,
            version,
        };

        #[derive(Debug)]
        struct WrongKey(Arc<CertifiedKey>);
        impl ResolvesServerCert for WrongKey {
            fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
                Some(self.0.clone())
            }
        }

        let provider = ensure_provider();
        let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let different_key = rcgen::KeyPair::generate().unwrap();
        let key = provider
            .key_provider
            .load_private_key(different_key.serialize_der().try_into().unwrap())
            .unwrap();
        let certificate = Arc::new(CertifiedKey::new(vec![signed.cert.der().clone()], key));
        for version in [&version::TLS12, &version::TLS13] {
            let config = ServerConfig::builder_with_protocol_versions(&[version])
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(WrongKey(certificate.clone())));
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (client, _) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                tokio::join!(
                    unverified_connector().connect(server_name("localhost").unwrap(), client_io),
                    acceptor.accept(server_io),
                )
            })
            .await
            .unwrap();
            assert!(
                client.is_err(),
                "{version:?}: accepted a signature from the wrong private key"
            );
        }
    }

    #[tokio::test]
    async fn webpki_rejects_untrusted_certificates_in_tls12_and_tls13() {
        use tokio_rustls::TlsAcceptor;
        use tokio_rustls::rustls::{CertificateError, ServerConfig, version};

        // RFC 5280 §6.1.1(d): a self-signed certificate is trusted only
        // when supplied as a trust anchor through a trusted channel.
        // Supporting more signature algorithms must preserve WebPKI trust.
        let connector = webpki_connector();
        let signed = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        for version in [&version::TLS12, &version::TLS13] {
            let key = tokio_rustls::rustls::pki_types::PrivateKeyDer::try_from(
                signed.signing_key.serialize_der(),
            )
            .unwrap();
            let config = ServerConfig::builder_with_protocol_versions(&[version])
                .with_no_client_auth()
                .with_single_cert(vec![signed.cert.der().clone()], key)
                .unwrap();
            let acceptor = TlsAcceptor::from(Arc::new(config));
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (client, server) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(
                    connector.connect(server_name("localhost").unwrap(), client_io),
                    acceptor.accept(server_io),
                )
            })
            .await
            .expect("TLS handshake timed out");
            let error = client.unwrap_err();
            assert!(
                matches!(
                    error
                        .get_ref()
                        .and_then(|error| error.downcast_ref::<Error>()),
                    Some(Error::InvalidCertificate(CertificateError::UnknownIssuer))
                ),
                "{version:?}: expected an untrusted issuer error, got {error:?}"
            );
            assert!(
                server.is_err(),
                "{version:?}: server completed an untrusted handshake"
            );
        }
    }

    #[test]
    fn client_identities_mint_and_load_in_any_order() {
        // All identity tests share one temp dir (the env var is
        // process-global); each uses its own hostname.
        let dir = std::env::temp_dir().join(format!("trust-test-ids-{}", std::process::id()));
        unsafe {
            std::env::set_var("TRUST_IDENTITIES", &dir);
        }

        // Nothing on file: anonymous connection.
        let (_, presented) = gemini_connector("capsule.test", 1965).unwrap();
        assert!(!presented);

        // Mint one — private, both blocks present — and it gets used.
        let path = create_identity("capsule.test", "ruby").unwrap();
        let pem = std::fs::read_to_string(&path).unwrap();
        assert!(pem.contains("BEGIN CERTIFICATE") && pem.contains("PRIVATE KEY"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file is owner-only");
        }
        let (_, presented) = gemini_connector("capsule.test", 1965).unwrap();
        assert!(presented);

        // Never overwrite: the capsule pinned this certificate.
        assert!(create_identity("capsule.test", "someone-else").is_err());

        // Her openssl-made files may put the key first; order is free.
        let key_start = pem.find("-----BEGIN PRIVATE KEY-----").unwrap();
        let reversed = format!("{}{}", &pem[key_start..], &pem[..key_start]);
        std::fs::write(dir.join("reversed.test.pem"), reversed).unwrap();
        let (_, presented) = gemini_connector("reversed.test", 1965).unwrap();
        assert!(presented, "key-before-cert PEM loads fine");

        // A broken file is an error, never a silent anonymous visit.
        std::fs::write(dir.join("broken.test.pem"), "not pem at all").unwrap();
        assert!(gemini_connector("broken.test", 1965).is_err());
    }

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
