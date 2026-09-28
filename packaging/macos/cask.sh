#!/bin/sh
# Print Ferry's Homebrew cask for a DMG, from ferry.rb.in.
#
#   cask.sh VERSION URL DMG > ferry.rb
#
# VERSION is MAJOR.MINOR.PATCH, URL where Homebrew downloads the DMG from
# (the release's asset, or a file:// URL to test a build), and DMG that
# file here, for its checksum.
set -eu

if [ $# -ne 3 ]; then
  echo "usage: $0 VERSION URL DMG" >&2
  exit 2
fi
version=$1
url=$2
dmg=$3

sha256=$(shasum -a 256 "$dmg" 2>/dev/null || sha256sum "$dmg")
sed -e "s|@VERSION@|$version|" \
  -e "s|@URL@|$url|" \
  -e "s|@SHA256@|${sha256%% *}|" \
  "$(dirname "$0")/ferry.rb.in"
