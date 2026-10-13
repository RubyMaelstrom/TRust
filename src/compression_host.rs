//! Compression Standard compression contexts (local whatwg/compression@770f314,
//! #supported-formats) behind `CompressionStream` and `DecompressionStream`.
//!
//! The JavaScript TransformStream owns chunking; each stream owns one native
//! context here, addressed by a handle kept in the stream's internal slot. A
//! context is created by the constructor, fed every written chunk, finished
//! by the end of the input and released then, on error, on cancel, or when
//! its stream is garbage collected. Past [`MAX_CONTEXTS`] live contexts the
//! oldest is dropped; its stream then errors instead of producing output.
//!
//! Formats (#supported-formats):
//! - `deflate`: RFC 1950 ZLIB, with its ADLER32 checked; FDICT is an error.
//! - `deflate-raw`: RFC 1951 DEFLATE.
//! - `gzip`: RFC 1952, one member, header flags and FHCRC validated, CRC32
//!   and ISIZE checked.
//! - `brotli`: RFC 7932, decoded strictly (no large-window extension).
//!
//! A decompressor reports an error for input after the end of the compressed
//! data, and at the end of the input when the compressed data is incomplete.

use flate2::{Compress, Compression, Crc, Decompress, FlushCompress, FlushDecompress, Status};
use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use trust_js::embed::{HostRetainedMemoryVisitor, RetainedManagedAllocation};

/// Live compression contexts kept per Agent before the oldest is dropped.
const MAX_CONTEXTS: usize = 256;

/// The result of feeding input to a context: output to enqueue, then whether
/// the operation failed (#decompress-and-enqueue-a-chunk enqueues the output
/// decoded before trailing input, then throws).
pub(crate) struct Output {
    pub bytes: Vec<u8>,
    pub failed: bool,
}

impl Output {
    fn ok(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            failed: false,
        }
    }

    fn failed(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            failed: true,
        }
    }
}

#[derive(Default)]
pub(crate) struct Registry {
    next: u32,
    contexts: HashMap<u32, Context>,
    order: VecDeque<u32>,
}

impl Registry {
    /// A new context for `format`, or `None` for an unsupported format.
    pub(crate) fn create(&mut self, format: &str, decompress: bool) -> Option<u32> {
        let context = if decompress {
            Context::Decompress(Decompressor::new(format)?)
        } else {
            Context::Compress(Compressor::new(format)?)
        };
        while self.contexts.len() >= MAX_CONTEXTS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.contexts.remove(&oldest);
        }
        self.next = self.next.wrapping_add(1).max(1);
        while self.contexts.contains_key(&self.next) {
            self.next = self.next.wrapping_add(1).max(1);
        }
        let handle = self.next;
        self.contexts.insert(handle, context);
        self.order.push_back(handle);
        Some(handle)
    }

    /// Feed `input`; `finish` marks the end of the input. `None` means the
    /// context no longer exists.
    pub(crate) fn write(&mut self, handle: u32, input: &[u8], finish: bool) -> Option<Output> {
        let context = self.contexts.get_mut(&handle)?;
        let output = match context {
            Context::Compress(compressor) => compressor.write(input, finish),
            Context::Decompress(decompressor) => decompressor.write(input, finish),
        };
        if finish || output.failed {
            self.free(handle);
        }
        Some(output)
    }

    pub(crate) fn free(&mut self, handle: u32) {
        if self.contexts.remove(&handle).is_some() {
            self.order.retain(|&live| live != handle);
        }
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.contexts.len()
    }

    pub(crate) fn scan_retained_memory(&self, visitor: &mut dyn HostRetainedMemoryVisitor) {
        if self.contexts.capacity() != 0 {
            visitor.allocation(RetainedManagedAllocation::new(
                "trust.compression-contexts",
                &self.contexts as *const _ as usize,
                self.contexts.capacity() * std::mem::size_of::<(u32, Context)>()
                    + self.order.capacity() * std::mem::size_of::<u32>(),
            ));
        }
        if !self.contexts.is_empty() {
            visitor.opaque_storage();
        }
    }
}

