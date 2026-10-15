#!/bin/bash
# Install a verified release into the current user's terminal PATH directory.
set -euo pipefail

if [ "$#" -gt 1 ] || [ "${1:-}" = '--help' ]; then
    echo "Usage: bash install.sh [vX.Y.Z|latest]" >&2
    # shellcheck disable=SC2016 # Print the variable literally in usage help.
    echo 'Optional destination: DIAL_INSTALL_DIR (default: $HOME/.local/bin)' >&2
    echo 'DIAL_NO_MODIFY_PATH=1 leaves shell profiles unchanged; DIAL_REQUIRE_SIGNATURE=1 refuses' >&2
    echo 'to install without Ed25519 verification; DIAL_OPENSSL selects the OpenSSL 3 binary.' >&2
    [ "${1:-}" = '--help' ] && exit 0
    exit 1
fi

case "$(uname -s):$(uname -m)" in
    Darwin:arm64) target=aarch64-apple-darwin ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    *) echo 'Supported: macOS Apple Silicon/Intel or Linux x86-64. Native Windows: irm https://github.com/KiidxAtlas/dial/releases/latest/download/install.ps1 | iex' >&2; exit 1 ;;
esac

# Authenticated GitHub CLI also supports private releases without exposing tokens
# in command arguments, logs, or URLs. Public releases retain the curl path.
use_gh=false
if command -v gh >/dev/null && gh auth status --hostname github.com >/dev/null 2>&1; then
    use_gh=true
fi
required_tools='tar install mktemp'
if [ "$use_gh" = false ]; then required_tools="$required_tools curl"; fi
for tool in $required_tools; do
    command -v "$tool" >/dev/null || { echo "Missing required tool: $tool" >&2; exit 1; }
done
# The Ed25519 signature on SHA256SUMS needs OpenSSL 3+ (`pkeyutl -rawin`). macOS ships LibreSSL as
# `openssl`, which cannot verify Ed25519, so also try Homebrew's OpenSSL 3.
openssl_bin=
if [ -n "${DIAL_OPENSSL:-}" ]; then
    openssl_candidates=("$DIAL_OPENSSL")
else
    openssl_candidates=("$(command -v openssl || true)" /opt/homebrew/opt/openssl@3/bin/openssl
        /usr/local/opt/openssl@3/bin/openssl /opt/homebrew/opt/openssl/bin/openssl /usr/local/opt/openssl/bin/openssl)
fi
for candidate in "${openssl_candidates[@]}"; do
    [ -n "$candidate" ] && [ -x "$candidate" ] || continue
    # `-help` exits non-zero on some OpenSSL builds; only its text matters.
    case "$("$candidate" pkeyutl -help 2>&1 || true)" in
        *-rawin*) openssl_bin=$candidate; break ;;
    esac
done
if [ -z "${DIAL_RELEASE_PUBLIC_KEY:-}" ]; then
    echo 'DIAL_RELEASE_PUBLIC_KEY must contain the trusted Ed25519 public key PEM; refusing unsigned installation.' >&2
    exit 1
fi
if [ -z "$openssl_bin" ]; then
    if [ "${DIAL_REQUIRE_SIGNATURE:-0}" = 1 ]; then
        echo 'DIAL_REQUIRE_SIGNATURE=1 needs OpenSSL 3 or newer to verify the Ed25519 release signature. On macOS: brew install openssl@3' >&2
        exit 1
    fi
    # Without OpenSSL 3 the archive is still checked against the release's SHA256SUMS, fetched
    # over HTTPS from the same GitHub release.
    echo 'Note: OpenSSL 3 not found, so the release signature is not checked; verifying the SHA-256 checksum from the GitHub release instead. For signature verification: brew install openssl@3 (macOS) and rerun.' >&2
