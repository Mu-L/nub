// UserPromptSubmit hook — injects a model-only orchestrator reminder each turn.
// Run with nub (dogfood): `nub .claude/hooks/iw-reminder.ts`.
//
// DYNAMIC: reads `autonomous_mode` from the epic tracker's YAML front-matter
// (the IW structured-state store) so autonomous-specific nudges fire only when
// actually in autonomous mode, and parses the checkbox counts to give a
// high-level board status. Emits hookSpecificOutput.additionalContext (model-
// only). Robust: any failure → just the always-applicable core reminder; never
// throws (a broken hook must not disrupt the prompt).
import { readFileSync } from "node:fs";
import { join } from "node:path";

const core =
  "⟦orchestrator reminder⟧ You are the ORCHESTRATOR: delegate ALL project work — code/doc edits, GitHub writes (comments/PR edits/resolves), builds, tests, investigations — to BACKGROUND sub-agents; never do them yourself in the foreground. Your foreground = dispatch, synthesize returns, decide, and edit your own control surfaces (tracker/memory/skill/settings) + final reviewed git. Keep the epic tracker (epics/*/todo.md) synced THIS turn: fold every returned sub-agent's facts, advance statuses, log decisions/questions — it is your persistent memory, a stale tracker loses work on context drop. Reconcile EVERY in-flight sub-agent; never drop a thread. Before asserting how nub/aube is STRUCTURED, ground it in wiki/architecture.md / the nub-aube-architecture memory / code you just read — never reason from stale or secondhand framing.";

function emit(ctx: string): never {
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: { hookEventName: "UserPromptSubmit", additionalContext: ctx },
    }),
  );
  process.exit(0);
}

try {
  const dir = process.env.CLAUDE_PROJECT_DIR ?? ".";
  const todo = readFileSync(join(dir, "epics/final-polish/todo.md"), "utf8");

  const fm = todo.match(/^---\n([\s\S]*?)\n---/);
  const mode = fm?.[1].match(/^autonomous_mode:\s*(\S+)/m)?.[1] ?? "unknown";

  const body = fm ? todo.slice(fm[0].length) : todo;
  const n = (re: RegExp) => (body.match(re) ?? []).length;
  const status = `tracker: ${n(/^\s*-?\s*\[ \]/gm)} open · ${n(/^\s*-?\s*\[\/\]/gm)} in-progress · ${n(/^\s*-?\s*\[\?\]/gm)} awaiting-the maintainer.`;

  const modeLine =
    mode === "on"
      ? "AUTONOMOUS MODE = ON: don't ask the human questions; bias HARD to action; keep the fleet busy; resolve+document+proceed; never autonomously land a default/security/product/brand decision the maintainer owns."
      : `autonomous_mode=${mode} → interactive: surface decisions + ask rather than auto-landing.`;

  emit(`${core}  ${modeLine}  ${status}`);
} catch {
  emit(core);
}
