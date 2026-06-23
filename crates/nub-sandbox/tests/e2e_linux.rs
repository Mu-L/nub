//! Real end-to-end enforcement test for the Linux Landlock+seccomp backend.
//!
//! Asserts what the Linux first cut DOES enforce: fs-write confinement
//! (Landlock) + network egress deny (seccomp), and that a legit build write
//! into the package dir succeeds. The secret-READ deny is a documented Linux
//! follow-on (Landlock is allow-only — the script-sandbox grants `/` read), so this
//! file deliberately does NOT assert secret-read denial on Linux (that would be
//! a false claim; the backend reports `fs-read-deny` as degraded).
//!
//! Needs a live kernel with Landlock (>= 5.19) — run under Docker/CI:
//! `cargo test -p nub-sandbox --test e2e_linux`. If Landlock is unavailable the
//! backend degrades (no fs sandbox); the write-confine assertions are then skipped
//! via a capability check so the test doesn't false-fail on an old kernel.
#![cfg(target_os = "linux")]

use nub_sandbox::script_sandbox::{self, ScriptSandboxParams};
use nub_sandbox::{SandboxPolicy, apply_to_command};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_policy(root: &Path) -> (SandboxPolicy, PathBuf, PathBuf) {
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
        sandbox_home,
        user_home: home,
        extra_write: vec![],
        registry_hosts: vec![],
        extra_hosts: vec![],
        bundle_browser_cdns: false,
    });
    (policy, project, package_dir)
}

fn run_sandboxed(policy: &SandboxPolicy, cwd: &Path, script: &str) -> (bool, String) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).current_dir(cwd);
    apply_to_command(&mut cmd, policy).expect("apply sandbox");
    let out = cmd.output().expect("spawn sandboxed");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// Probe: does a write into a denied path actually fail? If not (no Landlock),
/// skip the write-confine assertions rather than false-fail.
fn landlock_enforcing(root: &Path) -> bool {
    let (_policy, project, package_dir) = fixture_policy(root);
    let probe = project.join("probe");
    run_sandboxed(
        &_policy,
        &package_dir,
        &format!("echo x > {}", probe.display()),
    );
    !probe.exists()
}

#[test]
fn legit_write_into_package_dir_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let (policy, _project, package_dir) = fixture_policy(tmp.path());
    let artifact = package_dir.join("dep.node");
    let (ok, log) = run_sandboxed(
        &policy,
        &package_dir,
        &format!("echo built > {}", artifact.display()),
    );
    assert!(ok, "legit package-dir write blocked: {log}");
    assert!(artifact.exists(), "artifact not written: {log}");
}

#[test]
fn write_outside_package_dir_is_blocked_when_landlock_available() {
    let tmp = tempfile::tempdir().unwrap();
    if !landlock_enforcing(tmp.path()) {
        eprintln!("Landlock not enforcing on this kernel — skipping write-confine assertion");
        return;
    }
    let (policy, project, package_dir) = fixture_policy(tmp.path());
    let backdoor = project.join("backdoor.js");
    run_sandboxed(
        &policy,
        &package_dir,
        &format!("echo pwned > {}", backdoor.display()),
    );
    assert!(
        !backdoor.exists(),
        "Landlock allowed a write into the read-only project source"
    );
}

/// A POSIX C client that calls `socket(AF_INET, SOCK_STREAM, 0)` then
/// `connect()` to `127.0.0.1:<port>` and prints exactly one of:
///   - `SOCKET_EPERM`   — socket()/connect() returned EPERM/EACCES (seccomp deny)
///   - `CONNECTED`      — the connection succeeded (no sandbox / wrong reason)
///   - `CONNFAIL:<e>`   — connect failed for SOME OTHER reason (e.g. ECONNREFUSED)
///
/// This is the REAL test the `/dev/tcp` shell trick could not be: dash/busybox
/// has no `/dev/tcp`, so the old test passed even with seccomp removed. Here the
/// `socket()` syscall is genuinely issued; only seccomp can turn it into EPERM,
/// and we DISTINGUISH EPERM (the seccomp deny we want) from ECONNREFUSED/
/// ETIMEDOUT (which would mean the socket was created and routing merely failed —
/// the wrong reason, and the bug HIGH-1 warns about).
const NET_CLIENT_C: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
int main(int argc, char **argv) {
    int port = argc > 1 ? atoi(argv[1]) : 0;
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        if (errno == EPERM || errno == EACCES) { printf("SOCKET_EPERM\n"); return 0; }
        printf("SOCKFAIL:%d\n", errno); return 0;
    }
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons(port);
    inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);
    if (connect(fd, (struct sockaddr*)&a, sizeof a) == 0) { printf("CONNECTED\n"); return 0; }
    if (errno == EPERM || errno == EACCES) { printf("SOCKET_EPERM\n"); return 0; }
    printf("CONNFAIL:%d\n", errno); return 0;
}
"#;

