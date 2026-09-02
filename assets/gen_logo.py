#!/usr/bin/env python3
"""Generate every tdy logo asset from one pixel-grid definition.

The mark is "messy in, tidy out": scattered cells on the left resolve into a
sorted three-column table (slate header row, one color per column) on the
right. The pixels ARE table cells, which is why one 16x16 definition serves
every context: README banner, favicon-size icons, a 1280x640 social card,
and a half-block ANSI rendering for the terminal.

Like testdata/, everything in assets/ except this script is generated —
edit the definitions here and re-run:

    python3 assets/gen_logo.py

stdlib only (the PNGs are written by hand via zlib), so regeneration is
byte-deterministic: same script, same bytes.

Outputs (all into assets/):
  logo-mark.svg            the square mark alone, fractional offsets kept
  logo-light.svg           mark + pixel wordmark, ink for light grounds
  logo-dark.svg            same lockup, ink for dark grounds
  logo-16.png              mark snapped to a 16x16 integer raster (favicon)
  logo-32.png, logo-64.png mark at exact 2x / 4x (offsets land on pixels)
  logo-banner-light.png    lockup at 8x on transparent, light-ground ink
  logo-banner-dark.png     lockup at 8x on transparent, dark-ground ink
  social-card.png          1280x640 dark card for the GitHub social preview
  logo.ansi                truecolor half-block rendering: cat assets/logo.ansi
"""

import struct
import zlib
from pathlib import Path

OUT = Path(__file__).resolve().parent

# ---------------------------------------------------------------------------
# Palette. Mid-value hues chosen to hold on both white and GitHub-dark.
# ---------------------------------------------------------------------------
COLORS = {
    "a": (0xD9, 0x82, 0x2B),  # amber   - column one
    "t": (0x2A, 0x9D, 0x8F),  # teal    - column two
    "c": (0xD9, 0x5F, 0x4C),  # coral   - column three
    "s": (0x78, 0x81, 0x8F),  # slate   - header row / structure
}
INK_LIGHT = (0x2B, 0x2E, 0x36)  # wordmark on light grounds
INK_DARK = (0xDF, 0xE2, 0xE8)  # wordmark on dark grounds
CARD_GROUND = (0x0D, 0x11, 0x17)  # GitHub dark

# ---------------------------------------------------------------------------
# The mark, V1 "Ledger": rects as (x, y, w, h, color) on a 16x16 canvas.
# Scatter is art-directed, not random — half-cell offsets are deliberate
# misalignment (the messy file); the table is exact.
# ---------------------------------------------------------------------------
SCATTER = [
    (0, 2, 2, 1, "a"), (3, 0.5, 1, 1, "c"), (5, 1.5, 1, 1, "t"),
    (1, 4.5, 1, 2, "t"), (4, 3, 1, 1, "s"), (0, 7, 1, 1, "c"),
    (2.5, 6, 2, 1, "a"), (5, 5, 1, 1, "a"), (1, 9.5, 2, 1, "c"),
    (4, 8, 1, 1.5, "t"), (0, 12, 1, 1, "a"), (3, 11, 1, 1, "t"),
    (5.5, 10.5, 1, 1, "c"), (2, 13.5, 2, 1, "s"), (5, 13, 1, 1, "a"),
]

def table():
    cols = [(8, "a"), (11, "t"), (14, "c")]
    rects = [(x, 1, 2, 2, "s") for x, _ in cols]          # header row
    for x, color in cols:
        for y in (4, 7, 10, 13):                           # four data rows
            rects.append((x, y, 2, 2, color))
    return rects

MARK = SCATTER + table()

# The wordmark: "tdy" in the same pixel module, 4x8 per glyph, 1 column gap.
GLYPHS = {
    "t": [".#..", ".#..", "####", ".#..", ".#..", ".###", "....", "...."],
    "d": ["...#", "...#", ".###", "#..#", "#..#", ".###", "....", "...."],
    "y": ["....", "....", "#..#", "#..#", "#..#", ".###", "...#", ".##."],
}

