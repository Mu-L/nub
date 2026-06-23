// Regression: a JS handler that drives the SAME native engine re-entrantly
// (write/end from inside a handler) must throw a clean error, never alias
// &mut HtmlRewriterEngine and re-enter lol-html → aliasing UB. The per-instance
// re-entrancy guard rejects same-instance re-entry; cross-instance must still work.
//
// Loads the native addon directly to reach the low-level engine where re-entrancy
// is reachable. If the guard were absent, this would CRASH the process (non-zero
// exit), so completing with the asserted output is itself the proof.
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
let addon = null;
for (const rel of [
  "../../../runtime/addons/nub-native.node",
]) {
  try {
    addon = require(fileURLToPath(new URL(rel, import.meta.url)));
    break;
  } catch {
    // try next
  }
}
if (!addon || !addon.HtmlRewriterEngine) {
  // Fall back: resolve via the same dist path the wrapper uses by going through
  // the global (which lazy-loads the engine), then read its constructor off a
  // throwaway instance is not exposed — so require the addon next to the preload.
  throw new Error("nub-native addon with HtmlRewriterEngine not found for the re-entrancy test");
}

const { HtmlRewriterEngine } = addon;

// --- same-instance re-entrancy is rejected cleanly ---
let sameInstanceGuarded = false;
{
  const out = [];
  const engine = new HtmlRewriterEngine((buf) => out.push(buf));
  engine.on("div", {
    element() {
      // Re-enter write() on the SAME engine from inside its own handler.
      engine.write(Buffer.from("<span></span>"));
    },
  });
  try {
    engine.write(Buffer.from("<div>x</div>"));
    engine.end();
  } catch (e) {
    // The re-entrant write throws; lol-html surfaces it out of the outer write.
    sameInstanceGuarded = /re-entrant|already processing/i.test(String(e.message));
  }
}
console.log("SAME_INSTANCE_GUARDED:", sameInstanceGuarded);

// --- cross-instance re-entrancy still works (engine A handler drives engine B) ---
let crossInstanceWorks = false;
{
  const outB = [];
  const b = new HtmlRewriterEngine((buf) => outB.push(buf));
  b.on("i", { element(el) { el.setAttribute("done", "1"); } });

  const outA = [];
  const a = new HtmlRewriterEngine((buf) => outA.push(buf));
  a.on("div", {
    element() {
      // Drive a DIFFERENT engine from inside A's handler — must be allowed.
      b.write(Buffer.from("<i>y</i>"));
      b.end();
    },
  });
  a.write(Buffer.from("<div>x</div>"));
  a.end();
  const decoded = Buffer.concat(outB.map((u) => Buffer.from(u))).toString();
  crossInstanceWorks = decoded.includes('done="1"');
}
console.log("CROSS_INSTANCE_WORKS:", crossInstanceWorks);

console.log("DONE");
