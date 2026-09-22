//! QUIC stream lifetime guards and a bounded frame-boundary observer.
//! `h3` remains the protocol/QPACK parser. RFC 9114 §§4.1.1, 10.5.1:
//! cancel BOTH directions, and cap encoded frames before decoder buffering.

use bytes::Bytes;
use h3::error::Code;
use h3::quic::{self, BidiStream, Connection, OpenStreams, RecvStream, SendStream};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

pub(super) const MAX_INTERIM: usize = 128;
const MAX_FRAME: u64 = 512 * 1024;

#[derive(Default)]
pub(super) struct ResponseClock(Mutex<HashMap<u64, VecDeque<f64>>>);
impl ResponseClock {
    pub(super) fn take(&self, id: u64) -> f64 {
        self.0
            .lock()
            .unwrap()
            .get_mut(&id)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(crate::performance::now_ms)
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Uni,
    Type,
    Length,
    Payload,
    Ignore,
}

struct Observer {
    stage: Stage,
    integer: u64,
    needed: u8,
    frame: u64,
    remaining: u64,
    started: f64,
    headers: usize,
}
impl Observer {
    fn new(uni: bool) -> Self {
        Self {
            stage: if uni { Stage::Uni } else { Stage::Type },
            integer: 0,
            needed: 0,
            frame: 0,
            remaining: 0,
            started: 0.0,
            headers: 0,
        }
    }
    fn observe(&mut self, mut bytes: &[u8], clock: &ResponseClock, id: u64) -> Result<(), ()> {
        while !bytes.is_empty() {
            match self.stage {
                Stage::Ignore => return Ok(()),
                Stage::Payload => {
                    let count = self.remaining.min(bytes.len() as u64) as usize;
                    self.remaining -= count as u64;
                    bytes = &bytes[count..];
                    if self.remaining == 0 {
                        self.stage = Stage::Type;
                    }
                }
                Stage::Uni | Stage::Type | Stage::Length => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    if self.needed == 0 {
                        if matches!(self.stage, Stage::Type) {
                            self.started = crate::performance::now_ms();
                        }
                        self.needed = 1 << (byte >> 6);
                        self.integer = u64::from(byte & 0x3f);
                    } else {
                        self.integer = (self.integer << 8) | u64::from(byte);
                    }
                    self.needed -= 1;
                    if self.needed != 0 {
                        continue;
                    }
                    match self.stage {
                        Stage::Uni => {
                            // Only control streams use HTTP framing. QPACK
                            // and unknown stream types are handled by h3.
                            self.stage = if self.integer == 0 {
                                Stage::Type
                            } else {
                                Stage::Ignore
                            };
                        }
                        Stage::Type => {
                            self.frame = self.integer;
                            self.stage = Stage::Length;
                        }
                        Stage::Length => {
                            // DATA is streamed, not accumulated by h3. Other
                            // frames must fit the encoded metadata budget.
                            if self.frame != 0 && self.integer > MAX_FRAME {
                                return Err(());
                            }
                            if self.frame == 1 {
                                self.headers += 1;
                                if self.headers > MAX_INTERIM + 2 {
                                    return Err(());
                                }
                                if let Some(times) = clock.0.lock().unwrap().get_mut(&id) {
                                    times.push_back(self.started);
                                }
                            }
                            self.remaining = self.integer;
                            self.stage = if self.remaining == 0 {
                                Stage::Type
                            } else {
                                Stage::Payload
                            };
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
        Ok(())
    }
}

pub(super) struct Send<S: SendStream<Bytes>> {
    inner: S,
    ended: bool,
}
impl<S: SendStream<Bytes>> Send<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            ended: false,
        }
    }
}
impl<S: SendStream<Bytes>> Drop for Send<S> {
    fn drop(&mut self) {
        if !self.ended {
            self.inner.reset(Code::H3_REQUEST_CANCELLED.value());
        }
    }
}
impl<S: SendStream<Bytes>> SendStream<Bytes> for Send<S> {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), quic::StreamErrorIncoming>> {
        self.inner.poll_ready(cx)
    }
    fn send_data<T: Into<quic::WriteBuf<Bytes>>>(
        &mut self,
        data: T,
    ) -> Result<(), quic::StreamErrorIncoming> {
        self.inner.send_data(data)
    }
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), quic::StreamErrorIncoming>> {
        let result = self.inner.poll_finish(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.ended = true;
        }
        result
    }
    fn reset(&mut self, code: u64) {
        if !self.ended {
            self.inner.reset(code);
            self.ended = true;
        }
    }
    fn send_id(&self) -> quic::StreamId {
        self.inner.send_id()
    }
}

