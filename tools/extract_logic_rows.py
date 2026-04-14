from __future__ import annotations

import argparse
from pathlib import Path

from PIL import Image


def is_logic_pixel(r: int, g: int, b: int) -> bool:
    # 逻辑分析仪彩色胶囊字节常见为高饱和的青/绿/粉色，背景接近白色。
    if r > 245 and g > 245 and b > 245:
        return False
    if g > 140 and b > 140:
        return True
    if g > 150:
        return True
    if r > 170 and b > 150:
        return True
    return False


def find_row_groups(img: Image.Image, min_fraction: float = 0.16) -> list[tuple[int, int]]:
    rgb = img.convert("RGB")
    w, h = rgb.size
    rows: list[int] = []
    min_hits = int(w * min_fraction)

    for y in range(h):
        hits = 0
        for x in range(w):
            if is_logic_pixel(*rgb.getpixel((x, y))):
                hits += 1
        rows.append(hits)

    groups: list[tuple[int, int]] = []
    start: int | None = None
    for y, hits in enumerate(rows):
        if hits >= min_hits:
            if start is None:
                start = y
        elif start is not None:
            if y - start >= 8:
                groups.append((start, y - 1))
            start = None
    if start is not None and h - start >= 8:
        groups.append((start, h - 1))

    merged: list[tuple[int, int]] = []
    for s, e in groups:
        if merged and s - merged[-1][1] <= 10:
            merged[-1] = (merged[-1][0], e)
        else:
            merged.append((s, e))
    return merged


def find_col_bounds(img: Image.Image, y0: int, y1: int) -> tuple[int, int]:
    rgb = img.convert("RGB")
    w, _ = rgb.size
    cols = []
    min_hits = max(3, (y1 - y0 + 1) // 5)
    for x in range(w):
        hits = 0
        for y in range(y0, y1 + 1):
            if is_logic_pixel(*rgb.getpixel((x, y))):
                hits += 1
        cols.append(hits)

    left = 0
    while left < w and cols[left] < min_hits:
        left += 1
    right = w - 1
    while right >= 0 and cols[right] < min_hits:
        right -= 1
    return max(0, left - 8), min(w - 1, right + 8)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("image", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--scale", type=int, default=6)
    args = parser.parse_args()

    img = Image.open(args.image)
    args.out.mkdir(parents=True, exist_ok=True)

    groups = find_row_groups(img)
    for idx, (y0, y1) in enumerate(groups, start=1):
        x0, x1 = find_col_bounds(img, y0, y1)
        crop = img.crop((x0, max(0, y0 - 6), x1 + 1, min(img.height, y1 + 7)))
        raw_path = args.out / f"row_{idx:02d}.png"
        zoom_path = args.out / f"row_{idx:02d}_x{args.scale}.png"
        crop.save(raw_path)
        crop.resize(
            (crop.width * args.scale, crop.height * args.scale),
            Image.Resampling.NEAREST,
        ).save(zoom_path)
        print(f"{idx:02d} y={y0}-{y1} x={x0}-{x1} -> {zoom_path}")


if __name__ == "__main__":
    main()
