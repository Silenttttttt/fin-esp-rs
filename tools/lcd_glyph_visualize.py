#!/usr/bin/env python3
"""
Print HD44780-style 5×8 glyphs as ASCII art (bit 4 = left pixel, matching LiquidCrystal).

  python3 lcd_glyph_visualize.py 0x10 0x0F 0x11 ...
  python3 lcd_glyph_visualize.py --file glyph.inc
  echo '0x10,0x0F' | python3 lcd_glyph_visualize.py --stdin
  python3 lcd_glyph_visualize.py --invert 0x10 0x0F ...   # XOR 0x1F per row (swap lit/unlit)
  python3 lcd_glyph_visualize.py --fin-esp --array GLYPH_HOURGLASS   # dump animation from Fin_ESP.ino
  python3 lcd_glyph_visualize.py --fin-esp --array GLYPH_HOURGLASS --invert   # XOR rows (if bytes were generated without --invert-lcd-rows)
  python3 lcd_glyph_visualize.py --fin-esp --array GLYPH_ASSET_SOL
  python3 image_to_5x8_grid.py shot.png --crop X0,Y0,X1,Y1 --visualize   # PNG → grid, same ASCII preview
  python3 image_to_5x8_grid.py shot.png --crop ... --bytes | python3 lcd_glyph_visualize.py --stdin

CGRAM rules: exactly 8 row bytes, 5 dots per row (lower 5 bits). Labels b0–b7 are those bytes in order:
b0 = top scanline of the 5×8 cell, b7 = bottom (LiquidCrystal createChar / HD44780 CGRAM). This is not
the 16×2 LCD’s text “row 0 / row 1”. You cannot draw more than 5×8 lit pixels per custom character;
bits 5–7 of each byte are ignored.
"""

from __future__ import annotations

import argparse
import ast
import re
import sys
from pathlib import Path


def _default_fin_esp_sketch() -> Path:
    return Path(__file__).resolve().parent.parent / "Fin_ESP" / "Fin_ESP.ino"


def parse_sketch_array(text: str, name: str) -> list[list[int]]:
    """Parse `static byte name...[8] = { {..8..}, ... };` or `static byte name[8] = { ..8.. };`."""
    if re.search(rf"static\s+byte\s+{re.escape(name)}\s*\[\s*8\s*\]\s*=", text):
        m = re.search(
            rf"static\s+byte\s+{re.escape(name)}\s*\[\s*8\s*\]\s*=\s*\{{([^}}]+)\}}",
            text,
            re.S,
        )
        if not m:
            raise ValueError(f"could not parse 1D array {name}")
        nums = re.findall(r"0[xX][0-9a-fA-F]+", m.group(1))
        if len(nums) != 8:
            raise ValueError(f"{name}: expected 8 bytes, got {len(nums)}")
        return [[int(x, 16) for x in nums]]

    key = f"static byte {name}"
    i = text.find(key)
    if i < 0:
        raise ValueError(f"no static byte {name} in sketch")
    eq = text.find("=", i)
    lb = text.find("{", eq)
    depth = 0
    end = -1
    for j in range(lb, len(text)):
        if text[j] == "{":
            depth += 1
        elif text[j] == "}":
            depth -= 1
            if depth == 0:
                end = j
                break
    if end < 0:
        raise ValueError(f"unclosed initializer for {name}")
    body = text[lb + 1 : end]
    frames: list[list[int]] = []
    for m in re.finditer(r"\{([0-9a-fA-Fx,\s]+)\}", body):
        nums = re.findall(r"0[xX][0-9a-fA-F]+", m.group(1))
        if len(nums) == 8:
            frames.append([int(x, 16) for x in nums])
    if not frames:
        raise ValueError(f"no 8-byte rows parsed for {name}")
    return frames


