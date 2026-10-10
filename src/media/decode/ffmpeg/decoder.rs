//! The decoding machinery shared by the video and audio decoders: one
//! `AVCodecContext` driven through the send/receive API, and a queue that
//! lets callers ignore `EAGAIN`.

use std::collections::VecDeque;
use std::ffi::{CStr, c_int};
use std::ptr::{self, null, null_mut};
use std::sync::Arc;

use super::abi::{
    AV_INPUT_BUFFER_PADDING_SIZE, AV_PKT_FLAG_KEY, AVCodec, AVCodecContext, AVERROR_EAGAIN,
    AVERROR_EOF, AVFrame, AVMEDIA_TYPE_AUDIO, AVMEDIA_TYPE_VIDEO, AVPacket, FrameFields,
    PacketHead, ParametersHead,
};
use super::codec::CodecId;
use super::error::Error;
use super::library::Library;

/// `AVMediaType` of the codec context being built.
#[derive(Clone, Copy)]
#[repr(i32)]
pub(crate) enum MediaType {
    Video = AVMEDIA_TYPE_VIDEO,
    Audio = AVMEDIA_TYPE_AUDIO,
}

/// Packets and extradata are `int`-sized in FFmpeg; stay well inside that.
const MAX_PACKET_SIZE: usize = i32::MAX as usize - AV_INPUT_BUFFER_PADDING_SIZE;
const MAX_EXTRADATA_SIZE: usize = 16 << 20;

/// What `avcodec_receive_frame` had to say.
pub(crate) enum Pulled<T> {
    Item(T),
    /// More input is needed.
    Again,
    /// The decoder is drained.
    End,
}

/// An opened (or being opened) decoder context with its scratch packet and
/// frame.
pub(crate) struct Core {
    lib: Arc<Library>,
    ctx: *mut AVCodecContext,
    codec: *const AVCodec,
    packet: *mut AVPacket,
    frame: *mut AVFrame,
}

// SAFETY: an AVCodecContext may be used from any thread as long as only one
// thread uses it at a time, which `&mut self` on every operation guarantees.
// (It is deliberately not `Sync`: raw pointers are not.)
unsafe impl Send for Core {}

impl Core {
    /// Allocates a context for `codec`'s decoder, with `extradata` copied in
    /// when given. Options are set with [`set_int`](Self::set_int) and
    /// [`set_str`](Self::set_str) before [`open`](Self::open).
    pub(crate) fn new(
        lib: &Arc<Library>,
        codec: CodecId,
        media: MediaType,
        extradata: Option<&[u8]>,
    ) -> Result<Core, Error> {
        let decoder = lib.find_decoder(codec);
        if decoder.is_null() {
            return Err(Error::NoDecoder(codec));
        }
        let extradata = extradata.filter(|data| !data.is_empty());
        if extradata.is_some_and(|data| data.len() > MAX_EXTRADATA_SIZE) {
            return Err(Error::InvalidInput("extradata is too large"));
        }
        // Each allocation is stored as soon as it exists, so that `Drop`
        // releases what was allocated when a later step fails.
        let mut core = Core {
            lib: Arc::clone(lib),
            ctx: null_mut(),
            codec: decoder,
            packet: null_mut(),
            frame: null_mut(),
        };
        // SAFETY: allocation functions with valid arguments.
        unsafe {
            core.ctx = (lib.api.avcodec_alloc_context3)(decoder);
            core.packet = (lib.api.av_packet_alloc)();
            core.frame = (lib.api.av_frame_alloc)();
        }
        if core.ctx.is_null() || core.packet.is_null() || core.frame.is_null() {
            return Err(Error::OutOfMemory);
        }
        if let Some(data) = extradata {
            core.apply_extradata(codec, media, data)?;
        }
        Ok(core)
    }

    /// Hands `extradata` to the context through `AVCodecParameters`, writing
    /// only the members every release lays out the same way.
    fn apply_extradata(
        &mut self,
        codec: CodecId,
        media: MediaType,
        data: &[u8],
    ) -> Result<(), Error> {
        let api = &self.lib.api;
        // SAFETY: `parameters` and `buffer` are fresh FFmpeg allocations;
        // `ParametersHead` describes the start of `AVCodecParameters` in
        // every supported release, and the parameters own the buffer once it
        // is stored in them (`avcodec_parameters_free` releases it).
        let ret = unsafe {
            let mut parameters = (api.avcodec_parameters_alloc)();
            if parameters.is_null() {
                return Err(Error::OutOfMemory);
            }
            let buffer = (api.av_mallocz)(data.len() + AV_INPUT_BUFFER_PADDING_SIZE).cast::<u8>();
            if buffer.is_null() {
                (api.avcodec_parameters_free)(&mut parameters);
                return Err(Error::OutOfMemory);
            }
            ptr::copy_nonoverlapping(data.as_ptr(), buffer, data.len());
            let head = parameters.cast::<ParametersHead>();
            (*head).codec_type = media as c_int;
            (*head).codec_id = self.lib.codec_id(codec);
            (*head).codec_tag = 0;
            (*head).extradata = buffer;
            (*head).extradata_size = data.len() as c_int;
            let ret = (api.avcodec_parameters_to_context)(self.ctx, parameters);
            (api.avcodec_parameters_free)(&mut parameters);
            ret
        };
        self.check("avcodec_parameters_to_context", ret)
    }

