#!/bin/sh
# npm snapshot of dial-web/public/install.sh; CLI installer is bundled alongside it.
# Environment variables:
#   DIAL_CLI_ONLY=1                 Install only the dial CLI (skip Dial Desktop)
#   DIAL_INSTALL_DESKTOP=0|1        Disable or force Dial Desktop installation
#   DIAL_INSTALL_DIR=<dir>          Override CLI install directory (default ~/.local/bin)
#   DIAL_APP_DIR=<dir>              Override Desktop install directory (/Applications or ~/Applications on macOS; ~/.local/bin on Linux)
#   DIAL_NO_MODIFY_PATH=1           Do not modify shell profile files
#   DIAL_REQUIRE_DESKTOP_SIGNATURE=1  Fail if no minisign verifier is available for Dial Desktop
set -eu

DESKTOP_REPO="KiidxAtlas/dial-desktop"
# Tauri updater minisign public key from dial-desktop/src-tauri/tauri.release.conf.json
DEFAULT_DESKTOP_PUBKEY="dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDlBRERBOEM2MkQ5MThDQUYKUldTdmpKRXR4cWpkbW9PUnBZQ3lrM2xjajJrQTZjWlJEcGJpM0pPTklyRmhlR1BXSUpiQlZGemMK"

INSTALL_CLI=1
INSTALL_DESKTOP=1
DESKTOP_EXPLICIT=0
DESKTOP_INSTALLED=0
CLI_VERSION=""
DESKTOP_VERSION="${DIAL_DESKTOP_VERSION:-}"
usage() {
  cat <<'EOF'
Dial installer (Dial Desktop + dial CLI)

Usage:
  dial-install [options] [cli-version]

Options:
  --cli, --cli-only            Install only the dial CLI
  --desktop, --desktop-only    Install only Dial Desktop
  --desktop-version <tag>      Install a specific Dial Desktop release tag (e.g. v0.4.0)
  -h, --help                   Show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --cli|--cli-only)
      INSTALL_CLI=1
      INSTALL_DESKTOP=0
      DESKTOP_EXPLICIT=1
      shift
      ;;
    --desktop|--desktop-only)
      INSTALL_CLI=0
      INSTALL_DESKTOP=1
      DESKTOP_EXPLICIT=1
      shift
      ;;
    --desktop-version)
      shift
      if [ $# -eq 0 ] || [ -z "${1:-}" ]; then
        printf 'error: --desktop-version requires a value\n' >&2
        exit 1
      fi
      DESKTOP_VERSION="$1"
      shift
      ;;
    --desktop-version=*)
      DESKTOP_VERSION="${1#*=}"
      if [ -z "$DESKTOP_VERSION" ]; then
        printf 'error: --desktop-version requires a value\n' >&2
        exit 1
      fi
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      printf 'error: unknown option: %s\n' "$1" >&2
      usage >&2
      exit 1
      ;;
    *)
      if [ -n "$CLI_VERSION" ]; then
        printf 'error: unexpected argument: %s\n' "$1" >&2
        exit 1
      fi
      CLI_VERSION="$1"
      shift
      ;;
  esac
done

if [ "${DIAL_CLI_ONLY:-0}" = "1" ]; then
  INSTALL_DESKTOP=0
fi
if [ "${DIAL_INSTALL_DESKTOP:-}" = "0" ]; then
  INSTALL_DESKTOP=0
elif [ "${DIAL_INSTALL_DESKTOP:-}" = "1" ]; then
  INSTALL_DESKTOP=1
  DESKTOP_EXPLICIT=1
fi

# Backward compatibility: Dial Desktop's in-app engine installer invokes this URL
# with DIAL_INSTALL_DIR=<tempdir> and DIAL_NO_MODIFY_PATH=1. Keep that path CLI-only
# so a running Desktop app never tries to overwrite itself.
if [ "$DESKTOP_EXPLICIT" -eq 0 ] && [ "${DIAL_CLI_ONLY:-}" != "0" ] && [ -n "${DIAL_INSTALL_DIR:-}" ] && [ "${DIAL_NO_MODIFY_PATH:-}" = "1" ]; then
  INSTALL_DESKTOP=0
