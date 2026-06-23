// Cloudflare-Workers-shape `HTMLRewriter` global for Node.js, backed by the
// first-party lol-html binding in the nub-native N-API addon. Code written
// against Cloudflare Workers' (or Bun's) HTMLRewriter ports here unchanged:
//
//   new HTMLRewriter()
//     .on("a[href]", { element(el) { el.setAttribute("rel", "noopener"); } })
//     .transform(response)
//
// WHY a thin JS wrapper over a native engine: lol-html drives the element/text/
// comment/doctype handlers SYNCHRONOUSLY as it parses each written chunk, so the
// fluent `.on()/.onDocument()/.transform()` surface, the streaming bridge onto a
// WHATWG Response body, and the `{html}`→ContentType mapping all live in JS; the
// native `HtmlRewriterEngine` owns parsing + the rewritable-unit methods.
//
// FIRST CUT — SYNC HANDLERS ONLY. lol-html's write() is fully synchronous and has
// no yield points; true async-handler support needs the Asyncify trampoline CF/Bun
// run above their WASM build, which a native N-API addon cannot reproduce. So a
// handler returning a Promise throws a TypeError (rather than silently dropping the
// awaited mutations). Async handlers are a documented follow-up.

// node: builtins via process.getBuiltinModule (NOT static import) — the same loader-
// chain-leak avoidance the Worker polyfill documents: a static `import` would route
// the builtin through the user's --loader/register() hooks. On the floor where
// getBuiltinModule is absent, createRequire is threaded in via the setter below.
let _bootstrapCreateRequire = null;
export function setBootstrapCreateRequire(fn) {
  _bootstrapCreateRequire = fn;
}
function __getBuiltin(id) {
  if (typeof process.getBuiltinModule === "function") return process.getBuiltinModule(id);
  return _bootstrapCreateRequire(import.meta.url)(id);
}

// Resolve the native engine lazily + memoized. The addon ships in nub's
// distribution next to this file (runtime/addons/nub-native.node); it is loaded by
// ABSOLUTE path so it never touches the ESM loader chain.
let _engineCtor;
function getEngineCtor() {
  if (_engineCtor !== undefined) return _engineCtor;
  const { createRequire } = __getBuiltin("node:module");
  const { fileURLToPath } = __getBuiltin("node:url");
  const require = createRequire(import.meta.url);
  _engineCtor = null;
  for (const rel of ["./addons/nub-native.node", "../runtime/addons/nub-native.node"]) {
    try {
      const addon = require(fileURLToPath(new URL(rel, import.meta.url)));
      if (addon && addon.HtmlRewriterEngine) {
        _engineCtor = addon.HtmlRewriterEngine;
        break;
      }
    } catch {
      // try the next candidate path
    }
  }
  return _engineCtor;
}

// Output sink for the throwaway engine used only to validate selectors at .on().
const NOOP_SINK = () => {};

// A handler that returns a thenable can't be awaited inside lol-html's synchronous
// write loop (see the file header). Surfacing it as a clear error beats silently
// emitting the un-awaited output.
function guardSync(result) {
  if (result != null && typeof result.then === "function") {
    throw new TypeError(
      "HTMLRewriter: async handlers are not supported. Use synchronous handlers, " +
        "or await/buffer the content before transforming.",
    );
  }
}

// Wrap each user handler so (a) a returned Promise is rejected eagerly and (b) the
// native unit object is passed straight through. The native object is valid ONLY
// for the duration of this call (the engine invalidates it on return), matching
// Cloudflare's "the element is only usable inside the handler" contract.
function wrapHandlers(handlers) {
  if (handlers == null || typeof handlers !== "object") {
    throw new TypeError("HTMLRewriter: handlers must be an object");
  }
  const out = {};
  for (const key of ["element", "comments", "text", "doctype", "end"]) {
    const fn = handlers[key];
    if (typeof fn === "function") {
      out[key] = (unit) => {
        guardSync(fn(unit));
      };
    }
  }
  return out;
}

class HTMLRewriter {
  // Registrations are buffered until transform(): the engine is built per-transform
  // so one HTMLRewriter instance can transform multiple inputs (CF parity).
  #elementHandlers = [];
  #documentHandlers = [];

