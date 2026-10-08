//! Web Cryptography API key algorithms: RSASSA-PKCS1-v1_5, RSA-PSS, RSA-OAEP,
//! ECDSA, ECDH, Ed25519, X25519, AES-CBC, AES-KW, HKDF and PBKDF2
//! (w3c/webcrypto@811c24c, `spec/Overview.html` §§ "RSASSA-PKCS1-v1_5" through
//! "PBKDF2", plus the SPKI / PKCS #8 / JWK mappings).
//!
//! The primitives come from AWS-LC, the libcrypto rustls and quinn already link
//! through aws-lc-rs. Its high-level API cannot express Web Crypto's arbitrary
//! PSS salt lengths, RSA modulus/exponent choices, private JWK components or
//! hash/curve combinations, so this adapter calls the EVP and low-level APIs
//! directly. Keys reach JavaScript only as canonical DER (PKCS #8
//! `PrivateKeyInfo` or `SubjectPublicKeyInfo`) held in a `CryptoKey`'s private
//! `[[handle]]` slot. Every operation parses them again, so no native key state
//! outlives a call and structured cloning copies plain bytes.

use aws_lc_sys as ffi;
use std::mem::MaybeUninit;
use std::os::raw::c_int;
use std::ptr;

/// The DOMException a failed step throws; the JavaScript layer constructs it
/// in the relevant Realm (`#SubtleCrypto-Exceptions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Data,
    Operation,
    NotSupported,
}

impl Failure {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Failure::Data => "DataError",
            Failure::Operation => "OperationError",
            Failure::NotSupported => "NotSupportedError",
        }
    }
}

pub(crate) type Outcome<T> = Result<T, Failure>;

/// AWS-LC reports failures through a thread-local error queue. Clear it on
/// entry and exit so failed imports and verifications never accumulate
/// entries that a later, unrelated TLS operation would misread.
struct ErrorQueue;

impl ErrorQueue {
    fn new() -> Self {
        unsafe { ffi::ERR_clear_error() };
        ErrorQueue
    }
}

impl Drop for ErrorQueue {
    fn drop(&mut self) {
        unsafe { ffi::ERR_clear_error() };
    }
}

macro_rules! owned {
    ($name:ident, $ty:ty, $free:path) => {
        struct $name(*mut $ty);

        impl $name {
            fn new(raw: *mut $ty) -> Outcome<Self> {
                if raw.is_null() {
                    Err(Failure::Operation)
                } else {
                    Ok(Self(raw))
                }
            }

            /// Transfer ownership to an AWS-LC call that takes it on success.
            #[allow(dead_code)]
            fn into_raw(self) -> *mut $ty {
                let raw = self.0;
                std::mem::forget(self);
                raw
            }
        }

        impl Drop for $name {
            fn drop(&mut self) {
                unsafe { $free(self.0) }
            }
        }
    };
}

owned!(Pkey, ffi::EVP_PKEY, ffi::EVP_PKEY_free);
owned!(PkeyCtx, ffi::EVP_PKEY_CTX, ffi::EVP_PKEY_CTX_free);
owned!(MdCtx, ffi::EVP_MD_CTX, ffi::EVP_MD_CTX_free);
owned!(Bn, ffi::BIGNUM, ffi::BN_clear_free);
owned!(BnCtx, ffi::BN_CTX, ffi::BN_CTX_free);
owned!(EcKey, ffi::EC_KEY, ffi::EC_KEY_free);
owned!(EcPoint, ffi::EC_POINT, ffi::EC_POINT_free);
owned!(Rsa, ffi::RSA, ffi::RSA_free);
owned!(EcdsaSig, ffi::ECDSA_SIG, ffi::ECDSA_SIG_free);
owned!(CipherCtx, ffi::EVP_CIPHER_CTX, ffi::EVP_CIPHER_CTX_free);

/// `#sha`: the registered digest algorithms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hash {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "SHA-1" => Hash::Sha1,
            "SHA-256" => Hash::Sha256,
            "SHA-384" => Hash::Sha384,
            "SHA-512" => Hash::Sha512,
            _ => return None,
        })
    }

    fn md(self) -> *const ffi::EVP_MD {
        unsafe {
            match self {
                Hash::Sha1 => ffi::EVP_sha1(),
                Hash::Sha256 => ffi::EVP_sha256(),
                Hash::Sha384 => ffi::EVP_sha384(),
                Hash::Sha512 => ffi::EVP_sha512(),
            }
        }
    }

    pub(crate) fn output_len(self) -> usize {
        match self {
            Hash::Sha1 => 20,
            Hash::Sha256 => 32,
            Hash::Sha384 => 48,
            Hash::Sha512 => 64,
        }
    }

    pub(crate) fn digest(self, data: &[u8]) -> Outcome<Vec<u8>> {
        let mut out = vec![0; self.output_len()];
        let mut len = 0;
        let ok = unsafe {
            ffi::EVP_Digest(
                data.as_ptr().cast(),
                data.len(),
                out.as_mut_ptr(),
                &mut len,
                self.md(),
                ptr::null_mut(),
            )
        };
        if ok != 1 || len as usize != out.len() {
            return Err(Failure::Operation);
        }
        Ok(out)
    }
}

/// `#dfn-NamedCurve`: the NIST curves registered for ECDSA and ECDH.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Curve {
    P256,
    P384,
    P521,
}

// RFC 5480 §2.1.1.1 named curve object identifiers (DER contents).
const OID_SECP256R1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_SECP384R1: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
const OID_SECP521R1: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x23];
// RFC 3447 Appendix A.1 rsaEncryption, RFC 5480 §2.1.1 id-ecPublicKey and
// RFC 8410 §3 id-X25519 / id-Ed25519.
const OID_RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_X25519: &[u8] = &[0x2b, 0x65, 0x6e];
const OID_ED25519: &[u8] = &[0x2b, 0x65, 0x70];

impl Curve {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "P-256" => Curve::P256,
            "P-384" => Curve::P384,
            "P-521" => Curve::P521,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Curve::P256 => "P-256",
            Curve::P384 => "P-384",
            Curve::P521 => "P-521",
        }
    }

    fn nid(self) -> c_int {
        match self {
            Curve::P256 => ffi::NID_X9_62_prime256v1,
            Curve::P384 => ffi::NID_secp384r1,
            Curve::P521 => ffi::NID_secp521r1,
        }
    }

    fn from_nid(nid: c_int) -> Option<Self> {
        [Curve::P256, Curve::P384, Curve::P521]
            .into_iter()
            .find(|curve| curve.nid() == nid)
    }

    fn from_oid(oid: &[u8]) -> Option<Self> {
        Some(match oid {
            OID_SECP256R1 => Curve::P256,
            OID_SECP384R1 => Curve::P384,
            OID_SECP521R1 => Curve::P521,
            _ => return None,
        })
    }

    /// Octets of a field element / scalar: RFC 6090 §6.2 and JWA §6.2.1.2.
    pub(crate) fn field_len(self) -> usize {
        match self {
            Curve::P256 => 32,
            Curve::P384 => 48,
            Curve::P521 => 66,
        }
    }
}

/// The key structures behind the registered asymmetric algorithms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Rsa,
    Ec,
    Ed25519,
    X25519,
}

impl Family {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "RSA" => Family::Rsa,
            "EC" => Family::Ec,
            "Ed25519" => Family::Ed25519,
            "X25519" => Family::X25519,
            _ => return None,
        })
    }

    fn evp_id(self) -> c_int {
        match self {
            Family::Rsa => ffi::EVP_PKEY_RSA,
            Family::Ec => ffi::EVP_PKEY_EC,
            Family::Ed25519 => ffi::EVP_PKEY_ED25519,
            Family::X25519 => ffi::EVP_PKEY_X25519,
        }
    }
}

