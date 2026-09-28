#!/bin/sh
# Build Ferry's signed apt repository from a release's .debs.
#
#   GNUPGHOME=... apt_repo.sh OUT_DIR DEB...
#
# GNUPGHOME signs with the release key (packaging/release_key.sh). OUT_DIR
# becomes a flat "stable" suite with one component, main, holding only
# these packages, which the website serves at $URL:
#
#   OUT_DIR/pool/main/ferry_<version>_<arch>.deb
#   OUT_DIR/dists/stable/{InRelease,Release,Release.gpg}
#   OUT_DIR/dists/stable/main/binary-<arch>/Packages{,.gz}
#   OUT_DIR/ferry.gpg       the release key, for /etc/apt/keyrings
#   OUT_DIR/ferry.sources   the source, for /etc/apt/sources.list.d
#
# Needs apt-ftparchive (apt-utils), dpkg-deb and gpg. check_apt_repo.sh
# installs from the result.
set -eu

URL=https://simophin.github.io/ferryapp/apt

if [ $# -lt 2 ]; then
  echo "usage: $0 OUT_DIR DEB..." >&2
  exit 2
fi
out=$1
shift
key=$(cd "$(dirname "$0")/.." && pwd)/release-key.asc

rm -rf "$out"
mkdir -p "$out/pool/main"
cp "$@" "$out/pool/main/"
cd "$out"

archs=$(for deb in pool/main/*.deb; do dpkg-deb --field "$deb" Architecture; done | sort -u | tr '\n' ' ')
archs=${archs% }
for arch in $archs; do
  dir=dists/stable/main/binary-$arch
  mkdir -p "$dir"
  apt-ftparchive --arch "$arch" packages pool >"$dir/Packages"
  gzip -9nk "$dir/Packages"
done
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=Ferry \
  -o APT::FTPArchive::Release::Label=Ferry \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o "APT::FTPArchive::Release::Architectures=$archs" \
  -o APT::FTPArchive::Release::Components=main \
  -o "APT::FTPArchive::Release::Description=Ferry, a KDE Connect client" \
  release dists/stable >Release.tmp
mv Release.tmp dists/stable/Release
gpg --clearsign --output dists/stable/InRelease dists/stable/Release
gpg --detach-sign --armor --output dists/stable/Release.gpg dists/stable/Release

gpg --dearmor <"$key" >ferry.gpg
# The signatures must check against the published key alone.
gpgv --keyring "$PWD/ferry.gpg" dists/stable/InRelease
gpgv --keyring "$PWD/ferry.gpg" dists/stable/Release.gpg dists/stable/Release

cat >ferry.sources <<SOURCES
Types: deb
URIs: $URL
Suites: stable
Components: main
Architectures: $archs
Signed-By: /etc/apt/keyrings/ferry.gpg
SOURCES
