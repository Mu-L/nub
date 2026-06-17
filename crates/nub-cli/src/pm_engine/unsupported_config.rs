//! Unsupported-config detection + the cheap config-driven install wins.
//!
//! Two halves, both grounded in the same per-incumbent config readers:
//!
//! **A) IMPLEMENT-wins** — config that nub's existing machinery can honor once
//! it's read. Rather than warn/error on these, nub mirrors the incumbent:
//!   1. Dep-type selection — npm `.npmrc` `omit`/`include`, bun bunfig
//!      `[install].production` → the engine's `DepSelection`
//!      (`--prod`/`--dev`/`--no-optional` axis).
//!   2. Frozen-from-config — bun bunfig in-file `frozenLockfile`, yarn
//!      `enableImmutableInstalls`/`immutablePatterns` → the engine's frozen
//!      mode (same path `--frozen-lockfile` takes).
//!   3. `enableScripts: false` (yarn) → force a block-all-builds policy that
//!      overrides even nub's `defaultTrust` floor.
//!   4. `dependenciesMeta.*.injected` → auto-switch to the isolated linker (the
//!      only layout where aube materializes injected copies) instead of
//!      silently dropping the directive under nub's hoisted default.
//!
//! (`minimumReleaseAge` from bunfig is wired in [`super::bun_config`] — it maps
//! to a synthetic `.npmrc` entry the settings registry already reads.)
//!
//! **B) The scan** ([`scan_unsupported_config`]) — for the genuinely-hard set
//! that nub does NOT implement, a curated FATAL/WARN sweep so the launch claim
//! "nub aborts if unsupported config is detected" holds. FATAL fields abort
//! with an `ERR_NUB_*` code + a remedy (no `--force`); WARN fields proceed with
//! a dim line. NOT a blanket unknown-key warn — only a curated load-bearing set.
//!
//! All readers are name-gated by the resolved [`Role`]: a field is only read
//! from a config surface the active PM owns (an `.npmrc` `omit` is npm's; a
//! `bunfig.toml` key is bun's; a `.yarnrc.yml` key is yarn's), matching the
//! symmetric brand-boundary discipline the rest of `pm_engine` enforces.

use std::path::{Path, PathBuf};

use super::config_scope::{IgnoredField, Role};

/// A config-derived override of the dependency selection axis
/// (`--prod`/`--dev`/`--no-optional`). `None` per field means "not pinned by
/// config — leave the CLI/default behavior". Composed onto the install args
/// only when the active PM owns the config that set it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DepSelectionConfig {
    pub(crate) prod: bool,
    pub(crate) dev: bool,
    pub(crate) no_optional: bool,
}

impl DepSelectionConfig {
    fn is_empty(self) -> bool {
        !self.prod && !self.dev && !self.no_optional
    }
}

/// Read the dependency-selection axis the active PM's persistent config pins.
///
/// - **npm** — `.npmrc` `omit` / `include` (comma- or space-separated lists of
///   `dev` / `optional` / `peer`). `omit=dev` ⇒ prod; `omit=optional` ⇒
///   no-optional; `include=` un-sets a prior `omit` of the same type (npm's own
///   precedence: `include` wins). nub honors the `--prod`/`--no-optional`
///   *flags* already; this is the persistent `.npmrc` spelling of the same.
/// - **bun** — bunfig `[install].production = true` ⇒ prod (omit devDeps).
///
/// Returns `None` (no pin) for roles whose config carries no dep-axis signal,
/// or when the config doesn't set one. The CLI flag still composes on top —
/// this only seeds the default when a flag is absent.
pub(crate) fn dep_selection_from_config(role: Role, root: &Path) -> Option<DepSelectionConfig> {
    let cfg = match role {
        Role::Npm => npm_omit_include(root),
        Role::Bun => bunfig_production(root),
        // pnpm/yarn/nub: no persistent dep-axis config nub doesn't already
        // read through its own surfaces.
        Role::Pnpm | Role::Yarn | Role::Nub => DepSelectionConfig::default(),
    };
    (!cfg.is_empty()).then_some(cfg)
}