/// A canonical key handle plus the facts its `KeyAlgorithm` reflects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyMaterial {
    /// PKCS #8 for private keys, SubjectPublicKeyInfo for public keys.
    pub(crate) der: Vec<u8>,
    pub(crate) private: bool,
    /// `RsaKeyAlgorithm.modulusLength` (RSA only).
    pub(crate) modulus_bits: u32,
    /// `RsaKeyAlgorithm.publicExponent` as a minimal BigInteger (RSA only).
    pub(crate) public_exponent: Vec<u8>,
    /// `EcKeyAlgorithm.namedCurve` (ECDSA/ECDH only).
    pub(crate) curve: Option<Curve>,
}

// --- DER structure checks -------------------------------------------------

/// A minimal DER reader for the outer SPKI / PKCS #8 structures, so the
/// algorithm identifier can be validated exactly as `#concept-parse-an-asn1-structure`
/// requires (definite minimal lengths, exact data) before AWS-LC parses the key.
struct Der<'a>(&'a [u8]);

impl<'a> Der<'a> {
    fn peek_tag(&self) -> Option<u8> {
        self.0.first().copied()
    }

    fn read(&mut self, tag: u8) -> Option<&'a [u8]> {
        let (&found, rest) = self.0.split_first()?;
        if found != tag {
            return None;
        }
        let (&first, mut rest) = rest.split_first()?;
        let length = if first < 0x80 {
            usize::from(first)
        } else {
            let count = usize::from(first & 0x7f);
            if count == 0 || count > 4 {
                return None;
            }
            let (bytes, tail) = rest.split_at_checked(count)?;
            rest = tail;
            if bytes[0] == 0 {
                return None;
            }
            let length = bytes
                .iter()
                .fold(0usize, |value, &byte| (value << 8) | usize::from(byte));
            if length < 0x80 {
                return None;
            }
            length
        };
        let (content, tail) = rest.split_at_checked(length)?;
        self.0 = tail;
        Some(content)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

struct AlgorithmIdentifier<'a> {
    oid: &'a [u8],
    parameters: Option<(u8, &'a [u8])>,
}

fn algorithm_identifier<'a>(der: &mut Der<'a>) -> Option<AlgorithmIdentifier<'a>> {
    let mut sequence = Der(der.read(0x30)?);
    let oid = sequence.read(0x06)?;
    let parameters = match sequence.peek_tag() {
        None => None,
        Some(tag) => Some((tag, sequence.read(tag)?)),
    };
    sequence
        .is_empty()
        .then_some(AlgorithmIdentifier { oid, parameters })
}

/// RFC 5280 §4.1: SubjectPublicKeyInfo ::= SEQUENCE { algorithm, BIT STRING }.
fn spki_algorithm(data: &[u8]) -> Option<AlgorithmIdentifier<'_>> {
    let mut outer = Der(data);
    let mut spki = Der(outer.read(0x30)?);
    if !outer.is_empty() {
        return None;
    }
    let algorithm = algorithm_identifier(&mut spki)?;
    spki.read(0x03)?;
    spki.is_empty().then_some(algorithm)
}

/// RFC 5208 §5 / RFC 5958 §2: PrivateKeyInfo (OneAsymmetricKey) ::= SEQUENCE {
/// version, privateKeyAlgorithm, privateKey OCTET STRING, [0] attributes OPTIONAL,
/// [1] publicKey OPTIONAL (version 1 only) }.
fn pkcs8_algorithm(data: &[u8]) -> Option<AlgorithmIdentifier<'_>> {
    let mut outer = Der(data);
    let mut info = Der(outer.read(0x30)?);
    if !outer.is_empty() {
        return None;
    }
    let version = info.read(0x02)?;
    if version != [0] && version != [1] {
        return None;
    }
    let algorithm = algorithm_identifier(&mut info)?;
    info.read(0x04)?;
    if info.peek_tag() == Some(0xa0) {
        info.read(0xa0)?;
    }
    if info.peek_tag() == Some(0x81) {
        if version != [1] {
            return None;
        }
        info.read(0x81)?;
    }
    info.is_empty().then_some(algorithm)
}

/// The algorithm checks of each "Import Key" `spki`/`pkcs8` branch.
fn check_algorithm(family: Family, algorithm: &AlgorithmIdentifier<'_>) -> Outcome<Option<Curve>> {
    match family {
        Family::Rsa if algorithm.oid == OID_RSA_ENCRYPTION => Ok(None),
        Family::Ec if algorithm.oid == OID_EC_PUBLIC_KEY => match algorithm.parameters {
            // ECParameters must be the namedCurve choice (RFC 5480 §2.1.1);
            // an unregistered curve has no applicable specification.
            Some((0x06, oid)) => Curve::from_oid(oid).map(Some).ok_or(Failure::Data),
            _ => Err(Failure::Data),
        },
        Family::Ed25519 if algorithm.oid == OID_ED25519 && algorithm.parameters.is_none() => {
            Ok(None)
        }
        Family::X25519 if algorithm.oid == OID_X25519 && algorithm.parameters.is_none() => Ok(None),
        _ => Err(Failure::Data),
    }
}

// --- AWS-LC helpers -------------------------------------------------------

unsafe fn parse_with(
    der: &[u8],
    parse: unsafe extern "C" fn(*mut ffi::CBS) -> *mut ffi::EVP_PKEY,
) -> Option<Pkey> {
    let mut cbs = MaybeUninit::<ffi::CBS>::zeroed();
    unsafe {
        ffi::CBS_init(cbs.as_mut_ptr(), der.as_ptr(), der.len());
        let key = Pkey::new(parse(cbs.as_mut_ptr())).ok()?;
        (ffi::CBS_len(cbs.as_ptr()) == 0).then_some(key)
    }
}

fn parse_public(der: &[u8]) -> Option<Pkey> {
    unsafe { parse_with(der, ffi::EVP_parse_public_key) }
}

fn parse_private(der: &[u8]) -> Option<Pkey> {
    unsafe { parse_with(der, ffi::EVP_parse_private_key) }
}

/// Parse a handle this module produced; a mismatch means corrupted state.
fn stored_key(family: Family, der: &[u8], private: bool) -> Outcome<Pkey> {
    let key = if private {
        parse_private(der)
    } else {
        parse_public(der)
    }
    .ok_or(Failure::Operation)?;
    if unsafe { ffi::EVP_PKEY_id(key.0) } != family.evp_id() {
        return Err(Failure::Operation);
    }
    Ok(key)
}

fn marshal(write: impl FnOnce(*mut ffi::CBB) -> c_int) -> Outcome<Vec<u8>> {
    let mut cbb = MaybeUninit::<ffi::CBB>::zeroed();
    unsafe {
        if ffi::CBB_init(cbb.as_mut_ptr(), 512) != 1 {
            return Err(Failure::Operation);
        }
        if write(cbb.as_mut_ptr()) != 1 {
            ffi::CBB_cleanup(cbb.as_mut_ptr());
            return Err(Failure::Operation);
        }
        let mut data = ptr::null_mut();
        let mut length = 0;
        if ffi::CBB_finish(cbb.as_mut_ptr(), &mut data, &mut length) != 1 {
            ffi::CBB_cleanup(cbb.as_mut_ptr());
            return Err(Failure::Operation);
        }
        let bytes = std::slice::from_raw_parts(data, length).to_vec();
        ffi::OPENSSL_free(data.cast());
        Ok(bytes)
    }
}

fn bn_from(bytes: &[u8]) -> Outcome<Bn> {
    Bn::new(unsafe { ffi::BN_bin2bn(bytes.as_ptr(), bytes.len(), ptr::null_mut()) })
}

/// Minimal big-endian bytes; zero is one zero octet (`#dfn-BigInteger`,
/// JWA §2 Base64urlUInt).
fn bn_minimal(bn: *const ffi::BIGNUM) -> Outcome<Vec<u8>> {
    if bn.is_null() {
        return Err(Failure::Operation);
    }
    let length = unsafe { ffi::BN_num_bytes(bn) } as usize;
    let mut out = vec![0; length.max(1)];
    if length > 0 {
        unsafe { ffi::BN_bn2bin(bn, out.as_mut_ptr()) };
    }
    Ok(out)
}

