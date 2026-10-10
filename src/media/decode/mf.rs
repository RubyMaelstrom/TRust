//! Media Foundation decoders (Windows): the system's synchronous decoder
//! MFTs for H.264, HEVC, VP9 and AAC, as Firefox's WMF decoder module and
//! Chrome's MediaFoundation decoders use them. HEVC and VP9 come from the
//! Store extensions most Windows installations carry; whichever decoders
//! the system has are what `canPlayType()` and MSE report.
//!
//! H.264 and HEVC samples are converted to Annex B ([`super::annexb`]);
//! video comes out as NV12 (P010 for 10-bit), audio as 32-bit float. COM is
//! initialised (multithreaded apartment) on each thread that uses a
//! decoder.

use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_MT_SUBTYPE, MF_MT_USER_DATA, MF_VERSION, MFAudioFormat_AAC, MFAudioFormat_Float,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio, MFMediaType_Video,
    MFSTARTUP_NOSOCKET, MFSampleExtension_CleanPoint, MFStartup, MFT_CATEGORY_AUDIO_DECODER,
    MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoArea, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MFVideoFormat_P010, MFVideoFormat_VP90,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::{GUID, Interface};

use super::annexb::AnnexB;
use super::{AudioDecoder, DecoderModule, Error, VideoDecoder};
use crate::media::codec::{Codec, CodecString};
use crate::media::frame::{self, AudioBlock, Layout, Plane, PlaneData, VideoFrame};
use crate::media::track::{Packet, TrackInfo};

/// Initialise COM on this thread once (multithreaded apartment).
fn com() {
    thread_local! {
        static READY: Cell<bool> = const { Cell::new(false) };
    }
    READY.with(|ready| {
        if !ready.get() {
            // SAFETY: initialising this thread's apartment; never balanced,
            // as the thread keeps using COM until it ends.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            ready.set(true);
        }
    });
}

fn startup() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        com();
        // SAFETY: process-wide Media Foundation start-up, never shut down.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.is_ok()
    })
}

/// The MFT category and input type of a codec.
fn input_of(codec: Codec) -> Option<(GUID, GUID, GUID)> {
    Some(match codec {
        Codec::H264 => (
            MFT_CATEGORY_VIDEO_DECODER,
            MFMediaType_Video,
            MFVideoFormat_H264,
        ),
        Codec::Hevc => (
            MFT_CATEGORY_VIDEO_DECODER,
            MFMediaType_Video,
            MFVideoFormat_HEVC,
        ),
        Codec::Vp9 => (
            MFT_CATEGORY_VIDEO_DECODER,
            MFMediaType_Video,
            MFVideoFormat_VP90,
        ),
        Codec::Aac => (
            MFT_CATEGORY_AUDIO_DECODER,
            MFMediaType_Audio,
            MFAudioFormat_AAC,
        ),
        _ => return None,
    })
}

/// A new instance of the first synchronous decoder MFT for `codec`.
fn create(codec: Codec) -> Option<IMFTransform> {
    if !startup() {
        return None;
    }
    com();
    let (category, major, subtype) = input_of(codec)?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: major,
        guidSubtype: subtype,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx fills an array of `count` activation objects that we
    // release and free below.
    unsafe {
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut activates,
            &mut count,
        )
        .ok()?;
        if activates.is_null() {
            return None;
        }
        let list = std::slice::from_raw_parts_mut(activates, count as usize);
        let mut transform = None;
        for activate in list.iter_mut() {
            if let Some(activate) = activate.take()
                && transform.is_none()
            {
                transform = activate.ActivateObject::<IMFTransform>().ok();
            }
        }
        CoTaskMemFree(Some(activates.cast()));
        transform
    }
}

/// Whether the system has a decoder for `codec` (probed once).
fn available(codec: Codec) -> bool {
    static PROBED: OnceLock<Mutex<Vec<(Codec, bool)>>> = OnceLock::new();
    let probed = PROBED.get_or_init(Default::default);
    if let Some(&(_, answer)) = probed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(c, _)| *c == codec)
    {
        return answer;
    }
    let answer = create(codec).is_some();
    probed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((codec, answer));
    answer
}

