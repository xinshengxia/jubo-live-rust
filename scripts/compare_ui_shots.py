#!/usr/bin/env python3
"""Compare C# original launcher screenshots with Rust --ui-shot captures.

No third-party tools beyond Pillow + numpy (available in the local conda python
environments; ImageMagick is not installed). Fully offline.

Each page produces <out>/<page>.png = [reference | rust | 3x amplified diff]
(comparison width defaults to 1240 = one logical page at 200% DPI) and the
aggregate summary.json / summary.md records:

* native sizes and whether either side was rescaled for comparison,
* mean absolute error and changed-pixel ratio,
* ink ratio (how much content is painted) and mean colour per side,
* per-region text bounding boxes (physical boxes from the original control tree)
  so font-size / offset drift is measurable, not just "looks different".

Usage:
  python scripts/compare_ui_shots.py --reference <dir> --rust <dir> --out <dir>
                                     [--theme dark|light] [--max-width 1240]
                                     [--pages favorites,recent,...]
                                     [--changed-threshold 24]
                                     [--allow-missing]

Limits: pixel statistics are a screening signal only. Font rasterisation, DPI
scaling, avatar/status network data and the original's own transient state still
differ; review the composite.
"""

import argparse
import json
import os
import sys

try:
    import numpy as np
    from PIL import Image
except ImportError as error:  # pragma: no cover - environment guard
    sys.exit(
        "compare_ui_shots.py needs Pillow + numpy; use a python with both "
        "(provide an interpreter with those dependencies). Import error: %s" % error
    )

DEFAULT_PAGES = ["favorites", "recent", "lookup", "sessions", "players", "danmaku", "accounts", "data"]
SEPARATOR = 8

# Physical pixel boxes (2480x1440 reference) used to measure text drift. Boxes come
# from the original control tree capture; a region without ink reports None.
REGIONS = {
    "page_title": (336, 74, 1000, 160),
    "brand": (80, 100, 380, 190),
    "nav_first": (16, 260, 320, 360),
    "chrome_hint": (1880, 0, 2400, 70),
    "footer_text": (360, 1340, 2400, 1420),
}


def find_reference(directory, page, theme):
    for name in (
        "%s-%s-200.png" % (page, theme),
        "%s-%s-100.png" % (page, theme),
        "%s-%s-150.png" % (page, theme),
        "%s-%s.png" % (page, theme),
        "%s.png" % page,
    ):
        path = os.path.join(directory, name)
        if os.path.isfile(path):
            return path, name
    return None, None


def find_rust(directory, page):
    for name in ("%s.png" % page, "%s-dark.png" % page):
        path = os.path.join(directory, name)
        if os.path.isfile(path):
            return path, name
    return None, None


def load_rgb(path):
    with Image.open(path) as image:
        return image.convert("RGB")


def comparison_size(native, max_width):
    width, height = native
    if width <= max_width or height == 0:
        return native
    return max_width, max(1, round(height * max_width / width))


def scaled(image, target):
    if image.size == target:
        return image
    return image.resize(target, Image.LANCZOS)


def region_ink_box(array, box, tolerance=10):
    """Bounding box of pixels that differ from the region's top-left colour.

    Returns (x, y, w, h) in the same physical coordinates as the reference box,
    or None when the region is flat.
    """
    top, left, bottom, right = box[1], box[0], box[3], box[2]
    height, width = array.shape[0], array.shape[1]
    top, left = max(0, top), max(0, left)
    bottom, right = min(height, bottom), min(width, right)
    if bottom <= top or right <= left:
        return None
    crop = array[top:bottom, left:right].astype(np.int16)
    background = crop[0, 0]
    mask = np.abs(crop - background).sum(axis=2) > tolerance * 3
    rows = np.where(mask.any(axis=1))[0]
    cols = np.where(mask.any(axis=0))[0]
    if rows.size == 0:
        return None
    return {
        "x": int(cols[0]),
        "y": int(rows[0]),
        "w": int(cols[-1] - cols[0] + 1),
        "h": int(rows[-1] - rows[0] + 1),
    }


def ink_ratio(array):
    luminance = 0.2126 * array[:, :, 0] + 0.7152 * array[:, :, 1] + 0.0722 * array[:, :, 2]
    hist, edges = np.histogram(luminance, bins=32, range=(0, 256))
    background = float(edges[int(np.argmax(hist))]) + 4.0
    return float(np.count_nonzero(np.abs(luminance - background) > 18.0)) / luminance.size


