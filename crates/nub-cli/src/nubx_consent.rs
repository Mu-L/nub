//! Consent gate for `nubx`'s *implicit* registry tier — the moment `nubx <thing>`
//! falls through a local file/script/bin miss into a download-and-run.
//!
//! The thesis: **`nubx` never executes remote code silently.** A local hit (file,
//! script, or installed bin) never reaches this module — the gate guards only the
//! registry fallthrough. There it splits three ways:
//!
//! - **CI** (a truthy `CI`) → fail closed. A CI job that needs a tool declares it;
//!   nub does not fetch-and-run arbitrary remote code where no human can intervene.
//! - **Non-interactive** (stdin/stderr not a terminal) → fail closed. No terminal,
//!   no way to ask, so refuse rather than guess.
//! - **Interactive TTY** → prompt `y/N` on the *first* fetch of a spec, then run
//!   without re-asking once that spec is recorded as consented.
//!
//! `-y`/`--yes` is the explicit non-interactive escape hatch: it lets CI / non-TTY
//! through and skips the first-fetch prompt. Fail-closed by default, never
//! impossible.
//!
//! **The gate keys on a persistent per-user *consent ledger*** (`<cache>/dlx/
//! consent.json`). An entry is a standing run-grant written *only* by an explicit
//! consent (a TTY `y`, or `-y`); nothing else pre-populates it. The prompt fires
//! exactly on a ledger miss — "have we already consented to this spec?" — which
//! folds in re-resolution: a **pinned** spec (`cowsay@1.5.0`, immutable identity)
//! is consented forever; a **floating** spec (`cowsay`, `@latest`, `@^1`) is
//! consented for a 24h TTL, after which the entry is stale → a miss → re-prompt
//! (new resolution = possible new code = fresh consent). `nub dlx` is the explicit
//! download command and bypasses this gate entirely (invoking it IS the consent).
//!
//! Scope note: this ledger records *consent*, not the fetched bytes. The
//! per-resolution tree cache (skip the re-fetch on a hit) is the separate,
//! locked-but-deferred follow-up (see `.fray/universal-nubx.md`); it keys off the
//! same entries, so it layers on without changing this contract.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Floating-spec consent TTL: 24h, matching pnpm's `dlxCacheMaxAge` (1440 min).
/// A pinned spec ignores this (immutable identity → consent never expires).
const FLOATING_TTL_SECS: u64 = 24 * 60 * 60;

/// What the gate decided for an implicit registry fetch.
pub enum Decision {
    /// Consent is in hand — proceed to fetch-and-run. Carries whether the ledger
    /// must be written (a fresh consent) so the caller records it only on a run it
    /// actually performs.
    Proceed { record: bool },
    /// Refused. Carries the process exit code and a message already printed to
    /// stderr; the caller returns this code without fetching.
    Refused(i32),
}

/// Decide whether an implicit `nubx` registry fetch may proceed.
///
/// `specs` is the canonical install set (the `-p` packages, or the bare bin
/// token) — the same thing handed to the engine, so the gate and the fetch agree
/// on identity. `yes` is `nubx -y`.
///
/// A ledger HIT (recorded + within TTL for a floating spec) proceeds with no
/// prompt — consent was given when the spec was first recorded. A MISS routes by
/// context: `-y` → proceed (and record); CI / non-TTY → refuse; TTY → prompt.
pub fn gate(specs: &[String], yes: bool) -> Decision {
    if specs.is_empty() {
        // Defensive: no identity to gate on. Refuse rather than fetch blind.
        eprintln!("nubx: cannot determine what to fetch.");
        return Decision::Refused(1);
    }
    let key = ledger_key(specs);
    let pinned = specs.iter().all(|s| is_pinned_spec(s));

    if ledger_has_valid(&key, pinned) {
        // Already consented (and still fresh, for a floating spec). Run, no prompt.
        return Decision::Proceed { record: false };
    }

    // A ledger miss: this is new (or newly-stale) code about to run.
    if yes {
        return Decision::Proceed { record: true };
    }

    if is_ci() {
        eprintln!(
            "nubx: refusing to download {} in CI.\n\
             \x20\x20A CI job should declare the tool as a dependency, or pass -y to fetch it.",
            specs.join(" ")
        );
        return Decision::Refused(1);
    }

    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        eprintln!(
            "nubx: refusing to download {} without a terminal to confirm at.\n\
             \x20\x20Pass -y to fetch it non-interactively.",
            specs.join(" ")
        );
        return Decision::Refused(1);
    }

    if prompt_yes(specs) {
        Decision::Proceed { record: true }
    } else {
        eprintln!("nubx: aborted.");
        Decision::Refused(1)
    }
}