fn bn_padded(bn: *const ffi::BIGNUM, length: usize) -> Outcome<Vec<u8>> {
    if bn.is_null() {
        return Err(Failure::Operation);
    }
    let mut out = vec![0; length];
    if unsafe { ffi::BN_bn2bin_padded(out.as_mut_ptr(), length, bn) } != 1 {
        return Err(Failure::Operation);
    }
    Ok(out)
}

fn curve_of(key: &Pkey) -> Option<Curve> {
    unsafe {
        let ec = ffi::EVP_PKEY_get0_EC_KEY(key.0);
        if ec.is_null() {
            return None;
        }
        Curve::from_nid(ffi::EC_GROUP_get_curve_name(ffi::EC_KEY_get0_group(ec)))
    }
}

fn pkey_from_rsa(rsa: Rsa) -> Outcome<Pkey> {
    let key = Pkey::new(unsafe { ffi::EVP_PKEY_new() })?;
    if unsafe { ffi::EVP_PKEY_assign_RSA(key.0, rsa.0) } != 1 {
        return Err(Failure::Operation);
    }
    rsa.into_raw();
    Ok(key)
}

fn pkey_from_ec(ec: EcKey) -> Outcome<Pkey> {
    let key = Pkey::new(unsafe { ffi::EVP_PKEY_new() })?;
    if unsafe { ffi::EVP_PKEY_assign_EC_KEY(key.0, ec.0) } != 1 {
        return Err(Failure::Operation);
    }
    ec.into_raw();
    Ok(key)
}

/// Serialize a key in the canonical form its export steps produce: EC keys
/// use the uncompressed point and an ECPrivateKey with both `parameters` and
/// `publicKey` present (ECDSA "Export Key", `pkcs8`).
fn describe(family: Family, key: &Pkey, private: bool) -> Outcome<KeyMaterial> {
    unsafe {
        let ec = ffi::EVP_PKEY_get0_EC_KEY(key.0);
        if !ec.is_null() {
            ffi::EC_KEY_set_conv_form(
                ec,
                ffi::point_conversion_form_t::POINT_CONVERSION_UNCOMPRESSED,
            );
            ffi::EC_KEY_set_enc_flags(ec, 0);
        }
    }
    let der = if private {
        marshal(|cbb| unsafe { ffi::EVP_marshal_private_key(cbb, key.0) })?
    } else {
        marshal(|cbb| unsafe { ffi::EVP_marshal_public_key(cbb, key.0) })?
    };
    let mut material = KeyMaterial {
        der,
        private,
        modulus_bits: 0,
        public_exponent: Vec::new(),
        curve: None,
    };
    match family {
        Family::Rsa => unsafe {
            let rsa = ffi::EVP_PKEY_get0_RSA(key.0);
            if rsa.is_null() {
                return Err(Failure::Operation);
            }
            material.modulus_bits = ffi::BN_num_bits(ffi::RSA_get0_n(rsa));
            material.public_exponent = bn_minimal(ffi::RSA_get0_e(rsa))?;
        },
        Family::Ec => material.curve = Some(curve_of(key).ok_or(Failure::Operation)?),
        Family::Ed25519 | Family::X25519 => {}
    }
    Ok(material)
}

// --- Generate Key ---------------------------------------------------------

/// "Generate Key" for the asymmetric algorithms. RSA moduli follow AWS-LC's
/// generator: a multiple of 128 bits from 256 to 16384, with an exponent of at
/// most 32 bits. Anything else cannot be generated and throws OperationError.
pub(crate) fn generate(
    family: Family,
    curve: Option<Curve>,
    modulus_bits: u32,
    public_exponent: &[u8],
) -> Outcome<(KeyMaterial, KeyMaterial)> {
    let _errors = ErrorQueue::new();
    let key = match family {
        Family::Rsa => {
            if !(256..=16384).contains(&modulus_bits) || !modulus_bits.is_multiple_of(128) {
                return Err(Failure::Operation);
            }
            let exponent = bn_from(public_exponent)?;
            if unsafe { ffi::BN_num_bits(exponent.0) } > 32 {
                return Err(Failure::Operation);
            }
            let rsa = Rsa::new(unsafe { ffi::RSA_new() })?;
            let generated = unsafe {
                ffi::RSA_generate_key_ex(rsa.0, modulus_bits as c_int, exponent.0, ptr::null_mut())
            };
            if generated != 1 || unsafe { ffi::RSA_bits(rsa.0) } != modulus_bits {
                return Err(Failure::Operation);
            }
            pkey_from_rsa(rsa)?
        }
        Family::Ec => {
            let curve = curve.ok_or(Failure::NotSupported)?;
            let ec = EcKey::new(unsafe { ffi::EC_KEY_new_by_curve_name(curve.nid()) })?;
            if unsafe { ffi::EC_KEY_generate_key(ec.0) } != 1 {
                return Err(Failure::Operation);
            }
            pkey_from_ec(ec)?
        }
        Family::Ed25519 | Family::X25519 => {
            let context = PkeyCtx::new(unsafe {
                ffi::EVP_PKEY_CTX_new_id(family.evp_id(), ptr::null_mut())
            })?;
            let mut raw = ptr::null_mut();
            if unsafe { ffi::EVP_PKEY_keygen_init(context.0) } != 1
                || unsafe { ffi::EVP_PKEY_keygen(context.0, &mut raw) } != 1
            {
                return Err(Failure::Operation);
            }
            Pkey::new(raw)?
        }
    };
    Ok((
        describe(family, &key, true)?,
        describe(family, &key, false)?,
    ))
}

// --- Import Key -----------------------------------------------------------

/// `spki` import: `#concept-parse-a-spki` with exactData, the algorithm
/// identifier checks, then the key structure (DataError on any failure).
pub(crate) fn import_spki(family: Family, der: &[u8]) -> Outcome<KeyMaterial> {
    let _errors = ErrorQueue::new();
    let algorithm = spki_algorithm(der).ok_or(Failure::Data)?;
    let curve = check_algorithm(family, &algorithm)?;
    let key = parse_public(der).ok_or(Failure::Data)?;
    if unsafe { ffi::EVP_PKEY_id(key.0) } != family.evp_id() || curve_of(&key) != curve {
        return Err(Failure::Data);
    }
    describe(family, &key, false).map_err(|_| Failure::Data)
}

/// `pkcs8` import: `#concept-parse-a-privateKeyInfo`, the algorithm checks,
/// then RSAPrivateKey / ECPrivateKey / CurvePrivateKey. AWS-LC validates the
/// RSA key (RSA_check_key), the ECPrivateKey `parameters` against the outer
/// identifier, the scalar range and any embedded public key.
pub(crate) fn import_pkcs8(family: Family, der: &[u8]) -> Outcome<KeyMaterial> {
    let _errors = ErrorQueue::new();
    let algorithm = pkcs8_algorithm(der).ok_or(Failure::Data)?;
    let curve = check_algorithm(family, &algorithm)?;
    let key = parse_private(der).ok_or(Failure::Data)?;
    if unsafe { ffi::EVP_PKEY_id(key.0) } != family.evp_id() || curve_of(&key) != curve {
        return Err(Failure::Data);
    }
    describe(family, &key, true).map_err(|_| Failure::Data)
}

