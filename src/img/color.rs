//! Colour management for tagged images.
//!
//! CSS Color 4 #tagged-images (local csswg-drafts snapshot 81c27f686901):
//! an image explicitly assigned a color profile by its format, "if the color
//! profile or other identifying information is valid, must be treated as
//! being in the specified color space"; with an invalid profile it is an
//! untagged image, which #untagged treats as sRGB. TRust composites and
//! presents in sRGB, so every tagged image is converted to sRGB when it is
//! decoded and the rest of the pipeline keeps treating samples as sRGB.
//!
//! The tags come from the formats: an embedded ICC profile (JPEG APP2, PNG
//! iCCP, WebP ICCP, GIF `ICCRGBG1` application extension, AVIF/HEIF `colr`
//! `prof`/`rICC`), ITU-T H.273 code points (PNG 3 cICP, AVIF `colr` nclx or
//! the AV1 sequence header) and PNG's gAMA/cHRM. ICC and SDR code points are
//! converted by moxcms (a pure-Rust CMS). PQ and HLG (ITU-R BT.2100) have no
//! SDR meaning by themselves: they are decoded to display light here, with
//! HDR reference white (203 cd/m², ITU-R BT.2408) at SDR white, and highlights
//! above a knee rolled off towards the content's peak before conversion to
//! sRGB primaries and the sRGB transfer function. CSS Color 4 does not define
//! that tone mapping; it is TRust's choice, kept deterministic.
//!
//! Work is bounded: profiles over `MAX_PROFILE_BYTES` (and CLUTs and curves
//! over moxcms's parsing limits) are untagged, a tag whose conversion is the
//! identity (the ubiquitous "sRGB IEC61966-2.1" profile, an sRGB chunk) costs
//! no per-pixel work, and prepared conversions are cached by tag.

use std::sync::{Arc, Mutex, OnceLock};

use image::DynamicImage;
use moxcms::{
    CicpColorPrimaries, CicpProfile, ColorPrimaries, ColorProfile, DataColorSpace, Layout,
    MatrixCoefficients, ParsingOptions, ToneReprCurve, TransferCharacteristics, TransformOptions,
};

/// Largest embedded ICC profile honoured; larger ones are untagged.
pub(crate) const MAX_PROFILE_BYTES: usize = 4 << 20;
/// Prepared conversions kept for reuse across images.
const CACHE_ENTRIES: usize = 16;
/// ITU-R BT.2408: HDR reference white, in cd/m², shown at SDR white.
const HDR_REFERENCE_WHITE: f32 = 203.0;
/// Linear SDR level below which HDR content is not compressed.
const TONE_KNEE: f32 = 0.8;

/// How an image's samples are tagged by its format.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tag {
    /// An embedded ICC profile.
    Icc(Arc<[u8]>),
    /// ITU-T H.273 ColourPrimaries and TransferCharacteristics of RGB
    /// samples, with VideoFullRangeFlag. `peak` is the content's peak
    /// luminance in cd/m² when the image declares it (HDR only).
    Cicp {
        primaries: u8,
        transfer: u8,
        full_range: bool,
        peak: Option<f32>,
    },
    /// PNG gAMA (the image gamma, an encoding exponent) and cHRM (white
    /// point then red, green and blue x/y), either of which may be absent.
    Gamma {
        gamma: Option<f32>,
        chromaticities: Option<[f32; 8]>,
    },
}

/// A prepared conversion of tagged samples to sRGB.
pub(crate) struct Conversion {
    kind: Kind,
}

enum Kind {
    Cms(Box<Cms>),
    Hdr(Hdr),
}

/// The tag of a PNG datastream (PNG 3 §4.3 "Color chunk priority"): cICP,
/// then iCCP, then sRGB, then cHRM and gAMA, each used only when no chunk
/// of a lower priority number is understood.
pub(crate) fn png_tag(bytes: &[u8]) -> Option<Tag> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_limits(png::Limits {
        bytes: MAX_PROFILE_BYTES * 2,
    });
    let reader = decoder.read_info().ok()?;
    let info = reader.info();
    if let Some(cicp) = info.coding_independent_code_points
        // §11.3.2.6: RGB is the only colour model, so MatrixCoefficients
        // must be 0; other values are not understood.
        && cicp.matrix_coefficients == 0
    {
        let peak = info
            .content_light_level
            .map(|level| level.max_content_light_level as f32 / 10_000.0)
            .filter(|peak| *peak > 0.0)
            .or_else(|| {
                info.mastering_display_color_volume
                    .map(|volume| volume.max_luminance as f32 / 10_000.0)
                    .filter(|peak| *peak > 0.0)
            });
        let tag = Tag::Cicp {
            primaries: cicp.color_primaries,
            transfer: cicp.transfer_function,
            full_range: cicp.is_video_full_range_image,
            peak,
        };
        if understood(&tag) {
            return Some(tag);
        }
    }
    if let Some(profile) = &info.icc_profile
        && profile.len() <= MAX_PROFILE_BYTES
    {
        return Some(Tag::Icc(Arc::from(profile.as_ref())));
    }
    if info.srgb.is_some() {
        return None;
    }
    let gamma = info
        .gama_chunk
        .map(|gamma| gamma.into_value())
        .filter(|gamma| gamma.is_finite() && *gamma > 0.0);
    let chromaticities = info.chrm_chunk.map(|chrm| {
        [
            chrm.white.0.into_value(),
            chrm.white.1.into_value(),
            chrm.red.0.into_value(),
            chrm.red.1.into_value(),
            chrm.green.0.into_value(),
            chrm.green.1.into_value(),
            chrm.blue.0.into_value(),
            chrm.blue.1.into_value(),
        ]
    });
    (gamma.is_some() || chromaticities.is_some()).then_some(Tag::Gamma {
        gamma,
        chromaticities,
    })
}

