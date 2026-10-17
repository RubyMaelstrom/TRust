//! Hardware decoding through FFmpeg's hwaccels (libavutil/hwcontext.h):
//! the GPU's video engine decodes, through VA-API, NVDEC (CUDA) or Vulkan
//! Video, whichever the system has, and each picture is copied back to
//! system memory (`av_hwframe_transfer_data`), usually as NV12 or P010, for
//! presentation. This is the copy-back mode of Firefox's FFmpeg VA-API
//! decoder without DMA-BUF export and of mpv's `*-copy` hwdecs: decoding,
//! the expensive part, leaves the CPU, and the frames stay ordinary
//! [`VideoFrame`](super::VideoFrame)s.
//!
//! One device serves the process. Creating one takes up to a second (it
//! opens a driver, and a CUDA or Vulkan context), so it is created on first
//! use and every decoder takes a reference to it. Whether the device decodes
//! a format is learned once per format by decoding a small keyframe with it
//! (`probe/`), so `hardware_decodes` (Media Capabilities' `powerEfficient`)
//! says what decoding will actually do. Libavcodec picks the device's pixel
//! format itself when one is attached (`avcodec_default_get_format`), and
//! returns to software when the hwaccel fails to initialize
//! (`ff_get_format`), so an attached device never makes a stream
//! undecodable that software decodes.
//!
//! `TRUST_HWDEC=0` turns hardware decoding off; `TRUST_HWDEC=vulkan,cuda`
//! names the device types to try, in order (default: `vaapi,cuda,vulkan`:
//! the Mesa drivers' VA-API first, as Firefox, then NVIDIA's own decoder,
//! then the vendor-neutral Vulkan Video).

use std::ffi::{CStr, CString, c_int};
use std::ptr::null_mut;
use std::sync::{Arc, OnceLock};

use super::abi::{AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX, AVBufferRef, AVCodec};
use super::codec::CodecId;
use super::library::Library;
use super::video::{VideoConfig, VideoDecoder};

/// Device types tried when `TRUST_HWDEC` does not say.
const DEFAULT_ORDER: [&str; 3] = ["vaapi", "cuda", "vulkan"];

/// The device types to try, best first; empty when hardware decoding is off.
pub(super) fn order_from_env() -> Vec<CString> {
    parse_order(std::env::var("TRUST_HWDEC").ok().as_deref())
}

fn parse_order(setting: Option<&str>) -> Vec<CString> {
    let names: Vec<&str> = match setting.map(str::trim) {
        None | Some("") => DEFAULT_ORDER.to_vec(),
        Some("0" | "off" | "none" | "no") => Vec::new(),
        Some(list) => list
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect(),
    };
    names
        .into_iter()
        .filter_map(|name| CString::new(name).ok())
        .collect()
}

/// The process's hardware decoding device (an `AVHWDeviceContext`).
pub(crate) struct Device {
    /// Its `AVHWDeviceType`.
    pub(crate) kind: c_int,
    pub(crate) name: String,
    /// This device's own reference; decoders take theirs.
    pub(crate) buffer: *mut AVBufferRef,
    unref: unsafe extern "C" fn(*mut *mut AVBufferRef),
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: the reference is this device's (the library's code stays
        // mapped for the life of the process).
        unsafe { (self.unref)(&mut self.buffer) };
    }
}

// SAFETY: `AVBufferRef` reference counts are atomic, and a device context
// is used by libavcodec from its own decoding threads; this crate only
// takes references to it.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

/// Hardware decoding state of one loaded library.
pub(crate) struct Hardware {
    order: Vec<CString>,
    device: OnceLock<Option<Device>>,
    /// Per [`CodecId`]: whether the device decodes the probe keyframe.
    decodes: [OnceLock<bool>; CodecId::ALL.len()],
}

impl Hardware {
    pub(super) fn new(order: Vec<CString>) -> Self {
        Self {
            order,
            device: OnceLock::new(),
            decodes: Default::default(),
        }
    }
}

/// A keyframe of `codec` to probe the device with, if it is a format the
/// device may decode.
fn probe_keyframe(codec: CodecId) -> Option<&'static [u8]> {
    Some(match codec {
        CodecId::H264 => include_bytes!("probe/h264.bin"),
        CodecId::Hevc => include_bytes!("probe/hevc.bin"),
        CodecId::Vp9 => include_bytes!("probe/vp9.bin"),
        CodecId::Av1 => include_bytes!("probe/av1.bin"),
        _ => return None,
    })
}

impl Library {
    /// The hardware device, created by the first call (when the system has
    /// one of the configured types) and shared by every decoder after.
    pub(crate) fn hardware_device(&self) -> Option<&Device> {
        self.hardware
            .device
            .get_or_init(|| self.create_device())
            .as_ref()
    }

