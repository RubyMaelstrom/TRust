//! Frontend-neutral TRust presentation constants.
//!
//! The terminal and desktop frontends deliberately share this palette.  RGB
//! values live here rather than in Ratatui or Vello types so neither frontend
//! becomes the style authority for the other.

pub type Rgb = [u8; 3];

pub const NEON_PINK: Rgb = [0xff, 0x2b, 0xd6];
pub const NEON_CYAN: Rgb = [0x00, 0xff, 0xf9];
pub const NEON_GREEN: Rgb = [0x39, 0xff, 0x14];
pub const PASTEL_GREEN: Rgb = [0xa8, 0xe6, 0xa1];
pub const AMBER: Rgb = [0xff, 0xb0, 0x00];
pub const DIM: Rgb = [0x6e, 0x4e, 0x9e];
pub const TEXT: Rgb = [0xc8, 0xc8, 0xdc];
pub const BG: Rgb = [0x0b, 0x02, 0x21];

/// The configured terminal face used by the user's TRust environment.
pub const TERMINAL_FONT_FAMILY: &str = "JetBrainsMono Nerd Font";
pub const TERMINAL_FONT_SIZE_PT: f32 = 11.0;
/// Foot's `weight=bold` request maps to the CSS/OpenType weight 700. Keeping
/// the weight alongside the family and size makes every graphical terminal
/// adapter request the same face instead of relying on a frontend default.
pub const TERMINAL_FONT_WEIGHT: f32 = 700.0;
/// CSS uses 96 reference pixels per inch and 72 points per inch.  Device
/// scale is applied later by the renderer, so it must not enter this value.
pub const TERMINAL_FONT_SIZE_CSS_PX: f32 = TERMINAL_FONT_SIZE_PT * (96.0 / 72.0);
