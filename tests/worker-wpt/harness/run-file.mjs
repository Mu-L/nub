// Per-file WPT driver. Runs ONE web-platform-test `.any.js` file through nub's
// runtime and emits its subtest results as a single JSON line on stdout, framed
// by `__WPT_RESULT__` so the parent (run-wpt.mjs) can parse it apart from any
// console noise a test may emit.
//
// Mechanism mirrors Node's `test/common/wpt/worker.js`: install the multi-global
// `self`/`GLOBAL` hooks, load the REAL `testharness.js` (so the harness's own
// assert/deep-equals/result machinery is exercised — NOT a hand-rolled shim, the
// quirk that inflated the Phase-1 prototype's fail count), then load the `META:
// script=` includes and the test body in the same realm. nub's polyfilled
// globals (Worker, MessageChannel, MessagePort, MessageEvent, structuredClone)
// are what the harness tests, because this driver itself runs UNDER nub.
//
// Two scopes, per the test's `// META: global=` directive (decision: drive nub's
// polyfilled globals, not raw worker_threads — the runner's whole point):
//   - window / default → run in this main realm.
//   - worker-only      → run the harness + body INSIDE a real nub `Worker`, so
//     the worker-scope globals (self.addEventListener / dispatchEvent / onmessage)
//     are genuinely under test. A `global=window,worker` file runs main-scope here
//     (the parent runner schedules the worker-scope pass separately if configured).
//
// Usage (invoked by run-wpt.mjs, never by hand):
//   nub run-file.mjs <wpt-root> <test-rel-path> <scope>
//     scope ∈ { window, worker }

import { readFileSync } from "node:fs";
import { resolve, dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import vm from "node:vm";

const WPT_ROOT = process.argv[2];
const TEST_REL = process.argv[3];
const SCOPE = process.argv[4] || "window";

if (!WPT_ROOT || !TEST_REL) {
  process.stderr.write("usage: run-file.mjs <wpt-root> <test-rel> <scope>\n");
  process.exit(2);
}

const HARNESS = join(WPT_ROOT, "resources", "testharness.js");

// Parse the `// META:` block (script includes + global=). The block is the run
// of leading comment lines; the first non-comment line ends it.
function parseMeta(src) {
  const scripts = [];
  let globals = "window,dedicatedworker";
  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (line === "") continue;
    if (!line.startsWith("//")) break;
    const m = /^\/\/\s*META:\s*([^=]+?)=(.*)$/.exec(line);
    if (!m) continue;
    const key = m[1].trim();
    const val = m[2].trim();
    if (key === "script") scripts.push(val);
    else if (key === "global") globals = val;
  }
  return { scripts, globals };
}

function resolveScript(spec, testDir) {
  // A leading "/" is wpt-root-relative; otherwise relative to the test file's dir.
  return spec.startsWith("/") ? join(WPT_ROOT, spec.slice(1)) : resolve(testDir, spec);
}

// Emit the framed result line and exit. Status: 0 ok / 1 file-error.
function emit(results, fileError) {
  process.stdout.write(
    "\n__WPT_RESULT__" + JSON.stringify({ rel: TEST_REL, scope: SCOPE, fileError: fileError ?? null, results }) + "\n",
    // A test may leave a Worker/MessagePort holding the event loop open (or testharness
    // may still think a test is pending). Once the result is written, exit hard so the
    // subprocess doesn't linger to the parent's kill timeout.
    () => process.exit(0)
  );
}

// Build the source that, when run in a realm whose globals are nub's, drives the
// harness and harvests results. Shared between the main-realm path and the
// worker path (the worker receives this string and eval-runs it).
//
// It returns a Promise that resolves with the harness's collected results. The
// realm must already expose `self`, `GLOBAL`, the harness code, the includes, and
// the test body — all passed in via the closure params below.
function makeRealmDriver({ harnessCode, harnessFile, includes, bodyCode, bodyFile, isWindow }) {
  // Everything below executes in the TARGET realm (main or worker). We keep it a
  // single function body so the worker can receive it as a string and run it.
  return function driveHarness() {
    return new Promise((resolveOuter) => {
      // Grace window for async tests to settle before we emit whatever finished.
      // The parent (run-wpt.mjs) sets a hard kill above this; keep them in sync.
      const GRACE_MS = Number((typeof process !== "undefined" && process.env && process.env.WPT_GRACE_MS) || 2000);
      globalThis.self = globalThis;
      globalThis.GLOBAL = {
        isWindow: () => isWindow,
        isWorker: () => !isWindow,
        isShadowRealm: () => false,
      };
      const collected = [];
      let settled = false;
      let watchdog = null;
      const done = () => {
        if (settled) return;
        settled = true;
        if (watchdog) clearTimeout(watchdog);
        resolveOuter(collected);
      };

      // ShellTestEnvironment (what testharness picks when there is no `document`
      // and no DedicatedWorkerGlobalScope — our case under nub) imposes NO default
      // per-test timeout, so a test that never reaches `done()` would hang the
      // harness's completion callback forever. WPT tests that legitimately use a
      // short `setTimeout(t.step_func_done(), N)` finish well within a grace
      // window; if completion hasn't fired by then we emit the subtests that DID
      // finish (captured per-test via add_result_callback). This mirrors Node's
      // wpt-runner, where the parent process kills on a max timeout — except here
      // we self-emit the finished results instead of losing them all.
      watchdog = setTimeout(done, GRACE_MS);

      try {
        vm.runInThisContext(harnessCode, { filename: harnessFile });
      } catch (e) {
        collected.push({ name: "(load testharness.js)", status: 1, message: String(e && e.stack || e) });
        return done();
      }

      // The harness's setup: don't let an unterminated async test hang the realm
      // forever. testharness honors `explicit_timeout`; we drive our own outer
      // timer in the parent process instead, but also register completion here.
      try {
        globalThis.setup({ explicit_timeout: false, allow_uncaught_exception: false });
      } catch { /* setup is optional / may already have run */ }

      globalThis.add_result_callback((t) => {
        collected.push({ name: t.name, status: t.status, message: t.message ?? null });
      });
      globalThis.add_completion_callback(() => done());

      // Load META script includes first (e.g. the structured-clone battery), then
      // the test body — all in this realm so they see nub's globals + the harness.
      try {
        for (const inc of includes) {
          vm.runInThisContext(inc.code, { filename: inc.file });
        }
        vm.runInThisContext(bodyCode, { filename: bodyFile });
      } catch (e) {
        collected.push({ name: "(top-level)", status: 1, message: String(e && e.stack || e) });
        return done();
      }
      // Synchronous-only files never trigger a pending async test, so the harness
      // completion callback fires on a microtask; async files complete when their
      // last async_test calls done(). The parent's per-file timeout is the backstop.
    });
  };
}

