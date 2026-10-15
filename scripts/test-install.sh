#!/bin/bash
# Offline behavioral tests: all downloads and installation paths stay in a disposable fixture.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$root/.tmp"
test_root=$(mktemp -d "$root/.tmp/install-test.XXXXXX")
trap 'rm -rf "$test_root"' EXIT
mkdir -p "$test_root/bin" "$test_root/releases" "$test_root/temp"

# Fixture signing needs OpenSSL 3+; macOS's LibreSSL `openssl` cannot sign Ed25519.
openssl=
for candidate in "$(command -v openssl || true)" /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
    [ -n "$candidate" ] && [ -x "$candidate" ] || continue
    case "$("$candidate" pkeyutl -help 2>&1 || true)" in *-rawin*) openssl=$candidate; break ;; esac
done
[ -n "$openssl" ] || { echo 'Installer tests need OpenSSL 3+ (macOS: brew install openssl@3)' >&2; exit 1; }
"$openssl" genpkey -algorithm ED25519 -out "$test_root/release-private.pem" >/dev/null 2>&1
"$openssl" pkey -in "$test_root/release-private.pem" -pubout -out "$test_root/release-public.pem" >/dev/null 2>&1
export DIAL_RELEASE_PUBLIC_KEY="$(cat "$test_root/release-public.pem")"
for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
    package="dial-v0.1.0-$target"
    mkdir -p "$test_root/releases/$package"
    printf '#!/bin/sh\necho "dial 0.1.0"\n' > "$test_root/releases/$package/dial"
    chmod +x "$test_root/releases/$package/dial"
    tar -czf "$test_root/releases/$package.tar.gz" -C "$test_root/releases" "$package"
done
(cd "$test_root/releases" && shasum -a 256 dial-v0.1.0-*.tar.gz > SHA256SUMS)
"$openssl" pkeyutl -sign -inkey "$test_root/release-private.pem" -rawin \
    -in "$test_root/releases/SHA256SUMS" -out "$test_root/releases/SHA256SUMS.sig.raw"
"$openssl" base64 -A -in "$test_root/releases/SHA256SUMS.sig.raw" -out "$test_root/releases/SHA256SUMS.sig"

cat > "$test_root/bin/uname" <<'SH'
#!/bin/sh
case "$1" in
    -s) echo "$FAKE_OS" ;;
    -m) echo "$FAKE_ARCH" ;;
    *) exit 1 ;;
esac
SH
cat > "$test_root/bin/curl" <<'SH'
#!/bin/bash
set -eu
if [ "${PRIVATE_REPO:-0}" = 1 ]; then
    echo 'Unauthenticated access to private releases is forbidden' >&2; exit 1
fi
output= url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output) output=$2; shift 2 ;;
        --retry|--write-out) shift 2 ;;
        --*) shift ;;
        *) url=$1; shift ;;
    esac
done
case "$url" in
    https://github.com/KiidxAtlas/dial/releases/latest)
        echo 'https://github.com/KiidxAtlas/dial/releases/tag/v0.1.0'; exit 0 ;;
    https://github.com/KiidxAtlas/dial/releases/download/v0.1.0/*)
        file=${url##*/}; cp "$FIXTURES/$file" "$output" ;;
    *) echo 'Unexpected download' >&2; exit 1 ;;
esac
if [ "${TAMPER:-0}" = 1 ] && [[ "$url" = *.tar.gz ]]; then
    printf tampered >> "$output"
fi
if [ "${BAD_MANIFEST:-0}" = 1 ] && [[ "$url" = */SHA256SUMS ]]; then
    printf '%064d  ../outside\n' 0 > "$output"
fi
SH
cat > "$test_root/bin/gh" <<'SH'
#!/bin/bash
set -eu
if [ "${FAKE_GH_AUTH:-0}" != 1 ] || [ "${FAKE_GH_DENIED:-0}" = 1 ]; then exit 1; fi
case "$1 $2" in
    'auth status') exit 0 ;;
    'release view') echo v0.1.0; exit 0 ;;
    'release download')
        shift 2
        [ "$1" = v0.1.0 ] || exit 1
        shift
        destination= repository=
        files=()
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --repo) repository=$2; shift 2 ;;
                --dir) destination=$2; shift 2 ;;
                --pattern) files+=("$2"); shift 2 ;;
                *) exit 1 ;;
            esac
        done
        [ "$repository" = KiidxAtlas/dial ] || exit 1
        for file in "${files[@]}"; do cp "$FIXTURES/$file" "$destination/$file"; done ;;
    *) exit 1 ;;
esac
SH
chmod +x "$test_root/bin/uname" "$test_root/bin/curl" "$test_root/bin/gh"
export PATH="$test_root/bin:$PATH" FIXTURES="$test_root/releases" TMPDIR="$test_root/temp"
export FAKE_OS=Darwin FAKE_ARCH=arm64 DIAL_INSTALL_DIR="$test_root/mac/bin"
bash "$root/scripts/install.sh" v0.1.0 > "$test_root/mac.log"
 [ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0' ]
export FAKE_OS=Darwin FAKE_ARCH=x86_64 DIAL_INSTALL_DIR="$test_root/intel/bin"
bash "$root/scripts/install.sh" latest > "$test_root/intel.log"
 [ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0' ]
export FAKE_OS=Linux FAKE_ARCH=x86_64 DIAL_INSTALL_DIR="$test_root/linux/bin"
bash "$root/scripts/install.sh" latest > "$test_root/linux.log"
 [ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0' ]
export FAKE_GH_AUTH=1 PRIVATE_REPO=1 DIAL_INSTALL_DIR="$test_root/private/bin"
bash "$root/scripts/install.sh" latest > "$test_root/private.log"
 [ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0' ]
cp "$DIAL_INSTALL_DIR/dial" "$test_root/private-original"
if FAKE_GH_DENIED=1 bash "$root/scripts/install.sh" v0.1.0 > "$test_root/denied.log" 2>&1; then
    echo 'Denied private release access was accepted' >&2; exit 1
fi
cmp "$DIAL_INSTALL_DIR/dial" "$test_root/private-original"
export FAKE_GH_AUTH=0 PRIVATE_REPO=0 DIAL_INSTALL_DIR="$test_root/linux/bin"
cp "$DIAL_INSTALL_DIR/dial" "$test_root/original"
if TAMPER=1 bash "$root/scripts/install.sh" v0.1.0 > "$test_root/tamper.log" 2>&1; then
    echo 'Tampered archive was accepted' >&2; exit 1
fi
cmp "$DIAL_INSTALL_DIR/dial" "$test_root/original"
export DIAL_INSTALL_DIR="$test_root/not-installed/bin"
if BAD_MANIFEST=1 bash "$root/scripts/install.sh" v0.1.0 > "$test_root/manifest.log" 2>&1; then
    echo 'Invalid manifest was accepted' >&2; exit 1
fi
if bash "$root/scripts/install.sh" 'v0.1.0/../../other' > "$test_root/version.log" 2>&1; then
    echo 'Unsafe version was accepted' >&2; exit 1
fi
export FAKE_OS=Windows_NT FAKE_ARCH=x86_64
if bash "$root/scripts/install.sh" v0.1.0 > "$test_root/platform.log" 2>&1; then
    echo 'Unsupported native Windows was accepted' >&2; exit 1
fi
[ ! -e "$DIAL_INSTALL_DIR/dial" ]
echo 'Installer tests passed: Apple Silicon/Intel macOS, Linux/WSL target selection, latest version, authenticated private downloads and access denial, tamper rejection, no overwrite on failure, unsafe manifest/version/platform rejection.'
