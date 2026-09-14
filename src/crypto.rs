//! Small cryptographic primitives used by the browser platform.
//!
//! Web Crypto's AES operations stay behind this narrow adapter. The API
//! layer owns Web IDL normalization and Promise behavior; RustCrypto owns
//! AES and GCM's authentication, including constant-time tag verification.

use aes_gcm::AesGcm;
use aes_gcm::aes::cipher::{BlockCipherEncrypt, KeyInit, array::Array, consts};
use aes_gcm::aes::{Aes128, Aes192, Aes256};

/// Perform the Web Crypto AES-CTR operation for a copied raw key, counter, and
/// input byte sequence.
///
/// Web Cryptography Level 2 §27.7 requires a 16-byte counter, a counter
/// length in the inclusive range 1..=128, and increments the rightmost
/// `counter_bits` as a big-endian integer (the nonce bits remain unchanged).
/// AES-CTR encryption and decryption are the same XOR operation.
pub(crate) fn aes_ctr_crypt(
    key: &[u8],
    counter: &[u8],
    counter_bits: u8,
    input: &[u8],
) -> Option<Vec<u8>> {
    if !matches!(key.len(), 16 | 24 | 32)
        || counter.len() != 16
        || !(1..=128).contains(&counter_bits)
    {
        return None;
    }

    let mut counter_block = [0u8; 16];
    counter_block.copy_from_slice(counter);
    let mut output = input.to_vec();
    for chunk in output.chunks_mut(16) {
        let mut encrypted_counter = Array::from(counter_block);
        match key.len() {
            16 => Aes128::new_from_slice(key)
                .ok()?
                .encrypt_block(&mut encrypted_counter),
            24 => Aes192::new_from_slice(key)
                .ok()?
                .encrypt_block(&mut encrypted_counter),
            32 => Aes256::new_from_slice(key)
                .ok()?
                .encrypt_block(&mut encrypted_counter),
            _ => unreachable!("AES key length validated above"),
        }
        for (byte, mask) in chunk.iter_mut().zip(encrypted_counter.iter()) {
            *byte ^= *mask;
        }
        increment_counter(&mut counter_block, counter_bits);
    }
    Some(output)
}

/// Web Crypto #aes-gcm-operations: ciphertext is C || T. Dispatch all three
/// AES key sizes and all seven tag sizes to RustCrypto. Decryption returns
/// bytes only after RustCrypto has authenticated the IV, AAD, and ciphertext.
pub(crate) fn aes_gcm_crypt(
    key: &[u8],
    iv: &[u8],
    additional_data: &[u8],
    tag_bits: u8,
    input: &[u8],
    decrypt: bool,
) -> Option<Vec<u8>> {
    // Validate lengths before allocating the result; NIST SP 800-38D
    // §5.2.1.1 bounds the bit lengths used by GCM's length encoding.
    let plaintext_len = if decrypt {
        input.len().checked_sub(usize::from(tag_bits / 8))?
    } else {
        input.len()
    };
    if iv.is_empty()
        || iv.len() as u64 > u64::MAX / 8
        || additional_data.len() as u64 > aes_gcm::A_MAX
        || plaintext_len as u64 > aes_gcm::P_MAX
    {
        return None;
    }
    macro_rules! crypt {
        ($aes:ty, $tag:ty) => {{
            let cipher = AesGcm::<$aes, consts::U12, $tag>::new_from_slice(key).ok()?;
            let tag_bytes = usize::from(tag_bits / 8);
            if decrypt {
                let split = input.len().checked_sub(tag_bytes)?;
                let (ciphertext, tag) = input.split_at(split);
                let mut output = ciphertext.to_vec();
                cipher
                    .decrypt_inout_detached_with_nonce(
                        iv,
                        additional_data,
                        output.as_mut_slice().into(),
                        tag.try_into().ok()?,
                    )
                    .ok()?;
                Some(output)
            } else {
                let mut output = input.to_vec();
                let tag = cipher
                    .encrypt_inout_detached_with_nonce(
                        iv,
                        additional_data,
                        output.as_mut_slice().into(),
                    )
                    .ok()?;
                output.extend_from_slice(&tag);
                Some(output)
            }
        }};
    }
    macro_rules! dispatch_tag {
        ($aes:ty) => {
            match tag_bits {
                32 => crypt!($aes, consts::U4),
                64 => crypt!($aes, consts::U8),
                96 => crypt!($aes, consts::U12),
                104 => crypt!($aes, consts::U13),
                112 => crypt!($aes, consts::U14),
                120 => crypt!($aes, consts::U15),
                128 => crypt!($aes, consts::U16),
                _ => None,
            }
        };
    }
    match key.len() {
        16 => dispatch_tag!(Aes128),
        24 => dispatch_tag!(Aes192),
        32 => dispatch_tag!(Aes256),
        _ => None,
    }
}

fn increment_counter(counter: &mut [u8; 16], counter_bits: u8) {
    let full_bytes = usize::from(counter_bits / 8);
    let partial_bits = counter_bits % 8;
    let mut carry = true;

    // The rightmost full byte is the least-significant byte. A partial
    // counter byte, when present, is more significant than those full bytes,
    // so it is reached only after the full-byte portion overflows.
    for index in (16 - full_bytes..16).rev() {
        let (value, overflow) = counter[index].overflowing_add(1);
        counter[index] = value;
        if !overflow {
            carry = false;
            break;
        }
    }

    if carry && partial_bits != 0 {
        let index = 15 - full_bytes;
        let mask = (1u8 << partial_bits) - 1;
        let value = counter[index] & mask;
        counter[index] = (counter[index] & !mask) | (value.wrapping_add(1) & mask);
    }
}

#[cfg(test)]
mod tests {
    use super::{aes_ctr_crypt, aes_gcm_crypt, increment_counter};

