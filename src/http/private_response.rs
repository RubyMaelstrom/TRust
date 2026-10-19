//! Native-only backing for filtered Fetch responses. In particular opaque
//! bytes and CORS-hidden headers never enter a JavaScript value: author
//! prototype hooks cannot turn a CacheStorage operation into a read of them.
//!
//! Fetch 394d20d #concept-filtered-response / #concept-response-clone;
//! Service Workers 92aba3be #cache-put / #fetch-event-respondwith. All data is
//! RAM-only. A shared replay body lets cloned opaque responses retain bytes
//! while native consumers read arriving pieces, without buffering a whole
//! response before delivering its head or its first body piece.
use super::{BodyStream, ExternalBody};
use futures::future::BoxFuture;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const PROFILE_BYTES: usize = 128 * 1024 * 1024;
const PREFETCH_BYTES: usize = super::body_stream::BODY_FLOW_UPPER;
const PIECE: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct Memory(AtomicUsize);
impl Memory {
    pub(crate) fn reserve(&self, bytes: usize) -> Result<(), String> {
        self.0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= PROFILE_BYTES)
            })
            .map(|_| ())
            .map_err(|_| "Private response memory quota exceeded".into())
    }
    pub(crate) fn release(&self, bytes: usize) {
        self.0.fetch_sub(bytes, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct Signal {
    dropped: AtomicBool,
    notify: Notify,
}
impl Signal {
    async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.dropped.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}
struct State {
    pieces: Vec<Arc<[u8]>>,
    bytes: usize,
    wanted: usize,
    drain: bool,
    end: Option<Result<(), String>>,
}

pub(crate) struct Replay {
    state: Mutex<State>,
    memory: Arc<Memory>,
    available: Notify,
    demand: Arc<Notify>,
    signal: Arc<Signal>,
}
impl Drop for Replay {
    fn drop(&mut self) {
        self.signal.dropped.store(true, Ordering::Release);
        self.signal.notify.notify_waiters();
        self.demand.notify_waiters();
        self.memory
            .0
            .fetch_sub(self.state.get_mut().unwrap().bytes, Ordering::AcqRel);
    }
}

impl Replay {
    pub(crate) fn new(bytes: Vec<u8>, memory: Arc<Memory>) -> Result<Arc<Self>, String> {
        memory.reserve(bytes.len())?;
        let length = bytes.len();
        let pieces = bytes.chunks(PIECE).map(Arc::from).collect();
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                pieces,
                bytes: length,
                wanted: 0,
                drain: false,
                end: None,
            }),
            memory,
            available: Notify::new(),
            demand: Arc::new(Notify::new()),
            signal: Arc::default(),
        }))
    }
    pub(crate) fn finished(self: &Arc<Self>) {
        self.state.lock().unwrap().end = Some(Ok(()));
        self.available.notify_waiters();
    }
    pub(crate) fn size(&self) -> usize {
        self.state.lock().unwrap().bytes
    }
    pub(crate) fn pending(&self) -> bool {
        self.state.lock().unwrap().end.is_none()
    }
    pub(crate) fn cancel(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.end.is_some() {
            return false;
        }
        state.end = Some(Err("The response was aborted".into()));
        drop(state);
        self.signal.dropped.store(true, Ordering::Release);
        self.signal.notify.notify_waiters();
        self.available.notify_waiters();
        true
    }
    pub(crate) fn cursor(self: &Arc<Self>) -> Cursor {
        Cursor {
            replay: self.clone(),
            next: 0,
        }
    }
    pub(crate) async fn complete(self: &Arc<Self>) -> Result<(), String> {
        self.state.lock().unwrap().drain = true;
        self.demand.notify_one();
        loop {
            let notified = self.available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.state.lock().unwrap().end.clone() {
                return result;
            }
            notified.await;
        }
    }
    /// The producer holds only a weak body reference while waiting, so the
    /// last JS/cache/native owner disappearing cancels even a blocked read.
    pub(crate) fn stream(
        self: &Arc<Self>,
        handle: &tokio::runtime::Handle,
        mut body: BodyStream,
        keepalive: bool,
        finished: impl FnOnce(bool, usize, usize) + Send + 'static,
    ) {
        let weak = Arc::downgrade(self);
        let demand = self.demand.clone();
        let signal = self.signal.clone();
        let keep = keepalive.then(|| self.clone());
        handle.spawn(async move {
            let _keep = keep;
            let result: Result<(), String> = async {
                loop {
                    let notified = demand.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    let Some(replay) = weak.upgrade() else {
                        return Err("Response was released".into());
                    };
                    let ready = {
                        let state = replay.state.lock().unwrap();
                        keepalive
                            || state.drain
                            || state.bytes < PREFETCH_BYTES
                            || state.wanted > state.pieces.len()
                    };
                    drop(replay);
                    if !ready {
                        tokio::select! { biased;
                            () = signal.cancelled() => return Err("Response was released".into()),
                            () = &mut notified => {},
                        }
                        continue;
                    }
                    let piece = tokio::select! { biased;
                        () = signal.cancelled() => return Err("Response was released".into()),
                        piece = body.next() => piece?,
                    };
                    let Some(piece) = piece else {
                        return Ok(());
                    };
                    let Some(replay) = weak.upgrade() else {
                        return Err("Response was released".into());
                    };
                    replay.memory.reserve(piece.len())?;
                    let mut state = replay.state.lock().unwrap();
                    state.bytes += piece.len();
                    state.pieces.extend(piece.chunks(PIECE).map(Arc::from));
                    drop(state);
                    replay.available.notify_waiters();
                }
            }
            .await;
            if let Some(replay) = weak.upgrade() {
                replay.state.lock().unwrap().end = Some(result.clone());
                replay.available.notify_waiters();
            }
            finished(result.is_ok(), body.encoded_size(), body.decoded_size());
        });
    }
}