/// `raw` public import: SEC1 §2.3.4 point decoding (uncompressed and
/// compressed; the identity is rejected) or a 32-byte RFC 8410 key.
pub(crate) fn import_raw_public(
    family: Family,
    curve: Option<Curve>,
    bytes: &[u8],
) -> Outcome<KeyMaterial> {
    let _errors = ErrorQueue::new();
    let key = match family {
        Family::Ec => {
            let curve = curve.ok_or(Failure::Data)?;
            let ec = EcKey::new(unsafe { ffi::EC_KEY_new_by_curve_name(curve.nid()) })?;
            let group = unsafe { ffi::EC_KEY_get0_group(ec.0) };
            let point = EcPoint::new(unsafe { ffi::EC_POINT_new(group) })?;
            let decoded = unsafe {
                ffi::EC_POINT_oct2point(
                    group,
                    point.0,
                    bytes.as_ptr(),
                    bytes.len(),
                    ptr::null_mut(),
                )
            };
            if decoded != 1
                || unsafe { ffi::EC_KEY_set_public_key(ec.0, point.0) } != 1
                || unsafe { ffi::EC_KEY_check_key(ec.0) } != 1
            {
                return Err(Failure::Data);
            }
            pkey_from_ec(ec)?
        }
        Family::Ed25519 | Family::X25519 => {
            if bytes.len() != 32 {
                return Err(Failure::Data);
            }
            Pkey::new(unsafe {
                ffi::EVP_PKEY_new_raw_public_key(
                    family.evp_id(),
                    ptr::null_mut(),
                    bytes.as_ptr(),
                    bytes.len(),
                )
            })
            .map_err(|_| Failure::Data)?
        }
        Family::Rsa => return Err(Failure::NotSupported),
    };
    describe(family, &key, false).map_err(|_| Failure::Data)
}

/// The private members of an RSA JWK (JWA §6.3.2). `crt` holds p, q, dp, dq
/// and qi when present; JWA permits a private key carrying only `d`.
pub(crate) struct RsaPrivateJwk<'a> {
    pub(crate) d: &'a [u8],
    pub(crate) crt: Option<[&'a [u8]; 5]>,
}

/// RSA `jwk` import (JWA §6.3.1 / §6.3.2) after the Web Crypto layer has
/// decoded the Base64urlUInt members. The key must be valid per RFC 3447.
pub(crate) fn import_rsa_jwk(
    n: &[u8],
    e: &[u8],
    private: Option<RsaPrivateJwk<'_>>,
) -> Outcome<KeyMaterial> {
    let _errors = ErrorQueue::new();
    let modulus = bn_from(n)?;
    let exponent = bn_from(e)?;
    let rsa = match private {
        None => Rsa::new(unsafe { ffi::RSA_new_public_key(modulus.0, exponent.0) })
            .map_err(|_| Failure::Data)?,
        Some(private) => {
            let d = bn_from(private.d)?;
            let [p, q, dp, dq, qi] = match private.crt {
                Some([p, q, dp, dq, qi]) => [
                    bn_from(p)?,
                    bn_from(q)?,
                    bn_from(dp)?,
                    bn_from(dq)?,
                    bn_from(qi)?,
                ],
                None => recover_crt(&modulus, &exponent, &d).ok_or(Failure::Data)?,
            };
            Rsa::new(unsafe {
                ffi::RSA_new_private_key(modulus.0, exponent.0, d.0, p.0, q.0, dp.0, dq.0, qi.0)
            })
            .map_err(|_| Failure::Data)?
        }
    };
    let private = !unsafe { ffi::RSA_get0_d(rsa.0) }.is_null();
    let key = pkey_from_rsa(rsa)?;
    describe(Family::Rsa, &key, private).map_err(|_| Failure::Data)
}

/// Recover p and q from (n, e, d) for a JWK private key that omits the CRT
/// members: NIST SP 800-56B Rev. 2 Appendix C.2 ("Prime Factor Recovery"),
/// then dp = d mod (p − 1), dq = d mod (q − 1), qi = q⁻¹ mod p.
fn recover_crt(n: &Bn, e: &Bn, d: &Bn) -> Option<[Bn; 5]> {
    unsafe {
        let ctx = BnCtx::new(ffi::BN_CTX_new()).ok()?;
        let fresh = || Bn::new(ffi::BN_new()).ok();
        // k = de − 1 = 2^t · r with r odd.
        let k = fresh()?;
        if ffi::BN_mul(k.0, d.0, e.0, ctx.0) != 1 || ffi::BN_sub_word(k.0, 1) != 1 {
            return None;
        }
        if ffi::BN_is_odd(k.0) == 1 {
            return None;
        }
        let t = ffi::BN_count_low_zero_bits(k.0);
        let r = fresh()?;
        if ffi::BN_rshift(r.0, k.0, t) != 1 {
            return None;
        }
        let n_minus_1 = fresh()?;
        if ffi::BN_copy(n_minus_1.0, n.0).is_null() || ffi::BN_sub_word(n_minus_1.0, 1) != 1 {
            return None;
        }
        let (g, y, x, p) = (fresh()?, fresh()?, fresh()?, fresh()?);
        'attempt: for _ in 0..100 {
            if ffi::BN_rand_range_ex(g.0, 2, n_minus_1.0) != 1
                || ffi::BN_mod_exp(y.0, g.0, r.0, n.0, ctx.0) != 1
            {
                return None;
            }
            if ffi::BN_is_one(y.0) == 1 || ffi::BN_cmp(y.0, n_minus_1.0) == 0 {
                continue;
            }
            for _ in 0..t {
                if ffi::BN_mod_mul(x.0, y.0, y.0, n.0, ctx.0) != 1 {
                    return None;
                }
                if ffi::BN_is_one(x.0) == 1 {
                    // y is a non-trivial square root of 1: gcd(y − 1, n) = p.
                    if ffi::BN_sub_word(y.0, 1) != 1 || ffi::BN_gcd(p.0, y.0, n.0, ctx.0) != 1 {
                        return None;
                    }
                    break 'attempt;
                }
                if ffi::BN_cmp(x.0, n_minus_1.0) == 0 {
                    continue 'attempt;
                }
                if ffi::BN_copy(y.0, x.0).is_null() {
                    return None;
                }
            }
        }
        if ffi::BN_is_one(p.0) == 1 || ffi::BN_cmp(p.0, n.0) >= 0 || ffi::BN_num_bits(p.0) == 0 {
            return None;
        }
        let q = fresh()?;
        let remainder = fresh()?;
        if ffi::BN_div(q.0, remainder.0, n.0, p.0, ctx.0) != 1 || ffi::BN_num_bits(remainder.0) != 0
        {
            return None;
        }
        let (p_minus_1, q_minus_1, dp, dq, qi) = (fresh()?, fresh()?, fresh()?, fresh()?, fresh()?);
        if ffi::BN_copy(p_minus_1.0, p.0).is_null()
            || ffi::BN_sub_word(p_minus_1.0, 1) != 1
            || ffi::BN_copy(q_minus_1.0, q.0).is_null()
            || ffi::BN_sub_word(q_minus_1.0, 1) != 1
            || ffi::BN_nnmod(dp.0, d.0, p_minus_1.0, ctx.0) != 1
            || ffi::BN_nnmod(dq.0, d.0, q_minus_1.0, ctx.0) != 1
            || ffi::BN_mod_inverse(qi.0, q.0, p.0, ctx.0).is_null()
        {
            return None;
        }
        Some([p, q, dp, dq, qi])
    }
}

/// EC `jwk` import (JWA §6.2.1 / §6.2.2): full-length coordinates and scalar,
/// a point on the curve, and a private scalar that matches it.
pub(crate) fn import_ec_jwk(
    curve: Curve,
    x: &[u8],
    y: &[u8],
    d: Option<&[u8]>,
) -> Outcome<KeyMaterial> {
    let _errors = ErrorQueue::new();
    let length = curve.field_len();
    if x.len() != length || y.len() != length || d.is_some_and(|d| d.len() != length) {
        return Err(Failure::Data);
    }
    let ec = EcKey::new(unsafe { ffi::EC_KEY_new_by_curve_name(curve.nid()) })?;
    let (bx, by) = (bn_from(x)?, bn_from(y)?);
    if unsafe { ffi::EC_KEY_set_public_key_affine_coordinates(ec.0, bx.0, by.0) } != 1 {
        return Err(Failure::Data);
    }
    if let Some(d) = d {
        let scalar = bn_from(d)?;
        if unsafe { ffi::EC_KEY_set_private_key(ec.0, scalar.0) } != 1
            || unsafe { ffi::EC_KEY_check_key(ec.0) } != 1
        {
            return Err(Failure::Data);
        }
    }
    let key = pkey_from_ec(ec)?;
    describe(Family::Ec, &key, d.is_some()).map_err(|_| Failure::Data)
}

