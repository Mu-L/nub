//! Windows / other-OS scaffold backend.
//!
//! TODO(script-sandbox Windows Tier 1, `.fray/script-sandbox-design.md` §1 "Windows"):
//! the unprivileged OS write-confinement is `CreateRestrictedToken(WRITE_RESTRICTED,
//! restrictingSids=[per-sandbox synthetic write-SID])` + `SetNamedSecurityInfo`
//! ACL grants on the writable roots, spawned `CREATE_SUSPENDED` → assign-to-job
//! → apply-token → `ResumeThread`. Plus the per-`.env*` deny-read ACEs and the
//! cap-SID inheritable allow-rw from `.fray/sandbox-fs-deny-list.md` (Windows
//! mechanism, MS-docs-confirmed, no admin). The Job-Object active-process /
//! memory limits (Tier 0) are already applied by aube's `windows_job.rs`
//! reaping path — the env-scrub (Tier 0) is applied by the engine's
//! `apply_env_scrub` regardless of OS.
//!
//! Until that lands, this backend enforces NOTHING at the OS layer (env-scrub
//! still applies via the caller) and reports the gap honestly so the caller
//! surfaces the reduced-mode WARNING. It is FAIL-SAFE in the sense that it never
//! claims enforcement it didn't deliver — but it is NOT yet at parity, which is
//! the explicit first-cut scope (the other OS backends are scaffolded/stubbed).

use crate::backend::Degradation;
use crate::policy::SandboxPolicy;
use std::process::Command;

pub fn apply(_cmd: &mut Command, policy: &SandboxPolicy) -> std::io::Result<Degradation> {
    let mut lost = Vec::new();
    if policy.fs.write_enforce || policy.fs.read_enforce {
        lost.push("fs".into());
    }
    if policy.net.enforce {
        lost.push("net".into());
    }
    Ok(Degradation {
        lost,
        reason: Some(
            "OS write/net sandbox not yet implemented on this platform (env-scrub only)".into(),
        ),
    })
}

// NOTE: this module is `#[cfg(not(any(target_os = "linux", target_os = "macos")))]`,
// so these tests compile + run ONLY on the Windows/other-OS CI leg — exactly the
// platforms where the stub is the live backend. They are the regression guard for
// the brief's "reports the gap rather than claiming enforcement" claim: a stub
// that returned `Degradation::full()` (falsely claiming a write/net sandbox it
// does not enforce) would ship undetected without these.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::script_sandbox::{self, ScriptSandboxParams};
    use std::path::PathBuf;

    fn enforcing_policy() -> SandboxPolicy {
        script_sandbox::policy(&ScriptSandboxParams {
            package_dir: PathBuf::from("C:/proj/node_modules/dep"),
            project_root: PathBuf::from("C:/proj"),
            sandbox_home: PathBuf::from("C:/tmp/nub-sandbox/1/dep"),
            user_home: PathBuf::from("C:/Users/me"),
            extra_write: vec![],
            registry_hosts: vec!["registry.npmjs.org".into()],
            extra_hosts: vec![],
            bundle_browser_cdns: false,
        })
    }

    #[test]
    fn stub_reports_fs_and_net_lost_never_claims_full() {
        let policy = enforcing_policy();
        // sanity: the script-sandbox profile DOES request fs+net enforcement.
        assert!(policy.fs.write_enforce && policy.net.enforce);

        let mut cmd = Command::new("cmd");
        let deg = apply(&mut cmd, &policy).expect("stub apply");
        // The honest gap: the stub enforces NOTHING at the OS layer, so for an
        // enforcing policy it must report BOTH axes lost and NEVER read as full.
        assert!(
            !deg.is_full(),
            "stub falsely claimed full enforcement it does not deliver"
        );
        assert!(deg.lost.iter().any(|a| a == "fs"), "fs not reported lost");
        assert!(deg.lost.iter().any(|a| a == "net"), "net not reported lost");
        assert!(
            deg.warning().is_some(),
            "stub must surface a reduced-mode warning"
        );
    }
}