pub(crate) struct Cursor {
    replay: Arc<Replay>,
    next: usize,
}
impl ExternalBody for Cursor {
    fn next(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            loop {
                let notified = self.replay.available.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let mut state = self.replay.state.lock().unwrap();
                    if let Some(Err(error)) = &state.end {
                        return Err(error.clone());
                    }
                    if let Some(piece) = state.pieces.get(self.next) {
                        let bytes = piece.to_vec();
                        self.next += 1;
                        return Ok(Some(bytes));
                    }
                    if let Some(result) = state.end.clone() {
                        return result.map(|()| None);
                    }
                    state.wanted = state.wanted.max(self.next + 1);
                }
                self.replay.demand.notify_one();
                notified.await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Source {
        receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
        gone: Arc<AtomicBool>,
    }
    impl Drop for Source {
        fn drop(&mut self) {
            self.gone.store(true, Ordering::Release);
        }
    }
    impl ExternalBody for Source {
        fn next(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
            Box::pin(async move { Ok(self.receiver.recv().await) })
        }
    }
    #[tokio::test]
    async fn cloned_native_cursors_stream_without_exposing_bytes_and_drop_cancels() {
        let memory = Arc::new(Memory::default());
        let replay = Replay::new(Vec::new(), memory.clone()).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let gone = Arc::new(AtomicBool::new(false));
        replay.stream(
            &tokio::runtime::Handle::current(),
            BodyStream::external(Box::new(Source {
                receiver: rx,
                gone: gone.clone(),
            })),
            false,
            |_, _, _| {},
        );
        let mut a = replay.cursor();
        let mut b = replay.cursor();
        tx.send(b"first".to_vec()).await.unwrap();
        assert_eq!(a.next().await.unwrap().unwrap(), b"first");
        assert_eq!(b.next().await.unwrap().unwrap(), b"first");
        tx.send(b"second".to_vec()).await.unwrap();
        assert_eq!(a.next().await.unwrap().unwrap(), b"second");
        assert_eq!(b.next().await.unwrap().unwrap(), b"second");
        drop((a, b, replay));
        tokio::time::timeout(std::time::Duration::from_secs(2), tx.closed())
            .await
            .unwrap();
        assert!(gone.load(Ordering::Acquire));
        assert_eq!(memory.0.load(Ordering::Acquire), 0);
    }
    #[tokio::test]
    async fn an_opaque_body_prefetches_only_a_bounded_amount_without_a_consumer() {
        let memory = Arc::new(Memory::default());
        let replay = Replay::new(Vec::new(), memory.clone()).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        replay.stream(
            &tokio::runtime::Handle::current(),
            BodyStream::external(Box::new(Source {
                receiver: rx,
                gone: Arc::default(),
            })),
            false,
            |_, _, _| {},
        );
        for _ in 0..PREFETCH_BYTES / PIECE {
            tx.send(vec![1; PIECE]).await.unwrap();
        }
        tx.send(vec![2; PIECE]).await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                tx.send(vec![3; PIECE])
            )
            .await
            .is_err()
        );
        assert_eq!(replay.size(), PREFETCH_BYTES);
        drop(tx);
        replay.complete().await.unwrap();
        assert_eq!(replay.size(), PREFETCH_BYTES + PIECE);
        drop(replay);
        assert_eq!(memory.0.load(Ordering::Acquire), 0);
    }
}
