#!/usr/bin/env sh
# vhrn installer: fetch the right prebuilt CLI from GitHub Releases and drop it on
# PATH. The container images are pulled on demand by `vhrn install <harness>`.
#
#   curl -fsSL https://raw.githubusercontent.com/aravind-n/vhrn/master/install.sh | sh
#
# Overridable via env: VHRN_REPO, VHRN_VERSION (default: latest), VHRN_BINDIR.
set -eu

REPO="${VHRN_REPO:-aravind-n/vhrn}"
VERSION="${VHRN_VERSION:-latest}"
BINDIR="${VHRN_BINDIR:-/usr/local/bin}"
BIN_NAME=vhrn

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$arch" in
  x86_64 | amd64) arch=amd64 ;;
  arm64 | aarch64) arch=arm64 ;;
  *) echo "vhrn: unsupported architecture: $arch" >&2; exit 1 ;;
esac
case "$os" in
  darwin | linux) ;;
  *) echo "vhrn: unsupported OS: $os" >&2; exit 1 ;;
esac

asset="${BIN_NAME}-${os}-${arch}"
if [ "$VERSION" = latest ]; then
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
  url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
echo "vhrn: downloading ${url}"
curl -fsSL "$url" -o "$tmp"
chmod +x "$tmp"

dest="${BINDIR}/${BIN_NAME}"
if [ -w "$BINDIR" ]; then
  mv "$tmp" "$dest"
else
  echo "vhrn: installing to ${dest} (sudo)"
  sudo mv "$tmp" "$dest"
fi
trap - EXIT

echo "vhrn: installed ${dest}"
echo "Next: run 'vhrn install claude' to pull the images and add a shell alias, then 'vhrn claude' in any project."
