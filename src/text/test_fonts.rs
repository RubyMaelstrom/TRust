//! A minimal variable TrueType font for conformance tests of CSS Fonts 4
//! variation handling. Each test glyph's advance varies along exactly one
//! axis (through an `HVAR` item variation store), so a shaped advance
//! reports the normalized value the shaper applied to that axis.

/// The test font's axes: (tag, min, default, max).
pub(crate) const AXES: [([u8; 4], f32, f32, f32); 5] = [
    (*b"wght", 100.0, 400.0, 900.0),
    (*b"wdth", 50.0, 100.0, 200.0),
    (*b"opsz", 8.0, 12.0, 72.0),
    (*b"slnt", -20.0, 0.0, 0.0),
    (*b"ital", 0.0, 0.0, 1.0),
];

/// The glyph for each axis, in `AXES` order, then an invariant one.
pub(crate) const AXIS_CHARS: [char; 5] = ['w', 'd', 'o', 's', 'i'];
pub(crate) const PLAIN_CHAR: char = 'x';

/// Units per em, the default advance and each axis glyph's advance change
/// at the axis's extreme (its normalized peak).
pub(crate) const UPEM: u16 = 1000;
pub(crate) const ADVANCE: i16 = 500;
pub(crate) const DELTA: i16 = 400;

/// The value an axis takes for a shaped advance of its glyph at font size
/// `UPEM` (one CSS pixel per unit). Only values between an axis's default
/// and its peak (the maximum; the minimum for `slnt`) are observable.
pub(crate) fn axis_value(axis: usize, advance: f32) -> f32 {
    let (_, min, default, max) = AXES[axis];
    let normalized = (advance - f32::from(ADVANCE)) / f32::from(DELTA);
    if axis == 3 {
        // `slnt` peaks at its minimum.
        default - normalized * (default - min)
    } else if normalized >= 0.0 {
        default + normalized * (max - default)
    } else {
        default + normalized * (default - min)
    }
}

fn be16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn be32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn fixed(out: &mut Vec<u8>, value: f32) {
    be32(out, (value * 65536.0).round() as i32 as u32);
}

fn f2dot14(out: &mut Vec<u8>, value: f32) {
    be16(out, (value * 16384.0).round() as i16 as u16);
}