pub(super) struct MediaFoundation;

impl DecoderModule for MediaFoundation {
    fn name(&self) -> &'static str {
        "media-foundation"
    }

    fn supports(&self, codec: &CodecString) -> bool {
        let depth = codec.details.bit_depth;
        let plausible = match codec.codec {
            // The H.264 decoder is 8-bit only.
            Codec::H264 => depth.is_none_or(|depth| depth <= 8),
            Codec::Hevc | Codec::Vp9 => depth.is_none_or(|depth| depth <= 10),
            Codec::Aac => true,
            _ => false,
        };
        plausible && available(codec.codec)
    }

    fn video(&self, track: &TrackInfo) -> Option<Result<Box<dyn VideoDecoder>, Error>> {
        input_of(track.codec).filter(|(category, ..)| *category == MFT_CATEGORY_VIDEO_DECODER)?;
        Some(MfVideo::new(track).map(|decoder| Box::new(decoder) as Box<dyn VideoDecoder>))
    }

    fn audio(&self, track: &TrackInfo) -> Option<Result<Box<dyn AudioDecoder>, Error>> {
        (track.codec == Codec::Aac)
            .then(|| MfAudio::new(track).map(|decoder| Box::new(decoder) as Box<dyn AudioDecoder>))
    }
}

fn failed(what: &str, error: windows::core::Error) -> Error {
    Error::Failed(format!("Media Foundation {what}: {error}"))
}

/// A decoder MFT with its single input and output stream.
struct Transform {
    mft: IMFTransform,
    /// The MFT allocates its output samples.
    provides: bool,
    output_size: u32,
}

/// What a request for output produced.
enum Output {
    Sample(IMFSample),
    NeedInput,
    /// The output format changed and was renegotiated.
    Changed,
}

