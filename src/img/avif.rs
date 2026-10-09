//! AVIF (AV1 Image File Format) still-image decoding.
//!
//! An AVIF file is a HEIF/MIAF container whose image items carry AV1 data
//! (AVIF, local AOMediaCodec/av1-avif snapshot bf4c18d1). Decoding is three
//! steps: `mp4parse` (TRust's patched copy, see vendor/mp4parse-0.17.0)
//! parses the container and returns item data and properties; [`crate::av1`]
//! decodes each AV1 item to YUV planes; [`super::yuv`] converts them into a
//! straight-alpha RGBA canvas. Then the primary item's transformative
//! properties are applied in the order MIAF requires: clean aperture
//! (`clap`), rotation (`irot`), mirroring (`imir`).
//!
//! Supported: `av01` and `grid` primary items (AVIF §4.2.1, HEIF §6.6.2.3),
//! an alpha auxiliary item (AVIF §4.1, `auxl` + `auxC` alpha URN) of either
//! kind, premultiplied alpha (`prem`), 8/10/12-bit monochrome and 4:2:0, 4:2:2
//! and 4:4:4 pictures, `colr` nclx with the AV1 sequence header as fallback,
//! film grain, and image sequences (AVIF §3), whose primary item or, without
//! one, first sample is decoded as a still image.
//!
//! A tagged picture is converted to sRGB while it is reconstructed
//! (`super::color`): an ICC `colr` (`prof`/`rICC`) takes precedence, then
//! the nclx colour primaries and transfer characteristics (PQ and HLG
//! tone-mapped to SDR), then the AV1 sequence header's.
//!
//! An image sequence of two or more samples plays as an animation
//! ([`Sequence`]): the colour track's samples in decoding order, each an AV1
//! temporal unit (AV1-ISOBMFF), shown for its time-to-sample duration, with
//! an alpha auxiliary track's samples as alpha.
//!
//! Not supported: layered images beyond the default operating point
//! (`a1op`/`lsel`), gain maps (`tmap`) and sample transforms (`sato`), which
//! fall back to their base image when the file offers one as the primary
//! item; and for sequences, transformative properties and premultiplied
//! (`prem`) alpha tracks, which mp4parse does not report for tracks.

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use image::RgbaImage;
use mp4parse::{AvifContext, ImageMirror, ImageRotation, ParseStrictness};

use super::color::{self, Conversion, Tag};
use super::yuv::{self, Canvas, Cicp};
use crate::av1::{self, Decoder, DecoderConfig, Frame};

pub(crate) const MIME: &str = "image/avif";

/// Resource limits for one decode. `max_bytes` bounds the RGBA canvas.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub max_dimension: u32,
    pub max_bytes: u64,
}

impl Limits {
    fn check(self, width: u32, height: u32) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Err(String::from("AVIF image has no pixels"));
        }
        if width > self.max_dimension || height > self.max_dimension {
            return Err(format!(
                "image dimensions {width}x{height} exceed {}px cap",
                self.max_dimension
            ));
        }
        if u64::from(width) * u64::from(height) * 4 > self.max_bytes {
            return Err(format!(
                "image dimensions {width}x{height} exceed the decode budget"
            ));
        }
        Ok(())
    }

    fn max_pixels(self) -> u32 {
        u32::try_from(self.max_bytes / 4).unwrap_or(u32::MAX)
    }
}

/// Whether `bytes` begins with an ISOBMFF FileTypeBox listing an AVIF brand.
///
/// WHATWG MIME Sniffing (local snapshot 39aa5351) has no AVIF pattern yet. AVIF
/// §6 requires `avif` (image items) or `avis` (image sequences) among the
/// FileTypeBox brands; this reads the box like MIME Sniffing's "matches the
/// signature for MP4" (whole `ftyp` box present, size a multiple of four,
/// major brand at offset 8, compatible brands from offset 16), with those
/// brands instead of `mp4`.
pub(crate) fn sniff(bytes: &[u8]) -> bool {
    if bytes.len() < 16 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if size < 16 || !size.is_multiple_of(4) || bytes.len() < size {
        return false;
    }
    let is_avif_brand = |brand: &[u8]| brand == b"avif" || brand == b"avis";
    is_avif_brand(&bytes[8..12])
        || bytes[16..size]
            .as_chunks::<4>()
            .0
            .iter()
            .any(|brand| is_avif_brand(brand))
}

/// Decode the primary image of an AVIF file to straight-alpha RGBA.
pub(crate) fn decode(bytes: &[u8], limits: Limits) -> Result<RgbaImage, String> {
    // The container parser and rav1d are memory safe, but report some states
    // they consider impossible with panics (assertions). Contain those here so
    // a malformed file fails like any other undecodable image.
    std::panic::catch_unwind(|| {
        let context = mp4parse::read_avif(&mut Cursor::new(bytes), ParseStrictness::Normal)
            .map_err(|error| format!("AVIF container: {error}"))?;
        decode_context(&context, bytes, limits)
    })
    .unwrap_or_else(|_| Err(String::from("AVIF decoder failed")))
}

fn decode_context(
    context: &AvifContext,
    bytes: &[u8],
    limits: Limits,
) -> Result<RgbaImage, String> {
    let colour = Item::primary(context, bytes)?;
    let alpha = Item::alpha(context)?;
    let nclx = match context.nclx_colour_information() {
        Some(Ok(nclx)) => Some(Nclx::of(nclx)),
        Some(Err(error)) => return Err(format!("AVIF colour: {error}")),
        None => None,
    };
    let icc = match context.icc_colour_information() {
        Some(Ok(profile)) => color::icc_tag(Some(profile.to_vec())),
        _ => None,
    };

    let mut image = colour.decode(limits, |frame| picture_colour(nclx, icc.as_ref(), frame))?;
    if let Some(alpha) = alpha {
        alpha.decode_alpha(&mut image)?;
        if context.premultiplied_alpha {
            yuv::unpremultiply(&mut image);
        }
    }
    apply_transforms(context, image)
}

/// A primary or alpha image: one coded AV1 item, or a grid of them.
enum Item<'a> {
    Coded {
        data: &'a [u8],
        /// The item's 'ispe', when the container gives it.
        size: Option<(u32, u32)>,
    },
    Grid(mp4parse::AvifGrid<'a>),
}

