CodeTrail @VERSION@ - commit-diff viewer with AI change-reason tracking (Linux x86_64)

INSTALL
  ./install.sh                  per-user install into ~/.local (no root)
  ./install.sh --prefix DIR     install into DIR
  sudo ./install.sh --system    install into /usr/local (needs root)
  <prefix>/share/codetrail/uninstall.sh      removes exactly what was installed

The installer never touches ~/.claude, ~/.agents or any repository. To let an agent record its
edits, run explicitly afterwards:   codetrail install --claude   (or --omp / --codex; --dry-run first)

RUN
  codetrail [REPO]              open the GUI (reopens the last project when REPO is omitted)
  codetrail --version | --help  work without a display
  codetrail <install|hook|note|record|ask|export> ...   command line, no GUI libraries are loaded

REQUIREMENTS
  - glibc >= @GLIBC@ (this build), git on PATH
  - a display server: X11 or Wayland, plus these libraries (loaded at run time):
      Debian/Ubuntu: sudo apt install libegl1 libgl1 libx11-6 libx11-xcb1 libxcb1 libxcursor1 libxi6 \
                     libxrender1 libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libwayland-egl1
  - recommended: fonts-noto-cjk (Korean/CJK text). Fonts are NOT bundled; the UI falls back to the
    built-in Latin font when no CJK font is installed. Pretendard / JetBrains Mono are used if present.
  - AI features (optional): the claude / codex CLIs are used as installed by you; the pi agent runtime is
    downloaded into CodeTrail's own data directory on first use and never touches your global node/npm.

DATA
  Settings:      ~/.config/codetrail/
  Change log:    <repo>/.git/codetrail/ (local only; use `codetrail export --json` for backups)

LICENSE: MIT (LICENSE). Third-party crates: THIRD_PARTY_LICENSES.txt
