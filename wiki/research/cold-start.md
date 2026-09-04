# Cold Start: where Node spends its 12–20 ms

Most of Node's warm startup is C++ that runs before any bootstrap JavaScript — dyld fixups, OpenSSL one-time init, V8 isolate construction and snapshot deserialize — which is what decides how much of the gap to Bun Nub can reclaim.

> Research target: source-grounded breakdown of why `node hello.js` takes ~15 ms warm on macOS arm64 while `bun hello.js` finishes in <5 ms. Locally measured on Node v24.14.0 / Bun 1.3.9. Goal: name the costs, name the upstream work, decide what Nub should do.

## Local measurements (Apple Silicon, macOS 15)

Hyperfine, `--shell=none`, 100 runs after a 10-run warmup:

| Invocation | Mean | Min … Max |
|---|---|---|
| `node -e ''`            | 26.7 ms | 25.3 … 28.9 |
| `node hello.cjs`        | 27.8 ms | 26.3 … 35.1 |
| `node hello.mjs`        | 29.4 ms | 28.1 … 32.4 |
| `node import-fs.mjs`    | 31.7 ms | 29.2 … 110.0 |
| `bun -e ''`             |  4.4 ms |  3.8 … 5.4  |
| `bun hello.cjs`         | 11.3 ms |  9.7 … 17.4 |
| `bun hello.mjs`         | 10.8 ms |  9.7 … 12.6 |
| `bun import-fs.mjs`     | 17.0 ms | 15.7 … 19.4 |

Cold first-run (fs cache evicted) is far worse — Node hit **137 ms** on the very first invocation before warmup — but the warm case dominates day-to-day developer experience.

The `node -e ''` case loads no script file; the ~1 ms it shaves vs `node hello.cjs` is the fopen+stat+read+parse of an empty script. Neither `--no-warnings` nor `--disable-warning` made a measurable difference, so the per-warning install cost is below noise.

## Where v26.7 spends it, measured in 2026-09

Three mechanisms explain most of Node's remaining gap to Deno 2.9 and Bun: V8's per-isolate search for three random primes on Node 25+ (every platform), and on macOS only, dyld weak-def coalescing before `main` plus two `atexit()` symbol-table scans.

The prime search exists because Node's build omits a define that V8's own build sets by default; the macOS costs come from ~2100 default-visibility weak-def symbols and a 204K-entry symbol table that Apple's `atexit` scans through `dladdr`.

Deno 2.9 halved Deno's own cold start (34.2 → 17.3 ms on Deno's x86_64 Linux box, per its release post), which is where a "Node is ~2.4× slower than Deno" figure comes from. Against Deno 2.8.1 the gap was ~1.2×. Re-measured here with Node v26.7.0 official binaries, Deno 2.9.0 and Bun 1.4 (hyperfine minimums; the Mac was under load ~40, the Linux box was idle):

| Invocation | macOS arm64 (M-series, loaded) | Linux x64 (n2-standard-64, idle) |
|---|---|---|
| `node hello.js` | 38.8 ms | 20.7 ms |
| `node --version` | 18.2 ms | 2.8 ms |
| `deno run hello.js` (2.9.0) | 17.3 ms | 12.2 ms |
| `deno run hello.js` (2.8.1) | 27.4 ms | 27.6 ms |
| `bun hello.js` | 18.0 ms | 3.4 ms |

`node --version` is the tell: it runs no JavaScript and creates no isolate, yet costs 18 ms on macOS and 3 ms on Linux. The macOS gap is outside Node's code; the Linux gap is entirely inside the isolate.

### Phase split on macOS

An interposer inserted with `DYLD_INSERT_LIBRARIES` timestamps its own initializer, `exit()` and `_exit()`; a `posix_spawn` harness reads them and keeps the minimum of 40 spawns.

