This test-only ASCII subset retains the original TrueType hint programs from
JetBrains Mono Bold (the installed Nerd Font build). It is not a runtime font.
The source is licensed under the accompanying SIL Open Font License.

Generated with HarfBuzz's `hb-subset`:

```sh
hb-subset /usr/share/fonts/JetBrainsMonoNerdFont/JetBrainsMonoNerdFont-Bold.ttf \
  --unicodes=20-7E --output-file=Bold-ASCII.ttf
```

Keeping ASCII, including the glyphs used for automatic-hinter blue zones,
preserves the fractional-size regression: an `F` at 11pt / 96dpi has 10 ink
rows with FreeType `FT_LOAD_TARGET_LIGHT`, compared with 12 under native
TrueType hinting. At 120dpi and 192dpi the light-hinted heights are 13 and 22.
These references were measured with fcft 3.3.3 and FreeType 2.14.3 on 2026-09-13.
