#!/bin/bash
# Install a verified release into the current user's terminal PATH directory.
set -euo pipefail

if [ "$#" -gt 1 ] || [ "${1:-}" = '--help' ]; then
    echo "Usage: bash install.sh [vX.Y.Z|latest]" >&2
    # shellcheck disable=SC2016 # Print the variable literally in usage help.
    echo 'Optional destination: DIAL_INSTALL_DIR (default: $HOME/.local/bin)' >&2
    [ "${1:-}" = '--help' ] && exit 0
    exit 1
fi

case "$(uname -s):$(uname -m)" in
    Darwin:arm64) target=aarch64-apple-darwin ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    *) echo 'Supported: macOS Apple Silicon/Intel or Linux x86-64 (Windows users: run in WSL2 Ubuntu).' >&2; exit 1 ;;
esac

for tool in curl tar install mktemp; do
    command -v "$tool" >/dev/null || { echo "Missing required tool: $tool" >&2; exit 1; }
done
if command -v sha256sum >/dev/null; then
    hash_tool=sha256sum
elif command -v shasum >/dev/null; then
    hash_tool=shasum
else
    echo 'Install sha256sum or shasum before installing Dial.' >&2
    exit 1
fi

version=${1:-latest}
repository=https://github.com/KiidxAtlas/dial
if [ "$version" = latest ]; then
    release_url=$(curl --fail --silent --show-error --location --retry 2 \
        --output /dev/null --write-out '%{url_effective}' "$repository/releases/latest")
    version=${release_url##*/}
fi
if ! [[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
    echo "Invalid release version (or no published release): $version" >&2
    exit 1
fi

package="dial-$version-$target"
archive="$package.tar.gz"
base="$repository/releases/download/$version"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/dial-install.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
curl --fail --silent --show-error --location --retry 2 "$base/$archive" --output "$scratch/$archive"
curl --fail --silent --show-error --location --retry 2 "$base/$archive.sha256" --output "$scratch/$archive.sha256"

read -r expected filename < "$scratch/$archive.sha256"
if ! [[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || [ "$filename" != "$archive" ]; then
    echo 'Invalid release checksum file; nothing was installed.' >&2
    exit 1
fi
if [ "$hash_tool" = sha256sum ]; then
    digest=$(sha256sum "$scratch/$archive")
else
    digest=$(shasum -a 256 "$scratch/$archive")
fi
actual=${digest%% *}
if [ "$actual" != "$expected" ]; then
    echo 'Release checksum verification failed; nothing was installed.' >&2
    exit 1
fi

tar -xzf "$scratch/$archive" -C "$scratch" "$package/dial"
if [ ! -f "$scratch/$package/dial" ] || [ -L "$scratch/$package/dial" ]; then
    echo 'Release archive did not contain a regular Dial binary.' >&2
    exit 1
fi
destination=${DIAL_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$destination"
install -m 755 "$scratch/$package/dial" "$destination/dial"
"$destination/dial" --version
printf 'Installed Dial to %s/dial\n' "$destination"
printf 'Add %s to your PATH, restart your terminal, then run dial in a project.\n' "$destination"
if [ "$target" = x86_64-unknown-linux-gnu ]; then
    echo 'Linux/WSL2: install bubblewrap and use a normal user; WSL Windows interop must be disabled (see README).'
fi