fi

say() {
  printf '%s\n' "$*"
}

warn() {
  printf 'warning: %s\n' "$*" >&2
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

detect_os() {
  case "$(uname -s)" in
    Darwin) printf 'macos' ;;
    Linux)  printf 'linux' ;;
    *)      die "unsupported operating system: $(uname -s)" ;;
  esac
}

# Normalized CPU architecture (aarch64|x86_64). On macOS, inspect hw.optional.arm64
# so Apple Silicon Macs running inside a Rosetta 2 shell still get native arm64 builds.
detect_arch() {
  os="$1"
  if [ "$os" = "macos" ]; then
    arm64_flag=$(sysctl -in hw.optional.arm64 2>/dev/null || /usr/sbin/sysctl -in hw.optional.arm64 2>/dev/null || true)
    if [ "$arm64_flag" = "1" ]; then
      printf 'aarch64'
      return
    fi
  fi
  case "$(uname -m)" in
    arm64|aarch64) printf 'aarch64' ;;
    x86_64|amd64)  printf 'x86_64' ;;
    *)             die "unsupported architecture: $(uname -m)" ;;
  esac
}

is_wsl() {
  [ -r /proc/version ] && grep -qi microsoft /proc/version 2>/dev/null
}

is_musl() {
  if command -v ldd >/dev/null 2>&1; then
    { ldd --version 2>&1 || true; } | grep -qi musl
    return $?
  fi
  ls /lib/ld-musl-*.so.1 >/dev/null 2>&1
}

is_headless_linux() {
  [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ] && [ -z "${XDG_CURRENT_DESKTOP:-}" ] && [ -z "${DESKTOP_SESSION:-}" ]
}

has_libfuse2() {
  if command -v ldconfig >/dev/null 2>&1 && ldconfig -p 2>/dev/null | grep -q 'libfuse\.so\.2'; then
    return 0
  fi
  for p in /lib/libfuse.so.2 /lib64/libfuse.so.2 /usr/lib/libfuse.so.2 /usr/lib64/libfuse.so.2 /lib/*/libfuse.so.2 /usr/lib/*/libfuse.so.2; do
    [ -e "$p" ] && return 0
  done
  return 1
}
download_file() {
  url="$1"
  out="$2"
  case "$url" in
    https://*)
      if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 -H "User-Agent: dial-installer" -o "$out" "$url"
      elif command -v wget >/dev/null 2>&1; then
        wget -qO "$out" --https-only --header="User-Agent: dial-installer" "$url"
      else
        die "curl or wget is required to download Dial"
      fi
      ;;
    http://127.0.0.1:*|http://localhost:*)
      # Permitted only for explicit local test overrides.
      if [ -z "${DIAL_DESKTOP_BASE_URL:-}" ]; then
        die "refusing insecure URL: $url"
      fi
      if command -v curl >/dev/null 2>&1; then
        curl -fsSL -H "User-Agent: dial-installer" -o "$out" "$url"
      elif command -v wget >/dev/null 2>&1; then
        wget -qO "$out" --header="User-Agent: dial-installer" "$url"
      else
        die "curl or wget is required to download Dial"
      fi
      ;;
    *)
      die "refusing non-HTTPS URL: $url"
      ;;
  esac
}

b64_decode() {
  if printf 'YQ==' | base64 -d >/dev/null 2>&1; then
    base64 -d
  else
    base64 -D
  fi
}

find_openssl3() {
  for cand in /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl "$(command -v openssl 2>/dev/null || true)"; do
    [ -n "$cand" ] && [ -x "$cand" ] || continue
    case "$("$cand" version 2>/dev/null || true)" in
      "OpenSSL 3."*)
        if "$cand" list -digest-algorithms 2>/dev/null | grep -qi blake2b512; then
          printf '%s' "$cand"
          return 0
        fi
        ;;
    esac
  done
  return 1
}

