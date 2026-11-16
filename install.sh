#!/bin/sh
# Install gocut from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/emoss08/gocut/main/install.sh | sh
#
# GOCUT_VERSION       release tag to install, such as v0.2.0 (default: the latest release)
# GOCUT_INSTALL_DIR   where to put the binary (default: $HOME/.local/bin)
set -eu

repo="emoss08/gocut"
install_dir="${GOCUT_INSTALL_DIR:-$HOME/.local/bin}"

die() {
	printf 'gocut install: %s\n' "$*" >&2
	exit 1
}

command -v curl > /dev/null 2>&1 || die "curl is required"
command -v tar > /dev/null 2>&1 || die "tar is required"

case "$(uname -s)" in
Linux) os="unknown-linux-gnu" ;;
Darwin) os="apple-darwin" ;;
*) die "unsupported OS $(uname -s): gocut releases are built for Linux and macOS" ;;
esac

case "$(uname -m)" in
x86_64 | amd64) arch="x86_64" ;;
aarch64 | arm64) arch="aarch64" ;;
*) die "unsupported architecture $(uname -m): gocut releases are built for x86_64 and aarch64" ;;
esac

target="$arch-$os"

version="${GOCUT_VERSION:-}"
if [ -z "$version" ]; then
	latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
		die "could not look up the latest release"
	version="${latest##*/}"
	case "$version" in
	v*) ;;
	*) die "no release found at https://github.com/$repo/releases" ;;
	esac
fi

name="gocut-$version-$target"
base="https://github.com/$repo/releases/download/$version"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

printf 'Downloading gocut %s for %s\n' "$version" "$target"
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" ||
	die "no build for $target in $version (see https://github.com/$repo/releases/tag/$version)"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "could not download SHA256SUMS for $version"

want=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$want" ] || die "SHA256SUMS has no entry for $name.tar.gz"
if command -v sha256sum > /dev/null 2>&1; then
	got=$(sha256sum "$tmp/$name.tar.gz" | awk '{ print $1 }')
elif command -v shasum > /dev/null 2>&1; then
	got=$(shasum -a 256 "$tmp/$name.tar.gz" | awk '{ print $1 }')
else
	die "sha256sum or shasum is required to verify the download"
fi
[ "$want" = "$got" ] || die "checksum mismatch for $name.tar.gz (want $want, got $got)"

tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$install_dir"
cp "$tmp/$name/gocut" "$install_dir/gocut.tmp"
chmod 755 "$install_dir/gocut.tmp"
mv -f "$install_dir/gocut.tmp" "$install_dir/gocut"

printf 'Installed gocut %s to %s/gocut\n' "$version" "$install_dir"
case ":$PATH:" in
*":$install_dir:"*) ;;
*) printf 'Add %s to your PATH to run gocut by name.\n' "$install_dir" ;;
esac