/// The font's bytes; its glyphs have no outlines.
pub(crate) fn variable_font() -> Vec<u8> {
    let chars: Vec<char> = std::iter::once(' ')
        .chain(AXIS_CHARS)
        .chain(std::iter::once(PLAIN_CHAR))
        .collect();
    let glyphs = chars.len() as u16 + 1;
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();

    let mut head = Vec::new();
    be32(&mut head, 0x0001_0000);
    be32(&mut head, 0x0001_0000);
    be32(&mut head, 0);
    be32(&mut head, 0x5F0F_3CF5);
    be16(&mut head, 0);
    be16(&mut head, UPEM);
    head.extend_from_slice(&[0; 16]);
    for value in [0i16, -200, 1000, 800] {
        be16(&mut head, value as u16);
    }
    be16(&mut head, 0);
    be16(&mut head, 8);
    be16(&mut head, 2);
    be16(&mut head, 0);
    be16(&mut head, 0);
    tables.push((*b"head", head));

    let mut hhea = Vec::new();
    be32(&mut hhea, 0x0001_0000);
    for value in [800i16, -200, 0] {
        be16(&mut hhea, value as u16);
    }
    be16(&mut hhea, 2000);
    hhea.extend_from_slice(&[0; 22]);
    be16(&mut hhea, glyphs);
    tables.push((*b"hhea", hhea));

    let mut maxp = Vec::new();
    be32(&mut maxp, 0x0001_0000);
    be16(&mut maxp, glyphs);
    maxp.extend_from_slice(&[0; 26]);
    tables.push((*b"maxp", maxp));

    let mut os2 = Vec::new();
    be16(&mut os2, 4);
    be16(&mut os2, 500);
    be16(&mut os2, 400);
    be16(&mut os2, 5);
    os2.extend_from_slice(&[0; 54]);
    be16(&mut os2, 0x40);
    be16(&mut os2, 0x20);
    be16(&mut os2, 0x78);
    for value in [800i16, -200, 0] {
        be16(&mut os2, value as u16);
    }
    be16(&mut os2, 800);
    be16(&mut os2, 200);
    os2.extend_from_slice(&[0; 8]);
    be16(&mut os2, 500);
    be16(&mut os2, 700);
    be16(&mut os2, 0);
    be16(&mut os2, 0x20);
    be16(&mut os2, 0);
    tables.push((*b"OS/2", os2));

    let mut hmtx = Vec::new();
    for _ in 0..glyphs {
        be16(&mut hmtx, ADVANCE as u16);
        be16(&mut hmtx, 0);
    }
    tables.push((*b"hmtx", hmtx));

    // cmap: one format 4 segment per character, then the final segment.
    let mut codes: Vec<(u16, u16)> = chars
        .iter()
        .enumerate()
        .map(|(index, c)| (*c as u16, index as u16 + 1))
        .collect();
    codes.sort_unstable();
    let segments = codes.len() as u16 + 1;
    let mut subtable = Vec::new();
    be16(&mut subtable, 4);
    be16(&mut subtable, 16 + 8 * segments);
    be16(&mut subtable, 0);
    be16(&mut subtable, segments * 2);
    let power = 1u16 << (15 - segments.leading_zeros());
    be16(&mut subtable, 2 * power);
    be16(&mut subtable, power.trailing_zeros() as u16);
    be16(&mut subtable, segments * 2 - 2 * power);
    for (code, _) in &codes {
        be16(&mut subtable, *code);
    }
    be16(&mut subtable, 0xFFFF);
    be16(&mut subtable, 0);
    for (code, _) in &codes {
        be16(&mut subtable, *code);
    }
    be16(&mut subtable, 0xFFFF);
    for (code, glyph) in &codes {
        be16(&mut subtable, glyph.wrapping_sub(*code));
    }
    be16(&mut subtable, 1);
    for _ in 0..segments {
        be16(&mut subtable, 0);
    }
    let mut cmap = Vec::new();
    be16(&mut cmap, 0);
    be16(&mut cmap, 1);
    be16(&mut cmap, 3);
    be16(&mut cmap, 1);
    be32(&mut cmap, 12);
    cmap.extend_from_slice(&subtable);
    tables.push((*b"cmap", cmap));

    let mut loca = Vec::new();
    for _ in 0..=glyphs {
        be16(&mut loca, 0);
    }
    tables.push((*b"loca", loca));
    tables.push((*b"glyf", Vec::new()));

    let mut post = Vec::new();
    be32(&mut post, 0x0003_0000);
    post.extend_from_slice(&[0; 28]);
    tables.push((*b"post", post));

    // name: family, full and PostScript names (Windows, US English).
    let names = [
        (1u16, "TRust Axes"),
        (2, "Regular"),
        (4, "TRust Axes Regular"),
        (6, "TRustAxes-Regular"),
        (300, "Heavy Wide"),
    ];
    let mut strings = Vec::new();
    let mut name = Vec::new();
    be16(&mut name, 0);
    be16(&mut name, names.len() as u16);
    be16(&mut name, 6 + 12 * names.len() as u16);
    for (id, text) in names {
        let encoded: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        for value in [
            3u16,
            1,
            0x409,
            id,
            encoded.len() as u16,
            strings.len() as u16,
        ] {
            be16(&mut name, value);
        }
        strings.extend_from_slice(&encoded);
    }
    name.extend_from_slice(&strings);
    tables.push((*b"name", name));

    let mut fvar = Vec::new();
    be16(&mut fvar, 1);
    be16(&mut fvar, 0);
    be16(&mut fvar, 16);
    be16(&mut fvar, 2);
    be16(&mut fvar, AXES.len() as u16);
    be16(&mut fvar, 20);
    be16(&mut fvar, 1);
    be16(&mut fvar, 4 + 4 * AXES.len() as u16);
    for (index, (tag, min, default, max)) in AXES.iter().enumerate() {
        fvar.extend_from_slice(tag);
        fixed(&mut fvar, *min);
        fixed(&mut fvar, *default);
        fixed(&mut fvar, *max);
        be16(&mut fvar, 0);
        be16(&mut fvar, 256 + index as u16);
    }
    // One named instance, "Heavy Wide": wght 900, wdth 200.
    be16(&mut fvar, 300);
    be16(&mut fvar, 0);
    for (tag, _, default, _) in AXES {
        fixed(
            &mut fvar,
            match &tag {
                b"wght" => 900.0,
                b"wdth" => 200.0,
                _ => default,
            },
        );
    }
    tables.push((*b"fvar", fvar));

    // HVAR: an item variation store with one region per axis (peaking at
    // +1, or at -1 for `slnt`) and implicit glyph-to-item mapping.
    let axes = AXES.len() as u16;
    let mut regions = Vec::new();
    be16(&mut regions, axes);
    be16(&mut regions, axes);
    for region in 0..AXES.len() {
        for axis in 0..AXES.len() {
            let peak: f32 = if axis != region {
                0.0
            } else if region == 3 {
                -1.0
            } else {
                1.0
            };
            f2dot14(&mut regions, peak.min(0.0));
            f2dot14(&mut regions, peak);
            f2dot14(&mut regions, peak.max(0.0));
        }
    }
    let mut data = Vec::new();
    be16(&mut data, glyphs);
    be16(&mut data, axes);
    be16(&mut data, axes);
    for region in 0..axes {
        be16(&mut data, region);
    }
    for glyph in 0..glyphs {
        for region in 0..AXES.len() {
            // Glyph 2 + axis varies along that axis only.
            let delta = if usize::from(glyph) == region + 2 {
                DELTA
            } else {
                0
            };
            be16(&mut data, delta as u16);
        }
    }
    let mut store = Vec::new();
    be16(&mut store, 1);
    be32(&mut store, 12);
    be16(&mut store, 1);
    be32(&mut store, 12 + regions.len() as u32);
    store.extend_from_slice(&regions);
    store.extend_from_slice(&data);
    let mut hvar = Vec::new();
    be16(&mut hvar, 1);
    be16(&mut hvar, 0);
    be32(&mut hvar, 20);
    be32(&mut hvar, 0);
    be32(&mut hvar, 0);
    be32(&mut hvar, 0);
    hvar.extend_from_slice(&store);
    tables.push((*b"HVAR", hvar));

    tables.sort_by_key(|(tag, _)| *tag);
    let count = tables.len() as u16;
    let mut font = Vec::new();
    be32(&mut font, 0x0001_0000);
    be16(&mut font, count);
    let search = 16 * 2u16.pow(15 - count.leading_zeros());
    be16(&mut font, search);
    be16(&mut font, (search / 16).trailing_zeros() as u16);
    be16(&mut font, count * 16 - search);
    let mut offset = 12 + 16 * tables.len();
    let mut body = Vec::new();
    for (tag, data) in &tables {
        font.extend_from_slice(tag);
        be32(&mut font, 0);
        be32(&mut font, offset as u32);
        be32(&mut font, data.len() as u32);
        body.extend_from_slice(data);
        let padded = data.len().next_multiple_of(4);
        body.resize(body.len() + padded - data.len(), 0);
        offset += padded;
    }
    font.extend_from_slice(&body);
    font
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_font_parses_with_its_axes() {
        let font = variable_font();
        let face = skrifa::FontRef::new(&font).unwrap();
        use skrifa::MetadataProvider as _;
        let axes: Vec<_> = face
            .axes()
            .iter()
            .map(|axis| axis.tag().to_be_bytes())
            .collect();
        assert_eq!(axes, AXES.map(|(tag, ..)| tag));
        assert!(face.charmap().map('w').is_some());
        assert!(face.charmap().map(PLAIN_CHAR).is_some());
    }
}
