//! CP437 ("IBM PC") to Unicode translation for BBS ANSI art.
//!
//! Classic BBSes draw with bytes 0x80-0xFF (box drawing, shades, symbols),
//! which are invalid UTF-8 and would render as garbage in the vt100
//! emulator. Bytes below 0x80 coincide with ASCII and pass through,
//! including the C0 control range, which telnet uses as controls rather
//! than the CP437 dingbats.

/// Unicode equivalents of CP437 bytes 0x80..=0xFF.
const HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', // 0x80
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', // 0x90
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', // 0xA0
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', // 0xB0
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', // 0xC0
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', // 0xD0
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', // 0xE0
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■',
    '\u{a0}', // 0xF0
];

/// Translate a CP437 byte stream into UTF-8.
pub fn decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut buf = [0u8; 4];
    for &byte in data {
        if byte < 0x80 {
            out.push(byte);
        } else {
            let ch = HIGH[(byte - 0x80) as usize];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

/// Encode user text without substituting an unrelated byte for an unsupported
/// character. The caller retains the draft/paste and can select UTF-8 instead.
pub fn encode(text: &str) -> Result<Vec<u8>, char> {
    text.chars()
        .map(|character| {
            if character.is_ascii() {
                Ok(character as u8)
            } else {
                HIGH.iter()
                    .position(|&mapped| mapped == character)
                    .map(|index| index as u8 + 0x80)
                    .ok_or(character)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn translates_box_drawing_and_passes_ascii() {
        assert_eq!(decode(b"\xC9\xCD\xBB"), "╔═╗".as_bytes());
        assert_eq!(decode(b"\xB0\xB1\xB2\xDB"), "░▒▓█".as_bytes());
        assert_eq!(decode(b"plain ascii\r\n"), b"plain ascii\r\n");
        // Escape sequences pass through untouched.
        assert_eq!(decode(b"\x1b[1;35m"), b"\x1b[1;35m");
    }
}