impl<'a> Item<'a> {
    fn primary(context: &'a AvifContext, bytes: &'a [u8]) -> Result<Self, String> {
        if let Some(grid) = context.primary_item_grid() {
            return grid
                .map(Self::Grid)
                .map_err(|error| format!("AVIF grid: {error}"));
        }
        let size = context
            .spatial_extents()
            .map_err(|error| format!("AVIF ispe: {error}"))?
            .map(|ispe| (ispe.width(), ispe.height()));
        if let Some(data) = context.primary_item_coded_data() {
            return Ok(Self::Coded { data, size });
        }
        // An image sequence without a primary item (AVIF §3, MIAF image
        // sequences): present its first sample.
        first_sequence_sample(context, bytes)
            .map(|data| Self::Coded { data, size: None })
            .ok_or_else(|| String::from("AVIF file has no displayable image"))
    }

    fn alpha(context: &'a AvifContext) -> Result<Option<Self>, String> {
        if let Some(grid) = context.alpha_item_grid() {
            return grid
                .map(|grid| Some(Self::Grid(grid)))
                .map_err(|error| format!("AVIF alpha grid: {error}"));
        }
        Ok(context
            .alpha_item_coded_data()
            .map(|data| Self::Coded { data, size: None }))
    }

    /// The output size the container declares before any decoding.
    fn declared_size(&self) -> Option<(u32, u32)> {
        match self {
            Self::Coded { size, .. } => *size,
            Self::Grid(grid) => Some((grid.output_width, grid.output_height)),
        }
    }

    /// A decoder whose frames may not exceed `max_pixels`. A conforming
    /// item's frame is exactly its 'ispe' (AVIF §2.2.2), which tightens the
    /// bound before rav1d allocates anything.
    fn decoder(&self, max_pixels: u32) -> Result<Decoder, String> {
        let declared = match self {
            Self::Coded {
                size: Some((width, height)),
                ..
            }
            | Self::Grid(mp4parse::AvifGrid {
                tile_size: Some((width, height)),
                ..
            }) => width.saturating_mul(*height),
            _ => max_pixels,
        };
        Decoder::new(&DecoderConfig {
            frame_size_limit: declared.clamp(1, max_pixels.max(1)),
            ..DecoderConfig::default()
        })
        .map_err(|error| error.to_string())
    }

    /// Decode the colour image into a new canvas.
    fn decode(
        &self,
        limits: Limits,
        colour: impl Fn(&Frame) -> Colour,
    ) -> Result<RgbaImage, String> {
        if let Some((width, height)) = self.declared_size() {
            limits.check(width, height)?;
        }
        let mut decoder = self.decoder(limits.max_pixels())?;
        match self {
            Self::Coded { data, .. } => {
                let frame = decode_frame(&mut decoder, data)?;
                limits.check(frame.width, frame.height)?;
                let mut image = RgbaImage::new(frame.width, frame.height);
                let (cicp, conversion) = colour(&frame);
                yuv::write_rgb(
                    &frame,
                    cicp,
                    conversion.as_deref(),
                    &mut canvas(&mut image),
                    0,
                    0,
                );
                Ok(image)
            }
            Self::Grid(grid) => {
                let mut image = RgbaImage::new(grid.output_width, grid.output_height);
                let mut tiles_colour = None;
                for_each_tile(grid, &mut decoder, |frame, x, y| {
                    let (cicp, conversion) = tiles_colour.get_or_insert_with(|| colour(frame));
                    yuv::write_rgb(
                        frame,
                        *cicp,
                        conversion.as_deref(),
                        &mut canvas(&mut image),
                        x,
                        y,
                    );
                })?;
                Ok(image)
            }
        }
    }

    /// Decode the alpha image into the canvas's alpha channel. An alpha plane
    /// has its colour image's dimensions (AVIF §4.1 via MIAF), as libavif also
    /// requires, which bounds its frames.
    fn decode_alpha(&self, image: &mut RgbaImage) -> Result<(), String> {
        let (width, height) = image.dimensions();
        let mut decoder = self.decoder(width.saturating_mul(height))?;
        match self {
            Self::Coded { data, .. } => {
                let frame = decode_frame(&mut decoder, data)?;
                if (frame.width, frame.height) != image.dimensions() {
                    return Err(String::from("AVIF alpha size differs from the image"));
                }
                yuv::write_alpha(&frame, &mut canvas(image), 0, 0);
                Ok(())
            }
            Self::Grid(grid) => {
                if (grid.output_width, grid.output_height) != image.dimensions() {
                    return Err(String::from("AVIF alpha size differs from the image"));
                }
                for_each_tile(grid, &mut decoder, |frame, x, y| {
                    yuv::write_alpha(frame, &mut canvas(image), x, y);
                })
            }
        }
    }
}

/// How a picture's samples are reconstructed and converted to sRGB.
type Colour = (Cicp, Option<Arc<Conversion>>);

/// An nclx colour description (ISOBMFF §12.1.5, ITU-T H.273 code points).
#[derive(Clone, Copy, Debug)]
struct Nclx {
    cicp: Cicp,
    transfer: u8,
}

impl Nclx {
    fn of(nclx: &mp4parse::NclxColourInformation) -> Self {
        Self {
            cicp: Cicp {
                color_primaries: nclx.colour_primaries(),
                matrix_coefficients: nclx.matrix_coefficients(),
                full_range: nclx.full_range_flag(),
            },
            transfer: nclx.transfer_characteristics(),
        }
    }

    /// The AV1 sequence header's colour description (AV1 §6.4.2), for a
    /// picture without an nclx box.
    fn of_frame(frame: &Frame) -> Self {
        Self {
            cicp: Cicp {
                color_primaries: frame.color.color_primaries,
                matrix_coefficients: frame.color.matrix_coefficients,
                full_range: frame.color.full_range,
            },
            transfer: frame.color.transfer_characteristics,
        }
    }
}

/// The matrix and range that reconstruct R'G'B' from a picture, and the
/// conversion of those samples to sRGB (CSS Color 4 #tagged-images). An ICC
/// profile describes the RGB colour space when present; otherwise the
/// primaries and transfer characteristics of the nclx box or of the
/// sequence header do.
fn picture_colour(nclx: Option<Nclx>, icc: Option<&Tag>, frame: &Frame) -> Colour {
    let nclx = nclx.unwrap_or_else(|| Nclx::of_frame(frame));
    let tag = icc.cloned().unwrap_or(Tag::Cicp {
        primaries: nclx.cicp.color_primaries,
        transfer: nclx.transfer,
        // The reconstruction below already maps the coded range to [0, 1].
        full_range: true,
        peak: None,
    });
    (nclx.cicp, color::conversion(&tag))
}