  on(selector, handlers) {
    if (typeof selector !== "string") {
      throw new TypeError("HTMLRewriter.on: selector must be a string");
    }
    const wrapped = wrapHandlers(handlers);
    // Validate the selector eagerly so an invalid selector throws HERE, matching
    // Cloudflare's "throws at .on() registration" contract. The real engine is
    // built lazily at transform() (lol-html consumes its Settings by value, so it
    // can't be reused across transforms), but a throwaway engine parses the
    // selector immediately and surfaces the SelectorError now rather than later.
    const Engine = getEngineCtor();
    if (Engine) new Engine(NOOP_SINK).on(selector, {});
    this.#elementHandlers.push([selector, wrapped]);
    return this;
  }

  onDocument(handlers) {
    this.#documentHandlers.push(wrapHandlers(handlers));
    return this;
  }

  #buildEngine(sink) {
    const Engine = getEngineCtor();
    if (!Engine) {
      throw new Error(
        "HTMLRewriter: the native engine is unavailable (nub-native addon not found).",
      );
    }
    const engine = new Engine(sink);
    // Selector parse errors throw synchronously here, matching CF's "throws at
    // .on() registration" contract closely enough (we register at transform()).
    for (const [selector, h] of this.#elementHandlers) engine.on(selector, h);
    for (const h of this.#documentHandlers) engine.onDocument(h);
    return engine;
  }

  transform(input) {
    // String form (Bun-style ergonomic extension): rewrite eagerly, return a string.
    if (typeof input === "string") {
      const chunks = [];
      const engine = this.#buildEngine((buf) => chunks.push(buf));
      const { TextEncoder, TextDecoder } = globalThis;
      engine.write(Buffer.from(new TextEncoder().encode(input)));
      engine.end();
      return new TextDecoder().decode(concatChunks(chunks));
    }

    // Response form (Cloudflare parity): stream the body through the engine.
    if (!(input instanceof Response)) {
      throw new TypeError(
        "HTMLRewriter.transform: input must be a Response or a string",
      );
    }

    const sourceBody = input.body;
    if (sourceBody == null) {
      // No body to rewrite — return an equivalent empty-body Response.
      return new Response(null, input);
    }

    const self = this;
    const stream = new ReadableStream({
      async start(controller) {
        const engine = self.#buildEngine((buf) => {
          if (buf && buf.length) controller.enqueue(new Uint8Array(buf));
        });
        const reader = sourceBody.getReader();
        try {
          for (;;) {
            const { done, value } = await reader.read();
            if (done) break;
            engine.write(Buffer.from(value));
          }
          engine.end();
          controller.close();
        } catch (err) {
          controller.error(err);
        } finally {
          reader.releaseLock();
        }
      },
    });

    // Rewriting changes byte length, so content-length must not carry over.
    const headers = new Headers(input.headers);
    headers.delete("content-length");
    return new Response(stream, {
      status: input.status,
      statusText: input.statusText,
      headers,
    });
  }
}

function concatChunks(chunks) {
  let total = 0;
  for (const c of chunks) total += c.length;
  const out = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

export function installHTMLRewriter() {
  // Forward-compat / additive: if a real HTMLRewriter is already present (a future
  // Node, or the user's own), do nothing.
  if (typeof globalThis.HTMLRewriter !== "undefined") return;
  // NON-ENUMERABLE: invisible to Object.keys(globalThis)/for-in is the additive
  // contract — vanilla-Node code enumerating the global object must not observe
  // nub's injected global. writable+configurable so user code can override it.
  Object.defineProperty(globalThis, "HTMLRewriter", {
    value: HTMLRewriter,
    enumerable: false,
    writable: true,
    configurable: true,
  });
}

// Fast tier (getBuiltinModule present): install eagerly at module eval, preserving
// the side-effect-on-require contract the lazy preload getter relies on. The engine
// itself is still resolved lazily (first transform), so this costs ~nothing for the
// common "never touch HTMLRewriter" run. On the floor the compat preload calls
// setBootstrapCreateRequire(...) + installHTMLRewriter() explicitly.
if (typeof process.getBuiltinModule === "function") installHTMLRewriter();