async function main() {
  let src;
  try {
    src = readFileSync(join(WPT_ROOT, TEST_REL), "utf8");
  } catch (e) {
    return emit([], "missing test file: " + e.message);
  }
  const { scripts } = parseMeta(src);
  const testDir = dirname(join(WPT_ROOT, TEST_REL));
  const harnessCode = readFileSync(HARNESS, "utf8");

  const includes = [];
  for (const spec of scripts) {
    const p = resolveScript(spec, testDir);
    try {
      includes.push({ file: p, code: readFileSync(p, "utf8") });
    } catch (e) {
      return emit([], `include ${spec}: ${e.message}`);
    }
  }

  const isWindow = SCOPE === "window";

  if (isWindow) {
    // Main-realm path: drive directly here, under nub.
    const drive = makeRealmDriver({
      harnessCode,
      harnessFile: HARNESS,
      includes,
      bodyCode: src,
      bodyFile: join(WPT_ROOT, TEST_REL),
      isWindow: true,
    });
    const results = await drive();
    return emit(results, null);
  }

  // Worker-scope path: run the harness + body INSIDE a real nub Worker so the
  // worker-side globals are the ones under test. The worker is a data: module
  // that reconstructs the same realm driver from the params we serialize in,
  // then posts the collected results back. This is what genuinely exercises
  // worker-scope conformance (the prototype ran everything main-scope).
  const { Worker } = globalThis;
  if (typeof Worker !== "function") {
    return emit([], "Worker global unavailable (polyfill not installed)");
  }

  const payload = {
    harnessCode,
    harnessFile: HARNESS,
    includes,
    bodyCode: src,
    bodyFile: join(WPT_ROOT, TEST_REL),
  };

  // The worker module: rebuild the driver and run it, then post results. We pass
  // the driver function as source text (driveHarness body) so it executes in the
  // worker realm, not the parent.
  const driverFnSource = makeRealmDriver({}).toString();
  const workerSource = `
import vm from "node:vm";
const makeRealmDriver = (cfg) => {
  const { harnessCode, harnessFile, includes, bodyCode, bodyFile, isWindow } = cfg;
  return ${driverFnSource};
};
self.onmessage = async (ev) => {
  const cfg = { ...ev.data, isWindow: false };
  try {
    const results = await makeRealmDriver(cfg)();
    self.postMessage({ ok: true, results });
  } catch (e) {
    self.postMessage({ ok: false, message: String(e && e.stack || e) });
  }
};
`;
  const dataUrl = new URL(
    "data:text/javascript;base64," + Buffer.from(workerSource, "utf8").toString("base64")
  );

  const results = await new Promise((res) => {
    let settled = false;
    const finish = (v) => { if (!settled) { settled = true; res(v); } };
    let w;
    try {
      w = new Worker(dataUrl, { type: "module" });
    } catch (e) {
      return finish({ fileError: "worker spawn: " + e.message, results: [] });
    }
    w.onmessage = (ev) => {
      if (ev.data && ev.data.ok) finish({ fileError: null, results: ev.data.results });
      else finish({ fileError: "worker error: " + (ev.data && ev.data.message), results: [] });
      try { w.terminate(); } catch { /* already gone */ }
    };
    w.onerror = (ev) => {
      finish({ fileError: "worker onerror: " + (ev.message || ev), results: [] });
      try { w.terminate(); } catch { /* already gone */ }
    };
    w.postMessage(payload);
  });

  return emit(results.results, results.fileError);
}

main().catch((e) => {
  emit([], "driver crash: " + String(e && e.stack || e));
});
