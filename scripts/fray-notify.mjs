#!/usr/bin/env node
// fray-notify — a DURABLE, human-facing notification queue for the orchestrator.
//
// The problem it fixes: in a long autonomous session, the things the human most
// needs to see — a headline WIN that landed, a decision that's genuinely theirs
// to make, a blocker — get BURIED under per-turn status churn and scroll out of
// reach. The human asked: a queue I write to the instant something noteworthy
// happens, that persists until THEY dismiss it, and that re-surfaces every time
// I go idle (via the companion Stop hook `.claude/hooks/fray-notify-surface.mjs`).
//
// Storage: `.fray/notify-queue.jsonl`, one JSON object per line:
//   {id, ts, kind, text, status: "open"|"dismissed", surfaced: bool}
// kind ∈ WIN | DECISION | BLOCKER | FYI. `surfaced` is set by the Stop hook once
// it has shown the item once (so it interrupts idle exactly once per new item,
// then persists quietly until the human dismisses it).
//
// Usage:
//   node scripts/fray-notify.mjs add <WIN|DECISION|BLOCKER|FYI> "text"   # → prints the new id
//   node scripts/fray-notify.mjs list [--all]                            # open items (or all)
//   node scripts/fray-notify.mjs dismiss <id>[ <id> ...] | --all
// Runs under plain node (type-stripping) and nub.
import { readFileSync, writeFileSync, existsSync, mkdirSync } from "node:fs";
import { dirname, resolve, join } from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const QUEUE = join(REPO_ROOT, ".fray", "notify-queue.jsonl");
const KINDS = new Set(["WIN", "DECISION", "BLOCKER", "FYI"]);

function read() {
  if (!existsSync(QUEUE)) return [];
  return readFileSync(QUEUE, "utf8")
    .split("\n")
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l));
}
function write(items) {
  mkdirSync(dirname(QUEUE), { recursive: true });
  writeFileSync(QUEUE, items.map((i) => JSON.stringify(i)).join("\n") + (items.length ? "\n" : ""));
}
function newId(items) {
  const n = items.reduce((m, i) => Math.max(m, Number(String(i.id).replace(/\D/g, "")) || 0), 0);
  return "n" + (n + 1);
}

const [cmd, ...rest] = process.argv.slice(2);
const items = read();

if (cmd === "add") {
  const kind = (rest[0] || "").toUpperCase();
  const text = rest.slice(1).join(" ").trim();
  if (!KINDS.has(kind) || !text) {
    console.error("usage: fray-notify add <WIN|DECISION|BLOCKER|FYI> \"text\"");
    process.exit(1);
  }
  const id = newId(items);
  items.push({ id, ts: new Date().toISOString(), kind, text, status: "open", surfaced: false });
  write(items);
  console.log(id);
} else if (cmd === "list") {
  const all = rest.includes("--all");
  const rows = items.filter((i) => all || i.status === "open");
  for (const i of rows) console.log(`${i.id} [${i.kind}]${i.status === "dismissed" ? " (dismissed)" : ""} ${i.text}`);
  if (!rows.length) console.log("(no notifications)");
} else if (cmd === "dismiss") {
  const all = rest.includes("--all");
  const ids = new Set(rest);
  let n = 0;
  for (const i of items) {
    if (i.status === "open" && (all || ids.has(i.id))) {
      i.status = "dismissed";
      n++;
    }
  }
  write(items);
  console.log(`dismissed ${n}`);
} else {
  console.error('usage: fray-notify <add|list|dismiss> …');
  process.exit(1);
}
