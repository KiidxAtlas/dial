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
# The python verifier path is only exercised when python3 has the cryptography package.
python3 -c 'import cryptography.hazmat.primitives.asymmetric.ed25519' >/dev/null 2>&1 \
    || { echo 'Installer tests need python3 with the cryptography package (pip install cryptography)' >&2; exit 1; }
"$openssl" genpkey -algorithm ED25519 -out "$test_root/release-private.pem" >/dev/null 2>&1
"$openssl" pkey -in "$test_root/release-private.pem" -pubout -out "$test_root/release-public.pem" >/dev/null 2>&1
export DIAL_RELEASE_PUBLIC_KEY="$(cat "$test_root/release-public.pem")"
targets='aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-musl'
for target in $targets; do
    package="dial-v0.1.0-$target"
    mkdir -p "$test_root/releases/$package"
    printf '#!/bin/sh\necho "dial 0.1.0 %s"\n' "$target" > "$test_root/releases/$package/dial"
    chmod +x "$test_root/releases/$package/dial"
    tar -czf "$test_root/releases/$package.tar.gz" -C "$test_root/releases" "$package"
done
(cd "$test_root/releases" && shasum -a 256 dial-v0.1.0-*.tar.gz > SHA256SUMS)
"$openssl" pkeyutl -sign -inkey "$test_root/release-private.pem" -rawin \
    -in "$test_root/releases/SHA256SUMS" -out "$test_root/releases/SHA256SUMS.sig.raw"
"$openssl" base64 -A -in "$test_root/releases/SHA256SUMS.sig.raw" -out "$test_root/releases/SHA256SUMS.sig"
python3 "$root/scripts/make-sshsig.py" --openssl "$openssl" --key "$test_root/release-private.pem" \
    --input "$test_root/releases/SHA256SUMS" --output "$test_root/releases/SHA256SUMS.sshsig"

cat > "$test_root/bin/uname" <<'SH'
#!/bin/sh
case "$1" in
    -s) echo "$FAKE_OS" ;;
    -m) echo "$FAKE_ARCH" ;;
    *) exit 1 ;;
esac
SH
# Stands in for ldd on Linux hosts: reports musl or glibc as the C library.
cat > "$test_root/bin/ldd" <<'SH'
#!/bin/sh
if [ "${FAKE_LIBC:-glibc}" = musl ]; then echo 'musl libc (x86_64)' >&2; exit 1; fi
echo 'ldd (GNU libc) 2.35'
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
if [ "${BAD_SIGNATURE:-0}" = 1 ] && [[ "$url" = */SHA256SUMS.sig ]]; then
    printf 'AAAA' >> "$output"
fi
if [ "${BAD_SSHSIG:-0}" = 1 ] && [[ "$url" = */SHA256SUMS.sshsig ]]; then
    printf -- '-----BEGIN SSH SIGNATURE-----\n' > "$output"
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
cat > "$test_root/bin/libressl" <<'SH'
#!/bin/sh
# Stands in for macOS LibreSSL: no Ed25519 `pkeyutl -rawin`.
echo 'usage: pkeyutl [-verify] [-sign] [-inkey file]' >&2
exit 1
SH
chmod +x "$test_root/bin/uname" "$test_root/bin/ldd" "$test_root/bin/curl" "$test_root/bin/gh" "$test_root/bin/libressl"
# The installer edits shell profiles; keep HOME inside the fixture.
export PATH="$test_root/bin:$PATH" FIXTURES="$test_root/releases" TMPDIR="$test_root/temp" \
    HOME="$test_root/home" SHELL=/bin/zsh
