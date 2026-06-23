//! Real end-to-end enforcement test for the macOS Seatbelt backend.
//!
//! This is the load-bearing verification: it spawns ACTUAL `sandbox-exec`-
//! wrapped processes and asserts the sandbox CONTAINS a malicious-shaped script
//! (can't read a seeded secret, can't write outside the package dir, can't
//! egress) while a legit build operation (read project, write package dir)
//! STILL SUCCEEDS. A green unit suite over policy structs is not enough — the
//! design is default-ON and security-critical, so the enforcement must be
//! proven against the real OS sandbox, not just the SBPL string.
//!
//! macOS-only; the Linux equivalent runs under Docker/CI (Landlock needs a
//! live kernel). Run: `cargo test -p nub-sandbox --test e2e_macos`.
#![cfg(target_os = "macos")]

use nub_sandbox::script_sandbox::{self, ScriptSandboxParams};
use nub_sandbox::{SandboxPolicy, apply_to_command};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a sandbox policy for a fixture laid out as <root>/{project, home, sandbox_home}.
fn fixture_policy(root: &Path) -> (SandboxPolicy, PathBuf, PathBuf, PathBuf) {
    let project = root.join("project");
    let package_dir = project.join("node_modules/dep");
    let home = root.join("home");
    let sandbox_home = root.join("sandboxhome");
    for d in [&package_dir, &home, &sandbox_home] {
        fs::create_dir_all(d).unwrap();
    }
    let policy = script_sandbox::policy(&ScriptSandboxParams {
        package_dir: package_dir.clone(),
        project_root: project.clone(),
        sandbox_home: sandbox_home.clone(),
        user_home: home.clone(),
        extra_write: vec![],
        registry_hosts: vec!["registry.npmjs.org".into()],
        extra_hosts: vec![],
        bundle_browser_cdns: false,
    });
    (policy, project, package_dir, home)
}

/// Run a `sh -c <script>` under the sandbox (fs/net only); return (exit_ok, log).
fn run_sandboxed(policy: &SandboxPolicy, cwd: &Path, script: &str) -> (bool, String) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).current_dir(cwd);
    apply_to_command(&mut cmd, policy).expect("apply sandbox");
    let out = cmd.output().expect("spawn sandboxed");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// Run a `sh -c <script>` with the FULL sandbox incl. the env-axis scrub applied
/// against `inherited` (then injected plumbing), exactly the embedder path. This
/// is the only helper that exercises the env axis end-to-end.
fn run_sandboxed_with_env(
    policy: &SandboxPolicy,
    cwd: &Path,
    inherited: Vec<(String, String)>,
    script: &str,
) -> (bool, String) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).current_dir(cwd);
    // env-scrub FIRST (clears + re-admits the allowlist), then the OS backend
    // wrap — the documented order.
    nub_sandbox::apply_env_scrub(&mut cmd, &policy.env, inherited);
    apply_to_command(&mut cmd, policy).expect("apply sandbox");
    let out = cmd.output().expect("spawn sandboxed");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

#[test]
fn legit_build_can_read_project_and_write_package_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let (policy, project, package_dir, _home) = fixture_policy(tmp.path());
    // a source file the build legitimately reads
    fs::write(project.join("binding.gyp"), "{}").unwrap();

    // legit: read the project file, write a build artifact into the package dir
    let script = format!(
        "cat {src} && echo built > {out}",
        src = project.join("binding.gyp").display(),
        out = package_dir.join("dep.node").display()
    );
    let (ok, log) = run_sandboxed(&policy, &package_dir, &script);
    assert!(ok, "legit build was blocked by the sandbox: {log}");
    assert!(
        package_dir.join("dep.node").exists(),
        "build artifact not written: {log}"
    );
}

#[test]
fn malicious_write_outside_package_dir_is_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let (policy, project, package_dir, home) = fixture_policy(tmp.path());

    // attempt 1: drop a backdoor into the PROJECT SOURCE (read-only)
    let backdoor = project.join("backdoor.js");
    let (_ok, _log) = run_sandboxed(
        &policy,
        &package_dir,
        &format!("echo pwned > {}", backdoor.display()),
    );
    assert!(
        !backdoor.exists(),
        "sandbox allowed a write into the read-only project source"
    );

    // attempt 2: persistence write into HOME (e.g. ~/.bashrc-style)
    let rc = home.join(".bashrc");
    run_sandboxed(
        &policy,
        &package_dir,
        &format!("echo evil >> {}", rc.display()),
    );
    assert!(
        !rc.exists(),
        "sandbox allowed a persistence write into HOME"
    );
}

