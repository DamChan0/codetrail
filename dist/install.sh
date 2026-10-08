#!/usr/bin/env bash
# CodeTrail installer. User-level by default (~/.local). Never touches ~/.claude, ~/.agents or any repo.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PREFIX="${HOME:-/tmp}/.local"
SYSTEM=0 FORCE=0
usage() {
  cat <<USAGE
usage: install.sh [--prefix DIR] [--system] [--force] [-h]
  (default)       install under \$HOME/.local (no root needed)
  --prefix DIR    install under DIR (bin/, share/)
  --system        install under /usr/local; needs root (run with sudo)
  --force         continue although the glibc check fails
USAGE
}
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) [ $# -ge 2 ] || { echo "--prefix needs a directory" >&2; exit 2; }; PREFIX=$2; shift 2 ;;
    --prefix=*) PREFIX=${1#--prefix=}; shift ;;
    --system) SYSTEM=1; PREFIX=/usr/local; shift ;;
    --force) FORCE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done
if [ "$SYSTEM" = 1 ] && [ "$(id -u)" != 0 ]; then
  echo "error: --system installs into $PREFIX and needs root. Re-run: sudo $0 --system" >&2
  echo "       (or drop --system for a per-user install in \$HOME/.local)" >&2
  exit 1
fi
case "$PREFIX" in /*) ;; *) PREFIX="$(pwd)/$PREFIX" ;; esac
PREFIX=${PREFIX%/}

BIN_SRC="$HERE/bin/codetrail"
[ -x "$BIN_SRC" ] || { echo "error: $BIN_SRC missing - run install.sh from the unpacked release directory" >&2; exit 1; }
# shellcheck disable=SC1091
[ -f "$HERE/BUILDINFO" ] && . "$HERE/BUILDINFO"
REQ_GLIBC=${REQ_GLIBC:-2.35}

# ---- compatibility checks -------------------------------------------------------------------
ver_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -n1)" = "$2" ]; }
HAVE_GLIBC=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')
if [ -z "$HAVE_GLIBC" ]; then
  echo "warning: cannot determine the glibc version (musl system?). This build needs glibc >= $REQ_GLIBC." >&2
elif ! ver_ge "$HAVE_GLIBC" "$REQ_GLIBC"; then
  echo "error: this CodeTrail build needs glibc >= $REQ_GLIBC but the system has $HAVE_GLIBC." >&2
  echo "       Build from source on this machine (cargo build --release -p ct-app) or use --force at your own risk." >&2
  [ "$FORCE" = 1 ] || exit 1
fi
if command -v ldd >/dev/null 2>&1; then
  if missing=$(ldd "$BIN_SRC" 2>&1 | grep 'not found'); then
    echo "error: required shared libraries are missing:" >&2
    echo "$missing" | sed 's/^/  /' >&2
    exit 1
  fi
fi
# libraries the GUI loads at run time (dlopen): soname -> Debian/Ubuntu package
declare -A PKG=(
  [libEGL.so.1]=libegl1 [libGL.so.1]=libgl1 [libX11.so.6]=libx11-6 [libX11-xcb.so.1]=libx11-xcb1
  [libxcb.so.1]=libxcb1 [libXcursor.so.1]=libxcursor1 [libXi.so.6]=libxi6 [libXrender.so.1]=libxrender1
  [libxkbcommon.so.0]=libxkbcommon0 [libxkbcommon-x11.so.0]=libxkbcommon-x11-0
  [libwayland-client.so.0]=libwayland-client0 [libwayland-egl.so.1]=libwayland-egl1
)
LDCONF=$(command -v ldconfig || true); [ -n "$LDCONF" ] || [ ! -x /sbin/ldconfig ] || LDCONF=/sbin/ldconfig
if [ -n "$LDCONF" ]; then
  CACHE=$("$LDCONF" -p 2>/dev/null | awk 'NF {print $1}' || true)
  miss_pkgs=()
  for so in "${!PKG[@]}"; do
    grep -qxF "$so" <<<"$CACHE" || miss_pkgs+=("${PKG[$so]} ($so)")
  done
  if [ ${#miss_pkgs[@]} -gt 0 ]; then
    echo "warning: GUI libraries not found (needed to open a window):" >&2
    printf '  %s\n' "${miss_pkgs[@]}" | sort >&2
    pk=$(printf '%s\n' "${miss_pkgs[@]}" | awk '{print $1}' | sort -u | tr '\n' ' ')
    echo "  Debian/Ubuntu: sudo apt install $pk" >&2
    echo "  (either the X11 set or the Wayland set is enough at run time; EGL/GL, xkbcommon are always needed)" >&2
  fi
else
  echo "note: ldconfig not available - skipped the GUI library check" >&2
fi
command -v git >/dev/null 2>&1 || echo "warning: git not found in PATH - CodeTrail needs git at run time" >&2

# ---- install (everything recorded in the manifest) --------------------------------------------
SHARE="$PREFIX/share/codetrail"
MANIFEST="$SHARE/install-manifest.txt"
if [ -f "$MANIFEST" ]; then
  echo "note: an earlier install in $PREFIX exists; replacing it"
  bash "$SHARE/uninstall.sh" --prefix "$PREFIX" >/dev/null 2>&1 || true
fi
declare -a FILES=() DIRS=()
mk() { # mkdir -p, remembering every directory we create (outermost first)
  local d=$1 up=() p
  p=$d
  while [ ! -d "$p" ] && [ "$p" != / ]; do up=("$p" "${up[@]}"); p=$(dirname "$p"); done
  mkdir -p "$d"
  [ ${#up[@]} -gt 0 ] && DIRS+=("${up[@]}")
  return 0
}
put() { # put SRC DEST MODE
  mk "$(dirname "$2")"
  install -m "$3" "$1" "$2"
  FILES+=("$2")
}
ICON_NAME=codetrail
case "$PREFIX" in "$HOME/.local"|/usr|/usr/local) ;; *) ICON_NAME="$PREFIX/share/icons/hicolor/256x256/apps/codetrail.png" ;; esac

put "$BIN_SRC" "$PREFIX/bin/codetrail" 755
put "$HERE/share/icons/hicolor/scalable/apps/codetrail.svg" "$PREFIX/share/icons/hicolor/scalable/apps/codetrail.svg" 644
put "$HERE/share/icons/hicolor/256x256/apps/codetrail.png" "$PREFIX/share/icons/hicolor/256x256/apps/codetrail.png" 644
tmp=$(mktemp)
sed -e "s|@BIN@|$PREFIX/bin/codetrail|" -e "s|@ICON@|$ICON_NAME|" "$HERE/share/applications/codetrail.desktop" >"$tmp"
put "$tmp" "$PREFIX/share/applications/codetrail.desktop" 644
rm -f "$tmp"
for f in LICENSE THIRD_PARTY_LICENSES.txt README.txt; do put "$HERE/$f" "$SHARE/$f" 644; done
put "$HERE/uninstall.sh" "$SHARE/uninstall.sh" 755

# desktop/icon caches, only if the tools exist (and, for icons, only into a real icon theme)
apps="$PREFIX/share/applications"
if command -v update-desktop-database >/dev/null 2>&1; then
  had=0; [ -e "$apps/mimeinfo.cache" ] && had=1
  update-desktop-database "$apps" >/dev/null 2>&1 || true
  [ "$had" = 0 ] && [ -e "$apps/mimeinfo.cache" ] && FILES+=("$apps/mimeinfo.cache")
fi
theme="$PREFIX/share/icons/hicolor"
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$theme/index.theme" ]; then
  had=0; [ -e "$theme/icon-theme.cache" ] && had=1
  gtk-update-icon-cache -q -f -t "$theme" >/dev/null 2>&1 || true
  [ "$had" = 0 ] && [ -e "$theme/icon-theme.cache" ] && FILES+=("$theme/icon-theme.cache")
fi

{
  echo "# codetrail install manifest - written by install.sh, consumed by uninstall.sh"
  echo "prefix $PREFIX"
  for f in "${FILES[@]}"; do echo "file $f"; done
  for d in "${DIRS[@]}"; do echo "dir $d"; done
  echo "file $MANIFEST"
} >"$MANIFEST"

echo "CodeTrail installed to $PREFIX"
echo "  binary:   $PREFIX/bin/codetrail"
echo "  manifest: $MANIFEST  (uninstall: bash $SHARE/uninstall.sh --prefix $PREFIX)"
case ":${PATH:-}:" in *":$PREFIX/bin:"*) ;; *) echo "hint: $PREFIX/bin is not in your PATH; add:  export PATH=\"$PREFIX/bin:\$PATH\"" ;; esac
echo
echo "Next step (optional, explicit - this installer never edits ~/.claude or ~/.agents):"
echo "  codetrail install --claude      # Claude Code hooks (add --omp / --codex for those agents; --dry-run to preview)"
