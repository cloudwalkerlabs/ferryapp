#!/bin/sh
# Install Ferry from the apt repository apt_repo.sh built, on a clean
# Debian or Ubuntu, as its users would (the website's instructions), except
# that the source points at REPO_DIR instead of the website.
#
#   check_apt_repo.sh REPO_DIR
#
# Runs as root, in a throwaway container: it changes the system's apt
# sources. apt checks the signatures against ferry.gpg alone.
set -eu

if [ $# -ne 1 ]; then
  echo "usage: $0 REPO_DIR" >&2
  exit 2
fi
# apt downloads as the _apt user, which must be able to read it.
repo=/srv/ferry-apt
rm -rf "$repo"
cp -R "$1" "$repo"
chmod -R a+rX "$repo"

export DEBIAN_FRONTEND=noninteractive
install -d -m 755 /etc/apt/keyrings
install -m 644 "$repo/ferry.gpg" /etc/apt/keyrings/ferry.gpg
sed "s|^URIs: .*|URIs: file:$repo|" "$repo/ferry.sources" \
  >/etc/apt/sources.list.d/ferry.sources
apt-get update
apt-get install -y --no-install-recommends ferry
ferry-cli --version
test -x /usr/bin/ferry-gui
test -s /usr/share/doc/ferry/copyright