/// OKP `jwk` import (RFC 8037 §2): `x` is the public key and `d` the private
/// key; a private key must reproduce the supplied public key.
pub(crate) fn import_okp_jwk(family: Family, x: &[u8], d: Option<&[u8]>) -> Outcome<KeyMaterial> {
    let _errors = ErrorQueue::new();
    if x.len() != 32 || d.is_some_and(|d| d.len() != 32) {
        return Err(Failure::Data);
    }
    let Some(d) = d else {
        return import_raw_public(family, None, x);
    };
    let key = Pkey::new(unsafe {
        ffi::EVP_PKEY_new_raw_private_key(family.evp_id(), ptr::null_mut(), d.as_ptr(), d.len())
    })
    .map_err(|_| Failure::Data)?;
    if raw_public_of(&key)? != x {
        return Err(Failure::Data);
    }
    describe(family, &key, true).map_err(|_| Failure::Data)
}

// --- Export Key -----------------------------------------------------------

fn raw_public_of(key: &Pkey) -> Outcome<Vec<u8>> {
    let mut out = [0u8; 32];
    let mut length = out.len();
    if unsafe { ffi::EVP_PKEY_get_raw_public_key(key.0, out.as_mut_ptr(), &mut length) } != 1
        || length != 32
    {
        return Err(Failure::Operation);
    }
    Ok(out.to_vec())
}

fn ec_public_point(key: &Pkey, curve: Curve) -> Outcome<(Vec<u8>, Vec<u8>)> {
    unsafe {
        let ec = ffi::EVP_PKEY_get0_EC_KEY(key.0);
        if ec.is_null() {
            return Err(Failure::Operation);
        }
        let (x, y) = (Bn::new(ffi::BN_new())?, Bn::new(ffi::BN_new())?);
        if ffi::EC_POINT_get_affine_coordinates_GFp(
            ffi::EC_KEY_get0_group(ec),
            ffi::EC_KEY_get0_public_key(ec),
            x.0,
            y.0,
            ptr::null_mut(),
        ) != 1
        {
            return Err(Failure::Operation);
        }
        Ok((
            bn_padded(x.0, curve.field_len())?,
            bn_padded(y.0, curve.field_len())?,
        ))
    }
}

/// The JWK members of a key, in the order its export steps set them:
/// RSA n, e[, d, p, q, dp, dq, qi]; EC x, y[, d]; OKP x[, d].
pub(crate) fn jwk_components(family: Family, der: &[u8], private: bool) -> Outcome<Vec<Vec<u8>>> {
    let _errors = ErrorQueue::new();
    let key = stored_key(family, der, private)?;
    match family {
        Family::Rsa => unsafe {
            let rsa = ffi::EVP_PKEY_get0_RSA(key.0);
            if rsa.is_null() {
                return Err(Failure::Operation);
            }
            let mut members = vec![
                bn_minimal(ffi::RSA_get0_n(rsa))?,
                bn_minimal(ffi::RSA_get0_e(rsa))?,
            ];
            if private {
                let (mut p, mut q) = (ptr::null(), ptr::null());
                let (mut dp, mut dq, mut qi) = (ptr::null(), ptr::null(), ptr::null());
                ffi::RSA_get0_factors(rsa, &mut p, &mut q);
                ffi::RSA_get0_crt_params(rsa, &mut dp, &mut dq, &mut qi);
                for value in [ffi::RSA_get0_d(rsa), p, q, dp, dq, qi] {
                    members.push(bn_minimal(value)?);
                }
            }
            Ok(members)
        },
        Family::Ec => {
            let curve = curve_of(&key).ok_or(Failure::Operation)?;
            let (x, y) = ec_public_point(&key, curve)?;
            let mut members = vec![x, y];
            if private {
                let scalar =
                    unsafe { ffi::EC_KEY_get0_private_key(ffi::EVP_PKEY_get0_EC_KEY(key.0)) };
                members.push(bn_padded(scalar, curve.field_len())?);
            }
            Ok(members)
        }
        Family::Ed25519 | Family::X25519 => {
            let mut members = vec![raw_public_of(&key)?];
            if private {
                let mut out = [0u8; 32];
                let mut length = out.len();
                if unsafe {
                    ffi::EVP_PKEY_get_raw_private_key(key.0, out.as_mut_ptr(), &mut length)
                } != 1
                    || length != 32
                {
                    return Err(Failure::Operation);
                }
                members.push(out.to_vec());
            }
            Ok(members)
        }
    }
}

/// `raw` public export: SEC1 §2.3.3 uncompressed point, or the RFC 8410 key.
pub(crate) fn raw_public(family: Family, spki: &[u8]) -> Outcome<Vec<u8>> {
    let _errors = ErrorQueue::new();
    let key = stored_key(family, spki, false)?;
    match family {
        Family::Ec => {
            let curve = curve_of(&key).ok_or(Failure::Operation)?;
            let (x, y) = ec_public_point(&key, curve)?;
            let mut point = Vec::with_capacity(1 + 2 * curve.field_len());
            point.push(4);
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            Ok(point)
        }
        Family::Ed25519 | Family::X25519 => raw_public_of(&key),
        Family::Rsa => Err(Failure::NotSupported),
    }
}

// --- Sign / Verify --------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub(crate) enum Scheme {
    /// RFC 3447 §8.2 RSASSA-PKCS1-v1_5.
    RsaPkcs1(Hash),
    /// RFC 3447 §8.1 RSASSA-PSS with MGF1 over the same hash and a salt length.
    RsaPss(Hash, u32),
    /// RFC 6090 §5.4 ECDSA over the digest, IEEE P1363 r ‖ s output.
    Ecdsa(Hash),
    /// RFC 8032 §5.1.6 / §5.1.7 Ed25519.
    Ed25519,
}

impl Scheme {
    fn family(self) -> Family {
        match self {
            Scheme::RsaPkcs1(_) | Scheme::RsaPss(..) => Family::Rsa,
            Scheme::Ecdsa(_) => Family::Ec,
            Scheme::Ed25519 => Family::Ed25519,
        }
    }
}

fn digest_context(key: &Pkey, scheme: Scheme, sign: bool) -> Outcome<MdCtx> {
    let context = MdCtx::new(unsafe { ffi::EVP_MD_CTX_new() })?;
    let mut pkey_context = ptr::null_mut();
    let md = match scheme {
        Scheme::RsaPkcs1(hash) | Scheme::RsaPss(hash, _) => hash.md(),
        Scheme::Ed25519 => ptr::null(),
        Scheme::Ecdsa(_) => return Err(Failure::Operation),
    };
    let initialized = unsafe {
        if sign {
            ffi::EVP_DigestSignInit(context.0, &mut pkey_context, md, ptr::null_mut(), key.0)
        } else {
            ffi::EVP_DigestVerifyInit(context.0, &mut pkey_context, md, ptr::null_mut(), key.0)
        }
    };
    if initialized != 1 {
        return Err(Failure::Operation);
    }
    if let Scheme::RsaPss(hash, salt_length) = scheme {
        let salt_length = c_int::try_from(salt_length).map_err(|_| Failure::Operation)?;
        unsafe {
            if ffi::EVP_PKEY_CTX_set_rsa_padding(pkey_context, ffi::RSA_PKCS1_PSS_PADDING) != 1
                || ffi::EVP_PKEY_CTX_set_rsa_pss_saltlen(pkey_context, salt_length) != 1
                || ffi::EVP_PKEY_CTX_set_rsa_mgf1_md(pkey_context, hash.md()) != 1
            {
                return Err(Failure::Operation);
            }
        }
    }
    Ok(context)
}

fn ec_order_bytes(ec: *const ffi::EC_KEY) -> usize {
    let bits = unsafe { ffi::EC_GROUP_order_bits(ffi::EC_KEY_get0_group(ec)) };
    (bits.max(0) as usize).div_ceil(8)
}