That splits each launch into exec+dyld (spawn → first dylib initializer, before any of Node's static constructors), in-process, exit handlers, and kernel teardown:

| Runtime, `-e 0` / `eval 0` | exec+dyld | in-process | exit handlers | teardown |
|---|---|---|---|---|
| node v26.7.0 | 18.3 ms | 17.0 ms | 0.3 ms | 0.9 ms |
| deno 2.9.0 | 7.4 ms | 11.9 ms | 0.0 ms | 1.1 ms |
| bun 1.3.14 | 7.8 ms | 8.6 ms | 0.0 ms | 1.0 ms |
| a trivial C binary | 4.5 ms | 0.1 ms | | 0.3 ms |
| the same, linked to CoreFoundation + Security + libc++ | 5.6 ms | 0.7 ms | | 0.4 ms |

**dyld weak-def coalescing is the 10 ms.** The v26.7 Mach-O carries `WEAK_DEFINES` and `BINDS_TO_WEAK`: 2311 weak-def exports (inline and template instantiations compiled at default visibility: 1348 from `node::`, 417 `std::__1` instantiations, 265 abseil C symbols, 134 ICU, 60 abseil, 43 ada, 32 V8 header inlines) and 2125 of its 2840 unique dyld imports are `weak-def-coalesce` lookups, each a search across the loaded images at every launch. Two synthetic binaries isolate the cost: a C++ executable with 2500 default-visibility inline functions spends 14.6 ms in exec+dyld; the same source built with `-fvisibility-inlines-hidden` spends 4.8 ms. A binary with 130,000 rebases spends 4.5 ms, the same as a trivial one, so fixup count is not a factor and neither is binary size. Deno 2.9 and Bun carry no weak defines (Deno 2.9 also dropped its CoreFoundation, Security, Foundation and Metal links; it now depends only on libSystem, libobjc and libiconv). V8's part of this was fixed in [#56275][pr56275]; the rest of the binary is what [#65526][pr65526] (open, 2026-08) applies the same flags to, and `-fvisibility-inlines-hidden` alone removes the weak defs while leaving every non-inline symbol exported for addons and embedders.

**`atexit()` costs 1.1 ms per call on macOS.** Apple's `atexit` calls `dladdr` to find the image that owns the handler, and `dladdr` scans the executable's `nlist` symbol table linearly. The shipped binary keeps 204,315 entries (145,496 of them local `t` symbols), and Node registers two handlers, `ResetStdio` in `PlatformInit` and OpenSSL's `OPENSSL_cleanup` during static initialization; the interposer measures the pair at 2.2–2.4 ms per run, half of `node --version`'s 4.6 ms in-process time. Deno registers none; Bun registers one against a 1-symbol table (0.003 ms). `strip -x` on the shipped binary (204K → 39K symbols, 144 → 107 MB) cuts in-process time by 4.6 ms on `-e 0` with no source change. [#65549][pr65549] (open, 2026-08) replaces `atexit(ResetStdio)` with a static destructor and passes `OPENSSL_INIT_NO_ATEXIT`; its lazy cipher-table half landed separately in [#65484][pr65484].

Node's launch also runs 657 dyld initializers (365 in Network.framework, 253 its own, plus the Swift runtime, SkyLight and CoreDisplay reached through the CoreFoundation/Security dependency tree) and dlopens CoreFoundation, libswiftCore, ColorSync and QuartzCore from Foundation's `_NSInitializePlatform`; Deno 2.9 runs 20 initializers and Bun 6. Measured as the trivial-binary delta above, that tree costs ~1–2 ms.

### The isolate on Linux: a prime search on every start

With the dynamic loader at ~0.2 ms, a Linux `perf` profile of 200 runs of the official v26.7 `node -e 0` puts its single largest symbol at **12.1% of samples in `v8::internal::HashSeed::InitializeRoots`**.

Another 2.8% sits in `detail::sprp`, a Miller–Rabin strong-probable-prime test, and `LD_DEBUG=statistics` counts only 3.6K relocations, so the loader is not the cost. A sweep of official linux-x64 binaries dates it:

| Node | V8 | `-e 0` min | `HashSeed::InitializeRoots` share |
|---|---|---|---|
| v24.14.0 | 13.6 | 18.6 ms | absent |
| v25.9.0 | 14.1 | 20.6 ms | 13.4% |
| v26.0.0 | 14.6 | 20.5 ms | 12.6% |
| v26.3.0 | 14.6 | 20.7 ms | 11.9% |
| v26.7.0 | 14.6 | 19.6 ms | 12.1% |

The mechanism is in `deps/v8/src/numbers/hash-seed.cc`. Since V8 14.1 the string hash is rapidhash, whose secret is three 64-bit words that must each have balanced bytes, pairwise Hamming distance 32, and be prime ("a feeling to be perfect", per the wyhash author quoted in `third_party/rapidhash-v8/secret.h`). `HashSeed::InitializeRoots` either copies a compile-time default secret and stores a fresh random meta seed, when `V8_USE_DEFAULT_HASHER_SECRET` is defined, or calls `rapidhash_make_secret(seed, …)`, which rejection-samples the three primes with a 12-base Miller–Rabin test. V8's `BUILD.gn` sets `v8_use_default_hasher_secret = true`, so Chrome takes the first path; Node's gyp files (`tools/v8_gypfiles/features.gypi`, `common.gypi`) never pass the define, so every Node isolate, main thread and each `worker_threads` Worker alike, takes the second. The code's own comment estimates the generation at ~200 µs; measured, it is 2–3 ms of a 20 ms process.

Adding `'V8_USE_DEFAULT_HASHER_SECRET=1'` to the `defines` in `tools/v8_gypfiles/features.gypi` (one line) and rebuilding main (`53234233acb`, gcc 13, `./configure --ninja`) on the same box, paired in one hyperfine session with the unpatched build:

| `node -e 0`, Linux x64 | min | median |
|---|---|---|
| main | 21.7 ms | 25.4 ms |
| main + `V8_USE_DEFAULT_HASHER_SECRET=1` | 19.3 ms | 20.5 ms |

`performance.nodeTiming`'s `v8Start → environment` span drops from 8.4 to 6.4 ms, `HashSeed::InitializeRoots` and `sprp` leave the profile (the top symbol becomes the snapshot deserializer at 8.4%), and `test/parallel/test-hash-seed.mjs` passes, since the per-process meta seed stays random; only the multiplier secret becomes the fixed default that Chrome already uses. Both `-fvisibility` variants of [#65526][pr65526] were built on the same box for comparison and change nothing on Linux (19.9 / 20.5 / 21.4 ms are within noise), which is expected: `-fvisibility=hidden` there shrinks `.dynsym` from 208,643 to 186,791 entries and weak symbols from 14,135 to 869, but glibc's loader was never the cost.

### The three fixes built on macOS

Six builds of Node main (`53234233acb`) on macOS 15 arm64 runners, benchmarked interleaved on one Mac, confirm it: the visibility flags empty the dyld bucket, the `atexit` change saves 2 ms in-process, and the hasher define trims the isolate.

Every number below comes from the same session on a host under load ~40, so absolute values are inflated and only the differences matter (hyperfine minimum / median of 40 runs; the phase split is the interposer minimum of 40 spawns):

| Variant | weak-coalesce imports | exec+dyld | in-process | `--version` | `-e 0` | `hello.js` |
|---|---|---|---|---|---|---|
| main | 2305 | 17.6 ms | 14.3 ms | 18.6 / 20.8 | 30.6 / 34.5 | 32.5 / 35.7 |
| main + `atexit` change ([#65549][pr65549], `node.cc` half) | 2305 | 17.9 ms | 12.0 ms | 15.9 / 18.8 | 27.7 / 31.7 | 29.8 / 31.8 |
| main + `-fvisibility-inlines-hidden` only | 985 | 12.5 ms | 14.2 ms | 12.7 / 15.1 | 25.9 / 29.3 | 26.1 / 30.1 |
| main + [#65526][pr65526] (`-fvisibility=hidden` too) | 18 | 8.5 ms | 14.8 ms | 8.6 / 11.1 | 21.7 / 25.5 | 22.4 / 25.3 |
| main + #65526 + `atexit` change | 18 | 7.9 ms | 12.1 ms | 7.1 / 9.9 | 18.5 / 22.1 | 20.7 / 23.3 |
| main + `V8_USE_DEFAULT_HASHER_SECRET=1` | 2305 | 18.0 ms | 13.1 ms | 19.6 / 21.8 | 29.8 / 32.9 | 30.7 / 33.1 |

What the rows say:

- `-fvisibility-inlines-hidden` alone removes 57% of the weak-coalesce imports and ~5 ms of exec+dyld; the 985 that survive are non-inline weak definitions (415 libc++ template instantiations, 354 in `node::`, 196 abseil C symbols such as `AbslInternalPerThreadSemPost`, 134 ICU, 54 abseil, 34 ada), which only `-fvisibility=hidden` reaches. The full [#65526][pr65526] leaves 18 and brings exec+dyld to 8.5 ms, level with Deno 2.9 and Bun on the same box.
- The `atexit` change is worth 2.0–2.3 ms in-process in every pairing (the interposer counts the registrations at 0), independent of the visibility flags.
- The hasher define shows up where it should: the in-process bucket drops 1.2 ms and the runner's `performance.nodeTiming` `v8Start → environment` span goes from 5.2 to 3.7 ms, the macOS counterpart of the Linux 8.4 → 6.4.
- Together, [#65526][pr65526] plus the `atexit` change take `node --version` from 18.6 to 7.1 ms and `node -e 0` from 30.6 to 18.5 ms on this host, 40% off; the hasher define's ~1.2 ms lands in a phase neither of them touches. In the same session `deno run hello.js` (2.9.0) took 11.6 ms and `bun hello.js` 11.8 ms against the patched Node's 20.7 ms, so the remaining gap is the in-process 12 ms (isolate and snapshot deserialization, then the bootstrap chain), no longer anything before `main`.

## TL;DR

A warm `node ./hello.js` on macOS arm64 today spends its ~15–27 ms roughly like this (apportioned from flamegraphs and `--no-node-snapshot` A/B numbers in [nodejs/performance#180][perf180]):

| Phase | Approx. share | Notes |
|---|---|---|
| `dyld` + image load + global ctors (incl. weak-symbol fixups) | 4–6 ms | macOS-specific; was 8 ms worse before [#56275][pr56275] (already landed in v24) |
| `node::InitializeOncePerProcess` (OpenSSL, cppgc, ncrypto, ICU register) | 3–4 ms | dominated by `OPENSSL_init_crypto` and `cppgc::InitializeProcess` |
| `NodeMainInstance` ctor (Isolate + 4 context snapshots deserialize) | 3–4 ms | this is the V8 startup snapshot fast path |
| Bootstrap JS (`internal/process/pre_execution.js`, ESM loader init, source-map cache, diagnostics_channel, etc.) | 2–3 ms | a lot of "setup X" calls that run even when unused |
| Compile + run user file, CJS loader, module resolution | 0.5–1 ms | the actual work |
| `__cxa_atexit` / teardown (counted by hyperfine) | 1–2 ms | non-trivial on macOS |

Takeaways:

1. **Most of the 15 ms is not "Node bootstrap" in the JS sense.** It's `dyld`, libc++ template fixups, OpenSSL one-time init, V8 isolate construction, and snapshot deserialization — all C++ that runs before a single line of `lib/internal/bootstrap/node.js` executes.
2. **The snapshot is doing its job.** Without it, empty-script startup on Linux is ~53 ms vs ~18 ms with it (the 2021 builtins-snapshot work, ~3× speedup, per the PR description on [#27321][pr27321]). The remaining 15 ms is what's left _after_ the biggest already-fixed lever.
3. **Bun's headline win is mostly that it links statically (no dyld weak-symbol fixups), uses JSC instead of V8 (smaller engine init, smaller snapshot footprint), and skips OpenSSL on macOS in favor of BoringSSL/Apple frameworks.** That is, the gap is mostly _under_ Node's main(), not above it.

## Sources of cost, itemized

Six items, ordered by cost: dyld and global C++ constructors, one-time process init, isolate construction plus snapshot deserialize, the bootstrap JS that survives the snapshot, module resolution and the user script, then teardown.

### 1. `dyld` and global C++ constructors (macOS-only tax)

The single biggest macOS-specific cost, and the one with a proper post-mortem in the PR description on [#56275][pr56275].

Between v20 and v23, macOS startup regressed from ~19 ms to ~30 ms. Root cause: V8's upgrade 11.3 → 11.8 added many templated `StaticCallInterfaceDescriptor` instantiations, and without `-fvisibility=hidden` on the V8 build those templates produced weak symbols that `dyld` resolved at process start. From the bench in [nodejs/performance#180][perf180]:

> "DYLD_PRINT_BINDINGS=1 ./node --version 2>&1 | grep 'looking for weak-def symbol' | wc -l: 7317" versus the fixed build: "1755"

Fix: add `-fvisibility=hidden` (plus `BUILDING_V8_SHARED`) to V8's gypfiles. Result: **2.33× faster startup on macOS arm64 (28.9 ms → 12.4 ms), binary 10 MB smaller (118 → 108 MB)**, landed in [#56275][pr56275] (Dec 2024, in v23.7/v22.13). V8's own `node-ci` fork did not have the regression because Chromium's build always sets `-fvisibility=hidden`; that contrast is how the regression was located.

**This fix is in v24, so the local 27 ms baseline already reflects it.**

Even with the fix in, `dyld` is still the largest single contributor on macOS. From `otool -L node`:

```
/System/Library/Frameworks/CoreFoundation.framework/.../CoreFoundation
/usr/lib/libSystem.B.dylib
/usr/lib/libc++.1.dylib
```

A proposal to remove CoreFoundation ([#44715][pr44715]) was closed unactioned because ICU pulls it in. From Daniel Lemire in [#180][perf180]:

> "One maybe significant difference between bun and node is that node depends on Core Foundation whereas bun does not appear to do so... So I am back at my theory: this is a case where dynamic linking and rebasing is expensive under macOS for some reason."

There is no equivalent post-mortem for Linux, where startup stayed roughly flat across versions (Lemire's measurements on Linux i5: node 17.9 → 21.8 ms across the same versions — actually _improving_).

### 2. `node::InitializeOncePerProcess` (OpenSSL, cppgc, ICU)

Source: [`src/node.cc`][nodecc] `InitializeOncePerProcessInternal`. The flamegraph diff in [#180][perf180] (billywhizz) put this at **~30% of v22 wall time** (6.5 ms of the 16 ms regression). Sub-costs:

- `OPENSSL_init_crypto` — ~6.5% of total. Building with `./configure --without-ssl` drops about 3.5 ms in A/B (35.4 → 26.9 ms). Node forks [quictls/openssl][quictls] and links it statically; OpenSSL v3's provider model is heavier than 1.1 was.
- `cppgc::InitializeProcess` — ~2.5%, added between v20 and v22 when V8's cppgc became a hard dependency. No tracking issue for removal.
- `ncrypto::CSPRNG` — ~2.5%, the seed gather for `crypto.randomUUID` etc. (Separately, [#59550][pr59550] moved _system CA_ loading off-thread — saved ~48 ms on first TLS context, 57 → 8.5 ms — but this is a TLS warmup fix, not a startup fix.)
- ICU registration — `--without-intl` saves ~30 MB binary but "didn't seem to make a difference" to startup ([#180][perf180]).

### 3. V8 isolate construction + snapshot deserialize

Source: `node::NodeMainInstance::NodeMainInstance` constructor. The flamegraph attributes ~26% of wall time to it in both fast and slow builds — i.e. constant, not regressed. So roughly **3.5 ms** at v23 macOS.

What gets deserialized is documented in [`tools/snapshot/README.md`][snapREADME]: one isolate snapshot plus **four** context snapshots:

> 1. The default context snapshot ... 2. The vm context snapshot ... 3. The base context snapshot ... 4. The main context snapshot ... captures initializations done by `node::CommonEnvironmentSetup::CreateForSnapshotting()`, most notably `node::CreateEnvironment()`, which runs the following scripts via `node::Realm::RunBootstrapping()` for the main context as a principal realm, so that at runtime, these scripts do not need to be run. Instead only the context initialized by them is deserialized at runtime. 1. `internal/bootstrap/realm` 2. `internal/bootstrap/node` 3. `internal/bootstrap/web/exposed-wildcard` 4. `internal/bootstrap/web/exposed-window-or-worker` 5. `internal/bootstrap/switches/is_main_thread` 6. `internal/bootstrap/switches/does_own_process_state`

That's the only JS that doesn't run at runtime in a default build. As of Feb 2026, the ESM loader joined them via [#61769][pr61769]; before that it was being initialized from scratch on every start.

Snapshot compression was disabled by default in [#45716][pr45716] — +2.7 MB binary in exchange for **9–18% faster startup**, validating that decompression was on the hot path.

### 4. Bootstrap JS that still runs on every start

The post-snapshot JS bootstrap lives in [`lib/internal/process/pre_execution.js`][preExec] (26 KB), a sequential `setup…()` chain:

```
patchProcessObject     setupTraceCategoryState     setupInspectorHooks
setupNetworkInspection setupNavigator              setupWarningHandler
setupFFI               setupSQLite                 setupStreamIter
setupQuic              setupWebStorage             setupWebsocket
setupEventsource       setupCodeCoverage           setupDebugEnv
initializeReport       setupDiagnosticsChannel     initializePermission
initializeSourceMapsHandlers                       initializeDeprecations
initializeConfigFileSupport                        initializeDns
setupStacktracePrinterOnSigint                     initializeReportSignalHandlers
initializeHeapSnapshotSignalHandlers               setupChildProcessIpcChannel
initializeClusterIPC                               initializeExtensionFormatMap
setupVmModules                                     initializeModuleLoaders
setupHttpProxy
```

Every one runs on every hello-world. [#45659][pr45659] ("bootstrap: lazy load non-essential modules", merged Dec 2022) was the big project to push this back. From the PR description:

> "It turns out that even with startup snapshots, there is a non-trivial overhead for loading internal modules. This patch makes the loading of the non-essential modules lazy again."

Result: **~17% faster basic startup, ~37% faster worker startup**, at the cost of 5–10% on apps that need every builtin. The PR primarily recovered a regression rather than going below v14.

Follow-ups still landing in 2025–26: [#62267][pr62267] lazy source-map cache in the CJS loader; [#59517][pr59517] lazy `internal/tty` in tests; [#56980][pr56980] lazy modules in test runner; [#57307][pr57307] `fs.getLazy`; [#59473][pr59473] simdjson for `--snapshot-config`. Pattern: each PR shaves micro/single-digit milliseconds; nobody has the lever to drop 5+ ms in one PR anymore.

### 5. Module resolution and the actual user script

Almost free. From the discussion in [#180][perf180]:

> "in recent versions of Node.js no internal JS code is compiled at all when executing a CJS script, that's because we moved the internal JS code compilation into build time and serialized the bytecode into the snapshot."

For ESM (`hello.mjs`), the loader is now snapshotted as of [#61769][pr61769] (Feb 2026) but only just merged. From the PR description:

> "empty/minimal CJS startup is now slightly slower in worker but other metrics get a slight boost (because they all incur ESM loader initialization). In reality ESM loading is likely to happen at some point in the lifetime of an application especially with the growing adoption of ESM and `require(esm)`."

This explains the ~2 ms gap between `hello.cjs` (27.8 ms) and `hello.mjs` (29.4 ms) in our local bench — and that gap should narrow once #61769 propagates.

### 6. Teardown (counted by hyperfine)

Teardown is not startup, but every hyperfine number quoted in this doc includes it.

billywhizz, [#180][perf180]: "the small traces on left and right of the graph are system code being run when tearing down the process and take ~6% of time in the fast instance and ~12% of time in the slow instance" — roughly **1–2 ms** of `__cxa_atexit`/`Isolate::Delete`. Hyperfine includes this; it is not strictly "startup" but is in every benchmark quoted here.

## What Node has already done

Timeline of landmark startup work, all from [#35711][issue35711] ("Tracking issue: snapshot integration in Node.js core", open since 2020):

| When | PR | What |
|---|---|---|
| v12.5 (2019) | [#27321][pr27321] / [#28181][pr28181] | First V8 startup snapshot landed |
| 2021 | `tools/js2c.py` | Internal JS encoded into C++ arrays at build time, V8 code cache pre-compiled into `node_code_cache.cc`, bootstrap context serialized into `node_snapshot.cc`. "Node.js does not need to execute this part of the bootstrap at all." |
| v19.6 | [#42466][pr42466] | Build-time user-land snapshot via `--node-snapshot-main`; foundation for SEA |
| v19.6 | [#45659][pr45659] | Bootstrap lazy-loads non-essential modules — **+17% startup** |
| v19.6 | [#45716][pr45716] | Disable snapshot compression by default — **+9–18% startup, +2.7 MB binary** |
| v23.7 / v22.13 | [#56275][pr56275] | `-fvisibility=hidden` for V8 on macOS — **+2.33× macOS startup, –10 MB binary** |
| 2025 | [#59550][pr59550] | System-CA load off the main thread — first TLS context 57 → 8.5 ms |
| 2026-02 | [#61769][pr61769] | ESM loader baked into the built-in snapshot |
| ongoing | many | `getLazy()` retrofits across `fs`, source-map cache, test runner, internal/tty |

For runtime user-land snapshotting, [#44014][issue44014] is the open tracking issue, gated on V8 not supporting a long list of types in run-time-built snapshots. The build-time path is what powers SEA (Single Executable Applications), and the integration story for packagers is still cumbersome ([#42566][pr42566]).

## What's still on the table upstream

Ten items are open or unowned: the V8 hasher define, the macOS weak defs and `atexit` registrations, user-land snapshots, OpenSSL and cppgc init, config-file probing, the CoreFoundation dependency, the bootstrap chain, and options parsing.

- **`V8_USE_DEFAULT_HASHER_SECRET` in Node's V8 build** — V8's own default since rapidhash landed in 14.1, omitted by Node's gyp files; measured above at 2–3 ms of every isolate start on Node 25+, and a one-line change in `tools/v8_gypfiles/features.gypi`.
- **Default-visibility weak defs in the macOS binary** — [#65526][pr65526], open; the interposer numbers above put dyld's coalescing of them at ~10 ms of the 18 ms exec+dyld bucket.
- **`atexit()` on macOS** — [#65549][pr65549], open; 2.2 ms for the two registrations, because Apple's `atexit` scans the 204K-entry symbol table via `dladdr`.
- **Run-time snapshots for arbitrary user code** — [#44014][issue44014], open since 2022. Blocked on V8 supporting more embedder types outside build-time snapshots.
- **Macro-level OpenSSL init cost** — no tracking issue; the `OPENSSL_init_crypto` line is the largest single C++ frame still visible. Bun avoids it by using BoringSSL.
- **`cppgc::InitializeProcess`** — added by V8 upgrade; no Node-side fix being worked.
- **Config-file initialization** ([#53787][issue53787]) is adjacent: loading a config file by default "adds overhead to the startup to probe the file system." Same problem will face any package.json-based hooks config; the lean is on a field _inside package.json_ rather than a new dotfile.
- **CoreFoundation dependency on macOS** — [#44715][pr44715] proposed removal; closed without action because ICU pulls it in.
- **Single-pass bootstrap JS** — `pre_execution.js` is still a long imperative chain. Each `setup…` adds μs; together they're a couple of ms. There are `TODO: move this to vm.js?` markers in the source suggesting more lazification is wanted.
- **Run-time options parsing in JS** — `refreshRuntimeOptions()` runs every start. [#59473][pr59473] moved snapshot-config parsing to simdjson; the full options system is still JS.

## Why hasn't Node done this already?

Three reasons: part of the work already shipped, most of the rest is landing slowly behind compat guarantees, and what remains is held by distro-packaging, FIPS and embedder promises.

1. **Some they did.** `-fvisibility=hidden` shipped in [#56275][pr56275] (Dec 2024). The 16 ms reclaim is _already in the v24 baseline_.

2. **Most of the rest, Node is doing — slowly.** The `getLazy()` PR train ([#45659][pr45659] and follow-ups) has been clawing back `pre_execution.js` overhead for three years and is maybe halfway. Each `setup{Inspector, Permission, DiagnosticsChannel, …}` call has subtle ordering guarantees: `process.on('warning')` listeners installed by user code must fire if a warning is emitted by another setup; the permission model must be live before any fs/net access; diagnostics channels must precede async_hooks. Each step has to land behind tests and a release cycle, so they cannot take the cut in one swing. A from-scratch runtime can, in exchange for accepting compat risk on userland that introspects globals before touching them.

3. **Some they can't do without breaking promises.**
   - **Static linking**: Debian/Fedora packaging policy forbids it — distros want to swap OpenSSL for CVEs without rebuilding Node. Bun ships static because it's distributed direct from `bun.sh/install`.
   - **Narrower snapshot**: Node's snapshot is shared across `node`, `--eval`, `vm.Script`, workers. Specializing it means either binary bloat (multiple snapshots) or build-at-install — and the productized "build-at-install snapshot" already exists as SEA, which is opt-in. Changing the default breaks embedders.
   - **Lazy OpenSSL init**: tracked off and on, stalled on FIPS mode (must be configured pre-crypto), eager `globalThis.crypto` (Web Crypto is a spec-visible global), and OpenSSL thread callbacks needing to be installed before any worker spawns. Not impossible, but the Node team has chosen predictability over ~3 ms.

## Third-party analysis (quoted from nodejs/performance#180)

From Daniel Lemire (TSC, perf-focused), in [#180][perf180]:

> "10 ms is quite a large effect. Especially if you account for the fact that bun can print 'hello' in 5 ms."

> "Bun is 58 MB and it runs in about 7 ms on my mac with the same benchmark. One maybe significant difference between bun and node is that node depends on Core Foundation whereas bun does not appear to do so..."

> "So I am back at my theory: this is a case where dynamic linking and rebasing is expensive under macOS for some reason."

From billywhizz (independent runtime author of `lo` / `just-js`), [#180][perf180], after the M3 Max flamegraph diff:

> "most of the overhead in InitializeOncePerProcessInternal seems to be coming from OpenSSL initialization. Most of the overhead in NodeMainInstance::NodeMainInstance constructor seems to be coming from v8 snapshot initialization."

> "bun is crazy fast for a micro bench like this. i think is more to do with JSC than anything. i have tried building a minimal runtime on v8 for macos and best i can do is ~15 ms, which is roughly same as deno. while bun on same hardware is ~7 ms. on linux the situation is the opposite ime — bun/JSC is almost 2x slower than a minimal v8 runtime."

**JSC vs V8 is a big lever on macOS specifically**, not in general — JSC is not a free lunch cross-platform.

From Geoffrey Booth (ESM lead), on config-file proposals in [#53787][issue53787]:

> "The recent .env file support was meant as a bridge to this; that effort got us the ability to parse JSON files without needing to start V8."

From isaacs (npm originator), [#53787][issue53787]:

> "We all hate json. No comments, excessive quoting, no multi line strings, no trailing commas, etc. But: it's specified very clearly (unlike ini, which is not specified at all); it's built into the language; It's FAST, like, omg wow, much faster than yaml or toml, not even close."

## Sources

Every number above comes from the nodejs/performance startup thread, one of the landmark PRs listed in the timeline, or Node's own snapshot README; the link definitions below resolve those references.

[perf180]: https://github.com/nodejs/performance/issues/180
[pr56275]: https://github.com/nodejs/node/pull/56275 [pr65526]: https://github.com/nodejs/node/pull/65526 [pr65549]: https://github.com/nodejs/node/pull/65549 [pr65484]: https://github.com/nodejs/node/pull/65484
[pr45659]: https://github.com/nodejs/node/pull/45659
[pr45716]: https://github.com/nodejs/node/pull/45716
[pr42466]: https://github.com/nodejs/node/pull/42466
[pr59550]: https://github.com/nodejs/node/pull/59550
[pr61769]: https://github.com/nodejs/node/pull/61769
[pr59473]: https://github.com/nodejs/node/pull/59473
[pr27321]: https://github.com/nodejs/node/pull/27321
[pr28181]: https://github.com/nodejs/node/pull/28181
[pr44715]: https://github.com/nodejs/node/issues/44715
[pr62267]: https://github.com/nodejs/node/pull/62267
[pr59517]: https://github.com/nodejs/node/pull/59517
[pr56980]: https://github.com/nodejs/node/pull/56980
[pr57307]: https://github.com/nodejs/node/pull/57307
[pr42566]: https://github.com/nodejs/node/issues/42566
[issue35711]: https://github.com/nodejs/node/issues/35711
[issue44014]: https://github.com/nodejs/node/issues/44014
[issue53787]: https://github.com/nodejs/node/issues/53787
[snapREADME]: https://github.com/nodejs/node/blob/main/tools/snapshot/README.md
[preExec]: https://github.com/nodejs/node/blob/main/lib/internal/process/pre_execution.js
[nodecc]: https://github.com/nodejs/node/blob/main/src/node.cc
[quictls]: https://github.com/quictls/openssl

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Initial publication.
- 2026-08-28 — Trimmed to the measured findings and current behavior.
- 2026-09-04 — Added the v26.7 decomposition against Deno 2.9 and Bun on macOS and Linux: dyld weak-def coalescing and `atexit`/`dladdr` on macOS, and V8's per-isolate rapidhash prime search on Node 25+ (absent from Node 24), with the `V8_USE_DEFAULT_HASHER_SECRET=1` build verified on Linux.
- 2026-09-04 — Added the macOS build verification: six variants of Node main built on macOS 15 arm64 runners and benchmarked interleaved on one host, confirming the weak-def, `atexit` and hasher attributions and putting the patched Node at 20.7 ms `hello.js` against Deno 2.9 at 11.6 ms.