pub(super) struct Recv {
    inner: h3_quinn::RecvStream,
    ended: bool,
    observer: Observer,
    clock: Arc<ResponseClock>,
}
impl Recv {
    fn new(inner: h3_quinn::RecvStream, uni: bool, clock: Arc<ResponseClock>) -> Self {
        if !uni {
            clock
                .0
                .lock()
                .unwrap()
                .insert(inner.recv_id().into_inner(), VecDeque::new());
        }
        Self {
            inner,
            ended: false,
            observer: Observer::new(uni),
            clock,
        }
    }
}
impl Drop for Recv {
    fn drop(&mut self) {
        self.clock
            .0
            .lock()
            .unwrap()
            .remove(&self.inner.recv_id().into_inner());
        if !self.ended {
            self.inner.stop_sending(Code::H3_REQUEST_CANCELLED.value());
        }
    }
}
impl RecvStream for Recv {
    type Buf = Bytes;
    fn poll_data(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Bytes>, quic::StreamErrorIncoming>> {
        let result = self.inner.poll_data(cx);
        match &result {
            Poll::Ready(Ok(Some(data))) => {
                if self
                    .observer
                    .observe(data, &self.clock, self.inner.recv_id().into_inner())
                    .is_err()
                {
                    self.stop_sending(Code::H3_EXCESSIVE_LOAD.value());
                    return Poll::Ready(Err(quic::StreamErrorIncoming::Unknown(Box::new(
                        std::io::Error::other("HTTP/3 encoded metadata exceeds size limit"),
                    ))));
                }
            }
            Poll::Ready(Ok(None)) => self.ended = true,
            _ => {}
        }
        result
    }
    fn stop_sending(&mut self, code: u64) {
        self.ended = true;
        self.inner.stop_sending(code);
    }
    fn recv_id(&self) -> quic::StreamId {
        self.inner.recv_id()
    }
}

pub(super) type SendHalf = Send<h3_quinn::SendStream<Bytes>>;
pub(super) struct Bidi {
    send: SendHalf,
    recv: Recv,
}
impl Bidi {
    fn new(inner: h3_quinn::BidiStream<Bytes>, clock: Arc<ResponseClock>) -> Self {
        let (send, recv) = inner.split();
        Self {
            send: Send::new(send),
            recv: Recv::new(recv, false, clock),
        }
    }
}
impl SendStream<Bytes> for Bidi {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), quic::StreamErrorIncoming>> {
        self.send.poll_ready(cx)
    }
    fn send_data<T: Into<quic::WriteBuf<Bytes>>>(
        &mut self,
        data: T,
    ) -> Result<(), quic::StreamErrorIncoming> {
        self.send.send_data(data)
    }
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), quic::StreamErrorIncoming>> {
        self.send.poll_finish(cx)
    }
    fn reset(&mut self, code: u64) {
        self.send.reset(code);
    }
    fn send_id(&self) -> quic::StreamId {
        self.send.send_id()
    }
}
impl RecvStream for Bidi {
    type Buf = Bytes;
    fn poll_data(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Bytes>, quic::StreamErrorIncoming>> {
        self.recv.poll_data(cx)
    }
    fn stop_sending(&mut self, code: u64) {
        self.recv.stop_sending(code);
    }
    fn recv_id(&self) -> quic::StreamId {
        self.recv.recv_id()
    }
}
impl BidiStream<Bytes> for Bidi {
    type SendStream = SendHalf;
    type RecvStream = Recv;
    fn split(self) -> (SendHalf, Recv) {
        (self.send, self.recv)
    }
}

#[derive(Clone)]
pub(super) struct Open<T> {
    inner: T,
    clock: Arc<ResponseClock>,
}
impl<T> Open<T> {
    pub(super) fn new(inner: T, clock: Arc<ResponseClock>) -> Self {
        Self { inner, clock }
    }
}
impl<T> OpenStreams<Bytes> for Open<T>
where
    T: OpenStreams<
            Bytes,
            BidiStream = h3_quinn::BidiStream<Bytes>,
            SendStream = h3_quinn::SendStream<Bytes>,
        >,
{
    type BidiStream = Bidi;
    type SendStream = SendHalf;
    fn poll_open_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Bidi, quic::StreamErrorIncoming>> {
        self.inner
            .poll_open_bidi(cx)
            .map_ok(|inner| Bidi::new(inner, self.clock.clone()))
    }
    fn poll_open_send(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<SendHalf, quic::StreamErrorIncoming>> {
        self.inner.poll_open_send(cx).map_ok(Send::new)
    }
    fn close(&mut self, code: Code, reason: &[u8]) {
        self.inner.close(code, reason);
    }
}
impl Connection<Bytes> for Open<h3_quinn::Connection> {
    type RecvStream = Recv;
    type OpenStreams = Open<h3_quinn::OpenStreams>;
    fn poll_accept_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Recv, quic::ConnectionErrorIncoming>> {
        <h3_quinn::Connection as Connection<Bytes>>::poll_accept_recv(&mut self.inner, cx)
            .map_ok(|inner| Recv::new(inner, true, self.clock.clone()))
    }
    fn poll_accept_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Bidi, quic::ConnectionErrorIncoming>> {
        self.inner
            .poll_accept_bidi(cx)
            .map_ok(|inner| Bidi::new(inner, self.clock.clone()))
    }
    fn opener(&self) -> Self::OpenStreams {
        Open::new(
            <h3_quinn::Connection as Connection<Bytes>>::opener(&self.inner),
            self.clock.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn http3_frame_observer_handles_fragmented_varints_and_bounds_before_payload() {
        let clock = ResponseClock::default();
        clock.0.lock().unwrap().insert(0, VecDeque::new());
        let mut observer = Observer::new(false);
        for b in [0x40, 1, 0x40, 2, 0, 0, 0, 3, 5, 6, 7, 1, 0] {
            observer.observe(&[b], &clock, 0).unwrap();
        }
        assert_eq!(clock.0.lock().unwrap()[&0].len(), 2);
        // HEADERS with a 512 KiB+1 length fails without receiving its payload.
        assert!(observer.observe(&[1, 0x80, 8, 0, 1], &clock, 0).is_err());
        let mut data = Observer::new(false);
        assert!(data.observe(&[0, 0x80, 8, 0, 1], &clock, 0).is_ok());
        let mut uni = Observer::new(true);
        assert!(uni.observe(&[0, 4, 0x80, 8, 0, 1], &clock, 0).is_err());
        let mut qpack = Observer::new(true);
        assert!(qpack.observe(&[2, 4, 0x80, 8, 0, 1], &clock, 0).is_ok());
    }
}