#[test]
fn malicious_secret_read_is_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let (policy, _project, package_dir, home) = fixture_policy(tmp.path());

    // seed a credential in the (real, for-this-test) home secret dir
    let ssh = home.join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    let key = ssh.join("id_rsa");
    fs::write(&key, "SUPER-SECRET-KEY").unwrap();

    // the script tries to exfiltrate the key to stdout
    let (_ok, log) = run_sandboxed(&policy, &package_dir, &format!("cat {}", key.display()));
    assert!(
        !log.contains("SUPER-SECRET-KEY"),
        "sandbox leaked a seeded secret: {log}"
    );
}

#[test]
fn dotenv_read_is_blocked_at_any_depth() {
    let tmp = tempfile::tempdir().unwrap();
    let (policy, project, package_dir, _home) = fixture_policy(tmp.path());

    // a .env inside the (readable) project tree must still be read-denied
    let env = project.join(".env");
    fs::write(&env, "DATABASE_PASSWORD=hunter2").unwrap();
    let (_ok, log) = run_sandboxed(&policy, &package_dir, &format!("cat {}", env.display()));
    assert!(
        !log.contains("hunter2"),
        "sandbox leaked a project .env secret: {log}"
    );
}

#[test]
fn network_egress_is_blocked() {
    use std::io::Read;
    use std::net::TcpListener;

    let tmp = tempfile::tempdir().unwrap();
    let (policy, _project, package_dir, _home) = fixture_policy(tmp.path());

    // NEGATIVE CONTROL — bind a loopback listener OUTSIDE the sandbox that is
    // provably accepting, so a failed connect can ONLY mean the sandbox denied
    // the socket (no routing/firewall/DNS confound, unlike dialing 1.1.1.1).
    // The accept thread reads one byte so a real connection leaves a trace.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().unwrap().port();
    // Accept in a LOOP for the whole test (detached; dies on process exit). The
    // listener stays live through BOTH dials — so the sandboxed dial WOULD
    // connect if the sandbox were a no-op. (A one-shot accept would let the
    // sandboxed assertion pass vacuously once the listener closed — the exact
    // hollowness this control exists to kill.)
    std::thread::spawn(move || {
        while let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4];
            let _ = s.read(&mut buf);
        }
    });

    // Same dial script for both runs — bash's /dev/tcp (macOS `/bin/sh` is bash
    // in POSIX mode and supports it). The ONLY difference is the sandbox wrap.
    let dial = format!(
        "exec 3<>/dev/tcp/127.0.0.1/{port} && printf ping >&3 && echo CONNECTED || echo BLOCKED"
    );

    // (a) BASELINE — unsandboxed: the dial MUST connect. If this fails, the test
    // environment can't even reach its own loopback listener and the sandboxed
    // assertion below would be vacuous — so this proves the target is reachable.
    let baseline = Command::new("sh")
        .arg("-c")
        .arg(&dial)
        .current_dir(&package_dir)
        .output()
        .expect("spawn baseline dial");
    let baseline_log = format!(
        "{}{}",
        String::from_utf8_lossy(&baseline.stdout),
        String::from_utf8_lossy(&baseline.stderr)
    );
    assert!(
        baseline_log.contains("CONNECTED"),
        "negative control FAILED: unsandboxed dial to a live loopback listener \
         did not connect — the sandboxed assertion would be vacuous: {baseline_log}"
    );

    // (b) SANDBOXED — the identical dial, listener STILL live. Seatbelt denies the
    // outbound socket at creation, so the dial cannot CONNECT.
    let (ok, log) = run_sandboxed(&policy, &package_dir, &dial);
    assert!(
        !log.contains("CONNECTED"),
        "sandbox allowed an outbound network connection (loopback): ok={ok} log={log}"
    );
}

