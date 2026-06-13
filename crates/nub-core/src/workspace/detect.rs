//! Workspace and project root detection.

use std::fs;
use std::path::{Path, PathBuf};

/// A detected workspace or standalone project.
#[derive(Debug)]
pub struct Project {
    /// The project root (nearest package.json).
    pub root: PathBuf,
    /// The workspace root, if different from root.
    pub workspace_root: Option<PathBuf>,
    /// Parsed package.json at root.
    pub manifest: serde_json::Value,
}

/// Walk up from `cwd` to find the project root and workspace root.
pub fn detect_project(cwd: &Path) -> Option<Project> {
    let mut dir = cwd.to_path_buf();
    let mut project_root = None;
    let mut workspace_root = None;

    for _ in 0..32 {
        let pkg_path = dir.join("package.json");
        if pkg_path.is_file() {
            if let Ok(content) = fs::read_to_string(&pkg_path) {
                if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                    if project_root.is_none() {
                        project_root = Some((dir.clone(), manifest.clone()));
                    }
                    if manifest.get("workspaces").is_some() {
                        workspace_root = Some(dir.clone());
                        break;
                    }
                }
            }
        }

        // Also check for pnpm-workspace.yaml — but ONLY when pnpm is the
        // incumbent PM here. The brand hard gate (AGENTS.md): when the project's
        // PM is not pnpm, nub must never read a pnpm-NAMED path. A committed
        // `pnpm-lock.yaml` beside it is the incumbent signal (file-presence
        // detection, not config-consumption). Without it, a stray
        // `pnpm-workspace.yaml` must not make this dir the workspace root.
        let pnpm_ws = dir.join("pnpm-workspace.yaml");
        if pnpm_ws.is_file() && crate::workspace::filter::pnpm_is_incumbent(&dir) {
            workspace_root = Some(dir.clone());
            if project_root.is_none() {
                let pkg_path = dir.join("package.json");
                if let Ok(content) = fs::read_to_string(&pkg_path) {
                    if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&content) {
                        project_root = Some((dir.clone(), manifest));
                    }
                }
            }
            break;
        }

        if !dir.pop() {
            break;
        }
    }

    project_root.map(|(root, manifest)| Project {
        root,
        workspace_root,
        manifest,
    })
}

/// List workspace member package.json paths matching a filter.
pub fn find_workspace_members(workspace_root: &Path, _filter: Option<&str>) -> Vec<PathBuf> {
    // Simplified: read the workspace root's package.json for the
    // workspaces field and glob-match. Full glob support deferred.
    let pkg_path = workspace_root.join("package.json");
    let content = match fs::read_to_string(&pkg_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let manifest: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let patterns = match manifest.get("workspaces") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<_>>(),
        _ => return vec![],
    };

    let mut members = Vec::new();
    for pattern in &patterns {
        let base = pattern.trim_end_matches("/*").trim_end_matches("/**");
        let search_dir = workspace_root.join(base);
        if let Ok(entries) = fs::read_dir(&search_dir) {
            for entry in entries.flatten() {
                let member_pkg = entry.path().join("package.json");
                if member_pkg.is_file() {
                    members.push(entry.path());
                }
            }
        }
    }

    members
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-detect-gate-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"root"}"#).unwrap();
        std::fs::write(dir.join("pnpm-workspace.yaml"), "packages:\n  - 'pkgs/*'\n").unwrap();
        dir
    }

    // pnpm-workspace.yaml brand hard gate (AGENTS.md): `detect_project` may treat
    // a dir as a workspace root via `pnpm-workspace.yaml` ONLY when pnpm is the
    // incumbent PM (a committed `pnpm-lock.yaml`). A root package.json with no
    // `workspaces` field isolates the pnpm-workspace.yaml signal.

    #[test]
    fn pnpm_workspace_yaml_does_not_set_root_when_pnpm_not_incumbent() {
        let dir = fixture("no-lock");
        let proj = detect_project(&dir).expect("root package.json detected");
        assert_eq!(
            proj.workspace_root, None,
            "a stray pnpm-workspace.yaml (no pnpm-lock.yaml) must not make this a workspace root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pnpm_workspace_yaml_sets_root_when_pnpm_lock_present() {
        let dir = fixture("with-lock");
        std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        let proj = detect_project(&dir).expect("root package.json detected");
        assert_eq!(
            proj.workspace_root.as_deref(),
            Some(dir.as_path()),
            "pnpm-lock.yaml proves pnpm incumbent → pnpm-workspace.yaml sets the workspace root"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