/// The "Sign" operations. Only private keys reach here.
pub(crate) fn sign(scheme: Scheme, pkcs8: &[u8], message: &[u8]) -> Outcome<Vec<u8>> {
    let _errors = ErrorQueue::new();
    let key = stored_key(scheme.family(), pkcs8, true)?;
    if let Scheme::Ecdsa(hash) = scheme {
        let digest = hash.digest(message)?;
        let ec = unsafe { ffi::EVP_PKEY_get0_EC_KEY(key.0) };
        let signature =
            EcdsaSig::new(unsafe { ffi::ECDSA_do_sign(digest.as_ptr(), digest.len(), ec) })?;
        let (mut r, mut s) = (ptr::null(), ptr::null());
        unsafe { ffi::ECDSA_SIG_get0(signature.0, &mut r, &mut s) };
        let length = ec_order_bytes(ec);
        let mut out = bn_padded(r, length)?;
        out.extend(bn_padded(s, length)?);
        return Ok(out);
    }
    let context = digest_context(&key, scheme, true)?;
    let mut length = 0;
    unsafe {
        if ffi::EVP_DigestSign(
            context.0,
            ptr::null_mut(),
            &mut length,
            message.as_ptr(),
            message.len(),
        ) != 1
        {
            return Err(Failure::Operation);
        }
        let mut out = vec![0; length];
        if ffi::EVP_DigestSign(
            context.0,
            out.as_mut_ptr(),
            &mut length,
            message.as_ptr(),
            message.len(),
        ) != 1
        {
            return Err(Failure::Operation);
        }
        out.truncate(length);
        Ok(out)
    }
}

/// The 255-bit y values of the Ed25519 points of small order (1, 2, 4, 8),
/// including the non-canonical encodings y + p that fit in 255 bits.
const ED25519_SMALL_ORDER: [[u8; 32]; 7] = {
    const fn high(low: u8) -> [u8; 32] {
        let mut value = [0xff; 32];
        value[0] = low;
        value[31] = 0x7f;
        value
    }
    let zero = [0; 32];
    let mut one = [0; 32];
    one[0] = 1;
    [
        zero,
        one,
        high(0xec), // p − 1, order 2
        [
            0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef,
            0x98, 0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88,
            0x6d, 0x53, 0xfc, 0x05,
        ],
        [
            0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10,
            0x67, 0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77,
            0x92, 0xac, 0x03, 0x7a,
        ],
        high(0xed), // p ≡ 0
        high(0xee), // p + 1 ≡ 1
    ]
};

/// Ed25519 "Verify" steps 2 and 3: a public key or R that encodes a point of
/// small order (whatever its sign bit) never verifies.
fn ed25519_small_order(encoding: &[u8]) -> bool {
    ED25519_SMALL_ORDER.iter().any(|candidate| {
        encoding.len() == 32
            && encoding[..31] == candidate[..31]
            && encoding[31] & 0x7f == candidate[31]
    })
}

/// The "Verify" operations: any failure to verify is `false`.
pub(crate) fn verify(scheme: Scheme, spki: &[u8], signature: &[u8], message: &[u8]) -> bool {
    let _errors = ErrorQueue::new();
    let Ok(key) = stored_key(scheme.family(), spki, false) else {
        return false;
    };
    match scheme {
        Scheme::Ecdsa(hash) => {
            let Ok(digest) = hash.digest(message) else {
                return false;
            };
            let ec = unsafe { ffi::EVP_PKEY_get0_EC_KEY(key.0) };
            let length = ec_order_bytes(ec);
            if signature.len() != 2 * length {
                return false;
            }
            let (Ok(r), Ok(s), Ok(parsed)) = (
                bn_from(&signature[..length]),
                bn_from(&signature[length..]),
                EcdsaSig::new(unsafe { ffi::ECDSA_SIG_new() }),
            ) else {
                return false;
            };
            if unsafe { ffi::ECDSA_SIG_set0(parsed.0, r.0, s.0) } != 1 {
                return false;
            }
            r.into_raw();
            s.into_raw();
            unsafe { ffi::ECDSA_do_verify(digest.as_ptr(), digest.len(), parsed.0, ec) == 1 }
        }
        _ => {
            if let Scheme::Ed25519 = scheme {
                let small_key = raw_public_of(&key).map_or(true, |raw| ed25519_small_order(&raw));
                if signature.len() != 64 || small_key || ed25519_small_order(&signature[..32]) {
                    return false;
                }
            }
            let Ok(context) = digest_context(&key, scheme, false) else {
                return false;
            };
            unsafe {
                ffi::EVP_DigestVerify(
                    context.0,
                    signature.as_ptr(),
                    signature.len(),
                    message.as_ptr(),
                    message.len(),
                ) == 1
            }
        }
    }
}

// --- RSA-OAEP -------------------------------------------------------------

/// RSA-OAEP "Encrypt" / "Decrypt" (RFC 3447 §7.1) with MGF1 over the key's
/// hash and the optional label.
pub(crate) fn rsa_oaep(
    der: &[u8],
    hash: Hash,
    label: &[u8],
    input: &[u8],
    decrypt: bool,
) -> Outcome<Vec<u8>> {
    let _errors = ErrorQueue::new();
    let key = stored_key(Family::Rsa, der, decrypt)?;
    let context = PkeyCtx::new(unsafe { ffi::EVP_PKEY_CTX_new(key.0, ptr::null_mut()) })?;
    unsafe {
        let initialized = if decrypt {
            ffi::EVP_PKEY_decrypt_init(context.0)
        } else {
            ffi::EVP_PKEY_encrypt_init(context.0)
        };
        if initialized != 1
            || ffi::EVP_PKEY_CTX_set_rsa_padding(context.0, ffi::RSA_PKCS1_OAEP_PADDING) != 1
            || ffi::EVP_PKEY_CTX_set_rsa_oaep_md(context.0, hash.md()) != 1
            || ffi::EVP_PKEY_CTX_set_rsa_mgf1_md(context.0, hash.md()) != 1
        {
            return Err(Failure::Operation);
        }
        if !label.is_empty() {
            // The context takes ownership of an OPENSSL_malloc'd copy.
            let copy = ffi::OPENSSL_malloc(label.len()).cast::<u8>();
            if copy.is_null() {
                return Err(Failure::Operation);
            }
            ptr::copy_nonoverlapping(label.as_ptr(), copy, label.len());
            if ffi::EVP_PKEY_CTX_set0_rsa_oaep_label(context.0, copy, label.len()) != 1 {
                ffi::OPENSSL_free(copy.cast());
                return Err(Failure::Operation);
            }
        }
        let run = |out: *mut u8, length: &mut usize| {
            if decrypt {
                ffi::EVP_PKEY_decrypt(context.0, out, length, input.as_ptr(), input.len())
            } else {
                ffi::EVP_PKEY_encrypt(context.0, out, length, input.as_ptr(), input.len())
            }
        };
        let mut length = 0;
        if run(ptr::null_mut(), &mut length) != 1 {
            return Err(Failure::Operation);
        }
        let mut out = vec![0; length];
        if run(out.as_mut_ptr(), &mut length) != 1 {
            return Err(Failure::Operation);
        }
        out.truncate(length);
        Ok(out)
    }
}

// --- ECDH / X25519 ----------------------------------------------------------

/// ECDH "Derive Bits" (RFC 6090 §4, the x-coordinate as a field element) and
/// X25519 (RFC 7748 §5; AWS-LC rejects the all-zero output in constant time,
/// which the spec requires to throw OperationError).
pub(crate) fn derive(family: Family, pkcs8: &[u8], spki: &[u8]) -> Outcome<Vec<u8>> {
    let _errors = ErrorQueue::new();
    let private = stored_key(family, pkcs8, true)?;
    let public = stored_key(family, spki, false)?;
    if family == Family::Ec && curve_of(&private) != curve_of(&public) {
        return Err(Failure::Operation);
    }
    let context = PkeyCtx::new(unsafe { ffi::EVP_PKEY_CTX_new(private.0, ptr::null_mut()) })?;
    unsafe {
        if ffi::EVP_PKEY_derive_init(context.0) != 1
            || ffi::EVP_PKEY_derive_set_peer(context.0, public.0) != 1
        {
            return Err(Failure::Operation);
        }
        let mut length = 0;
        if ffi::EVP_PKEY_derive(context.0, ptr::null_mut(), &mut length) != 1 {
            return Err(Failure::Operation);
        }
        let mut secret = vec![0; length];
        if ffi::EVP_PKEY_derive(context.0, secret.as_mut_ptr(), &mut length) != 1 {
            return Err(Failure::Operation);
        }
        secret.truncate(length);
        Ok(secret)
    }
}