fn canvas(image: &mut RgbaImage) -> Canvas<'_> {
    let (width, height) = image.dimensions();
    Canvas {
        rgba: image,
        width: width as usize,
        height: height as usize,
    }
}

fn decode_frame(decoder: &mut Decoder, data: &[u8]) -> Result<Frame, String> {
    decoder
        .decode_still(data)
        .map_err(|error: av1::Error| error.to_string())
}

/// Decode a grid's tiles in row-major order, passing each frame and its
/// position on the reconstructed canvas (HEIF §6.6.2.3.3). Every tile has the
/// same size; together they cover the output, and none lies wholly outside.
fn for_each_tile(
    grid: &mp4parse::AvifGrid<'_>,
    decoder: &mut Decoder,
    mut place: impl FnMut(&Frame, usize, usize),
) -> Result<(), String> {
    let mut tile_size = None;
    for (index, tile) in grid.tiles.iter().enumerate() {
        let frame = decode_frame(decoder, tile)?;
        let size = (frame.width, frame.height);
        if *tile_size.get_or_insert(size) != size || grid.tile_size.is_some_and(|s| s != size) {
            return Err(String::from("AVIF grid tiles differ in size"));
        }
        let covers = |tile: u32, count: u32, output: u32| {
            u64::from(tile) * u64::from(count) >= u64::from(output)
                && u64::from(tile) * u64::from(count - 1) < u64::from(output)
        };
        if index == 0
            && !(covers(size.0, grid.columns, grid.output_width)
                && covers(size.1, grid.rows, grid.output_height))
        {
            return Err(String::from("AVIF grid tiles do not match its output size"));
        }
        let row = index as u32 / grid.columns;
        let column = index as u32 % grid.columns;
        place(&frame, (column * size.0) as usize, (row * size.1) as usize);
    }
    Ok(())
}

/// The first sample of the image sequence's colour track.
fn first_sequence_sample<'a>(context: &AvifContext, bytes: &'a [u8]) -> Option<&'a [u8]> {
    let track = colour_track(context.sequence.as_ref()?)?;
    let first = *sample_table(track, bytes.len())?.first()?;
    bytes.get(first.start..first.end)
}

/// The image sequence's colour track: the first AV1 video or picture track
/// that is not an auxiliary track of another.
fn colour_track(sequence: &mp4parse::MediaContext) -> Option<&mp4parse::Track> {
    let auxiliary = |track: &mp4parse::Track| {
        track.tref.as_ref().is_some_and(|tref| {
            sequence
                .tracks
                .iter()
                .filter_map(|other| other.track_id)
                .any(|id| Some(id) != track.track_id && tref.has_auxl_reference(id))
        })
    };
    sequence.tracks.iter().find(|track| {
        matches!(
            track.track_type,
            mp4parse::TrackType::Video | mp4parse::TrackType::Picture
        ) && !auxiliary(track)
            && av1_entry(track).is_some()
    })
}

/// A track's AV1 visual sample entry.
fn av1_entry(track: &mp4parse::Track) -> Option<&mp4parse::VideoSampleEntry> {
    track
        .stsd
        .as_ref()?
        .descriptions
        .iter()
        .find_map(|entry| match entry {
            mp4parse::SampleEntry::Video(video) if video.codec_type == mp4parse::CodecType::AV1 => {
                Some(video)
            }
            _ => None,
        })
}

/// AVIF §4.1 "Auxiliary Image Items and Sequences": an AV1 Alpha Image
/// Sequence is an auxiliary track (an 'auxl' reference to the colour
/// track) whose AuxiliaryTypeInfoBox names the alpha URN.
fn alpha_track<'a>(
    sequence: &'a mp4parse::MediaContext,
    colour: &mp4parse::Track,
) -> Option<&'a mp4parse::Track> {
    const ALPHA: &[u8] = b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha";
    let colour_id = colour.track_id?;
    sequence.tracks.iter().find(|track| {
        track.track_id != colour.track_id
            && track
                .tref
                .as_ref()
                .is_some_and(|tref| tref.has_auxl_reference(colour_id))
            && av1_entry(track)
                .and_then(|entry| entry.auxiliary_type.as_ref())
                .is_some_and(|aux_type| {
                    let aux_type = aux_type.as_slice();
                    aux_type.strip_suffix(&[0]).unwrap_or(aux_type) == ALPHA
                })
    })
}

/// One sample: its bytes in the file and its duration in media ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SampleRef {
    start: usize,
    end: usize,
    ticks: u32,
}

/// Samples beyond this many are not played.
const MAX_SEQUENCE_SAMPLES: usize = 1 << 16;

/// The track's samples in decoding order (ISOBMFF §8.7.3 'stsz', §8.7.4
/// 'stsc': runs of chunks with a sample count, first_chunk numbered from 1;
/// §8.7.5 'stco'/'co64': chunk offsets, a chunk's samples being contiguous;
/// §8.6.1.2 'stts': runs of sample durations). `None` when the tables are
/// missing or place a sample outside the file.
fn sample_table(track: &mp4parse::Track, file_len: usize) -> Option<Vec<SampleRef>> {
    let sizes = track.stsz.as_ref()?;
    let chunks = &track.stco.as_ref()?.offsets;
    let runs = &track.stsc.as_ref()?.samples;
    let times = track.stts.as_ref()?;
    let timed: usize = times
        .samples
        .iter()
        .map(|run| run.sample_count as usize)
        .fold(0, usize::saturating_add);
    let count = if sizes.sample_size == 0 {
        sizes.sample_sizes.len()
    } else {
        timed
    }
    .min(MAX_SEQUENCE_SAMPLES);
    let mut durations = times
        .samples
        .iter()
        .flat_map(|run| std::iter::repeat_n(run.sample_delta, run.sample_count as usize));
    let mut samples = Vec::with_capacity(count);
    let mut run = 0;
    for (index, &chunk_offset) in chunks.iter().enumerate() {
        if samples.len() == count {
            break;
        }
        let chunk = u32::try_from(index + 1).ok()?;
        while runs
            .get(run + 1)
            .is_some_and(|next| next.first_chunk <= chunk)
        {
            run += 1;
        }
        let per_chunk = runs.get(run).filter(|entry| entry.first_chunk <= chunk)?;
        let mut offset = usize::try_from(chunk_offset).ok()?;
        for _ in 0..per_chunk.samples_per_chunk {
            if samples.len() == count {
                break;
            }
            let size = match sizes.sample_size {
                0 => *sizes.sample_sizes.get(samples.len())?,
                size => size,
            };
            let end = offset.checked_add(usize::try_from(size).ok()?)?;
            if end > file_len {
                return None;
            }
            samples.push(SampleRef {
                start: offset,
                end,
                ticks: durations.next().unwrap_or(0),
            });
            offset = end;
        }
    }
    Some(samples)
}

