//! Incremental content-coding decoding for response bodies that Fetch reads
//! as they arrive.
//!
//! Fetch (local whatwg/fetch@394d20d) #concept-http-network-fetch hands each
//! group of transmitted message-body bytes to #handle-content-codings before
//! appending them to the response body's buffer; the result is a failure when
//! decoding fails. RFC 9110 §8.4.1 lists codings in the order they were
//! applied, so they are undone right to left. When any listed coding is not
//! supported, #handle-content-codings returns the bytes unchanged.
//!
//! `gzip`, `deflate` and `br` use the same strict incremental decoders as
//! `DecompressionStream` (`compression_host`); `zstd` (RFC 8878, RFC 9659)
//! uses ruzstd's frame decoder. Like the whole-body path
//! (`http::decode_content_encoding`), the end of the body is not required to
//! complete the compressed data, and bytes after the end of the compressed
//! data are ignored; corrupt compressed data is a failure.

use crate::compression_host::Decompressor;

/// The HTTP `deflate` coding is a zlib stream (RFC 9110 §8.4.1.2), but many
/// servers send raw DEFLATE. Like the whole-body decoder, accept both: the
/// first two bytes decide.
enum Deflate {
    Sniffing(Vec<u8>),
    Decoding(Decompressor),
}

enum Stage {
    Decompress(Decompressor),
    Deflate(Deflate),
    Zstd(Box<Zstd>),
}

/// RFC 8878 frame decoding over input that arrives in arbitrary pieces.
struct Zstd {
    decoder: ruzstd::decoding::FrameDecoder,
    pending: Vec<u8>,
    started: bool,
}

/// Bytes of decoded output drained from the zstd window per step.
const ZSTD_DRAIN: usize = 64 * 1024;

impl Zstd {
    fn new() -> Self {
        let mut decoder = ruzstd::decoding::FrameDecoder::new();
        // RFC 9659 §3: HTTP zstd decoders must accept windows up to 8 MiB
        // and may refuse larger ones.
        decoder.set_max_window_size(8 * 1024 * 1024);
        Self {
            decoder,
            pending: Vec::new(),
            started: false,
        }
    }

    /// The length of the frame header at the start of `bytes`, once enough
    /// bytes are present to know it (RFC 8878 §3.1.1.1).
    fn header_length(bytes: &[u8]) -> Option<usize> {
        let descriptor = *bytes.get(4)?;
        let single_segment = descriptor & 0x20 != 0;
        let dictionary = [0, 1, 2, 4][usize::from(descriptor & 3)];
        let content_size = match descriptor >> 6 {
            0 if single_segment => 1,
            0 => 0,
            1 => 2,
            2 => 4,
            _ => 8,
        };
        Some(5 + usize::from(!single_segment) + dictionary + content_size)
    }

    fn write(&mut self, input: &[u8]) -> Result<Vec<u8>, ()> {
        let mut out = Vec::new();
        if self.started && self.decoder.is_finished() && self.decoder.can_collect() == 0 {
            // Bytes after the end of the frame are ignored.
            return Ok(out);
        }
        self.pending.extend_from_slice(input);
        if !self.started {
            // The frame decoder cannot resume a partially read frame header;
            // wait for all of it. A body that ends first decodes to nothing.
            match Self::header_length(&self.pending) {
                Some(length) if self.pending.len() >= length => self.started = true,
                _ => return Ok(out),
            }
        }
        let mut buffer = vec![0u8; ZSTD_DRAIN];
        loop {
            // Decode the complete blocks buffered so far. While a finished
            // frame's checksum is still incomplete, this reports the checksum
            // as read without draining output; the reads below drain it.
            let (read, written) = self
                .decoder
                .decode_from_to(&self.pending, &mut buffer)
                .map_err(|_| ())?;
            let read = read.min(self.pending.len());
            self.pending.drain(..read);
            out.extend_from_slice(&buffer[..written]);
            loop {
                let drained =
                    std::io::Read::read(&mut self.decoder, &mut buffer).map_err(|_| ())?;
                if drained == 0 {
                    break;
                }
                out.extend_from_slice(&buffer[..drained]);
            }
            if out.len() > super::MAX_BODY {
                return Err(());
            }
            if read == 0 {
                break;
            }
            if self.decoder.is_finished() && self.decoder.can_collect() == 0 {
                self.pending.clear();
                break;
            }
        }
        Ok(out)
    }
}