/// npm `.npmrc` `omit` / `include` → dep-selection axis. Reads the project
/// `.npmrc` (walk-up to the root) then the user `~/.npmrc`, project winning.
fn npm_omit_include(root: &Path) -> DepSelectionConfig {
    // Collect `omit` / `include` from user then project so project wins.
    let mut omit: Vec<String> = Vec::new();
    let mut include: Vec<String> = Vec::new();
    for path in npmrc_paths(root) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(v) = npmrc_scalar(&content, "omit") {
            omit = split_list(&v);
        }
        if let Some(v) = npmrc_scalar(&content, "include") {
            include = split_list(&v);
        }
    }
    // npm: `include` removes a type from the effective omit set.
    let omits = |ty: &str| omit.iter().any(|o| o == ty) && !include.iter().any(|i| i == ty);
    DepSelectionConfig {
        prod: omits("dev"),
        dev: false,
        no_optional: omits("optional"),
    }
}

/// bun bunfig `[install].production = true` → prod (omit devDependencies).
fn bunfig_production(root: &Path) -> DepSelectionConfig {
    let prod = bunfig_install_bool(root, "production").unwrap_or(false);
    DepSelectionConfig {
        prod,
        dev: false,
        no_optional: false,
    }
}

/// Whether the active PM's config requests a frozen / immutable install — the
/// in-file / config spellings of `--frozen-lockfile` that nub's CLI flag path
/// already honors but the config readers do not.
///
/// - **bun** — bunfig `[install].frozenLockfile = true`.
/// - **yarn** — `.yarnrc.yml` `enableImmutableInstalls: true` (Berry's default
///   in CI) or a non-empty `immutablePatterns` list. Either is the Yarn
///   `--immutable` contract: abort if the lockfile would change.
///
/// Maps to `FrozenMode::Frozen` (the strict CI guard), mirroring what the real
/// PM does. The CLI `--no-frozen-lockfile` still overrides (it's applied after).
pub(crate) fn frozen_from_config(role: Role, root: &Path) -> bool {
    match role {
        Role::Bun => bunfig_install_bool(root, "frozenLockfile").unwrap_or(false),
        Role::Yarn => yarn_immutable(root),
        Role::Npm | Role::Pnpm | Role::Nub => false,
    }
}

/// yarn `.yarnrc.yml` `enableImmutableInstalls: true` or a non-empty
/// `immutablePatterns:` block.
fn yarn_immutable(root: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(root.join(".yarnrc.yml")) else {
        return false;
    };
    if yarnrc_top_level_bool(&content, "enableImmutableInstalls") == Some(true) {
        return true;
    }
    // `immutablePatterns:` followed by an indented list ⇒ non-empty.
    yarnrc_block_nonempty(&content, "immutablePatterns")
}

/// Whether yarn's `enableScripts: false` is set — the security opt-out that
/// disables ALL lifecycle scripts. When true the install must force a
/// block-all-builds policy that overrides even nub's `defaultTrust` floor.
pub(crate) fn yarn_scripts_disabled(role: Role, root: &Path) -> bool {
    if role != Role::Yarn {
        return false;
    }
    std::fs::read_to_string(root.join(".yarnrc.yml"))
        .ok()
        .and_then(|c| yarnrc_top_level_bool(&c, "enableScripts"))
        == Some(false)
}

/// Whether the root (or any workspace member) manifest declares
/// `dependenciesMeta.<pkg>.injected: true`. aube materializes injected copies
/// only under the isolated linker, so when injection is present nub auto-
/// switches the embedder's `nodeLinker` default to `isolated` (instead of the
/// hoisted default for flat-layout incumbents) rather than silently dropping
/// the directive.
pub(crate) fn injected_deps_present(root: &Path) -> bool {
    if manifest_has_injected(&root.join("package.json")) {
        return true;
    }
    if let Ok(members) = aube_workspace::find_workspace_packages(root) {
        for dir in members {
            if manifest_has_injected(&dir.join("package.json")) {
                return true;
            }
        }
    }
    false
}

fn manifest_has_injected(manifest_path: &Path) -> bool {
    let Ok(manifest) = aube_manifest::PackageJson::from_path(manifest_path) else {
        return false;
    };
    let Some(meta) = manifest
        .extra
        .get("dependenciesMeta")
        .and_then(|v| v.as_object())
    else {
        return false;
    };
    meta.values().any(|v| {
        v.as_object()
            .and_then(|o| o.get("injected"))
            .and_then(|b| b.as_bool())
            == Some(true)
    })
}