/// How many times a sequence plays, `None` for endlessly. ISOBMFF §8.6.6:
/// an edit list with flags bit 0 set repeats its edits; as libavif reads it,
/// the repetitions fill the track header's duration, all ones meaning an
/// indefinite one, and a count beyond `i32::MAX` is endless too (FFmpeg
/// writes `i64::MAX` for an indefinite version 1 duration). An edit list
/// without that flag plays once. A track without an edit list states no
/// repetition count; it plays endlessly, the count libavif-based browsers
/// give such sequences.
fn sequence_plays(track: &mp4parse::Track) -> Option<u32> {
    match track.looped {
        None => None,
        Some(false) => Some(1),
        Some(true) => {
            let duration = track.tkhd.as_ref()?.duration;
            if duration == u64::MAX || duration == u64::from(u32::MAX) {
                return None;
            }
            let segment = track.edited_duration?.0;
            if segment == 0 {
                return None;
            }
            let plays = duration.div_ceil(segment).max(1);
            (plays <= i32::MAX as u64).then_some(plays as u32)
        }
    }
}

/// An AVIF image sequence (AVIF §3 "Image Sequences", `avis`) played from
/// its first sample: each sample of the colour track is an AV1 temporal unit
/// (AV1-ISOBMFF) shown for its 'stts' duration, with the alpha auxiliary
/// track's sample of the same index as its alpha.
pub(crate) struct Sequence {
    bytes: Arc<[u8]>,
    colour: Vec<SampleRef>,
    alpha: Option<Vec<SampleRef>>,
    /// Media ticks per second ('mdhd').
    timescale: u64,
    plays: Option<u32>,
    nclx: Option<Nclx>,
    icc: Option<Tag>,
    limits: Limits,
    /// The sample entry's size, which every frame should have.
    declared: Option<(u32, u32)>,
    colour_decoder: Option<Decoder>,
    alpha_decoder: Option<Decoder>,
    next: usize,
    size: Option<(u32, u32)>,
}

impl Sequence {
    /// The image sequence of an AVIF file when it has at least two samples
    /// (one sample is a still image), positioned before its first frame.
    pub(crate) fn open(bytes: Arc<[u8]>, limits: Limits) -> Result<Option<Self>, String> {
        let parsed = std::panic::catch_unwind(|| {
            mp4parse::read_avif(&mut Cursor::new(&bytes[..]), ParseStrictness::Normal)
        })
        .map_err(|_| String::from("AVIF decoder failed"))?
        .map_err(|error| format!("AVIF container: {error}"))?;
        let Some(sequence) = parsed.sequence.as_ref() else {
            return Ok(None);
        };
        let Some(track) = colour_track(sequence) else {
            return Ok(None);
        };
        let Some(colour) = sample_table(track, bytes.len()) else {
            return Err(String::from(
                "AVIF image sequence has no valid sample table",
            ));
        };
        if colour.len() < 2 {
            return Ok(None);
        }
        let alpha = alpha_track(sequence, track)
            .and_then(|alpha| sample_table(alpha, bytes.len()))
            .filter(|alpha| alpha.len() == colour.len());
        let entry = av1_entry(track);
        let mut nclx = None;
        let mut icc = None;
        for information in entry
            .into_iter()
            .flat_map(|entry| entry.colour_information.iter())
        {
            match information {
                mp4parse::ColourInformation::Nclx(value) => {
                    nclx.get_or_insert(Nclx::of(value));
                }
                mp4parse::ColourInformation::Icc(profile, _) => {
                    if icc.is_none() {
                        icc = color::icc_tag(Some(profile.bytes().to_vec()));
                    }
                }
            }
        }
        let declared = entry
            .map(|entry| (u32::from(entry.width), u32::from(entry.height)))
            .filter(|&(width, height)| width > 0 && height > 0);
        if let Some((width, height)) = declared {
            limits.check(width, height)?;
        }
        Ok(Some(Self {
            timescale: track.timescale.map_or(0, |scale| scale.0),
            plays: sequence_plays(track),
            bytes: Arc::clone(&bytes),
            colour,
            alpha,
            nclx,
            icc,
            limits,
            declared,
            colour_decoder: None,
            alpha_decoder: None,
            next: 0,
            size: None,
        }))
    }

    /// How many times the sequence plays; `None` for endlessly.
    pub(crate) fn plays(&self) -> Option<u32> {
        self.plays
    }

    fn decoder(&self) -> Result<Decoder, String> {
        let max = self.limits.max_pixels();
        let declared = self
            .declared
            .map_or(max, |(width, height)| width.saturating_mul(height));
        Decoder::new(&DecoderConfig {
            frame_size_limit: declared.clamp(1, max.max(1)),
            ..DecoderConfig::default()
        })
        .map_err(|error| error.to_string())
    }

    /// The next frame and how long it is shown, or `None` after the last.
    pub(crate) fn next_frame(&mut self) -> Result<Option<(RgbaImage, Duration)>, String> {
        let mut ticks = 0u64;
        while let Some(&sample) = self.colour.get(self.next) {
            let index = self.next;
            self.next += 1;
            ticks = ticks.saturating_add(u64::from(sample.ticks));
            if self.colour_decoder.is_none() {
                self.colour_decoder = Some(self.decoder()?);
            }
            let decoder = self
                .colour_decoder
                .as_mut()
                .expect("decoder was just created");
            // A temporal unit without a shown frame lengthens the next one.
            let Some(frame) = decoder
                .decode(&self.bytes[sample.start..sample.end])
                .map_err(|error| error.to_string())?
                .pop()
            else {
                continue;
            };
            self.limits.check(frame.width, frame.height)?;
            if *self.size.get_or_insert((frame.width, frame.height)) != (frame.width, frame.height)
            {
                return Err(String::from("AVIF sequence frames differ in size"));
            }
            let mut image = RgbaImage::new(frame.width, frame.height);
            let (cicp, conversion) = picture_colour(self.nclx, self.icc.as_ref(), &frame);
            yuv::write_rgb(
                &frame,
                cicp,
                conversion.as_deref(),
                &mut canvas(&mut image),
                0,
                0,
            );
            if let Some(&alpha) = self.alpha.as_ref().and_then(|alpha| alpha.get(index)) {
                if self.alpha_decoder.is_none() {
                    self.alpha_decoder = Some(self.decoder()?);
                }
                let decoder = self
                    .alpha_decoder
                    .as_mut()
                    .expect("decoder was just created");
                if let Some(plane) = decoder
                    .decode(&self.bytes[alpha.start..alpha.end])
                    .map_err(|error| error.to_string())?
                    .pop()
                {
                    if (plane.width, plane.height) != image.dimensions() {
                        return Err(String::from("AVIF alpha size differs from the image"));
                    }
                    yuv::write_alpha(&plane, &mut canvas(&mut image), 0, 0);
                }
            }
            let delay = if self.timescale == 0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f64(ticks as f64 / self.timescale as f64)
            };
            return Ok(Some((image, delay)));
        }
        Ok(None)
    }
}