def wordmark_rects(ink_key):
    rects = []
    for li, letter in enumerate("tdy"):
        for y, row in enumerate(GLYPHS[letter]):
            for x, ch in enumerate(row):
                if ch == "#":
                    rects.append((li * 5 + x, y, 1, 1, ink_key))
    return rects  # 14 wide, 8 tall

# Lockup geometry, in mark units: mark 16 wide, 3 gap, wordmark 14 wide;
# wordmark dropped 4 units so its mass sits on the mark's vertical center.
LOCKUP_W, LOCKUP_H, WORD_X, WORD_Y = 33, 16, 19, 4

def lockup(ink_key):
    return MARK + [
        (WORD_X + x, WORD_Y + y, w, h, c)
        for x, y, w, h, c in wordmark_rects(ink_key)
    ]

# ---------------------------------------------------------------------------
# SVG
# ---------------------------------------------------------------------------
def fnum(v):
    return f"{v:g}"

def svg(rects, w, h, colors, unit=8):
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w * unit}" '
        f'height="{h * unit}" viewBox="0 0 {w} {h}" '
        f'shape-rendering="crispEdges" role="img" aria-label="tdy">'
    ]
    for x, y, rw, rh, c in rects:
        r, g, b = colors[c]
        parts.append(
            f'<rect x="{fnum(x)}" y="{fnum(y)}" width="{fnum(rw)}" '
            f'height="{fnum(rh)}" fill="#{r:02x}{g:02x}{b:02x}"/>'
        )
    parts.append("</svg>\n")
    return "".join(parts)

# ---------------------------------------------------------------------------
# PNG (hand-rolled: RGBA8, zlib level 9, no ancillary chunks => deterministic)
# ---------------------------------------------------------------------------
def write_png(path, width, height, pixel):  # pixel(x, y) -> (r, g, b, a)
    rows = []
    for y in range(height):
        row = bytearray(b"\x00")
        for x in range(width):
            row += bytes(pixel(x, y))
        rows.append(bytes(row))

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data)) + tag + data
            + struct.pack(">I", zlib.crc32(tag + data))
        )

    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"".join(rows), 9))
        + chunk(b"IEND", b"")
    )
    path.write_bytes(png)

def painter(rects, unit, colors, ground=None):
    """pixel() over rects scaled by `unit`. Every coordinate must land on an
    integer at this scale — half-cell offsets need an even unit."""
    grid = {}
    for x, y, w, h, c in rects:
        x0, y0 = x * unit, y * unit
        x1, y1 = (x + w) * unit, (y + h) * unit
        for k in (x0, y0, x1, y1):
            if k != int(k):
                raise SystemExit(f"unit {unit} puts {k} between pixels")
        for yy in range(int(y0), int(y1)):
            for xx in range(int(x0), int(x1)):
                grid[(xx, yy)] = colors[c]

    def pixel(x, y):
        rgb = grid.get((x, y))
        if rgb is not None:
            return (*rgb, 255)
        return (*ground, 255) if ground else (0, 0, 0, 0)

    return pixel

def snapped_raster(rects):
    """The mark on a 16x16 integer grid: half offsets snap toward the cell
    they mostly cover. This is the hand-snapped favicon (and the ANSI art) —
    fractional pixels at 1x would only antialias into mush."""
    snap = lambda v: int(v + 0.5)
    grid = [[None] * 16 for _ in range(16)]
    for x, y, w, h, c in rects:
        for yy in range(snap(y), snap(y + h)):
            for xx in range(snap(x), snap(x + w)):
                if 0 <= xx < 16 and 0 <= yy < 16:
                    grid[yy][xx] = c
    return grid

