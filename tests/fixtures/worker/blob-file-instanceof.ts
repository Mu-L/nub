// Regression: nub wraps globalThis.Blob (to support blob: workers) by subclassing
// it. `File` is a bootstrap global that extends the NATIVE Blob, so the wrap must
// re-point File's prototype chain or `new File(...) instanceof Blob` silently
// becomes false — an additivity violation vs vanilla Node (File IS-A Blob).
//
// `File` only became a Node global in v20; on the 18.x floor it is absent (in
// vanilla Node too), so the check is conditional — when File exists it MUST still
// be a Blob after the wrap; when it doesn't, that is vanilla-equivalent and the
// line is reported as a skip so the assertion still passes.
const FileCtor = (globalThis as { File?: unknown }).File;
if (typeof FileCtor === "function") {
  const f = new (FileCtor as new (p: unknown[], n: string, o?: unknown) => unknown)(
    ["x"],
    "a.txt",
    { type: "text/plain" },
  );
  console.log("file-instanceof-blob:" + (f instanceof Blob));
} else {
  console.log("file-instanceof-blob:skip-no-File-global");
}
// And a Blob made through the wrapper is still a Blob with the full API.
const b = new Blob(["hello"], { type: "text/plain" });
console.log("blob-instanceof-blob:" + (b instanceof Blob));
console.log("blob-size:" + b.size);
