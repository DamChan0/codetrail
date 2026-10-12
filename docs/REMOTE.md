# Remote repositories (SSH / Tailscale)

CodeTrail can show diffs, history, blame and the "Why" panel of a repository that lives on another
machine. Nothing is installed on the remote side: CodeTrail runs `git` there over `ssh`.

> Status legend: statements marked **[unverified]** are derived from the design contract and were
> not exercised end-to-end by the author of this document (the transport itself is owned by
> `ct-core::remote`). Discovery (`remote::discover`) is covered by unit and integration tests.

## How it works

- A *remote* is a saved connection: name, host, optional user/port/identity file, transport and the
  host-key policy (`RemoteSpec`). A remote *repository* is a remote plus an absolute path on it.
- Each git call becomes one `ssh` child:
  `ssh -T -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=10 -o ServerAliveCountMax=3
  -o ControlMaster=auto -o ControlPersist=120 -o ControlPath=<dir>/%C … -- user@host '<remote command>'`.
  The remote command is `cd <root> && exec git -c core.quotepath=false -c color.ui=never
  -c core.pager=cat <args>` with `LC_ALL=C GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0 GIT_PAGER=cat`
  set inline. Every argument is single-quote escaped. **[unverified]**
- One ssh *ControlMaster* connection is reused for 120 s of idleness, so only the first call pays
  the handshake. The control socket lives in `$XDG_RUNTIME_DIR/codetrail-ssh` (or
  `~/.cache/codetrail/ssh`), mode 0700. CodeTrail closes it (`ssh -O exit`) when you switch project
  or quit. **[unverified]**
- AI change records (`<git_dir>/codetrail/log.ct` on the remote) are read through ssh and cached
  under `~/.cache/codetrail/remote/<hash>/` (hard cap 64 MiB), so the Why panel works. **[unverified]**
- The `ssh` binary can be overridden with the environment variable `CT_SSH_BIN` (used by tests).

## SSH requirements

- The remote needs only `git` (any reasonably recent version) and a POSIX shell.
- Authentication must work **without prompts**: `BatchMode=yes` forbids password and passphrase
  prompts. Use an ssh key loaded in `ssh-agent`, an unencrypted/agent-cached key, a key given via
  *Identity file* (passed with `-i … -o IdentitiesOnly=yes`), or Tailscale SSH (below).
- Aliases from `~/.ssh/config` work as the *host*; the settings page can import them
  (`ssh_config_hosts`: `Host` aliases with `HostName`, `User`, `Port`, `IdentityFile`; one level of
  `Include`, max 8 files of 256 KiB; wildcard/negated patterns and `Match` blocks are skipped and
  `Host *` options are not copied into entries).
- Test any remote with the **Test connection** button before saving; it reports hostname, `$HOME`,
  git version and round-trip latency.

## Host key policy

| Setting | ssh option | Behaviour |
|---|---|---|
| *Trust new host key* off (default) | `StrictHostKeyChecking=yes` | Unknown host → connection refused (`host key unknown`). Run `ssh <host>` once in a terminal and accept the key, or enable the option. |
| *Trust new host key* on | `StrictHostKeyChecking=accept-new` | First connection records the key automatically; a **changed** key is still refused. |

CodeTrail never uses `StrictHostKeyChecking=no`. A changed host key is always an error
(`host key changed`); fix it deliberately with `ssh-keygen -R <host>` after you have verified why
it changed.

## Tailscale

1. **Peers list**: the settings page runs `tailscale status --json` (cleared environment, only
   `PATH`; 5 s timeout, process-group kill, 4 MiB output cap) and lists this node and up to 200
   peers, online ones first. Peers without a Tailscale IP are skipped. Choosing a peer fills the
   host with its MagicDNS name (or first IP if MagicDNS is off). Verified against a real tailnet
   (172 nodes parsed).
2. **Plain ssh over the tailnet**: the MagicDNS name / `100.x` address is just an ssh host.
   Transport *SSH* works as long as your normal `ssh` can reach it.
