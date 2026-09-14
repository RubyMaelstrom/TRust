TRust adaptation of RustCrypto aes-gcm 0.11.1

Source: the crates.io aes-gcm 0.11.1 archive. Original source provenance is
retained in .cargo_vcs_info.json; upstream licenses and tests are unchanged.
Archive SHA-256: 7f2b8006a0c83f52b62ba44a97b58bf76fe2f70a329e588f67f89691d93d498f

Web Cryptography's AesGcmParams accepts BufferSource IVs sized at runtime:
https://w3c.github.io/webcrypto/#aes-gcm-params
https://w3c.github.io/webcrypto/#aes-gcm-operations
Local specification snapshot: w3c/webcrypto 811c24c69eb22d477af5f1678cb70dbc611c7c40.

The patch in src/lib.rs exposes the existing detached operations as
encrypt_inout_detached_with_nonce / decrypt_inout_detached_with_nonce,
accepting &[u8]. The fixed-size AeadInOut methods delegate to these methods.
init_ctr uses the slice length in the existing NIST SP 800-38D section 7
algorithm. The new entry points reject empty IVs and bit-length overflow
(section 5.2.1.1). AES, GHASH, tag calculation, and constant-time tag
verification remain RustCrypto's implementations.

TRust enables the upstream hazmat feature only to support Web Crypto's
specified 32- and 64-bit authentication tags, in addition to 96..128 bits.
Regressions in src/crypto.rs cover NIST vectors and input authentication;
src/lumen_backend.rs covers Web Crypto's observable API behavior.
