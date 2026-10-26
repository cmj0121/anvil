#!/bin/sh
# Package a release build: dist/anvil-<version>-<target>.tar.gz holding
# anvil-<version>-<target>/{anvil,README.md}, plus a sha256sum-format .sha256.
# Usage: package.sh <target> <version>
set -eu

if [ $# -ne 2 ]; then
  echo "usage: $0 <target> <version>" >&2
  exit 2
fi
target=$1
version=$2
name="anvil-$version-$target"
bin="target/$target/release/anvil"

if [ ! -x "$bin" ]; then
  echo "$0: $bin not found; run cargo build --release --target $target first" >&2
  exit 1
fi

mkdir -p dist
rm -rf "dist/$name"
mkdir "dist/$name"
cp "$bin" README.md "dist/$name/"
# Keep macOS tar from adding AppleDouble (._*) entries.
COPYFILE_DISABLE=1 tar -C dist -czf "dist/$name.tar.gz" "$name"
rm -rf "dist/$name"

cd dist
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$name.tar.gz" >"$name.tar.gz.sha256"
else
  shasum -a 256 "$name.tar.gz" >"$name.tar.gz.sha256"
fi
cat "$name.tar.gz.sha256"