// ───────────────────────── the scan ─────────────────────────

/// One unsupported field the scan flagged FATAL: an `ERR_NUB_*` code, a
/// one-line explanation of what nub does NOT support, and a remedy.
struct FatalField {
    code: &'static str,
    field: &'static str,
    detail: &'static str,
    remedy: &'static str,
}

/// Result of the curated unsupported-config scan: a FATAL abort (the first
/// load-bearing field nub can't honor) or a list of WARN fields to surface.
pub(crate) enum ScanResult {
    Fatal(anyhow::Error),
    Warn(Vec<IgnoredField>),
}

/// Curated unsupported-config scan for one install. FATAL on the genuinely-hard
/// load-bearing fields nub does not implement (returns the first hit so the
/// abort names a concrete remedy); otherwise returns the WARN set.
///
/// The FATAL set is deliberately SHORT — only fields whose silent omission
/// produces a correctness-divergent install AND which nub cannot honor:
/// npm `legacy-peer-deps` (different peer graph), npm `install-strategy=nested`
/// (different resolution/layout), and yarn `supportedArchitectures` (changes
/// which platform deps land — the arch-filter resolver isn't built). PnP stays
/// a WARN+downgrade (handled separately in `warn_if_pnp_requested`), not
/// promoted. `checksumBehavior`/`enableHardenedMode` are NOT here: aube verifies
/// every tarball's SHA-512 by default (`verifyStoreIntegrity=true`), satisfying
/// the `throw` posture.
pub(crate) fn scan_unsupported_config(
    role: Role,
    major: Option<u64>,
    minor: Option<u64>,
    root: &Path,
) -> ScanResult {
    let _ = (major, minor);
    // FATAL — first hit aborts.
    if let Some(fatal) = scan_fatal(role, root) {
        return ScanResult::Fatal(anyhow::anyhow!(
            "nub: {} ({}) is not supported — {}. {} [{}]",
            fatal.field,
            role.display(),
            fatal.detail,
            fatal.remedy,
            fatal.code,
        ));
    }
    // WARN — non-load-bearing but unsupported, surfaced as dim lines.
    ScanResult::Warn(scan_warn(role, root))
}

fn scan_fatal(role: Role, root: &Path) -> Option<FatalField> {
    match role {
        Role::Npm => {
            if npmrc_bool_set(root, "legacy-peer-deps") {
                return Some(FatalField {
                    code: "ERR_NUB_UNSUPPORTED_CONFIG",
                    field: "`legacy-peer-deps`",
                    detail: "nub always resolves peer dependencies; npm's legacy escape hatch \
                             would produce a different peer graph",
                    remedy: "remove `legacy-peer-deps` from .npmrc and fix the peer conflict, \
                             or pin the conflicting versions in `overrides`",
                });
            }
            if let Some(strategy) = npmrc_value(root, "install-strategy")
                && strategy.eq_ignore_ascii_case("nested")
            {
                return Some(FatalField {
                    code: "ERR_NUB_UNSUPPORTED_CONFIG",
                    field: "`install-strategy=nested`",
                    detail: "nub installs a hoisted/isolated tree; npm's nested layout can change \
                             which version a require() resolves to",
                    remedy: "remove `install-strategy=nested` from .npmrc",
                });
            }
            None
        }
        Role::Yarn => {
            if yarn_supported_architectures(root) {
                return Some(FatalField {
                    code: "ERR_NUB_UNSUPPORTED_CONFIG",
                    field: "`supportedArchitectures`",
                    detail: "nub installs only the current platform's optional/platform deps; \
                             this setting changes which packages land on disk",
                    remedy: "remove `supportedArchitectures` from .yarnrc.yml, or run the install \
                             on each target platform",
                });
            }
            None
        }
        Role::Pnpm | Role::Bun | Role::Nub => None,
    }
}

