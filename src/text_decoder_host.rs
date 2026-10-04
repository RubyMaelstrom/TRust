//! Encoding Standard decoders behind `TextDecoder` (local whatwg/encoding;
//! #interface-textdecoder and #legacy-single-byte-encodings onwards).
//!
//! `encoding_rs`, Firefox's implementation of the Encoding Standard, supplies
//! the label table and every decoder. The platform's `TextDecoder` keeps its
//! UTF-8 fast path in JavaScript; all other encodings decode here. A streaming
//! decode (`{stream: true}`) keeps its decoder, whose state may span chunks
//! (partial sequences, ISO-2022-JP's mode), in this per-Agent registry under
//! a handle held in the TextDecoder's internal slot. The final, flushing call
//! releases it. Abandoned streams are bounded: past [`MAX_STREAMS`] live
//! decoders the oldest is dropped, and its next chunk starts a fresh decoder.

use encoding_rs::{CoderResult, Decoder, DecoderResult, Encoding};
use lumen::embed::{HostRetainedMemoryVisitor, RetainedManagedAllocation};
use std::collections::{HashMap, VecDeque};

/// Live streaming decoders kept per Agent before the oldest is dropped.
const MAX_STREAMS: usize = 1024;

/// Encoding #concept-encoding-get as used by the TextDecoder constructor:
/// failure and the replacement encoding are both refused. Returns the
/// encoding's name, lowercased (#dom-textdecoder-encoding).
pub(crate) fn encoding_name(label: &str) -> Option<String> {
    Encoding::for_label_no_replacement(label.as_bytes())
        .map(|encoding| encoding.name().to_ascii_lowercase())
}

/// The output of one `decode()` call.
pub(crate) struct Decoded {
    pub text: String,
    /// The handle to pass to the next streaming call, or 0 when the decoder
    /// was flushed or failed.
    pub handle: u32,
    /// A fatal decoder met an error (#decode-and-enqueue-a-chunk returns error).
    pub failed: bool,
}

#[derive(Default)]
pub(crate) struct Registry {
    next: u32,
    /// Each stream's decoder and, after a fatal error, the unread rest of its
    /// I/O queue.
    decoders: HashMap<u32, (Decoder, Vec<u8>)>,
    order: VecDeque<u32>,
}

impl Registry {
    /// Decode `bytes` with the encoding called `name`, continuing the stream
    /// identified by `handle` (0 starts a new one). `stream` is TextDecodeOptions'
    /// `stream`; without it the decoder is flushed (end-of-queue) and released.
    /// BOM removal applies only to the UTF-8/UTF-16 encodings whose BOM it is,
    /// and only at the start of a stream, as #concept-td-serialize requires.
    pub(crate) fn decode(
        &mut self,
        name: &str,
        bytes: &[u8],
        stream: bool,
        fatal: bool,
        ignore_bom: bool,
        handle: u32,
    ) -> Option<Decoded> {
        let encoding = Encoding::for_label_no_replacement(name.as_bytes())?;
        let (mut decoder, mut queued) = match self.release(handle) {
            Some((decoder, queued)) if decoder.encoding() == encoding => (decoder, queued),
            _ if ignore_bom => (encoding.new_decoder_without_bom_handling(), Vec::new()),
            _ => (encoding.new_decoder_with_bom_removal(), Vec::new()),
        };
        let bytes = if queued.is_empty() {
            bytes
        } else {
            queued.extend_from_slice(bytes);
            queued.as_slice()
        };
        let last = !stream;
        let mut text = String::with_capacity(bytes.len().saturating_add(16));
        let mut read = 0;
        let mut failed = false;
        loop {
            let full = if fatal {
                let (result, consumed) =
                    decoder.decode_to_string_without_replacement(&bytes[read..], &mut text, last);
                read += consumed;
                match result {
                    DecoderResult::InputEmpty => false,
                    DecoderResult::OutputFull => true,
                    DecoderResult::Malformed(..) => {
                        failed = true;
                        false
                    }
                }
            } else {
                let (result, consumed, _) =
                    decoder.decode_to_string(&bytes[read..], &mut text, last);
                read += consumed;
                matches!(result, CoderResult::OutputFull)
            };
            if !full {
                break;
            }
            text.reserve(text.capacity().max(64));
        }
        // #dom-textdecoder-decode: an error leaves the rest of the I/O queue,
        // and a streaming decoder continues with it on the next call.
        let rest = if failed {
            bytes[read..].to_vec()
        } else {
            Vec::new()
        };
        let handle = if stream {
            self.keep(handle, decoder, rest)
        } else {
            0
        };
        Some(Decoded {
            text,
            handle,
            failed,
        })
    }