/// An ICC profile tag, untagged when absent or over the size limit.
pub(crate) fn icc_tag(profile: Option<Vec<u8>>) -> Option<Tag> {
    profile
        .filter(|profile| !profile.is_empty() && profile.len() <= MAX_PROFILE_BYTES)
        .map(|profile| Tag::Icc(Arc::from(profile)))
}

/// Whether TRust can convert samples with this tag.
fn understood(tag: &Tag) -> bool {
    match tag {
        Tag::Cicp {
            primaries,
            transfer,
            ..
        } => {
            h273_primaries(*primaries).is_some()
                && (matches!(*transfer, 2 | 16 | 18)
                    || TransferCharacteristics::try_from(*transfer).is_ok_and(|transfer| {
                        !matches!(
                            transfer,
                            TransferCharacteristics::Reserved
                                | TransferCharacteristics::Unspecified
                        )
                    }))
        }
        _ => true,
    }
}

/// The prepared conversion for a tag, or `None` when its samples are
/// already sRGB or the tag is invalid (both are then presented as decoded).
pub(crate) fn conversion(tag: &Tag) -> Option<Arc<Conversion>> {
    static CACHE: Mutex<Vec<(Tag, Option<Arc<Conversion>>)>> = Mutex::new(Vec::new());
    {
        let mut cache = CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = cache.iter().position(|(cached, _)| cached == tag) {
            let entry = cache.remove(index);
            let result = entry.1.clone();
            cache.push(entry);
            return result;
        }
    }
    // A malformed profile makes moxcms fail, not panic, but its arithmetic
    // on hostile curves is contained like the image decoders' panics.
    let prepared = std::panic::catch_unwind(|| prepare(tag))
        .ok()
        .flatten()
        .map(Arc::new);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.len() >= CACHE_ENTRIES {
        cache.remove(0);
    }
    cache.push((tag.clone(), prepared.clone()));
    prepared
}

fn prepare(tag: &Tag) -> Option<Conversion> {
    match tag {
        Tag::Icc(bytes) => {
            let profile = ColorProfile::new_from_slice_with_options(
                bytes,
                ParsingOptions {
                    max_profile_size: MAX_PROFILE_BYTES,
                    max_allowed_clut_size: 4_000_000,
                    max_allowed_trc_size: 40_000,
                },
            )
            .ok()?;
            // ICC v4.4 HDR profiles identify PQ or HLG with a cicp tag; their
            // curves are normalized to the format's peak, not SDR white.
            if let Some(cicp) = profile.cicp
                && matches!(
                    cicp.transfer_characteristics,
                    TransferCharacteristics::Smpte2084 | TransferCharacteristics::Hlg
                )
            {
                return Hdr::new(
                    cicp.color_primaries as u8,
                    cicp.transfer_characteristics as u8,
                    true,
                    None,
                )
                .map(|hdr| Conversion {
                    kind: Kind::Hdr(hdr),
                });
            }
            Cms::new(profile).map(|cms| Conversion {
                kind: Kind::Cms(Box::new(cms)),
            })
        }
        Tag::Cicp {
            primaries,
            transfer,
            full_range,
            peak,
        } => {
            if !understood(tag) {
                return None;
            }
            if matches!(transfer, 16 | 18) {
                return Hdr::new(*primaries, *transfer, *full_range, *peak).map(|hdr| Conversion {
                    kind: Kind::Hdr(hdr),
                });
            }
            // H.273: "unspecified" leaves the choice to the application,
            // which for an image is sRGB (CSS Color 4 #untagged).
            let primaries = match primaries {
                2 => 1,
                other => *other,
            };
            let transfer = match transfer {
                2 => 13,
                other => *other,
            };
            if primaries == 1 && transfer == 13 && *full_range {
                return None;
            }
            let mut profile = ColorProfile::new_from_cicp(CicpProfile {
                color_primaries: CicpColorPrimaries::try_from(primaries).ok()?,
                transfer_characteristics: TransferCharacteristics::try_from(transfer).ok()?,
                matrix_coefficients: MatrixCoefficients::Identity,
                full_range: true,
            });
            if !profile.is_matrix_shaper() {
                return None;
            }
            profile.cicp = None;
            let mut cms = Cms::new_with_identity(profile, *full_range)?;
            cms.narrow_range = !*full_range;
            Some(Conversion {
                kind: Kind::Cms(Box::new(cms)),
            })
        }
        Tag::Gamma {
            gamma,
            chromaticities,
        } => {
            let mut profile = ColorProfile::new_srgb();
            profile.cicp = None;
            if let Some([wx, wy, rx, ry, gx, gy, bx, by]) = *chromaticities {
                if [wx, wy, rx, ry, gx, gy, bx, by]
                    .iter()
                    .any(|value| !value.is_finite())
                    || wy <= 0.0
                    || ry <= 0.0
                    || gy <= 0.0
                    || by <= 0.0
                {
                    return None;
                }
                profile.update_rgb_colorimetry(
                    moxcms::Chromaticity::new(wx, wy).to_xyyb(),
                    ColorPrimaries {
                        red: moxcms::Chromaticity::new(rx, ry),
                        green: moxcms::Chromaticity::new(gx, gy),
                        blue: moxcms::Chromaticity::new(bx, by),
                    },
                );
            }
            if let Some(gamma) = gamma {
                // PNG 3 §11.3.3.3: samples are intensity^gamma, so the
                // decoding exponent is its reciprocal.
                let curve = ToneReprCurve::Parametric(vec![1.0 / gamma]);
                profile.red_trc = Some(curve.clone());
                profile.green_trc = Some(curve.clone());
                profile.blue_trc = Some(curve);
            }
            Cms::new(profile).map(|cms| Conversion {
                kind: Kind::Cms(Box::new(cms)),
            })
        }
    }
}

