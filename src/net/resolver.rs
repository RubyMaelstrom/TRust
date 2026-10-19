//! Keep name-service policy in the operating system while exposing A and AAAA
//! completion independently (RFC 8305 §3, RFC Editor snapshot 2026-09-06).
//!
//! A canceled getaddrinfo cannot in general be interrupted. Its semaphore
//! permit therefore belongs to the blocking job, not its awaiting future:
//! navigation cancellation never leaves an unbounded population of resolver
//! threads behind. Waiting for admission is asynchronous and cancelable.

use super::{Family, Query};
use std::{io, net::SocketAddr, sync::Arc};
use tokio::sync::Semaphore;

const MAX_RESOLVER_JOBS: usize = 32;
const MAX_ADDRESSES: usize = 256;

fn slots() -> &'static Arc<Semaphore> {
    static SLOTS: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_RESOLVER_JOBS)))
}

pub(super) async fn lookup(host: &str, port: u16, query: Query) -> io::Result<Vec<SocketAddr>> {
    blocking(slots().clone(), {
        let host = host.to_owned();
        move || {
            let family = match query {
                Query::Addresses(family) => family,
                // This must remain an AAAA lookup through the system's DNS64
                // resolver, not an A lookup of the well-known discovery name.
                Query::Pref64 => Family::V6,
            };
            system_lookup(&host, port, family)
        }
    })
    .await
}

async fn blocking<T: Send + 'static>(
    slots: Arc<Semaphore>,
    operation: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let permit = slots
        .acquire_owned()
        .await
        .map_err(|_| io::Error::other("System resolver is unavailable"))?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // Native name-service modules are not necessarily cancelable. Detached
    // admission-bounded threads also keep one slow module from blocking
    // Tokio runtime shutdown after a successful connection or navigation.
    std::thread::Builder::new()
        .name("trust-dns".into())
        .spawn(move || {
            let _permit = permit;
            let _ = sender.send(operation());
        })?;
    receiver
        .await
        .map_err(|_| io::Error::other("System resolver thread ended without a result"))?
}

#[cfg(unix)]
fn system_lookup(host: &str, port: u16, family: Family) -> io::Result<Vec<SocketAddr>> {
    use std::{
        ffi::CString,
        mem,
        net::{Ipv4Addr, Ipv6Addr, SocketAddrV6},
        ptr,
    };

    let host = CString::new(host)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid hostname"))?;
    // SOCK_STREAM avoids duplicate UDP/raw results. Do not request V4MAPPED:
    // DNS64 AAAA answers must be distinguishable from native A records.
    let mut hints: libc::addrinfo = unsafe { mem::zeroed() };
    hints.ai_family = if family == Family::V6 {
        libc::AF_INET6
    } else {
        libc::AF_INET
    };
    hints.ai_socktype = libc::SOCK_STREAM;
    let mut head = ptr::null_mut();
    // SAFETY: strings and hints outlive this synchronous operation; libc
    // initializes head on success and retains ownership until freeaddrinfo.
    let result = unsafe { libc::getaddrinfo(host.as_ptr(), ptr::null(), &hints, &mut head) };
    if result != 0 {
        return Err(if result == libc::EAI_NONAME {
            io::Error::new(io::ErrorKind::NotFound, "No addresses found")
        } else if result == libc::EAI_SYSTEM {
            io::Error::last_os_error()
        } else {
            // gai_strerror points to immutable system-owned text.
            let text = unsafe { std::ffi::CStr::from_ptr(libc::gai_strerror(result)) };
            io::Error::other(text.to_string_lossy().into_owned())
        });
    }
    struct Results(*mut libc::addrinfo);
    impl Drop for Results {
        fn drop(&mut self) {
            // SAFETY: this is the unchanged successful getaddrinfo allocation.
            unsafe { libc::freeaddrinfo(self.0) };
        }
    }
    let _results = Results(head);
    let mut addresses = Vec::new();
    while !head.is_null() && addresses.len() < MAX_ADDRESSES {
        // SAFETY: getaddrinfo's live linked list has correctly-sized sockaddr
        // objects; verify family and length before each concrete cast.
        let info = unsafe { &*head };
        if !info.ai_addr.is_null() {
            if info.ai_family == libc::AF_INET
                && info.ai_addrlen as usize >= mem::size_of::<libc::sockaddr_in>()
            {
                let addr = unsafe { ptr::read_unaligned(info.ai_addr.cast::<libc::sockaddr_in>()) };
                addresses.push(SocketAddr::from((
                    Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()),
                    port,
                )));
            } else if info.ai_family == libc::AF_INET6
                && info.ai_addrlen as usize >= mem::size_of::<libc::sockaddr_in6>()
            {
                let addr =
                    unsafe { ptr::read_unaligned(info.ai_addr.cast::<libc::sockaddr_in6>()) };
                addresses.push(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::from(addr.sin6_addr.s6_addr),
                    port,
                    u32::from_be(addr.sin6_flowinfo),
                    addr.sin6_scope_id,
                )));
            }
        }
        head = info.ai_next;
    }
    Ok(addresses)
}

