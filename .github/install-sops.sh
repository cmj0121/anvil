#!/bin/sh
# Install sops for CI: Homebrew on macOS, the pinned release binary
# (SOPS_VERSION, checked against SOPS_SHA256) on Linux.
set -eu

case "$(uname -s)" in
  Darwin)
    brew install sops
    ;;
  Linux)
    bin="sops-$SOPS_VERSION.linux.amd64"
    curl -fsSLo /tmp/sops "https://github.com/getsops/sops/releases/download/$SOPS_VERSION/$bin"
    echo "$SOPS_SHA256  /tmp/sops" | sha256sum -c -
    sudo install -m 0755 /tmp/sops /usr/local/bin/sops
    ;;
esac
sops --version
