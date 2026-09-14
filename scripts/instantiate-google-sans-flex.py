#!/usr/bin/env python3
"""Generate bundled static faces from the official Google Sans Flex release ZIP.

Requires fonttools==4.61.1. See assets/fonts/google-sans-flex/README.md.
"""

import io
import sys
import zipfile
from pathlib import Path

from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont


def main():
    destination = Path(__file__).resolve().parents[1] / "assets/fonts/google-sans-flex"
    with zipfile.ZipFile(sys.argv[1]) as archive:
        source = archive.read("GoogleSansFlex[GRAD,ROND,opsz,slnt,wdth,wght].ttf")
    for weight, label in [(400, "Regular"), (600, "SemiBold"), (700, "Bold")]:
        for italic in [False, True]:
            font = TTFont(io.BytesIO(source), recalcTimestamp=False)
            axes = {axis.axisTag: axis.defaultValue for axis in font["fvar"].axes}
            axes.update(wght=weight, slnt=-10 if italic else 0)
            font = instantiateVariableFont(font, axes, inplace=True)
            style = ("Italic" if weight == 400 else label + " Italic") if italic else label
            # Explicit legacy and typographic names keep weight/style matching
            # consistent across fontdb, CoreText, and system font discovery.
            family = "Google Sans Flex"
            legacy_family = family + " SemiBold" if weight == 600 else family
            legacy_style = ("Italic" if italic else "Regular") if weight == 600 else style
            postscript = "GoogleSansFlex-" + style.replace(" ", "")
            names = {1: legacy_family, 2: legacy_style, 3: "4.007;Devcroft;" + postscript,
                     4: family + " " + style, 6: postscript, 16: family, 17: style}
            for name_id, value in names.items():
                font["name"].removeNames(nameID=name_id)
                font["name"].setName(value, name_id, 3, 1, 0x409)
            font["OS/2"].usWeightClass = weight
            font["OS/2"].fsSelection &= ~((1 << 0) | (1 << 5) | (1 << 6) | (1 << 9))
            font["OS/2"].fsSelection |= (1 if italic else 0) | (32 if weight == 700 else 0)
            if weight == 400 and not italic:
                font["OS/2"].fsSelection |= 64
            font["head"].macStyle = (1 if weight == 700 else 0) | (2 if italic else 0)
            font.save(destination / (postscript + ".ttf"))


if __name__ == "__main__":
    main()