    pub(crate) fn set_int(&mut self, name: &CStr, value: i64) -> Result<(), Error> {
        // SAFETY: AVOptions on an AVCodecContext, NUL-terminated name.
        let ret =
            unsafe { (self.lib.api.av_opt_set_int)(self.ctx.cast(), name.as_ptr(), value, 0) };
        self.check("av_opt_set_int", ret)
    }

    pub(crate) fn set_str(&mut self, name: &CStr, value: &CStr) -> Result<(), Error> {
        // SAFETY: as above.
        let ret =
            unsafe { (self.lib.api.av_opt_set)(self.ctx.cast(), name.as_ptr(), value.as_ptr(), 0) };
        self.check("av_opt_set", ret)
    }

    pub(crate) fn open(&mut self) -> Result<(), Error> {
        // SAFETY: the context was allocated for this codec.
        let ret = unsafe { (self.lib.api.avcodec_open2)(self.ctx, self.codec, null_mut()) };
        self.check("avcodec_open2", ret)
    }

    fn check(&self, call: &'static str, ret: c_int) -> Result<(), Error> {
        if ret < 0 {
            Err(self.lib.ffmpeg_error(call, ret))
        } else {
            Ok(())
        }
    }

    /// Rejects what cannot be a packet.
    fn check_packet(data: &[u8]) -> Result<(), Error> {
        if data.is_empty() {
            Err(Error::InvalidInput("empty packet"))
        } else if data.len() > MAX_PACKET_SIZE {
            Err(Error::InvalidInput("packet is too large"))
        } else {
            Ok(())
        }
    }

    /// Submits one packet (copied). `Ok(false)` means the decoder is full
    /// and its output must be received first.
    pub(crate) fn submit(&mut self, data: &[u8], pts: i64, keyframe: bool) -> Result<bool, Error> {
        Self::check_packet(data)?;
        let api = &self.lib.api;
        // SAFETY: `av_new_packet` allocates `data.len()` bytes (plus zeroed
        // padding) at `PacketHead::data`, which this writes only within
        // bounds; the decoder takes its own reference to the packet, so ours
        // is released right after.
        let ret = unsafe {
            let ret = (api.av_new_packet)(self.packet, data.len() as c_int);
            if ret < 0 {
                return Err(self.lib.ffmpeg_error("av_new_packet", ret));
            }
            let head = self.packet.cast::<PacketHead>();
            ptr::copy_nonoverlapping(data.as_ptr(), (*head).data, data.len());
            (*head).pts = pts;
            (*head).flags = if keyframe { AV_PKT_FLAG_KEY } else { 0 };
            let ret = (api.avcodec_send_packet)(self.ctx, self.packet);
            (api.av_packet_unref)(self.packet);
            ret
        };
        match ret {
            0 => Ok(true),
            AVERROR_EAGAIN => Ok(false),
            _ => Err(self.lib.ffmpeg_error("avcodec_send_packet", ret)),
        }
    }

    /// Signals the end of the stream (the NULL packet); the decoder then
    /// returns its buffered frames and, finally, [`Pulled::End`].
    fn send_end_of_stream(&mut self) -> Result<(), Error> {
        // SAFETY: a NULL packet is the documented drain request.
        let ret = unsafe { (self.lib.api.avcodec_send_packet)(self.ctx, null()) };
        self.check("avcodec_send_packet", ret)
    }

    /// Takes the next decoded frame, converts it with `convert` and releases
    /// the frame.
    fn pull<T>(
        &mut self,
        convert: fn(&Library, &FrameFields) -> Result<T, Error>,
    ) -> Result<Pulled<T>, Error> {
        let api = &self.lib.api;
        // SAFETY: `frame` is a valid, unreferenced AVFrame; when
        // `avcodec_receive_frame` fills it, `snapshot` reads the layout that
        // matches this library (checked when it was loaded), and the frame
        // is unreferenced again whatever `convert` makes of it.
        unsafe {
            let ret = (api.avcodec_receive_frame)(self.ctx, self.frame);
            match ret {
                0 => {
                    let fields = self.lib.layout.snapshot(self.frame);
                    let item = convert(&self.lib, &fields);
                    (api.av_frame_unref)(self.frame);
                    item.map(Pulled::Item)
                }
                AVERROR_EAGAIN => Ok(Pulled::Again),
                AVERROR_EOF => Ok(Pulled::End),
                _ => Err(self.lib.ffmpeg_error("avcodec_receive_frame", ret)),
            }
        }
    }