    #[test]
    fn aes_gcm_nist_vectors_all_keys_tags_and_iv_lengths() {
        // NIST CAVS gcmEncryptExtIV{128,192,256}.rsp, Count=0 for
        // each selected parameter set. SP 800-38D §7.1 takes MSB_t(T),
        // so the full-tag known answers also verify every shorter tag.
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/webcrypto/aes-gcm-nist.json"
        ))
        .unwrap();
        let vectors = fixture["vectors"].as_array().unwrap();
        assert_eq!(vectors.len(), 15);
        for vector in vectors {
            let bytes = |name| hex(vector[name].as_str().unwrap());
            let key = bytes("key");
            let iv = bytes("iv");
            let plaintext = bytes("pt");
            let aad = bytes("aad");
            let ciphertext = bytes("ct");
            let tag = bytes("tag");
            for tag_bits in [32, 64, 96, 104, 112, 120, 128] {
                let mut expected = ciphertext.clone();
                expected.extend_from_slice(&tag[..usize::from(tag_bits / 8)]);
                assert_eq!(
                    aes_gcm_crypt(&key, &iv, &aad, tag_bits, &plaintext, false),
                    Some(expected.clone()),
                    "encrypt: {} tag={tag_bits}",
                    vector["source"]
                );
                assert_eq!(
                    aes_gcm_crypt(&key, &iv, &aad, tag_bits, &expected, true),
                    Some(plaintext.clone()),
                    "decrypt: {} tag={tag_bits}",
                    vector["source"]
                );

                // Every input to authentication must be checked, including
                // truncated tags and IVs that need GHASH instead of J0's fast path.
                let mut wrong_key = key.clone();
                wrong_key[0] ^= 1;
                let mut wrong_iv = iv.clone();
                wrong_iv[0] ^= 1;
                let mut wrong_aad = aad.clone();
                wrong_aad.push(1);
                for (k, v, a) in [
                    (&wrong_key, &iv, &aad),
                    (&key, &wrong_iv, &aad),
                    (&key, &iv, &wrong_aad),
                ] {
                    assert!(aes_gcm_crypt(k, v, a, tag_bits, &expected, true).is_none());
                }
                for index in [0, expected.len() - 1] {
                    let mut altered = expected.clone();
                    altered[index] ^= 1;
                    assert!(aes_gcm_crypt(&key, &iv, &aad, tag_bits, &altered, true).is_none());
                }
            }
        }
    }

    #[test]
    fn aes_gcm_invalid_lengths_and_incomplete_tags_fail() {
        let key = [0; 32];
        let iv = [0; 12];
        for key_length in [0, 15, 17, 23, 25, 31, 33] {
            assert!(aes_gcm_crypt(&vec![0; key_length], &iv, &[], 128, &[], false).is_none());
        }
        assert!(aes_gcm_crypt(&key, &[], &[], 128, &[], false).is_none());
        for tag_bits in [0, 8, 31, 33, 63, 65, 95, 97, 129, 255] {
            assert!(aes_gcm_crypt(&key, &iv, &[], tag_bits, &[], false).is_none());
        }
        for tag_bits in [32, 64, 96, 104, 112, 120, 128] {
            assert!(
                aes_gcm_crypt(
                    &key,
                    &iv,
                    &[],
                    tag_bits,
                    &vec![0; usize::from(tag_bits / 8) - 1],
                    true
                )
                .is_none()
            );
        }
    }

    #[test]
    fn aes_ctr_matches_nist_sp800_38a_f_5_1() {
        // NIST-SP800-38A §F.5.1 AES-CTR-AES128, also the byte operation named
        // by Web Cryptography Level 2 §27.7.1/§27.7.2.
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let counter = hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
        let plaintext = hex("6bc1bee22e409f96e93d7e117393172a");
        let ciphertext = hex("874d6191b620e3261bef6864990db6ce");

        assert_eq!(
            aes_ctr_crypt(&key, &counter, 128, &plaintext),
            Some(ciphertext.clone())
        );
        assert_eq!(
            aes_ctr_crypt(&key, &counter, 128, &ciphertext),
            Some(plaintext)
        );
    }

    #[test]
    fn aes_ctr_preserves_nonce_when_counter_is_partial() {
        let key = [0u8; 16];
        let mut counter = [0u8; 16];
        counter[0] = 0xa5;
        counter[15] = 0xfe;
        let input = [0u8; 32];

        // A two-block operation with an 8-bit counter must use A5:FE then
        // A5:FF, not increment the nonce byte at index 0.
        let first = aes_ctr_crypt(&key, &counter, 8, &input).unwrap();
        let mut expected = aes_ctr_crypt(&key, &counter, 8, &[0u8; 16]).unwrap();
        let mut second_counter = counter;
        second_counter[15] = 0xff;
        let second = aes_ctr_crypt(&key, &second_counter, 8, &[0u8; 16]).unwrap();
        expected.extend(second);
        assert_eq!(first, expected);
        assert_eq!(counter[0], 0xa5);
    }

    #[test]
    fn partial_counter_increments_least_significant_bits_first() {
        let mut counter = [0u8; 16];
        counter[14] = 0xaf;
        counter[15] = 0xff;

        increment_counter(&mut counter, 12);

        assert_eq!(counter[14], 0xa0);
        assert_eq!(counter[15], 0x00);
    }

    fn hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (hex_digit(pair[0]) << 4) | hex_digit(pair[1]))
            .collect()
    }

    fn hex_digit(value: u8) -> u8 {
        match value {
            b'0'..=b'9' => value - b'0',
            b'a'..=b'f' => value - b'a' + 10,
            _ => panic!("invalid hexadecimal test vector"),
        }
    }
}
