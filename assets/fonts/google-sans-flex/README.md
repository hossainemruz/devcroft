# Google Sans Flex

Markdown body text and headings use these embedded fonts. No system font
installation or runtime download is required. Code retains its monospace font.

Source: [official Google Sans Flex v4.007 release](https://github.com/googlefonts/googlesans-flex/releases/tag/v4.007).
Archive: `GoogleSansFlex-v4.007.zip`.
SHA-256: `b7375131bffc5eaaebeb65c69529ba10e7f7a9962b913488f6ef35c432ce19c9`.

The accompanying `OFL.txt` and `TRADEMARKS.md` come from
[Google Fonts](https://github.com/google/fonts/tree/main/ofl/googlesansflex).
The fonts are distributed under the SIL Open Font License 1.1.

## Static faces

GPUI's current font loader does not reliably instantiate variable weight and
slant axes. These files are static instances of the upstream variable font:
weights 400, 600, and 700, each at slant 0 and -10. Other axes retain upstream
defaults: optical size 18, width 100, grade 0, roundness 0. The slanted instances
are registered as italic so Markdown emphasis selects them. All glyphs are kept.
Font names and style flags are normalized for cross-platform face selection.

Regenerate from the downloaded official archive with Python and
`fonttools==4.61.1`:

```sh
python scripts/instantiate-google-sans-flex.py /path/to/GoogleSansFlex-v4.007.zip
```

Generated files are checked in and embedded with `include_bytes!`; FontTools
is only needed to regenerate them, not to build or run Devcroft.
