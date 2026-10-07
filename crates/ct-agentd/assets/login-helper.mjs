// codetrail login helper: drives pi-ai's own OAuth flow for one provider.
//
// argv: <providerId> <authJsonPath>
// stdin  (JSONL): {"code":"..."} | {"cancel":true}
// stdout (JSONL): {"t":"url","url":..,"instructions":..} | {"t":"need_code","prompt":..}
//                 {"t":"progress","msg":..} | {"t":"done"} | {"t":"failed","msg":..}
//
// Credentials are written straight into auth.json (mode 0600, existing entries merged);
// they are never printed to stdout/stderr and never leave this process.
import { chmodSync, closeSync, existsSync, fsyncSync, openSync, readFileSync, renameSync, writeSync } from "node:fs";
import { dirname, join } from "node:path";
import { getOAuthProvider } from "@earendil-works/pi-ai/oauth";

const [providerId, authPath] = process.argv.slice(2);
const ALLOWED = new Set(["openai-codex", "github-copilot"]);

function emit(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function fail(msg) {
  emit({ t: "failed", msg: String(msg) });
  process.exit(0);
}

if (!ALLOWED.has(providerId)) fail(`provider not supported for in-app login: ${providerId}`);

const abort = new AbortController();
const waiters = [];
let cancelled = false;

function nextCode() {
  return new Promise((resolve, reject) => {
    if (cancelled) return reject(new Error("cancelled"));
    waiters.push({ resolve, reject });
  });
}

let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buf += chunk;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.cancel) {
      cancelled = true;
      abort.abort();
      for (const w of waiters.splice(0)) w.reject(new Error("cancelled"));
    } else if (typeof msg.code === "string") {
      const w = waiters.shift();
      if (w) w.resolve(msg.code);
    }
  }
});
process.stdin.on("end", () => {
  cancelled = true;
  abort.abort();
  for (const w of waiters.splice(0)) w.reject(new Error("cancelled"));
});

function saveCredentials(credentials) {
  let auth = {};
  if (existsSync(authPath)) {
    try {
      auth = JSON.parse(readFileSync(authPath, "utf8"));
    } catch {
      throw new Error("auth.json exists but is not valid JSON; refusing to overwrite");
    }
  }
  auth[providerId] = { type: "oauth", ...credentials };
  const tmp = join(dirname(authPath), `.auth.json.${process.pid}.tmp`);
  const fd = openSync(tmp, "w", 0o600);
  try {
    writeSync(fd, JSON.stringify(auth, null, 2));
    fsyncSync(fd);
  } finally {
    closeSync(fd);
  }
  chmodSync(tmp, 0o600);
  renameSync(tmp, authPath);
}

try {
  const provider = getOAuthProvider(providerId);
  if (!provider) fail(`unknown provider: ${providerId}`);
  const needCode = (message) => {
    emit({ t: "need_code", prompt: message });
    return nextCode();
  };
  const credentials = await provider.login({
    onAuth: (info) => emit({ t: "url", url: info.url, instructions: info.instructions ?? null }),
    onPrompt: async (p) => {
      if (p.allowEmpty) return "";
      return needCode(p.message);
    },
    onProgress: (msg) => emit({ t: "progress", msg }),
    onManualCodeInput: provider.usesCallbackServer
      ? () => needCode("Paste the authorization code or redirect URL if the browser did not complete the login")
      : undefined,
    signal: abort.signal,
  });
  if (cancelled) fail("cancelled");
  saveCredentials(credentials);
  emit({ t: "done" });
  process.exit(0);
} catch (e) {
  fail(cancelled ? "cancelled" : e && e.message ? e.message : e);
}