def compare_page(reference_path, rust_path, out_dir, page, max_width, threshold, theme):
    reference_native = load_rgb(reference_path)
    rust_native = load_rgb(rust_path)
    target = comparison_size(rust_native.size, max_width)
    reference_image = scaled(reference_native, target)
    rust_image = scaled(rust_native, target)

    reference_array = np.asarray(reference_image, dtype=np.int16)
    rust_array = np.asarray(rust_image, dtype=np.int16)
    delta = np.abs(reference_array - rust_array)
    mae = float(delta.mean())
    changed = float(np.count_nonzero(delta.max(axis=2) > threshold)) * 100.0 / delta[:, :, 0].size

    visual = np.clip(delta * 3, 0, 255).astype(np.uint8)
    width, height = target
    canvas = Image.new("RGB", (width * 3 + SEPARATOR * 2, height), (24, 24, 28))
    canvas.paste(reference_image, (0, 0))
    canvas.paste(rust_image, (width + SEPARATOR, 0))
    canvas.paste(Image.fromarray(visual, "RGB"), (2 * (width + SEPARATOR), 0))
    os.makedirs(out_dir, exist_ok=True)
    composite_name = "%s.png" % page
    canvas.save(os.path.join(out_dir, composite_name))

    # Region text boxes are measured on the native 2480x1440 reference scale.
    native_reference = np.asarray(reference_native, dtype=np.int16)
    native_rust = np.asarray(scaled(rust_native, reference_native.size), dtype=np.int16)
    regions = {}
    for name, box in REGIONS.items():
        regions[name] = {
            "reference": region_ink_box(native_reference, box),
            "rust": region_ink_box(native_rust, box),
        }

    return {
        "key": page,
        "reference": os.path.basename(reference_path),
        "rust": os.path.basename(rust_path),
        "reference_size": list(reference_native.size),
        "rust_size": list(rust_native.size),
        "comparison_size": list(target),
        "reference_rescaled": reference_native.size != target,
        "rust_rescaled": rust_native.size != target,
        "mean_absolute_error": round(mae, 3),
        "changed_percent_above_threshold": round(changed, 3),
        "ink_ratio_reference": round(ink_ratio(reference_array), 4),
        "ink_ratio_rust": round(ink_ratio(rust_array), 4),
        "mean_color_reference": [round(float(v), 2) for v in reference_array.reshape(-1, 3).mean(axis=0)],
        "mean_color_rust": [round(float(v), 2) for v in rust_array.reshape(-1, 3).mean(axis=0)],
        "regions": regions,
        "composite": composite_name,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--reference", required=True)
    parser.add_argument("--rust", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--theme", default="dark", choices=["dark", "light"])
    parser.add_argument("--max-width", type=int, default=1240)
    parser.add_argument("--pages", default="")
    parser.add_argument("--changed-threshold", type=int, default=24)
    parser.add_argument("--allow-missing", action="store_true")
    args = parser.parse_args()

    pages = [page.strip() for page in args.pages.split(",") if page.strip()] or DEFAULT_PAGES
    rows = []
    for page in pages:
        reference_path, reference_name = find_reference(args.reference, page, args.theme)
        rust_path, rust_name = find_rust(args.rust, page)
        if not reference_path or not rust_path:
            rows.append(
                {
                    "key": page,
                    "reference": reference_name,
                    "rust": rust_name,
                    "missing": True,
                }
            )
            print("ui-compare %s: missing reference=%s rust=%s" % (page, reference_name, rust_name))
            continue
        row = compare_page(
            reference_path,
            rust_path,
            args.out,
            page,
            args.max_width,
            args.changed_threshold,
            args.theme,
        )
        rows.append(row)
        print(
            "ui-compare %s: mae=%.2f changed=%.2f%% ink(ref/rust)=%.3f/%.3f -> %s"
            % (
                page,
                row["mean_absolute_error"],
                row["changed_percent_above_threshold"],
                row["ink_ratio_reference"],
                row["ink_ratio_rust"],
                row["composite"],
            )
        )

    compared = [row for row in rows if not row.get("missing")]
    os.makedirs(args.out, exist_ok=True)
    document = {
        "stage": "ui-compare",
        "reference_directory": os.path.basename(os.path.normpath(args.reference)),
        "rust_directory": os.path.basename(os.path.normpath(args.rust)),
        "theme": args.theme,
        "comparison_max_width": args.max_width,
        "changed_threshold": args.changed_threshold,
        "pages": rows,
        "compared": len(compared),
        "missing": len(rows) - len(compared),
        "layout": "each composite is [reference | rust | 3x amplified diff], 8 dark pixels between columns",
        "limits": "Pixel statistics only: font rasterisation, DPI scaling, avatars/live badges (network) and transient state still differ; review the composite.",
    }
    with open(os.path.join(args.out, "summary.json"), "w", encoding="utf-8") as handle:
        json.dump(document, handle, ensure_ascii=False, indent=2)

    lines = [
        "| page | mae | changed% | ink ref | ink rust | ref size | rust size | composite |",
        "| --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for row in rows:
        if row.get("missing"):
            lines.append("| %s | - | - | - | - | %s | %s | - |" % (row["key"], row.get("reference"), row.get("rust")))
            continue
        lines.append(
            "| %s | %.2f | %.2f | %.3f | %.3f | %s | %s | %s |"
            % (
                row["key"],
                row["mean_absolute_error"],
                row["changed_percent_above_threshold"],
                row["ink_ratio_reference"],
                row["ink_ratio_rust"],
                "x".join(str(v) for v in row["reference_size"]),
                "x".join(str(v) for v in row["rust_size"]),
                row["composite"],
            )
        )
    with open(os.path.join(args.out, "summary.md"), "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")

    print("ui-compare: %d of %d pages compared -> %s" % (len(compared), len(rows), args.out))
    if not compared:
        sys.exit("no page could be compared")
    if not args.allow_missing and len(compared) != len(rows):
        sys.exit("missing pairs: %d of %d compared" % (len(compared), len(rows)))


if __name__ == "__main__":
    main()
