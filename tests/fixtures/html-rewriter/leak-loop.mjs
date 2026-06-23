// Bounded leak guard: many rejecting + cancelled transforms must NOT accumulate
// WASM engines. Before the asyncify.js rewind-on-reject + safeFree fixes, each
// rejecting/cancel-mid-suspend transform leaked its engine (held Rust borrow →
// free() throws), measured at ~20KB/engine of WASM linear memory. Here we run
// enough cycles that an unbounded leak would balloon RSS well past the threshold;
// with the fixes RSS stays near the JS-churn baseline.
//
// Run with --expose-gc (the test harness passes it) for a stable reading.

const N = 3000;
const enc = new TextEncoder();
const rssMB = () => Math.round(process.memoryUsage().rss / 1024 / 1024);

if (typeof global.gc === "function") global.gc();
const before = rssMB();

for (let i = 0; i < N; i++) {
  // Alternate: rejecting async handler, and cancel-mid-suspend.
  if (i % 2 === 0) {
    try {
      await new HTMLRewriter()
        .on("a", { async element() { await Promise.resolve(); throw new Error("x"); } })
        .transform(new Response("<a>x</a>"))
        .text();
    } catch {
      // expected
    }
  } else {
    const src = new Response(
      new ReadableStream({ start(c) { c.enqueue(enc.encode("<a>x</a>")); } }),
    );
    const res = new HTMLRewriter()
      .on("a", { async element(el) { await new Promise((r) => setTimeout(r, 0)); el.setAttribute("x", "1"); } })
      .transform(src);
    const reader = res.body.getReader();
    const rp = reader.read().catch(() => {});
    await reader.cancel("abort").catch(() => {});
    await rp;
  }
}

if (typeof global.gc === "function") global.gc();
const delta = rssMB() - before;

// A per-engine WASM leak over 3000 cycles would add tens of MB and climb with N;
// the fixed path stays within JS churn. 60MB is a generous ceiling that an
// unbounded leak (was +100MB at 5000 rejects alone) blows past.
console.log("LEAK_DELTA_MB:", delta);
console.log("LEAK_BOUNDED:", delta < 60);
console.log("DONE");