impl Stage {
    fn write(&mut self, input: &[u8], finish: bool) -> Result<Vec<u8>, ()> {
        match self {
            Self::Decompress(decoder) => lenient(decoder, input, finish),
            Self::Deflate(state) => {
                if let Deflate::Sniffing(head) = state {
                    head.extend_from_slice(input);
                    if head.len() < 2 && !finish {
                        return Ok(Vec::new());
                    }
                    // RFC 1950 §2.2: CM 8 and a CMF/FLG pair divisible by 31.
                    let zlib = head.len() >= 2
                        && head[0] & 0x0f == 8
                        && (u16::from(head[0]) << 8 | u16::from(head[1])) % 31 == 0;
                    let head = std::mem::take(head);
                    let format = if zlib { "deflate" } else { "deflate-raw" };
                    let Some(decoder) = Decompressor::new(format) else {
                        return Err(());
                    };
                    *state = Deflate::Decoding(decoder);
                    let Deflate::Decoding(decoder) = state else {
                        unreachable!()
                    };
                    return lenient(decoder, &head, finish);
                }
                let Deflate::Decoding(decoder) = state else {
                    unreachable!()
                };
                lenient(decoder, input, finish)
            }
            Self::Zstd(decoder) => decoder.write(input),
        }
    }
}

/// Decode with the whole-body path's tolerance: after the end of the
/// compressed data further input is ignored, and the end of the body need
/// not complete the compressed data. Corrupt data is a failure.
fn lenient(decoder: &mut Decompressor, input: &[u8], finish: bool) -> Result<Vec<u8>, ()> {
    if decoder.finished() {
        return Ok(Vec::new());
    }
    let output = decoder.write(input, false);
    if output.failed && !decoder.finished() {
        return Err(());
    }
    if finish && !decoder.finished() {
        // An incomplete stream at the end of the body keeps what decoded.
        let _ = decoder.write(&[], true);
    }
    Ok(output.bytes)
}

/// The decoding pipeline for one response's `Content-Encoding`.
pub(crate) struct ContentDecoder {
    /// Applied in order to the transmitted bytes: the last listed coding first.
    stages: Vec<Stage>,
    failed: bool,
}

impl ContentDecoder {
    pub(crate) fn new(headers: &super::Headers) -> Self {
        let mut stages = Vec::new();
        let codings = headers
            .get("content-encoding")
            .map_or_else(Vec::new, |value| {
                value
                    .split(',')
                    .map(|coding| coding.trim().to_ascii_lowercase())
                    .filter(|coding| !coding.is_empty())
                    .collect::<Vec<_>>()
            });
        for coding in codings.iter().rev() {
            let stage = match coding.as_str() {
                "identity" => continue,
                "gzip" | "x-gzip" => Decompressor::new("gzip").map(Stage::Decompress),
                "br" => Decompressor::new("brotli").map(Stage::Decompress),
                "deflate" => Some(Stage::Deflate(Deflate::Sniffing(Vec::new()))),
                "zstd" => Some(Stage::Zstd(Box::new(Zstd::new()))),
                _ => None,
            };
            let Some(stage) = stage else {
                // #handle-content-codings: unsupported codings leave the
                // bytes as transmitted.
                stages.clear();
                break;
            };
            stages.push(stage);
        }
        Self {
            stages,
            failed: false,
        }
    }

    fn run(&mut self, input: &[u8], finish: bool) -> Result<Vec<u8>, String> {
        if self.failed {
            return Err(String::from("content decoding failed"));
        }
        let mut bytes = input.to_vec();
        for stage in &mut self.stages {
            match stage.write(&bytes, finish) {
                Ok(output) => bytes = output,
                Err(()) => {
                    self.failed = true;
                    return Err(String::from("content decoding failed"));
                }
            }
        }
        Ok(bytes)
    }

