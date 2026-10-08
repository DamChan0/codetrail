# Changelog

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
