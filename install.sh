#!/bin/sh
# Install altitude from a GitHub release.
set -eu

repo="gitsquared/altitude"
version="${ALTITUDE_VERSION:-}"
install_dir="${ALTITUDE_INSTALL_DIR:-${HOME}/.local/bin}"

usage() {
    cat <<'EOF'
usage: install.sh [--version VERSION] [--dir DIR]

environment:
  ALTITUDE_VERSION       release tag to install (default: latest)
  ALTITUDE_INSTALL_DIR   directory for the binary (default: ~/.local/bin)
EOF
}

die() {
    printf 'altitude: %s\n' "$*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || die "--version needs a value"
            version="$2"
            shift 2
            ;;
        --dir)
            [ $# -ge 2 ] || die "--dir needs a value"
            install_dir="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

command -v curl >/dev/null 2>&1 || die "curl is required"

case "$(uname -s)" in
    Linux)
        case "$(uname -m)" in
            x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
            *) die "unsupported Linux architecture: $(uname -m)" ;;
        esac
        ;;
    Darwin)
        case "$(uname -m)" in
            arm64) target="aarch64-apple-darwin" ;;
            *) die "altitude needs Apple silicon" ;;
        esac
        ;;
    *)
        die "unsupported operating system: $(uname -s)"
        ;;
esac

if [ -z "$version" ]; then
    latest="$(curl -fsSIL --retry 3 --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' \
        "https://github.com/${repo}/releases/latest")" \
        || die "could not find the latest release"
    case "$latest" in
        */tag/v*) version="${latest##*/}" ;;
        *) die "no released version found; set ALTITUDE_VERSION" ;;
    esac
fi
version="v${version#v}"
version_number="${version#v}"

asset="altitude-${version_number}-${target}.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM

curl -fsSL --retry 3 --proto '=https' --tlsv1.2 -o "$tmp/$asset" \
    "https://github.com/${repo}/releases/download/${version}/${asset}" \
    || die "could not download $asset"
curl -fsSL --retry 3 --proto '=https' --tlsv1.2 -o "$tmp/SHA256SUMS" \
    "https://github.com/${repo}/releases/download/${version}/SHA256SUMS" \
    || die "could not download SHA256SUMS"

expected="$(awk -v asset="$asset" '$2 == asset { print $1; exit }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || die "no checksum for $asset"

if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
else
    die "need sha256sum or shasum to verify the download"
fi
[ "$actual" = "$expected" ] || die "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
binary="$tmp/altitude-${version_number}-${target}/altitude"
[ -f "$binary" ] || die "archive did not contain the altitude binary"
"$binary" --version >/dev/null 2>&1 \
    || die "the downloaded binary does not run on this machine"

mkdir -p "$install_dir"
install -m 0755 "$binary" "$install_dir/altitude"

printf 'installed %s to %s\n' "$version" "$install_dir/altitude"
case ":${PATH}:" in
    *":${install_dir}:"*) ;;
    *) printf 'add it to PATH with: export PATH="%s:%s"\n' "$install_dir" "\$PATH" ;;
esac

if [ "$(uname -s)" = "Linux" ] && [ ! -e /dev/kvm ]; then
    printf 'note: /dev/kvm was not found; altitude needs Linux KVM for sandboxes\n' >&2
fi
