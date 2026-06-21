#!/usr/bin/env node
// @ts-check
/**
 * fray — SubagentStop hook. The HARD GUARANTEE behind the push-then-EXIT rule:
 * refuse to let a sub-agent come to rest while a feature/PR worktree holds a
 * committed-but-UNPUSHED change (local branch tip ahead of its remote upstream).
 *
 * WHY: the #1 strand failure is an agent that creates a commit/merge in a worktree,
 * then rests (e.g. "I'll wait for CI") BEFORE pushing — leaving the only copy of
 * verified work local until the orchestrator manually re-pokes it (~20min strands
 * observed). This hook converts the *rule* "push the instant a commit exists" into a
 * structural floor: an agent literally cannot rest on unpushed work.
 *
 * HOW: on SubagentStop, scan the project's git worktrees. If any FEATURE worktree
 * (a `/tmp/nub-wt-*` worktree, NOT the shared main tree, NOT on `main`) has a branch
 * whose tip is ahead of its upstream → BLOCK the rest with a reason telling the agent
 * to `git push` first. The block re-prompts the sub-agent (decision:"block").
 *
 * NARROW (never over-blocks):
 *   - only `/tmp/nub-wt-*` worktrees (the fray PR-worktree convention);
 *   - never the shared main tree, never a branch named `main`/`master`;
 *   - only a branch WITH an upstream that is provably ahead (rev-list count > 0) —
 *     a branch with no upstream (never pushed at all) is NOT flagged here, because we
 *     can't distinguish "about to push" from "scratch branch"; the rule + epilogue
 *     cover that case, and a false block there would be the wedge we must avoid;
 *   - uncommitted-only / clean state is never flagged (only a real local-ahead commit).
 *
 * FAIL-OPEN, ABSOLUTELY: any error, any uncertainty → allow the rest (emit {} / exit 0).
 * A bug here could wedge EVERY agent, so every path degrades to "allow". We never throw,
 * never block on a git command that errored, and never block on parse failure.
 */
import { readFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';

/** Allow the rest (the safe default). @returns {never} */
function allow() {
  process.stdout.write('{}');
  process.exit(0);
}

/**
 * Block the rest and feed a reason back to the sub-agent.
 * @param {string} reason
 * @returns {never}
 */
function block(reason) {
  process.stdout.write(JSON.stringify({ decision: 'block', reason }));
  process.exit(0);
}

/**
 * Run a git command in a given dir; return trimmed stdout, or null on ANY error.
 * @param {string} cwd
 * @param {string[]} args
 * @returns {string|null}
 */
function git(cwd, args) {
  try {
    return execFileSync('git', ['-C', cwd, ...args], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
      timeout: 5000,
    }).trim();
  } catch {
    return null;
  }
}

try {
  // Parse stdin (SubagentStop payload). A parse failure must not block.
  let payload = {};
  try {
    const raw = readFileSync(0, 'utf8');
    if (raw.trim()) payload = JSON.parse(raw);
  } catch {
    allow();
  }

  // Guard against an infinite block loop: if a prior fray block is already active for
  // this stop, do not stack another. (SubagentStop may re-fire; never wedge.)
  if (payload && payload.stop_hook_active === true) allow();

  const projectDir = process.env.CLAUDE_PROJECT_DIR || process.cwd();

  // Enumerate worktrees from the project tree. If git can't, fail open.
  const porcelain = git(projectDir, ['worktree', 'list', '--porcelain']);
  if (!porcelain) allow();

  // Parse `git worktree list --porcelain` into {path, branch} records.
  /** @type {{path:string, branch:string|null}[]} */
  const worktrees = [];
  let cur = /** @type {{path:string, branch:string|null}|null} */ (null);
  for (const line of porcelain.split('\n')) {
    if (line.startsWith('worktree ')) {
      if (cur) worktrees.push(cur);
      cur = { path: line.slice('worktree '.length).trim(), branch: null };
    } else if (line.startsWith('branch ') && cur) {
      // e.g. "branch refs/heads/my-feature" → "my-feature"
      cur.branch = line.slice('branch '.length).trim().replace(/^refs\/heads\//, '');
    }
  }
  if (cur) worktrees.push(cur);

  /** @type {{path:string, branch:string, ahead:number}[]} */
  const stranded = [];
  for (const wt of worktrees) {
    // NARROW: only fray PR-worktrees (/tmp/nub-wt-* or /private/tmp/nub-wt-*).
    if (!/(^|\/)(private\/)?tmp\/nub-wt-/.test(wt.path)) continue;
    // Never the main tree / a main-ish branch.
    if (!wt.branch || wt.branch === 'main' || wt.branch === 'master') continue;
    // Only a branch WITH an upstream that is provably ahead.
    const upstream = git(wt.path, ['rev-parse', '--abbrev-ref', '--symbolic-full-name', '@{upstream}']);
    if (!upstream) continue; // no upstream → can't prove unpushed → don't block (fail open)
    const aheadStr = git(wt.path, ['rev-list', '--count', '@{upstream}..HEAD']);
    if (aheadStr === null) continue; // git errored → fail open for this worktree
    const ahead = Number.parseInt(aheadStr, 10);
    if (Number.isFinite(ahead) && ahead > 0) {
      stranded.push({ path: wt.path, branch: wt.branch, ahead });
    }
  }

  if (stranded.length === 0) allow();

  const lines = stranded
    .map((s) => `  • ${s.path} (branch ${s.branch}) is ${s.ahead} commit(s) ahead of its remote — UNPUSHED.`)
    .join('\n');
  block(
    `⟦fray push-before-rest guard⟧ You are coming to rest with a committed-but-UNPUSHED change in a PR worktree:\n${lines}\n\n` +
      `Per the push-then-EXIT rule: a commit is stranded until it's pushed. PUSH IT NOW before resting — ` +
      `\`cd <worktree> && git push origin HEAD:<branch>\` (safe pre-CI; CI runs on the pushed commit). ` +
      `Do NOT arm a CI watcher or poll loop afterward — push, report "pushed <sha>, awaiting CI" (and append a line to ` +
      `.fray/merge-queue.jsonl if this is a PR), and EXIT. The orchestrator's heartbeat owns merge-on-green. ` +
      `(If this worktree is genuinely not yours / not ready to push, that is the ONLY case to ignore this — but never leave a verified commit unpushed.)`,
  );
} catch {
  allow(); // fail open — a bug here must never wedge an agent
}
