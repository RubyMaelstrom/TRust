//! Small cryptographic primitives used by the browser platform.
//!
//! The Web Cryptography API's AES-CTR operation is deliberately kept behind
//! this narrow adapter.  The API layer owns Web IDL normalization and Promise
//! behavior; this module owns only AES-CTR's byte operation.

use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use aes::{Aes128, Aes192, Aes256};

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
        let mut encrypted_counter = GenericArray::clone_from_slice(&counter_block);
        match key.len() {
            16 => Aes128::new(GenericArray::from_slice(key)).encrypt_block(&mut encrypted_counter),
            24 => Aes192::new(GenericArray::from_slice(key)).encrypt_block(&mut encrypted_counter),
            32 => Aes256::new(GenericArray::from_slice(key)).encrypt_block(&mut encrypted_counter),
            _ => unreachable!("AES key length validated above"),
        }
        for (byte, mask) in chunk.iter_mut().zip(encrypted_counter.iter()) {
            *byte ^= *mask;
        }
        increment_counter(&mut counter_block, counter_bits);
    }
    Some(output)
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
    use super::{aes_ctr_crypt, increment_counter};

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
