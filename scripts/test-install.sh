#!/bin/bash
# Offline behavioral tests: all downloads and installation paths stay in a disposable fixture.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$root/.tmp"
test_root=$(mktemp -d "$root/.tmp/install-test.XXXXXX")
trap 'rm -rf "$test_root"' EXIT
mkdir -p "$test_root/bin" "$test_root/releases" "$test_root/temp"

for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
    package="dial-v0.1.0-$target"
    mkdir -p "$test_root/releases/$package"
    printf '#!/bin/sh\necho "dial fixture"\n' > "$test_root/releases/$package/dial"
    chmod +x "$test_root/releases/$package/dial"
    tar -czf "$test_root/releases/$package.tar.gz" -C "$test_root/releases" "$package"
    (cd "$test_root/releases" && shasum -a 256 "$package.tar.gz" > "$package.tar.gz.sha256")
done

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
if [ "${BAD_MANIFEST:-0}" = 1 ] && [[ "$url" = *.sha256 ]]; then
    printf '%064d  ../outside\n' 0 > "$output"
fi
SH
chmod +x "$test_root/bin/uname" "$test_root/bin/curl"
export PATH="$test_root/bin:$PATH" FIXTURES="$test_root/releases" TMPDIR="$test_root/temp"
export FAKE_OS=Darwin FAKE_ARCH=arm64 DIAL_INSTALL_DIR="$test_root/mac/bin"
bash "$root/scripts/install.sh" v0.1.0 > "$test_root/mac.log"
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial fixture' ]
export FAKE_OS=Darwin FAKE_ARCH=x86_64 DIAL_INSTALL_DIR="$test_root/intel/bin"
bash "$root/scripts/install.sh" latest > "$test_root/intel.log"
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial fixture' ]
export FAKE_OS=Linux FAKE_ARCH=x86_64 DIAL_INSTALL_DIR="$test_root/linux/bin"
bash "$root/scripts/install.sh" latest > "$test_root/linux.log"
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial fixture' ]
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
echo 'Installer tests passed: Apple Silicon/Intel macOS, Linux/WSL target selection, latest version, tamper rejection, no overwrite on failure, unsafe manifest/version/platform rejection.'
