#!/bin/bash
# Install a verified release into the current user's terminal PATH directory.
set -euo pipefail

if [ "$#" -gt 1 ] || [ "${1:-}" = '--help' ]; then
    echo "Usage: bash install.sh [vX.Y.Z|latest]" >&2
    # shellcheck disable=SC2016 # Print the variable literally in usage help.
    echo 'Optional destination: DIAL_INSTALL_DIR (default: $HOME/.local/bin)' >&2
    echo 'DIAL_NO_MODIFY_PATH=1 leaves shell profiles unchanged.' >&2
    echo 'Signature verifiers, tried in order: DIAL_OPENSSL (OpenSSL 3+), python3 with the' >&2
    echo '`cryptography` package (DIAL_PYTHON selects it), and ssh-keygen (DIAL_SSH_KEYGEN selects it).' >&2
    echo 'With none available the install is refused. DIAL_ALLOW_UNSIGNED_INSTALL=1 (insecure) installs' >&2
    echo 'with only the SHA-256 checksum from the same release.' >&2
    [ "${1:-}" = '--help' ] && exit 0
    exit 1
fi

case "$(uname -s):$(uname -m)" in
    Darwin:arm64) target=aarch64-apple-darwin ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Linux:x86_64 | Linux:amd64 | Linux:aarch64 | Linux:arm64)
        case "$(uname -m)" in
            x86_64 | amd64) linux_arch=x86_64 ;;
            *) linux_arch=aarch64 ;;
        esac
        # Prefer the active libc reported by ldd: glibc development hosts can also have
        # musl loaders installed. Only use loader presence when ldd is unavailable.
        if { command -v ldd >/dev/null && { ldd --version 2>&1 || true; } | grep -qi musl; } \
            || { ! command -v ldd >/dev/null && compgen -G '/lib/ld-musl-*.so.1' >/dev/null; }; then
            target=$linux_arch-unknown-linux-musl
        else
            target=$linux_arch-unknown-linux-gnu
        fi
        ;;
    *) echo 'Supported: macOS (Apple Silicon or Intel) or Linux (x86-64 or arm64). Native Windows: irm https://github.com/KiidxAtlas/dial/releases/latest/download/install.ps1 | iex' >&2; exit 1 ;;
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

# SHA256SUMS is signed with the release Ed25519 key. The first available verifier is used:
#   1. OpenSSL 3+ (`pkeyutl -rawin`). macOS ships LibreSSL as `openssl`, which cannot verify
#      Ed25519, so Homebrew's OpenSSL 3 is also tried.
#   2. python3 with the `cryptography` package.
#   3. OpenSSH `ssh-keygen -Y verify` over SHA256SUMS.sshsig (present on macOS and modern Linux).
# If none is available the install is refused, unless DIAL_ALLOW_UNSIGNED_INSTALL=1 is set.
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
python_bin=${DIAL_PYTHON:-python3}
ssh_keygen_bin=${DIAL_SSH_KEYGEN:-ssh-keygen}
if [ -z "${DIAL_RELEASE_PUBLIC_KEY:-}" ]; then
    echo 'DIAL_RELEASE_PUBLIC_KEY must contain the trusted Ed25519 public key PEM; refusing unsigned installation.' >&2
    exit 1
fi
signature_verifier=
if [ -n "$openssl_bin" ]; then
    signature_verifier=openssl
elif command -v "$python_bin" >/dev/null && "$python_bin" -c 'import cryptography.hazmat.primitives.asymmetric.ed25519' >/dev/null 2>&1; then
    signature_verifier=python
elif command -v "$ssh_keygen_bin" >/dev/null; then
    signature_verifier=ssh-keygen
elif [ "${DIAL_ALLOW_UNSIGNED_INSTALL:-0}" = 1 ]; then
    signature_verifier=none
    echo 'WARNING: DIAL_ALLOW_UNSIGNED_INSTALL=1 and no signature verifier is available. The SHA-256 checksum from the GitHub release is checked, but the release signature is NOT. Install OpenSSL 3 (macOS: brew install openssl@3) to verify signatures.' >&2
