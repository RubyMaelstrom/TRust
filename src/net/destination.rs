//! RFC 6724 §§2.1, 3, 6 destination ordering, using the kernel's source
//! selection and local policy without another, combined DNS lookup.
//! Native getaddrinfo supplies family-local ordering; this also lets us merge
//! late answers using the same destination properties. Snapshot: 2026-09-06.

use std::{
    cmp::Reverse,
    collections::HashMap,
    net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket},
};

#[derive(Clone, Copy, Default)]
struct Source {
    prefix: u8,
    deprecated: bool,
    home: bool,
    tunnel: bool,
}

struct Row {
    prefix: u128,
    bits: u8,
    value: u32,
}

fn row(prefix: &str, value: u32) -> Option<Row> {
    let (address, bits) = prefix.split_once('/')?;
    let address: IpAddr = address.parse().ok()?;
    let bits: u8 = bits.parse().ok()?;
    let bits = if address.is_ipv4() {
        bits.checked_add(96)?
    } else {
        bits
    };
    if bits > 128 {
        return None;
    }
    Some(Row {
        prefix: u128::from(mapped(address)),
        bits,
        value,
    })
}

fn mapped(address: IpAddr) -> Ipv6Addr {
    match address {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    }
}

fn lookup(rows: &[Row], address: IpAddr) -> u32 {
    let value = u128::from(mapped(address));
    rows.iter()
        .filter(|row| row.bits == 0 || (value ^ row.prefix).leading_zeros() >= u32::from(row.bits))
        .max_by_key(|row| row.bits)
        .map_or(0, |row| row.value)
}

struct Policy {
    precedence: Vec<Row>,
    label: Vec<Row>,
    scope_v4: Vec<Row>,
    sources: HashMap<IpAddr, Source>,
}

