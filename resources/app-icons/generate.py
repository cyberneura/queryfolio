#!/usr/bin/env python3
"""Draws the Queryfolio app icon at each size (standard library only)."""
from __future__ import annotations

import math
import struct
import sys
from pathlib import Path
import zlib

PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"

NAVY = (30, 41, 59)
WHITE = (255, 255, 255)

# --- Measurements on the 1024px canvas (from ic10 of the existing icon.icns) ---
REF = 1024.0
SQ_INSET = 103.0
SQ_SIDE = 819.0
SQ_RADIUS = 178.4
CYL_CX = 511.5
CYL_RX = 182.5          # radius through the center of the stroke
CYL_RY = 83.0
STROKE = 32.0
BAND_CY = (379.0, 479.0, 579.0, 679.0)   # top ellipse + 3 bands

# At small sizes the strokes would vanish if scaled proportionally, so set a lower bound.
MIN_STROKE = {16: 2.0, 32: 3.0, 64: 3.6}
# At 16px all 4 bands do not fit. Reduce the number of levels to keep the outline.
BAND_COUNT = {16: 2, 32: 3}

SIZES = (16, 32, 64, 128, 256, 512, 1024)


def stroke_width(size: int) -> float:
    return max(STROKE * size / REF, MIN_STROKE.get(size, 0.0))


def band_centers(size: int) -> list[float]:
    """Center y of the ellipses drawn at that size (1024px basis)."""
    count = BAND_COUNT.get(size, len(BAND_CY))
    if count >= len(BAND_CY):
        return list(BAND_CY)
    top, bottom = BAND_CY[0], BAND_CY[-1]
    if count == 1:
        return [top]
    step = (bottom - top) / (count - 1)
    return [top + step * i for i in range(count)]


def _inside_round_rect(x, y, left, top, side, radius):
    right, bottom = left + side, top + side
    if x < left or x > right or y < top or y > bottom:
        return False
    cx = min(max(x, left + radius), right - radius)
    cy = min(max(y, top + radius), bottom - radius)
    return math.hypot(x - cx, y - cy) <= radius


def render(size: int, supersample: int = 4, small: bool = False) -> list[bytearray]:
    s = size / REF
    left = top = SQ_INSET * s
    side = SQ_SIDE * s
    radius = SQ_RADIUS * s
    half = stroke_width(size) / 2.0
    cx = CYL_CX * s
    rx, ry = CYL_RX * s, CYL_RY * s
    centers = [c * s for c in band_centers(size)]
    if small:
        # Background and glyph have independent bounds. Keep the per-size strokes
        # and band counts while fitting the complete mark into the central 80%.
        left = top = 0.0
        side = float(size)
        radius = size * SQ_RADIUS / SQ_SIDE
        factor = (size * 0.8 - 2 * half) / max(2 * CYL_RX, BAND_CY[-1] - BAND_CY[0] + 2 * CYL_RY)
        cx = size / 2
        rx, ry = CYL_RX * factor, CYL_RY * factor
        mid_y = (BAND_CY[0] + BAND_CY[-1]) / 2
        centers = [size / 2 + (c - mid_y) * factor for c in band_centers(size)]
    body_top, body_bottom = centers[0], centers[-1]
    side_x = (cx - rx, cx + rx)

    rows: list[bytearray] = []
    n = supersample
    inv = 1.0 / (n * n)
    for py in range(size):
        line = bytearray(size * 4)
        for px in range(size):
            covered = 0
            white = 0
            for sy in range(n):
                y = py + (sy + 0.5) / n
                for sx in range(n):
                    x = px + (sx + 0.5) / n
                    if not _inside_round_rect(x, y, left, top, side, radius):
                        continue
                    covered += 1

                    hit = False
                    # vertical sides of the cylinder
                    if body_top <= y <= body_bottom:
                        for sxp in side_x:
                            if abs(x - sxp) <= half:
                                hit = True
                                break
                    if not hit:
                        for i, cy in enumerate(centers):
                            # the first is the full circumference, the rest only the lower half (bands)
                            if i > 0 and y < cy:
                                continue
                            dx, dy = x - cx, y - cy
                            r2 = (dx / rx) ** 2 + (dy / ry) ** 2
                            gx = 2 * dx / (rx * rx)
                            gy = 2 * dy / (ry * ry)
                            g = math.hypot(gx, gy)
                            if g > 0 and abs(r2 - 1.0) / g <= half:
                                hit = True
                                break
                    if hit:
                        white += 1

            out = px * 4
            if covered == 0:
                line[out:out + 4] = b"\x00\x00\x00\x00"
                continue
            fraction = white / covered
            for i in range(3):
                line[out + i] = int(round(NAVY[i] + (WHITE[i] - NAVY[i]) * fraction))
            line[out + 3] = int(round(covered * inv * 255))
        rows.append(line)
    return rows


def write_png(path: Path, size: int, rows: list[bytearray]) -> None:
    raw = bytearray()
    for row in rows:
        raw.append(0)
        raw += row

    def chunk(kind: bytes, payload: bytes) -> bytes:
        return (struct.pack(">I", len(payload)) + kind + payload
                + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF))

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    path.write_bytes(PNG_SIGNATURE + chunk(b"IHDR", header)
                     + chunk(b"IDAT", zlib.compress(bytes(raw), 9)) + chunk(b"IEND", b""))


def main(argv: list[str]) -> int:
    if len(argv) == 3 and argv[1] == "--mac-small":
        out_dir = Path(argv[2])
        out_dir.mkdir(parents=True, exist_ok=True)
        for size in (16, 32):
            rows = render(size, small=True)
            for y, row in enumerate(rows):
                for x in range(size):
                    if x == 0 or y == 0 or x == size - 1 or y == size - 1:
                        row[x * 4 + 3] = min(239, row[x * 4 + 3])
            write_png(out_dir / f"icon_{size}x{size}.png", size, rows)
        return 0
    out_dir = Path(argv[1]) if len(argv) > 1 else Path(__file__).parent / "rendered"
    out_dir.mkdir(parents=True, exist_ok=True)
    for size in SIZES:
        supersample = 4 if size <= 256 else 2
        path = out_dir / f"icon-{size}.png"
        write_png(path, size, render(size, supersample))
        print(f"{path}  stroke={stroke_width(size):.2f}px bands={len(band_centers(size))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