/// A moxcms conversion from a source profile to sRGB.
struct Cms {
    source: ColorProfile,
    gray: bool,
    /// Narrow-range (studio swing) samples are expanded first (H.273
    /// VideoFullRangeFlag 0).
    narrow_range: bool,
    rgba8: Arc<moxcms::Transform8BitExecutor>,
    rgba16: OnceLock<Option<Arc<moxcms::Transform16BitExecutor>>>,
    rgb_f32: OnceLock<Option<Arc<moxcms::TransformF32Executor>>>,
}

fn options() -> TransformOptions {
    TransformOptions::default()
}

impl Cms {
    fn new(source: ColorProfile) -> Option<Self> {
        Self::new_with_identity(source, true)
    }

    /// Prepare the 8-bit transform, then skip conversions that are the
    /// identity to within one 8-bit step (a full-range sRGB-equivalent
    /// profile); `full_range` false always converts.
    fn new_with_identity(source: ColorProfile, full_range: bool) -> Option<Self> {
        let gray = match source.color_space {
            DataColorSpace::Rgb => false,
            DataColorSpace::Gray => true,
            _ => return None,
        };
        let srgb = ColorProfile::new_srgb();
        let layout = if gray {
            Layout::GrayAlpha
        } else {
            Layout::Rgba
        };
        let rgba8 = source
            .create_transform_8bit(layout, &srgb, Layout::Rgba, options())
            .ok()?;
        let cms = Self {
            source,
            gray,
            narrow_range: false,
            rgba8,
            rgba16: OnceLock::new(),
            rgb_f32: OnceLock::new(),
        };
        (!full_range || !cms.is_identity()).then_some(cms)
    }

    fn is_identity(&self) -> bool {
        const LEVELS: [u8; 9] = [0, 32, 64, 96, 128, 160, 192, 224, 255];
        let (source, channels): (Vec<u8>, usize) = if self.gray {
            ((0..=255).flat_map(|v| [v, 255]).collect(), 2)
        } else {
            (
                LEVELS
                    .iter()
                    .flat_map(|&r| {
                        LEVELS
                            .iter()
                            .flat_map(move |&g| LEVELS.iter().map(move |&b| [r, g, b, 255]))
                    })
                    .flatten()
                    .collect(),
                4,
            )
        };
        let pixels = source.len() / channels;
        let mut converted = vec![0u8; pixels * 4];
        if self.rgba8.transform(&source, &mut converted).is_err() {
            return false;
        }
        source
            .chunks_exact(channels)
            .zip(converted.as_chunks::<4>().0)
            .all(|(input, output)| {
                (0..3).all(|channel| {
                    let expected = input[if self.gray { 0 } else { channel }];
                    expected.abs_diff(output[channel]) <= 1
                })
            })
    }

    fn rgba16(&self) -> Option<&Arc<moxcms::Transform16BitExecutor>> {
        self.rgba16
            .get_or_init(|| {
                let layout = if self.gray {
                    Layout::GrayAlpha
                } else {
                    Layout::Rgba
                };
                self.source
                    .create_transform_16bit(
                        layout,
                        &ColorProfile::new_srgb(),
                        Layout::Rgba,
                        options(),
                    )
                    .ok()
            })
            .as_ref()
    }