3. **Tailscale SSH**: if the target runs `tailscale up --ssh`, authentication is handled by
   Tailscale (ACLs), no keys needed. Use transport *SSH* with the tailnet name. **[unverified]**
4. **Transport *Tailscale (nc)***: adds `-o ProxyCommand="tailscale nc %h %p"`. Use it when
   the machine runs in userspace-networking mode or the OS has no route into the tailnet.
   **[unverified]**

Other people's nodes can appear in the list (shared nodes). CodeTrail only connects to a host you
explicitly saved and test.

## Read-only in this version

Remote repositories can be browsed (changes, commits, diffs, blame, search, Why). The following are
disabled with a one-line reason in the UI: Edit, Runs / Ask-AI worktrees, agent hook installation,
applying changes, anything that writes to the remote.

## Resource limits

- Every ssh child goes through the same git slot limiter as local git (**max 4 concurrent**), runs
  in its own process group, has a timeout and an output cap; cancel/timeout kills the whole group.
- ssh `ServerAliveInterval=10`/`CountMax=3` ends dead connections in ~30 s; `ConnectTimeout=10`.
- Directory browsing lists at most 500 entries per folder; the Tailscale list is capped at 200;
  the remote AI-record cache is capped at 64 MiB; ssh_config parsing is capped (8 files, 256 KiB
  each, 2000 hosts).
- Idle CPU stays under 1 %: no polling loops; the only background process is the ssh ControlMaster
  (idle, exits 120 s after the last use).

## Troubleshooting by error kind

| Error | Meaning | What to do |
|---|---|---|
| `HostKeyUnknown` | Host not in `known_hosts` and policy is strict. | Run `ssh <host>` once, or enable *Trust new host key*. |
| `HostKeyChanged` | Key differs from `known_hosts`. | Verify the machine was reinstalled/changed, then `ssh-keygen -R <host>`. Never bypass blindly. |
| `AuthFailed` | No key accepted, or a prompt would be needed (BatchMode). | `ssh-add` your key, set *Identity file*, check `user`, or use Tailscale SSH. Test in a terminal with `ssh -o BatchMode=yes <host> true`. |
| `Unreachable` | DNS/route/port failure. | Check the name/IP/port, that sshd runs, that Tailscale is up (`tailscale status`). For tailnet hosts try transport *Tailscale (nc)*. |
| `Timeout` | No answer within the time budget. | Host asleep/offline or very slow link; retry. |
| `NoGit` | `git` not found on the remote `PATH` for non-interactive shells. | Install git, or make it available in the non-login shell `PATH`. |
| `NotARepo` | Chosen folder has no git repository. | Pick the repository root (the folder containing `.git`). |
| `Other` | Anything else, e.g. Tailscale not installed / not logged in / daemon down. | Read the message; it contains the command to run (`tailscale up`, `sudo systemctl start tailscaled`, `sudo tailscale set --operator=$USER`). |

Tailscale discovery hints (these are `Other`): binary missing from `PATH`; `NeedsLogin` /
`Stopped` state; daemon not running; permission denied on the tailscaled socket; no answer in 5 s.

## Security notes

- No agent forwarding (`-a` semantics: it is never enabled), no X11 forwarding, no TTY (`-T`), no
  password prompts (`BatchMode=yes`).
- `StrictHostKeyChecking=no` is never used; the weakest mode is `accept-new`.
- `RemoteSpec::validate()` rejects hosts that start with `-` (option injection), and whitespace or
  control characters in host/user/identity file; the destination is always passed after `--`.
  **[unverified]**
- Remote command arguments are single-quote escaped; paths with spaces, quotes, `$()` or newlines
  are passed as data, not interpreted. **[unverified]**
- Saved remotes contain no secrets: only an optional *path* to an identity file is stored, never a
  key or passphrase.
- The ControlPath directory is private (0700) and the socket name is the ssh `%C` hash.
- The remote log cache holds repository metadata (who changed what and why) in your home directory;
  delete `~/.cache/codetrail/remote/` to wipe it.
