#!/usr/bin/env bash
# Removes exactly what install.sh recorded. User data (config, <repo>/.git/codetrail) is left alone.
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PREFIX=""
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX=$2; shift 2 ;;
    --prefix=*) PREFIX=${1#--prefix=}; shift ;;
    -h|--help) echo "usage: uninstall.sh [--prefix DIR]   (default: the prefix this script was installed into)"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
# installed copy lives at <prefix>/share/codetrail/uninstall.sh
[ -n "$PREFIX" ] || PREFIX=$(dirname "$(dirname "$HERE")")
PREFIX=${PREFIX%/}
MANIFEST="$PREFIX/share/codetrail/install-manifest.txt"
[ -f "$MANIFEST" ] || { echo "no CodeTrail install found under $PREFIX (missing $MANIFEST)" >&2; exit 1; }
if [ "$(id -u)" != 0 ] && [ ! -w "$PREFIX/share/codetrail" ]; then
  echo "error: cannot write to $PREFIX; re-run with sudo for a --system install" >&2; exit 1
fi
removed=0
dirs=()
while IFS=' ' read -r kind path; do
  case "$kind" in
    file) if [ -e "$path" ] || [ -L "$path" ]; then rm -f -- "$path"; removed=$((removed+1)); fi ;;
    dir) dirs+=("$path") ;;
  esac
done < <(grep -E '^(file|dir) ' "$MANIFEST" | sed 's/^\(file\|dir\) /\1 /')
# newest (deepest) directories first; only empty ones, never anything we did not create
for ((i=${#dirs[@]}-1; i>=0; i--)); do rmdir -- "${dirs[$i]}" 2>/dev/null || true; done
echo "CodeTrail removed from $PREFIX ($removed files). Your settings (~/.config/codetrail) and repo logs (.git/codetrail) were not touched."