# Locate a real python3 without triggering Apple's uninstalled Xcode CLT GUI dialog.
find_real_python3() {
  py=$(command -v python3 2>/dev/null || true)
  [ -n "$py" ] || return 1
  if [ "$HOST_OS" = "macos" ] && [ "$py" = "/usr/bin/python3" ]; then
    xcode-select -p >/dev/null 2>&1 || return 1
  fi
  if "$py" -c 'import hashlib, sys; assert sys.version_info >= (3, 6)' >/dev/null 2>&1; then
    printf '%s' "$py"
    return 0
  fi
  return 1
}

# Verify a Tauri minisign signature (.sig containing base64-encoded minisign text)
# against the embedded minisign public key (also base64-encoded minisign text).
# Returns 0 on verified signature, 2 if no verifier exists, or exits 1 on bad signature.
verify_minisign() {
  payload="$1"
  sig_b64="$2"
  pubkey_b64="${DIAL_DESKTOP_PUBKEY:-$DEFAULT_DESKTOP_PUBKEY}"

  vdir="$TMP_DIR/verify"
  rm -rf "$vdir"
  mkdir -p "$vdir"

  printf '%s' "$pubkey_b64" | tr -d ' \t\r\n' | b64_decode > "$vdir/pubkey.txt" || die "failed to decode desktop signing public key"
  tr -d ' \t\r\n' < "$sig_b64" | b64_decode > "$vdir/sig.txt" || die "failed to decode desktop signature"

  if command -v minisign >/dev/null 2>&1; then
    if minisign -Vm "$payload" -p "$vdir/pubkey.txt" -x "$vdir/sig.txt" >/dev/null 2>&1; then
      return 0
    fi
    die "Dial Desktop signature verification failed (minisign)"
  fi

  if openssl_bin=$(find_openssl3); then
    sed -n '2p' "$vdir/pubkey.txt" | tr -d ' \t\r\n' | b64_decode > "$vdir/pubkey.bin" || die "malformed desktop public key"
    sed -n '2p' "$vdir/sig.txt" | tr -d ' \t\r\n' | b64_decode > "$vdir/sig.bin" || die "malformed desktop signature"
    sed -n '4p' "$vdir/sig.txt" | tr -d ' \t\r\n' | b64_decode > "$vdir/global_sig.bin" || die "malformed desktop global signature"

    [ "$(wc -c < "$vdir/pubkey.bin" | tr -d ' \t\r\n')" = "42" ] || die "invalid desktop public key length"
    [ "$(wc -c < "$vdir/sig.bin" | tr -d ' \t\r\n')" = "74" ] || die "invalid desktop signature length"
    [ "$(wc -c < "$vdir/global_sig.bin" | tr -d ' \t\r\n')" = "64" ] || die "invalid desktop global signature length"

    dd if="$vdir/pubkey.bin" bs=1 skip=2 count=8 2>/dev/null > "$vdir/pub_keyid"
    dd if="$vdir/pubkey.bin" bs=1 skip=10 count=32 2>/dev/null > "$vdir/pub_raw"
    dd if="$vdir/sig.bin" bs=1 skip=0 count=2 2>/dev/null > "$vdir/sig_alg"
    dd if="$vdir/sig.bin" bs=1 skip=2 count=8 2>/dev/null > "$vdir/sig_keyid"
    dd if="$vdir/sig.bin" bs=1 skip=10 count=64 2>/dev/null > "$vdir/sig_raw"

    cmp -s "$vdir/pub_keyid" "$vdir/sig_keyid" || die "Dial Desktop signature key ID mismatch"

    { printf '\060\052\060\005\006\003\053\145\160\003\041\000'; cat "$vdir/pub_raw"; } > "$vdir/pub.der"
    {
      printf '%s\n' '-----BEGIN PUBLIC KEY-----'
      base64 < "$vdir/pub.der" | tr -d ' \t\r\n'
      printf '\n%s\n' '-----END PUBLIC KEY-----'
    } > "$vdir/pub.pem"

    alg=$(od -An -tx1 "$vdir/sig_alg" | tr -d ' \t\r\n')
    if [ "$alg" = "4544" ]; then
      "$openssl_bin" dgst -blake2b512 -binary "$payload" > "$vdir/msg.bin"
    elif [ "$alg" = "4564" ]; then
      cp "$payload" "$vdir/msg.bin"
    else
      die "unsupported minisign signature algorithm: $alg"
    fi

    "$openssl_bin" pkeyutl -verify -pubin -inkey "$vdir/pub.pem" -rawin -in "$vdir/msg.bin" -sigfile "$vdir/sig_raw" >/dev/null 2>&1 \
      || die "Dial Desktop signature verification failed (OpenSSL)"

    trusted_line=$(sed -n '3p' "$vdir/sig.txt")
    case "$trusted_line" in
      "trusted comment: "*) trusted_body="${trusted_line#trusted comment: }" ;;
      *) die "malformed minisign trusted comment" ;;
    esac
    { cat "$vdir/sig_raw"; printf '%s' "$trusted_body"; } > "$vdir/global_msg.bin"
    "$openssl_bin" pkeyutl -verify -pubin -inkey "$vdir/pub.pem" -rawin -in "$vdir/global_msg.bin" -sigfile "$vdir/global_sig.bin" >/dev/null 2>&1 \
      || die "Dial Desktop trusted comment signature verification failed (OpenSSL)"
    return 0
  fi

  if py_bin=$(find_real_python3); then
    "$py_bin" - "$payload" "$vdir/pubkey.txt" "$vdir/sig.txt" <<'PY' || die "Dial Desktop signature verification failed (Python Ed25519)"
