#!/bin/sh
# Install minisign for the release job: the pinned Linux x86_64 release
# (MINISIGN_VERSION, its tarball checked against MINISIGN_SHA256).
set -eu

tarball="minisign-$MINISIGN_VERSION-linux.tar.gz"
dir=$(mktemp -d)
curl -fsSLo "$dir/$tarball" "https://github.com/jedisct1/minisign/releases/download/$MINISIGN_VERSION/$tarball"
echo "$MINISIGN_SHA256  $dir/$tarball" | sha256sum -c -
tar -xzf "$dir/$tarball" -C "$dir" minisign-linux/x86_64/minisign
sudo install -m 0755 "$dir/minisign-linux/x86_64/minisign" /usr/local/bin/minisign
rm -rf "$dir"
minisign -v
