#!/usr/bin/env python3
"""Generate THIRD_PARTY_LICENSES.txt for the normal (non-dev, non-build) dependency closure of ct-app.

usage: gen-licenses.py OUT.txt [--report]   (--report prints flagged packages to stderr)
Uses `cargo metadata --offline --locked`; no network, no extra tools.
"""
import hashlib, json, os, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FLAG_WORDS = ("GPL", "AGPL", "LGPL", "MPL", "EPL", "CDDL", "SSPL", "OFL", "Ubuntu-font")
TEXT_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "UNLICENSE", "NOTICE")


def main() -> int:
    out = sys.argv[1]
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--offline", "--locked", "--format-version", "1", "--filter-platform", "x86_64-unknown-linux-gnu"],
        cwd=ROOT))
    pk = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = next(p["id"] for p in meta["packages"] if p["name"] == "ct-app")
    seen, stack = set(), [root]
    while stack:
        i = stack.pop()
        if i in seen:
            continue
        seen.add(i)
        for d in nodes[i]["deps"]:
            if any(k["kind"] is None for k in d["dep_kinds"]):  # normal deps only
                stack.append(d["pkg"])
    pkgs = sorted((pk[i] for i in seen), key=lambda p: (p["name"], p["version"]))
    ours = [p for p in pkgs if p["source"] is None]
    third = [p for p in pkgs if p["source"] is not None]

    flagged, unknown = [], []
    lines = []
    w = lines.append
    w("THIRD-PARTY LICENSES - CodeTrail")
    w("=" * 72)
    w("CodeTrail itself is MIT licensed (see LICENSE). The release binary statically links the")
    w(f"{len(third)} Rust crates below (normal, non-dev dependencies of ct-app; generated from")
    w("`cargo metadata --offline --locked`). Nothing here is dynamically linked except the system")
    w("libraries named in README.txt (glibc, libgcc_s; X11/Wayland/GL are dlopen'ed at run time).")
    w("")
    w("Fonts: egui's `default_fonts` embeds Ubuntu-Light, Hack, NotoEmoji-Regular and emoji-icon-font")
    w("(licenses: Ubuntu Font Licence 1.0, MIT / Bitstream Vera, SIL OFL 1.1). CJK and monospace UI")
    w("fonts (Noto Sans CJK, Pretendard, JetBrains Mono, ...) are NOT bundled; the system ones are used.")
    w("")
    w("%-34s %-14s %-48s %s" % ("crate", "version", "license", "repository"))
    w("-" * 130)
    for p in third:
        lic = p["license"] or ""
        if not lic:
            unknown.append(p)
            lic = "UNKNOWN (license-file: %s)" % (p.get("license_file") or "none")
        if any(f in lic for f in FLAG_WORDS):
            flagged.append(p)
        w("%-34s %-14s %-48s %s" % (p["name"], p["version"], lic, p.get("repository") or p.get("homepage") or ""))
    w("")
    notes = {
        "MPL": "is MPL-2.0 (file-level copyleft). It is used UNMODIFIED; its source is available at the repository above and on crates.io.",
        "OFL": "embeds fonts under SIL OFL 1.1 / Ubuntu Font Licence 1.0; the fonts are not modified or sold separately.",
    }
    for p in flagged:
        for key, text in notes.items():
            if key in (p["license"] or ""):
                w(f"NOTE: {p['name']} {p['version']} {text}")
    w("No GPL/AGPL/LGPL crates are linked." if not any(x in (p["license"] or "") for p in third for x in ("GPL",)) else "WARNING: GPL-family crate present.")
    w("")
    w("Workspace crates (MIT, see LICENSE): " + ", ".join(p["name"] for p in ours))
    w("")

    # license texts, de-duplicated by content
    texts: dict[str, tuple[str, list[str]]] = {}
    for p in third:
        d = os.path.dirname(p["manifest_path"])
        names = sorted(n for n in os.listdir(d) if n.upper().startswith(TEXT_PREFIXES) and os.path.isfile(os.path.join(d, n)))
        for n in names:
            try:
                t = open(os.path.join(d, n), encoding="utf-8", errors="replace").read().strip()
            except OSError:
                continue
            if not t:
                continue
            h = hashlib.sha256(t.encode()).hexdigest()
            texts.setdefault(h, (t, []))[1].append(f"{p['name']} {p['version']}")
    w("LICENSE TEXTS (as shipped in the crates' sources; identical texts are listed once)")
    w("=" * 72)
    for _, (t, users) in sorted(texts.items(), key=lambda kv: kv[1][1][0]):
        w("")
        w("Applies to: " + ", ".join(sorted(set(users))))
        w("-" * 72)
        w(t)
    open(out, "w", encoding="utf-8").write("\n".join(lines) + "\n")

    if "--report" in sys.argv:
        print(f"{len(third)} third-party crates, {len(texts)} distinct license texts", file=sys.stderr)
        for p in flagged:
            print(f"FLAG {p['name']} {p['version']}: {p['license']}", file=sys.stderr)
        for p in unknown:
            print(f"UNKNOWN {p['name']} {p['version']}", file=sys.stderr)
    return 0


sys.exit(main())
