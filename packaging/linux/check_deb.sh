#!/bin/sh
# Install a .deb on this (clean, Debian or Ubuntu) system and check the app
# starts: under Xvfb and a private D-Bus session it must open its window,
# with its app id as the window class and its icon, and serve the API the
# installed CLI talks to. Only the package's Depends are installed, not its
# Recommends.
#
#   check_deb.sh PACKAGE
#
# Needs root (apt). Meant for a throwaway container, as the Build workflow
# runs it.
set -eu

if [ $# -ne 1 ]; then
  echo "usage: $0 PACKAGE" >&2
  exit 2
fi
package=$(realpath "$1")

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends "$package" \
  xvfb xauth dbus x11-utils python3 desktop-file-utils >/dev/null

test -x /usr/bin/ferry-gui
test -x /usr/bin/ferry-cli
# Debian 12's validator predates SingleMainWindow (Desktop Entry 1.5).
problems=$(desktop-file-validate /usr/share/applications/dev.fanchao.Ferry.desktop |
  grep -v '"SingleMainWindow"' || true)
if [ -n "$problems" ]; then
  echo "$problems" >&2
  exit 1
fi
test -f /usr/share/icons/hicolor/256x256/apps/dev.fanchao.Ferry.png
test -s /usr/share/doc/ferry/THIRD_PARTY_LICENSES.html
ferry-gui --version
ferry-cli --version

run=$(mktemp -d)
trap 'rm -rf "$run"' EXIT

cat >"$run/session.sh" <<SESSION
set -eu
ferry-gui --data-dir "$run/data" --download-dir "$run/downloads" \
  --discovery-loopback --no-system-clipboard --device-name "Package check" \
  --cli-access >"$run/app.log" 2>&1 &
app=\$!
trap 'kill \$app 2>/dev/null || true' EXIT
for _ in \$(seq 60); do
  if xwininfo -root -tree 2>/dev/null | grep -q '"Ferry"' &&
    ferry-cli --data-dir "$run/data" settings >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 \$app 2>/dev/null; then
    echo "the app exited"
    exit 1
  fi
  sleep 1
done
xwininfo -root -tree | grep '"Ferry"'
xprop -name Ferry WM_CLASS | tee /dev/stderr | grep -q '"dev.fanchao.Ferry"'
xprop -name Ferry _NET_WM_ICON | grep -q 'Icon'
ferry-cli --data-dir "$run/data" settings
kill \$app
wait \$app || true
SESSION

if ! dbus-run-session -- xvfb-run --auto-servernum sh "$run/session.sh"; then
  cat "$run/app.log"
  echo "check_deb.sh: the app didn't start" >&2
  exit 1
fi
cat "$run/app.log"
echo "check_deb.sh: $package installs and starts"
