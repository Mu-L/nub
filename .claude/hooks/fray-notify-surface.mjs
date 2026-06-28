#!/usr/bin/env node
// fray-notify-surface — Stop hook that surfaces the durable notification queue
// (`.fray/notify-queue.jsonl`, written via scripts/fray-notify.mjs) to the HUMAN
// the moment the orchestrator goes idle, so headline wins / decisions / blockers
// can't scroll out of reach or get buried under status churn.
//
// Contract (mirrors the fray plugin's stop-reminder, the known-good channel):
//   - User-facing text rides `systemMessage` (shows as a calm line, not a red wall).
//   - Model-facing text rides `hookSpecificOutput.additionalContext`.
//   - We BLOCK (decision:block) ONLY when there are OPEN items not yet surfaced,
//     so a new notification interrupts idle exactly ONCE to guarantee the human
//     sees it; we then stamp surfaced:true so it never loops. Items persist
//     (status:open) until the human dismisses them — they keep showing in
//     `fray-notify list` and in any future surface, so nothing is lost.
//   - Any error → allow the stop (never wedge the session on a notify bug).
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { resolve, join } from "node:path";

function allow() {
  process.exit(0);
}

try {
  const root = process.env.CLAUDE_PROJECT_DIR || process.cwd();
  const queue = join(root, ".fray", "notify-queue.jsonl");
  if (!existsSync(queue)) allow();

  const items = readFileSync(queue, "utf8")
    .split("\n")
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l));

  const open = items.filter((i) => i.status === "open");
  if (!open.length) allow();

  const unsurfaced = open.filter((i) => !i.surfaced);
  if (!unsurfaced.length) allow(); // already shown once; persists quietly until dismissed

  // Stamp the newly-surfaced items so we don't re-block on them next idle.
  for (const i of items) if (i.status === "open" && !i.surfaced) i.surfaced = true;
  writeFileSync(queue, items.map((i) => JSON.stringify(i)).join("\n") + "\n");

  const line = (i) => `• [${i.kind}] ${i.text}  (id ${i.id})`;
  const userMsg =
    `📌 ${open.length} pending notification${open.length > 1 ? "s" : ""} for you ` +
    `(persist until dismissed — \`node scripts/fray-notify.mjs dismiss <id>\`):\n` +
    open.map(line).join("\n");
  const modelMsg =
    `Durable notification queue has ${unsurfaced.length} NEW item(s). Relay them to the human ` +
    `verbatim in your next message (they're surfaced to the user via systemMessage already), then you may rest. ` +
    `Do NOT dismiss them yourself — the human dismisses. Open items:\n` +
    open.map(line).join("\n");

  process.stdout.write(
    JSON.stringify({
      decision: "block",
      hookSpecificOutput: { hookEventName: "Stop", additionalContext: modelMsg },
      systemMessage: userMsg,
    }) + "\n",
  );
  process.exit(0);
} catch {
  allow();
}
