#!/bin/sh
# Pack the release binaries into a .deb.
#
#   build_deb.sh APP CLI LICENSES VERSION OUT_DIR
#
# APP is cargo's ferry-gui, installed under that name as /usr/bin/ferry-gui;
# CLI is cargo's ferry-cli, the command line, installed next to it as
# /usr/bin/ferry-cli (docs/adr/0001, "Packaging"). LICENSES is the
# THIRD_PARTY_LICENSES.html cargo-about wrote (about.toml), installed in
# /usr/share/doc/ferry, where About opens it, next to Ferry's own license
# (LICENSE) as the copyright file.
# The menu entry, in each of the app's languages (packaging/i18n.sh), and
# the icons go to /usr/share. Runs on Debian or
# Ubuntu: it needs dpkg-deb, and dpkg-shlibdeps to work out the dependencies
# from the ELF files. Build on the oldest release you want to support, since
# the glibc it links against is the oldest one the package will install on.
set -eu

if [ $# -ne 5 ]; then
  echo "usage: $0 APP CLI LICENSES VERSION OUT_DIR" >&2
  exit 2
fi
app=$1
cli=$2
licenses=$3
version=$4
out_dir=$5

app_id=dev.fanchao.Ferry
packaging=$(cd "$(dirname "$0")" && pwd)
assets=$packaging/../../assets
arch=$(dpkg --print-architecture)

# The CLI must be built on its own: built together with the app, cargo
# turns the gui feature on for it too.
if LC_ALL=C grep -aq iced_winit "$cli"; then
  echo "build_deb.sh: $cli has the UI in it; build it with cargo build -p ferry alone" >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
root=$work/root

mkdir -p "$root/usr/bin" "$root/usr/share/applications" \
  "$root/usr/share/icons" "$root/usr/share/doc/ferry" "$root/DEBIAN"
install -m 755 -s "$app" "$root/usr/bin/ferry-gui"
install -m 755 -s "$cli" "$root/usr/bin/ferry-cli"
# Written whole first, so that set -e stops at a missing translation.
"$packaging/../i18n.sh" desktop "$packaging/$app_id.desktop" >"$work/$app_id.desktop"
install -m 644 "$work/$app_id.desktop" "$root/usr/share/applications/"
cp -R "$assets/linux/hicolor" "$root/usr/share/icons/"
install -m 644 "$licenses" "$root/usr/share/doc/ferry/THIRD_PARTY_LICENSES.html"
# Debian's machine-readable format, with LICENSE's text (after its title
# line) as the license: indented, blank lines as " .".
{
  printf 'Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n'
  printf 'Upstream-Name: ferry\nSource: https://github.com/cloudwalkerlabs/ferryapp\n\n'
  printf 'Files: *\n'
  printf 'Copyright: %s\n' "$(sed -n 's/^Copyright (c) //p' "$packaging/../../LICENSE")"
  printf 'License: MIT\n'
  sed -e '1,2d' -e 's/^$/./' -e 's/^/ /' "$packaging/../../LICENSE"
} >"$root/usr/share/doc/ferry/copyright"
chmod 644 "$root/usr/share/doc/ferry/copyright"
chmod -R u=rwX,go=rX "$root/usr/share"

# dpkg-shlibdeps wants to run from a source tree; give it a minimal one.
mkdir -p "$work/src/debian"
printf 'Source: ferry\n\nPackage: ferry\nArchitecture: any\n' \
  >"$work/src/debian/control"
depends=$(
  cd "$work/src" &&
    dpkg-shlibdeps -O "$root/usr/bin/ferry-gui" "$root/usr/bin/ferry-cli" \
      2>"$work/shlibdeps.log" |
    sed -n 's/^shlibs:Depends=//p'
) || {
  cat "$work/shlibdeps.log" >&2
  exit 1
}
test -n "$depends"
# Loaded at runtime (dlopen), so dpkg-shlibdeps can't see them: winit's
# keyboard, Wayland and X11 libraries, and the libxcb `display-info` lists
# the monitors with. The app draws in software, so it needs no GPU
# driver. The app bundles its Latin font, but a system
# font is needed for the monospace pairing code and for other scripts:
# Noto CJK for Chinese, Japanese and Korean (device and file names, and
# the zh-CN translation), which DejaVu lacks.
depends="$depends, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libx11-6, libx11-xcb1, libxcursor1, libxi6, libxrandr2, fontconfig, fonts-dejavu-core | fonts-freefont-ttf | fonts-liberation"
recommends="xdg-desktop-portal, fonts-noto-cjk"

cat >"$root/DEBIAN/control" <<CONTROL
Package: ferry
Version: $version
Architecture: $arch
Maintainer: fanchao <dev@fanchao.dev>
Installed-Size: $(du -sk --exclude=DEBIAN "$root" | cut -f1)
Depends: $depends
Recommends: $recommends
Section: net
Priority: optional
Homepage: https://github.com/cloudwalkerlabs/ferryapp
Description: Pair with your devices and share files and the clipboard
 Ferry connects your computer with your phone and other devices on the
 local network, using the KDE Connect protocol: send files, share the
 clipboard and ping devices you have paired with. It comes with the
 ferry-cli command line, which drives the app once its Command line
 access setting is on.
CONTROL

mkdir -p "$out_dir"
package=$out_dir/ferry_${version}_$arch.deb
dpkg-deb --root-owner-group --build "$root" "$package"
echo "$package"
