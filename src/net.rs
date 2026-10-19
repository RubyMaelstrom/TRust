//! Shared TCP connection scheduling. RFC 8305 §§4–5 (RFC Editor snapshot
//! 2026-09-06): interleave address families, stagger attempts, and cancel the
//! losers once a connection succeeds. The system resolver supplies RFC 6724
//! destination ordering, including the host's configured family preference.
//!
//! DNS still uses the system's combined getaddrinfo operation, rather than
//! separate, incremental A/AAAA queries (§3). A deadline bounds resolution
//! and the entire race; a silent route never delays the next address attempt.

use futures::{StreamExt, stream::FuturesUnordered};
use std::{collections::HashSet, future::Future, io, net::SocketAddr, time::Duration};
use tokio::{
    net::TcpStream,
    time::{Instant, sleep_until, timeout_at},
};

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);
const FAILURE_DELAY: Duration = Duration::from_millis(100);

pub(crate) async fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let addresses = resolve(host, port, deadline).await?;
    connect_addresses(addresses, deadline).await
}

/// Keep DNS and TCP boundaries available to Fetch connection timing.
pub(crate) async fn resolve(
    host: &str,
    port: u16,
    deadline: Instant,
) -> io::Result<Vec<SocketAddr>> {
    timeout_at(deadline, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "DNS lookup timed out"))?
        .map(|addresses| addresses.collect())
}

pub(crate) async fn connect_addresses(
    addresses: Vec<SocketAddr>,
    deadline: Instant,
) -> io::Result<TcpStream> {
    race(addresses, deadline, TcpStream::connect).await
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
}