impl Transform {
    fn new(
        codec: Codec,
        input: &IMFMediaType,
        output: impl Fn(&IMFMediaType) -> bool,
    ) -> Result<Self, Error> {
        let mft = create(codec).ok_or_else(|| {
            Error::Unsupported(format!("no Media Foundation decoder for {codec:?}"))
        })?;
        // SAFETY: plain calls on the MFT we own.
        unsafe { mft.SetInputType(0, input, 0) }.map_err(|e| failed("input type", e))?;
        let mut transform = Self {
            mft,
            provides: false,
            output_size: 0,
        };
        transform.choose_output(&output)?;
        // SAFETY: streaming notifications on our MFT.
        unsafe {
            let _ = transform
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = transform
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        Ok(transform)
    }

    /// Set the first offered output type `accept` takes.
    fn choose_output(&mut self, accept: &dyn Fn(&IMFMediaType) -> bool) -> Result<(), Error> {
        for index in 0..64 {
            // SAFETY: enumerating our MFT's output types.
            let Ok(kind) = (unsafe { self.mft.GetOutputAvailableType(0, index) }) else {
                break;
            };
            if accept(&kind) {
                // SAFETY: setting a type the MFT offered.
                unsafe { self.mft.SetOutputType(0, &kind, 0) }
                    .map_err(|e| failed("output type", e))?;
                // SAFETY: querying our MFT.
                let info = unsafe { self.mft.GetOutputStreamInfo(0) }
                    .map_err(|e| failed("stream info", e))?;
                self.provides = info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
                    != 0;
                self.output_size = info.cbSize.max(1);
                return Ok(());
            }
        }
        Err(Error::Unsupported(String::from(
            "Media Foundation offers no usable output format",
        )))
    }

    /// Hand the decoder one sample; false when it must give output first.
    fn send(
        &mut self,
        data: &[u8],
        time: i64,
        duration: i64,
        keyframe: bool,
    ) -> Result<bool, Error> {
        // SAFETY: a memory buffer we fill within its length, wrapped in a
        // sample the MFT takes a reference to.
        unsafe {
            let buffer =
                MFCreateMemoryBuffer(data.len().max(1) as u32).map_err(|e| failed("buffer", e))?;
            let mut pointer = std::ptr::null_mut();
            buffer
                .Lock(&mut pointer, None, None)
                .map_err(|e| failed("buffer lock", e))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), pointer, data.len());
            let _ = buffer.Unlock();
            buffer
                .SetCurrentLength(data.len() as u32)
                .map_err(|e| failed("buffer length", e))?;
            let sample = MFCreateSample().map_err(|e| failed("sample", e))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| failed("sample buffer", e))?;
            let _ = sample.SetSampleTime(time);
            let _ = sample.SetSampleDuration(duration);
            if keyframe {
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }
            match self.mft.ProcessInput(0, &sample, 0) {
                Ok(()) => Ok(true),
                Err(error) if error.code() == MF_E_NOTACCEPTING => Ok(false),
                Err(error) => Err(Error::Corrupt(format!("Media Foundation input: {error}"))),
            }
        }
    }

    fn receive(&mut self, accept: &dyn Fn(&IMFMediaType) -> bool) -> Result<Output, Error> {
        // SAFETY: an output buffer description holding our sample (when the
        // MFT does not allocate), whose ownership we take back afterwards.
        unsafe {
            let sample = if self.provides {
                None
            } else {
                let buffer =
                    MFCreateMemoryBuffer(self.output_size).map_err(|e| failed("buffer", e))?;
                let sample = MFCreateSample().map_err(|e| failed("sample", e))?;
                sample
                    .AddBuffer(&buffer)
                    .map_err(|e| failed("sample buffer", e))?;
                Some(sample)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.mft.ProcessOutput(0, &mut buffers, &mut status);
            let [buffer] = buffers;
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => sample.map(Output::Sample).ok_or_else(|| {
                    Error::Failed(String::from("Media Foundation produced no sample"))
                }),
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    Ok(Output::NeedInput)
                }
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.choose_output(accept)?;
                    Ok(Output::Changed)
                }
                Err(error) => Err(Error::Corrupt(format!("Media Foundation output: {error}"))),
            }
        }
    }

    fn message(&self, message: windows::Win32::Media::MediaFoundation::MFT_MESSAGE_TYPE) {
        // SAFETY: a control message to our MFT.
        let _ = unsafe { self.mft.ProcessMessage(message, 0) };
    }

    fn output_type(&self) -> Option<IMFMediaType> {
        // SAFETY: querying our MFT.
        unsafe { self.mft.GetOutputCurrentType(0) }.ok()
    }
}

fn subtype(kind: &IMFMediaType) -> Option<GUID> {
    // SAFETY: reading an attribute of a media type.
    unsafe { kind.GetGUID(&MF_MT_SUBTYPE) }.ok()
}

/// Microseconds to Media Foundation's 100-nanosecond units and back.
fn to_hns(micros: i64) -> i64 {
    micros.saturating_mul(10)
}

fn from_hns(hns: i64) -> i64 {
    hns / 10
}

struct MfVideo {
    transform: Transform,
    annexb: Option<AnnexB>,
    ten_bit: bool,
    container_color: Option<crate::media::track::ColorInfo>,
    aspect: Option<(u32, u32, u32, u32)>,
    name: &'static str,
}

