// Exercises the HTMLRewriter global end-to-end under nub. Prints `LINE: <value>`
// lines the integration test asserts against. Each line pins one contract.
import assert from "node:assert";

// The global must be present and a constructor under nub augmentation.
assert.strictEqual(typeof HTMLRewriter, "function", "HTMLRewriter global missing");
// Additive contract: invisible to enumeration.
assert.ok(
  !Object.keys(globalThis).includes("HTMLRewriter"),
  "HTMLRewriter must be non-enumerable",
);

// --- string form: element attribute + content mutation ---
const out1 = new HTMLRewriter()
  .on("a[href]", {
    element(el) {
      el.setAttribute("rel", "noopener");
    },
  })
  .on("h1", {
    element(el) {
      el.setInnerContent("Hi");
    },
  })
  .transform(`<h1>x</h1><a href="/">link</a>`);
console.log("ATTR:", out1);

// --- escaped vs raw insertion ---
const out2 = new HTMLRewriter()
  .on("p", {
    element(el) {
      el.append("<b>raw</b>", { html: true });
      el.append("<i>esc</i>");
    },
  })
  .transform("<p>x</p>");
console.log("CONTENT:", out2);

// --- remove + document end append + doctype read ---
let doctypeName = "";
const out3 = new HTMLRewriter()
  .on("script", { element(el) { el.remove(); } })
  .onDocument({
    doctype(dt) { doctypeName = String(dt.name); },
    end(end) { end.append("<!--end-->", { html: true }); },
  })
  .transform(`<!DOCTYPE html><div>keep</div><script>evil()</script>`);
console.log("DOCTYPE:", doctypeName);
console.log("REMOVE:", out3);

// --- text handler ---
const out4 = new HTMLRewriter()
  .on("span", {
    text(t) {
      if (t.text) t.replace(t.text.toUpperCase());
    },
  })
  .transform("<span>hello</span>");
console.log("TEXT:", out4);

// --- streaming over a Response ---
const res = new HTMLRewriter()
  .on("title", { element(el) { el.setInnerContent("Streamed"); } })
  .transform(new Response("<title>old</title>", { headers: { "content-type": "text/html" } }));
assert.ok(res instanceof Response, "transform(Response) must return a Response");
const body = await res.text();
console.log("STREAM:", body);

// --- invalid selector throws synchronously at .on() ---
let threw = false;
try {
  new HTMLRewriter().on("a + b", { element() {} });
} catch {
  threw = true;
}
console.log("BADSEL:", threw);

// --- async handler is rejected (first-cut: sync only) ---
let asyncThrew = false;
try {
  new HTMLRewriter()
    .on("a", { element() { return Promise.resolve(); } })
    .transform("<a>x</a>");
} catch (e) {
  asyncThrew = e instanceof TypeError;
}
console.log("ASYNC:", asyncThrew);

console.log("DONE");