#[test]
fn parent_env_secret_does_not_leak_into_sandboxed_script() {
    // TWO-SIDED allowlist proof through the macOS sandbox-exec re-wrap (the exact
    // site of the env-leak regression — the wrapped Command would otherwise
    // inherit this process's FULL env). Both halves must hold END-TO-END, post-
    // wrap, or the scrub is wrong:
    //   DENY half  — a seeded `AWS_*`/`*_TOKEN`/`*_PAT` secret must be ABSENT.
    //   ALLOW half — a seeded ALLOWLISTED var (`npm_config_registry`) must be
    //                PRESENT, and PATH must be non-empty. WITHOUT this half an
    //                all-nuking scrub (`env_clear` + empty re-admit) would pass
    //                the deny-only assertion while silently breaking every build.
    let tmp = tempfile::tempdir().unwrap();
    let (policy, _project, package_dir, _home) = fixture_policy(tmp.path());

    // Seed a secret into THIS process's REAL environment too — under a unique,
    // never-allowlisted name. This is what makes the deny half guard the actual
    // env-leak REGRESSION SITE: the macOS wrap builds a FRESH `sandbox-exec`
    // Command that would inherit our full real env unless it `env_clear()`s. If
    // that clear ever regresses, this real-env secret leaks into the child —
    // independent of the `inherited` vec (which only exercises `apply_env_scrub`).
    // SAFETY: single-threaded test setup, before any spawn reads the env.
    unsafe { std::env::set_var("NUB_TEST_REAL_PARENT_SECRET", "REAL-LEAK-XYZ") };

    let inherited = vec![
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        ),
        // allowlisted via the `npm_config_` prefix — a benign, build-essential var.
        (
            "npm_config_registry".to_string(),
            "https://safe-registry.example".to_string(),
        ),
        (
            "AWS_SECRET_ACCESS_KEY".to_string(),
            "LEAKED-SECRET".to_string(),
        ),
        ("NPM_TOKEN".to_string(), "LEAKED-TOKEN".to_string()),
        ("GH_PAT".to_string(), "LEAKED-PAT".to_string()),
    ];
    let (_ok, log) = run_sandboxed_with_env(
        &policy,
        &package_dir,
        inherited,
        // print every var so both halves are asserted from ONE child env dump.
        // R= proves the REAL-env secret was cleared by the wrap (regression site).
        "echo S=$AWS_SECRET_ACCESS_KEY T=$NPM_TOKEN P=$GH_PAT R=$NUB_TEST_REAL_PARENT_SECRET \
         REG=$npm_config_registry PATHLEN=${#PATH}",
    );
    // DENY half (scrub) — no `inherited` secret survived `apply_env_scrub`.
    assert!(
        !log.contains("LEAKED"),
        "parent secret env leaked into the sandboxed script: {log}"
    );
    // DENY half (wrap env_clear regression site) — the REAL-env secret must NOT
    // appear; if the macOS wrap stops clearing the inherited env, this leaks.
    assert!(
        !log.contains("REAL-LEAK-XYZ"),
        "real parent-env secret leaked past the sandbox-exec re-wrap env_clear: {log}"
    );
    // ALLOW half — the allowlisted var reached the child through the re-wrap.
    assert!(
        log.contains("REG=https://safe-registry.example"),
        "allowlisted npm_config_registry did NOT reach the sandboxed child \
         (an over-nuking scrub would drop it and silently break builds): {log}"
    );
    // PATH must be non-empty in the child (PATHLEN=0 means PATH was dropped).
    assert!(
        !log.contains("PATHLEN=0") && log.contains("PATHLEN="),
        "allowlisted PATH was empty in the sandboxed child: {log}"
    );
}

#[test]
fn node_gyp_style_cache_write_succeeds_even_when_dir_absent() {
    // The §5-mandatory carve-out: a build writes into ~/.cache/node-gyp, which on
    // a COLD cache does not exist at sandbox-apply time. apply_to_command must
    // pre-create the confined write roots so the grant lands and the write
    // succeeds (regression for the canonicalize-on-missing-path silent-deny bug).
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let package_dir = project.join("node_modules/dep");
    let home = tmp.path().join("home");
    let sandbox_home = tmp.path().join("sandboxhome");
    for d in [&package_dir, &home, &sandbox_home] {
        fs::create_dir_all(d).unwrap();
    }
    // node-gyp cache dir DELIBERATELY not created — simulate a cold cache.
    let gyp_cache = home.join(".cache/node-gyp");
    assert!(!gyp_cache.exists());

    let policy = script_sandbox::policy(&ScriptSandboxParams {
        package_dir: package_dir.clone(),
        project_root: project.clone(),
        sandbox_home: sandbox_home.clone(),
        user_home: home.clone(),
        extra_write: script_sandbox::default_extra_write(&home, None),
        registry_hosts: vec![],
        extra_hosts: vec![],
        bundle_browser_cdns: false,
    });

    let target = gyp_cache.join("26.0.0/node.lib");
    let (ok, log) = run_sandboxed(
        &policy,
        &package_dir,
        &format!(
            "mkdir -p {dir} && echo hdr > {f}",
            dir = gyp_cache.join("26.0.0").display(),
            f = target.display()
        ),
    );
    assert!(ok, "node-gyp cache write was blocked by the sandbox: {log}");
    assert!(target.exists(), "node-gyp cache file not written: {log}");
}
