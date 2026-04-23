#!/usr/bin/env python3
"""
Visualize the falling-sand simulation used in fin-esp-rs.

Mirrors sand.rs exactly:
  - W=15, H=24, SOURCE_X=7
  - Wall: x>=10 and y>=16 (missing bottom-right LCD char)
  - 8 CGRAM slots in a 3×3(-1) grid
  - seed_pile(): pyramid at y=16..23 + sparse grains at y=8,10,12,14
  - Physics: bottom-to-top, alternating left/right each tick
  - Renders each CGRAM slot as ASCII art AND the full 15×24 canvas
"""

W, H, SOURCE_X = 15, 24, 7

# Map slot → (char_col, char_row)
SLOT_CHAR = {
    0: (0, 0), 1: (1, 0), 2: (2, 0),
    3: (0, 1), 4: (1, 1), 5: (2, 1),
    6: (0, 2), 7: (1, 2),
}

class SandGrid:
    def __init__(self):
        self.cells = [[False]*H for _ in range(W)]
        self.tick = 0

    def seed_pile(self):
        # Bottom char row (y=16..23): pyramid widening toward the floor
        for y in range(16, H):
            half_w = (y - 16) // 2 + 1
            left  = max(0, SOURCE_X - half_w)
            right = min(W, SOURCE_X + half_w + 1)
            for x in range(left, right):
                if not (x >= 10 and y >= 16):
                    self.cells[x][y] = True
        # Middle char row: sparse grains to show motion
        for y in (8, 10, 12, 14):
            self.cells[SOURCE_X][y] = True

    def can_enter(self, x, y):
        if x < 0 or x >= W or y < 0 or y >= H:
            return False
        if x >= 10 and y >= 16:      # wall (missing LCD char)
            return False
        return not self.cells[x][y]

    def try_fall(self, x, y):
        if not self.cells[x][y]:
            return
        by = y + 1
        if self.can_enter(x, by):
            self.cells[x][by] = True
            self.cells[x][y]  = False
            return
        xl, xr = x - 1, x + 1
        can_l = xl >= 0 and self.can_enter(xl, by)
        can_r = xr < W  and self.can_enter(xr, by)
        if can_l and can_r:
            dest = xl if (self.tick ^ x ^ y) & 1 == 0 else xr
        elif can_l:
            dest = xl
        elif can_r:
            dest = xr
        else:
            return
        self.cells[dest][by] = True
        self.cells[x][y]     = False

    def step(self):
        self.tick = (self.tick + 1) & 0xFF
        if not self.cells[SOURCE_X][0]:
            self.cells[SOURCE_X][0] = True
        if self.tick & 1 == 0:
            xs = range(W)
        else:
            xs = range(W - 1, -1, -1)
        for y in range(H - 2, -1, -1):
            for x in xs:
                self.try_fall(x, y)

    def glyphs(self):
        out = []
        for slot in range(8):
            cx, cy = SLOT_CHAR[slot]
            px0, py0 = cx * 5, cy * 8
            g = []
            for row in range(8):
                byte = 0
                for col in range(5):
                    if self.cells[px0 + col][py0 + row]:
                        byte |= 1 << (4 - col)
                g.append(byte)
            out.append(g)
        return out


def render_slot(g, slot_idx):
    cx, cy = SLOT_CHAR[slot_idx]
    print(f"  slot {slot_idx}  char({cx},{cy})  bytes: {' '.join(f'{b:02X}' for b in g)}")
    for b in g:
        row_str = ''.join('#' if b & (1 << (4 - col)) else '.' for col in range(5))
        print(f"    |{row_str}|")
    print()

def render_canvas(grid, label=""):
    if label:
        print(f"=== {label} ===")
    # Header: x axis
    print("   " + "".join(str(x % 10) for x in range(W)))
    for y in range(H):
        row = ""
        for x in range(W):
            if x >= 10 and y >= 16:
                row += "X"    # wall
            elif grid.cells[x][y]:
                row += "#"
            else:
                row += "."
        char_row = y // 8
        print(f"y{y:02d}[{row}] cr{char_row}")
    print()

def render_lcd_preview(glyphs):
    """Show what rows 1-3 of the LCD look like (the 3×3-1 char grid)."""
    print("=== LCD rows 1-3 preview (CGRAM chars 0-7) ===")
    print("  chars: [slot0 slot1 slot2] [slot3 slot4 slot5] [slot6 slot7  SP ]")
    print()

    # Print as a 3-row × 3-col grid, each cell is 5×8
    row_slots = [
        [0, 1, 2],   # LCD row 1
        [3, 4, 5],   # LCD row 2
        [6, 7, None],# LCD row 3  (None = space)
    ]
    for lcd_row, slots in enumerate(row_slots):
        print(f"  LCD row {lcd_row + 1}:")
        for pixel_row in range(8):
            line = "  |"
            for slot in slots:
                if slot is None:
                    line += "....." + "|"
                else:
                    b = glyphs[slot][pixel_row]
                    line += "".join('#' if b & (1 << (4 - c)) else '.' for c in range(5)) + "|"
            print(line)
        print()


def main():
    import sys
    steps_arg = int(sys.argv[1]) if len(sys.argv) > 1 else 30

    grid = SandGrid()
    grid.seed_pile()

    print(f"After seed_pile() (before any steps):")
    g0 = grid.glyphs()
    render_lcd_preview(g0)

    for _ in range(steps_arg):
        grid.step()

    print(f"After seed_pile() + {steps_arg} physics steps:")
    g = grid.glyphs()
    render_canvas(grid, f"Full canvas after {steps_arg} steps")
    render_lcd_preview(g)

    print("=== All 8 CGRAM slot bitmaps ===")
    for i in range(8):
        render_slot(g[i], i)

if __name__ == "__main__":
    main()
