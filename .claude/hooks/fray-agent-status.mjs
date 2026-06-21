// @ts-check
/**
 * fray — DERIVED agent state. The single shared derivation used by BOTH the Stop-hook
 * liveness helper (`fray-agent-liveness.mjs`) and the board (`scripts/fray/index.mjs`),
 * so an agent's reported state can never drift between the two.
 *
 * THE PRINCIPLE — compute, don't store (the same rule the board already follows for
 * thread status). A thread's `agents:` binding records ONLY immutable-at-dispatch facts
 * (`{id, label}`); it carries NO hand-maintained per-agent `status` field. (A legacy
 * `status:` may still be PRESENT in old frontmatter — it is IGNORED, never trusted.)
 * Every liveness/doneness judgement is DERIVED here from ground truth:
 *
 *   - output-file (`tasks/<id>.output`) mtime → how long since the agent last wrote,
 *   - the THREAD's own `status:` (done/dismissed = terminal) → whether the orchestrator
 *     has deliberately reconciled the thread.
 *
 * There is NO durable per-agent completion signal a hook can read: `.rested-agents.jsonl`
 * records only `{ts, transcript, session}` — NO agent id — so it cannot attribute a rest
 * to a specific agent. "Done" is therefore INFERRED (terminal-or-stale output + thread
 * status), never read from a stored per-agent flag. That absence is exactly why the old
 * hand-maintained `status` field drifted and false-flagged a completed agent as idle;
 * removing it makes that drift class structurally impossible.
 *
 * Derived states (one per dispatched agent):
 *   - 'terminal'       — the THREAD is terminal (done/dismissed). Nothing to flag.
 *   - 'unreconciled'   — output is stale (>frozenMin) BUT the thread is non-terminal:
 *                        a likely-finished agent the orchestrator never folded. THE one
 *                        signal that matters.
 *   - 'frozen'         — output stale (>frozenMin), thread non-terminal, treated the same
 *                        as unreconciled (a stale-output agent IS the unreconciled case);
 *                        kept as a distinct return only for callers that want the wording.
 *   - 'idle'           — output quiet (>idleMin, <=frozenMin), thread non-terminal: poke
 *                        or confirm it's mid-long-build.
 *   - 'fresh'          — output recent (<=idleMin): actively working, say nothing.
 *   - 'unknown'        — no readable output file (placeholder id, never-started): can't
 *                        judge; fail-open (say nothing).
 */

import { readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

export const DEFAULT_IDLE_MIN = 10;
export const DEFAULT_FROZEN_MIN = 25;

/**
 * GROUND-TRUTH age of an agent's last activity, in minutes — globbed across ALL local
 * Claude task dirs (`<tmp>/claude-<uid>/<project>/<session>/tasks/<id>.output`). Unlike
 * the Stop hook's `deriveTasksDir` (which has the transcript_path and so knows the exact
 * session), the BOARD runs standalone with no session id, so it must search every session
 * for the agent's output symlink. `statSync` follows the symlink → the TARGET transcript's
 * real last-write (the symlink's own mtime is stale). Returns the freshest match's age, or
 * null when no output file exists (placeholder id / never-started / different machine).
 * Fail-open: any error → null.
 * @param {string} agentId
 * @param {number} [now]
 * @returns {number|null} minutes since last activity, or null
 */
export function findAgentOutputAge(agentId, now = Date.now()) {
  if (!agentId) return null;
  let best = null; // most-recent mtimeMs found
  try {
    for (const base of ['/tmp', '/private/tmp']) {
      let claudeDirs;
      try {
        claudeDirs = readdirSync(base).filter((d) => d.startsWith('claude-'));
      } catch {
        continue;
      }
      for (const cd of claudeDirs) {
        const root = join(base, cd);
        let projects;
        try {
          projects = readdirSync(root);
        } catch {
          continue;
        }
        for (const proj of projects) {
          let sessions;
          try {
            sessions = readdirSync(join(root, proj));
          } catch {
            continue;
          }
          for (const sess of sessions) {
            try {
              const st = statSync(join(root, proj, sess, 'tasks', `${agentId}.output`));
              if (best == null || st.mtimeMs > best) best = st.mtimeMs;
            } catch {
              /* not in this session */
            }
          }
        }
      }
    }
  } catch {
    /* fail-open */
  }
  return best == null ? null : (now - best) / 60000;
}

/**
 * Derive one agent's state PURELY from ground truth. No per-agent stored status is
 * consulted — `ageMin` comes from the output-file mtime and the thread status from the
 * thread's own `status:` frontmatter.
 *
 * SCOPING — ACTIVE-ONLY flagging. `status: active` is the invariant that an agent is WORKING
 * the thread RIGHT NOW (the binding hook auto-sets it on dispatch; the orchestrator sets it on
 * a warm-resume). So a stale agent on an `active` thread is a REAL stall — flag it, at any age.
 * EVERY other status means no agent is supposed to be live on the thread, so a stale agent is
 * expected, never flagged:
 *   - `needs-decision` = the agent FINISHED and parked the thread on a human decision — it
 *     clears the instant the status is set, so it is NEVER flagged.
 *   - `blocked` / `enqueued` / `planned` / `todo` = nothing is actively running.
 *   - `done` / `dismissed` (terminal) = deliberately reconciled.
 * There is no recency band: a stale agent is flagged iff the thread is `active`, regardless of
 * age. A done-but-parked thread therefore stops false-flagging IMMEDIATELY (no time-window wait).
 *
 * @param {object} a
 * @param {string} [a.threadStatus]      the owning thread's `status:` (active / needs-decision
 *                                       / enqueued / blocked / planned / todo / done /
 *                                       dismissed). Only `active` is flaggable. If omitted,
 *                                       falls back to the legacy `threadTerminal` boolean
 *                                       (treated as `active` when non-terminal).
 * @param {boolean} [a.threadTerminal]   LEGACY — is the thread done/dismissed? Used only when
 *                                       `threadStatus` is not supplied (back-compat).
 * @param {number|null} a.ageMin         minutes since the agent's output last changed, or
 *                                       null when there is no readable output file.
 * @param {number} [a.idleMin]           idle threshold (min). Default {@link DEFAULT_IDLE_MIN}.
 * @param {number} [a.frozenMin]         frozen/stale threshold (min). Default {@link DEFAULT_FROZEN_MIN}.
 * @returns {'terminal'|'unreconciled'|'idle'|'fresh'|'unknown'}
 */
export function deriveAgentState({
  ageMin,
  threadStatus,
  threadTerminal,
  idleMin = DEFAULT_IDLE_MIN,
  frozenMin = DEFAULT_FROZEN_MIN,
}) {
  // Normalize to a status string. If only the legacy boolean was passed, map it: terminal →
  // a terminal status; non-terminal → treat as `active` (the active-only flag behavior).
  const status = threadStatus ?? (threadTerminal ? 'done' : 'active');

  // ACTIVE-ONLY: a stale/idle agent is surfaced ONLY when the thread is `active`. Every other
  // status (needs-decision / blocked / enqueued / planned / todo / done / dismissed) means no
  // agent should be live on the thread → never flag.
  if (status !== 'active') return 'terminal';

  if (ageMin == null) return 'unknown'; // no activity file → can't judge (fail-open)

  if (ageMin > frozenMin) return 'unreconciled';
  if (ageMin > idleMin) return 'idle';
  return 'fresh';
}
