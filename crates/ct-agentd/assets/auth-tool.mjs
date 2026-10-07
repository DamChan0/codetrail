// codetrail auth.json tool: the only code outside pi that touches pi's auth.json contents.
// The Rust side never reads token values; it asks this script instead.
//
//   node auth-tool.mjs status <authJsonPath>
//     stdout: {"providers":{"<id>":{"type":"oauth","expires":true}}}   (token-free)
//   node auth-tool.mjs logout <authJsonPath> <providerId>
//     stdout: {"removed":true|false}   (rewrites auth.json atomically, mode 0600, other entries verbatim)
//
// Errors: {"error":"<short message>"} on stdout, exit 0. Messages never include file contents.
import { chmodSync, closeSync, fsyncSync, openSync, readFileSync, renameSync, writeSync } from "node:fs";

const [cmd, authPath, providerId] = process.argv.slice(2);

function out(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
  process.exit(0);
}

function load() {
  let text;
  try {
    text = readFileSync(authPath, "utf8");
  } catch (e) {
    if (e && e.code === "ENOENT") return {};
    return out({ error: "cannot read auth.json" });
  }
  if (text.trim() === "") return {};
  let v;
  try {
    v = JSON.parse(text);
  } catch {
    return out({ error: "auth.json is not valid JSON" });
  }
  if (v === null || typeof v !== "object" || Array.isArray(v)) return out({ error: "auth.json is not an object" });
  return v;
}

if (cmd === "status") {
  const auth = load();
  const providers = {};
  for (const [id, entry] of Object.entries(auth)) {
    const e = entry && typeof entry === "object" ? entry : {};
    providers[id] = {
      type: typeof e.type === "string" ? e.type.slice(0, 32) : null,
      expires: e.expires !== undefined && e.expires !== null,
    };
  }
  out({ providers });
} else if (cmd === "logout") {
  if (!providerId) out({ error: "missing provider" });
  const auth = load();
  if (!Object.prototype.hasOwnProperty.call(auth, providerId)) out({ removed: false });
  delete auth[providerId];
  const tmp = `${authPath}.tmp`;
  try {
    const fd = openSync(tmp, "w", 0o600);
    writeSync(fd, JSON.stringify(auth, null, 2));
    fsyncSync(fd);
    closeSync(fd);
    chmodSync(tmp, 0o600);
    renameSync(tmp, authPath);
  } catch {
    out({ error: "cannot write auth.json" });
  }
  out({ removed: true });
} else {
  out({ error: "unknown command" });
}
