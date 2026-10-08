#!/bin/bash
# Offline behavioral tests: all downloads and installation paths stay in a disposable fixture.
set -euo pipefail
installer_dir=$(cd "$(dirname "$0")" && pwd)
test_root=$(mktemp -d "${TMPDIR:-/tmp}/dial-install-test.XXXXXX")
trap 'rm -rf "$test_root"' EXIT
mkdir -p "$test_root/bin" "$test_root/releases" "$test_root/temp"

for target in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
    package="dial-v0.1.0-$target"
    mkdir -p "$test_root/releases/$package"
    printf '#!/bin/sh\necho "dial 0.1.0"\n' > "$test_root/releases/$package/dial"
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
if [ "${PRIVATE_REPO:-0}" = 1 ] && [[ "$url" = https://github.com/KiidxAtlas/* ]]; then
    echo 'Unauthenticated access to private releases is forbidden' >&2; exit 1
fi
case "$url" in
    https://github.com/cli/cli/releases/latest)
        echo 'https://github.com/cli/cli/releases/tag/v2.102.0'; exit 0 ;;
    https://github.com/cli/cli/releases/download/v2.102.0/*)
        file=${url##*/}; cp "$FIXTURES/$file" "$output" ;;
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
cat > "$test_root/bin/gh" <<'SH'
#!/bin/bash
set -eu
if [ "${FAKE_GH_AUTH:-0}" != 1 ] || [ "${FAKE_GH_DENIED:-0}" = 1 ]; then exit 1; fi
case "${1:-} ${2:-}" in
    'auth status') exit 0 ;;
    '--version ') echo 'gh version fixture'; exit 0 ;;
    'api repos/KiidxAtlas/dial') exit 0 ;;
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
export DIAL_INSTALL_BOOTSTRAP=0
export PATH="$test_root/bin:$PATH" FIXTURES="$test_root/releases" TMPDIR="$test_root/temp"
export FAKE_OS=Darwin FAKE_ARCH=arm64 DIAL_INSTALL_DIR="$test_root/mac/bin"
bash "$installer_dir/install.sh" v0.1.0 > "$test_root/mac.log"
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0' ]
export FAKE_OS=Darwin FAKE_ARCH=x86_64 DIAL_INSTALL_DIR="$test_root/intel/bin"
bash "$installer_dir/install.sh" latest > "$test_root/intel.log"
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0' ]
export FAKE_OS=Linux FAKE_ARCH=x86_64 DIAL_INSTALL_DIR="$test_root/linux/bin"
bash "$installer_dir/install.sh" latest > "$test_root/linux.log"
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0' ]
export FAKE_GH_AUTH=1 PRIVATE_REPO=1 DIAL_INSTALL_DIR="$test_root/private/bin"
bash "$installer_dir/install.sh" latest > "$test_root/private.log"
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0' ]
cp "$DIAL_INSTALL_DIR/dial" "$test_root/private-original"
if FAKE_GH_DENIED=1 bash "$installer_dir/install.sh" v0.1.0 > "$test_root/denied.log" 2>&1; then
    echo 'Denied private release access was accepted' >&2; exit 1
fi
cmp "$DIAL_INSTALL_DIR/dial" "$test_root/private-original"
export FAKE_GH_AUTH=0 PRIVATE_REPO=0 DIAL_INSTALL_DIR="$test_root/linux/bin"
cp "$DIAL_INSTALL_DIR/dial" "$test_root/original"
if TAMPER=1 bash "$installer_dir/install.sh" v0.1.0 > "$test_root/tamper.log" 2>&1; then
    echo 'Tampered archive was accepted' >&2; exit 1
fi
cmp "$DIAL_INSTALL_DIR/dial" "$test_root/original"
export DIAL_INSTALL_DIR="$test_root/not-installed/bin"
if BAD_MANIFEST=1 bash "$installer_dir/install.sh" v0.1.0 > "$test_root/manifest.log" 2>&1; then
    echo 'Invalid manifest was accepted' >&2; exit 1
fi
if bash "$installer_dir/install.sh" 'v0.1.0/../../other' > "$test_root/version.log" 2>&1; then
    echo 'Unsafe version was accepted' >&2; exit 1
fi
export FAKE_OS=Windows_NT FAKE_ARCH=x86_64
if bash "$installer_dir/install.sh" v0.1.0 > "$test_root/platform.log" 2>&1; then
    echo 'Unsupported native Windows was accepted' >&2; exit 1
fi
[ ! -e "$DIAL_INSTALL_DIR/dial" ]

# Bootstrap a missing GitHub CLI from verified public archives, then use private downloads.
export FAKE_GH_AUTH=1 PRIVATE_REPO=1 FAKE_OS=Darwin FAKE_ARCH=arm64
for gh_platform in macOS_arm64 macOS_amd64 linux_amd64; do
    gh_package="gh_2.102.0_$gh_platform"
    mkdir -p "$test_root/releases/$gh_package/bin"
    cp "$test_root/bin/gh" "$test_root/releases/$gh_package/bin/gh"
    if [[ "$gh_platform" = macOS_* ]]; then
        (cd "$test_root/releases" && zip -q -r "$gh_package.zip" "$gh_package")
        gh_archive="$gh_package.zip"
    else
        tar -czf "$test_root/releases/$gh_package.tar.gz" -C "$test_root/releases" "$gh_package"
        gh_archive="$gh_package.tar.gz"
    fi
    (cd "$test_root/releases" && shasum -a 256 "$gh_archive" >> gh_2.102.0_checksums.txt)