impl MfVideo {
    fn new(track: &TrackInfo) -> Result<Self, Error> {
        com();
        let (_, major, subtype_guid) =
            input_of(track.codec).ok_or_else(|| Error::Unsupported(String::from("codec")))?;
        let annexb = match track.codec {
            Codec::H264 => Some(
                AnnexB::from_avcc(&track.config)
                    .ok_or_else(|| Error::Corrupt(String::from("avcC")))?,
            ),
            Codec::Hevc => Some(
                AnnexB::from_hvcc(&track.config)
                    .ok_or_else(|| Error::Corrupt(String::from("hvcC")))?,
            ),
            _ => None,
        };
        let video = track.video.unwrap_or_default();
        let ten_bit = track.details.bit_depth.is_some_and(|depth| depth > 8);
        // SAFETY: building a media type we own.
        let input = unsafe {
            let input = MFCreateMediaType().map_err(|e| failed("media type", e))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &major)
                .map_err(|e| failed("media type", e))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &subtype_guid)
                .map_err(|e| failed("media type", e))?;
            if video.width > 0 && video.height > 0 {
                let _ = input.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(video.width) << 32) | u64::from(video.height),
                );
            }
            if let Some(annexb) = &annexb {
                let _ = input.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, annexb.parameter_sets());
            }
            input
        };
        let wanted = if ten_bit {
            MFVideoFormat_P010
        } else {
            MFVideoFormat_NV12
        };
        let transform = Transform::new(track.codec, &input, move |kind| {
            subtype(kind) == Some(wanted)
        })
        .or_else(|_| {
            Transform::new(track.codec, &input, |kind| {
                subtype(kind) == Some(MFVideoFormat_NV12)
            })
        })?;
        Ok(Self {
            transform,
            annexb,
            ten_bit,
            container_color: video.color,
            aspect: (video.display_width != video.width || video.display_height != video.height)
                .then_some((
                    video.width,
                    video.height,
                    video.display_width,
                    video.display_height,
                ))
                .filter(|&(w, h, dw, dh)| w > 0 && h > 0 && dw > 0 && dh > 0),
            name: match track.codec {
                Codec::H264 => "mf-h264",
                Codec::Hevc => "mf-hevc",
                _ => "mf-vp9",
            },
        })
    }

    fn accept(ten_bit: bool) -> impl Fn(&IMFMediaType) -> bool {
        move |kind| {
            let subtype = subtype(kind);
            subtype
                == Some(if ten_bit {
                    MFVideoFormat_P010
                } else {
                    MFVideoFormat_NV12
                })
                || subtype == Some(MFVideoFormat_NV12)
        }
    }

    /// Copy a decoded NV12/P010 sample out of Media Foundation's buffer.
    fn convert(&self, sample: &IMFSample, duration: i64) -> Option<VideoFrame> {
        let kind = self.transform.output_type()?;
        let p010 = subtype(&kind) == Some(MFVideoFormat_P010);
        // SAFETY: reading attributes of the current output type, and the
        // sample's buffer within the extent its type and pitch describe,
        // under its lock.
        unsafe {
            let size = kind.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
            let (coded_width, coded_height) =
                ((size >> 32) as usize, (size & 0xFFFF_FFFF) as usize);
            let mut aperture: MFVideoArea = std::mem::zeroed();
            let visible = {
                let bytes = std::slice::from_raw_parts_mut(
                    (&raw mut aperture).cast::<u8>(),
                    size_of::<MFVideoArea>(),
                );
                kind.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None)
                    .is_ok()
            };
            let (width, height) = if visible && aperture.Area.cx > 0 && aperture.Area.cy > 0 {
                (
                    (aperture.Area.cx as usize).min(coded_width),
                    (aperture.Area.cy as usize).min(coded_height),
                )
            } else {
                (coded_width, coded_height)
            };
            if width == 0 || height == 0 {
                return None;
            }
            let time = sample.GetSampleTime().ok()?;
            let buffer = sample.ConvertToContiguousBuffer().ok()?;
            let sample_bytes = if p010 { 2 } else { 1 };
            let (pointer, pitch, locked_2d) = if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                let mut scan0 = std::ptr::null_mut();
                let mut pitch = 0i32;
                two_d.Lock2D(&mut scan0, &mut pitch).ok()?;
                (scan0, pitch as isize, Some(two_d))
            } else {
                let mut pointer = std::ptr::null_mut();
                buffer.Lock(&mut pointer, None, None).ok()?;
                let stride = kind
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .map(|s| s as i32 as isize)
                    .unwrap_or((coded_width * sample_bytes) as isize);
                (pointer, stride, None)
            };
            let row = width * sample_bytes;
            let chroma_rows = height.div_ceil(2);
            let mut luma = Vec::with_capacity(row * height);
            let mut chroma = Vec::with_capacity(row * chroma_rows);
            if pitch.unsigned_abs() >= row {
                for y in 0..height {
                    let line = pointer.offset(y as isize * pitch);
                    luma.extend_from_slice(std::slice::from_raw_parts(line, row));
                }
                let uv = pointer.offset(coded_height as isize * pitch);
                for y in 0..chroma_rows {
                    let line = uv.offset(y as isize * pitch);
                    chroma.extend_from_slice(std::slice::from_raw_parts(line, row));
                }
            }
            match locked_2d {
                Some(two_d) => {
                    let _ = two_d.Unlock2D();
                }
                None => {
                    let _ = buffer.Unlock();
                }
            }
            if luma.len() != row * height {
                return None;
            }
            let planes = if p010 {
                let words = |bytes: Vec<u8>| {
                    bytes
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .collect()
                };
                vec![
                    Plane {
                        data: PlaneData::U16(words(luma)),
                        stride: width,
                        width,
                        height,
                    },
                    Plane {
                        data: PlaneData::U16(words(chroma)),
                        stride: width,
                        width,
                        height: chroma_rows,
                    },
                ]
            } else {
                vec![
                    Plane {
                        data: PlaneData::U8(luma),
                        stride: width,
                        width,
                        height,
                    },
                    Plane {
                        data: PlaneData::U8(chroma),
                        stride: width,
                        width,
                        height: chroma_rows,
                    },
                ]
            };
            let (display_width, display_height) =
                super::bundled::display_size(self.aspect, width as u32, height as u32);
            Some(VideoFrame {
                pts: from_hns(time),
                duration,
                width: width as u32,
                height: height as u32,
                display_width,
                display_height,
                bit_depth: if p010 { 16 } else { 8 },
                layout: Layout::SemiPlanar,
                color: frame::resolve_color(self.container_color, width as u32, height as u32),
                planes,
            })
        }
    }

    fn collect(&mut self, duration: i64, frames: &mut Vec<VideoFrame>) -> Result<(), Error> {
        let accept = Self::accept(self.ten_bit);
        for _ in 0..64 {
            match self.transform.receive(&accept)? {
                Output::Sample(sample) => frames.extend(self.convert(&sample, duration)),
                Output::Changed => {}
                Output::NeedInput => return Ok(()),
            }
        }
        Ok(())
    }
}

