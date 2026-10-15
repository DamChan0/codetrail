# Changelog

## 0.3.0

- **Files tab** opens a file the current comparison does not touch in the stored Diff / Blame / Edit mode (Diff shows Blame with a "not changed in this comparison" note) instead of always the editor.
- **Inspector > Blame** loads the selection's history by itself and summarises it: last change (commit, author, date, message), commit / author counts, and the commit that introduced the lines.
- **Zoom** (`zoom` in `config.toml`, 50–300%): Settings > Appearance, or `Ctrl+Plus` / `Ctrl+Minus` / `Ctrl+0`; scales the whole UI and is remembered.
- The Unified / Side by side switch in the top bar only shows while a diff is on screen.
- The window uses the CodeTrail icon.

## 0.2.0

- **Current tab**: always present; staged / unstaged / untracked groups, per-group diffs, badge with changed-file count.
- **Line-history time travel**: clicking a history entry shows the file at that revision in the centre pane only (rail selection untouched), with the selected lines highlighted and a Back button; ↑/↓ steps through entries.
- **Sticky Diff / Blame / Edit mode** across files, commits and tabs (`view_mode` in `config.toml`).
- **Remote repositories** over SSH, Tailscale-first: Tailscale SSH (no keys), SSH via `tailscale nc`, or key-based SSH; peer discovery and `~/.ssh/config` import; read-only. Per-connection ControlMaster, symlink-safe reads, byte budgets, bounded caches.
- **Settings page** (General, Appearance, Agents & Accounts, Remotes, Resources & Limits, Shortcuts, About) replaces the settings window; `Ctrl+,`.
- **File-name filter** in Commits and Current (AND terms, `!exclude`, globs, `ext:`), diff restricted to matches; optional "only commits touching these files".
- **Folder path autocomplete** in the project picker (local and remote).
- Panel widths are clamped to the window; one connection banner instead of repeated errors; search thread pool capped at 4.
- Packaging: `dist/build.sh` (tarball, `.deb`, checksums), `install.sh` / `uninstall.sh`, CI and release workflows.

## 0.1.0

First release.

- Commit review: commit list, file list, unified / side-by-side diff, syntax highlighting, rename and binary handling; **Current** tab for staged / unstaged / untracked changes.
- Base selection: parent, previous selection, working tree, any ref, merge-base.
- AI change history: `codetrail install` (Claude Code hooks + skill, omp skill, Codex AGENTS.md), `hook`, `record`, `note`, `export`; append-only binary log in `<git_dir>/codetrail/`; Why panel with confidence levels; Blame.
- Ask AI from a selection (`path:L1-L2@rev`) via Pi (ChatGPT / GitHub Copilot), Claude CLI or Codex CLI; `codetrail ask` on the command line.
- Accounts: Pi OAuth in an app-private runtime, Claude and Codex through their official CLIs; no API keys stored.
- Runs: background agent runs in isolated git worktrees, Apply (`merge --no-ff`) / Discard, concurrency, per-run and global RAM/time limits, process-group abort.
- Search: fuzzy file names, text search (literal / regex / case), commit filter; simple editor (files up to 1 MB).
- Resource guards: max 4 concurrent git processes, timeouts, 64 MiB output cap, status-bar RAM/CPU indicator.
- Themeable (`theme.toml`), `config.toml` settings, Korean display with CJK font fallback.
- Known limits: see README.
