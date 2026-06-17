// @ts-check
/**
 * fray — SubagentStop hook. Fires when a background sub-agent comes to REST
 * (stops with no live children of its own; it may be resumed, so this can fire
 * MORE THAN ONCE for the same agent, and a rest does NOT mean the agent's
 * deliverable is finished).
 *
 * Its ONE job: append a timestamped line to `.fray/.rested-agents.jsonl` so the
 * Stop hook (fray-stop-reminder) can refuse to let the orchestrator go idle while
 * a rest sits un-reconciled. This is the mechanism backstop for the #1 recurring
 * failure — a rested agent's findings never getting folded + its queue drained.
 *
 * FAIL-OPEN: any error → exit 0. A sub-agent must NEVER be blocked from stopping
 * by this recorder, and a write failure must not surface as an error.
 */
import { appendFileSync, readFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

const PROJECT_DIR = process.env.CLAUDE_PROJECT_DIR || process.cwd();
const FRAY_DIR = join(PROJECT_DIR, '.fray');
const REST_LOG = join(FRAY_DIR, '.rested-agents.jsonl');

try {
  let payload = {};
  try {
    const raw = readFileSync(0, 'utf8');
    if (raw.trim()) payload = JSON.parse(raw);
  } catch {
    /* no/invalid stdin → record the bare event anyway */
  }
  try {
    mkdirSync(FRAY_DIR, { recursive: true });
  } catch {
    /* already exists */
  }
  const rec = {
    ts: new Date().toISOString(),
    // best-effort identifiers — payload shape varies; record whatever is present
    transcript: payload.transcript_path || null,
    session: payload.session_id || null,
  };
  appendFileSync(REST_LOG, JSON.stringify(rec) + '\n');
} catch {
  /* fail-open */
}
process.exit(0);