    fn flush(&mut self) {
        // SAFETY: valid for any opened context; also leaves draining mode.
        unsafe { (self.lib.api.avcodec_flush_buffers)(self.ctx) };
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let api = &self.lib.api;
        // SAFETY: each pointer is null or owned by this `Core`; the free
        // functions null them and tolerate null.
        unsafe {
            (api.av_frame_free)(&mut self.frame);
            (api.av_packet_free)(&mut self.packet);
            (api.avcodec_free_context)(&mut self.ctx);
        }
    }
}

/// FFmpeg buffers far fewer frames than this; the bound only keeps a
/// misbehaving library from looping `drain` forever.
const MAX_DRAIN_STEPS: usize = 1024;

/// A [`Core`] plus the outputs it has produced but the caller has not taken:
/// frames converted to `T`, and the errors that took the place of frames.
///
/// `avcodec_send_packet` answers `EAGAIN` when decoded frames must be
/// received first. The pipeline receives them into its queue and resends, so
/// callers see a plain `send` / `receive` pair. The queue is bounded, so a
/// caller that never receives gets [`Error::QueueFull`] rather than
/// unbounded memory growth.
pub(crate) struct Pipeline<T> {
    core: Core,
    queue: VecDeque<Result<T, Error>>,
    max_queued: usize,
    convert: fn(&Library, &FrameFields) -> Result<T, Error>,
}

impl<T> Pipeline<T> {
    pub(crate) fn new(
        core: Core,
        max_queued: usize,
        convert: fn(&Library, &FrameFields) -> Result<T, Error>,
    ) -> Self {
        Pipeline {
            core,
            queue: VecDeque::new(),
            max_queued,
            convert,
        }
    }

    /// Sends one packet. An `Err` always concerns this packet, which was not
    /// consumed: FFmpeg rejected it (the decoder stays usable) or the queue
    /// is full. A failure to decode an earlier packet that surfaces while
    /// making room is queued for `receive`, in order.
    pub(crate) fn send(&mut self, data: &[u8], pts: i64, keyframe: bool) -> Result<(), Error> {
        loop {
            if self.core.submit(data, pts, keyframe)? {
                return Ok(());
            }
            if self.queue.len() >= self.max_queued {
                return Err(Error::QueueFull);
            }
            match self.core.pull(self.convert) {
                Ok(Pulled::Item(item)) => self.queue.push_back(Ok(item)),
                Ok(Pulled::Again | Pulled::End) => {
                    // A decoder that refuses input must have output to give.
                    return Err(self
                        .core
                        .lib
                        .ffmpeg_error("avcodec_send_packet", AVERROR_EAGAIN));
                }
                Err(error) => self.queue.push_back(Err(error)),
            }
        }
    }

    /// The next decoded frame, if one is ready; an `Err` stands in for a
    /// frame that could not be decoded or converted, and later calls carry
    /// on.
    pub(crate) fn receive(&mut self) -> Result<Option<T>, Error> {
        if let Some(queued) = self.queue.pop_front() {
            return queued.map(Some);
        }
        match self.core.pull(self.convert)? {
            Pulled::Item(item) => Ok(Some(item)),
            Pulled::Again | Pulled::End => Ok(None),
        }
    }

    /// Ends the stream: everything still buffered, in order. The decoder is
    /// then flushed and ready for a new sequence starting at a keyframe.
    pub(crate) fn drain(&mut self) -> Vec<Result<T, Error>> {
        let mut outputs: Vec<Result<T, Error>> = self.queue.drain(..).collect();
        match self.core.send_end_of_stream() {
            Err(error) => outputs.push(Err(error)),
            Ok(()) => {
                for _ in 0..MAX_DRAIN_STEPS {
                    match self.core.pull(self.convert) {
                        Ok(Pulled::Item(item)) => outputs.push(Ok(item)),
                        Ok(Pulled::Again | Pulled::End) => break,
                        Err(error) => outputs.push(Err(error)),
                    }
                }
            }
        }
        self.core.flush();
        outputs
    }

    /// Drops buffered frames and decoder state (use after seeking).
    pub(crate) fn flush(&mut self) {
        self.queue.clear();
        self.core.flush();
    }
}
