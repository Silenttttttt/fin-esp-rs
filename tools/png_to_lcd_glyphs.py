#!/usr/bin/env python3
"""
Convert PNG(s) into C byte arrays for HD44780 / LiquidCrystal `createChar`
(5 dots × 8 rows per character).

Bit order matches Arduino LiquidCrystal_I2C: each row byte, bit 4 = leftmost dot.

Usage — raw pixel strip (each cell exactly 5×8 px wide in the PNG):
  python3 png_to_lcd_glyphs.py hourglass/frames.png --frames 10 -o glyphs.inc

Usage — Fin-ESP editor screenshots (one 5×8 character per PNG, UI chrome around it):
  python3 png_to_lcd_glyphs.py --screenshots-dir hourglass \\
      --crop-x0 111 --crop-y0 108 --cell-size 80 --on-if-above 115 -o glyphs.inc

  # Add `--lcd-row-invert` only if the physical LCD inverts segment polarity; it XORs non-zero rows (0x11 ↔ 0x0E).
  # Omit `--compact-vertical` for full 8-row sampling; tune `--crop-y0` / `--crop-dy` instead.

  # Single SOL (or any one glyph) — align grid; SOL sample often needs --crop-y0 72:
  python3 png_to_lcd_glyphs.py --editor-png tools/reference_sol_editor.png \\
      --crop-x0 111 --crop-y0 72 --cell-size 80 --on-if-above 115 \\
      --flat-c -n GLYPH_ASSET_SOL

Fin-ESP hourglass: `--crop-y0` ~108 (tune with `--crop-dy`). **`--lcd-row-invert`**: optional; XOR per row
swaps lit patterns—omit unless the module needs polarity correction. **`--mirror-v`**: only if upside-down.
**Left/right**: `--mirror-h`. **Lit/unlit in PNG**: `--invert-bits`. **Optional** `--compact-vertical`.

**Vertical alignment**: if the glyph looks **one row too high/low**, nudge with `--crop-dy` (pixels, can be
negative). Hourglass defaults use `--crop-y0 77`; a SOL capture from the same app may need a different
`--crop-y0` (e.g. 72) if the grid sits higher in the PNG.

HD44780 CGRAM: **exactly 5×8 dots** per character; only bits 0–4 of each row byte are used (bit 4 = left).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path


def row_to_byte(pixels: list[int], threshold: int) -> int:
    """pixels: 5 luminance values 0-255, left to right."""
    b = 0
    for x in range(5):
        on = pixels[x] <= threshold  # dark pixel = "on" for typical editor screenshot
        if on:
            b |= 1 << (4 - x)
    return b & 0x1F


def extract_frame(
    gray,
    left: int,
    top: int,
    threshold: int,
    invert: bool,
) -> list[int]:
    """Return 8 row bytes for one 5×8 cell."""
    rows: list[int] = []
    for y in range(8):
        pix = []
        for x in range(5):
            v = int(gray.getpixel((left + x, top + y)))
            pix.append(v)
        if invert:
            pix = [255 - v for v in pix]
        rows.append(row_to_byte(pix, threshold))
    return rows


def mirror_row5(b: int) -> int:
    """Mirror one row left↔right (HD44780: bit 4 = left)."""
    b &= 0x1F
    return int(f"{b:05b}"[::-1], 2)


def transform_glyph_rows(rows: list[int], mirror_h: bool, mirror_v: bool, invert_bits: bool) -> list[int]:
    out = [(r & 0x1F) for r in rows]
    if mirror_h:
        out = [mirror_row5(r) for r in out]
    if mirror_v:
        out = out[::-1]
    if invert_bits:
        out = [r ^ 0x1F for r in out]
    return out


def compact_vertical(rows: list[int], align: str = "top") -> list[int]:
    """Strip leading/trailing empty rows; pack into 8 rows (top- or center-aligned)."""
    r = [(b & 0x1F) for b in rows[:8]]
    while len(r) < 8:
        r.append(0)
    nz = [i for i, x in enumerate(r) if x != 0]
    if not nz:
        return [0] * 8
    i, j = nz[0], nz[-1]
    chunk = r[i : j + 1]
    h = len(chunk)
    if h > 8:
        chunk = chunk[:8]
        h = 8
    if align == "top":
        return chunk + [0] * (8 - h)
    if align == "center":
        top_pad = (8 - h) // 2
        bot_pad = 8 - h - top_pad
        return [0] * top_pad + chunk + [0] * bot_pad
    if align == "bottom":
        return [0] * (8 - h) + chunk
    raise ValueError(f"compact_vertical: unknown align {align!r}")


def lcd_row_invert(rows: list[int]) -> list[int]:
    """XOR row with 0x1F only for non-zero rows (blank stays 0). Swaps patterns (e.g. 0x11 ↔ 0x0E); use only if the LCD inverts segments."""
    out: list[int] = []
    for b in rows[:8]:
        x = b & 0x1F
        out.append((x ^ 0x1F) if x else 0)
    while len(out) < 8:
        out.append(0)
    return out[:8]


def extract_screenshot_glyph(gray, x0: int, y0: int, cw: int, ch: int, on_if_above: int) -> list[int]:
    """Each logical pixel is a cw×ch block; lit pixels are bright (mean > on_if_above)."""
    n = float(cw * ch)
    rows: list[int] = []
    for yy in range(8):
        b = 0
        for xx in range(5):
            box = (x0 + xx * cw, y0 + yy * ch, x0 + (xx + 1) * cw, y0 + (yy + 1) * ch)
            region = gray.crop(box)
            mean = sum(region.getdata()) / n
            if mean > on_if_above:
                b |= 1 << (4 - xx)
        rows.append(b & 0x1F)
    return rows


def format_c_array(name: str, frames: list[list[int]], flat_single: bool = False) -> str:
    n = len(frames)
    if flat_single and n == 1:
        parts = ", ".join(f"0x{b:02X}" for b in frames[0])
        return "\n".join(
            [
                f"// Generated by png_to_lcd_glyphs.py — single 5×8 glyph",
                f"static byte {name}[8] = {{ {parts} }};",
            ]
        )
    lines = [
        f"// Generated by png_to_lcd_glyphs.py — {n} frame(s), 5×8 each",
        f"static const uint8_t HOURGLASS_FRAME_COUNT = {n};",
        f"static byte {name}[HOURGLASS_FRAME_COUNT][8] = {{",
    ]
    for fi, rows in enumerate(frames):
        parts = ", ".join(f"0x{b:02X}" for b in rows)
        comma = "," if fi < len(frames) - 1 else ""
        lines.append(f"    {{{parts}}}{comma}")
    lines.append("};")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser(description="PNG → HD44780 5×8 glyph arrays")
    ap.add_argument(
        "image",
        nargs="?",
        type=Path,
        help="PNG path (strip or grid of 5×8 cells); omit if using --screenshots-dir",
    )
    ap.add_argument(
        "--frames",
        type=int,
        default=0,
        help="Number of frames in one horizontal row (auto: image_width//5 if h==8)",
    )
    ap.add_argument("--cell-w", type=int, default=5)
    ap.add_argument("--cell-h", type=int, default=8)
    ap.add_argument("--grid-cols", type=int, default=0, help="Grid: columns of cells")
    ap.add_argument("--grid-rows", type=int, default=1)
    ap.add_argument(
        "--threshold",
        type=int,
        default=128,
        help="Luminance threshold: pixel <= threshold counts as ON (dark ink)",
    )
    ap.add_argument(
        "--invert",
        action="store_true",
        help="Invert luminance (bright pixels = ON) — for light ink on dark BG",
    )
    ap.add_argument("-n", "--name", default="GLYPH_HOURGLASS", help="C array base name")
    ap.add_argument("-o", "--output", type=Path, help="Write .inc / .h (stdout if omitted)")
    ap.add_argument(
        "--screenshots-dir",
        type=Path,
        help="One PNG per frame (sorted by filename); use with crop options below",
    )
    ap.add_argument("--crop-x0", type=int, default=111, help="Left offset of 5×8 grid (screenshots)")
    ap.add_argument("--crop-y0", type=int, default=77, help="Top offset of 5×8 grid (screenshots)")
    ap.add_argument(
        "--crop-dx",
        type=int,
        default=0,
        help="Added to crop-x0 before sampling (fine horizontal nudge in pixels)",
    )
    ap.add_argument(
        "--crop-dy",
        type=int,
        default=0,
        help="Added to crop-y0 before sampling (fine vertical nudge, e.g. +5 to align grid)",
    )
    ap.add_argument(
        "--cell-size",
        type=int,
        default=80,
        help="Square pixel size in the screenshot for each logical dot (screenshots)",
    )
    ap.add_argument(
        "--on-if-above",
        type=int,
        default=115,
        help="Cell mean luminance above this ⇒ pixel ON (bright ink on dark BG)",
    )
    ap.add_argument(
        "--mirror-h",
        action="store_true",
        help="Mirror each row horizontally (fixes left/right swap vs LCD)",
    )
    ap.add_argument(
        "--mirror-v",
        action="store_true",
        help="Flip rows (top↔bottom)",
    )
    ap.add_argument(
        "--invert-bits",
        action="store_true",
        help="XOR each row with 0x1F (swap lit/unlit)",
    )
    ap.add_argument(
        "--editor-png",
        type=Path,
        metavar="PATH",
        help="Single Fin-ESP editor screenshot: same as --screenshots-dir with one file (no temp dir)",
    )
    ap.add_argument(
        "--flat-c",
        action="store_true",
        help="With exactly one frame: emit static byte name[8] = { ... }; (for SOL, WiFi, etc.)",
    )
    ap.add_argument(
        "--compact-vertical",
        action="store_true",
        help="Per frame: remove leading/trailing blank rows, then align in the 8-row cell",
    )
    ap.add_argument(
        "--compact-align",
        choices=("top", "center", "bottom"),
        default="top",
        help="With --compact-vertical: top, center, or bottom-align the non-blank row run in the 8 rows",
    )
    ap.add_argument(
        "--lcd-row-invert",
        action="store_true",
        help="After transforms/compact: XOR each non-zero row with 0x1F (LCD polarity; keeps blank rows 0)",
    )
    args = ap.parse_args()

    try:
        from PIL import Image
    except ImportError:
        print("Install Pillow: pip install Pillow", file=sys.stderr)
        return 1

    frames: list[list[int]] = []

    crop_x = args.crop_x0 + args.crop_dx
    crop_y = args.crop_y0 + args.crop_dy

    if args.editor_png is not None:
        p = args.editor_png
        im = Image.open(p).convert("L")
        w, h = im.size
        cw = ch = args.cell_size
        if crop_x + 5 * cw > w or crop_y + 8 * ch > h:
            print(f"Crop out of bounds for {p} ({w}×{h})", file=sys.stderr)
            return 1
        rows = extract_screenshot_glyph(im, crop_x, crop_y, cw, ch, args.on_if_above)
        rows = transform_glyph_rows(rows, args.mirror_h, args.mirror_v, args.invert_bits)
        frames.append(rows)
    elif args.screenshots_dir is not None:
        paths = sorted(args.screenshots_dir.glob("*.png"), key=lambda p: p.name)
        if not paths:
            print(f"No PNG files in {args.screenshots_dir}", file=sys.stderr)
            return 1
        cw = ch = args.cell_size
        for p in paths:
            im = Image.open(p).convert("L")
            w, h = im.size
            if crop_x + 5 * cw > w or crop_y + 8 * ch > h:
                print(f"Crop out of bounds for {p} ({w}×{h})", file=sys.stderr)
                return 1
            rows = extract_screenshot_glyph(
                im, crop_x, crop_y, cw, ch, args.on_if_above
            )
            rows = transform_glyph_rows(
                rows, args.mirror_h, args.mirror_v, args.invert_bits
            )
            frames.append(rows)
    elif args.image is not None and args.editor_png is None:
        img = Image.open(args.image).convert("L")
        w, h = img.size

        if args.grid_cols > 0:
            gc, gr = args.grid_cols, args.grid_rows
            for ry in range(gr):
                for cx in range(gc):
                    left = cx * args.cell_w
                    top = ry * args.cell_h
                    if left + args.cell_w > w or top + args.cell_h > h:
                        print(f"Cell ({cx},{ry}) out of bounds for image {w}×{h}", file=sys.stderr)
                        return 1
                    frames.append(extract_frame(img, left, top, args.threshold, args.invert))
        else:
            nf = args.frames
            if nf <= 0:
                if h == args.cell_h:
                    nf = w // args.cell_w
                else:
                    print("Set --frames or use image height == cell-h for auto count", file=sys.stderr)
                    return 1
            if w < nf * args.cell_w or h < args.cell_h:
                print(f"Image too small: need {nf * args.cell_w}×{args.cell_h}, got {w}×{h}", file=sys.stderr)
                return 1
            for i in range(nf):
                left = i * args.cell_w
                frames.append(extract_frame(img, left, 0, args.threshold, args.invert))
    else:
        print("Provide a PNG file, --screenshots-dir, or --editor-png", file=sys.stderr)
        return 1

    if args.compact_vertical:
        frames = [compact_vertical(f, args.compact_align) for f in frames]
    if args.lcd_row_invert:
        frames = [lcd_row_invert(f) for f in frames]

    out = format_c_array(args.name, frames, flat_single=args.flat_c)
    if args.output:
        args.output.write_text(out + "\n", encoding="utf-8")
        print(f"Wrote {args.output}", file=sys.stderr)
    else:
        print(out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
