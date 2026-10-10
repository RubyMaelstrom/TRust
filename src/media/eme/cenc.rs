//! Sample decryption for ISO Common Encryption (ISO/IEC 23001-7) `cenc`
//! (AES-128 CTR) and `cbcs` (AES-128 CBC with a crypt:skip pattern), and
//! WebM encryption (AES-128 CTR, carried as `cenc` with subsamples), as the
//! EME stream format registry (local w3c/encrypted-media format-registry,
//! stream/mp4 and stream/webm) describes them.

use aes_gcm::aes::Aes128;
use aes_gcm::aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit, array::Array};

use crate::media::track::{EncryptionScheme, SampleEncryption};

/// The sample's encryption metadata does not describe its bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Malformed;

/// The protected byte ranges of a sample: its subsamples' protected parts,
/// or the whole sample.
fn protected_ranges(
    len: usize,
    encryption: &SampleEncryption,
) -> Result<Vec<(usize, usize)>, Malformed> {
    if encryption.subsamples.is_empty() {
        return Ok(vec![(0, len)]);
    }
    let mut ranges = Vec::with_capacity(encryption.subsamples.len());
    let mut at = 0usize;
    for subsample in &encryption.subsamples {
        let clear = subsample.clear as usize;
        let protected = subsample.protected as usize;
        let start = at.checked_add(clear).ok_or(Malformed)?;
        let end = start.checked_add(protected).ok_or(Malformed)?;
        if end > len {
            return Err(Malformed);
        }
        if protected > 0 {
            ranges.push((start, end));
        }
        at = end;
    }
    Ok(ranges)
}

/// Decrypt `data` in place with `key`.
pub fn decrypt(
    key: &[u8; 16],
    encryption: &SampleEncryption,
    data: &mut [u8],
) -> Result<(), Malformed> {
    let ranges = protected_ranges(data.len(), encryption)?;
    let cipher = Aes128::new(&Array::from(*key));
    match encryption.scheme {
        EncryptionScheme::Cenc => {
            // ISO/IEC 23001-7 §9.5.2: an 8-byte IV fills the counter's high
            // half; the protected bytes of all subsamples form one key
            // stream.
            let mut counter = [0u8; 16];
            match encryption.iv.len() {
                8 => counter[..8].copy_from_slice(&encryption.iv),
                16 => counter.copy_from_slice(&encryption.iv),
                _ => return Err(Malformed),
            }
            let mut stream = [0u8; 16];
            let mut used = 16;
            for (start, end) in ranges {
                for byte in &mut data[start..end] {
                    if used == 16 {
                        let mut block = Array::from(counter);
                        cipher.encrypt_block(&mut block);
                        stream.copy_from_slice(&block);
                        increment(&mut counter);
                        used = 0;
                    }
                    *byte ^= stream[used];
                    used += 1;
                }
            }
        }
        EncryptionScheme::Cbcs => {
            // §9.6.2 and §10.4: each protected range starts the CBC chain
            // at the IV; the pattern encrypts `crypt` blocks and skips
            // `skip` blocks (0:0 encrypts every block); a trailing partial
            // block is clear.
            if encryption.iv.len() != 16 {
                return Err(Malformed);
            }
            let (crypt, skip) = match (encryption.crypt_byte_block, encryption.skip_byte_block) {
                (0, 0) => (usize::MAX, 0),
                (crypt, skip) => (usize::from(crypt), usize::from(skip)),
            };
            for (start, end) in ranges {
                let mut chain = [0u8; 16];
                chain.copy_from_slice(&encryption.iv);
                let mut at = start;
                while end - at >= 16 {
                    let mut blocks = 0;
                    while blocks < crypt && end - at >= 16 {
                        let mut block = [0u8; 16];
                        block.copy_from_slice(&data[at..at + 16]);
                        let mut plain = Array::from(block);
                        cipher.decrypt_block(&mut plain);
                        for (out, (p, c)) in data[at..at + 16]
                            .iter_mut()
                            .zip(plain.iter().zip(chain.iter()))
                        {
                            *out = p ^ c;
                        }
                        chain = block;
                        at += 16;
                        blocks += 1;
                    }
                    at = at.saturating_add(skip * 16).min(end);
                }
            }
        }
    }
    Ok(())
}

