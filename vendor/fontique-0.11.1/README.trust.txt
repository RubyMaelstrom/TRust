TRust changes to fontique 0.11.1
================================

- Font::synthesis emboldens only for a bold request (weight 600 and up)
  against a non-bold face (weight below 600), as Blink and Gecko do. CSS Fonts 4
  #font-weight-prop synthesizes bold faces "for families that lack actual bold
  faces"; upstream emboldened any request heavier than the matched face, so a
  `font-weight: 500` heading on a family with only Regular and Bold was drawn
  synthetically bold.

- CSS Fonts 4 #font-style-matching with ranges (src/matching.rs): a face
  supports the widths, weights, italic values and oblique angles of its
  `wdth`, `wght`, `ital` and `slnt` axes, or of its `@font-face`
  descriptors, and each matching step settles on the nearest supported
  value in the specified search order (11deg oblique threshold, the
  `normal` order oblique >= 0, italic >= 0, oblique < 0, italic < 0).
- FontInfoOverride::descriptors (FaceDescriptors): `@font-face` font-width,
  font-weight and font-style descriptors (`auto` matches as normal and does
  not clamp; a value or range replaces the font data's style). Synthesis
  applies the matched values through the axes clamped to descriptor ranges,
  and suggests synthetic bold or oblique only where the descriptors (or,
  without them, the font data) imply the face lacks the requested style.
- OpenType `slnt` and `post.italicAngle` count counter-clockwise degrees;
  CSS oblique angles count clockwise. Oblique requests set `slnt` to the
  negated angle and static oblique faces report the negated italic angle
  (upstream applied them unnegated, so `oblique 10deg` leaned backwards).
  An `italic` request on a `slnt`-only face applies the angle the italic
  search finds (>= 11deg ascending, then below 11deg descending).
- Synthesis::skew keeps fractional degrees (CSS oblique angles may be
  fractional).