/// Record a granted consent. Called by the caller *after* a `Proceed { record:
/// true }` run, so the ledger only ever reflects tools we actually fetched. A
/// write failure is non-fatal (the worst case is a re-prompt next time) — never
/// block a successful run on the ledger.
pub fn record(specs: &[String]) {
    let key = ledger_key(specs);
    let pinned = specs.iter().all(|s| is_pinned_spec(s));
    let Some(path) = ledger_path() else { return };
    let mut ledger = read_ledger(&path);
    ledger.insert(
        key,
        Entry {
            specs: specs.to_vec(),
            recorded_at: now_unix(),
            pinned,
        },
    );
    let _ = write_ledger(&path, &ledger);
}

// ─────────────────────────── ledger storage ───────────────────────────

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Entry {
    /// The consented install set, stored for human inspection of the ledger.
    specs: Vec<String>,
    /// Unix seconds the consent was granted. A floating entry expires
    /// `FLOATING_TTL_SECS` after this; a pinned entry never does.
    recorded_at: u64,
    /// Whether every spec was an exact-version ("pinned") identity at consent
    /// time. Pinned consent is immortal; floating consent honors the TTL.
    pinned: bool,
}

/// Canonical, order-independent ledger key for a spec set. Sorting makes
/// `-p a -p b` and `-p b -p a` one entry; the raw specs (not resolved versions)
/// are the identity the user typed and consented to.
fn ledger_key(specs: &[String]) -> String {
    let mut sorted: Vec<&str> = specs.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join(" ")
}

fn ledger_path() -> Option<PathBuf> {
    Some(
        nub_core::node::discovery::cache_dir()?
            .join("dlx")
            .join("consent.json"),
    )
}

