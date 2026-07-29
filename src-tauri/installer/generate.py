#!/usr/bin/env python3
"""Generate the installer artwork committed next to this file.

    python src-tauri/installer/generate.py

Outputs, all committed so a build never depends on this script running:

    header.bmp          150x57   NSIS page header (Tauri: bundle.windows.nsis.headerImage)
    sidebar.bmp         164x314  NSIS welcome/finish panel (sidebarImage)
    dmg-background.png  660x400  macOS DMG window (bundle.macOS.dmg.background)

The two BMPs are 24-bit, which is what NSIS wants; an alpha channel there
renders as garbage rather than transparency, so everything is flattened onto
the graphite ground.

The mark is drawn rather than scaled from src-tauri/icons/, because those carry
the rounded graphite tile baked in. On a graphite panel the tile would be
invisible and only its corners would show. Geometry is taken from
../../app-icon.svg, which stays the source of truth: two concentric open arcs
on a 1024 grid, centre 512,512 — outer r340 stroke 88 with a 96 deg gap, inner
r224 stroke 76 with a 76 deg gap, both opening right.

Requires Pillow. Text uses Segoe UI, so regenerating on a machine without it
will substitute a different face; the committed output is the reference.
"""

from PIL import Image, ImageDraw, ImageFont
import os

HERE = os.path.dirname(os.path.abspath(__file__))

GRAPHITE = (0x1F, 0x25, 0x2C)
GRAPHITE_DEEP = (0x14, 0x19, 0x1E)
CLOUD = (0xF6, 0xF7, 0xF8)
STEEL = (0x4E, 0x78, 0x96)
MUTED = (0x8A, 0x97, 0xA3)

SS = 4  # supersampling factor; arcs are drawn large and downsampled


def font(name, size):
    for candidate in (name, "segoeui.ttf", "arial.ttf"):
        try:
            return ImageFont.truetype(candidate, size)
        except OSError:
            continue
    return ImageFont.load_default()


def draw_mark(size):
    """The nested-C monogram, transparent background, `size` px square."""
    px = size * SS
    img = Image.new("RGBA", (px, px), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    def arc(radius, stroke, gap_deg, colour):
        # 1024-grid values scaled to this canvas.
        r = radius / 1024 * px
        w = max(1, round(stroke / 1024 * px))
        c = px / 2
        box = (c - r, c - r, c + r, c + r)
        half = gap_deg / 2
        # Pillow measures degrees clockwise from 3 o'clock, so the gap sits on
        # the right by starting after it and ending before it.
        d.arc(box, start=half, end=360 - half, fill=colour, width=w)

    arc(340, 88, 96, CLOUD)
    arc(224, 76, 76, STEEL)
    return img.resize((size, size), Image.LANCZOS)


def vertical_gradient(size, top, bottom):
    w, h = size
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(h):
        t = y / max(1, h - 1)
        d.line(
            [(0, y), (w, y)],
            fill=tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(3)),
        )
    return img


def save_bmp(img, name):
    path = os.path.join(HERE, name)
    img.convert("RGB").save(path, "BMP")
    print(f"  {name}  {img.size[0]}x{img.size[1]}")


# --- header.bmp: 150x57, sits in the NSIS page header -----------------------
# MUI pins this flush to the window's right edge, so whatever sits at the right
# of the bitmap sits against the window border. An earlier draft carried a
# "Claude Code accounts" subtitle here and left 3px of right padding, which read
# as clipped text rather than a caption. 150x57 has room for the mark and the
# wordmark, and nothing else.
def header():
    img = Image.new("RGB", (150, 57), GRAPHITE)
    mark = draw_mark(34)
    img.paste(mark, (14, (57 - 34) // 2), mark)

    d = ImageDraw.Draw(img)
    f = font("segoeuib.ttf", 15)
    text = "CC Logins"
    bbox = d.textbbox((0, 0), text, font=f)
    d.text((56, (57 - (bbox[3] - bbox[1])) / 2 - bbox[1]), text, font=f, fill=CLOUD)
    save_bmp(img, "header.bmp")


# --- sidebar.bmp: 164x314, the welcome and finish panels --------------------
def sidebar():
    img = vertical_gradient((164, 314), GRAPHITE, GRAPHITE_DEEP)
    mark = draw_mark(92)
    img.paste(mark, ((164 - 92) // 2, 54), mark)

    d = ImageDraw.Draw(img)

    def centred(y, text, f, fill):
        w = d.textbbox((0, 0), text, font=f)[2]
        d.text(((164 - w) / 2, y), text, font=f, fill=fill)

    centred(168, "CC Logins", font("segoeuib.ttf", 19), CLOUD)
    centred(194, "Quota visibility and", font("segoeui.ttf", 10), MUTED)
    centred(208, "account switching", font("segoeui.ttf", 10), MUTED)

    # A steel rule echoing the inner arc, well clear of the NSIS button row.
    d.line([(46, 232), (118, 232)], fill=STEEL, width=1)
    centred(244, "for Claude Code", font("segoeui.ttf", 10), STEEL)
    save_bmp(img, "sidebar.bmp")


# --- dmg-background.png: 660x400 --------------------------------------------
# Matches bundle.macOS.dmg.windowSize. The drag arrow is positioned against
# appPosition (180,170) and applicationFolderPosition (480,170), which are icon
# centres in this same coordinate space — so the arrow spans the gap between
# them and nothing is drawn underneath either icon.
def dmg():
    img = vertical_gradient((660, 400), GRAPHITE, GRAPHITE_DEEP)
    d = ImageDraw.Draw(img)

    mark = draw_mark(44)
    img.paste(mark, (24, 22), mark)
    d.text((78, 28), "CC Logins", font=font("segoeuib.ttf", 21), fill=CLOUD)
    d.text((80, 55), "Quota visibility and account switching for Claude Code",
           font=font("segoeui.ttf", 11), fill=MUTED)

    # Arrow between the two icons, at their shared centre height.
    y = 170
    x0, x1 = 268, 392
    d.line([(x0, y), (x1 - 12, y)], fill=STEEL, width=3)
    d.polygon([(x1, y), (x1 - 14, y - 8), (x1 - 14, y + 8)], fill=STEEL)

    def centred(y_, text, f, fill):
        w = d.textbbox((0, 0), text, font=f)[2]
        d.text(((660 - w) / 2, y_), text, font=f, fill=fill)

    centred(196, "Drag to install", font("segoeui.ttf", 12), MUTED)
    centred(330, "This build is unsigned — macOS will ask before opening it.",
            font("segoeui.ttf", 11), MUTED)
    centred(348, "Right-click the app and choose Open the first time.",
            font("segoeui.ttf", 11), STEEL)

    path = os.path.join(HERE, "dmg-background.png")
    img.save(path, "PNG")
    print(f"  dmg-background.png  {img.size[0]}x{img.size[1]}")


if __name__ == "__main__":
    print("writing installer artwork:")
    header()
    sidebar()
    dmg()
