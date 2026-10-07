# codetrail

Lightweight commit-diff viewer (Rust + egui) with AI change-reason tracking. Design and decisions: `PLAN.md` (§9 = v2 overrides).

## Build / run

```
cargo build --release -p ct-app          # binary: target/release/codetrail
codetrail [repo]                         # GUI
codetrail --smoke <repo> --screenshot out.png --scene commit|split|search|why|light|settings
```

Keys: Ctrl+P files · Ctrl+Shift+F search · Ctrl+K palette · Ctrl+B base · Ctrl+E edit · Ctrl+S save · Ctrl+Shift+C copy code ref · Ctrl+L Ask AI · `[` `]` hunk · `j` `k` commits.
Colors: `~/.config/codetrail/theme.toml` (or the settings panel). Invalid file falls back to defaults with a banner.

## AI change tracking

```
codetrail install [--claude] [--omp] [--codex] [--dry-run]   # idempotent, backs up before editing
codetrail note --record <id> --reason "<why>"
codetrail ask path:L1-L2@<commit|worktree> [--question Q] [--agent claude|omp|codex] [--print]
codetrail export --json
```

Claude Code: Pre/PostToolUse/PostToolUseFailure/Stop hooks record every edit (always exit 0) and tell the agent the exact `note` command. Records live in `<git_dir>/codetrail/log.ct` (binary frames, zstd reasons) and are **local only**: deleting `.git` or re-cloning loses them; use `codetrail export`.
Commit linking: `post_blob` match = High, added-line fingerprint = Medium, otherwise Unknown (no time-based guessing).

## Known limits

- Non-UTF-8 paths are lossy. Combined (`-c`/`--cc`) merge diffs unsupported (first-parent / chosen parent shown).
- Editor: files > 1 MB or binary are read-only. Korean IME input unverified (display verified).
- omp/Codex get skill/AGENTS instructions only; automatic hooks are Claude Code only.