/// Compile `NET_CLIENT_C` to `<dir>/netclient` with `cc`. Returns None if no C
/// compiler is present (then the test self-skips rather than false-pass).
fn build_net_client(dir: &Path) -> Option<PathBuf> {
    let src = dir.join("netclient.c");
    let bin = dir.join("netclient");
    fs::write(&src, NET_CLIENT_C).unwrap();
    let cc = if Command::new("cc").arg("--version").output().is_ok() {
        "cc"
    } else if Command::new("gcc").arg("--version").output().is_ok() {
        "gcc"
    } else {
        return None;
    };
    let out = Command::new(cc)
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "cc failed to build net client: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    Some(bin)
}

#[test]
fn network_egress_is_blocked() {
    use std::io::Read;
    use std::net::TcpListener;

    let tmp = tempfile::tempdir().unwrap();
    let (policy, _project, package_dir) = fixture_policy(tmp.path());

    let Some(client) = build_net_client(tmp.path()) else {
        eprintln!("no C compiler — skipping real socket() egress test");
        return;
    };

    // NEGATIVE CONTROL — a provably-accepting loopback listener bound OUTSIDE the
    // sandbox. An unsandboxed dial MUST connect (proves the target is reachable);
    // a sandboxed dial MUST get SOCKET_EPERM from seccomp (NOT ECONNREFUSED — that
    // would mean the socket was created and routing failed, the wrong reason).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = listener.local_addr().unwrap().port();
    // Accept in a LOOP for the whole test (detached). The listener stays live
    // through BOTH dials, so the sandboxed dial WOULD connect if seccomp were a
    // no-op — a one-shot accept would let the sandboxed assertion pass vacuously
    // once the listener closed.
    std::thread::spawn(move || {
        while let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4];
            let _ = s.read(&mut buf);
        }
    });

    // (a) BASELINE — unsandboxed: must CONNECT. If not, the env can't reach its
    // own loopback listener and the sandboxed assertion would be vacuous.
    let baseline = Command::new(&client)
        .arg(port.to_string())
        .current_dir(&package_dir)
        .output()
        .expect("spawn baseline net client");
    let baseline_log = String::from_utf8_lossy(&baseline.stdout).into_owned();
    assert!(
        baseline_log.contains("CONNECTED"),
        "negative control FAILED: unsandboxed socket()+connect() to a live \
         loopback listener did not connect — sandboxed assertion would be \
         vacuous: {baseline_log}"
    );

    // (b) SANDBOXED — the identical client under seccomp, listener STILL live.
    // socket(AF_INET) must
    // return EPERM. Asserting SOCKET_EPERM (not merely "no CONNECTED") is what
    // makes this prove the seccomp filter is the cause — a hollow test (seccomp
    // removed) would print CONNECTED or CONNFAIL, never SOCKET_EPERM.
    let mut cmd = Command::new(&client);
    cmd.arg(port.to_string()).current_dir(&package_dir);
    apply_to_command(&mut cmd, &policy).expect("apply sandbox");
    let out = cmd.output().expect("spawn sandboxed net client");
    let log = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !log.contains("CONNECTED"),
        "seccomp allowed an outbound network connection: {log}"
    );
    assert!(
        log.contains("SOCKET_EPERM"),
        "seccomp did NOT deny socket(AF_INET) with EPERM — egress is NOT enforced \
         (a CONNFAIL/ECONNREFUSED here means the socket was created and routing \
         merely failed, which is the WRONG reason / a hollow pass): {log}"
    );
}