import base64, hashlib, sys

payload_path, pub_path, sig_path = sys.argv[1:4]
pub_lines = open(pub_path, "r", encoding="utf-8").read().splitlines()
sig_lines = open(sig_path, "r", encoding="utf-8").read().splitlines()
if len(pub_lines) < 2 or len(sig_lines) < 4:
    sys.exit(1)

pub_raw = base64.b64decode(pub_lines[1].strip())
sig_raw = base64.b64decode(sig_lines[1].strip())
global_sig = base64.b64decode(sig_lines[3].strip())
if len(pub_raw) != 42 or len(sig_raw) != 74 or len(global_sig) != 64:
    sys.exit(1)
if pub_raw[2:10] != sig_raw[2:10]:
    sys.exit(1)
if not sig_lines[2].startswith("trusted comment: "):
    sys.exit(1)
trusted_comment = sig_lines[2][len("trusted comment: "):].encode("utf-8")

pk = pub_raw[10:42]
alg = sig_raw[0:2]
sig = sig_raw[10:74]

# Pure stdlib RFC 8032 Ed25519 verification
p = (1 << 255) - 19
L = (1 << 252) + 27742317777372353535851937790883648493
d = (-121665 * pow(121666, p - 2, p)) % p
I = pow(2, (p - 1) // 4, p)

def xrecover(y):
    xx = (y * y - 1) * pow(d * y * y + 1, p - 2, p) % p
    x = pow(xx, (p + 3) // 8, p)
    if (x * x - xx) % p != 0:
        x = (x * I) % p
    if (x * x - xx) % p != 0:
        raise ValueError("not on curve")
    if x & 1:
        x = p - x
    return x

By = (4 * pow(5, p - 2, p)) % p
Bx = xrecover(By)
B = (Bx % p, By % p, 1, (Bx * By) % p)

def add(P, Q):
    X1, Y1, Z1, T1 = P
    X2, Y2, Z2, T2 = Q
    A = ((Y1 - X1) * (Y2 - X2)) % p
    B_ = ((Y1 + X1) * (Y2 + X2)) % p
    C = (2 * d * T1 * T2) % p
    D = (2 * Z1 * Z2) % p
    E = (B_ - A) % p
    F = (D - C) % p
    G = (D + C) % p
    H = (B_ + A) % p
    return ((E * F) % p, (G * H) % p, (F * G) % p, (E * H) % p)

def mul(P, e):
    Q = (0, 1, 1, 0)
    while e > 0:
        if e & 1:
            Q = add(Q, P)
        P = add(P, P)
        e >>= 1
    return Q

def decode_point(b):
    y = int.from_bytes(b, "little")
    sign = (y >> 255) & 1
    y &= (1 << 255) - 1
    if y >= p:
        raise ValueError("y >= p")
    x = xrecover(y)
    if x == 0 and sign:
        raise ValueError("invalid zero sign")
    if (x & 1) != sign:
        x = p - x
    return (x, y, 1, (x * y) % p)

def eq(P, Q):
    return (P[0] * Q[2] - Q[0] * P[2]) % p == 0 and (P[1] * Q[2] - Q[1] * P[2]) % p == 0

def verify_ed25519(public_key, message, signature):
    R = decode_point(signature[:32])
    A = decode_point(public_key)
    S = int.from_bytes(signature[32:], "little")
    if S >= L:
        return False
    h = int.from_bytes(hashlib.sha512(signature[:32] + public_key + message).digest(), "little") % L
    return eq(mul(B, S), add(R, mul(A, h)))

if alg == b"ED":
    h = hashlib.blake2b(digest_size=64)
    with open(payload_path, "rb") as f:
        while True:
            chunk = f.read(1024 * 1024)
            if not chunk:
                break
            h.update(chunk)
    msg = h.digest()
elif alg == b"Ed":
    msg = open(payload_path, "rb").read()
else:
    sys.exit(1)

if not verify_ed25519(pk, msg, sig):
    sys.exit(1)
if not verify_ed25519(pk, sig + trusted_comment, global_sig):
    sys.exit(1)
PY
    return 0
  fi

  return 2
}

install_cli() {
  cli_script="$(dirname "$0")/install-cli.sh"
  say "==> Installing dial CLI..."

  # On Apple Silicon under Rosetta 2, invoke the CLI installer under native arm64
  # so the downloaded dial binary matches the native hardware architecture.
  if [ "$HOST_OS" = "macos" ] && [ "$HOST_ARCH" = "aarch64" ] && [ "$(uname -m)" != "arm64" ] && command -v arch >/dev/null 2>&1; then
    if [ -n "$CLI_VERSION" ]; then
      arch -arm64 sh "$cli_script" "$CLI_VERSION"
    else
      arch -arm64 sh "$cli_script"
    fi
  else
    if [ -n "$CLI_VERSION" ]; then
      sh "$cli_script" "$CLI_VERSION"
    else
      sh "$cli_script"
    fi
  fi
}

desktop_base_url() {
  if [ -n "${DIAL_DESKTOP_BASE_URL:-}" ]; then
    printf '%s' "${DIAL_DESKTOP_BASE_URL%/}"
  elif [ -n "$DESKTOP_VERSION" ] && [ "$DESKTOP_VERSION" != "latest" ]; then
    printf 'https://github.com/%s/releases/download/%s' "$DESKTOP_REPO" "$DESKTOP_VERSION"
  else
    printf 'https://github.com/%s/releases/latest/download' "$DESKTOP_REPO"
  fi
}

install_desktop_macos() {
  base=$(desktop_base_url)
  if [ "$HOST_ARCH" = "aarch64" ]; then
    archive_name="Dial_aarch64.app.tar.gz"
  else
    archive_name="Dial_x64.app.tar.gz"
  fi

  say "==> Installing Dial Desktop for macOS (${HOST_ARCH})..."
  archive_path="$TMP_DIR/${archive_name}"
  sig_path="$TMP_DIR/${archive_name}.sig"
  download_file "${base}/${archive_name}" "$archive_path"
  download_file "${base}/${archive_name}.sig" "$sig_path"

  verify_rc=0
  verify_minisign "$archive_path" "$sig_path" || verify_rc=$?
  if [ "$verify_rc" -eq 0 ]; then
    say "    Verified Dial Desktop release signature."
  elif [ "$verify_rc" -eq 2 ]; then
    if [ "${DIAL_REQUIRE_DESKTOP_SIGNATURE:-0}" = "1" ]; then
      die "no minisign verifier (minisign, OpenSSL 3, or python3) found"
    fi
    warn "no minisign verifier (minisign, OpenSSL 3, or python3) found; skipping archive signature check"
  else
    exit "$verify_rc"
  fi

  extract_dir="$TMP_DIR/extract"
  mkdir -p "$extract_dir"
  tar -xzf "$archive_path" -C "$extract_dir"

  src_app="$extract_dir/Dial.app"
  [ -d "$src_app" ] && [ ! -L "$src_app" ] || die "Dial.app not found in downloaded archive"
  [ -f "$src_app/Contents/MacOS/dial-desktop" ] && [ ! -L "$src_app/Contents/MacOS/dial-desktop" ] \
    || die "Dial.app bundle is missing Contents/MacOS/dial-desktop"
  [ -x "$src_app/Contents/MacOS/dial-desktop" ] || chmod 755 "$src_app/Contents/MacOS/dial-desktop"

  if command -v codesign >/dev/null 2>&1; then
    codesign --verify --deep --strict "$src_app" >/dev/null 2>&1 || die "Dial.app failed macOS codesign integrity check"
  fi
  if command -v xattr >/dev/null 2>&1; then
    xattr -dr com.apple.quarantine "$src_app" 2>/dev/null || true
  fi

  if [ -n "${DIAL_APP_DIR:-}" ]; then
    app_dir="$DIAL_APP_DIR"
    mkdir -p "$app_dir"
  elif [ -d /Applications ] && [ -w /Applications ]; then
    app_dir="/Applications"
  else
    app_dir="$HOME/Applications"
    mkdir -p "$app_dir"
  fi

  dest_app="$app_dir/Dial.app"
  staged_app="$app_dir/.Dial.app.new.$$"
  backup_app="$app_dir/.Dial.app.bak.$$"
  rm -rf "$staged_app" "$backup_app"

  if command -v ditto >/dev/null 2>&1; then
    ditto "$src_app" "$staged_app"
  else
    cp -R "$src_app" "$staged_app"
  fi
  if command -v xattr >/dev/null 2>&1; then
    xattr -dr com.apple.quarantine "$staged_app" 2>/dev/null || true
  fi

  had_prev=0
  if [ -e "$dest_app" ] || [ -L "$dest_app" ]; then
    mv "$dest_app" "$backup_app"
    had_prev=1
  fi
  if mv "$staged_app" "$dest_app"; then
    [ "$had_prev" -eq 1 ] && rm -rf "$backup_app"
  else
    rm -rf "$staged_app"
    [ "$had_prev" -eq 1 ] && mv "$backup_app" "$dest_app" || true
    die "failed to move Dial.app into $app_dir"
  fi
  DESKTOP_INSTALLED=1
  say "    Installed Dial Desktop -> ${dest_app}"
  if command -v pgrep >/dev/null 2>&1 && pgrep -x dial-desktop >/dev/null 2>&1; then
    say "    Dial is currently running — quit and reopen Dial to use the updated version."
  fi
}

# Extract URL and signature for the first matching platform key in latest.json.
# Prints two lines: line 1 = URL, line 2 = signature (base64).
resolve_linux_desktop_from_manifest() {
  manifest="$1"
  if py_bin=$(find_real_python3); then
    "$py_bin" - "$manifest" "$HOST_ARCH" <<'PY'
import json, sys

path, arch = sys.argv[1:3]
data = json.load(open(path, "r", encoding="utf-8"))
platforms = data.get("platforms") or {}
keys = (
    ["linux-aarch64-appimage", "linux-arm64-appimage", "linux-aarch64"]
    if arch == "aarch64"
    else ["linux-x86_64-appimage", "linux-x86_64"]
)
for k in keys:
    entry = platforms.get(k)
    if isinstance(entry, dict) and entry.get("url") and entry.get("signature"):
        print(entry["url"])
        print(entry["signature"])
        sys.exit(0)
sys.exit(1)
PY
    return $?
  fi

  # POSIX awk fallback when python3 is absent on minimal Linux hosts (works on mawk, gawk, BSD awk).
  awk -v arch="$HOST_ARCH" '
    { content = content "\n" $0 }
    END {
      if (arch == "aarch64") {
        n = split("linux-aarch64-appimage linux-arm64-appimage linux-aarch64", keys, " ")
      } else {
        n = split("linux-x86_64-appimage linux-x86_64", keys, " ")
      }
      for (i = 1; i <= n; i++) {
        pat = "\"" keys[i] "\"[[:space:]]*:[[:space:]]*\\{[^}]*\\}"
        if (match(content, pat)) {
          block = substr(content, RSTART, RLENGTH)
          url = ""
          sig = ""
          if (match(block, "\"url\"[[:space:]]*:[[:space:]]*\"[^\"]+\"")) {
            u = substr(block, RSTART, RLENGTH)
            sub(/^"url"[[:space:]]*:[[:space:]]*"/, "", u)
            sub(/"$/, "", u)
            url = u
          }
          if (match(block, "\"signature\"[[:space:]]*:[[:space:]]*\"[^\"]+\"")) {
            s = substr(block, RSTART, RLENGTH)
            sub(/^"signature"[[:space:]]*:[[:space:]]*"/, "", s)
            sub(/"$/, "", s)
            sig = s
          }
          if (url != "" && sig != "") {
            print url
            print sig
            exit 0
          }
        }
      }
      exit 1
    }
  ' "$manifest"
}

install_desktop_linux() {
  if is_wsl && [ "${DIAL_INSTALL_DESKTOP:-}" != "1" ]; then
    say "==> WSL2 detected: skipping Linux desktop app (use the Windows PowerShell installer for Dial Desktop)."
    return 0
  fi
  if is_musl && [ "${DIAL_INSTALL_DESKTOP:-}" != "1" ]; then
    say "==> musl Linux detected: skipping glibc Dial Desktop AppImage (pass DIAL_INSTALL_DESKTOP=1 to force)."
    return 0
  fi
  if [ "$DESKTOP_EXPLICIT" -eq 0 ] && [ "${DIAL_INSTALL_DESKTOP:-}" != "1" ] && is_headless_linux; then
    say "==> No graphical session detected (DISPLAY/WAYLAND_DISPLAY unset): skipping Dial Desktop (pass --desktop to install anyway)."
    return 0
  fi
  base=$(desktop_base_url)
  say "==> Installing Dial Desktop for Linux (${HOST_ARCH})..."
  manifest="$TMP_DIR/latest.json"
  if ! download_file "${base}/latest.json" "$manifest"; then
    warn "could not fetch Dial Desktop manifest; skipping desktop app"
    return 0
  fi

  resolved=$(resolve_linux_desktop_from_manifest "$manifest" || true)
  if [ -z "$resolved" ]; then
    say "    No Linux desktop AppImage published for ${HOST_ARCH} in this release; skipped desktop app."
    return 0
  fi

  asset_url=$(printf '%s\n' "$resolved" | sed -n '1p')
  asset_sig=$(printf '%s\n' "$resolved" | sed -n '2p')
  appimage_tmp="$TMP_DIR/Dial.AppImage"
  sig_tmp="$TMP_DIR/Dial.AppImage.sig"
  printf '%s' "$asset_sig" > "$sig_tmp"
  download_file "$asset_url" "$appimage_tmp"

  verify_rc=0
  verify_minisign "$appimage_tmp" "$sig_tmp" || verify_rc=$?
  if [ "$verify_rc" -eq 0 ]; then
    say "    Verified Dial Desktop release signature."
  elif [ "$verify_rc" -eq 2 ]; then
    if [ "${DIAL_REQUIRE_DESKTOP_SIGNATURE:-0}" = "1" ]; then
      die "no minisign verifier (minisign, OpenSSL 3, or python3) found"
    fi
    warn "no minisign verifier (minisign, OpenSSL 3, or python3) found; skipping AppImage signature check"
  else
    exit "$verify_rc"
  fi

  app_dir="${DIAL_APP_DIR:-$HOME/.local/bin}"
  mkdir -p "$app_dir"
  dest_bin="$app_dir/dial-desktop"
  staged_bin="$app_dir/.dial-desktop.new.$$"
  cp "$appimage_tmp" "$staged_bin"
  chmod 755 "$staged_bin"
  mv -f "$staged_bin" "$dest_bin"

  exec_cmd="${dest_bin} %U"
  fuse_fallback=0
  if ! has_libfuse2; then
    exec_cmd="env APPIMAGE_EXTRACT_AND_RUN=1 ${dest_bin} %U"
    fuse_fallback=1
  fi

  icon_dir="${DIAL_DESKTOP_ICONS_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps}"
  if mkdir -p "$icon_dir" 2>/dev/null; then
    cat > "$icon_dir/dial.svg" <<'EOF'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">
  <rect x="3" y="3" width="29" height="29" fill="#16140f"/>
  <rect x="1" y="1" width="27" height="27" fill="#f5b14c" stroke="#16140f" stroke-width="2"/>
  <circle cx="14.5" cy="14.5" r="7.6" fill="#fbf8f1" stroke="#16140f" stroke-width="2.4"/>
  <line x1="14.5" y1="14.5" x2="18.6" y2="10.4" stroke="#16140f" stroke-width="2.6" stroke-linecap="square"/>
</svg>
EOF
  fi

  desktop_dir="${DIAL_DESKTOP_APPLICATIONS_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/applications}"
  if mkdir -p "$desktop_dir" 2>/dev/null; then
    cat > "$desktop_dir/dial.desktop" <<EOF
[Desktop Entry]
Name=Dial
Comment=Local-first AI coding agent
Exec=${exec_cmd}
Icon=dial
Terminal=false
Type=Application
Categories=Development;IDE;
StartupWMClass=dial-desktop
EOF
  fi

  DESKTOP_INSTALLED=1
  say "    Installed Dial Desktop -> ${dest_bin}"
  if [ "$fuse_fallback" -eq 1 ]; then
    say "    Note: libfuse.so.2 was not found; configured launcher with APPIMAGE_EXTRACT_AND_RUN=1."
    say "          Install libfuse2 (Ubuntu/Debian) or fuse-libs (Fedora) for faster AppImage startup."
  fi
}

main() {
  need_cmd uname
  need_cmd tar
  HOST_OS=$(detect_os)
  HOST_ARCH=$(detect_arch "$HOST_OS")

  TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/dial-install.XXXXXX")
  trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

  if [ "$INSTALL_CLI" -eq 1 ]; then
    install_cli
  fi

  if [ "$INSTALL_DESKTOP" -eq 1 ]; then
    if [ "$HOST_OS" = "macos" ]; then
      install_desktop_macos
    else
      install_desktop_linux
    fi
  fi

  if [ "$INSTALL_CLI" -eq 1 ] && [ "$DESKTOP_INSTALLED" -eq 1 ]; then
    say ""
    say "Dial Desktop + CLI installed."
    if [ "$HOST_OS" = "macos" ]; then
      say "  Desktop: open Dial from Applications"
    else
      say "  Desktop: dial-desktop (or open Dial from your applications menu)"
    fi
    say "  CLI:     dial (open a new terminal if dial is not yet on PATH)"
  fi
}

main "$@"