#[cfg(windows)]
fn system_lookup(host: &str, port: u16, family: Family) -> io::Result<Vec<SocketAddr>> {
    use std::{
        mem,
        net::{Ipv4Addr, Ipv6Addr, SocketAddrV6},
        ptr,
    };
    use windows_sys::Win32::Networking::WinSock::*;

    // Initialize Winsock through std before calling its resolver directly.
    let _startup =
        std::net::UdpSocket::bind("[::]:0").or_else(|_| std::net::UdpSocket::bind("0.0.0.0:0"));
    if host.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid hostname",
        ));
    }
    let host: Vec<u16> = host.encode_utf16().chain(Some(0)).collect();
    let mut hints: ADDRINFOW = unsafe { mem::zeroed() };
    hints.ai_family = if family == Family::V6 {
        AF_INET6 as i32
    } else {
        AF_INET as i32
    };
    hints.ai_socktype = SOCK_STREAM;
    let mut head = ptr::null_mut();
    // SAFETY: buffers and hints remain valid until this blocking call returns.
    let result = unsafe { GetAddrInfoW(host.as_ptr(), ptr::null(), &hints, &mut head) };
    if result != 0 {
        return Err(if result == WSAHOST_NOT_FOUND || result == WSANO_DATA {
            io::Error::new(io::ErrorKind::NotFound, "No addresses found")
        } else {
            io::Error::from_raw_os_error(result)
        });
    }
    struct Results(*mut ADDRINFOW);
    impl Drop for Results {
        fn drop(&mut self) {
            unsafe { FreeAddrInfoW(self.0) };
        }
    }
    let _results = Results(head);
    let mut addresses = Vec::new();
    while !head.is_null() && addresses.len() < MAX_ADDRESSES {
        let info = unsafe { &*head };
        if !info.ai_addr.is_null() {
            if info.ai_family == AF_INET as i32 && info.ai_addrlen >= mem::size_of::<SOCKADDR_IN>()
            {
                let addr = unsafe { ptr::read_unaligned(info.ai_addr.cast::<SOCKADDR_IN>()) };
                let bytes = unsafe { addr.sin_addr.S_un.S_addr }.to_ne_bytes();
                addresses.push(SocketAddr::from((Ipv4Addr::from(bytes), port)));
            } else if info.ai_family == AF_INET6 as i32
                && info.ai_addrlen >= mem::size_of::<SOCKADDR_IN6>()
            {
                let addr = unsafe { ptr::read_unaligned(info.ai_addr.cast::<SOCKADDR_IN6>()) };
                addresses.push(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::from(unsafe { addr.sin6_addr.u.Byte }),
                    port,
                    u32::from_be(addr.sin6_flowinfo),
                    unsafe { addr.Anonymous.sin6_scope_id },
                )));
            }
        }
        head = info.ai_next;
    }
    Ok(addresses)
}

#[cfg(not(any(unix, windows)))]
fn system_lookup(host: &str, port: u16, family: Family) -> io::Result<Vec<SocketAddr>> {
    use std::net::ToSocketAddrs;
    // Platforms without native family hints retain bounded name-service work.
    Ok((host, port)
        .to_socket_addrs()?
        .filter(|a| Family::of(*a) == family)
        .take(MAX_ADDRESSES)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn canceled_system_jobs_keep_admission_until_the_native_call_finishes() {
        let slots = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(blocking(slots.clone(), move || {
            let _ = started_tx.send(());
            finish_rx.recv().unwrap();
            Ok(())
        }));
        started_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(slots.available_permits(), 0);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let queued = tokio::spawn(blocking(slots.clone(), {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }));
        tokio::task::yield_now().await;
        queued.abort();
        let _ = queued.await;
        finish_tx.send(()).unwrap();
        let _permit = tokio::time::timeout(std::time::Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn system_family_queries_preserve_numeric_addresses_and_ports() {
        let v4 = lookup("127.0.0.1", 17391, Query::Addresses(Family::V4))
            .await
            .unwrap();
        assert_eq!(v4, ["127.0.0.1:17391".parse().unwrap()]);
        let v6 = lookup("::1", 17391, Query::Addresses(Family::V6))
            .await
            .unwrap();
        assert_eq!(v6, ["[::1]:17391".parse().unwrap()]);
        assert!(
            lookup("broken\0name", 80, Query::Addresses(Family::V4))
                .await
                .is_err()
        );
    }

    #[test]
    fn canceled_native_lookup_does_not_delay_runtime_shutdown() {
        let (native_started, started) = tokio::sync::oneshot::channel();
        let (finish, native_finish) = std::sync::mpsc::channel();
        let (runtime_finished, finished) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let task = tokio::spawn(blocking(Arc::new(Semaphore::new(1)), move || {
                    let _ = native_started.send(());
                    let _ = native_finish.recv();
                    Ok(())
                }));
                started.await.unwrap();
                task.abort();
                let _ = task.await;
            });
            drop(runtime);
            runtime_finished.send(()).unwrap();
        });
        let result = finished.recv_timeout(std::time::Duration::from_secs(2));
        let _ = finish.send(());
        thread.join().unwrap();
        result.expect("a canceled resolver must not keep the runtime alive");
    }
}
