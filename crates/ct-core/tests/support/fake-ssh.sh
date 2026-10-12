#!/bin/sh
# Fake `ssh` for tests (CT_SSH_BIN): parses ssh's options up to `--`, skips the target and runs the
# command string through `sh -c` locally, exactly like sshd would run it through the login shell.
#   CT_FAKE_SSH_LOG     append one line per call with the option words
#   CT_FAKE_SSH_ACTIVE  directory; a file named after our pid exists while a call runs
# Behaviour by target host (so tests can run in parallel without touching the environment):
#   hk-unknown hk-changed auth-fail no-resolve refused net-timeout  -> that ssh failure on stderr, exit 255
#   delay-<n>                                                       -> sleep n seconds, then run
#   slowcmd-<n>                                                     -> same, but only for commands containing --verify (git rev-parse --verify)
ctl=""
opts=""
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break ;;
    -O) ctl="$2"; shift 2 ;;
    -o|-p|-i) opts="$opts $1 $2"; shift 2 ;;
    -T) opts="$opts -T"; shift ;;
    *) echo "fake-ssh: unexpected option $1" >&2; exit 254 ;;
  esac
done
[ -n "$CT_FAKE_SSH_LOG" ] && printf '%s | ctl=%s | target=%s | cmd=%s\n' "$opts" "$ctl" "$1" "$(printf %s "$2" | tr '\n' ' ' | cut -c1-300)" >> "$CT_FAKE_SSH_LOG"
if [ -n "$ctl" ]; then
  [ "$ctl" = exit ] && exit 0
  exit 1
fi
host="${1#*@}"
case "$host" in
  hk-unknown) echo "Host key verification failed." >&2; exit 255 ;;
  hk-changed) printf '@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nHost key verification failed.\n' >&2; exit 255 ;;
  auth-fail) echo "$1: Permission denied (publickey,password)." >&2; exit 255 ;;
  no-resolve) echo "ssh: Could not resolve hostname $host: Name or service not known" >&2; exit 255 ;;
  refused) echo "ssh: connect to host $host port 22: Connection refused" >&2; exit 255 ;;
  net-timeout) echo "ssh: connect to host $host port 22: Connection timed out" >&2; exit 255 ;;
esac
shift # target
if [ -n "$CT_FAKE_SSH_ACTIVE" ]; then
  : > "$CT_FAKE_SSH_ACTIVE/$$"
  trap 'rm -f "$CT_FAKE_SSH_ACTIVE/$$"' EXIT
fi
case "$host" in
  delay-*) sleep "${host#delay-}" ;;
  slowcmd-*) case "$1" in *--verify*) sleep "${host#slowcmd-}" ;; esac ;;
esac
sh -c "$1"
