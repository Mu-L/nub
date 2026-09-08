# Runtime benchmarks

Measurements of the augmentations Nub applies to a running Node process, each against plain `node` on the same box. These are the numbers behind the runtime figures on the site; the charts are drawn with the `nub-charts` skill from a saved run, never from typed-in values.

| Script | What it measures |
|--------|------------------|
| `async-context-frame.sh` | `AsyncLocalStorage` on Node 22 with and without `--experimental-async-context-frame` (the flag Nub injects on 22.9–23.x): a request-shaped store/await loop, and Fastify 5 + OpenTelemetry SDK under autocannon. |
| `threadpool.sh` | `UV_THREADPOOL_SIZE` at Node's fixed 4 versus the core count Nub installs: Fastify 5 routes that queue on the pool (pbkdf2, gzip, file read, stat) under autocannon, then a `dns.lookup` burst. Plain `node` with the variable set, so only the pool size varies. |

Both run on a Linux box at the repo root with `NUB_BIN` set, which is what `remote-build --job adhoc` provides:

```sh
nub scripts/remote-build.ts --job adhoc --script tests/bench/runtime/threadpool.sh --detach
nub scripts/remote-build.ts --attach <vm-name>
```

Every measurement is also printed as a `ROW {...}` JSON line. Save a run worth keeping under `results/<date>.json` with the machine, the Node version and the per-round values, and point a chart generator at that file.

The load generator shares the box with the server, so a route bound by the event loop can read a few percent lower when more pool threads compete for the same cores. Report that alongside the routes that gain; it is part of the result.