# ---------------------------------------------------------------------------
# ANSI half-blocks: one character per 1x2 pixel pair, truecolor.
# ---------------------------------------------------------------------------
def ansi(grid):
    fg = lambda c: "\x1b[38;2;%d;%d;%dm" % COLORS[c]
    bg = lambda c: "\x1b[48;2;%d;%d;%dm" % COLORS[c]
    lines = []
    for y in range(0, 16, 2):
        line = []
        for x in range(16):
            top, bot = grid[y][x], grid[y + 1][x]
            if not top and not bot:
                line.append(" ")
            elif top and bot:
                line.append(fg(top) + bg(bot) + "▀\x1b[0m")
            elif top:
                line.append(fg(top) + "▀\x1b[0m")
            else:
                line.append(fg(bot) + "▄\x1b[0m")
        lines.append("".join(line))
    return "\n".join(lines) + "\n"

# ---------------------------------------------------------------------------
# Rust: the same 16x16 raster, as a `const` grid the terminal UI renders with
# half-block glyphs. `grid` here already holds RGB tuples (or None) — the
# color-key lookup happens in `main`, once, the same way `logo-16.png` does
# it, rather than teaching this function about `COLORS`.
# ---------------------------------------------------------------------------
def emit_rust(grid):
    lines = [
        "//! GENERATED by assets/gen_logo.py — edit the definitions there, never this file.",
        "//! The mark's 16x16 snapped raster; None is transparent.",
        "#![cfg_attr(rustfmt, rustfmt::skip)]",
        "pub const WIDTH: usize = 16;",
        "pub const HEIGHT: usize = 16;",
        "pub const GRID: [[Option<(u8, u8, u8)>; WIDTH]; HEIGHT] = [",
    ]
    for row in grid:
        cells = []
        for px in row:
            if px is None:
                cells.append("None")
            else:
                r, g, b = px[:3]
                cells.append(f"Some(({r}, {g}, {b}))")
        lines.append("    [" + ", ".join(cells) + "],")
    lines.append("];")
    return "\n".join(lines) + "\n"

# ---------------------------------------------------------------------------
def main():
    light = {**COLORS, "i": INK_LIGHT}
    dark = {**COLORS, "i": INK_DARK}

    (OUT / "logo-mark.svg").write_text(svg(MARK, 16, 16, COLORS))
    (OUT / "logo-light.svg").write_text(svg(lockup("i"), LOCKUP_W, LOCKUP_H, light))
    (OUT / "logo-dark.svg").write_text(svg(lockup("i"), LOCKUP_W, LOCKUP_H, dark))

    snapped = snapped_raster(MARK)
    write_png(
        OUT / "logo-16.png", 16, 16,
        lambda x, y: (*COLORS[snapped[y][x]], 255) if snapped[y][x] else (0, 0, 0, 0),
    )
    write_png(OUT / "logo-32.png", 32, 32, painter(MARK, 2, COLORS))
    write_png(OUT / "logo-64.png", 64, 64, painter(MARK, 4, COLORS))

    u = 8  # banner: mark 128px tall
    write_png(
        OUT / "logo-banner-light.png", LOCKUP_W * u, LOCKUP_H * u,
        painter(lockup("i"), u, light),
    )
    write_png(
        OUT / "logo-banner-dark.png", LOCKUP_W * u, LOCKUP_H * u,
        painter(lockup("i"), u, dark),
    )

    # Social card: the lockup at 24x, centered on the GitHub-dark ground.
    u, w, h = 24, 1280, 640
    ox, oy = (w - LOCKUP_W * u) // 2, (h - LOCKUP_H * u) // 2
    inner = painter(lockup("i"), u, dark)
    write_png(
        OUT / "social-card.png", w, h,
        lambda x, y: (
            inner(x - ox, y - oy)
            if inner(x - ox, y - oy)[3] else (*CARD_GROUND, 255)
        ),
    )

    (OUT / "logo.ansi").write_text(ansi(snapped))

    rgb_grid = [[COLORS[c] if c else None for c in row] for row in snapped]
    mark_rs = OUT.parent / "tdy-tui" / "src" / "mark.rs"
    mark_rs.write_text(emit_rust(rgb_grid))

    print("assets regenerated:", ", ".join(sorted(p.name for p in OUT.iterdir())))

if __name__ == "__main__":
    main()