impl VideoDecoder for MfVideo {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<VideoFrame>, Error> {
        com();
        let data = match &self.annexb {
            Some(annexb) => annexb
                .convert(&packet.data, packet.keyframe)
                .ok_or_else(|| {
                    Error::Corrupt(String::from("NAL unit lengths overrun the sample"))
                })?,
            None => packet.data.to_vec(),
        };
        let mut frames = Vec::new();
        for _ in 0..4 {
            if self.transform.send(
                &data,
                to_hns(packet.pts),
                to_hns(packet.duration),
                packet.keyframe,
            )? {
                break;
            }
            // The decoder holds output: take it, then offer the sample again.
            self.collect(packet.duration, &mut frames)?;
        }
        self.collect(packet.duration, &mut frames)?;
        Ok(frames)
    }

    fn drain(&mut self) -> Result<Vec<VideoFrame>, Error> {
        com();
        self.transform.message(MFT_MESSAGE_COMMAND_DRAIN);
        let mut frames = Vec::new();
        self.collect(0, &mut frames)?;
        Ok(frames)
    }

    fn flush(&mut self) {
        com();
        self.transform.message(MFT_MESSAGE_COMMAND_FLUSH);
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn hardware(&self) -> bool {
        false
    }
}

struct MfAudio {
    transform: Transform,
}

/// The HEAACWAVEINFO fields after WAVEFORMATEX that `MF_MT_USER_DATA`
/// carries ahead of the AudioSpecificConfig: raw AAC, no profile given.
const HEAAC_INFO: [u8; 12] = [0, 0, 0xFE, 0, 0, 0, 0, 0, 0, 0, 0, 0];

impl MfAudio {
    fn new(track: &TrackInfo) -> Result<Self, Error> {
        com();
        let audio = track.audio.unwrap_or_default();
        // SAFETY: building a media type we own.
        let input = unsafe {
            let input = MFCreateMediaType().map_err(|e| failed("media type", e))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|e| failed("media type", e))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
                .map_err(|e| failed("media type", e))?;
            let _ = input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio.sample_rate.max(1));
            let _ = input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(audio.channels.max(1)));
            let _ = input.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0);
            let mut user_data = HEAAC_INFO.to_vec();
            user_data.extend_from_slice(&track.config);
            let _ = input.SetBlob(&MF_MT_USER_DATA, &user_data);
            input
        };
        let transform = Transform::new(track.codec, &input, |kind| {
            subtype(kind) == Some(MFAudioFormat_Float)
        })?;
        Ok(Self { transform })
    }

    fn convert(&self, sample: &IMFSample, fallback_pts: i64) -> Option<AudioBlock> {
        let kind = self.transform.output_type()?;
        // SAFETY: reading the current output type and the sample's
        // contiguous buffer under its lock.
        unsafe {
            let rate = kind.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).ok()?;
            let channels = kind.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok()? as u16;
            if rate == 0 || channels == 0 {
                return None;
            }
            let pts = sample.GetSampleTime().map(from_hns).unwrap_or(fallback_pts);
            let buffer = sample.ConvertToContiguousBuffer().ok()?;
            let mut pointer = std::ptr::null_mut();
            let mut length = 0u32;
            buffer
                .Lock(&mut pointer, None, Some(&raw mut length))
                .ok()?;
            let bytes = std::slice::from_raw_parts(pointer, length as usize);
            let samples: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|word| f32::from_le_bytes([word[0], word[1], word[2], word[3]]))
                .collect();
            let _ = buffer.Unlock();
            Some(AudioBlock {
                pts,
                rate,
                channels,
                samples: Arc::from(samples),
            })
        }
    }

    fn collect(&mut self, pts: i64, blocks: &mut Vec<AudioBlock>) -> Result<(), Error> {
        let accept = |kind: &IMFMediaType| subtype(kind) == Some(MFAudioFormat_Float);
        for _ in 0..64 {
            match self.transform.receive(&accept)? {
                Output::Sample(sample) => blocks.extend(self.convert(&sample, pts)),
                Output::Changed => {}
                Output::NeedInput => return Ok(()),
            }
        }
        Ok(())
    }
}

impl AudioDecoder for MfAudio {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<AudioBlock>, Error> {
        com();
        let mut blocks = Vec::new();
        for _ in 0..4 {
            if self.transform.send(
                &packet.data,
                to_hns(packet.pts),
                to_hns(packet.duration),
                true,
            )? {
                break;
            }
            self.collect(packet.pts, &mut blocks)?;
        }
        self.collect(packet.pts, &mut blocks)?;
        Ok(blocks)
    }

    fn drain(&mut self) -> Result<Vec<AudioBlock>, Error> {
        com();
        self.transform.message(MFT_MESSAGE_COMMAND_DRAIN);
        let mut blocks = Vec::new();
        self.collect(0, &mut blocks)?;
        Ok(blocks)
    }

    fn flush(&mut self) {
        com();
        self.transform.message(MFT_MESSAGE_COMMAND_FLUSH);
    }

    fn name(&self) -> &'static str {
        "mf-aac"
    }
}

// SAFETY: Media Foundation objects created in the multithreaded apartment
// may be used from any thread of it; each decoder is used by one thread at a
// time (`&mut self`), which initialises COM before using it.
unsafe impl Send for MfVideo {}
// SAFETY: as above.
unsafe impl Send for MfAudio {}