/// Apply the primary item's transformative properties: `clap`, then `irot`,
/// then `imir` (MIAF §7.3.6.7; mp4parse rejects other orders).
fn apply_transforms(context: &AvifContext, mut image: RgbaImage) -> Result<RgbaImage, String> {
    let clap = context
        .clean_aperture()
        .map_err(|error| format!("AVIF clap: {error}"))?;
    if let Some((x, y, width, height)) =
        clap.and_then(|clap| clean_aperture_rect(clap, image.width(), image.height()))
    {
        image = image::imageops::crop_imm(&image, x, y, width, height).to_image();
    }
    // `irot` turns the image anti-clockwise by angle × 90° (HEIF §6.5.10).
    image = match context
        .image_rotation()
        .map_err(|error| format!("AVIF irot: {error}"))?
    {
        ImageRotation::D0 => image,
        ImageRotation::D90 => image::imageops::rotate270(&image),
        ImageRotation::D180 => image::imageops::rotate180(&image),
        ImageRotation::D270 => image::imageops::rotate90(&image),
    };
    // HEIF (ISO/IEC 23008-12:2022) §6.5.12: `imir` axis 0 exchanges the top
    // and bottom parts, axis 1 the left and right parts.
    match context
        .image_mirror()
        .map_err(|error| format!("AVIF imir: {error}"))?
    {
        Some(ImageMirror::TopBottom) => image::imageops::flip_vertical_in_place(&mut image),
        Some(ImageMirror::LeftRight) => image::imageops::flip_horizontal_in_place(&mut image),
        None => {}
    }
    Ok(image)
}

/// The integer crop rectangle of a clean aperture (ISOBMFF §12.1.4): a
/// `width`×`height` region whose centre is offset by (horizOff, vertOff) from
/// the image centre, i.e. whose left edge is horizOff + (W − width) / 2.
/// Apertures that are fractional, empty or outside the image are ignored, as
/// libavif-based browsers do, and the full image is shown.
fn clean_aperture_rect(
    clap: &mp4parse::CleanAperture,
    image_width: u32,
    image_height: u32,
) -> Option<(u32, u32, u32, u32)> {
    fn axis(size_n: u32, size_d: u32, off_n: i32, off_d: u32, image: u32) -> Option<(u32, u32)> {
        if size_d == 0 || off_d == 0 || !size_n.is_multiple_of(size_d) {
            return None;
        }
        let size = i128::from(size_n / size_d);
        let (off_n, off_d) = (i128::from(off_n), i128::from(off_d));
        // start = off_n / off_d + (image − size) / 2, which must be integral.
        let numerator = 2 * off_n + (i128::from(image) - size) * off_d;
        let denominator = 2 * off_d;
        if numerator % denominator != 0 {
            return None;
        }
        let start = numerator / denominator;
        (size > 0 && start >= 0 && start + size <= i128::from(image))
            .then_some((start as u32, size as u32))
    }
    let (x, width) = axis(
        clap.width_n,
        clap.width_d,
        clap.horiz_off_n,
        clap.horiz_off_d,
        image_width,
    )?;
    let (y, height) = axis(
        clap.height_n,
        clap.height_d,
        clap.vert_off_n,
        clap.vert_off_d,
        image_height,
    )?;
    ((x, y, width, height) != (0, 0, image_width, image_height)).then_some((x, y, width, height))
}

#[cfg(test)]
mod tests {
    //! Fixtures in `src/img/fixtures/avif` (64×64 unless noted) were made on
    //! 2026-10-04 with ImageMagick 7 and FFmpeg 9.0.2 (libaom-av1,
    //! `-still-picture 1 -crf 10`, FFmpeg's AVIF muxer) from a quadrant image:
    //! red top left, green top right, blue bottom left, white bottom right.
    //! - opaque-420-8bit: yuv420p, BT.709 matrix, studio swing (colr nclx).
    //! - opaque-420-10bit: yuv420p10le, BT.709, full swing.
    //! - opaque-444-8bit: yuv444p, BT.601 (SMPTE 170M) matrix, full swing.
    //! - alpha-420-8bit: as opaque-420-8bit plus an alpha auxiliary item:
    //!   opaque, 50%, transparent and opaque quadrants.
    //! - monochrome-128: a gray(128) monochrome (4:0:0) picture.
    //! - grid-alpha, irot-imir and clap were assembled from those encodes'
    //!   AV1 items by a small HEIF box writer (not part of TRust): a 2×2 grid
    //!   of solid red/green/blue/yellow 64×64 tiles cropped to 120×100, with a
    //!   2×2 alpha grid (255, 128, 0, 255); the quadrant image with essential
    //!   `irot` angle 1 and `imir` axis 1; and with an essential `clap` of
    //!   32×16 offset (16, −16) from the centre. FFmpeg's dav1d-based decoder
    //!   renders the same colour results for all three.
    //! - anim-loop2, anim-infinite and anim-alpha (16×16, made 2026-10-08
    //!   with FFmpeg's AVIF muxer, libaom-av1 4:4:4 `-crf 10`, 4 fps) are
    //!   three-sample `avis` sequences of solid red, green and blue frames,
    //!   each 4096 ticks of a 16384 Hz media timescale. anim-loop2 has an
    //!   edit list with the repeat flag and a track duration of two plays
    //!   (`-loop 2`), anim-infinite an indefinite one (`-loop 0`); anim-alpha
    //!   adds an `auxl`/`auxi` alpha track (255, 128, 0) and an edit list
    //!   without the repeat flag (`-loop 1`).
    use super::*;

