# macOS filesystem write-confine probes (build-jail)

Ground-truth validation of the macOS Seatbelt (SBPL) **write-confine** mechanism nub's
build-jail relies on: confine a dependency lifecycle script's filesystem **writes** to its
own package dir (+ a private scratch tmp), leaving reads/exec broad so the toolchain works.
The companion axis to the Windows probes in [`../sandbox-win-probes/`](../sandbox-win-probes/).
The question: does strict "write only your own package dir" break the out-of-package writes
native builds make, what exact path does each need, and does a single base-anchor repoint
collapse the cache-allowlist toward one dir.

Mirrors the Windows-probe shape: a self-contained harness + a `results.md` record. No cargo
build needed — the harness is a Node profile generator + a bash runner, runnable on any macOS.

## Harness

| File | Role |
|---|---|
| `gen-profile.mjs` | SBPL write-confine profile generator. Canonicalizes every write-allow path (the load-bearing correctness rule — see `results.md` §Canon). `--mode strict` = pkg + tmp only; `--mode relaxed` = + the `--write` cache allowlist; `--darwin-temp` grants the Apple-toolchain confstr scratch. |
| `gen-profile.test.mjs` | Generator unit tests (`node gen-profile.test.mjs`) — canonicalization, write-deny floor, device set, strict/relaxed gating, SBPL escaping, darwin-temp. |
| `jail-run.sh` | Wraps a command in `sandbox-exec -p <profile>` with cwd = the package dir and the npm-lifecycle env (PATH incl. the tree `.bin`, `INIT_CWD`, `npm_config_*`, tmp anchors repointed at the granted scratch). Captures exit code + parsed denied write paths. `--mode control` runs un-sandboxed (the baseline). |
| `smoke-test.sh` | Self-contained enforcement + bypass smoke suite (no fixture, no network) — the fast CI core. `./smoke-test.sh`. |
| `results.md` | The findings: bypass/correctness analysis, the holes catalog per cache-family, the minimal cache-allowlist, the base-anchor collapse verdict, and the optimization analysis. |

CI: `.github/workflows/sandbox-macos-writeconfine.yml` runs the unit tests + smoke suite on
`macos-latest` (mirror of `sandbox-win-probes.yml`); the heavy per-family runs are reproduced on
demand, not in CI.

## The experiment (per package / cache-family)

Three passes over the same build, so a sandbox FAIL is attributable to write-confine, not a
broken package:

1. **control** — un-sandboxed; confirms the build works on this machine at all.
2. **strict** — pkg dir + private tmp only. The write-confine floor. A denial here is a *hole*.
3. **relaxed** — + the candidate cache path(s) via `--write`. Confirms the hole closes with
   exactly that grant (and records which path it was).

## Reproducing (the proven loop)

Built against a mega-fixture: the Bun-trusted list (~365, pruned dead `@softvisio/core` /
`webdev-toolkit`) + the major frameworks, installed with `pnpm install --ignore-scripts` (pnpm
links transitive bins so node-gyp / prebuild-install / node-pre-gyp resolve). A modern
`node-gyp` (v13) is pinned in a tools dir because the host's global node-gyp may be ancient.

```sh
# locate a package's real dir in the pnpm virtual store
PKGDIR=$(echo "$NM"/.pnpm/better-sqlite3@*/node_modules/better-sqlite3 | head -1)
PROJ="$(dirname "$(dirname "$PKGDIR")")"   # the .pnpm/<pkg@ver> dir — its node_modules/.bin
export PATH="$TOOLS/node_modules/.bin:$PATH"   # modern node-gyp

# strict — expect the out-of-pkg cache write to be DENIED (the hole)
DEVDIR=$(mktemp -d)
./jail-run.sh --pkg "$PKGDIR" --project "$PROJ" --mode strict \
  -- node-gyp rebuild --release --devdir="$DEVDIR"

# relaxed — grant the cache path, expect the build to COMPLETE
./jail-run.sh --pkg "$PKGDIR" --project "$PROJ" --mode relaxed --write "$DEVDIR" \
  -- node-gyp rebuild --release --devdir="$DEVDIR"
```

## The base-anchor collapse (the optimization the cache-allowlist hinges on)

Per [`wiki/research/native-build-cache-paths.md`](../../wiki/research/native-build-cache-paths.md),
every cache path derives from a tiny set of base anchors — chiefly `os.homedir()`. Repointing
`HOME` (and `npm_config_cache`, `TMPDIR`) at ONE nub-owned writable cache root lands every
convention-following tool's cache inside that root, so the cache-allowlist collapses from N
per-tool dirs to a single granted root. `jail-run.sh` already repoints the tmp anchors; the
HOME-repoint experiment is recorded in `results.md` §Collapse.
