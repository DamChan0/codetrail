# Changelog

## Unreleased

- **Remotes: pick a Tailscale peer from the Host field**: the add/edit form lists the tailnet (`name — 100.x.y.z · OS`, online first, offline only on request, filtered by what you type, plus *Type manually*). Picking fills Host with the peer's Tailscale IP and Name with its host name; User is prefilled only from a saved remote or `~/.ssh/config` and says where from. Replaces the separate *Discover Tailscale peers* panel. Without Tailscale, Host stays a text field with a short note.
- **New remotes default to plain SSH** (`ssh user@host` with your agent, keys and `~/.ssh/config`), which also works with Tailscale SSH hosts; Tailscale SSH is an explicit choice. Before, new remotes defaulted to Tailscale SSH, which fails unless the host runs `tailscale set --ssh`.
- **Clearer authentication errors**: an empty User is called out in the form and in the error ("connecting as local user …"), and a failed login names the account ssh tried and the fix (`ssh-copy-id` / `ssh-add`; `tailscale set --ssh` and the ACL `ssh` rules for Tailscale SSH). codetrail never prompts for passwords.

## 0.5.8

- **Tab in the Open folder path box (local and remote) now reaches the suggestions**: Tab completes the common prefix, then highlights the first suggestion and cycles (Shift+Tab back), Enter accepts. Before, egui moved keyboard focus to the next widget on Tab, so the dropdown never got it. Esc likewise closes only the dropdown and keeps the cursor in the field; with the dropdown closed, Tab moves focus as usual.
- **The PRs page follows the active GitHub CLI account**: after `gh auth login`, `logout` or `switch`, the list, reviewers, checks, *Review requested* and the activity-bar badge reload as the new account: on the next refresh check while the page is open, or when the window regains focus. The header shows the account (`@login`) the list was loaded as. Switching accounts clears the selection and phase filter, and a failed load after a switch no longer leaves the previous account's PRs on screen. Before, the page kept the old account's PRs until the minute timer or Refresh.

## 0.5.7

- **Optional failing checks no longer mark a PR "Needs work"**: only a failing check that branch protection requires does. Optional failures show as a dim "N optional failing" chip on the card, and the inspector tags every check *required* or *optional*. When GitHub cannot say which checks are required, any failure still counts.

## 0.5.6

- **Character-precise selection in diff and blame**: dragging selects and highlights exactly the text dragged over, also part of a line or across partial first / last lines; `Ctrl+C` copies just that text. A click or shift-click still selects whole lines.
- **Copy absolute file path** is a command (`Ctrl+Alt+Shift+C`); it and *Copy absolute path:line* / *Copy code reference* can be rebound in Settings > Shortcuts.

## 0.5.5

- **Copy code from the diff and blame views**: drag (or click / shift-click) over lines and press `Ctrl+C` to copy the code without line numbers or diff markers. In side by side, the pane you select in is the one copied (old lines on the left, new on the right).
- **Copy absolute path:line**: `Ctrl+Alt+C`, a right-click on a code line, or the copy button beside the file path copies `/abs/path/file:12` (`:12-15` for several lines; old-side numbers on the left pane), or the file's absolute path when no line is selected. Remote projects use the remote path. The path bar menu also copies the repo-relative path.
- **Ctrl+C chords with Shift / Alt** (`Ctrl+Shift+C` copy code reference, `Ctrl+Alt+C`) now reach the shortcut table and can be recorded on the Shortcuts page; the windowing layer delivered them only as a plain copy event.

## 0.5.4

- **GitHub CLI setup**: PR authentication or permission errors offer sign-in / repository-scope refresh, also available in Settings > GitHub CLI.
- **My approval** is highlighted in yellow on PR cards and details.
- **PR cards** open their GitHub link with Ctrl-click; a normal click still selects the card.
- README screenshots refreshed with CodeTrail source and clearly labeled fictional PR stack data.

## 0.5.3

- **Review requested** uses GitHub's review-list query: `is:pr user-review-requested:@me archived:false is:open user:<owner> draft:false sort:updated-desc` (only PRs that ask you personally, across the owner's repositories, no drafts). A PR from another repository names it in the detail split.

## 0.5.2

- **Self-update restart fixed**: after the binary was swapped, the restart used the path Linux reports for the replaced file (`codetrail (deleted)`) and failed with "No such file or directory"; a second update then wrote a stray `codetrail (deleted)` file. The binary's path is now taken once at startup, a re-check after installing keeps offering the restart, and a failed restart no longer hides it.

## 0.5.1

- **Search highlights every match** in the opened file, not only the first one of the hit's line; the opened hit is outlined. An **overview ruler** beside the editor shows where the matches are (and the visible part); a hovered marker grows and names its line, a click opens that hit.
- **Section shortcuts**: `Ctrl+1` Commits, `Ctrl+2` Changes, `Ctrl+4` Files, `Ctrl+6` Runs (with `Ctrl+Shift+F` Search and `Ctrl+Shift+P` Pull requests); all rebindable, shown in the activity bar tooltips.
- **PR stacks**: more room between a PR and the one stacked on it, with arrows in merge order; the PR to merge first (bottom of a stack) is drawn bold with a "Merge first" badge. Cards shrink and simplify as the detail split is dragged wider (full, compact, mini; the rest is on hover).
- **Review requested** loads again: PRs are fetched 20 per request (a 50-PR page timed out with HTTP 504 when many reviews were requested), up to the 60 most recently updated; the header says when more match.

## 0.5.0

- **Pull requests page** (activity bar > PRs, `Ctrl+Shift+P`): open PRs of the project from `gh api graphql`, refreshed every minute. Phase tiles (needs work / in review / ready to merge / in merge queue / draft) count and filter; stacks are drawn lane by lane from their base branch; the detail split lists every reviewer's state (approved, changes requested, waiting, asked again), unresolved threads, CI checks (failing first, click to open) and the merge state. ↑ / ↓ walk the cards, Enter opens the PR. Mine / Review requested switch.
- **Self-update**: checks the latest GitHub release at startup (Settings > About: check now, turn off). The notice installs it in place (SHA-256 verified, atomic swap) and restarts; a system-wide install gets a link to the release page.
- **Layout**: an activity bar replaces the rail's tab strip (icons, labels, counts for local changes, failing PRs and runs); each section has a title; comparison controls show only in the code views; settings navigation is a list with the content beside it; font details moved from the status bar to Settings > About.

## 0.4.0

- **Search**: ↑ / ↓ step to the previous / next hit (across files, opening collapsed ones) while the search field has focus or nothing does; the field keeps focus.
- **Search**: opening a hit highlights the match (and its line) in the editor until the text is edited; on a remote project the hit line is selected in Blame.
- **Shortcuts are editable**: Settings > Shortcuts, click a binding and press the new keys (Ctrl/Alt or an F-key required; a key already in use moves over). Stored as overrides in `[keys]` of `config.toml`, e.g. `OpenProject = "Ctrl+Shift+O"`, `""` unbinds.
- **Remote folder picker** also accepts a folder inside a git repository (`.git` in any parent).
- **Path autocomplete**: Tab completes the common prefix; when there is nothing more to complete it steps through the suggestions (Shift+Tab back), Enter accepts.

## 0.3.1

- The project picker opens a folder inside a git repository (`.git` in a parent folder), not only the repository root.

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
