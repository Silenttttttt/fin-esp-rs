#!/usr/bin/env python3
"""
Convert an image to a 5×8 binary grid: dark pixels = 1 (on), light = 0 (off).
Also prints the bit-inverted grid (0↔1).

Sampling:
  - If the image is exactly 5×8 pixels, each pixel is one cell.
  - With --sample-centers --first-center X,Y (after --crop): one pixel per cell at
    (X + col·W/5, Y + row·H/8) on the cropped image (Fin-ESP preset: 45,38 on 464×740 grid).
  - With --cell-grid FX,FY,CW,CH: mean luminance per fixed cell.
  - Otherwise the image is split evenly into 5×8 cells (mean per cell).

Requires: pillow (PIL)
  pip install pillow

Examples:
  python3 tools/image_to_5x8_grid.py hourglass/some.png
  python3 tools/image_to_5x8_grid.py capture.png --threshold 140
  python3 tools/image_to_5x8_grid.py editor.png --crop 111,108,400,640 --json
  python3 tools/image_to_5x8_grid.py shot.png --crop 111,108,511,748 --visualize
  python3 tools/image_to_5x8_grid.py shot.png --crop ... --bytes | \\
      python3 tools/lcd_glyph_visualize.py --stdin

  python3 tools/image_to_5x8_grid.py --batch-dir hourglass --fin-esp-hourglass --emit-c
  # Add --invert-lcd-rows only if your HD44780 module inverts segment polarity vs these bytes.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def grid_to_row_bytes(grid: list[list[int]]) -> list[int]:
    """5×8 grid (row major, cols 0–4 left→right) → 8 HD44780 row bytes (bit 4 = left dot)."""
    rows: list[int] = []
    for y in range(8):
        b = 0
        for x in range(5):
            if grid[y][x]:
                b |= 1 << (4 - x)
        rows.append(b & 0x1F)
    return rows


def _load_visualize():
    tools_dir = Path(__file__).resolve().parent
    if str(tools_dir) not in sys.path:
        sys.path.insert(0, str(tools_dir))
    from lcd_glyph_visualize import visualize_frame

    return visualize_frame


def parse_crop(s: str) -> tuple[int, int, int, int]:
    parts = [int(x.strip()) for x in s.replace(" ", "").split(",")]
    if len(parts) != 4:
        raise argparse.ArgumentTypeError("crop must be x0,y0,x1,y1 (four integers)")
    x0, y0, x1, y1 = parts
    if x1 <= x0 or y1 <= y0:
        raise argparse.ArgumentTypeError("crop: need x1>x0 and y1>y0")
    return (x0, y0, x1, y1)


def parse_pair_xy(s: str) -> tuple[int, int]:
    parts = [int(x.strip()) for x in s.replace(" ", "").replace("x", ",").split(",") if x.strip()]
    if len(parts) != 2:
        raise argparse.ArgumentTypeError("expected two integers, e.g. 175,118 or 74x81")
    return (parts[0], parts[1])


def parse_cell_grid(s: str) -> tuple[int, int, int, int]:
    """fx,fy,cw,ch — first cell top-left (relative to cropped image) and cell width/height."""
    parts = [int(x.strip()) for x in s.replace(" ", "").split(",")]
    if len(parts) != 4:
        raise argparse.ArgumentTypeError("cell-grid must be fx,fy,cw,ch (four integers)")
    fx, fy, cw, ch = parts
    if cw < 1 or ch < 1:
        raise argparse.ArgumentTypeError("cell width/height must be positive")
    return (fx, fy, cw, ch)


def load_gray(path: Path):
    try:
        from PIL import Image
    except ImportError as e:
        print("Install pillow: pip install pillow", file=sys.stderr)
        raise SystemExit(1) from e
    im = Image.open(path).convert("L")
    return im


def cell_mean(gray, x0: int, y0: int, x1: int, y1: int) -> float:
    region = gray.crop((x0, y0, x1, y1))
    data = region.getdata()
    n = region.width * region.height
    if n == 0:
        return 255.0
    return sum(data) / n


def image_to_grid(
    gray,
    threshold: int,
    dark_is_on: bool,
    cell_grid: tuple[int, int, int, int] | None = None,
    center_anchor: tuple[int, int] | None = None,
) -> list[list[int]]:
    """Return 8 rows × 5 columns; values 0 or 1.

    If center_anchor is (cx, cy), sample that pixel for cell (0,0); cell (col,row) center is
    (cx + col·W/5, cy + row·H/8). Otherwise if cell_grid is set, mean per fixed cell; else mean per equal tile.
    """
    w, h = gray.size
    grid: list[list[int]] = []

    def classify(lum: float) -> bool:
        if dark_is_on:
            return lum <= threshold
        return lum > threshold

    for yy in range(8):
        row: list[int] = []
        for xx in range(5):
            if center_anchor is not None:
                ax, ay = center_anchor
                dx = w / 5.0
                dy = h / 8.0
                px = int(round(ax + xx * dx))
                py = int(round(ay + yy * dy))
                px = min(max(0, px), w - 1)
                py = min(max(0, py), h - 1)
                lum = float(gray.getpixel((px, py)))
                on = classify(lum)
            elif cell_grid is not None:
                fx, fy, cw, ch = cell_grid
                x0 = fx + xx * cw
                y0 = fy + yy * ch
                x1 = x0 + cw
                y1 = y0 + ch
                mean = cell_mean(gray, x0, y0, x1, y1)
                on = classify(mean)
            else:
                x0 = xx * w // 5
                x1 = (xx + 1) * w // 5
                y0 = yy * h // 8
                y1 = (yy + 1) * h // 8
                mean = cell_mean(gray, x0, y0, x1, y1)
                on = classify(mean)
            row.append(1 if on else 0)
        grid.append(row)
    return grid


def invert_grid(g: list[list[int]]) -> list[list[int]]:
    return [[1 - v for v in row] for row in g]


def format_grid_lines(g: list[list[int]]) -> list[str]:
    return [" ".join(str(v) for v in row) for row in g]


def frame_bytes_from_image(
    gray,
    threshold: int,
    dark_is_on: bool,
    cell_grid: tuple[int, int, int, int] | None = None,
    center_anchor: tuple[int, int] | None = None,
) -> list[int]:
    g = image_to_grid(
        gray,
        threshold,
        dark_is_on=dark_is_on,
        cell_grid=cell_grid,
        center_anchor=center_anchor,
    )
    return grid_to_row_bytes(g)


def lcd_row_xor_invert(rows: list[int]) -> list[int]:
    """Flip each row’s 5 dots for LCDs with inverted segment polarity (HD44780: XOR lower 5 bits)."""
    return [(b & 0x1F) ^ 0x1F for b in rows]


def format_c_array(
    frames: list[list[int]],
    count_name: str,
    array_name: str,
) -> str:
    n = len(frames)
    lines = [
        f"static const uint8_t {count_name} = {n};",
        f"static byte {array_name}[{count_name}][8] = {{",
    ]
    for row_bytes in frames:
        parts = ", ".join(f"0x{b:02X}" for b in row_bytes)
        lines.append(f"    {{{parts}}},")
    lines.append("};")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Image → 5×8 binary grid (dark=1). Also prints inverted grid."
    )
    ap.add_argument(
        "image",
        nargs="?",
        type=Path,
        default=None,
        help="One image (omit when using --batch-dir)",
    )
    ap.add_argument(
        "--batch-dir",
        type=Path,
        metavar="DIR",
        help="Process all *.png in DIR, sorted by filename (e.g. hourglass frames)",
    )
    ap.add_argument(
        "--threshold",
        type=int,
        default=128,
        help="Luminance 0–255: sampled pixel/mean ≤ this ⇒ 1 when dark-on; or > when --bright-on (default: 128)",
    )
    ap.add_argument(
        "--bright-on",
        action="store_true",
        help="Treat bright pixels as on (mean > threshold); default is dark=on",
    )
    ap.add_argument(
        "--crop",
        type=parse_crop,
        metavar="X0,Y0,X1,Y1",
        help="Use only this rectangle before gridding (pixel coords, exclusive end)",
    )
    ap.add_argument(
        "--cell-grid",
        type=parse_cell_grid,
        default=None,
        metavar="FX,FY,CW,CH",
        help="Fixed 5×8 lattice: top-left of cell (0,0) and cell size, relative to cropped image (after --crop)",
    )
    ap.add_argument(
        "--first-cell-abs",
        type=parse_pair_xy,
        default=None,
        metavar="X,Y",
        help="Corner of cell (0,0) in full-image coords; requires --crop and --cell-size (see --first-cell-br)",
    )
    ap.add_argument(
        "--first-cell-br",
        action="store_true",
        help="With --first-cell-abs: point is bottom-right of cell (0,0); top-left = point minus cell size",
    )
    ap.add_argument(
        "--cell-size",
        type=parse_pair_xy,
        default=None,
        metavar="W,H",
        help="Cell width and height in pixels (e.g. 74,81); use with --first-cell-abs or alone with --cell-grid",
    )
    ap.add_argument(
        "--sample-centers",
        action="store_true",
        help="Sample one pixel at each cell’s center (use with --first-center); excludes --cell-grid",
    )
    ap.add_argument(
        "--first-center",
        type=parse_pair_xy,
        default=None,
        metavar="X,Y",
        help="Center of cell (0,0) after crop; other centers at +col·W/5, +row·H/8 (Fin-ESP default 45,38)",
    )
    ap.add_argument(
        "--json",
        action="store_true",
        help="Emit JSON with keys grid, inverted",
    )
    ap.add_argument(
        "--bytes",
        action="store_true",
        help="Print only 8 hex row bytes (for piping to lcd_glyph_visualize.py --stdin)",
    )
    ap.add_argument(
        "--visualize",
        action="store_true",
        help="ASCII preview via lcd_glyph_visualize (main grid + inverted)",
    )
    ap.add_argument(
        "--emit-c",
        action="store_true",
        help="Emit static byte NAME[COUNT][8] = { ... }; (batch or single image)",
    )
    ap.add_argument(
        "--count-name",
        default="HOURGLASS_FRAME_COUNT",
        help="C symbol for frame count (with --emit-c)",
    )
    ap.add_argument(
        "--array-name",
        default="GLYPH_HOURGLASS",
        help="C array base name (with --emit-c)",
    )
    ap.add_argument(
        "--fin-esp-hourglass",
        action="store_true",
        help="Preset: crop 81,26,545,766; centers (45,38)+…; th 145; black=on",
    )
    ap.add_argument(
        "--fin-esp-hourglass-legacy",
        action="store_true",
        help="Old preset: crop 111,108,511,748, equal split, threshold 115, bright dots on (png_to_lcd_glyphs style)",
    )
    ap.add_argument(
        "--invert-lcd-rows",
        action="store_true",
        help="XOR each row byte with 0x1F before output (default with --fin-esp-hourglass for Fin-ESP LCD)",
    )
    args = ap.parse_args()

    if args.fin_esp_hourglass and args.fin_esp_hourglass_legacy:
        ap.print_help()
        print("\nUse only one of --fin-esp-hourglass or --fin-esp-hourglass-legacy", file=sys.stderr)
        return 2

    if args.first_cell_abs is not None:
        if args.crop is None or args.cell_size is None:
            print("--first-cell-abs requires --crop and --cell-size", file=sys.stderr)
            return 1
        if args.cell_grid is not None:
            print("Use either --cell-grid or --first-cell-abs, not both", file=sys.stderr)
            return 1
        x0, y0, _, _ = args.crop
        ax, ay = args.first_cell_abs
        cw, ch = args.cell_size
        if args.first_cell_br:
            args.cell_grid = (ax - x0 - cw, ay - y0 - ch, cw, ch)
        else:
            args.cell_grid = (ax - x0, ay - y0, cw, ch)

    if args.fin_esp_hourglass_legacy:
        if args.crop is None:
            args.crop = parse_crop("111,108,511,748")
        args.threshold = 115
        args.bright_on = True
        args.cell_grid = None

    if args.fin_esp_hourglass:
        if args.crop is None:
            args.crop = parse_crop("81,26,545,766")
        args.threshold = 145
        args.bright_on = False
        if args.cell_grid is None and args.first_center is None:
            args.first_center = (45, 38)

    if args.sample_centers and args.cell_grid is not None:
        print("Use either --sample-centers or --cell-grid, not both", file=sys.stderr)
        return 1
    if args.sample_centers and args.first_center is None and not args.fin_esp_hourglass:
        print("--sample-centers requires --first-center", file=sys.stderr)
        return 1

    center_anchor: tuple[int, int] | None = None
    if args.cell_grid is None and args.first_center is not None:
        if args.fin_esp_hourglass or args.sample_centers:
            center_anchor = args.first_center

    if args.batch_dir:
        if not args.batch_dir.is_dir():
            print(f"Not a directory: {args.batch_dir}", file=sys.stderr)
            return 1
        paths = sorted(args.batch_dir.glob("*.png"))
        if not paths:
            print(f"No *.png in {args.batch_dir}", file=sys.stderr)
            return 1
        dark_on = not args.bright_on
        frames: list[list[int]] = []
        for p in paths:
            gray = load_gray(p)
            if args.crop:
                gray = gray.crop(args.crop)
            fr = frame_bytes_from_image(
                gray,
                args.threshold,
                dark_on,
                cell_grid=args.cell_grid,
                center_anchor=center_anchor,
            )
            if args.invert_lcd_rows:
                fr = lcd_row_xor_invert(fr)
            frames.append(fr)
        if args.emit_c:
            print(format_c_array(frames, args.count_name, args.array_name))
            return 0
        for i, p in enumerate(paths):
            parts = ", ".join(f"0x{b:02X}" for b in frames[i])
            print(f"{i:3}  {p.name}  {parts}")
        return 0

    if args.image is None or not args.image.is_file():
        print("Provide a file path or use --batch-dir DIR", file=sys.stderr)
        return 1

    gray = load_gray(args.image)
    if args.crop:
        gray = gray.crop(args.crop)

    dark_on = not args.bright_on
    grid = image_to_grid(
        gray,
        args.threshold,
        dark_is_on=dark_on,
        cell_grid=args.cell_grid,
        center_anchor=center_anchor,
    )
    inv = invert_grid(grid)
    row_bytes = grid_to_row_bytes(grid)
    inv_bytes = grid_to_row_bytes(inv)
    if args.invert_lcd_rows:
        row_bytes = lcd_row_xor_invert(row_bytes)
        inv_bytes = lcd_row_xor_invert(inv_bytes)

    if args.emit_c:
        print(format_c_array([row_bytes], args.count_name, args.array_name))
        return 0

    if args.json:
        obj = {
            "grid": grid,
            "inverted": inv,
            "row_bytes": [f"0x{b:02X}" for b in row_bytes],
            "row_bytes_inverted": [f"0x{b:02X}" for b in inv_bytes],
            "threshold": args.threshold,
            "dark_is_on": dark_on,
            "source_size": [gray.width, gray.height],
        }
        print(json.dumps(obj, indent=2))
        return 0

    if args.bytes:
        print(" ".join(f"0x{b:02X}" for b in row_bytes))
        return 0

    if args.visualize:
        visualize_frame = _load_visualize()
        print(visualize_frame(row_bytes, "--- image → grid (dark = on) ---", False))
        print()
        print(visualize_frame(inv_bytes, "--- inverted grid (1↔0) ---", False))
        return 0

    print("5×8 — dark=1 (on), light=0 (off)")
    for line in format_grid_lines(grid):
        print(line)
    print()
    print("Inverted (1↔0)")
    for line in format_grid_lines(inv):
        print(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
