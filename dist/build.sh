#!/usr/bin/env bash
# Reproducible release build -> dist/out/{codetrail-<ver>-linux-x86_64.tar.gz, codetrail_<ver>_amd64.deb, SHA256SUMS}
# Nothing is published. Env: CARGO_TARGET_DIR (default <repo>/target), CT_OFFLINE=1 (cargo --offline),
#      SOURCE_DATE_EPOCH (default: last commit time), CT_SKIP_BUILD=1 (reuse the binary already built).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT=$PWD
OUT=$ROOT/dist/out
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}
export TZ=UTC LC_ALL=C
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
export CARGO_TARGET_DIR=$TARGET
VER=$(cargo metadata --no-deps --offline --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="ct-app"))')
NAME=codetrail-$VER-linux-x86_64
OFFLINE=(); [ "${CT_OFFLINE:-0}" = 1 ] && OFFLINE=(--offline)

if [ "${CT_SKIP_BUILD:-0}" != 1 ]; then
  # path remapping keeps the binary independent of where it was built
  export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$ROOT=/build --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"
  cargo build --release --locked "${OFFLINE[@]}" -p ct-app
fi
BIN=$TARGET/release/codetrail
[ -x "$BIN" ] || { echo "missing $BIN" >&2; exit 1; }
"$BIN" --version

# highest glibc symbol version the binary needs -> install-time check, deb Depends
REQ_GLIBC=$(objdump -T "$BIN" | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)+' | sed 's/GLIBC_//' | sort -Vu | tail -1)
echo "requires glibc >= $REQ_GLIBC"

rm -rf "$OUT"; mkdir -p "$OUT"
python3 dist/gen-licenses.py "$OUT/THIRD_PARTY_LICENSES.txt" --report

# ---- tarball ------------------------------------------------------------------------------------
STAGE=$OUT/stage/$NAME
mkdir -p "$STAGE/bin" "$STAGE/share/applications" "$STAGE/share/icons/hicolor/scalable/apps" "$STAGE/share/icons/hicolor/256x256/apps"
install -m 755 "$BIN" "$STAGE/bin/codetrail"
install -m 644 dist/codetrail.desktop "$STAGE/share/applications/codetrail.desktop"
install -m 644 assets/icons/codetrail.svg "$STAGE/share/icons/hicolor/scalable/apps/codetrail.svg"
install -m 644 assets/icons/codetrail-256.png "$STAGE/share/icons/hicolor/256x256/apps/codetrail.png"
install -m 644 LICENSE "$STAGE/LICENSE"
install -m 644 "$OUT/THIRD_PARTY_LICENSES.txt" "$STAGE/THIRD_PARTY_LICENSES.txt"
sed -e "s/@VERSION@/$VER/" -e "s/@GLIBC@/$REQ_GLIBC/" dist/README.txt >"$STAGE/README.txt"
chmod 644 "$STAGE/README.txt"
install -m 755 dist/install.sh "$STAGE/install.sh"
install -m 755 dist/uninstall.sh "$STAGE/uninstall.sh"
printf 'VERSION=%s\nREQ_GLIBC=%s\n' "$VER" "$REQ_GLIBC" >"$STAGE/BUILDINFO"
chmod 644 "$STAGE/BUILDINFO"
find "$STAGE" -type d -exec chmod 755 {} +
find "$STAGE" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
( cd "$OUT/stage" && tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$SOURCE_DATE_EPOCH" -cf - "$NAME" | gzip -9n >"$OUT/$NAME.tar.gz" )

# ---- .deb ---------------------------------------------------------------------------------------
DEB=$OUT/deb-root
mkdir -p "$DEB/DEBIAN" "$DEB/usr/bin" "$DEB/usr/share/applications" "$DEB/usr/share/doc/codetrail" \
         "$DEB/usr/share/icons/hicolor/scalable/apps" "$DEB/usr/share/icons/hicolor/256x256/apps"
install -m 755 "$BIN" "$DEB/usr/bin/codetrail"
sed -e 's|@BIN@|codetrail|' -e 's|@ICON@|codetrail|' dist/codetrail.desktop >"$DEB/usr/share/applications/codetrail.desktop"
chmod 644 "$DEB/usr/share/applications/codetrail.desktop"
install -m 644 assets/icons/codetrail.svg "$DEB/usr/share/icons/hicolor/scalable/apps/codetrail.svg"
install -m 644 assets/icons/codetrail-256.png "$DEB/usr/share/icons/hicolor/256x256/apps/codetrail.png"
cat >"$DEB/usr/share/doc/codetrail/copyright" <<COPY
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: codetrail
Source: https://github.com/dmkimd/codetrail

Files: *
Copyright: 2026 dmkimd
License: MIT

License: MIT
$(sed -e '1,2d' -e 's/^$/./' -e 's/^/ /' LICENSE)

Comment: Statically linked Rust crates and their licenses are listed in
 /usr/share/doc/codetrail/THIRD_PARTY_LICENSES.txt.gz
COPY
gzip -9n -c "$OUT/THIRD_PARTY_LICENSES.txt" >"$DEB/usr/share/doc/codetrail/THIRD_PARTY_LICENSES.txt.gz"
chmod 644 "$DEB/usr/share/doc/codetrail/"*
SIZE=$(du -sk --apparent-size "$DEB/usr" | cut -f1)
# Depends = measured: DT_NEEDED (ldd) + dlopen'ed sonames found in the binary
cat >"$DEB/DEBIAN/control" <<CTRL
Package: codetrail
Version: $VER
Section: devel
Priority: optional
Architecture: amd64
Maintainer: dmkimd <dmkimd@users.noreply.github.com>
Installed-Size: $SIZE
Depends: libc6 (>= $REQ_GLIBC), libgcc-s1, git, libegl1, libgl1, libx11-6, libx11-xcb1, libxcb1, libxcursor1, libxi6, libxrender1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libwayland-egl1
Recommends: fonts-noto-cjk
Homepage: https://github.com/dmkimd/codetrail
Description: commit-diff viewer with AI change-reason tracking
 CodeTrail shows commit diffs and records why AI agents (Claude Code, omp,
 Codex) changed code. The agent runtime it downloads for its own use is kept
 in CodeTrail's data directory; no system nodejs is required.
CTRL
( cd "$DEB" && find usr -type f -print0 | sort -z | xargs -0 md5sum >DEBIAN/md5sums )
find "$DEB" -type d -exec chmod 755 {} +
find "$DEB" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
dpkg-deb --root-owner-group -Zxz -b "$DEB" "$OUT/codetrail_${VER}_amd64.deb" >/dev/null

rm -rf "$OUT/stage" "$DEB" "$OUT/THIRD_PARTY_LICENSES.txt"
( cd "$OUT" && sha256sum "$NAME.tar.gz" "codetrail_${VER}_amd64.deb" >SHA256SUMS )
ls -l "$OUT"
cat "$OUT/SHA256SUMS"