impl Policy {
    fn new(text: &str) -> Self {
        let mut precedence = Vec::new();
        let mut label = Vec::new();
        let mut scope_v4 = Vec::new();
        for line in text.lines() {
            let mut fields = line.split('#').next().unwrap_or("").split_whitespace();
            let (Some(kind), Some(prefix), Some(value)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Some(entry) = value.parse().ok().and_then(|value| row(prefix, value)) else {
                continue;
            };
            match kind {
                "precedence" => precedence.push(entry),
                "label" => label.push(entry),
                "scopev4" => scope_v4.push(entry),
                _ => {}
            }
        }
        const DEFAULTS: [(&str, u32, u32); 9] = [
            ("::1/128", 50, 0),
            ("::/0", 40, 1),
            ("::ffff:0:0/96", 35, 4),
            ("2002::/16", 30, 2),
            ("2001::/32", 5, 5),
            ("fc00::/7", 3, 13),
            ("::/96", 1, 3),
            ("fec0::/10", 1, 11),
            ("3ffe::/16", 1, 12),
        ];
        // Like getaddrinfo's policy file, the first configured row replaces
        // the corresponding default table instead of silently extending it.
        if precedence.is_empty() {
            precedence.extend(
                DEFAULTS
                    .iter()
                    .map(|(prefix, value, _)| row(prefix, *value).unwrap()),
            );
        }
        if label.is_empty() {
            label.extend(
                DEFAULTS
                    .iter()
                    .map(|(prefix, _, value)| row(prefix, *value).unwrap()),
            );
        }
        if scope_v4.is_empty() {
            scope_v4.extend(
                [
                    ("::ffff:169.254.0.0/112", 2),
                    ("::ffff:127.0.0.0/104", 2),
                    ("::/0", 14),
                ]
                .map(|(prefix, value)| row(prefix, value).unwrap()),
            );
        }
        Self {
            precedence,
            label,
            scope_v4,
            sources: HashMap::new(),
        }
    }

    fn system() -> Self {
        // Bound policy reads even if an administrator accidentally replaces
        // a configuration file with an enormous file. No network I/O occurs.
        fn read(path: &str) -> String {
            use std::io::Read;
            let mut text = String::new();
            if let Ok(file) = std::fs::File::open(path) {
                let _ = file.take(131_072).read_to_string(&mut text);
            }
            text
        }
        let mut policy = Self::new(&read("/etc/gai.conf"));
        #[cfg(unix)]
        {
            let mut head = std::ptr::null_mut();
            // SAFETY: getifaddrs returns a linked list owned by freeifaddrs;
            // its sockaddr pointers remain live until the guard is dropped.
            if unsafe { libc::getifaddrs(&mut head) } == 0 {
                struct Addresses(*mut libc::ifaddrs);
                impl Drop for Addresses {
                    fn drop(&mut self) {
                        unsafe { libc::freeifaddrs(self.0) }
                    }
                }
                let _addresses = Addresses(head);
                while !head.is_null() {
                    let interface = unsafe { &*head };
                    if !interface.ifa_addr.is_null() && !interface.ifa_netmask.is_null() {
                        let family = unsafe { (*interface.ifa_addr).sa_family } as i32;
                        let value = if family == libc::AF_INET {
                            let address =
                                unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in>() };
                            let mask =
                                unsafe { &*interface.ifa_netmask.cast::<libc::sockaddr_in>() };
                            Some((
                                IpAddr::V4(address.sin_addr.s_addr.to_ne_bytes().into()),
                                u32::from_be(mask.sin_addr.s_addr).leading_ones() as u8,
                            ))
                        } else if family == libc::AF_INET6 {
                            let address =
                                unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in6>() };
                            let mask =
                                unsafe { &*interface.ifa_netmask.cast::<libc::sockaddr_in6>() };
                            Some((
                                IpAddr::V6(address.sin6_addr.s6_addr.into()),
                                u128::from_be_bytes(mask.sin6_addr.s6_addr).leading_ones() as u8,
                            ))
                        } else {
                            None
                        };
                        if let Some((address, prefix)) = value {
                            policy.sources.insert(
                                address,
                                Source {
                                    prefix,
                                    ..Source::default()
                                },
                            );
                        }
                    }
                    head = interface.ifa_next;
                }
            }
        }
        #[cfg(target_os = "linux")]
        for line in read("/proc/net/if_inet6").lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 6 {
                continue;
            }
            let (Ok(address), Ok(prefix), Ok(flags)) = (
                u128::from_str_radix(fields[0], 16),
                u8::from_str_radix(fields[2], 16),
                u32::from_str_radix(fields[4], 16),
            ) else {
                continue;
            };
            // Interface type is kernel-provided; only mark an encapsulating
            // tunnel when it is known, never infer it from an interface name.
            let tunnel = fields[5]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
                && matches!(
                    read(&format!("/sys/class/net/{}/type", fields[5])).trim(),
                    "768" | "769" | "776"
                );
            policy.sources.insert(
                IpAddr::V6(address.into()),
                Source {
                    prefix,
                    deprecated: flags & 0x20 != 0,
                    home: flags & 0x10 != 0,
                    tunnel,
                },
            );
        }
        policy
    }

    fn scope(&self, address: IpAddr) -> u32 {
        match address {
            IpAddr::V4(_) => lookup(&self.scope_v4, address),
            IpAddr::V6(v6) if v6.is_loopback() || v6.is_unicast_link_local() => 2,
            IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfec0 => 5,
            IpAddr::V6(v6) if v6.is_multicast() => u32::from(v6.octets()[1] & 15),
            _ => 14,
        }
    }

    fn rank(&self, destination: SocketAddr, source: Option<IpAddr>) -> Rank {
        let destination = destination.ip();
        let properties = source
            .and_then(|source| self.sources.get(&source))
            .copied()
            .unwrap_or_default();
        Rank {
            usable: source.is_some(),
            matching_scope: source
                .is_some_and(|source| self.scope(source) == self.scope(destination)),
            preferred: !properties.deprecated,
            home: properties.home,
            matching_label: source.is_some_and(|source| {
                lookup(&self.label, source) == lookup(&self.label, destination)
            }),
            precedence: lookup(&self.precedence, destination),
            native: !properties.tunnel,
            scope: Reverse(self.scope(destination)),
            prefix: source.map_or(0, |source| {
                let common =
                    (u128::from(mapped(source)) ^ u128::from(mapped(destination))).leading_zeros();
                let common = if destination.is_ipv4() {
                    common.saturating_sub(96)
                } else {
                    common
                };
                common.min(u32::from(properties.prefix))
            }),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Rank {
    usable: bool,
    matching_scope: bool,
    preferred: bool,
    home: bool,
    matching_label: bool,
    precedence: u32,
    native: bool,
    scope: Reverse<u32>,
    prefix: u32,
}

impl Rank {
    fn compare(self, other: Self, same_family: bool) -> std::cmp::Ordering {
        (
            self.usable,
            self.matching_scope,
            self.preferred,
            self.home,
            self.matching_label,
            self.precedence,
            self.native,
            self.scope,
        )
            .cmp(&(
                other.usable,
                other.matching_scope,
                other.preferred,
                other.home,
                other.matching_label,
                other.precedence,
                other.native,
                other.scope,
            ))
            .then_with(|| {
                if same_family {
                    self.prefix.cmp(&other.prefix)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
    }
}

/// UDP connect selects a route/source locally and sends no datagram.
fn source(destination: SocketAddr) -> Option<IpAddr> {
    let socket = UdpSocket::bind(if destination.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    })
    .ok()?;
    socket.connect(destination).ok()?;
    let source = socket.local_addr().ok()?.ip();
    (!source.is_unspecified()).then_some(source)
}

pub(super) fn ipv6_only() -> bool {
    source("192.0.0.170:9".parse().unwrap()).is_none()
        && source("[2001:4860:4860::8888]:9".parse().unwrap()).is_some()
}

pub(super) fn sort(addresses: &mut [SocketAddr]) {
    #[cfg(windows)]
    if let Some(sorted) = windows_sort(addresses) {
        addresses.copy_from_slice(&sorted);
        return;
    }
    let policy = Policy::system();
    let ranks: HashMap<_, _> = addresses
        .iter()
        .copied()
        .map(|address| (address, policy.rank(address, source(address))))
        .collect();
    // Stable ties retain native resolver order (RFC 6724 rule 10).
    addresses.sort_by(|a, b| ranks[b].compare(ranks[a], a.is_ipv6() == b.is_ipv6()));
}

/// Native policy includes administrator-configured Windows prefix policies.
/// https://learn.microsoft.com/windows/win32/api/netioapi/nf-netioapi-createsortedaddresspairs
#[cfg(windows)]
fn windows_sort(addresses: &[SocketAddr]) -> Option<Vec<SocketAddr>> {
    use std::{mem, ptr};
    use windows_sys::Win32::{
        NetworkManagement::IpHelper::{CreateSortedAddressPairs, FreeMibTable},
        Networking::WinSock::*,
    };
    if addresses.is_empty() {
        return Some(Vec::new());
    }
    let destinations: Vec<SOCKADDR_IN6> = addresses
        .iter()
        .map(|address| {
            let mut destination: SOCKADDR_IN6 = unsafe { mem::zeroed() };
            destination.sin6_family = AF_INET6;
            destination.sin6_port = address.port().to_be();
            destination.sin6_addr.u.Byte = mapped(address.ip()).octets();
            if let SocketAddr::V6(address) = address {
                destination.sin6_flowinfo = address.flowinfo().to_be();
                destination.Anonymous.sin6_scope_id = address.scope_id();
            }
            destination
        })
        .collect();
    let mut pairs = ptr::null_mut();
    let mut count = 0;
    // SAFETY: the input array is live for the entire call; Windows allocates
    // the result and all associated addresses in one FreeMibTable allocation.
    let result = unsafe {
        CreateSortedAddressPairs(
            ptr::null(),
            0,
            destinations.as_ptr(),
            destinations.len() as u32,
            0,
            &mut pairs,
            &mut count,
        )
    };
    if result != 0 {
        return None;
    }
    struct Results(*mut SOCKADDR_IN6_PAIR);
    impl Drop for Results {
        fn drop(&mut self) {
            unsafe { FreeMibTable(self.0.cast()) }
        }
    }
    let _results = Results(pairs);
    if pairs.is_null() || count as usize > addresses.len() {
        return None;
    }
    let mut used = std::collections::HashSet::new();
    let mut sorted = Vec::with_capacity(addresses.len());
    for pair in unsafe { std::slice::from_raw_parts(pairs, count as usize) } {
        if pair.DestinationAddress.is_null() {
            return None;
        }
        let destination = unsafe { &*pair.DestinationAddress };
        let address = Ipv6Addr::from(unsafe { destination.sin6_addr.u.Byte });
        let port = u16::from_be(destination.sin6_port);
        let scope = unsafe { destination.Anonymous.sin6_scope_id };
        if let Some((index, original)) =
            addresses
                .iter()
                .copied()
                .enumerate()
                .find(|(index, original)| {
                    !used.contains(index)
                        && mapped(original.ip()) == address
                        && original.port() == port
                        && match original {
                            SocketAddr::V4(_) => true,
                            SocketAddr::V6(v6) => v6.scope_id() == 0 || v6.scope_id() == scope,
                        }
                })
        {
            used.insert(index);
            sorted.push(match original {
                SocketAddr::V6(mut v6) => {
                    v6.set_scope_id(scope);
                    SocketAddr::V6(v6)
                }
                v4 => v4,
            });
        }
    }
    sorted.extend(
        addresses
            .iter()
            .enumerate()
            .filter(|(index, _)| !used.contains(index))
            .map(|(_, address)| *address),
    );
    Some(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_rules_apply_before_family_precedence_and_keep_configured_policy() {
        let policy = Policy::new("");
        let v4: SocketAddr = "192.0.2.1:80".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:80".parse().unwrap();
        let source4: IpAddr = "192.0.2.2".parse().unwrap();
        let source6: IpAddr = "2001:db8::2".parse().unwrap();
        assert!(
            policy
                .rank(v6, Some(source6))
                .compare(policy.rank(v4, Some(source4)), false)
                .is_gt()
        );
        assert!(
            policy
                .rank(v4, Some(source4))
                .compare(policy.rank(v6, None), false)
                .is_gt()
        );
        assert!(
            policy
                .rank(v4, Some(source4))
                .compare(policy.rank(v6, Some("fe80::1".parse().unwrap())), false)
                .is_gt()
        );
        let policy = Policy::new("precedence ::ffff:0:0/96 100\nprecedence ::/0 40");
        assert!(
            policy
                .rank(v4, Some(source4))
                .compare(policy.rank(v6, Some(source6)), false)
                .is_gt()
        );
    }

    #[test]
    fn source_status_and_ipv4_scope_follow_normative_rule_order() {
        let mut policy = Policy::new("");
        let v4: SocketAddr = "192.0.2.1:80".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:80".parse().unwrap();
        let source4: IpAddr = "192.0.2.2".parse().unwrap();
        let source6: IpAddr = "2001:db8::2".parse().unwrap();
        policy.sources.insert(
            source6,
            Source {
                deprecated: true,
                ..Source::default()
            },
        );
        assert!(
            policy
                .rank(v4, Some(source4))
                .compare(policy.rank(v6, Some(source6)), false)
                .is_gt()
        );
        assert_eq!(policy.scope("192.168.1.2".parse().unwrap()), 14);
        assert_eq!(policy.scope("127.0.0.1".parse().unwrap()), 2);
        assert_eq!(policy.scope("169.254.1.2".parse().unwrap()), 2);
        assert_eq!(policy.scope("fc00::1".parse().unwrap()), 14);
    }
}