    /// Decode the next transmitted bytes; the result may be empty.
    pub(crate) fn decode(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        self.run(input, false)
    }

    /// The end of the transmitted bytes: anything the decoders still hold.
    pub(crate) fn finish(&mut self) -> Result<Vec<u8>, String> {
        self.run(&[], true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn headers(coding: &str) -> super::super::Headers {
        let mut headers = super::super::Headers::new();
        headers.insert(String::from("content-encoding"), coding.to_string());
        headers
    }

    fn decode_in_pieces(coding: &str, encoded: &[u8], piece: usize) -> Result<Vec<u8>, String> {
        let mut decoder = ContentDecoder::new(&headers(coding));
        let mut out = Vec::new();
        for chunk in encoded.chunks(piece.max(1)) {
            out.extend(decoder.decode(chunk)?);
        }
        out.extend(decoder.finish()?);
        Ok(out)
    }

    fn sample() -> Vec<u8> {
        (0..20_000u32)
            .flat_map(|i| format!("{{\"line\":{i},\"text\":\"Hello! 👋\"}}\n").into_bytes())
            .collect()
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn every_supported_coding_decodes_in_any_piece_size() {
        let data = sample();
        let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        zlib.write_all(&data).unwrap();
        let zlib = zlib.finish().unwrap();
        let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        raw.write_all(&data).unwrap();
        let raw = raw.finish().unwrap();
        let mut brotli = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut brotli, 4096, 5, 22);
            writer.write_all(&data).unwrap();
        }
        let zstd = ruzstd::encoding::compress_to_vec(
            data.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        for piece in [1, 7, 1024, 65_536, usize::MAX] {
            assert_eq!(decode_in_pieces("gzip", &gzip(&data), piece).unwrap(), data);
            assert_eq!(
                decode_in_pieces("x-gzip", &gzip(&data), piece).unwrap(),
                data
            );
            assert_eq!(decode_in_pieces("deflate", &zlib, piece).unwrap(), data);
            assert_eq!(decode_in_pieces("deflate", &raw, piece).unwrap(), data);
            assert_eq!(decode_in_pieces("br", &brotli, piece).unwrap(), data);
            assert_eq!(decode_in_pieces("zstd", &zstd, piece).unwrap(), data);
            assert_eq!(decode_in_pieces("identity", &data, piece).unwrap(), data);
        }
        // RFC 9110 §8.4.1: codings are undone in the reverse of their order.
        let twice = gzip(&zstd);
        assert_eq!(decode_in_pieces("zstd, gzip", &twice, 333).unwrap(), data);
    }

    #[test]
    fn decoded_output_is_produced_before_the_end_of_the_body() {
        // A streamed response must not wait for the end of its body: the
        // first compressed piece already yields its decoded bytes.
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(b"{\"type\":\"first\"}\n").unwrap();
        encoder.flush().unwrap();
        let first = encoder.get_ref().clone();
        let mut decoder = ContentDecoder::new(&headers("gzip"));
        assert_eq!(decoder.decode(&first).unwrap(), b"{\"type\":\"first\"}\n");
    }

    #[test]
    fn unsupported_codings_leave_the_bytes_unchanged() {
        let data = gzip(b"payload");
        assert_eq!(decode_in_pieces("compress, gzip", &data, 3).unwrap(), data);
        assert_eq!(decode_in_pieces("x-unknown", b"raw", 1).unwrap(), b"raw");
    }

    #[test]
    fn corrupt_data_fails_while_truncation_and_trailing_bytes_are_tolerated() {
        let data = sample();
        let encoded = gzip(&data);
        assert!(decode_in_pieces("gzip", b"definitely not gzip", 4).is_err());
        assert!(decode_in_pieces("br", &[0xff; 64], 8).is_err());
        let truncated = decode_in_pieces("gzip", &encoded[..encoded.len() / 2], 512).unwrap();
        assert!(!truncated.is_empty() && data.starts_with(&truncated));
        let mut trailing = encoded.clone();
        trailing.extend_from_slice(b"garbage");
        assert_eq!(decode_in_pieces("gzip", &trailing, 100).unwrap(), data);
    }
}
