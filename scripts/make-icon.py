#!/usr/bin/env python3
"""Draw the application icon — deterministically, with no image libraries.

The icon is a build input: the AppImage needs a PNG, the `.desktop` file points
at one, and a missing icon fails the packaging step on a machine nobody is
watching. Committing a generator instead of only the binary keeps it honest —
anyone can read what the picture is, change a colour, and re-run:

    python3 scripts/make-icon.py                 # writes assets/icon.png
    python3 scripts/make-icon.py --size 512      # preview a larger render

The renderer is a 3x3-supersampled scalar painter: a handful of analytic shapes
(rounded rectangles, a triangle) composited in order. That is all this needs —
no dependencies, no fonts, no rasteriser, and byte-for-byte reproducible output.
"""

from __future__ import annotations

import argparse
import struct
import zlib
from pathlib import Path

# Colours (straight RGBA, 0-255).
BACKDROP = (0x23, 0x29, 0x36, 255)
BODY = (0xE7, 0xEC, 0xF6, 255)
SLOT = (0x93, 0x9C, 0xAE, 255)
ACCENT = (0x5A, 0xD8, 0x9A, 255)
SHADOW = (0x11, 0x14, 0x1C, 90)


def rounded_rect(x: float, y: float, x0: float, y0: float, x1: float, y1: float, r: float) -> bool:
    """True inside a rectangle with rounded corners."""
    if x < x0 or x > x1 or y < y0 or y > y1:
        return False
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def ring(x: float, y: float, box: tuple[float, float, float, float], r: float, width: float) -> bool:
    """True inside the `width`-thick outline of a rounded rectangle."""
    x0, y0, x1, y1 = box
    if not rounded_rect(x, y, x0, y0, x1, y1, r):
        return False
    return not rounded_rect(
        x, y, x0 + width, y0 + width, x1 - width, y1 - width, max(r - width, 0)
    )


def triangle(x: float, y: float, a: tuple[float, float], b: tuple[float, float], c: tuple[float, float]) -> bool:
    """True inside a triangle, by consistent edge orientation."""
    def side(p, q) -> float:
        return (q[0] - p[0]) * (y - p[1]) - (q[1] - p[1]) * (x - p[0])

    d1, d2, d3 = side(a, b), side(b, c), side(c, a)
    has_neg = d1 < 0 or d2 < 0 or d3 < 0
    has_pos = d1 > 0 or d2 > 0 or d3 > 0
    return not (has_neg and has_pos)


def shade(x: float, y: float, size: int) -> tuple[int, int, int, int]:
    """The colour of one sample, in a 0..size coordinate space."""
    s = size / 256.0
    layer = (0, 0, 0, 0)

    def paint(colour: tuple[int, int, int, int]) -> None:
        nonlocal layer
        layer = colour

    # Backdrop: a rounded square with a soft drop shadow underneath.
    if rounded_rect(x, y, 12 * s, 18 * s, 244 * s, 250 * s, 52 * s):
        paint(SHADOW if y > 240 * s else BACKDROP)
    if rounded_rect(x, y, 12 * s, 12 * s, 244 * s, 244 * s, 52 * s):
        paint(BACKDROP)

    # Phone: an upright rounded outline, with a speaker slot at the top.
    if ring(x, y, (82 * s, 44 * s, 174 * s, 212 * s), 18 * s, 9 * s):
        paint(BODY)
    if rounded_rect(x, y, 112 * s, 60 * s, 144 * s, 66 * s, 3 * s):
        paint(SLOT)

    # Backup mark: an arrow pointing down into the phone's storage.
    if rounded_rect(x, y, 121 * s, 96 * s, 135 * s, 152 * s, 4 * s):
        paint(ACCENT)
    if triangle(
        x,
        y,
        (100 * s, 140 * s),
        (156 * s, 140 * s),
        (128 * s, 184 * s),
    ):
        paint(ACCENT)

    return layer


def render(size: int) -> list[bytearray]:
    """Render `size` x `size` RGBA rows, 3x3 supersampled."""
    samples = 3
    step = 1.0 / samples
    rows: list[bytearray] = []
    for py in range(size):
        row = bytearray()
        for px in range(size):
            acc = [0.0, 0.0, 0.0, 0.0]
            for sy in range(samples):
                for sx in range(samples):
                    r, g, b, a = shade(
                        px + (sx + 0.5) * step, py + (sy + 0.5) * step, size
                    )
                    # Composite over the accumulated average, premultiplied so
                    # that partially covered edges do not darken the backdrop.
                    alpha = a / 255.0
                    acc[0] = acc[0] * (1 - 1 / (samples * samples)) + r * alpha / (samples * samples)
                    acc[1] = acc[1] * (1 - 1 / (samples * samples)) + g * alpha / (samples * samples)
                    acc[2] = acc[2] * (1 - 1 / (samples * samples)) + b * alpha / (samples * samples)
                    acc[3] = acc[3] * (1 - 1 / (samples * samples)) + a / (samples * samples)
            coverage = acc[3] / 255.0
            if coverage > 0.0:
                # `acc` is the mean of premultiplied samples; PNG wants straight
                # alpha, so undo the premultiplication.
                rgb = [min(acc[i] / coverage, 255.0) for i in range(3)]
            else:
                rgb = [0.0, 0.0, 0.0]
            row += bytes(
                (
                    round(rgb[0]),
                    round(rgb[1]),
                    round(rgb[2]),
                    round(min(acc[3], 255.0)),
                )
            )
        rows.append(row)
    return rows


def write_png(path: Path, size: int, rows: list[bytearray]) -> None:
    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    raw = b"".join(b"\x00" + bytes(row) for row in rows)
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as handle:
        handle.write(b"\x89PNG\r\n\x1a\n")
        handle.write(chunk(b"IHDR", header))
        handle.write(chunk(b"IDAT", zlib.compress(raw, 9)))
        handle.write(chunk(b"IEND", b""))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "assets" / "icon.png",
    )
    parser.add_argument("--size", type=int, default=256)
    args = parser.parse_args()

    rows = render(args.size)
    write_png(args.out, args.size, rows)
    print(f"wrote {args.out} ({args.size}x{args.size})")


if __name__ == "__main__":
    main()
