#!/usr/bin/env python3
"""Builds the website into an output directory, filling in the download
links from a GitHub release.

    gh release view --json tagName,url,assets > release.json
    site/build.py release.json _site

`gh release view` without a tag reads the latest release. The page is
index.html with {{name}} placeholders; each package is found among the
release's assets by the end of its file name, so the names may carry any
version (or the app's old name). The release key's public half
(packaging/release-key.asc) is published next to the page as
ferry-release.asc; the Build workflow adds the apt repository in apt/.
"""

import json
import re
import shutil
import sys
from pathlib import Path

SITE = Path(__file__).resolve().parent
RELEASE_KEY = SITE.parent / "packaging" / "release-key.asc"

# Placeholder prefix -> how the package's file name ends.
PACKAGES = {
    "macos": "-macos-universal.dmg",
    "windows": "-windows-x64-setup.exe",
    "deb_amd64": "_amd64.deb",
    "deb_arm64": "_arm64.deb",
    "arch": "PKGBUILD",
}


def size(n):
    return f"{n / 1_000_000:.0f} MB" if n >= 1_000_000 else f"{n / 1000:.0f} kB"


def main(release_json, out):
    release = json.loads(Path(release_json).read_text())
    values = {"tag": release["tagName"], "release_url": release["url"]}
    for key, suffix in PACKAGES.items():
        matches = [a for a in release["assets"] if a["name"].endswith(suffix)]
        if len(matches) != 1:
            sys.exit(f"{release['tagName']}: expected one asset ending in {suffix}, found {len(matches)}")
        values[f"{key}_url"] = matches[0]["url"]
        values[f"{key}_size"] = size(matches[0]["size"])

    def fill(m):
        if m.group(1) not in values:
            sys.exit(f"index.html: unknown placeholder {m.group(0)}")
        return values[m.group(1)]

    page = re.sub(r"\{\{(\w+)\}\}", fill, (SITE / "index.html").read_text())

    out = Path(out)
    if out.exists():
        shutil.rmtree(out)
    shutil.copytree(SITE, out, ignore=shutil.ignore_patterns("build.py", "screenshots.py", "index.html"))
    (out / "index.html").write_text(page)
    shutil.copy(RELEASE_KEY, out / "ferry-release.asc")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(*sys.argv[1:])
