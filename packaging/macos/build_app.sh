#!/bin/sh
# Assemble Ferry.app from the app's and the CLI's binaries and pack it into
# a DMG.
#
#   build_app.sh VERSION BUILD DMG LICENSES RELEASE_DIR...
#
# Each RELEASE_DIR is cargo's release directory for one architecture (e.g.
# target/aarch64-apple-darwin/release), holding ferry-gui and ferry-cli; with
# more than one, lipo joins each into a universal binary. The app is
# Contents/MacOS/Ferry and the CLI Contents/MacOS/ferry-cli, which users
# link onto their PATH. LICENSES is the THIRD_PARTY_LICENSES.html
# cargo-about wrote (about.toml), which goes in Resources, where About
# opens it. VERSION is MAJOR.MINOR.PATCH
# (macOS accepts nothing else) and BUILD a number. Writes the DMG, and leaves
# Ferry.app next to it. Needs macOS: lipo, iconutil, codesign and
# hdiutil. Each of the app's languages gets a <lang>.lproj in Resources,
# from its i18n/<lang>/ferry.ftl (packaging/i18n.sh).
#
# The bundle is signed with FERRY_CODESIGN_IDENTITY, an identity in the
# keychain (the Build workflow's self-signed "Ferry Code Signing"
# certificate), or ad hoc without it. The certificate gives every release
# the same identity, so macOS keeps what the user allowed it (the
# Downloads folder) across updates; an ad-hoc signature is a new identity
# each build. It isn't notarized or sandboxed (docs/adr/0001, "Deliberate
# differences"), so Gatekeeper blocks a DMG a browser downloaded until the
# user allows it in System Settings → Privacy & Security; the Homebrew cask
# (ferry.rb.in) clears the quarantine instead.
set -eu

if [ $# -lt 5 ]; then
  echo "usage: $0 VERSION BUILD DMG LICENSES RELEASE_DIR..." >&2
  exit 2
fi
version=$1
build=$2
dmg=$3
licenses=$4
shift 4
out_dir=$(dirname "$dmg")

packaging=$(cd "$(dirname "$0")" && pwd)
assets=$packaging/../../assets
# Keep in step with MACOSX_DEPLOYMENT_TARGET in the Build workflow.
minimum=${MACOSX_DEPLOYMENT_TARGET:-12.0}

mkdir -p "$out_dir"
app=$out_dir/Ferry.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
apps=
clis=
for dir in "$@"; do
  # The CLI must be built on its own: built together with the app, cargo
  # turns the gui feature on for it too.
  if LC_ALL=C grep -aq iced_winit "$dir/ferry-cli"; then
    echo "build_app.sh: $dir/ferry-cli has the UI in it; build it with cargo build -p ferry alone" >&2
    exit 1
  fi
  apps="$apps $dir/ferry-gui"
  clis="$clis $dir/ferry-cli"
done
# shellcheck disable=SC2086 # Word splitting is wanted; paths have no spaces.
lipo -create -output "$app/Contents/MacOS/Ferry" $apps
# shellcheck disable=SC2086
lipo -create -output "$app/Contents/MacOS/ferry-cli" $clis
# Assigned first, so that set -e stops at a missing translation.
names=$("$packaging/../i18n.sh" macos "$app/Contents/Resources")
localizations=
for name in $names; do
  localizations="$localizations<string>$name</string>"
done
plutil -lint "$app"/Contents/Resources/*.lproj/InfoPlist.strings
sed -e "s|@VERSION@|$version|" -e "s|@BUILD@|$build|" \
  -e "s|@MINIMUM_SYSTEM_VERSION@|$minimum|" \
  -e "s|@LOCALIZATIONS@|$localizations|" \
  "$packaging/Info.plist.in" >"$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
printf 'APPL????' >"$app/Contents/PkgInfo"
iconutil -c icns -o "$app/Contents/Resources/AppIcon.icns" \
  "$assets/macos/AppIcon.iconset"
cp "$licenses" "$app/Contents/Resources/THIRD_PARTY_LICENSES.html"
cp "$packaging/../../LICENSE" "$app/Contents/Resources/LICENSE"
identity=${FERRY_CODESIGN_IDENTITY:--}
# Nested code first: signing the bundle seals the CLI's signature into it.
codesign --force --sign "$identity" --identifier dev.fanchao.Ferry.cli \
  "$app/Contents/MacOS/ferry-cli"
codesign --force --sign "$identity" --identifier dev.fanchao.Ferry "$app"
codesign --verify --strict --verbose=2 "$app"

staging=$(mktemp -d)
trap 'rm -rf "$staging"' EXIT
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
rm -f "$dmg"
hdiutil create -volname Ferry -srcfolder "$staging" -format UDZO "$dmg"
echo "$dmg"
