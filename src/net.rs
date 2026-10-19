//! Shared asynchronous DNS/TCP scheduling. RFC 8305 §§3–7, RFC 6724 §6,
//! RFC 7050 §3 and RFC 6052 §2 (RFC Editor snapshot 2026-09-06).
//! Family-specific system lookups preserve NSS/VPN/name-service policy while
//! allowing connection attempts before the other DNS family has finished.

use futures::{StreamExt, stream::FuturesUnordered};
use std::{
    collections::HashSet,
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    net::TcpStream,
    time::{Instant, sleep_until, timeout_at},
};

mod destination;
mod nat64;
mod resolver;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);
const FAILURE_DELAY: Duration = Duration::from_millis(100);
const RESOLUTION_DELAY: Duration = Duration::from_millis(50);
const LOCAL_SYNTHESIS_DELAY: Duration = Duration::from_secs(2);
// Also the maximum accepted by Windows' native destination sorter.
const MAX_CANDIDATES: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Family {
    V4,
    V6,
}

impl Family {
    fn of(address: SocketAddr) -> Self {
        if address.is_ipv6() {
            Self::V6
        } else {
            Self::V4
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Query {
    Addresses(Family),
    Pref64,
}

pub(crate) async fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    connect_with_dns_callback(host, port, deadline, || {}).await
}

/// Fetch's DNS interval ends when an address is ready for the first TCP
/// attempt; a slower family remains part of the same cancellable race.
pub(crate) async fn connect_with_dns_callback(
    host: &str,
    port: u16,
    deadline: Instant,
    dns_ready: impl FnOnce(),
) -> io::Result<TcpStream> {
    establish_with(host, port, deadline, dns_ready, TcpStream::connect).await
}

/// Shared address scheduling for transports with their own handshake, such
/// as QUIC. A successful future owns its connection; dropping every other
/// future cancels its transport resources before this function returns.
pub(crate) async fn establish_with<T, C, CF>(
    host: &str,
    port: u16,
    deadline: Instant,
    dns_ready: impl FnOnce(),
    connect: C,
) -> io::Result<T>
where
    C: FnMut(SocketAddr) -> CF,
    CF: Future<Output = io::Result<T>>,
{
    // URL host serialization brackets IPv6 literals; native getaddrinfo
    // expects the address itself, including any platform scope suffix.
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    establish(
        host.parse().ok(),
        port,
        deadline,
        destination::ipv6_only(),
        |query| {
            resolver::lookup(
                if query == Query::Pref64 {
                    "ipv4only.arpa."
                } else {
                    host
                },
                port,
                query,
            )
        },
        connect,
        dns_ready,
        destination::sort,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn connect_addresses(
    addresses: Vec<SocketAddr>,
    deadline: Instant,
) -> io::Result<TcpStream> {
    race(addresses, deadline, TcpStream::connect).await
}

async fn query_result<F: Future<Output = io::Result<Vec<SocketAddr>>>>(
    query: Query,
    future: F,
) -> (Query, io::Result<Vec<SocketAddr>>) {
    (query, future.await)
}

fn add_candidates(addresses: &mut Vec<SocketAddr>, new: impl IntoIterator<Item = SocketAddr>) {
    for address in new {
        if addresses.len() == MAX_CANDIDATES {
            break;
        }
        if !addresses.contains(&address) {
            addresses.push(address)
        }
    }
}

// Keep resolver, connection, timing and policy boundaries injectable so the
// same state machine is exercised under deterministic clocks.
#[allow(clippy::too_many_arguments)]
async fn establish<T, L, LF, C, CF, R, S>(
    literal: Option<IpAddr>,
    port: u16,
    deadline: Instant,
    ipv6_only: bool,
    mut lookup: L,
    mut connect: C,
    dns_ready: R,
    mut sort: S,
) -> io::Result<T>
where
    L: FnMut(Query) -> LF,
    LF: Future<Output = io::Result<Vec<SocketAddr>>>,
    C: FnMut(SocketAddr) -> CF,
    CF: Future<Output = io::Result<T>>,
    R: FnOnce(),
    S: FnMut(&mut [SocketAddr]),
{
    timeout_at(deadline, async {
        let mut queries = FuturesUnordered::new();
        let mut addresses = Vec::new();
        let mut attempted = HashSet::new();
        let mut v4_addresses = Vec::new();
        let mut prefixes = Vec::new();
        let mut v6_done = literal.is_some();
        let mut v4_started = literal.is_some();
        let mut pref64_started = false;
        let mut resolution_until = None;
        let mut synthesis_at = None;
        let mut last_start = None;
        let mut dns_ready = Some(dns_ready);
        let mut next = Instant::now();
        let mut attempts = FuturesUnordered::new();
        let mut error = io::Error::new(io::ErrorKind::AddrNotAvailable, "No addresses found");
        if let Some(address) = literal {
            addresses.push(SocketAddr::new(address, port));
            if address.is_ipv4() && ipv6_only {
                v4_addresses.extend(addresses.iter().copied());
                pref64_started = true;
                queries.push(query_result(Query::Pref64, lookup(Query::Pref64)));
            }
        } else {
            // RFC 8305 §3: AAAA immediately followed by A on dual stack.
            let query = Query::Addresses(Family::V6);
            queries.push(query_result(query, lookup(query)));
            if !ipv6_only {
                let query = Query::Addresses(Family::V4);
                queries.push(query_result(query, lookup(query)));
                v4_started = true;
            }
        }
        loop {
            let candidate = interleave(addresses.clone()).into_iter().find(|address| !attempted.contains(address));
            // RFC 8305 §7.2: a native AAAA record can prevent DNS64 from
            // synthesizing. After the final native attempt, allow A lookup
            // and local synthesis without canceling any native attempt.
            if ipv6_only && !v4_started && v6_done && candidate.is_none() && synthesis_at.is_none() {
                synthesis_at = Some(last_start.map_or_else(Instant::now, |last| last + LOCAL_SYNTHESIS_DELAY));
            }
            if candidate.is_none() && attempts.is_empty() && queries.is_empty() && synthesis_at.is_none() {
                return Err(error);
            }
            let launch_at = next.max(resolution_until.unwrap_or(next));
            tokio::select! {
                biased;
                Some(result) = attempts.next(), if !attempts.is_empty() => {
                    match result {
                        Ok(stream) => return Ok(stream),
                        Err(e) => {
                            error = e;
                            if let Some(last) = last_start { next = next.min(last + FAILURE_DELAY) }
                        }
                    }
                }
                Some((query, result)) = queries.next(), if !queries.is_empty() => {
                    match query {
                        Query::Addresses(family) => {
                            let resolved = match result {
                                Ok(addresses) => addresses.into_iter().filter(|address| Family::of(*address) == family).collect::<Vec<_>>(),
                                Err(e) => { if attempted.is_empty() { error = e } Vec::new() },
                            };
                            if family == Family::V6 {
                                v6_done = true;
                                resolution_until = None;
                                // No usable AAAA: local synthesis need not
                                // wait for the last-resort timer.
                                if resolved.is_empty() && ipv6_only && !v4_started {
                                    synthesis_at = Some(Instant::now());
                                }
                            } else {
                                if !v6_done && !resolved.is_empty() && last_start.is_none() {
                                    resolution_until = Some(Instant::now() + RESOLUTION_DELAY);
                                }
                                if ipv6_only { add_candidates(&mut v4_addresses, resolved.iter().copied()) }
                            }
                            add_candidates(&mut addresses, resolved);
                        }
                        Query::Pref64 => {
                            // Failure to discover a translator does not
                            // invalidate native connection attempts.
                            if let Ok(answers) = result { prefixes = nat64::discover(&answers) }
                        }
                    }
                    // RFC 7050 §3 requires every discovered prefix, in DNS
                    // order. Deduplication also handles DNS64 answers already
                    // equal to a locally synthesized destination.
                    for prefix in &prefixes {
                        add_candidates(&mut addresses, v4_addresses.iter().filter_map(|address| {
                            let SocketAddr::V4(address) = address else { return None };
                            prefix.synthesize(*address.ip()).map(|ip| SocketAddr::from((ip, port)))
                        }));
                    }
                    sort(&mut addresses);
                }
                _ = sleep_until(synthesis_at.unwrap_or(deadline)), if synthesis_at.is_some() => {
                    synthesis_at = None;
                    v4_started = true;
                    let query = Query::Addresses(Family::V4);
                    queries.push(query_result(query, lookup(query)));
                    if !pref64_started {
                        pref64_started = true;
                        queries.push(query_result(Query::Pref64, lookup(Query::Pref64)));
                    }
                }
                _ = sleep_until(launch_at), if candidate.is_some() => {
                    if let Some(dns_ready) = dns_ready.take() { dns_ready() }
                    let candidate = candidate.unwrap();
                    attempted.insert(candidate);
                    attempts.push(connect(candidate));
                    let start = Instant::now();
                    last_start = Some(start);
                    next = start + ATTEMPT_DELAY;
                }
            }
        }
    }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Connection establishment timed out"))?
}

fn interleave(addresses: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let first_v6 = addresses.first().is_some_and(SocketAddr::is_ipv6);
    let mut seen = HashSet::new();
    let (first, second): (Vec<_>, Vec<_>) = addresses
        .into_iter()
        .filter(|address| seen.insert(*address))
        .partition(|address| address.is_ipv6() == first_v6);
    let mut first = first.into_iter();
    let mut second = second.into_iter();
    let mut ordered = Vec::with_capacity(seen.len());
    loop {
        match (first.next(), second.next()) {
            (None, None) => break,
            (a, b) => ordered.extend(a.into_iter().chain(b)),
        }
    }
    ordered
}

#[cfg(test)]
async fn race<T, F, Fut>(
    addresses: Vec<SocketAddr>,
    deadline: Instant,
    mut connect: F,
) -> io::Result<T>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    timeout_at(deadline, async {
        let mut candidates = interleave(addresses).into_iter().peekable();
        let mut attempts = FuturesUnordered::new();
        let mut last_start = Instant::now();
        let mut next = last_start;
        let mut error = io::Error::new(io::ErrorKind::AddrNotAvailable, "No addresses found");
        loop {
            if candidates.peek().is_none() && attempts.is_empty() {
                return Err(error);
            }
            tokio::select! {
                // Prefer a completed socket over launching another attempt.
                biased;
                Some(result) = attempts.next(), if !attempts.is_empty() => {
                    match result {
                        Ok(stream) => return Ok(stream),
                        Err(e) => {
                            error = e;
                            // A hard failure can advance the race, but §5
                            // forbids unpaced bursts even when all routes fail.
                            next = next.min(last_start + FAILURE_DELAY);
                        }
                    }
                }
                _ = sleep_until(next), if candidates.peek().is_some() => {
                    attempts.push(connect(candidates.next().unwrap()));
                    last_start = Instant::now();
                    next = last_start + ATTEMPT_DELAY;
                }
            }
        }
        // Futures own their sockets: return, timeout, and caller cancellation
        // all close pending attempts without leaving background dial tasks.
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TCP connection timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    fn addresses() -> [SocketAddr; 4] {
        [
            "192.0.2.1:80",
            "192.0.2.2:80",
            "[2001:db8::1]:80",
            "[2001:db8::2]:80",
        ]
        .map(|address| address.parse().unwrap())
    }

    struct Active(Rc<Cell<usize>>);
    impl Active {
        fn new(count: &Rc<Cell<usize>>) -> Self {
            count.set(count.get() + 1);
            Self(count.clone())
        }
    }
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.set(self.0.get() - 1);
        }
    }

    fn prefer_v6(addresses: &mut [SocketAddr]) {
        addresses.sort_by_key(SocketAddr::is_ipv4);
    }

    #[tokio::test(start_paused = true)]
    async fn delayed_aaaa_does_not_hold_a_positive_a_answer_or_leave_a_dns_future_alive() {
        let start = Instant::now();
        let active = Rc::new(Cell::new(0));
        let launches = Rc::new(RefCell::new(Vec::new()));
        let a = addresses()[0];
        let result = establish(
            None,
            80,
            start + CONNECT_TIMEOUT,
            false,
            |query| {
                let active = active.clone();
                async move {
                    let _active = Active::new(&active);
                    match query {
                        Query::Addresses(Family::V4) => {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            Ok(vec![a])
                        }
                        Query::Addresses(Family::V6) => std::future::pending().await,
                        Query::Pref64 => unreachable!(),
                    }
                }
            },
            |address| {
                let launches = launches.clone();
                async move {
                    launches
                        .borrow_mut()
                        .push((address, Instant::now() - start));
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(address)
                }
            },
            || {},
            prefer_v6,
        )
        .await
        .unwrap();
        assert_eq!(result, a);
        assert_eq!(*launches.borrow(), [(a, Duration::from_millis(70))]);
        assert_eq!(Instant::now() - start, Duration::from_millis(80));
        assert_eq!(active.get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn timely_aaaa_is_preferred_but_negative_aaaa_releases_a_without_the_full_delay() {
        let [a, _, v6, _] = addresses();
        for (aaaa, delay, winner, elapsed) in [(vec![v6], 40, v6, 40), (vec![], 20, a, 20)] {
            let start = Instant::now();
            let result = establish(
                None,
                80,
                start + CONNECT_TIMEOUT,
                false,
                |query| {
                    let aaaa = aaaa.clone();
                    async move {
                        match query {
                            Query::Addresses(Family::V4) => Ok(vec![a]),
                            Query::Addresses(Family::V6) => {
                                tokio::time::sleep(Duration::from_millis(delay)).await;
                                Ok(aaaa)
                            }
                            Query::Pref64 => unreachable!(),
                        }
                    }
                },
                |address| std::future::ready(Ok(address)),
                || {},
                prefer_v6,
            )
            .await
            .unwrap();
            assert_eq!(result, winner);
            assert_eq!(Instant::now() - start, Duration::from_millis(elapsed));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn late_aaaa_joins_a_running_race_with_the_connection_attempt_spacing_intact() {
        let [a, b, v6, _] = addresses();
        let start = Instant::now();
        let launches = Rc::new(RefCell::new(Vec::new()));
        let active = Rc::new(Cell::new(0));
        let result = establish(
            None,
            80,
            start + CONNECT_TIMEOUT,
            false,
            |query| async move {
                match query {
                    Query::Addresses(Family::V4) => Ok(vec![a, b]),
                    Query::Addresses(Family::V6) => {
                        tokio::time::sleep(Duration::from_millis(125)).await;
                        Ok(vec![v6])
                    }
                    Query::Pref64 => unreachable!(),
                }
            },
            |address| {
                let launches = launches.clone();
                let active = active.clone();
                async move {
                    let _active = Active::new(&active);
                    launches
                        .borrow_mut()
                        .push((address, Instant::now() - start));
                    if address == v6 {
                        Ok(address)
                    } else {
                        std::future::pending().await
                    }
                }
            },
            || {},
            prefer_v6,
        )
        .await
        .unwrap();
        assert_eq!(result, v6);
        assert_eq!(
            *launches.borrow(),
            [
                (a, RESOLUTION_DELAY),
                (v6, RESOLUTION_DELAY + ATTEMPT_DELAY)
            ]
        );
        assert_eq!(active.get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn incremental_dns_and_pending_connections_share_the_callers_deadline_and_cancellation() {
        let a = addresses()[0];
        for cancel in [false, true] {
            let start = Instant::now();
            let active = Rc::new(Cell::new(0));
            let ready = Rc::new(Cell::new(0));
            let future = establish(
                None,
                80,
                start + Duration::from_millis(200),
                false,
                |query| {
                    let active = active.clone();
                    async move {
                        let _active = Active::new(&active);
                        if query == Query::Addresses(Family::V4) {
                            Ok(vec![a])
                        } else {
                            std::future::pending().await
                        }
                    }
                },
                |_| {
                    let active = active.clone();
                    async move {
                        let _active = Active::new(&active);
                        std::future::pending::<io::Result<SocketAddr>>().await
                    }
                },
                {
                    let ready = ready.clone();
                    move || ready.set(ready.get() + 1)
                },
                prefer_v6,
            );
            if cancel {
                assert!(
                    timeout_at(start + Duration::from_millis(120), future)
                        .await
                        .is_err()
                );
                assert_eq!(Instant::now() - start, Duration::from_millis(120));
            } else {
                assert_eq!(future.await.unwrap_err().kind(), io::ErrorKind::TimedOut);
                assert_eq!(Instant::now() - start, Duration::from_millis(200));
            }
            assert_eq!(ready.get(), 1);
            assert_eq!(active.get(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ipv4_literals_synthesize_every_discovered_nat64_prefix_in_order() {
        let start = Instant::now();
        let raw: SocketAddr = "8.8.8.8:80".parse().unwrap();
        let first: SocketAddr = "[2001:db8:12::808:808]:80".parse().unwrap();
        let second: SocketAddr = "[64:ff9b::808:808]:80".parse().unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let launches = Rc::new(RefCell::new(Vec::new()));
        let result = establish(
            Some(raw.ip()),
            80,
            start + CONNECT_TIMEOUT,
            true,
            |query| {
                calls.borrow_mut().push(query);
                async move {
                    assert_eq!(query, Query::Pref64);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    Ok(vec![
                        "[2001:db8:12::c000:aa]:80".parse().unwrap(),
                        "[64:ff9b::c000:aa]:80".parse().unwrap(),
                    ])
                }
            },
            |address| {
                launches
                    .borrow_mut()
                    .push((address, Instant::now() - start));
                std::future::ready(if address == second {
                    Ok(address)
                } else {
                    Err(io::Error::from(io::ErrorKind::NetworkUnreachable))
                })
            },
            || {},
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(result, second);
        assert_eq!(*calls.borrow(), [Query::Pref64]);
        assert_eq!(
            *launches.borrow(),
            [
                (raw, Duration::ZERO),
                (first, FAILURE_DELAY),
                (second, FAILURE_DELAY * 2)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ipv6_only_broken_native_aaaa_uses_delayed_local_synthesis_without_dropping_native_attempts()
     {
        let start = Instant::now();
        let native: SocketAddr = "[2001:db8::1]:80".parse().unwrap();
        let raw: SocketAddr = "8.8.8.8:80".parse().unwrap();
        let synthetic: SocketAddr = "[64:ff9b::808:808]:80".parse().unwrap();
        let active = Rc::new(Cell::new(0));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let result = establish(
            None,
            80,
            start + CONNECT_TIMEOUT,
            true,
            |query| {
                calls.borrow_mut().push((query, Instant::now() - start));
                async move {
                    Ok(match query {
                        Query::Addresses(Family::V6) => vec![native],
                        Query::Addresses(Family::V4) => vec![raw],
                        Query::Pref64 => vec!["[64:ff9b::c000:aa]:80".parse().unwrap()],
                    })
                }
            },
            |address| {
                let active = active.clone();
                async move {
                    let _active = Active::new(&active);
                    if address == native {
                        std::future::pending().await
                    } else if address == synthetic {
                        Ok(address)
                    } else {
                        Err(io::Error::from(io::ErrorKind::NetworkUnreachable))
                    }
                }
            },
            || {},
            prefer_v6,
        )
        .await
        .unwrap();
        assert_eq!(result, synthetic);
        assert_eq!(
            *calls.borrow(),
            [
                (Query::Addresses(Family::V6), Duration::ZERO),
                (Query::Addresses(Family::V4), LOCAL_SYNTHESIS_DELAY),
                (Query::Pref64, LOCAL_SYNTHESIS_DELAY),
            ]
        );
        assert_eq!(
            Instant::now() - start,
            LOCAL_SYNTHESIS_DELAY + FAILURE_DELAY
        );
        assert_eq!(active.get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn ipv6_only_negative_aaaa_starts_synthesis_without_the_last_resort_delay() {
        let start = Instant::now();
        let synthetic: SocketAddr = "[64:ff9b::808:808]:80".parse().unwrap();
        let result = establish(
            None,
            80,
            start + CONNECT_TIMEOUT,
            true,
            |query| async move {
                Ok(match query {
                    Query::Addresses(Family::V6) => vec![],
                    Query::Addresses(Family::V4) => vec!["8.8.8.8:80".parse().unwrap()],
                    Query::Pref64 => vec!["[64:ff9b::c000:aa]:80".parse().unwrap()],
                })
            },
            |address| {
                std::future::ready(if address == synthetic {
                    Ok(address)
                } else {
                    Err(io::Error::from(io::ErrorKind::NetworkUnreachable))
                })
            },
            || {},
            prefer_v6,
        )
        .await
        .unwrap();
        assert_eq!(result, synthetic);
        assert_eq!(Instant::now(), start);
    }

    #[test]
    fn address_order_preserves_host_preference_and_each_family_order() {
        let [a, b, c, d] = addresses();
        assert_eq!(interleave(vec![a, b, c, d]), [a, c, b, d]);
        assert_eq!(interleave(vec![c, d, a, b]), [c, a, d, b]);
        assert_eq!(interleave(vec![a, a, b, a]), [a, b]);
        assert_eq!(interleave(vec![c, d, a, c]), [c, a, d]);
        assert!(interleave(vec![]).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_first_family_does_not_block_the_other_family() {
        let [a, b, c, d] = addresses();
        for (ordered, first, winner) in [(vec![a, b, c, d], a, c), (vec![c, d, a, b], c, a)] {
            let start = Instant::now();
            let launches = Rc::new(RefCell::new(Vec::new()));
            let active = Rc::new(Cell::new(0));
            let result = race(ordered, start + CONNECT_TIMEOUT, |address| {
                let launches = launches.clone();
                let active = active.clone();
                async move {
                    let _active = Active::new(&active);
                    launches
                        .borrow_mut()
                        .push((address, Instant::now() - start));
                    if address == winner {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok(address)
                    } else {
                        std::future::pending().await
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(result, winner);
            assert_eq!(
                *launches.borrow(),
                [(first, Duration::ZERO), (winner, ATTEMPT_DELAY)]
            );
            assert_eq!(
                Instant::now() - start,
                ATTEMPT_DELAY + Duration::from_millis(20)
            );
            assert_eq!(active.get(), 0, "the stalled loser must be dropped");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_attempt_advances_without_canceling_the_pending_first_socket() {
        let [a, b, c, _] = addresses();
        let start = Instant::now();
        let launches = Rc::new(RefCell::new(Vec::new()));
        let winner = race(vec![a, b, c], start + CONNECT_TIMEOUT, |address| {
            let launches = launches.clone();
            async move {
                launches
                    .borrow_mut()
                    .push((address, Instant::now() - start));
                if address == c {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else if address == a {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    Ok(address)
                } else {
                    std::future::pending().await
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            winner, a,
            "starting a later attempt must leave earlier sockets alive"
        );
        assert_eq!(
            *launches.borrow(),
            [
                (a, Duration::ZERO),
                (c, ATTEMPT_DELAY),
                (b, ATTEMPT_DELAY + FAILURE_DELAY)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_addresses_are_bounded_and_canceled_at_the_deadline() {
        let start = Instant::now();
        let active = Rc::new(Cell::new(0));
        let error = race::<(), _, _>(addresses().to_vec(), start + Duration::from_secs(1), |_| {
            let active = active.clone();
            async move {
                let _active = Active::new(&active);
                std::future::pending().await
            }
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(Instant::now() - start, Duration::from_secs(1));
        assert_eq!(active.get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn fast_success_and_empty_or_failed_resolutions_finish_without_extra_attempts() {
        let start = Instant::now();
        let mut launches = 0;
        let result = race(addresses().to_vec(), start + CONNECT_TIMEOUT, |address| {
            launches += 1;
            std::future::ready(Ok(address))
        })
        .await
        .unwrap();
        assert_eq!(result, addresses()[0]);
        assert_eq!(launches, 1);
        assert_eq!(Instant::now(), start);
        let error = race::<(), _, _>(vec![], start + CONNECT_TIMEOUT, |_| async {
            unreachable!()
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrNotAvailable);
        let error = race::<(), _, _>(addresses().to_vec(), start + CONNECT_TIMEOUT, |_| async {
            Err(io::Error::from(io::ErrorKind::ConnectionRefused))
        })
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(Instant::now() - start, FAILURE_DELAY * 3);
    }

    #[tokio::test]
    async fn connects_to_a_real_socket_after_a_refused_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = refused.local_addr().unwrap();
        drop(refused);
        let address = listener.local_addr().unwrap();
        let stream = connect_addresses(vec![closed, address], Instant::now() + CONNECT_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), address);
    }

    #[tokio::test]
    async fn bracketed_ipv6_url_hosts_connect_as_numeric_literals() {
        let listener = match tokio::net::TcpListener::bind("[::1]:0").await {
            Ok(listener) => listener,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
                ) =>
            {
                return;
            }
            Err(error) => panic!("IPv6 loopback listener: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let stream = connect("[::1]", address.port()).await.unwrap();
        assert_eq!(stream.peer_addr().unwrap(), address);
    }
}