fn scan_warn(role: Role, root: &Path) -> Vec<IgnoredField> {
    let mut out = Vec::new();
    // enableHardenedMode (yarn): aube verifies tarball SHA-512 by default, so
    // the integrity core is covered, but Berry's extra registry-range
    // re-verification is not — surface it as ignored.
    if role == Role::Yarn && yarnrc_top_level_bool_str(root, "enableHardenedMode") == Some(true) {
        out.push(IgnoredField {
            field: "enableHardenedMode",
            fix: "nub verifies every tarball's checksum by default; the extra \
                  registry-range re-verification is not applied"
                .to_string(),
        });
    }
    // Brand-symmetry consistency warn: a `pnpm.overrides` block present under a
    // role that isn't pnpm is dropped silently by the scope filter. Surface it.
    if role != Role::Pnpm && manifest_has_pnpm_overrides(root) {
        out.push(IgnoredField {
            field: "pnpm.overrides",
            fix: "nub mirrors this project's package manager and does not apply another PM's \
                  branded config; move the pins to `overrides` or `resolutions`"
                .to_string(),
        });
    }
    out
}

fn manifest_has_pnpm_overrides(root: &Path) -> bool {
    let Ok(manifest) = aube_manifest::PackageJson::from_path(&root.join("package.json")) else {
        return false;
    };
    manifest
        .extra
        .get("pnpm")
        .and_then(|v| v.as_object())
        .and_then(|p| p.get("overrides"))
        .and_then(|v| v.as_object())
        .is_some_and(|o| !o.is_empty())
}

fn yarn_supported_architectures(root: &Path) -> bool {
    std::fs::read_to_string(root.join(".yarnrc.yml"))
        .ok()
        .is_some_and(|c| yarnrc_has_top_level_key(&c, "supportedArchitectures"))
}

fn yarnrc_top_level_bool_str(root: &Path, key: &str) -> Option<bool> {
    let content = std::fs::read_to_string(root.join(".yarnrc.yml")).ok()?;
    yarnrc_top_level_bool(&content, key)
}

// ───────────────────────── npmrc reading ─────────────────────────

/// `.npmrc` files in precedence-low-to-high order: user `~/.npmrc` first, then
/// project `.npmrc` (walk from root up to filesystem root). Later entries win.
fn npmrc_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = dirs_next::home_dir() {
        paths.push(home.join(".npmrc"));
    }
    // Walk-up: ancestors first (less specific) so the project's own .npmrc wins.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut current = root.to_path_buf();
    loop {
        dirs.push(current.clone());
        if !current.pop() {
            break;
        }
    }
    dirs.reverse();
    paths.extend(dirs.into_iter().map(|d| d.join(".npmrc")));
    paths
}

/// Read a scalar `.npmrc` key from across all `.npmrc` files (project wins).
fn npmrc_value(root: &Path, key: &str) -> Option<String> {
    let mut value = None;
    for path in npmrc_paths(root) {
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Some(v) = npmrc_scalar(&content, key)
        {
            value = Some(v);
        }
    }
    value
}

/// Whether a boolean `.npmrc` key is set truthy (`key=true`, or bare `key`).
fn npmrc_bool_set(root: &Path, key: &str) -> bool {
    npmrc_value(root, key)
        .map(|v| {
            let v = v.trim();
            v.is_empty() || v.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Parse a single scalar key from `.npmrc` content (ini-style `key=value`,
/// `#`/`;` comments). Returns the LAST occurrence's value. Key match is
/// kebab/camel insensitive on the exact spelling passed.
fn npmrc_scalar(content: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            // Bare `key` (no `=`) — npm treats it as `key=true`.
            if line.eq_ignore_ascii_case(key) {
                found = Some(String::new());
            }
            continue;
        };
        if k.trim().eq_ignore_ascii_case(key) {
            found = Some(strip_inline_value(v));
        }
    }
    found
}

/// Strip surrounding quotes from an npmrc value. (npmrc does not support inline
/// `#` comments on a value line, so only quote-stripping applies.)
fn strip_inline_value(raw: &str) -> String {
    let v = raw.trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q)
            && let Some(end) = inner.find(q)
        {
            return inner[..end].to_string();
        }
    }
    v.to_string()
}

