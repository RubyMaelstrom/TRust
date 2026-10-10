//! Encrypted Media Extensions (local w3c/encrypted-media@fb9130a): the
//! "org.w3.clearkey" key system (#clear-key), whose CDM runs here, and the
//! decryption of ISO Common Encryption and WebM encrypted samples
//! ([`cenc`]). Script runs the EME IDL (`navigator.requestMediaKeySystemAccess`,
//! MediaKeys, MediaKeySession, the media element's `setMediaKeys()`); a
//! MediaKeys object's [`ClearKey`] CDM puts the keys its sessions receive in
//! a [`KeyRing`] that the players of the media elements it is attached to
//! decrypt with, waking them when keys arrive.
//!
//! No other key system exists (Widevine and PlayReady are proprietary
//! CDMs), so `requestMediaKeySystemAccess()` rejects them.

pub mod cenc;
mod clearkey;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

pub use clearkey::{ClearKey, Error};

use super::track::Packet;

/// Keys usable for decryption: a MediaKeys object's keys, by key ID, for
/// the players that decrypt with them.
#[derive(Clone, Default)]
pub struct KeyRing {
    inner: Arc<Ring>,
}

/// Keys by key ID: (key ID, AES-128 key).
pub(crate) type Keys = Vec<(Vec<u8>, [u8; 16])>;

/// Holders of one key ID: (session, key).
type Holders = Vec<(u32, [u8; 16])>;

#[derive(Default)]
struct Ring {
    /// Key ID to the sessions holding a key for it (sessions keep their own
    /// keys: closing one leaves another's identical key ID usable).
    keys: Mutex<HashMap<Vec<u8>, Holders>>,
    wakers: Mutex<Vec<Weak<dyn Fn() + Send + Sync>>>,
}

/// A packet could not be decrypted.
#[derive(Debug, PartialEq, Eq)]
pub enum DecryptError {
    /// No session has a usable key for the packet's key ID (playback waits
    /// for one: EME #wait-for-key).
    NoKey,
    /// The encryption metadata does not fit the packet (the media data is
    /// corrupted).
    Malformed,
}

impl KeyRing {
    pub fn key(&self, kid: &[u8]) -> Option<[u8; 16]> {
        let keys = self.inner.keys.lock().unwrap_or_else(|e| e.into_inner());
        keys.get(kid)
            .and_then(|holders| holders.first())
            .map(|&(_, key)| key)
    }

    /// Add or replace keys of `session`.
    pub fn insert(&self, session: u32, keys: &[(Vec<u8>, [u8; 16])]) {
        {
            let mut ring = self.inner.keys.lock().unwrap_or_else(|e| e.into_inner());
            for (kid, key) in keys {
                let holders = ring.entry(kid.clone()).or_default();
                holders.retain(|&(holder, _)| holder != session);
                holders.push((session, *key));
            }
        }
        self.wake();
    }

    /// Forget every key of `session`.
    pub fn remove_session(&self, session: u32) {
        let mut ring = self.inner.keys.lock().unwrap_or_else(|e| e.into_inner());
        ring.retain(|_, holders| {
            holders.retain(|&(holder, _)| holder != session);
            !holders.is_empty()
        });
    }

    /// Call `waker` whenever keys arrive, while it lives.
    pub fn add_waker(&self, waker: &Arc<dyn Fn() + Send + Sync>) {
        let mut wakers = self.inner.wakers.lock().unwrap_or_else(|e| e.into_inner());
        wakers.retain(|waker| waker.strong_count() > 0);
        wakers.push(Arc::downgrade(waker));
    }

    fn wake(&self) {
        let wakers: Vec<_> = self
            .inner
            .wakers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for waker in wakers {
            waker();
        }
    }

    /// The decrypted form of an encrypted packet (EME #attempt-to-decrypt).
    pub fn decrypt(&self, packet: &Packet) -> Result<Packet, DecryptError> {
        let Some(encryption) = &packet.encryption else {
            return Ok(packet.clone());
        };
        let key = self.key(&encryption.kid).ok_or(DecryptError::NoKey)?;
        let mut data = packet.data.to_vec();
        cenc::decrypt(&key, encryption, &mut data).map_err(|_| DecryptError::Malformed)?;
        let mut clear = packet.clone();
        clear.data = Arc::from(data);
        clear.encryption = None;
        Ok(clear)
    }
}

/// EME #using-base64url: base64url without padding.
pub(crate) fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let digits = chunk.len() + 1;
        for i in 0..digits {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

/// base64url without padding (RFC 7515 Appendix C); padding and other
/// alphabets are refused.
pub(crate) fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32)
    };
    let bytes = text.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= value(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_without_padding() {
        // EME's Clear Key examples.
        let kid = base64url_decode("LwVHf8JLtPrv2GUXFW2v_A").unwrap();
        assert_eq!(kid.len(), 16);
        assert_eq!(base64url_encode(&kid), "LwVHf8JLtPrv2GUXFW2v_A");
        for len in 0..10 {
            let bytes: Vec<u8> = (0..len).map(|i: u8| i.wrapping_mul(37)).collect();
            assert_eq!(base64url_decode(&base64url_encode(&bytes)).unwrap(), bytes);
        }
        assert!(base64url_decode("ab+c").is_none());
        assert!(base64url_decode("abc=").is_none());
    }

    #[test]
    fn sessions_keep_their_own_keys() {
        let ring = KeyRing::default();
        ring.insert(1, &[(vec![1; 16], [10; 16])]);
        ring.insert(2, &[(vec![1; 16], [10; 16]), (vec![2; 16], [20; 16])]);
        ring.remove_session(1);
        assert_eq!(ring.key(&[1; 16]), Some([10; 16]));
        ring.remove_session(2);
        assert_eq!(ring.key(&[1; 16]), None);
        assert_eq!(ring.key(&[2; 16]), None);
    }
}
