#!/bin/sh
# Install a verified release into the current user's terminal PATH directory.
# POSIX sh (dash, busybox ash, bash, zsh): `curl -fsSL https://dial.rootatlas.dev/install | sh`.
set -eu

if [ "$#" -gt 1 ] || [ "${1:-}" = '--help' ]; then
    echo "Usage: sh install.sh [vX.Y.Z|latest]" >&2
    # shellcheck disable=SC2016 # Print the variable literally in usage help.
    echo 'Optional destination: DIAL_INSTALL_DIR (default: $HOME/.local/bin)' >&2
    echo 'DIAL_NO_MODIFY_PATH=1 leaves shell profiles unchanged.' >&2
    echo 'Signature verifiers, tried in order: OpenSSH ssh-keygen (DIAL_SSH_KEYGEN selects it), OpenSSL 3+' >&2
    echo '(DIAL_OPENSSL), and python3 with the cryptography package (DIAL_PYTHON selects it).' >&2
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
        # musl loaders installed. Only use loader presence when ldd is unavailable; `ls`
        # fails when the glob matches nothing (the pattern is then passed literally).
        if command -v ldd >/dev/null 2>&1; then
            if { ldd --version 2>&1 || true; } | grep -qi musl; then libc=musl; else libc=gnu; fi
        elif ls /lib/ld-musl-*.so.1 >/dev/null 2>&1; then
            libc=musl
        else
            libc=gnu
        fi
        target=$linux_arch-unknown-linux-$libc
        ;;
    *) echo 'Supported: macOS (Apple Silicon or Intel) or Linux (x86-64 or arm64). Native Windows: irm https://dial.rootatlas.dev/install | iex' >&2; exit 1 ;;
esac

# Authenticated GitHub CLI also supports private releases without exposing tokens
# in command arguments, logs, or URLs. Public releases retain the curl path.
use_gh=false
if command -v gh >/dev/null 2>&1 && gh auth status --hostname github.com >/dev/null 2>&1; then
    use_gh=true
fi
required_tools='tar install mktemp'
if [ "$use_gh" = false ]; then required_tools="$required_tools curl"; fi
for tool in $required_tools; do
    command -v "$tool" >/dev/null 2>&1 || { echo "Missing required tool: $tool" >&2; exit 1; }
done

# SHA256SUMS is signed with the release Ed25519 key. Verifiers, in order of preference:
#   1. OpenSSH `ssh-keygen -Y verify` (OpenSSH 8.1+) over SHA256SUMS.sshsig. It ships with
#      macOS and nearly every Linux, so most installs need nothing extra.
#   2. OpenSSL 3+ (`pkeyutl -rawin`). macOS ships LibreSSL as `openssl`, which cannot verify
#      Ed25519, so Homebrew's OpenSSL 3 is also tried.
#   3. python3 with the `cryptography` package.
# OpenSSL and Python verify SHA256SUMS.sig, which every release has; they also cover older
# releases published without SHA256SUMS.sshsig. If no verifier can run the install is refused,
# unless DIAL_ALLOW_UNSIGNED_INSTALL=1 is set.
if [ -z "${DIAL_RELEASE_PUBLIC_KEY:-}" ]; then
    echo 'DIAL_RELEASE_PUBLIC_KEY must contain the trusted Ed25519 public key PEM; refusing unsigned installation.' >&2
    exit 1
fi
ssh_keygen_bin=
candidate=${DIAL_SSH_KEYGEN:-ssh-keygen}
# OpenSSH before 8.1 has no -Y and reports an unknown/illegal option instead.
if command -v "$candidate" >/dev/null 2>&1 \
    && { "$candidate" -Y 2>&1 || true; } | grep -q 'requires an argument'; then
    ssh_keygen_bin=$candidate
fi
openssl_bin=
openssl_supports_ed25519() {
    # `-help` exits non-zero on some OpenSSL builds; only its text matters.
    [ -n "$1" ] && [ -x "$1" ] && { "$1" pkeyutl -help 2>&1 || true; } | grep -q -- '-rawin'
}
if [ -n "${DIAL_OPENSSL:-}" ]; then
    if openssl_supports_ed25519 "$DIAL_OPENSSL"; then openssl_bin=$DIAL_OPENSSL; fi
else
    for candidate in "$(command -v openssl || true)" /opt/homebrew/opt/openssl@3/bin/openssl \
        /usr/local/opt/openssl@3/bin/openssl /opt/homebrew/opt/openssl/bin/openssl /usr/local/opt/openssl/bin/openssl; do
        if openssl_supports_ed25519 "$candidate"; then openssl_bin=$candidate; break; fi
    done
fi
python_bin=${DIAL_PYTHON:-python3}
if ! command -v "$python_bin" >/dev/null 2>&1 \
    || ! "$python_bin" -c 'import cryptography.hazmat.primitives.asymmetric.ed25519' >/dev/null 2>&1; then
    python_bin=
fi
if [ -z "$ssh_keygen_bin$openssl_bin$python_bin" ]; then
    if [ "${DIAL_ALLOW_UNSIGNED_INSTALL:-0}" = 1 ]; then
        echo 'WARNING: DIAL_ALLOW_UNSIGNED_INSTALL=1 and no signature verifier is available. The SHA-256 checksum from the GitHub release is checked, but the release signature is NOT. Install OpenSSH 8.1+ or OpenSSL 3 (macOS: brew install openssl@3) to verify signatures.' >&2
    else
        echo 'No release signature verifier found: install OpenSSH 8.1+ (ssh-keygen), OpenSSL 3 (macOS: brew install openssl@3), or Python 3 with the cryptography package. Refusing to install an unverified release. DIAL_ALLOW_UNSIGNED_INSTALL=1 installs with checksum verification only (insecure).' >&2
        exit 1
    fi
