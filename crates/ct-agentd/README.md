# ct-agentd

Agent backends (pi / claude / codex), app-private runtime, accounts and login.

## Child environment

Children never inherit the parent environment. They get `PATH`, `HOME`, `LANG`/`LC_*` plus a small
fixed set documented on `PASS_THROUGH` in `src/proc.rs`, backend-specific vars and the caller's
`env_allow`. **Proxy variables (`HTTP(S)_PROXY`, `ALL_PROXY`, `NO_PROXY`), `SSL_CERT_*` and
`NODE_EXTRA_CA_CERTS` are forwarded on purpose** so agents work behind proxies; they may contain
credentials of the user's own network. Provider API keys/tokens are never forwarded implicitly.

## Credentials

pi's `auth.json` is only opened by `assets/auth-tool.mjs` (status / logout) and `assets/login-helper.mjs`
(login), both run by the app-private node. The Rust side parses their token-free stdout only.

## pi settle signal

pi 0.74.2 emits no `agent_settled`; `Settled` is derived from `agent_end` after a quiet period
(`SETTLE_QUIET_PERIOD` in `src/pi.rs`). See the doc comment there for the limitation.
