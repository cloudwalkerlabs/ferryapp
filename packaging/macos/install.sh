#!/bin/sh
# Install Ferry on macOS from the latest release's DMG, without the
# Gatekeeper prompt:
#
#   curl -fsSL https://simophin.github.io/ferryapp/install.sh | sh
#
# Browsers mark what they download as quarantined, and Gatekeeper refuses a
# quarantined app that isn't notarized (Ferry's has only an ad-hoc
# signature, build_app.sh). curl sets no such mark, so the app this script
# fetches opens directly; it also clears the mark in case something set one.
#
# Copies Ferry.app into /Applications, or ~/Applications when that isn't
# writable, replacing (and first quitting) an installed one; links ferry-cli
# into /usr/local/bin when that is writable; then opens the app.
# Environment:
#   FERRY_TAG          release to install, e.g. v1.7.0 (default: the latest)
#   FERRY_INSTALL_DIR  where to put Ferry.app
#   FERRY_NO_OPEN      set to anything to not open the app afterwards
#
# The site deploys this file (site/build.py) with each release.
set -eu

repo=simophin/ferryapp

say() { printf '%s\n' "$*"; }
fail() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# Everything runs from here, after sh has read the whole script, so a
# download cut short can't run half of it.
main() {
  [ "$(uname -s)" = Darwin ] || fail "this installs the macOS app; see https://simophin.github.io/ferryapp/ for other systems"
  # Keep in step with MACOSX_DEPLOYMENT_TARGET in the Build workflow.
  major=$(sw_vers -productVersion | cut -d. -f1)
  [ "$major" -ge 12 ] || fail "Ferry needs macOS 12 or later"

  tag=${FERRY_TAG:-}
  if [ -z "$tag" ]; then
    # releases/latest redirects to .../releases/tag/<tag>; the API would
    # work too, but is rate-limited.
    latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
      "https://github.com/$repo/releases/latest") ||
      fail "couldn't reach GitHub"
    tag=${latest##*/}
    case $tag in
      v[0-9]*) ;;
      *) fail "couldn't find the latest release (got $latest)" ;;
    esac
  fi
  # The name the Build workflow gives the DMG.
  dmg_name=ferry-${tag#v}-macos-universal.dmg
  url=https://github.com/$repo/releases/download/$tag/$dmg_name

  dest=${FERRY_INSTALL_DIR:-}
  if [ -z "$dest" ]; then
    if [ -w /Applications ]; then
      dest=/Applications
    else
      dest=$HOME/Applications
    fi
  fi
  mkdir -p "$dest"
  [ -w "$dest" ] || fail "can't write to $dest; set FERRY_INSTALL_DIR"
  app=$dest/Ferry.app

  tmp=$(mktemp -d)
  mount=$tmp/mount
  trap 'hdiutil detach -quiet "$mount" 2>/dev/null || true; rm -rf "$tmp"' EXIT
  trap 'exit 1' HUP INT TERM

  say "Downloading Ferry $tag..."
  curl -fL --progress-bar -o "$tmp/$dmg_name" "$url" ||
    fail "couldn't download $url"

  mkdir "$mount"
  hdiutil attach -quiet -nobrowse -readonly -noautoopen \
    -mountpoint "$mount" "$tmp/$dmg_name" || fail "couldn't open $dmg_name"
  [ -d "$mount/Ferry.app" ] || fail "$dmg_name has no Ferry.app"

  if pgrep -xq Ferry; then
    say "Quitting the running Ferry..."
    osascript -e 'quit app "Ferry"' >/dev/null 2>&1 || true
    i=0
    while pgrep -xq Ferry && [ $i -lt 20 ]; do
      sleep 0.5
      i=$((i + 1))
    done
    pgrep -xq Ferry && fail "Ferry is still running; quit it and run this again"
  fi

  say "Installing to $app..."
  rm -rf "$app"
  # ditto keeps the bundle's signature and extended attributes intact.
  ditto "$mount/Ferry.app" "$app"
  xattr -dr com.apple.quarantine "$app" 2>/dev/null || true
  hdiutil detach -quiet "$mount" || true

  cli=$app/Contents/MacOS/ferry-cli
  if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
    ln -sf "$cli" /usr/local/bin/ferry-cli
    say "Linked ferry-cli into /usr/local/bin."
  else
    say "To use ferry-cli, link it onto your PATH:"
    say "  sudo ln -sf '$cli' /usr/local/bin/ferry-cli"
  fi

  say "Ferry $tag is installed."
  if [ -z "${FERRY_NO_OPEN:-}" ]; then
    open "$app"
  fi
}

main "$@"