    fn create_device(&self) -> Option<Device> {
        self.hw_device_offset?;
        for name in &self.hardware.order {
            // SAFETY: a lookup by NUL-terminated name; -1 (NONE) when the
            // type is unknown to this libavutil.
            let kind = unsafe { (self.api.av_hwdevice_find_type_by_name)(name.as_ptr()) };
            if kind <= 0 {
                continue;
            }
            let mut buffer = null_mut();
            // SAFETY: creates a device of a known type on its default
            // adapter; `buffer` receives the only reference on success.
            let ret = unsafe {
                (self.api.av_hwdevice_ctx_create)(
                    &mut buffer,
                    kind,
                    std::ptr::null(),
                    null_mut(),
                    0,
                )
            };
            if ret >= 0 && !buffer.is_null() {
                return Some(Device {
                    kind,
                    name: name.to_string_lossy().into_owned(),
                    buffer,
                    unref: self.api.av_buffer_unref,
                });
            }
        }
        None
    }

    /// The name of the hardware device decoders use (`vaapi`, `cuda`,
    /// `vulkan`), if there is one; creates it on first use.
    pub fn hardware_device_name(&self) -> Option<&str> {
        self.hardware_device().map(|device| device.name.as_str())
    }

    /// The pixel format `decoder` outputs through `device`, if it can use
    /// the device (`avcodec_get_hw_config`).
    pub(crate) fn hardware_format(
        &self,
        decoder: *const AVCodec,
        device: &Device,
    ) -> Option<c_int> {
        (0..)
            .map_while(|index| {
                // SAFETY: indices run from 0 until the first NULL config,
                // which is static data of the decoder.
                let config = unsafe { (self.api.avcodec_get_hw_config)(decoder, index) };
                (!config.is_null()).then(|| unsafe { &*config })
            })
            .find(|config| {
                config.methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX != 0
                    && config.device_type == device.kind
            })
            .map(|config| config.pix_fmt)
    }

    /// The decoders to try for `codec` on the hardware device: FFmpeg's
    /// own decoders, which drive hwaccels, and for AV1 the built-in `av1`
    /// decoder, which decodes only through one.
    pub(crate) fn hardware_decoder(&self, codec: CodecId) -> *const AVCodec {
        let names: &[&CStr] = match codec {
            CodecId::Av1 => &[c"av1"],
            CodecId::H264 => &[c"h264"],
            CodecId::Hevc => &[c"hevc"],
            CodecId::Vp8 => &[c"vp8"],
            CodecId::Vp9 => &[c"vp9"],
            _ => &[],
        };
        names
            .iter()
            // SAFETY: a lookup by NUL-terminated name.
            .map(|name| unsafe { (self.api.avcodec_find_decoder_by_name)(name.as_ptr()) })
            .find(|decoder| !decoder.is_null())
            .unwrap_or(std::ptr::null())
    }

    /// Whether the hardware device decodes `codec`: decided once per codec,
    /// by decoding its probe keyframe, the first time it is asked (which
    /// creates the device).
    pub fn hardware_decodes(self: &Arc<Self>, codec: CodecId) -> bool {
        *self.hardware.decodes[codec.index()].get_or_init(|| probe(self, codec))
    }

    /// [`Self::hardware_decodes`] if `codec` has been probed, without waiting.
    pub fn hardware_decodes_now(&self, codec: CodecId) -> Option<bool> {
        self.hardware.decodes[codec.index()].get().copied()
    }
}

/// Decode `codec`'s probe keyframe on the hardware device.
fn probe(library: &Arc<Library>, codec: CodecId) -> bool {
    let Some(keyframe) = probe_keyframe(codec) else {
        return false;
    };
    if library.hardware_device().is_none() {
        return false;
    }
    let config = VideoConfig {
        threads: 1,
        extradata: None,
        hardware: true,
    };
    let Ok(mut decoder) = VideoDecoder::new(library, codec, config) else {
        return false;
    };
    if !decoder.hardware() || decoder.send(keyframe, 0, true).is_err() {
        return false;
    }
    let mut decoded = false;
    while let Ok(Some(_)) = decoder.receive() {
        decoded = true;
    }
    decoded |= decoder.drain().iter().any(Result::is_ok);
    decoded && decoder.hardware_frames() > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_order_follows_trust_hwdec() {
        let names = |setting| {
            parse_order(setting)
                .into_iter()
                .map(|name| name.into_string().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(None), ["vaapi", "cuda", "vulkan"]);
        assert_eq!(names(Some("")), ["vaapi", "cuda", "vulkan"]);
        for off in ["0", "off", "none", "no"] {
            assert!(names(Some(off)).is_empty(), "{off}");
        }
        assert_eq!(names(Some("vulkan, cuda")), ["vulkan", "cuda"]);
    }

    #[test]
    fn every_hardware_format_has_a_probe_keyframe() {
        for codec in [CodecId::H264, CodecId::Hevc, CodecId::Vp9, CodecId::Av1] {
            assert!(probe_keyframe(codec).is_some_and(|data| !data.is_empty()));
        }
        assert!(probe_keyframe(CodecId::Aac).is_none());
    }
}