fi
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
    if [ "$use_gh" = true ]; then
        version=$(gh release view --repo KiidxAtlas/dial --json tagName --jq .tagName)
    else
        release_url=$(curl --fail --silent --show-error --location --retry 2 \
            --output /dev/null --write-out '%{url_effective}' "$repository/releases/latest")
        version=${release_url##*/}
    fi
fi
if ! [[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
    echo "Invalid release version (or no published release): $version" >&2
    exit 1
fi

package="dial-$version-$target"
archive="$package.tar.gz"
base="$repository/releases/download/$version"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/dial-install.XXXXXX")
trap 'rm -rf "$scratch"; [ -z "${staged:-}" ] || rm -f "$staged"' EXIT
if [ "$use_gh" = true ]; then
    gh release download "$version" --repo KiidxAtlas/dial --dir "$scratch" \
        --pattern "$archive" --pattern SHA256SUMS --pattern SHA256SUMS.sig
else
    for asset in "$archive" SHA256SUMS SHA256SUMS.sig; do
        curl --fail --silent --show-error --location --retry 2 "$base/$asset" --output "$scratch/$asset" || {
            echo "Could not download release asset $asset" >&2
            exit 1
        }
    done
fi
if [ -n "$openssl_bin" ]; then
    printf '%s\n' "$DIAL_RELEASE_PUBLIC_KEY" > "$scratch/release-public.pem"
    # Release signatures are one unwrapped line. Normalize optional wrapping before
    # decoding: OpenSSL's default decoder otherwise waits for a newline and emits no bytes.
    tr -d '[:space:]' < "$scratch/SHA256SUMS.sig" | "$openssl_bin" base64 -d -A > "$scratch/SHA256SUMS.sig.raw"
    if ! "$openssl_bin" pkeyutl -verify -pubin -inkey "$scratch/release-public.pem" -rawin \
        -in "$scratch/SHA256SUMS" -sigfile "$scratch/SHA256SUMS.sig.raw" >/dev/null; then
        echo 'Release signature verification failed; nothing was installed.' >&2
        exit 1
    fi
fi
line=$(awk -v name="$archive" '$2 == name || $2 == "*" name { print; exit }' "$scratch/SHA256SUMS")
read -r expected filename <<< "$line"
filename=${filename#\*}
if ! [[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || [ "$filename" != "$archive" ]; then
    echo 'Invalid release checksum manifest; nothing was installed.' >&2
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
staged=$(mktemp "$destination/.dial.new.XXXXXX")
backup="$destination/dial.bak"
install -m 755 "$scratch/$package/dial" "$staged"
if ! "$staged" --version | awk -v want="${version#v}" '$1 == "dial" && $2 == want { found = 1 } END { exit !found }'; then
    rm -f "$staged"
    echo 'Downloaded binary failed its version check; nothing was installed.' >&2
    exit 1
fi
if [ -e "$destination/dial" ]; then
    rm -f "$backup"
    mv "$destination/dial" "$backup"
fi
if ! mv "$staged" "$destination/dial"; then
    [ ! -e "$backup" ] || mv "$backup" "$destination/dial"
    echo 'Could not activate downloaded binary; previous version restored.' >&2
    exit 1
fi
if ! "$destination/dial" --version | awk -v want="${version#v}" '$1 == "dial" && $2 == want { found = 1 } END { exit !found }'; then
    rm -f "$destination/dial"
    [ ! -e "$backup" ] || mv "$backup" "$destination/dial"
    echo 'Installed binary failed its version check; previous version restored.' >&2
    exit 1
fi

# `curl | bash` cannot change the calling shell, so persist the PATH entry in the login shell's
# profile once; a later install finds the line and leaves the profile alone.
path_hint=
case ":$PATH:" in
    *":$destination:"*) ;;
    *)
        case "$(basename "${SHELL:-}")" in
            zsh) profile="${ZDOTDIR:-$HOME}/.zshrc" ;;
            bash) if [ "$(uname -s)" = Darwin ]; then profile="$HOME/.bash_profile"; else profile="$HOME/.bashrc"; fi ;;
            fish) profile="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish" ;;
            *) profile="$HOME/.profile" ;;
        esac
        case "$profile" in
            *.fish) line="fish_add_path \"$destination\"" ;;
            *) line="export PATH=\"$destination:\$PATH\"" ;;
        esac
        if [ "${DIAL_NO_MODIFY_PATH:-0}" = 1 ]; then
            path_hint="Add $destination to your PATH: $line"
        elif [ -f "$profile" ] && grep -Fqx "$line" "$profile"; then
            path_hint="Restart your terminal (or run: $line) to use dial."
        else
            mkdir -p "$(dirname "$profile")"
            printf '\n# Added by the Dial installer\n%s\n' "$line" >> "$profile"
            path_hint="Added $destination to PATH in $profile. Restart your terminal (or run: $line) to use dial."
        fi
        ;;
esac
printf 'Installed dial %s to %s/dial.\n' "${version#v}" "$destination"
[ -z "$path_hint" ] || printf '%s\n' "$path_hint"
echo 'Run dial in a project directory to get started.'
if [ "$target" = x86_64-unknown-linux-gnu ]; then
    echo 'Linux/WSL2: install bubblewrap and use a normal user; WSL Windows interop must be disabled (see README).'
fi