mkdir -p "$HOME"
export FAKE_OS=Darwin FAKE_ARCH=arm64 DIAL_INSTALL_DIR="$test_root/mac/bin"
bash "$root/scripts/install.sh" v0.1.0 > "$test_root/mac.log" 2>&1
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0 aarch64-apple-darwin' ]
mac_path_line="export PATH=\"$DIAL_INSTALL_DIR:\$PATH\""
[ "$(grep -Fxc "$mac_path_line" "$HOME/.zshrc")" = 1 ] || { echo 'PATH was not added to ~/.zshrc' >&2; exit 1; }
bash "$root/scripts/install.sh" v0.1.0 > "$test_root/mac-again.log" 2>&1
[ "$(grep -Fxc "$mac_path_line" "$HOME/.zshrc")" = 1 ] || { echo 'Reinstall duplicated the PATH line' >&2; exit 1; }
export FAKE_OS=Darwin FAKE_ARCH=x86_64 DIAL_INSTALL_DIR="$test_root/intel/bin"
bash "$root/scripts/install.sh" latest > "$test_root/intel.log" 2>&1
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0 x86_64-apple-darwin' ]
export FAKE_OS=Linux FAKE_ARCH=x86_64 FAKE_LIBC=glibc DIAL_INSTALL_DIR="$test_root/linux/bin"
bash "$root/scripts/install.sh" latest > "$test_root/linux.log" 2>&1
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0 x86_64-unknown-linux-gnu' ]
export FAKE_OS=Linux FAKE_ARCH=aarch64 FAKE_LIBC=glibc DIAL_INSTALL_DIR="$test_root/linux-arm/bin"
bash "$root/scripts/install.sh" v0.1.0 > "$test_root/linux-arm.log" 2>&1
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0 aarch64-unknown-linux-gnu' ]
export FAKE_OS=Linux FAKE_ARCH=x86_64 FAKE_LIBC=musl DIAL_INSTALL_DIR="$test_root/musl/bin"
bash "$root/scripts/install.sh" v0.1.0 > "$test_root/musl.log" 2>&1
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0 x86_64-unknown-linux-musl' ]
export FAKE_OS=Linux FAKE_ARCH=aarch64 FAKE_LIBC=musl DIAL_INSTALL_DIR="$test_root/musl-arm/bin"
bash "$root/scripts/install.sh" v0.1.0 > "$test_root/musl-arm.log" 2>&1
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0 aarch64-unknown-linux-musl' ]
[ ! -e "$HOME/.bashrc" ] && [ ! -e "$HOME/.profile" ] || { echo 'A zsh install edited another profile' >&2; exit 1; }
SHELL=/bin/bash DIAL_NO_MODIFY_PATH=1 DIAL_INSTALL_DIR="$test_root/unlisted/bin" \
    bash "$root/scripts/install.sh" v0.1.0 > "$test_root/no-path.log" 2>&1
[ ! -e "$HOME/.bashrc" ] || { echo 'DIAL_NO_MODIFY_PATH=1 edited ~/.bashrc' >&2; exit 1; }
grep -Fq "Add $test_root/unlisted/bin to your PATH" "$test_root/no-path.log"

# Signature verifier chain. OpenSSL 3 is the default; the others are forced by hiding OpenSSL.
export FAKE_OS=Darwin FAKE_ARCH=arm64
DIAL_INSTALL_DIR="$test_root/openssl/bin" bash "$root/scripts/install.sh" v0.1.0 > "$test_root/openssl.log" 2>&1
grep -q 'Verifying release signature with openssl' "$test_root/openssl.log"
DIAL_OPENSSL="$test_root/bin/libressl" DIAL_PYTHON=python3 DIAL_INSTALL_DIR="$test_root/python/bin" \
    bash "$root/scripts/install.sh" v0.1.0 > "$test_root/python.log" 2>&1
grep -q 'Verifying release signature with python' "$test_root/python.log"
[ "$("$test_root/python/bin/dial" --version)" = 'dial 0.1.0 aarch64-apple-darwin' ]
DIAL_OPENSSL="$test_root/bin/libressl" DIAL_PYTHON="$test_root/no-python" DIAL_INSTALL_DIR="$test_root/sshkeygen/bin" \
    bash "$root/scripts/install.sh" v0.1.0 > "$test_root/sshkeygen.log" 2>&1
grep -q 'Verifying release signature with ssh-keygen' "$test_root/sshkeygen.log"
[ "$("$test_root/sshkeygen/bin/dial" --version)" = 'dial 0.1.0 aarch64-apple-darwin' ]

# Fail closed: with no verifier at all, nothing is installed and no checksum-only fallback happens.
if DIAL_OPENSSL="$test_root/bin/libressl" DIAL_PYTHON="$test_root/no-python" DIAL_SSH_KEYGEN="$test_root/no-ssh-keygen" \
    DIAL_INSTALL_DIR="$test_root/failclosed/bin" bash "$root/scripts/install.sh" v0.1.0 > "$test_root/failclosed.log" 2>&1; then
    echo 'Installed without any signature verifier' >&2; exit 1
fi
grep -q 'No release signature verifier found' "$test_root/failclosed.log"
[ ! -e "$test_root/failclosed/bin/dial" ]
# Opt-out: explicit and loud, still checksum-verified.
DIAL_OPENSSL="$test_root/bin/libressl" DIAL_PYTHON="$test_root/no-python" DIAL_SSH_KEYGEN="$test_root/no-ssh-keygen" \
    DIAL_ALLOW_UNSIGNED_INSTALL=1 DIAL_INSTALL_DIR="$test_root/unsigned/bin" \
    bash "$root/scripts/install.sh" v0.1.0 > "$test_root/unsigned.log" 2>&1
