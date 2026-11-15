// Spike: append whatever a hook receives to events.jsonl, unmodified.
// Usage: node log-event.mjs <configured-for> <event>
// Prints nothing, so no hook decision is ever returned.
import { appendFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [configuredFor = "?", event = "?"] = process.argv.slice(2);
const out = join(dirname(fileURLToPath(import.meta.url)), "events.jsonl");

let raw = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => (raw += chunk));
let done = false;
process.stdin.on("end", () => {
  if (done) return;
  done = true;
  let payload;
  try {
    payload = JSON.parse(raw);
  } catch {
    payload = { unparsed: raw };
  }
  const record = {
    at: new Date().toISOString(),
    configuredFor,
    event,
    ppid: process.ppid,
    cwd: process.cwd(),
    // Names only: values may hold tokens.
    envHints: Object.keys(process.env).filter((k) => /claude|copilot|codex|openai|github/i.test(k)).sort(),
    payload,
  };
  appendFileSync(out, JSON.stringify(record) + "\n");
});
// Some callers may never close stdin; do not hold the agent up.
setTimeout(() => process.stdin.emit("end"), 3000).unref();