def parse_bytes(s: str) -> list[int]:
    s = re.sub(r"//.*", "", s)
    s = re.sub(r"/\*.*?\*/", "", s, flags=re.S)
    nums = re.findall(r"0[xX][0-9a-fA-F]+|\b\d+\b", s)
    out = []
    for n in nums:
        out.append(int(n, 16) if n.lower().startswith("0x") else int(n))
    if len(out) % 8 != 0 and len(out) > 8:
        raise ValueError(f"byte count {len(out)} is not a multiple of 8 (frame rows)")
    return out


def rows_to_grid(rows: list[int]) -> list[str]:
    lines = []
    for ri, b in enumerate(rows[:8]):
        b &= 0x1F
        row_chars = []
        for col in range(5):
            bit = 4 - col
            on = (b >> bit) & 1
            row_chars.append("██" if on else "··")
        lines.append(f"  b{ri} " + "".join(row_chars) + f"   0x{b:02X}  {b:05b}")
    return lines


def visualize_frame(rows: list[int], title: str | None, invert: bool) -> str:
    rows = rows[:8]
    while len(rows) < 8:
        rows.append(0)
    if invert:
        rows = [(r & 0x1F) ^ 0x1F for r in rows]
    out = []
    if title:
        out.append(title)
    if invert:
        out.append("(inverted: each row XOR 0x1F)")
    out.append("      col: 0   1   2   3   4")
    out.append("      b0 = top of glyph, b7 = bottom (CGRAM byte order)")
    out.extend(rows_to_grid(rows))
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(description="Visualize 5×8 HD44780 glyph bytes")
    ap.add_argument("bytes", nargs="*", help="8 hex bytes per frame, e.g. 0x10 0x0F ...")
    ap.add_argument("--stdin", action="store_true", help="read comma/space-separated hex from stdin")
    ap.add_argument("--file", type=Path, help="file containing a braced list or C array initializer")
    ap.add_argument("--all-frames", action="store_true", help="split stdin/file into 8-byte frames")
    ap.add_argument(
        "-i",
        "--invert",
        action="store_true",
        help="XOR each row with 0x1F before drawing (swap lit/unlit vs stored bytes)",
    )
    ap.add_argument(
        "--fin-esp",
        type=Path,
        nargs="?",
        const=_default_fin_esp_sketch(),
        default=None,
        metavar="Fin_ESP.ino",
        help="Read static byte ARRAY from sketch (default: ../Fin_ESP/Fin_ESP.ino)",
    )
    ap.add_argument(
        "--array",
        default="GLYPH_HOURGLASS",
        help="Array name with --fin-esp (e.g. GLYPH_HOURGLASS, GLYPH_ASSET_SOL)",
    )
    args = ap.parse_args()

    if args.fin_esp is not None:
        sketch = args.fin_esp.read_text(encoding="utf-8", errors="replace")
        try:
            frames = parse_sketch_array(sketch, args.array)
        except ValueError as e:
            print(str(e), file=sys.stderr)
            return 1
        for fi, fr in enumerate(frames):
            if len(frames) == 1:
                title = f"--- {args.array} ---"
            else:
                title = f"--- {args.array} frame {fi} ---"
            print(visualize_frame(fr, title, args.invert))
            print()
        return 0

    raw = ""
    if args.stdin:
        raw = sys.stdin.read()
    elif args.file:
        raw = args.file.read_text(encoding="utf-8", errors="replace")
    elif args.bytes:
        raw = " ".join(args.bytes)
    else:
        ap.print_help()
        return 1

    nums = parse_bytes(raw)
    if not nums:
        print("No numbers parsed", file=sys.stderr)
        return 1

    if args.all_frames or (len(nums) > 8 and len(nums) % 8 == 0):
        frames = [nums[i : i + 8] for i in range(0, len(nums), 8)]
        for fi, fr in enumerate(frames):
            print(visualize_frame(fr, f"--- frame {fi} ---", args.invert))
            print()
    else:
        if len(nums) != 8:
            print(f"Expected 8 bytes, got {len(nums)}", file=sys.stderr)
            return 1
        print(visualize_frame(nums, None, args.invert))

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