// --- AES-CBC / AES-KW -------------------------------------------------------

/// AES-CBC "Encrypt" / "Decrypt": NIST SP 800-38A §6.2 with RFC 2315 §10.3
/// PKCS #7 padding (k = 16); bad padding throws OperationError.
pub(crate) fn aes_cbc(key: &[u8], iv: &[u8], input: &[u8], decrypt: bool) -> Outcome<Vec<u8>> {
    let _errors = ErrorQueue::new();
    let cipher = unsafe {
        match key.len() {
            16 => ffi::EVP_aes_128_cbc(),
            24 => ffi::EVP_aes_192_cbc(),
            32 => ffi::EVP_aes_256_cbc(),
            _ => return Err(Failure::Operation),
        }
    };
    if iv.len() != 16 || (decrypt && (input.is_empty() || !input.len().is_multiple_of(16))) {
        return Err(Failure::Operation);
    }
    let input_length = c_int::try_from(input.len()).map_err(|_| Failure::Operation)?;
    if input_length > c_int::MAX - 32 {
        return Err(Failure::Operation);
    }
    let context = CipherCtx::new(unsafe { ffi::EVP_CIPHER_CTX_new() })?;
    let mut out = vec![0; input.len() + 16];
    let (mut written, mut last) = (0, 0);
    unsafe {
        let ok = if decrypt {
            ffi::EVP_DecryptInit_ex(
                context.0,
                cipher,
                ptr::null_mut(),
                key.as_ptr(),
                iv.as_ptr(),
            ) == 1
                && ffi::EVP_DecryptUpdate(
                    context.0,
                    out.as_mut_ptr(),
                    &mut written,
                    input.as_ptr(),
                    input_length,
                ) == 1
                && ffi::EVP_DecryptFinal_ex(
                    context.0,
                    out.as_mut_ptr().add(written as usize),
                    &mut last,
                ) == 1
        } else {
            ffi::EVP_EncryptInit_ex(
                context.0,
                cipher,
                ptr::null_mut(),
                key.as_ptr(),
                iv.as_ptr(),
            ) == 1
                && ffi::EVP_EncryptUpdate(
                    context.0,
                    out.as_mut_ptr(),
                    &mut written,
                    input.as_ptr(),
                    input_length,
                ) == 1
                && ffi::EVP_EncryptFinal_ex(
                    context.0,
                    out.as_mut_ptr().add(written as usize),
                    &mut last,
                ) == 1
        };
        if !ok {
            return Err(Failure::Operation);
        }
    }
    out.truncate(written as usize + last as usize);
    Ok(out)
}

/// AES-KW "Wrap Key" / "Unwrap Key": RFC 3394 §2.2.1 / §2.2.2 with the default
/// initial value (§2.2.3.1). Inputs must be at least two 64-bit blocks.
pub(crate) fn aes_kw(key: &[u8], input: &[u8], unwrap: bool) -> Outcome<Vec<u8>> {
    let _errors = ErrorQueue::new();
    let bits = match key.len() {
        16 | 24 | 32 => key.len() as u32 * 8,
        _ => return Err(Failure::Operation),
    };
    if !input.len().is_multiple_of(8) || input.len() < if unwrap { 24 } else { 16 } {
        return Err(Failure::Operation);
    }
    let mut schedule = MaybeUninit::<ffi::AES_KEY>::zeroed();
    let mut out = vec![0; input.len() + 8];
    unsafe {
        let set = if unwrap {
            ffi::AES_set_decrypt_key(key.as_ptr(), bits, schedule.as_mut_ptr())
        } else {
            ffi::AES_set_encrypt_key(key.as_ptr(), bits, schedule.as_mut_ptr())
        };
        if set != 0 {
            return Err(Failure::Operation);
        }
        let length = if unwrap {
            ffi::AES_unwrap_key(
                schedule.as_ptr(),
                ptr::null(),
                out.as_mut_ptr(),
                input.as_ptr(),
                input.len(),
            )
        } else {
            ffi::AES_wrap_key(
                schedule.as_ptr(),
                ptr::null(),
                out.as_mut_ptr(),
                input.as_ptr(),
                input.len(),
            )
        };
        // Do not leave the key schedule on the stack.
        ffi::OPENSSL_cleanse(
            schedule.as_mut_ptr().cast(),
            std::mem::size_of::<ffi::AES_KEY>(),
        );
        if length < 0 {
            return Err(Failure::Operation);
        }
        out.truncate(length as usize);
    }
    Ok(out)
}

// --- HKDF / PBKDF2 ----------------------------------------------------------

/// HKDF "Derive Bits": RFC 5869 §2 extract-then-expand producing `length`
/// octets (the caller has enforced length ≤ 255 · HashLen).
pub(crate) fn hkdf(
    hash: Hash,
    ikm: &[u8],
    salt: &[u8],
    info: &[u8],
    length: usize,
) -> Outcome<Vec<u8>> {
    let _errors = ErrorQueue::new();
    if length > 255 * hash.output_len() {
        return Err(Failure::Operation);
    }
    let mut out = vec![0; length];
    if length == 0 {
        return Ok(out);
    }
    let ok = unsafe {
        ffi::HKDF(
            out.as_mut_ptr(),
            length,
            hash.md(),
            ikm.as_ptr(),
            ikm.len(),
            salt.as_ptr(),
            salt.len(),
            info.as_ptr(),
            info.len(),
        )
    };
    if ok != 1 {
        return Err(Failure::Operation);
    }
    Ok(out)
}

