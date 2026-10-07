---
name: codetrail
description: Record WHY code was changed. After every file edit, run `codetrail note` with a short reason so the change can be explained later in the codetrail viewer.
---

# codetrail: record the reason for each change

After you edit or create a file, record the **why** (not the what):

```
codetrail note --record <id> --reason "<why, 1-3 sentences, include the user's request intent>"
```

- Claude Code: a hook prints `codetrail record <id> recorded; run: codetrail note --record <id> ...` after each edit. Use that exact `<id>`.
- omp / Codex (no hook): first run `codetrail record --path <file> --agent <omp|codex>`; it prints `recorded <id>`. Then run the `note` command above with that id.

Rules:
- Reason = intent behind the change, quoting or summarising the user's request. 1-3 sentences.
- One note per edit; you may batch several edits of one task by repeating the command per id.
- Never put secrets, tokens or credentials in a reason.
- If `codetrail` is not on PATH or the command fails, continue the task; never block on it.
