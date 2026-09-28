#!/usr/bin/env python3
"""Export the documentation gallery from the UI's 2x snapshot renders.

Run the snapshot tests with SNAPSHOT_DIR set first (docs/HANDOFF.md), then:
    python3 site/screenshots.py "$SNAPSHOT_DIR"

Requires Pillow. Only English light/dark tiny-skia snapshots are exported;
locale and other test captures stay in the scratch directory.
"""

import re
import sys
from pathlib import Path

from PIL import Image

GALLERY = {
    "devices": "gallery-devices",
    "device": "gallery-device",
    "pairing-request": "pairing-request",
    "add-device": "add-device",
    "transfers": "transfers",
    "settings": "gallery-settings",
    "files": "files-folder-narrow",
    "file-preview": "files-preview",
}


def main(directory):
    directory = Path(directory)
    output = Path(__file__).resolve().parent / "img"
    # Validate the entire input set before replacing any gallery images.
    captures = []
    for page, snapshot in GALLERY.items():
        for theme in ("light", "dark"):
            matches = list(directory.glob(f"{snapshot}-{theme}-tiny-skia.png"))
            if len(matches) != 1:
                sys.exit(f"Expected one {snapshot}-{theme}-tiny-skia.png in {directory}")
            captures.append((page, theme, matches[0]))
    dimensions = {}
    for page, theme, capture in captures:
        with Image.open(capture) as image:
            image = image.convert("RGB")
            if image.width != 880:
                image = image.resize(
                    (880, round(image.height * 880 / image.width)),
                    Image.Resampling.LANCZOS,
                )
            target = output / f"{page}-{theme}.webp"
            image.save(target, "WEBP", quality=90, method=6)
            dimensions[target.name] = image.size
            print(f"{target.name}: {image.width}×{image.height}")

    page = output.parent / "index.html"

    def update_size(match):
        width, height = dimensions[match[1]]
        tag = re.sub(r'width="[0-9]+"', f'width="{width}"', match[0])
        return re.sub(r'height="[0-9]+"', f'height="{height}"', tag)

    page.write_text(re.sub(
        r'<img src="img/([^"]+-light\.webp)"[^>]*>',
        update_size,
        page.read_text(),
    ))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