done
mkdir -p "$test_root/bootstrap-bin"
for tool in bash cp tar gzip install mktemp shasum awk unzip sed grep mkdir rm dirname; do
    ln -s "$(command -v "$tool")" "$test_root/bootstrap-bin/$tool"
done
ln -s "$test_root/bin/curl" "$test_root/bootstrap-bin/curl"
ln -s "$test_root/bin/uname" "$test_root/bootstrap-bin/uname"
# The missing-CLI path covers the Intel ZIP and Linux tar.gz formats too.
for pair in Darwin:x86_64 Linux:x86_64; do
    fixture_os=${pair%:*}; fixture_arch=${pair#*:}
    fixture_home="$test_root/gh-$fixture_os"
    mkdir -p "$fixture_home"
    env HOME="$fixture_home" SHELL=/bin/bash PATH="$test_root/bootstrap-bin" DIAL_INSTALL_BOOTSTRAP=1 \
        DIAL_INSTALL_DIR="$fixture_home/bin" FAKE_OS="$fixture_os" FAKE_ARCH="$fixture_arch" \
        bash "$installer_dir/install.sh" latest > "$test_root/gh-$fixture_os.log"
    [ "$("$fixture_home/bin/dial" --version)" = 'dial 0.1.0' ]
done
bootstrap_home="$test_root/home space 'quote'"
mkdir -p "$bootstrap_home"
export DIAL_INSTALL_DIR="$test_root/bootstrap/bin"
env HOME="$bootstrap_home" SHELL=/bin/zsh PATH="$test_root/bootstrap-bin" DIAL_INSTALL_BOOTSTRAP=1 \
    bash "$installer_dir/install.sh" latest > "$test_root/bootstrap.log"
[ -x "$bootstrap_home/.local/share/dial/tools/bin/gh" ]
[ "$("$DIAL_INSTALL_DIR/dial" --version)" = 'dial 0.1.0' ]
bash -n "$bootstrap_home/.zshrc"
case "$(env PATH=/initial /bin/bash -c 'source "$1"; printf "%s" "$PATH"' bash "$bootstrap_home/.zshrc")" in
    "$DIAL_INSTALL_DIR":/initial) ;;
    *) echo 'Shell PATH configuration did not preserve the existing PATH' >&2; exit 1 ;;
esac
env HOME="$bootstrap_home" SHELL=/bin/zsh PATH="$test_root/bootstrap-bin" DIAL_INSTALL_BOOTSTRAP=1 \
    bash "$installer_dir/install.sh" latest > "$test_root/bootstrap-update.log"
[ "$(grep -Fc '# Dial installer PATH' "$bootstrap_home/.zshrc")" = 1 ]

# A different install directory is quoted correctly, including spaces/apostrophes.
export DIAL_INSTALL_DIR="$test_root/custom space 'quote'/bin"
env HOME="$bootstrap_home" SHELL=/bin/zsh DIAL_INSTALL_BOOTSTRAP=1 \
    bash "$installer_dir/install.sh" latest > "$test_root/path.log"
bash -n "$bootstrap_home/.zshrc"
[ "$(env PATH=/initial /bin/bash -c 'source "$1"; printf "%s" "$PATH"' bash "$bootstrap_home/.zshrc")" = "$DIAL_INSTALL_DIR:$test_root/bootstrap/bin:/initial" ]
cp "$DIAL_INSTALL_DIR/dial" "$test_root/bootstrap-original"
# A hash-valid archive with the wrong executable version must not replace Dial.
mkdir -p "$test_root/bad-version/dial-v0.1.0-aarch64-apple-darwin"
printf '#!/bin/sh\necho "dial 9.9.9"\n' > "$test_root/bad-version/dial-v0.1.0-aarch64-apple-darwin/dial"
chmod +x "$test_root/bad-version/dial-v0.1.0-aarch64-apple-darwin/dial"
tar -czf "$test_root/bad-version/dial-v0.1.0-aarch64-apple-darwin.tar.gz" -C "$test_root/bad-version" dial-v0.1.0-aarch64-apple-darwin
(cd "$test_root/bad-version" && shasum -a 256 dial-v0.1.0-aarch64-apple-darwin.tar.gz > dial-v0.1.0-aarch64-apple-darwin.tar.gz.sha256)
if FIXTURES="$test_root/bad-version" bash "$installer_dir/install.sh" v0.1.0 > "$test_root/bad-version.log" 2>&1; then
    echo 'Wrong binary version was accepted' >&2; exit 1
fi
cmp "$DIAL_INSTALL_DIR/dial" "$test_root/bootstrap-original"
echo 'Installer tests passed: platform selection, private access, verified GitHub CLI bootstrap, PATH setup/idempotence/quoting, checksums, version checks and update preservation.'