    const LIMITS: Limits = Limits {
        max_dimension: 12_000,
        max_bytes: 512 * 1024 * 1024,
    };

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/src/img/fixtures/avif/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    fn assert_near(image: &RgbaImage, x: u32, y: u32, expected: [u8; 4], tolerance: u8) {
        let actual = image.get_pixel(x, y).0;
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(&a, e)| a.abs_diff(e) <= tolerance),
            "pixel ({x}, {y}) is {actual:?}, expected {expected:?} ± {tolerance}"
        );
    }

    fn assert_quadrants(image: &RgbaImage, expected: [[u8; 4]; 4]) {
        let (w, h) = image.dimensions();
        let points = [
            (w / 4, h / 4),
            (3 * w / 4, h / 4),
            (w / 4, 3 * h / 4),
            (3 * w / 4, 3 * h / 4),
        ];
        for ((x, y), expected) in points.into_iter().zip(expected) {
            assert_near(image, x, y, expected, 6);
        }
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    #[test]
    fn sniffing_reads_ftyp_brands_like_the_mp4_signature() {
        fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
            let size = 16 + 4 * compatible.len();
            let mut bytes = (size as u32).to_be_bytes().to_vec();
            bytes.extend_from_slice(b"ftyp");
            bytes.extend_from_slice(major);
            bytes.extend_from_slice(&0u32.to_be_bytes());
            for brand in compatible {
                bytes.extend_from_slice(*brand);
            }
            bytes.extend_from_slice(b"\0\0\0\x08free");
            bytes
        }
        assert!(sniff(&ftyp(b"avif", &[b"mif1", b"miaf"])));
        assert!(sniff(&ftyp(b"avis", &[b"msf1"])));
        // Brands may appear only among the compatible brands.
        assert!(sniff(&ftyp(b"mif1", &[b"miaf", b"avif"])));
        assert!(sniff(&ftyp(b"msf1", &[b"avis"])));
        // Ordinary MP4 and HEIC files are not AVIF.
        assert!(!sniff(&ftyp(b"isom", &[b"iso2", b"mp41"])));
        assert!(!sniff(&ftyp(b"heic", &[b"mif1", b"heic"])));
        // The minor version is not a brand.
        let mut minor = ftyp(b"mif1", &[b"miaf"]);
        minor[12..16].copy_from_slice(b"avif");
        assert!(!sniff(&minor));
        // The whole box must be present and sized in four-byte steps.
        let full = ftyp(b"mif1", &[b"miaf", b"avif"]);
        assert!(!sniff(&full[..full.len() - 12]));
        let mut odd = full.clone();
        odd[3] += 1;
        assert!(!sniff(&odd));
        assert!(!sniff(b"\0\0\0\x0cftypavif"));
        for name in ["opaque-420-8bit.avif", "grid-alpha.avif"] {
            assert_eq!(crate::img::sniff(&fixture(name)), Some(MIME), "{name}");
        }
        assert!(crate::img::raster_mime_supported("image/avif"));
        assert!(crate::img::raster_mime_supported("IMAGE/AVIF"));
    }

    #[test]
    fn decodes_8bit_420_studio_swing() {
        let (image, mime) = crate::img::decode(&fixture("opaque-420-8bit.avif")).unwrap();
        assert_eq!(mime, MIME);
        let image = image.into_rgba8();
        assert_eq!(image.dimensions(), (64, 64));
        assert_quadrants(&image, [RED, GREEN, BLUE, WHITE]);
        assert!(image.pixels().all(|pixel| pixel[3] == 255));
        let info = crate::img::info(&fixture("opaque-420-8bit.avif")).unwrap();
        assert_eq!(
            (info.width, info.height, info.mime, info.has_alpha),
            (64, 64, MIME, false)
        );
    }

    #[test]
    fn decodes_10bit_full_swing_and_444() {
        for name in ["opaque-420-10bit.avif", "opaque-444-8bit.avif"] {
            let image = decode(&fixture(name), LIMITS).unwrap();
            assert_eq!(image.dimensions(), (64, 64), "{name}");
            assert_quadrants(&image, [RED, GREEN, BLUE, WHITE]);
        }
        // 4:4:4 keeps full chroma resolution right up to a quadrant edge.
        let image = decode(&fixture("opaque-444-8bit.avif"), LIMITS).unwrap();
        assert_near(&image, 31, 10, RED, 12);
        assert_near(&image, 32, 10, GREEN, 12);
    }

    #[test]
    fn decodes_monochrome_as_gray() {
        let image = decode(&fixture("monochrome-128.avif"), LIMITS).unwrap();
        assert_eq!(image.dimensions(), (64, 64));
        assert_near(&image, 20, 20, [128, 128, 128, 255], 3);
    }

    #[test]
    fn alpha_auxiliary_image_becomes_straight_alpha() {
        let bytes = fixture("alpha-420-8bit.avif");
        let image = decode(&bytes, LIMITS).unwrap();
        let alpha = |x, y| image.get_pixel(x, y)[3];
        assert!(alpha(16, 16) >= 250);
        assert!(alpha(48, 16).abs_diff(128) <= 4, "{}", alpha(48, 16));
        assert!(alpha(16, 48) <= 4);
        assert!(alpha(48, 48) >= 250);
        // Colour is straight, not premultiplied: the half-transparent green
        // quadrant keeps full-intensity green.
        assert_near(&image, 48, 16, [0, 255, 0, 128], 6);
        let resource = crate::img::decode_graphical(&bytes).unwrap();
        assert!(resource.has_alpha);
        assert_eq!(&resource.rgba[..], image.as_raw().as_slice());
    }

    #[test]
    fn premultiplied_colour_is_divided_by_alpha() {
        let mut rgba = [100, 50, 0, 128, 10, 20, 30, 0, 1, 2, 3, 255];
        yuv::unpremultiply(&mut rgba);
        assert_eq!(rgba, [199, 100, 0, 128, 0, 0, 0, 0, 1, 2, 3, 255]);
    }

    #[test]
    fn grid_tiles_are_placed_row_major_and_cropped_to_the_output() {
        let image = decode(&fixture("grid-alpha.avif"), LIMITS).unwrap();
        assert_eq!(image.dimensions(), (120, 100));
        // Tile boundaries are at x = 64 and y = 64; the right column and
        // bottom row are cut to 56 and 36 pixels.
        assert_near(&image, 10, 10, RED, 6);
        assert_near(&image, 63, 63, RED, 6);
        assert_near(&image, 64, 10, [0, 255, 0, 128], 6);
        assert_near(&image, 119, 0, [0, 255, 0, 128], 6);
        assert_near(&image, 10, 64, [0, 0, 255, 0], 6);
        assert_near(&image, 119, 99, [255, 255, 0, 255], 6);
        // The same file through the HTML image path.
        let resource = crate::img::decode_graphical(&fixture("grid-alpha.avif")).unwrap();
        assert_eq!(
            (resource.width, resource.height, resource.has_alpha),
            (120, 100, true)
        );
    }

    #[test]
    fn rotation_applies_before_mirroring() {
        // irot 1 turns the quadrants anti-clockwise (green to the top left),
        // then imir axis 1 exchanges left and right.
        let image = decode(&fixture("irot-imir.avif"), LIMITS).unwrap();
        assert_eq!(image.dimensions(), (64, 64));
        assert_quadrants(&image, [WHITE, GREEN, BLUE, RED]);
    }

    #[test]
    fn clean_aperture_crops_around_the_offset_centre() {
        let image = decode(&fixture("clap.avif"), LIMITS).unwrap();
        assert_eq!(image.dimensions(), (32, 16));
        // Column 0 is the green quadrant's edge, where upsampled chroma still
        // blends in some of the red quadrant; sample just inside it.
        for (x, y) in [(3, 0), (31, 0), (3, 15), (31, 15)] {
            assert_near(&image, x, y, GREEN, 8);
        }
    }

    #[test]
    fn clean_aperture_rectangles_follow_isobmff_semantics() {
        let clap = |w: u32, h: u32, x: i32, y: i32| mp4parse::CleanAperture {
            width_n: w,
            width_d: 1,
            height_n: h,
            height_d: 1,
            horiz_off_n: x,
            horiz_off_d: 1,
            vert_off_n: y,
            vert_off_d: 1,
        };
        assert_eq!(
            clean_aperture_rect(&clap(32, 16, 16, -16), 64, 64),
            Some((32, 8, 32, 16))
        );
        assert_eq!(
            clean_aperture_rect(&clap(480, 256, 0, 0), 480, 270),
            Some((0, 7, 480, 256))
        );
        // Half-pixel offsets express integral edges.
        let half = mp4parse::CleanAperture {
            horiz_off_n: 1,
            horiz_off_d: 2,
            ..clap(63, 64, 0, 0)
        };
        assert_eq!(clean_aperture_rect(&half, 64, 64), Some((1, 0, 63, 64)));
        // Identity, fractional, empty and out-of-bounds apertures are ignored.
        assert_eq!(clean_aperture_rect(&clap(64, 64, 0, 0), 64, 64), None);
        assert_eq!(clean_aperture_rect(&clap(63, 64, 0, 0), 64, 64), None);
        assert_eq!(clean_aperture_rect(&clap(0, 64, 0, 0), 64, 64), None);
        assert_eq!(clean_aperture_rect(&clap(32, 32, 17, 0), 64, 64), None);
        assert_eq!(clean_aperture_rect(&clap(65, 64, 0, 0), 64, 64), None);
        let zero_denominator = mp4parse::CleanAperture {
            width_d: 0,
            ..clap(32, 32, 0, 0)
        };
        assert_eq!(clean_aperture_rect(&zero_denominator, 64, 64), None);
    }

    #[test]
    fn limits_reject_oversized_images_before_decoding() {
        let tight = Limits {
            max_dimension: 100,
            max_bytes: LIMITS.max_bytes,
        };
        let error = decode(&fixture("grid-alpha.avif"), tight).unwrap_err();
        assert!(error.contains("100px cap"), "{error}");
        let small_budget = Limits {
            max_dimension: 12_000,
            max_bytes: 64 * 64 * 4 - 1,
        };
        assert!(decode(&fixture("opaque-420-8bit.avif"), small_budget).is_err());
        assert!(decode(&fixture("opaque-420-8bit.avif"), LIMITS).is_ok());
    }

    #[test]
    fn malformed_files_fail_without_panicking() {
        for name in ["alpha-420-8bit.avif", "grid-alpha.avif", "clap.avif"] {
            let bytes = fixture(name);
            // Every truncation point.
            for len in 0..bytes.len() {
                let _ = decode(&bytes[..len], LIMITS);
                let _ = crate::img::decode(&bytes[..len]);
            }
            // Corrupt each byte in turn, covering box headers, properties and
            // AV1 payloads.
            for index in 0..bytes.len() {
                let mut corrupt = bytes.clone();
                corrupt[index] ^= 0xa5;
                let _ = decode(&corrupt, LIMITS);
            }
        }
        assert!(decode(b"", LIMITS).is_err());
        let mut garbage = fixture("opaque-420-8bit.avif")[..32].to_vec();
        garbage.extend(std::iter::repeat_n(0xff, 64));
        assert!(decode(&garbage, LIMITS).is_err());
        assert!(crate::img::decode(&garbage).is_err());
    }

    /// Decode every `.avif` under `TRUST_AVIF_CORPUS` (for example the AOM
    /// av1-avif test files) and, with `TRUST_AVIF_CORPUS_OUT`, write PNGs for
    /// comparison with a reference decoder:
    /// `TRUST_AVIF_CORPUS=… cargo test --release --lib avif_corpus -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a local AVIF corpus"]
    fn avif_corpus() {
        let Some(root) = std::env::var_os("TRUST_AVIF_CORPUS") else {
            return;
        };
        let out = std::env::var_os("TRUST_AVIF_CORPUS_OUT").map(std::path::PathBuf::from);
        let mut pending = vec![std::path::PathBuf::from(root)];
        let mut files = Vec::new();
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            } else if path
                .extension()
                .is_some_and(|e| e == "avif" || e == "avifs")
            {
                files.push(path);
            }
        }
        files.sort();
        for path in files {
            let bytes = std::fs::read(&path).unwrap();
            let start = std::time::Instant::now();
            match decode(&bytes, LIMITS) {
                Ok(image) => {
                    let translucent = image.pixels().any(|pixel| pixel[3] != 255);
                    println!(
                        "ok   {}x{} alpha={translucent} {:?} {}",
                        image.width(),
                        image.height(),
                        start.elapsed(),
                        path.display()
                    );
                    if let Some(out) = &out {
                        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                        image.save(out.join(format!("{name}.png"))).unwrap();
                    }
                }
                Err(error) => println!("FAIL {error} {}", path.display()),
            }
        }
    }

    /// Decode time and peak-memory growth for one large file:
    /// `TRUST_AVIF_BENCH=big.avif cargo test --release --lib avif_decode_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn avif_decode_bench() {
        let Some(path) = std::env::var_os("TRUST_AVIF_BENCH") else {
            return;
        };
        fn high_water_kib() -> u64 {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|status| {
                    status
                        .lines()
                        .find_map(|line| line.strip_prefix("VmHWM:"))
                        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
                })
                .unwrap_or(0)
        }
        let bytes = std::fs::read(path).unwrap();
        let before = high_water_kib();
        let mut times = Vec::new();
        let mut size = (0, 0);
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let image = decode(&bytes, LIMITS).unwrap();
            times.push(start.elapsed());
            size = image.dimensions();
        }
        times.sort();
        println!(
            "{}x{} from {} bytes: best {:?}, median {:?}; VmHWM grew {} KiB (from {} KiB)",
            size.0,
            size.1,
            bytes.len(),
            times[0],
            times[2],
            high_water_kib().saturating_sub(before),
            before
        );
    }

    #[test]
    fn yuv_matrices_and_ranges_follow_h273() {
        fn convert(bit_depth: u8, cicp: Cicp, yuv: [u16; 3]) -> [u8; 4] {
            let plane = |value: u16| av1::Plane {
                width: 1,
                height: 1,
                samples: if bit_depth == 8 {
                    av1::Samples::Eight(vec![value as u8])
                } else {
                    av1::Samples::High(vec![value])
                },
            };
            let frame = Frame {
                width: 1,
                height: 1,
                bit_depth,
                layout: av1::PixelLayout::Yuv444,
                color: av1::ColorDescription {
                    color_primaries: 1,
                    transfer_characteristics: 13,
                    matrix_coefficients: cicp.matrix_coefficients,
                    full_range: cicp.full_range,
                },
                y: plane(yuv[0]),
                chroma: Some([plane(yuv[1]), plane(yuv[2])]),
            };
            let mut rgba = [0u8; 4];
            let mut canvas = Canvas {
                rgba: &mut rgba,
                width: 1,
                height: 1,
            };
            yuv::write_rgb(&frame, cicp, None, &mut canvas, 0, 0);
            rgba
        }
        let cicp = |matrix_coefficients, full_range| Cicp {
            color_primaries: 1,
            matrix_coefficients,
            full_range,
        };
        let near = |actual: [u8; 4], expected: [u8; 4]| {
            assert!(
                actual
                    .iter()
                    .zip(expected)
                    .all(|(&a, e)| a.abs_diff(e) <= 1),
                "{actual:?} != {expected:?}"
            );
        };
        // Studio swing black and white at 8 and 10 bits; full swing white.
        near(convert(8, cicp(1, false), [16, 128, 128]), [0, 0, 0, 255]);
        near(
            convert(8, cicp(1, false), [235, 128, 128]),
            [255, 255, 255, 255],
        );
        near(
            convert(10, cicp(1, false), [940, 512, 512]),
            [255, 255, 255, 255],
        );
        near(
            convert(10, cicp(9, true), [1023, 512, 512]),
            [255, 255, 255, 255],
        );
        // Red under BT.709 (KR 0.2126, KB 0.0722) and BT.601 (0.299, 0.114).
        near(convert(8, cicp(1, true), [54, 99, 255]), [255, 0, 0, 255]);
        near(convert(8, cicp(6, true), [76, 85, 255]), [255, 0, 0, 255]);
        // The BT.709 code values decoded as BT.601 are visibly different.
        assert!(convert(8, cicp(6, true), [54, 99, 255])[0] < 240);
        // Unspecified matrices use BT.601.
        near(convert(8, cicp(2, true), [76, 85, 255]), [255, 0, 0, 255]);
        // Identity (GBR) stores G, B, R.
        near(convert(8, cicp(0, true), [10, 20, 30]), [30, 10, 20, 255]);
        // YCgCo: Y 0.5, Cg 0, Co +0.25 gives R 0.75, G 0.5, B 0.25.
        near(
            convert(8, cicp(8, true), [128, 128, 192]),
            [192, 128, 64, 255],
        );
    }

    fn play(name: &str) -> (Option<u32>, Vec<(RgbaImage, Duration)>) {
        let mut sequence = Sequence::open(Arc::from(fixture(name)), LIMITS)
            .unwrap()
            .expect("an image sequence");
        let mut frames = Vec::new();
        while let Some(frame) = sequence.next_frame().unwrap() {
            frames.push(frame);
        }
        (sequence.plays(), frames)
    }

    #[test]
    fn image_sequences_play_each_sample_for_its_duration() {
        // AVIF §3: each colour-track sample is a frame; ISOBMFF 'stts' gives
        // 4096 / 16384 s.
        let (plays, frames) = play("anim-loop2.avif");
        assert_eq!(
            plays,
            Some(2),
            "repeat flag with two plays of track duration"
        );
        assert_eq!(frames.len(), 3);
        for ((image, delay), expected) in
            frames
                .iter()
                .zip([[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]])
        {
            assert_eq!(*delay, Duration::from_millis(250));
            assert_eq!(image.dimensions(), (16, 16));
            assert_near(image, 8, 8, expected, 6);
        }
        assert_eq!(
            play("anim-infinite.avif").0,
            None,
            "indefinite duration loops"
        );
        // A still image has no sequence; the decoder shows its primary item.
        assert!(
            Sequence::open(Arc::from(fixture("opaque-420-8bit.avif")), LIMITS)
                .unwrap()
                .is_none()
        );
        // The still decode of a sequence file is its first frame.
        assert_near(
            &decode(&fixture("anim-loop2.avif"), LIMITS).unwrap(),
            8,
            8,
            [255, 0, 0, 255],
            6,
        );
    }

    #[test]
    fn alpha_sequences_take_alpha_from_their_auxiliary_track() {
        // AVIF §4.1: the alpha auxiliary sequence's sample of each index.
        let (plays, frames) = play("anim-alpha.avif");
        assert_eq!(
            plays,
            Some(1),
            "an edit list without the repeat flag plays once"
        );
        let alphas: Vec<u8> = frames
            .iter()
            .map(|(image, _)| image.get_pixel(8, 8)[3])
            .collect();
        assert_eq!(alphas.len(), 3);
        assert!(
            alphas[0] >= 250 && alphas[1].abs_diff(128) <= 4 && alphas[2] <= 4,
            "{alphas:?}"
        );
        assert_near(&frames[0].0, 8, 8, [255, 0, 0, 255], 6);
    }

    #[test]
    fn sequence_limits_reject_frames_before_decoding() {
        let tight = Limits {
            max_dimension: 8,
            max_bytes: 512 * 1024 * 1024,
        };
        assert!(Sequence::open(Arc::from(fixture("anim-loop2.avif")), tight).is_err());
    }
}