else
    echo 'No release signature verifier found: install OpenSSL 3 (macOS: brew install openssl@3), or Python 3 with the cryptography package, or OpenSSH ssh-keygen. Refusing to install an unverified release. DIAL_ALLOW_UNSIGNED_INSTALL=1 installs with checksum verification only (insecure).' >&2
    exit 1
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
# SHA256SUMS.sshsig exists only for releases that publish the OpenSSH signature; fetch it only
# when ssh-keygen is the verifier, so older releases still install through OpenSSL or Python.
assets=("$archive" SHA256SUMS SHA256SUMS.sig)
if [ "$signature_verifier" = ssh-keygen ]; then assets+=(SHA256SUMS.sshsig); fi
if [ "$use_gh" = true ]; then
    patterns=()
    for asset in "${assets[@]}"; do patterns+=(--pattern "$asset"); done
    gh release download "$version" --repo KiidxAtlas/dial --dir "$scratch" "${patterns[@]}"
else
    for asset in "${assets[@]}"; do
        curl --fail --silent --show-error --location --retry 2 "$base/$asset" --output "$scratch/$asset" || {
            echo "Could not download release asset $asset" >&2
            exit 1
        }
    done
fi

reject_signature() {
    echo 'Release signature verification failed; nothing was installed.' >&2
    exit 1
}
printf '%s\n' "$DIAL_RELEASE_PUBLIC_KEY" > "$scratch/release-public.pem"
[ "$signature_verifier" = none ] || echo "Verifying release signature with $signature_verifier." >&2
case "$signature_verifier" in
    openssl | python)
        # Release signatures are one unwrapped line; strip any wrapping before decoding.
        tr -d '[:space:]' < "$scratch/SHA256SUMS.sig" | base64 -d > "$scratch/SHA256SUMS.sig.raw" || reject_signature
        if [ "$signature_verifier" = openssl ]; then
            "$openssl_bin" pkeyutl -verify -pubin -inkey "$scratch/release-public.pem" -rawin \
                -in "$scratch/SHA256SUMS" -sigfile "$scratch/SHA256SUMS.sig.raw" >/dev/null 2>&1 || reject_signature
        else
            "$python_bin" - "$scratch/release-public.pem" "$scratch/SHA256SUMS" "$scratch/SHA256SUMS.sig.raw" <<'PY' >/dev/null 2>&1 || reject_signature
import sys
from cryptography.hazmat.primitives.serialization import load_pem_public_key

with open(sys.argv[1], 'rb') as key_file:
    public_key = load_pem_public_key(key_file.read())
with open(sys.argv[2], 'rb') as data_file, open(sys.argv[3], 'rb') as sig_file:
    public_key.verify(sig_file.read(), data_file.read())
PY
        fi
        ;;
    ssh-keygen)
        # The OpenSSH public key blob is ssh-string("ssh-ed25519") + ssh-string(raw 32-byte key);
        # the raw key is the last 32 bytes of the SPKI DER behind the PEM body.
        pem_body=$(sed -n '/BEGIN PUBLIC KEY/,/END PUBLIC KEY/p' "$scratch/release-public.pem" | sed '1d;$d' | tr -d '[:space:]')
        case "$pem_body" in
            MCowBQYDK2VwAyEA*) ;;
            *) echo 'DIAL_RELEASE_PUBLIC_KEY is not an Ed25519 public key; nothing was installed.' >&2; exit 1 ;;
        esac
        key_blob=$({ printf '\x00\x00\x00\x0bssh-ed25519\x00\x00\x00\x20'; printf '%s' "$pem_body" | base64 -d | tail -c 32; } | base64 | tr -d '\n') || reject_signature
        printf 'dial-release ssh-ed25519 %s\n' "$key_blob" > "$scratch/allowed_signers"
        "$ssh_keygen_bin" -Y verify -f "$scratch/allowed_signers" -I dial-release -n dial-release \
            -s "$scratch/SHA256SUMS.sshsig" < "$scratch/SHA256SUMS" >/dev/null 2>&1 || reject_signature
        ;;
    none) ;;
esac
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
case "$target" in
    *-linux-*) echo 'Linux/WSL2: install bubblewrap and use a normal user; WSL Windows interop must be disabled (see README).' ;;
esac