    fn rgb_f32(&self) -> Option<&Arc<moxcms::TransformF32Executor>> {
        self.rgb_f32
            .get_or_init(|| {
                let layout = if self.gray { Layout::Gray } else { Layout::Rgb };
                self.source
                    .create_transform_f32(layout, &ColorProfile::new_srgb(), Layout::Rgb, options())
                    .ok()
            })
            .as_ref()
    }
}

impl Conversion {
    /// Convert a decoded image to sRGB. The result keeps an 8-bit, 16-bit
    /// or floating-point representation as the source had.
    pub(crate) fn apply_image(&self, image: DynamicImage) -> DynamicImage {
        match &self.kind {
            Kind::Cms(cms) => cms.apply_image(image),
            Kind::Hdr(hdr) => hdr.apply_image(image),
        }
    }

    /// Convert straight-alpha RGBA8 pixels in place (animation frames).
    pub(crate) fn apply_rgba8(&self, rgba: &mut [u8], width: usize) {
        match &self.kind {
            Kind::Cms(cms) => cms.apply_rgba8(rgba, width),
            Kind::Hdr(hdr) => {
                for pixel in rgba.as_chunks_mut::<4>().0 {
                    let mut rgb = [pixel[0], pixel[1], pixel[2]].map(|v| f32::from(v) / 255.0);
                    hdr.convert(&mut rgb);
                    for (out, value) in pixel.iter_mut().zip(rgb) {
                        *out = unit_to_u8(value);
                    }
                }
            }
        }
    }

    /// Convert interleaved non-linear RGB samples in [0, 1] in place: one
    /// row of a picture whose samples were reconstructed at full precision.
    pub(crate) fn apply_rgb_f32(&self, rgb: &mut [f32]) {
        match &self.kind {
            Kind::Cms(cms) => {
                if cms.narrow_range {
                    for value in rgb.iter_mut() {
                        *value = ((*value * 255.0 - 16.0) / 219.0).clamp(0.0, 1.0);
                    }
                }
                let Some(transform) = cms.rgb_f32() else {
                    return;
                };
                if cms.gray {
                    let gray: Vec<f32> = rgb
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .map(|pixel| pixel[0])
                        .collect();
                    let _ = transform.transform(&gray, rgb);
                } else {
                    let source = rgb.to_vec();
                    let _ = transform.transform(&source, rgb);
                }
            }
            Kind::Hdr(hdr) => {
                for pixel in rgb.as_chunks_mut::<3>().0 {
                    hdr.convert(pixel);
                }
            }
        }
    }
}

