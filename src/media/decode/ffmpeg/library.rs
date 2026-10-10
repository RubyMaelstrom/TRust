//! Loading libavcodec and libavutil at run time.
//!
//! The libraries are opened with `dlopen(RTLD_NOW | RTLD_LOCAL)`, newest
//! release first, every function the crate calls is resolved before the
//! result is accepted, and nothing here panics or aborts when the libraries
//! are absent, too old, not FFmpeg, or lack a symbol. Accepted libraries are
//! never closed: decoders and their threads can then never outlive the code
//! they run.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::fmt;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use libloading::os::unix::{Library as RawLibrary, RTLD_LOCAL, RTLD_NOW, Symbol};

use super::abi::{
    AV_LOG_QUIET, AV_NOPTS_VALUE, AVCOL_RANGE_UNSPECIFIED, AVCOL_UNSPECIFIED, AVCodec,
    AVCodecContext, AVCodecParameters, AVFrame, AVPacket, DescriptorHead, FrameLayout,
};
use super::codec::CodecId;
use super::error::{Attempt, Error, LoadError};
use super::formats::FormatTables;

/// Library majors this crate can drive, newest first: libavcodec major,
/// libavutil major from the same release, and the `AVFrame` layout of that
/// libavutil. The pairs come from `LIBAVCODEC_VERSION_MAJOR` /
/// `LIBAVUTIL_VERSION_MAJOR` in libavcodec/version_major.h and
/// libavutil/version.h of n5.0.3 (59/57), n6.0.1 (60/58), n7.0.3 (61/59),
/// n8.0.3 (62/60) and n9.0.2 (63/61).
const SUPPORTED: [(u32, u32, FrameLayout); 5] = [
    (63, 61, FrameLayout::V60),
    (62, 60, FrameLayout::V60),
    (61, 59, FrameLayout::V59),
    (60, 58, FrameLayout::V57),
    (59, 57, FrameLayout::V57),
];

/// A library version (`LIBAV*_VERSION_{MAJOR,MINOR,MICRO}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub micro: u32,
}

impl Version {
    /// Unpacks `AV_VERSION_INT(major, minor, micro)`.
    fn from_packed(packed: u32) -> Self {
        Version {
            major: packed >> 16,
            minor: (packed >> 8) & 0xff,
            micro: packed & 0xff,
        }
    }

    /// FFmpeg numbers its micro versions from 100; Libav's stay below that.
    fn is_ffmpeg(self) -> bool {
        self.micro >= 100
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.micro)
    }
}

/// What the environment asks for.
#[derive(Debug, Default)]
pub(super) struct Settings {
    /// `TRUST_FFMPEG=0`.
    pub(super) disabled: bool,
    /// `TRUST_FFMPEG_LIB`: a libavcodec to use instead of searching.
    pub(super) library: Option<PathBuf>,
    /// `TRUST_FFMPEG_LOG` is set: leave FFmpeg's own logging alone.
    pub(super) keep_logging: bool,
}

impl Settings {
    fn from_env() -> Self {
        Settings {
            disabled: std::env::var_os("TRUST_FFMPEG").is_some_and(|value| value == "0"),
            library: std::env::var_os("TRUST_FFMPEG_LIB")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            keep_logging: std::env::var_os("TRUST_FFMPEG_LOG").is_some(),
        }
    }
}

/// The two open shared objects.
struct Libs {
    avcodec: RawLibrary,
    avutil: RawLibrary,
}

fn symbol<T: Copy>(lib: &RawLibrary, name: &str) -> Result<T, String> {
    // SAFETY: `name` is NUL-terminated by the callers; the type `T` is the
    // function signature the FFmpeg headers declare for it (see `api!`).
    let symbol: Symbol<T> = unsafe { lib.get(name.as_bytes()) }
        .map_err(|_| format!("missing symbol {}", name.trim_end_matches('\0')))?;
    Ok(*symbol)
}