    /// Native streaming state retained by this Agent; decoder internals
    /// are opaque.
    pub(crate) fn scan_retained_memory(&self, visitor: &mut dyn HostRetainedMemoryVisitor) {
        if self.decoders.capacity() != 0 {
            visitor.allocation(RetainedManagedAllocation::new(
                "trust.text-decoders",
                &self.decoders as *const _ as usize,
                self.decoders.capacity() * std::mem::size_of::<(u32, (Decoder, Vec<u8>))>()
                    + self.order.capacity() * std::mem::size_of::<u32>(),
            ));
        }
        if !self.decoders.is_empty() {
            visitor.opaque_storage();
        }
    }

    fn release(&mut self, handle: u32) -> Option<(Decoder, Vec<u8>)> {
        let state = self.decoders.remove(&handle)?;
        self.order.retain(|&kept| kept != handle);
        Some(state)
    }

    fn keep(&mut self, handle: u32, decoder: Decoder, rest: Vec<u8>) -> u32 {
        let handle = if handle != 0 && !self.decoders.contains_key(&handle) {
            handle
        } else {
            loop {
                self.next = self.next.wrapping_add(1);
                if self.next != 0 && !self.decoders.contains_key(&self.next) {
                    break self.next;
                }
            }
        };
        while self.order.len() >= MAX_STREAMS {
            if let Some(oldest) = self.order.pop_front() {
                self.decoders.remove(&oldest);
            }
        }
        self.decoders.insert(handle, (decoder, rest));
        self.order.push_back(handle);
        handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_resolve_to_lowercase_names_without_replacement() {
        assert_eq!(encoding_name(" Shift_JIS\n").as_deref(), Some("shift_jis"));
        assert_eq!(encoding_name("latin1").as_deref(), Some("windows-1252"));
        assert_eq!(encoding_name("ucs-2").as_deref(), Some("utf-16le"));
        assert_eq!(encoding_name("iso-2022-kr"), None, "replacement is refused");
        assert_eq!(encoding_name("utf-7"), None);
    }

    #[test]
    fn streaming_decoders_keep_state_across_chunks_and_release_on_flush() {
        let mut registry = Registry::default();
        // ISO-2022-JP: the katakana mode set by the first chunk applies to the second.
        let first = registry
            .decode("iso-2022-jp", b"\x1b(I", true, false, false, 0)
            .unwrap();
        assert!(first.text.is_empty() && first.handle != 0);
        let second = registry
            .decode("iso-2022-jp", b"\x31", false, false, false, first.handle)
            .unwrap();
        assert_eq!(second.text, "\u{ff71}");
        assert_eq!(second.handle, 0);
        assert!(registry.decoders.is_empty() && registry.order.is_empty());
        // A Shift_JIS lead byte split across chunks.
        let lead = registry
            .decode("shift_jis", b"\x82", true, false, false, 0)
            .unwrap();
        let trail = registry
            .decode("shift_jis", b"\xa0", false, false, false, lead.handle)
            .unwrap();
        assert_eq!(lead.text + &trail.text, "\u{3042}");
        // A truncated sequence at end-of-queue is an error.
        let truncated = registry
            .decode("shift_jis", b"\x82", false, true, false, 0)
            .unwrap();
        assert!(truncated.failed && truncated.handle == 0);
        let replaced = registry
            .decode("shift_jis", b"\x82", false, false, false, 0)
            .unwrap();
        assert_eq!(replaced.text, "\u{fffd}");
        // A fatal streaming error keeps the rest of the queue for the next call.
        // (The ASCII byte after an invalid Shift_JIS lead is restored.)
        let error = registry
            .decode("shift_jis", b"A\x82 BC", true, true, false, 0)
            .unwrap();
        assert!(error.failed && error.handle != 0);
        let resumed = registry
            .decode("shift_jis", b"D", false, true, false, error.handle)
            .unwrap();
        assert_eq!(resumed.text, " BCD");
    }

    #[test]
    fn utf16_bom_is_removed_only_for_its_own_byte_order() {
        let mut registry = Registry::default();
        let le = registry
            .decode("utf-16le", b"\xff\xfeA\x00", false, false, false, 0)
            .unwrap();
        assert_eq!(le.text, "A");
        let kept = registry
            .decode("utf-16le", b"\xff\xfeA\x00", false, false, true, 0)
            .unwrap();
        assert_eq!(kept.text, "\u{feff}A");
        let other = registry
            .decode("utf-16le", b"\xfe\xffA\x00", false, false, false, 0)
            .unwrap();
        assert_eq!(other.text, "\u{fffe}A");
    }

    #[test]
    fn abandoned_streams_are_bounded() {
        let mut registry = Registry::default();
        let first = registry
            .decode("big5", b"\x81", true, false, false, 0)
            .unwrap()
            .handle;
        for _ in 0..MAX_STREAMS {
            registry
                .decode("big5", b"\x81", true, false, false, 0)
                .unwrap();
        }
        assert_eq!(registry.decoders.len(), MAX_STREAMS);
        assert!(!registry.decoders.contains_key(&first));
    }
}