/// Split a comma- or whitespace-separated list value into lowercased tokens.
fn split_list(v: &str) -> Vec<String> {
    v.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

// ───────────────────────── bunfig reading ─────────────────────────

/// Read a boolean `[install].<key>` from the project + global bunfig.
fn bunfig_install_bool(root: &Path, key: &str) -> Option<bool> {
    let mut value = None;
    for path in bunfig_paths(root) {
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(parsed) = raw.parse::<toml::Value>()
            && let Some(b) = parsed
                .get("install")
                .and_then(toml::Value::as_table)
                .and_then(|t| t.get(key))
                .and_then(toml::Value::as_bool)
        {
            value = Some(b);
        }
    }
    value
}

/// bunfig files in low-to-high precedence: global `~/.bunfig.toml` then the
/// project `bunfig.toml` (project wins). Mirrors [`super::bun_config`]'s path
/// resolution.
fn bunfig_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let global = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .map(|dir| dir.join(".bunfig.toml"));
    if let Some(g) = global {
        paths.push(g);
    }
    paths.push(root.join("bunfig.toml"));
    paths
}

// ───────────────────────── yarnrc reading ─────────────────────────

/// Read a top-level (unindented) boolean `key:` from `.yarnrc.yml` content.
fn yarnrc_top_level_bool(content: &str, key: &str) -> Option<bool> {
    for line in content.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(key)
            && let Some(rest) = rest.strip_prefix(':')
        {
            let v = strip_yarnrc_scalar(rest);
            return match v.to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
    }
    None
}

/// Whether a top-level `key:` exists at all in `.yarnrc.yml` (scalar or block).
fn yarnrc_has_top_level_key(content: &str, key: &str) -> bool {
    content.lines().any(|line| {
        !line.starts_with(char::is_whitespace) && line.trim().starts_with(&format!("{key}:"))
    })
}

/// Whether a top-level `key:` introduces a non-empty indented block (a YAML
/// list/map) in `.yarnrc.yml`.
fn yarnrc_block_nonempty(content: &str, key: &str) -> bool {
    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(key)
            && let Some(rest) = rest.strip_prefix(':')
        {
            let inline = strip_yarnrc_scalar(rest);
            if !inline.is_empty() {
                // `key: [a, b]` inline non-empty.
                return inline != "[]";
            }
            // Block form: the next non-blank line must be indented.
            for next in lines.by_ref() {
                if next.trim().is_empty() {
                    continue;
                }
                return next.starts_with(char::is_whitespace);
            }
            return false;
        }
    }
    false
}