/// Declares the FFmpeg functions the crate calls, with the signature of the
/// public headers, and resolves all of them at once. Signatures are those of
/// FFmpeg 5 to 9 (`const` is not modelled).
macro_rules! api {
    ($($name:ident: $lib:ident fn($($arg:ty),* $(,)?) $(-> $ret:ty)?;)*) => {
        pub(crate) struct Api {
            $(pub(crate) $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Api {
            fn resolve(libs: &Libs) -> Result<Api, String> {
                Ok(Api {
                    $($name: symbol(&libs.$lib, concat!(stringify!($name), "\0"))?,)*
                })
            }
        }
    };
}

api! {
    avcodec_find_decoder_by_name: avcodec fn(*const c_char) -> *const AVCodec;
    avcodec_descriptor_get_by_name: avcodec fn(*const c_char) -> *const DescriptorHead;
    avcodec_alloc_context3: avcodec fn(*const AVCodec) -> *mut AVCodecContext;
    avcodec_free_context: avcodec fn(*mut *mut AVCodecContext);
    avcodec_open2: avcodec fn(*mut AVCodecContext, *const AVCodec, *mut *mut c_void) -> c_int;
    avcodec_send_packet: avcodec fn(*mut AVCodecContext, *const AVPacket) -> c_int;
    avcodec_receive_frame: avcodec fn(*mut AVCodecContext, *mut AVFrame) -> c_int;
    avcodec_flush_buffers: avcodec fn(*mut AVCodecContext);
    avcodec_parameters_alloc: avcodec fn() -> *mut AVCodecParameters;
    avcodec_parameters_free: avcodec fn(*mut *mut AVCodecParameters);
    avcodec_parameters_to_context: avcodec fn(*mut AVCodecContext, *const AVCodecParameters) -> c_int;
    av_packet_alloc: avcodec fn() -> *mut AVPacket;
    av_packet_free: avcodec fn(*mut *mut AVPacket);
    av_packet_unref: avcodec fn(*mut AVPacket);
    av_new_packet: avcodec fn(*mut AVPacket, c_int) -> c_int;
    av_log_set_level: avutil fn(c_int);
    av_opt_set: avutil fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int;
    av_opt_set_int: avutil fn(*mut c_void, *const c_char, i64, c_int) -> c_int;
    av_mallocz: avutil fn(usize) -> *mut c_void;
    av_frame_alloc: avutil fn() -> *mut AVFrame;
    av_frame_free: avutil fn(*mut *mut AVFrame);
    av_frame_unref: avutil fn(*mut AVFrame);
    av_get_pix_fmt: avutil fn(*const c_char) -> c_int;
    av_get_pix_fmt_name: avutil fn(c_int) -> *const c_char;
    av_get_sample_fmt: avutil fn(*const c_char) -> c_int;
    av_get_sample_fmt_name: avutil fn(c_int) -> *const c_char;
    av_strerror: avutil fn(c_int, *mut c_char, usize) -> c_int;
}

/// The system FFmpeg: libavcodec and libavutil loaded with `dlopen`, never
/// linked. Share it as an `Arc`; decoders keep it alive.
pub struct Library {
    pub(crate) api: Api,
    pub(crate) layout: FrameLayout,
    pub(crate) formats: FormatTables,
    /// `AVCodecID` of each [`CodecId`] in this release (0, `AV_CODEC_ID_NONE`,
    /// when the release does not know the codec).
    codec_ids: [c_int; CodecId::ALL.len()],
    avcodec_version: Version,
    avutil_version: Version,
    /// Keeps the code the function pointers above point into mapped for the
    /// life of the process.
    _libs: ManuallyDrop<Libs>,
}

impl fmt::Debug for Library {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Library")
            .field("avcodec", &self.avcodec_version.to_string())
            .field("avutil", &self.avutil_version.to_string())
            .finish()
    }
}

static SHARED: OnceLock<Result<Arc<Library>, LoadError>> = OnceLock::new();

impl Library {
    /// Opens the system FFmpeg afresh: `libavcodec.so.63` down to `.59`,
    /// newest first, with the `libavutil` of the same release.
    ///
    /// `TRUST_FFMPEG=0` makes this fail with [`LoadError::Disabled`];
    /// `TRUST_FFMPEG_LIB=/path/libavcodec.so.N` tries only that file (and the
    /// libavutil beside it) instead of searching. FFmpeg's own logging is
    /// silenced unless `TRUST_FFMPEG_LOG` is set.
    ///
    /// Most callers want [`Library::get`], which loads once per process.
    pub fn load() -> Result<Arc<Library>, LoadError> {
        load_with(&Settings::from_env())
    }

    /// The process-wide library: loaded by the first call, `None` when
    /// FFmpeg is disabled, absent or unusable ([`Library::status`] says why).
    pub fn get() -> Option<Arc<Library>> {
        Self::status().ok().cloned()
    }

    /// Like [`Library::get`], with the reason when there is no library.
    pub fn status() -> Result<&'static Arc<Library>, &'static LoadError> {
        SHARED.get_or_init(Library::load).as_ref()
    }

    pub fn avcodec_version(&self) -> Version {
        self.avcodec_version
    }

    pub fn avutil_version(&self) -> Version {
        self.avutil_version
    }

    /// Whether this FFmpeg has a software decoder for `codec`. A distribution
    /// may build FFmpeg without the patent-encumbered ones (H.264, HEVC,
    /// AAC); this reports that honestly. Hardware-only decoders do not count.
    /// Opening can still fail for reasons only `VideoDecoder::new` /
    /// `AudioDecoder::new` can tell.
    pub fn has_decoder(&self, codec: CodecId) -> bool {
        !self.find_decoder(codec).is_null()
    }

    pub(crate) fn codec_id(&self, codec: CodecId) -> c_int {
        self.codec_ids[codec.index()]
    }

    /// The first of the codec's software decoders this FFmpeg has, or null.
    pub(crate) fn find_decoder(&self, codec: CodecId) -> *const AVCodec {
        codec
            .decoder_names()
            .iter()
            // SAFETY: a lookup by NUL-terminated name.
            .map(|name| unsafe { (self.api.avcodec_find_decoder_by_name)(name.as_ptr()) })
            .find(|decoder| !decoder.is_null())
            .unwrap_or(std::ptr::null())
    }

    /// An [`Error::Ffmpeg`] for a failed call, with FFmpeg's message.
    pub(crate) fn ffmpeg_error(&self, call: &'static str, code: c_int) -> Error {
        let mut buffer: [c_char; 128] = [0; 128];
        // SAFETY: the buffer is writable for its stated size, and
        // `av_strerror` always NUL-terminates it.
        let message = unsafe {
            if (self.api.av_strerror)(code, buffer.as_mut_ptr(), buffer.len()) < 0 {
                format!("error {code}")
            } else {
                CStr::from_ptr(buffer.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            }
        };
        Error::Ffmpeg {
            call,
            code,
            message,
        }
    }

    pub(crate) fn pixel_format_name(&self, raw: c_int) -> String {
        // SAFETY: returns a static string or NULL.
        name_or_number(unsafe { (self.api.av_get_pix_fmt_name)(raw) }, raw)
    }

    pub(crate) fn sample_format_name(&self, raw: c_int) -> String {
        // SAFETY: returns a static string or NULL.
        name_or_number(unsafe { (self.api.av_get_sample_fmt_name)(raw) }, raw)
    }
}

fn name_or_number(name: *const c_char, raw: c_int) -> String {
    if name.is_null() {
        return format!("format {raw}");
    }
    // SAFETY: non-null results of the `av_get_*_name` functions are static
    // NUL-terminated strings.
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

pub(super) fn load_with(settings: &Settings) -> Result<Arc<Library>, LoadError> {
    if settings.disabled {
        return Err(LoadError::Disabled);
    }
    let candidates: Vec<PathBuf> = match &settings.library {
        Some(path) => vec![path.clone()],
        None => SUPPORTED
            .iter()
            .map(|&(avcodec, ..)| PathBuf::from(format!("libavcodec.so.{avcodec}")))
            .collect(),
    };
    let mut attempts = Vec::new();
    for path in candidates {
        match open_candidate(&path, settings) {
            Ok(library) => return Ok(Arc::new(library)),
            Err(reason) => attempts.push(Attempt {
                path: path.display().to_string(),
                reason,
            }),
        }
    }
    Err(LoadError::Unavailable(attempts))
}

fn open(path: &Path) -> Result<RawLibrary, String> {
    // SAFETY: loading a shared object runs its initializers. These are
    // FFmpeg's (or whatever the user configured, as with any dlopen use).
    unsafe { RawLibrary::open(Some(path), RTLD_NOW | RTLD_LOCAL) }
        .map_err(|error| error.to_string())
}

fn read_version(lib: &RawLibrary, function: &str) -> Result<Version, String> {
    let version: unsafe extern "C" fn() -> c_uint = symbol(lib, function)?;
    // SAFETY: `avcodec_version` / `avutil_version` take no arguments.
    Ok(Version::from_packed(unsafe { version() }))
}

/// The directory of an explicitly configured libavcodec (none for a bare
/// soname the loader searches for).
fn directory_of(avcodec_path: &Path) -> Option<&Path> {
    avcodec_path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
}

/// The libavutil beside an explicitly configured libavcodec, opened ahead of
/// it: the loader then resolves libavcodec's dependency on it by soname even
/// when that directory is not on its search path (a private build under
/// `/opt/ffmpeg` has no RPATH).
fn open_siblings(avcodec_path: &Path) -> Vec<RawLibrary> {
    let Some(directory) = directory_of(avcodec_path) else {
        return Vec::new();
    };
    SUPPORTED
        .iter()
        .filter_map(|&(_, avutil, _)| open(&directory.join(format!("libavutil.so.{avutil}"))).ok())
        .collect()
}

/// The libavutil to pair with a libavcodec: the one beside an explicitly
/// configured libavcodec, else whatever the loader finds by soname.
fn open_avutil(avcodec_path: &Path, major: u32) -> Result<RawLibrary, String> {
    let soname = format!("libavutil.so.{major}");
    if let Some(directory) = directory_of(avcodec_path) {
        let sibling = directory.join(&soname);
        if sibling.exists() {
            return open(&sibling);
        }
    }
    open(Path::new(&soname))
}

fn open_candidate(path: &Path, settings: &Settings) -> Result<Library, String> {
    let _siblings = open_siblings(path);
    let avcodec = open(path)?;
    let avcodec_version = read_version(&avcodec, "avcodec_version\0")?;
    if !avcodec_version.is_ffmpeg() {
        return Err(format!(
            "libavcodec {avcodec_version} is not FFmpeg (Libav?)"
        ));
    }
    let &(_, avutil_major, layout) = SUPPORTED
        .iter()
        .find(|&&(major, ..)| major == avcodec_version.major)
        .ok_or_else(|| {
            format!(
                "libavcodec {avcodec_version} is outside the supported range {} to {}",
                SUPPORTED[SUPPORTED.len() - 1].0,
                SUPPORTED[0].0
            )
        })?;

    let avutil = open_avutil(path, avutil_major)?;
    let avutil_version = read_version(&avutil, "avutil_version\0")?;
    if avutil_version.major != avutil_major || !avutil_version.is_ffmpeg() {
        return Err(format!(
            "libavutil {avutil_version} does not belong to libavcodec {avcodec_version}"
        ));
    }

    let libs = Libs { avcodec, avutil };
    let api = Api::resolve(&libs)?;
    check_frame_layout(&api, layout, avutil_version)?;
    if !settings.keep_logging {
        // SAFETY: a plain setter.
        unsafe { (api.av_log_set_level)(AV_LOG_QUIET) };
    }
    Ok(Library {
        formats: FormatTables::resolve(&api),
        codec_ids: CodecId::ALL.map(|codec| {
            // SAFETY: a lookup by NUL-terminated name; a non-null descriptor
            // is static and starts with the codec's id.
            let descriptor =
                unsafe { (api.avcodec_descriptor_get_by_name)(codec.descriptor_name().as_ptr()) };
            if descriptor.is_null() {
                0
            } else {
                unsafe { (*descriptor).id }
            }
        }),
        api,
        layout,
        avcodec_version,
        avutil_version,
        _libs: ManuallyDrop::new(libs),
    })
}

/// Rejects a libavutil whose `AVFrame` does not look like the layout chosen
/// for its version. `av_frame_alloc` initializes the members read here to
/// values that are the same in every supported release (libavutil/frame.c,
/// `get_frame_defaults`), so wrong offsets show up before any frame is read.
fn check_frame_layout(api: &Api, layout: FrameLayout, version: Version) -> Result<(), String> {
    // SAFETY: the frame is allocated, inspected and freed right here.
    let fresh = unsafe {
        let mut frame = (api.av_frame_alloc)();
        if frame.is_null() {
            return Err("av_frame_alloc failed".to_owned());
        }
        let fields = layout.snapshot(frame);
        let fresh = std::ptr::addr_eq(fields.extended_data, frame) // points at data[]
            && fields.format == -1
            && fields.pts == AV_NOPTS_VALUE
            && fields.sample_aspect_ratio == (0, 1)
            && fields.color_range == AVCOL_RANGE_UNSPECIFIED
            && [fields.color_primaries, fields.color_trc, fields.colorspace] == [AVCOL_UNSPECIFIED; 3];
        (api.av_frame_free)(&mut frame);
        fresh
    };
    if fresh {
        Ok(())
    } else {
        Err(format!(
            "the AVFrame of libavutil {version} does not have the expected layout"
        ))
    }
}