enum Context {
    Compress(Compressor),
    Decompress(Decompressor),
}

// ---------------------------------------------------------------------------
// Compression
// ---------------------------------------------------------------------------

enum Compressor {
    /// `deflate` (zlib header and ADLER32) or `deflate-raw`.
    Deflate(Compress),
    Gzip {
        deflate: Compress,
        crc: Crc,
        header_written: bool,
    },
    Brotli(Option<Box<brotli::CompressorWriter<Vec<u8>>>>),
}

impl Compressor {
    fn new(format: &str) -> Option<Self> {
        Some(match format {
            "deflate" => Self::Deflate(Compress::new(Compression::default(), true)),
            "deflate-raw" => Self::Deflate(Compress::new(Compression::default(), false)),
            "gzip" => Self::Gzip {
                deflate: Compress::new(Compression::default(), false),
                crc: Crc::new(),
                header_written: false,
            },
            // Quality 5 balances speed and size; lgwin 22 is RFC 7932's
            // conventional window, never the large-window extension.
            "brotli" => Self::Brotli(Some(Box::new(brotli::CompressorWriter::new(
                Vec::new(),
                4096,
                5,
                22,
            )))),
            _ => return None,
        })
    }

    fn write(&mut self, input: &[u8], finish: bool) -> Output {
        match self {
            Self::Deflate(deflate) => {
                let mut out = Vec::new();
                match deflate_into(deflate, input, finish, &mut out) {
                    Ok(()) => Output::ok(out),
                    Err(()) => Output::failed(Vec::new()),
                }
            }
            Self::Gzip {
                deflate,
                crc,
                header_written,
            } => {
                let mut out = Vec::new();
                if !*header_written {
                    // RFC 1952 §2.3: ID1 ID2 CM=8, no flags, no MTIME, XFL 0,
                    // OS 255 (unknown).
                    out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255]);
                    *header_written = true;
                }
                crc.update(input);
                if deflate_into(deflate, input, finish, &mut out).is_err() {
                    return Output::failed(Vec::new());
                }
                if finish {
                    out.extend_from_slice(&crc.sum().to_le_bytes());
                    out.extend_from_slice(&crc.amount().to_le_bytes());
                }
                Output::ok(out)
            }
            Self::Brotli(writer) => {
                let Some(active) = writer.as_mut() else {
                    return Output::failed(Vec::new());
                };
                if active.write_all(input).is_err() {
                    return Output::failed(Vec::new());
                }
                if finish {
                    let Some(active) = writer.take() else {
                        return Output::failed(Vec::new());
                    };
                    return Output::ok(active.into_inner());
                }
                Output::ok(std::mem::take(active.get_mut()))
            }
        }
    }
}