/// Strip surrounding quotes / trailing `# comment` from a yarnrc scalar.
fn strip_yarnrc_scalar(rest: &str) -> String {
    let rest = rest.trim();
    for q in ['"', '\''] {
        if let Some(inner) = rest.strip_prefix(q)
            && let Some(end) = inner.find(q)
        {
            return inner[..end].to_string();
        }
    }
    rest.split('#')
        .next()
        .map(str::trim)
        .unwrap_or(rest)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn npm_omit_dev_selects_prod() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "omit=dev\n").unwrap();
        let cfg = npm_omit_include(d.path());
        assert!(cfg.prod, "omit=dev must select prod-only");
        assert!(!cfg.no_optional);
    }

    #[test]
    fn npm_omit_optional_skips_optional() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "omit=optional\n").unwrap();
        let cfg = npm_omit_include(d.path());
        assert!(cfg.no_optional);
        assert!(!cfg.prod);
    }

    #[test]
    fn npm_include_overrides_omit_of_same_type() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "omit=dev\ninclude=dev\n").unwrap();
        let cfg = npm_omit_include(d.path());
        assert!(!cfg.prod, "include=dev must cancel omit=dev");
    }

    #[test]
    fn bunfig_production_selects_prod() {
        let d = tmp();
        fs::write(
            d.path().join("bunfig.toml"),
            "[install]\nproduction = true\n",
        )
        .unwrap();
        let cfg = bunfig_production(d.path());
        assert!(cfg.prod);
    }

    #[test]
    fn bunfig_frozen_lockfile_is_frozen() {
        let d = tmp();
        fs::write(
            d.path().join("bunfig.toml"),
            "[install]\nfrozenLockfile = true\n",
        )
        .unwrap();
        assert!(frozen_from_config(Role::Bun, d.path()));
    }

    #[test]
    fn yarn_immutable_installs_is_frozen() {
        let d = tmp();
        fs::write(
            d.path().join(".yarnrc.yml"),
            "enableImmutableInstalls: true\n",
        )
        .unwrap();
        assert!(frozen_from_config(Role::Yarn, d.path()));
    }

    #[test]
    fn yarn_immutable_patterns_block_is_frozen() {
        let d = tmp();
        fs::write(
            d.path().join(".yarnrc.yml"),
            "immutablePatterns:\n  - \"**/*.lock\"\n",
        )
        .unwrap();
        assert!(frozen_from_config(Role::Yarn, d.path()));
    }

    #[test]
    fn yarn_enable_scripts_false_disables_scripts() {
        let d = tmp();
        fs::write(d.path().join(".yarnrc.yml"), "enableScripts: false\n").unwrap();
        assert!(yarn_scripts_disabled(Role::Yarn, d.path()));
        // Not yarn role ⇒ ignored.
        assert!(!yarn_scripts_disabled(Role::Npm, d.path()));
    }

    #[test]
    fn injected_deps_detected_in_root_manifest() {
        let d = tmp();
        fs::write(
            d.path().join("package.json"),
            r#"{"name":"x","dependenciesMeta":{"foo":{"injected":true}}}"#,
        )
        .unwrap();
        assert!(injected_deps_present(d.path()));
    }

    #[test]
    fn scan_fatal_on_legacy_peer_deps() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "legacy-peer-deps=true\n").unwrap();
        match scan_unsupported_config(Role::Npm, Some(10), None, d.path()) {
            ScanResult::Fatal(e) => {
                let msg = e.to_string();
                assert!(msg.contains("legacy-peer-deps"), "msg: {msg}");
                assert!(msg.contains("ERR_NUB_UNSUPPORTED_CONFIG"));
            }
            ScanResult::Warn(_) => panic!("legacy-peer-deps must be FATAL"),
        }
    }

    #[test]
    fn scan_fatal_on_install_strategy_nested() {
        let d = tmp();
        fs::write(d.path().join(".npmrc"), "install-strategy=nested\n").unwrap();
        assert!(matches!(
            scan_unsupported_config(Role::Npm, None, None, d.path()),
            ScanResult::Fatal(_)
        ));
    }

    #[test]
    fn scan_fatal_on_supported_architectures() {
        let d = tmp();
        fs::write(
            d.path().join(".yarnrc.yml"),
            "supportedArchitectures:\n  os:\n    - linux\n",
        )
        .unwrap();
        assert!(matches!(
            scan_unsupported_config(Role::Yarn, None, None, d.path()),
            ScanResult::Fatal(_)
        ));
    }

    #[test]
    fn scan_warn_on_hardened_mode_not_fatal() {
        let d = tmp();
        fs::write(d.path().join(".yarnrc.yml"), "enableHardenedMode: true\n").unwrap();
        match scan_unsupported_config(Role::Yarn, None, None, d.path()) {
            ScanResult::Warn(w) => {
                assert!(w.iter().any(|f| f.field == "enableHardenedMode"));
            }
            ScanResult::Fatal(_) => panic!("hardened mode is WARN (checksum core covered by CAS)"),
        }
    }

    #[test]
    fn supported_config_does_not_trip_scan() {
        let d = tmp();
        // A benign, fully-supported .npmrc — registry + save-exact.
        fs::write(
            d.path().join(".npmrc"),
            "registry=https://registry.npmjs.org/\nsave-exact=true\n",
        )
        .unwrap();
        match scan_unsupported_config(Role::Npm, Some(10), None, d.path()) {
            ScanResult::Warn(w) => assert!(w.is_empty(), "supported config must not warn: {w:?}"),
            ScanResult::Fatal(e) => panic!("supported config tripped FATAL: {e}"),
        }
    }

    #[test]
    fn pnpm_overrides_under_npm_warns() {
        let d = tmp();
        fs::write(
            d.path().join("package.json"),
            r#"{"name":"x","pnpm":{"overrides":{"lodash":"4.17.21"}}}"#,
        )
        .unwrap();
        match scan_unsupported_config(Role::Npm, Some(10), None, d.path()) {
            ScanResult::Warn(w) => assert!(w.iter().any(|f| f.field == "pnpm.overrides")),
            ScanResult::Fatal(_) => panic!("pnpm.overrides is a WARN, not FATAL"),
        }
    }
}
