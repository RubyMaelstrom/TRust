TRust changes to fontique 0.11.1
================================

- Font::synthesis emboldens only for a bold request (weight 600 and up)
  against a non-bold face (weight below 600), as Blink and Gecko do. CSS Fonts 4
  #font-weight-prop synthesizes bold faces "for families that lack actual bold
  faces"; upstream emboldened any request heavier than the matched face, so a
  `font-weight: 500` heading on a family with only Regular and Bold was drawn
  synthetically bold.