fn unit_to_u8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn unit_to_u16(value: f32) -> u16 {
    (value.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16
}

impl Cms {
    fn expand_narrow8(&self, value: u8) -> u8 {
        if self.narrow_range {
            unit_to_u8((f32::from(value) - 16.0) / 219.0)
        } else {
            value
        }
    }

    fn expand_narrow16(&self, value: u16) -> u16 {
        if self.narrow_range {
            unit_to_u16((f32::from(value) - 4096.0) / 56064.0)
        } else {
            value
        }
    }

    fn apply_rgba8(&self, rgba: &mut [u8], width: usize) {
        let row_bytes = width.max(1) * 4;
        let mut source = vec![0u8; row_bytes];
        for row in rgba.chunks_mut(row_bytes) {
            let pixels = row.len() / 4;
            if self.gray {
                for (pixel, gray) in row
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(source.as_chunks_mut::<2>().0.iter_mut())
                {
                    *gray = [self.expand_narrow8(pixel[0]), pixel[3]];
                }
                let _ = self.rgba8.transform(&source[..pixels * 2], row);
            } else {
                for (value, out) in row.iter().zip(source.iter_mut()) {
                    *out = *value;
                }
                if self.narrow_range {
                    for pixel in source.as_chunks_mut::<4>().0 {
                        for channel in &mut pixel[..3] {
                            *channel = self.expand_narrow8(*channel);
                        }
                    }
                }
                let _ = self.rgba8.transform(&source[..row.len()], row);
            }
        }
    }

    fn apply_image(&self, image: DynamicImage) -> DynamicImage {
        use DynamicImage::*;
        let gray_source = matches!(
            image,
            ImageLuma8(_) | ImageLumaA8(_) | ImageLuma16(_) | ImageLumaA16(_)
        );
        if gray_source != self.gray {
            // An RGB profile on gray samples (or the reverse) is not a
            // valid tag for them: present the image untagged.
            return image;
        }
        match image {
            ImageLuma8(_) | ImageLumaA8(_) => {
                let gray = image.into_luma_alpha8();
                let (width, height) = gray.dimensions();
                let mut rgba = image::RgbaImage::new(width, height);
                let mut source = gray.into_raw();
                for value in source.as_chunks_mut::<2>().0 {
                    value[0] = self.expand_narrow8(value[0]);
                }
                if self.rgba8.transform(&source, &mut rgba).is_err() {
                    return ImageLumaA8(
                        image::GrayAlphaImage::from_raw(width, height, source).unwrap_or_default(),
                    );
                }
                ImageRgba8(rgba)
            }
            ImageLuma16(_) | ImageLumaA16(_) => {
                let gray = image.into_luma_alpha16();
                let (width, height) = gray.dimensions();
                let Some(transform) = self.rgba16() else {
                    return ImageLumaA16(gray);
                };
                let mut source = gray.into_raw();
                for value in source.as_chunks_mut::<2>().0 {
                    value[0] = self.expand_narrow16(value[0]);
                }
                let mut rgba = vec![0u16; width as usize * height as usize * 4];
                if transform.transform(&source, &mut rgba).is_err() {
                    return DynamicImage::ImageLumaA16(
                        image::ImageBuffer::from_raw(width, height, source).unwrap_or_default(),
                    );
                }
                ImageRgba16(image::ImageBuffer::from_raw(width, height, rgba).unwrap_or_default())
            }
            ImageRgb16(_) | ImageRgba16(_) => {
                let mut rgba = image.into_rgba16();
                let Some(transform) = self.rgba16() else {
                    return ImageRgba16(rgba);
                };
                let width = rgba.width() as usize;
                let mut source = vec![0u16; width * 4];
                for row in rgba.chunks_mut(width.max(1) * 4) {
                    source[..row.len()].copy_from_slice(row);
                    for pixel in source.as_chunks_mut::<4>().0 {
                        for channel in &mut pixel[..3] {
                            *channel = self.expand_narrow16(*channel);
                        }
                    }
                    let _ = transform.transform(&source[..row.len()], row);
                }
                ImageRgba16(rgba)
            }
            ImageRgb32F(_) | ImageRgba32F(_) => {
                let mut rgba = image.into_rgba32f();
                let Some(transform) = self.rgb_f32() else {
                    return ImageRgba32F(rgba);
                };
                let width = rgba.width() as usize;
                let mut rgb = vec![0f32; width * 3];
                let mut out = vec![0f32; width * 3];
                for row in rgba.chunks_mut(width.max(1) * 4) {
                    let pixels = row.len() / 4;
                    for (pixel, target) in row
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .zip(rgb.as_chunks_mut::<3>().0)
                    {
                        target.copy_from_slice(&pixel[..3]);
                    }
                    if transform
                        .transform(&rgb[..pixels * 3], &mut out[..pixels * 3])
                        .is_ok()
                    {
                        for (pixel, source) in row
                            .as_chunks_mut::<4>()
                            .0
                            .iter_mut()
                            .zip(out.as_chunks::<3>().0)
                        {
                            pixel[..3].copy_from_slice(source);
                        }
                    }
                }
                ImageRgba32F(rgba)
            }
            other => {
                let mut rgba = other.into_rgba8();
                let width = rgba.width() as usize;
                self.apply_rgba8(&mut rgba, width);
                ImageRgba8(rgba)
            }
        }
    }
}

/// ITU-R BT.2100 PQ or HLG samples, shown on an SDR sRGB display.
struct Hdr {
    hlg: bool,
    full_range: bool,
    /// Linear source RGB (display light relative to reference white) to
    /// linear sRGB, white points adapted (Bradford).
    to_srgb: [[f32; 3]; 3],
    /// The source primaries' luminance coefficients (the Y row of their
    /// RGB to XYZ matrix), for the HLG OOTF.
    luminance: [f32; 3],
    /// The content's peak relative to reference white.
    peak: f32,
    /// The signal's transfer function sampled over [0, 1]: PQ display
    /// light relative to reference white, or HLG scene light.
    signal: Box<[f32]>,
    /// The sRGB encoding sampled over the square root of linear light, so
    /// that the steep dark end keeps its precision.
    encode: Box<[f32]>,
}

/// Samples in each transfer-function table; with linear interpolation the
/// per-pixel work is a few multiplications instead of powers.
const TRANSFER_TABLE: usize = 4096;

fn sample_table(function: impl Fn(f32) -> f32) -> Box<[f32]> {
    (0..=TRANSFER_TABLE)
        .map(|index| function(index as f32 / TRANSFER_TABLE as f32))
        .collect()
}

fn lookup(table: &[f32], x: f32) -> f32 {
    let scaled = x.clamp(0.0, 1.0) * TRANSFER_TABLE as f32;
    let index = (scaled as usize).min(TRANSFER_TABLE - 1);
    let fraction = scaled - index as f32;
    table[index] + (table[index + 1] - table[index]) * fraction
}

impl Hdr {
    fn new(primaries: u8, transfer: u8, full_range: bool, peak: Option<f32>) -> Option<Self> {
        let primaries = if primaries == 2 { 1 } else { primaries };
        let source = h273_primaries(primaries)?;
        let srgb = h273_primaries(1)?;
        let source_to_xyz = rgb_to_xyz(source)?;
        let xyz_to_srgb = invert(rgb_to_xyz(srgb)?)?;
        let adapt = bradford([source[6], source[7]], [srgb[6], srgb[7]])?;
        let to_srgb = multiply(xyz_to_srgb, multiply(adapt, source_to_xyz));
        let hlg = transfer == 18;
        // ITU-R BT.2100: HLG's nominal display peak is 1000 cd/m²; PQ can
        // reach 10 000 cd/m² unless the image declares its own peak.
        let peak = peak.unwrap_or(if hlg { 1000.0 } else { 10_000.0 });
        Some(Self {
            hlg,
            full_range,
            to_srgb: to_srgb.map(|row| row.map(|v| v as f32)),
            luminance: source_to_xyz[1].map(|v| v as f32),
            peak: (peak / HDR_REFERENCE_WHITE).max(1.0),
            signal: if hlg {
                sample_table(hlg_inverse_oetf)
            } else {
                sample_table(|value| pq_eotf(value) * 10_000.0 / HDR_REFERENCE_WHITE)
            },
            encode: sample_table(|root| srgb_oetf(root * root)),
        })
    }

    fn convert(&self, rgb: &mut [f32; 3]) {
        if !self.full_range {
            for value in rgb.iter_mut() {
                *value = (*value * 255.0 - 16.0) / 219.0;
            }
        }
        // Display light in cd/m² (BT.2100 Table 4 PQ EOTF; HLG inverse OETF
        // then the OOTF for the nominal 1000 cd/m² display, gamma 1.2).
        let mut light = rgb.map(|value| lookup(&self.signal, value));
        if self.hlg {
            let luminance = light
                .iter()
                .zip(self.luminance)
                .map(|(value, weight)| value * weight)
                .sum::<f32>()
                .max(0.0);
            let gain = 1000.0 * luminance.powf(0.2) / HDR_REFERENCE_WHITE;
            for value in &mut light {
                *value *= gain;
            }
        }
        // Hue-preserving roll-off of the brightest component above the knee.
        let max = light[0].max(light[1]).max(light[2]);
        if max > TONE_KNEE {
            let range = (self.peak - TONE_KNEE).max(1e-3) / (1.0 - TONE_KNEE);
            let u = (max.min(self.peak) - TONE_KNEE) / (1.0 - TONE_KNEE);
            let compressed = u * (1.0 + u / (range * range)) / (1.0 + u);
            let mapped = TONE_KNEE + (1.0 - TONE_KNEE) * compressed;
            let scale = mapped / max;
            for value in &mut light {
                *value *= scale;
            }
        }
        let m = self.to_srgb;
        for (row, out) in m.iter().zip(rgb.iter_mut()) {
            let linear = row[0] * light[0] + row[1] * light[1] + row[2] * light[2];
            *out = lookup(&self.encode, linear.clamp(0.0, 1.0).sqrt());
        }
    }

    fn apply_image(&self, image: DynamicImage) -> DynamicImage {
        use DynamicImage::*;
        let output_16 = matches!(
            image,
            ImageLuma16(_) | ImageLumaA16(_) | ImageRgb16(_) | ImageRgba16(_)
        );
        let float = matches!(image, ImageRgb32F(_) | ImageRgba32F(_));
        let mut rgba = image.into_rgba32f();
        for pixel in rgba.as_chunks_mut::<4>().0 {
            let mut rgb = [pixel[0], pixel[1], pixel[2]];
            self.convert(&mut rgb);
            pixel[..3].copy_from_slice(&rgb);
        }
        let converted = ImageRgba32F(rgba);
        if float {
            converted
        } else if output_16 {
            ImageRgba16(converted.into_rgba16())
        } else {
            ImageRgba8(converted.into_rgba8())
        }
    }
}

/// SMPTE ST 2084 / ITU-R BT.2100 PQ EOTF, normalized to 10 000 cd/m².
fn pq_eotf(value: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let power = value.powf(1.0 / M2);
    ((power - C1).max(0.0) / (C2 - C3 * power)).powf(1.0 / M1)
}

/// ITU-R BT.2100 HLG inverse OETF: scene light in [0, 1].
fn hlg_inverse_oetf(value: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 1.0 - 4.0 * A;
    const C: f32 = 0.559_910_7;
    if value <= 0.5 {
        value * value / 3.0
    } else {
        (((value - C) / A).exp() + B) / 12.0
    }
}

/// IEC 61966-2-1 sRGB encoding.
fn srgb_oetf(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

/// ITU-T H.273 Table 2 ColourPrimaries: red, green, blue and white x/y as
/// `[xr, yr, xg, yg, xb, yb, xw, yw]`.
pub(crate) fn h273_primaries(code: u8) -> Option<[f64; 8]> {
    Some(match code {
        1 | 2 => [0.64, 0.33, 0.30, 0.60, 0.15, 0.06, 0.3127, 0.3290],
        4 => [0.67, 0.33, 0.21, 0.71, 0.14, 0.08, 0.310, 0.316],
        5 => [0.64, 0.33, 0.29, 0.60, 0.15, 0.06, 0.3127, 0.3290],
        6 | 7 => [0.630, 0.340, 0.310, 0.595, 0.155, 0.070, 0.3127, 0.3290],
        8 => [0.681, 0.319, 0.243, 0.692, 0.145, 0.049, 0.310, 0.316],
        9 => [0.708, 0.292, 0.170, 0.797, 0.131, 0.046, 0.3127, 0.3290],
        10 => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0 / 3.0, 1.0 / 3.0],
        11 => [0.680, 0.320, 0.265, 0.690, 0.150, 0.060, 0.314, 0.351],
        12 => [0.680, 0.320, 0.265, 0.690, 0.150, 0.060, 0.3127, 0.3290],
        22 => [0.630, 0.340, 0.295, 0.605, 0.155, 0.077, 0.3127, 0.3290],
        _ => return None,
    })
}

type Matrix = [[f64; 3]; 3];

/// SMPTE RP 177: the RGB to XYZ matrix of a set of primaries and white.
fn rgb_to_xyz(p: [f64; 8]) -> Option<Matrix> {
    let column = |x: f64, y: f64| (y > 0.0).then(|| [x / y, 1.0, (1.0 - x - y) / y]);
    let (r, g, b) = (
        column(p[0], p[1])?,
        column(p[2], p[3])?,
        column(p[4], p[5])?,
    );
    let primaries = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
    let white = column(p[6], p[7])?;
    let inverse = invert(primaries)?;
    let s = apply(inverse, white);
    Some(primaries.map(|row| [row[0] * s[0], row[1] * s[1], row[2] * s[2]]))
}

/// The Bradford chromatic adaptation from one white point's x/y to another.
fn bradford(from: [f64; 2], to: [f64; 2]) -> Option<Matrix> {
    const BRADFORD: Matrix = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    let xyz = |[x, y]: [f64; 2]| (y > 0.0).then(|| [x / y, 1.0, (1.0 - x - y) / y]);
    let (source, target) = (apply(BRADFORD, xyz(from)?), apply(BRADFORD, xyz(to)?));
    if source.iter().any(|value| value.abs() < 1e-12) {
        return None;
    }
    let scale = [
        [target[0] / source[0], 0.0, 0.0],
        [0.0, target[1] / source[1], 0.0],
        [0.0, 0.0, target[2] / source[2]],
    ];
    Some(multiply(invert(BRADFORD)?, multiply(scale, BRADFORD)))
}

fn apply(m: Matrix, v: [f64; 3]) -> [f64; 3] {
    m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
}

fn multiply(a: Matrix, b: Matrix) -> Matrix {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

fn invert(m: Matrix) -> Option<Matrix> {
    let determinant = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if determinant.abs() < 1e-12 || !determinant.is_finite() {
        return None;
    }
    let cofactor =
        |r0: usize, r1: usize, c0: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    Some([
        [
            cofactor(1, 2, 1, 2) / determinant,
            -cofactor(0, 2, 1, 2) / determinant,
            cofactor(0, 1, 1, 2) / determinant,
        ],
        [
            -cofactor(1, 2, 0, 2) / determinant,
            cofactor(0, 2, 0, 2) / determinant,
            -cofactor(0, 1, 0, 2) / determinant,
        ],
        [
            cofactor(1, 2, 0, 1) / determinant,
            -cofactor(0, 2, 0, 1) / determinant,
            cofactor(0, 1, 0, 1) / determinant,
        ],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display_p3_icc() -> Vec<u8> {
        ColorProfile::new_display_p3().encode().unwrap()
    }

    #[test]
    fn srgb_equivalent_tags_need_no_conversion() {
        let srgb = ColorProfile::new_srgb().encode().unwrap();
        assert!(conversion(&Tag::Icc(Arc::from(srgb))).is_none());
        for (primaries, transfer) in [(1, 13), (2, 2), (1, 2)] {
            let tag = Tag::Cicp {
                primaries,
                transfer,
                full_range: true,
                peak: None,
            };
            assert!(conversion(&tag).is_none(), "{primaries}/{transfer}");
        }
        // CSS Color 4 #tagged-images: an invalid profile leaves the image
        // untagged.
        assert!(conversion(&Tag::Icc(Arc::from(&b"not a profile"[..]))).is_none());
        let reserved = Tag::Cicp {
            primaries: 3,
            transfer: 13,
            full_range: true,
            peak: None,
        };
        assert!(conversion(&reserved).is_none());
    }

    #[test]
    fn display_p3_primaries_convert_into_srgb() {
        // Display P3 red lies outside sRGB: it saturates sRGB red and keeps
        // green and blue at zero, while P3 white stays sRGB white.
        let p3 = conversion(&Tag::Icc(Arc::from(display_p3_icc()))).expect("P3 converts");
        let mut pixels = [255, 0, 0, 255, 255, 255, 255, 128, 0, 255, 0, 255];
        p3.apply_rgba8(&mut pixels, 3);
        assert!(
            pixels[0] == 255 && pixels[1] < 10 && pixels[2] < 10,
            "{pixels:?}"
        );
        assert!(pixels[4..7].iter().all(|v| *v >= 254), "{pixels:?}");
        assert_eq!(pixels[7], 128, "alpha is untouched");
        assert!(pixels[9] == 255 && pixels[8] < 10, "{pixels:?}");
        // The same through H.273 code points (PNG cICP 12/13/0/1).
        let cicp = conversion(&Tag::Cicp {
            primaries: 12,
            transfer: 13,
            full_range: true,
            peak: None,
        })
        .expect("Display P3 code points convert");
        let mut mid = [128u8, 64, 32, 255];
        cicp.apply_rgba8(&mut mid, 1);
        let mut via_icc = [128u8, 64, 32, 255];
        p3.apply_rgba8(&mut via_icc, 1);
        for (a, b) in mid.iter().zip(via_icc) {
            assert!(a.abs_diff(b) <= 2, "{mid:?} {via_icc:?}");
        }
    }

    #[test]
    fn png_gamma_decodes_with_its_reciprocal_exponent() {
        // PNG 3 gAMA: samples are intensity^gamma. A linear image (gamma
        // 1.0) stores mid-grey intensity 0.5 as 128, which sRGB encodes
        // as about 188.
        let linear = conversion(&Tag::Gamma {
            gamma: Some(1.0),
            chromaticities: None,
        })
        .expect("linear gamma converts");
        let mut pixel = [128u8, 128, 128, 255];
        linear.apply_rgba8(&mut pixel, 1);
        assert!(pixel[0].abs_diff(188) <= 2, "{pixel:?}");
        // gAMA 1/2.2 is close to sRGB but not the identity at the dark end.
        let gamma22 = conversion(&Tag::Gamma {
            gamma: Some(1.0 / 2.2),
            chromaticities: None,
        });
        if let Some(gamma22) = gamma22 {
            let mut pixel = [128u8, 128, 128, 255];
            gamma22.apply_rgba8(&mut pixel, 1);
            assert!(pixel[0].abs_diff(128) <= 3, "{pixel:?}");
        }
    }

    #[test]
    fn pq_and_hlg_tone_map_reference_white_near_sdr_white() {
        // BT.2408: 203 cd/m² is HDR reference white. Its PQ code value is
        // 0.58; HLG puts it at 75% signal.
        let pq = Hdr::new(9, 16, true, None).unwrap();
        let mut white = [0.580_689; 3];
        pq.convert(&mut white);
        assert!(white.iter().all(|v| (0.9..=1.0).contains(v)), "{white:?}");
        let mut black = [0.0; 3];
        pq.convert(&mut black);
        assert!(black.iter().all(|v| *v < 0.01), "{black:?}");
        // 10 000 cd/m² saturates without exceeding SDR white.
        let mut peak = [1.0; 3];
        pq.convert(&mut peak);
        assert!(peak.iter().all(|v| (0.99..=1.0).contains(v)), "{peak:?}");
        let hlg = Hdr::new(9, 18, true, None).unwrap();
        let mut white = [0.75; 3];
        hlg.convert(&mut white);
        assert!(white.iter().all(|v| (0.85..=1.0).contains(v)), "{white:?}");
        // Darker PQ content stays below reference white, monotonic.
        let mut levels = [0.2f32, 0.4, 0.5, 0.58, 0.7, 0.9].map(|v| {
            let mut rgb = [v; 3];
            pq.convert(&mut rgb);
            rgb[0]
        });
        let sorted = levels;
        levels.sort_by(f32::total_cmp);
        assert_eq!(levels, sorted);
    }

    #[test]
    fn primaries_matrices_round_trip() {
        let srgb = rgb_to_xyz(h273_primaries(1).unwrap()).unwrap();
        // sRGB's luminance row (IEC 61966-2-1).
        for (value, expected) in srgb[1].iter().zip([0.2126, 0.7152, 0.0722]) {
            assert!((value - expected).abs() < 1e-3, "{:?}", srgb[1]);
        }
        let identity = multiply(srgb, invert(srgb).unwrap());
        for (i, row) in identity.iter().enumerate() {
            for (j, value) in row.iter().enumerate() {
                assert!((value - f64::from(i == j)).abs() < 1e-9);
            }
        }
        let same = bradford([0.3127, 0.329], [0.3127, 0.329]).unwrap();
        assert!((same[0][0] - 1.0).abs() < 1e-9 && same[0][1].abs() < 1e-9);
    }
}
