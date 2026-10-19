//! RFC 7050 §3 discovery (updated by RFC 8880) and RFC 6052 §§2.2–2.3
//! address synthesis, RFC Editor snapshot 2026-09-06.
//!
//! Prefixes are learned from the configured system resolver's AAAA answers for
//! ipv4only.arpa.; the well-known prefix is never guessed. Discovery is scoped
//! to a connection and its current network, so switching networks cannot reuse
//! an old translator. DNS caching/TTL handling stays with the system resolver.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Prefix {
    bytes: [u8; 16],
    length: usize,
}

const LENGTHS: [usize; 6] = [32, 40, 48, 56, 64, 96];
const WELL_KNOWN: [[u8; 4]; 2] = [[192, 0, 0, 170], [192, 0, 0, 171]];

fn positions(length: usize) -> [usize; 4] {
    std::array::from_fn(|i| {
        let position = length / 8 + i;
        position + usize::from(length != 96 && position >= 8)
    })
}

pub(super) fn discover(addresses: &[SocketAddr]) -> Vec<Prefix> {
    // If one WKA occurs twice (for example once in the network prefix and
    // once in the embedded address), RFC 7050 directs discovery to the other
    // WKA. Remember that decision across the whole response, not just that
    // record: the prefix occurrence remains in its companion AAAA record.
    let mut ambiguous = [false; 2];
    for address in addresses {
        let SocketAddr::V6(address) = address else {
            continue;
        };
        let bytes = address.ip().octets();
        for (index, value) in WELL_KNOWN.iter().enumerate() {
            let contiguous = bytes.windows(4).filter(|window| *window == *value).count();
            let split = [40, 48, 56]
                .into_iter()
                .filter(|length| positions(*length).map(|i| bytes[i]) == *value)
                .count();
            ambiguous[index] |= contiguous + split > 1;
        }
    }
    let mut prefixes = Vec::new();
    for address in addresses {
        let SocketAddr::V6(address) = address else {
            continue;
        };
        let bytes = address.ip().octets();
        // RFC 6052 reserves this octet for every allowed format, including /96.
        if bytes[8] != 0 {
            continue;
        }
        for length in LENGTHS {
            let indexes = positions(length);
            let value = indexes.map(|i| bytes[i]);
            let Some(wka) = WELL_KNOWN
                .iter()
                .position(|well_known| *well_known == value)
            else {
                continue;
            };
            if ambiguous[wka] {
                continue;
            }
            // RFC 7050 requires unique occurrence on octet boundaries. A
            // split IPv4 value has an extra u octet, so inspect that encoding
            // as well as every ordinary contiguous location in the answer.
            let contiguous = bytes.windows(4).filter(|window| *window == value).count();
            let split = usize::from(indexes[3] - indexes[0] != 3);
            if contiguous + split != 1 {
                continue;
            }
            let mut prefix = [0; 16];
            prefix[..length / 8].copy_from_slice(&bytes[..length / 8]);
            let prefix = Prefix {
                bytes: prefix,
                length,
            };
            if !prefixes.contains(&prefix) {
                prefixes.push(prefix)
            }
        }
    }
    prefixes
}

impl Prefix {
    fn embed(&self, address: Ipv4Addr) -> Ipv6Addr {
        let mut result = self.bytes;
        for (index, byte) in positions(self.length).into_iter().zip(address.octets()) {
            result[index] = byte;
        }
        Ipv6Addr::from(result)
    }

    pub(super) fn synthesize(&self, address: Ipv4Addr) -> Option<Ipv6Addr> {
        // RFC 6052 §3.1 forbids non-global IPv4 destinations with the WKP.
        // Network-specific prefixes can intentionally reach private networks.
        if self.length == 96
            && self.bytes[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0]
            && !global_v4(address)
        {
            return None;
        }
        Some(self.embed(address))
    }
}

fn global_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, d] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0 && d != 9 && d != 10)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(address: &str) -> Prefix {
        let (address, length) = address.split_once('/').unwrap();
        Prefix {
            bytes: address.parse::<Ipv6Addr>().unwrap().octets(),
            length: length.parse().unwrap(),
        }
    }

    #[test]
    fn every_rfc6052_format_matches_the_normative_table_and_is_discoverable() {
        for (input, expected) in [
            ("2001:db8::/32", "2001:db8:c000:221::"),
            ("2001:db8:100::/40", "2001:db8:1c0:2:21::"),
            ("2001:db8:122::/48", "2001:db8:122:c000:2:2100::"),
            ("2001:db8:122:300::/56", "2001:db8:122:3c0:0:221::"),
            ("2001:db8:122:344::/64", "2001:db8:122:344:c0:2:2100::"),
            ("2001:db8:122:344::/96", "2001:db8:122:344::c000:221"),
        ] {
            let prefix = prefix(input);
            assert_eq!(
                prefix.embed(Ipv4Addr::new(192, 0, 2, 33)),
                expected.parse::<Ipv6Addr>().unwrap()
            );
            let answers: Vec<_> = WELL_KNOWN
                .iter()
                .map(|v4| SocketAddr::from((prefix.embed((*v4).into()), 0)))
                .collect();
            assert_eq!(discover(&answers), [prefix]);
        }
    }

    #[test]
    fn discovery_keeps_all_prefixes_in_received_order_and_rejects_ambiguous_answers() {
        let a = prefix("2001:db8:12::/48");
        let b = prefix("64:ff9b::/96");
        let answers: Vec<_> = [&a, &b, &a]
            .map(|prefix| SocketAddr::from((prefix.embed(WELL_KNOWN[0].into()), 0)))
            .into_iter()
            .collect();
        assert_eq!(discover(&answers), [a, b]);

        // The first WKA also occurs in the network prefix, so learning must
        // use the second WKA rather than choose an ambiguous /96 format.
        let ambiguous = prefix("2001:db8:c000:aa::/96");
        assert!(
            discover(&[SocketAddr::from((ambiguous.embed(WELL_KNOWN[0].into()), 0))]).is_empty()
        );
        let answers: Vec<_> = WELL_KNOWN
            .map(|value| SocketAddr::from((ambiguous.embed(value.into()), 0)))
            .into_iter()
            .collect();
        assert_eq!(discover(&answers), [ambiguous]);
        assert!(discover(&["[2001:db8::1234]:0".parse().unwrap()]).is_empty());
        assert!(discover(&["[64:ff9b::100:0:c000:aa]:0".parse().unwrap()]).is_empty());
    }

    #[test]
    fn well_known_prefix_never_translates_private_or_special_addresses() {
        let well_known = prefix("64:ff9b::/96");
        let network_specific = prefix("2001:db8::/96");
        for address in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.0.1",
            "172.16.2.3",
            "192.168.2.3",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            let address = address.parse().unwrap();
            assert_eq!(well_known.synthesize(address), None);
            assert!(network_specific.synthesize(address).is_some());
        }
        assert_eq!(
            well_known.synthesize("8.8.8.8".parse().unwrap()),
            Some("64:ff9b::808:808".parse().unwrap())
        );
    }
}