/// PBKDF2 "Derive Bits": RFC 8018 §5.2 with HMAC over the hash as the PRF.
pub(crate) fn pbkdf2(
    hash: Hash,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    length: usize,
) -> Outcome<Vec<u8>> {
    let _errors = ErrorQueue::new();
    if iterations == 0 {
        return Err(Failure::Operation);
    }
    let mut out = vec![0; length];
    if length == 0 {
        return Ok(out);
    }
    let ok = unsafe {
        ffi::PKCS5_PBKDF2_HMAC(
            password.as_ptr().cast(),
            password.len(),
            salt.as_ptr(),
            salt.len(),
            iterations,
            hash.md(),
            length,
            out.as_mut_ptr(),
        )
    };
    if ok != 1 {
        return Err(Failure::Operation);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&value[index..index + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rsa_generation_import_export_and_signatures_round_trip() {
        let (private, public) = generate(Family::Rsa, None, 1024, &[1, 0, 1]).unwrap();
        assert_eq!(
            (private.modulus_bits, public.public_exponent.as_slice()),
            (1024, &[1, 0, 1][..])
        );
        // Canonical DER re-imports to itself.
        assert_eq!(
            import_pkcs8(Family::Rsa, &private.der).unwrap().der,
            private.der
        );
        assert_eq!(
            import_spki(Family::Rsa, &public.der).unwrap().der,
            public.der
        );
        // A key from its JWK members, with and without the CRT members, is the same key.
        let members = jwk_components(Family::Rsa, &private.der, true).unwrap();
        let crt = [
            &members[3][..],
            &members[4][..],
            &members[5][..],
            &members[6][..],
            &members[7][..],
        ];
        let from_crt = import_rsa_jwk(
            &members[0],
            &members[1],
            Some(RsaPrivateJwk {
                d: &members[2],
                crt: Some(crt),
            }),
        )
        .unwrap();
        assert_eq!(from_crt.der, private.der);
        let recovered = import_rsa_jwk(
            &members[0],
            &members[1],
            Some(RsaPrivateJwk {
                d: &members[2],
                crt: None,
            }),
        )
        .unwrap();
        assert_eq!(
            jwk_components(Family::Rsa, &recovered.der, true).unwrap()[..3],
            members[..3]
        );
        for scheme in [
            Scheme::RsaPkcs1(Hash::Sha256),
            Scheme::RsaPss(Hash::Sha384, 0),
            Scheme::RsaPss(Hash::Sha1, 77),
        ] {
            let signature = sign(scheme, &recovered.der, b"message").unwrap();
            assert_eq!(signature.len(), 128);
            assert!(verify(scheme, &public.der, &signature, b"message"));
            assert!(!verify(scheme, &public.der, &signature, b"messagf"));
        }
        // A salt longer than emLen − hLen − 2 cannot be encoded.
        assert_eq!(
            sign(Scheme::RsaPss(Hash::Sha512, 100), &private.der, b"m"),
            Err(Failure::Operation)
        );
        let ciphertext = rsa_oaep(&public.der, Hash::Sha256, b"label", b"secret", false).unwrap();
        assert_eq!(
            rsa_oaep(&private.der, Hash::Sha256, b"label", &ciphertext, true).unwrap(),
            b"secret"
        );
        assert_eq!(
            rsa_oaep(&private.der, Hash::Sha256, b"other", &ciphertext, true),
            Err(Failure::Operation)
        );
        // Unsupported sizes fail instead of silently generating another size.
        assert_eq!(
            generate(Family::Rsa, None, 1000, &[1, 0, 1]).unwrap_err(),
            Failure::Operation
        );
        // The wrong algorithm identifier is a DataError.
        assert_eq!(
            import_spki(Family::Ec, &public.der).unwrap_err(),
            Failure::Data
        );
        let mut trailing = public.der.clone();
        trailing.push(0);
        assert_eq!(
            import_spki(Family::Rsa, &trailing).unwrap_err(),
            Failure::Data
        );
    }

    #[test]
    fn ec_keys_points_signatures_and_ecdh_follow_sec1_and_rfc6090() {
        for curve in [Curve::P256, Curve::P384, Curve::P521] {
            let (private, public) = generate(Family::Ec, Some(curve), 0, &[]).unwrap();
            assert_eq!(private.curve, Some(curve));
            let raw = raw_public(Family::Ec, &public.der).unwrap();
            assert_eq!(raw.len(), 1 + 2 * curve.field_len());
            // Compressed points import and re-export uncompressed.
            let mut compressed = vec![2 + (raw[raw.len() - 1] & 1)];
            compressed.extend_from_slice(&raw[1..=curve.field_len()]);
            assert_eq!(
                import_raw_public(Family::Ec, Some(curve), &compressed)
                    .unwrap()
                    .der,
                public.der
            );
            assert_eq!(
                import_raw_public(Family::Ec, Some(curve), &[0]).unwrap_err(),
                Failure::Data
            );
            let members = jwk_components(Family::Ec, &private.der, true).unwrap();
            assert_eq!(
                import_ec_jwk(curve, &members[0], &members[1], Some(&members[2]))
                    .unwrap()
                    .der,
                private.der
            );
            let mut wrong = members[2].clone();
            wrong[5] ^= 1;
            assert_eq!(
                import_ec_jwk(curve, &members[0], &members[1], Some(&wrong)).unwrap_err(),
                Failure::Data
            );
            for hash in [Hash::Sha1, Hash::Sha256, Hash::Sha384, Hash::Sha512] {
                let signature = sign(Scheme::Ecdsa(hash), &private.der, b"data").unwrap();
                assert_eq!(signature.len(), 2 * curve.field_len());
                assert!(verify(
                    Scheme::Ecdsa(hash),
                    &public.der,
                    &signature,
                    b"data"
                ));
                assert!(!verify(
                    Scheme::Ecdsa(hash),
                    &public.der,
                    &signature[1..],
                    b"data"
                ));
            }
            let (other_private, other_public) = generate(Family::Ec, Some(curve), 0, &[]).unwrap();
            let shared = derive(Family::Ec, &private.der, &other_public.der).unwrap();
            assert_eq!(shared.len(), curve.field_len());
            assert_eq!(
                derive(Family::Ec, &other_private.der, &public.der).unwrap(),
                shared
            );
        }
    }

    #[test]
    fn rfc8032_ed25519_vector_and_small_order_rejection() {
        // RFC 8032 §7.1 TEST 1.
        let secret = hex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
        let public = hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let expected = hex(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        );
        let private = import_okp_jwk(Family::Ed25519, &public, Some(&secret)).unwrap();
        let public_key = import_raw_public(Family::Ed25519, None, &public).unwrap();
        assert_eq!(sign(Scheme::Ed25519, &private.der, b"").unwrap(), expected);
        assert!(verify(Scheme::Ed25519, &public_key.der, &expected, b""));
        assert_eq!(
            import_okp_jwk(Family::Ed25519, &secret, Some(&secret)).unwrap_err(),
            Failure::Data
        );
        for small in ED25519_SMALL_ORDER {
            assert!(ed25519_small_order(&small));
            let mut negative = small;
            negative[31] |= 0x80;
            assert!(ed25519_small_order(&negative));
        }
        assert!(!ed25519_small_order(&public));
    }

    #[test]
    fn rfc7748_x25519_and_symmetric_vectors() {
        // RFC 7748 §6.1.
        let alice = hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let alice_public = hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
        let bob_public = hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let shared = hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
        let private = import_okp_jwk(Family::X25519, &alice_public, Some(&alice)).unwrap();
        let bob = import_raw_public(Family::X25519, None, &bob_public).unwrap();
        assert_eq!(
            derive(Family::X25519, &private.der, &bob.der).unwrap(),
            shared
        );
        let zero = import_raw_public(Family::X25519, None, &[0; 32]).unwrap();
        assert_eq!(
            derive(Family::X25519, &private.der, &zero.der).unwrap_err(),
            Failure::Operation
        );
        // RFC 5869 A.1.
        assert_eq!(
            hkdf(
                Hash::Sha256,
                &[0x0b; 22],
                &hex("000102030405060708090a0b0c"),
                &hex("f0f1f2f3f4f5f6f7f8f9"),
                42
            )
            .unwrap(),
            hex(
                "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
            )
        );
        // RFC 6070 test 2.
        assert_eq!(
            pbkdf2(Hash::Sha1, b"password", b"salt", 2, 20).unwrap(),
            hex("ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957")
        );
        assert_eq!(
            pbkdf2(Hash::Sha1, b"p", b"s", 0, 20).unwrap_err(),
            Failure::Operation
        );
        // RFC 3394 §4.1.
        let kek = hex("000102030405060708090a0b0c0d0e0f");
        let data = hex("00112233445566778899aabbccddeeff");
        let wrapped = hex("1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5");
        assert_eq!(aes_kw(&kek, &data, false).unwrap(), wrapped);
        assert_eq!(aes_kw(&kek, &wrapped, true).unwrap(), data);
        let mut tampered = wrapped.clone();
        tampered[0] ^= 1;
        assert_eq!(
            aes_kw(&kek, &tampered, true).unwrap_err(),
            Failure::Operation
        );
        // NIST SP 800-38A F.2.1 (first block) and PKCS #7 padding.
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        let plaintext = hex("6bc1bee22e409f96e93d7e117393172a");
        let ciphertext = aes_cbc(&key, &iv, &plaintext, false).unwrap();
        assert_eq!(
            ciphertext[..16],
            hex("7649abac8119b246cee98e9b12e9197d")[..]
        );
        assert_eq!(ciphertext.len(), 32);
        assert_eq!(aes_cbc(&key, &iv, &ciphertext, true).unwrap(), plaintext);
        assert_eq!(
            aes_cbc(&key, &iv, &ciphertext[..16], true).unwrap_err(),
            Failure::Operation
        );
        assert_eq!(
            aes_cbc(&key, &iv, &[], true).unwrap_err(),
            Failure::Operation
        );
    }
}