fi
if command -v sha256sum >/dev/null 2>&1; then
    hash_tool=sha256sum
elif command -v shasum >/dev/null 2>&1; then
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
newline='
'
case "$version" in
    *"$newline"*) valid_version=false ;;
    *) if printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'; then
           valid_version=true
       else
           valid_version=false
       fi ;;
esac
if [ "$valid_version" != true ]; then
    echo "Invalid release version (or no published release): $version" >&2
    exit 1
fi

package="dial-$version-$target"
archive="$package.tar.gz"
base="$repository/releases/download/$version"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/dial-install.XXXXXX")
staged=
trap 'rm -rf "$scratch"; [ -z "$staged" ] || rm -f "$staged"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# fetch ASSET: download one release asset into the scratch directory; non-zero when unavailable.
fetch() {
    if [ "$use_gh" = true ]; then
        gh release download "$version" --repo KiidxAtlas/dial --dir "$scratch" --pattern "$1" >/dev/null 2>&1
    else
        curl --fail --silent --show-error --location --retry 2 "$base/$1" --output "$scratch/$1" 2>/dev/null
    fi
}
for asset in "$archive" SHA256SUMS; do
    fetch "$asset" || { echo "Could not download release asset $asset" >&2; exit 1; }
done
# SHA256SUMS.sshsig exists only for releases that publish the OpenSSH signature; without it,
# fall back to SHA256SUMS.sig with OpenSSL or Python.
signature_verifier=none
if [ -n "$ssh_keygen_bin" ] && fetch SHA256SUMS.sshsig; then
    signature_verifier=ssh-keygen
elif [ -n "$openssl_bin$python_bin" ]; then
    fetch SHA256SUMS.sig || { echo 'Could not download release asset SHA256SUMS.sig' >&2; exit 1; }
    if [ -n "$openssl_bin" ]; then signature_verifier=openssl; else signature_verifier=python; fi
elif [ -n "$ssh_keygen_bin" ]; then
    echo 'This release has no SHA256SUMS.sshsig for ssh-keygen; install OpenSSL 3 (macOS: brew install openssl@3) or Python 3 with the cryptography package to verify it. Nothing was installed.' >&2
    exit 1
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
        printf '%s' "$pem_body" | base64 -d > "$scratch/release-public.der" || reject_signature
        { printf '\000\000\000\013ssh-ed25519\000\000\000\040'; tail -c 32 "$scratch/release-public.der"; } \
            > "$scratch/release-public.blob"
        key_blob=$(base64 < "$scratch/release-public.blob" | tr -d '\n')
        [ -n "$key_blob" ] || reject_signature
        printf 'dial-release ssh-ed25519 %s\n' "$key_blob" > "$scratch/allowed_signers"
        "$ssh_keygen_bin" -Y verify -f "$scratch/allowed_signers" -I dial-release -n dial-release \
            -s "$scratch/SHA256SUMS.sshsig" < "$scratch/SHA256SUMS" >/dev/null 2>&1 || reject_signature
        ;;
    none) ;;
esac
expected=$(awk -v name="$archive" '$2 == name || $2 == "*" name { print $1; exit }' "$scratch/SHA256SUMS")
if ! printf '%s\n' "$expected" | grep -Eqx '[0-9a-fA-F]{64}'; then
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
reports_version() {
    "$1" --version | awk -v want="${version#v}" '$1 == "dial" && $2 == want { found = 1 } END { exit !found }'
}
if ! reports_version "$staged"; then
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
staged=
if ! reports_version "$destination/dial"; then
    rm -f "$destination/dial"
    [ ! -e "$backup" ] || mv "$backup" "$destination/dial"
    echo 'Installed binary failed its version check; previous version restored.' >&2
    exit 1
fi

# `curl | sh` cannot change the calling shell, so persist the PATH entry in the login shell's
# profile once; a later install finds the line and leaves the profile alone.
path_hint=
case ":$PATH:" in
    *":$destination:"*) ;;
    *)
        case "$(basename "${SHELL:-sh}")" in
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
    *-linux-*)
        if ! command -v bwrap >/dev/null 2>&1; then
            if command -v apt-get >/dev/null 2>&1; then bwrap_install='sudo apt-get install -y bubblewrap'
            elif command -v dnf >/dev/null 2>&1; then bwrap_install='sudo dnf install -y bubblewrap'
            elif command -v pacman >/dev/null 2>&1; then bwrap_install='sudo pacman -S bubblewrap'
            elif command -v zypper >/dev/null 2>&1; then bwrap_install='sudo zypper install bubblewrap'
            elif command -v apk >/dev/null 2>&1; then bwrap_install='sudo apk add bubblewrap'
            else bwrap_install='install the bubblewrap package'; fi
            echo "Dial runs commands inside a bubblewrap sandbox, which is not installed. Run: $bwrap_install"
        fi
        if grep -qi microsoft /proc/version 2>/dev/null; then
            echo 'WSL2: disable Windows interop before running commands (see docs/windows.md); run dial as a normal user.'
        fi
        ;;
esac