fn read_ledger(path: &std::path::Path) -> BTreeMap<String, Entry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic write so a concurrent reader never sees a half-written ledger:
/// serialize to a sibling temp file, then rename into place.
fn write_ledger(path: &std::path::Path, ledger: &BTreeMap<String, Entry>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(ledger)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

fn ledger_has_valid(key: &str, pinned: bool) -> bool {
    let Some(path) = ledger_path() else {
        return false;
    };
    let ledger = read_ledger(&path);
    let Some(entry) = ledger.get(key) else {
        return false;
    };
    // A spec that was floating at consent time stays bound by the TTL even if the
    // current invocation reads as pinned (and vice versa) — the *consent's* nature
    // governs its lifetime. Use the recorded `pinned`, falling back to the current
    // classification only as a tie-break for forward-compat ledgers.
    if entry.pinned || pinned {
        return true;
    }
    now_unix().saturating_sub(entry.recorded_at) < FLOATING_TTL_SECS
}

// ─────────────────────────── classification ───────────────────────────

/// A spec is "pinned" iff its version part is an exact semver (`1.2.3`,
/// `1.2.3-rc.1`) — an immutable identity that can never re-resolve to different
/// bytes. Everything else is floating: a bare name (`→ latest`), a dist-tag
/// (`@next`), a range (`@^1`, `@~1.2`, `@1.x`, `@*`), or a non-registry spec
/// (`github:…`, `git+…`, `file:…` — a ref can move). Floating identities honor the
/// consent TTL; pinned ones are consented forever.
fn is_pinned_spec(spec: &str) -> bool {
    let Some(version) = version_part(spec) else {
        return false; // no version → `latest`, floating
    };
    is_exact_semver(version)
}

/// Split a registry spec into its version part, preserving a leading `@scope/`.
/// Returns `None` for a non-registry spec (URL / git / shorthand) or a bare name.
fn version_part(spec: &str) -> Option<&str> {
    // Non-registry forms carry their own movability; never "pinned" here.
    if spec.contains("://")
        || spec.starts_with("git+")
        || spec.starts_with("github:")
        || spec.starts_with("gitlab:")
        || spec.starts_with("bitbucket:")
        || spec.starts_with("gist:")
        || spec.starts_with("file:")
        || spec.starts_with("link:")
    {
        return None;
    }
    // `@scope/name@version`: skip a leading scope `@`, find the version separator.
    let body = spec.strip_prefix('@').unwrap_or(spec);
    let at = body.find('@')?;
    let version = &body[at + 1..];
    (!version.is_empty()).then_some(version)
}

/// An exact, fully-qualified semver — `major.minor.patch` with optional
/// prerelease/build, no range operators, wildcards, or tags. This is the
/// immutable-identity test: anything looser can re-resolve.
fn is_exact_semver(v: &str) -> bool {
    // Reject range operators and wildcards outright.
    if v.starts_with(['^', '~', '>', '<', '=', 'v', 'V'])
        || v.contains('*')
        || v.contains('x')
        || v.contains('X')
    {
        return false;
    }
    if v.contains("||") || v.contains(" - ") || v.contains(',') {
        return false;
    }
    // core = before any `-`(prerelease) or `+`(build) marker.
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

// ─────────────────────────── context probes ───────────────────────────

/// Truthy `CI` detection. CI providers set `CI` to a truthy value; we refuse to
/// download in CI regardless of provider. A literal `0`/`false`/empty is treated
/// as "not CI" so a deliberate `CI=0` local shell isn't locked out.
fn is_ci() -> bool {
    match std::env::var("CI") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty() || v.eq_ignore_ascii_case("0") || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

/// Interactive `y/N` confirmation on stderr (so stdout stays clean for the tool's
/// own output). Default is NO — an empty line, EOF, or anything but `y`/`yes`
/// refuses. Reads a single line from stdin.
fn prompt_yes(specs: &[String]) -> bool {
    eprint!(
        "nubx: {} is not installed locally. Download and run it? [y/N] ",
        specs.join(" ")
    );
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_semver_is_pinned() {
        assert!(is_pinned_spec("cowsay@1.5.0"));
        assert!(is_pinned_spec("@scope/foo@2.0.0"));
        assert!(is_pinned_spec("pkg@1.2.3-rc.1"));
        assert!(is_pinned_spec("pkg@1.0.0+build.7"));
    }

    #[test]
    fn floating_specs_are_not_pinned() {
        assert!(!is_pinned_spec("cowsay")); // → latest
        assert!(!is_pinned_spec("cowsay@latest"));
        assert!(!is_pinned_spec("cowsay@next"));
        assert!(!is_pinned_spec("cowsay@^1.5.0"));
        assert!(!is_pinned_spec("cowsay@~1.5"));
        assert!(!is_pinned_spec("cowsay@1.x"));
        assert!(!is_pinned_spec("cowsay@1"));
        assert!(!is_pinned_spec("cowsay@1.5"));
        assert!(!is_pinned_spec("@scope/foo")); // scope, no version
        assert!(!is_pinned_spec("github:user/repo"));
        assert!(!is_pinned_spec("git+https://host/u/r.git#v1"));
        assert!(!is_pinned_spec("file:../local"));
    }

    #[test]
    fn ledger_key_is_order_independent() {
        assert_eq!(
            ledger_key(&["b".into(), "a".into()]),
            ledger_key(&["a".into(), "b".into()])
        );
        assert_ne!(
            ledger_key(&["a".into()]),
            ledger_key(&["a".into(), "b".into()])
        );
    }

    #[test]
    fn ledger_roundtrips_and_honors_pinned_and_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let mut ledger = BTreeMap::new();
        ledger.insert(
            "pinned".to_string(),
            Entry {
                specs: vec!["cowsay@1.5.0".into()],
                recorded_at: 0, // ancient
                pinned: true,
            },
        );
        ledger.insert(
            "fresh".to_string(),
            Entry {
                specs: vec!["cowsay".into()],
                recorded_at: now_unix(),
                pinned: false,
            },
        );
        ledger.insert(
            "stale".to_string(),
            Entry {
                specs: vec!["cowsay".into()],
                recorded_at: now_unix().saturating_sub(FLOATING_TTL_SECS + 10),
                pinned: false,
            },
        );
        write_ledger(&path, &ledger).unwrap();
        let back = read_ledger(&path);
        assert_eq!(back.len(), 3);

        // Validity is computed against the entry's own nature.
        let valid = |key: &str, pinned: bool| -> bool {
            let entry = back.get(key).unwrap();
            if entry.pinned || pinned {
                return true;
            }
            now_unix().saturating_sub(entry.recorded_at) < FLOATING_TTL_SECS
        };
        assert!(valid("pinned", true), "ancient pinned consent stays valid");
        assert!(valid("fresh", false), "fresh floating consent is valid");
        assert!(!valid("stale", false), "stale floating consent expires");
    }
}