/// Compress `input` into `out`; with `finish`, until the stream end.
fn deflate_into(
    deflate: &mut Compress,
    mut input: &[u8],
    finish: bool,
    out: &mut Vec<u8>,
) -> Result<(), ()> {
    let flush = if finish {
        FlushCompress::Finish
    } else {
        FlushCompress::None
    };
    loop {
        if out.capacity() - out.len() < 4096 {
            out.reserve(input.len() / 2 + 8192);
        }
        let consumed_before = deflate.total_in();
        let produced_before = deflate.total_out();
        let status = deflate.compress_vec(input, out, flush).map_err(|_| ())?;
        let consumed = (deflate.total_in() - consumed_before) as usize;
        input = &input[consumed..];
        let produced = deflate.total_out() - produced_before;
        match status {
            Status::StreamEnd => return Ok(()),
            Status::Ok | Status::BufError => {
                if !finish && input.is_empty() {
                    return Ok(());
                }
                if consumed == 0 && produced == 0 && out.len() < out.capacity() {
                    // No progress with room to write: nothing left to do.
                    if !finish {
                        return Ok(());
                    }
                    return Err(());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Decompression
// ---------------------------------------------------------------------------

/// One incremental decompression context. HTTP content-coding decoding of
/// incrementally read response bodies (`http::content_coding`) shares these
/// decoders with `DecompressionStream`.
pub(crate) enum Decompressor {
    /// `deflate` (zlib) or `deflate-raw`.
    Deflate {
        inflate: Decompress,
        ended: bool,
    },
    Gzip(Box<GzipDecoder>),
    Brotli(Box<BrotliDecoder>),
}

impl Decompressor {
    /// Whether the end of the compressed data has been decoded. Output that
    /// [`Self::write`] reports as failed after this is true failed only
    /// because input followed the compressed data.
    pub(crate) fn finished(&self) -> bool {
        match self {
            Self::Deflate { ended, .. } => *ended,
            Self::Gzip(decoder) => matches!(decoder.phase, GzipPhase::Done),
            Self::Brotli(decoder) => decoder.ended,
        }
    }

    pub(crate) fn new(format: &str) -> Option<Self> {
        Some(match format {
            "deflate" => Self::Deflate {
                inflate: Decompress::new(true),
                ended: false,
            },
            "deflate-raw" => Self::Deflate {
                inflate: Decompress::new(false),
                ended: false,
            },
            "gzip" => Self::Gzip(Box::default()),
            "brotli" => Self::Brotli(Box::new(BrotliDecoder::new())),
            _ => return None,
        })
    }

    pub(crate) fn write(&mut self, input: &[u8], finish: bool) -> Output {
        match self {
            Self::Deflate { inflate, ended } => {
                let mut out = Vec::new();
                let rest = match inflate_into(inflate, ended, input, &mut out) {
                    Ok(rest) => rest,
                    Err(()) => return Output::failed(Vec::new()),
                };
                // Additional data after the end of the compressed stream, or
                // an incomplete stream at the end of the input, is an error.
                if rest > 0 || (finish && !*ended) {
                    return Output::failed(out);
                }
                Output::ok(out)
            }
            Self::Gzip(decoder) => decoder.write(input, finish),
            Self::Brotli(decoder) => decoder.write(input, finish),
        }
    }
}

/// Inflate `input` into `out`, returning how many input bytes remain after
/// the end of the compressed stream.
fn inflate_into(
    inflate: &mut Decompress,
    ended: &mut bool,
    mut input: &[u8],
    out: &mut Vec<u8>,
) -> Result<usize, ()> {
    if *ended {
        return Ok(input.len());
    }
    loop {
        if out.capacity() - out.len() < 8192 {
            out.reserve(input.len().saturating_mul(2).max(16384));
        }
        let consumed_before = inflate.total_in();
        let produced_before = inflate.total_out();
        let status = inflate
            .decompress_vec(input, out, FlushDecompress::None)
            .map_err(|_| ())?;
        let consumed = (inflate.total_in() - consumed_before) as usize;
        input = &input[consumed..];
        let produced = inflate.total_out() - produced_before;
        if status == Status::StreamEnd {
            *ended = true;
            return Ok(input.len());
        }
        if input.is_empty() && out.len() < out.capacity() {
            return Ok(0);
        }
        if consumed == 0 && produced == 0 && out.len() < out.capacity() {
            return Ok(0);
        }
    }
}

#[derive(Default)]
enum GzipPhase {
    #[default]
    Header,
    Body,
    Trailer,
    Done,
}

/// RFC 1952 member decoder: header, raw DEFLATE body, CRC32/ISIZE trailer.
pub(crate) struct GzipDecoder {
    phase: GzipPhase,
    header: Vec<u8>,
    inflate: Decompress,
    inflate_ended: bool,
    crc: Crc,
    trailer: Vec<u8>,
}

impl Default for GzipDecoder {
    fn default() -> Self {
        Self {
            phase: GzipPhase::Header,
            header: Vec::new(),
            inflate: Decompress::new(false),
            inflate_ended: false,
            crc: Crc::new(),
            trailer: Vec::new(),
        }
    }
}

const FHCRC: u8 = 2;
const FEXTRA: u8 = 4;
const FNAME: u8 = 8;
const FCOMMENT: u8 = 16;

impl GzipDecoder {
    /// The length of the complete header in `self.header`, `Ok(None)` while
    /// more bytes are needed, or an error for an invalid header.
    fn header_length(&self) -> Result<Option<usize>, ()> {
        let header = &self.header;
        if !header.is_empty() && header[0] != 0x1f {
            return Err(());
        }
        if header.len() >= 2 && header[1] != 0x8b {
            return Err(());
        }
        if header.len() >= 3 && header[2] != 8 {
            return Err(());
        }
        if header.len() < 10 {
            return Ok(None);
        }
        let flags = header[3];
        // Reserved FLG bits must be zero (RFC 1952 §2.3.1.2).
        if flags & 0xe0 != 0 {
            return Err(());
        }
        let mut position = 10;
        if flags & FEXTRA != 0 {
            if header.len() < position + 2 {
                return Ok(None);
            }
            let length = usize::from(u16::from_le_bytes([header[position], header[position + 1]]));
            position += 2 + length;
            if header.len() < position {
                return Ok(None);
            }
        }
        for flag in [FNAME, FCOMMENT] {
            if flags & flag != 0 {
                match header[position.min(header.len())..]
                    .iter()
                    .position(|&byte| byte == 0)
                {
                    Some(end) => position += end + 1,
                    None => return Ok(None),
                }
            }
        }
        if flags & FHCRC != 0 {
            if header.len() < position + 2 {
                return Ok(None);
            }
            let mut crc = Crc::new();
            crc.update(&header[..position]);
            let expected = u16::from_le_bytes([header[position], header[position + 1]]);
            if (crc.sum() & 0xffff) as u16 != expected {
                return Err(());
            }
            position += 2;
        }
        Ok(Some(position))
    }

    fn write(&mut self, mut input: &[u8], finish: bool) -> Output {
        let mut out = Vec::new();
        loop {
            match self.phase {
                GzipPhase::Header => {
                    if input.is_empty() {
                        break;
                    }
                    // Feed bytes one header field at a time: append, then
                    // find the end of the header within what is buffered.
                    let before = self.header.len();
                    self.header.extend_from_slice(input);
                    match self.header_length() {
                        Err(()) => return Output::failed(Vec::new()),
                        Ok(None) => {
                            input = &[];
                        }
                        Ok(Some(length)) => {
                            let used = length - before;
                            input = &input[used..];
                            self.header.truncate(length);
                            self.phase = GzipPhase::Body;
                        }
                    }
                }
                GzipPhase::Body => {
                    let start = out.len();
                    let rest = match inflate_into(
                        &mut self.inflate,
                        &mut self.inflate_ended,
                        input,
                        &mut out,
                    ) {
                        Ok(rest) => rest,
                        Err(()) => return Output::failed(Vec::new()),
                    };
                    self.crc.update(&out[start..]);
                    input = &input[input.len() - rest..];
                    if self.inflate_ended {
                        self.phase = GzipPhase::Trailer;
                    } else {
                        break;
                    }
                }
                GzipPhase::Trailer => {
                    let needed = 8 - self.trailer.len();
                    let take = needed.min(input.len());
                    self.trailer.extend_from_slice(&input[..take]);
                    input = &input[take..];
                    if self.trailer.len() < 8 {
                        break;
                    }
                    let crc = u32::from_le_bytes([
                        self.trailer[0],
                        self.trailer[1],
                        self.trailer[2],
                        self.trailer[3],
                    ]);
                    let size = u32::from_le_bytes([
                        self.trailer[4],
                        self.trailer[5],
                        self.trailer[6],
                        self.trailer[7],
                    ]);
                    if crc != self.crc.sum() || size != self.crc.amount() {
                        return Output::failed(Vec::new());
                    }
                    self.phase = GzipPhase::Done;
                }
                GzipPhase::Done => {
                    // A gzip stream contains exactly one member.
                    if !input.is_empty() {
                        return Output::failed(out);
                    }
                    break;
                }
            }
        }
        if finish && !matches!(self.phase, GzipPhase::Done) {
            return Output::failed(out);
        }
        Output::ok(out)
    }
}

pub(crate) struct BrotliDecoder {
    state: brotli_decompressor::BrotliState<
        brotli_decompressor::StandardAlloc,
        brotli_decompressor::StandardAlloc,
        brotli_decompressor::StandardAlloc,
    >,
    ended: bool,
}

impl BrotliDecoder {
    fn new() -> Self {
        Self {
            state: brotli_decompressor::BrotliState::new_strict(
                brotli_decompressor::StandardAlloc::default(),
                brotli_decompressor::StandardAlloc::default(),
                brotli_decompressor::StandardAlloc::default(),
            ),
            ended: false,
        }
    }

    fn write(&mut self, input: &[u8], finish: bool) -> Output {
        use brotli_decompressor::BrotliResult;
        let mut out = Vec::new();
        if self.ended {
            return if input.is_empty() {
                Output::ok(out)
            } else {
                Output::failed(out)
            };
        }
        let mut available_in = input.len();
        let mut input_offset = 0;
        let mut buffer = vec![0u8; 65536];
        let mut total_out = 0;
        loop {
            let mut available_out = buffer.len();
            let mut output_offset = 0;
            let result = brotli_decompressor::BrotliDecompressStream(
                &mut available_in,
                &mut input_offset,
                input,
                &mut available_out,
                &mut output_offset,
                &mut buffer,
                &mut total_out,
                &mut self.state,
            );
            out.extend_from_slice(&buffer[..output_offset]);
            match result {
                BrotliResult::ResultSuccess => {
                    self.ended = true;
                    if available_in > 0 {
                        return Output::failed(out);
                    }
                    break;
                }
                BrotliResult::NeedsMoreOutput => continue,
                BrotliResult::NeedsMoreInput => break,
                BrotliResult::ResultFailure => return Output::failed(Vec::new()),
            }
        }
        if finish && !self.ended {
            return Output::failed(out);
        }
        Output::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(format: &str, data: &[u8], chunk: usize) -> Vec<u8> {
        let mut registry = Registry::default();
        let compressor = registry.create(format, false).unwrap();
        let mut compressed = Vec::new();
        for piece in data.chunks(chunk.max(1)) {
            let output = registry.write(compressor, piece, false).unwrap();
            assert!(!output.failed);
            compressed.extend(output.bytes);
        }
        let output = registry.write(compressor, &[], true).unwrap();
        assert!(!output.failed);
        compressed.extend(output.bytes);
        assert_eq!(registry.live(), 0, "finishing releases the context");
        let decompressor = registry.create(format, true).unwrap();
        let mut decoded = Vec::new();
        for piece in compressed.chunks(chunk.max(1)) {
            let output = registry.write(decompressor, piece, false).unwrap();
            assert!(!output.failed, "{format} chunk failed");
            decoded.extend(output.bytes);
        }
        let output = registry.write(decompressor, &[], true).unwrap();
        assert!(!output.failed, "{format} end failed");
        decoded.extend(output.bytes);
        decoded
    }

    #[test]
    fn every_format_round_trips_in_any_chunking() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
        for format in ["deflate", "deflate-raw", "gzip", "brotli"] {
            for chunk in [1, 7, 4096, 1 << 20] {
                let sample = if chunk == 1 { &data[..3000] } else { &data[..] };
                assert_eq!(
                    round_trip(format, sample, chunk),
                    sample,
                    "{format}/{chunk}"
                );
            }
            assert_eq!(
                round_trip(format, &[], 16),
                Vec::<u8>::new(),
                "{format} empty"
            );
        }
    }

    #[test]
    fn decompressors_reject_trailing_incomplete_and_corrupt_input() {
        let mut registry = Registry::default();
        // gzip of "a" followed by a second member: one member only.
        let compressor = registry.create("gzip", false).unwrap();
        let mut member = registry.write(compressor, b"a", true).unwrap().bytes;
        let single = member.clone();
        member.extend_from_slice(&single);
        let handle = registry.create("gzip", true).unwrap();
        let output = registry.write(handle, &member, false).unwrap();
        assert!(output.failed);
        assert_eq!(
            output.bytes, b"a",
            "output before the trailing data is kept"
        );
        // Truncated input fails at the end of the input.
        let handle = registry.create("gzip", true).unwrap();
        assert!(
            !registry
                .write(handle, &single[..single.len() - 3], false)
                .unwrap()
                .failed
        );
        assert!(registry.write(handle, &[], true).unwrap().failed);
        // A corrupt CRC32.
        let mut corrupt = single.clone();
        let at = corrupt.len() - 8;
        corrupt[at] ^= 1;
        let handle = registry.create("gzip", true).unwrap();
        assert!(registry.write(handle, &corrupt, true).unwrap().failed);
        // zlib FDICT is unsupported.
        let handle = registry.create("deflate", true).unwrap();
        assert!(
            registry
                .write(handle, &[0x78, 0xbb, 0, 0, 0, 1], true)
                .unwrap()
                .failed
        );
        // Reserved gzip flag bits.
        let handle = registry.create("gzip", true).unwrap();
        assert!(
            registry
                .write(handle, &[0x1f, 0x8b, 8, 0x20, 0, 0, 0, 0, 0, 3], false)
                .unwrap()
                .failed
        );
        assert_eq!(registry.live(), 0, "failures release their contexts");
    }

    #[test]
    fn gzip_header_fields_and_fhcrc_are_validated() {
        // FNAME "x" and a correct FHCRC, then a stored empty block.
        let mut header = vec![0x1f, 0x8b, 8, FNAME | FHCRC, 0, 0, 0, 0, 0, 3, b'x', 0];
        let mut crc = Crc::new();
        crc.update(&header);
        header.extend_from_slice(&((crc.sum() & 0xffff) as u16).to_le_bytes());
        let mut stream = header.clone();
        stream.extend_from_slice(&[3, 0]); // final fixed block, end of block
        stream.extend_from_slice(&Crc::new().sum().to_le_bytes());
        stream.extend_from_slice(&0u32.to_le_bytes());
        let mut registry = Registry::default();
        for split in 0..stream.len() {
            let handle = registry.create("gzip", true).unwrap();
            assert!(
                !registry
                    .write(handle, &stream[..split], false)
                    .unwrap()
                    .failed,
                "split {split}"
            );
            let output = registry.write(handle, &stream[split..], true).unwrap();
            assert!(!output.failed, "split {split}");
            assert!(output.bytes.is_empty());
        }
        let mut bad = stream.clone();
        bad[12] ^= 1;
        let handle = registry.create("gzip", true).unwrap();
        assert!(registry.write(handle, &bad, true).unwrap().failed);
    }

    #[test]
    fn the_oldest_context_is_evicted_past_the_bound() {
        let mut registry = Registry::default();
        let first = registry.create("deflate", false).unwrap();
        for _ in 0..MAX_CONTEXTS {
            registry.create("deflate", false).unwrap();
        }
        assert_eq!(registry.live(), MAX_CONTEXTS);
        assert!(registry.write(first, b"x", false).is_none());
        assert!(registry.create("zstd", false).is_none());
    }
}
