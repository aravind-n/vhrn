#!/usr/bin/env sh
# vhrn installer: fetch the right prebuilt CLI from GitHub Releases and drop it on
# PATH. The container images are pulled on demand by `vhrn install <harness>`.
#
#   curl -fsSL https://aravind-n.github.io/vhrn/install.sh | sh
#
# Overridable via env: VHRN_REPO, VHRN_BINDIR, and VHRN_VERSION — the latest stable
# release by default, or a tag like v0.3.0 or nightly.
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
  base="https://github.com/${REPO}/releases/latest/download"
else
  base="https://github.com/${REPO}/releases/download/${VERSION}"
fi

tmp=$(mktemp)
sums=$(mktemp)
trap 'rm -f "$tmp" "$sums"' EXIT

echo "vhrn: downloading ${base}/${asset}"
curl -fsSL "${base}/${asset}" -o "$tmp"

# Verify the download against the release's SHA256SUMS.
curl -fsSL "${base}/SHA256SUMS" -o "$sums"
want=$(awk -v f="$asset" '$2 == f { print $1 }' "$sums")
[ -n "$want" ] || { echo "vhrn: no checksum for ${asset}" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  got=$(shasum -a 256 "$tmp" | cut -d' ' -f1)
else
  echo "vhrn: need sha256sum or shasum to verify the download" >&2; exit 1
fi
[ "$want" = "$got" ] || { echo "vhrn: checksum mismatch for ${asset}" >&2; exit 1; }
chmod +x "$tmp"

dest="${BINDIR}/${BIN_NAME}"
if [ -w "$BINDIR" ]; then
  mv "$tmp" "$dest"
else
  echo "vhrn: installing to ${dest} (sudo)"
  sudo mv "$tmp" "$dest"
fi

echo "vhrn: installed ${dest}"
echo "Next: run 'vhrn install claude' to pull the images and add a shell alias, then 'vhrn claude' in any project."
