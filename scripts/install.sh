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
    *) echo 'Supported: macOS Apple Silicon/Intel or Linux x86-64 (native Windows: use the PowerShell installer).' >&2; exit 1 ;;
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
if command -v sha256sum >/dev/null; then
    hash_tool=sha256sum
elif command -v shasum >/dev/null; then
    hash_tool=shasum
else
    echo 'Install sha256sum or shasum before installing Dial.' >&2
    exit 1
fi


# The public bootstrap handles private-release prerequisites; no Homebrew or Rust needed.
# Set DIAL_INSTALL_BOOTSTRAP=0 for an already-configured/offline automation environment.
if [ "${DIAL_INSTALL_BOOTSTRAP:-1}" != 0 ]; then
    gh_bin="$HOME/.local/share/dial/tools/bin"
    if ! command -v gh >/dev/null && [ -x "$gh_bin/gh" ] && "$gh_bin/gh" --version >/dev/null 2>&1; then
        export PATH="$gh_bin:$PATH"
    fi
    if ! command -v gh >/dev/null; then
        command -v curl >/dev/null || { echo 'curl is required to install GitHub CLI.' >&2; exit 1; }
        case "$target" in
            aarch64-apple-darwin) gh_platform=macOS_arm64; gh_suffix=zip ;;
            x86_64-apple-darwin) gh_platform=macOS_amd64; gh_suffix=zip ;;
            *) gh_platform=linux_amd64; gh_suffix=tar.gz ;;
        esac
        gh_release=$(curl --fail --silent --show-error --location --retry 2 \
            --output /dev/null --write-out '%{url_effective}' https://github.com/cli/cli/releases/latest)
        gh_version=${gh_release##*/}
        if ! [[ "$gh_version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
            echo 'Could not determine the GitHub CLI release; nothing was installed.' >&2; exit 1
        fi
        gh_package="gh_${gh_version#v}_$gh_platform"
        gh_archive="$gh_package.$gh_suffix"
        gh_temp=$(mktemp -d "${TMPDIR:-/tmp}/dial-gh.XXXXXX")
        trap 'rm -rf "$gh_temp"' EXIT
        gh_base="https://github.com/cli/cli/releases/download/$gh_version"
        echo 'Installing GitHub CLI for private release access...'
        curl --fail --silent --show-error --location --retry 2 "$gh_base/$gh_archive" --output "$gh_temp/$gh_archive"
        curl --fail --silent --show-error --location --retry 2 "$gh_base/gh_${gh_version#v}_checksums.txt" --output "$gh_temp/checksums.txt"
        expected=$(awk -v name="$gh_archive" '$2 == name {print $1}' "$gh_temp/checksums.txt")
        if ! [[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]]; then echo 'Invalid GitHub CLI checksum manifest.' >&2; exit 1; fi
        if [ "$hash_tool" = sha256sum ]; then
            digest=$(sha256sum "$gh_temp/$gh_archive")
        else
            digest=$(shasum -a 256 "$gh_temp/$gh_archive")
        fi
        [ "${digest%% *}" = "$expected" ] || { echo 'GitHub CLI checksum verification failed.' >&2; exit 1; }
        if [ "$gh_suffix" = zip ]; then
            command -v unzip >/dev/null || { echo 'unzip is required to install GitHub CLI.' >&2; exit 1; }
            unzip -p "$gh_temp/$gh_archive" "$gh_package/bin/gh" > "$gh_temp/gh"
        else
            tar -xOf "$gh_temp/$gh_archive" "$gh_package/bin/gh" > "$gh_temp/gh"
        fi
        gh_bin="$HOME/.local/share/dial/tools/bin"
        mkdir -p "$gh_bin"
        install -m 755 "$gh_temp/gh" "$gh_bin/gh"
        export PATH="$gh_bin:$PATH"
        rm -rf "$gh_temp"
        trap - EXIT
        gh --version >/dev/null
    fi
    if ! gh auth status --hostname github.com >/dev/null 2>&1; then
        echo 'Dial releases are private. Sign in with a GitHub account granted access to KiidxAtlas/dial.'
        # Piped installers must read the terminal, not the script being piped into bash.
        if ! ( : </dev/tty ) 2>/dev/null; then
            echo 'An interactive terminal or an authenticated GitHub CLI is required.' >&2; exit 1
        fi
        gh auth login --hostname github.com --web --git-protocol https </dev/tty
    fi
    gh api repos/KiidxAtlas/dial --silent >/dev/null 2>&1 || {
        echo 'Your GitHub account cannot access KiidxAtlas/dial. Ask the repository owner to grant access.' >&2; exit 1
    }
    use_gh=true
fi

version=${1:-latest}
repository=https://github.com/KiidxAtlas/dial
if [ "$version" = latest ]; then
    if [ "$use_gh" = true ]; then
        version=$(gh release view --repo KiidxAtlas/dial --json tagName --jq .tagName) || {
            echo 'No Dial release has been published yet; the existing installation was preserved.' >&2; exit 1
        }
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
trap 'rm -rf "$scratch"' EXIT
if [ "$use_gh" = true ]; then
    gh release download "$version" --repo KiidxAtlas/dial --dir "$scratch" \
        --pattern "$archive" --pattern "$archive.sha256" || {
            echo "The $target download is not published for $version, or access was denied. The existing installation was preserved." >&2
            exit 1
        }
else
    curl --fail --silent --show-error --location --retry 2 "$base/$archive" --output "$scratch/$archive" || {
        echo 'For private releases, install GitHub CLI and run gh auth login with an account granted repository access.' >&2
        exit 1
    }
    curl --fail --silent --show-error --location --retry 2 "$base/$archive.sha256" --output "$scratch/$archive.sha256"
fi

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
reported=$("$scratch/$package/dial" --version) || { echo 'The downloaded binary could not start; the existing installation was preserved.' >&2; exit 1; }
if [ "$reported" != "dial ${version#v}" ]; then
    echo 'The downloaded binary failed its version check; the existing installation was preserved.' >&2; exit 1
fi
destination=${DIAL_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$destination"
install -m 755 "$scratch/$package/dial" "$destination/dial"
"$destination/dial" --version
printf 'Installed Dial to %s/dial\n' "$destination"
# Persist PATH once in the user's usual shell startup file; preserve all existing content.
if [ "${DIAL_INSTALL_BOOTSTRAP:-1}" != 0 ]; then
    case "${SHELL:-/bin/bash}" in
        */zsh) profile="$HOME/.zshrc" ;;
        */bash) if [ "$(uname -s)" = Darwin ]; then profile="$HOME/.bash_profile"; else profile="$HOME/.bashrc"; fi ;;
        */fish) profile="$HOME/.config/fish/conf.d/dial.fish" ;;
        *) profile= ;;
    esac
    case "$destination" in
        *$'\n'*) echo 'Install directory contains a newline; add it to PATH manually.' >&2; profile= ;;
    esac
    if [ -n "$profile" ]; then
        mkdir -p "$(dirname "$profile")"
        quoted=$(printf '%s' "$destination" | sed "s/'/'\\\\''/g")
        if [[ "$profile" = *.fish ]]; then
            path_line="fish_add_path '$quoted'"
        else
            path_line="export PATH='$quoted':\"\$PATH\""
        fi
        if ! grep -Fqx "$path_line" "$profile" 2>/dev/null; then
            printf '\n# Dial installer PATH\n%s\n' "$path_line" >> "$profile"
        fi
    fi
fi
if [ "$destination" = "$HOME/.local/bin" ]; then
    echo 'Installed. Open a new terminal and run dial, or start now with: ~/.local/bin/dial'
else
    printf 'Installed. Start now with: "%s/dial"\n' "$destination"
fi
if [ "$target" = x86_64-unknown-linux-gnu ]; then
    echo 'Linux/WSL2: install bubblewrap and use a normal user; WSL Windows interop must be disabled (see README).'
fi