grep -q 'release signature is NOT' "$test_root/unsigned.log"
[ "$("$test_root/unsigned/bin/dial" --version)" = 'dial 0.1.0 aarch64-apple-darwin' ]
if DIAL_OPENSSL="$test_root/bin/libressl" DIAL_PYTHON="$test_root/no-python" DIAL_SSH_KEYGEN="$test_root/no-ssh-keygen" \
    DIAL_ALLOW_UNSIGNED_INSTALL=1 TAMPER=1 DIAL_INSTALL_DIR="$test_root/unsigned-tamper/bin" \
    bash "$root/scripts/install.sh" v0.1.0 > "$test_root/unsigned-tamper.log" 2>&1; then
    echo 'Tampered archive was accepted by the opt-out path' >&2; exit 1
fi
[ ! -e "$test_root/unsigned-tamper/bin/dial" ]

# Bad signatures fail closed on each verifier, and nothing is installed.
if BAD_SIGNATURE=1 bash "$root/scripts/install.sh" v0.1.0 > "$test_root/bad-openssl.log" 2>&1; then
    echo 'Invalid OpenSSL signature was accepted' >&2; exit 1
fi
grep -q 'Release signature verification failed' "$test_root/bad-openssl.log"
if BAD_SIGNATURE=1 DIAL_OPENSSL="$test_root/bin/libressl" DIAL_PYTHON=python3 DIAL_INSTALL_DIR="$test_root/bad-python/bin" \
    bash "$root/scripts/install.sh" v0.1.0 > "$test_root/bad-python.log" 2>&1; then
    echo 'Invalid Python signature was accepted' >&2; exit 1
fi
grep -q 'Release signature verification failed' "$test_root/bad-python.log"
[ ! -e "$test_root/bad-python/bin/dial" ]
if BAD_SSHSIG=1 DIAL_OPENSSL="$test_root/bin/libressl" DIAL_PYTHON="$test_root/no-python" \
    DIAL_INSTALL_DIR="$test_root/bad-sshkeygen/bin" bash "$root/scripts/install.sh" v0.1.0 > "$test_root/bad-sshkeygen.log" 2>&1; then
    echo 'Invalid SSH signature was accepted' >&2; exit 1
fi
grep -q 'Release signature verification failed' "$test_root/bad-sshkeygen.log"
[ ! -e "$test_root/bad-sshkeygen/bin/dial" ]
# A checksum-only release (no SHA256SUMS.sshsig) still installs through OpenSSL or Python.
rm -f "$FIXTURES/SHA256SUMS.sshsig"
DIAL_INSTALL_DIR="$test_root/no-sshsig/bin" bash "$root/scripts/install.sh" v0.1.0 > "$test_root/no-sshsig.log" 2>&1
[ "$("$test_root/no-sshsig/bin/dial" --version)" = 'dial 0.1.0 aarch64-apple-darwin' ]
python3 "$root/scripts/make-sshsig.py" --openssl "$openssl" --key "$test_root/release-private.pem" \
    --input "$test_root/releases/SHA256SUMS" --output "$test_root/releases/SHA256SUMS.sshsig"

# Tampered archive is rejected, and the existing binary is left untouched.
export DIAL_INSTALL_DIR="$test_root/nosig/bin"
DIAL_INSTALL_DIR="$test_root/nosig/bin" bash "$root/scripts/install.sh" v0.1.0 > "$test_root/nosig.log" 2>&1
cp "$DIAL_INSTALL_DIR/dial" "$test_root/nosig-original"
if TAMPER=1 bash "$root/scripts/install.sh" v0.1.0 > "$test_root/nosig-tamper.log" 2>&1; then
    echo 'Tampered archive was accepted' >&2; exit 1
fi
cmp "$DIAL_INSTALL_DIR/dial" "$test_root/nosig-original"

export FAKE_OS=Linux FAKE_ARCH=x86_64 FAKE_LIBC=glibc
export FAKE_GH_AUTH=1 PRIVATE_REPO=1 DIAL_INSTALL_DIR="$test_root/private/bin"
bash "$root/scripts/install.sh" latest > "$test_root/private.log" 2>&1
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0 x86_64-unknown-linux-gnu' ]
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
echo 'Installer tests passed: macOS and Linux (glibc/musl, x86-64/arm64) target selection, latest version, authenticated private downloads and access denial, OpenSSL/Python/ssh-keygen signature verifiers, fail-closed refusal and explicit opt-out, bad-signature and tamper rejection, checksum-only releases, shell profile PATH setup (idempotent, opt-out), no overwrite on failure, unsafe manifest/version/platform rejection.'