fn increment(counter: &mut [u8; 16]) {
    for byte in counter.iter_mut().rev() {
        *byte = byte.wrapping_add(1);
        if *byte != 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::track::Subsample;

    fn encrypt_ctr(key: &[u8; 16], iv: [u8; 16], data: &mut [u8]) {
        // CTR is its own inverse.
        let encryption = SampleEncryption {
            scheme: EncryptionScheme::Cenc,
            kid: [0; 16],
            iv: iv.to_vec(),
            subsamples: Vec::new(),
            crypt_byte_block: 0,
            skip_byte_block: 0,
        };
        decrypt(key, &encryption, data).unwrap();
    }

    #[test]
    fn ctr_matches_nist_sp800_38a() {
        // NIST SP 800-38A F.5.1 (CTR-AES128.Encrypt), first two blocks.
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let iv = [
            0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd,
            0xfe, 0xff,
        ];
        let mut data = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a, 0xae, 0x2d, 0x8a, 0x57, 0x1e, 0x03, 0xac, 0x9c, 0x9e, 0xb7, 0x6f, 0xac,
            0x45, 0xaf, 0x8e, 0x51,
        ];
        encrypt_ctr(&key, iv, &mut data);
        assert_eq!(
            data,
            [
                0x87, 0x4d, 0x61, 0x91, 0xb6, 0x20, 0xe3, 0x26, 0x1b, 0xef, 0x68, 0x64, 0x99, 0x0d,
                0xb6, 0xce, 0x98, 0x06, 0xf6, 0x6b, 0x79, 0x70, 0xfd, 0xff, 0x86, 0x17, 0x18, 0x7b,
                0xb9, 0xff, 0xfd, 0xff,
            ]
        );
    }

    #[test]
    fn ctr_subsamples_share_one_key_stream() {
        let key = [7u8; 16];
        let plain: Vec<u8> = (0..100u8).collect();
        // Encrypt the protected bytes (5..40 and 50..100) as one stream.
        let mut stream: Vec<u8> = plain[5..40]
            .iter()
            .chain(&plain[50..100])
            .copied()
            .collect();
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        encrypt_ctr(&key, iv, &mut stream);
        let mut sample = plain.clone();
        sample[5..40].copy_from_slice(&stream[..35]);
        sample[50..100].copy_from_slice(&stream[35..]);
        let encryption = SampleEncryption {
            scheme: EncryptionScheme::Cenc,
            kid: [0; 16],
            iv: vec![1, 2, 3, 4, 5, 6, 7, 8],
            subsamples: vec![
                Subsample {
                    clear: 5,
                    protected: 35,
                },
                Subsample {
                    clear: 10,
                    protected: 50,
                },
            ],
            crypt_byte_block: 0,
            skip_byte_block: 0,
        };
        decrypt(&key, &encryption, &mut sample).unwrap();
        assert_eq!(sample, plain);
        let mut short = vec![0u8; 50];
        assert_eq!(decrypt(&key, &encryption, &mut short), Err(Malformed));
    }

    #[test]
    fn cbcs_pattern_restarts_per_subsample_and_leaves_partial_blocks() {
        let key = [3u8; 16];
        let iv = [9u8; 16];
        let cipher = Aes128::new(&Array::from(key));
        // One subsample: 4 clear bytes, then 16*12 + 5 protected bytes with
        // a 1:9 pattern: blocks 0 and 10 are encrypted.
        let plain: Vec<u8> = (0..(4 + 16 * 12 + 5))
            .map(|i| (i * 7 % 251) as u8)
            .collect();
        let mut sample = plain.clone();
        let mut chain = iv;
        for block in [0usize, 10] {
            let at = 4 + block * 16;
            let mut bytes = [0u8; 16];
            for i in 0..16 {
                bytes[i] = sample[at + i] ^ chain[i];
            }
            let mut encrypted = Array::from(bytes);
            cipher.encrypt_block(&mut encrypted);
            sample[at..at + 16].copy_from_slice(&encrypted);
            chain.copy_from_slice(&encrypted);
        }
        let encryption = SampleEncryption {
            scheme: EncryptionScheme::Cbcs,
            kid: [0; 16],
            iv: iv.to_vec(),
            subsamples: vec![Subsample {
                clear: 4,
                protected: 16 * 12 + 5,
            }],
            crypt_byte_block: 1,
            skip_byte_block: 9,
        };
        decrypt(&key, &encryption, &mut sample).unwrap();
        assert_eq!(sample, plain);
    }
}
